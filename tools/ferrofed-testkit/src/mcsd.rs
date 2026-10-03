// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The harness care services directory, a test device answering ITI-90 and
//! ITI-91.
//!
//! It serves the `Organization` and `Endpoint` resources a test puts in it
//! (IHE mCSD 4.0.0; Federation Tier with AQL §15.1, Annex A.5).
//!
//! **This is a test device, not an mCSD Directory.** It plays the directory a
//! gateway reads its registry from, so a test can load, change and break the
//! registry over the wire (no specification governs this: our own design).
//! It stands on the [`Server`] wrapper, and keeps every version of every
//! resource with the instant of a clock of its own, which moves one hour per
//! change and never on its own:
//!
//! - **ITI-90:** `GET [base]/Organization` and `GET [base]/Endpoint`, with an
//!   optional `identifier=[system]|` token, answer a `searchset` Bundle of the
//!   current resources that match (`CapabilityStatement-IHE.mCSD.Directory`).
//! - **ITI-91:** `GET [base]/Organization/_history?_since=[instant]` and the
//!   `Endpoint` one answer a `history` Bundle of every version made at or
//!   after the instant, newest first, a deletion as a `DELETE` entry with no
//!   resource (`CapabilityStatement-IHE.mCSD.Directory.Update`; FHIR R4
//!   history).
//!
//! Every answer carries the clock's reading as its `Date`. A member's
//! resources are built from the IG's own examples under
//! `docs/specs/ihe-mcsd/` ([`Member::organisation`], [`Member::endpoint`]),
//! with the identifiers of the registry's FHIR form and a synthetic address.
//! An outage makes every answer a `503` ([`Outage::Refusing`]) or no answer
//! within any timeout a test sets ([`Outage::Silent`]).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use fhir_types::codec::{Json, Path, Value};
use fhir_types::r4::bundle::{
    Bundle, BundleEntry, BundleEntryRequest, BundleEntryResponse, BundleEntrySearch,
};
use fhir_types::r4::codeable_concept::CodeableConcept;
use fhir_types::r4::coding::Coding;
use fhir_types::r4::endpoint::Endpoint;
use fhir_types::r4::identifier::Identifier;
use fhir_types::r4::organization::Organization;
use fhir_types::r4::reference::Reference;
use fhir_types::r4::resource::Resource;
use jiff::{SignedDuration, Timestamp};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, Request, Respond, ResponseTemplate};

use crate::mock::Server;

/// The FHIR base path the device serves under.
const BASE: &str = "/fhir";

/// The FHIR JSON media type (ITI TF-2 Appendix Z.6).
const FHIR_JSON: &str = "application/fhir+json";

/// The identifier system of an organisation's registry id.
pub const ORGANISATION_ID: &str = "https://ferrofed.eu/fhir/sid/organisation-id";

/// The identifier system of an endpoint's `endpoint_id`.
pub const ENDPOINT_ID: &str = "https://ferrofed.eu/fhir/sid/endpoint-id";

/// The identifier system of the `node_id` an endpoint belongs to.
pub const NODE_ID: &str = "https://ferrofed.eu/fhir/sid/node-id";

/// The identifier system of a node's openEHR `system_id`.
pub const SYSTEM_ID: &str = "https://ferrofed.eu/fhir/sid/system-id";

/// The code system of the openEHR Query API connection type.
pub const CONNECTION_TYPE: &str = "https://ferrofed.eu/fhir/CodeSystem/connection-type";

/// The instant the device's clock starts at.
const EPOCH: &str = "2026-01-01T00:00:00Z";

/// How far the clock moves for each change.
const TICK: SignedDuration = SignedDuration::from_hours(1);

/// How the device fails to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outage {
    /// Every request answers `503`.
    Refusing,
    /// Every request waits a minute before answering.
    Silent,
}

/// One member of the directory: an organisation operating one node through
/// one endpoint, as the registry's FHIR form spells it.
#[derive(Debug, Clone)]
pub struct Member {
    /// The organisation's registry id, also its logical id.
    pub organisation: String,
    /// The endpoint's `endpoint_id`, also its logical id.
    pub endpoint: String,
    /// The node the endpoint belongs to.
    pub node: String,
    /// The node's openEHR `system_id`.
    pub system_id: String,
    /// The endpoint's base URL.
    pub address: String,
}

impl Member {
    /// The member's `Organization`, built from the IG's example
    /// `Organization-ex-OrgA`: its id and identifiers replaced, its name left
    /// out as a registry document that names only ids leaves it out, and its
    /// `endpoint` list naming the member's endpoint.
    ///
    /// # Errors
    /// [`McsdHarnessError::Example`] when the vendored example cannot be read.
    pub fn organisation(&self) -> Result<Organization, McsdHarnessError> {
        let mut organisation: Organization = example("Organization-ex-OrgA.json")?;
        organisation.id = Some(self.organisation.clone());
        organisation.name = None;
        organisation.text = None;
        organisation.identifier = vec![identifier(ORGANISATION_ID, &self.organisation)];
        organisation.endpoint = vec![reference(&format!("Endpoint/{}", self.endpoint))];
        Ok(organisation)
    }

    /// The member's `Endpoint`, built from the IG's example
    /// `Endpoint-ex-endpointXCAquery`: the registry's identifiers, the
    /// openEHR Query API connection type, the member's organisation as its
    /// manager and its address; the document-sharing extension, profile,
    /// narrative and media types of the example are left out, since an
    /// openEHR endpoint shares no documents.
    ///
    /// # Errors
    /// [`McsdHarnessError::Example`] when the vendored example cannot be read.
    pub fn endpoint(&self) -> Result<Endpoint, McsdHarnessError> {
        let mut endpoint: Endpoint = example("Endpoint-ex-endpointXCAquery.json")?;
        endpoint.id = Some(self.endpoint.clone());
        endpoint.text = None;
        endpoint.extension.clear();
        if let Some(meta) = endpoint.meta.as_mut() {
            meta.profile = vec![
                "https://profiles.ihe.net/ITI/mCSD/StructureDefinition/IHE.mCSD.Endpoint".into(),
            ];
        }
        endpoint.identifier = vec![
            identifier(ENDPOINT_ID, &self.endpoint),
            identifier(NODE_ID, &self.node),
            identifier(SYSTEM_ID, &self.system_id),
        ];
        endpoint.status = "active".into();
        endpoint.connection_type = Coding {
            system: Some(CONNECTION_TYPE.into()),
            code: Some("openehr-rest-query".into()),
            ..Coding::default()
        };
        endpoint.managing_organization =
            Some(reference(&format!("Organization/{}", self.organisation)));
        endpoint.payload_type = vec![CodeableConcept {
            text: Some("openEHR".into()),
            ..CodeableConcept::default()
        }];
        endpoint.payload_mime_type.clear();
        endpoint.address = self.address.as_str().into();
        Ok(endpoint)
    }

    /// The member's `Endpoint` of [`Member::endpoint`], with the
    /// `connectionType` `code` in `system` in place of the openEHR one.
    ///
    /// # Errors
    /// [`McsdHarnessError::Example`] when the vendored example cannot be read.
    pub fn endpoint_with_connection_type(
        &self,
        system: &str,
        code: &str,
    ) -> Result<Endpoint, McsdHarnessError> {
        let mut endpoint = self.endpoint()?;
        endpoint.connection_type = Coding {
            system: Some(system.into()),
            code: Some(code.into()),
            ..Coding::default()
        };
        Ok(endpoint)
    }
}

/// The harness directory could not be set up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McsdHarnessError {
    /// A vendored example cannot be read or decoded.
    #[error("the vendored mCSD example {file} cannot be read")]
    Example {
        /// The example's file name.
        file: &'static str,
    },
}

/// The harness care services directory.
pub struct HarnessDirectory {
    server: Server,
    state: Arc<Mutex<State>>,
}

/// Every version of every resource, the clock and the outage.
struct State {
    clock: Timestamp,
    organizations: BTreeMap<String, Vec<(Timestamp, Option<Organization>)>>,
    endpoints: BTreeMap<String, Vec<(Timestamp, Option<Endpoint>)>>,
    outage: Option<Outage>,
    base: String,
}

impl HarnessDirectory {
    /// Starts an empty directory.
    pub async fn start() -> Self {
        let server = Server::start().await;
        let state = Arc::new(Mutex::new(State {
            clock: EPOCH.parse().unwrap_or(Timestamp::UNIX_EPOCH),
            organizations: BTreeMap::new(),
            endpoints: BTreeMap::new(),
            outage: None,
            base: format!("{}{BASE}", server.uri()),
        }));
        Mock::given(method("GET"))
            .and(path_regex(r"^/fhir/(Organization|Endpoint)(/_history)?$"))
            .respond_with(Answer(Arc::clone(&state)))
            .mount(&server)
            .await;
        Self { server, state }
    }

    /// The directory's FHIR base URL.
    #[must_use]
    pub fn base(&self) -> String {
        format!("{}{BASE}", self.server.uri())
    }

    /// Puts each member's `Organization` and `Endpoint` in the directory.
    ///
    /// # Errors
    /// [`McsdHarnessError::Example`] when a vendored example cannot be read.
    pub fn publish(&self, members: &[Member]) -> Result<(), McsdHarnessError> {
        for member in members {
            self.put_organization(member.organisation()?);
            self.put_endpoint(member.endpoint()?);
        }
        Ok(())
    }

    /// Puts the IG's own example `Organization`s and `Endpoint`s in the
    /// directory as they are: content of a shared directory that carries none
    /// of the registry's identifiers and is no member.
    ///
    /// # Errors
    /// [`McsdHarnessError::Example`] when a vendored example cannot be read.
    pub fn publish_examples(&self) -> Result<(), McsdHarnessError> {
        for file in [
            "Organization-ex-OrgA.json",
            "Organization-ex-OrgB.json",
            "Organization-ex-OrgC.json",
        ] {
            self.put_organization(example(file)?);
        }
        for file in [
            "Endpoint-ex-endpointDicom.json",
            "Endpoint-ex-endpointXCAquery.json",
            "Endpoint-ex-endpointXCAretrieve.json",
        ] {
            self.put_endpoint(example(file)?);
        }
        Ok(())
    }

    /// Creates or updates an `Organization`, a new version under its logical
    /// id.
    pub fn put_organization(&self, organization: Organization) {
        let mut state = self.state();
        let at = state.tick();
        let id = organization.id.clone().unwrap_or_default();
        state
            .organizations
            .entry(id)
            .or_default()
            .push((at, Some(organization)));
    }

    /// Creates or updates an `Endpoint`, a new version under its logical id.
    pub fn put_endpoint(&self, endpoint: Endpoint) {
        let mut state = self.state();
        let at = state.tick();
        let id = endpoint.id.clone().unwrap_or_default();
        state
            .endpoints
            .entry(id)
            .or_default()
            .push((at, Some(endpoint)));
    }

    /// Deletes the `Endpoint` with `id`.
    pub fn delete_endpoint(&self, id: &str) {
        let mut state = self.state();
        let at = state.tick();
        state
            .endpoints
            .entry(id.to_owned())
            .or_default()
            .push((at, None));
    }

    /// Makes every later request fail as `outage` says, or answer again with
    /// `None`.
    pub fn outage(&self, outage: Option<Outage>) {
        self.state().outage = outage;
    }

    /// The path and query of every request the directory received, in order.
    pub async fn requests(&self) -> Vec<String> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| match request.url.query() {
                Some(query) => format!("{}?{query}", request.url.path()),
                None => request.url.path().to_owned(),
            })
            .collect()
    }

    /// The `Authorization` header of every request the directory received,
    /// in order; `None` for a request that carried none.
    pub async fn authorizations(&self) -> Vec<Option<String>> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|request| {
                request
                    .headers
                    .get(http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            })
            .collect()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for HarnessDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state();
        f.debug_struct("HarnessDirectory")
            .field("organizations", &state.organizations.len())
            .field("endpoints", &state.endpoints.len())
            .field("outage", &state.outage)
            .finish_non_exhaustive()
    }
}

impl State {
    /// Moves the clock one change on and returns the new reading.
    fn tick(&mut self) -> Timestamp {
        self.clock = self.clock.checked_add(TICK).unwrap_or(self.clock);
        self.clock
    }

    /// The answer to a request for `path` with `query`.
    fn answer(&self, path: &str, query: &BTreeMap<String, String>) -> ResponseTemplate {
        match self.outage {
            Some(Outage::Refusing) => return ResponseTemplate::new(503),
            Some(Outage::Silent) => {
                return ResponseTemplate::new(200).set_delay(Duration::from_secs(60));
            }
            None => {}
        }
        let rest = path.trim_start_matches(BASE).trim_start_matches('/');
        let (kind, history) = match rest.split_once('/') {
            Some((kind, "_history")) => (kind, true),
            _ => (rest, false),
        };
        let bundle = if history {
            let Some(since) = query.get("_since").and_then(|since| since.parse().ok()) else {
                return ResponseTemplate::new(400);
            };
            self.history(kind, since)
        } else {
            let system = query
                .get("identifier")
                .and_then(|token| token.strip_suffix('|'));
            self.search(kind, system)
        };
        let date = jiff::fmt::rfc2822::DateTimePrinter::new()
            .timestamp_to_rfc9110_string(&self.clock)
            .unwrap_or_default();
        match serde_json::to_vec(&bundle) {
            Ok(body) => ResponseTemplate::new(200)
                .set_body_raw(body, FHIR_JSON)
                .insert_header("Date", date.as_str()),
            Err(_unencodable) => ResponseTemplate::new(500),
        }
    }

    /// The `searchset` of the current resources of `kind` carrying an
    /// identifier in `system`, or every one when `system` is `None`.
    fn search(&self, kind: &str, system: Option<&str>) -> Bundle {
        let mut entry = Vec::new();
        for (id, resource) in self.current(kind) {
            if system.is_some_and(|system| !identified(&resource, system)) {
                continue;
            }
            entry.push(BundleEntry {
                full_url: Some(format!("{}/{kind}/{id}", self.base).as_str().into()),
                resource: Some(resource),
                search: Some(BundleEntrySearch {
                    mode: Some("match".into()),
                    ..BundleEntrySearch::default()
                }),
                ..BundleEntry::default()
            });
        }
        Bundle {
            r#type: "searchset".into(),
            total: u32::try_from(entry.len()).ok().map(Into::into),
            entry,
            ..Bundle::default()
        }
    }

    /// The `history` of `kind` since `since`, newest first.
    fn history(&self, kind: &str, since: Timestamp) -> Bundle {
        let mut versions: Vec<(Timestamp, String, Option<Resource>)> = Vec::new();
        match kind {
            "Organization" => {
                for (id, history) in &self.organizations {
                    for (at, version) in history {
                        let resource = version
                            .clone()
                            .map(|resource| Resource::Organization(Box::new(resource)));
                        versions.push((*at, id.clone(), resource));
                    }
                }
            }
            _ => {
                for (id, history) in &self.endpoints {
                    for (at, version) in history {
                        let resource = version
                            .clone()
                            .map(|resource| Resource::Endpoint(Box::new(resource)));
                        versions.push((*at, id.clone(), resource));
                    }
                }
            }
        }
        versions.retain(|(at, _, _)| *at >= since);
        versions.sort_by_key(|version| std::cmp::Reverse(version.0));
        let entry = versions
            .into_iter()
            .map(|(_, id, resource)| {
                let url = format!("{kind}/{id}");
                let deleted = resource.is_none();
                BundleEntry {
                    full_url: (!deleted).then(|| format!("{}/{url}", self.base).as_str().into()),
                    request: Some(BundleEntryRequest {
                        method: if deleted { "DELETE" } else { "PUT" }.into(),
                        url: url.as_str().into(),
                        ..BundleEntryRequest::default()
                    }),
                    response: Some(BundleEntryResponse {
                        status: if deleted { "204 No Content" } else { "200 OK" }.into(),
                        ..BundleEntryResponse::default()
                    }),
                    resource,
                    ..BundleEntry::default()
                }
            })
            .collect();
        Bundle {
            r#type: "history".into(),
            entry,
            ..Bundle::default()
        }
    }

    /// The current version of every resource of `kind` that is not deleted.
    fn current(&self, kind: &str) -> Vec<(String, Resource)> {
        match kind {
            "Organization" => self
                .organizations
                .iter()
                .filter_map(|(id, history)| {
                    let (_, last) = history.last()?;
                    let resource = last.clone()?;
                    Some((id.clone(), Resource::Organization(Box::new(resource))))
                })
                .collect(),
            _ => self
                .endpoints
                .iter()
                .filter_map(|(id, history)| {
                    let (_, last) = history.last()?;
                    let resource = last.clone()?;
                    Some((id.clone(), Resource::Endpoint(Box::new(resource))))
                })
                .collect(),
        }
    }
}

/// The responder every request reaches.
struct Answer(Arc<Mutex<State>>);

impl Respond for Answer {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let query: BTreeMap<String, String> = request.url.query_pairs().into_owned().collect();
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .answer(request.url.path(), &query)
    }
}

/// Whether `resource` carries an identifier in `system`.
fn identified(resource: &Resource, system: &str) -> bool {
    let identifiers = match resource {
        Resource::Organization(resource) => &resource.identifier,
        Resource::Endpoint(resource) => &resource.identifier,
        _ => return false,
    };
    identifiers.iter().any(|identifier| {
        identifier
            .system
            .as_ref()
            .and_then(|uri| uri.value.as_deref())
            == Some(system)
    })
}

/// An identifier with `system` and `value`.
fn identifier(system: &str, value: &str) -> Identifier {
    Identifier {
        system: Some(system.into()),
        value: Some(value.into()),
        ..Identifier::default()
    }
}

/// A literal reference.
fn reference(literal: &str) -> Reference {
    Reference {
        reference: Some(literal.into()),
        ..Reference::default()
    }
}

/// The vendored example `file`, decoded.
fn example<T: Json>(file: &'static str) -> Result<T, McsdHarnessError> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "../../docs/specs/ihe-mcsd/package/example",
        file,
    ]
    .iter()
    .collect();
    let refused = || McsdHarnessError::Example { file };
    let text = std::fs::read_to_string(path).map_err(|_unreadable| refused())?;
    let value: Value = serde_json::from_str(&text).map_err(|_not_json| refused())?;
    let object = value.as_object().ok_or_else(refused)?;
    let kind = object
        .get("resourceType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    T::from_json(object, &mut Path::root(&kind)).map_err(|_undecodable| refused())
}

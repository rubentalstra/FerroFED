// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The PIXm resolver and localizer over ITI-83.
//!
//! The [`Resolver`] seam runs against one or more Patient Identifier
//! Cross-reference Managers (N3, §5.2, Annex A.1), and the [`Localizer`] over
//! the same calls is §14.2's "demographic-registration" kind: the members
//! whose domain holds an identifier for the patient.
//!
//! Each member is bound to one PIX Manager and to its `ehr_id` domain there:
//! the assigning authority whose identifier values are that member's
//! `ehr_id`s (Annex A.1, "`targetSystem=<the domain's ehr_id system>`"). The
//! resolver asks each Manager once per query, with one `targetSystem` per
//! member it serves (`targetSystem` is `0..*`), and reads the identifier the
//! answer holds in a member's domain as that member's `ehr_id`.
//!
//! The patient identifier reaches the PIX Manager only, which is the
//! transaction's purpose. It travels inside `ihe_iti`'s redacting
//! [`SourceIdentifier`]; nothing here logs it, and no error carries it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ferrofed_registry::id::{EhrId, NodeId};
use ferrofed_registry::secret::SecretUrl;
use ferrofed_registry::snapshot::RegistrySnapshot;
use http::header::{AUTHORIZATION, HeaderMap};
use ihe_iti::pixm::PixmClient;
use ihe_iti::pixm::error::{InvalidInput, PixmError};
use ihe_iti::pixm::identifier::{CrossReference, SourceIdentifier, TargetSystem};
use openehr_its::rest::client::{Credentials, InvalidCredentials};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use tokio::task::JoinSet;
use url::Url;

use crate::localizer::{Localization, Localizer, LocalizerError};
use crate::patient::{IdentifierNamespace, PatientRef};
use crate::resolver::{Resolution, Resolver, ResolverError};

/// How the gateway authenticates to one PIX Manager (ITI TF-2 Appendix Z.8).
///
/// `Debug` redacts every secret, because [`SecretString`] does.
#[derive(Debug)]
#[non_exhaustive]
pub enum PixAuth {
    /// No `Authorization` header: the transport (mutual TLS, a private
    /// network) authenticates the gateway.
    None,
    /// An RFC 6750 bearer token.
    Bearer(SecretString),
    /// RFC 7617 basic authentication.
    Basic {
        /// The user name, which is not a secret.
        user: String,
        /// The password.
        password: SecretString,
    },
}

/// One PIX Manager as the configuration names it.
#[derive(Debug)]
pub struct ManagerConfig {
    /// The Manager's FHIR base URL, which `Debug` shows without its userinfo.
    pub base: SecretUrl,
    /// How the gateway authenticates to it.
    pub auth: PixAuth,
    /// The members this Manager resolves, each with its `ehr_id` domain: the
    /// assigning authority whose identifiers are that member's `ehr_id`s.
    pub members: BTreeMap<NodeId, String>,
}

/// A PIXm resolver that cannot be built.
///
/// The errors name a member, a domain or a Manager, never an identifier value.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PixmConfigError {
    /// No PIX Manager is configured.
    #[error("the PIXm resolver names no PIX Manager")]
    NoManager,
    /// A Manager names a member the registry does not hold.
    #[error("a PIX Manager names member {0}, which is not in the registry")]
    UnknownMember(NodeId),
    /// Two Managers, or two entries, name one member.
    #[error("member {0} is resolved by more than one PIX Manager")]
    DuplicateMember(NodeId),
    /// A registry member has no PIX Manager, so no patient query could be
    /// scoped there and every one would fail.
    #[error("registry member {0} has no PIX Manager and ehr_id domain")]
    UnresolvedMember(NodeId),
    /// A member's `ehr_id` domain is not an absolute URI.
    #[error("the ehr_id domain of member {member} is not an absolute URI")]
    Domain {
        /// The member.
        member: NodeId,
        /// What the PIXm client reported.
        #[source]
        source: InvalidInput,
    },
    /// A Manager's base URL does not parse as a URL.
    #[error("a PIX Manager base URL is not a URL")]
    BaseUrl(#[source] url::ParseError),
    /// A Manager's base URL is not an `http(s)` URL without a query.
    #[error("a PIX Manager base URL is not an http(s) URL without a query or a fragment")]
    Base(#[source] InvalidInput),
    /// A namespace mapping does not name an absolute URI.
    #[error("the PIX domain mapped from namespace {0} is not an absolute URI")]
    Namespace(IdentifierNamespace),
    /// A credential does not form an `Authorization` value (RFC 7617 §2,
    /// RFC 6750 §2.1).
    #[error("the credentials of a PIX Manager cannot be sent in the Authorization header")]
    Credentials(#[source] InvalidCredentials),
    /// The HTTP client could not be built.
    #[error("the HTTP client for a PIX Manager could not be built")]
    Client(#[source] reqwest::Error),
}

/// Why the PIXm resolver could not answer for a member.
///
/// None of them carries an identifier value: a value the Manager answered
/// with is never repeated, only the fact that it broke a rule.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PixmResolveError {
    /// The patient's issuing namespace maps to no PIX assigning authority.
    #[error("the namespace {0} maps to no PIX assigning authority")]
    UnmappedNamespace(IdentifierNamespace),
    /// The ITI-83 exchange failed.
    #[error("the PIX Manager could not cross-reference the patient")]
    Exchange(#[source] PixmError),
    /// The member's domain holds an identifier that is not an `ehr_id`.
    #[error("the PIX Manager answered an identifier for member {0} that is not an ehr_id")]
    NotAnEhrId(NodeId),
    /// The member's domain holds more than one identifier, so the gateway
    /// cannot choose the `ehr_id` and does not guess.
    #[error("the PIX Manager answered more than one identifier for member {0}")]
    Ambiguous(NodeId),
    /// The PIXm client read an answer this resolver does not know how to
    /// interpret.
    #[error("the PIX Manager answered in a form this resolver does not interpret")]
    UnexpectedAnswer,
}

/// One PIX Manager and the members it resolves.
struct Manager {
    client: PixmClient,
    members: Vec<(NodeId, TargetSystem)>,
}

/// The [`Resolver`] and the [`Localizer`] over ITI-83.
///
/// As a resolver it answers [`Resolution::Resolved`] when a member's domain
/// holds exactly one identifier that reads as an `ehr_id`,
/// [`Resolution::Unknown`] when the Manager does not know the patient or the
/// member's domain holds nothing, and [`Resolution::Unavailable`] for every
/// failure, a namespace it cannot map included, so the query fails closed
/// (§11.3 covers only an answered lookup; no specification governs this: our
/// own design).
///
/// As a localizer it names the members whose domain holds an identifier for
/// the patient, and the resolution of the same query reuses the answers it
/// read, so a query asks each Manager once.
pub struct PixmResolver {
    managers: Vec<Arc<Manager>>,
    namespaces: BTreeMap<IdentifierNamespace, String>,
    shared: Mutex<Vec<Shared>>,
}

impl PixmResolver {
    /// Builds the resolver over `managers`, whose members must cover every
    /// member of `registry` exactly once.
    ///
    /// `namespaces` maps a client's issuing namespace to the PIX assigning
    /// authority it stands for; a namespace that is itself an absolute URI
    /// needs no entry.
    ///
    /// # Errors
    /// A [`PixmConfigError`] for an unknown, doubled or uncovered member, a
    /// base URL that does not parse, a domain or base URL the PIXm client
    /// refuses, a namespace mapping that is not a URI, credentials no header
    /// can carry, or an HTTP client that cannot be built.
    pub fn from_config(
        managers: Vec<ManagerConfig>,
        namespaces: BTreeMap<IdentifierNamespace, String>,
        registry: &RegistrySnapshot,
    ) -> Result<Self, PixmConfigError> {
        if managers.is_empty() {
            return Err(PixmConfigError::NoManager);
        }
        let mut seen: Vec<NodeId> = Vec::new();
        let mut built = Vec::with_capacity(managers.len());
        for manager in managers {
            let mut members = Vec::with_capacity(manager.members.len());
            for (member, domain) in manager.members {
                if registry.node(&member).is_none() {
                    return Err(PixmConfigError::UnknownMember(member));
                }
                if seen.contains(&member) {
                    return Err(PixmConfigError::DuplicateMember(member));
                }
                let target =
                    TargetSystem::new(domain).map_err(|source| PixmConfigError::Domain {
                        member: member.clone(),
                        source,
                    })?;
                seen.push(member.clone());
                members.push((member, target));
            }
            let http = http_client(&manager.auth)?;
            let base = Url::parse(manager.base.expose()).map_err(PixmConfigError::BaseUrl)?;
            let client = PixmClient::new(base, http).map_err(PixmConfigError::Base)?;
            built.push(Arc::new(Manager { client, members }));
        }
        if let Some(uncovered) = registry
            .nodes()
            .map(ferrofed_registry::snapshot::Node::id)
            .find(|id| !seen.contains(id))
        {
            return Err(PixmConfigError::UnresolvedMember(uncovered.clone()));
        }
        for (namespace, system) in &namespaces {
            if TargetSystem::new(system.clone()).is_err() {
                return Err(PixmConfigError::Namespace(namespace.clone()));
            }
        }
        Ok(Self {
            managers: built,
            namespaces,
            shared: Mutex::new(Vec::new()),
        })
    }

    /// The PIX assigning authority `namespace` stands for.
    fn system(&self, namespace: &IdentifierNamespace) -> Option<String> {
        if let Some(system) = self.namespaces.get(namespace) {
            return Some(system.clone());
        }
        // NOTE: no specification governs this: our own design. A namespace
        // that is itself an absolute URI names the assigning authority.
        TargetSystem::new(namespace.as_str())
            .ok()
            .map(|_absolute| namespace.as_str().to_owned())
    }
}

impl fmt::Debug for PixmResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PixmResolver")
            .field("managers", &self.managers.len())
            .field("namespaces", &self.namespaces.len())
            .field(
                "shared",
                &self
                    .shared
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len(),
            )
            .finish()
    }
}

/// The HTTP client one Manager is asked through: no redirects, because the
/// request URL holds the source identifier, and the credentials sent as a
/// sensitive default header, composed as the node client composes them.
fn http_client(auth: &PixAuth) -> Result<reqwest::Client, PixmConfigError> {
    let mut headers = HeaderMap::new();
    let credentials = match auth {
        PixAuth::None => None,
        PixAuth::Bearer(token) => Some(Credentials::bearer(token.clone())),
        PixAuth::Basic { user, password } => {
            Some(Credentials::basic(user.as_str(), password.clone()))
        }
    };
    if let Some(credentials) = credentials {
        let header = credentials
            .header_value()
            .map_err(PixmConfigError::Credentials)?;
        headers.insert(AUTHORIZATION, header);
    }
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(headers)
        .build()
        .map_err(PixmConfigError::Client)
}

/// What one ITI-83 exchange said of one member.
///
/// It is read once per member and becomes the member's [`Resolution`] or,
/// for localization, whether the member's domain holds the patient.
#[derive(Debug)]
enum Lookup {
    /// The domain holds exactly one identifier, which is an `ehr_id`.
    Resolved(EhrId),
    /// The Manager does not know the patient, or the domain holds nothing.
    Unknown,
    /// The domain holds identifiers this resolver cannot use as the member's
    /// `ehr_id`: one that is not an `ehr_id`, or more than one.
    Unusable(PixmResolveError),
    /// The exchange failed.
    Failed(Arc<PixmError>),
    /// The resolution budget ran out before the Manager answered.
    TimedOut,
    /// The Manager answered in a form this resolver does not interpret, or
    /// the patient's namespace maps to no assigning authority.
    Unread(PixmResolveError),
}

impl Lookup {
    /// The member's resolution (N3, N6).
    fn into_resolution(self) -> Resolution {
        match self {
            Self::Resolved(ehr_id) => Resolution::Resolved(ehr_id),
            Self::Unknown => Resolution::Unknown,
            Self::Unusable(error) | Self::Unread(error) => unavailable(error),
            Self::Failed(error) => {
                Resolution::Unavailable(ResolverError::Backend(Box::new(SharedExchange(error))))
            }
            Self::TimedOut => Resolution::Unavailable(ResolverError::DeadlineExceeded),
        }
    }
}

/// What one Manager answered, per member it was asked about.
fn read(
    answer: Result<CrossReference, PixmError>,
    members: &[(NodeId, TargetSystem)],
) -> Vec<(NodeId, Lookup)> {
    match answer {
        Ok(CrossReference::Matched(found)) => members
            .iter()
            .map(|(member, domain)| {
                let mut in_domain = found.in_domain(domain.as_str());
                let lookup = match (in_domain.next(), in_domain.next()) {
                    (None, _) => Lookup::Unknown,
                    (Some(identifier), None) => {
                        match EhrId::new(identifier.value().expose_secret()) {
                            Ok(ehr_id) => Lookup::Resolved(ehr_id),
                            Err(_not_an_ehr_id) => {
                                Lookup::Unusable(PixmResolveError::NotAnEhrId(member.clone()))
                            }
                        }
                    }
                    (Some(_), Some(_)) => {
                        Lookup::Unusable(PixmResolveError::Ambiguous(member.clone()))
                    }
                };
                (member.clone(), lookup)
            })
            .collect(),
        Ok(CrossReference::SourceNotFound) => members
            .iter()
            .map(|(member, _)| (member.clone(), Lookup::Unknown))
            .collect(),
        Ok(_) => members
            .iter()
            .map(|(member, _)| {
                (
                    member.clone(),
                    Lookup::Unread(PixmResolveError::UnexpectedAnswer),
                )
            })
            .collect(),
        Err(PixmError::Timeout) => members
            .iter()
            .map(|(member, _)| (member.clone(), Lookup::TimedOut))
            .collect(),
        Err(error) => {
            let shared = Arc::new(error);
            members
                .iter()
                .map(|(member, _)| (member.clone(), Lookup::Failed(Arc::clone(&shared))))
                .collect()
        }
    }
}

fn unavailable(error: PixmResolveError) -> Resolution {
    Resolution::Unavailable(ResolverError::Backend(Box::new(error)))
}

/// One failed exchange, reported once for every member the Manager serves.
#[derive(Debug)]
struct SharedExchange(Arc<PixmError>);

impl fmt::Display for SharedExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the PIX Manager could not cross-reference the patient")
    }
}

impl std::error::Error for SharedExchange {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// How long a localization's ITI-83 answers wait for the resolution of the
/// same query (no specification governs this: our own design; a query
/// resolves within its overall budget, well inside this window).
const SHARED_WINDOW: Duration = Duration::from_secs(30);

/// The most localizations whose ITI-83 answers are kept at once.
///
/// No specification governs this: our own design. A localization past it
/// keeps nothing, so its resolution asks the Manager again.
pub const SHARED_CAPACITY: usize = 1024;

/// The ITI-83 answers one localization read, kept for the resolution of the
/// same query so that it asks no Manager again (§14.2's
/// "demographic-registration" localizer over the resolver's own call).
///
/// It is held in memory only, for [`SHARED_WINDOW`] at most, and consumed by
/// the resolution that reads it; it is never logged or written anywhere.
struct Shared {
    namespace: IdentifierNamespace,
    value: SecretString,
    until: Instant,
    lookups: BTreeMap<NodeId, Lookup>,
}

impl Shared {
    fn is_for(&self, patient: &PatientRef) -> bool {
        self.namespace == *patient.namespace() && self.value.expose_secret() == patient.value()
    }
}

impl PixmResolver {
    /// Asks every Manager about the members of `members` it serves, within
    /// the time left before `deadline`.
    async fn lookup(
        &self,
        patient: &PatientRef,
        members: &[NodeId],
        deadline: Instant,
    ) -> BTreeMap<NodeId, Lookup> {
        let mut out = BTreeMap::new();
        let unmapped = || {
            Lookup::Unread(PixmResolveError::UnmappedNamespace(
                patient.namespace().clone(),
            ))
        };
        let source = self.system(patient.namespace()).and_then(|system| {
            SourceIdentifier::new(system, SecretString::from(patient.value())).ok()
        });
        let Some(source) = source else {
            for member in members {
                out.insert(member.clone(), unmapped());
            }
            return out;
        };
        let timeout = deadline.saturating_duration_since(Instant::now());
        let mut tasks = JoinSet::new();
        for manager in &self.managers {
            let asked: Vec<(NodeId, TargetSystem)> = manager
                .members
                .iter()
                .filter(|(member, _)| members.contains(member))
                .cloned()
                .collect();
            if asked.is_empty() {
                continue;
            }
            let manager = Arc::clone(manager);
            let source = source.clone();
            tasks.spawn(async move { ask(&manager, &source, asked, timeout).await });
        }
        while let Some(joined) = tasks.join_next().await {
            // NOTE: a task that panicked leaves its members out of the map,
            // which the resolution step reads as no answer and fails closed.
            if let Ok(lookups) = joined {
                out.extend(lookups);
            }
        }
        out
    }

    /// Keeps `lookups` for the resolution of the same query, unless
    /// [`SHARED_CAPACITY`] answers are already kept, and drops every kept
    /// answer whose window has passed.
    fn keep(&self, patient: &PatientRef, lookups: BTreeMap<NodeId, Lookup>) {
        let now = Instant::now();
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        shared.retain(|kept| kept.until > now && !kept.is_for(patient));
        if shared.len() >= SHARED_CAPACITY {
            return;
        }
        shared.push(Shared {
            namespace: patient.namespace().clone(),
            value: SecretString::from(patient.value()),
            until: now.checked_add(SHARED_WINDOW).unwrap_or(now),
            lookups,
        });
    }

    /// Takes the kept answers about `patient` for `members`, leaving none
    /// behind; a member with no kept answer is absent.
    fn take(&self, patient: &PatientRef, members: &[NodeId]) -> BTreeMap<NodeId, Lookup> {
        let now = Instant::now();
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        let found = shared
            .iter()
            .position(|kept| kept.until > now && kept.is_for(patient));
        let mut taken = BTreeMap::new();
        if let Some(index) = found {
            let mut kept = shared.swap_remove(index);
            for member in members {
                if let Some(lookup) = kept.lookups.remove(member) {
                    taken.insert(member.clone(), lookup);
                }
            }
        }
        shared.retain(|kept| kept.until > now);
        taken
    }
}

#[async_trait]
impl Resolver for PixmResolver {
    async fn resolve(
        &self,
        patient: &PatientRef,
        members: &[NodeId],
        deadline: Instant,
    ) -> BTreeMap<NodeId, Resolution> {
        let mut lookups = self.take(patient, members);
        let missing: Vec<NodeId> = members
            .iter()
            .filter(|member| !lookups.contains_key(*member))
            .cloned()
            .collect();
        if !missing.is_empty() {
            lookups.extend(self.lookup(patient, &missing, deadline).await);
        }
        lookups
            .into_iter()
            .map(|(member, lookup)| (member, lookup.into_resolution()))
            .collect()
    }
}

#[async_trait]
impl Localizer for PixmResolver {
    /// Names the members whose `ehr_id` domain holds an identifier for the
    /// patient at its PIX Manager: §14.2's "demographic-registration" kind,
    /// over one ITI-83 call per Manager that the resolution of the same
    /// query reuses.
    ///
    /// A Manager that does not answer, or answers in a form this localizer
    /// cannot read, leaves the localization [`Localization::Unavailable`], so
    /// §14.1 fail-closed applies: a member behind it might hold the patient.
    async fn localize(
        &self,
        patient: &PatientRef,
        members: &[NodeId],
        deadline: Instant,
    ) -> Localization {
        let lookups = self.lookup(patient, members, deadline).await;
        let answered = members.iter().all(|member| lookups.contains_key(member));
        let failing = lookups.values().any(|lookup| {
            matches!(
                lookup,
                Lookup::Failed(_) | Lookup::TimedOut | Lookup::Unread(_)
            )
        });
        if failing || !answered {
            let failure = lookups
                .into_values()
                .find_map(|lookup| match lookup {
                    Lookup::Failed(error) => Some(failed(&error)),
                    Lookup::TimedOut => Some(LocalizerError::DeadlineExceeded),
                    Lookup::Unread(error) => Some(LocalizerError::Backend(Box::new(error))),
                    Lookup::Resolved(_) | Lookup::Unknown | Lookup::Unusable(_) => None,
                })
                .unwrap_or_else(|| LocalizerError::Backend(Box::new(NoAnswer)));
            return Localization::Unavailable(failure);
        }
        let candidates: BTreeSet<NodeId> = lookups
            .iter()
            .filter(|(_, lookup)| matches!(lookup, Lookup::Resolved(_) | Lookup::Unusable(_)))
            .map(|(member, _)| member.clone())
            .collect();
        // NOTE: no specification governs this: our own design; only a
        // localization with candidates is followed by a resolution to keep for.
        if candidates.is_empty() {
            return Localization::NoRecords;
        }
        self.keep(patient, lookups);
        Localization::Candidates(candidates)
    }
}

/// The localizer failure of a failed ITI-83 exchange, with the status the
/// Manager answered when it answered (§2:3.83.4.2.2).
fn failed(error: &Arc<PixmError>) -> LocalizerError {
    let status = match error.as_ref() {
        PixmError::Rejected { status, .. } => Some(*status),
        PixmError::SourceDomainNotRecognized => Some(http::StatusCode::BAD_REQUEST),
        PixmError::TargetDomainNotRecognized => Some(http::StatusCode::FORBIDDEN),
        _ => None,
    };
    let source = Box::new(SharedExchange(Arc::clone(error)));
    match status {
        Some(status) => LocalizerError::Answered { status, source },
        None => LocalizerError::Backend(source),
    }
}

/// A member no Manager's task answered for.
#[derive(Debug, Error)]
#[error("a PIX Manager gave no answer for a member")]
struct NoAnswer;

/// Asks one Manager about `asked`, within `timeout`.
async fn ask(
    manager: &Manager,
    source: &SourceIdentifier,
    asked: Vec<(NodeId, TargetSystem)>,
    timeout: Duration,
) -> Vec<(NodeId, Lookup)> {
    if timeout.is_zero() {
        return asked
            .into_iter()
            .map(|(member, _)| (member, Lookup::TimedOut))
            .collect();
    }
    let targets: Vec<TargetSystem> = asked.iter().map(|(_, domain)| domain.clone()).collect();
    let answer = manager
        .client
        .cross_reference(source, &targets, timeout)
        .await;
    read(answer, &asked)
}

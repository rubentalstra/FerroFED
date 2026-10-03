// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The admission check against mock nodes: each identifier-integrity
//! condition of §12b.2 reported as pass, fail or cannot-check with its
//! evidence, a node that cannot be reached failing and never passing, and the
//! only subjects on the wire being fresh synthetic ones that the report never
//! prints (§12b.1, §12b.2, §5.4.1; N33, N42a).
//!
//! CP-33a is an Operator point (§17), so these tests carry no conformance
//! marker: they test the evidence the gateway hands the operator.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

mod command;
mod generation;
mod onward;
mod overtaken;
mod round_trip;

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use ferrofed_registry::id::EndpointId;
use ferrofed_server::admission::report::{Condition, Report, Verdict};
use ferrofed_server::config::Config;
use ferrofed_server::federation::Federation;
use ferrofed_testkit::mock::Server;
use openehr_base::v1_3::base_types::identification::object_id::ObjectId;
use openehr_its::json::from_canonical_json;
use openehr_rm::v1_2::ehr::ehr_status::EhrStatus;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Match, Mock, Request, Respond, ResponseTemplate};

use crate::facade::{crossref, registry};

type TestResult = Result<(), Box<dyn Error>>;

/// The `system_id`s the registry records for node A and node B.
const SYSTEM_A: &str = "cdr-a.example.org";
const SYSTEM_B: &str = "cdr-b.example.org";

/// Three version-4 UUIDs, as a conformant node mints them.
const V4: [&str; 3] = [
    "0b6f4f1e-6a55-4c2b-9d0e-1f2a3b4c5d6e",
    "1c7a5e2f-7b66-4d3c-8e1f-2a3b4c5d6e7f",
    "2d8b6f3a-8c77-4e4d-9f2a-3b4c5d6e7f80",
];

/// A second base URL no connection reaches, for node B beside an unreachable
/// node A: the registry refuses two endpoints at one URL.
const UNREACHABLE_B: &str = "http://127.0.0.1:0/node-b";

/// The `ehr_id` domain of node A and node B at the PIX Manager.
const DOMAIN_A: &str = "urn:oid:2.999.10";
const DOMAIN_B: &str = "urn:oid:2.999.20";

/// The subject of each test EHR a node created, by the `ehr_id` it issued.
type Issued = Arc<Mutex<BTreeMap<String, String>>>;

/// Returns the subject value of the `EHR_STATUS` a create carries, read
/// through the strict canonical reader, or `None` when the body is not an
/// `EHR_STATUS` whose subject is a `GENERIC_ID`.
fn subject(request: &Request) -> Option<String> {
    let text = std::str::from_utf8(&request.body).ok()?;
    let status: EhrStatus = from_canonical_json(text).ok()?;
    match status.subject.external_ref?.id {
        ObjectId::GenericId(id) => Some(id.value),
        _ => None,
    }
}

/// Matches a create whose body [`subject`] can read.
struct ReadableSubject;

impl Match for ReadableSubject {
    fn matches(&self, request: &Request) -> bool {
        subject(request).is_some()
    }
}

/// A node answering `POST /v1/ehr` with the next `ehr_id` of a list,
/// recording the subject each one was issued for.
struct Minting {
    ids: Mutex<VecDeque<String>>,
    issued: Issued,
}

impl Respond for Minting {
    #[expect(
        clippy::expect_used,
        reason = "the mock is mounted behind ReadableSubject, so every request it answers has a subject"
    )]
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(id) = ids.pop_front() else {
            return ResponseTemplate::new(500);
        };
        ids.push_back(id.clone());
        let subject = subject(request)
            .expect("the ReadableSubject matcher should admit only readable bodies");
        self.issued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), subject);
        ResponseTemplate::new(201)
            .insert_header("ETag", format!("\"{id}\"").as_str())
            .insert_header("Location", format!("{}/{id}", request.url).as_str())
    }
}

/// A node answering `GET /v1/ehr/{ehr_id}` with an `EHR` whose
/// `system_id` is fixed.
struct Reading {
    system_id: String,
}

impl Respond for Reading {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let ehr_id = request
            .url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or_default();
        let system_id = &self.system_id;
        let body = format!(
            r#"{{"_type":"EHR","system_id":{{"_type":"HIER_OBJECT_ID","value":"{system_id}"}},"ehr_id":{{"_type":"HIER_OBJECT_ID","value":"{ehr_id}"}},"ehr_status":{{"_type":"OBJECT_REF","namespace":"local","type":"EHR_STATUS","id":{{"_type":"HIER_OBJECT_ID","value":"7f1e2d3c-4b5a-4968-8776-655443322110"}}}},"ehr_access":{{"_type":"OBJECT_REF","namespace":"local","type":"EHR_ACCESS","id":{{"_type":"HIER_OBJECT_ID","value":"8a2f3e4d-5c6b-4a79-9887-766554433221"}}}},"time_created":{{"_type":"DV_DATE_TIME","value":"2026-10-03T09:00:00Z"}}}}"#
        );
        ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/json")
    }
}

/// A node that issues `ids` in turn and reports `system_id` in every EHR.
///
/// A create whose body is not a readable `EHR_STATUS` falls through to a
/// mock that expects no request, so the node fails its test when dropped.
async fn node(ids: &[&str], system_id: &str) -> (Server, Issued) {
    let server = Server::start().await;
    let issued = Issued::default();
    Mock::given(method("POST"))
        .and(path("/v1/ehr"))
        .and(ReadableSubject)
        .respond_with(Minting {
            ids: Mutex::new(ids.iter().map(|id| (*id).to_owned()).collect()),
            issued: Arc::clone(&issued),
        })
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/ehr"))
        .respond_with(ResponseTemplate::new(400))
        .with_priority(2)
        .expect(0)
        .named("a create whose EHR_STATUS the strict canonical reader refuses")
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v1/ehr/[^/]+$"))
        .respond_with(Reading {
            system_id: system_id.to_owned(),
        })
        .mount(&server)
        .await;
    (server, issued)
}

/// A PIX Manager that maps each subject to the `ehr_id` `issued` holds for
/// it at node A, shifted by `shift` places when it is to answer wrongly.
struct Crossref {
    issued: Issued,
    shift: usize,
}

impl Respond for Crossref {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let subject = request
            .url
            .query_pairs()
            .find(|(key, _)| key == "sourceIdentifier")
            .and_then(|(_, value)| value.split_once('|').map(|(_, id)| id.to_owned()));
        let issued = self.issued.lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<&String> = issued.keys().collect();
        let found = ids
            .iter()
            .position(|id| issued.get(*id) == subject.as_ref())
            .and_then(|at| ids.get((at + self.shift) % ids.len()));
        let body = match found {
            Some(ehr_id) => format!(
                r#"{{"resourceType":"Parameters","parameter":[{{"name":"targetIdentifier","valueIdentifier":{{"system":"{DOMAIN_A}","value":"{ehr_id}"}}}}]}}"#
            ),
            None => r#"{"resourceType":"Parameters"}"#.to_owned(),
        };
        ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/fhir+json")
    }
}

/// A PIX Manager over `issued`, answering wrongly when `shift` is not zero.
async fn manager(issued: &Issued, shift: usize) -> Server {
    let server = Server::start().await;
    Mock::given(method("GET"))
        .and(path("/fhir/Patient/$ihe-pix"))
        .respond_with(Crossref {
            issued: Arc::clone(issued),
            shift,
        })
        .mount(&server)
        .await;
    server
}

/// The `[pixm]` table of one Manager at `pix` serving node A and node B.
fn pixm(pix: &str) -> String {
    format!(
        "[[pixm.manager]]\nurl = \"{pix}/fhir/\"\n\n[pixm.manager.members]\n\"node-a\" = \"{DOMAIN_A}\"\n\"node-b\" = \"{DOMAIN_B}\"\n"
    )
}

/// The configuration text over the registry document `document`.
fn configuration(document: &Path, top: &str, tables: &str) -> String {
    let document = toml::Value::String(document.display().to_string());
    format!(
        "{top}\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{tables}"
    )
}

/// The federation over `registry`, with the top-level keys `top` and the
/// tables `tables`.
fn federation(
    dir: &Path,
    registry: &str,
    top: &str,
    tables: &str,
) -> Result<Federation, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry)?;
    let text = configuration(&document, top, tables);
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    Ok(Federation::load(&settings)?.ok_or("a registry is configured")?)
}

/// A development federation over node A at `a` and node B at `b`, with the
/// static cross-reference, which knows no synthetic subject.
fn dev_federation(dir: &Path, a: &str, b: &str) -> Result<Federation, Box<dyn Error>> {
    federation(
        dir,
        &registry(a, b, ""),
        "profile = \"development\"",
        &crossref(&[("node-a", V4[0])]),
    )
}

/// The admission check of node A's endpoint with three test EHRs.
async fn check_a(federation: &Federation) -> Result<Report, Box<dyn Error>> {
    Ok(ferrofed_server::admission::check(federation, &EndpointId::new("node-a-pub")?, 3).await?)
}

/// The verdict on `condition`.
fn verdict(report: &Report, condition: Condition) -> Result<Verdict, Box<dyn Error>> {
    Ok(report
        .finding(condition)
        .ok_or("every condition has a finding")?
        .verdict())
}

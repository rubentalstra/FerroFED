// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The first federated query end to end, behind the `FERROFED_E2E` gate: one
//! `RESULT_SET` over two FerroEHR nodes, each behind its capturing proxy,
//! and no node request carrying the patient identifier (§7, §9, §11; N1, N5,
//! N7, N16, N17, N33).
//!
//! The gateway resolves the patient through the development cross-reference
//! ([`crossref`]) or the harness PIX Manager ([`pixm`]). Both nodes hold the
//! patient's identifier on `EHR_STATUS.subject`, so a subject predicate the
//! gateway leaked would match there, and the journals show that none reached
//! either node.
//!
//! A commit routed to one node lands there byte-identical, its
//! `DV_IDENTIFIER` included, with the node's `Location` and `ETag` and the
//! acting endpoint's headers on the answer (§7a.3, N22, N31, track 10), and a
//! versioned write reaches only the node that controls its version (§12.4,
//! N23), in [`commit`].
//!
//! A directed query adds each node's ENDPOINT attributes to its rows and
//! sends the directive to no node (§9.4, N12), in [`attributes`].
//!
//! Track 10, the adversarial identifier-leakage suite, runs against the same
//! two nodes in [`track10`], and a plain client given only a prefixed base
//! URL reads and writes through the gateway in [`track9`] (N28, N29).
//!
//! The admission check creates its test EHRs on node A and reads each back,
//! with only synthetic subjects on the wire (§12b.1, §12b.2, N42a), in
//! [`admission`].
//!
//! With the `postgres` feature, the stored-query registry's PostgreSQL store
//! runs its store suite, and two gateway instances sharing it race one new
//! version with exactly one stored (§12.7, N44), in `stored_postgres`.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use ferrofed_server::config::Config;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::containers::{self, ProxiedNode};
use ferrofed_testkit::proxy::Capture;
use ferrofed_testkit::seed::{CompositionSeed, DemoComposition, EhrSeed, PatientId, SeedPlan};
use http::{Request, header};
use serde::Deserialize;
use uuid::Uuid;

use crate::support::settings;

mod admission;
mod attributes;
mod commit;
mod crossref;
mod pixm;
#[cfg(feature = "postgres")]
mod stored_postgres;
mod track10;
mod track9;

type TestResult = Result<(), Box<dyn Error>>;

/// The synthetic patient the gateway resolves, in the example arc.
pub(crate) const PATIENT: PatientId = PatientId::new(1, 38);

/// The patient's `ehr_id` on node A and on node B.
pub(crate) const EHR_A: Uuid = Uuid::from_u128(0x3333_3333_3333_4333_8333_3333_3333_3333);
pub(crate) const EHR_B: Uuid = Uuid::from_u128(0x4444_4444_4444_4444_8444_4444_4444_4444);

/// A seed of the patient's EHR, its subject naming [`PATIENT`], and one
/// composition in it.
pub(crate) fn plan(ehr_id: Uuid, composition: DemoComposition) -> SeedPlan {
    SeedPlan {
        ehrs: vec![EhrSeed {
            ehr_id,
            subject: Some(PATIENT),
        }],
        template: true,
        compositions: vec![CompositionSeed {
            ehr_id,
            composition,
        }],
    }
}

/// The gateway over node A and node B, resolving the patient at both through
/// the development cross-reference.
pub(crate) fn gateway(
    dir: &std::path::Path,
    a: &ProxiedNode,
    b: &ProxiedNode,
) -> Result<axum::Router, Box<dyn Error>> {
    gateway_resolving(dir, a, b, &dev_resolver())
}

/// The development cross-reference resolving [`PATIENT`] to [`EHR_A`] at
/// node A and [`EHR_B`] at node B.
pub(crate) fn dev_resolver() -> String {
    let (namespace, value) = (PATIENT.namespace(), PATIENT.value());
    format!(
        r#"profile = "development"

[[dev.crossref]]
namespace = "{namespace}"
value = "{value}"
member = "node-a"
ehr_id = "{EHR_A}"

[[dev.crossref]]
namespace = "{namespace}"
value = "{value}"
member = "node-b"
ehr_id = "{EHR_B}"
"#
    )
}

/// The gateway over node A and node B, with the resolver `resolver`
/// configures.
fn gateway_resolving(
    dir: &std::path::Path,
    a: &ProxiedNode,
    b: &ProxiedNode,
    resolver: &str,
) -> Result<axum::Router, Box<dyn Error>> {
    gateway_mounted(dir, (a, b), resolver, "/")
}

/// The gateway over node A and node B, with the resolver `resolver`
/// configures, mounted at `base` (N28).
pub(crate) fn gateway_mounted(
    dir: &std::path::Path,
    (a, b): (&ProxiedNode, &ProxiedNode),
    resolver: &str,
    base: &str,
) -> Result<axum::Router, Box<dyn Error>> {
    let federation = federation_resolving(dir, a, b, resolver)?;
    let mut server = settings();
    server.base_path = base.parse()?;
    server.request_timeout = Duration::from_secs(30);
    server.body_limit = 64 * 1024;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &server,
    ))
}

/// The federation over node A and node B, with the resolver `resolver`
/// configures.
pub(crate) fn federation_resolving(
    dir: &std::path::Path,
    a: &ProxiedNode,
    b: &ProxiedNode,
    resolver: &str,
) -> Result<Federation, Box<dyn Error>> {
    let registry = format!(
        r#"
[[organisation]]
id = "org-a"

[[organisation]]
id = "org-b"

[[node]]
id = "node-a"
organisation = "org-a"
system_id = "{}"

[[node]]
id = "node-b"
organisation = "org-b"
system_id = "{}"

[[endpoint]]
id = "node-a-pub"
node = "node-a"
url = "{}"
connection_type = "openehr-rest-query"
managing_organisation = "org-a"

[[endpoint]]
id = "node-b-pub"
node = "node-b"
url = "{}"
connection_type = "openehr-rest-query"
managing_organisation = "org-b"
"#,
        a.node.system_id(),
        b.node.system_id(),
        a.api_root(),
        b.api_root()
    );
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry)?;
    let document = toml::Value::String(document.display().to_string());
    let config = format!(
        "{resolver}\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 20000\noverall_timeout_ms = 25000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
    );
    let settings_ = Config::from_sources(Some(&crate::support::signed(&config)), &BTreeMap::new())?
        .resolve()?;
    Ok(Federation::load(&settings_)?.ok_or("a registry is configured")?)
}

/// Whether the raw body of `capture` holds `needle`'s bytes.
fn body_holds(capture: &Capture, needle: &str) -> bool {
    let needle = needle.as_bytes();
    needle.is_empty()
        || capture
            .body
            .windows(needle.len())
            .any(|window| window == needle)
}

/// `POST /v1/query/aql` with `aql`.
pub(crate) fn query(aql: &str) -> Result<Request<Body>, Box<dyn Error>> {
    #[derive(serde::Serialize)]
    struct Adhoc<'a> {
        q: &'a str,
    }
    Ok(Request::post("/v1/query/aql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&Adhoc { q: aql })?))?)
}

/// The members of the answer the test reads.
#[derive(Debug, Deserialize)]
struct Answer {
    rows: Vec<Vec<String>>,
    meta: Meta,
}

#[derive(Debug, Deserialize)]
struct Meta {
    federation: FederationMeta,
}

#[derive(Debug, Deserialize)]
struct FederationMeta {
    complete: bool,
    endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
struct Endpoint {
    id: String,
    status: String,
    row_count: Option<u64>,
}

/// The patient query for [`PATIENT`], through `external_ref`.
fn patient_query() -> String {
    format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = '{}' \
         AND e/ehr_status/subject/external_ref/namespace = '{}'",
        PATIENT.value(),
        PATIENT.namespace()
    )
}

/// Asserts that, since the journals were last cleared, neither node saw
/// [`PATIENT`]'s identifier or its namespace in any carrier (N33).
pub(crate) fn assert_no_patient_identifier_on_the_wire(nodes: &containers::TwoNodes) {
    for node in [&nodes.a, &nodes.b] {
        for carried in [PATIENT.value(), PATIENT.namespace()] {
            assert!(
                !node.proxy.journal_contains(carried.as_bytes()),
                "{} saw the patient identifier or its namespace (N33)",
                node.node.system_id()
            );
        }
    }
}

/// The vendored hospital composition with `patient`'s own identifier added
/// to its composer as a `DV_IDENTIFIER`, the content track 10's converse
/// check commits.
pub(crate) fn composition_carrying(patient: PatientId) -> Result<String, Box<dyn Error>> {
    let vendored = std::fs::read_to_string(DemoComposition::FirstHospital.path())?;
    let composer = "\"name\": \"Dr. Mark Antonio\"";
    if !vendored.contains(composer) {
        return Err("the vendored composition names its composer".into());
    }
    let identifier = format!(
        "{composer},\n  \"identifiers\": [{{\"_type\": \"DV_IDENTIFIER\", \"issuer\": \"{ns}\", \"assigner\": \"{ns}\", \"id\": \"{id}\", \"type\": \"MR\"}}]",
        ns = patient.namespace(),
        id = patient.value()
    );
    Ok(vendored.replacen(composer, &identifier, 1))
}

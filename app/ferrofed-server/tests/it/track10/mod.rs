// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Track 10, the adversarial identifier-leakage suite (Federation Tier with
//! AQL §16.3 track 10; §5.4; N33, N34; CP-26).
//!
//! "The same directly identifying identifier is supplied four ways in the
//! query surface (`external_ref` predicate, `PARTY_IDENTIFIED`/`DV_IDENTIFIER`
//! predicate, `SELECT` projection, and query string or header), and node-side
//! capture is inspected for any occurrence of the value in the dispatched
//! AQL, path, query string or headers. Zero occurrences is a pass, and so is
//! a `400` rejection." Every [`Case`] here is one of those positions, sent on
//! the AQL fan-out, on a query directed at one node, or on the single-node
//! route of the EHR area, and judged by [`run`] on the journal of the
//! capturing proxy in front of each node, through `ferrofed_testkit::leak`:
//! the identifier, its namespace and its fragments, in every carrier, raw
//! and percent-decoded. A refusal must also have asked nobody (§5.4.1). A
//! request that was dispatched must locate its node by the node's own
//! `ehr_id` alone (N34). The gateway's own log is searched as well, as a
//! separate check and never in place of the capture.
//!
//! The converse check commits a `COMPOSITION` carrying the identifier in a
//! `DV_IDENTIFIER` and compares the body the node received with the body
//! sent, byte for byte and by digest (§5.4 scope note, N22, N33).
//!
//! The cases run against two mock nodes in [`mock`], in the normal suite,
//! and against two FerroEHR nodes behind the `FERROFED_E2E` gate in
//! `crate::e2e::track10`.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

pub(crate) mod cases;
mod mock;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::Config;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_server::telemetry::{Rendering, subscriber};
use ferrofed_testkit::leak::{Carrier, Needles, Sighting};
use ferrofed_testkit::proxy::{Capture, CapturingProxy};
use ferrofed_testkit::seed::PatientId;
use http::{Method, Request, StatusCode};
use uuid::Uuid;

use crate::support::{Logs, request_lines, send, settings};

pub(crate) type TestResult = Result<(), Box<dyn Error>>;

/// The synthetic patient of the track, inside the `urn:oid:2.999` example
/// arc: `ffd-test-0010` in `urn:oid:2.999.1.1`.
pub(crate) const PATIENT: PatientId = PatientId::new(1, 10);

/// The patient's `ehr_id` at node A and at node B.
pub(crate) const EHR_A: Uuid = Uuid::from_u128(0x7a7a_7a7a_7a7a_4a7a_8a7a_7a7a_7a7a_7a7a);
pub(crate) const EHR_B: Uuid = Uuid::from_u128(0x7b7b_7b7b_7b7b_4b7b_8b7b_7b7b_7b7b_7b7b);

/// The endpoint of node A and of node B in the registry.
pub(crate) const ENDPOINT_A: &str = "node-a-pub";
pub(crate) const ENDPOINT_B: &str = "node-b-pub";

/// The needles of [`PATIENT`]: its value, its namespace and its fragments.
pub(crate) fn needles() -> Result<Needles, Box<dyn Error>> {
    Ok(Needles::of(PATIENT)?)
}

/// One node of the topology under test, reached through its proxy.
#[derive(Debug)]
pub(crate) struct Member<'a> {
    /// The endpoint id the registry gives the node.
    pub(crate) endpoint: &'static str,
    /// The proxy every request to the node passes, whose journal is the
    /// oracle.
    pub(crate) proxy: &'a CapturingProxy,
    /// The ITS-REST API root through the proxy, the registry's endpoint URL.
    pub(crate) api_root: String,
    /// The patient's `ehr_id` at the node.
    pub(crate) ehr_id: Uuid,
}

impl Member<'_> {
    /// The path of the API root, the prefix of every path the node receives.
    fn base_path(&self) -> &str {
        self.api_root
            .strip_prefix(self.proxy.origin())
            .unwrap_or_default()
    }
}

/// The two nodes a gateway under test federates.
#[derive(Debug)]
pub(crate) struct Topology<'a> {
    /// Node A.
    pub(crate) a: Member<'a>,
    /// Node B.
    pub(crate) b: Member<'a>,
    /// Whether the nodes are real CDRs, whose answers carry the patient's
    /// rows, rather than mocks answering a fixed row.
    pub(crate) real: bool,
}

impl Topology<'_> {
    /// Forgets every request either proxy received so far.
    pub(crate) fn clear(&self) {
        self.a.proxy.clear_journal();
        self.b.proxy.clear_journal();
    }

    /// The gateway over both nodes, resolving [`PATIENT`] at both through
    /// the development cross-reference, with its registry written into `dir`.
    pub(crate) fn gateway(&self, dir: &Path) -> Result<Router, Box<dyn Error>> {
        let document = dir.join("registry.toml");
        std::fs::write(
            &document,
            crate::facade::registry(&self.a.api_root, &self.b.api_root, ""),
        )?;
        let document = toml::Value::String(document.display().to_string());
        let (namespace, value) = (PATIENT.namespace(), PATIENT.value());
        let mut config = format!(
            "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 20000\noverall_timeout_ms = 25000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
        );
        for (member, ehr_id) in [("node-a", self.a.ehr_id), ("node-b", self.b.ehr_id)] {
            write!(
                config,
                "\n[[dev.crossref]]\nnamespace = \"{namespace}\"\nvalue = \"{value}\"\nmember = \"{member}\"\nehr_id = \"{ehr_id}\"\n"
            )?;
        }
        let resolved =
            Config::from_sources(Some(&crate::support::signed(&config)), &BTreeMap::new())?
                .resolve()?;
        let federation = Federation::load(&resolved)?.ok_or("a registry is configured")?;
        let mut server = settings();
        server.request_timeout = Duration::from_secs(30);
        server.body_limit = 256 * 1024;
        Ok(ferrofed_server::router(
            Arc::new(AppState::with_federation(federation)),
            &server,
        ))
    }
}

/// Which node a case reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    /// Node A.
    A,
    /// Node B.
    B,
}

/// What a case must come to, besides leaving the identifier in no capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expect {
    /// A `200` after one ITS-REST query to each node named, located by its
    /// own `ehr_id` alone (N7, N34).
    Queried(Vec<Side>),
    /// As [`Expect::Queried`], and every row's first column is the
    /// re-injected resolution input, never a value read from a node (N5).
    Reinjected(Vec<Side>),
    /// The node's answer after one request to node A, routed to the EHR
    /// resource its own `ehr_id` names (§7a.1, N34).
    Routed(StatusCode),
    /// This status, and no node asked anything (§5.4.1).
    Unsent(StatusCode),
}

/// One adversarial request and what it must come to.
#[derive(Debug, Clone)]
pub(crate) struct Case {
    /// What the case supplies, and where.
    pub(crate) name: String,
    /// The request method.
    pub(crate) method: Method,
    /// The request target at the gateway.
    pub(crate) uri: String,
    /// The header lines the client sends.
    pub(crate) headers: Vec<(String, String)>,
    /// The request body.
    pub(crate) body: Payload,
    /// The outcome the specification asks for.
    pub(crate) expect: Expect,
}

impl Case {
    /// The request the client sends.
    fn request(&self) -> Result<Request<Body>, Box<dyn Error>> {
        let mut request = Request::builder()
            .method(self.method.clone())
            .uri(self.uri.as_str());
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let bytes = match &self.body {
            Payload::Empty => String::new(),
            Payload::Raw(text) => text.clone(),
            Payload::Query { q, parameters } => serde_json::to_string(&Adhoc {
                q,
                query_parameters: (!parameters.is_empty()).then_some(parameters),
            })?,
        };
        Ok(request.body(Body::from(bytes))?)
    }
}

/// A request body.
#[derive(Debug, Clone)]
pub(crate) enum Payload {
    /// No body.
    Empty,
    /// The ITS-REST ad hoc query `q`, with `parameters` as its
    /// `query_parameters` when there are any.
    Query {
        /// The AQL.
        q: String,
        /// The parameter bindings, by name.
        parameters: BTreeMap<String, String>,
    },
    /// This text, byte for byte.
    Raw(String),
}

/// The ITS-REST `AdhocQueryExecute` body.
#[derive(serde::Serialize)]
struct Adhoc<'a> {
    q: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    query_parameters: Option<&'a BTreeMap<String, String>>,
}

/// Every sighting in `sightings`, one per line, for a failure message.
fn listed(sightings: &[Sighting]) -> String {
    sightings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n  ")
}

/// Whether `haystack` holds `needle`, byte for byte.
fn holds(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Asserts that `journal` is one ITS-REST query that locates `member` by its
/// own `ehr_id` alone (N7, N34): the query path under the node's base, no
/// query string, the node's own `ehr_id` in the AQL, the other node's
/// nowhere, and no patient carrier path left in it (§5.4.1).
fn located_by_ehr_id(
    case: &str,
    member: &Member<'_>,
    other: Uuid,
    journal: &[Capture],
) -> TestResult {
    let [sent] = journal else {
        return Err(format!("{case}: {} is asked once: {journal:?}", member.endpoint).into());
    };
    let body = String::from_utf8_lossy(&sent.body);
    assert_eq!(
        (
            "POST",
            format!("{}/v1/query/aql", member.base_path()).as_str(),
            None
        ),
        (
            sent.method.as_str(),
            sent.path.as_str(),
            sent.query.as_deref()
        ),
        "{case}: the ITS-REST query and nothing else"
    );
    assert!(
        holds(&sent.body, &format!("e/ehr_id/value='{}'", member.ehr_id)),
        "{case}: {} is keyed on its own ehr_id (N7, N34): {body}",
        member.endpoint
    );
    assert!(
        !holds(&sent.body, &other.to_string()),
        "{case}: a node never learns another node's ehr_id: {body}"
    );
    for carrier in [
        "ehr_status/subject",
        "subject/identifiers",
        "subject/external_ref",
    ] {
        assert!(
            !holds(&sent.body, carrier),
            "{case}: no patient carrier reaches {} (§5.4.1): {body}",
            member.endpoint
        );
    }
    Ok(())
}

/// Sends `case` through `app` and judges it on what each node received.
///
/// The identifier, its namespace and its fragments reach no capture in any
/// carrier, whatever the case expects (§5.4.1, N33, CP-26). Then the outcome
/// is the one [`Expect`] names.
pub(crate) async fn run(app: &Router, topology: &Topology<'_>, case: &Case) -> TestResult {
    topology.clear();
    let response = send(app.clone(), case.request()?).await?;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024).await?;
    let text = String::from_utf8_lossy(&bytes);
    let name = case.name.as_str();
    let needles = needles()?;
    for member in [&topology.a, &topology.b] {
        let sightings = needles.in_journal(&member.proxy.journal());
        assert!(
            sightings.is_empty(),
            "{name}: {} received the identifier (§5.4.1, N33, CP-26):\n  {}",
            member.endpoint,
            listed(&sightings)
        );
    }
    let (a, b) = (topology.a.proxy.journal(), topology.b.proxy.journal());
    match &case.expect {
        Expect::Queried(sides) | Expect::Reinjected(sides) => {
            assert_eq!(StatusCode::OK, status, "{name}: {text}");
            for (side, member, other, journal) in [
                (Side::A, &topology.a, topology.b.ehr_id, &a),
                (Side::B, &topology.b, topology.a.ehr_id, &b),
            ] {
                if sides.contains(&side) {
                    located_by_ehr_id(name, member, other, journal)?;
                } else {
                    assert!(
                        journal.is_empty(),
                        "{name}: {} is not asked",
                        member.endpoint
                    );
                }
            }
            if topology.real {
                let answer: crate::facade::Answer = serde_json::from_str(&text)?;
                assert!(
                    !answer.rows.is_empty(),
                    "{name}: the nodes answered on the ehr_id alone (N34): {text}"
                );
            }
            if matches!(case.expect, Expect::Reinjected(_)) {
                let answer: crate::facade::Answer = serde_json::from_str(&text)?;
                let value = PATIENT.value();
                assert!(
                    !answer.rows.is_empty()
                        && answer.rows.iter().all(|row| row.first() == Some(&value)),
                    "{name}: the subject column is the re-injected input (N5): {text}"
                );
            }
        }
        Expect::Routed(expected) => {
            assert_eq!(*expected, status, "{name}: {text}");
            let [sent] = a.as_slice() else {
                return Err(format!("{name}: node A is asked once: {a:?}").into());
            };
            let ehr = format!("{}/v1/ehr/{}", topology.a.base_path(), topology.a.ehr_id);
            assert!(
                (sent.path == ehr || sent.path.starts_with(&format!("{ehr}/")))
                    && sent.query.is_none(),
                "{name}: the node is addressed by its own ehr_id alone (N34): {} {:?}",
                sent.path,
                sent.query
            );
            assert!(b.is_empty(), "{name}: node B is not asked");
        }
        Expect::Unsent(expected) => {
            assert_eq!(*expected, status, "{name}: {text}");
            assert!(
                a.is_empty() && b.is_empty(),
                "{name}: a refused request asks nobody (§5.4.1)"
            );
        }
    }
    Ok(())
}

/// Runs every case of `cases` through `app` under a capturing log at
/// `trace`, then asserts that no log line holds a needle and that every
/// request has its line, so the search is not vacuous.
pub(crate) async fn run_all(app: &Router, topology: &Topology<'_>, cases: &[Case]) -> TestResult {
    let logs = Logs::default();
    let capture = subscriber(Rendering::Json, "trace", false, logs.clone())?;
    let guard = tracing::subscriber::set_default(capture);
    for case in cases {
        run(app, topology, case).await?;
    }
    drop(guard);
    assert_logs_clean(&logs.text(), cases.len())
}

/// Asserts that the gateway's log `text` holds no needle in any line (the
/// request log, the security events, a panic line) and one request line per
/// request sent (§5.4.3).
pub(crate) fn assert_logs_clean(text: &str, requests: usize) -> TestResult {
    let needles = needles()?;
    assert_eq!(
        requests,
        request_lines(text)?.len(),
        "every request was logged, so the search is not vacuous"
    );
    let found = needles.in_text(text);
    assert!(
        found.is_empty(),
        "the gateway's log holds {found:?} (§5.4.3): {text}"
    );
    Ok(())
}

/// The digest a forwarded body is compared by.
pub(crate) fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Commits `composition` to node A through `app` and asserts it arrived
/// byte-identical, its `DV_IDENTIFIER` included, with the identifier in no
/// other carrier of any request and node B never asked (§5.4 scope note,
/// N22, N33, track 10's converse check).
pub(crate) async fn commit_lands_byte_identical(
    app: &Router,
    topology: &Topology<'_>,
    composition: &str,
) -> TestResult {
    let commit = Case {
        name: "a COMPOSITION carrying the identifier in a DV_IDENTIFIER".to_owned(),
        method: Method::POST,
        uri: format!("/v1/ehr/{}/composition", topology.a.ehr_id),
        headers: vec![
            (
                "openEHR-federation-endpoint".to_owned(),
                ENDPOINT_A.to_owned(),
            ),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("prefer".to_owned(), "return=minimal".to_owned()),
        ],
        body: Payload::Raw(composition.to_owned()),
        expect: Expect::Routed(StatusCode::CREATED),
    };
    let logs = Logs::default();
    let capture = subscriber(Rendering::Json, "trace", false, logs.clone())?;
    let guard = tracing::subscriber::set_default(capture);
    topology.clear();
    let response = send(app.clone(), commit.request()?).await?;
    drop(guard);
    let status = response.status();
    let answered = axum::body::to_bytes(response.into_body(), 256 * 1024).await?;
    assert_eq!(
        StatusCode::CREATED,
        status,
        "{}",
        String::from_utf8_lossy(&answered)
    );
    let journal = topology.a.proxy.journal();
    let [landed] = journal.as_slice() else {
        return Err(format!("one commit reached node A: {journal:?}").into());
    };
    assert_eq!(
        format!(
            "{}/v1/ehr/{}/composition",
            topology.a.base_path(),
            topology.a.ehr_id
        ),
        landed.path,
        "the client's path under the node's own base, keyed on its ehr_id (N34)"
    );
    assert_eq!(
        (composition.len(), digest(composition.as_bytes())),
        (landed.body.len(), digest(&landed.body)),
        "the body arrives byte-identical (track 10's converse check)"
    );
    assert_eq!(
        composition.as_bytes(),
        landed.body.as_slice(),
        "byte for byte"
    );
    let needles = needles()?;
    let in_body = needles.in_journal(&journal);
    assert!(
        !in_body.is_empty() && in_body.iter().all(|seen| seen.carrier == Carrier::Body),
        "the identifier rides in the body alone, so the check is not vacuous:\n  {}",
        listed(&in_body)
    );
    let outside = needles.in_journal_outside_bodies(&journal);
    assert!(
        outside.is_empty(),
        "the identifier outside the body (N33):\n  {}",
        listed(&outside)
    );
    assert!(
        topology.b.proxy.journal().is_empty(),
        "node B is never asked"
    );
    assert_logs_clean(&logs.text(), 1)
}

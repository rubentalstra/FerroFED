// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! `POST {base}/v1/query/aql` against two mock nodes, configured through the
//! real configuration path: one federated `RESULT_SET` with the rows of every
//! member that knows the patient, the subject column re-injected, and no node
//! request carrying the patient identifier (§5.4, §7, §9, §11.1; N1, N2, N5,
//! N7, N16, N17, N33).
//!
//! This module holds the fixtures other test modules share: the synthetic
//! patient, the two mock nodes, the registry and the gateway over them, and
//! the typed reads of an answer; `query` holds the tests and [`schema`] the
//! validation against the vendored schemas.

mod query;
mod scan;
pub(crate) mod schema;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_engine::onward::conveyance;
use ferrofed_server::config::Config;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Request, header};
use serde::Deserialize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::support::{MINTED_REQUEST_ID, is_minted_form, searched_claims, settings};

/// The synthetic patient identifier: visibly synthetic, under no real scheme.
pub(crate) const PATIENT: &str = "SENTINEL-PATIENT-38kq";

/// The tail of [`PATIENT`], which a test searches for as a partial echo.
///
/// It holds letters outside hexadecimal, so a minted request id (a version 4
/// UUID, lowercase hexadecimal and hyphens) in a response can never contain it.
pub(crate) const PATIENT_TAIL: &str = "38kq";

/// The synthetic issuing namespace, under the example OID arc.
pub(crate) const NAMESPACE: &str = "urn:oid:2.999.1";

/// The patient's `ehr_id` at node A and at node B.
pub(crate) const EHR_A: &str = "2222aaaa-2222-4222-8222-222222222222";
pub(crate) const EHR_B: &str = "1111bbbb-1111-4111-8111-111111111111";

/// The façade query of §7.2: the patient identified the openEHR way, the
/// identifier selected back, and one composition column.
pub(crate) fn patient_query() -> String {
    format!(
        "SELECT e/ehr_status/subject/external_ref/id/value AS patient, c/uid/value \
         FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = '{PATIENT}' \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}'"
    )
}

/// The ITS-REST `AdhocQueryExecute` body carrying `aql`.
pub(crate) fn body(aql: &str) -> Result<String, serde_json::Error> {
    #[derive(serde::Serialize)]
    struct Adhoc<'a> {
        q: &'a str,
    }
    serde_json::to_string(&Adhoc { q: aql })
}

/// A node answering `POST /v1/query/aql` with one row holding `uid`.
pub(crate) async fn node_answering(uid: &str) -> Server {
    let server = Server::start().await;
    let answer = format!(
        r##"{{"q":"node","columns":[{{"name":"#0","path":"c/uid/value"}}],"rows":[["{uid}"]]}}"##
    );
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(answer.into_bytes(), "application/json"),
        )
        .mount(&server)
        .await;
    server
}

/// A node answering `POST /v1/query/aql` with `status` and an ITS-REST error.
pub(crate) async fn node_failing(status: u16) -> Server {
    let server = Server::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(ResponseTemplate::new(status).set_body_raw(
            br#"{"message":"synthetic node failure"}"#.to_vec(),
            "application/json",
        ))
        .mount(&server)
        .await;
    server
}

/// The registry document of node A and node B at `a` and `b`, with
/// `extra` appended.
pub(crate) fn registry(a: &str, b: &str, extra: &str) -> String {
    format!(
        r#"
[[organisation]]
id = "org-a"

[[organisation]]
id = "org-b"

[[node]]
id = "node-a"
organisation = "org-a"
system_id = "cdr-a.example.org"

[[node]]
id = "node-b"
organisation = "org-b"
system_id = "cdr-b.example.org"

[[endpoint]]
id = "node-a-pub"
node = "node-a"
url = "{a}"
connection_type = "openehr-rest-query"
managing_organisation = "org-a"

[[endpoint]]
id = "node-b-pub"
node = "node-b"
url = "{b}"
connection_type = "openehr-rest-query"
managing_organisation = "org-b"
{extra}"#
    )
}

/// The `[dev]` rows mapping the patient to `rows`, each `(member, ehr_id)`.
pub(crate) fn crossref(rows: &[(&str, &str)]) -> String {
    rows.iter().fold(String::new(), |mut text, (member, ehr_id)| {
        // NOTE: writing to a String cannot fail, so the result is dropped.
        let _written: std::fmt::Result = write!(
            text,
            "\n[[dev.crossref]]\nnamespace = \"{NAMESPACE}\"\nvalue = \"{PATIENT}\"\nmember = \"{member}\"\nehr_id = \"{ehr_id}\"\n"
        );
        text
    })
}

/// The per-node timeout [`gateway`] configures, in milliseconds.
pub(crate) const PER_NODE_TIMEOUT_MS: u64 = 2_000;

/// The gateway configured by the top-level keys `top` and the tables
/// `tables`, with the registry document `registry` written into `dir`.
pub(crate) fn gateway(
    dir: &Path,
    registry: &str,
    top: &str,
    tables: &str,
) -> Result<Router, Box<dyn Error>> {
    gateway_within(dir, registry, top, tables, (PER_NODE_TIMEOUT_MS, 3_000))
}

/// The gateway of [`gateway`], with a per-node timeout of `per_node_ms` and
/// an overall budget of `overall_ms`.
pub(crate) fn gateway_within(
    dir: &Path,
    registry: &str,
    top: &str,
    tables: &str,
    (per_node_ms, overall_ms): (u64, u64),
) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry)?;
    let document = toml::Value::String(document.display().to_string());
    let text = format!(
        "{top}\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = {per_node_ms}\noverall_timeout_ms = {overall_ms}\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{tables}"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// The middleware settings, with a request timeout past the fan-out budget.
pub(crate) fn settings_with_room() -> ferrofed_server::config::settings::ServerSettings {
    let mut server = settings();
    server.request_timeout = std::time::Duration::from_secs(10);
    server.body_limit = 64 * 1024;
    server
}

/// A development gateway over node A and node B at `a` and `b`, resolving the
/// patient at the members `rows` name.
pub(crate) fn dev_gateway(
    dir: &Path,
    a: &str,
    b: &str,
    rows: &[(&str, &str)],
) -> Result<Router, Box<dyn Error>> {
    gateway(
        dir,
        &registry(a, b, ""),
        "profile = \"development\"",
        &crossref(rows),
    )
}

/// `POST /v1/query/aql` with `body`.
pub(crate) fn post(body: String) -> Result<Request<Body>, http::Error> {
    Request::post("/v1/query/aql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
}

/// The bodies of every request `server` received.
pub(crate) async fn received(server: &Server) -> Result<Vec<String>, Box<dyn Error>> {
    let requests = server.received_requests().await.ok_or("recording is on")?;
    let mut bodies = Vec::new();
    for request in requests {
        bodies.push(String::from_utf8(request.body.clone())?);
    }
    Ok(bodies)
}

/// Every byte a mock server received, kept as bytes so a search sees a header
/// value or a body that is not UTF-8 too.
#[derive(Debug)]
pub(crate) struct Wire(Vec<u8>);

impl Wire {
    /// Whether the received bytes hold `needle`'s bytes anywhere.
    pub(crate) fn contains(&self, needle: &str) -> bool {
        let needle = needle.as_bytes();
        needle.is_empty() || self.0.windows(needle.len()).any(|window| window == needle)
    }

    /// Whether `needle` occurs in the received bytes, ASCII case ignored.
    pub(crate) fn contains_ignoring_ascii_case(&self, needle: &str) -> bool {
        let needle = needle.as_bytes();
        needle.is_empty()
            || self
                .0
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle))
    }

    /// Whether nothing was received.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for Wire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

/// What a search reads in place of the name of the gateway's own
/// `openEHR-federation-client` header, before the claims its token carries.
pub(crate) const CONVEYED: &str = "<conveyed-claims>";

/// Every byte `server` received: the request target, each header name and
/// raw value, and the raw body.
///
/// Two values are recorded otherwise. An `x-request-id` in the form the
/// gateway mints is recorded as [`MINTED_REQUEST_ID`]: a random UUID can
/// hold a short synthetic identifier by chance. The gateway's
/// `openEHR-federation-client` token is recorded as [`CONVEYED`] and the
/// claims a node decodes from it, so a search reads every claim value and
/// the name never reads as a federation request header a client sent. Every
/// other value stays raw, so a client value reaching a node is still
/// searched.
pub(crate) async fn wire(server: &Server) -> Result<Wire, Box<dyn Error>> {
    let requests = server.received_requests().await.ok_or("recording is on")?;
    let mut bytes = Vec::new();
    for request in requests {
        bytes.extend_from_slice(request.url.as_str().as_bytes());
        for (name, value) in &request.headers {
            if name.as_str().eq_ignore_ascii_case(conveyance::HEADER) {
                bytes.extend_from_slice(CONVEYED.as_bytes());
                bytes.extend_from_slice(searched_claims(value.to_str()?)?.as_bytes());
                continue;
            }
            bytes.extend_from_slice(name.as_str().as_bytes());
            if minted(name, value) {
                bytes.extend_from_slice(MINTED_REQUEST_ID.as_bytes());
            } else {
                bytes.extend_from_slice(value.as_bytes());
            }
        }
        bytes.extend_from_slice(&request.body);
    }
    Ok(Wire(bytes))
}

/// Whether `name` and `value` are the `x-request-id` the gateway mints.
fn minted(name: &http::HeaderName, value: &http::HeaderValue) -> bool {
    // NOTE: §5.4.1, N33; exempting the minted id is our own design: the outbound
    // gate skips that one value, so the scan skips its form and no other value.
    name == "x-request-id" && value.to_str().is_ok_and(is_minted_form)
}

/// The federated answer, read for the members the tests assert on.
#[derive(Debug, Deserialize)]
pub(crate) struct Answer {
    pub(crate) q: String,
    columns: Vec<Column>,
    pub(crate) rows: Vec<Vec<String>>,
    pub(crate) meta: Meta,
}

impl Answer {
    /// The `path` of every column of `columns[]`, in order.
    pub(crate) fn column_paths(&self) -> Vec<Option<&str>> {
        self.columns
            .iter()
            .map(|column| column.path.as_deref())
            .collect()
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Column {
    name: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Meta {
    pub(crate) federation: FederationMeta,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FederationMeta {
    pub(crate) complete: bool,
    pub(crate) endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Endpoint {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) row_count: Option<u64>,
    pub(crate) error: Option<openehr_federation::outcome::ErrorDetail>,
}

/// Each endpoint's status, in the envelope's order.
pub(crate) fn statuses(answer: &Answer) -> Vec<(&str, &str)> {
    answer
        .meta
        .federation
        .endpoints
        .iter()
        .map(|endpoint| (endpoint.id.as_str(), endpoint.status.as_str()))
        .collect()
}

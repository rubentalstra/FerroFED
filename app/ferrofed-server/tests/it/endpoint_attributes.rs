// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! ENDPOINT attributes in rows through `POST {base}/v1/query/aql`, against
//! mock nodes (§9.2, §9.3, §9.4; N12, N13, N17, N18, N33; CP-35, CP-37).
//!
//! A directed query that selects an attribute through the directive's
//! variable gets its value in every row, from the registry entry of the
//! endpoint the row came from; no node is asked for it, and a query that
//! selects none keeps the row shape of a single CDR.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use ferrofed_server::config::Config;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::StatusCode;
use serde::Deserialize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{NAMESPACE, body, post, received, schema, settings_with_room, wire};
use crate::support::{call, error_body};

type TestResult = Result<(), Box<dyn Error>>;

/// The §9.4 example query, as the specification writes it.
const EXAMPLE: &str = r#"SELECT p/id AS endpoint_id, p/system_id AS system_id, c/uid/value AS composition_id FROM ENDPOINT p ["node_1","node_2"] CONTAINS EHR e CONTAINS COMPOSITION c WHERE e/ehr_status/subject/external_ref/id/value = '12345'"#;

/// The composition ids of the §9.4 rows.
const UID_1: &str = "8849182a-1d4b-4e3d-a3f3-f303d2f4f34b::cdr1.rso.nl::1";
const UID_2: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8::cdr2.rso.nl::1";

/// The patient's `ehr_id` at node 1 and at node 2, synthetic.
const EHR_1: &str = "5555eeee-5555-4555-8555-555555555555";
const EHR_2: &str = "6666ffff-6666-4666-8666-666666666666";

/// A node answering `POST /v1/query/aql` with `rows` of string cells.
async fn node(rows: &[&[&str]]) -> Server {
    let rows: Vec<String> = rows
        .iter()
        .map(|cells| {
            let cells: Vec<String> = cells.iter().map(|cell| format!("\"{cell}\"")).collect();
            format!("[{}]", cells.join(","))
        })
        .collect();
    let answer = format!(r#"{{"q":"node","rows":[{}]}}"#, rows.join(","));
    let server = Server::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(answer.into_bytes(), "application/json"),
        )
        .mount(&server)
        .await;
    server
}

/// The §9.4 members: `node_1` of Org A at `one`, `node_2` of Org B at `two`,
/// and `node_3` of Org C at `three`, which the example does not direct at.
fn members(one: &str, two: &str, three: &str, second_organisation: &str) -> String {
    format!(
        r#"
[[organisation]]
id = "org-a"

[[organisation]]
id = "org-b"

[[organisation]]
id = "org-c"

[[node]]
id = "node_1"
organisation = "org-a"
system_id = "cdr1.rso.nl"

[[node]]
id = "node_2"
organisation = "{second_organisation}"
system_id = "cdr2.rso.nl"

[[node]]
id = "node_3"
organisation = "org-c"
system_id = "cdr3.rso.nl"

[[endpoint]]
id = "node_1"
node = "node_1"
url = "{one}"
connection_type = "openehr-rest-query"
managing_organisation = "org-a"

[[endpoint]]
id = "node_2"
node = "node_2"
url = "{two}"
connection_type = "openehr-rest-query"
managing_organisation = "{second_organisation}"

[[endpoint]]
id = "node_3"
node = "node_3"
url = "{three}"
connection_type = "openehr-rest-query"
managing_organisation = "org-c"
"#
    )
}

/// The three mock nodes, each answering `rows`.
struct Nodes {
    one: Server,
    two: Server,
    three: Server,
}

impl Nodes {
    async fn answering(one: &[&[&str]], two: &[&[&str]]) -> Self {
        Self {
            one: node(one).await,
            two: node(two).await,
            three: node(&[]).await,
        }
    }

    /// A development gateway over the three nodes, `node_2` managed by
    /// `second_organisation`, resolving the patient `12345` at node 1 and 2
    /// in the default namespace.
    fn gateway(&self, dir: &Path, second_organisation: &str) -> Result<Router, Box<dyn Error>> {
        let document = dir.join("registry.toml");
        std::fs::write(
            &document,
            members(
                &self.one.uri(),
                &self.two.uri(),
                &self.three.uri(),
                second_organisation,
            ),
        )?;
        let document = toml::Value::String(document.display().to_string());
        let mut text = format!(
            "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\ndefault_namespace = \"{NAMESPACE}\"\n"
        );
        for (member, ehr_id) in [("node_1", EHR_1), ("node_2", EHR_2)] {
            write!(
                text,
                "\n[[dev.crossref]]\nnamespace = \"{NAMESPACE}\"\nvalue = \"12345\"\nmember = \"{member}\"\nehr_id = \"{ehr_id}\"\n"
            )?;
        }
        let settings =
            Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?
                .resolve()?;
        let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
        Ok(ferrofed_server::router(
            Arc::new(AppState::with_federation(federation)),
            &settings_with_room(),
        ))
    }

    /// How many requests node 1, node 2 and node 3 received.
    async fn asked(&self) -> Result<[usize; 3], Box<dyn Error>> {
        Ok([
            received(&self.one).await?.len(),
            received(&self.two).await?.len(),
            received(&self.three).await?.len(),
        ])
    }
}

/// The members of the answer the tests assert on.
#[derive(Debug, Deserialize)]
struct Answer {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    meta: Meta,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Column {
    name: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Meta {
    federation: FederationRecord,
}

#[derive(Debug, Deserialize)]
struct FederationRecord {
    complete: bool,
    endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
struct Endpoint {
    id: String,
    status: String,
    system_id: Option<String>,
    url: Option<String>,
}

/// The `(name, path)` of every column of `columns[]`.
fn columns(answer: &Answer) -> Vec<(&str, Option<&str>)> {
    answer
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column.path.as_deref()))
        .collect()
}

/// `columns`, selected `FROM ENDPOINT p` over node 1 and node 2.
fn selecting(columns: &str) -> String {
    format!(
        r#"SELECT {columns} FROM ENDPOINT p ["node_1", "node_2"] CONTAINS EHR e CONTAINS COMPOSITION c WHERE e/ehr_status/subject/external_ref/id/value = '12345'"#
    )
}

/// The rows, as strings, of `query` answered by `nodes`, after asserting a
/// `200` with a schema-valid envelope.
async fn rows_of(app: Router, query: &str) -> Result<Answer, Box<dyn Error>> {
    let (status, text) = call(app, post(body(query)?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    schema::validate(&text)?;
    Ok(serde_json::from_str(&text)?)
}

// conformance: CP-35 CP-37
#[tokio::test]
async fn the_example_query_answers_the_example_columns_and_rows() -> TestResult {
    let nodes = Nodes::answering(&[&[UID_1]], &[&[UID_2]]).await;
    let dir = tempfile::tempdir()?;
    let answer = rows_of(nodes.gateway(dir.path(), "org-b")?, EXAMPLE).await?;
    assert_eq!(
        vec![
            ("endpoint_id", Some("/id")),
            ("system_id", Some("/system_id")),
            ("composition_id", Some("/uid/value")),
        ],
        columns(&answer),
        "§9.4 names, in the gateway's ITS-REST path rendering (§9.2)"
    );
    assert_eq!(
        vec![
            vec![
                "node_1".to_owned(),
                "cdr1.rso.nl".to_owned(),
                UID_1.to_owned()
            ],
            vec![
                "node_2".to_owned(),
                "cdr2.rso.nl".to_owned(),
                UID_2.to_owned()
            ],
        ],
        answer.rows,
        "§9.4, N12: each row carries its own endpoint's attributes"
    );
    let endpoints: Vec<(&str, &str, Option<&str>)> = answer
        .meta
        .federation
        .endpoints
        .iter()
        .map(|endpoint| {
            (
                endpoint.id.as_str(),
                endpoint.status.as_str(),
                endpoint.system_id.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        vec![
            ("node_1", "active", Some("cdr1.rso.nl")),
            ("node_2", "active", Some("cdr2.rso.nl")),
            ("node_3", "excluded", Some("cdr3.rso.nl")),
        ],
        endpoints,
        "§9.4: node_3 is reported excluded"
    );
    assert!(answer.meta.federation.complete, "§9.4: complete is true");
    assert_eq!([1, 1, 0], nodes.asked().await?);
    for node in [&nodes.one, &nodes.two] {
        let captured = wire(node).await?;
        for absent in ["ENDPOINT", "p/", "system_id", "node_1", "node_2", "12345"] {
            assert!(
                !captured.contains_ignoring_ascii_case(absent),
                "§8.1, N33: a node received {absent:?}: {captured}"
            );
        }
    }
    Ok(())
}

// conformance: CP-37
#[tokio::test]
async fn every_attribute_reads_the_registry_entry_of_its_endpoint() -> TestResult {
    let nodes = Nodes::answering(&[&[UID_1]], &[&[UID_2]]).await;
    let dir = tempfile::tempdir()?;
    let query = selecting(
        "p/endpoint_id AS e_id, p/organisation AS org, p/organization_id AS org_id, p/url AS url, c/uid/value AS composition_id",
    );
    let answer = rows_of(nodes.gateway(dir.path(), "org-b")?, &query).await?;
    let url = |id: &str| {
        answer
            .meta
            .federation
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == id)
            .and_then(|endpoint| endpoint.url.clone())
            .unwrap_or_default()
    };
    assert_eq!(
        vec![
            vec![
                "node_1".to_owned(),
                "org-a".to_owned(),
                "org-a".to_owned(),
                url("node_1"),
                UID_1.to_owned(),
            ],
            vec![
                "node_2".to_owned(),
                "org-b".to_owned(),
                "org-b".to_owned(),
                url("node_2"),
                UID_2.to_owned(),
            ],
        ],
        answer.rows,
        "§9.3, §9.5: endpoint_id, the managing organisation and the base URL, as meta reports them"
    );
    Ok(())
}

// conformance: CP-35 CP-37
#[tokio::test]
async fn a_query_selecting_no_attribute_keeps_the_single_cdr_row_shape() -> TestResult {
    let nodes = Nodes::answering(&[&[UID_1]], &[&[UID_2]]).await;
    let dir = tempfile::tempdir()?;
    let query = selecting("c/uid/value AS composition_id");
    let answer = rows_of(nodes.gateway(dir.path(), "org-b")?, &query).await?;
    assert_eq!(
        vec![("composition_id", Some("/uid/value"))],
        columns(&answer)
    );
    assert_eq!(
        vec![vec![UID_1.to_owned()], vec![UID_2.to_owned()]],
        answer.rows,
        "N17: no endpoint column unless one is selected"
    );
    Ok(())
}

// conformance: CP-35
#[tokio::test]
async fn an_alias_resolves_a_collision_and_both_columns_survive() -> TestResult {
    let nodes = Nodes::answering(&[&["ehr-system-at-1"]], &[&["ehr-system-at-2"]]).await;
    let dir = tempfile::tempdir()?;
    let query = selecting("p/system_id AS node_system, e/system_id/value AS system_id");
    let answer = rows_of(nodes.gateway(dir.path(), "org-b")?, &query).await?;
    assert_eq!(
        vec![
            ("node_system", Some("/system_id")),
            ("system_id", Some("/system_id/value")),
        ],
        columns(&answer)
    );
    assert_eq!(
        vec![
            vec!["cdr1.rso.nl".to_owned(), "ehr-system-at-1".to_owned()],
            vec!["cdr2.rso.nl".to_owned(), "ehr-system-at-2".to_owned()],
        ],
        answer.rows,
        "N18, CP-35: the registry's value and the node's both survive"
    );
    Ok(())
}

// conformance: CP-35
#[tokio::test]
async fn an_alias_that_leaves_the_collision_standing_is_refused_400() -> TestResult {
    let nodes = Nodes::answering(&[&[UID_1]], &[&[UID_2]]).await;
    let dir = tempfile::tempdir()?;
    let query = selecting("p/system_id AS system_id, e/system_id/value AS system_id");
    let (status, text) = call(nodes.gateway(dir.path(), "org-b")?, post(body(&query)?)?).await?;
    assert_eq!(StatusCode::BAD_REQUEST, status, "{text}");
    assert_eq!("endpoint-name-collision", error_body(&text)?.code);
    assert!(!text.contains("12345"), "§5.4.3: {text}");
    assert_eq!([0, 0, 0], nodes.asked().await?, "nothing is dispatched");
    Ok(())
}

// conformance: CP-8 CP-37
#[tokio::test]
async fn under_distinct_an_endpoint_id_keeps_the_rows_of_two_endpoints_apart() -> TestResult {
    let nodes = Nodes::answering(&[&["alpha"]], &[&["alpha"]]).await;
    let dir = tempfile::tempdir()?;
    let query = selecting("DISTINCT p/id AS endpoint_id, c/name/value AS name");
    let answer = rows_of(nodes.gateway(dir.path(), "org-a")?, &query).await?;
    assert_eq!(
        vec![
            vec!["node_1".to_owned(), "alpha".to_owned()],
            vec!["node_2".to_owned(), "alpha".to_owned()],
        ],
        answer.rows,
        "N13: the rows differ in their endpoint id"
    );
    for node in [&nodes.one, &nodes.two] {
        let sent = received(node).await?;
        assert!(
            sent.iter()
                .all(|sent| sent.contains("SELECT DISTINCT c/name/value AS name FROM")),
            "the node is asked its own column only: {sent:?}"
        );
    }
    Ok(())
}

// conformance: CP-8 CP-37
#[tokio::test]
async fn under_distinct_a_shared_organisation_collapses_the_rows() -> TestResult {
    let nodes = Nodes::answering(&[&["alpha"]], &[&["alpha"]]).await;
    let dir = tempfile::tempdir()?;
    let query = selecting("DISTINCT p/organisation AS org, c/name/value AS name");
    let answer = rows_of(nodes.gateway(dir.path(), "org-a")?, &query).await?;
    assert_eq!(
        vec![vec!["org-a".to_owned(), "alpha".to_owned()]],
        answer.rows,
        "N13: one organisation manages both endpoints, so the rows are one value"
    );
    Ok(())
}

// conformance: CP-37
#[tokio::test]
async fn ordering_on_an_attribute_is_refused_400_before_any_node_is_asked() -> TestResult {
    let nodes = Nodes::answering(&[&[UID_1]], &[&[UID_2]]).await;
    let dir = tempfile::tempdir()?;
    for tail in [" ORDER BY p/id", " ORDER BY p/system_id LIMIT 1"] {
        let query = selecting("p/id AS endpoint_id, c/uid/value") + tail;
        let (status, text) =
            call(nodes.gateway(dir.path(), "org-b")?, post(body(&query)?)?).await?;
        assert_eq!(StatusCode::BAD_REQUEST, status, "{tail}: {text}");
        assert_eq!("endpoint-variable", error_body(&text)?.code, "{tail}");
    }
    assert_eq!([0, 0, 0], nodes.asked().await?);
    Ok(())
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The full per-endpoint report: every registry member appears in
//! `meta.federation.endpoints[]`, in scope with its outcome or out of scope
//! with why, carrying exactly the members its status requires and allows
//! (§9.5, §11.1, N16, N40, CP-11, CP-31).
//!
//! In process: three mock CDRs, the plan the façade builds for a directed
//! query over two of them, and the fan-out; every envelope is validated
//! against the vendored result-set schema.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]
#![expect(
    clippy::disallowed_types,
    reason = "the test seam: the tests read the emitted envelope as JSON values"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::Write as _;
use std::time::Duration;

use ferrofed_engine::dispatch::NodeClients;
use ferrofed_engine::fanout::{Budget, FederatedAnswer, fan_out};
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_server::facade::plan::{self, Selection};
use ferrofed_testkit::mock::Server;
use http::StatusCode;
use openehr_federation::aql::{Analysis, Context, Paging, Targeting, analyse};
use openehr_federation::status::EndpointStatus;
use openehr_its::rest::client::ReqwestTransport;
use openehr_query::bind::Parameters;
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::schema;

type TestResult = Result<(), Box<dyn Error>>;

/// The façade query: no patient, so the node set is the request's to name.
const FACADE_AQL: &str = "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c";

/// A mock CDR answering `POST /v1/query/aql` with one row per uid.
async fn node(uids: &[&str]) -> Server {
    let rows: Vec<String> = uids.iter().map(|uid| format!("[\"{uid}\"]")).collect();
    let body = format!(
        r##"{{"q":"{FACADE_AQL}","columns":[{{"name":"#0","path":"c/uid/value"}}],"rows":[{}]}}"##,
        rows.join(",")
    );
    let server = Server::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/json"),
        )
        .mount(&server)
        .await;
    server
}

/// A registry of the members `a`, `b` and `c` at `urls`; member `a` records
/// its CDR product and version, `b` and `c` record neither.
fn registry(urls: [&str; 3]) -> Result<RegistrySnapshot, Box<dyn Error>> {
    let mut text = String::from("[[organisation]]\nid = \"org-1\"\n");
    for (member, url) in ["a", "b", "c"].into_iter().zip(urls) {
        let described = if member == "a" {
            "product = \"FerroEHR\"\nversion = \"4.3.1\"\n"
        } else {
            ""
        };
        write!(
            text,
            "\n[[node]]\nid = \"node-{member}\"\norganisation = \"org-1\"\nsystem_id = \"cdr-{member}.example.org\"\n{described}\n[[endpoint]]\nid = \"node-{member}-pub\"\nnode = \"node-{member}\"\nurl = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-1\"\n"
        )?;
    }
    Ok(RegistrySnapshot::from_toml_str(&text)?)
}

/// The directed query over `named`, planned and fanned out over `snapshot`.
async fn directed(
    snapshot: &RegistrySnapshot,
    named: &[&str],
) -> Result<FederatedAnswer, Box<dyn Error>> {
    let endpoints = std::num::NonZeroUsize::new(named.len()).ok_or("name an endpoint")?;
    let context = Context::new(Targeting::Directed { endpoints });
    let Analysis::Unscoped(query) =
        analyse(FACADE_AQL, &Parameters::new(), Paging::default(), &context)?
    else {
        return Err("the façade query names no patient".into());
    };
    let named: BTreeSet<EndpointId> = named
        .iter()
        .map(|id| EndpointId::new(*id))
        .collect::<Result<_, _>>()?;
    let targets = plan::unscoped(snapshot, Selection::Directed(&named), &query)?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(10))?;
    let clients = NodeClients::from_snapshot(snapshot, &transport, &BTreeMap::new())?;
    let budget = Budget::new(Duration::from_secs(5), Duration::from_secs(8))?;
    let conveyance = crate::support::conveyance()?;
    Ok(fan_out(
        &clients,
        snapshot,
        targets.plan,
        budget,
        (&conveyance, None),
    )
    .await?)
}

/// The answer as the federated `RESULT_SET` body, validated against the
/// vendored schema, read back as JSON.
fn envelope(answer: FederatedAnswer) -> Result<Value, Box<dyn Error>> {
    let body = answer.into_result_set(Some(FACADE_AQL.to_owned()), None)?;
    let text = serde_json::to_string(&body)?;
    schema::validate(&text)?;
    Ok(serde_json::from_str(&text)?)
}

/// The `meta.federation.endpoints[]` entry of `id`.
fn entry<'a>(envelope: &'a Value, id: &str) -> Result<&'a Value, Box<dyn Error>> {
    envelope
        .pointer("/meta/federation/endpoints")
        .and_then(Value::as_array)
        .ok_or("no endpoints[]")?
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("{id} is not reported").into())
}

// conformance: CP-11
#[tokio::test]
async fn a_member_a_directed_query_did_not_name_is_reported_excluded_and_out_of_scope() -> TestResult
{
    let (a, b, c) = (
        node(&["a::1"]).await,
        node(&["b::1"]).await,
        node(&["c::1"]).await,
    );
    let snapshot = registry([&a.uri(), &b.uri(), &c.uri()])?;
    let answer = directed(&snapshot, &["node-a-pub", "node-b-pub"]).await?;
    assert_eq!(StatusCode::OK, answer.status(), "excluded fails nothing");
    assert!(
        answer.federation().complete(),
        "an excluded member was never in scope, so it does not clear `complete` (§11.1)"
    );
    let reported: Vec<(&str, EndpointStatus)> = answer
        .federation()
        .endpoints()
        .iter()
        .map(|record| (record.id().as_str(), record.status()))
        .collect();
    assert_eq!(
        vec![
            ("node-a-pub", EndpointStatus::Active),
            ("node-b-pub", EndpointStatus::Active),
            ("node-c-pub", EndpointStatus::Excluded),
        ],
        reported,
        "every member, in endpoint id order"
    );
    let envelope = envelope(answer)?;
    let excluded = entry(&envelope, "node-c-pub")?;
    assert!(
        excluded.get("error").is_some_and(Value::is_string),
        "an excluded entry says why (§11.1)"
    );
    assert_eq!(
        Some(&Value::from("node-c")),
        excluded.get("node_id"),
        "an out-of-scope member is still identified"
    );
    assert_eq!(
        0,
        c.received_requests().await.ok_or("no capture")?.len(),
        "an excluded member is never asked"
    );
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn latency_is_reported_exactly_for_the_endpoints_dispatched_to() -> TestResult {
    let (a, b, c) = (
        node(&["a::1"]).await,
        node(&["b::1"]).await,
        node(&[]).await,
    );
    let snapshot = registry([&a.uri(), &b.uri(), &c.uri()])?;
    let envelope = envelope(directed(&snapshot, &["node-a-pub", "node-b-pub"]).await?)?;
    for dispatched in ["node-a-pub", "node-b-pub"] {
        assert!(
            entry(&envelope, dispatched)?
                .get("latency_ms")
                .is_some_and(Value::is_u64),
            "{dispatched} was asked, so its latency is reported (N40)"
        );
    }
    assert!(
        entry(&envelope, "node-c-pub")?.get("latency_ms").is_none(),
        "a status settled before dispatch carries no latency, never a 0 (§9.5, N40)"
    );
    Ok(())
}

#[tokio::test]
async fn product_and_version_come_from_the_registry_and_are_absent_when_unknown() -> TestResult {
    let (a, b, c) = (
        node(&["a::1"]).await,
        node(&["b::1"]).await,
        node(&[]).await,
    );
    let snapshot = registry([&a.uri(), &b.uri(), &c.uri()])?;
    let envelope = envelope(directed(&snapshot, &["node-a-pub", "node-b-pub"]).await?)?;
    let described = entry(&envelope, "node-a-pub")?;
    assert_eq!(Some(&Value::from("FerroEHR")), described.get("product"));
    assert_eq!(Some(&Value::from("4.3.1")), described.get("version"));
    for unknown in ["node-b-pub", "node-c-pub"] {
        let record = entry(&envelope, unknown)?;
        assert!(
            record.get("product").is_none() && record.get("version").is_none(),
            "{unknown}: the registry records neither, and the gateway never invents them (N40)"
        );
    }
    Ok(())
}

#[tokio::test]
async fn row_count_is_what_each_node_contributed_before_the_tier_touches_the_rows() -> TestResult {
    // The same uid at both nodes, with no DISTINCT and the default dedup mode,
    // so both rows come back; `row_count` counts what each node sent.
    let a = node(&["shared::1", "a::2", "a::3"]).await;
    let b = node(&["shared::1"]).await;
    let c = node(&[]).await;
    let snapshot = registry([&a.uri(), &b.uri(), &c.uri()])?;
    let answer = directed(&snapshot, &["node-a-pub", "node-b-pub"]).await?;
    assert_eq!(4, answer.rows().len(), "every contributed row");
    let envelope = envelope(answer)?;
    assert_eq!(
        Some(&Value::from(3)),
        entry(&envelope, "node-a-pub")?.get("row_count")
    );
    assert_eq!(
        Some(&Value::from(1)),
        entry(&envelope, "node-b-pub")?.get("row_count")
    );
    assert!(
        entry(&envelope, "node-c-pub")?.get("row_count").is_none(),
        "a member never asked contributed nothing to count"
    );
    Ok(())
}

#[tokio::test]
async fn an_undirected_query_asks_every_member_and_excludes_none() -> TestResult {
    let (a, b, c) = (
        node(&["a::1"]).await,
        node(&["b::1"]).await,
        node(&["c::1"]).await,
    );
    let snapshot = registry([&a.uri(), &b.uri(), &c.uri()])?;
    let Analysis::Unscoped(query) = analyse(
        FACADE_AQL,
        &Parameters::new(),
        Paging::default(),
        &Context::new(Targeting::AskAll),
    )?
    else {
        return Err("the façade query names no patient".into());
    };
    let targets = plan::unscoped(&snapshot, Selection::Undirected, &query)?;
    assert_eq!(
        3,
        targets.plan.dispatched().count(),
        "undirected, every member is asked"
    );
    Ok(())
}

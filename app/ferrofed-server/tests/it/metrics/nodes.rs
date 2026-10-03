// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The node request counter and duration histogram: one count per request
//! sent to a member, by registry endpoint and the §11.1 outcome the
//! per-endpoint report gives it, and one duration per request counted,
//! the ask-all probe included (§9.5, §11.1, §12.5.1).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use ferrofed_engine::dispatch::NodeClients;
use ferrofed_engine::fanout::Budget;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::unreachable;
use http::{Request, StatusCode, header};
use openehr_federation::aql::{Context, Targeting};
use openehr_federation::headers::{COMPLETENESS, ENDPOINT};
use openehr_federation::id::FederationId;
use openehr_its::rest::client::ReqwestTransport;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{
    EHR_A, EHR_B, body, crossref, node_answering, node_failing, patient_query, registry,
    settings_with_room,
};
use crate::metrics::{Metered, count, parse, value};
use crate::path_ehr_id::{holder, stranger};
use crate::support::{SLACK, call, millis, observed, states};

type TestResult = Result<(), Box<dyn Error>>;

/// The counter's and the histogram's Prometheus names.
const REQUESTS: &str = "ferrofed_node_requests_total";
const DURATION_COUNT: &str = "ferrofed_node_request_duration_seconds_count";
const DURATION_SUM: &str = "ferrofed_node_request_duration_seconds_sum";
const DURATION_BUCKET: &str = "ferrofed_node_request_duration_seconds_bucket";

/// The patient's `ehr_id` at node C and at node D.
const EHR_C: &str = "3333cccc-3333-4333-8333-333333333333";
const EHR_D: &str = "4444dddd-4444-4444-8444-444444444444";

/// The per-node timeout of the fan-out, and how long node C keeps quiet.
const PER_NODE_MS: u64 = 600;
const SILENCE: Duration = Duration::from_millis(2_500);

/// Node C and node D, appended to the registry of node A and node B.
fn two_more(c: &str, d: &str) -> String {
    format!(
        r#"
[[node]]
id = "node-c"
organisation = "org-a"
system_id = "cdr-c.example.org"

[[node]]
id = "node-d"
organisation = "org-b"
system_id = "cdr-d.example.org"

[[endpoint]]
id = "node-c-pub"
node = "node-c"
url = "{c}"
connection_type = "openehr-rest-query"
managing_organisation = "org-a"

[[endpoint]]
id = "node-d-pub"
node = "node-d"
url = "{d}"
connection_type = "openehr-rest-query"
managing_organisation = "org-b"
"#
    )
}

/// A node answering the query only after [`SILENCE`].
async fn node_silent() -> Server {
    let server = Server::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(ResponseTemplate::new(200).set_delay(SILENCE))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_fan_out_counts_and_times_each_member_by_its_outcome() -> TestResult {
    let a = node_answering("uid-a::cdr-a.example.org::1").await;
    let b = node_failing(500).await;
    let c = node_silent().await;
    let dir = tempfile::tempdir()?;
    let rows = crossref(&[
        ("node-a", EHR_A),
        ("node-b", EHR_B),
        ("node-c", EHR_C),
        ("node-d", EHR_D),
    ]);
    let gateway = Metered::start(
        dir.path(),
        &registry(&a.uri(), &b.uri(), &two_more(&c.uri(), unreachable::BASE)),
        ("profile = \"development\"", &rows),
        (PER_NODE_MS, 3_000),
    )?;
    let request = Request::post("/v1/query/aql")
        .header(header::CONTENT_TYPE, "application/json")
        .header(COMPLETENESS, "partial")
        .body(Body::from(body(&patient_query())?))?;
    let (status, text) = call(gateway.app.clone(), request).await?;
    assert_eq!(StatusCode::OK, status, "a partial answer: {text}");

    let samples = gateway.scraped()?;
    for (endpoint, outcome) in [
        ("node-a-pub", "active"),
        ("node-b-pub", "node-error"),
        ("node-c-pub", "time-out"),
        ("node-d-pub", "offline"),
    ] {
        assert_eq!(
            Some("1".to_owned()),
            count(
                &samples,
                REQUESTS,
                &[("endpoint", endpoint), ("outcome", outcome)]
            ),
            "{endpoint} {outcome}: {samples:?}"
        );
        assert_eq!(
            Some("1".to_owned()),
            count(&samples, DURATION_COUNT, &[("endpoint", endpoint)]),
            "{endpoint} is timed once"
        );
        assert_eq!(
            Some("1".to_owned()),
            count(
                &samples,
                DURATION_BUCKET,
                &[("endpoint", endpoint), ("le", "+Inf")]
            ),
        );
    }
    let waited =
        value(&samples, DURATION_SUM, &[("endpoint", "node-c-pub")]).ok_or("node C is timed")?;
    assert!(
        waited >= Duration::from_millis(PER_NODE_MS).as_secs_f64() * 0.5,
        "the silent node is timed to its deadline: {waited}"
    );
    let counted = samples
        .iter()
        .filter(|sample| sample.name == REQUESTS)
        .count();
    assert_eq!(4, counted, "one series per member asked: {samples:?}");
    Ok(())
}

#[tokio::test]
async fn a_routed_read_counts_and_times_the_probe_of_each_member_and_the_read() -> TestResult {
    let a = holder().await;
    let b = stranger().await;
    let dir = tempfile::tempdir()?;
    let gateway = Metered::start(
        dir.path(),
        &registry(&a.uri(), &b.uri(), ""),
        ("", ""),
        (2_000, 3_000),
    )?;
    let version = "8849182c-82ad-4088-a07f-48ead4180515::cdr-a.example.org::1";
    let read = Request::get(format!("/v1/ehr/{EHR_A}/composition/{version}")).body(Body::empty())?;
    let (status, text) = call(gateway.app.clone(), read).await?;
    assert_eq!(StatusCode::OK, status, "{text}");

    let samples = gateway.scraped()?;
    assert_eq!(
        Some("2".to_owned()),
        count(
            &samples,
            REQUESTS,
            &[("endpoint", "node-a-pub"), ("outcome", "active")]
        ),
        "the probe and the read: {samples:?}"
    );
    assert_eq!(
        Some("1".to_owned()),
        count(
            &samples,
            REQUESTS,
            &[("endpoint", "node-b-pub"), ("outcome", "active")]
        ),
        "a member that holds no such EHR answered the probe: {samples:?}"
    );
    assert_eq!(
        Some("2".to_owned()),
        count(&samples, DURATION_COUNT, &[("endpoint", "node-a-pub")]),
        "the probe and the read are each timed"
    );
    assert_eq!(
        Some("1".to_owned()),
        count(&samples, DURATION_COUNT, &[("endpoint", "node-b-pub")]),
        "a probe carries its latency"
    );
    Ok(())
}

#[tokio::test]
async fn a_probe_past_its_deadline_is_a_time_out_timed_to_the_deadline() -> TestResult {
    let bound = Duration::from_millis(PER_NODE_MS) + SLACK;
    let a = holder().await;
    let b = Server::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/ehr/{EHR_A}")))
        .respond_with(ResponseTemplate::new(404).set_delay(bound + SLACK))
        .mount(&b)
        .await;
    let dir = tempfile::tempdir()?;
    let gateway = Metered::start(
        dir.path(),
        &registry(&a.uri(), &b.uri(), ""),
        ("", ""),
        (PER_NODE_MS, millis(bound)?),
    )?;
    let read = Request::get(format!("/v1/ehr/{EHR_A}")).body(Body::empty())?;
    let (status, text) = call(gateway.app.clone(), read).await?;
    assert_eq!(
        StatusCode::GATEWAY_TIMEOUT,
        status,
        "§11.5: a silent member may hold the EHR: {text}"
    );

    let samples = gateway.scraped()?;
    assert_eq!(
        Some("1".to_owned()),
        count(
            &samples,
            REQUESTS,
            &[("endpoint", "node-b-pub"), ("outcome", "time-out")]
        ),
        "{samples:?}"
    );
    assert_eq!(
        Some("1".to_owned()),
        count(&samples, DURATION_COUNT, &[("endpoint", "node-b-pub")]),
        "the late probe is timed"
    );
    let waited =
        value(&samples, DURATION_SUM, &[("endpoint", "node-b-pub")]).ok_or("node B is timed")?;
    assert!(
        waited >= Duration::from_millis(PER_NODE_MS).as_secs_f64() * 0.5,
        "the late probe is timed to its deadline: {waited}"
    );
    assert!(
        waited < bound.as_secs_f64(),
        "the probe is never waited on past its deadline: {waited}"
    );
    Ok(())
}

/// The ADL 1.4 template collection (ITS-REST Definition API).
const ADL14: &str = "/v1/definition/template/adl1.4";

/// The registry document of node A at `a` alone.
fn only_node_a(a: &str) -> String {
    format!(
        "[[organisation]]\nid = \"org-a\"\n\n[[node]]\nid = \"node-a\"\norganisation = \"org-a\"\n\
         system_id = \"cdr-a.example.org\"\n\n[[endpoint]]\nid = \"node-a-pub\"\nnode = \"node-a\"\n\
         url = \"{a}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n"
    )
}

#[tokio::test]
async fn a_member_request_that_never_left_the_gateway_is_neither_counted_nor_timed() -> TestResult {
    let a = Server::start().await;
    Mock::given(method("POST"))
        .and(path(ADL14))
        .respond_with(ResponseTemplate::new(201))
        .mount(&a)
        .await;
    let b = Server::start().await;
    let snapshot = RegistrySnapshot::from_toml_str(&registry(&a.uri(), &b.uri(), ""))?;
    let only_a = RegistrySnapshot::from_toml_str(&only_node_a(&a.uri()))?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(5))?;
    // Node B has no client, so the gateway sends it nothing.
    let clients = NodeClients::from_snapshot(&only_a, &transport, &BTreeMap::new())?;
    let federation = Federation::new(
        FederationId::new("example-federation")?,
        snapshot,
        clients,
        None,
        Context::new(Targeting::AskAll),
        Budget::new(Duration::from_secs(2), Duration::from_secs(3))?,
    )
    .with_template_fan_out(true)
    .with_signer(crate::support::signer("example-federation")?);
    let state = Arc::new(AppState::with_federation(federation));
    let app = ferrofed_server::router(Arc::clone(&state), &settings_with_room());
    let template = "<template><template_id><value>synthetic.t.v1</value></template_id></template>";
    let upload = Request::post(ADL14)
        .header(header::CONTENT_TYPE, "application/xml")
        .header(ENDPOINT, "*")
        .body(Body::from(template))?;
    let (status, text) = call(app.clone(), upload).await?;
    assert_eq!(StatusCode::MULTI_STATUS, status, "{text}");
    assert!(
        text.contains(r#""id":"node-b-pub""#) && text.contains(r#""status":"offline""#),
        "§11.1: the per-member record still reports node B: {text}"
    );
    assert!(
        b.received_requests()
            .await
            .ok_or("recording is on")?
            .is_empty()
    );

    let samples = parse(&state.metrics().render()?)?;
    assert_eq!(
        Some("1".to_owned()),
        count(
            &samples,
            REQUESTS,
            &[("endpoint", "node-a-pub"), ("outcome", "active")]
        ),
        "{samples:?}"
    );
    let about_b = samples
        .iter()
        .filter(|sample| sample.labels.get("endpoint").map(String::as_str) == Some("node-b-pub"))
        .count();
    assert_eq!(0, about_b, "no request left for node B: {samples:?}");
    assert_eq!(
        states(&[("node-a-pub", "up"), ("node-b-pub", "unknown")]),
        observed(&app).await?,
        "nothing was observed of node B"
    );
    Ok(())
}

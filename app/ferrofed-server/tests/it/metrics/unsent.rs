// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A request the overall budget overtook before it left the gateway: the
//! node was never asked, so the request is neither counted nor timed in the
//! node request metrics and leaves the member's state on `GET
//! /health/dependencies` as it was, for a request routed to one node, the
//! ask-all probe and a fan-out template upload (§11.5, §12.5.1, §12.6). No
//! specification governs the metrics or the dependency report: our own design.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use ferrofed_engine::dispatch::NodeClients;
use ferrofed_engine::fanout::Budget;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Request, StatusCode, header};
use openehr_federation::aql::{Context, Targeting};
use openehr_federation::headers::ENDPOINT;
use openehr_federation::id::FederationId;
use openehr_its::rest::client::ReqwestTransport;
use wiremock::ResponseTemplate;
use wiremock::matchers::any;

use crate::facade::{EHR_A, registry, settings_with_room};
use crate::metrics::parse;
use crate::support::{call, observed, states};

type TestResult = Result<(), Box<dyn Error>>;

/// The ADL 1.4 template collection (ITS-REST Definition API).
const ADL14: &str = "/v1/definition/template/adl1.4";

/// A member that answers anything `200`, so any request that reached it
/// would show.
async fn member() -> Server {
    let server = Server::start().await;
    wiremock::Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    server
}

/// A metered gateway over node A at `a` and node B at `b` whose budget has
/// run out before any request can leave it: one nanosecond per node and
/// overall, counted from each request's arrival.
fn overtaken(a: &Server, b: &Server) -> Result<(Router, Arc<AppState>), Box<dyn Error>> {
    let snapshot = RegistrySnapshot::from_toml_str(&registry(&a.uri(), &b.uri(), ""))?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(5))?;
    let clients = NodeClients::from_snapshot(&snapshot, &transport, &BTreeMap::new())?;
    let federation = Federation::new(
        FederationId::new("example-federation")?,
        snapshot,
        clients,
        None,
        Context::new(Targeting::AskAll),
        Budget::new(Duration::from_nanos(1), Duration::from_nanos(1))?,
    )
    .with_template_fan_out(true)
    .with_signer(crate::support::signer("example-federation")?);
    let state = Arc::new(AppState::with_federation(federation));
    let app = ferrofed_server::router(Arc::clone(&state), &settings_with_room());
    Ok((app, state))
}

/// Asserts that neither member received a request, that no node request was
/// counted or timed, and that both members are still `unknown`.
async fn nothing_observed(
    (app, state): (&Router, &AppState),
    a: &Server,
    b: &Server,
) -> TestResult {
    for (name, server) in [("node A", a), ("node B", b)] {
        let received = server.received_requests().await.ok_or("recording is on")?;
        assert!(received.is_empty(), "{name} was never asked");
    }
    let samples = parse(&state.metrics().render()?)?;
    let about_nodes = samples
        .iter()
        .filter(|sample| sample.labels.contains_key("endpoint"))
        .count();
    assert_eq!(0, about_nodes, "no request left the gateway: {samples:?}");
    assert_eq!(
        states(&[("node-a-pub", "unknown"), ("node-b-pub", "unknown")]),
        observed(app).await?,
        "nothing was observed of either member"
    );
    Ok(())
}

#[tokio::test]
async fn a_routed_request_the_budget_overtook_is_neither_counted_nor_observed() -> TestResult {
    let a = member().await;
    let b = member().await;
    let (app, state) = overtaken(&a, &b)?;
    let read = Request::get(ADL14)
        .header(ENDPOINT, "node-a-pub")
        .body(Body::empty())?;
    let (status, text) = call(app.clone(), read).await?;
    assert_eq!(
        StatusCode::GATEWAY_TIMEOUT,
        status,
        "§11.2: the budget ran out: {text}"
    );
    nothing_observed((&app, &state), &a, &b).await
}

#[tokio::test]
async fn a_probe_the_budget_overtook_is_neither_counted_nor_observed() -> TestResult {
    let a = member().await;
    let b = member().await;
    let (app, state) = overtaken(&a, &b)?;
    let read = Request::get(format!("/v1/ehr/{EHR_A}")).body(Body::empty())?;
    let (status, text) = call(app.clone(), read).await?;
    assert_eq!(
        StatusCode::GATEWAY_TIMEOUT,
        status,
        "§11.5: a member never asked may hold the EHR: {text}"
    );
    nothing_observed((&app, &state), &a, &b).await
}

#[tokio::test]
async fn a_template_upload_the_budget_overtook_is_neither_counted_nor_observed() -> TestResult {
    let a = member().await;
    let b = member().await;
    let (app, state) = overtaken(&a, &b)?;
    let template = "<template><template_id><value>synthetic.t.v1</value></template_id></template>";
    let upload = Request::post(ADL14)
        .header(header::CONTENT_TYPE, "application/xml")
        .header(ENDPOINT, "*")
        .body(Body::from(template))?;
    let (status, text) = call(app.clone(), upload).await?;
    assert!(
        !text.contains(r#""status":"active""#)
            && text.contains(r#""id":"node-a-pub""#)
            && text.contains(r#""id":"node-b-pub""#)
            && text.matches(r#""status":"time-out""#).count() == 2,
        "§11.1: each member is reported, neither answered in time ({status}): {text}"
    );
    nothing_observed((&app, &state), &a, &b).await
}

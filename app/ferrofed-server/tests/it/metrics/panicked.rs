// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A fan-out task that panics is the gateway's own defect: the template
//! upload fails as the probe and the query fan-out do, and no member is
//! counted in the node request metrics or changes state on `GET
//! /health/dependencies` for it (§11.1 reserves `time-out` for a node that
//! did not answer). No specification governs the metrics or the dependency
//! report: our own design.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use ferrofed_engine::dispatch::{NodeClients, SharedCredentials};
use ferrofed_engine::fanout::Budget;
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Request, StatusCode, header};
use openehr_federation::aql::{Context, Targeting};
use openehr_federation::headers::ENDPOINT;
use openehr_federation::id::FederationId;
use openehr_its::rest::client::{
    Credentials, CredentialsError, CredentialsProvider, ReqwestTransport,
};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{registry, settings_with_room};
use crate::metrics::parse;
use crate::support::{call, observed, states};

type TestResult = Result<(), Box<dyn Error>>;

/// A credentials provider that panics, a stand-in for any defect inside a
/// fan-out task.
#[derive(Debug)]
struct Panicking;

#[async_trait::async_trait]
impl CredentialsProvider for Panicking {
    #[expect(
        clippy::panic,
        reason = "the panic is the defect this test puts inside a fan-out task"
    )]
    async fn credentials(&self) -> Result<Credentials, CredentialsError> {
        panic!("a synthetic defect inside the gateway");
    }
}

/// A member that answers anything `201`.
async fn member() -> Server {
    let server = Server::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_fan_out_task_that_panics_fails_the_upload_and_blames_no_member() -> TestResult {
    let a = member().await;
    let b = member().await;
    let snapshot = RegistrySnapshot::from_toml_str(&registry(&a.uri(), &b.uri(), ""))?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(5))?;
    let panicking: SharedCredentials = Arc::new(Panicking);
    let credentials = BTreeMap::from([(EndpointId::new("node-b-pub")?, panicking)]);
    let clients = NodeClients::from_snapshot(&snapshot, &transport, &credentials)?;
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
    let upload = Request::post("/v1/definition/template/adl1.4")
        .header(header::CONTENT_TYPE, "application/xml")
        .header(ENDPOINT, "*")
        .body(Body::from(template))?;
    let (status, text) = call(app.clone(), upload).await?;
    assert_eq!(
        StatusCode::INTERNAL_SERVER_ERROR,
        status,
        "the gateway's own defect fails the request: {text}"
    );
    let received = b.received_requests().await.ok_or("recording is on")?;
    assert!(received.is_empty(), "node B was never sent the upload");

    let samples = parse(&state.metrics().render()?)?;
    let about_b = samples
        .iter()
        .filter(|sample| sample.labels.get("endpoint").map(String::as_str) == Some("node-b-pub"))
        .count();
    assert_eq!(0, about_b, "node B is not counted: {samples:?}");
    assert_eq!(
        states(&[("node-a-pub", "unknown"), ("node-b-pub", "unknown")]),
        observed(&app).await?,
        "a failed request records no member"
    );
    Ok(())
}

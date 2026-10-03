// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! An admission check whose budget runs out before its first EHR call
//! leaves the gateway: the report names a call never sent, never a node that
//! did not answer, and the node request metrics and `GET
//! /health/dependencies` record nothing of the member (§11.5).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ferrofed_engine::dispatch::NodeClients;
use ferrofed_engine::fanout::Budget;
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_server::admission::report::{Condition, Verdict};
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::unreachable;
use openehr_federation::aql::{Context, Targeting};
use openehr_federation::id::FederationId;
use openehr_its::rest::client::ReqwestTransport;
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

use super::{TestResult, verdict};
use crate::facade::{registry, settings_with_room};
use crate::metrics::parse;
use crate::support::{observed, states};

#[tokio::test]
async fn a_check_the_budget_overtook_names_a_call_never_sent_and_records_nothing() -> TestResult {
    let a = Server::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(201))
        .mount(&a)
        .await;
    let snapshot = RegistrySnapshot::from_toml_str(&registry(&a.uri(), unreachable::BASE, ""))?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(5))?;
    let clients = NodeClients::from_snapshot(&snapshot, &transport, &BTreeMap::new())?;
    // One nanosecond per node: every call's deadline has passed before the
    // client checks it, so none leaves the gateway.
    let federation = Federation::new(
        FederationId::new("example-federation")?,
        snapshot,
        clients,
        None,
        Context::new(Targeting::AskAll),
        Budget::new(Duration::from_nanos(1), Duration::from_nanos(1))?,
    )
    .with_signer(crate::support::signer("example-federation")?);
    let state = Arc::new(AppState::with_federation(federation));
    let federation = state.federation().ok_or("the state holds the federation")?;
    let report =
        ferrofed_server::admission::check(&federation, &EndpointId::new("node-a-pub")?, 3).await?;

    assert_eq!(
        Verdict::Fail,
        verdict(&report, Condition::EhrIdGeneration)?,
        "a check that sent nothing passes nothing: {report}"
    );
    let text = report.to_string();
    assert!(
        text.contains("the deadline for endpoint node-a-pub passed before the request was sent"),
        "the call never sent is named as such: {text}"
    );
    assert!(
        !text.contains("did not answer"),
        "no node is said to have stayed silent: {text}"
    );
    let received = a.received_requests().await.ok_or("recording is on")?;
    assert!(received.is_empty(), "node A was never asked");

    let samples = parse(&state.metrics().render()?)?;
    let about_nodes = samples
        .iter()
        .filter(|sample| sample.labels.contains_key("endpoint"))
        .count();
    assert_eq!(0, about_nodes, "no request left the gateway: {samples:?}");
    let app = ferrofed_server::router(Arc::clone(&state), &settings_with_room());
    assert_eq!(
        states(&[("node-a-pub", "unknown"), ("node-b-pub", "unknown")]),
        observed(&app).await?,
        "nothing was observed of either member"
    );
    Ok(())
}

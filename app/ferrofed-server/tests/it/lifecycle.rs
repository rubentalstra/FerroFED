// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Readiness over the life of the process and the dependency report: `503`
//! until boot completes and from the moment the stop signal arrives, `200`
//! with every member down, and `GET /health/dependencies` naming each
//! endpoint with the state the gateway last observed of it. No specification
//! governs health probes: our own design.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::Config;
use ferrofed_server::health::lifecycle::drain_on;
use ferrofed_server::state::AppState;
use ferrofed_testkit::unreachable;
use http::{Request, StatusCode};
use serde::Deserialize;

use crate::facade::{
    EHR_A, EHR_B, PATIENT, body, crossref, node_answering, node_failing, patient_query, post,
};
use crate::support::call;

type TestResult = Result<(), Box<dyn Error>>;

/// The readiness document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Readiness {
    state: String,
    phase: String,
    indicators: BTreeMap<String, Indicator>,
}

/// One indicator in the readiness document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Indicator {
    state: String,
}

/// The dependency document: endpoint ids and states, and nothing else.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dependencies {
    endpoints: BTreeMap<String, String>,
    resolver: Option<String>,
}

/// The registry document of node A and node B at `a` and `b`.
fn registry(a: &str, b: &str) -> String {
    crate::facade::registry(a, b, "")
}

/// A second unreachable base, because two endpoints never share a URL.
fn unreachable_b() -> String {
    format!("{}/node-b", unreachable::BASE)
}

/// The state a booting gateway builds from a development configuration over
/// node A at `a` and node B at `b`, resolving the patient at both, with the
/// stored-query store in `dir`.
fn booting(dir: &Path, a: &str, b: &str) -> Result<Arc<AppState>, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(a, b))?;
    let document = toml::Value::String(document.display().to_string());
    let store = toml::Value::String(dir.join("definitions.redb").display().to_string());
    let rows = crossref(&[("node-a", EHR_A), ("node-b", EHR_B)]);
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n\
         [federation]\nid = \"example-federation\"\nnode_selection = \"ask-all\"\n\
         per_node_timeout_ms = 2000\noverall_timeout_ms = 3000\n\n\
         [stored_queries]\npath = {store}\n{rows}"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    Ok(Arc::new(AppState::build(&settings)?))
}

/// The application over `state`.
fn app(state: &Arc<AppState>) -> Router {
    ferrofed_server::router(Arc::clone(state), &crate::facade::settings_with_room())
}

/// Asks `GET /health/readiness`.
async fn readiness(state: &Arc<AppState>) -> Result<(StatusCode, Readiness), Box<dyn Error>> {
    let (status, text) = call(
        app(state),
        Request::get("/health/readiness").body(Body::empty())?,
    )
    .await?;
    Ok((status, serde_json::from_str(&text)?))
}

/// Asks `GET /health/dependencies` and returns the status, the parsed
/// document and the raw text.
async fn dependencies(
    state: &Arc<AppState>,
) -> Result<(StatusCode, Dependencies, String), Box<dyn Error>> {
    let (status, text) = call(
        app(state),
        Request::get("/health/dependencies").body(Body::empty())?,
    )
    .await?;
    Ok((status, serde_json::from_str(&text)?, text))
}

/// The state of `endpoint` in `report`.
fn state_of<'a>(report: &'a Dependencies, endpoint: &str) -> Option<&'a str> {
    report.endpoints.get(endpoint).map(String::as_str)
}

#[tokio::test]
async fn readiness_is_503_until_boot_completes_and_names_what_boot_built() -> TestResult {
    let dir = tempfile::tempdir()?;
    let state = booting(dir.path(), unreachable::BASE, &unreachable_b())?;

    let (status, report) = readiness(&state).await?;
    assert_eq!(StatusCode::SERVICE_UNAVAILABLE, status);
    assert_eq!("down", report.state);
    assert_eq!("booting", report.phase);
    assert_eq!(
        vec![
            "configuration",
            "outbound_clients",
            "registry",
            "stored_queries"
        ],
        report.indicators.keys().collect::<Vec<_>>(),
        "every subsystem boot built, and no member and no resolver"
    );
    assert!(report.indicators.values().all(|i| i.state == "up"));

    state.lifecycle().booted();
    let (status, report) = readiness(&state).await?;
    assert_eq!(StatusCode::OK, status);
    assert_eq!("up", report.state);
    assert_eq!("serving", report.phase);
    Ok(())
}

#[tokio::test]
async fn readiness_is_503_from_the_moment_the_stop_signal_arrives() -> TestResult {
    let state = Arc::new(AppState::default());
    state.lifecycle().booted();
    let (status, _) = readiness(&state).await?;
    assert_eq!(StatusCode::OK, status);

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let signal = async move {
        if stopped.await.is_err() {
            tracing::debug!("the stop channel closed");
        }
    };
    let draining = tokio::spawn(drain_on(signal, state.lifecycle().clone()));
    let (status, _) = readiness(&state).await?;
    assert_eq!(StatusCode::OK, status, "no signal yet");

    stop.send(()).map_err(|()| "the signal task is gone")?;
    draining.await?;
    let (status, report) = readiness(&state).await?;
    assert_eq!(StatusCode::SERVICE_UNAVAILABLE, status);
    assert_eq!("down", report.state);
    assert_eq!("draining", report.phase);

    let (status, _) = call(app(&state), Request::get("/health").body(Body::empty())?).await?;
    assert_eq!(StatusCode::OK, status, "a draining process is still live");
    Ok(())
}

#[tokio::test]
async fn readiness_stays_200_with_every_member_down_and_the_dependencies_name_them() -> TestResult {
    let dir = tempfile::tempdir()?;
    let state = booting(dir.path(), unreachable::BASE, &unreachable_b())?;
    state.lifecycle().booted();

    let (status, report, _) = dependencies(&state).await?;
    assert_eq!(StatusCode::OK, status);
    assert_eq!(Some("unknown"), state_of(&report, "node-a-pub"));
    assert_eq!(Some("unknown"), state_of(&report, "node-b-pub"));
    assert_eq!(Some("unknown"), report.resolver.as_deref());

    let (status, _) = call(app(&state), post(body(&patient_query())?)?).await?;
    assert_eq!(
        StatusCode::GATEWAY_TIMEOUT,
        status,
        "every member is offline, so the query fails closed"
    );

    let (status, readiness) = readiness(&state).await?;
    assert_eq!(StatusCode::OK, status, "no member gates readiness");
    assert_eq!("up", readiness.state);

    let (status, report, text) = dependencies(&state).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "the dependency report is always 200"
    );
    assert_eq!(Some("down"), state_of(&report, "node-a-pub"));
    assert_eq!(Some("down"), state_of(&report, "node-b-pub"));
    assert_eq!(
        Some("up"),
        report.resolver.as_deref(),
        "the cross-reference answered for both"
    );
    assert!(!text.contains("127.0.0.1"), "no URL: {text}");
    assert!(!text.contains(PATIENT), "no identifier: {text}");
    Ok(())
}

#[tokio::test]
async fn the_dependencies_keep_each_endpoints_last_state_apart() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let dir = tempfile::tempdir()?;
    let state = booting(dir.path(), &a.uri(), &unreachable_b())?;
    state.lifecycle().booted();

    let (status, _) = call(app(&state), post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::GATEWAY_TIMEOUT, status);

    let (_, report, _) = dependencies(&state).await?;
    assert_eq!(Some("up"), state_of(&report, "node-a-pub"));
    assert_eq!(Some("down"), state_of(&report, "node-b-pub"));
    Ok(())
}

#[tokio::test]
async fn a_query_member_answering_a_4xx_is_up_and_one_answering_a_5xx_is_failing() -> TestResult {
    let refusing = node_failing(400).await;
    let failing = node_failing(500).await;
    let dir = tempfile::tempdir()?;
    let state = booting(dir.path(), &refusing.uri(), &failing.uri())?;
    state.lifecycle().booted();

    let (status, text) = call(app(&state), post(body(&patient_query())?)?).await?;
    assert_eq!(
        StatusCode::FAILED_DEPENDENCY,
        status,
        "§11.4: both are node-error in the record: {text}"
    );

    let (_, report, _) = dependencies(&state).await?;
    assert_eq!(
        Some("up"),
        state_of(&report, "node-a-pub"),
        "a 400 says the request was refused, and the node answered"
    );
    assert_eq!(Some("failing"), state_of(&report, "node-b-pub"));
    Ok(())
}

#[tokio::test]
async fn without_a_registry_the_dependency_report_is_empty() -> TestResult {
    let state = Arc::new(AppState::default());
    let (status, report, text) = dependencies(&state).await?;
    assert_eq!(StatusCode::OK, status);
    assert!(report.endpoints.is_empty());
    assert_eq!(None, report.resolver, "no resolver is configured: {text}");
    Ok(())
}

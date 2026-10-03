// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The completion strategy a request selects with
//! `openEHR-federation-completeness`, through the real configuration path:
//! `all` and no header fail closed, `partial` returns the answering nodes'
//! rows where best-effort is offered and is refused where it is not, and any
//! other value is a `400` that asks no node (§11.2, §11.4, N37, CP-30). An
//! incomplete answer says so in `meta.federation.complete` and carries no
//! `OperationOutcome` (§11.4, N17, CP-12).
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
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Request, StatusCode, header};
use openehr_federation::headers::COMPLETENESS;

use crate::facade::{
    Answer, EHR_A, EHR_B, body, crossref, node_answering, node_failing, patient_query, received,
    registry, schema, settings_with_room, statuses,
};
use crate::support::{ErrorBody, call, error_body};

type TestResult = Result<(), Box<dyn Error>>;

/// A development gateway over node A and node B resolving the patient at both,
/// with `federation.best_effort = offered`, or with no resolver at all when
/// `resolving` is `false`.
fn gateway(
    dir: &Path,
    a: &Server,
    b: &Server,
    offered: bool,
    resolving: bool,
) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(&a.uri(), &b.uri(), ""))?;
    let document = toml::Value::String(document.display().to_string());
    let rows = if resolving {
        crossref(&[("node-a", EHR_A), ("node-b", EHR_B)])
    } else {
        String::new()
    };
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\nbest_effort = {offered}\n\n{rows}"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// `POST /v1/query/aql` for the patient, with one completeness header per
/// entry of `values`.
fn post(values: &[&str]) -> Result<Request<Body>, Box<dyn Error>> {
    let mut request =
        Request::post("/v1/query/aql").header(header::CONTENT_TYPE, "application/json");
    for value in values {
        request = request.header(COMPLETENESS, *value);
    }
    Ok(request.body(Body::from(body(&patient_query())?))?)
}

/// The error body of a refusal.
fn refusal(text: &str) -> Result<ErrorBody, Box<dyn Error>> {
    error_body(text)
}

/// Asserts that neither node received a request.
async fn nobody_asked(a: &Server, b: &Server) -> TestResult {
    for server in [a, b] {
        assert!(
            received(server).await?.is_empty(),
            "a refused request asks no node"
        );
    }
    Ok(())
}

// conformance: CP-30
#[tokio::test]
async fn partial_where_offered_is_a_200_with_the_answering_nodes_rows() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_failing(500).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, true, true)?;

    let (status, text) = call(app, post(&["partial"])?).await?;
    assert_eq!(StatusCode::OK, status, "best-effort does not fail: {text}");
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(!answer.meta.federation.complete);
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "node-error")],
        statuses(&answer),
        "the failing node is named with its status"
    );
    assert_eq!(
        1,
        answer.rows.len(),
        "node A's row, and nothing from node B"
    );
    assert_eq!(
        Some(&"uid-at-a".to_owned()),
        answer.rows.first().and_then(|row| row.get(1))
    );
    Ok(())
}

// conformance: CP-30
#[tokio::test]
async fn all_stated_explicitly_is_accepted_and_fails_closed() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_failing(500).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, true, true)?;

    let (status, text) = call(app, post(&["all"])?).await?;
    assert_eq!(StatusCode::FAILED_DEPENDENCY, status, "{text}");
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(answer.rows.is_empty(), "a failing query returns no rows");
    assert!(!answer.meta.federation.complete);
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "node-error")],
        statuses(&answer)
    );
    Ok(())
}

// conformance: CP-30
#[tokio::test]
async fn partial_where_best_effort_is_not_offered_is_refused() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_failing(500).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, false, true)?;

    let (status, text) = call(app, post(&["partial"])?).await?;
    assert_eq!(
        StatusCode::BAD_REQUEST,
        status,
        "never served all-or-nothing in its place: {text}"
    );
    let error = refusal(&text)?;
    assert_eq!("partial-unsupported", error.code, "§11.4, N37");
    assert!(error.message.contains("not offered"), "{}", error.message);
    nobody_asked(&a, &b).await
}

/// Asserts that an incomplete answer reports its coverage in
/// `meta.federation.complete` alone and carries no FHIR resource: §11.4
/// conditions the `OperationOutcome` of CP-12 on a FHIR-facing consumer, and
/// N17 makes the answer an ITS-REST `RESULT_SET` whose additions live under
/// `meta.federation` only (§9.1).
fn incomplete_without_an_operation_outcome(text: &str) -> TestResult {
    schema::validate(text)?;
    for word in ["OperationOutcome", "resourceType"] {
        assert!(
            !text.contains(word),
            "no FHIR resource travels in a RESULT_SET (N17): {text}"
        );
    }
    let answer: Answer = serde_json::from_str(text)?;
    assert!(!answer.meta.federation.complete, "§11.4: {text}");
    Ok(())
}

// conformance: CP-12
#[tokio::test]
async fn incompleteness_is_the_complete_flag_and_never_an_operation_outcome() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_failing(500).await;
    let dir = tempfile::tempdir()?;

    let app = gateway(dir.path(), &a, &b, true, true)?;
    let (status, text) = call(app, post(&["partial"])?).await?;
    assert_eq!(StatusCode::OK, status, "best-effort: {text}");
    incomplete_without_an_operation_outcome(&text)?;

    let app = gateway(dir.path(), &a, &b, true, true)?;
    let (status, text) = call(app, post(&[])?).await?;
    assert_eq!(
        StatusCode::FAILED_DEPENDENCY,
        status,
        "all-or-nothing: {text}"
    );
    incomplete_without_an_operation_outcome(&text)
}

#[tokio::test]
async fn all_is_accepted_where_best_effort_is_not_offered() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, false, true)?;

    let (status, text) = call(app, post(&["all"])?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(answer.meta.federation.complete);
    assert_eq!(2, answer.rows.len());
    Ok(())
}

#[tokio::test]
async fn an_unknown_value_is_a_400_that_quotes_nothing_and_asks_nobody() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    for value in ["Partial", "best-effort", "SENTINEL-VALUE-77"] {
        let app = gateway(dir.path(), &a, &b, true, true)?;
        let (status, text) = call(app, post(&[value])?).await?;
        assert_eq!(StatusCode::BAD_REQUEST, status, "{value}: {text}");
        assert!(!text.contains(value), "the value is never echoed: {text}");
        let error = refusal(&text)?;
        assert_eq!("completeness-invalid", error.code);
        assert!(
            error.message.contains("openEHR-federation-completeness"),
            "{}",
            error.message
        );
    }
    nobody_asked(&a, &b).await
}

#[tokio::test]
async fn a_repeated_header_is_a_400_that_asks_nobody() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, true, true)?;

    let (status, text) = call(app, post(&["partial", "all"])?).await?;
    assert_eq!(StatusCode::BAD_REQUEST, status, "{text}");
    assert_eq!("completeness-invalid", refusal(&text)?.code);
    nobody_asked(&a, &b).await
}

#[tokio::test]
async fn a_cross_reference_that_cannot_answer_is_reported_under_partial() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, true, false)?;

    let (status, text) = call(app, post(&["partial"])?).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "under best-effort the unanswered resolution is reported, not a 424: {text}"
    );
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(answer.rows.is_empty(), "no member could be scoped");
    assert!(!answer.meta.federation.complete);
    assert_eq!(
        vec![
            ("node-a-pub", "not-resolved"),
            ("node-b-pub", "not-resolved")
        ],
        statuses(&answer)
    );
    nobody_asked(&a, &b).await
}

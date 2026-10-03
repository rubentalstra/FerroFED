// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The budget a federated query runs under, through the real configuration
//! path: a `Prefer: wait` shortens it and never extends it, the effective
//! budget is reported in `meta.federation.timeout`, a malformed `Prefer` is
//! ignored and never refused, and the gateway answers inside the budget
//! (§11.5, N38, CP-31; RFC 7240 §2, §3, §4.3). The optional asynchronous
//! pattern of §11.7 is not offered, so `Prefer: respond-async` is ignored and
//! the request is answered synchronously under the same budget (RFC 7240
//! §2, §4.1).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::{COMBINING_MARGIN_MS, Config};
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{HeaderMap, Request, StatusCode, header};
use openehr_federation::headers::COMPLETENESS;
use serde::Deserialize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{
    Answer, EHR_A, EHR_B, body, crossref, node_answering, patient_query, received, registry,
    schema, settings_with_room, statuses,
};
use crate::support::{SLACK, millis, send};

type TestResult = Result<(), Box<dyn Error>>;

/// The `Prefer` request header (RFC 7240 §2).
const PREFER: &str = "prefer";

/// The `Preference-Applied` response header (RFC 7240 §3).
const PREFERENCE_APPLIED: &str = "preference-applied";

/// The timeout policy the gateway declares.
const POLICY: &str = "abandon-and-mark";

/// The client wait a shortening test sends: the slack, so the answering node
/// always beats it.
const WAIT: Duration = SLACK;

/// The `Prefer` value asking for [`WAIT`] (RFC 7240 §4.3).
fn wait_preference() -> String {
    format!("wait={}", WAIT.as_secs())
}

/// Node A answering at once and node B silent one slack past [`WAIT`], behind
/// a configured budget long enough to wait for node B.
async fn shortened() -> Result<(Server, Server, tempfile::TempDir, Router), Box<dyn Error>> {
    let a = node_answering("uid-at-a").await;
    let b = node_after("uid-at-b", WAIT + SLACK).await;
    let dir = tempfile::tempdir()?;
    let per_node_ms = millis(WAIT + SLACK)? + 1_000;
    let app = gateway(dir.path(), &a, &b, per_node_ms, per_node_ms + 1_000)?;
    Ok((a, b, dir, app))
}

/// A node answering one row holding `uid` after `delay`.
async fn node_after(uid: &str, delay: Duration) -> Server {
    let server = Server::start().await;
    let answer = format!(
        r##"{{"q":"node","columns":[{{"name":"#0","path":"c/uid/value"}}],"rows":[["{uid}"]]}}"##
    );
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(answer.into_bytes(), "application/json")
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

/// A development gateway over node A and node B resolving the patient at
/// both, with `per_node_ms` and `overall_ms` configured and best-effort
/// offered.
fn gateway(
    dir: &Path,
    a: &Server,
    b: &Server,
    per_node_ms: u64,
    overall_ms: u64,
) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(&a.uri(), &b.uri(), ""))?;
    let document = toml::Value::String(document.display().to_string());
    let rows = crossref(&[("node-a", EHR_A), ("node-b", EHR_B)]);
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = {per_node_ms}\noverall_timeout_ms = {overall_ms}\nnode_selection = \"ask-all\"\nid = \"example-federation\"\nbest_effort = true\n\n{rows}"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// `POST /v1/query/aql` for the patient, with one `Prefer` header per entry
/// of `prefer` and the completeness header when `partial`.
fn post(prefer: &[&str], partial: bool) -> Result<Request<Body>, Box<dyn Error>> {
    let mut request =
        Request::post("/v1/query/aql").header(header::CONTENT_TYPE, "application/json");
    for value in prefer {
        request = request.header(PREFER, *value);
    }
    if partial {
        request = request.header(COMPLETENESS, "partial");
    }
    Ok(request.body(Body::from(body(&patient_query())?))?)
}

/// What one federated call answered, and how long it took.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
    took: Duration,
}

/// Sends `request` through `app`, timing it.
async fn timed(app: Router, request: Request<Body>) -> Result<Reply, Box<dyn Error>> {
    let started = Instant::now();
    let response = send(app, request).await?;
    let took = started.elapsed();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
    Ok(Reply {
        status,
        headers,
        text: String::from_utf8(bytes.to_vec())?,
        took,
    })
}

/// The `meta.federation.timeout` member of a federated answer.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Timeout {
    per_node_ms: u64,
    overall_ms: u64,
    policy: String,
}

/// The effective budget the answer `text` reports.
fn reported(text: &str) -> Result<Timeout, Box<dyn Error>> {
    #[derive(Deserialize)]
    struct Envelope {
        meta: Meta,
    }
    #[derive(Deserialize)]
    struct Meta {
        federation: Members,
    }
    #[derive(Deserialize)]
    struct Members {
        timeout: Timeout,
    }
    Ok(serde_json::from_str::<Envelope>(text)?
        .meta
        .federation
        .timeout)
}

/// A reported budget of `per_node_ms` and `overall_ms`.
fn budget(per_node_ms: u64, overall_ms: u64) -> Timeout {
    Timeout {
        per_node_ms,
        overall_ms,
        policy: POLICY.to_owned(),
    }
}

/// The `Preference-Applied` value of a reply, if any.
fn applied(reply: &Reply) -> Option<&str> {
    reply
        .headers
        .get(PREFERENCE_APPLIED)
        .and_then(|value| value.to_str().ok())
}

// conformance: CP-31
#[tokio::test]
async fn a_shorter_wait_shortens_the_budget_and_is_reported() -> TestResult {
    let (_a, _b, _dir, app) = shortened().await?;
    let wait = wait_preference();

    let reply = timed(app, post(&[wait.as_str()], false)?).await?;
    assert_eq!(StatusCode::GATEWAY_TIMEOUT, reply.status, "{}", reply.text);
    assert!(
        reply.took < WAIT + SLACK,
        "the gateway took {:?} under a {WAIT:?} client wait",
        reply.took
    );
    schema::validate(&reply.text)?;
    let answer: Answer = serde_json::from_str(&reply.text)?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "time-out")],
        statuses(&answer),
        "the node the configured budget would have waited for is abandoned"
    );
    let wait_ms = millis(WAIT)?;
    assert_eq!(budget(wait_ms, wait_ms), reported(&reply.text)?);
    assert_eq!(Some(wait.as_str()), applied(&reply));
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn a_shorter_wait_under_partial_returns_the_answering_rows() -> TestResult {
    let (_a, _b, _dir, app) = shortened().await?;

    let reply = timed(app, post(&[wait_preference().as_str()], true)?).await?;
    assert_eq!(StatusCode::OK, reply.status, "{}", reply.text);
    assert!(reply.took < WAIT + SLACK, "{:?}", reply.took);
    schema::validate(&reply.text)?;
    let answer: Answer = serde_json::from_str(&reply.text)?;
    assert!(!answer.meta.federation.complete);
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "time-out")],
        statuses(&answer)
    );
    assert_eq!(
        Some(&"uid-at-a".to_owned()),
        answer.rows.first().and_then(|row| row.get(1))
    );
    assert_eq!(1, answer.rows.len(), "nothing from the abandoned node");
    let wait_ms = millis(WAIT)?;
    assert_eq!(budget(wait_ms, wait_ms), reported(&reply.text)?);
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn a_longer_wait_never_extends_the_configured_budget() -> TestResult {
    let overall = Duration::from_millis(500);
    let a = node_answering("uid-at-a").await;
    let b = node_after("uid-at-b", overall + SLACK).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, 300, millis(overall)?)?;

    let reply = timed(app, post(&["wait=60"], false)?).await?;
    assert_eq!(StatusCode::GATEWAY_TIMEOUT, reply.status, "{}", reply.text);
    assert!(
        reply.took < overall + SLACK,
        "the gateway took {:?}: a client wait extended its budget",
        reply.took
    );
    schema::validate(&reply.text)?;
    assert_eq!(budget(300, 500), reported(&reply.text)?);
    assert_eq!(
        None,
        applied(&reply),
        "the gateway's own budget applied, not the wait"
    );
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn a_zero_wait_asks_no_node_and_reports_every_node_time_out() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, 2_000, 3_000)?;

    let reply = timed(app, post(&["wait=0"], false)?).await?;
    assert_eq!(StatusCode::GATEWAY_TIMEOUT, reply.status, "{}", reply.text);
    schema::validate(&reply.text)?;
    let answer: Answer = serde_json::from_str(&reply.text)?;
    assert_eq!(
        vec![("node-a-pub", "time-out"), ("node-b-pub", "time-out")],
        statuses(&answer),
        "a node never asked is unknown, never empty"
    );
    assert_eq!(budget(0, 0), reported(&reply.text)?);
    assert_eq!(Some("wait=0"), applied(&reply));
    for server in [&a, &b] {
        assert!(received(server).await?.is_empty(), "no budget, no request");
    }
    Ok(())
}

#[tokio::test]
async fn without_prefer_the_configured_budget_is_reported() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, 2_000, 3_000)?;

    let reply = timed(app, post(&[], false)?).await?;
    assert_eq!(StatusCode::OK, reply.status, "{}", reply.text);
    schema::validate(&reply.text)?;
    assert_eq!(budget(2_000, 3_000), reported(&reply.text)?);
    assert_eq!(None, applied(&reply));
    Ok(())
}

#[tokio::test]
async fn a_malformed_or_unknown_preference_is_ignored_never_refused() -> TestResult {
    for prefer in [
        "wait",
        "wait=",
        "wait=soon",
        "wait=-1",
        "wait=1.5",
        "wait=\"1\"",
        "respond-later=1",
        "return=minimal",
    ] {
        let a = node_answering("uid-at-a").await;
        let b = node_answering("uid-at-b").await;
        let dir = tempfile::tempdir()?;
        let app = gateway(dir.path(), &a, &b, 2_000, 3_000)?;

        let reply = timed(app, post(&[prefer], false)?).await?;
        assert_eq!(StatusCode::OK, reply.status, "{prefer:?}: {}", reply.text);
        schema::validate(&reply.text)?;
        assert_eq!(
            budget(2_000, 3_000),
            reported(&reply.text)?,
            "{prefer:?} changed the budget"
        );
        assert_eq!(None, applied(&reply), "{prefer:?}");
    }
    Ok(())
}

#[tokio::test]
async fn only_the_first_wait_counts() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, 2_000, 3_000)?;

    let reply = timed(app, post(&["return=minimal, wait=2", "wait=1"], false)?).await?;
    assert_eq!(StatusCode::OK, reply.status, "{}", reply.text);
    assert_eq!(budget(2_000, 2_000), reported(&reply.text)?);
    assert_eq!(Some("wait=2"), applied(&reply));
    Ok(())
}

/// The `Prefer` token asking for an asynchronous answer (RFC 7240 §4.1).
const RESPOND_ASYNC: &str = "respond-async";

/// Asserts the synchronous shape of a reply to a request that asked for
/// `respond-async`: never the `202` and `Content-Location` of the §11.7
/// pattern, never `respond-async` in `Preference-Applied` (RFC 7240 §3), and
/// an envelope the schema accepts (§11.7 leaves the envelope unchanged).
fn assert_answered_synchronously(reply: &Reply) -> TestResult {
    assert_ne!(StatusCode::ACCEPTED, reply.status, "{}", reply.text);
    assert!(
        reply.headers.get(header::CONTENT_LOCATION).is_none(),
        "no polling URL: {:?}",
        reply.headers
    );
    assert!(
        !applied(reply).is_some_and(|value| value.contains(RESPOND_ASYNC)),
        "RFC 7240 §3: an ignored preference is never reported applied: {:?}",
        applied(reply)
    );
    schema::validate(&reply.text)?;
    Ok(())
}

#[tokio::test]
async fn respond_async_is_ignored_and_answered_synchronously() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, 2_000, 3_000)?;

    let reply = timed(app, post(&[RESPOND_ASYNC], false)?).await?;
    assert_eq!(StatusCode::OK, reply.status, "{}", reply.text);
    assert_answered_synchronously(&reply)?;
    let answer: Answer = serde_json::from_str(&reply.text)?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "active")],
        statuses(&answer),
        "RFC 7240 §2: the request is processed as if the token were absent"
    );
    assert_eq!(budget(2_000, 3_000), reported(&reply.text)?);
    assert_eq!(None, applied(&reply));
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn respond_async_never_exempts_a_request_from_the_overall_budget() -> TestResult {
    let per_node_ms = millis(SLACK)?;
    let overall = SLACK + Duration::from_millis(500);
    let a = node_answering("uid-at-a").await;
    let b = node_after("uid-at-b", overall + SLACK).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a, &b, per_node_ms, millis(overall)?)?;

    let reply = timed(app, post(&[RESPOND_ASYNC], false)?).await?;
    assert_eq!(StatusCode::GATEWAY_TIMEOUT, reply.status, "{}", reply.text);
    assert!(
        reply.took < overall + SLACK,
        "§11.7: respond-async does not exempt the gateway from its budget, took {:?}",
        reply.took
    );
    assert_answered_synchronously(&reply)?;
    let answer: Answer = serde_json::from_str(&reply.text)?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "time-out")],
        statuses(&answer)
    );
    assert_eq!(
        budget(per_node_ms, millis(overall)?),
        reported(&reply.text)?
    );
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn respond_async_beside_a_shorter_wait_still_shortens_the_budget() -> TestResult {
    let wait = wait_preference();
    let combined = format!("{RESPOND_ASYNC}, {wait}");
    let one_header: &[&str] = &[&combined];
    let two_headers: &[&str] = &[RESPOND_ASYNC, &wait];
    for prefer in [one_header, two_headers] {
        let (_a, _b, _dir, app) = shortened().await?;

        let reply = timed(app, post(prefer, false)?).await?;
        assert_eq!(
            StatusCode::GATEWAY_TIMEOUT,
            reply.status,
            "{prefer:?}: {}",
            reply.text
        );
        assert!(
            reply.took < WAIT + SLACK,
            "{prefer:?}: the gateway took {:?} under a {WAIT:?} client wait",
            reply.took
        );
        assert_answered_synchronously(&reply)?;
        let answer: Answer = serde_json::from_str(&reply.text)?;
        assert_eq!(
            vec![("node-a-pub", "active"), ("node-b-pub", "time-out")],
            statuses(&answer),
            "{prefer:?}"
        );
        let wait_ms = millis(WAIT)?;
        assert_eq!(
            budget(wait_ms, wait_ms),
            reported(&reply.text)?,
            "{prefer:?}"
        );
        assert_eq!(
            Some(wait.as_str()),
            applied(&reply),
            "{prefer:?}: only the wait was applied"
        );
    }
    Ok(())
}

/// A development gateway over node A and node B whose middleware, request
/// timeout included, comes from the same configuration as its budget:
/// `server` and `federation` are the keys of those tables.
fn configured(
    dir: &Path,
    a: &Server,
    b: &Server,
    server: &str,
    federation: &str,
) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(&a.uri(), &b.uri(), ""))?;
    let document = toml::Value::String(document.display().to_string());
    let rows = crossref(&[("node-a", EHR_A), ("node-b", EHR_B)]);
    let text = format!(
        "profile = \"development\"\n\n[server]\n{server}\n\n[registry]\ndocument = {document}\n\n[federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n{federation}\n\n{rows}"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    let mut server = settings.server.clone();
    server.auth = crate::support::auth();
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &server,
    ))
}

/// Asserts the §11.5 failure: a `504` carrying the envelope, node B
/// `time-out`, and never the request timeout's `408`.
fn assert_budget_answered(reply: &Reply) -> TestResult {
    assert_ne!(
        StatusCode::REQUEST_TIMEOUT,
        reply.status,
        "the request timeout never cuts the fan-out"
    );
    assert_eq!(StatusCode::GATEWAY_TIMEOUT, reply.status, "{}", reply.text);
    schema::validate(&reply.text)?;
    let answer: Answer = serde_json::from_str(&reply.text)?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "time-out")],
        statuses(&answer),
        "§11.4: the failing response still carries meta.federation.endpoints[]"
    );
    assert!(
        !answer.meta.federation.complete,
        "a failing answer is incomplete"
    );
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn a_slow_node_under_the_default_timeouts_answers_the_504_envelope() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_after("uid-at-b", Duration::from_secs(15)).await;
    let dir = tempfile::tempdir()?;
    let app = configured(dir.path(), &a, &b, "", "")?;

    let reply = timed(app, post(&[], false)?).await?;
    assert_budget_answered(&reply)?;
    assert!(
        reply.took < Duration::from_secs(25),
        "the default per-node timeout abandons node B, took {:?}",
        reply.took
    );
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn the_overall_budget_fires_before_the_tightest_accepted_request_timeout() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_after("uid-at-b", Duration::from_secs(10)).await;
    let dir = tempfile::tempdir()?;
    let overall_ms = millis(SLACK)?;
    let request_ms = overall_ms + COMBINING_MARGIN_MS + 1;
    let app = configured(
        dir.path(),
        &a,
        &b,
        &format!("request_timeout_ms = {request_ms}"),
        &format!("per_node_timeout_ms = 5000\noverall_timeout_ms = {overall_ms}"),
    )?;

    let reply = timed(app, post(&[], false)?).await?;
    assert_budget_answered(&reply)?;
    assert!(
        reply.took < Duration::from_millis(request_ms),
        "§11.5: answered within the budget plus combining time, took {:?}",
        reply.took
    );
    Ok(())
}

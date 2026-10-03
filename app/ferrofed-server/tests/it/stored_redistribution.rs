// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A held stored-query version sent again to the members that miss it,
//! against three mock nodes (§12.7 stored-query-versioning,
//! stored-query-fanout and stored-query-drift, N44, CP-40). A second `PUT` of
//! a held name and version is refused in every form and reaches no node. The
//! repair is the operator's `POST` on the admin listener, which exists only
//! where `[metrics] listen` is set: it sends the registry's held copy to the
//! members the targeting headers name and leaves the registry's copy as it
//! is. Every assertion on what a node received reads the node's own capture
//! (§16, track 10).
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
use ferrofed_server::admin;
use ferrofed_server::config::Config;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Method, Request, StatusCode};
use openehr_federation::status::EndpointStatus;
use openehr_its::rest::generated::definition::StoredQuery;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{PATIENT, registry, wire};
use crate::stored_fan_out::{
    COMMENT, ENDPOINT, NAME, Reported, VERSION, definition, get, node_path, put, state, statuses,
    storing, three,
};
use crate::support::{asked, call, error_body, exchange, field, mount};
use crate::template_fan_out::schema::validate_federation;

type TestResult = Result<(), Box<dyn Error>>;

/// A comment only a later `PUT` carries, which no node receives.
const SECOND_COMMENT: &str = "SYNTHETIC-COMMENT-9b4d";

/// The `[federation]` line that offers definition distribution.
const OFFERED: &str = "fan_out_stored_queries = true";

/// The query of [`definition`], laid out and commented differently.
fn relaid() -> String {
    definition()
        .replace(COMMENT, SECOND_COMMENT)
        .replace("\n  FROM", "\nFROM")
        .replace("SELECT c/uid/value", "SELECT   c/uid/value")
}

/// A query that differs from [`definition`] in its projection.
fn differing() -> String {
    definition().replace("SELECT c/uid/value", "SELECT c/name/value")
}

/// The gateway's client application and its admin listener's application
/// over node A, node B and node C, with `federation` in `[federation]`.
fn both(
    dir: &Path,
    [a, b, c]: [&Server; 3],
    federation: &str,
) -> Result<(Router, Router), Box<dyn Error>> {
    let state = state(dir, &three(a, b, c), federation)?;
    let client = ferrofed_server::router(Arc::clone(&state), &crate::facade::settings_with_room());
    Ok((client, admin::router(state)))
}

/// The operator's `POST` distributing [`NAME`] at `version`, naming `target`
/// when given, with `body`.
fn distribute(
    version: &str,
    target: Option<&str>,
    body: &str,
) -> Result<Request<Body>, http::Error> {
    let mut request = Request::post(format!("/admin/stored-queries/{NAME}/{version}/distribute"));
    if let Some(target) = target {
        request = request.header(ENDPOINT, target);
    }
    request.body(Body::from(body.to_owned()))
}

/// A node that answers its first definition `PUT` with `500` and every later
/// one with `200`.
async fn failing_once() -> Server {
    let server = Server::start().await;
    Mock::given(method("PUT"))
        .and(path(node_path()))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(&server, "PUT", node_path(), ResponseTemplate::new(200)).await;
    server
}

/// The bodies of the definition `PUT`s `server` received, in order; any
/// other request it received fails the call.
async fn puts(server: &Server) -> Result<Vec<String>, Box<dyn Error>> {
    let mut bodies = Vec::new();
    for request in server.received_requests().await.ok_or("recording is on")? {
        if request.method.as_str() != Method::PUT.as_str() || request.url.path() != node_path() {
            return Err(format!("{} {} was sent", request.method, request.url.path()).into());
        }
        assert_eq!(
            Some("query_type=AQL"),
            request.url.query(),
            "the query type"
        );
        bodies.push(String::from_utf8(request.body)?);
    }
    let composed = wire(server).await?;
    assert!(
        !composed.contains(COMMENT) && !composed.contains(SECOND_COMMENT),
        "§5.4.2: no comment reaches a node"
    );
    assert!(!composed.contains(PATIENT), "N33");
    Ok(bodies)
}

/// The registry's copy of [`NAME`] at [`VERSION`], read without naming a
/// member.
async fn registry_copy(app: &Router) -> Result<StoredQuery, Box<dyn Error>> {
    let (status, text) = call(app.clone(), get(None)?).await?;
    assert_eq!(StatusCode::OK, status, "the registry holds it: {text}");
    Ok(serde_json::from_str(&text)?)
}

/// Asserts that `app` answers `request` with `status` and `code`.
async fn refused(
    app: &Router,
    request: Request<Body>,
    (status, code): (StatusCode, &str),
) -> TestResult {
    let (answered, text) = call(app.clone(), request).await?;
    assert_eq!(status, answered, "{code}: {text}");
    assert_eq!(code, error_body(&text)?.code, "{text}");
    Ok(())
}

/// Asserts that none of `nodes` received anything.
async fn nothing_sent(nodes: [&Server; 3]) -> TestResult {
    for node in nodes {
        assert!(asked(node).await?.is_empty(), "§12.7: nothing is sent");
    }
    Ok(())
}

// conformance: CP-40
#[tokio::test]
async fn the_admin_action_sends_the_held_version_to_the_member_that_missed_it() -> TestResult {
    let (a, b, c) = (storing(200).await, storing(200).await, failing_once().await);
    let dir = tempfile::tempdir()?;
    let (app, operator) = both(dir.path(), [&a, &b, &c], OFFERED)?;
    let (status, text) = call(app.clone(), put(&definition(), Some("*"))?).await?;
    assert_eq!(StatusCode::MULTI_STATUS, status, "node C missed it: {text}");
    let before = registry_copy(&app).await?;

    let request = distribute(VERSION, Some("node-c-pub"), "")?;
    let (status, headers, body) = exchange(operator, request).await?;
    let text = String::from_utf8(body)?;
    assert_eq!(
        StatusCode::OK,
        status,
        "§12.7 stored-query-drift: the member that missed it accepts: {text}"
    );
    validate_federation(&text)?;
    let answer: Reported = serde_json::from_str(&text)?;
    assert_eq!(
        Some("held"),
        answer.meta.registry.as_deref(),
        "the answer says the registry held the version: {text}"
    );
    assert_eq!(
        (&before.q, &before.saved),
        (&answer.definition.q, &answer.definition.saved),
        "the answer is the registry's held copy: {text}"
    );
    assert!(answer.meta.federation.complete(), "{text}");
    assert_eq!(
        vec![
            ("node-a-pub".to_owned(), EndpointStatus::Excluded),
            ("node-b-pub".to_owned(), EndpointStatus::Excluded),
            ("node-c-pub".to_owned(), EndpointStatus::Active),
        ],
        statuses(&answer.meta.federation),
        "§9.5: only the named member is sent it: {text}"
    );
    assert_eq!(Some(VERSION), field(&headers, "location"), "the version");
    assert_eq!(Some("node-c-pub"), field(&headers, ENDPOINT));

    assert_eq!(
        vec![before.q.clone(), before.q.clone()],
        puts(&c).await?,
        "node C was sent the registry's copy twice"
    );
    for node in [&a, &b] {
        assert_eq!(vec![before.q.clone()], puts(node).await?, "sent it once");
    }
    let after = registry_copy(&app).await?;
    assert_eq!(
        (&before.q, &before.saved),
        (&after.q, &after.saved),
        "§12.7 stored-query-versioning: the registry's copy is unchanged"
    );
    Ok(())
}

// conformance: CP-40
#[tokio::test]
async fn the_admin_action_reaches_a_member_admitted_after_the_distribution() -> TestResult {
    let (a, b, c) = (storing(200).await, storing(200).await, storing(200).await);
    let dir = tempfile::tempdir()?;
    let before = {
        let two = registry(&a.uri(), &b.uri(), "");
        let app = crate::stored_fan_out::gateway(dir.path(), &two, OFFERED)?;
        let (status, text) = call(app.clone(), put(&definition(), Some("*"))?).await?;
        assert_eq!(StatusCode::OK, status, "both members accept: {text}");
        registry_copy(&app).await?
    };
    let (app, operator) = both(dir.path(), [&a, &b, &c], OFFERED)?;
    let (status, text) = call(operator, distribute(VERSION, Some("node-c-pub"), "")?).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "§12.7 stored-query-drift: the admitted member accepts: {text}"
    );
    let answer: Reported = serde_json::from_str(&text)?;
    assert_eq!(Some("held"), answer.meta.registry.as_deref(), "{text}");
    assert_eq!(
        vec![before.q.clone()],
        puts(&c).await?,
        "the registry's copy"
    );
    for node in [&a, &b] {
        assert_eq!(vec![before.q.clone()], puts(node).await?, "sent it once");
    }
    let after = registry_copy(&app).await?;
    assert_eq!(
        (&before.q, &before.saved),
        (&after.q, &after.saved),
        "§12.7 stored-query-versioning: the registry's copy is unchanged"
    );
    Ok(())
}

// conformance: CP-40
#[tokio::test]
async fn a_second_put_of_a_held_version_is_refused_in_every_form() -> TestResult {
    let (a, b, c) = (storing(200).await, storing(200).await, storing(200).await);
    let dir = tempfile::tempdir()?;
    let (app, _operator) = both(dir.path(), [&a, &b, &c], OFFERED)?;
    let (status, text) = call(app.clone(), put(&definition(), Some("*"))?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let before = registry_copy(&app).await?;
    for (aql, target) in [
        (definition(), Some("*")),
        (relaid(), Some("node-c-pub")),
        (differing(), Some("*")),
        (definition(), None),
        (relaid(), None),
    ] {
        let held = (StatusCode::CONFLICT, "stored-query-held");
        refused(&app, put(&aql, target)?, held).await?;
    }
    for node in [&a, &b, &c] {
        assert_eq!(
            vec![before.q.clone()],
            puts(node).await?,
            "§12.7: only the first PUT was distributed"
        );
    }
    let after = registry_copy(&app).await?;
    assert_eq!(
        (&before.q, &before.saved),
        (&after.q, &after.saved),
        "the held text stands"
    );
    Ok(())
}

// conformance: CP-40
#[tokio::test]
async fn the_admin_action_refuses_what_it_cannot_distribute_and_sends_nothing() -> TestResult {
    let (a, b, c) = (storing(200).await, storing(200).await, storing(200).await);
    let dir = tempfile::tempdir()?;
    let (app, operator) = both(dir.path(), [&a, &b, &c], OFFERED)?;
    let unknown = (StatusCode::NOT_FOUND, "stored-query-unknown");
    refused(&operator, distribute(VERSION, Some("*"), "")?, unknown).await?;
    let (status, text) = call(app, put(&definition(), None)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let cases = [
        (distribute("2.0.0", Some("*"), "")?, unknown),
        (
            distribute(VERSION, None, "")?,
            (StatusCode::BAD_REQUEST, "target-required"),
        ),
        (
            distribute(VERSION, Some("*"), &definition())?,
            (StatusCode::BAD_REQUEST, "body-invalid"),
        ),
        (
            distribute("1.0", Some("*"), "")?,
            (StatusCode::BAD_REQUEST, "query-version-invalid"),
        ),
    ];
    for (request, refusal) in cases {
        refused(&operator, request, refusal).await?;
    }
    nothing_sent([&a, &b, &c]).await
}

// conformance: CP-40
#[tokio::test]
async fn the_admin_action_refuses_a_held_targeted_definition() -> TestResult {
    let (a, b, c) = (storing(200).await, storing(200).await, storing(200).await);
    let dir = tempfile::tempdir()?;
    let (app, operator) = both(dir.path(), [&a, &b, &c], OFFERED)?;
    let targeted = definition().replacen("FROM ", r#"FROM ENDPOINT p ["node-a-pub"] CONTAINS "#, 1);
    let (status, text) = call(app, put(&targeted, None)?).await?;
    assert_eq!(StatusCode::OK, status, "§12.7: it MAY be stored: {text}");
    let refusal = (StatusCode::BAD_REQUEST, "definition-endpoint-targeted");
    refused(&operator, distribute(VERSION, Some("*"), "")?, refusal).await?;
    nothing_sent([&a, &b, &c]).await
}

// conformance: CP-40
#[tokio::test]
async fn without_distribution_offered_the_admin_action_is_refused() -> TestResult {
    let (a, b, c) = (storing(200).await, storing(200).await, storing(200).await);
    let dir = tempfile::tempdir()?;
    let (app, operator) = both(dir.path(), [&a, &b, &c], "")?;
    let (status, text) = call(app, put(&definition(), None)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let refusal = (StatusCode::BAD_REQUEST, "stored-query-fan-out-unsupported");
    refused(&operator, distribute(VERSION, Some("*"), "")?, refusal).await?;
    nothing_sent([&a, &b, &c]).await
}

// conformance: CP-40
#[tokio::test]
async fn a_redistribution_one_member_rejects_is_partial_and_the_registry_is_unchanged() -> TestResult
{
    let (a, b, c) = (storing(200).await, storing(200).await, storing(500).await);
    let dir = tempfile::tempdir()?;
    let (app, operator) = both(dir.path(), [&a, &b, &c], OFFERED)?;
    let (status, text) = call(app.clone(), put(&definition(), None)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let before = registry_copy(&app).await?;
    let (status, text) = call(operator, distribute(VERSION, Some("*"), "")?).await?;
    assert_eq!(
        StatusCode::MULTI_STATUS,
        status,
        "§12.6 item 3: a partial success, never overall success: {text}"
    );
    validate_federation(&text)?;
    let answer: Reported = serde_json::from_str(&text)?;
    assert!(!answer.meta.federation.complete(), "{text}");
    assert_eq!(Some("held"), answer.meta.registry.as_deref(), "{text}");
    assert_eq!(
        vec![
            ("node-a-pub".to_owned(), EndpointStatus::Active),
            ("node-b-pub".to_owned(), EndpointStatus::Active),
            ("node-c-pub".to_owned(), EndpointStatus::NodeError),
        ],
        statuses(&answer.meta.federation),
        "§9.5: the failing member is named: {text}"
    );
    let after = registry_copy(&app).await?;
    assert_eq!(
        (&before.q, &before.saved),
        (&after.q, &after.saved),
        "the registry's copy is unchanged"
    );
    Ok(())
}

// conformance: CP-40
#[tokio::test]
async fn a_redistribution_every_member_rejects_is_a_424_and_the_registry_is_unchanged() -> TestResult
{
    let (a, b, c) = (storing(409).await, storing(400).await, storing(409).await);
    let dir = tempfile::tempdir()?;
    let (app, operator) = both(dir.path(), [&a, &b, &c], OFFERED)?;
    let (status, text) = call(app.clone(), put(&definition(), None)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let before = registry_copy(&app).await?;
    let (status, text) = call(operator, distribute(VERSION, Some("*"), "")?).await?;
    assert_eq!(
        StatusCode::FAILED_DEPENDENCY,
        status,
        "§11.2: every member refused: {text}"
    );
    let answer: Reported = serde_json::from_str(&text)?;
    assert_eq!(Some("held"), answer.meta.registry.as_deref(), "{text}");
    let after = registry_copy(&app).await?;
    assert_eq!(
        (&before.q, &before.saved),
        (&after.q, &after.saved),
        "the registry's copy is unchanged"
    );
    Ok(())
}

/// The state of a gateway with no registry, whose `[metrics]` table is
/// `metrics`.
fn bare(metrics: &str) -> Result<(Arc<AppState>, Option<std::net::SocketAddr>), Box<dyn Error>> {
    let settings = Config::from_sources(Some(&crate::support::signed(metrics)), &BTreeMap::new())?
        .resolve()?;
    let listen = settings.metrics.listen;
    let state = Arc::new(AppState::build(&settings)?);
    let listener = admin::listener(&settings.metrics, &state).map(|(address, _app)| address);
    assert_eq!(listen, listener, "the admin listener is metrics.listen");
    Ok((state, listener))
}

// conformance: CP-40
#[tokio::test]
async fn the_admin_action_exists_only_on_the_admin_listener() -> TestResult {
    let (state, listener) = bare("")?;
    assert_eq!(None, listener, "no admin listener, so no admin action");
    let client = ferrofed_server::router(state, &crate::facade::settings_with_room());
    let (status, _) = call(client, distribute(VERSION, Some("*"), "")?).await?;
    assert_eq!(
        StatusCode::NOT_FOUND,
        status,
        "the client listener never serves the admin action"
    );
    let (_state, listener) = bare("[metrics]\nlisten = \"127.0.0.1:9464\"\n")?;
    assert_eq!(Some("127.0.0.1:9464".parse()?), listener);
    Ok(())
}

// conformance: CP-40
#[test]
fn the_admin_listener_is_refused_off_loopback_without_allow_remote() -> TestResult {
    let remote = "[metrics]\nlisten = \"192.0.2.10:9464\"\n";
    let refused = Config::from_sources(Some(&crate::support::signed(remote)), &BTreeMap::new())?
        .resolve()
        .err()
        .ok_or("a remote admin listener is refused")?;
    assert!(
        refused.to_string().contains("metrics.allow_remote"),
        "{refused}"
    );
    let settings = Config::from_sources(
        Some(&crate::support::signed(&format!(
            "{remote}allow_remote = true\n"
        ))),
        &BTreeMap::new(),
    )?
    .resolve()?;
    assert_eq!(Some("192.0.2.10:9464".parse()?), settings.metrics.listen);
    Ok(())
}

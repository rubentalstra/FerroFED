// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Fan-out template upload against three mock nodes (§12.6, N43, CP-34): off
//! by default, so `*` stays a `400` and nothing is sent; offered, a template
//! upload naming `*` or several endpoints reaches each member independently,
//! byte-identical, and is answered per node in the `meta.federation` shape,
//! partial success as `207`, never as overall success, with nothing rolled
//! back, a rejecting node's message carried after its status and no other
//! node body copied into the answer. A plain upload still names
//! its one node, and every other definition request still routes to one node.
//! Every assertion on what a node received reads the node's own capture
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
use ferrofed_server::config::Config;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Method, Request, StatusCode, header};
use openehr_federation::headers::{ENDPOINT, SYSTEM_ID};
use openehr_federation::meta::FederationMeta;
use openehr_federation::outcome::ErrorDetail;
use openehr_federation::status::EndpointStatus;
use serde::Deserialize;
use wiremock::ResponseTemplate;

use crate::facade::{PATIENT, registry, settings_with_room, wire};
use crate::support::{asked, error_body, exchange, field, mount, observed, states};

type TestResult = Result<(), Box<dyn Error>>;

/// The ADL 1.4 template collection (ITS-REST Definition API).
const ADL14: &str = "/v1/definition/template/adl1.4";

/// The ADL 2 template collection (ITS-REST Definition API).
const ADL2: &str = "/v1/definition/template/adl2";

/// A synthetic template id.
const TEMPLATE: &str = "synthetic.fan_out.v1";

/// The client's own credential, which no node ever sees.
use crate::support::CLIENT_TOKEN;

/// A body an accepting node answers with, which the answer never copies.
const ACCEPTED_BODY: &str = "SYNTHETIC-ACCEPTED-BODY-41fd";

/// The message a rejecting node answers with, which the answer carries after
/// the node's status (§9.5, §11.1).
const REJECTED_BODY: &str = "SYNTHETIC-REJECTED-BODY-9b3e";

/// The third member, beside node A and node B of [`registry`].
const THIRD: &str = r#"
[[organisation]]
id = "org-c"

[[node]]
id = "node-c"
organisation = "org-c"
system_id = "cdr-c.example.org"
"#;

/// The registry of node A, node B and node C at `a`, `b` and `c`.
fn three(a: &Server, b: &Server, c: &Server) -> String {
    let endpoint = format!(
        r#"{THIRD}
[[endpoint]]
id = "node-c-pub"
node = "node-c"
url = "{}"
connection_type = "openehr-rest-query"
managing_organisation = "org-c"
"#,
        c.uri()
    );
    registry(&a.uri(), &b.uri(), &endpoint)
}

/// The gateway over `registry`, with `federation` in its `[federation]`
/// table.
fn gateway(dir: &Path, registry: &str, federation: &str) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry)?;
    let document = toml::Value::String(document.display().to_string());
    let text = format!(
        "[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n{federation}\n"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// The gateway over the three nodes with template fan-out offered.
fn offered(dir: &Path, nodes: [&Server; 3]) -> Result<Router, Box<dyn Error>> {
    let [a, b, c] = nodes;
    gateway(dir, &three(a, b, c), "fan_out_template_upload = true")
}

/// A synthetic operational template.
fn template() -> String {
    format!(
        "<template xmlns=\"http://schemas.openehr.org/v1\">\n  <template_id><value>{TEMPLATE}</value></template_id>\n   <concept>Synthétic   fan-out</concept>\n</template>\n"
    )
}

/// A request of `verb` to `at` with the body `sent`, naming `target` in the
/// endpoint header when given, and carrying a client credential and a
/// patient-shaped header no node may receive.
fn request(
    verb: &Method,
    at: &str,
    target: Option<&str>,
    sent: &str,
) -> Result<Request<Body>, http::Error> {
    let mut request = Request::builder()
        .method(verb.clone())
        .uri(at)
        .header(header::AUTHORIZATION, format!("Bearer {}", *CLIENT_TOKEN))
        .header("x-patient", PATIENT);
    if let Some(target) = target {
        request = request.header(ENDPOINT, target);
    }
    if !sent.is_empty() {
        let media = if at.starts_with(ADL14) {
            "application/xml"
        } else {
            "text/plain"
        };
        request = request.header(header::CONTENT_TYPE, media);
    }
    request.body(Body::from(sent.to_owned()))
}

/// An upload of the synthetic template to `at`, naming `target`.
fn upload(at: &str, target: Option<&str>) -> Result<Request<Body>, http::Error> {
    request(&Method::POST, at, target, &template())
}

/// A node that accepts a template upload to `at` with `201`.
async fn accepting(at: &str) -> Server {
    let server = Server::start().await;
    let created = format!("{}{at}/{TEMPLATE}", server.uri());
    mount(
        &server,
        "POST",
        at.to_owned(),
        ResponseTemplate::new(201)
            .insert_header("Location", created.as_str())
            .set_body_raw(ACCEPTED_BODY.as_bytes().to_vec(), "text/plain"),
    )
    .await;
    server
}

/// A node that rejects a template upload to `at` with `status` and its own
/// error body.
async fn rejecting(at: &str, status: u16) -> Server {
    let server = Server::start().await;
    let body = format!(r#"{{"message":"{REJECTED_BODY}"}}"#);
    mount(
        &server,
        "POST",
        at.to_owned(),
        ResponseTemplate::new(status).set_body_raw(body.into_bytes(), "application/json"),
    )
    .await;
    server
}

/// The answer of a fan-out upload, read through the wire type.
#[derive(Debug, Deserialize)]
struct Uploaded {
    meta: Meta,
}

/// The `meta` of [`Uploaded`].
#[derive(Debug, Deserialize)]
struct Meta {
    federation: FederationMeta,
}

/// Each endpoint the answer reports, with its status, in answer order.
fn reported(meta: &FederationMeta) -> Vec<(String, EndpointStatus)> {
    meta.endpoints()
        .iter()
        .map(|outcome| (outcome.id().as_str().to_owned(), outcome.status()))
        .collect()
}

/// Asserts that `server` received exactly one upload to `at`, carrying
/// `sent` byte for byte, and nothing else: nothing was rolled back.
async fn uploaded_once(server: &Server, at: &str, sent: &str) -> TestResult {
    let requests = server.received_requests().await.ok_or("recording is on")?;
    let [only] = requests.as_slice() else {
        return Err(format!("one request, not {}", requests.len()).into());
    };
    assert_eq!(Method::POST.as_str(), only.method.as_str(), "no rollback");
    assert_eq!(at, only.url.path(), "the upload path");
    assert_eq!(sent.as_bytes(), only.body.as_slice(), "byte-identical");
    let composed = wire(server).await?;
    assert!(!composed.contains(PATIENT), "N33: {at}");
    assert!(!composed.contains(CLIENT_TOKEN.as_str()), "N33: {at}");
    Ok(())
}

/// Asserts that `app` refuses `request` with `400` and `code`, and that no
/// node in `nodes` received anything.
async fn refused(
    app: Router,
    request: Request<Body>,
    code: &str,
    nodes: [&Server; 3],
) -> TestResult {
    let (status, headers, body) = exchange(app, request).await?;
    let text = String::from_utf8(body)?;
    assert_eq!(StatusCode::BAD_REQUEST, status, "{text}");
    assert_eq!(code, error_body(&text)?.code, "{text}");
    assert_eq!(None, field(&headers, ENDPOINT), "no endpoint acted: {text}");
    for node in nodes {
        assert!(asked(node).await?.is_empty(), "no node received anything");
    }
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn with_the_setting_off_a_star_upload_is_refused_and_nothing_is_sent() -> TestResult {
    for at in [ADL14, ADL2] {
        for (federation, target, code) in [
            ("", "*", "endpoint-unknown"),
            ("", "node-a-pub, node-b-pub", "endpoint-several"),
            ("fan_out_template_upload = false", "*", "endpoint-unknown"),
        ] {
            let (a, b, c) = (
                accepting(at).await,
                accepting(at).await,
                accepting(at).await,
            );
            let dir = tempfile::tempdir()?;
            let app = gateway(dir.path(), &three(&a, &b, &c), federation)?;
            refused(app, upload(at, Some(target))?, code, [&a, &b, &c]).await?;
        }
    }
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn with_one_member_rejecting_the_answer_is_partial_and_names_it() -> TestResult {
    for at in [ADL14, ADL2] {
        let a = accepting(at).await;
        let b = accepting(at).await;
        let c = rejecting(at, 400).await;
        let dir = tempfile::tempdir()?;
        let app = offered(dir.path(), [&a, &b, &c])?;
        let (status, headers, body) = exchange(app, upload(at, Some("*"))?).await?;
        let text = String::from_utf8(body)?;
        assert_eq!(
            StatusCode::MULTI_STATUS,
            status,
            "§12.6: a partial success, never overall success: {text}"
        );
        schema::validate_federation(&text)?;
        let answer: Uploaded = serde_json::from_str(&text)?;
        let meta = &answer.meta.federation;
        assert!(!meta.complete(), "§12.6: complete is false: {text}");
        assert_eq!(
            vec![
                ("node-a-pub".to_owned(), EndpointStatus::Active),
                ("node-b-pub".to_owned(), EndpointStatus::Active),
                ("node-c-pub".to_owned(), EndpointStatus::NodeError),
            ],
            reported(meta),
            "§9.5: one entry per member, the rejecting one named: {text}"
        );
        let rejected = meta
            .endpoints()
            .iter()
            .find(|outcome| outcome.id().as_str() == "node-c-pub")
            .ok_or("node C is reported")?;
        let Some(ErrorDetail::Text(error)) = rejected.outcome().error() else {
            return Err(format!("§9.5: a node-error carries its error: {text}").into());
        };
        assert!(error.contains("400"), "§9.5: the node's status: {error}");
        assert_eq!(
            Some("node-a-pub, node-b-pub"),
            field(&headers, ENDPOINT),
            "§7a.3: the members that accepted"
        );
        assert_eq!(
            Some("cdr-a.example.org, cdr-b.example.org"),
            field(&headers, SYSTEM_ID),
            "§7a.3: their system ids"
        );
        let sent = template();
        uploaded_once(&a, at, &sent).await?;
        uploaded_once(&b, at, &sent).await?;
        uploaded_once(&c, at, &sent).await?;
    }
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn every_member_accepting_answers_success_with_every_member_named() -> TestResult {
    let at = ADL14;
    let (a, b, c) = (
        accepting(at).await,
        accepting(at).await,
        accepting(at).await,
    );
    let dir = tempfile::tempdir()?;
    let (status, headers, body) =
        exchange(offered(dir.path(), [&a, &b, &c])?, upload(at, Some("*"))?).await?;
    let text = String::from_utf8(body)?;
    assert_eq!(StatusCode::OK, status, "{text}");
    schema::validate_federation(&text)?;
    let answer: Uploaded = serde_json::from_str(&text)?;
    assert!(answer.meta.federation.complete(), "{text}");
    assert_eq!(
        Some("node-a-pub, node-b-pub, node-c-pub"),
        field(&headers, ENDPOINT),
        "§7a.3: every member accepted"
    );
    for node in [&a, &b, &c] {
        uploaded_once(node, at, &template()).await?;
    }
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn a_named_subset_reaches_only_its_members_and_reports_the_rest_excluded() -> TestResult {
    let at = ADL2;
    let (a, b, c) = (
        accepting(at).await,
        accepting(at).await,
        accepting(at).await,
    );
    let dir = tempfile::tempdir()?;
    let request = upload(at, Some("node-c-pub, node-a-pub"))?;
    let (status, headers, body) = exchange(offered(dir.path(), [&a, &b, &c])?, request).await?;
    let text = String::from_utf8(body)?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let answer: Uploaded = serde_json::from_str(&text)?;
    assert_eq!(
        vec![
            ("node-a-pub".to_owned(), EndpointStatus::Active),
            ("node-b-pub".to_owned(), EndpointStatus::Excluded),
            ("node-c-pub".to_owned(), EndpointStatus::Active),
        ],
        reported(&answer.meta.federation),
        "§8.1, §11.1: the member not named is excluded: {text}"
    );
    assert!(answer.meta.federation.complete(), "{text}");
    assert_eq!(
        Some("node-a-pub, node-c-pub"),
        field(&headers, ENDPOINT),
        "§7a.3: the named members accepted"
    );
    uploaded_once(&a, at, &template()).await?;
    uploaded_once(&c, at, &template()).await?;
    assert!(asked(&b).await?.is_empty(), "node B is never asked");
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn every_member_rejecting_is_a_failed_dependency_naming_each() -> TestResult {
    let at = ADL14;
    let (a, b, c) = (
        rejecting(at, 400).await,
        rejecting(at, 409).await,
        rejecting(at, 422).await,
    );
    let dir = tempfile::tempdir()?;
    let (status, headers, body) =
        exchange(offered(dir.path(), [&a, &b, &c])?, upload(at, Some("*"))?).await?;
    let text = String::from_utf8(body)?;
    assert_eq!(StatusCode::FAILED_DEPENDENCY, status, "§11.2: {text}");
    schema::validate_federation(&text)?;
    let answer: Uploaded = serde_json::from_str(&text)?;
    assert!(
        reported(&answer.meta.federation)
            .iter()
            .all(|(_, status)| *status == EndpointStatus::NodeError),
        "{text}"
    );
    assert_eq!(None, field(&headers, ENDPOINT), "no member accepted");
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn a_member_past_its_timeout_is_reported_and_the_others_keep_the_template() -> TestResult {
    let at = ADL14;
    let a = accepting(at).await;
    let b = accepting(at).await;
    let c = Server::start().await;
    mount(
        &c,
        "POST",
        at.to_owned(),
        ResponseTemplate::new(201).set_delay(std::time::Duration::from_millis(2500)),
    )
    .await;
    let dir = tempfile::tempdir()?;
    let (status, _, body) =
        exchange(offered(dir.path(), [&a, &b, &c])?, upload(at, Some("*"))?).await?;
    let text = String::from_utf8(body)?;
    assert_eq!(StatusCode::MULTI_STATUS, status, "§12.6: {text}");
    let answer: Uploaded = serde_json::from_str(&text)?;
    assert_eq!(
        vec![
            ("node-a-pub".to_owned(), EndpointStatus::Active),
            ("node-b-pub".to_owned(), EndpointStatus::Active),
            ("node-c-pub".to_owned(), EndpointStatus::TimeOut),
        ],
        reported(&answer.meta.federation),
        "§11.1, §11.5: the late member is time-out: {text}"
    );
    uploaded_once(&a, at, &template()).await?;
    uploaded_once(&b, at, &template()).await?;
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn a_rejecting_nodes_message_is_carried_and_no_other_body_is() -> TestResult {
    let at = ADL14;
    let a = accepting(at).await;
    let b = rejecting(at, 400).await;
    let c = rejecting(at, 500).await;
    let dir = tempfile::tempdir()?;
    let (status, headers, body) =
        exchange(offered(dir.path(), [&a, &b, &c])?, upload(at, Some("*"))?).await?;
    let text = String::from_utf8(body)?;
    assert_eq!(StatusCode::MULTI_STATUS, status, "{text}");
    assert!(
        !text.contains(ACCEPTED_BODY),
        "an accepting node's body: {text}"
    );
    let answer: Uploaded = serde_json::from_str(&text)?;
    let errors: Vec<(&str, Option<&ErrorDetail>)> = answer
        .meta
        .federation
        .endpoints()
        .iter()
        .map(|outcome| (outcome.id().as_str(), outcome.outcome().error()))
        .collect();
    let said =
        |status: &str| ErrorDetail::Text(format!("the node answered {status}: {REJECTED_BODY}"));
    assert_eq!(
        vec![
            ("node-a-pub", None),
            ("node-b-pub", Some(&said("400 Bad Request"))),
            ("node-c-pub", Some(&said("500 Internal Server Error"))),
        ],
        errors,
        "§9.5, §11.1: a rejecting node's status, then its own message: {text}"
    );
    assert!(
        !text.contains(TEMPLATE),
        "the node's Location or the template: {text}"
    );
    assert_eq!(None, field(&headers, header::LOCATION.as_str()), "{text}");
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn a_plain_upload_naming_one_endpoint_routes_to_that_node_alone() -> TestResult {
    let at = ADL14;
    let (a, b, c) = (
        accepting(at).await,
        accepting(at).await,
        accepting(at).await,
    );
    let dir = tempfile::tempdir()?;
    let request = upload(at, Some("node-b-pub"))?;
    let (status, headers, body) = exchange(offered(dir.path(), [&a, &b, &c])?, request).await?;
    assert_eq!(StatusCode::CREATED, status, "§12.6: the node's own answer");
    assert_eq!(ACCEPTED_BODY.as_bytes(), body.as_slice(), "routed as is");
    assert_eq!(Some("node-b-pub"), field(&headers, ENDPOINT), "N31");
    uploaded_once(&b, at, &template()).await?;
    assert!(asked(&a).await?.is_empty(), "node A is never asked");
    assert!(asked(&c).await?.is_empty(), "node C is never asked");
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn a_plain_upload_naming_no_endpoint_never_fans_out() -> TestResult {
    for at in [ADL14, ADL2] {
        let (a, b, c) = (
            accepting(at).await,
            accepting(at).await,
            accepting(at).await,
        );
        let dir = tempfile::tempdir()?;
        let app = offered(dir.path(), [&a, &b, &c])?;
        refused(app, upload(at, None)?, "target-required", [&a, &b, &c]).await?;
    }
    Ok(())
}

// conformance: CP-34
#[tokio::test]
async fn a_definition_request_other_than_an_upload_still_routes_to_one_node() -> TestResult {
    let query = "/v1/definition/query/org.example::synthetic_compositions/1.0.0";
    for (verb, at, sent, target, code) in [
        (Method::GET, ADL14, String::new(), "*", "endpoint-unknown"),
        (
            Method::GET,
            ADL2,
            String::new(),
            "node-a-pub, node-b-pub",
            "endpoint-several",
        ),
        (
            Method::GET,
            "/v1/definition/template/adl1.4/synthetic.fan_out.v1",
            String::new(),
            "*",
            "endpoint-unknown",
        ),
        (
            Method::PUT,
            query,
            "SELECT c FROM EHR e CONTAINS COMPOSITION c".to_owned(),
            "*",
            "endpoint-unknown",
        ),
    ] {
        let (a, b, c) = (
            accepting(at).await,
            accepting(at).await,
            accepting(at).await,
        );
        let dir = tempfile::tempdir()?;
        let app = offered(dir.path(), [&a, &b, &c])?;
        refused(
            app,
            request(&verb, at, Some(target), &sent)?,
            code,
            [&a, &b, &c],
        )
        .await?;
    }
    Ok(())
}

// conformance: CP-34 CP-28
#[tokio::test]
async fn a_star_beside_an_organisation_naming_fewer_members_is_a_conflict() -> TestResult {
    let at = ADL14;
    let (a, b, c) = (
        accepting(at).await,
        accepting(at).await,
        accepting(at).await,
    );
    let dir = tempfile::tempdir()?;
    let mut sent = upload(at, Some("*"))?;
    sent.headers_mut()
        .insert("openEHR-federation-organisation", "org-a".parse()?);
    refused(
        offered(dir.path(), [&a, &b, &c])?,
        sent,
        "targeting-conflict",
        [&a, &b, &c],
    )
    .await
}

#[tokio::test]
async fn a_fan_out_upload_records_each_members_state_on_the_dependencies() -> TestResult {
    let at = ADL14;
    let a = accepting(at).await;
    let b = rejecting(at, 500).await;
    let c = Server::start().await;
    let late = ResponseTemplate::new(201).set_delay(std::time::Duration::from_millis(2500));
    mount(&c, "POST", at.to_owned(), late).await;
    let dir = tempfile::tempdir()?;
    let app = offered(dir.path(), [&a, &b, &c])?;
    assert_eq!(
        states(&[
            ("node-a-pub", "unknown"),
            ("node-b-pub", "unknown"),
            ("node-c-pub", "unknown"),
        ]),
        observed(&app).await?,
        "nothing asked yet"
    );
    let (status, _, body) = exchange(app.clone(), upload(at, Some("*"))?).await?;
    assert_eq!(
        StatusCode::MULTI_STATUS,
        status,
        "{}",
        String::from_utf8(body)?
    );
    assert_eq!(
        states(&[
            ("node-a-pub", "up"),
            ("node-b-pub", "failing"),
            ("node-c-pub", "down"),
        ]),
        observed(&app).await?,
        "each member as the upload found it"
    );
    Ok(())
}

#[tokio::test]
async fn a_member_rejecting_the_upload_answered_and_one_not_named_is_not_observed() -> TestResult {
    let at = ADL2;
    let (a, b, c) = (
        rejecting(at, 422).await,
        rejecting(at, 503).await,
        accepting(at).await,
    );
    let dir = tempfile::tempdir()?;
    let app = offered(dir.path(), [&a, &b, &c])?;
    let (status, _, body) =
        exchange(app.clone(), upload(at, Some("node-a-pub, node-b-pub"))?).await?;
    assert_eq!(
        StatusCode::FAILED_DEPENDENCY,
        status,
        "{}",
        String::from_utf8(body)?
    );
    assert_eq!(
        states(&[
            ("node-a-pub", "up"),
            ("node-b-pub", "failing"),
            ("node-c-pub", "unknown"),
        ]),
        observed(&app).await?,
        "a client error is an answer, a server error a failure, and node C was not asked"
    );
    Ok(())
}

/// Validation of the answer's `meta.federation` against the vendored
/// result-set schema's `federationMeta` definition.
pub(crate) mod schema {
    #![expect(
        clippy::disallowed_types,
        reason = "the test seam: schema validation reads JSON as values, in tests only"
    )]

    use std::error::Error;

    use serde_json::{Value, json};

    /// The vendored result-envelope schema.
    const RESULT_SET_SCHEMA: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/specs/federation-spec/modules/ROOT/attachments/federated-result-set.schema.json"
    );

    /// Validates `meta.federation` of the JSON `text` against
    /// `$defs/federationMeta`, formats included.
    pub(crate) fn validate_federation(text: &str) -> Result<(), Box<dyn Error>> {
        let schema: Value = serde_json::from_str(&std::fs::read_to_string(RESULT_SET_SCHEMA)?)?;
        let rooted = json!({
            "$schema": schema.get("$schema").ok_or("the schema names its dialect")?,
            "$defs": schema.get("$defs").ok_or("the schema has definitions")?,
            "$ref": "#/$defs/federationMeta",
        });
        let validator = jsonschema::options()
            .should_validate_formats(true)
            .build(&rooted)?;
        let instance: Value = serde_json::from_str(text)?;
        let federation = instance
            .pointer("/meta/federation")
            .ok_or("the answer carries meta.federation")?;
        let errors: Vec<String> = validator
            .iter_errors(federation)
            .map(|error| format!("{} at {}", error, error.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; ").into())
        }
    }
}

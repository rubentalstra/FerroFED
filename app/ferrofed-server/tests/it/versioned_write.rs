// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Follow-up writes (§12.4, §12a.1, N23, CP-15): a versioned write reaches
//! only the CDR that controls the version it amends, a write its path node
//! does not control is refused and reaches no node, and a new EHR is created
//! at exactly one explicitly named node and is never probed for (N41).
//!
//! A versioned write is EHR-scoped, so its node is the one its path `ehr_id`
//! routes to (§12a.1 `route-ehr`, N41), and the gateway sends it only when the
//! registry routes the preceding version's `creating_system_id` to that same
//! node (§12a.1 `route-write`). Every assertion on what a node received reads
//! the node's own capture, never the gateway's logs (§16, track 6 and
//! track 10).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;

use axum::Router;
use axum::body::Body;
use ferrofed_engine::onward::conveyance;
use ferrofed_testkit::mock::Server;
use http::{Method, Request, StatusCode, header};
use wiremock::ResponseTemplate;

use crate::facade::{
    CONVEYED, EHR_A, EHR_B, PATIENT, body, gateway, node_answering, post, registry,
};
use crate::path_ehr_id::{answer, probe_at};
use crate::support::{asked, error_body, mount, searched_claims, send};

type TestResult = Result<(), Box<dyn Error>>;

pub(crate) const ENDPOINT_A: &str = "node-a-pub";
pub(crate) const ENDPOINT_B: &str = "node-b-pub";

/// A version node A created.
pub(crate) const CREATED_AT_A: &str = "8849182c-82ad-4088-a07f-48ead4180515::cdr-a.example.org::1";
/// The version node A answers a write of [`CREATED_AT_A`] with.
const SECOND_AT_A: &str = "8849182c-82ad-4088-a07f-48ead4180515::cdr-a.example.org::2";
/// The versioned object [`CREATED_AT_A`] is a version of.
const OBJECT_AT_A: &str = "8849182c-82ad-4088-a07f-48ead4180515";
/// A version of a system the registry does not map.
pub(crate) const CREATED_ELSEWHERE: &str =
    "5c3e9b1a-7d2f-4e8a-9b6c-1f0e2d3c4b5a::external.example.org::3";
/// A version of a retired system the registry maps to node A.
pub(crate) const CREATED_BY_LEGACY: &str =
    "7a6b5c4d-3e2f-4a1b-8c9d-0e1f2a3b4c5d::legacy-a.example.org::4";

/// The registry mapping of the retired system of [`CREATED_BY_LEGACY`].
pub(crate) const LEGACY_MAPPING: &str = "\n[[creating_system]]\ncreating_system_id = \"legacy-a.example.org\"\nendpoint = \"node-a-pub\"\n";

/// The client's own credential, which no node ever sees.
use crate::support::CLIENT_TOKEN;

/// The gateway over node A and node B, with the registry `extra` appended.
pub(crate) fn over(
    dir: &std::path::Path,
    a: &Server,
    b: &Server,
    extra: &str,
) -> Result<Router, Box<dyn Error>> {
    gateway(dir, &registry(&a.uri(), &b.uri(), extra), "", "")
}

/// `If-Match` naming `version` as ITS-REST writes it: quoted.
fn quoted(version: &str) -> String {
    format!("\"{version}\"")
}

/// A versioned write of `verb` to `at`, naming `endpoint` as its target when
/// given and `version` in `If-Match` when given.
pub(crate) fn versioned(
    verb: &Method,
    at: &str,
    endpoint: Option<&str>,
    version: Option<&str>,
    sent: &str,
) -> Result<Request<Body>, http::Error> {
    let mut request = Request::builder()
        .method(verb.clone())
        .uri(at)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(endpoint) = endpoint {
        request = request.header("openEHR-federation-endpoint", endpoint);
    }
    if let Some(version) = version {
        request = request.header(header::IF_MATCH, quoted(version));
    }
    request.body(Body::from(sent.to_owned()))
}

/// The versioned writes of ITS-REST 1.1.0 under the EHR of [`EHR_A`] that
/// name their preceding version in `If-Match`.
fn if_match_writes() -> Vec<(Method, String)> {
    vec![
        (
            Method::PUT,
            format!("/v1/ehr/{EHR_A}/composition/{OBJECT_AT_A}"),
        ),
        (Method::PUT, format!("/v1/ehr/{EHR_A}/ehr_status")),
        (Method::PUT, format!("/v1/ehr/{EHR_A}/directory")),
        (Method::DELETE, format!("/v1/ehr/{EHR_A}/directory")),
    ]
}

/// A `COMPOSITION` carrying the patient's identifier as a `DV_IDENTIFIER`,
/// spaced as a re-serialisation would not keep it.
fn composition() -> String {
    format!(
        "{{ \"_type\":\"COMPOSITION\",\n  \"composer\":{{\"_type\":\"PARTY_IDENTIFIED\",\"name\":\"Synthetic clinician\",\n   \"identifiers\":[{{\"_type\":\"DV_IDENTIFIER\",\"issuer\":\"urn:oid:2.999.1\",\"id\":\"{PATIENT}\",\"type\":\"MR\"}}]}},\n  \"note\": \"é ünïcode   kept\"\n}}\n"
    )
}

/// The request target and every header `server` received, without the
/// bodies: what the gateway composes for a node (§5.4.1, N33), the
/// gateway's own `openEHR-federation-client` token read as its claims.
pub(crate) async fn outside_bodies(server: &Server) -> Result<String, Box<dyn Error>> {
    let requests = server.received_requests().await.ok_or("recording is on")?;
    let mut text = String::new();
    for request in requests {
        text.push_str(request.url.as_str());
        for (name, value) in &request.headers {
            if name.as_str().eq_ignore_ascii_case(conveyance::HEADER) {
                text.push_str(CONVEYED);
                text.push_str(&searched_claims(value.to_str()?)?);
                continue;
            }
            text.push_str(name.as_str());
            text.push_str(&String::from_utf8_lossy(value.as_bytes()));
        }
    }
    Ok(text)
}

/// Asserts that `request` is refused with `status` and `code`, names no
/// acting endpoint, and that neither node received anything.
pub(crate) async fn refused_at_neither(
    app: Router,
    request: Request<Body>,
    (status, code): (StatusCode, &str),
    (a, b): (&Server, &Server),
) -> Result<String, Box<dyn Error>> {
    let (answered, acting, text) = answer(app, request).await?;
    assert_eq!(status, answered, "{text}");
    assert_eq!(code, error_body(&text)?.code, "{text}");
    assert!(acting.is_none(), "no endpoint acted: {text}");
    assert!(asked(a).await?.is_empty(), "node A received nothing");
    assert!(asked(b).await?.is_empty(), "node B received nothing");
    Ok(text)
}

// conformance: CP-15
#[tokio::test]
async fn a_versioned_write_reaches_its_controlling_cdr_and_no_other_node() -> TestResult {
    let mut writes = if_match_writes();
    writes.push((
        Method::DELETE,
        format!("/v1/ehr/{EHR_A}/composition/{CREATED_AT_A}"),
    ));
    for (verb, at) in writes {
        let a = Server::start().await;
        mount(
            &a,
            verb.as_str(),
            at.clone(),
            ResponseTemplate::new(200).insert_header("ETag", quoted(SECOND_AT_A).as_str()),
        )
        .await;
        let b = Server::start().await;
        let dir = tempfile::tempdir()?;
        let preceding = (verb == Method::PUT || at.ends_with("directory")).then_some(CREATED_AT_A);
        let request = versioned(&verb, &at, Some(ENDPOINT_A), preceding, "{}")?;
        let (status, acting, text) = answer(over(dir.path(), &a, &b, "")?, request).await?;
        assert_eq!(StatusCode::OK, status, "{verb} {at}: {text}");
        assert_eq!(Some(ENDPOINT_A), acting.as_deref(), "N31: {verb} {at}");
        assert_eq!(
            vec![(verb.to_string(), at.clone())],
            asked(&a).await?,
            "the controlling CDR is sent the write once (§12.4, N23)"
        );
        assert!(
            asked(&b).await?.is_empty(),
            "no other node is contacted: {verb} {at}"
        );
    }
    Ok(())
}

// conformance: CP-15 CP-33
#[tokio::test]
async fn a_versioned_write_the_index_routes_reaches_its_controlling_cdr_unprobed() -> TestResult {
    let a = crate::path_ehr_id::holder().await;
    let at = format!("/v1/ehr/{EHR_A}/composition/{OBJECT_AT_A}");
    mount(&a, "PUT", at.clone(), ResponseTemplate::new(200)).await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let app = over(dir.path(), &a, &b, "")?;
    let (read, _, _) = answer(
        app.clone(),
        Request::get(format!("/v1/ehr/{EHR_A}")).body(Body::empty())?,
    )
    .await?;
    assert_eq!(StatusCode::OK, read, "the probe teaches the index");
    let request = versioned(&Method::PUT, &at, None, Some(CREATED_AT_A), "{}")?;
    let (status, acting, text) = answer(app, request).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    assert_eq!(Some(ENDPOINT_A), acting.as_deref());
    assert_eq!(vec![probe_at(), ("PUT".to_owned(), at)], asked(&a).await?);
    assert_eq!(
        vec![probe_at()],
        asked(&b).await?,
        "the write itself probes nobody (N41)"
    );
    Ok(())
}

// conformance: CP-15 CP-13
#[tokio::test]
async fn a_registered_creating_system_mapping_makes_its_node_the_controlling_cdr() -> TestResult {
    let at = format!("/v1/ehr/{EHR_A}/composition/{OBJECT_AT_A}");
    let a = Server::start().await;
    mount(&a, "PUT", at.clone(), ResponseTemplate::new(200)).await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let app = over(dir.path(), &a, &b, LEGACY_MAPPING)?;
    let request = versioned(
        &Method::PUT,
        &at,
        Some(ENDPOINT_A),
        Some(CREATED_BY_LEGACY),
        "{}",
    )?;
    let (status, acting, text) = answer(app, request).await?;
    assert_eq!(StatusCode::OK, status, "N21, §12.2: {text}");
    assert_eq!(Some(ENDPOINT_A), acting.as_deref());
    assert_eq!(vec![("PUT".to_owned(), at)], asked(&a).await?);
    assert!(asked(&b).await?.is_empty());
    Ok(())
}

// conformance: CP-15
#[tokio::test]
async fn a_write_its_path_node_does_not_control_is_refused_409_and_reaches_no_node() -> TestResult {
    let mut writes = if_match_writes();
    writes.push((
        Method::DELETE,
        format!("/v1/ehr/{EHR_A}/composition/{CREATED_AT_A}"),
    ));
    for (verb, at) in writes {
        let a = Server::start().await;
        let b = Server::start().await;
        let dir = tempfile::tempdir()?;
        let preceding = (verb == Method::PUT || at.ends_with("directory")).then_some(CREATED_AT_A);
        let request = versioned(&verb, &at, Some(ENDPOINT_B), preceding, "{}")?;
        let text = refused_at_neither(
            over(dir.path(), &a, &b, "")?,
            request,
            (StatusCode::CONFLICT, "controlling-system-unreachable"),
            (&a, &b),
        )
        .await?;
        assert!(
            text.contains("cdr-a.example.org")
                && text.contains("node-a")
                && text.contains(ENDPOINT_A),
            "the error identifies the controlling system (§10.3): {text}"
        );
        assert!(
            !text.contains(EHR_A) && !text.contains(OBJECT_AT_A),
            "no value of the request is quoted: {text}"
        );
    }
    Ok(())
}

// conformance: CP-15
#[tokio::test]
async fn a_write_of_a_version_no_member_is_known_to_control_is_refused_409() -> TestResult {
    let a = Server::start().await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let request = versioned(
        &Method::PUT,
        &format!("/v1/ehr/{EHR_A}/composition/5c3e9b1a-7d2f-4e8a-9b6c-1f0e2d3c4b5a"),
        Some(ENDPOINT_A),
        Some(CREATED_ELSEWHERE),
        "{}",
    )?;
    let text = refused_at_neither(
        over(dir.path(), &a, &b, "")?,
        request,
        (StatusCode::CONFLICT, "controlling-system-unreachable"),
        (&a, &b),
    )
    .await?;
    assert!(
        !text.contains("external.example.org"),
        "the client's creating_system_id is not quoted: {text}"
    );
    assert!(
        error_body(&text)?
            .message
            .contains("the version If-Match names"),
        "the error points at the controlling system it cannot quote (§10.3): {text}"
    );
    Ok(())
}

// conformance: CP-15 CP-13
#[tokio::test]
async fn a_learned_holder_is_never_the_controlling_cdr_of_a_write() -> TestResult {
    let a = Server::start().await;
    mount(
        &a,
        "POST",
        "/v1/query/aql".to_owned(),
        ResponseTemplate::new(200).set_body_raw(
            br##"{"q":"node","columns":[{"name":"#0","path":"c/uid/value"}],"rows":[]}"##.to_vec(),
            "application/json",
        ),
    )
    .await;
    let b = node_answering(CREATED_ELSEWHERE).await;
    let dir = tempfile::tempdir()?;
    let app = over(dir.path(), &a, &b, "")?;
    let query = post(body(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c",
    )?)?;
    let (queried, _, text) = answer(app.clone(), query).await?;
    assert_eq!(StatusCode::OK, queried, "{text}");
    let delete = versioned(
        &Method::DELETE,
        &format!("/v1/ehr/{EHR_B}/composition/{CREATED_ELSEWHERE}"),
        Some(ENDPOINT_B),
        None,
        "",
    )?;
    let (status, acting, text) = answer(app, delete).await?;
    assert_eq!(
        (
            StatusCode::CONFLICT,
            "controlling-system-unreachable".to_owned()
        ),
        (status, error_body(&text)?.code),
        "a holder the map learned is no write's controlling CDR (§10.3, §12a.1)"
    );
    assert!(acting.is_none());
    let query_only = vec![("POST".to_owned(), "/v1/query/aql".to_owned())];
    assert_eq!(query_only, asked(&b).await?, "the holder is never written");
    assert_eq!(query_only, asked(&a).await?);
    Ok(())
}

// conformance: CP-15
#[tokio::test]
async fn a_versioned_write_naming_no_single_preceding_version_is_a_400_before_any_node()
-> TestResult {
    let at = format!("/v1/ehr/{EHR_A}/composition/{OBJECT_AT_A}");
    let cases: Vec<(Method, String, Vec<String>)> = vec![
        (Method::PUT, at.clone(), Vec::new()),
        (
            Method::PUT,
            at.clone(),
            vec![quoted(CREATED_AT_A), quoted(SECOND_AT_A)],
        ),
        (
            Method::PUT,
            at.clone(),
            vec![format!("{}, {}", quoted(CREATED_AT_A), quoted(SECOND_AT_A))],
        ),
        (
            Method::PUT,
            at.clone(),
            vec![format!("W/{}", quoted(CREATED_AT_A))],
        ),
        (Method::PUT, at.clone(), vec![CREATED_AT_A.to_owned()]),
        (Method::PUT, at.clone(), vec!["*".to_owned()]),
        (
            Method::PUT,
            format!("/v1/ehr/{EHR_A}/ehr_status"),
            vec![quoted(OBJECT_AT_A)],
        ),
        (
            Method::DELETE,
            format!("/v1/ehr/{EHR_A}/composition/{OBJECT_AT_A}"),
            Vec::new(),
        ),
    ];
    for (verb, at, tags) in cases {
        let a = Server::start().await;
        let b = Server::start().await;
        let dir = tempfile::tempdir()?;
        let mut request = versioned(&verb, &at, Some(ENDPOINT_A), None, "{}")?;
        for tag in &tags {
            request.headers_mut().append(header::IF_MATCH, tag.parse()?);
        }
        refused_at_neither(
            over(dir.path(), &a, &b, "")?,
            request,
            (StatusCode::BAD_REQUEST, "preceding-version-invalid"),
            (&a, &b),
        )
        .await?;
    }
    Ok(())
}

// conformance: CP-15 CP-24 CP-26
#[tokio::test]
async fn a_versioned_write_lands_byte_identical_and_carries_no_identifier_outside_its_body()
-> TestResult {
    let at = format!("/v1/ehr/{EHR_A}/composition/{OBJECT_AT_A}");
    let location =
        format!("https://cdr-a.example.org/openehr/v1/ehr/{EHR_A}/composition/{SECOND_AT_A}");
    let a = Server::start().await;
    mount(
        &a,
        "PUT",
        at.clone(),
        ResponseTemplate::new(200)
            .insert_header("ETag", quoted(SECOND_AT_A).as_str())
            .insert_header("Location", location.as_str()),
    )
    .await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let sent = composition();
    let mut request = versioned(
        &Method::PUT,
        &at,
        Some(ENDPOINT_A),
        Some(CREATED_AT_A),
        &sent,
    )?;
    let fields = request.headers_mut();
    fields.insert(
        header::AUTHORIZATION,
        format!("Bearer {}", *CLIENT_TOKEN).parse()?,
    );
    fields.insert("x-patient", PATIENT.parse()?);
    fields.insert("x-request-id", format!("req-{PATIENT}").parse()?);
    let response = send(over(dir.path(), &a, &b, "")?, request).await?;
    assert_eq!(StatusCode::OK, response.status());
    let field = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(Some(quoted(SECOND_AT_A)), field(header::ETAG), "N31");
    assert_eq!(Some(location), field(header::LOCATION), "N31");
    let requests = a.received_requests().await.ok_or("recording is on")?;
    let received = requests.first().ok_or("node A was sent the write")?;
    assert_eq!(
        sent.as_bytes(),
        received.body.as_slice(),
        "the body lands byte-identical, its DV_IDENTIFIER included (track 10)"
    );
    assert_eq!(
        Some(quoted(CREATED_AT_A).as_bytes()),
        received
            .headers
            .get(header::IF_MATCH)
            .map(http::HeaderValue::as_bytes),
        "If-Match reaches the node as sent"
    );
    let composed = outside_bodies(&a).await?;
    assert!(!composed.contains(PATIENT), "N33: {composed}");
    assert!(!composed.contains(CLIENT_TOKEN.as_str()), "{composed}");
    assert!(
        !composed.to_ascii_lowercase().contains("openehr-federation"),
        "{composed}"
    );
    assert!(asked(&b).await?.is_empty());
    Ok(())
}

// conformance: CP-15 CP-33
#[tokio::test]
async fn no_write_without_a_target_is_ever_probed_for() -> TestResult {
    let mut writes = if_match_writes();
    writes.extend([
        (Method::PUT, format!("/v1/ehr/{EHR_A}")),
        (Method::POST, "/v1/ehr".to_owned()),
        (Method::POST, format!("/v1/ehr/{EHR_A}/directory")),
        (Method::POST, format!("/v1/ehr/{EHR_A}/contribution")),
    ]);
    for (verb, at) in writes {
        let a = crate::path_ehr_id::holder().await;
        let b = crate::path_ehr_id::holder().await;
        let dir = tempfile::tempdir()?;
        let request = versioned(&verb, &at, None, Some(CREATED_AT_A), "{}")?;
        refused_at_neither(
            over(dir.path(), &a, &b, "")?,
            request,
            (StatusCode::BAD_REQUEST, "target-required"),
            (&a, &b),
        )
        .await?;
    }
    Ok(())
}

// conformance: CP-15
#[tokio::test]
async fn a_new_ehr_is_created_by_the_targeting_headers_alone_never_by_the_index() -> TestResult {
    let a = crate::path_ehr_id::holder().await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let app = over(dir.path(), &a, &b, "")?;
    let (read, _, _) = answer(
        app.clone(),
        Request::get(format!("/v1/ehr/{EHR_A}")).body(Body::empty())?,
    )
    .await?;
    assert_eq!(StatusCode::OK, read, "the index now names node A");
    let create = Request::put(format!("/v1/ehr/{EHR_A}")).body(Body::empty())?;
    let (status, acting, text) = answer(app, create).await?;
    assert_eq!(
        (StatusCode::BAD_REQUEST, "target-required".to_owned()),
        (status, error_body(&text)?.code),
        "a new EHR has no owner for the index to name (§12.4, N23)"
    );
    assert!(acting.is_none());
    assert_eq!(vec![probe_at()], asked(&a).await?, "only the earlier probe");
    assert_eq!(vec![probe_at()], asked(&b).await?);
    Ok(())
}

// conformance: CP-15
#[tokio::test]
async fn a_new_ehr_naming_more_than_one_endpoint_is_refused_and_reaches_no_node() -> TestResult {
    for (verb, at) in [
        (Method::POST, "/v1/ehr".to_owned()),
        (Method::PUT, format!("/v1/ehr/{EHR_A}")),
    ] {
        for (named, code) in [
            ("node-a-pub, node-b-pub", "endpoint-several"),
            ("*", "endpoint-unknown"),
        ] {
            let a = Server::start().await;
            let b = Server::start().await;
            let dir = tempfile::tempdir()?;
            let request = Request::builder()
                .method(verb.clone())
                .uri(&at)
                .header("openEHR-federation-endpoint", named)
                .body(Body::from("{}"))?;
            refused_at_neither(
                over(dir.path(), &a, &b, "")?,
                request,
                (StatusCode::BAD_REQUEST, code),
                (&a, &b),
            )
            .await?;
        }
    }
    Ok(())
}

// conformance: CP-15 CP-24 CP-26
#[tokio::test]
async fn a_new_ehr_lands_byte_identical_at_the_one_named_node() -> TestResult {
    let created = format!("https://cdr-b.example.org/openehr/v1/ehr/{EHR_B}");
    let b = Server::start().await;
    mount(
        &b,
        "POST",
        "/v1/ehr".to_owned(),
        ResponseTemplate::new(201)
            .insert_header("Location", created.as_str())
            .insert_header("ETag", quoted(EHR_B).as_str()),
    )
    .await;
    let a = Server::start().await;
    let dir = tempfile::tempdir()?;
    let sent = format!(
        "{{\"_type\":\"EHR_STATUS\", \"subject\":{{\"external_ref\":{{\"id\":{{\"_type\":\"GENERIC_ID\",\"value\":\"{PATIENT}\",\"scheme\":\"synthetic\"}},\"namespace\":\"urn:oid:2.999.1\",\"type\":\"PERSON\"}}}},\n \"is_queryable\":true, \"is_modifiable\":true}}\n"
    );
    let request = Request::post("/v1/ehr")
        .header("openEHR-federation-endpoint", ENDPOINT_B)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-patient", PATIENT)
        .body(Body::from(sent.clone()))?;
    let response = send(over(dir.path(), &a, &b, "")?, request).await?;
    assert_eq!(StatusCode::CREATED, response.status());
    let field = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(
        Some(ENDPOINT_B.to_owned()),
        field("openEHR-federation-endpoint"),
        "N31"
    );
    assert_eq!(
        Some("cdr-b.example.org".to_owned()),
        field("openEHR-federation-system-id"),
        "§9.6"
    );
    assert_eq!(Some(created), field("location"), "N31");
    assert_eq!(Some(quoted(EHR_B)), field("etag"), "N31");
    let requests = b.received_requests().await.ok_or("recording is on")?;
    assert_eq!(1, requests.len(), "created at one node, once (§2.3, N23)");
    let received = requests.first().ok_or("node B was sent the create")?;
    assert_eq!(
        sent.as_bytes(),
        received.body.as_slice(),
        "byte-identical (N22)"
    );
    assert_eq!(
        Some("application/json"),
        received
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        "a declared media type travels with the body"
    );
    let composed = outside_bodies(&b).await?;
    assert!(!composed.contains(PATIENT), "N33: {composed}");
    assert!(asked(&a).await?.is_empty(), "node A is never asked");
    Ok(())
}

#[tokio::test]
async fn a_read_of_the_ehr_collection_without_a_subject_asks_nobody() -> TestResult {
    let a = Server::start().await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let request = Request::get("/v1/ehr")
        .header("openEHR-federation-endpoint", ENDPOINT_A)
        .body(Body::empty())?;
    refused_at_neither(
        over(dir.path(), &a, &b, "")?,
        request,
        (StatusCode::BAD_REQUEST, "patient-invalid"),
        (&a, &b),
    )
    .await?;
    Ok(())
}

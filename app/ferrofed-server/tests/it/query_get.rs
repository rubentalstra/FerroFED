// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The ITS-REST `GET` forms of query execution against two mock nodes:
//! `GET {base}/v1/query/aql` and `GET {base}/v1/query/{name}[/{version}]`
//! run the pipeline of their `POST` forms, so every node receives the same
//! request and the client the same answer (N1, CP-1; ITS-REST Query API). The
//! query string is decoded by the generated `openehr-its` parameters, a `+`
//! is a literal plus, and a refusal is `400 body-invalid` that asks nobody.
//! No node request carries the patient identifier (§5.4.1, N33), and the
//! request log carries neither `q` nor a parameter value (§5.4.3).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;

use axum::Router;
use axum::body::Body;
use ferrofed_engine::onward::conveyance;
use ferrofed_testkit::mock::Server;
use http::{Request, StatusCode, header};

use crate::facade::{
    Answer, EHR_A, EHR_B, NAMESPACE, PATIENT, PATIENT_TAIL, dev_gateway, node_answering, post,
    received, registry, statuses, wire,
};
use crate::request_log::logged;
use crate::support::{call, error_body, request_lines, stable_claims};

type TestResult = Result<(), Box<dyn Error>>;

/// The qualified name the stored-query fixtures run.
const NAME: &str = "org.example::visits";

/// A synthetic composition name carrying a `+`.
const VISIT: &str = "Visit+1";

/// A synthetic `ehr_id` no member holds, which a client might send.
const FOREIGN_EHR: &str = "9999cccc-9999-4999-8999-999999999999";

/// The patient query with the identifier bound through `$patient` and the
/// composition name through `$name`.
fn parameterised() -> String {
    format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = $patient \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}' \
         AND c/name/value = $name"
    )
}

/// `text` percent-encoded byte for byte, the unreserved set of RFC 3986
/// §2.3 kept.
pub(crate) fn encoded(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            // NOTE: writing to a String cannot fail, so the result is dropped.
            let _written: std::fmt::Result = write!(out, "%{byte:02X}");
        }
        out
    })
}

/// `GET` of `uri`.
fn get(uri: &str) -> Result<Request<Body>, http::Error> {
    Request::get(uri).body(Body::empty())
}

/// A development gateway over two fresh mock nodes that both hold the
/// patient, with the nodes.
async fn both_members(dir: &Path) -> Result<(Router, Server, Server), Box<dyn Error>> {
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let app = dev_gateway(
        dir,
        &a.uri(),
        &b.uri(),
        &[("node-a", EHR_A), ("node-b", EHR_B)],
    )?;
    Ok((app, a, b))
}

/// A registry gateway offering stored queries over two fresh mock nodes,
/// with the nodes.
async fn registry_members(dir: &Path) -> Result<(Router, Server, Server), Box<dyn Error>> {
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let app = crate::stored::gateway(
        dir,
        &registry(&a.uri(), &b.uri(), ""),
        &[("node-a", EHR_A), ("node-b", EHR_B)],
    )?;
    Ok((app, a, b))
}

/// What a node received, less what differs per gateway by design: the
/// minted `X-Request-Id`, the `Host` of the mock, and the `iat`, `exp` and
/// `jti` minted for each `openEHR-federation-client` token.
async fn capture(server: &Server) -> Result<Vec<String>, Box<dyn Error>> {
    let requests = server.received_requests().await.ok_or("recording is on")?;
    let mut captured = Vec::new();
    for request in requests {
        let mut headers: Vec<String> = request
            .headers
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "x-request-id" | "host"))
            .map(|(name, value)| -> Result<String, Box<dyn Error>> {
                if name.as_str().eq_ignore_ascii_case(conveyance::HEADER) {
                    return Ok(format!("{name}: {}", stable_claims(value.to_str()?)?));
                }
                Ok(format!(
                    "{name}: {}",
                    String::from_utf8_lossy(value.as_bytes())
                ))
            })
            .collect::<Result<_, _>>()?;
        headers.sort();
        captured.push(format!(
            "{} {} {:?}\n{}\n{}",
            request.method,
            request.url.path(),
            request.url.query(),
            headers.join("\n"),
            String::from_utf8(request.body.clone())?
        ));
    }
    Ok(captured)
}

/// The status, the `q`, the rows sorted, and each endpoint's status of an
/// answer.
type Outcome = (StatusCode, String, Vec<Vec<String>>, Vec<(String, String)>);

/// The [`Outcome`] of the answer `text` with `status`.
fn outcome(status: StatusCode, text: &str) -> Result<Outcome, Box<dyn Error>> {
    let answer: Answer = serde_json::from_str(text)?;
    let mut rows = answer.rows.clone();
    rows.sort();
    let endpoints = statuses(&answer)
        .into_iter()
        .map(|(id, status)| (id.to_owned(), status.to_owned()))
        .collect();
    Ok((status, answer.q, rows, endpoints))
}

/// Asserts that no capture of `servers` holds the patient identifier, its
/// tail or its namespace (§5.4.1, N33).
async fn nothing_identifying(servers: [&Server; 2]) -> TestResult {
    for server in servers {
        let captured = wire(server).await?;
        assert!(!captured.is_empty(), "each member was asked");
        for needle in [PATIENT, PATIENT_TAIL, NAMESPACE] {
            assert!(
                !captured.contains(needle),
                "{needle} reached a node (§5.4.1, N33): {captured}"
            );
        }
    }
    Ok(())
}

/// The `POST` body binding the patient and [`VISIT`], with `fetch`.
fn bound_body(aql: &str) -> String {
    let q = serde_json::to_string(aql).unwrap_or_default();
    format!(
        r#"{{"q":{q},"fetch":10,"query_parameters":{{"patient":"{PATIENT}","name":"{VISIT}"}}}}"#
    )
}

/// The query string binding the patient and [`VISIT`], with `fetch`, the
/// `+` of [`VISIT`] sent as itself.
fn bound_query() -> String {
    format!("fetch=10&patient={PATIENT}&name={VISIT}")
}

// conformance: CP-1
#[tokio::test]
async fn get_and_post_send_every_node_the_same_request_and_answer_the_same() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (posted, a, b) = both_members(dir.path()).await?;
    let (status, text) = call(posted, post(bound_body(&parameterised()))?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let by_post = outcome(status, &text)?;

    let elsewhere = tempfile::tempdir()?;
    let (got, c, d) = both_members(elsewhere.path()).await?;
    let uri = format!(
        "/v1/query/aql?q={}&{}",
        encoded(&parameterised()),
        bound_query()
    );
    let (status, text) = call(got, get(&uri)?).await?;
    assert_eq!(StatusCode::OK, status, "N1: {text}");
    assert_eq!(by_post, outcome(status, &text)?, "N1: the same answer");
    assert_eq!(
        capture(&a).await?,
        capture(&c).await?,
        "node A receives the same request"
    );
    assert_eq!(
        capture(&b).await?,
        capture(&d).await?,
        "node B receives the same request"
    );
    let sent = received(&c).await?.concat();
    assert!(
        sent.contains(VISIT),
        "the + is a literal plus (RFC 3986 §2.1): {sent}"
    );
    nothing_identifying([&c, &d]).await
}

// conformance: CP-1 CP-40
#[tokio::test]
async fn a_stored_query_runs_by_get_with_and_without_a_version() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (posted, a, b) = registry_members(dir.path()).await?;
    let put = Request::put(format!("/v1/definition/query/{NAME}/1.0.0"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(parameterised()))?;
    let (status, text) = call(posted.clone(), put).await?;
    assert_eq!(StatusCode::OK, status, "§12.7: stored: {text}");
    let invoke = Request::post(format!("/v1/query/{NAME}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            r#"{{"fetch":10,"query_parameters":{{"patient":"{PATIENT}","name":"{VISIT}"}}}}"#
        )))?;
    let (status, text) = call(posted.clone(), invoke).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let by_post = outcome(status, &text)?;

    for path in [
        format!("/v1/query/{NAME}"),
        format!("/v1/query/{NAME}/1.0.0"),
    ] {
        let uri = format!("{path}?{}", bound_query());
        let (status, text) = call(posted.clone(), get(&uri)?).await?;
        assert_eq!(StatusCode::OK, status, "N1, §12.7: {path}: {text}");
        assert_eq!(by_post, outcome(status, &text)?, "{path}");
    }
    for node in [&a, &b] {
        let captured = capture(node).await?;
        let [by_post, unversioned, versioned] = captured.as_slice() else {
            return Err(format!("one POST and two GETs reach each node: {captured:?}").into());
        };
        assert_eq!(
            by_post, unversioned,
            "the unversioned GET sends the POST's request"
        );
        assert_eq!(
            by_post, versioned,
            "the versioned GET sends the POST's request"
        );
    }
    nothing_identifying([&a, &b]).await
}

// conformance: CP-1 CP-40
#[tokio::test]
async fn a_stored_get_reads_a_q_key_as_a_query_parameter() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (app, a, b) = registry_members(dir.path()).await?;
    let aql = format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = $patient \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}' \
         AND c/name/value = $q"
    );
    let put = Request::put(format!("/v1/definition/query/{NAME}/1.0.0"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(aql))?;
    let (status, text) = call(app.clone(), put).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let uri = format!("/v1/query/{NAME}?patient={PATIENT}&q=SYNTHETIC-NAME-q7");
    let (status, text) = call(app, get(&uri)?).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "the stored GET forms declare no q, so it binds $q: {text}"
    );
    let sent = received(&a).await?.concat();
    assert!(sent.contains("SYNTHETIC-NAME-q7"), "$q is bound: {sent}");
    nothing_identifying([&a, &b]).await
}

// conformance: CP-1
#[tokio::test]
async fn a_malformed_query_string_is_a_400_body_invalid_that_asks_nobody() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (app, a, b) = both_members(dir.path()).await?;
    let q = encoded(&parameterised());
    let patient = format!("patient={PATIENT}&name=x");
    for uri in [
        format!("/v1/query/aql?{patient}"),
        format!("/v1/query/aql?q={q}&q={q}&{patient}"),
        format!("/v1/query/aql?q=SELECT%FF&{patient}"),
        format!("/v1/query/aql?q={q}&patient=SENTINEL-%FF&name=x"),
        format!("/v1/query/aql?q={q}&{patient}&offset=ten"),
        format!("/v1/query/aql?q={q}&{patient}&fetch=1.5"),
        format!("/v1/query/aql?q={q}&{patient}&name=y"),
    ] {
        let (status, text) = call(app.clone(), get(&uri)?).await?;
        assert_eq!(StatusCode::BAD_REQUEST, status, "{uri}: {text}");
        assert_eq!("body-invalid", error_body(&text)?.code, "{uri}");
        for needle in [PATIENT_TAIL, "SELECT", "SENTINEL"] {
            assert!(!text.contains(needle), "the answer quotes {needle}: {text}");
        }
    }
    let (status, text) = call(app, get(&format!("/v1/query/aql?q={q}&{patient}&p=null"))?).await?;
    assert_eq!(StatusCode::BAD_REQUEST, status, "{text}");
    assert_eq!(
        "parameter-invalid",
        error_body(&text)?.code,
        "a null parameter, as in a POST body: {text}"
    );
    assert!(received(&a).await?.is_empty() && received(&b).await?.is_empty());
    Ok(())
}

// conformance: CP-1
#[tokio::test]
async fn a_malformed_stored_get_query_string_is_a_400_body_invalid_that_asks_nobody() -> TestResult
{
    let dir = tempfile::tempdir()?;
    let (app, a, b) = registry_members(dir.path()).await?;
    let put = Request::put(format!("/v1/definition/query/{NAME}/1.0.0"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(parameterised()))?;
    let (status, text) = call(app.clone(), put).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    for uri in [
        format!("/v1/query/{NAME}?patient=SENTINEL-%FF&name=x"),
        format!("/v1/query/{NAME}/1.0.0?patient={PATIENT}&name=x&offset=ten"),
        format!("/v1/query/{NAME}?patient={PATIENT}&name=x&name=y"),
    ] {
        let (status, text) = call(app.clone(), get(&uri)?).await?;
        assert_eq!(StatusCode::BAD_REQUEST, status, "{uri}: {text}");
        assert_eq!("body-invalid", error_body(&text)?.code, "{uri}");
        assert!(!text.contains(PATIENT_TAIL), "{text}");
    }
    assert!(received(&a).await?.is_empty() && received(&b).await?.is_empty());
    Ok(())
}

// conformance: CP-1 CP-26
#[tokio::test]
async fn a_client_ehr_id_in_the_query_string_is_dropped() -> TestResult {
    let dir = tempfile::tempdir()?;
    let (app, a, b) = both_members(dir.path()).await?;
    let uri = format!(
        "/v1/query/aql?q={}&ehr_id={FOREIGN_EHR}&{}",
        encoded(&parameterised()),
        bound_query()
    );
    let (status, text) = call(app, get(&uri)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    for server in [&a, &b] {
        let captured = wire(server).await?;
        assert!(
            !captured.contains(FOREIGN_EHR),
            "the node is scoped by its own ehr_id alone (N33): {captured}"
        );
    }
    nothing_identifying([&a, &b]).await
}

#[test]
fn neither_q_nor_a_parameter_value_of_a_get_reaches_the_log() -> TestResult {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let dir = tempfile::tempdir()?;
    let (app, _a, _b) = runtime.block_on(both_members(dir.path()))?;
    let uri = format!(
        "/v1/query/aql?q={}&offset=0&{}",
        encoded(&parameterised()),
        bound_query()
    );
    let refused = format!("/v1/query/aql?q=SYNTHETIC-AQL-ql9&patient={PATIENT}&p=null");
    let text = logged(&app, "trace", vec![get(&uri)?, get(&refused)?])?;
    let lines = request_lines(&text)?;
    assert_eq!(2, lines.len(), "both requests were logged: {text}");
    for needle in [
        PATIENT,
        PATIENT_TAIL,
        "Visit",
        "SYNTHETIC-AQL",
        "SELECT",
        "c%2Fuid",
    ] {
        assert!(!text.contains(needle), "{needle} reached the log: {text}");
    }
    Ok(())
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The status mapping of §11.2 and the error vocabulary, through the real
//! configuration path: one case per row the gateway can reach today, each
//! failure's stable code, no openEHR `Error` body that quotes the query, a
//! parameter value or a header value (§5.4.3), the failing §11.4 envelope
//! echoing the client's own `q` (N17), and the book page held to the code
//! table.
//!
//! The rows of follow-up routing (§12: a path with no destination, an
//! `ehr_id` collision, an unreachable controlling system) and the node
//! pass-through of a single-node route are held at the unit level until
//! their routes exist.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::Config;
use ferrofed_server::error::Code;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::unreachable;
use http::{Method, Request, StatusCode, header};
use openehr_federation::headers::COMPLETENESS;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{
    Answer, EHR_A, EHR_B, NAMESPACE, PATIENT, PATIENT_TAIL, body, crossref, node_answering,
    node_failing, patient_query, received, registry, schema, settings_with_room, statuses,
};
use crate::support::{self, ErrorBody, SLACK, call, millis};

type TestResult = Result<(), Box<dyn Error>>;

/// A synthetic value only a query parameter carries.
const PARAMETER_VALUE: &str = "SENTINEL-PARAMETER-5be2";

/// The book page that documents the vocabulary.
const BOOK_PAGE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../website/book/src/integrate/errors.md"
);

/// A development gateway over `a` and `b` resolving the patient at both, with
/// the given budget and best-effort offer.
fn gateway(
    dir: &Path,
    a: &str,
    b: &str,
    per_node_ms: u64,
    best_effort: bool,
) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(a, b, ""))?;
    let document = toml::Value::String(document.display().to_string());
    let rows = crossref(&[("node-a", EHR_A), ("node-b", EHR_B)]);
    let overall_ms = per_node_ms.saturating_mul(2);
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = {per_node_ms}\noverall_timeout_ms = {overall_ms}\nnode_selection = \"ask-all\"\nid = \"example-federation\"\nbest_effort = {best_effort}\n\n{rows}"
    );
    let settings =
        Config::from_sources(Some(&support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// `POST /v1/query/aql` with `body` and the completeness header `completeness`.
fn post(body: String, completeness: Option<&str>) -> Result<Request<Body>, http::Error> {
    let mut request =
        Request::post("/v1/query/aql").header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = completeness {
        request = request.header(COMPLETENESS, value);
    }
    request.body(Body::from(body))
}

/// A node answering one row after `delay`.
async fn node_after(delay: Duration) -> Server {
    let server = Server::start().await;
    let answer =
        r##"{"q":"node","columns":[{"name":"#0","path":"c/uid/value"}],"rows":[["uid-late"]]}"##;
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(answer.as_bytes().to_vec(), "application/json")
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

/// The synthetic identifiers no error body may quote.
const IDENTIFIERS: [&str; 4] = [PATIENT, PATIENT_TAIL, PARAMETER_VALUE, NAMESPACE];

/// The AQL fragments no error body may quote.
const AQL: [&str; 2] = ["SELECT", "FROM EHR"];

/// Asserts that `text` quotes none of the synthetic identifiers and no AQL.
fn quotes_nothing(text: &str) {
    for quoted in IDENTIFIERS {
        assert!(
            !text.contains(quoted),
            "the body quotes the synthetic identifier {quoted}: {text}"
        );
    }
    for aql in AQL {
        assert!(!text.contains(aql), "the body quotes the AQL: {text}");
    }
}

#[test]
fn no_minted_request_id_can_contain_a_searched_fragment() {
    let minted = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c) || c == '-';
    for fragment in IDENTIFIERS.into_iter().chain(AQL) {
        assert!(
            !fragment.chars().all(minted),
            "{fragment} is spelled in the alphabet of a minted request id, \
             which every error body carries"
        );
    }
}

/// The error body of `text`, with `status` checked against its code's.
fn checked(status: StatusCode, text: &str) -> Result<ErrorBody, Box<dyn Error>> {
    let error = support::error_body(text)?;
    let code = Code::every()
        .find(|code| code.as_str() == error.code)
        .ok_or_else(|| format!("{} is in the vocabulary", error.code))?;
    assert_eq!(code.status(), status, "the code names the status: {text}");
    assert!(error.validation_errors.is_empty(), "{text}");
    assert!(
        !error.request_id.is_empty(),
        "a request id is named: {text}"
    );
    Ok(error)
}

/// The failing envelope of `text` for the client query `aql`: the §11.4
/// result set, with no rows and the client's own `q` (N17, CP-30).
fn failing_envelope(text: &str, aql: &str) -> Result<Answer, Box<dyn Error>> {
    schema::validate(text)?;
    let answer: Answer = serde_json::from_str(text)?;
    assert!(answer.rows.is_empty(), "a failing query returns no rows");
    assert_eq!(
        aql, answer.q,
        "a failing answer echoes the client's q (N17)"
    );
    assert!(!answer.meta.federation.complete, "{text}");
    assert!(
        !text.contains(PARAMETER_VALUE),
        "the envelope quotes no parameter value: {text}"
    );
    Ok(answer)
}

/// The patient query that takes its identifier from the `$patient` parameter.
fn bound_aql() -> String {
    format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = $patient \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}'"
    )
}

/// The ad hoc query body of [`bound_aql`] with the JSON object `parameters`.
fn bound_query(parameters: &str) -> String {
    format!(
        r#"{{"q":"{}","query_parameters":{parameters}}}"#,
        bound_aql()
    )
}

/// One request that fails with a `400`: the body, the completeness header,
/// and the code it answers.
type Invalid = (String, Option<&'static str>, &'static str);

/// The requests that fail with a `400`, each with the code it answers.
fn invalid_requests() -> Result<Vec<Invalid>, Box<dyn Error>> {
    let subject = format!(
        "e/ehr_status/subject/external_ref/id/value = '{PATIENT}' \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}'"
    );
    Ok(vec![
        (
            format!(
                r#"{{"q":5,"note":"{PATIENT}","query_parameters":{{"p":"{PARAMETER_VALUE}"}}}}"#
            ),
            None,
            "body-invalid",
        ),
        (
            body(&patient_query())?,
            Some(PATIENT),
            "completeness-invalid",
        ),
        (
            bound_query(&format!(
                r#"{{"patient":["{PATIENT}","{PARAMETER_VALUE}"]}}"#
            )),
            None,
            "parameter-invalid",
        ),
        (
            bound_query(&format!(
                r#"{{"patient":"{PATIENT}","unused":"{PARAMETER_VALUE}"}}"#
            )),
            None,
            "parameters",
        ),
        (body(&format!("SELECT {PATIENT} FROM"))?, None, "not-aql"),
        (
            body(&format!(
                "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
                 WHERE e/ehr_status/subject/external_ref/id/value = '{PATIENT}' \
                 OR c/name/value = 'x'"
            ))?,
            None,
            "unreducible",
        ),
        (
            body(&format!(
                "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
                 WHERE {subject} AND e/ehr_status/subject/external_ref/id/value = '{PARAMETER_VALUE}'"
            ))?,
            None,
            "second-subject",
        ),
        (
            body(&format!(
                "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
                 WHERE {subject} AND c/name/value = '{PATIENT}'"
            ))?,
            None,
            "identifier-elsewhere",
        ),
    ])
}

// conformance: CP-12
#[tokio::test]
async fn an_invalid_request_is_a_400_with_its_code_and_quotes_nothing() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    for (request, completeness, expected) in invalid_requests()? {
        let app = gateway(dir.path(), &a.uri(), &b.uri(), 2000, true)?;
        let (status, text) = call(app, post(request.clone(), completeness)?).await?;
        assert_eq!(StatusCode::BAD_REQUEST, status, "{request}: {text}");
        let error = checked(status, &text)?;
        assert_eq!(expected, error.code, "{request}: {text}");
        quotes_nothing(&text);
    }
    for server in [&a, &b] {
        assert!(
            received(server).await?.is_empty(),
            "an invalid request reaches no node (§11.2)"
        );
    }
    Ok(())
}

#[tokio::test]
async fn partial_where_best_effort_is_not_offered_is_a_400_partial_unsupported() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a.uri(), &b.uri(), 2000, false)?;
    let (status, text) = call(app, post(body(&patient_query())?, Some("partial"))?).await?;
    assert_eq!(StatusCode::BAD_REQUEST, status, "{text}");
    assert_eq!(
        "partial-unsupported",
        checked(status, &text)?.code,
        "§11.4, N37"
    );
    quotes_nothing(&text);
    Ok(())
}

// conformance: CP-12 CP-30
#[tokio::test]
async fn a_node_error_under_all_or_nothing_is_a_424_carrying_the_envelope() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_failing(500).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a.uri(), &b.uri(), 2000, true)?;
    let request = bound_query(&format!(r#"{{"patient":"{PATIENT}"}}"#));
    let (status, text) = call(app, post(request, None)?).await?;
    assert_eq!(StatusCode::FAILED_DEPENDENCY, status, "§11.2, N37: {text}");
    let answer = failing_envelope(&text, &bound_aql())?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "node-error")],
        statuses(&answer),
        "the node's error is reported per endpoint (§11.4)"
    );
    Ok(())
}

// conformance: CP-12 CP-30
#[tokio::test]
async fn a_node_not_found_inside_a_fan_out_is_a_node_error_and_a_424() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_failing(404).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a.uri(), &b.uri(), 2000, true)?;
    let (status, text) = call(app, post(body(&patient_query())?, None)?).await?;
    assert_eq!(
        StatusCode::FAILED_DEPENDENCY,
        status,
        "a fan-out does not pass a node's 404 through (§11.2, §11.4): {text}"
    );
    let answer = failing_envelope(&text, &patient_query())?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "node-error")],
        statuses(&answer)
    );
    Ok(())
}

// conformance: CP-12 CP-30
#[tokio::test]
async fn an_unreachable_node_under_all_or_nothing_is_a_504_carrying_the_envelope() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a.uri(), unreachable::BASE, 2000, true)?;
    let (status, text) = call(app, post(body(&patient_query())?, None)?).await?;
    assert_eq!(StatusCode::GATEWAY_TIMEOUT, status, "§11.2, N37: {text}");
    let answer = failing_envelope(&text, &patient_query())?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "offline")],
        statuses(&answer)
    );
    Ok(())
}

// conformance: CP-12 CP-30
#[tokio::test]
async fn a_node_timing_out_under_all_or_nothing_is_a_504_carrying_the_envelope() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_after(SLACK + SLACK).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a.uri(), &b.uri(), millis(SLACK)?, true)?;
    let (status, text) = call(app, post(body(&patient_query())?, None)?).await?;
    assert_eq!(StatusCode::GATEWAY_TIMEOUT, status, "§11.2, N37: {text}");
    let answer = failing_envelope(&text, &patient_query())?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "time-out")],
        statuses(&answer)
    );
    Ok(())
}

// conformance: CP-30
#[tokio::test]
async fn a_424_and_a_504_echo_the_clients_q() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let failing = node_failing(500).await;
    let dir = tempfile::tempdir()?;
    for (b, expected) in [
        (failing.uri(), StatusCode::FAILED_DEPENDENCY),
        (unreachable::BASE.to_owned(), StatusCode::GATEWAY_TIMEOUT),
    ] {
        let app = gateway(dir.path(), &a.uri(), &b, 2000, true)?;
        let (status, text) = call(app, post(body(&patient_query())?, None)?).await?;
        assert_eq!(expected, status, "{text}");
        let answer: Answer = serde_json::from_str(&text)?;
        assert_eq!(
            patient_query(),
            answer.q,
            "the §11.4 envelope is the result set, and N17 populates q: {text}"
        );
    }
    Ok(())
}

// conformance: CP-12 CP-30
#[tokio::test]
async fn a_node_timing_out_under_best_effort_is_a_200_reporting_it() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_after(SLACK + SLACK).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &a.uri(), &b.uri(), millis(SLACK)?, true)?;
    let (status, text) = call(app, post(body(&patient_query())?, Some("partial"))?).await?;
    assert_eq!(StatusCode::OK, status, "§11.2, §11.4: {text}");
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(!answer.meta.federation.complete);
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "time-out")],
        statuses(&answer)
    );
    assert_eq!(1, answer.rows.len(), "the answering node's rows come back");
    Ok(())
}

// conformance: CP-25
#[tokio::test]
async fn an_unexposed_its_rest_area_is_a_501_not_implemented() -> TestResult {
    let dir = tempfile::tempdir()?;
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    // Each request the book's not-implemented entry names, on a gateway with a
    // registry and no stored-query registry.
    for (verb, path) in [
        (Method::GET, format!("/v1/demographic/party/{PATIENT}")),
        (Method::GET, format!("/v1/admin/ehr/{PATIENT}")),
        (
            Method::GET,
            "/v1/query/org.example::compositions".to_owned(),
        ),
        (
            Method::POST,
            "/v1/query/org.example::compositions".to_owned(),
        ),
        (Method::GET, format!("/v1/no-such-area/{PATIENT}")),
        (Method::DELETE, "/v1/query/aql".to_owned()),
        (Method::PATCH, format!("/v1/ehr/{EHR_A}")),
    ] {
        let app = gateway(dir.path(), &a.uri(), &b.uri(), 2000, true)?;
        let request = Request::builder()
            .method(verb.clone())
            .uri(&path)
            .body(Body::empty())?;
        let (status, text) = call(app, request).await?;
        assert_eq!(
            StatusCode::NOT_IMPLEMENTED,
            status,
            "§7a.1, N32: {verb} {path}"
        );
        assert_eq!("not-implemented", checked(status, &text)?.code);
        quotes_nothing(&text);
    }
    let asked = a.received_requests().await.ok_or("recording is on")?.len()
        + b.received_requests().await.ok_or("recording is on")?.len();
    assert_eq!(0, asked, "a 501 asks no node");
    Ok(())
}

#[tokio::test]
async fn a_path_outside_every_surface_is_a_404_not_found() -> TestResult {
    let dir = tempfile::tempdir()?;
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let app = gateway(dir.path(), &a.uri(), &b.uri(), 2000, true)?;
    let path = format!("/patients/{PATIENT}");
    let (status, text) = call(app, Request::get(&path).body(Body::empty())?).await?;
    assert_eq!(StatusCode::NOT_FOUND, status);
    assert_eq!("not-found", checked(status, &text)?.code);
    quotes_nothing(&text);
    Ok(())
}

/// The `(code, status)` of every row of a code table on the book page.
fn documented() -> Result<BTreeMap<String, String>, Box<dyn Error>> {
    let page = std::fs::read_to_string(BOOK_PAGE)?;
    let mut rows = BTreeMap::new();
    for line in page.lines() {
        let mut cells = line.split('|').map(str::trim).skip(1);
        let (Some(first), Some(second)) = (cells.next(), cells.next()) else {
            continue;
        };
        let Some(code) = first
            .strip_prefix('`')
            .and_then(|rest| rest.strip_suffix('`'))
        else {
            continue;
        };
        if code.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            let previous = rows.insert(code.to_owned(), second.to_owned());
            assert_eq!(None, previous, "{code} is documented once");
        }
    }
    Ok(rows)
}

#[test]
fn the_book_documents_every_code_with_its_status_and_nothing_else() -> TestResult {
    let documented = documented()?;
    let answered: BTreeMap<String, String> = Code::every()
        .map(|code| (code.as_str().to_owned(), code.status().as_str().to_owned()))
        .collect();
    let documented_codes: BTreeSet<&String> = documented.keys().collect();
    let answered_codes: BTreeSet<&String> = answered.keys().collect();
    assert_eq!(
        answered_codes, documented_codes,
        "website/book/src/integrate/errors.md lists exactly the codes the gateway answers"
    );
    assert_eq!(
        answered, documented,
        "the book names each code's status (§11.2)"
    );
    Ok(())
}

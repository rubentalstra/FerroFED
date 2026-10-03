// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The resolution step through a PIX Manager configured by `[pixm]`: one
//! ITI-83 call resolves the patient at every member, only a member that knows
//! the patient is asked, an unknown patient is `not-resolved` and fails
//! nothing, and a Manager that cannot answer fails the query `424` (§5.2,
//! §5.3, §11.1, §11.3; N3, N6, N8, N33; Annex A.1). §11.3 covers only an
//! answered lookup, so the outage case has no governing specification: our own
//! design.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

use axum::Router;
use ferrofed_identity::pixm::PixmConfigError;
use ferrofed_server::config::Config;
use ferrofed_server::config::error;
use ferrofed_server::federation::{Federation, FederationError};
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::unreachable;
use http::StatusCode;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{
    Answer, EHR_A, EHR_B, NAMESPACE, PATIENT, body, gateway, node_answering, patient_query, post,
    registry, schema, statuses, wire,
};
use crate::request_log::logged;
use crate::support::{call, request_lines};

type TestResult = Result<(), Box<dyn Error>>;

/// The `ehr_id` domains of node A and node B at the PIX Manager.
const DOMAIN_A: &str = "urn:oid:2.999.10";
const DOMAIN_B: &str = "urn:oid:2.999.20";

const OPERATION: &str = "/fhir/Patient/$ihe-pix";

/// A PIX Manager answering ITI-83 with one `targetIdentifier` per
/// `(domain, ehr_id)`.
async fn manager_matching(identifiers: &[(&str, &str)]) -> Server {
    let parameter: Vec<String> = identifiers
        .iter()
        .map(|(system, value)| {
            format!(
                r#"{{"name":"targetIdentifier","valueIdentifier":{{"system":"{system}","value":"{value}"}}}}"#
            )
        })
        .collect();
    // NOTE: FHIR R4 JSON §2.6.2, an array is never empty, so a match with no
    // identifier omits `parameter`.
    let answer = if parameter.is_empty() {
        r#"{"resourceType":"Parameters"}"#.to_owned()
    } else {
        format!(
            r#"{{"resourceType":"Parameters","parameter":[{}]}}"#,
            parameter.join(",")
        )
    };
    manager_answering(200, answer).await
}

/// A PIX Manager answering ITI-83 with `status` and `answer`.
async fn manager_answering(status: u16, answer: String) -> Server {
    let server = Server::start().await;
    Mock::given(method("GET"))
        .and(path(OPERATION))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_raw(answer.into_bytes(), "application/fhir+json"),
        )
        .mount(&server)
        .await;
    server
}

/// A PIX Manager that does not know the patient (ITI-83 §2:3.83.4.2.3, case 3).
async fn manager_not_knowing() -> Server {
    manager_answering(
        404,
        r#"{"resourceType":"OperationOutcome","issue":[{"severity":"error","code":"not-found"}]}"#
            .to_owned(),
    )
    .await
}

/// The `[pixm]` table of one Manager at `pix` serving node A and node B.
fn pixm(pix: &str) -> String {
    format!(
        "[[pixm.manager]]\nurl = \"{pix}/fhir/\"\n\n[pixm.manager.members]\n\"node-a\" = \"{DOMAIN_A}\"\n\"node-b\" = \"{DOMAIN_B}\"\n"
    )
}

/// A production gateway over node A and node B, resolving through the PIX
/// Manager at `pix`.
fn pix_gateway(dir: &Path, a: &str, b: &str, pix: &str) -> Result<Router, Box<dyn Error>> {
    gateway(dir, &registry(a, b, ""), "", &pixm(pix))
}

/// Every byte the PIX Manager received.
async fn asked(server: &Server) -> Result<usize, Box<dyn Error>> {
    Ok(server
        .received_requests()
        .await
        .ok_or("recording is on")?
        .len())
}

// conformance: CP-3
#[tokio::test]
async fn one_pix_call_resolves_the_patient_and_both_members_answer() -> TestResult {
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let pix = Server::start().await;
    Mock::given(method("GET"))
        .and(path(OPERATION))
        .and(query_param("sourceIdentifier", format!("{NAMESPACE}|{PATIENT}")))
        .and(query_param("targetSystem", DOMAIN_A))
        .and(query_param("targetSystem", DOMAIN_B))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(
                r#"{{"resourceType":"Parameters","parameter":[{{"name":"targetIdentifier","valueIdentifier":{{"system":"{DOMAIN_A}","value":"{EHR_A}"}}}},{{"name":"targetIdentifier","valueIdentifier":{{"system":"{DOMAIN_B}","value":"{EHR_B}"}}}}]}}"#
            )
            .into_bytes(),
            "application/fhir+json",
        ))
        .expect(1)
        .mount(&pix)
        .await;
    let dir = tempfile::tempdir()?;
    let app = pix_gateway(dir.path(), &a.uri(), &b.uri(), &pix.uri())?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(answer.meta.federation.complete, "{text}");
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "active")],
        statuses(&answer)
    );
    assert_eq!(2, answer.rows.len(), "{text}");
    assert!(
        wire(&a).await?.contains(EHR_A),
        "node A is asked by its own ehr_id"
    );
    assert!(
        wire(&b).await?.contains(EHR_B),
        "node B is asked by its own ehr_id"
    );
    Ok(())
}

// conformance: CP-36
#[tokio::test]
async fn a_member_the_patient_is_not_known_at_is_not_resolved_and_never_asked() -> TestResult {
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let pix = manager_matching(&[(DOMAIN_A, EHR_A)]).await;
    let dir = tempfile::tempdir()?;
    let app = pix_gateway(dir.path(), &a.uri(), &b.uri(), &pix.uri())?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "an unknown patient fails nothing (N6): {text}"
    );
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(
        !answer.meta.federation.complete,
        "a not-resolved member leaves the answer incomplete (§11.1): {text}"
    );
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "not-resolved")],
        statuses(&answer)
    );
    assert_eq!(1, answer.rows.len(), "{text}");
    assert_eq!(1, asked(&a).await?, "node A is asked");
    assert_eq!(0, asked(&b).await?, "node B is not asked (N8)");
    Ok(())
}

// conformance: CP-12 CP-36
#[tokio::test]
async fn a_patient_known_nowhere_is_200_with_no_rows_and_every_member_not_resolved() -> TestResult {
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let pix = manager_not_knowing().await;
    let dir = tempfile::tempdir()?;
    let app = pix_gateway(dir.path(), &a.uri(), &b.uri(), &pix.uri())?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "never 404 or 424 (§11.3): {text}");
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(!answer.meta.federation.complete, "{text}");
    assert_eq!(
        vec![
            ("node-a-pub", "not-resolved"),
            ("node-b-pub", "not-resolved")
        ],
        statuses(&answer)
    );
    assert!(answer.rows.is_empty(), "{text}");
    assert_eq!(0, asked(&a).await?, "node A is not asked");
    assert_eq!(0, asked(&b).await?, "node B is not asked");
    Ok(())
}

// conformance: CP-12
#[tokio::test]
async fn an_empty_cross_reference_is_200_with_no_rows_and_every_member_not_resolved() -> TestResult
{
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let pix = manager_matching(&[]).await;
    let dir = tempfile::tempdir()?;
    let app = pix_gateway(dir.path(), &a.uri(), &b.uri(), &pix.uri())?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(!answer.meta.federation.complete, "{text}");
    assert_eq!(
        vec![
            ("node-a-pub", "not-resolved"),
            ("node-b-pub", "not-resolved")
        ],
        statuses(&answer)
    );
    assert!(answer.rows.is_empty(), "{text}");
    assert_eq!(0, asked(&a).await? + asked(&b).await?, "no node is asked");
    Ok(())
}

#[tokio::test]
async fn a_pix_manager_that_fails_fails_the_query_424_and_no_node_is_asked() -> TestResult {
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let pix = manager_answering(500, "{}".to_owned()).await;
    let dir = tempfile::tempdir()?;
    let app = pix_gateway(dir.path(), &a.uri(), &b.uri(), &pix.uri())?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(
        StatusCode::FAILED_DEPENDENCY,
        status,
        "a resolver that cannot answer fails the query (§11.1): {text}"
    );
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(!answer.meta.federation.complete, "{text}");
    assert_eq!(
        vec![
            ("node-a-pub", "not-resolved"),
            ("node-b-pub", "not-resolved")
        ],
        statuses(&answer)
    );
    assert!(
        answer.rows.is_empty(),
        "a failing query returns no rows: {text}"
    );
    assert_eq!(0, asked(&a).await? + asked(&b).await?, "no node is asked");
    Ok(())
}

#[tokio::test]
async fn an_unreachable_pix_manager_fails_the_query_424() -> TestResult {
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let dir = tempfile::tempdir()?;
    let app = pix_gateway(dir.path(), &a.uri(), &b.uri(), unreachable::BASE)?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::FAILED_DEPENDENCY, status, "{text}");
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(answer.rows.is_empty(), "{text}");
    assert_eq!(0, asked(&a).await? + asked(&b).await?, "no node is asked");
    Ok(())
}

// conformance: CP-3
#[tokio::test]
async fn the_identifier_and_namespace_reach_the_pix_manager_and_no_node() -> TestResult {
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let pix = manager_matching(&[(DOMAIN_A, EHR_A), (DOMAIN_B, EHR_B)]).await;
    let dir = tempfile::tempdir()?;
    let app = pix_gateway(dir.path(), &a.uri(), &b.uri(), &pix.uri())?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    assert!(
        wire(&pix).await?.contains(PATIENT),
        "the PIX Manager is asked about the patient (§5.2)"
    );
    for (node, captured) in [("A", wire(&a).await?), ("B", wire(&b).await?)] {
        assert!(!captured.is_empty(), "node {node} was asked");
        assert!(
            !captured.contains(PATIENT),
            "the identifier reached node {node} (§5.4.1, N33): {captured}"
        );
        assert!(
            !captured.contains(NAMESPACE),
            "the namespace reached node {node} (§5.4.1, N33): {captured}"
        );
    }
    Ok(())
}

/// The federation `text` configures, refused, with the registry document of
/// two unreachable nodes written into `dir`.
fn load_refusal(dir: &Path, text: &str) -> Result<FederationError, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(
        &document,
        registry("http://127.0.0.1:9/a/", "http://127.0.0.1:9/b/", ""),
    )?;
    let document = toml::Value::String(document.display().to_string());
    let text = format!(
        "{text}\n[registry]\ndocument = {document}\n\n[federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    match Federation::load(&settings) {
        Ok(_) => Err("the federation was built".into()),
        Err(error) => Ok(error),
    }
}

#[test]
fn a_pix_manager_and_the_dev_cross_reference_together_refuse_to_boot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let dev = format!(
        "profile = \"development\"\n\n[[dev.crossref]]\nnamespace = \"{NAMESPACE}\"\nvalue = \"{PATIENT}\"\nmember = \"node-a\"\nehr_id = \"{EHR_A}\"\n"
    );
    let error = load_refusal(
        dir.path(),
        &format!("{dev}\n{}", pixm("http://127.0.0.1:9")),
    )?;
    assert!(
        matches!(error, FederationError::TwoResolvers),
        "exactly one resolver is active (no specification governs this: our own design): {error:?}"
    );
    Ok(())
}

#[test]
fn a_pix_manager_that_leaves_a_member_unresolved_refuses_to_boot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = format!(
        "[[pixm.manager]]\nurl = \"http://127.0.0.1:9/fhir/\"\n\n[pixm.manager.members]\n\"node-a\" = \"{DOMAIN_A}\"\n"
    );
    let error = load_refusal(dir.path(), &text)?;
    assert!(
        matches!(
            &error,
            FederationError::Pixm(PixmConfigError::UnresolvedMember(member)) if member.as_str() == "node-b"
        ),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn a_registry_without_a_declared_node_selection_refuses_to_boot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let document = dir.path().join("registry.toml");
    std::fs::write(
        &document,
        registry("http://127.0.0.1:9/a/", "http://127.0.0.1:9/b/", ""),
    )?;
    let document = toml::Value::String(document.display().to_string());
    let text = format!(
        "{}\n[registry]\ndocument = {document}\n",
        pixm("http://127.0.0.1:9")
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    match Federation::load(&settings) {
        Err(FederationError::NodeSelectionUndeclared) => Ok(()),
        other => Err(format!(
            "the node selection is declared, never defaulted (§4.3, N4): {other:?}"
        )
        .into()),
    }
}

#[test]
fn a_node_selection_the_gateway_does_not_know_refuses_to_boot() -> TestResult {
    let text = "[federation]\nnode_selection = \"nearest\"\n";
    match Config::from_sources(Some(&crate::support::signed(text)), &BTreeMap::new()) {
        Err(error::Error::Parse { .. }) => Ok(()),
        other => Err(format!(
            "only ask-all and localized are node selections the gateway offers, refused at parse: {other:?}"
        )
        .into()),
    }
}

#[test]
fn a_pix_manager_without_a_registry_refuses_to_boot() -> TestResult {
    let settings = Config::from_sources(
        Some(&crate::support::signed(&pixm("http://127.0.0.1:9"))),
        &BTreeMap::new(),
    )?
    .resolve()?;
    match Federation::load(&settings) {
        Err(FederationError::PixmWithoutRegistry) => Ok(()),
        other => Err(format!("refused for its missing registry: {other:?}").into()),
    }
}

#[test]
fn a_pix_manager_url_that_does_not_parse_refuses_to_boot_naming_its_key() -> TestResult {
    let text = "[[pixm.manager]]\nurl = \"not a url\"\n";
    match Config::from_sources(Some(&crate::support::signed(text)), &BTreeMap::new())?.resolve() {
        Err(error::Error::Url { key, .. }) if key == "pixm.manager[0].url" => Ok(()),
        other => Err(format!("refused naming the key: {other:?}").into()),
    }
}

#[test]
fn a_configured_binding_lifetime_is_the_one_resolved() -> TestResult {
    let text = "[federation]\nbinding_ttl_ms = 1500\n";
    let settings =
        Config::from_sources(Some(&crate::support::signed(text)), &BTreeMap::new())?.resolve()?;
    if settings.federation.binding_ttl == std::time::Duration::from_millis(1_500) {
        Ok(())
    } else {
        Err(format!(
            "the configured lifetime, not the default: {:?}",
            settings.federation.binding_ttl
        )
        .into())
    }
}

#[test]
fn a_zero_binding_lifetime_refuses_to_boot() -> TestResult {
    let text = "[federation]\nbinding_ttl_ms = 0\n";
    match Config::from_sources(Some(&crate::support::signed(text)), &BTreeMap::new())?.resolve() {
        Err(error::Error::Zero { key }) if key == "federation.binding_ttl_ms" => Ok(()),
        other => Err(format!("refused naming the key: {other:?}").into()),
    }
}

#[test]
fn a_pix_manager_secret_never_reaches_debug_output() -> TestResult {
    let text = format!(
        "{}\n[pixm.manager.credentials]\nbearer_token = \"synthetic-pix-token\"\n",
        pixm("http://127.0.0.1:9")
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    // NOTE: a failure message never echoes the rendering, which would put the
    // leaked secret into the test log it is meant to keep it out of.
    assert!(
        !format!("{settings:?}").contains("synthetic-pix-token"),
        "Debug output carries the PIX Manager token"
    );
    Ok(())
}

#[test]
fn a_resolved_query_leaves_the_identifier_in_no_log_line() -> TestResult {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (a, b, pix) = runtime.block_on(async {
        (
            node_answering("uid-at-a::cdr-a.example.org::1").await,
            node_answering("uid-at-b::cdr-b.example.org::1").await,
            manager_matching(&[(DOMAIN_A, EHR_A)]).await,
        )
    });
    let failing = runtime.block_on(manager_answering(500, "{}".to_owned()));
    let dir = tempfile::tempdir()?;
    let resolved = pix_gateway(dir.path(), &a.uri(), &b.uri(), &pix.uri())?;
    let failed = pix_gateway(dir.path(), &a.uri(), &b.uri(), &failing.uri())?;

    let mut text = logged(&resolved, "trace", vec![post(body(&patient_query())?)?])?;
    text.push_str(&logged(
        &failed,
        "trace",
        vec![post(body(&patient_query())?)?],
    )?);
    assert_eq!(
        2,
        request_lines(&text)?.len(),
        "both queries were logged, so the check is not vacuous: {text}"
    );
    assert!(
        !text.contains(PATIENT),
        "the identifier reached the log: {text}"
    );
    Ok(())
}

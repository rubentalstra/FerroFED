// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Consent stays with the node, through the real configuration path: track 7's
//! three configurations (§16.3), the optional Step-1 pre-filter (N27a,
//! §13.2.1), and the node as the gate in every case (N26, N27, §14.3).
//!
//! These score the gateway's half of the consent points: CP-36, which CP-19
//! names as that half. CP-19 itself is a Node obligation, scored against the
//! harness nodes (§16.2). A node's refusal is `consent-denied` only where the
//! registry lists the `code` its ITS-REST `Error` carries; ITS-REST defines no
//! consent signal, so every other refusal is `node-error` (§11.1).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;

use axum::Router;
use axum::body::Body;
use ferrofed_testkit::mock::Server;
use http::{Method, Request, StatusCode};
use serde::Deserialize;

use crate::directive::directed;
use crate::facade::{
    Answer, EHR_A, EHR_B, NAMESPACE, PATIENT, body, crossref, gateway, node_answering, post,
    received, registry, schema, statuses, wire,
};
use crate::support::call;

type TestResult = Result<(), Box<dyn Error>>;

/// The consent refusal code the registry lists for node B.
const REFUSAL_CODE: &str = "consent-refused";

/// The registry line that lists [`REFUSAL_CODE`] for node B's endpoint.
fn refusal_codes() -> String {
    format!("consent_refusal_codes = [\"{REFUSAL_CODE}\"]\n")
}

/// A node refusing `POST /v1/query/aql` with a `403` whose ITS-REST `Error`
/// carries `code`.
async fn node_refusing(code: &str) -> Server {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = Server::start().await;
    let error = format!(
        r#"{{"message":"synthetic consent refusal","validationErrors":[],"code":"{code}"}}"#
    );
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(
            ResponseTemplate::new(403).set_body_raw(error.into_bytes(), "application/json"),
        )
        .mount(&server)
        .await;
    server
}

/// The `[[dev.consent_denied]]` row denying asking `member` about `value`.
fn denied(value: &str, member: &str) -> String {
    format!(
        "\n[[dev.consent_denied]]\nnamespace = \"{NAMESPACE}\"\nvalue = \"{value}\"\nmember = \"{member}\"\n"
    )
}

/// A development gateway over node A and node B resolving the patient at
/// both, with `extra` in node B's endpoint entry and `consent` beside the
/// cross-reference.
fn gateway_over(
    dir: &std::path::Path,
    (a, b): (&Server, &Server),
    extra: &str,
    consent: &str,
) -> Result<Router, Box<dyn Error>> {
    gateway(
        dir,
        &registry(&a.uri(), &b.uri(), extra),
        "profile = \"development\"",
        &format!(
            "{}{consent}",
            crossref(&[("node-a", EHR_A), ("node-b", EHR_B)])
        ),
    )
}

/// The `latency_ms` each endpoint record carries.
#[derive(Debug, Deserialize)]
struct Timed {
    meta: TimedMeta,
}

#[derive(Debug, Deserialize)]
struct TimedMeta {
    federation: TimedFederation,
}

#[derive(Debug, Deserialize)]
struct TimedFederation {
    endpoints: Vec<TimedEndpoint>,
}

#[derive(Debug, Deserialize)]
struct TimedEndpoint {
    id: String,
    latency_ms: Option<u64>,
}

/// The `latency_ms` of `endpoint` in the answer `text`.
fn latency_of(text: &str, endpoint: &str) -> Result<Option<u64>, Box<dyn Error>> {
    let timed: Timed = serde_json::from_str(text)?;
    timed
        .meta
        .federation
        .endpoints
        .into_iter()
        .find(|record| record.id == endpoint)
        .map(|record| record.latency_ms)
        .ok_or_else(|| format!("{endpoint} is not reported").into())
}

/// Runs the patient query, checks the answer against the schema, and
/// returns its status, its text and its typed read.
async fn ask(app: Router, aql: &str) -> Result<(StatusCode, String, Answer), Box<dyn Error>> {
    let (status, text) = call(app, post(body(aql)?)?).await?;
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    Ok((status, text, answer))
}

// conformance: CP-30 CP-36
#[tokio::test]
async fn with_no_consent_service_a_node_refusal_is_reported_and_the_query_succeeds() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_refusing(REFUSAL_CODE).await;
    let dir = tempfile::tempdir()?;
    let app = gateway_over(dir.path(), (&a, &b), &refusal_codes(), "")?;

    let (status, text, answer) = ask(app, &crate::facade::patient_query()).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "§11.3: a refusal fails nothing: {text}"
    );
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "consent-denied")],
        statuses(&answer),
        "track 7 (a): the endpoints reflect the refusal"
    );
    assert!(!answer.meta.federation.complete, "§11.3 clears complete");
    let uids: Vec<Option<&String>> = answer.rows.iter().map(|row| row.get(1)).collect();
    assert_eq!(
        vec![Some(&"uid-at-a".to_owned())],
        uids,
        "node B adds no row"
    );
    assert!(
        latency_of(&text, "node-b-pub")?.is_some(),
        "N40: node B was dispatched to, so its latency is reported"
    );
    assert_eq!(
        1,
        received(&b).await?.len(),
        "N27: the node was asked and decided"
    );
    Ok(())
}

#[tokio::test]
async fn a_refusal_whose_code_the_registry_does_not_list_is_a_node_error() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_refusing("some-other-code").await;
    let dir = tempfile::tempdir()?;
    let app = gateway_over(dir.path(), (&a, &b), &refusal_codes(), "")?;

    let (status, text, answer) = ask(app, &crate::facade::patient_query()).await?;
    assert_eq!(StatusCode::FAILED_DEPENDENCY, status, "§11.4: {text}");
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "node-error")],
        statuses(&answer)
    );
    Ok(())
}

// conformance: CP-36
#[tokio::test]
async fn a_member_the_prefilter_denies_is_reported_and_never_contacted() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway_over(dir.path(), (&a, &b), "", &denied(PATIENT, "node-b"))?;

    let (status, text, answer) = ask(app, &crate::facade::patient_query()).await?;
    assert_eq!(StatusCode::OK, status, "§11.3: {text}");
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "consent-denied")],
        statuses(&answer),
        "track 7 (b), N27a: the dropped member is reported"
    );
    assert!(!answer.meta.federation.complete);
    assert_eq!(1, answer.rows.len(), "node A's row only");
    assert_eq!(
        None,
        latency_of(&text, "node-b-pub")?,
        "N40: no request existed, so no latency is invented"
    );
    assert!(
        wire(&b).await?.is_empty(),
        "track 7 (b): node B receives nothing at all"
    );
    let at_a = wire(&a).await?;
    assert!(!at_a.is_empty(), "node A was asked");
    assert!(!at_a.contains(PATIENT), "§5.4.1, N33: {at_a}");
    Ok(())
}

// conformance: CP-30 CP-36
#[tokio::test]
async fn a_member_the_prefilter_leaves_in_is_asked_and_its_refusal_still_reported() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_refusing(REFUSAL_CODE).await;
    let dir = tempfile::tempdir()?;
    let consent = denied("SENTINEL-OTHER-PATIENT-7z", "node-b");
    let app = gateway_over(dir.path(), (&a, &b), &refusal_codes(), &consent)?;

    let (status, text, answer) = ask(app, &crate::facade::patient_query()).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "track 7 (c): the query still succeeds: {text}"
    );
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "consent-denied")],
        statuses(&answer),
        "§14.3: a member nothing upstream ruled out is asked, never cleared"
    );
    assert_eq!(
        1,
        received(&b).await?.len(),
        "N26, N27: the node decides, and it refused"
    );
    assert!(latency_of(&text, "node-b-pub")?.is_some(), "N40");
    Ok(())
}

/// The registry entries of a third member, node C at `c`, which no
/// cross-reference row names.
fn node_c(c: &str) -> String {
    format!(
        "\n[[organisation]]\nid = \"org-c\"\n\n[[node]]\nid = \"node-c\"\norganisation = \"org-c\"\nsystem_id = \"cdr-c.example.org\"\n\n[[endpoint]]\nid = \"node-c-pub\"\nnode = \"node-c\"\nurl = \"{c}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-c\"\n"
    )
}

/// A development gateway over nodes A, B and C under localized node
/// selection, the development cross-reference (rows for A and B) serving as
/// the localizer, node B listing [`REFUSAL_CODE`], and `consent` beside the
/// cross-reference.
fn localized_gateway(
    dir: &std::path::Path,
    (a, b, c): (&Server, &Server, &Server),
    consent: &str,
) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    let extra = format!("{}{}", refusal_codes(), node_c(&c.uri()));
    std::fs::write(&document, registry(&a.uri(), &b.uri(), &extra))?;
    let document = toml::Value::String(document.display().to_string());
    let rows = crossref(&[("node-a", EHR_A), ("node-b", EHR_B)]);
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"localized\"\nid = \"example-federation\"\n\n[federation.localization]\ntimeout_ms = 500\n\n{rows}{consent}"
    );
    let settings = ferrofed_server::config::Config::from_sources(
        Some(&crate::support::signed(&text)),
        &std::collections::BTreeMap::new(),
    )?
    .resolve()?;
    let federation = ferrofed_server::federation::Federation::load(&settings)?
        .ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        std::sync::Arc::new(ferrofed_server::state::AppState::with_federation(
            federation,
        )),
        &crate::facade::settings_with_room(),
    ))
}

// conformance: CP-30 CP-36
#[tokio::test]
async fn a_node_the_localizer_names_still_refuses_and_the_query_succeeds() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_refusing(REFUSAL_CODE).await;
    let c = node_answering("uid-at-c").await;
    let dir = tempfile::tempdir()?;
    let app = localized_gateway(dir.path(), (&a, &b, &c), "")?;

    let (status, text, answer) = ask(app, &crate::facade::patient_query()).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "track 7 (c): the query still succeeds: {text}"
    );
    assert_eq!(
        vec![
            ("node-a-pub", "active"),
            ("node-b-pub", "consent-denied"),
            ("node-c-pub", "not-localized"),
        ],
        statuses(&answer),
        "N27, §14.3: a localized candidate is asked, never consent-cleared"
    );
    assert!(!answer.meta.federation.complete, "§11.3");
    assert_eq!(1, answer.rows.len(), "node A's row only");
    assert_eq!(1, received(&b).await?.len(), "N26, N27: node B decided");
    assert!(
        received(&c).await?.is_empty(),
        "§14: node C was not localized"
    );
    assert!(latency_of(&text, "node-b-pub")?.is_some(), "N40");
    Ok(())
}

// conformance: CP-36
#[tokio::test]
async fn the_prefilter_runs_after_localization_over_its_candidates_only() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let c = node_answering("uid-at-c").await;
    let dir = tempfile::tempdir()?;
    let consent = format!("{}{}", denied(PATIENT, "node-b"), denied(PATIENT, "node-c"));
    let app = localized_gateway(dir.path(), (&a, &b, &c), &consent)?;

    let (status, text, answer) = ask(app, &crate::facade::patient_query()).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    assert_eq!(
        vec![
            ("node-a-pub", "active"),
            ("node-b-pub", "consent-denied"),
            ("node-c-pub", "not-localized"),
        ],
        statuses(&answer),
        "localize, then pre-filter: node C was never a candidate, so nothing decided about it"
    );
    assert!(
        wire(&b).await?.is_empty(),
        "N27a: node B is never contacted"
    );
    assert!(wire(&c).await?.is_empty());
    Ok(())
}

// conformance: CP-36
#[tokio::test]
async fn a_directed_query_reaches_the_node_that_checks_consent_itself() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_refusing(REFUSAL_CODE).await;
    let dir = tempfile::tempdir()?;
    let app = gateway_over(dir.path(), (&a, &b), &refusal_codes(), "")?;

    let (status, text, answer) = ask(app, &directed(r#"ENDPOINT ["node-b-pub"]"#)).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    assert_eq!(
        vec![("node-a-pub", "excluded"), ("node-b-pub", "consent-denied")],
        statuses(&answer),
        "§13.2.1: no localization ran, and the node still applied consent"
    );
    assert_eq!(
        1,
        received(&b).await?.len(),
        "N27: the directed node is asked"
    );
    assert!(received(&a).await?.is_empty(), "§8: node A was not named");
    Ok(())
}

#[tokio::test]
async fn the_prefilter_is_declared_in_options_as_development() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway_over(dir.path(), (&a, &b), "", &denied(PATIENT, "node-b"))?;

    let request = Request::builder()
        .method(Method::OPTIONS)
        .uri("/")
        .body(Body::empty())?;
    let (status, text) = call(app, request).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    schema::validate_options(&text)?;
    let declared: OptionsConsent = serde_json::from_str(&text)?;
    assert_eq!(
        ("development-static", "pass-to-node"),
        (
            declared.federation.consent.prefilter.as_str(),
            declared.federation.consent.on_unavailable.as_str()
        )
    );
    assert!(!text.contains(PATIENT), "§7a.2: no patient data");
    Ok(())
}

/// The `federation.consent` member of `OPTIONS {base}/`.
#[derive(Debug, Deserialize)]
struct OptionsConsent {
    federation: OptionsFederation,
}

#[derive(Debug, Deserialize)]
struct OptionsFederation {
    consent: Declared,
}

#[derive(Debug, Deserialize)]
struct Declared {
    prefilter: String,
    on_unavailable: String,
}

#[test]
fn consent_rows_outside_the_development_profile_refuse_to_start() -> TestResult {
    let dir = tempfile::tempdir()?;
    let built = gateway(
        dir.path(),
        &registry("https://cdr-a.example.org", "https://cdr-b.example.org", ""),
        "profile = \"production\"",
        &format!(
            "{}{}",
            crossref(&[("node-a", EHR_A)]),
            denied(PATIENT, "node-b")
        ),
    );
    assert!(
        built.is_err(),
        "a production gateway refuses the [dev] table"
    );
    Ok(())
}

/// A resolver that knows the patient at node A and node B.
struct BothKnown;

#[async_trait::async_trait]
impl ferrofed_identity::resolver::Resolver for BothKnown {
    async fn resolve(
        &self,
        _patient: &ferrofed_identity::patient::PatientRef,
        members: &[ferrofed_registry::id::NodeId],
        _deadline: std::time::Instant,
    ) -> std::collections::BTreeMap<
        ferrofed_registry::id::NodeId,
        ferrofed_identity::resolver::Resolution,
    > {
        members
            .iter()
            .filter_map(|member| {
                let ehr = if member.as_str() == "node-a" {
                    EHR_A
                } else {
                    EHR_B
                };
                let ehr = ferrofed_registry::id::EhrId::new(ehr).ok()?;
                Some((
                    member.clone(),
                    ferrofed_identity::resolver::Resolution::Resolved(ehr),
                ))
            })
            .collect()
    }
}

/// A consent pre-filter whose service never answers.
struct Down;

#[async_trait::async_trait]
impl ferrofed_identity::consent::ConsentPrefilter for Down {
    async fn prefilter(
        &self,
        _patient: &ferrofed_identity::patient::PatientRef,
        _candidates: &[ferrofed_registry::id::NodeId],
        _deadline: std::time::Instant,
    ) -> ferrofed_identity::consent::ConsentDecision {
        ferrofed_identity::consent::ConsentDecision::Unavailable(
            ferrofed_identity::consent::ConsentError::DeadlineExceeded,
        )
    }

    fn mode(&self) -> &'static str {
        "test-down"
    }
}

// conformance: CP-36
#[tokio::test]
async fn a_prefilter_that_cannot_answer_leaves_every_candidate_to_its_node() -> TestResult {
    use std::sync::Arc;
    use std::time::Duration;

    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let snapshot = ferrofed_registry::snapshot::RegistrySnapshot::from_toml_str(&registry(
        &a.uri(),
        &b.uri(),
        "",
    ))?;
    let transport =
        openehr_its::rest::client::ReqwestTransport::with_timeout(Duration::from_secs(5))?;
    let clients = ferrofed_engine::dispatch::NodeClients::from_snapshot(
        &snapshot,
        &transport,
        &std::collections::BTreeMap::new(),
    )?;
    let federation = ferrofed_server::federation::Federation::new(
        openehr_federation::id::FederationId::new("example-federation")?,
        snapshot,
        clients,
        Some(Arc::new(BothKnown)),
        openehr_federation::aql::Context::new(openehr_federation::aql::Targeting::AskAll),
        ferrofed_engine::fanout::Budget::new(Duration::from_secs(2), Duration::from_secs(3))?,
    )
    .with_consent_prefilter(Arc::new(Down))
    .with_signer(crate::support::signer("example-federation")?);
    let app = ferrofed_server::router(
        Arc::new(ferrofed_server::state::AppState::with_federation(
            federation,
        )),
        &crate::facade::settings_with_room(),
    );

    let (status, text, answer) = ask(app, &crate::facade::patient_query()).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "active")],
        statuses(&answer),
        "N27a, §13.2.1: no consent signal, so N27 is the sole gate and both are asked"
    );
    assert!(answer.meta.federation.complete);
    for node in [&a, &b] {
        assert_eq!(1, received(node).await?.len());
    }
    Ok(())
}

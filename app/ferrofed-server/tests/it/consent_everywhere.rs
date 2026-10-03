// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The consent pre-filter on every patient route, and its outage made
//! visible (N27a, §13.2.1).
//!
//! The read of an EHR by subject asks the pre-filter as a federated query
//! does: a denied member is never resolved or contacted, and a pre-filter
//! that cannot answer leaves every candidate to its node (N27). An outage is
//! carried in `meta.federation.consent.error`, mirroring the localizer's
//! member of §14.1 as our own design, never changing `complete` or the
//! status, and it shows on `GET /health/dependencies` under the rule the
//! members follow and in the pre-filter call metrics.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use ferrofed_engine::dispatch::NodeClients;
use ferrofed_engine::fanout::Budget;
use ferrofed_identity::consent::{ConsentDecision, ConsentError, ConsentPrefilter};
use ferrofed_identity::patient::PatientRef;
use ferrofed_identity::resolver::{Resolution, Resolver};
use ferrofed_registry::id::{EhrId, NodeId};
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Request, StatusCode, header};
use openehr_federation::aql::{Context, Targeting};
use openehr_federation::id::FederationId;
use openehr_its::rest::client::ReqwestTransport;
use serde::Deserialize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{
    EHR_A, EHR_B, NAMESPACE, PATIENT, body, crossref, gateway, node_answering, patient_query, post,
    received, registry, schema, settings_with_room, statuses, wire,
};
use crate::metrics::{count, parse};
use crate::support::{call, error_body};

type TestResult = Result<(), Box<dyn Error>>;

/// The Prometheus name of the pre-filter call counter.
const PREFILTER_CALLS: &str = "ferrofed_consent_prefilter_requests_total";

/// What a scripted consent pre-filter answers every time.
#[derive(Debug, Clone, Copy)]
enum Script {
    /// The service never answers.
    Down,
    /// The service answers with a failure `status`.
    Answers(StatusCode),
    /// The service denies asking `node-b`.
    DeniesB,
}

#[async_trait]
impl ConsentPrefilter for Script {
    async fn prefilter(
        &self,
        _patient: &PatientRef,
        candidates: &[NodeId],
        _deadline: Instant,
    ) -> ConsentDecision {
        match self {
            Self::Down => ConsentDecision::Unavailable(ConsentError::DeadlineExceeded),
            Self::Answers(status) => ConsentDecision::Unavailable(ConsentError::Answered {
                status: *status,
                source: "synthetic consent service failure".into(),
            }),
            Self::DeniesB => ConsentDecision::Denied(
                candidates
                    .iter()
                    .filter(|member| member.as_str() == "node-b")
                    .cloned()
                    .collect(),
            ),
        }
    }

    fn mode(&self) -> &'static str {
        "test-scripted"
    }
}

/// A resolver that knows the patient at the members `at` names.
struct KnownAt(&'static [(&'static str, &'static str)]);

#[async_trait]
impl Resolver for KnownAt {
    async fn resolve(
        &self,
        _patient: &PatientRef,
        members: &[NodeId],
        _deadline: Instant,
    ) -> BTreeMap<NodeId, Resolution> {
        members
            .iter()
            .filter_map(|member| {
                let known = self.0.iter().find(|(at, _)| *at == member.as_str());
                let resolution = match known {
                    Some((_, ehr)) => Resolution::Resolved(EhrId::new(*ehr).ok()?),
                    None => Resolution::Unknown,
                };
                Some((member.clone(), resolution))
            })
            .collect()
    }
}

/// A metered gateway over node A and node B resolving through `resolver`
/// and pre-filtering through `script`.
fn scripted(
    (a, b): (&Server, &Server),
    resolver: KnownAt,
    script: Script,
) -> Result<(Router, Arc<AppState>), Box<dyn Error>> {
    let snapshot = RegistrySnapshot::from_toml_str(&registry(&a.uri(), &b.uri(), ""))?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(5))?;
    let clients = NodeClients::from_snapshot(&snapshot, &transport, &BTreeMap::new())?;
    let federation = Federation::new(
        FederationId::new("example-federation")?,
        snapshot,
        clients,
        Some(Arc::new(resolver)),
        Context::new(Targeting::AskAll),
        Budget::new(Duration::from_secs(2), Duration::from_secs(3))?,
    )
    .with_consent_prefilter(Arc::new(script))
    .with_signer(crate::support::signer("example-federation")?);
    let state = Arc::new(AppState::with_federation(federation));
    Ok((
        ferrofed_server::router(Arc::clone(&state), &settings_with_room()),
        state,
    ))
}

/// The patient known at both members.
const BOTH: &[(&str, &str)] = &[("node-a", EHR_A), ("node-b", EHR_B)];

/// The patient known at node A only.
const AT_A: &[(&str, &str)] = &[("node-a", EHR_A)];

/// The patient known at node B only.
const AT_B: &[(&str, &str)] = &[("node-b", EHR_B)];

/// The `meta.federation.consent` member of an answer, when present.
#[derive(Debug, Deserialize)]
struct WithConsent {
    meta: ConsentMeta,
}

#[derive(Debug, Deserialize)]
struct ConsentMeta {
    federation: ConsentFederation,
}

#[derive(Debug, Deserialize)]
struct ConsentFederation {
    consent: Option<ConsentReport>,
}

#[derive(Debug, Deserialize)]
struct ConsentReport {
    error: String,
}

/// The state `GET /health/dependencies` reports of the consent pre-filter.
async fn consent_state(app: &Router) -> Result<Option<String>, Box<dyn Error>> {
    #[derive(Deserialize)]
    struct Report {
        consent: Option<String>,
    }
    let request = Request::get("/health/dependencies").body(Body::empty())?;
    let (status, text) = call(app.clone(), request).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    Ok(serde_json::from_str::<Report>(&text)?.consent)
}

/// The pre-filter calls the metrics of `state` counted with `outcome`.
fn prefilter_calls(state: &AppState, outcome: &str) -> Result<Option<String>, Box<dyn Error>> {
    let samples = parse(&state.metrics().render()?)?;
    Ok(count(&samples, PREFILTER_CALLS, &[("outcome", outcome)]))
}

#[tokio::test]
async fn a_query_answered_through_a_prefilter_outage_carries_it_in_meta_federation() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let (app, _state) = scripted((&a, &b), KnownAt(BOTH), Script::Down)?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "N27: every candidate is asked: {text}"
    );
    schema::validate(&text)?;
    let answer: crate::facade::Answer = serde_json::from_str(&text)?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "active")],
        statuses(&answer)
    );
    assert!(
        answer.meta.federation.complete,
        "the outage changes no status"
    );
    let carried: WithConsent = serde_json::from_str(&text)?;
    assert_eq!(
        Some(
            "the consent pre-filter could not answer: the consent service did not answer within its budget"
        ),
        carried
            .meta
            .federation
            .consent
            .as_ref()
            .map(|report| report.error.as_str()),
    );
    Ok(())
}

#[tokio::test]
async fn a_query_the_prefilter_answered_carries_no_consent_member() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let (app, _state) = scripted((&a, &b), KnownAt(BOTH), Script::DeniesB)?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let carried: WithConsent = serde_json::from_str(&text)?;
    assert!(carried.meta.federation.consent.is_none(), "{text}");
    Ok(())
}

#[tokio::test]
async fn the_prefilter_is_on_the_health_report_under_the_members_rule() -> TestResult {
    for (script, expected) in [
        (Script::Down, "down"),
        (Script::Answers(StatusCode::SERVICE_UNAVAILABLE), "failing"),
        (Script::Answers(StatusCode::BAD_REQUEST), "up"),
        (Script::DeniesB, "up"),
    ] {
        let a = node_answering("uid-at-a").await;
        let b = node_answering("uid-at-b").await;
        let (app, _state) = scripted((&a, &b), KnownAt(BOTH), script)?;
        assert_eq!(
            Some("unknown".to_owned()),
            consent_state(&app).await?,
            "nothing observed before the first request"
        );
        let (status, text) = call(app.clone(), post(body(&patient_query())?)?).await?;
        assert_eq!(StatusCode::OK, status, "{text}");
        assert_eq!(
            Some(expected.to_owned()),
            consent_state(&app).await?,
            "{script:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_gateway_with_no_prefilter_reports_none() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let dir = tempfile::tempdir()?;
    let app = gateway(
        dir.path(),
        &registry(&a.uri(), &b.uri(), ""),
        "profile = \"development\"",
        &crossref(&[("node-a", EHR_A)]),
    )?;
    assert_eq!(None, consent_state(&app).await?);
    Ok(())
}

#[tokio::test]
async fn each_prefilter_call_is_counted_by_outcome() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let (app, state) = scripted((&a, &b), KnownAt(BOTH), Script::Down)?;
    let (status, _) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status);
    assert_eq!(
        Some("1".to_owned()),
        prefilter_calls(&state, "unavailable")?
    );

    let (app, state) = scripted((&a, &b), KnownAt(BOTH), Script::DeniesB)?;
    let (status, _) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status);
    assert_eq!(Some("1".to_owned()), prefilter_calls(&state, "denied")?);
    assert_eq!(None, prefilter_calls(&state, "unavailable")?);
    Ok(())
}

/// A node answering `GET /v1/ehr/{ehr_id}` with a synthetic `EHR`.
async fn ehr_node(system: &str, ehr_id: &str) -> Server {
    let server = Server::start().await;
    let ehr = format!(
        r#"{{"system_id":{{"value":"{system}"}},"ehr_id":{{"value":"{ehr_id}"}},"time_created":{{"value":"2026-01-01T00:00:00Z"}}}}"#
    );
    Mock::given(method("GET"))
        .and(path(format!("/v1/ehr/{ehr_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ehr.into_bytes(), "application/json"))
        .mount(&server)
        .await;
    server
}

/// `GET {base}/v1/ehr` for the patient.
fn by_subject() -> Result<Request<Body>, http::Error> {
    Request::get(format!(
        "/v1/ehr?subject_id={PATIENT}&subject_namespace={NAMESPACE}"
    ))
    .header(header::ACCEPT, "application/json")
    .body(Body::empty())
}

// conformance: CP-36
#[tokio::test]
async fn the_read_by_subject_never_contacts_a_member_the_prefilter_denies() -> TestResult {
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let (app, state) = scripted((&a, &b), KnownAt(AT_A), Script::DeniesB)?;

    let (status, text) = call(app.clone(), by_subject()?).await?;
    assert_eq!(StatusCode::OK, status, "node A holds the EHR: {text}");
    assert_eq!(1, received(&a).await?.len());
    assert!(wire(&b).await?.is_empty(), "N27a: node B receives nothing");
    assert_eq!(Some("1".to_owned()), prefilter_calls(&state, "denied")?);
    assert_eq!(Some("up".to_owned()), consent_state(&app).await?);
    Ok(())
}

// conformance: CP-36
#[tokio::test]
async fn the_read_by_subject_whose_holders_are_all_denied_is_consent_denied() -> TestResult {
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let (app, _state) = scripted((&a, &b), KnownAt(AT_B), Script::DeniesB)?;

    let (status, text) = call(app, by_subject()?).await?;
    assert_eq!(StatusCode::FORBIDDEN, status, "{text}");
    let error = error_body(&text)?;
    assert_eq!("consent-denied", error.code);
    assert!(error.message.contains("node-b-pub"), "{}", error.message);
    assert!(!text.contains(PATIENT), "§5.4.3: {text}");
    assert!(wire(&b).await?.is_empty(), "N27a: node B receives nothing");
    assert!(wire(&a).await?.is_empty(), "node A does not hold it");
    Ok(())
}

#[tokio::test]
async fn the_read_by_subject_through_a_prefilter_outage_leaves_the_member_to_its_node() -> TestResult
{
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let (app, state) = scripted((&a, &b), KnownAt(AT_B), Script::Down)?;

    let (status, text) = call(app.clone(), by_subject()?).await?;
    assert_eq!(StatusCode::OK, status, "N27: node B decides: {text}");
    assert_eq!(1, received(&b).await?.len());
    assert_eq!(Some("down".to_owned()), consent_state(&app).await?);
    assert_eq!(
        Some("1".to_owned()),
        prefilter_calls(&state, "unavailable")?
    );
    Ok(())
}

#[tokio::test]
async fn the_static_prefilter_applies_to_the_read_by_subject_through_configuration() -> TestResult {
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let dir = tempfile::tempdir()?;
    let denied = format!(
        "\n[[dev.consent_denied]]\nnamespace = \"{NAMESPACE}\"\nvalue = \"{PATIENT}\"\nmember = \"node-b\"\n"
    );
    let app = gateway(
        dir.path(),
        &registry(&a.uri(), &b.uri(), ""),
        "profile = \"development\"",
        &format!("{}{denied}", crossref(&[("node-b", EHR_B)])),
    )?;

    let (status, text) = call(app, by_subject()?).await?;
    assert_eq!(StatusCode::FORBIDDEN, status, "{text}");
    assert_eq!("consent-denied", error_body(&text)?.code);
    assert!(wire(&b).await?.is_empty());
    Ok(())
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The localizer beyond the federated query: the read of an EHR by subject
//! is localized as an undirected query (N4, §5.2, §14.1), and the localizer
//! shows on `GET /health/dependencies` under the rule the members follow and
//! in the localizer call metrics.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use ferrofed_engine::dispatch::NodeClients;
use ferrofed_engine::fanout::Budget;
use ferrofed_identity::localizer::{Localization, Localizer, LocalizerError, OnFailure};
use ferrofed_identity::patient::PatientRef;
use ferrofed_identity::resolver::{Resolution, Resolver};
use ferrofed_registry::id::{EhrId, NodeId};
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_server::federation::Federation;
use ferrofed_server::localization::LocalizationPolicy;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{HeaderValue, Request, StatusCode, header};
use openehr_federation::aql::{Context, Targeting};
use openehr_federation::headers::ENDPOINT;
use openehr_federation::id::FederationId;
use openehr_its::rest::client::ReqwestTransport;
use serde::Deserialize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{
    EHR_A, EHR_B, NAMESPACE, PATIENT, body, node_answering, patient_query, post, received,
    registry, settings_with_room, wire,
};
use crate::metrics::{count, parse};
use crate::support::{call, error_body};

type TestResult = Result<(), Box<dyn Error>>;

/// A gateway, its state, and its localizer's call counter.
type Built = (Router, Arc<AppState>, Arc<AtomicUsize>);

/// The Prometheus name of the localizer call counter.
const LOCALIZER_CALLS: &str = "ferrofed_localizer_requests_total";

/// What a scripted localizer answers every time.
#[derive(Debug, Clone, Copy)]
enum Script {
    /// It names `node-a` only.
    NamesA,
    /// No member holds the patient's data.
    Nothing,
    /// The service never answers.
    Down,
    /// The service answers with a failure `status`.
    Answers(StatusCode),
    /// The exchange took place and its audit message could not be recorded.
    AuditFailed,
}

/// A scripted localizer that counts its calls.
struct Scripted {
    script: Script,
    asked: Arc<AtomicUsize>,
}

/// The failure a scripted service reports.
#[derive(Debug, thiserror::Error)]
#[error("synthetic localization service failure")]
struct Failure;

#[async_trait]
impl Localizer for Scripted {
    async fn localize(
        &self,
        _patient: &PatientRef,
        members: &[NodeId],
        _deadline: Instant,
    ) -> Localization {
        self.asked.fetch_add(1, Ordering::SeqCst);
        match self.script {
            Script::NamesA => Localization::Candidates(
                members
                    .iter()
                    .filter(|member| member.as_str() == "node-a")
                    .cloned()
                    .collect(),
            ),
            Script::Nothing => Localization::NoRecords,
            Script::Down => Localization::Unavailable(LocalizerError::DeadlineExceeded),
            Script::Answers(status) => Localization::Unavailable(LocalizerError::Answered {
                status,
                source: Box::new(Failure),
            }),
            Script::AuditFailed => {
                Localization::Unavailable(LocalizerError::AuditFailed(Box::new(Failure)))
            }
        }
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

/// The patient known at both members.
const BOTH: &[(&str, &str)] = &[("node-a", EHR_A), ("node-b", EHR_B)];

/// The patient known at node B only.
const AT_B: &[(&str, &str)] = &[("node-b", EHR_B)];

/// A metered gateway over node A and node B resolving through `resolver`
/// and localized by `script` under `on_failure`, with the localizer's call
/// counter.
fn scripted(
    (a, b): (&Server, &Server),
    resolver: KnownAt,
    (script, on_failure): (Script, OnFailure),
) -> Result<Built, Box<dyn Error>> {
    let snapshot = RegistrySnapshot::from_toml_str(&registry(&a.uri(), &b.uri(), ""))?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(5))?;
    let clients = NodeClients::from_snapshot(&snapshot, &transport, &BTreeMap::new())?;
    let asked = Arc::new(AtomicUsize::new(0));
    let localizer = Scripted {
        script,
        asked: Arc::clone(&asked),
    };
    let federation = Federation::new(
        FederationId::new("example-federation")?,
        snapshot,
        clients,
        Some(Arc::new(resolver)),
        Context::new(Targeting::AskAll),
        Budget::new(Duration::from_secs(2), Duration::from_secs(3))?,
    )
    .with_localization(LocalizationPolicy::new(
        Arc::new(localizer),
        "test-scripted",
        on_failure,
        Duration::from_millis(500),
    ))
    .with_signer(crate::support::signer("example-federation")?);
    let state = Arc::new(AppState::with_federation(federation));
    Ok((
        ferrofed_server::router(Arc::clone(&state), &settings_with_room()),
        state,
        asked,
    ))
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

/// The state `GET /health/dependencies` reports of the localizer.
async fn localizer_state(app: &Router) -> Result<Option<String>, Box<dyn Error>> {
    #[derive(Deserialize)]
    struct Report {
        localizer: Option<String>,
    }
    let request = Request::get("/health/dependencies").body(Body::empty())?;
    let (status, text) = call(app.clone(), request).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    Ok(serde_json::from_str::<Report>(&text)?.localizer)
}

/// The localizer calls the metrics of `state` counted with `outcome`.
fn localizer_calls(state: &AppState, outcome: &str) -> Result<Option<String>, Box<dyn Error>> {
    let samples = parse(&state.metrics().render()?)?;
    Ok(count(&samples, LOCALIZER_CALLS, &[("outcome", outcome)]))
}

// conformance: CP-5
#[tokio::test]
async fn the_read_by_subject_asks_only_the_members_the_localizer_names() -> TestResult {
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let (app, _state, asked) =
        scripted((&a, &b), KnownAt(BOTH), (Script::NamesA, OnFailure::Closed))?;

    let (status, text) = call(app, by_subject()?).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "node B, which the localizer did not name, is no second holder (N4): {text}"
    );
    assert_eq!(1, asked.load(Ordering::SeqCst));
    assert_eq!(1, received(&a).await?.len());
    assert!(wire(&b).await?.is_empty(), "§14.1: node B learns nothing");
    assert!(!wire(&a).await?.contains(PATIENT), "N33");
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn the_read_by_subject_fails_closed_when_the_localizer_does_not_answer() -> TestResult {
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let (app, _state, _asked) =
        scripted((&a, &b), KnownAt(AT_B), (Script::Down, OnFailure::Closed))?;

    let (status, text) = call(app, by_subject()?).await?;
    assert_eq!(StatusCode::FAILED_DEPENDENCY, status, "{text}");
    assert_eq!("localization-unavailable", error_body(&text)?.code);
    assert!(!text.contains(PATIENT), "§5.4.3: {text}");
    assert!(wire(&a).await?.is_empty(), "no member is asked (§14.1)");
    assert!(wire(&b).await?.is_empty(), "no member is asked (§14.1)");
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn the_read_by_subject_asks_every_member_when_ask_all_is_declared() -> TestResult {
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let (app, _state, _asked) =
        scripted((&a, &b), KnownAt(AT_B), (Script::Down, OnFailure::AskAll))?;

    let (status, text) = call(app, by_subject()?).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "the declared widening (§14.1): {text}"
    );
    assert_eq!(1, received(&b).await?.len());
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn a_subject_no_member_holds_by_localization_is_no_destination() -> TestResult {
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let (app, _state, _asked) = scripted(
        (&a, &b),
        KnownAt(BOTH),
        (Script::Nothing, OnFailure::Closed),
    )?;

    let (status, text) = call(app, by_subject()?).await?;
    assert_eq!(StatusCode::NOT_FOUND, status, "{text}");
    assert_eq!("no-destination", error_body(&text)?.code);
    assert!(wire(&a).await?.is_empty());
    assert!(wire(&b).await?.is_empty());
    Ok(())
}

// conformance: CP-5 CP-6
#[tokio::test]
async fn a_targeted_read_by_subject_is_never_localized() -> TestResult {
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let (app, _state, asked) =
        scripted((&a, &b), KnownAt(BOTH), (Script::NamesA, OnFailure::Closed))?;
    let mut request = by_subject()?;
    request
        .headers_mut()
        .insert(ENDPOINT, HeaderValue::from_static("node-b-pub"));

    let (status, text) = call(app, request).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "§8: the header selects node B: {text}"
    );
    assert_eq!(
        0,
        asked.load(Ordering::SeqCst),
        "the localizer is not asked"
    );
    assert_eq!(1, received(&b).await?.len());
    assert!(wire(&a).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn the_localizer_is_on_the_health_report_under_the_members_rule() -> TestResult {
    for (script, expected) in [
        (Script::Down, "down"),
        (Script::Answers(StatusCode::SERVICE_UNAVAILABLE), "failing"),
        (Script::Answers(StatusCode::OK), "up"),
        (Script::NamesA, "up"),
        (Script::Nothing, "up"),
    ] {
        let a = node_answering("uid-at-a").await;
        let b = node_answering("uid-at-b").await;
        let (app, _state, _asked) = scripted((&a, &b), KnownAt(BOTH), (script, OnFailure::Closed))?;
        assert_eq!(
            Some("unknown".to_owned()),
            localizer_state(&app).await?,
            "nothing observed before the first request"
        );
        let (status, text) = call(app.clone(), post(body(&patient_query())?)?).await?;
        assert_eq!(StatusCode::OK, status, "{text}");
        assert_eq!(
            Some(expected.to_owned()),
            localizer_state(&app).await?,
            "{script:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_gateway_with_no_localizer_reports_none() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let snapshot = RegistrySnapshot::from_toml_str(&registry(&a.uri(), &b.uri(), ""))?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(5))?;
    let clients = NodeClients::from_snapshot(&snapshot, &transport, &BTreeMap::new())?;
    let federation = Federation::new(
        FederationId::new("example-federation")?,
        snapshot,
        clients,
        Some(Arc::new(KnownAt(BOTH))),
        Context::new(Targeting::AskAll),
        Budget::new(Duration::from_secs(2), Duration::from_secs(3))?,
    )
    .with_signer(crate::support::signer("example-federation")?);
    let app = ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    );
    assert_eq!(None, localizer_state(&app).await?);
    Ok(())
}

#[tokio::test]
async fn each_localizer_call_is_counted_by_outcome() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    for (script, outcome) in [
        (Script::Down, "unavailable"),
        (Script::NamesA, "candidates"),
        (Script::Nothing, "no-records"),
    ] {
        let (app, state, _asked) = scripted((&a, &b), KnownAt(BOTH), (script, OnFailure::Closed))?;
        let (status, _) = call(app, post(body(&patient_query())?)?).await?;
        assert_eq!(StatusCode::OK, status);
        assert_eq!(
            Some("1".to_owned()),
            localizer_calls(&state, outcome)?,
            "{script:?}"
        );
    }
    Ok(())
}

/// What the tests read of an answer's `meta.federation`.
#[derive(Debug, Deserialize)]
struct Envelope {
    meta: EnvelopeMeta,
}

#[derive(Debug, Deserialize)]
struct EnvelopeMeta {
    federation: Federated,
}

#[derive(Debug, Deserialize)]
struct Federated {
    endpoints: Vec<Reported>,
    localization: Option<Report>,
}

#[derive(Debug, Deserialize)]
struct Reported {
    status: String,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Report {
    error: String,
}

// conformance: CP-5
#[tokio::test]
async fn an_unaudited_exchange_fails_closed_even_under_ask_all() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let (app, state, _asked) = scripted(
        (&a, &b),
        KnownAt(BOTH),
        (Script::AuditFailed, OnFailure::AskAll),
    )?;

    let (status, text) = call(app.clone(), post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let envelope: Envelope = serde_json::from_str(&text)?;
    for endpoint in &envelope.meta.federation.endpoints {
        assert_eq!("not-localized", endpoint.status, "{text}");
        assert!(
            endpoint
                .error
                .as_deref()
                .is_some_and(|error| error.contains("could not be audited")),
            "every member carries the audit error: {text}"
        );
    }
    assert!(
        envelope
            .meta
            .federation
            .localization
            .is_some_and(|report| report.error.contains("could not be audited")),
        "§14.1: the failure is in meta.federation: {text}"
    );
    assert!(wire(&a).await?.is_empty(), "nothing is dispatched");
    assert!(wire(&b).await?.is_empty(), "nothing is dispatched");
    assert_eq!(Some("failing".to_owned()), localizer_state(&app).await?);
    assert_eq!(
        Some("1".to_owned()),
        localizer_calls(&state, "audit-failed")?
    );
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn a_localizer_outage_still_widens_under_a_declared_ask_all() -> TestResult {
    let a = node_answering("uid-at-a").await;
    let b = node_answering("uid-at-b").await;
    let (app, _state, _asked) =
        scripted((&a, &b), KnownAt(BOTH), (Script::Down, OnFailure::AskAll))?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let envelope: Envelope = serde_json::from_str(&text)?;
    let statuses: Vec<&str> = envelope
        .meta
        .federation
        .endpoints
        .iter()
        .map(|endpoint| endpoint.status.as_str())
        .collect();
    assert_eq!(
        vec!["active", "active"],
        statuses,
        "§14.1: the declared widening"
    );
    assert_eq!(1, received(&a).await?.len());
    assert_eq!(1, received(&b).await?.len());
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn an_unaudited_read_by_subject_fails_closed_even_under_ask_all() -> TestResult {
    let a = ehr_node("cdr-a.example.org", EHR_A).await;
    let b = ehr_node("cdr-b.example.org", EHR_B).await;
    let (app, _state, _asked) = scripted(
        (&a, &b),
        KnownAt(AT_B),
        (Script::AuditFailed, OnFailure::AskAll),
    )?;

    let (status, text) = call(app, by_subject()?).await?;
    assert_eq!(StatusCode::FAILED_DEPENDENCY, status, "{text}");
    assert_eq!("localization-unavailable", error_body(&text)?.code);
    assert!(wire(&a).await?.is_empty());
    assert!(wire(&b).await?.is_empty());
    Ok(())
}

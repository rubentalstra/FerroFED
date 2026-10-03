// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The node set of an undirected patient query from a localizer: a member it
//! does not name is `not-localized` and never asked, a localizer that does
//! not answer fails closed with its error on every member and in
//! `meta.federation`, and ask-all on failure holds only where it is declared
//! (§8, §11.1, §14.1; N4, N10, N30; CP-5).
//!
//! In process: three mock CDRs, the development cross-reference resolving the
//! patient at every one of them, and a localizer stub the test sets, so that
//! every member the localizer leaves out would otherwise have been asked.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use ferrofed_identity::localizer::{Localization, Localizer, LocalizerError, OnFailure};
use ferrofed_identity::patient::PatientRef;
use ferrofed_registry::id::NodeId;
use ferrofed_server::config::Config;
use ferrofed_server::facade::options::{LOCALIZATION_MODE, LOCALIZATION_MS};
use ferrofed_server::federation::{Federation, FederationError};
use ferrofed_server::localization::{DEVELOPMENT_STATIC, LocalizationError, LocalizationPolicy};
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Request, StatusCode};
use openehr_federation::headers;
use openehr_federation::options::OptionsRoot;
use openehr_federation::outcome::ErrorDetail;
use serde::Deserialize;

use crate::facade::{
    Answer, NAMESPACE, PATIENT, body, node_answering, patient_query, post, received, schema,
    settings_with_room, statuses, wire,
};
use crate::support::call;

type TestResult = Result<(), Box<dyn Error>>;

/// The members, each with one endpoint `<member>-pub`.
const MEMBERS: [&str; 3] = ["node-a", "node-b", "node-c"];

/// The patient's `ehr_id` at each member, in [`MEMBERS`] order.
const EHR_IDS: [&str; 3] = [
    "5a5a5a5a-5a5a-4a5a-8a5a-5a5a5a5a5a5a",
    "5b5b5b5b-5b5b-4b5b-8b5b-5b5b5b5b5b5b",
    "5c5c5c5c-5c5c-4c5c-8c5c-5c5c5c5c5c5c",
];

/// The localizer's budget every gateway here declares, in milliseconds.
const LOCALIZATION_TIMEOUT_MS: u64 = 500;

/// What the localizer stub answers.
#[derive(Debug, Clone)]
enum Stub {
    /// The members it names as candidates.
    Names(&'static [&'static str]),
    /// No member holds the patient's data.
    Nothing,
    /// The localization service failed.
    Down,
    /// It never answers.
    Silent,
}

/// A localizer that answers [`Stub`] and counts how often it was asked.
#[derive(Debug)]
struct StubLocalizer {
    answer: Stub,
    asked: Arc<AtomicUsize>,
}

/// The localization service failure the stub reports.
#[derive(Debug, thiserror::Error)]
#[error("synthetic localization outage")]
struct Outage;

#[async_trait]
impl Localizer for StubLocalizer {
    async fn localize(
        &self,
        _patient: &PatientRef,
        _members: &[NodeId],
        _deadline: Instant,
    ) -> Localization {
        self.asked.fetch_add(1, Ordering::SeqCst);
        match &self.answer {
            Stub::Names(names) => Localization::Candidates(
                names
                    .iter()
                    .filter_map(|name| NodeId::new(*name).ok())
                    .collect::<BTreeSet<_>>(),
            ),
            Stub::Nothing => Localization::NoRecords,
            Stub::Down => Localization::Unavailable(LocalizerError::Backend(Box::new(Outage))),
            Stub::Silent => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Localization::NoRecords
            }
        }
    }
}

/// The registry document of the three members at `urls`.
fn registry(urls: [&str; 3]) -> String {
    let mut text = String::new();
    for (member, url) in MEMBERS.into_iter().zip(urls) {
        // NOTE: writing to a String cannot fail, so the result is dropped.
        let _written: std::fmt::Result = write!(
            text,
            "\n[[organisation]]\nid = \"org-{member}\"\n\n[[node]]\nid = \"{member}\"\norganisation = \"org-{member}\"\nsystem_id = \"{member}.example.org\"\n\n[[endpoint]]\nid = \"{member}-pub\"\nnode = \"{member}\"\nurl = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-{member}\"\n"
        );
    }
    text
}

/// The `[dev]` rows resolving the patient at each member of `at`.
fn crossref(at: &[&str]) -> String {
    MEMBERS
        .into_iter()
        .zip(EHR_IDS)
        .filter(|(member, _)| at.contains(member))
        .fold(String::new(), |mut text, (member, ehr_id)| {
            // NOTE: writing to a String cannot fail, so the result is dropped.
            let _written: std::fmt::Result = write!(
                text,
                "\n[[dev.crossref]]\nnamespace = \"{NAMESPACE}\"\nvalue = \"{PATIENT}\"\nmember = \"{member}\"\nehr_id = \"{ehr_id}\"\n"
            );
            text
        })
}

/// The configuration of a development gateway over the members at `urls`
/// under the localized node selection, with `localization` as the keys of
/// `[federation.localization]` and `rows` as the cross-reference.
fn config(dir: &Path, urls: [&str; 3], localization: &str, rows: &str) -> TestConfig {
    let document = dir.join("registry.toml");
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"localized\"\nid = \"example-federation\"\n\n[federation.localization]\ntimeout_ms = {LOCALIZATION_TIMEOUT_MS}\n{localization}\n\n{rows}",
        document = toml::Value::String(document.display().to_string()),
    );
    TestConfig {
        registry: (document, registry(urls)),
        text,
    }
}

/// A configuration and the registry document it names.
struct TestConfig {
    registry: (std::path::PathBuf, String),
    text: String,
}

impl TestConfig {
    /// The federation the configuration loads.
    fn load(&self) -> Result<Federation, Box<dyn Error>> {
        std::fs::write(&self.registry.0, &self.registry.1)?;
        let settings =
            Config::from_sources(Some(&crate::support::signed(&self.text)), &BTreeMap::new())?
                .resolve()?;
        Ok(Federation::load(&settings)?.ok_or("a registry is configured")?)
    }

    /// The gateway over the loaded federation, localized by `stub` with
    /// `on_failure` when one is given, and by the development
    /// cross-reference otherwise.
    fn gateway(&self, stub: Option<(StubLocalizer, OnFailure)>) -> Result<Router, Box<dyn Error>> {
        let mut federation = self.load()?;
        if let Some((stub, on_failure)) = stub {
            federation = federation.with_localization(LocalizationPolicy::new(
                Arc::new(stub),
                "test-stub",
                on_failure,
                Duration::from_millis(LOCALIZATION_TIMEOUT_MS),
            ));
        }
        Ok(ferrofed_server::router(
            Arc::new(AppState::with_federation(federation)),
            &settings_with_room(),
        ))
    }
}

/// The three mock members, each answering one row.
async fn members() -> [Server; 3] {
    [
        node_answering("a-uid::node-a.example.org::1").await,
        node_answering("b-uid::node-b.example.org::1").await,
        node_answering("c-uid::node-c.example.org::1").await,
    ]
}

/// The URLs of `servers`.
fn urls(servers: &[Server; 3]) -> [String; 3] {
    [servers[0].uri(), servers[1].uri(), servers[2].uri()]
}

/// A localizer stub answering `answer`, with the counter of its calls.
fn stub(answer: Stub) -> (StubLocalizer, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    (
        StubLocalizer {
            answer,
            asked: Arc::clone(&asked),
        },
        asked,
    )
}

/// What the tests read of `meta.federation` beyond [`Answer`].
#[derive(Debug, Deserialize)]
struct Envelope {
    meta: EnvelopeMeta,
}

#[derive(Debug, Deserialize)]
struct EnvelopeMeta {
    federation: Diagnostics,
}

#[derive(Debug, Deserialize)]
struct Diagnostics {
    localization: Option<LocalizationReport>,
}

#[derive(Debug, Deserialize)]
struct LocalizationReport {
    error: ErrorDetail,
}

/// The text of `error`, or the empty string for a structured one.
fn text(error: Option<&ErrorDetail>) -> &str {
    match error {
        Some(ErrorDetail::Text(message)) => message,
        _ => "",
    }
}

/// The undirected patient query, asked of `app`.
async fn ask(app: Router) -> Result<(StatusCode, String), Box<dyn Error>> {
    call(app, post(body(&patient_query())?)?).await
}

/// The number of requests each member received, in [`MEMBERS`] order.
async fn asked_counts(servers: &[Server; 3]) -> Result<[usize; 3], Box<dyn Error>> {
    Ok([
        received(&servers[0]).await?.len(),
        received(&servers[1]).await?.len(),
        received(&servers[2]).await?.len(),
    ])
}

// conformance: CP-5
#[tokio::test]
async fn a_member_the_localizer_does_not_name_is_not_localized_and_never_asked() -> TestResult {
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let (localizer, asked) = stub(Stub::Names(&["node-a", "node-b"]));
    let app = config(dir.path(), [&a, &b, &c], "", &crossref(&MEMBERS))
        .gateway(Some((localizer, OnFailure::Closed)))?;

    let (status, text_body) = ask(app).await?;
    assert_eq!(StatusCode::OK, status, "{text_body}");
    schema::validate(&text_body)?;
    let answer: Answer = serde_json::from_str(&text_body)?;
    assert_eq!(
        vec![
            ("node-a-pub", "active"),
            ("node-b-pub", "active"),
            ("node-c-pub", "not-localized"),
        ],
        statuses(&answer),
        "the member the localizer did not name is reported, not dropped (§11.1, N16)"
    );
    assert!(
        answer.meta.federation.complete,
        "a not-localized member was never in scope, so complete stays true (§11.4, N37)"
    );
    let not_localized = answer.meta.federation.endpoints.get(2).ok_or("node C")?;
    assert!(
        not_localized.error.is_none(),
        "nothing failed: a member localization did not name carries no error"
    );
    assert_eq!(2, answer.rows.len(), "one row from each localized member");
    assert_eq!(
        1,
        asked.load(Ordering::SeqCst),
        "the localizer is asked once"
    );
    assert_eq!(
        [1, 1, 0],
        asked_counts(&servers).await?,
        "node C resolves the patient, and is still never asked (§14.1, N4)"
    );
    let envelope: Envelope = serde_json::from_str(&text_body)?;
    assert!(
        envelope.meta.federation.localization.is_none(),
        "a localizer that answered reports no failure"
    );
    for server in &servers {
        assert!(
            !wire(server).await?.contains(PATIENT),
            "the patient identifier reached a member (N33)"
        );
    }
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn the_development_cross_reference_localizes_to_the_members_that_know_the_patient()
-> TestResult {
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let rows = crossref(&["node-a", "node-b"]);
    let app = config(dir.path(), [&a, &b, &c], "", &rows).gateway(None)?;

    let (status, text_body) = ask(app).await?;
    assert_eq!(StatusCode::OK, status, "{text_body}");
    let answer: Answer = serde_json::from_str(&text_body)?;
    assert_eq!(
        vec![
            ("node-a-pub", "active"),
            ("node-b-pub", "active"),
            ("node-c-pub", "not-localized"),
        ],
        statuses(&answer),
        "the cross-reference names the members that hold a row for the patient"
    );
    assert!(answer.meta.federation.complete);
    assert_eq!([1, 1, 0], asked_counts(&servers).await?);
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn a_localizer_that_is_down_fails_closed_dispatches_nothing_and_names_its_error() -> TestResult
{
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let (localizer, asked) = stub(Stub::Down);
    let app = config(dir.path(), [&a, &b, &c], "", &crossref(&MEMBERS))
        .gateway(Some((localizer, OnFailure::Closed)))?;

    let (status, text_body) = ask(app).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "no in-scope member failed, so the status cannot carry the outage (§14.1): {text_body}"
    );
    schema::validate(&text_body)?;
    let answer: Answer = serde_json::from_str(&text_body)?;
    assert_eq!(
        vec![
            ("node-a-pub", "not-localized"),
            ("node-b-pub", "not-localized"),
            ("node-c-pub", "not-localized"),
        ],
        statuses(&answer),
        "fail-closed: the candidate set is empty (§14.1, N4)"
    );
    for endpoint in &answer.meta.federation.endpoints {
        let message = text(endpoint.error.as_ref());
        assert!(
            message.contains("the localizer could not answer")
                && message.contains("synthetic localization outage"),
            "every member carries the localization error (§14.1): {message:?}"
        );
    }
    assert!(
        answer.meta.federation.complete,
        "§14.1: complete stays true"
    );
    assert!(answer.rows.is_empty());
    assert_eq!(1, asked.load(Ordering::SeqCst));
    assert_eq!(
        [0, 0, 0],
        asked_counts(&servers).await?,
        "nothing is dispatched, and there is no ask-all (§14.1, N4)"
    );
    let envelope: Envelope = serde_json::from_str(&text_body)?;
    let report = envelope
        .meta
        .federation
        .localization
        .ok_or("meta.federation carries the localization failure (§14.1 SHOULD)")?;
    assert!(text(Some(&report.error)).contains("synthetic localization outage"));
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn a_localizer_silent_past_its_budget_fails_closed() -> TestResult {
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let (localizer, _asked) = stub(Stub::Silent);
    let app = config(dir.path(), [&a, &b, &c], "", &crossref(&MEMBERS))
        .gateway(Some((localizer, OnFailure::Closed)))?;

    let started = Instant::now();
    let (status, text_body) = ask(app).await?;
    assert!(
        started.elapsed() < Duration::from_millis(LOCALIZATION_TIMEOUT_MS) + crate::support::SLACK,
        "the localizer's budget bounds the wait"
    );
    assert_eq!(StatusCode::OK, status, "{text_body}");
    let answer: Answer = serde_json::from_str(&text_body)?;
    for endpoint in &answer.meta.federation.endpoints {
        assert_eq!("not-localized", endpoint.status);
        assert!(text(endpoint.error.as_ref()).contains("within its budget"));
    }
    assert_eq!([0, 0, 0], asked_counts(&servers).await?);
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn ask_all_on_failure_asks_every_member_only_where_it_is_declared() -> TestResult {
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let (localizer, _asked) = stub(Stub::Down);
    let declared = config(
        dir.path(),
        [&a, &b, &c],
        "on_failure = \"ask-all\"",
        &crossref(&MEMBERS),
    );
    let settings = Config::from_sources(
        Some(&crate::support::signed(&declared.text)),
        &BTreeMap::new(),
    )?
    .resolve()?;
    let on_failure = settings
        .federation
        .localization
        .ok_or("the localization table is resolved")?
        .on_failure;
    assert_eq!(OnFailure::AskAll, on_failure, "the declaration is read");
    let app = declared.gateway(Some((localizer, on_failure)))?;

    let (status, text_body) = ask(app.clone()).await?;
    assert_eq!(StatusCode::OK, status, "{text_body}");
    let answer: Answer = serde_json::from_str(&text_body)?;
    assert_eq!(
        vec![
            ("node-a-pub", "active"),
            ("node-b-pub", "active"),
            ("node-c-pub", "active"),
        ],
        statuses(&answer),
        "the declared ask-all widens to every member (§14.1)"
    );
    assert_eq!([1, 1, 1], asked_counts(&servers).await?);
    let envelope: Envelope = serde_json::from_str(&text_body)?;
    assert!(
        envelope.meta.federation.localization.is_some(),
        "the widening is visible in meta.federation (§14.1)"
    );
    let options = options(app).await?;
    assert_eq!(
        "ask-all", options.federation.localization.on_failure,
        "the deployment declares ask-all in OPTIONS (§14.1, N30)"
    );
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn without_the_declaration_a_failed_localizer_is_never_widened_to_ask_all() -> TestResult {
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let undeclared = config(dir.path(), [&a, &b, &c], "", &crossref(&MEMBERS));
    let settings = Config::from_sources(
        Some(&crate::support::signed(&undeclared.text)),
        &BTreeMap::new(),
    )?
    .resolve()?;
    let on_failure = settings
        .federation
        .localization
        .ok_or("the localization table is resolved")?
        .on_failure;
    assert_eq!(
        OnFailure::Closed,
        on_failure,
        "closed is the default (§14.1)"
    );
    let (localizer, _asked) = stub(Stub::Down);
    let app = undeclared.gateway(Some((localizer, on_failure)))?;

    let (status, _text_body) = ask(app.clone()).await?;
    assert_eq!(StatusCode::OK, status);
    assert_eq!([0, 0, 0], asked_counts(&servers).await?);
    let options = options(app).await?;
    assert_eq!("closed", options.federation.localization.on_failure);
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn a_localizer_that_finds_no_records_leaves_every_member_not_localized_without_error()
-> TestResult {
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let (localizer, _asked) = stub(Stub::Nothing);
    let app = config(dir.path(), [&a, &b, &c], "", &crossref(&MEMBERS))
        .gateway(Some((localizer, OnFailure::Closed)))?;

    let (status, text_body) = ask(app).await?;
    assert_eq!(StatusCode::OK, status, "{text_body}");
    let answer: Answer = serde_json::from_str(&text_body)?;
    for endpoint in &answer.meta.federation.endpoints {
        assert_eq!("not-localized", endpoint.status);
        assert!(
            endpoint.error.is_none(),
            "no records is an answer, not an outage (§11.1)"
        );
    }
    let envelope: Envelope = serde_json::from_str(&text_body)?;
    assert!(envelope.meta.federation.localization.is_none());
    assert_eq!([0, 0, 0], asked_counts(&servers).await?);
    Ok(())
}

// conformance: CP-5 CP-6
#[tokio::test]
async fn a_directed_query_is_never_localized() -> TestResult {
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let (localizer, asked) = stub(Stub::Names(&["node-a"]));
    let app = config(dir.path(), [&a, &b, &c], "", &crossref(&MEMBERS))
        .gateway(Some((localizer, OnFailure::Closed)))?;
    let mut request = post(body(&patient_query())?)?;
    request.headers_mut().insert(
        headers::ENDPOINT,
        http::HeaderValue::from_static("node-c-pub"),
    );

    let (status, text_body) = call(app, request).await?;
    assert_eq!(StatusCode::OK, status, "{text_body}");
    let answer: Answer = serde_json::from_str(&text_body)?;
    assert_eq!(
        vec![
            ("node-a-pub", "excluded"),
            ("node-b-pub", "excluded"),
            ("node-c-pub", "active"),
        ],
        statuses(&answer),
        "the directive selects the node set, and its other members are excluded (§8, §11.1)"
    );
    assert_eq!(
        0,
        asked.load(Ordering::SeqCst),
        "the localizer is not asked (§8)"
    );
    assert_eq!([0, 0, 1], asked_counts(&servers).await?);
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn an_undirected_query_that_names_no_patient_is_refused_under_localization() -> TestResult {
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let app = config(dir.path(), [&a, &b, &c], "", &crossref(&MEMBERS)).gateway(None)?;
    let query = "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c";

    let (status, text_body) = call(app, post(body(query)?)?).await?;
    assert_eq!(
        StatusCode::BAD_REQUEST,
        status,
        "localization is keyed on the patient, so no node set is defined (N4): {text_body}"
    );
    assert_eq!([0, 0, 0], asked_counts(&servers).await?);
    Ok(())
}

#[tokio::test]
async fn options_declares_the_localizer_its_failure_policy_and_its_budget() -> TestResult {
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let [a, b, c] = urls(&servers);
    let app = config(dir.path(), [&a, &b, &c], "", &crossref(&MEMBERS)).gateway(None)?;

    let options = options(app).await?;
    let localization = &options.federation.localization;
    assert_eq!("closed", localization.on_failure);
    assert_eq!(
        Some(format!("\"{DEVELOPMENT_STATIC}\"").as_str()),
        localization
            .extra
            .get(LOCALIZATION_MODE)
            .map(serde_json::value::RawValue::get)
    );
    assert_eq!(
        Some(LOCALIZATION_TIMEOUT_MS.to_string().as_str()),
        options
            .federation
            .timeout
            .extra
            .get(LOCALIZATION_MS)
            .map(serde_json::value::RawValue::get)
    );
    Ok(())
}

/// The `OPTIONS {base}/` body of `app`, validated against the vendored
/// schema.
async fn options(app: Router) -> Result<OptionsRoot, Box<dyn Error>> {
    let request = Request::options("/").body(Body::empty())?;
    let (status, text_body) = call(app, request).await?;
    assert_eq!(StatusCode::OK, status, "{text_body}");
    schema::validate_options(&text_body)?;
    Ok(serde_json::from_str(&text_body)?)
}

/// The text of a configuration over three unreachable members, with
/// `federation` added to its `[federation]` table.
fn localized(dir: &Path, federation: &str) -> Result<String, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(
        &document,
        registry([
            "http://127.0.0.1:9",
            "http://127.0.0.1:10",
            "http://127.0.0.1:11",
        ]),
    )?;
    Ok(format!(
        "[registry]\ndocument = {document}\n\n[federation]\nid = \"example-federation\"\n{federation}\n",
        document = toml::Value::String(document.display().to_string()),
    ))
}

/// The federation `text` loads, or why it does not.
fn load(text: &str) -> Result<Result<Option<Federation>, FederationError>, Box<dyn Error>> {
    let settings =
        Config::from_sources(Some(&crate::support::signed(text)), &BTreeMap::new())?.resolve()?;
    Ok(Federation::load(&settings))
}

#[test]
fn the_localized_selection_without_a_localizer_refuses_to_boot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = localized(dir.path(), "node_selection = \"localized\"")?;
    match load(&text)? {
        Err(FederationError::Localization(LocalizationError::NoLocalizer)) => Ok(()),
        other => Err(format!("refused for its missing localizer (N4): {other:?}").into()),
    }
}

#[test]
fn a_localization_table_under_ask_all_refuses_to_boot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = localized(
        dir.path(),
        "node_selection = \"ask-all\"\n\n[federation.localization]\non_failure = \"ask-all\"\ntimeout_ms = 1000",
    )?;
    match load(&text)? {
        Err(FederationError::Localization(LocalizationError::NotLocalized)) => Ok(()),
        other => Err(format!("a policy for no localizer is refused: {other:?}").into()),
    }
}

#[test]
fn a_localization_budget_past_the_overall_budget_refuses_to_boot() -> TestResult {
    let text = "[federation]\noverall_timeout_ms = 3000\nnode_selection = \"localized\"\n\n[federation.localization]\ntimeout_ms = 3000\n";
    match Config::from_sources(Some(&crate::support::signed(text)), &BTreeMap::new())?.resolve() {
        Err(ferrofed_server::config::error::Error::LocalizationBudget {
            timeout_ms: 3000,
            overall_ms: 3000,
        }) => Ok(()),
        other => {
            Err(format!("the localizer's budget is a part of the overall one: {other:?}").into())
        }
    }
}

#[test]
fn an_on_failure_policy_the_specification_does_not_name_refuses_to_boot() -> TestResult {
    let text = "[federation]\nnode_selection = \"localized\"\n\n[federation.localization]\non_failure = \"open\"\n";
    match Config::from_sources(Some(&crate::support::signed(text)), &BTreeMap::new()) {
        Err(ferrofed_server::config::error::Error::Parse { .. }) => Ok(()),
        other => Err(format!("§14.1 names closed and ask-all only: {other:?}").into()),
    }
}

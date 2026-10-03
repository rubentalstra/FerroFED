// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The PIXm localizer configured by `[pixm]` under the localized selection:
//! the members whose `ehr_id` domain holds the patient at the PIX Manager are
//! the candidates, read from the one ITI-83 call the resolution reuses, and a
//! Manager that does not answer fails the query closed (N4, N10, §14.1,
//! §14.2; CP-5).
//!
//! In process: three mock CDRs and the harness PIX Manager fed over ITI-104,
//! behind a capturing and fault proxy.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::Config;
use ferrofed_server::federation::Federation;
use ferrofed_server::localization::PIXM;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::pix::PixManager;
use ferrofed_testkit::proxy::{CapturingProxy, Fault};
use ferrofed_testkit::seed::{self, CrossReferenceSeed, EhrDomain, PatientId};
use http::{Request, StatusCode};
use openehr_federation::options::OptionsRoot;
use uuid::Uuid;

use crate::facade::{
    Answer, body, node_answering, post, received, schema, settings_with_room, statuses, wire,
};
use crate::support::call;

type TestResult = Result<(), Box<dyn Error>>;

const MEMBERS: [&str; 3] = ["node-a", "node-b", "node-c"];

/// The `ehr_id` domains of the three members at the PIX Manager.
const DOMAINS: [EhrDomain; 3] = [EhrDomain::new(11), EhrDomain::new(12), EhrDomain::new(13)];

/// The patient's `ehr_id` at node A and node B; node C does not know them.
const EHR_A: Uuid = Uuid::from_u128(0x7a7a_7a7a_7a7a_4a7a_8a7a_7a7a_7a7a_7a7a);
const EHR_B: Uuid = Uuid::from_u128(0x7b7b_7b7b_7b7b_4b7b_8b7b_7b7b_7b7b_7b7b);

/// The patient the PIX Manager is fed with, in the example arc.
fn patient() -> PatientId {
    PatientId::new(1, 408)
}

/// The undirected patient query, through `external_ref`.
fn query() -> String {
    let patient = patient();
    format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = '{}' \
         AND e/ehr_status/subject/external_ref/namespace = '{}'",
        patient.value(),
        patient.namespace()
    )
}

/// The gateway over the members at `urls`, localized and resolved by the
/// `[pixm]` Manager at `manager`.
fn gateway(dir: &Path, urls: [&str; 3], manager: &str) -> Result<Router, Box<dyn Error>> {
    let mut registry = String::new();
    let mut members = String::new();
    for ((member, url), domain) in MEMBERS.into_iter().zip(urls).zip(DOMAINS) {
        write!(
            registry,
            "\n[[organisation]]\nid = \"org-{member}\"\n\n[[node]]\nid = \"{member}\"\norganisation = \"org-{member}\"\nsystem_id = \"{member}.example.org\"\n\n[[endpoint]]\nid = \"{member}-pub\"\nnode = \"{member}\"\nurl = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-{member}\"\n"
        )?;
        writeln!(members, "\"{member}\" = \"{}\"", domain.system())?;
    }
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry)?;
    let text = format!(
        "[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"localized\"\nid = \"example-federation\"\n\n[federation.localization]\ntimeout_ms = 1000\n\n[[pixm.manager]]\nurl = \"{manager}\"\n\n[pixm.manager.members]\n{members}",
        document = toml::Value::String(document.display().to_string()),
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// The harness PIX Manager, fed with the patient at node A and node B and an
/// unrelated patient at node C, so node C's domain is known to it.
async fn fed_manager() -> Result<PixManager, Box<dyn Error>> {
    let pix = PixManager::start().await?;
    let feeds = [
        CrossReferenceSeed {
            patient: patient(),
            ehrs: vec![(DOMAINS[0], EHR_A), (DOMAINS[1], EHR_B)],
        },
        CrossReferenceSeed {
            patient: PatientId::new(1, 409),
            ehrs: vec![(
                DOMAINS[2],
                Uuid::from_u128(0x7c7c_7c7c_7c7c_4c7c_8c7c_7c7c_7c7c_7c7c),
            )],
        },
    ];
    for feed in &feeds {
        assert_eq!(
            StatusCode::CREATED,
            seed::feed(&pix.base_url(), feed).await?,
            "ITI-104 creates the Patient"
        );
    }
    Ok(pix)
}

async fn members() -> [Server; 3] {
    [
        node_answering("a-uid::node-a.example.org::1").await,
        node_answering("b-uid::node-b.example.org::1").await,
        node_answering("c-uid::node-c.example.org::1").await,
    ]
}

async fn asked_counts(servers: &[Server; 3]) -> Result<[usize; 3], Box<dyn Error>> {
    Ok([
        received(&servers[0]).await?.len(),
        received(&servers[1]).await?.len(),
        received(&servers[2]).await?.len(),
    ])
}

// conformance: CP-5
#[tokio::test]
async fn the_members_whose_domain_holds_the_patient_are_asked_over_one_iti_83_call() -> TestResult {
    let pix = fed_manager().await?;
    let proxy = CapturingProxy::start(pix.origin()).await?;
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let app = gateway(
        dir.path(),
        [&servers[0].uri(), &servers[1].uri(), &servers[2].uri()],
        &format!("{}/fhir/", proxy.origin()),
    )?;

    let (status, text) = call(app.clone(), post(body(&query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    schema::validate(&text)?;
    let answer: Answer = serde_json::from_str(&text)?;
    assert_eq!(
        vec![
            ("node-a-pub", "active"),
            ("node-b-pub", "active"),
            ("node-c-pub", "not-localized"),
        ],
        statuses(&answer),
        "node C's domain holds no identifier for the patient (§14.2, N4)"
    );
    assert!(
        answer.meta.federation.complete,
        "§11.4: node C was never in scope"
    );
    assert_eq!([1, 1, 0], asked_counts(&servers).await?);
    assert_eq!(
        1,
        pix.queries(),
        "the localization and the resolution share one ITI-83 call"
    );

    let identifier = patient().value();
    for server in &servers {
        assert!(
            !wire(server).await?.contains(&identifier),
            "the identifier reached a member (N33)"
        );
    }

    let request = Request::options("/").body(Body::empty())?;
    let (status, text) = call(app, request).await?;
    assert_eq!(StatusCode::OK, status);
    schema::validate_options(&text)?;
    let options: OptionsRoot = serde_json::from_str(&text)?;
    assert_eq!("closed", options.federation.localization.on_failure);
    assert_eq!(
        Some(format!("\"{PIXM}\"").as_str()),
        options
            .federation
            .localization
            .extra
            .get("mode")
            .map(serde_json::value::RawValue::get)
    );
    Ok(())
}

// conformance: CP-5
#[tokio::test]
async fn a_pix_manager_that_does_not_answer_fails_the_query_closed() -> TestResult {
    let pix = fed_manager().await?;
    let proxy = CapturingProxy::start(pix.origin()).await?;
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let app = gateway(
        dir.path(),
        [&servers[0].uri(), &servers[1].uri(), &servers[2].uri()],
        &format!("{}/fhir/", proxy.origin()),
    )?;

    for fault in [
        Fault::Status(StatusCode::SERVICE_UNAVAILABLE),
        Fault::Refuse,
    ] {
        proxy.set_fault(fault);
        let (status, text) = call(app.clone(), post(body(&query())?)?).await?;
        assert_eq!(StatusCode::OK, status, "{text}");
        let answer: Answer = serde_json::from_str(&text)?;
        for endpoint in &answer.meta.federation.endpoints {
            assert_eq!("not-localized", endpoint.status, "{fault:?}");
            assert!(
                endpoint.error.is_some(),
                "every member carries the error (§14.1)"
            );
        }
        let errors = serde_json::to_string(
            &answer
                .meta
                .federation
                .endpoints
                .iter()
                .map(|endpoint| &endpoint.error)
                .collect::<Vec<_>>(),
        )?;
        assert!(
            !errors.contains(&patient().value()),
            "no error quotes the identifier: {errors}"
        );
    }
    assert_eq!(
        [0, 0, 0],
        asked_counts(&servers).await?,
        "no dispatch, no ask-all (§14.1)"
    );
    assert_eq!(0, pix.queries(), "the Manager was never reached");
    Ok(())
}

/// What the test reads of `meta.federation.localization`.
#[derive(Debug, serde::Deserialize)]
struct Envelope {
    meta: EnvelopeMeta,
}

#[derive(Debug, serde::Deserialize)]
struct EnvelopeMeta {
    federation: Diagnostics,
}

#[derive(Debug, serde::Deserialize)]
struct Diagnostics {
    localization: Option<LocalizationDiagnostic>,
}

#[derive(Debug, serde::Deserialize)]
struct LocalizationDiagnostic {
    error: String,
}

#[tokio::test]
async fn the_localizer_error_names_the_manager_status_once_and_the_log_keeps_the_chain()
-> TestResult {
    let pix = fed_manager().await?;
    let proxy = CapturingProxy::start(pix.origin()).await?;
    let servers = members().await;
    let dir = tempfile::tempdir()?;
    let app = gateway(
        dir.path(),
        [&servers[0].uri(), &servers[1].uri(), &servers[2].uri()],
        &format!("{}/fhir/", proxy.origin()),
    )?;
    proxy.set_fault(Fault::Status(StatusCode::SERVICE_UNAVAILABLE));
    let logs = crate::support::Logs::default();
    let capture = ferrofed_server::telemetry::subscriber(
        ferrofed_server::telemetry::Rendering::Json,
        "info",
        false,
        logs.clone(),
    )?;
    let guard = tracing::subscriber::set_default(capture);
    let (status, text) = call(app, post(body(&query())?)?).await?;
    drop(guard);
    assert_eq!(StatusCode::OK, status, "{text}");

    let status = StatusCode::SERVICE_UNAVAILABLE.to_string();
    let answer: Answer = serde_json::from_str(&text)?;
    for endpoint in &answer.meta.federation.endpoints {
        let error = serde_json::to_string(&endpoint.error)?;
        assert_eq!(1, error.matches(&status).count(), "{error}");
        assert!(
            error.contains("the PIX Manager could not cross-reference the patient"),
            "the binding's own reason: {error}"
        );
    }
    let envelope: Envelope = serde_json::from_str(&text)?;
    let localization = envelope
        .meta
        .federation
        .localization
        .ok_or("§14.1 SHOULD: the localizer's failure")?;
    assert_eq!(
        1,
        localization.error.matches(&status).count(),
        "{}",
        localization.error
    );

    let log = logs.text();
    assert!(
        log.contains("the PIX Manager answered 503 Service Unavailable"),
        "the log keeps the whole cause chain: {log}"
    );
    assert!(!log.contains(&patient().value()), "{log}");
    Ok(())
}

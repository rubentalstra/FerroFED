// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The XCPD localizer configured by `[xcpd]`: an undirected patient query
//! asks the configured responding gateways which communities hold the
//! patient, and only the members serving them are asked; a gateway that
//! fails leaves every member `not-localized` with the error and asks none
//! (N4, N10, §14.1, Annex A.3; ITI TF-2 §3.55; CP-5).
//!
//! In process: three mock CDRs, the development cross-reference resolving
//! the patient at every one of them, and the testkit's stub responding
//! gateways, reached over the development path since they speak `http`.
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
use ferrofed_server::config::error;
use ferrofed_server::federation::{Federation, FederationError};
use ferrofed_server::localization::{LocalizationError, XCPD};
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::xcpd::{Answer, Community, RespondingGateway};
use http::{Request, StatusCode};
use openehr_federation::options::OptionsRoot;
use serde::Deserialize;

use crate::facade::{
    Answer as Federated, NAMESPACE, PATIENT, body, node_answering, patient_query, post, received,
    schema, settings_with_room, statuses, wire,
};
use crate::support::call;

type TestResult = Result<(), Box<dyn Error>>;

const MEMBERS: [&str; 3] = ["node-a", "node-b", "node-c"];

const EHR_IDS: [&str; 3] = [
    "6a6a6a6a-6a6a-4a6a-8a6a-6a6a6a6a6a6a",
    "6b6b6b6b-6b6b-4b6b-8b6b-6b6b6b6b6b6b",
    "6c6c6c6c-6c6c-4c6c-8c6c-6c6c6c6c6c6c",
];

/// The community each member serves, in [`MEMBERS`] order.
const COMMUNITIES: [&str; 3] = ["2.999.50", "2.999.60", "2.999.70"];

/// The registry document and the configuration text of a gateway over the
/// members at `urls`, localized by `[xcpd]` over `gateways`, under
/// `profile`.
fn config(
    dir: &Path,
    urls: [&str; 3],
    gateways: &[String],
    profile: &str,
    extra: &str,
) -> Result<String, Box<dyn Error>> {
    let mut registry = String::new();
    let mut rows = String::new();
    let mut communities = String::new();
    for ((member, url), (ehr_id, community)) in MEMBERS
        .into_iter()
        .zip(urls)
        .zip(EHR_IDS.into_iter().zip(COMMUNITIES))
    {
        write!(
            registry,
            "\n[[organisation]]\nid = \"org-{member}\"\n\n[[node]]\nid = \"{member}\"\norganisation = \"org-{member}\"\nsystem_id = \"{member}.example.org\"\n\n[[endpoint]]\nid = \"{member}-pub\"\nnode = \"{member}\"\nurl = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-{member}\"\n"
        )?;
        write!(
            rows,
            "\n[[dev.crossref]]\nnamespace = \"{NAMESPACE}\"\nvalue = \"{PATIENT}\"\nmember = \"{member}\"\nehr_id = \"{ehr_id}\"\n"
        )?;
        writeln!(communities, "\"{community}\" = \"{member}\"")?;
    }
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry)?;
    let mut gateway_tables = String::new();
    for url in gateways {
        write!(
            gateway_tables,
            "\n[[xcpd.gateway]]\nurl = \"{url}\"\ndevice = \"2.999.50.1\"\n"
        )?;
    }
    Ok(format!(
        "profile = \"{profile}\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"localized\"\nid = \"example-federation\"\n\n[federation.localization]\ntimeout_ms = 1000\n\n[xcpd]\nsender_device = \"2.999.40.1\"\n{audit}{extra}\n{gateway_tables}\n[xcpd.communities]\n{communities}\n{rows}",
        document = toml::Value::String(document.display().to_string()),
        audit = if extra.contains("audit =") {
            ""
        } else {
            "audit = \"log\"\n"
        },
    ))
}

/// The router over the federation `text` loads.
fn gateway(text: &str) -> Result<Router, Box<dyn Error>> {
    let settings =
        Config::from_sources(Some(&crate::support::signed(text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// The federation `text` loads, or why it does not.
fn load(text: &str) -> Result<Result<Option<Federation>, FederationError>, Box<dyn Error>> {
    let settings =
        Config::from_sources(Some(&crate::support::signed(text)), &BTreeMap::new())?.resolve()?;
    Ok(Federation::load(&settings))
}

async fn members() -> [Server; 3] {
    [
        node_answering("a-uid::node-a.example.org::1").await,
        node_answering("b-uid::node-b.example.org::1").await,
        node_answering("c-uid::node-c.example.org::1").await,
    ]
}

fn urls(servers: &[Server; 3]) -> [String; 3] {
    [servers[0].uri(), servers[1].uri(), servers[2].uri()]
}

async fn asked_counts(servers: &[Server; 3]) -> Result<[usize; 3], Box<dyn Error>> {
    Ok([
        received(&servers[0]).await?.len(),
        received(&servers[1]).await?.len(),
        received(&servers[2]).await?.len(),
    ])
}

/// What the tests read of `meta.federation.localization`.
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
    localization: Option<serde::de::IgnoredAny>,
}

// conformance: CP-5
#[tokio::test]
async fn the_communities_xcpd_discovers_select_the_members_asked() -> TestResult {
    let servers = members().await;
    let [a, b, c] = urls(&servers);
    let holding = RespondingGateway::answering(Answer::Holds(vec![
        Community::new(COMMUNITIES[0], "2.999.50.2", "PID-SYNTH-A"),
        Community::new(COMMUNITIES[1], "2.999.60.2", "PID-SYNTH-B"),
    ]))
    .await;
    let empty = RespondingGateway::answering(Answer::NoMatch).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(&config(
        dir.path(),
        [&a, &b, &c],
        &[holding.endpoint(), empty.endpoint()],
        "development",
        "",
    )?)?;

    let (status, text) = call(app.clone(), post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    schema::validate(&text)?;
    let answer: Federated = serde_json::from_str(&text)?;
    assert_eq!(
        vec![
            ("node-a-pub", "active"),
            ("node-b-pub", "active"),
            ("node-c-pub", "not-localized"),
        ],
        statuses(&answer),
        "node C's community was discovered nowhere (§14.1, N4)"
    );
    assert!(
        answer.meta.federation.complete,
        "§11.4: node C was never in scope"
    );
    assert_eq!([1, 1, 0], asked_counts(&servers).await?);
    for stub in [&holding, &empty] {
        let requests = stub.requests().await;
        assert_eq!(
            1,
            requests.len(),
            "the discovery is a broadcast to every gateway"
        );
        assert!(
            requests[0].contains(PATIENT),
            "the localization service is asked by the identifier"
        );
    }
    for server in &servers {
        assert!(
            !wire(server).await?.contains(PATIENT),
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
        Some(format!("\"{XCPD}\"").as_str()),
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
async fn a_failing_gateway_fails_the_query_closed_and_asks_no_member() -> TestResult {
    let servers = members().await;
    let [a, b, c] = urls(&servers);
    let holding = RespondingGateway::answering(Answer::Holds(vec![Community::new(
        COMMUNITIES[0],
        "2.999.50.2",
        "PID-SYNTH-A",
    )]))
    .await;
    let down = RespondingGateway::answering(Answer::Fault).await;
    let dir = tempfile::tempdir()?;
    let app = gateway(&config(
        dir.path(),
        [&a, &b, &c],
        &[holding.endpoint(), down.endpoint()],
        "development",
        "",
    )?)?;

    let (status, text) = call(app, post(body(&patient_query())?)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let answer: Federated = serde_json::from_str(&text)?;
    for endpoint in &answer.meta.federation.endpoints {
        assert_eq!("not-localized", endpoint.status);
        assert!(
            endpoint.error.is_some(),
            "every member carries the error (§14.1)"
        );
    }
    assert_eq!(
        [0, 0, 0],
        asked_counts(&servers).await?,
        "no dispatch, no ask-all"
    );
    let envelope: Envelope = serde_json::from_str(&text)?;
    assert!(
        envelope.meta.federation.localization.is_some(),
        "§14.1 SHOULD"
    );
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
        !errors.contains(PATIENT),
        "no error quotes the identifier: {errors}"
    );
    Ok(())
}

#[test]
fn an_http_gateway_outside_development_refuses_to_boot_naming_its_key() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = config(
        dir.path(),
        [
            "https://a.example.org",
            "https://b.example.org",
            "https://c.example.org",
        ],
        &["http://xcpd.example.org/RespondingGateway".to_owned()],
        "production",
        "",
    )?
    .split("\n[[dev.crossref]]")
    .next()
    .unwrap_or_default()
    .to_owned();
    match Config::from_sources(Some(&text), &BTreeMap::new())?.resolve() {
        Err(error::Error::Cleartext(refused)) => {
            assert_eq!("xcpd.gateway[0].url", refused.site.url_key);
            Ok(())
        }
        other => Err(format!("plain http carries the identifier in clear text: {other:?}").into()),
    }
}

#[test]
fn xcpd_under_the_ask_all_selection_refuses_to_boot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = config(
        dir.path(),
        [
            "https://a.example.org",
            "https://b.example.org",
            "https://c.example.org",
        ],
        &["https://xcpd.example.org/RespondingGateway".to_owned()],
        "development",
        "",
    )?
    .replace(
        "node_selection = \"localized\"",
        "node_selection = \"ask-all\"",
    )
    .replace("[federation.localization]\ntimeout_ms = 1000\n", "");
    match load(&text)? {
        Err(FederationError::Localization(LocalizationError::XcpdUnused)) => Ok(()),
        other => Err(format!("a localizer no query uses is refused: {other:?}").into()),
    }
}

#[test]
fn an_assertion_that_is_no_saml_assertion_refuses_to_boot_naming_its_key() -> TestResult {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("assertion.xml");
    std::fs::write(&file, "<not-an-assertion/>")?;
    let extra = format!(
        "assertion_file = {}",
        toml::Value::String(file.display().to_string())
    );
    let text = config(
        dir.path(),
        [
            "https://a.example.org",
            "https://b.example.org",
            "https://c.example.org",
        ],
        &["https://xcpd.example.org/RespondingGateway".to_owned()],
        "development",
        &extra,
    )?;
    match load(&text)? {
        Err(FederationError::Localization(LocalizationError::XcpdAssertion { key })) => {
            assert_eq!("xcpd.assertion_file", key);
            Ok(())
        }
        other => Err(format!("refused for its assertion: {other:?}").into()),
    }
}

#[test]
fn xcpd_without_a_registry_refuses_to_boot() -> TestResult {
    let text = "[xcpd]\nsender_device = \"2.999.40.1\"\naudit = \"log\"\n\n[[xcpd.gateway]]\nurl = \"https://xcpd.example.org/rg\"\ndevice = \"2.999.50.1\"\n";
    match Config::from_sources(Some(text), &BTreeMap::new())?.resolve() {
        Err(error::Error::Missing { key }) if key == "registry.document" => Ok(()),
        other => Err(format!("refused for its missing registry: {other:?}").into()),
    }
}

/// The configuration text over three unreachable members and one `https`
/// gateway, under `profile`, with the `[xcpd]` keys `extra`.
fn unreachable_config(dir: &Path, profile: &str, extra: &str) -> Result<String, Box<dyn Error>> {
    config(
        dir,
        [
            "https://a.example.org",
            "https://b.example.org",
            "https://c.example.org",
        ],
        &["https://xcpd.example.org/RespondingGateway".to_owned()],
        profile,
        extra,
    )
}

#[test]
fn xcpd_without_an_audit_destination_refuses_to_boot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = unreachable_config(dir.path(), "development", "audit = \"log\"")?
        .replace("audit = \"log\"", "");
    match Config::from_sources(Some(&text), &BTreeMap::new())?.resolve() {
        Err(error::Error::Missing { key }) if key == "xcpd.audit" => Ok(()),
        other => Err(format!("the audit destination is declared (§3.55.5.1): {other:?}").into()),
    }
}

#[test]
fn audit_off_outside_development_refuses_to_boot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = unreachable_config(dir.path(), "production", "audit = \"off\"")?
        .split("\n[[dev.crossref]]")
        .next()
        .unwrap_or_default()
        .to_owned();
    match Config::from_sources(Some(&text), &BTreeMap::new())?.resolve() {
        Err(error::Error::AuditOff { key }) if key == "xcpd.audit" => Ok(()),
        other => Err(format!("no ITI-55 audit outside development: {other:?}").into()),
    }
}

#[tokio::test]
async fn options_declares_where_the_audit_messages_go() -> TestResult {
    for audit in ["log", "off"] {
        let dir = tempfile::tempdir()?;
        let text = unreachable_config(dir.path(), "development", &format!("audit = \"{audit}\""))?;
        let request = Request::options("/").body(Body::empty())?;
        let (status, body) = call(gateway(&text)?, request).await?;
        assert_eq!(StatusCode::OK, status, "{body}");
        let options: OptionsRoot = serde_json::from_str(&body)?;
        assert_eq!(
            Some(format!("\"{audit}\"").as_str()),
            options
                .federation
                .localization
                .extra
                .get("audit")
                .map(serde_json::value::RawValue::get)
        );
    }
    Ok(())
}

#[tokio::test]
async fn each_discovery_writes_an_audit_event_without_the_identifier() -> TestResult {
    let servers = members().await;
    let [a, b, c] = urls(&servers);
    let holding = RespondingGateway::answering(Answer::Holds(vec![Community::new(
        COMMUNITIES[0],
        "2.999.50.2",
        "PID-SYNTH-A",
    )]))
    .await;
    let dir = tempfile::tempdir()?;
    let app = gateway(&config(
        dir.path(),
        [&a, &b, &c],
        &[holding.endpoint()],
        "development",
        "",
    )?)?;
    let logs = crate::support::Logs::default();
    let capture = ferrofed_server::telemetry::subscriber(
        ferrofed_server::telemetry::Rendering::Json,
        "info",
        false,
        logs.clone(),
    )?;
    let guard = tracing::subscriber::set_default(capture);
    let (status, _) = call(app, post(body(&patient_query())?)?).await?;
    drop(guard);
    assert_eq!(StatusCode::OK, status);

    let text = logs.text();
    let audit: Vec<&str> = text
        .lines()
        .filter(|line| line.contains(ferrofed_identity::xcpd::AUDIT_TARGET))
        .collect();
    assert_eq!(1, audit.len(), "one audit event per exchange: {text}");
    for expected in [
        "\"event_id\":\"110112\"",
        "\"event_type\":\"ITI-55\"",
        "\"event_outcome\":\"0\"",
    ] {
        assert!(audit[0].contains(expected), "{expected} in {}", audit[0]);
    }
    assert!(!text.contains(PATIENT), "no identifier in the log: {text}");
    Ok(())
}

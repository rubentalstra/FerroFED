// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! `OPTIONS {base}/` against a gateway configured through the real
//! configuration path: the body validates against the vendored
//! `options-root.schema.json`, every declared value follows the
//! configuration, nothing the gateway does not offer is declared, and
//! `OPTIONS` on a sub-path names the methods served there (§7a.2, N30, CP-23).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::Config;
use ferrofed_server::facade::options::MAX_WINDOW;
use ferrofed_server::federation::{Federation, FederationError};
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Request, StatusCode, header};
use openehr_federation::headers;
use openehr_federation::options::{DefinitionBehaviour, OptionsRoot, SpecVersion};

use crate::facade::{PATIENT, crossref, schema, settings_with_room};
use crate::support::{call, error_body, send};

type TestResult = Result<(), Box<dyn Error>>;

/// The federation id every gateway here is configured with.
const ID: &str = "example-federation";

/// A registry of two organisations, two nodes and three endpoints: node A
/// names its product and version, node B names neither, and node B's second
/// endpoint is suspended.
fn registry(url: &str) -> String {
    format!(
        r#"
[[organisation]]
id = "org-a"

[[organisation]]
id = "org-b"

[[node]]
id = "node-a"
organisation = "org-a"
system_id = "cdr-a.example.org"
product = "Example CDR"
version = "1.2.3"

[[node]]
id = "node-b"
organisation = "org-b"
system_id = "cdr-b.example.org"

[[endpoint]]
id = "node-a-pub"
node = "node-a"
url = "{url}/a"
connection_type = "openehr-rest-query"
managing_organisation = "org-a"

[[endpoint]]
id = "node-b-pub"
node = "node-b"
url = "{url}/b"
connection_type = "openehr-rest-query"
managing_organisation = "org-b"

[[endpoint]]
id = "node-b-spare"
node = "node-b"
url = "{url}/c"
connection_type = "openehr-rest-query"
managing_organisation = "org-b"
status = "suspended"
"#
    )
}

/// The settings text of a gateway over `document`, whose `[federation]`
/// table holds the id, the node selection and `federation`, followed by
/// `tables`.
fn settings_text(document: &Path, federation: &str, tables: &str) -> String {
    let document = toml::Value::String(document.display().to_string());
    format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nid = \"{ID}\"\nnode_selection = \"ask-all\"\n{federation}\n\n{tables}"
    )
}

/// The gateway over [`registry`] at `url`, with `federation` in its
/// `[federation]` table and `tables` after it.
fn gateway(
    dir: &Path,
    url: &str,
    federation: &str,
    tables: &str,
) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(url))?;
    let text = settings_text(&document, federation, tables);
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// `OPTIONS` on `uri`.
fn options(uri: &str) -> Result<Request<Body>, http::Error> {
    Request::options(uri).body(Body::empty())
}

/// The `OPTIONS /` text of the gateway with `federation` in its
/// `[federation]` table, checked against the vendored schema.
async fn described_text(federation: &str) -> Result<String, Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), "http://127.0.0.1:9", federation, "")?;
    let (status, text) = call(app, options("/")?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    schema::validate_options(&text)?;
    Ok(text)
}

/// The `OPTIONS /` body of the gateway with `federation` in its
/// `[federation]` table, read through the wire type.
async fn described(federation: &str) -> Result<OptionsRoot, Box<dyn Error>> {
    Ok(serde_json::from_str(&described_text(federation).await?)?)
}

/// The `paging.max_window` the body declares, if any.
fn max_window(body: &OptionsRoot) -> Result<Option<u32>, serde_json::Error> {
    body.federation
        .paging
        .extra
        .get(MAX_WINDOW)
        .map(|raw| serde_json::from_str(raw.get()))
        .transpose()
}

// conformance: CP-23
#[tokio::test]
async fn the_body_validates_against_the_vendored_schema() -> TestResult {
    let body = described("").await?;
    assert_eq!(ID, body.federation.id.as_str(), "N30: the configured id");
    assert_eq!(
        SpecVersion::of_release(openehr_federation::FEDERATION_SPEC)?,
        body.federation.spec_version,
        "§7a.2: major.minor of the pinned release"
    );
    assert!(body.federation.aql.fan_out, "ask-all fans out (§4.3, N4)");
    assert_eq!(
        "closed", body.federation.localization.on_failure,
        "§14.1: fail-closed, the default"
    );
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn the_options_root_answer_names_the_methods_of_the_base() -> TestResult {
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), "http://127.0.0.1:9", "", "")?;
    let response = send(app, options("/")?).await?;
    assert_eq!(StatusCode::OK, response.status());
    let allow = response
        .headers()
        .get(header::ALLOW)
        .ok_or("an Allow field")?
        .to_str()?;
    assert_eq!("GET, HEAD, OPTIONS", allow, "RFC 9110 §10.2.1");
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .ok_or("a Content-Type field")?
        .to_str()?;
    assert_eq!("application/json", content_type);
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn no_targeting_mechanism_no_carrier_and_no_async_member_is_declared() -> TestResult {
    let body = described("").await?;
    assert!(
        body.extra.is_empty(),
        "only federation and endpoints at the top"
    );
    assert!(
        body.federation.extra.is_empty(),
        "§7a.2: no member beside the modelled ones, so no async (§11.7) and no resolution carrier (§5.4.3)"
    );
    assert!(
        body.federation.aql.extra.is_empty(),
        "§7a.2 targeting-not-declared: fan_out only"
    );
    assert!(body.federation.dedup.extra.is_empty());
    assert!(body.federation.timeout.extra.is_empty());
    assert!(body.federation.completeness.extra().is_empty());
    assert!(body.federation.aggregates.extra.is_empty());
    assert!(body.federation.definition.extra().is_empty());
    assert!(body.federation.localization.extra.is_empty());
    assert!(body.federation.its_rest.extra.is_empty());
    let paging: Vec<&str> = body
        .federation
        .paging
        .extra
        .iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        vec![MAX_WINDOW],
        paging,
        "the bounded strategy's bound only"
    );
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn the_offset_strategy_follows_the_configuration() -> TestResult {
    let bounded = described("").await?;
    assert_eq!(
        "bounded", bounded.federation.paging.offset_strategy,
        "the default"
    );
    assert_eq!(Some(1000), max_window(&bounded)?, "the default bound");
    let narrow = described("max_offset_window = 25").await?;
    assert_eq!("bounded", narrow.federation.paging.offset_strategy);
    assert_eq!(
        Some(25),
        max_window(&narrow)?,
        "§11.6.2: the configured bound"
    );
    let reject = described("offset_strategy = \"reject\"").await?;
    assert_eq!(
        "reject", reject.federation.paging.offset_strategy,
        "§11.6.2 option 1"
    );
    assert_eq!(None, max_window(&reject)?, "a refusal has no bound");
    for body in [&bounded, &narrow, &reject] {
        assert_ne!(
            "cursor", body.federation.paging.offset_strategy,
            "§11.6.4: no materialised cursor is offered"
        );
    }
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn the_decomposable_aggregates_follow_the_configuration() -> TestResult {
    let every = described("").await?;
    assert_eq!(
        vec!["COUNT", "SUM", "MIN", "MAX", "AVG"],
        every.federation.aggregates.decomposable,
        "§11.6.3: the default set"
    );
    let some = described("decomposable_aggregates = [\"MAX\", \"COUNT\"]").await?;
    assert_eq!(
        vec!["COUNT", "MAX"],
        some.federation.aggregates.decomposable
    );
    let none = described("decomposable_aggregates = []").await?;
    assert!(
        none.federation.aggregates.decomposable.is_empty(),
        "§11.6.3: an empty list declares none"
    );
    let text = described_text("decomposable_aggregates = []").await?;
    assert!(
        text.contains(r#""decomposable":[]"#),
        "the empty list is written: {text}"
    );
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn the_dedup_policy_names_the_default_the_modes_and_the_header() -> TestResult {
    let body = described("").await?;
    let text = described_text("").await?;
    assert!(text.contains(r#""default":"none""#), "N15: {text}");
    assert_eq!(
        vec!["none", "version-identity"],
        body.federation.dedup.modes.as_slice(),
        "§10: every mode a request may select"
    );
    assert_eq!(
        Some(headers::DEDUP),
        body.federation.dedup.request_header.as_deref(),
        "§10: the header that selects a mode"
    );
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn the_timeout_budget_follows_the_configuration() -> TestResult {
    let defaults = described("").await?;
    assert_eq!(10_000, defaults.federation.timeout.per_node_ms, "N38");
    assert_eq!(25_000, defaults.federation.timeout.overall_ms, "N38");
    let set = described("per_node_timeout_ms = 1500\noverall_timeout_ms = 4000").await?;
    assert_eq!(
        1500, set.federation.timeout.per_node_ms,
        "N38: discoverable"
    );
    assert_eq!(4000, set.federation.timeout.overall_ms, "N38: discoverable");
    assert_eq!("abandon-and-mark", set.federation.timeout.policy, "§11.5");
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn the_completeness_modes_follow_the_configuration() -> TestResult {
    let offered = described("").await?;
    let completeness = &offered.federation.completeness;
    assert!(completeness.best_effort(), "offered by default");
    let opt_in = completeness
        .opt_in()
        .ok_or("N37: how partial is selected")?;
    assert_eq!(Some(headers::COMPLETENESS), opt_in.header.as_deref());
    assert_eq!(Some(headers::COMPLETENESS_PARTIAL), opt_in.value.as_deref());
    let withdrawn = described("best_effort = false").await?;
    assert!(
        !withdrawn.federation.completeness.best_effort(),
        "N37: not offered"
    );
    assert!(withdrawn.federation.completeness.opt_in().is_none());
    let text = described_text("best_effort = false").await?;
    assert!(
        text.contains(r#""default":"all-or-nothing""#),
        "N37: {text}"
    );
    Ok(())
}

// conformance: CP-23 CP-34
#[tokio::test]
async fn the_definition_area_declares_nothing_offered() -> TestResult {
    let body = described("").await?;
    assert_eq!(
        DefinitionBehaviour::new(false)
            .with_stored_query_registry(false)?
            .with_stored_query_fan_out(false)?,
        body.federation.definition,
        "N43, N44: no template fan-out, no registry, no definition fan-out"
    );
    assert!(
        body.federation
            .its_rest
            .definition
            .starts_with("routed-single-node"),
        "§7a.1, §12.6, N43: {}",
        body.federation.its_rest.definition
    );
    assert!(
        !body.federation.its_rest.definition.contains("unsupported"),
        "§12.7 registry-not-offered: every definition request routes, the versioned PUT included: {}",
        body.federation.its_rest.definition
    );
    assert_eq!(
        Some(crate::support::JWKS_URI),
        body.federation
            .auth
            .as_ref()
            .and_then(|auth| auth.jwks_uri.as_ref())
            .map(openehr_federation::object::Uri::as_str),
        "§13.1, N30: every federating gateway signs, so it declares its JWKS"
    );
    Ok(())
}

// conformance: CP-23 CP-34
#[tokio::test]
async fn the_template_fan_out_upload_follows_the_configuration() -> TestResult {
    for (federation, offered) in [
        ("", false),
        ("fan_out_template_upload = false", false),
        ("fan_out_template_upload = true", true),
    ] {
        let body = described(federation).await?;
        assert_eq!(
            DefinitionBehaviour::new(offered)
                .with_stored_query_registry(false)?
                .with_stored_query_fan_out(false)?,
            body.federation.definition,
            "§7a.2, N43: {federation:?} declares fan_out_template_upload {offered}"
        );
        let described = &body.federation.its_rest.definition;
        assert!(described.starts_with("routed-single-node"), "{described}");
        assert_eq!(
            offered,
            described.contains("fans out"),
            "§7a.1: {described}"
        );
    }
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn the_member_endpoints_follow_the_registry() -> TestResult {
    let body = described("").await?;
    let ids: Vec<&str> = body
        .endpoints
        .iter()
        .map(|endpoint| endpoint.id.as_str())
        .collect();
    assert_eq!(vec!["node-a-pub", "node-b-pub", "node-b-spare"], ids, "N30");
    let [a, b, spare] = body.endpoints.as_slice() else {
        return Err("three member endpoints".into());
    };
    assert_eq!("org-a", a.organisation, "N20, N30");
    assert_eq!("active", a.status.as_str(), "§7a.2: membership, not §11.1");
    assert_eq!(Some("node-a"), a.node_id.as_deref());
    assert_eq!(Some("cdr-a.example.org"), a.system_id.as_deref());
    assert_eq!(Some("Example CDR"), a.product.as_deref(), "§7a.2: product");
    assert_eq!(Some("1.2.3"), a.version.as_deref(), "§7a.2: version");
    assert_eq!("org-b", b.organisation);
    assert_eq!(None, b.product, "N40: never invented");
    assert_eq!(None, b.version, "N40: never invented");
    assert_eq!(
        Some("node-b"),
        spare.node_id.as_deref(),
        "§7a.2: two endpoints of one node"
    );
    assert_eq!(
        "suspended",
        spare.status.as_str(),
        "the registry's standing"
    );
    for endpoint in &body.endpoints {
        assert_eq!(None, endpoint.latency_ms_p50, "no latency is tracked");
        assert!(endpoint.extra.is_empty());
    }
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn the_description_needs_no_patient_identifier_and_holds_none() -> TestResult {
    let dir = tempfile::tempdir()?;
    let rows = crossref(&[("node-a", "2222aaaa-2222-4222-8222-222222222222")]);
    let app = gateway(dir.path(), "http://127.0.0.1:9", "", &rows)?;
    let (status, text) = call(app, options("/")?).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "N30: answered without a patient identifier"
    );
    assert!(
        !text.contains(PATIENT),
        "the cross-reference stays out of it"
    );
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn options_on_a_sub_path_names_the_methods_served_there() -> TestResult {
    let node = Server::start().await;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &node.uri(), "", "")?;
    for (uri, expected) in [
        ("/v1/query/aql", "GET, POST, OPTIONS"),
        ("/v1/ehr", "GET, POST, OPTIONS"),
        ("/v1/ehr/7d44", "GET, PUT, OPTIONS"),
        ("/v1/ehr/7d44/composition", "POST, OPTIONS"),
        (
            "/v1/ehr/7d44/composition/u::s::1",
            "GET, PUT, DELETE, OPTIONS",
        ),
        ("/v1/definition/template/adl1.4", "GET, POST, OPTIONS"),
        ("/v1/definition/template/adl2/t.v1", "GET, OPTIONS"),
        ("/v1/definition/query/org::q", "GET, PUT, OPTIONS"),
        ("/v1/definition/query/org::q/1.0.0", "GET, PUT, OPTIONS"),
    ] {
        let response = send(app.clone(), options(uri)?).await?;
        assert_eq!(StatusCode::NO_CONTENT, response.status(), "{uri}");
        let allow = response
            .headers()
            .get(header::ALLOW)
            .ok_or("an Allow field")?
            .to_str()?;
        assert_eq!(expected, allow, "§7a.2, RFC 9110 §10.2.1: {uri}");
    }
    let requests = node.received_requests().await.ok_or("recording is on")?;
    assert!(requests.is_empty(), "the gateway answers OPTIONS itself");
    Ok(())
}

// conformance: CP-23
#[tokio::test]
async fn options_on_a_path_the_gateway_does_not_serve_is_not_implemented() -> TestResult {
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), "http://127.0.0.1:9", "", "")?;
    for uri in ["/v1/demographic/agent/u::s::1", "/v1/"] {
        let (status, text) = call(app.clone(), options(uri)?).await?;
        assert_eq!(StatusCode::NOT_IMPLEMENTED, status, "N32: {uri}");
        assert_eq!("not-implemented", error_body(&text)?.code);
    }
    let (status, _) = call(app, options("/elsewhere")?).await?;
    assert_eq!(StatusCode::NOT_FOUND, status, "outside the ITS-REST prefix");
    Ok(())
}

#[tokio::test]
async fn without_a_registry_there_is_no_federation_to_describe() -> TestResult {
    let (status, text) = call(crate::support::app(), options("/")?).await?;
    assert_eq!(StatusCode::NOT_IMPLEMENTED, status);
    assert_eq!("not-implemented", error_body(&text)?.code);
    Ok(())
}

#[test]
fn a_registry_without_a_federation_id_refuses_to_boot() -> TestResult {
    let dir = tempfile::tempdir()?;
    let document = dir.path().join("registry.toml");
    std::fs::write(&document, registry("http://127.0.0.1:9"))?;
    let text = settings_text(&document, "", "").replace(&format!("id = \"{ID}\"\n"), "");
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let refused = Federation::load(&settings).err().ok_or("refused")?;
    assert!(
        matches!(refused, FederationError::IdUndeclared),
        "{refused:?}"
    );
    let empty = settings_text(&document, "", "").replace(&format!("\"{ID}\""), "\"\"");
    let error = Config::from_sources(Some(&crate::support::signed(&empty)), &BTreeMap::new())?
        .resolve()
        .err()
        .ok_or("an empty id is refused")?;
    assert!(error.to_string().contains("federation.id"), "{error}");
    Ok(())
}

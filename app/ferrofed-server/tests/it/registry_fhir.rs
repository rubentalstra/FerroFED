// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The registry document in FHIR form (`registry.format = "fhir"`, N19, N20,
//! §15.2): it loads and routes as the native form does, and `config check`
//! refuses what the form refuses with the configuration exit code.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use ferrofed_server::EXIT_CONFIG;
use ferrofed_server::config::Config;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use http::StatusCode;

use crate::facade::{
    Answer, EHR_A, EHR_B, body, crossref, node_answering, patient_query, post, received, registry,
    settings_with_room, statuses,
};
use crate::run::binary;
use crate::support::call;

type TestResult = Result<(), Box<dyn Error>>;

/// One `Organization` entry operating `endpoint`.
fn organisation(id: &str, endpoint: &str) -> String {
    format!(
        r#"{{
  "fullUrl": "https://registry.example.org/fhir/Organization/{id}",
  "resource": {{
    "resourceType": "Organization",
    "id": "{id}",
    "identifier": [{{"system": "https://ferrofed.eu/fhir/sid/organisation-id", "value": "{id}"}}],
    "endpoint": [{{"reference": "Endpoint/{endpoint}"}}]
  }}
}}"#
    )
}

/// One `Endpoint` entry of `node` at `url`, managed by `manager`, carrying
/// `connection_type` as its `connectionType`.
fn endpoint(
    id: &str,
    node: &str,
    system_id: &str,
    url: &str,
    manager: &str,
    connection_type: &str,
) -> String {
    format!(
        r#"{{
  "fullUrl": "https://registry.example.org/fhir/Endpoint/{id}",
  "resource": {{
    "resourceType": "Endpoint",
    "id": "{id}",
    "identifier": [
      {{"system": "https://ferrofed.eu/fhir/sid/endpoint-id", "value": "{id}"}},
      {{"system": "https://ferrofed.eu/fhir/sid/node-id", "value": "{node}"}},
      {{"system": "https://ferrofed.eu/fhir/sid/system-id", "value": "{system_id}"}}
    ],
    "status": "active",
    "connectionType": {connection_type},
    "managingOrganization": {{"reference": "Organization/{manager}"}},
    "payloadType": [{{"text": "openEHR"}}],
    "address": "{url}"
  }}
}}"#
    )
}

const OPENEHR: &str = r#"{"system": "https://ferrofed.eu/fhir/CodeSystem/connection-type", "code": "openehr-rest-query"}"#;

/// The registry of [`registry`] (node A and node B at `a` and `b`) as a FHIR
/// Bundle, node A's endpoint carrying `connection_type`.
fn fhir_registry(a: &str, b: &str, connection_type: &str) -> String {
    format!(
        r#"{{"resourceType": "Bundle", "type": "collection", "entry": [{}, {}, {}, {}]}}"#,
        organisation("org-a", "node-a-pub"),
        organisation("org-b", "node-b-pub"),
        endpoint(
            "node-a-pub",
            "node-a",
            "cdr-a.example.org",
            a,
            "org-a",
            connection_type
        ),
        endpoint(
            "node-b-pub",
            "node-b",
            "cdr-b.example.org",
            b,
            "org-b",
            OPENEHR
        ),
    )
}

/// The development federation over `document`, written into `dir` in
/// `format`, resolving the patient at both nodes.
fn federation(dir: &Path, document: &str, format: &str) -> Result<Federation, Box<dyn Error>> {
    let path = dir.join(format!("registry.{format}"));
    std::fs::write(&path, document)?;
    let path = toml::Value::String(path.display().to_string());
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {path}\nformat = \"{format}\"\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{}",
        crossref(&[("node-a", EHR_A), ("node-b", EHR_B)])
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    Ok(Federation::load(&settings)?.ok_or("a registry is configured")?)
}

fn router(federation: Federation) -> Router {
    ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    )
}

#[test]
fn the_fhir_form_loads_the_snapshot_of_the_native_form() -> TestResult {
    let dir = tempfile::tempdir()?;
    let a = "https://cdr-a.example.org/openehr";
    let b = "https://cdr-b.example.org/openehr";
    let native = federation(dir.path(), &registry(a, b, ""), "toml")?;
    let fhir = federation(dir.path(), &fhir_registry(a, b, OPENEHR), "fhir")?;
    assert_eq!(native.snapshot(), fhir.snapshot());
    Ok(())
}

#[tokio::test]
async fn a_patient_query_routes_over_the_fhir_form_as_over_the_native_form() -> TestResult {
    let mut answers = Vec::new();
    let mut dispatched = Vec::new();
    for format in ["toml", "fhir"] {
        let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
        let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
        let document = match format {
            "toml" => registry(&a.uri(), &b.uri(), ""),
            _ => fhir_registry(&a.uri(), &b.uri(), OPENEHR),
        };
        let dir = tempfile::tempdir()?;
        let app = router(federation(dir.path(), &document, format)?);
        let (status, text) = call(app, post(body(&patient_query())?)?).await?;
        assert_eq!(StatusCode::OK, status, "{format}: {text}");
        let answer: Answer = serde_json::from_str(&text)?;
        answers.push((
            answer.rows.clone(),
            statuses(&answer)
                .into_iter()
                .map(|(id, status)| (id.to_owned(), status.to_owned()))
                .collect::<Vec<_>>(),
        ));
        dispatched.push((received(&a).await?, received(&b).await?));
    }
    let [native, fhir] = answers.as_slice() else {
        return Err("two runs".into());
    };
    assert_eq!(native, fhir);
    let [native, fhir] = dispatched.as_slice() else {
        return Err("two runs".into());
    };
    assert_eq!(native, fhir);
    assert!(
        !native.0.is_empty() && !native.1.is_empty(),
        "both nodes were asked"
    );
    Ok(())
}

/// Runs `config check` over a FHIR-form document holding `document`.
fn config_check(document: &str) -> Result<std::process::Output, Box<dyn Error>> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(document.as_bytes())?;
    let path = toml::Value::String(file.path().display().to_string());
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:1\"\n\n[registry]\ndocument = {path}\nformat = \"fhir\"\n\n\
         [federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
    );
    binary(&["config", "check"], &toml)
}

#[test]
fn config_check_accepts_a_registry_in_fhir_form() -> TestResult {
    let output = config_check(&fhir_registry(
        "http://127.0.0.1:9/openehr",
        "http://127.0.0.1:10/openehr",
        OPENEHR,
    ))?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn config_check_refuses_an_endpoint_with_hl7_fhir_rest_naming_it() -> TestResult {
    let output = config_check(&fhir_registry(
        "http://127.0.0.1:9/openehr",
        "http://127.0.0.1:10/openehr",
        r#"{"system": "http://terminology.hl7.org/CodeSystem/endpoint-connection-type", "code": "hl7-fhir-rest"}"#,
    ))?;
    assert_eq!(Some(i32::from(EXIT_CONFIG)), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("endpoint node-a-pub") && stderr.contains("hl7-fhir-rest"),
        "the endpoint and the refused code are named: {stderr}"
    );
    Ok(())
}

#[test]
fn config_check_refuses_an_informal_connection_type_naming_the_endpoint() -> TestResult {
    let output = config_check(&fhir_registry(
        "http://127.0.0.1:9/openehr",
        "http://127.0.0.1:10/openehr",
        r#"{"code": "open-ehr-query-API"}"#,
    ))?;
    assert_eq!(Some(i32::from(EXIT_CONFIG)), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("endpoint node-a-pub") && stderr.contains("informal string"),
        "the endpoint and the reason are named: {stderr}"
    );
    Ok(())
}

#[test]
fn config_check_refuses_an_endpoint_whose_managing_organisation_does_not_resolve() -> TestResult {
    let document = fhir_registry(
        "http://127.0.0.1:9/openehr",
        "http://127.0.0.1:10/openehr",
        OPENEHR,
    )
    .replacen("Organization/org-a\"}", "Organization/org-z\"}", 1);
    let output = config_check(&document)?;
    assert_eq!(Some(i32::from(EXIT_CONFIG)), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("endpoint node-a-pub") && stderr.contains("Organization/org-z"),
        "the endpoint and the dangling reference are named: {stderr}"
    );
    Ok(())
}

#[test]
fn config_check_refuses_a_native_document_declared_as_fhir() -> TestResult {
    let output = config_check(&registry(
        "http://127.0.0.1:9/openehr",
        "http://127.0.0.1:10/openehr",
        "",
    ))?;
    assert_eq!(Some(i32::from(EXIT_CONFIG)), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not JSON"), "{stderr}");
    Ok(())
}

#[test]
fn an_unknown_registry_format_is_refused() -> TestResult {
    let output = binary(
        &["config", "check"],
        "[registry]\ndocument = \"/nonexistent/registry.json\"\nformat = \"xml\"\n",
    )?;
    assert_eq!(Some(i32::from(EXIT_CONFIG)), output.status.code());
    Ok(())
}

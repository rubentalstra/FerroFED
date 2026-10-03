// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The federated stored-query registry against two mock nodes (§12.7, N44,
//! N33; CP-40, CP-28): definitions held at the gateway on ITS-REST's semver
//! segment, a held version immutable across a restart, a literal patient
//! refused at storage, and a definition invoked by name as an ordinary
//! fan-out whose answer names the gateway's definition. The store suite every
//! writable backend passes is [`suite`], the `[stored_queries]` backends are
//! configured in [`backends`], and the read-only backend is [`files`].
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

mod backends;
mod declaration;
mod files;
mod invocation;
mod storage;
pub(crate) mod suite;

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::Config;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Request, StatusCode, header};
use serde::Deserialize;

use crate::facade::{EHR_A, EHR_B, NAMESPACE, PATIENT, crossref, registry, settings_with_room};
use crate::support::call;

type TestResult = Result<(), Box<dyn Error>>;

/// The qualified name every fixture stores under.
const NAME: &str = "org.example::patient_compositions";

/// A definition naming the patient through `$patient`, in the fixture
/// namespace, with one more parameter, `$name`.
fn parameterised() -> String {
    format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = $patient \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}' \
         AND c/name/value = $name"
    )
}

/// The `query_parameters` binding the patient and the composition name.
fn bound() -> String {
    format!(r#"{{"query_parameters":{{"patient":"{PATIENT}","name":"Visit"}}}}"#)
}

/// The settings text of a gateway over `document`, holding its definitions
/// in `store`, resolving the patient at the members `rows` name.
fn settings_text(document: &Path, store: &Path, rows: &[(&str, &str)]) -> String {
    let document = toml::Value::String(document.display().to_string());
    let store = toml::Value::String(store.display().to_string());
    format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n\
         [federation]\nid = \"example-federation\"\nnode_selection = \"ask-all\"\n\
         per_node_timeout_ms = 2000\noverall_timeout_ms = 3000\n\n\
         [stored_queries]\npath = {store}\n{}",
        crossref(rows)
    )
}

/// The store file of a gateway whose state lives in `dir`.
fn store_file(dir: &Path) -> PathBuf {
    dir.join("definitions.redb")
}

/// A gateway offering the registry, its registry document `registry` and its
/// store in `dir`, resolving the patient at the members `rows` name.
pub(crate) fn gateway(
    dir: &Path,
    registry: &str,
    rows: &[(&str, &str)],
) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry)?;
    let text = settings_text(&document, &store_file(dir), rows);
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let state = AppState::build(&settings)?;
    Ok(ferrofed_server::router(
        Arc::new(state),
        &settings_with_room(),
    ))
}

/// A registry gateway over node A and node B, the patient known at both.
fn two_members(dir: &Path, a: &Server, b: &Server) -> Result<Router, Box<dyn Error>> {
    gateway(
        dir,
        &registry(&a.uri(), &b.uri(), ""),
        &[("node-a", EHR_A), ("node-b", EHR_B)],
    )
}

/// `PUT {base}/v1/definition/query/{name}/{version}` with the AQL `aql`.
fn put(name: &str, version: &str, aql: &str) -> Result<Request<Body>, http::Error> {
    Request::put(format!("/v1/definition/query/{name}/{version}"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(aql.to_owned()))
}

/// `GET {base}/v1/definition/query/{path}`.
fn get(path: &str) -> Result<Request<Body>, http::Error> {
    Request::get(format!("/v1/definition/query/{path}")).body(Body::empty())
}

/// `POST {base}/v1/query/{path}` with the `Query` body `body` and the header
/// lines `fields`.
fn invoke(path: &str, body: &str, fields: &[(&str, &str)]) -> Result<Request<Body>, http::Error> {
    let mut request =
        Request::post(format!("/v1/query/{path}")).header(header::CONTENT_TYPE, "application/json");
    for (name, value) in fields {
        request = request.header(*name, *value);
    }
    request.body(Body::from(body.to_owned()))
}

/// The ITS-REST `name` member of a result set.
#[derive(Debug, Deserialize)]
struct Named {
    name: Option<String>,
}

/// Stores `aql` at `version` of [`NAME`] and checks it was stored.
async fn stored(app: &Router, version: &str, aql: &str) -> TestResult {
    let (status, text) = call(app.clone(), put(NAME, version, aql)?).await?;
    assert_eq!(StatusCode::OK, status, "§12.7: stored: {text}");
    Ok(())
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A registry read from the harness directory is the registry of the
//! bootstrap document: the same snapshot, the same nodes asked for the same
//! queries and follow-ups, and the same answers (§15.1, N21, CP-13).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mcsd::HarnessDirectory;
use ferrofed_testkit::mock::Server;
use http::{Request, StatusCode};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::{Gateway, config, members, settings};
use crate::facade::{
    Answer, EHR_A, EHR_B, body, crossref, patient_query, post, registry, settings_with_room,
    statuses,
};
use crate::support::{call, send};

type TestResult = Result<(), Box<dyn Error>>;

/// A version node A created, which its row names and a follow-up reads.
const AT_A: &str = "8849182c-82ad-4088-a07f-48ead4180515::cdr-a.example.org::1";

/// A version node B created.
const AT_B: &str = "5c3e9b1a-7d2f-4e8a-9b6c-1f0e2d3c4b5a::cdr-b.example.org::1";

/// A node holding the EHR `ehr_id` with the version `uid` in it, answering
/// the federated query with one row naming `uid`.
async fn node(ehr_id: &str, uid: &str) -> Server {
    let server = Server::start().await;
    let answer = format!(
        r##"{{"q":"node","columns":[{{"name":"#0","path":"c/uid/value"}}],"rows":[["{uid}"]]}}"##
    );
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(answer.into_bytes(), "application/json"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/ehr/{ehr_id}")))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/ehr/{ehr_id}/composition/{uid}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!(r#"{{"_type":"COMPOSITION","uid":{{"_type":"OBJECT_VERSION_ID","value":"{uid}"}}}}"#)
                .into_bytes(),
            "application/json",
        ))
        .mount(&server)
        .await;
    server
}

/// The method, path and body of every request `server` received.
async fn asked(server: &Server) -> Result<Vec<(String, String, String)>, Box<dyn Error>> {
    let requests = server.received_requests().await.ok_or("recording is on")?;
    let mut asked = Vec::new();
    for request in requests {
        asked.push((
            request.method.to_string(),
            request.url.path().to_owned(),
            String::from_utf8(request.body.clone())?,
        ));
    }
    Ok(asked)
}

/// The development gateway over the bootstrap document of node A at `a` and
/// node B at `b`.
fn over_document(dir: &std::path::Path, a: &str, b: &str) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(a, b, ""))?;
    let document = toml::Value::String(document.display().to_string());
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{}",
        crossref(&[("node-a", EHR_A), ("node-b", EHR_B)])
    );
    let federation = Federation::load(&settings(&text)?)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// What the client sees and what each node is asked: a patient query, then a
/// follow-up read of node A's version.
type Run = (
    (StatusCode, Vec<Vec<String>>, Vec<(String, String)>),
    (StatusCode, String),
    Vec<(String, String, String)>,
    Vec<(String, String, String)>,
);

/// Runs the patient query and the follow-up through `app` over `a` and `b`.
async fn run(app: Router, a: &Server, b: &Server) -> Result<Run, Box<dyn Error>> {
    let (status, text) = call(app.clone(), post(body(&patient_query())?)?).await?;
    let answer: Answer = serde_json::from_str(&text)?;
    let query = (
        status,
        answer.rows.clone(),
        statuses(&answer)
            .into_iter()
            .map(|(id, status)| (id.to_owned(), status.to_owned()))
            .collect(),
    );
    let read = Request::get(format!("/v1/ehr/{EHR_A}/composition/{AT_A}")).body(Body::empty())?;
    let response = send(app, read).await?;
    let acting = response
        .headers()
        .get("openEHR-federation-endpoint")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let follow_up = (response.status(), acting);
    Ok((query, follow_up, asked(a).await?, asked(b).await?))
}

// conformance: CP-13
#[tokio::test]
async fn the_directory_materialises_the_snapshot_of_the_bootstrap_document() -> TestResult {
    let harness = HarnessDirectory::start().await;
    let a = "https://cdr-a.example.org/openehr";
    let b = "https://cdr-b.example.org/openehr";
    harness.publish(&members(a, b))?;
    let dir = tempfile::tempdir()?;
    let document = dir.path().join("registry.toml");
    std::fs::write(&document, registry(a, b, ""))?;
    let named = toml::Value::String(document.display().to_string());
    let native = settings(&format!(
        "[registry]\ndocument = {named}\n\n[federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
    ))?;
    let native = Federation::load(&native)?.ok_or("a registry is configured")?;
    let directory = Federation::load(&settings(&config(&harness.base(), 5_000))?)?
        .ok_or("a registry is configured")?;
    assert_eq!(native.snapshot(), directory.snapshot());
    Ok(())
}

/// A registry loaded from a harness mCSD directory routes identically to the
/// bootstrap document: the patient query asks the same nodes with the same
/// AQL and merges the same rows, and a follow-up read of a version under a
/// path `ehr_id` reaches the same node.
// conformance: CP-13
#[tokio::test]
async fn a_registry_from_a_harness_directory_routes_identically_to_the_bootstrap_document()
-> TestResult {
    let mut runs = Vec::new();
    for source in ["document", "directory"] {
        let a = node(EHR_A, AT_A).await;
        let b = node(EHR_B, AT_B).await;
        let dir = tempfile::tempdir()?;
        let harness = HarnessDirectory::start().await;
        let app = if source == "document" {
            over_document(dir.path(), &a.uri(), &b.uri())?
        } else {
            harness.publish(&members(&a.uri(), &b.uri()))?;
            Gateway::boot(&harness, 5_000)?.router()
        };
        let run = run(app, &a, &b).await?;
        assert_eq!(StatusCode::OK, run.0.0, "{source}: the query is answered");
        assert_eq!(
            StatusCode::OK,
            run.1.0,
            "{source}: the follow-up is answered"
        );
        assert_eq!(
            "node-a-pub", run.1.1,
            "{source}: node A answers the follow-up"
        );
        assert!(
            !run.2.is_empty() && !run.3.is_empty(),
            "{source}: both nodes are asked"
        );
        runs.push(run);
    }
    let [document, directory] = runs.as_slice() else {
        return Err("two runs".into());
    };
    assert_eq!(document, directory);
    Ok(())
}

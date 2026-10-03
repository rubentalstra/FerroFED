// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The PostgreSQL store of the stored-query registry against a real
//! PostgreSQL 18, one database per use on one server (§12.7, N44, CP-40):
//! the store suite every writable store passes, two gateway instances in
//! one process racing the same new version with exactly one stored, and a
//! version one instance stored read and run by name at the other.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_registry::definition::store::{DefinitionStore, StoreError};
use ferrofed_registry::secret::SecretUrl;
use ferrofed_server::config::Config;
use ferrofed_server::state::AppState;
use ferrofed_server::stored::postgres::PostgresStore;
use ferrofed_testkit::containers;
use http::{Request, StatusCode, header};
use openehr_its::rest::generated::definition::StoredQuery;

use crate::e2e::TestResult;
use crate::facade::{
    Answer, EHR_A, EHR_B, NAMESPACE, PATIENT, crossref, node_answering, registry,
    settings_with_room, statuses,
};
use crate::stored::suite::SCENARIOS;
use crate::support::{call, error_body};

/// The qualified name the race stores under.
const NAME: &str = "org.example::raced";

/// How many new versions the two instances race for.
const ROUNDS: u32 = 24;

/// A gateway over `members` whose registry is the PostgreSQL database at
/// `url`, built where a blocking open may wait.
fn gateway(dir: &Path, members: (&str, &str), url: &str) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(members.0, members.1, ""))?;
    let document = toml::Value::String(document.display().to_string());
    let url = toml::Value::String(url.to_owned());
    let rows = crossref(&[("node-a", EHR_A), ("node-b", EHR_B)]);
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n\
         [federation]\nid = \"example-federation\"\nnode_selection = \"ask-all\"\n\
         per_node_timeout_ms = 2000\noverall_timeout_ms = 3000\n\n\
         [stored_queries]\nbackend = \"postgres\"\nurl = {url}\n{rows}"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let state = tokio::task::block_in_place(|| AppState::build(&settings))?;
    Ok(ferrofed_server::router(
        Arc::new(state),
        &settings_with_room(),
    ))
}

/// `PUT {base}/v1/definition/query/{NAME}/{version}` with the AQL `aql`.
fn put(version: &str, aql: &str) -> Result<Request<Body>, http::Error> {
    Request::put(format!("/v1/definition/query/{NAME}/{version}"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(aql.to_owned()))
}

/// `GET {base}/v1/definition/query/{path}`.
fn get(path: &str) -> Result<Request<Body>, http::Error> {
    Request::get(format!("/v1/definition/query/{path}")).body(Body::empty())
}

/// A definition naming no patient, marked by `who` stored it.
fn marked(who: &str) -> String {
    format!("SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c WHERE c/name/value = '{who}'")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_postgresql_store_passes_the_store_suite() -> TestResult {
    if !containers::e2e_enabled() {
        return Ok(());
    }
    let databases: Vec<String> = SCENARIOS
        .iter()
        .map(|(scenario, _)| format!("suite_{scenario}"))
        .collect();
    let (first, others) = databases.split_first().ok_or("one database or more")?;
    let others: Vec<&str> = others.iter().map(String::as_str).collect();
    let server = containers::postgres(first, &others).await?;
    for ((scenario, run), database) in SCENARIOS.into_iter().zip(&databases) {
        let url = SecretUrl::new(server.url(database));
        let open = || -> Result<Box<dyn DefinitionStore>, StoreError> {
            Ok(Box::new(PostgresStore::open(&url)?))
        };
        tokio::task::block_in_place(|| run(&open))
            .map_err(|failed| format!("{scenario}: {failed}"))?;
    }
    let url = SecretUrl::new(server.url(first));
    let store = tokio::task::block_in_place(|| PostgresStore::open(&url))?;
    assert!(store.is_shared(), "every replica inserts into it");
    assert!(!store.is_read_only());
    Ok(())
}

// conformance: CP-40
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_gateways_racing_one_new_version_store_exactly_one() -> TestResult {
    if !containers::e2e_enabled() {
        return Ok(());
    }
    let server = containers::postgres("stored_race", &[]).await?;
    let url = server.url("stored_race");
    let dir = tempfile::tempdir()?;
    let members = ("http://a.invalid", "http://b.invalid");
    let one = gateway(dir.path(), members, &url)?;
    let two = gateway(dir.path(), members, &url)?;
    for round in 0..ROUNDS {
        let version = format!("1.{round}.0");
        let (first, second) = tokio::join!(
            call(one.clone(), put(&version, &marked("one"))?),
            call(two.clone(), put(&version, &marked("two"))?),
        );
        let (first, second) = (first?, second?);
        let winner = match (first.0, second.0) {
            (StatusCode::OK, StatusCode::CONFLICT) => {
                assert_eq!("stored-query-held", error_body(&second.1)?.code);
                "one"
            }
            (StatusCode::CONFLICT, StatusCode::OK) => {
                assert_eq!("stored-query-held", error_body(&first.1)?.code);
                "two"
            }
            other => panic!(
                "§12.7, N44: exactly one PUT of {version} is stored, the other refused: {other:?} {first:?} {second:?}"
            ),
        };
        for gateway in [&one, &two] {
            let (status, text) = call(gateway.clone(), get(&format!("{NAME}/{version}"))?).await?;
            assert_eq!(StatusCode::OK, status, "{text}");
            let held: StoredQuery = serde_json::from_str(&text)?;
            assert!(
                held.q.contains(&format!("'{winner}'")),
                "both instances hold the winner's definition of {version}: {}",
                held.q
            );
        }
    }
    let (status, text) = call(two, get("org.example")?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let listed: Vec<StoredQuery> = serde_json::from_str(&text)?;
    assert_eq!(
        usize::try_from(ROUNDS)?,
        listed.len(),
        "one version a round, whichever instance stored it"
    );
    Ok(())
}

// conformance: CP-40
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_one_instance_stored_is_run_by_name_at_the_other() -> TestResult {
    if !containers::e2e_enabled() {
        return Ok(());
    }
    let server = containers::postgres("stored_shared", &[]).await?;
    let url = server.url("stored_shared");
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let dir = tempfile::tempdir()?;
    let members = (a.uri(), b.uri());
    let one = gateway(dir.path(), (&members.0, &members.1), &url)?;
    let two = gateway(dir.path(), (&members.0, &members.1), &url)?;
    let aql = format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = $patient \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}'"
    );
    let (status, text) = call(one, put("2.0.0", &aql)?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");

    let body = format!(r#"{{"query_parameters":{{"patient":"{PATIENT}"}}}}"#);
    let request = Request::post(format!("/v1/query/{NAME}/2"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))?;
    let (status, text) = call(two, request).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "§12.7: run at the other replica: {text}"
    );
    let answer: Answer = serde_json::from_str(&text)?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "active")],
        statuses(&answer)
    );
    Ok(())
}

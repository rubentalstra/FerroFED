// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The read-only stored-query registry (§12.7, N44): definitions loaded at
//! start from one file per name and version, read and run by name as the
//! writable registry's are, a `PUT` refused with `405` and an `Allow` naming
//! the read methods, `OPTIONS` listing no `PUT`, and a malformed directory
//! refusing the start with an error naming the file and never its content.
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
use ferrofed_registry::definition::store::StoreError;
use ferrofed_server::config::Config;
use ferrofed_server::config::error::Error as ConfigError;
use ferrofed_server::config::settings::Settings;
use ferrofed_server::config::stored_queries::Backend;
use ferrofed_server::state::{AppState, StateError};
use ferrofed_server::stored::files::FilesError;
use ferrofed_testkit::mock::Server;
use http::{Method, Request, StatusCode, header};
use openehr_its::rest::generated::definition::StoredQuery;

use crate::facade::{
    EHR_A, EHR_B, NAMESPACE, PATIENT, crossref, node_answering, registry, settings_with_room,
    statuses,
};
use crate::support::{call, error_body, exchange};

type TestResult = Result<(), Box<dyn Error>>;

/// The qualified name every fixture holds.
const NAME: &str = "org.example::patient_compositions";

/// Text that marks a definition's content, which no refusal may quote.
const SENTINEL: &str = "SENTINEL_DEFINITION_TEXT_7q";

/// A definition naming the patient through `$patient`.
fn parameterised() -> String {
    format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = $patient \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}'"
    )
}

/// Writes `aql` as the definition file of `name` at `version` under `dir`.
fn write(dir: &Path, name: &str, version: &str, aql: &[u8]) -> TestResult {
    let named = dir.join(name);
    std::fs::create_dir_all(&named)?;
    std::fs::write(named.join(format!("{version}.aql")), aql)?;
    Ok(())
}

/// The settings of a gateway over `a` and `b` whose registry reads the
/// definition directory `definitions`, with the `[federation]` lines
/// `federation`.
fn settings(
    dir: &Path,
    (a, b): (&str, &str),
    definitions: &Path,
    federation: &str,
) -> Result<Settings, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(a, b, ""))?;
    let document = toml::Value::String(document.display().to_string());
    let definitions = toml::Value::String(definitions.display().to_string());
    let rows = crossref(&[("node-a", EHR_A), ("node-b", EHR_B)]);
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n\
         [federation]\nid = \"example-federation\"\nnode_selection = \"ask-all\"\n\
         per_node_timeout_ms = 2000\noverall_timeout_ms = 3000\n{federation}\n\n\
         [stored_queries]\nbackend = \"files\"\npath = {definitions}\n{rows}"
    );
    Ok(Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?)
}

/// A gateway over two mock members whose registry reads `definitions`.
fn gateway(
    dir: &Path,
    (a, b): (&Server, &Server),
    definitions: &Path,
) -> Result<Router, Box<dyn Error>> {
    let settings = settings(dir, (&a.uri(), &b.uri()), definitions, "")?;
    let state = AppState::build(&settings)?;
    Ok(ferrofed_server::router(
        Arc::new(state),
        &settings_with_room(),
    ))
}

/// `PUT {base}/v1/definition/query/{path}` with the AQL `aql`.
fn put(path: &str, aql: &str) -> Result<Request<Body>, http::Error> {
    Request::put(format!("/v1/definition/query/{path}"))
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(aql.to_owned()))
}

/// `{method} {base}/v1/definition/query/{path}` with no body.
fn bare(method: Method, path: &str) -> Result<Request<Body>, http::Error> {
    Request::builder()
        .method(method)
        .uri(format!("/v1/definition/query/{path}"))
        .body(Body::empty())
}

/// A malformed directory: the directory, the file in it (none for a stray
/// file), the content, what the refusal says, and the entry it names.
type Case<'a> = (&'a str, Option<&'a str>, &'a [u8], &'a str, &'a str);

/// Every message of `error` and its causes, one line.
fn chain(error: &dyn Error) -> String {
    let mut line = error.to_string();
    let mut cause = error.source();
    while let Some(source) = cause {
        line.push_str(": ");
        line.push_str(&source.to_string());
        cause = source.source();
    }
    line
}

// conformance: CP-40
#[tokio::test]
async fn a_definition_file_is_read_listed_and_run_by_name() -> TestResult {
    let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
    let dir = tempfile::tempdir()?;
    let definitions = dir.path().join("definitions");
    write(&definitions, NAME, "1.0.0", parameterised().as_bytes())?;
    let app = gateway(dir.path(), (&a, &b), &definitions)?;

    let (status, text) = call(app.clone(), bare(Method::GET, &format!("{NAME}/1"))?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let held: StoredQuery = serde_json::from_str(&text)?;
    assert_eq!((NAME, "1.0.0"), (held.name.as_str(), held.version.as_str()));
    assert!(held.q.contains("$patient"), "the admitted text: {}", held.q);

    let (status, text) = call(app.clone(), bare(Method::GET, "org.example")?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let listed: Vec<StoredQuery> = serde_json::from_str(&text)?;
    assert_eq!(1, listed.len(), "{text}");

    let body = format!(r#"{{"query_parameters":{{"patient":"{PATIENT}"}}}}"#);
    let request = Request::post(format!("/v1/query/{NAME}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))?;
    let (status, text) = call(app, request).await?;
    assert_eq!(StatusCode::OK, status, "§12.7: run by name: {text}");
    let answer: crate::facade::Answer = serde_json::from_str(&text)?;
    assert_eq!(
        vec![("node-a-pub", "active"), ("node-b-pub", "active")],
        statuses(&answer),
        "an ordinary fan-out over both members"
    );
    Ok(())
}

// conformance: CP-40
#[tokio::test]
async fn a_put_at_a_read_only_registry_is_a_405_naming_the_read_methods() -> TestResult {
    let a = Server::start().await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let definitions = dir.path().join("definitions");
    write(&definitions, NAME, "1.0.0", parameterised().as_bytes())?;
    let app = gateway(dir.path(), (&a, &b), &definitions)?;

    for path in [
        format!("{NAME}/1.0.0"),
        format!("{NAME}/2.0.0"),
        NAME.to_owned(),
    ] {
        let (status, headers, body) = exchange(app.clone(), put(&path, &parameterised())?).await?;
        let text = String::from_utf8(body)?;
        assert_eq!(StatusCode::METHOD_NOT_ALLOWED, status, "{path}: {text}");
        assert_eq!("stored-query-read-only", error_body(&text)?.code);
        assert_eq!(
            Some("GET, OPTIONS"),
            headers.get(header::ALLOW).and_then(|v| v.to_str().ok()),
            "RFC 9110 §15.5.6: {path}"
        );
    }
    let (status, text) = call(app, bare(Method::GET, &format!("{NAME}/2.0.0"))?).await?;
    assert_eq!(StatusCode::NOT_FOUND, status, "nothing was stored: {text}");
    for node in [&a, &b] {
        let requests = node.received_requests().await.ok_or("recording is on")?;
        assert!(requests.is_empty(), "no node is asked");
    }
    Ok(())
}

#[tokio::test]
async fn options_on_a_read_only_definition_lists_no_put() -> TestResult {
    let a = Server::start().await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let definitions = dir.path().join("definitions");
    std::fs::create_dir_all(&definitions)?;
    let app = gateway(dir.path(), (&a, &b), &definitions)?;
    for path in [format!("{NAME}/1.0.0"), NAME.to_owned()] {
        let (status, headers, _) = exchange(app.clone(), bare(Method::OPTIONS, &path)?).await?;
        assert_eq!(StatusCode::NO_CONTENT, status, "{path}");
        assert_eq!(
            Some("GET, OPTIONS"),
            headers.get(header::ALLOW).and_then(|v| v.to_str().ok()),
            "§7a.2: {path}"
        );
    }
    Ok(())
}

/// The refusal a gateway over the directory `definitions` starts with.
fn refused_start(dir: &Path, definitions: &Path) -> Result<StateError, Box<dyn Error>> {
    let settings = settings(
        dir,
        ("http://a.invalid", "http://b.invalid"),
        definitions,
        "",
    )?;
    let checked = AppState::check(&settings)
        .err()
        .ok_or("config check refuses the directory")?;
    let built = AppState::build(&settings)
        .err()
        .ok_or("the start is refused")?;
    assert_eq!(chain(&checked), chain(&built), "config check says the same");
    Ok(built)
}

#[test]
fn a_malformed_definition_directory_refuses_the_start_naming_the_file() -> TestResult {
    let literal = format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = '{PATIENT}' \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}' \
         AND c/name/value = '{SENTINEL}'"
    );
    let not_aql = format!("SELECT {SENTINEL} FROM");
    let not_utf8 = [b'S', 0xff, 0xfe, b'Q'];
    let valid = parameterised();
    let cases: [Case<'_>; 7] = [
        (
            "stray.aql",
            None,
            b"",
            "is not a definition file",
            "stray.aql",
        ),
        (
            "aql",
            Some("1.0.0"),
            valid.as_bytes(),
            "is not named for a stored query",
            "aql",
        ),
        (
            NAME,
            Some("1.0"),
            valid.as_bytes(),
            "is not named for a stored-query version",
            "1.0.aql",
        ),
        (
            NAME,
            Some("01.0.0"),
            valid.as_bytes(),
            "is not named for a stored-query version",
            "01.0.0.aql",
        ),
        (
            NAME,
            Some("1.0.0"),
            &not_utf8,
            "is not UTF-8 text",
            "1.0.0.aql",
        ),
        (
            NAME,
            Some("1.0.0"),
            not_aql.as_bytes(),
            "is refused as a stored-query definition",
            "1.0.0.aql",
        ),
        (
            NAME,
            Some("1.0.0"),
            literal.as_bytes(),
            "names its patient by a literal",
            "1.0.0.aql",
        ),
    ];
    for (name, version, content, says, entry) in cases {
        let dir = tempfile::tempdir()?;
        let definitions = dir.path().join("definitions");
        std::fs::create_dir_all(&definitions)?;
        match version {
            Some(version) => write(&definitions, name, version, content)?,
            None => std::fs::write(definitions.join(name), content)?,
        }
        let refused = refused_start(dir.path(), &definitions)?;
        let StateError::StoredQueries {
            backend,
            source: StoreError::Corrupt(cause),
            ..
        } = &refused
        else {
            panic!("a corrupt store: {refused:?}");
        };
        assert_eq!(Backend::Files, *backend);
        assert!(cause.is::<FilesError>(), "{cause}");
        let line = chain(&refused);
        assert!(line.contains(says), "{name}/{version:?}: {line}");
        assert!(line.contains(entry), "names the entry: {line}");
        for quoted in [SENTINEL, PATIENT] {
            assert!(!line.contains(quoted), "never the content: {line}");
            assert!(!format!("{refused:?}").contains(quoted), "nor in Debug");
        }
    }
    Ok(())
}

#[test]
fn a_file_with_another_extension_or_a_nested_directory_refuses_the_start() -> TestResult {
    for (entry, directory) in [("1.0.0.txt", false), ("1.0.0.aql", true)] {
        let dir = tempfile::tempdir()?;
        let definitions = dir.path().join("definitions");
        let named = definitions.join(NAME);
        std::fs::create_dir_all(&named)?;
        if directory {
            std::fs::create_dir_all(named.join(entry))?;
        } else {
            std::fs::write(named.join(entry), parameterised())?;
        }
        let line = chain(&refused_start(dir.path(), &definitions)?);
        assert!(line.contains("is not a definition file"), "{entry}: {line}");
    }
    Ok(())
}

#[test]
fn a_missing_definition_directory_refuses_the_start() -> TestResult {
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("absent");
    let line = chain(&refused_start(dir.path(), &missing)?);
    assert!(line.contains("could not be read"), "{line}");
    assert!(line.contains(&missing.display().to_string()), "{line}");
    Ok(())
}

#[test]
fn an_empty_definition_directory_offers_an_empty_registry() -> TestResult {
    let dir = tempfile::tempdir()?;
    let definitions = dir.path().join("definitions");
    std::fs::create_dir_all(&definitions)?;
    let settings = settings(
        dir.path(),
        ("http://a.invalid", "http://b.invalid"),
        &definitions,
        "",
    )?;
    AppState::check(&settings)?;
    let state = AppState::build(&settings)?;
    let held = state.definitions().ok_or("the registry is offered")?;
    assert!(held.is_empty());
    assert!(held.is_read_only());
    Ok(())
}

// conformance: CP-40
#[tokio::test]
async fn a_held_version_put_naming_members_at_a_read_only_registry_is_a_405() -> TestResult {
    let a = Server::start().await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let definitions = dir.path().join("definitions");
    write(&definitions, NAME, "1.0.0", parameterised().as_bytes())?;
    let app = gateway(dir.path(), (&a, &b), &definitions)?;
    for target in ["*", "node-a-pub"] {
        let request = Request::put(format!("/v1/definition/query/{NAME}/1.0.0"))
            .header(header::CONTENT_TYPE, "text/plain")
            .header("openEHR-federation-endpoint", target)
            .body(Body::from(parameterised()))?;
        let (status, headers, body) = exchange(app.clone(), request).await?;
        let text = String::from_utf8(body)?;
        assert_eq!(
            StatusCode::METHOD_NOT_ALLOWED,
            status,
            "§12.7: a read-only registry sends nothing again: {target}: {text}"
        );
        assert_eq!("stored-query-read-only", error_body(&text)?.code);
        assert_eq!(
            Some("GET, OPTIONS"),
            headers.get(header::ALLOW).and_then(|v| v.to_str().ok()),
            "RFC 9110 §15.5.6: {target}"
        );
    }
    for node in [&a, &b] {
        let requests = node.received_requests().await.ok_or("recording is on")?;
        assert!(requests.is_empty(), "no node is asked");
    }
    Ok(())
}

// conformance: CP-40
#[tokio::test]
async fn the_admin_distribution_at_a_read_only_registry_is_a_405_allowing_nothing() -> TestResult {
    let a = Server::start().await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let definitions = dir.path().join("definitions");
    write(&definitions, NAME, "1.0.0", parameterised().as_bytes())?;
    let settings = settings(dir.path(), (&a.uri(), &b.uri()), &definitions, "")?;
    let operator = ferrofed_server::admin::router(Arc::new(AppState::build(&settings)?));
    for target in ["*", "node-a-pub"] {
        let request = Request::post(format!("/admin/stored-queries/{NAME}/1.0.0/distribute"))
            .header("openEHR-federation-endpoint", target)
            .body(Body::empty())?;
        let (status, headers, body) = exchange(operator.clone(), request).await?;
        let text = String::from_utf8(body)?;
        assert_eq!(
            StatusCode::METHOD_NOT_ALLOWED,
            status,
            "a read-only registry distributes nothing: {target}: {text}"
        );
        assert_eq!("stored-query-read-only", error_body(&text)?.code);
        assert_eq!(
            Some(""),
            headers.get(header::ALLOW).and_then(|v| v.to_str().ok()),
            "RFC 9110 §10.2.1: no method is allowed here: {target}"
        );
    }
    for node in [&a, &b] {
        let requests = node.received_requests().await.ok_or("recording is on")?;
        assert!(requests.is_empty(), "no node is asked");
    }
    Ok(())
}

#[test]
fn a_read_only_registry_refuses_to_distribute_definitions() -> TestResult {
    let dir = tempfile::tempdir()?;
    let refused = settings(
        dir.path(),
        ("http://a.invalid", "http://b.invalid"),
        &dir.path().join("definitions"),
        "fan_out_stored_queries = true",
    )
    .err()
    .ok_or("refused")?;
    let refused = refused
        .downcast_ref::<ConfigError>()
        .ok_or("a configuration error")?;
    assert!(
        matches!(refused, ConfigError::StoredQueryFanOutReadOnly),
        "§12.7: {refused:?}"
    );
    Ok(())
}

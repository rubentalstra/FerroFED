// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The surface at the deployment's base URL, `/` or a configured prefix, with
//! no prefix mandated or reserved (§4.1, §7a, N28, CP-21, track 9).
//!
//! Each case runs the same request at the root and under `/fed/openehr`,
//! a gateway configured through `[server] base_path`: a routed read, a
//! routed write, `OPTIONS {base}/` and on a sub-path, and the federated
//! query. Every node is asked at its own base, so the gateway's base never
//! reaches a node, and every `Location` the node writes passes through
//! unmodified (N31).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::settings::Settings;
use ferrofed_server::config::{Config, error};
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Method, Request, StatusCode, header};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{
    Answer, EHR_A, EHR_B, NAMESPACE, PATIENT, body, crossref, node_answering, patient_query,
    registry, schema, settings_with_room, wire,
};
use crate::support::{call, error_body, send};

type TestResult = Result<(), Box<dyn Error>>;

/// The two bases every case runs at: the root and a prefix of the
/// deployment's choosing.
const BASES: [&str; 2] = ["/", "/fed/openehr"];

/// A version uid node A minted, as its `ETag` and `Location` name it.
const VERSION_A: &str = "8849182c-82ad-4088-a07f-48ead4180515::cdr-a.example.org::1";

/// `path` under `base`.
pub(crate) fn under(base: &str, path: &str) -> String {
    if base == "/" {
        path.to_owned()
    } else {
        format!("{base}{path}")
    }
}

/// The development gateway over node A at `a` and node B at `b`, mounted at
/// `base` through `[server] base_path`, resolving the patient at both.
pub(crate) fn gateway_at(
    dir: &std::path::Path,
    base: &str,
    a: &str,
    b: &str,
) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(a, b, ""))?;
    let document = toml::Value::String(document.display().to_string());
    let rows = crossref(&[("node-a", EHR_A), ("node-b", EHR_B)]);
    let text = format!(
        "profile = \"development\"\n\n[server]\nbase_path = \"{base}\"\nrequest_timeout_ms = 10000\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{rows}"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    let mut server = settings_with_room();
    server.base_path = settings.server.base_path.clone();
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &server,
    ))
}

/// A request of `verb` to `uri` naming node A as its target.
fn to_a(verb: Method, uri: &str, body: Body) -> Result<Request<Body>, http::Error> {
    Request::builder()
        .method(verb)
        .uri(uri)
        .header("openEHR-federation-endpoint", "node-a-pub")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
}

/// The paths every request `server` received was sent to.
async fn paths(server: &Server) -> Result<Vec<String>, Box<dyn Error>> {
    let requests = server.received_requests().await.ok_or("recording is on")?;
    Ok(requests
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect())
}

// conformance: CP-21
#[tokio::test]
async fn a_routed_read_is_served_at_the_root_and_under_a_prefix() -> TestResult {
    for base in BASES {
        let a = Server::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/v1/ehr/{EHR_A}")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"ehr_id":{"value":"synthetic"}}"#.to_vec(),
                "application/json",
            ))
            .mount(&a)
            .await;
        let b = Server::start().await;
        let dir = tempfile::tempdir()?;
        let app = gateway_at(dir.path(), base, &a.uri(), &b.uri())?;
        let read = to_a(
            Method::GET,
            &under(base, &format!("/v1/ehr/{EHR_A}")),
            Body::empty(),
        )?;
        let (status, text) = call(app, read).await?;
        assert_eq!(StatusCode::OK, status, "{base}: {text}");
        assert_eq!(
            vec![format!("/v1/ehr/{EHR_A}")],
            paths(&a).await?,
            "{base}: the node is asked at its own base, never the gateway's (N28)"
        );
        assert!(paths(&b).await?.is_empty(), "{base}: node B is never asked");
    }
    Ok(())
}

// conformance: CP-21
#[tokio::test]
async fn a_read_by_subject_is_served_at_the_root_and_under_a_prefix() -> TestResult {
    for base in BASES {
        let a = Server::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/v1/ehr/{EHR_A}")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                br#"{"ehr_id":{"value":"synthetic"}}"#.to_vec(),
                "application/json",
            ))
            .mount(&a)
            .await;
        let b = Server::start().await;
        let dir = tempfile::tempdir()?;
        let app = gateway_at(dir.path(), base, &a.uri(), &b.uri())?;
        let uri = under(
            base,
            &format!("/v1/ehr?subject_id={PATIENT}&subject_namespace={NAMESPACE}"),
        );
        let read = to_a(Method::GET, &uri, Body::empty())?;
        let (status, text) = call(app, read).await?;
        assert_eq!(StatusCode::OK, status, "{base}: {text}");
        assert_eq!(
            vec![format!("/v1/ehr/{EHR_A}")],
            paths(&a).await?,
            "{base}: the node is asked at its own base by its own ehr_id (N28, N33)"
        );
        assert!(!wire(&a).await?.contains(PATIENT), "{base}: N33");
    }
    Ok(())
}

// conformance: CP-21
#[tokio::test]
async fn a_routed_write_is_served_at_the_root_and_under_a_prefix_with_location_untouched()
-> TestResult {
    for base in BASES {
        let a = Server::start().await;
        let location = format!("http://cdr-a.example.org/v1/ehr/{EHR_A}/composition/{VERSION_A}");
        Mock::given(method("POST"))
            .and(path(format!("/v1/ehr/{EHR_A}/composition")))
            .respond_with(
                ResponseTemplate::new(201)
                    .insert_header("Location", location.as_str())
                    .insert_header("ETag", format!("\"{VERSION_A}\"").as_str()),
            )
            .mount(&a)
            .await;
        let b = Server::start().await;
        let dir = tempfile::tempdir()?;
        let app = gateway_at(dir.path(), base, &a.uri(), &b.uri())?;
        let write = to_a(
            Method::POST,
            &under(base, &format!("/v1/ehr/{EHR_A}/composition")),
            Body::from(r#"{"_type":"COMPOSITION"}"#),
        )?;
        let response = send(app, write).await?;
        assert_eq!(StatusCode::CREATED, response.status(), "{base}");
        assert_eq!(
            Some(location.as_str()),
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok()),
            "{base}: the node's Location, unmodified (N31)"
        );
        assert_eq!(
            vec![format!("/v1/ehr/{EHR_A}/composition")],
            paths(&a).await?,
            "{base}: the write reaches the node at its own base (N28)"
        );
    }
    Ok(())
}

// conformance: CP-21
#[tokio::test]
async fn options_describes_the_surface_at_the_root_and_under_a_prefix() -> TestResult {
    for base in BASES {
        let dir = tempfile::tempdir()?;
        let app = gateway_at(
            dir.path(),
            base,
            "http://127.0.0.1:9/a",
            "http://127.0.0.1:9/b",
        )?;
        let roots = if base == "/" {
            vec!["/".to_owned()]
        } else {
            vec![format!("{base}/"), base.to_owned()]
        };
        for root in roots {
            let request = Request::builder()
                .method(Method::OPTIONS)
                .uri(root.as_str())
                .body(Body::empty())?;
            let (status, text) = call(app.clone(), request).await?;
            assert_eq!(StatusCode::OK, status, "OPTIONS {root}: {text}");
            schema::validate_options(&text)?;
        }
        let request = Request::builder()
            .method(Method::OPTIONS)
            .uri(under(base, &format!("/v1/ehr/{EHR_A}")))
            .body(Body::empty())?;
        let response = send(app, request).await?;
        assert_eq!(StatusCode::NO_CONTENT, response.status(), "{base}");
        assert!(
            response.headers().contains_key(header::ALLOW),
            "{base}: OPTIONS on a sub-path names the methods served there (§7a.2)"
        );
    }
    Ok(())
}

// conformance: CP-21
#[tokio::test]
async fn the_federated_query_is_served_at_the_root_and_under_a_prefix() -> TestResult {
    for base in BASES {
        let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
        let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
        let dir = tempfile::tempdir()?;
        let app = gateway_at(dir.path(), base, &a.uri(), &b.uri())?;
        let query = Request::post(under(base, "/v1/query/aql"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body(&patient_query())?))?;
        let (status, text) = call(app, query).await?;
        assert_eq!(StatusCode::OK, status, "{base}: {text}");
        schema::validate(&text)?;
        let answer: Answer = serde_json::from_str(&text)?;
        assert_eq!(2, answer.rows.len(), "{base}: one row from each node");
        for node in [&a, &b] {
            assert_eq!(
                vec!["/v1/query/aql".to_owned()],
                paths(node).await?,
                "{base}: each node is asked at its own base (N28)"
            );
        }
    }
    Ok(())
}

// conformance: CP-21
#[tokio::test]
async fn under_a_prefix_nothing_is_served_outside_it() -> TestResult {
    let dir = tempfile::tempdir()?;
    let app = gateway_at(
        dir.path(),
        "/fed/openehr",
        "http://127.0.0.1:9/a",
        "http://127.0.0.1:9/b",
    )?;
    for (verb, uri) in [
        (Method::POST, "/v1/query/aql".to_owned()),
        (Method::GET, format!("/v1/ehr/{EHR_A}")),
        (Method::OPTIONS, "/".to_owned()),
        (Method::GET, "/health".to_owned()),
        (Method::GET, "/fed/openehrx/v1/query/aql".to_owned()),
        (Method::GET, "/fed/v1/query/aql".to_owned()),
    ] {
        let request = Request::builder()
            .method(verb.clone())
            .uri(uri.as_str())
            .body(Body::empty())?;
        let (status, text) = call(app.clone(), request).await?;
        assert_eq!(StatusCode::NOT_FOUND, status, "{verb} {uri}");
        assert_eq!("not-found", error_body(&text)?.code, "{verb} {uri}");
    }
    let request = Request::get("/fed/openehr/health").body(Body::empty())?;
    let (status, _) = call(app, request).await?;
    assert_eq!(
        StatusCode::OK,
        status,
        "the health family sits under the base too"
    );
    Ok(())
}

// conformance: CP-21
#[tokio::test]
async fn no_prefix_is_reserved_at_the_root() -> TestResult {
    let dir = tempfile::tempdir()?;
    let app = gateway_at(
        dir.path(),
        "/",
        "http://127.0.0.1:9/a",
        "http://127.0.0.1:9/b",
    )?;
    let request = Request::post("/rest/openehr/v1/query/aql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body(&patient_query())?))?;
    let (status, text) = call(app, request).await?;
    assert_eq!(
        StatusCode::NOT_FOUND,
        status,
        "/rest/openehr is a vendor convention, not part of the surface (N28): {text}"
    );
    Ok(())
}

/// Resolves the configuration `text` with the environment `env`.
fn resolved(text: &str, env: &[(&str, &str)]) -> Result<Settings, error::Error> {
    let env = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    Config::from_sources(Some(&crate::support::signed(text)), &env)?.resolve()
}

// conformance: CP-21
#[test]
fn the_base_path_is_the_root_by_default_and_the_deployment_chooses_any_other() -> TestResult {
    assert!(
        resolved("", &[])?.server.base_path.is_root(),
        "the default is /"
    );
    let file = resolved("[server]\nbase_path = \"/rest/openehr\"\n", &[])?;
    assert_eq!(
        "/rest/openehr",
        file.server.base_path.as_str(),
        "N28 reserves no prefix and forbids none a deployment chooses"
    );
    let env = resolved("", &[("FERROFED__SERVER__BASE_PATH", "/fed/openehr")])?;
    assert_eq!("/fed/openehr", env.server.base_path.as_str());
    Ok(())
}

#[test]
fn a_malformed_base_path_refuses_to_boot_naming_its_key() {
    for base in [
        "",
        "fed",
        "/fed/",
        "/fed?x=1",
        "/fed#top",
        "//fed",
        "/fed/../x",
    ] {
        let refused = resolved(&format!("[server]\nbase_path = \"{base}\"\n"), &[]);
        assert!(
            matches!(
                &refused,
                Err(error::Error::BasePath { key, .. }) if key == "server.base_path"
            ),
            "{base:?}: {refused:?}"
        );
    }
}

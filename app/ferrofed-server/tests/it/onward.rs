// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The onward grant through the real configuration path: the gateway
//! publishes its JWK Set at `{base}/.well-known/jwks.json`, declares it as
//! `federation.auth.jwks_uri` in `OPTIONS {base}/`, obtains a token at each
//! node's token endpoint with a client assertion that endpoint verifies
//! against the published set, and sends the node that token and never the
//! caller's (§13.1, N25, N30, CP-17 onward half; RFC 6749 §4.4, RFC 7523
//! §2.2, RFC 7517).
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
use ferrofed_engine::dispatch::reported::UNAUTHENTICATED;
use ferrofed_server::config::Config;
use ferrofed_server::config::error::Error as ConfigError;
use ferrofed_server::federation::{Federation, FederationError};
use ferrofed_server::state::AppState;
use ferrofed_server::telemetry::{Rendering, subscriber};
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::oauth::{self, TokenEndpoint, Verdict};
use ferrofed_testkit::unreachable;
use http::{Request, StatusCode, header};
use jsonwebtoken::jwk::JwkSet;
use openehr_federation::options::OptionsRoot;
use openehr_federation::outcome::ErrorDetail;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::facade::{
    Answer, PATIENT, body, crossref, patient_query, post, registry, schema, settings_with_room,
    statuses,
};
use crate::support::{CLIENT_TOKEN, Logs, call};

type TestResult = Result<(), Box<dyn Error>>;

/// The client node A's authorization server registered the gateway as.
const CLIENT_ID: &str = "ferrofed-test-gateway";

/// The scope the gateway requests onward.
const SCOPE: &str = "system/aql-*.s";

/// The JWK Set location the gateway declares.
const JWKS_URI: &str = "https://gw.example.org/.well-known/jwks.json";

/// A one-row answer from a node.
const ONE_ROW: &str =
    r##"{"q":"node","columns":[{"name":"#0","path":"c/uid/value"}],"rows":[["uid-at-a"]]}"##;

/// A key file holding a fresh ES384 key, in `dir` under `name`.
fn key_file(dir: &Path, name: &str) -> Result<toml::Value, Box<dyn Error>> {
    let file = dir.join(name);
    std::fs::write(&file, oauth::es384_pem()?)?;
    Ok(toml::Value::String(file.display().to_string()))
}

/// The `[signing]` table with a current key, and a previous one when
/// `rotating`.
fn signing(dir: &Path, rotating: bool) -> Result<String, Box<dyn Error>> {
    let current = key_file(dir, "current.pem")?;
    let previous = if rotating {
        format!("previous_key_file = {}\n", key_file(dir, "previous.pem")?)
    } else {
        String::new()
    };
    Ok(format!(
        "[signing]\nkey_file = {current}\n{previous}jwks_uri = \"{JWKS_URI}\"\n"
    ))
}

/// The `[credentials]` table of node A's OAuth 2.0 grant at `token_url`.
fn oauth2(token_url: &str) -> String {
    format!(
        "[credentials.\"node-a-pub\".oauth2]\ngrant = \"client_credentials\"\nclient_auth = \"private_key_jwt\"\ntoken_endpoint = \"{token_url}\"\nclient_id = \"{CLIENT_ID}\"\nscope = \"{SCOPE}\"\n"
    )
}

/// The gateway over node A at `a` and node B at `b`, the patient resolving
/// at node A, with `tables` after the federation.
fn gateway(dir: &Path, a: &str, b: &str, tables: &str) -> Result<Router, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(a, b, ""))?;
    let document = toml::Value::String(document.display().to_string());
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\nbest_effort = false\n\n{}\n{tables}",
        crossref(&[("node-a", "2222aaaa-2222-4222-8222-222222222222")])
    );
    let settings = Config::from_sources(Some(&text), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    Ok(ferrofed_server::router(
        Arc::new(AppState::with_federation(federation)),
        &settings_with_room(),
    ))
}

/// A node that answers the query to a request carrying a token `endpoint`
/// issued, and `401` to every other.
async fn node_requiring(endpoint: &TokenEndpoint) -> Server {
    let server = Server::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .and(endpoint.bearer())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(ONE_ROW.as_bytes().to_vec(), "application/json"),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(2)
        .mount(&server)
        .await;
    server
}

/// The JWK Set the gateway serves.
async fn published(app: &Router) -> Result<JwkSet, Box<dyn Error>> {
    let request = Request::get("/.well-known/jwks.json").body(Body::empty())?;
    let response = tower::ServiceExt::oneshot(app.clone(), request).await?;
    assert_eq!(StatusCode::OK, response.status(), "the JWK Set is served");
    assert_eq!(
        Some("application/jwk-set+json"),
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        "RFC 7517 §8.5.1"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The patient query, carrying the caller's own `Authorization`.
fn patient_post() -> Result<Request<Body>, Box<dyn Error>> {
    let mut request = post(body(&patient_query())?)?;
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {}", *CLIENT_TOKEN).parse()?,
    );
    Ok(request)
}

/// The node reached with the token its token endpoint issued against the
/// published JWK Set, and the caller's own token reaches no node and no
/// token endpoint (§13.1, N25; RFC 9700 §2.3).
// conformance: CP-17
#[tokio::test]
async fn the_node_receives_the_token_its_endpoint_issued_against_the_published_jwks() -> TestResult
{
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    endpoint.expect_scope(SCOPE);
    let a = node_requiring(&endpoint).await;
    let dir = tempfile::tempdir()?;
    let tables = format!(
        "{}\n{}",
        signing(dir.path(), false)?,
        oauth2(&endpoint.token_url())
    );
    let app = gateway(dir.path(), &a.uri(), unreachable::BASE, &tables)?;
    endpoint.trust(published(&app).await?);

    let (status, text) = call(app, patient_post()?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    assert_eq!(vec![Verdict::Issued], endpoint.verdicts());
    let requests = a.received_requests().await.ok_or("recording is on")?;
    let [request] = requests.as_slice() else {
        return Err(format!("expected one node request, got {}", requests.len()).into());
    };
    let sent = request
        .headers
        .get(header::AUTHORIZATION)
        .ok_or("the node receives a credential")?
        .to_str()?;
    assert!(sent.starts_with("Bearer "), "{sent}");
    assert!(
        !sent.contains(CLIENT_TOKEN.as_str()),
        "the caller's token is never forwarded"
    );
    for form in endpoint.forms() {
        for (name, value) in form {
            assert!(
                !value.contains(CLIENT_TOKEN.as_str()),
                "{name} carries the caller's token"
            );
            assert!(!value.contains(PATIENT), "{name} carries the patient (N33)");
        }
    }
    Ok(())
}

/// `OPTIONS {base}/` declares the configured JWK Set location, and the body
/// still validates against the vendored schema (§13.1, §7a.2, N30).
// conformance: CP-17 CP-23
#[tokio::test]
async fn options_declares_the_jwks_uri_and_validates_against_the_schema() -> TestResult {
    let dir = tempfile::tempdir()?;
    let tables = format!(
        "{}\n{}",
        signing(dir.path(), false)?,
        oauth2("https://idp.example.org/token")
    );
    let app = gateway(
        dir.path(),
        "http://127.0.0.1:9/a",
        "http://127.0.0.1:9/b",
        &tables,
    )?;
    let (status, text) = call(app, Request::options("/").body(Body::empty())?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    schema::validate_options(&text)?;
    let body: OptionsRoot = serde_json::from_str(&text)?;
    let auth = body.federation.auth.ok_or("auth is declared")?;
    assert_eq!(
        Some(JWKS_URI),
        auth.jwks_uri
            .as_ref()
            .map(openehr_federation::object::Uri::as_str)
    );
    Ok(())
}

/// A gateway with no signing keys cannot sign the caller's identity for a
/// node, so it federates nothing (§13.1, N24, N25), and it publishes no
/// set.
#[tokio::test]
async fn a_gateway_without_keys_federates_nothing_and_publishes_no_jwks() -> TestResult {
    let dir = tempfile::tempdir()?;
    let unsigned = gateway(
        dir.path(),
        "http://127.0.0.1:9/a",
        "http://127.0.0.1:9/b",
        "",
    );
    let Err(refused) = unsigned else {
        return Err("a federating gateway without [signing] was built".into());
    };
    assert!(
        matches!(
            refused.downcast_ref::<FederationError>(),
            Some(FederationError::Unsigned)
        ),
        "{refused}"
    );
    let app = ferrofed_server::router(Arc::new(AppState::default()), &settings_with_room());
    let (status, _) = call(
        app,
        Request::get("/.well-known/jwks.json").body(Body::empty())?,
    )
    .await?;
    assert_eq!(StatusCode::NOT_FOUND, status);
    Ok(())
}

/// During a rotation the JWK Set publishes the current and the previous
/// key, each by its RFC 7638 `kid`, with no private member (RFC 7517 §5).
// conformance: CP-17
#[tokio::test]
async fn a_rotation_publishes_the_current_and_the_previous_key() -> TestResult {
    let dir = tempfile::tempdir()?;
    let app = gateway(
        dir.path(),
        "http://127.0.0.1:9/a",
        "http://127.0.0.1:9/b",
        &signing(dir.path(), true)?,
    )?;
    let set = published(&app).await?;
    assert_eq!(2, set.keys.len());
    let kids: Vec<Option<&str>> = set
        .keys
        .iter()
        .map(|jwk| jwk.common.key_id.as_deref())
        .collect();
    assert!(kids.iter().all(Option::is_some));
    assert_ne!(kids.first(), kids.get(1));
    let (_, text) = call(
        app,
        Request::get("/.well-known/jwks.json").body(Body::empty())?,
    )
    .await?;
    assert!(!text.contains("\"d\""), "no private key member: {text}");
    Ok(())
}

/// The JWK Set is served without any client authentication, as a node
/// fetches it (RFC 7517 §5, §13.1).
#[tokio::test]
async fn the_jwks_route_needs_no_client_authentication() -> TestResult {
    let dir = tempfile::tempdir()?;
    let app = gateway(
        dir.path(),
        "http://127.0.0.1:9/a",
        "http://127.0.0.1:9/b",
        &signing(dir.path(), false)?,
    )?;
    assert_eq!(1, published(&app).await?.keys.len());
    Ok(())
}

/// A token endpoint that cannot be reached fails node A as `node-error`:
/// all-or-nothing answers `424`, node A is sent nothing, and its `error`
/// names no address (§13.1, N25, §11.4, N37).
// conformance: CP-17
#[tokio::test]
async fn a_token_endpoint_down_fails_the_node_and_sends_it_nothing() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    let a = node_requiring(&endpoint).await;
    let b = node_requiring(&endpoint).await;
    let dir = tempfile::tempdir()?;
    let down = format!("{}{}", unreachable::BASE, oauth::TOKEN_PATH);
    let tables = format!("{}\n{}", signing(dir.path(), false)?, oauth2(&down));
    let app = gateway(dir.path(), &a.uri(), &b.uri(), &tables)?;

    let (status, text) = call(
        app,
        post(body(
            "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c",
        )?)?,
    )
    .await?;
    assert_eq!(StatusCode::FAILED_DEPENDENCY, status, "{text}");
    let answer: Answer = serde_json::from_str(&text)?;
    assert!(
        statuses(&answer).contains(&("node-a-pub", "node-error")),
        "{text}"
    );
    assert!(
        a.received_requests()
            .await
            .ok_or("recording is on")?
            .is_empty(),
        "never an unauthenticated dispatch"
    );
    let error = answer
        .meta
        .federation
        .endpoints
        .iter()
        .find(|endpoint| endpoint.id == "node-a-pub")
        .and_then(|endpoint| endpoint.error.clone());
    assert_eq!(
        Some(ErrorDetail::Text(UNAUTHENTICATED.to_owned())),
        error,
        "the fixed sentence, naming no address: {text}"
    );
    Ok(())
}

/// The token endpoint's `error_description` reaches the log and never the
/// answer, and neither carries the client assertion, the token or a key
/// path (§13.1, N33).
// conformance: CP-17
#[tokio::test]
async fn a_refusal_description_reaches_the_log_and_never_the_answer() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    endpoint.refuse(
        400,
        "invalid_client",
        "key /etc/ferrofed/secret.pem rejected at https://idp.internal.example/realm",
    );
    let a = node_requiring(&endpoint).await;
    let dir = tempfile::tempdir()?;
    let tables = format!(
        "{}\n{}",
        signing(dir.path(), false)?,
        oauth2(&endpoint.token_url())
    );
    let app = gateway(dir.path(), &a.uri(), unreachable::BASE, &tables)?;

    let logs = Logs::default();
    let capture = subscriber(Rendering::Json, "warn", false, logs.clone())?;
    let guard = tracing::subscriber::set_default(capture);
    let (status, text) = call(app, patient_post()?).await?;
    drop(guard);
    assert_eq!(StatusCode::FAILED_DEPENDENCY, status, "{text}");
    for hidden in ["/etc/ferrofed", "idp.internal.example", CLIENT_ID] {
        assert!(
            !text.contains(hidden),
            "{hidden} reaches the answer: {text}"
        );
    }
    assert!(text.contains("invalid_client"), "{text}");
    let logged = logs.text();
    assert!(
        logged.contains("idp.internal.example"),
        "the log keeps the account: {logged}"
    );
    assert!(logged.contains("node-a-pub"), "{logged}");
    for assertion in endpoint.assertions() {
        assert!(
            !logged.contains(&assertion),
            "the assertion is never logged"
        );
    }
    assert!(!logged.contains("BEGIN PRIVATE KEY"));
    assert!(!logged.contains(PATIENT), "N33: {logged}");
    Ok(())
}

/// `config check` refuses an OAuth 2.0 grant it cannot use, naming the key:
/// a P-256 key, a missing `client_id`, a missing `[signing]`, a scope
/// outside the `system` grammar, a lifetime past 300 s, an overlap shorter
/// than the lifetime plus the nodes' cache time, a token endpoint with a
/// query, and a `jwks_uri` that is no URL.
#[test]
fn the_configuration_refuses_a_grant_it_cannot_use() -> TestResult {
    let dir = tempfile::tempdir()?;
    let good_key = key_file(dir.path(), "good.pem")?;
    let p256 = dir.path().join("p256.pem");
    std::fs::write(&p256, oauth::p256_pem()?)?;
    let p256 = toml::Value::String(p256.display().to_string());
    let grant = oauth2("https://idp.example.org/token");
    let signing_with = |extra: &str, key: &toml::Value| {
        format!("[signing]\nkey_file = {key}\njwks_uri = \"{JWKS_URI}\"\n{extra}\n")
    };
    let resolve = |text: &str| -> Result<ConfigError, Box<dyn Error>> {
        match Config::from_sources(Some(text), &BTreeMap::new())?.resolve() {
            Ok(_) => Err(format!("accepted: {text}").into()),
            Err(error) => Ok(error),
        }
    };

    let error = resolve(&format!("{}{grant}", signing_with("", &p256)))?;
    assert!(
        matches!(&error, ConfigError::SigningKey { key, .. } if key == "signing.key_file"),
        "{error:?}"
    );
    let error = resolve(&format!(
        "{}{}",
        signing_with("", &good_key),
        grant.replace(&format!("client_id = \"{CLIENT_ID}\"\n"), "")
    ))?;
    assert!(
        matches!(&error, ConfigError::Missing { key } if key == "credentials.node-a-pub.oauth2.client_id"),
        "{error:?}"
    );
    let error = resolve(&grant)?;
    assert!(
        matches!(error, ConfigError::GrantWithoutSigning { .. }),
        "{error:?}"
    );
    let error = resolve(&format!(
        "{}{}",
        signing_with("", &good_key),
        grant.replace(SCOPE, "patient/aql-*.s")
    ))?;
    assert!(matches!(error, ConfigError::Scope { .. }), "{error:?}");
    let error = resolve(&signing_with("assertion_lifetime_s = 301", &good_key))?;
    assert!(
        matches!(error, ConfigError::AssertionLifetime { .. }),
        "{error:?}"
    );
    let error = resolve(&signing_with("rotation_overlap_s = 3899", &good_key))?;
    assert!(
        matches!(error, ConfigError::RotationOverlap { .. }),
        "{error:?}"
    );
    let error = resolve(&format!(
        "{}{}",
        signing_with("", &good_key),
        oauth2("https://idp.example.org/token?secret=x")
    ))?;
    assert!(matches!(error, ConfigError::Grant { .. }), "{error:?}");
    let error = resolve(&format!(
        "[signing]\nkey_file = {good_key}\njwks_uri = \"jwks.json\"\n"
    ))?;
    assert!(matches!(error, ConfigError::Url { .. }), "{error:?}");
    Ok(())
}

/// The binary's `config check` refuses a key file that holds no ES384 key,
/// naming the key and quoting no part of the file.
#[test]
fn config_check_refuses_a_bad_key_naming_its_key() -> TestResult {
    let dir = tempfile::tempdir()?;
    let bad = dir.path().join("bad.pem");
    std::fs::write(&bad, "Qz7-not-a-key\n")?;
    let bad = toml::Value::String(bad.display().to_string());
    let toml = format!("[signing]\nkey_file = {bad}\njwks_uri = \"{JWKS_URI}\"\n");
    let output = crate::run::binary(&["config", "check"], &toml)?;
    assert_eq!(
        Some(i32::from(ferrofed_server::EXIT_CONFIG)),
        output.status.code()
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("signing.key_file"), "{stderr}");
    assert!(!stderr.contains("Qz7"), "{stderr}");
    Ok(())
}

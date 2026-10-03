// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The caller's identity on every request the gateway sends a node, through
//! the real configuration path: a query, a routed read and write, a
//! definition request and the ask-all probe each reach their node with one
//! `openEHR-federation-client` token, signed for that node with the key the
//! gateway publishes, naming the caller its guard verified and never the
//! caller's own token (§13.1, N24, N25, §12.4, CP-16, CP-17). A caller claim
//! that carries the patient identifier stops the request before it is sent
//! (§5.4.1, N33, CP-26), and a request with no verified caller reaches no
//! node.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use ferrofed_engine::onward::conveyance::{HEADER, LIFETIME, TYPE};
use ferrofed_engine::onward::keys::ALGORITHM;
use ferrofed_server::config::Config;
use ferrofed_server::config::auth::{
    AuthMode, AuthSettings, IssuerSettings, KeySource, Verification,
};
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::issuer::{ACT_REASON, Claims, EVERY_SCOPE, Issuer};
use ferrofed_testkit::mock::Server;
use http::{HeaderMap, HeaderName, Request, StatusCode, header};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, Validation};
use openehr_federation::headers::ENDPOINT;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::auth::{Gateway, bearing, minted, query};
use crate::facade::{
    EHR_A, EHR_B, PATIENT, body, crossref, dev_gateway, patient_query, post, registry, wire,
};
use crate::support::{
    AUDIENCE, CLIENT_TOKEN, Conveyed, ConveyedPurpose, ISSUER, call, error_body, send,
};

type TestResult = Result<(), Box<dyn Error>>;

/// The federation every test gateway names itself by, the `iss` of each
/// token.
const FEDERATION: &str = "example-federation";

/// The default caller of the suite's tokens.
const CALLER: &str = "synthetic-caller";

/// The organisation of the suite's default token.
const ORGANISATION: &str = "urn:oid:2.999.7";

/// An `ehr_id` no member is known to hold, which the gateway probes for.
const UNKNOWN_EHR: &str = "3333cccc-3333-4333-8333-333333333333";

/// The JWK Set `app` publishes.
async fn published(app: &Router) -> Result<JwkSet, Box<dyn Error>> {
    let request = Request::get("/.well-known/jwks.json").body(Body::empty())?;
    let (status, text) = call(app.clone(), request).await?;
    if status != StatusCode::OK {
        return Err(format!("the JWK Set answered {status}: {text}").into());
    }
    Ok(serde_json::from_str(&text)?)
}

/// The one conveyed token of every request `server` received, in order; a
/// request with none, or with more than one, fails the read.
async fn tokens(server: &Server) -> Result<Vec<String>, Box<dyn Error>> {
    let requests = server.received_requests().await.ok_or("recording is on")?;
    let mut tokens = Vec::new();
    for request in requests {
        let values: Vec<_> = request.headers.get_all(HEADER).iter().collect();
        let [only] = values.as_slice() else {
            return Err(format!("{} carries {} {HEADER} values", request.url, values.len()).into());
        };
        tokens.push(only.to_str()?.to_owned());
    }
    Ok(tokens)
}

/// `token` verified as a node verifies it: its type and algorithm, its
/// signature against `keys` by `kid`, `iss` the federation, `aud`
/// `audience`, and `exp`.
fn verified(token: &str, keys: &JwkSet, audience: &str) -> Result<Conveyed, Box<dyn Error>> {
    let header = jsonwebtoken::decode_header(token)?;
    if header.typ.as_deref() != Some(TYPE) || header.alg != ALGORITHM {
        return Err(format!("typ {:?}, alg {:?}", header.typ, header.alg).into());
    }
    let kid = header.kid.ok_or("the token names its key")?;
    let jwk = keys.find(&kid).ok_or("the key is published")?;
    let mut validation = Validation::new(ALGORITHM);
    validation.set_issuer(&[FEDERATION]);
    validation.set_audience(&[audience]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    Ok(jsonwebtoken::decode::<Conveyed>(token, &DecodingKey::from_jwk(jwk)?, &validation)?.claims)
}

/// The tokens `server` received, each verified for `audience`.
async fn verified_at(
    server: &Server,
    keys: &JwkSet,
    audience: &str,
) -> Result<Vec<Conveyed>, Box<dyn Error>> {
    tokens(server)
        .await?
        .iter()
        .map(|token| verified(token, keys, audience))
        .collect()
}

/// Asserts that `read` names the suite's default caller, verified as
/// `verified_by` says, and lives at most a minute.
fn names_the_default_caller(read: &Conveyed, verified_by: &str) {
    assert_eq!(CALLER, read.sub, "sub is the verified caller");
    assert_eq!(Some(ISSUER), read.iss_upstream.as_deref(), "iss_upstream");
    assert_eq!(
        Some(verified_by),
        read.verified_by.as_deref(),
        "verified_by"
    );
    assert_eq!(
        Some(ORGANISATION),
        read.subject_organization_id.as_deref(),
        "the caller's organisation"
    );
    assert_eq!(
        vec![ConveyedPurpose {
            system: Some(ACT_REASON.to_owned()),
            code: "TREAT".to_owned(),
        }],
        read.purpose_of_use,
        "the purpose of use (§13.4)"
    );
    assert_eq!(
        Some(EVERY_SCOPE),
        read.scope.as_deref(),
        "the scopes as granted (N26)"
    );
    let lifetime = read.exp.saturating_sub(read.iat);
    assert!(
        lifetime > 0 && lifetime.unsigned_abs() <= LIFETIME.as_secs() && LIFETIME.as_secs() <= 60,
        "exp at most 60 s after iat: {lifetime}"
    );
}

// conformance: CP-16 CP-17
#[tokio::test]
async fn a_query_conveys_the_verified_caller_to_each_node_signed_for_it() -> TestResult {
    let gateway = Gateway::trusting_the_test_issuer().await?;
    let keys = published(&gateway.app).await?;
    let (status, text) = call(gateway.app.clone(), query()?).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let at_a = verified_at(&gateway.a, &keys, "node-a-pub").await?;
    let at_b = verified_at(&gateway.b, &keys, "node-b-pub").await?;
    let ([a], [b]) = (at_a.as_slice(), at_b.as_slice()) else {
        return Err("one query at each node".into());
    };
    for read in [a, b] {
        assert_eq!(FEDERATION, read.iss, "iss is the gateway");
        names_the_default_caller(read, "signature");
    }
    assert_eq!(
        ("node-a-pub", "node-b-pub"),
        (a.aud.as_str(), b.aud.as_str())
    );
    assert_ne!(a.jti, b.jti, "a fresh jti per token");
    for server in [&gateway.a, &gateway.b] {
        let captured = wire(server).await?;
        assert!(
            !captured.contains(CLIENT_TOKEN.as_str()) && !captured.contains(PATIENT),
            "neither the caller's token nor the patient reaches a node: {captured}"
        );
    }
    Ok(())
}

/// A node answering a routed read, a routed write and a definition list.
async fn routed_node() -> Server {
    let server = Server::start().await;
    for (verb, at, status) in [
        ("GET", format!("/v1/ehr/{EHR_A}"), 200),
        ("POST", format!("/v1/ehr/{EHR_A}/composition"), 201),
        ("GET", "/v1/definition/template/adl1.4".to_owned(), 200),
    ] {
        Mock::given(method(verb))
            .and(path(at))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
    }
    server
}

/// `request` naming node A as its target (§8.4).
fn to_a(mut request: Request<Body>) -> Result<Request<Body>, Box<dyn Error>> {
    request
        .headers_mut()
        .insert(ENDPOINT, "node-a-pub".parse()?);
    Ok(request)
}

// conformance: CP-16
#[tokio::test]
async fn a_routed_read_a_routed_write_and_a_definition_request_convey_the_caller() -> TestResult {
    let a = routed_node().await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let app = dev_gateway(dir.path(), &a.uri(), &b.uri(), &[("node-a", EHR_A)])?;
    let keys = published(&app).await?;
    let read = Request::get(format!("/v1/ehr/{EHR_A}")).body(Body::empty())?;
    let write = Request::post(format!("/v1/ehr/{EHR_A}/composition"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"_type":"COMPOSITION"}"#))?;
    let definition = Request::get("/v1/definition/template/adl1.4").body(Body::empty())?;
    for (request, expected) in [
        (read, StatusCode::OK),
        (write, StatusCode::CREATED),
        (definition, StatusCode::OK),
    ] {
        let answered = send(app.clone(), to_a(request)?).await?;
        assert_eq!(expected, answered.status());
    }
    let at_a = verified_at(&a, &keys, "node-a-pub").await?;
    assert_eq!(
        3,
        at_a.len(),
        "the read, the write and the definition request"
    );
    for read in &at_a {
        names_the_default_caller(read, "signature");
    }
    let captured = wire(&a).await?;
    assert!(
        !captured.contains(CLIENT_TOKEN.as_str()),
        "the caller's own token is never forwarded (§13.1): {captured}"
    );
    assert!(tokens(&b).await?.is_empty(), "node B is never asked");
    Ok(())
}

// conformance: CP-16
#[tokio::test]
async fn the_ask_all_probe_conveys_the_caller_to_every_member() -> TestResult {
    let at = format!("/v1/ehr/{UNKNOWN_EHR}");
    let a = Server::start().await;
    let b = Server::start().await;
    for (server, status) in [(&a, 200), (&b, 404)] {
        Mock::given(method("GET"))
            .and(path(at.clone()))
            .respond_with(ResponseTemplate::new(status))
            .mount(server)
            .await;
    }
    let dir = tempfile::tempdir()?;
    let app = dev_gateway(dir.path(), &a.uri(), &b.uri(), &[("node-a", EHR_A)])?;
    let keys = published(&app).await?;
    let answered = send(app, Request::get(at).body(Body::empty())?).await?;
    assert_eq!(StatusCode::OK, answered.status());
    for (server, audience) in [(&a, "node-a-pub"), (&b, "node-b-pub")] {
        let reads = verified_at(server, &keys, audience).await?;
        assert!(!reads.is_empty(), "{audience} was probed");
        for read in &reads {
            names_the_default_caller(read, "signature");
        }
    }
    Ok(())
}

/// The suite's default claims with `set` applied.
fn claims_with(set: fn(&mut Claims)) -> Claims {
    let mut claims = crate::auth::claims();
    set(&mut claims);
    claims
}

// conformance: CP-26
#[tokio::test]
async fn a_caller_claim_carrying_the_patient_stops_the_query_before_any_node() -> TestResult {
    let smuggling = [
        claims_with(|claims| PATIENT.clone_into(&mut claims.sub)),
        claims_with(|claims| {
            if let Some(extensions) = claims.extensions.as_mut() {
                extensions.ihe_iua.subject_organization_id = Some(format!("urn:x:{PATIENT}"));
            }
        }),
    ];
    for claims in smuggling {
        let gateway = Gateway::trusting_the_test_issuer().await?;
        let request = bearing(query()?, &minted(&claims)?)?;
        let (status, text) = call(gateway.app.clone(), request).await?;
        assert_eq!(StatusCode::INTERNAL_SERVER_ERROR, status, "{text}");
        assert!(
            !text.contains(PATIENT),
            "the answer never quotes it: {text}"
        );
        gateway.nobody_asked().await?;
    }
    Ok(())
}

/// The edge's header and issuer of [`an_edge_asserted_caller_is_conveyed_as_edge_asserted`].
const EDGE_HEADER: &str = "ferrofed-edge-assertion";
const EDGE: &str = "https://edge.example.test";

// conformance: CP-16
#[tokio::test]
async fn an_edge_asserted_caller_is_conveyed_as_edge_asserted() -> TestResult {
    let edge = Issuer::new(EDGE)?;
    let gateway = Gateway::with(AuthSettings {
        mode: AuthMode::Edge(HeaderName::from_static(EDGE_HEADER)),
        audience: Some(AUDIENCE.to_owned()),
        issuers: vec![IssuerSettings {
            issuer: EDGE.to_owned(),
            verification: Verification::KeySet(KeySource::Set(edge.jwks())),
            backend_clients: BTreeSet::new(),
            demographic_clients: BTreeSet::new(),
        }],
        ..AuthSettings::default()
    })
    .await?;
    let keys = published(&gateway.app).await?;
    let mut claims = Claims::new(EDGE, AUDIENCE);
    "synthetic-clinician-at-the-edge".clone_into(&mut claims.sub);
    let mut request = query()?;
    request
        .headers_mut()
        .insert(EDGE_HEADER, edge.mint(&claims)?.parse()?);
    let (status, text) = call(gateway.app.clone(), request).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    let at_a = verified_at(&gateway.a, &keys, "node-a-pub").await?;
    let [read] = at_a.as_slice() else {
        return Err("one query at node A".into());
    };
    assert_eq!("synthetic-clinician-at-the-edge", read.sub);
    assert_eq!(Some(EDGE), read.iss_upstream.as_deref());
    assert_eq!(
        Some("edge"),
        read.verified_by.as_deref(),
        "the claims record that the edge asserted the caller"
    );
    Ok(())
}

// conformance: CP-16
#[tokio::test]
async fn a_query_that_reaches_dispatch_with_no_verified_caller_reaches_no_node() -> TestResult {
    let a = Server::start().await;
    let b = Server::start().await;
    let dir = tempfile::tempdir()?;
    let document = dir.path().join("registry.toml");
    std::fs::write(&document, registry(&a.uri(), &b.uri(), ""))?;
    let document = toml::Value::String(document.display().to_string());
    let text = format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nnode_selection = \"ask-all\"\nid = \"{FEDERATION}\"\n{}",
        crossref(&[("node-a", EHR_A), ("node-b", EHR_B)])
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
    let state = Arc::new(AppState::with_federation(federation));
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/json".parse()?);
    let request = post(body(&patient_query())?)?;
    let sent = axum::body::to_bytes(request.into_body(), 64 * 1024).await?;
    let response =
        ferrofed_server::facade::query_aql(State(state), None, None, headers, sent).await;
    let status = response.status();
    let answer = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
    let text = String::from_utf8(answer.to_vec())?;
    assert_eq!(StatusCode::INTERNAL_SERVER_ERROR, status, "{text}");
    assert_eq!("internal", error_body(&text)?.code);
    assert!(tokens(&a).await?.is_empty() && tokens(&b).await?.is_empty());
    Ok(())
}

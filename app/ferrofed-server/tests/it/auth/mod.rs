// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Client authentication at the gateway: CP-17, inbound half (§13.1, N25),
//! `OPTIONS {base}/` behind it (§7a.2), the purpose of use (§13.4), and the
//! SMART on openEHR scopes (ITS-REST SMART on openEHR, master08).
//!
//! Every test here scores CP-17's inbound half: the client authenticates to
//! the gateway, and a request that fails reaches no node.
#![allow(
    clippy::panic_in_result_fn,
    reason = "a test asserts, and returns its setup errors"
)]

mod config;
mod edge;
mod introspection;
mod keys;
mod purpose;
mod scope;
mod token;

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_server::auth::Refusal;
use ferrofed_server::config::Config;
use ferrofed_server::config::auth::AuthSettings;
use ferrofed_server::federation::Federation;
use ferrofed_server::state::AppState;
use ferrofed_testkit::issuer::Claims;
use ferrofed_testkit::mock::Server;
use http::{HeaderMap, Request, StatusCode, header};
use tempfile::TempDir;

use crate::facade::{
    EHR_A, EHR_B, body, crossref, node_answering, patient_query, post, registry, settings_with_room,
};
use crate::support::{self, asked, error_body};

/// The test result.
pub(crate) type TestResult = Result<(), Box<dyn Error>>;

/// A development gateway over two answering nodes, with the `[auth]` a test
/// chose.
pub(crate) struct Gateway {
    /// The application.
    pub(crate) app: Router,
    /// Node A.
    pub(crate) a: Server,
    /// Node B.
    pub(crate) b: Server,
    /// Where the registry document is written.
    _dir: TempDir,
}

impl Gateway {
    /// The gateway over node A and node B, both holding the patient,
    /// authenticating its callers as `auth` says.
    pub(crate) async fn with(auth: AuthSettings) -> Result<Self, Box<dyn Error>> {
        let a = node_answering("8849182c-82ad-4088-a07f-48ead4180515::node-a::1").await;
        let b = node_answering("6cb19121-4307-4a29-9c1c-b6d6a2ab3b77::node-b::1").await;
        let dir = tempfile::tempdir()?;
        let document = dir.path().join("registry.toml");
        std::fs::write(&document, registry(&a.uri(), &b.uri(), ""))?;
        let document = toml::Value::String(document.display().to_string());
        let rows = crossref(&[("node-a", EHR_A), ("node-b", EHR_B)]);
        let text = format!(
            "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{rows}"
        );
        let settings =
            Config::from_sources(Some(&support::signed(&text)), &BTreeMap::new())?.resolve()?;
        let federation = Federation::load(&settings)?.ok_or("a registry is configured")?;
        let mut server = settings_with_room();
        server.auth = auth;
        let app = ferrofed_server::router(Arc::new(AppState::with_federation(federation)), &server);
        Ok(Self {
            app,
            a,
            b,
            _dir: dir,
        })
    }

    /// The gateway every other test drives: the suite's test issuer trusted.
    pub(crate) async fn trusting_the_test_issuer() -> Result<Self, Box<dyn Error>> {
        Self::with(support::auth()).await
    }

    /// Asserts that neither node was asked anything.
    pub(crate) async fn nobody_asked(&self) -> TestResult {
        assert!(asked(&self.a).await?.is_empty(), "node A received nothing");
        assert!(asked(&self.b).await?.is_empty(), "node B received nothing");
        Ok(())
    }
}

/// The federated patient query.
pub(crate) fn query() -> Result<Request<Body>, Box<dyn Error>> {
    Ok(post(body(&patient_query())?)?)
}

/// `request` with `Authorization: Bearer token`.
pub(crate) fn bearing(
    mut request: Request<Body>,
    token: &str,
) -> Result<Request<Body>, Box<dyn Error>> {
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, format!("Bearer {token}").parse()?);
    Ok(request)
}

/// The suite's default claims for a test to change.
pub(crate) fn claims() -> Claims {
    support::claims()
}

/// A token the suite's test issuer signs over `claims`.
pub(crate) fn minted(claims: &Claims) -> Result<String, Box<dyn Error>> {
    Ok(support::issuer().mint(claims)?)
}

/// Sends `request` to `gateway` and asserts it is refused for `refusal`:
/// its status, its code, its challenge, and no node asked.
pub(crate) async fn assert_refused(
    gateway: &Gateway,
    request: Request<Body>,
    refusal: Refusal,
) -> TestResult {
    let (status, headers, text) = sent(&gateway.app, request).await?;
    assert_eq!(refusal.code().status(), status, "{refusal:?}: {text}");
    assert_eq!(refusal.code().as_str(), error_body(&text)?.code, "{text}");
    let challenge = challenge(&headers);
    match refusal.code().status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if refusal != Refusal::Operation => {
            let challenge = challenge.ok_or("a 401 and a 403 carry a challenge (RFC 6750 §3)")?;
            assert!(
                challenge.starts_with("Bearer realm=\"ferrofed\""),
                "{challenge}"
            );
            if refusal != Refusal::Missing {
                assert!(
                    challenge.contains(refusal.description()),
                    "{refusal:?} is the reason: {challenge}"
                );
            }
        }
        _ => assert_eq!(None, challenge, "{refusal:?}"),
    }
    gateway.nobody_asked().await
}

/// Sends `request` to `app` as it is and reads the status, the headers and
/// the body text.
pub(crate) async fn sent(
    app: &Router,
    request: Request<Body>,
) -> Result<(StatusCode, HeaderMap, String), Box<dyn Error>> {
    let response = support::send_as_is(app.clone(), request).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024).await?;
    Ok((status, headers, String::from_utf8(bytes.to_vec())?))
}

/// The `WWW-Authenticate` challenge of `headers`.
fn challenge(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
}

/// Asserts that `request` passes the gate at `gateway` and is answered
/// `200` by the federated query.
pub(crate) async fn assert_admitted(gateway: &Gateway, request: Request<Body>) -> TestResult {
    let (status, _, text) = sent(&gateway.app, request).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    Ok(())
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Shared helpers: a capturing log writer, test settings, a test state, the
//! typed shapes the tests read the server's JSON with, a mock node's routes
//! and what it was asked, the reads of a routed answer, and the dependency
//! report.

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::auth::{AuthSettings, IssuerSettings, KeySource, Verification};
use ferrofed_server::config::settings::ServerSettings;
use ferrofed_server::error::{CODE_MEMBER, REQUEST_ID_MEMBER};
use ferrofed_server::state::AppState;
use ferrofed_testkit::issuer::{Claims, Issuer, IssuerError};
use ferrofed_testkit::mock::Server;
use http::{HeaderMap, Request, Response, StatusCode, header};
use openehr_federation::headers::{ENDPOINT, SYSTEM_ID};
use openehr_its::rest::generated::common::Error;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::io::{self, Write};
use std::num::TryFromIntError;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;
use tower::ServiceExt as _;
use tracing_subscriber::fmt::MakeWriter;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// The time a loaded host may add to any wait a test makes.
///
/// A bound on elapsed time sits this far past what the code under test
/// should take, and a node meant to be abandoned stays silent at least this
/// far past the bound, so neither side of the claim depends on how busy the
/// machine is.
pub(crate) const SLACK: Duration = Duration::from_secs(3);

/// Returns `duration` in whole milliseconds, as the configuration spells it.
pub(crate) fn millis(duration: Duration) -> Result<u64, TryFromIntError> {
    u64::try_from(duration.as_millis())
}

/// A `tracing` writer that keeps every line in memory.
#[derive(Debug, Clone, Default)]
pub(crate) struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    /// Returns everything written so far.
    pub(crate) fn text(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// The writer [`Logs`] hands out.
#[derive(Debug)]
pub(crate) struct LogsWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogsWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Logs {
    type Writer = LogsWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogsWriter(Arc::clone(&self.0))
    }
}

/// One JSON log line, read for the fields the tests assert on.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct LogLine {
    /// The event message.
    pub(crate) message: String,
    /// The HTTP method of a request line.
    pub(crate) method: Option<String>,
    /// The matched route of a request line.
    pub(crate) route: Option<String>,
    /// The status of a request line.
    pub(crate) status: Option<u16>,
    /// The latency of a request line.
    pub(crate) latency_ms: Option<f64>,
    /// The logged query pairs of a request line.
    pub(crate) query: Option<String>,
    /// The request id of a request line: the gateway's outbound id.
    pub(crate) request_id: Option<String>,
    /// Whether the client named its request, on a request line.
    pub(crate) client_named: Option<bool>,
    /// Where a thread panicked, on the panic hook's line.
    pub(crate) location: Option<String>,
}

/// Returns every JSON line in `text`, in order.
pub(crate) fn lines(text: &str) -> Result<Vec<LogLine>, serde_json::Error> {
    text.lines().map(serde_json::from_str).collect()
}

/// Returns the request lines in `text`.
pub(crate) fn request_lines(text: &str) -> Result<Vec<LogLine>, serde_json::Error> {
    Ok(lines(text)?
        .into_iter()
        .filter(|line| line.message == "request")
        .collect())
}

/// The issuer identifier of the suite's test issuer.
pub(crate) const ISSUER: &str = "https://issuer.example.test";

/// The audience the gateway under test is known by at the test issuer.
pub(crate) const AUDIENCE: &str = "urn:example:ferrofed-under-test";

/// The suite's test issuer, its key generated once per test process.
#[expect(
    clippy::expect_used,
    reason = "a test process that cannot generate a key pair cannot test anything"
)]
static TEST_ISSUER: LazyLock<Issuer> =
    LazyLock::new(|| Issuer::new(ISSUER).expect("a test key pair should generate"));

/// Returns the suite's test issuer, which every test gateway trusts.
pub(crate) fn issuer() -> &'static Issuer {
    &TEST_ISSUER
}

/// Returns the claims of a token the test gateway admits for every
/// operation: every scope and the purpose of use `TREAT`.
pub(crate) fn claims() -> Claims {
    Claims::new(ISSUER, AUDIENCE)
}

/// Returns a fresh token with the default [`claims`].
pub(crate) fn token() -> Result<String, IssuerError> {
    issuer().mint(&claims())
}

/// A default token minted once per test process: the client credential a
/// test sends explicitly and then searches for at the node, where it must
/// never arrive (§13.1).
#[expect(
    clippy::expect_used,
    reason = "a test process whose issuer cannot sign cannot test anything"
)]
pub(crate) static CLIENT_TOKEN: LazyLock<String> =
    LazyLock::new(|| token().expect("the test issuer should sign"));

/// Returns the `Authorization` value carrying a fresh default [`token`].
pub(crate) fn bearer() -> Result<String, IssuerError> {
    Ok(format!("Bearer {}", token()?))
}

/// Returns the `[auth]` of every test gateway: the test issuer is trusted,
/// its key set handed over with the configuration, and the default client is
/// a demographic client.
pub(crate) fn auth() -> AuthSettings {
    AuthSettings {
        audience: Some(AUDIENCE.to_owned()),
        issuers: vec![IssuerSettings {
            issuer: ISSUER.to_owned(),
            verification: Verification::KeySet(KeySource::Set(issuer().jwks())),
            backend_clients: BTreeSet::new(),
            demographic_clients: BTreeSet::from([claims().client_id]),
        }],
        ..AuthSettings::default()
    }
}

/// Returns the `[auth]` of [`auth`] as configuration text, the key set
/// inline.
pub(crate) fn auth_toml() -> Result<String, IssuerError> {
    Ok(format!(
        "\n[auth]\naudience = \"{AUDIENCE}\"\n\n[[auth.issuer]]\nissuer = \"{ISSUER}\"\njwks = '{}'\ndemographic_clients = [\"{}\"]\n",
        issuer().jwks_json()?,
        claims().client_id
    ))
}

/// Returns server settings a test drives the middleware with.
pub(crate) fn settings() -> ServerSettings {
    ServerSettings {
        listen: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        base_path: ferrofed_server::base_path::BasePath::default(),
        request_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(5),
        body_limit: 1024,
        auth: auth(),
    }
}

/// Returns a state with no indicator.
pub(crate) fn state() -> Arc<AppState> {
    Arc::new(AppState::default())
}

/// The application under test, with no indicator registered.
pub(crate) fn app() -> Router {
    ferrofed_server::router(state(), &settings())
}

/// Sends `request` through `app` and returns the whole response.
///
/// A request that carries no `Authorization` field is sent with a fresh
/// default [`token`], so every test reaches past client authentication
/// unless it sets the field itself.
pub(crate) async fn send(
    app: Router,
    mut request: Request<Body>,
) -> Result<Response<Body>, Box<dyn StdError>> {
    if !request.headers().contains_key(header::AUTHORIZATION) {
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, bearer()?.parse()?);
    }
    send_as_is(app, request).await
}

/// Returns an HTTP client that sends a fresh default [`token`] with every
/// request that carries no `Authorization` field of its own, for the tests
/// that drive a real socket.
pub(crate) fn authenticated_client() -> Result<reqwest::Client, Box<dyn StdError>> {
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, bearer()?.parse()?);
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .build()?)
}

/// Sends `request` through `app` as it is, with no credential added.
pub(crate) async fn send_as_is(
    app: Router,
    request: Request<Body>,
) -> Result<Response<Body>, Box<dyn StdError>> {
    Ok(app.oneshot(request).await?)
}

/// Sends `request` through `app` and reads the status and the body.
pub(crate) async fn call(
    app: Router,
    request: Request<Body>,
) -> Result<(StatusCode, String), Box<dyn StdError>> {
    let response = send(app, request).await?;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
    Ok((status, String::from_utf8(bytes.to_vec())?))
}

/// What the tests read of an error body.
#[derive(Debug)]
pub(crate) struct ErrorBody {
    /// The ITS-REST message.
    pub(crate) message: String,
    /// The ITS-REST validation errors.
    pub(crate) validation_errors: Vec<String>,
    /// The stable error code.
    pub(crate) code: String,
    /// The request id.
    pub(crate) request_id: String,
}

/// Reads `text` as the generated ITS-REST `Error`, which must carry the
/// stable code and the request id as its only extra members.
pub(crate) fn error_body(text: &str) -> Result<ErrorBody, Box<dyn StdError>> {
    let error: Error = serde_json::from_str(text)?;
    let member = |name: &str| -> Result<String, Box<dyn StdError>> {
        Ok(error
            .additional_properties
            .get(name)
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("the error body carries a string {name}: {text}"))?
            .to_owned())
    };
    let code = member(CODE_MEMBER)?;
    let request_id = member(REQUEST_ID_MEMBER)?;
    if error.additional_properties.len() != 2 {
        return Err(format!("the error body carries only code and request_id: {text}").into());
    }
    Ok(ErrorBody {
        message: error.message,
        validation_errors: error.validation_errors,
        code,
        request_id,
    })
}

/// The status, the headers and the body bytes `app` answers `request` with.
pub(crate) async fn exchange(
    app: Router,
    request: Request<Body>,
) -> Result<(StatusCode, HeaderMap, Vec<u8>), Box<dyn StdError>> {
    let response = send(app, request).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
    Ok((status, headers, bytes.to_vec()))
}

/// A node answering `verb` at `at` with `answer`, and `404` to the rest.
pub(crate) async fn mount(server: &Server, verb: &str, at: String, answer: ResponseTemplate) {
    Mock::given(method(verb))
        .and(path(at))
        .respond_with(answer)
        .mount(server)
        .await;
}

/// The method and path of every request `server` received, in order.
pub(crate) async fn asked(server: &Server) -> Result<Vec<(String, String)>, Box<dyn StdError>> {
    Ok(server
        .received_requests()
        .await
        .ok_or("recording is on")?
        .into_iter()
        .map(|request| (request.method.to_string(), request.url.path().to_owned()))
        .collect())
}

/// The value of the field `name` in `headers`, when it is text.
pub(crate) fn field<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Asserts that `headers` name `endpoint` and its node's `system_id` as the
/// ones that acted (§7a.3, N31, §9.6).
pub(crate) fn acted(headers: &HeaderMap, endpoint: &str, system_id: &str) {
    assert_eq!(Some(endpoint), field(headers, ENDPOINT), "N31");
    assert_eq!(Some(system_id), field(headers, SYSTEM_ID), "§9.6");
}

/// Asserts that `request` is refused `400` with `code`, names no acting
/// endpoint, and that neither node received anything.
pub(crate) async fn refused_at_neither(
    app: Router,
    request: Request<Body>,
    code: &str,
    (a, b): (&Server, &Server),
) -> Result<(), Box<dyn StdError>> {
    let (status, headers, body) = exchange(app, request).await?;
    let text = String::from_utf8(body)?;
    assert_eq!(StatusCode::BAD_REQUEST, status, "{text}");
    assert_eq!(code, error_body(&text)?.code, "{text}");
    assert_eq!(None, field(&headers, ENDPOINT), "no endpoint acted: {text}");
    assert!(asked(a).await?.is_empty(), "node A received nothing");
    assert!(asked(b).await?.is_empty(), "node B received nothing");
    Ok(())
}

/// The state `GET /health/dependencies` of `app` reports of each member
/// endpoint, by endpoint id.
pub(crate) async fn observed(app: &Router) -> Result<BTreeMap<String, String>, Box<dyn StdError>> {
    #[derive(Deserialize)]
    struct Report {
        endpoints: BTreeMap<String, String>,
    }
    let request = Request::get("/health/dependencies").body(Body::empty())?;
    let (status, text) = call(app.clone(), request).await?;
    if status != StatusCode::OK {
        return Err(format!("the dependency report answered {status}: {text}").into());
    }
    Ok(serde_json::from_str::<Report>(&text)?.endpoints)
}

/// The `(endpoint, state)` pairs `pairs` as [`observed`] returns them.
pub(crate) fn states(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(endpoint, state)| ((*endpoint).to_owned(), (*state).to_owned()))
        .collect()
}

/// What a search reads in place of a request id the gateway minted.
pub(crate) const MINTED_REQUEST_ID: &str = "<minted-request-id>";

/// Whether `text` has the form the gateway mints a request id in: a version
/// 4 UUID, hyphenated and lowercase, as `OutboundId` writes it.
pub(crate) fn is_minted_form(text: &str) -> bool {
    uuid::Uuid::try_parse(text).is_ok_and(|id| {
        id.get_version() == Some(uuid::Version::Random) && id.hyphenated().to_string() == text
    })
}

/// Returns `text` with each of `ids`, request ids the gateway minted, replaced
/// by [`MINTED_REQUEST_ID`].
///
/// A search for a short hexadecimal fragment reads the result, so a random
/// UUID cannot match it by chance.
///
/// # Errors
///
/// When an id is not in the minted form, so no other value is ever masked.
pub(crate) fn without_minted<'a>(
    text: &str,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<String, Box<dyn StdError>> {
    let mut masked = text.to_owned();
    for id in ids {
        if !is_minted_form(id) {
            return Err(format!("{id:?} is not a request id the gateway minted").into());
        }
        masked = masked.replace(id, MINTED_REQUEST_ID);
    }
    Ok(masked)
}

/// The address every test gateway declares its JWK Set at.
pub(crate) const JWKS_URI: &str = "https://gw.example.org/.well-known/jwks.json";

/// The gateway signing key every test gateway that federates is configured
/// with: a synthetic ES384 key written once per test process, in a directory
/// that lives as long as the process.
#[expect(
    clippy::expect_used,
    reason = "a test process that cannot write a synthetic key cannot test anything"
)]
static SIGNING_KEY: LazyLock<(tempfile::TempDir, String)> = LazyLock::new(|| {
    let dir = tempfile::tempdir().expect("a temporary directory should be made");
    let file = dir.path().join("signing-key.pem");
    let pem = ferrofed_testkit::oauth::es384_pem().expect("a test key should generate");
    std::fs::write(&file, pem).expect("the key file should be written");
    (dir, file.display().to_string())
});

/// The `[signing]` table every test gateway that federates carries, so it
/// can sign the caller's identity for each node (§13.1, N24).
pub(crate) fn signing_toml() -> String {
    format!(
        "\n[signing]\nkey_file = {}\njwks_uri = \"{JWKS_URI}\"\n",
        toml::Value::String(SIGNING_KEY.1.clone())
    )
}

/// The file the suite's gateway signing key is written to.
pub(crate) fn signing_key_file() -> &'static str {
    &SIGNING_KEY.1
}

/// `text` with the [`signing_toml`] table appended when it configures a
/// registry and no `[signing]` of its own.
pub(crate) fn signed(text: &str) -> String {
    if text.contains("[registry]") && !text.contains("[signing]") {
        format!("{text}{}", signing_toml())
    } else {
        text.to_owned()
    }
}

/// A signer over a fresh synthetic key, naming the gateway `federation`,
/// for a test that assembles its own federation.
pub(crate) fn signer(
    federation: &str,
) -> Result<Arc<ferrofed_engine::onward::conveyance::Signer>, Box<dyn StdError>> {
    use ferrofed_engine::onward::SystemClock;
    use ferrofed_engine::onward::keys::{KeyRing, SigningKey};
    let pem = secrecy::SecretString::from(ferrofed_testkit::oauth::es384_pem()?);
    let keys = KeyRing::new(
        SigningKey::from_pem(&pem)?,
        None,
        Duration::ZERO,
        Arc::new(SystemClock),
    )?;
    Ok(Arc::new(ferrofed_engine::onward::conveyance::Signer::new(
        Arc::new(keys),
        federation,
    )))
}

/// A conveyance of a synthetic verified caller, for a test that dispatches
/// through the engine itself.
pub(crate) fn conveyance()
-> Result<ferrofed_engine::onward::conveyance::Conveyance, Box<dyn StdError>> {
    use ferrofed_engine::onward::conveyance::{Caller, Conveyance, Principal, Verification};
    let caller = Caller {
        issuer: ISSUER.to_owned(),
        subject: "clinician-0042".to_owned(),
        organisation: None,
        purposes: Vec::new(),
        scope: String::new(),
        verified_by: Verification::Signature,
    };
    Ok(Conveyance::new(
        signer("example-federation")?,
        Principal::Caller(caller),
    ))
}

/// One purpose of use of a conveyed token, as a node reads it.
#[derive(Debug, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConveyedPurpose {
    /// The code system.
    pub(crate) system: Option<String>,
    /// The code.
    pub(crate) code: String,
}

/// The claims of an `openEHR-federation-client` token as a node reads them;
/// a claim not named here, `person_id` among them, fails the read (§5.4.1,
/// N33).
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Conveyed {
    pub(crate) iss: String,
    pub(crate) aud: String,
    pub(crate) iat: i64,
    pub(crate) exp: i64,
    pub(crate) jti: String,
    pub(crate) sub: String,
    pub(crate) iss_upstream: Option<String>,
    pub(crate) verified_by: Option<String>,
    pub(crate) subject_organization_id: Option<String>,
    #[serde(default)]
    pub(crate) purpose_of_use: Vec<ConveyedPurpose>,
    pub(crate) scope: Option<String>,
}

/// The claims `token` carries, read without verifying it.
pub(crate) fn conveyed_claims(token: &str) -> Result<Conveyed, Box<dyn StdError>> {
    Ok(jsonwebtoken::dangerous::insecure_decode_claims::<Conveyed>(
        token,
    )?)
}

/// What a search reads in place of a conveyed token's `aud`.
pub(crate) const CONVEYED_AUDIENCE: &str = "<audience>";

/// The claims of `token` as JSON, for a search of what a node received, its
/// `aud` read as [`CONVEYED_AUDIENCE`].
///
/// The `aud` is the endpoint id the registry gives the node it is sent to,
/// composed from no request, as the minted request id is; every other claim
/// stays as the node reads it, so a client value in one is still found.
pub(crate) fn searched_claims(token: &str) -> Result<String, Box<dyn StdError>> {
    let mut claims = conveyed_claims(token)?;
    CONVEYED_AUDIENCE.clone_into(&mut claims.aud);
    Ok(serde_json::to_string(&claims)?)
}

/// The claims of `token` as JSON with the values minted per token, `iat`,
/// `exp` and `jti`, cleared, so two requests the gateway sent for the same
/// caller compare equal.
pub(crate) fn stable_claims(token: &str) -> Result<String, Box<dyn StdError>> {
    let mut claims = conveyed_claims(token)?;
    (claims.iat, claims.exp) = (0, 0);
    claims.jti.clear();
    Ok(serde_json::to_string(&claims)?)
}

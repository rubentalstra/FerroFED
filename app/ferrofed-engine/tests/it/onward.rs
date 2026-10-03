// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The onward grant against the harness token endpoint and a mock node: the
//! gateway authenticates to a node as itself with OAuth 2.0 client
//! credentials and an ES384 client assertion verified against its published
//! JWK Set (§13.1, N25, CP-17 onward half; RFC 6749 §4.4, RFC 7523 §2.2 and
//! §3, RFC 7517, RFC 7638).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ferrofed_engine::dispatch::reported::UNAUTHENTICATED;
use ferrofed_engine::dispatch::{DispatchOptions, NodeClient, NodeQuery, NodeReply};
use ferrofed_engine::hygiene::Withheld;
use ferrofed_engine::onward::keys::{KeyError, KeyRing, SigningKey};
use ferrofed_engine::onward::provider::{ClientCredentials, REFRESH_MARGIN};
use ferrofed_engine::onward::token::{CLIENT_ASSERTION_TYPE, GRANT_TYPE};
use ferrofed_engine::onward::{Clock, Grant, Scope, ScopeError};
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::secret::SecretUrl;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::oauth::{self, TokenEndpoint, Verdict};
use ferrofed_testkit::unreachable;
use openehr_federation::outcome::ErrorDetail;
use openehr_federation::status::EndpointStatus;
use openehr_its::rest::client::{Credentials, CredentialsProvider, ReqwestTransport};
use secrecy::SecretString;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

type TestResult = Result<(), Box<dyn Error>>;

/// The client the node's authorization server registered the gateway as.
const CLIENT_ID: &str = "ferrofed-test-gateway";

/// The scope every token is requested with.
const SCOPE: &str = "system/aql-*.s system/composition-*.r";

/// A synthetic node query, scoped to an `ehr_id` under no real system.
const NODE_AQL: &str = "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c WHERE e/ehr_id/value = '7d44b88c-4199-4bad-97dc-d78268e01398'";

/// An empty ITS-REST `RESULT_SET`.
const EMPTY_RESULT_SET: &str = r##"{"q":"SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c","columns":[{"name":"#0","path":"c/uid/value"}],"rows":[]}"##;

/// A synthetic subject the gateway resolved on and withholds.
const SUBJECT: &str = "SYNTHETIC-SUBJECT-4f1a";

/// A clock the test moves by hand.
#[derive(Debug)]
struct ManualClock(Mutex<Instant>);

impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Instant::now())))
    }

    fn advance(&self, by: Duration) -> TestResult {
        let mut now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *now = now.checked_add(by).ok_or("the clock passed its range")?;
        Ok(())
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A fresh synthetic ES384 key.
fn key() -> Result<SigningKey, Box<dyn Error>> {
    Ok(SigningKey::from_pem(&SecretString::from(
        oauth::es384_pem()?,
    ))?)
}

/// A ring of `current` alone.
fn ring(current: SigningKey, clock: Arc<dyn Clock>) -> Result<Arc<KeyRing>, Box<dyn Error>> {
    Ok(Arc::new(KeyRing::new(
        current,
        None,
        Duration::from_mins(65),
        clock,
    )?))
}

/// The grant at `endpoint`'s token URL.
fn grant(endpoint: &TokenEndpoint) -> Result<Grant, Box<dyn Error>> {
    Ok(Grant::new(
        &SecretUrl::new(endpoint.token_url()),
        CLIENT_ID,
        Scope::parse(SCOPE)?,
    )?)
}

/// The provider of `grant` over `keys`, read by `clock`.
fn provider(
    grant: Grant,
    keys: Arc<KeyRing>,
    clock: Arc<dyn Clock>,
) -> Result<Arc<ClientCredentials<ReqwestTransport>>, Box<dyn Error>> {
    Ok(Arc::new(ClientCredentials::new(
        EndpointId::new("node-a-pub")?,
        grant,
        keys,
        (Duration::from_secs(300), Duration::from_secs(5)),
        ReqwestTransport::with_timeout(Duration::from_secs(10))?,
        clock,
    )))
}

/// A mock node that answers `200` to a request carrying a token `endpoint`
/// issued and still accepts, and `401` to every other.
async fn node_requiring(endpoint: &TokenEndpoint) -> Server {
    let server = Server::start().await;
    Mock::given(method("POST"))
        .and(path("/openehr/v1/query/aql"))
        .and(endpoint.bearer())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(EMPTY_RESULT_SET.as_bytes().to_vec(), "application/json"),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/openehr/v1/query/aql"))
        .respond_with(ResponseTemplate::new(401).set_body_raw(
            br#"{"message":"synthetic: the token is not accepted"}"#.to_vec(),
            "application/json",
        ))
        .with_priority(2)
        .mount(&server)
        .await;
    server
}

/// The client of the node at `server`, sending what `provider` gives it.
fn client(
    server: &Server,
    provider: Arc<ClientCredentials<ReqwestTransport>>,
) -> Result<NodeClient<ReqwestTransport>, Box<dyn Error>> {
    let document = format!(
        "[[organisation]]\nid = \"org-a\"\n\n[[node]]\nid = \"node-a\"\norganisation = \"org-a\"\nsystem_id = \"cdr-a.example.org\"\n\n[[endpoint]]\nid = \"node-a-pub\"\nnode = \"node-a\"\nurl = \"{}/openehr\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n",
        server.uri()
    );
    let snapshot = RegistrySnapshot::from_toml_str(&document)?;
    let endpoint = snapshot.endpoints().next().ok_or("one endpoint")?;
    Ok(NodeClient::new(
        endpoint,
        ReqwestTransport::with_timeout(Duration::from_secs(10))?,
    )?
    .with_credentials_provider(provider))
}

/// Options with a five-second deadline, withholding [`SUBJECT`].
fn options() -> Result<DispatchOptions, Box<dyn Error>> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .ok_or("the deadline is past the platform clock")?;
    Ok(
        DispatchOptions::new(deadline, crate::conveyed::conveyance())
            .with_withheld(Arc::new(Withheld::new([SecretString::from(SUBJECT)]))),
    )
}

/// Sends the node query through `client`.
async fn query(client: &NodeClient<ReqwestTransport>) -> Result<NodeReply, Box<dyn Error>> {
    Ok(client.query(&NodeQuery::new(NODE_AQL), &options()?).await?)
}

/// The text of a reply's `error`.
fn error_text(reply: &NodeReply) -> Result<String, Box<dyn Error>> {
    match reply.outcome().error() {
        Some(ErrorDetail::Text(text)) => Ok(text.clone()),
        other => Err(format!("an unexpected error: {other:?}").into()),
    }
}

/// The bearer token `credentials` carries.
fn bearer(credentials: &Credentials) -> Result<String, Box<dyn Error>> {
    let value = credentials.header_value()?;
    Ok(value
        .to_str()?
        .strip_prefix("Bearer ")
        .ok_or("a bearer credential")?
        .to_owned())
}

// conformance: CP-17
#[tokio::test]
async fn the_assertion_verifies_against_the_published_jwks_and_the_token_reaches_the_node()
-> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    endpoint.expect_scope(SCOPE);
    let clock: Arc<dyn Clock> = ManualClock::new();
    let keys = ring(key()?, Arc::clone(&clock))?;
    endpoint.trust(keys.published());
    let node = node_requiring(&endpoint).await;
    let client = client(&node, provider(grant(&endpoint)?, keys, clock)?)?;

    let reply = query(&client).await?;
    assert_eq!(
        EndpointStatus::Active,
        reply.status(),
        "{:?}",
        reply.outcome()
    );
    assert_eq!(vec![Verdict::Issued], endpoint.verdicts());
    Ok(())
}

/// The token request is the client-credentials grant with a JWT client
/// assertion, the configured scope, and no client secret (RFC 6749 §4.4.2,
/// RFC 7523 §2.2).
// conformance: CP-17
#[tokio::test]
async fn the_token_request_carries_the_rfc_7523_parameters() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    let clock: Arc<dyn Clock> = ManualClock::new();
    let keys = ring(key()?, Arc::clone(&clock))?;
    endpoint.trust(keys.published());
    let grant = grant(&endpoint)?
        .with_resource("https://cdr-a.example.org/openehr")?
        .with_audience("cdr-a")?;
    provider(grant, keys, clock)?.credentials().await?;

    let forms = endpoint.forms();
    let [form] = forms.as_slice() else {
        return Err(format!("expected one token request, got {}", forms.len()).into());
    };
    let names: Vec<&str> = form.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        vec![
            "grant_type",
            "client_assertion_type",
            "client_assertion",
            "scope",
            "resource",
            "audience"
        ],
        names
    );
    let field = |name: &str| {
        form.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(Some(GRANT_TYPE), field("grant_type"));
    assert_eq!(Some(CLIENT_ASSERTION_TYPE), field("client_assertion_type"));
    assert_eq!(Some(SCOPE), field("scope"));
    assert_eq!(Some("https://cdr-a.example.org/openehr"), field("resource"));
    assert_eq!(Some("cdr-a"), field("audience"));
    Ok(())
}

/// The assertion names the client as `iss` and `sub` and the token
/// endpoint as `aud`, lives the configured lifetime, carries `iat` and a
/// unique `jti`, and its header names the current key's RFC 7638 `kid`
/// (RFC 7523 §3).
// conformance: CP-17
#[tokio::test]
async fn the_assertion_carries_the_rfc_7523_claims() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, None).await;
    let clock: Arc<dyn Clock> = ManualClock::new();
    let current = key()?;
    let kid = current.kid().to_owned();
    let keys = ring(current, Arc::clone(&clock))?;
    endpoint.trust(keys.published());
    let provider = provider(grant(&endpoint)?, Arc::clone(&keys), clock)?;
    provider.credentials().await?;
    provider.credentials().await?;

    let assertions = endpoint.assertions();
    assert_eq!(
        2,
        assertions.len(),
        "a token with no lifetime is not cached"
    );
    let mut seen = Vec::new();
    for assertion in &assertions {
        let header = jsonwebtoken::decode_header(assertion)?;
        assert_eq!(jsonwebtoken::Algorithm::ES384, header.alg);
        assert_eq!(Some(kid.as_str()), header.kid.as_deref());
        let claims = oauth::verify(
            assertion,
            &keys.published(),
            CLIENT_ID,
            &endpoint.token_url(),
        )?;
        assert_eq!(CLIENT_ID, claims.iss);
        assert_eq!(CLIENT_ID, claims.sub);
        assert_eq!(endpoint.token_url(), claims.aud);
        assert_eq!(300, claims.exp - claims.iat);
        seen.push(claims.jti);
    }
    seen.dedup();
    assert_eq!(2, seen.len(), "every assertion has its own jti");
    Ok(())
}

/// The `kid` is the RFC 7638 thumbprint of the public key: the same key has
/// the same `kid`, and the JWK carries `use` `sig` and `alg` `ES384`.
#[test]
fn the_kid_is_the_rfc_7638_thumbprint_of_the_key() -> TestResult {
    let pem = oauth::es384_pem()?;
    let once = SigningKey::from_pem(&SecretString::from(pem.clone()))?;
    let again = SigningKey::from_pem(&SecretString::from(pem))?;
    assert_eq!(once.kid(), again.kid());
    let thumbprint = once
        .public()
        .thumbprint(jsonwebtoken::jwk::ThumbprintHash::SHA256)?;
    assert_eq!(thumbprint, once.kid());
    assert_eq!(Some(once.kid()), once.public().common.key_id.as_deref());
    assert_eq!(
        Some(jsonwebtoken::jwk::PublicKeyUse::Signature),
        once.public().common.public_key_use
    );
    assert_ne!(key()?.kid(), once.kid());
    Ok(())
}

#[test]
fn a_key_that_is_no_es384_key_is_refused() -> TestResult {
    let p256 = SigningKey::from_pem(&SecretString::from(oauth::p256_pem()?));
    assert!(matches!(p256, Err(KeyError::Curve(_))), "{p256:?}");
    let garbage = SigningKey::from_pem(&SecretString::from("not a key"));
    assert!(matches!(garbage, Err(KeyError::Pem(_))), "{garbage:?}");
    Ok(())
}

#[test]
fn a_previous_key_that_is_the_current_key_is_refused() -> TestResult {
    let pem = oauth::es384_pem()?;
    let current = SigningKey::from_pem(&SecretString::from(pem.clone()))?;
    let previous = SigningKey::from_pem(&SecretString::from(pem))?;
    let refused = KeyRing::new(
        current,
        Some(previous),
        Duration::from_mins(65),
        ManualClock::new(),
    );
    assert!(
        matches!(refused, Err(KeyError::SameKey { .. })),
        "{refused:?}"
    );
    Ok(())
}

/// During the overlap window the JWK Set publishes the current and the
/// previous key, and an assertion the previous key signed before the
/// rotation still verifies; once the window ends only the current key is
/// published and that assertion no longer verifies (RFC 7517 §5).
// conformance: CP-17
#[tokio::test]
async fn a_rotation_publishes_both_keys_for_the_overlap_window() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, None).await;
    let clock = ManualClock::new();
    let old = key()?;
    let before = ring(old.clone(), clock.clone())?;
    endpoint.trust(before.published());
    provider(grant(&endpoint)?, before, clock.clone())?
        .credentials()
        .await?;
    let signed_by_old = endpoint
        .assertions()
        .pop()
        .ok_or("the old key signed an assertion")?;

    let new = key()?;
    let overlap = Duration::from_mins(65);
    let rotated = KeyRing::new(new.clone(), Some(old.clone()), overlap, clock.clone())?;
    let published: Vec<Option<String>> = rotated
        .published()
        .keys
        .iter()
        .map(|jwk| jwk.common.key_id.clone())
        .collect();
    assert_eq!(
        vec![Some(new.kid().to_owned()), Some(old.kid().to_owned())],
        published
    );
    oauth::verify(
        &signed_by_old,
        &rotated.published(),
        CLIENT_ID,
        &endpoint.token_url(),
    )?;
    assert_eq!(new.kid(), rotated.current().kid(), "the new key signs");

    clock.advance(overlap)?;
    let published: Vec<Option<String>> = rotated
        .published()
        .keys
        .iter()
        .map(|jwk| jwk.common.key_id.clone())
        .collect();
    assert_eq!(vec![Some(new.kid().to_owned())], published);
    assert!(
        oauth::verify(
            &signed_by_old,
            &rotated.published(),
            CLIENT_ID,
            &endpoint.token_url()
        )
        .is_err()
    );
    Ok(())
}

/// A token is reused until [`REFRESH_MARGIN`] before the end of the lifetime
/// it stated, then replaced.
// conformance: CP-17
#[tokio::test]
async fn a_token_is_cached_until_thirty_seconds_before_it_expires() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(120)).await;
    let clock = ManualClock::new();
    let keys = ring(key()?, clock.clone())?;
    endpoint.trust(keys.published());
    let provider = provider(grant(&endpoint)?, keys, clock.clone())?;

    let first = bearer(&provider.credentials().await?)?;
    assert_eq!(first, bearer(&provider.credentials().await?)?);
    assert_eq!(1, endpoint.issued());

    let fresh_until = Duration::from_secs(120)
        .checked_sub(REFRESH_MARGIN)
        .ok_or("the lifetime outlasts the margin")?;
    clock.advance(
        fresh_until
            .checked_sub(Duration::from_secs(1))
            .ok_or("a second short of the margin")?,
    )?;
    assert_eq!(first, bearer(&provider.credentials().await?)?);
    assert_eq!(1, endpoint.issued(), "still inside the margin");

    clock.advance(Duration::from_secs(1))?;
    let second = bearer(&provider.credentials().await?)?;
    assert_ne!(first, second);
    assert_eq!(2, endpoint.issued(), "replaced at the margin");
    Ok(())
}

/// A token whose lifetime is shorter than the margin serves one request.
#[tokio::test]
async fn a_token_shorter_than_the_margin_is_not_cached() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(20)).await;
    let clock = ManualClock::new();
    let keys = ring(key()?, clock.clone())?;
    endpoint.trust(keys.published());
    let provider = provider(grant(&endpoint)?, keys, clock)?;
    provider.credentials().await?;
    provider.credentials().await?;
    assert_eq!(2, endpoint.issued());
    Ok(())
}

/// Concurrent requests that find no token wait for one token request and
/// share its token.
#[tokio::test]
async fn concurrent_requests_share_one_token_request() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    endpoint.delay(Duration::from_millis(200));
    let clock = ManualClock::new();
    let keys = ring(key()?, clock.clone())?;
    endpoint.trust(keys.published());
    let provider = provider(grant(&endpoint)?, keys, clock)?;

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let provider = Arc::clone(&provider);
        tasks.push(tokio::spawn(async move {
            provider
                .credentials()
                .await
                .map_err(|error| error.to_string())
        }));
    }
    let mut tokens = Vec::new();
    for task in tasks {
        tokens.push(bearer(&task.await??)?);
    }
    tokens.dedup();
    assert_eq!(1, tokens.len(), "every request has the one token");
    assert_eq!(1, endpoint.forms().len(), "one token request was sent");
    Ok(())
}

/// A node that answers `401` drops the cached token, so the next request
/// obtains a new one and is accepted.
// conformance: CP-17
#[tokio::test]
async fn a_401_drops_the_token_and_the_next_request_obtains_a_new_one() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    let clock: Arc<dyn Clock> = ManualClock::new();
    let keys = ring(key()?, Arc::clone(&clock))?;
    endpoint.trust(keys.published());
    let node = node_requiring(&endpoint).await;
    let client = client(&node, provider(grant(&endpoint)?, keys, clock)?)?;

    assert_eq!(EndpointStatus::Active, query(&client).await?.status());
    endpoint.revoke_all();
    let refused = query(&client).await?;
    assert_eq!(EndpointStatus::NodeError, refused.status());
    assert_eq!(1, endpoint.issued(), "the cached token was sent");
    let again = query(&client).await?;
    assert_eq!(
        EndpointStatus::Active,
        again.status(),
        "{:?}",
        again.outcome()
    );
    assert_eq!(2, endpoint.issued(), "the 401 dropped the token");
    Ok(())
}

#[test]
fn a_scope_outside_the_system_resource_grammar_is_refused() {
    for scope in [
        "patient/aql-*.s",
        "user/composition-*.r",
        "openid",
        "launch/patient",
        "system/*.rs",
        "system/aql-*.x",
    ] {
        assert!(
            matches!(
                Scope::parse(scope),
                Err(ScopeError::NotSystemResource { .. })
            ),
            "{scope}"
        );
    }
    assert_eq!(Err(ScopeError::Empty), Scope::parse("  "));
    assert!(Scope::parse("system/aql-*.s system/template-*.r").is_ok());
}

/// The provider names its endpoint and client and never a token.
#[tokio::test]
async fn a_provider_shows_no_token() -> TestResult {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    let clock: Arc<dyn Clock> = ManualClock::new();
    let keys = ring(key()?, Arc::clone(&clock))?;
    endpoint.trust(keys.published());
    let provider = provider(grant(&endpoint)?, keys, clock)?;
    let token = bearer(&provider.credentials().await?)?;
    let shown = format!("{provider:?}");
    assert!(!shown.contains(&token), "{shown}");
    assert!(shown.contains(CLIENT_ID), "{shown}");
    Ok(())
}

/// Refuses every token request of a fresh endpoint with `error` and
/// `description`, dispatches the node query, and returns the reply and
/// whether the node was sent anything.
async fn refused_with(
    status: u16,
    error: &str,
    description: &str,
) -> Result<(NodeReply, bool), Box<dyn Error>> {
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    endpoint.refuse(status, error, description);
    let clock: Arc<dyn Clock> = ManualClock::new();
    let keys = ring(key()?, Arc::clone(&clock))?;
    let node = node_requiring(&endpoint).await;
    let client = client(&node, provider(grant(&endpoint)?, keys, clock)?)?;
    let reply = query(&client).await?;
    let sent = !node
        .received_requests()
        .await
        .ok_or("recording is on")?
        .is_empty();
    Ok((reply, sent))
}

/// A token endpoint that refuses fails the node as `node-error` carrying the
/// fixed sentence and its registered RFC 6749 §5.2 code, and the node is sent
/// nothing (§13.1, N25, §11.1).
// conformance: CP-17
#[tokio::test]
async fn a_refused_token_request_is_a_node_error_and_nothing_reaches_the_node() -> TestResult {
    let (reply, sent) = refused_with(401, "invalid_client", "synthetic refusal").await?;
    assert_eq!(EndpointStatus::NodeError, reply.status());
    assert!(!reply.contact().sent());
    assert_eq!(
        format!("{UNAUTHENTICATED}: the token endpoint refused with invalid_client"),
        error_text(&reply)?
    );
    assert!(!sent, "never an unauthenticated dispatch");
    Ok(())
}

/// The token endpoint's `error_description` never reaches the answer: one
/// naming an internal URL, a file path and the withheld subject shows none
/// of them (§13.1, N33).
// conformance: CP-17 CP-26
#[tokio::test]
async fn a_token_endpoint_description_never_reaches_the_answer() -> TestResult {
    let description = format!(
        "key /etc/ferrofed/secret.pem rejected by https://idp.internal.example:8443/realm for {SUBJECT}"
    );
    let (reply, sent) = refused_with(400, "invalid_grant", &description).await?;
    let text = error_text(&reply)?;
    for hidden in [
        "/etc/ferrofed",
        "idp.internal.example",
        "https://",
        SUBJECT,
        CLIENT_ID,
    ] {
        assert!(!text.contains(hidden), "{hidden} is not shown: {text}");
    }
    assert!(text.ends_with("invalid_grant"), "{text}");
    assert!(!sent);
    Ok(())
}

/// A code RFC 6749 §5.2 does not register is not shown: the answer is the
/// fixed sentence alone.
#[tokio::test]
async fn an_unregistered_error_code_is_not_shown() -> TestResult {
    let (reply, sent) = refused_with(400, "backend_at_10.0.0.7_failed", "synthetic").await?;
    assert_eq!(UNAUTHENTICATED, error_text(&reply)?);
    assert!(!sent);
    Ok(())
}

/// A token endpoint that cannot be reached fails the node as `node-error`
/// with the fixed sentence alone, naming no address, and the node is sent
/// nothing.
// conformance: CP-17
#[tokio::test]
async fn an_unreachable_token_endpoint_is_a_node_error_and_nothing_reaches_the_node() -> TestResult
{
    let endpoint = TokenEndpoint::start(CLIENT_ID, Some(300)).await;
    let clock: Arc<dyn Clock> = ManualClock::new();
    let keys = ring(key()?, Arc::clone(&clock))?;
    let node = node_requiring(&endpoint).await;
    let down = Grant::new(
        &SecretUrl::new(format!("{}{}", unreachable::BASE, oauth::TOKEN_PATH)),
        CLIENT_ID,
        Scope::parse(SCOPE)?,
    )?;
    let client = client(&node, provider(down, keys, clock)?)?;

    let reply = query(&client).await?;
    assert_eq!(EndpointStatus::NodeError, reply.status());
    let text = error_text(&reply)?;
    assert_eq!(UNAUTHENTICATED, text);
    assert!(!text.contains("127.0.0.1"), "{text}");
    assert!(
        node.received_requests()
            .await
            .ok_or("recording is on")?
            .is_empty()
    );
    Ok(())
}

/// A token endpoint URL that could carry a secret into a log is refused:
/// one with a user name, a password, a query or a fragment (RFC 6749 §3.2).
#[test]
fn a_token_endpoint_that_could_carry_a_secret_is_refused() -> TestResult {
    for url in [
        "https://gateway:Qz7secret@idp.example.org/token",
        "https://idp.example.org/token?client_secret=Qz7secret",
        "https://idp.example.org/token#Qz7secret",
        "ftp://idp.example.org/token",
    ] {
        let refused = Grant::new(&SecretUrl::new(url), CLIENT_ID, Scope::parse(SCOPE)?);
        let Err(error) = refused else {
            return Err(format!("{url} was accepted").into());
        };
        assert!(
            !format!("{error} {error:?}").contains("Qz7secret"),
            "{error:?}"
        );
    }
    Ok(())
}

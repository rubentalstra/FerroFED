// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Node dispatch against a mock node: every generated outcome of
//! `POST {base}/v1/query/aql` and every `ClientError` kind maps to exactly one
//! §11.1 endpoint status (N16) or to a gateway-side `DispatchError`, the base
//! URL is used verbatim (N28), and nothing the client composes carries a value
//! that was not put in the body (§5.4.1, N33).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrofed_engine::dispatch::reported::{MESSAGE_LIMIT, UNAUTHENTICATED};
use ferrofed_engine::dispatch::{
    DispatchOptions, NodeClient, NodeClients, NodeQuery, NodeReply, REQUEST_ID_HEADER, SetupError,
    SharedCredentials,
};
use ferrofed_engine::hygiene::Withheld;
use ferrofed_engine::hygiene::mask::MASK;
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::snapshot::{Endpoint, RegistrySnapshot};
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::unreachable;
use openehr_federation::outcome::ErrorDetail;
use openehr_federation::status::EndpointStatus;
use openehr_its::rest::client::{
    Credentials, CredentialsError, CredentialsProvider, ReqwestTransport,
};
use secrecy::SecretString;
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};

type TestResult = Result<(), Box<dyn Error>>;

/// A synthetic node query, scoped to an `ehr_id` under no real system.
const NODE_AQL: &str = "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c WHERE e/ehr_id/value = '7d44b88c-4199-4bad-97dc-d78268e01398'";

/// A synthetic subject the gateway resolved on and withholds, with a quote so
/// its AQL literal form differs from the raw one.
const SUBJECT: &str = "O'SYNTHETIC-SUBJECT-7c1d";

/// The host of the testkit's unreachable base, withheld as a synthetic value
/// the HTTP client's reason names when the connection is refused.
const UNREACHABLE_HOST: &str = "127.0.0.1";

/// An empty ITS-REST `RESULT_SET`.
const EMPTY_RESULT_SET: &str = r##"{"q":"SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c","columns":[{"name":"#0","path":"c/uid/value"}],"rows":[]}"##;

/// A one-node federation with `endpoints` as its `(endpoint_id, url)` pairs.
fn snapshot(endpoints: &[(&str, &str)]) -> Result<RegistrySnapshot, Box<dyn Error>> {
    let mut document = String::from(
        "[[organisation]]\nid = \"org-a\"\n\n[[node]]\nid = \"node-a\"\norganisation = \"org-a\"\nsystem_id = \"cdr-a.example.org\"\n",
    );
    for (id, url) in endpoints {
        write!(
            document,
            "\n[[endpoint]]\nid = \"{id}\"\nnode = \"node-a\"\nurl = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n"
        )?;
    }
    Ok(RegistrySnapshot::from_toml_str(&document)?)
}

/// The one endpoint of a single-endpoint snapshot.
fn only_endpoint(snapshot: &RegistrySnapshot) -> Result<&Endpoint, Box<dyn Error>> {
    snapshot
        .endpoints()
        .next()
        .ok_or_else(|| "the snapshot holds no endpoint".into())
}

/// The engine over the default `reqwest` transport.
fn transport() -> Result<ReqwestTransport, Box<dyn Error>> {
    Ok(ReqwestTransport::with_timeout(Duration::from_secs(10))?)
}

/// A client for an endpoint at `url`.
fn client_at(url: &str) -> Result<NodeClient<ReqwestTransport>, Box<dyn Error>> {
    let snapshot = snapshot(&[("node-a-pub", url)])?;
    Ok(NodeClient::new(only_endpoint(&snapshot)?, transport()?)?)
}

/// Options with a deadline `budget` from now.
fn within(budget: Duration) -> Result<DispatchOptions, Box<dyn Error>> {
    let deadline = Instant::now()
        .checked_add(budget)
        .ok_or("the deadline is past the platform clock")?;
    Ok(DispatchOptions::new(
        deadline,
        crate::conveyed::conveyance(),
    ))
}

/// A mock node answering `POST {prefix}/v1/query/aql` with `answer`.
async fn node_answering(prefix: &str, answer: ResponseTemplate) -> Server {
    let server = Server::start().await;
    Mock::given(method("POST"))
        .and(path(format!("{prefix}/v1/query/aql")))
        .respond_with(answer)
        .mount(&server)
        .await;
    server
}

/// A JSON answer with `status` and `body`.
fn json(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

/// Dispatches `NODE_AQL` to `server` under `/openehr` and returns the reply.
async fn dispatch_to(server: &Server) -> Result<NodeReply, Box<dyn Error>> {
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    Ok(client
        .query(&NodeQuery::new(NODE_AQL), &within(Duration::from_secs(5))?)
        .await?)
}

/// The text of a reply's `error`, which every failure carries.
fn error_text(reply: &NodeReply) -> Result<String, Box<dyn Error>> {
    match reply.outcome().error() {
        Some(ErrorDetail::Text(text)) => Ok(text.clone()),
        Some(other) => Err(format!("an unexpected structured error: {other:?}").into()),
        None => Err("the failure carries no error".into()),
    }
}

/// The requests the mock node received.
async fn received(server: &Server) -> Result<Vec<Request>, Box<dyn Error>> {
    server
        .received_requests()
        .await
        .ok_or_else(|| "request recording is off".into())
}

#[tokio::test]
async fn a_result_set_is_active_and_carries_the_rows() -> TestResult {
    let server = node_answering("/openehr", json(200, EMPTY_RESULT_SET)).await;
    let reply = dispatch_to(&server).await?;
    assert_eq!(reply.status(), EndpointStatus::Active);
    match reply {
        NodeReply::Answered { result_set, .. } => {
            assert!(result_set.rows.is_empty());
            assert_eq!(result_set.columns.map(|columns| columns.len()), Some(1));
        }
        NodeReply::Failed { outcome, .. } => {
            return Err(format!("expected a result set, got {outcome:?}").into());
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_documented_400_is_a_node_error_with_the_nodes_status_and_message() -> TestResult {
    let server = node_answering(
        "/openehr",
        json(400, r#"{"message":"unknown archetype path in WHERE"}"#),
    )
    .await;
    let reply = dispatch_to(&server).await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError);
    let error = error_text(&reply)?;
    assert!(error.contains("400 Bad Request"), "{error}");
    assert!(error.contains("unknown archetype path in WHERE"), "{error}");
    Ok(())
}

#[tokio::test]
async fn a_documented_408_is_a_node_error_not_a_time_out() -> TestResult {
    let server = node_answering(
        "/openehr",
        json(408, r#"{"message":"query took too long"}"#),
    )
    .await;
    let reply = dispatch_to(&server).await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError);
    assert!(error_text(&reply)?.contains("408 Request Timeout"));
    Ok(())
}

#[tokio::test]
async fn a_401_is_a_node_error() -> TestResult {
    let server = node_answering("/openehr", ResponseTemplate::new(401)).await;
    let reply = dispatch_to(&server).await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError);
    assert_eq!(error_text(&reply)?, "the node answered 401 Unauthorized");
    Ok(())
}

#[tokio::test]
async fn a_403_is_a_node_error() -> TestResult {
    let server = node_answering("/openehr", ResponseTemplate::new(403)).await;
    let reply = dispatch_to(&server).await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError);
    assert_eq!(error_text(&reply)?, "the node answered 403 Forbidden");
    Ok(())
}

#[tokio::test]
async fn a_5xx_is_a_node_error_never_offline() -> TestResult {
    let server = node_answering(
        "/openehr",
        ResponseTemplate::new(503).set_body_string("maintenance window"),
    )
    .await;
    let reply = dispatch_to(&server).await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError);
    let error = error_text(&reply)?;
    assert!(error.contains("503 Service Unavailable"), "{error}");
    assert!(error.contains("maintenance window"), "{error}");
    Ok(())
}

/// Dispatches `NODE_AQL` to `server` under `/openehr`, withholding
/// [`SUBJECT`], and returns the text of the reply's `error`.
async fn error_withholding_the_subject(server: &Server) -> Result<String, Box<dyn Error>> {
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let options = within(Duration::from_secs(5))?
        .with_withheld(Arc::new(Withheld::new([SecretString::from(SUBJECT)])));
    let reply = client.query(&NodeQuery::new(NODE_AQL), &options).await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError, "§11.1");
    error_text(&reply)
}

#[tokio::test]
async fn a_node_message_carrying_the_withheld_subject_is_masked() -> TestResult {
    // The right-to-left override goes in as its JSON escape, so the source
    // carries no such mark.
    let rlo = format!("\\u{:04x}", 0x202e);
    let said = format!(
        r#"{{"message":"no EHR for subject {SUBJECT}\n{rlo} at this node; '{}' unknown"}}"#,
        SUBJECT.replace('\'', "\\\\'")
    );
    let server = node_answering("/openehr", json(400, &said)).await;
    let error = error_withholding_the_subject(&server).await?;
    assert_eq!(
        format!(
            "the node answered 400 Bad Request: no EHR for subject {MASK} at this node; '{MASK}' unknown"
        ),
        error,
        "§9.5: the node's status and message; §5.4.1, N33: never the subject"
    );
    Ok(())
}

#[tokio::test]
async fn a_node_message_carrying_the_subject_percent_encoded_is_withheld_whole() -> TestResult {
    let encoded = SUBJECT.replace('\'', "%27").replace('-', "%2D");
    let server = node_answering(
        "/openehr",
        ResponseTemplate::new(503).set_body_string(format!("no route for ?subject={encoded}")),
    )
    .await;
    let error = error_withholding_the_subject(&server).await?;
    assert_eq!(
        format!("the node answered 503 Service Unavailable: {MASK}"),
        error
    );
    assert!(!error.contains(&encoded), "{error}");
    Ok(())
}

#[tokio::test]
async fn a_long_node_message_is_cut_to_the_limit() -> TestResult {
    let long = "x".repeat(MESSAGE_LIMIT * 2);
    let server = node_answering("/openehr", json(400, &format!(r#"{{"message":"{long}"}}"#))).await;
    let error = error_text(&dispatch_to(&server).await?)?;
    let prefix = "the node answered 400 Bad Request: ";
    let kept = error.strip_prefix(prefix).ok_or("the status leads")?;
    assert_eq!(MESSAGE_LIMIT + 1, kept.chars().count(), "{kept}");
    assert!(kept.ends_with('…'), "{kept}");
    Ok(())
}

#[tokio::test]
async fn an_undocumented_status_is_a_node_error_with_that_status() -> TestResult {
    let server = node_answering("/openehr", ResponseTemplate::new(404)).await;
    let reply = dispatch_to(&server).await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError);
    assert_eq!(error_text(&reply)?, "the node answered 404 Not Found");
    Ok(())
}

// NOTE: ITS-REST 1.1.0 declares no 3xx for the query, so a redirect is an undocumented
// answer and is never followed to the host its Location names (§11.1).
#[tokio::test]
async fn a_redirect_is_a_node_error_and_is_never_followed() -> TestResult {
    let elsewhere = node_answering("/openehr", json(200, EMPTY_RESULT_SET)).await;
    let location = format!("{}/openehr/v1/query/aql", elsewhere.uri());
    let server = node_answering(
        "/openehr",
        ResponseTemplate::new(307).insert_header("location", location.as_str()),
    )
    .await;
    let reply = dispatch_to(&server).await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError);
    assert_eq!(
        error_text(&reply)?,
        "the node answered 307 Temporary Redirect"
    );
    assert!(
        received(&elsewhere).await?.is_empty(),
        "the query and its credentials go to the registered endpoint only"
    );
    Ok(())
}

#[tokio::test]
async fn a_200_that_is_not_a_result_set_is_a_node_error() -> TestResult {
    let server = node_answering("/openehr", json(200, r#"{"rows":"not rows"}"#)).await;
    let reply = dispatch_to(&server).await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError);
    let error = error_text(&reply)?;
    assert!(error.contains("not an ITS-REST RESULT_SET"), "{error}");
    assert!(error.contains("rows"), "{error}");
    assert!(
        !error.contains("not rows"),
        "the defect report echoed the node's data: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn a_row_shorter_than_the_query_selects_is_a_node_error() -> TestResult {
    let answer =
        r##"{"columns":[{"name":"#0"},{"name":"#1"}],"rows":[["a","b"],["SYNTHETIC-CELL"]]}"##;
    let server = node_answering("/openehr", json(200, answer)).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let reply = client
        .query(
            &NodeQuery::new(NODE_AQL).with_width(2),
            &within(Duration::from_secs(5))?,
        )
        .await?;
    assert_eq!(
        reply.status(),
        EndpointStatus::NodeError,
        "an answer the gateway cannot use is node-error (§11.1)"
    );
    let error = error_text(&reply)?;
    assert_eq!(
        error,
        "the node answered a row with 1 cells where the dispatched query selects 2"
    );
    Ok(())
}

#[tokio::test]
async fn rows_as_wide_as_the_query_selects_are_active() -> TestResult {
    let answer = r##"{"columns":[{"name":"#0"},{"name":"#1"}],"rows":[["a","b","c"],["d","e"]]}"##;
    let server = node_answering("/openehr", json(200, answer)).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let reply = client
        .query(
            &NodeQuery::new(NODE_AQL).with_width(2),
            &within(Duration::from_secs(5))?,
        )
        .await?;
    assert_eq!(reply.status(), EndpointStatus::Active);
    Ok(())
}

#[tokio::test]
async fn no_answer_before_the_deadline_is_a_time_out() -> TestResult {
    let server = node_answering(
        "/openehr",
        json(200, EMPTY_RESULT_SET).set_delay(Duration::from_secs(3)),
    )
    .await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let reply = client
        .query(
            &NodeQuery::new(NODE_AQL),
            &within(Duration::from_millis(200))?,
        )
        .await?;
    assert_eq!(reply.status(), EndpointStatus::TimeOut);
    assert!(error_text(&reply)?.starts_with("no answer before the deadline"));
    assert!(reply.outcome().latency_ms().is_some());
    Ok(())
}

#[tokio::test]
async fn a_deadline_already_passed_is_a_time_out_and_sends_nothing() -> TestResult {
    let server = node_answering("/openehr", json(200, EMPTY_RESULT_SET)).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let reply = client
        .query(
            &NodeQuery::new(NODE_AQL),
            &DispatchOptions::new(Instant::now(), crate::conveyed::conveyance()),
        )
        .await?;
    assert_eq!(reply.status(), EndpointStatus::TimeOut);
    assert!(received(&server).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_refused_connection_is_offline_with_a_reason() -> TestResult {
    let client = client_at(&format!("{}/openehr", unreachable::BASE))?;
    let reply = client
        .query(&NodeQuery::new(NODE_AQL), &within(Duration::from_secs(5))?)
        .await?;
    assert_eq!(reply.status(), EndpointStatus::Offline);
    let error = error_text(&reply)?;
    assert!(
        error.starts_with("the node could not be reached: "),
        "{error}"
    );
    assert!(
        error.len() > "the node could not be reached: ".len(),
        "the refusal carries no reason: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn a_withheld_value_in_the_reason_a_node_was_unreachable_is_masked() -> TestResult {
    let base = format!("{}/openehr", unreachable::BASE);
    let client = client_at(&base)?;
    let withheld = Withheld::new([SecretString::from(UNREACHABLE_HOST)]);
    let options = within(Duration::from_secs(5))?.with_withheld(Arc::new(withheld));
    let reply = client.query(&NodeQuery::new(NODE_AQL), &options).await?;
    assert_eq!(reply.status(), EndpointStatus::Offline, "§11.1");
    let error = error_text(&reply)?;
    assert!(
        error.starts_with("the node could not be reached: "),
        "{error}"
    );
    assert!(!error.contains(UNREACHABLE_HOST), "{error}");
    assert!(error.contains(MASK), "the reason named the host: {error}");
    Ok(())
}

#[tokio::test]
async fn a_broken_stream_is_offline() -> TestResult {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let closer = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let client = client_at(&format!("http://{address}/openehr"))?;
    let reply = client
        .query(&NodeQuery::new(NODE_AQL), &within(Duration::from_secs(5))?)
        .await?;
    closer.await?;
    assert_eq!(reply.status(), EndpointStatus::Offline);
    assert!(!error_text(&reply)?.is_empty());
    Ok(())
}

/// A provider that never produces a credential.
#[derive(Debug)]
struct NoCredential;

#[async_trait::async_trait]
impl CredentialsProvider for NoCredential {
    async fn credentials(&self) -> Result<Credentials, CredentialsError> {
        Err(CredentialsError::new("the token endpoint is unreachable"))
    }
}

/// An onward credential that cannot be obtained fails the node as
/// `node-error` carrying the provider's account, and sends nothing: the
/// gateway never dispatches unauthenticated (§13.1, N25, §11.1).
// conformance: CP-17
#[tokio::test]
async fn a_missing_credential_is_a_node_error_and_sends_nothing() -> TestResult {
    let server = node_answering("/openehr", json(200, EMPTY_RESULT_SET)).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?
        .with_credentials_provider(Arc::new(NoCredential));
    let reply = client
        .query(&NodeQuery::new(NODE_AQL), &within(Duration::from_secs(5))?)
        .await?;
    assert_eq!(EndpointStatus::NodeError, reply.status());
    assert!(!reply.contact().sent(), "{:?}", reply.contact());
    let text = error_text(&reply)?;
    assert_eq!(UNAUTHENTICATED, text, "the provider account is not shown");
    assert!(received(&server).await?.is_empty());
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn the_request_id_a_node_receives_is_the_minted_outbound_id_byte_for_byte() -> TestResult {
    let server = node_answering("/openehr", json(200, EMPTY_RESULT_SET)).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let id = OutboundId::mint();
    let options = within(Duration::from_secs(5))?.with_request_id(id);
    client.query(&NodeQuery::new(NODE_AQL), &options).await?;
    let requests = received(&server).await?;
    let [request] = requests.as_slice() else {
        return Err(format!("expected one request, got {}", requests.len()).into());
    };
    let sent: Vec<&[u8]> = request
        .headers
        .get_all(REQUEST_ID_HEADER)
        .iter()
        .map(http::HeaderValue::as_bytes)
        .collect();
    assert_eq!(vec![id.to_string().as_bytes()], sent, "one field line");
    Ok(())
}

/// Every header name a node request may carry, lowercase, with where its
/// value comes from: the inventory of `ferrofed_engine::outbound_id`.
const INVENTORY: [&str; 8] = [
    "accept",                    // the client runtime's default
    "accept-encoding",           // the HTTP engine, from its decompression features
    "authorization",             // the endpoint's configured onward credential
    "content-length",            // the HTTP engine, from the composed body
    "content-type",              // the client runtime, for the JSON body
    "host",                      // the HTTP engine, from the registry URL
    "openehr-federation-client", // the caller's identity, signed for the node
    "x-request-id",              // the minted outbound id
];

// conformance: CP-26
#[tokio::test]
async fn every_header_a_node_receives_is_in_the_inventory() -> TestResult {
    let server = node_answering("/openehr", json(200, EMPTY_RESULT_SET)).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?
        .with_credentials_provider(Arc::new(Credentials::bearer("synthetic-token")));
    let options = within(Duration::from_secs(5))?.with_request_id(OutboundId::mint());
    client.query(&NodeQuery::new(NODE_AQL), &options).await?;
    let requests = received(&server).await?;
    let [request] = requests.as_slice() else {
        return Err(format!("expected one request, got {}", requests.len()).into());
    };
    let names: Vec<&str> = {
        let mut names: Vec<&str> = request
            .headers
            .keys()
            .map(http::HeaderName::as_str)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    assert_eq!(INVENTORY.to_vec(), names, "no header outside the inventory");
    for name in INVENTORY {
        assert_eq!(
            1,
            request.headers.get_all(name).iter().count(),
            "{name} is one field line"
        );
    }
    Ok(())
}

#[tokio::test]
async fn the_base_url_is_used_verbatim_with_a_path_prefix() -> TestResult {
    let server = node_answering("/rest/openehr", json(200, EMPTY_RESULT_SET)).await;
    let client = client_at(&format!("{}/rest/openehr", server.uri()))?;
    assert_eq!(client.base().path(), "/rest/openehr/v1");
    let reply = client
        .query(&NodeQuery::new(NODE_AQL), &within(Duration::from_secs(5))?)
        .await?;
    assert_eq!(reply.status(), EndpointStatus::Active);
    Ok(())
}

#[tokio::test]
async fn the_base_url_is_used_verbatim_without_a_prefix() -> TestResult {
    let server = node_answering("", json(200, EMPTY_RESULT_SET)).await;
    let client = client_at(&server.uri())?;
    assert_eq!(client.base().path(), "/v1");
    let reply = client
        .query(&NodeQuery::new(NODE_AQL), &within(Duration::from_secs(5))?)
        .await?;
    assert_eq!(reply.status(), EndpointStatus::Active);
    Ok(())
}

#[tokio::test]
async fn a_trailing_slash_on_the_base_url_adds_no_empty_segment() -> TestResult {
    let server = node_answering("/openehr", json(200, EMPTY_RESULT_SET)).await;
    let client = client_at(&format!("{}/openehr/", server.uri()))?;
    assert_eq!(client.base().path(), "/openehr/v1");
    let reply = client
        .query(&NodeQuery::new(NODE_AQL), &within(Duration::from_secs(5))?)
        .await?;
    assert_eq!(reply.status(), EndpointStatus::Active);
    Ok(())
}

#[tokio::test]
async fn the_request_id_and_the_page_travel_and_nothing_else_is_added() -> TestResult {
    let server = node_answering("/openehr", json(200, EMPTY_RESULT_SET)).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let id = OutboundId::mint();
    let options = within(Duration::from_secs(5))?.with_request_id(id);
    let query = NodeQuery::new(NODE_AQL).with_offset(0).with_fetch(11);
    client.query(&query, &options).await?;
    let requests = received(&server).await?;
    let [request] = requests.as_slice() else {
        return Err(format!("expected one request, got {}", requests.len()).into());
    };
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(request.url.query(), None);
    assert_eq!(
        request
            .headers
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(id.to_string().as_str())
    );
    assert_eq!(request.headers.get("authorization"), None);
    let body = std::str::from_utf8(&request.body)?;
    assert!(body.contains(r#""offset":0"#), "{body}");
    assert!(body.contains(r#""fetch":11"#), "{body}");
    assert!(!body.contains("query_parameters"), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_sentinel_in_the_query_reaches_only_the_body() -> TestResult {
    let sentinel = "SENTINEL-2.999.4711";
    let server = node_answering("/openehr", json(200, EMPTY_RESULT_SET)).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let aql = format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c WHERE c/name/value = '{sentinel}'"
    );
    let options = within(Duration::from_secs(5))?.with_request_id(OutboundId::mint());
    client.query(&NodeQuery::new(aql), &options).await?;
    let requests = received(&server).await?;
    let [request] = requests.as_slice() else {
        return Err(format!("expected one request, got {}", requests.len()).into());
    };
    assert!(!request.url.as_str().contains(sentinel), "{}", request.url);
    for (name, value) in &request.headers {
        let carries = value
            .as_bytes()
            .windows(sentinel.len())
            .any(|window| window == sentinel.as_bytes());
        assert!(
            !name.as_str().contains(sentinel) && !carries,
            "the header {name} carries the sentinel"
        );
    }
    assert!(std::str::from_utf8(&request.body)?.contains(sentinel));
    Ok(())
}

#[tokio::test]
async fn a_failure_report_never_echoes_the_query() -> TestResult {
    let sentinel = "SENTINEL-2.999.4712";
    let server = node_answering("/openehr", ResponseTemplate::new(500)).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let aql = format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c WHERE c/name/value = '{sentinel}'"
    );
    let reply = client
        .query(&NodeQuery::new(aql), &within(Duration::from_secs(5))?)
        .await?;
    assert_eq!(reply.status(), EndpointStatus::NodeError);
    assert!(!format!("{reply:?}").contains(sentinel));
    assert!(!error_text(&reply)?.contains(sentinel));
    Ok(())
}

#[tokio::test]
async fn the_snapshot_gives_one_client_per_endpoint_with_its_own_credentials() -> TestResult {
    let first = Server::start().await;
    let second = Server::start().await;
    for server in [&first, &second] {
        Mock::given(method("POST"))
            .and(path("/openehr/v1/query/aql"))
            .respond_with(json(200, EMPTY_RESULT_SET))
            .mount(server)
            .await;
    }
    let snapshot = snapshot(&[
        ("node-a-one", &format!("{}/openehr", first.uri())),
        ("node-a-two", &format!("{}/openehr", second.uri())),
    ])?;
    let mut credentials: BTreeMap<EndpointId, SharedCredentials> = BTreeMap::new();
    credentials.insert(
        EndpointId::new("node-a-one")?,
        Arc::new(Credentials::bearer("synthetic-token")),
    );
    let clients = NodeClients::from_snapshot(&snapshot, &transport()?, &credentials)?;
    assert_eq!(clients.len(), 2);
    let options = within(Duration::from_secs(5))?;
    for client in clients.iter() {
        client.query(&NodeQuery::new(NODE_AQL), &options).await?;
    }
    let with = received(&first).await?;
    let without = received(&second).await?;
    assert_eq!(
        with.first()
            .and_then(|request| request.headers.get("authorization"))
            .and_then(|value| value.to_str().ok()),
        Some("Bearer synthetic-token")
    );
    assert_eq!(
        without
            .first()
            .map(|request| request.headers.contains_key("authorization")),
        Some(false)
    );
    Ok(())
}

#[test]
fn credentials_for_an_endpoint_the_snapshot_lacks_are_refused() -> TestResult {
    let snapshot = snapshot(&[("node-a-pub", "https://cdr-a.example.org/openehr")])?;
    let mut credentials: BTreeMap<EndpointId, SharedCredentials> = BTreeMap::new();
    credentials.insert(
        EndpointId::new("node-z-pub")?,
        Arc::new(Credentials::bearer("synthetic-token")),
    );
    let refused = NodeClients::from_snapshot(&snapshot, &transport()?, &credentials);
    assert!(
        matches!(refused, Err(SetupError::UnknownEndpoint { .. })),
        "{refused:?}"
    );
    Ok(())
}

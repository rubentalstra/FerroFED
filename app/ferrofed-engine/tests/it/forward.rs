// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Single-node forwarding against a mock node (§7a.3, N22, N31, N33): the
//! body arrives byte for byte, only the client headers and query parameters
//! the ITS-REST operation declares travel, each held to the kind the
//! operation declares for it, the outbound gate reads the URL and
//! the headers of a forwarded request, and the answer comes back as the node
//! sent it. Asserted on what the mock node received.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrofed_engine::declared::Refusal;
use ferrofed_engine::dispatch::{Contact, DispatchOptions, NodeClient};
use ferrofed_engine::forward::{ClientRequest, ForwardError};
use ferrofed_engine::hygiene::{Part, Withheld};
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_registry::id::EhrId;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_testkit::mock::Server;
use http::{HeaderMap, Method, StatusCode};
use openehr_its::rest::client::ReqwestTransport;
use secrecy::SecretString;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

type TestResult = Result<(), Box<dyn Error>>;

/// The synthetic patient identifier.
const PATIENT: &str = "Sentinel-7731";

/// A node-local `ehr_id`.
const EHR: &str = "7d44b88c-4199-4bad-97dc-d78268e01398";

fn client(url: &str) -> Result<NodeClient<ReqwestTransport>, Box<dyn Error>> {
    let snapshot = RegistrySnapshot::from_toml_str(&format!(
        "[[organisation]]\nid = \"org-a\"\n\n[[node]]\nid = \"node-a\"\norganisation = \"org-a\"\nsystem_id = \"cdr-a.example.org\"\n\n[[endpoint]]\nid = \"node-a-pub\"\nnode = \"node-a\"\nurl = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n"
    ))?;
    let endpoint = snapshot.endpoints().next().ok_or("one endpoint")?;
    Ok(NodeClient::new(
        endpoint,
        ReqwestTransport::with_timeout(Duration::from_secs(10))?,
    )?)
}

fn options_under(
    withheld: Withheld,
    outbound: OutboundId,
) -> Result<DispatchOptions, Box<dyn Error>> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .ok_or("the deadline is past the platform clock")?;
    Ok(
        DispatchOptions::new(deadline, crate::conveyed::conveyance())
            .with_request_id(outbound)
            .with_withheld(Arc::new(withheld)),
    )
}

fn options(withheld: Withheld) -> Result<DispatchOptions, Box<dyn Error>> {
    options_under(withheld, OutboundId::mint())
}

fn patient() -> Withheld {
    Withheld::new([SecretString::from(PATIENT)])
}

async fn node(verb: &str, at: &str, answer: ResponseTemplate) -> Server {
    let server = Server::start().await;
    Mock::given(method(verb))
        .and(path(at))
        .respond_with(answer)
        .mount(&server)
        .await;
    server
}

fn request(verb: Method, at: &str, headers: HeaderMap, body: &[u8]) -> ClientRequest {
    ClientRequest {
        method: verb,
        path: at.to_owned(),
        query: None,
        headers,
        body: body.to_vec(),
    }
}

async fn received(server: &Server) -> Result<Vec<wiremock::Request>, Box<dyn Error>> {
    Ok(server.received_requests().await.ok_or("recording is on")?)
}

// conformance: CP-24
#[tokio::test]
async fn the_body_arrives_byte_for_byte_and_only_the_declared_headers_travel() -> TestResult {
    let at = format!("/ehr/{EHR}/composition");
    let server = node(
        "POST",
        &format!("/v1{at}"),
        ResponseTemplate::new(201).insert_header("ETag", "\"u::cdr-a.example.org::1\""),
    )
    .await;
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse()?);
    headers.insert("authorization", "Bearer client-token".parse()?);
    headers.insert("x-request-id", "req-client-1".parse()?);
    headers.insert("x-patient", PATIENT.parse()?);
    let body = format!(
        "{{\"_type\":\"COMPOSITION\",  \"identifiers\":[{{\"_type\":\"DV_IDENTIFIER\",\"id\":\"{PATIENT}\"}}]}}"
    );
    let outbound = OutboundId::mint();
    let forwarded = client(&server.uri())?
        .forward(
            request(Method::POST, &at, headers, body.as_bytes()),
            &options_under(Withheld::none(), outbound)?,
        )
        .await?;
    assert_eq!(StatusCode::CREATED, forwarded.status());
    assert_eq!(
        Some("\"u::cdr-a.example.org::1\""),
        forwarded
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
    );
    let requests = received(&server).await?;
    let sent = requests.first().ok_or("one request")?;
    assert_eq!(body.as_bytes(), sent.body.as_slice(), "byte for byte (N33)");
    let request_ids: Vec<&[u8]> = sent
        .headers
        .get_all("x-request-id")
        .iter()
        .map(http::HeaderValue::as_bytes)
        .collect();
    assert_eq!(
        vec![outbound.to_string().as_bytes()],
        request_ids,
        "the node receives the minted id alone, never the client's (N33)"
    );
    assert_eq!(
        Some(b"application/json".as_slice()),
        sent.headers
            .get("content-type")
            .map(http::HeaderValue::as_bytes)
    );
    assert!(sent.headers.get("authorization").is_none());
    assert!(sent.headers.get("x-patient").is_none());
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn a_header_the_operation_declares_travels_and_one_it_does_not_is_stripped() -> TestResult {
    let uid = "8849182c-82ad-4088-a07f-48ead4180515::cdr-a.example.org::1";
    // NOTE: ITS-REST EHR API, PUT composition addresses the versioned_object_uid (format uuid) and
    // names the preceding version in If-Match, so the path carries the object id, not the version.
    let at = format!("/ehr/{EHR}/composition/8849182c-82ad-4088-a07f-48ead4180515");
    let server = Server::start().await;
    for verb in ["PUT", "GET"] {
        Mock::given(method(verb))
            .and(path(format!("/v1{at}")))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
    }
    let client = client(&server.uri())?;
    let mut headers = HeaderMap::new();
    headers.insert("if-match", format!("\"{uid}\"").parse()?);
    headers.insert("x-patient", PATIENT.parse()?);
    for verb in [Method::PUT, Method::GET] {
        let sent = request(verb, &at, headers.clone(), b"");
        client.forward(sent, &options(Withheld::none())?).await?;
    }
    let requests = received(&server).await?;
    let [update, read] = requests.as_slice() else {
        return Err(format!("two requests, got {}", requests.len()).into());
    };
    assert_eq!(
        Some(format!("\"{uid}\"").as_bytes()),
        update
            .headers
            .get("if-match")
            .map(http::HeaderValue::as_bytes),
        "PUT composition declares If-Match (ITS-REST, EHR API)"
    );
    assert!(
        read.headers.get("if-match").is_none(),
        "GET composition declares no If-Match, so it is stripped"
    );
    for sent in [update, read] {
        assert!(sent.headers.get("x-patient").is_none());
    }
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn a_query_parameter_the_operation_does_not_declare_is_refused_unsent() -> TestResult {
    let server = Server::start().await;
    let at = format!("/ehr/{EHR}/composition");
    let mut commit = request(Method::POST, &at, HeaderMap::new(), b"{}");
    commit.query = Some("version_at_time=2026-01-01T00:00:00Z".to_owned());
    let refused = client(&server.uri())?
        .forward(commit, &options(Withheld::none())?)
        .await;
    assert!(
        matches!(
            &refused,
            Err(ForwardError::QueryParameter(unlisted)) if unlisted.position == 1
        ),
        "{refused:?}"
    );
    assert!(received(&server).await?.is_empty(), "nothing is sent");
    Ok(())
}

#[tokio::test]
async fn a_request_that_names_no_operation_is_never_sent() -> TestResult {
    let server = Server::start().await;
    let unrouted = request(Method::PATCH, &format!("/ehr/{EHR}"), HeaderMap::new(), b"");
    let refused = client(&server.uri())?
        .forward(unrouted, &options(Withheld::none())?)
        .await;
    assert!(
        matches!(&refused, Err(ForwardError::Unrouted)),
        "{refused:?}"
    );
    assert!(received(&server).await?.is_empty(), "nothing is sent");
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn a_withheld_identifier_in_the_path_or_a_forwarded_header_is_never_sent() -> TestResult {
    let server = Server::start().await;
    let client = client(&server.uri())?;
    let in_path = request(
        Method::GET,
        &format!("/ehr/{PATIENT}"),
        HeaderMap::new(),
        b"",
    );
    let refused = client.forward(in_path, &options(patient())?).await;
    assert!(
        matches!(
            &refused,
            Err(ForwardError::Withheld {
                part: Part::Url,
                ..
            })
        ),
        "{refused:?}"
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        "openehr-audit-details",
        format!("committer.id={PATIENT}").parse()?,
    );
    let in_header = request(
        Method::POST,
        &format!("/ehr/{EHR}/composition"),
        headers,
        b"",
    );
    let refused = client.forward(in_header, &options(patient())?).await;
    assert!(
        matches!(
            &refused,
            Err(ForwardError::Withheld {
                part: Part::Header("openehr-audit-details"),
                ..
            })
        ),
        "{refused:?}"
    );
    let shown = refused.err().ok_or("refused")?.to_string();
    assert!(!shown.contains(PATIENT), "{shown}");
    assert!(received(&server).await?.is_empty(), "nothing is sent");
    Ok(())
}

/// A short synthetic identifier that occurs inside [`EHR`].
const SHORT: &str = "4199";

/// Options withholding [`SHORT`], naming [`EHR`] as the `ehr_id` the gateway
/// composed into the path when `composed` is set.
fn short_options(composed: bool) -> Result<DispatchOptions, Box<dyn Error>> {
    let options = options(Withheld::new([SecretString::from(SHORT)]))?;
    Ok(if composed {
        options.with_composed_ehr_id(EhrId::new(EHR)?)
    } else {
        options
    })
}

// conformance: CP-26
#[tokio::test]
async fn an_identifier_inside_the_composed_ehr_id_is_forwarded() -> TestResult {
    let server = node("GET", &format!("/v1/ehr/{EHR}"), ResponseTemplate::new(200)).await;
    let read = request(Method::GET, &format!("/ehr/{EHR}"), HeaderMap::new(), b"");
    let answer = client(&server.uri())?
        .forward(read, &short_options(true)?)
        .await?;
    assert_eq!(
        StatusCode::OK,
        answer.status(),
        "§5.4, N33: the composed ehr_id is masked"
    );
    assert_eq!(1, received(&server).await?.len(), "the node is asked once");
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn a_withheld_value_in_the_registry_path_is_never_forwarded() -> TestResult {
    let server = Server::start().await;
    let read = request(Method::GET, &format!("/ehr/{EHR}"), HeaderMap::new(), b"");
    let refused = client(&format!("{}/cdr-{SHORT}", server.uri()))?
        .forward(read, &short_options(true)?)
        .await;
    assert!(
        matches!(
            &refused,
            Err(ForwardError::Withheld {
                part: Part::Url,
                ..
            })
        ),
        "the operator's base path is never masked, as on the fan-out: {refused:?}"
    );
    assert!(received(&server).await?.is_empty(), "nothing is sent");
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn the_same_identifier_in_a_part_the_client_wrote_is_still_withheld() -> TestResult {
    let server = Server::start().await;
    let client = client(&server.uri())?;
    let mut headers = HeaderMap::new();
    headers.insert(
        "openehr-audit-details",
        format!("committer.id={SHORT}").parse()?,
    );
    let in_header = request(
        Method::POST,
        &format!("/ehr/{EHR}/composition"),
        headers,
        b"",
    );
    let refused = client.forward(in_header, &short_options(true)?).await;
    assert!(
        matches!(
            &refused,
            Err(ForwardError::Withheld {
                part: Part::Header("openehr-audit-details"),
                ..
            })
        ),
        "a client header is never masked: {refused:?}"
    );
    let mut in_query = request(
        Method::GET,
        &format!("/ehr/{EHR}/directory"),
        HeaderMap::new(),
        b"",
    );
    in_query.query = Some(format!("path={SHORT}"));
    let refused = client.forward(in_query, &short_options(true)?).await;
    assert!(
        matches!(
            &refused,
            Err(ForwardError::Withheld {
                part: Part::Url,
                ..
            })
        ),
        "a client query value is never masked: {refused:?}"
    );
    let unmasked = request(Method::GET, &format!("/ehr/{EHR}"), HeaderMap::new(), b"");
    let refused = client.forward(unmasked, &short_options(false)?).await;
    assert!(
        matches!(
            &refused,
            Err(ForwardError::Withheld {
                part: Part::Url,
                ..
            })
        ),
        "an ehr_id segment the client wrote is never masked: {refused:?}"
    );
    assert!(received(&server).await?.is_empty(), "nothing is sent");
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn a_withheld_identifier_in_the_body_of_a_commit_is_sent_unchanged() -> TestResult {
    let at = format!("/ehr/{EHR}/composition");
    let server = node("POST", &format!("/v1{at}"), ResponseTemplate::new(201)).await;
    let body = format!("{{\"identifiers\":[{{\"id\":\"{PATIENT}\"}}]}}");
    client(&server.uri())?
        .forward(
            request(Method::POST, &at, HeaderMap::new(), body.as_bytes()),
            &options(patient())?,
        )
        .await?;
    let requests = received(&server).await?;
    assert_eq!(
        Some(body.as_bytes()),
        requests.first().map(|sent| sent.body.as_slice()),
        "the gate never reads a write body (§5.4 scope note)"
    );
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn the_host_a_node_receives_is_the_registry_authority_never_the_client_host() -> TestResult {
    let at = format!("/ehr/{EHR}");
    let server = node("GET", &format!("/v1{at}"), ResponseTemplate::new(200)).await;
    let mut headers = HeaderMap::new();
    headers.insert("host", format!("cdr-{PATIENT}.example.org").parse()?);
    let answer = client(&server.uri())?
        .forward(
            request(Method::GET, &at, headers, b""),
            &options(patient())?,
        )
        .await?;
    assert_eq!(StatusCode::OK, answer.status());
    let requests = received(&server).await?;
    let [sent] = requests.as_slice() else {
        return Err(format!("expected one request, got {}", requests.len()).into());
    };
    let hosts: Vec<&[u8]> = sent
        .headers
        .get_all("host")
        .iter()
        .map(http::HeaderValue::as_bytes)
        .collect();
    assert_eq!(
        vec![server.address().to_string().as_bytes()],
        hosts,
        "§5.4.1, N33: Host is written from the registry's endpoint URL"
    );
    assert!(
        sent.headers
            .values()
            .all(|value| !String::from_utf8_lossy(value.as_bytes()).contains(PATIENT)),
        "the client's Host never travels"
    );
    Ok(())
}

/// A node-local `ehr_id` with no digit in it, so a port number cannot occur
/// in a path that holds it.
const NO_DIGIT_EHR: &str = "abcdefab-cdef-abcd-efab-cdefabcdefab";

// conformance: CP-26
#[tokio::test]
async fn a_withheld_value_in_a_client_header_is_refused_though_the_registry_authority_holds_it()
-> TestResult {
    let server = Server::start().await;
    let port = server.address().port().to_string();
    assert!(server.uri().contains(&port), "the port is in the authority");
    let mut headers = HeaderMap::new();
    headers.insert(
        "openehr-audit-details",
        format!("committer.id={port}").parse()?,
    );
    let commit = request(
        Method::POST,
        &format!("/ehr/{NO_DIGIT_EHR}/composition"),
        headers,
        b"",
    );
    let refused = client(&server.uri())?
        .forward(
            commit,
            &options(Withheld::new([SecretString::from(port.as_str())]))?,
        )
        .await;
    assert!(
        matches!(
            &refused,
            Err(ForwardError::Withheld {
                part: Part::Header("openehr-audit-details"),
                ..
            })
        ),
        "§5.4.1, N33: the unread authority and Host leave every header searched: {refused:?}"
    );
    assert!(received(&server).await?.is_empty(), "nothing is sent");
    Ok(())
}

#[tokio::test]
async fn a_401_is_the_node_refusing_the_onward_credentials() -> TestResult {
    let at = format!("/ehr/{EHR}");
    let server = node(
        "GET",
        &format!("/v1{at}"),
        ResponseTemplate::new(401)
            .set_body_raw(br#"{"message":"no"}"#.to_vec(), "application/json"),
    )
    .await;
    let refused = client(&server.uri())?
        .forward(
            request(Method::GET, &at, HeaderMap::new(), b""),
            &options(Withheld::none())?,
        )
        .await;
    match refused {
        Err(ForwardError::Refused { status, body, .. }) => {
            assert_eq!(StatusCode::UNAUTHORIZED, status);
            assert_eq!(Some("no"), body.message());
        }
        other => return Err(format!("a 401 is Refused: {other:?}").into()),
    }
    Ok(())
}

#[tokio::test]
async fn a_forward_whose_deadline_passed_before_it_left_is_expired_and_never_sent() -> TestResult {
    let at = format!("/ehr/{EHR}");
    let server = node("GET", &format!("/v1{at}"), ResponseTemplate::new(200)).await;
    let passed = DispatchOptions::new(Instant::now(), crate::conveyed::conveyance());
    let forwarded = client(&server.uri())?
        .forward(request(Method::GET, &at, HeaderMap::new(), b""), &passed)
        .await;
    assert!(
        matches!(&forwarded, Err(ForwardError::Expired { .. })),
        "§11.5: the node was never asked: {forwarded:?}"
    );
    assert_eq!(Contact::Unsent, Contact::of_forwarded(&forwarded));
    assert!(received(&server).await?.is_empty(), "nothing is sent");
    Ok(())
}

#[tokio::test]
async fn a_forward_the_node_leaves_unanswered_is_a_time_out_of_a_silent_node() -> TestResult {
    let at = format!("/ehr/{EHR}");
    let server = node(
        "GET",
        &format!("/v1{at}"),
        ResponseTemplate::new(200).set_delay(Duration::from_secs(3)),
    )
    .await;
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(200))
        .ok_or("the deadline is past the platform clock")?;
    let forwarded = client(&server.uri())?
        .forward(
            request(Method::GET, &at, HeaderMap::new(), b""),
            &DispatchOptions::new(deadline, crate::conveyed::conveyance()),
        )
        .await;
    assert!(
        matches!(&forwarded, Err(ForwardError::TimeOut { .. })),
        "§11.1: sent, and no answer in time: {forwarded:?}"
    );
    assert_eq!(Contact::Silent, Contact::of_forwarded(&forwarded));
    assert_eq!(1, received(&server).await?.len(), "the request left");
    Ok(())
}

/// The status a node that answers every directory read `200` gives a read of
/// the directory at `query` with `headers`, and what the node received.
async fn directory_read(
    query: &str,
    headers: HeaderMap,
) -> Result<(Result<StatusCode, ForwardError>, Vec<wiremock::Request>), Box<dyn Error>> {
    let at = format!("/ehr/{EHR}/directory");
    let server = node("GET", &format!("/v1{at}"), ResponseTemplate::new(200)).await;
    let mut read = request(Method::GET, &at, headers, b"");
    read.query = Some(query.to_owned());
    let answered = client(&server.uri())?
        .forward(read, &options(Withheld::none())?)
        .await
        .map(|answer| answer.status());
    Ok((answered, received(&server).await?))
}

// conformance: CP-26
#[tokio::test]
async fn a_malformed_date_time_is_refused_and_no_node_is_asked() -> TestResult {
    let (answered, sent) =
        directory_read(&format!("version_at_time={PATIENT}"), HeaderMap::new()).await?;
    match answered {
        Err(ForwardError::Value(malformed)) => {
            let shown = malformed.to_string();
            assert!(shown.contains("query parameter 1"), "{shown}");
            assert!(!shown.contains(PATIENT), "{shown}");
        }
        other => return Err(format!("a malformed date-time is refused: {other:?}").into()),
    }
    assert!(sent.is_empty(), "nothing is sent");
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn a_well_formed_date_time_is_forwarded_byte_identical() -> TestResult {
    let query = "version_at_time=2015-01-20T19:30:22.765%2B01:00&path=folders%2Fone";
    let (answered, sent) = directory_read(query, HeaderMap::new()).await?;
    assert_eq!(StatusCode::OK, answered?);
    let [one] = sent.as_slice() else {
        return Err(format!("one request reaches the node, not {}", sent.len()).into());
    };
    assert_eq!(Some(query), one.url.query());
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn an_accept_that_admits_nothing_listed_is_not_acceptable_unsent() -> TestResult {
    let mut headers = HeaderMap::new();
    headers.insert(
        "accept",
        format!("application/json; patient={PATIENT}").parse()?,
    );
    let (answered, sent) = directory_read("", headers).await?;
    assert!(
        matches!(
            &answered,
            Err(ForwardError::Value(Refusal::NotAcceptable { .. }))
        ),
        "RFC 9110 §12.5.1: {answered:?}"
    );
    assert!(sent.is_empty(), "nothing is sent");
    Ok(())
}

/// The `Accept` a directory read sent with the client's `accept` reaches the
/// node with.
async fn accept_at_the_node(accept: Option<&str>) -> Result<Option<String>, Box<dyn Error>> {
    let mut headers = HeaderMap::new();
    if let Some(accept) = accept {
        headers.insert("accept", accept.parse()?);
    }
    let (answered, sent) = directory_read("", headers).await?;
    assert_eq!(StatusCode::OK, answered?, "the node answers the read");
    let [one] = sent.as_slice() else {
        return Err(format!("one request reaches the node, not {}", sent.len()).into());
    };
    Ok(one
        .headers
        .get("accept")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned))
}

// conformance: CP-26
#[tokio::test]
async fn any_media_type_reaches_the_node_as_the_first_listed() -> TestResult {
    for accept in [Some("*/*"), None] {
        assert_eq!(
            Some("application/json".to_owned()),
            accept_at_the_node(accept).await?,
            "{accept:?}"
        );
    }
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn an_accept_list_reaches_the_node_as_its_best_listed_match() -> TestResult {
    let list = format!("text/html, application/xml;q=0.8;patient={PATIENT}, application/xml;q=0.5");
    assert_eq!(
        Some("application/xml".to_owned()),
        accept_at_the_node(Some(&list)).await?
    );
    Ok(())
}

// conformance: CP-26
#[tokio::test]
async fn a_free_text_parameter_passes_unclassified() -> TestResult {
    let query = format!("path={PATIENT}");
    let (answered, sent) = directory_read(&query, HeaderMap::new()).await?;
    assert_eq!(StatusCode::OK, answered?);
    assert_eq!(
        1,
        sent.len(),
        "§5.4.1, N33: free text cannot be classified, so it travels"
    );
    Ok(())
}

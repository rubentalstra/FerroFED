// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A commit routed to one FerroEHR node through the gateway: it lands
//! byte-identical, its `DV_IDENTIFIER` included, with the node's `Location`
//! and `ETag` and the acting endpoint's headers on the answer (§7a.3, N22,
//! N31, track 10). An update of that composition reaches node A, which
//! created it, and is refused `409` with no node written when it names node B
//! (§10.3, §12.4, §12a.1, N23).

use axum::body::Body;
use ferrofed_engine::onward::conveyance;
use ferrofed_testkit::containers::{self, API_PATH};
use ferrofed_testkit::proxy::Capture;
use ferrofed_testkit::seed::{self, EhrSeed, SeedPlan};
use http::{Request, StatusCode, header};

use crate::e2e::{EHR_A, PATIENT, TestResult, composition_carrying, gateway};
use crate::support::{CLIENT_TOKEN, searched_claims};

/// A request of `verb` to `uri` naming node A in the endpoint header.
fn routed_to_a(verb: http::Method, uri: &str, body: Body) -> Result<Request<Body>, http::Error> {
    Request::builder()
        .method(verb)
        .uri(uri)
        .header("openEHR-federation-endpoint", "node-a-pub")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", *CLIENT_TOKEN))
        .body(body)
}

/// The text of response header `name`.
fn field<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Whether `capture` carries `needle` outside its body: in the path, the
/// query or a header.
///
/// The gateway's own `openEHR-federation-client` token is searched as the
/// claims a node decodes from it ([`searched_claims`]); a token that does
/// not decode counts as carrying the needle.
fn outside_the_body(capture: &Capture, needle: &[u8]) -> bool {
    let found = |haystack: &[u8]| haystack.windows(needle.len()).any(|w| w == needle);
    found(capture.path.as_bytes())
        || capture
            .query
            .as_deref()
            .is_some_and(|query| found(query.as_bytes()))
        || capture.headers.iter().any(|(name, value)| {
            if name.eq_ignore_ascii_case(conveyance::HEADER) {
                return std::str::from_utf8(value)
                    .ok()
                    .and_then(|token| searched_claims(token).ok())
                    .is_none_or(|claims| found(claims.as_bytes()));
            }
            found(name.as_bytes()) || found(value)
        })
}

/// Asserts that node A's journal holds the one commit, byte-identical to
/// `sent`, that no request carried the identifier, the client's credential or
/// a federation header outside its body, and that node B was never asked.
fn assert_landed_as_sent(nodes: &containers::TwoNodes, sent: &str) -> TestResult {
    let journal = nodes.a.proxy.journal();
    let commits: Vec<_> = journal
        .iter()
        .filter(|capture| capture.method == "POST")
        .collect();
    assert_eq!(1, commits.len(), "one commit reached node A");
    let landed = commits.first().ok_or("one commit")?;
    assert_eq!(
        format!("{API_PATH}/v1/ehr/{EHR_A}/composition"),
        landed.path,
        "the client's path, under the node's own base (N28)"
    );
    assert_eq!(
        sent.as_bytes(),
        landed.body.as_slice(),
        "byte-identical, the DV_IDENTIFIER included (track 10)"
    );
    for capture in &journal {
        assert!(
            !outside_the_body(capture, PATIENT.value().as_bytes()),
            "no identifier outside the body (N33)"
        );
        assert!(
            !outside_the_body(capture, CLIENT_TOKEN.as_bytes()),
            "the client's credential stays at the gateway"
        );
        assert!(
            !outside_the_body(capture, b"openehr-federation"),
            "the federation's own headers stay at the gateway"
        );
    }
    assert!(nodes.b.proxy.journal().is_empty(), "node B is never asked");
    Ok(())
}

// conformance: CP-24
#[tokio::test]
async fn a_composition_committed_through_the_gateway_lands_byte_identical_at_one_node() -> TestResult
{
    if !containers::e2e_enabled() {
        return Ok(());
    }
    let nodes = containers::two_nodes().await?;
    let only_the_ehr = SeedPlan {
        ehrs: vec![EhrSeed {
            ehr_id: EHR_A,
            subject: Some(PATIENT),
        }],
        template: true,
        compositions: Vec::new(),
    };
    seed::seed(&nodes.a.api_root(), &only_the_ehr).await?;
    nodes.a.proxy.clear_journal();
    nodes.b.proxy.clear_journal();
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &nodes.a, &nodes.b)?;

    let sent = composition_carrying(PATIENT)?;
    let commit = routed_to_a(
        http::Method::POST,
        &format!("/v1/ehr/{EHR_A}/composition"),
        Body::from(sent.clone()),
    )?;
    let response = crate::support::send(app.clone(), commit).await?;
    let (status, headers) = (response.status(), response.headers().clone());
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024).await?;
    assert_eq!(
        StatusCode::CREATED,
        status,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        Some("node-a-pub"),
        field(&headers, "openEHR-federation-endpoint"),
        "N31"
    );
    assert_eq!(
        Some(containers::NODE_A_SYSTEM_ID),
        field(&headers, "openEHR-federation-system-id"),
        "§9.6"
    );
    let etag = field(&headers, "etag").ok_or("the node's ETag")?.to_owned();
    let version_uid = etag.trim_start_matches("W/").trim_matches('"');
    assert!(
        version_uid.contains(&format!("::{}::", containers::NODE_A_SYSTEM_ID)),
        "the ETag is node A's OBJECT_VERSION_ID, never rewritten (N22): {etag}"
    );
    let location = field(&headers, "location").ok_or("the node's Location")?;
    assert!(
        location.starts_with(API_PATH) && location.ends_with(version_uid),
        "the Location is the node's own, unmodified (N31): {location}"
    );

    assert_landed_as_sent(&nodes, &sent)?;

    let read = routed_to_a(
        http::Method::GET,
        &format!("/v1/ehr/{EHR_A}/composition/{version_uid}"),
        Body::empty(),
    )?;
    let response = crate::support::send(app, read).await?;
    assert_eq!(StatusCode::OK, response.status());
    assert_eq!(
        Some(etag.as_str()),
        field(response.headers(), "etag"),
        "the same version, read back"
    );
    assert_eq!(
        Some("node-a-pub"),
        field(response.headers(), "openEHR-federation-endpoint")
    );
    Ok(())
}

/// A request of `verb` to `uri` naming `endpoint` in the endpoint header and
/// `preceding` in `If-Match`, quoted as ITS-REST writes it.
fn versioned_at(
    verb: http::Method,
    uri: &str,
    endpoint: &str,
    preceding: &str,
    body: Body,
) -> Result<Request<Body>, http::Error> {
    Request::builder()
        .method(verb)
        .uri(uri)
        .header("openEHR-federation-endpoint", endpoint)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::IF_MATCH, format!("\"{preceding}\""))
        .header(header::AUTHORIZATION, format!("Bearer {}", *CLIENT_TOKEN))
        .body(body)
}

// conformance: CP-15 CP-24
#[tokio::test]
async fn a_versioned_write_reaches_its_controlling_node_and_never_another() -> TestResult {
    if !containers::e2e_enabled() {
        return Ok(());
    }
    let nodes = containers::two_nodes().await?;
    let only_the_ehr = SeedPlan {
        ehrs: vec![EhrSeed {
            ehr_id: EHR_A,
            subject: Some(PATIENT),
        }],
        template: true,
        compositions: Vec::new(),
    };
    seed::seed(&nodes.a.api_root(), &only_the_ehr).await?;
    let dir = tempfile::tempdir()?;
    let app = gateway(dir.path(), &nodes.a, &nodes.b)?;
    let sent = composition_carrying(PATIENT)?;
    let commit = routed_to_a(
        http::Method::POST,
        &format!("/v1/ehr/{EHR_A}/composition"),
        Body::from(sent.clone()),
    )?;
    let response = crate::support::send(app.clone(), commit).await?;
    assert_eq!(StatusCode::CREATED, response.status());
    let etag = field(response.headers(), "etag").ok_or("the node's ETag")?;
    let first = etag.trim_start_matches("W/").trim_matches('"').to_owned();
    let object = first.split("::").next().ok_or("an object id")?.to_owned();
    nodes.a.proxy.clear_journal();
    nodes.b.proxy.clear_journal();

    let resource = format!("/v1/ehr/{EHR_A}/composition/{object}");
    let elsewhere = versioned_at(
        http::Method::PUT,
        &resource,
        "node-b-pub",
        &first,
        Body::from(sent.clone()),
    )?;
    let response = crate::support::send(app.clone(), elsewhere).await?;
    assert_eq!(
        StatusCode::CONFLICT,
        response.status(),
        "node B does not control a version node A created (§10.3, §12a.1, N23)"
    );
    assert!(nodes.a.proxy.journal().is_empty(), "node A is not written");
    assert!(nodes.b.proxy.journal().is_empty(), "node B is not written");

    let update = versioned_at(
        http::Method::PUT,
        &resource,
        "node-a-pub",
        &first,
        Body::from(sent.clone()),
    )?;
    let response = crate::support::send(app, update).await?;
    let (status, headers) = (response.status(), response.headers().clone());
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024).await?;
    assert!(
        status.is_success(),
        "the node's own success, 200 or 204 by its Prefer default: {status} {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        Some("node-a-pub"),
        field(&headers, "openEHR-federation-endpoint"),
        "N31"
    );
    let second = field(&headers, "etag").ok_or("the new version's ETag")?;
    assert!(
        second.contains(&format!("{object}::{}::2", containers::NODE_A_SYSTEM_ID)),
        "node A committed the second version, its uid unmodified (N22): {second}"
    );
    let journal = nodes.a.proxy.journal();
    let updates: Vec<_> = journal
        .iter()
        .filter(|capture| capture.method == "PUT")
        .collect();
    assert_eq!(1, updates.len(), "the controlling node is written once");
    let landed = updates.first().ok_or("one update")?;
    assert_eq!(
        sent.as_bytes(),
        landed.body.as_slice(),
        "byte-identical (track 10)"
    );
    for capture in &journal {
        assert!(
            !outside_the_body(capture, PATIENT.value().as_bytes()),
            "no identifier outside the body (N33)"
        );
    }
    assert!(nodes.b.proxy.journal().is_empty(), "node B is never asked");
    Ok(())
}

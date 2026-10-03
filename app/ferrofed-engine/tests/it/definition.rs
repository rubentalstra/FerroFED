// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A stored-query definition at a mock node (§12.7, N44): the registry's copy
//! sent with the generated `PUT /definition/query/{name}/{version}`, the
//! node's copy read back with `GET` on the same path, and every answer
//! mapped to one §11.1 status; a `node-error` carries the node's HTTP status
//! and an excerpt of its message, and never a body that is no error (§9.5,
//! §12.6 item 2).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;
use std::time::{Duration, Instant};

use ferrofed_engine::dispatch::definition::{DefinitionAt, NodeCopy};
use ferrofed_engine::dispatch::{Contact, DispatchOptions, NodeClient};
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_testkit::mock::Server;
use ferrofed_testkit::unreachable;
use http::StatusCode;
use openehr_federation::outcome::{ErrorDetail, Outcome};
use openehr_federation::status::EndpointStatus;
use openehr_its::rest::client::ReqwestTransport;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

type TestResult = Result<(), Box<dyn Error>>;

/// The definition every case addresses.
const AT: DefinitionAt<'static> = DefinitionAt {
    name: "org.example::fanned",
    version: "1.0.0",
};

/// Its path at a node.
const NODE_PATH: &str = "/v1/definition/query/org.example::fanned/1.0.0";

/// A synthetic definition.
const AQL: &str = "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c";

/// A body a node answers with: the message of an error status, which a
/// `node-error` carries after the status, or a body that is no answer, which
/// no outcome copies.
const NODE_BODY: &str = "SYNTHETIC-NODE-BODY-91c0";

/// A client of a one-endpoint federation whose endpoint is at `url`.
fn client_at(url: &str) -> Result<NodeClient<ReqwestTransport>, Box<dyn Error>> {
    let document = format!(
        "[[organisation]]\nid = \"org-a\"\n\n[[node]]\nid = \"node-a\"\norganisation = \"org-a\"\n\
         system_id = \"cdr-a.example.org\"\n\n[[endpoint]]\nid = \"node-a-pub\"\nnode = \"node-a\"\n\
         url = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n"
    );
    let snapshot = RegistrySnapshot::from_toml_str(&document)?;
    let endpoint = snapshot.endpoints().next().ok_or("one endpoint")?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(10))?;
    Ok(NodeClient::new(endpoint, transport)?)
}

/// Options with a deadline two seconds from now.
fn options() -> Result<DispatchOptions, Box<dyn Error>> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .ok_or("the deadline is past the platform clock")?;
    Ok(DispatchOptions::new(
        deadline,
        crate::conveyed::conveyance(),
    ))
}

/// A node answering `verb` at the definition's path with `status` and
/// `body`.
async fn node(verb: &str, status: u16, body: &str) -> Server {
    let server = Server::start().await;
    Mock::given(method(verb))
        .and(path(NODE_PATH))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_raw(body.as_bytes().to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    server
}

/// The error text of `outcome`, which must be a text error.
fn error_text(outcome: &Outcome) -> Result<&str, Box<dyn Error>> {
    match outcome.error() {
        Some(ErrorDetail::Text(text)) => Ok(text),
        other => Err(format!("a text error, not {other:?}").into()),
    }
}

#[tokio::test]
async fn a_stored_definition_is_active_and_the_node_receives_the_aql_as_text() -> TestResult {
    let server = node("PUT", 200, "").await;
    let stored = client_at(&server.uri())?
        .store_definition(AT, AQL, &options()?)
        .await?;
    assert_eq!(EndpointStatus::Active, stored.outcome.status());
    assert_eq!(Contact::Answered(StatusCode::OK), stored.contact);
    let requests = server.received_requests().await.ok_or("recording is on")?;
    let [only] = requests.as_slice() else {
        return Err("one request".into());
    };
    assert_eq!(AQL.as_bytes(), only.body.as_slice(), "the AQL as sent");
    assert_eq!(Some("query_type=AQL"), only.url.query());
    let media = only
        .headers
        .get("content-type")
        .ok_or("a Content-Type")?
        .to_str()?;
    assert!(media.starts_with("text/plain"), "{media}");
    Ok(())
}

#[tokio::test]
async fn a_refused_store_is_a_node_error_with_the_status_and_the_nodes_message() -> TestResult {
    for (status, named) in [
        (409, "409 Conflict"),
        (400, "400 Bad Request"),
        (500, "500 Internal Server Error"),
    ] {
        let body = format!(r#"{{"message":"{NODE_BODY}"}}"#);
        let server = node("PUT", status, &body).await;
        let stored = client_at(&server.uri())?
            .store_definition(AT, AQL, &options()?)
            .await?;
        assert_eq!(
            EndpointStatus::NodeError,
            stored.outcome.status(),
            "{status}"
        );
        assert_eq!(
            Contact::Answered(StatusCode::from_u16(status)?),
            stored.contact,
            "the node's own status beside the record"
        );
        assert_eq!(
            format!("the node answered {named}: {NODE_BODY}"),
            error_text(&stored.outcome)?,
            "§9.5, §11.1: the node's status, then its own message"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_node_copy_is_read_back_or_reported_missing() -> TestResult {
    let copy = format!(
        r#"{{"name":"{}","type":"AQL","version":"{}","saved":"2026-10-03T00:00:00Z","q":"{AQL}"}}"#,
        AT.name, AT.version
    );
    let server = node("GET", 200, &copy).await;
    let read = client_at(&server.uri())?
        .read_definition(AT, &options()?)
        .await?;
    let NodeCopy::Held { aql, .. } = read else {
        return Err(format!("a held copy, not {read:?}").into());
    };
    assert_eq!(AQL, aql);

    let server = node("GET", 404, &format!(r#"{{"message":"{NODE_BODY}"}}"#)).await;
    let read = client_at(&server.uri())?
        .read_definition(AT, &options()?)
        .await?;
    assert!(matches!(read, NodeCopy::Missing { .. }), "{read:?}");
    Ok(())
}

#[tokio::test]
async fn a_copy_that_is_no_stored_query_or_a_node_out_of_reach_is_failed() -> TestResult {
    let server = node("GET", 200, NODE_BODY).await;
    let read = client_at(&server.uri())?
        .read_definition(AT, &options()?)
        .await?;
    let NodeCopy::Failed { outcome, contact } = read else {
        return Err(format!("a failed read, not {read:?}").into());
    };
    assert_eq!(EndpointStatus::NodeError, outcome.status());
    assert_eq!(
        Contact::Answered(StatusCode::OK),
        contact,
        "the node answered"
    );
    assert!(!error_text(&outcome)?.contains(NODE_BODY), "no node body");

    let read = client_at(&format!("{}/openehr", unreachable::BASE))?
        .read_definition(AT, &options()?)
        .await?;
    let NodeCopy::Failed { outcome, contact } = read else {
        return Err(format!("a failed read, not {read:?}").into());
    };
    assert_eq!(EndpointStatus::Offline, outcome.status(), "§11.1");
    assert_eq!(Contact::Silent, contact, "no answer");
    Ok(())
}

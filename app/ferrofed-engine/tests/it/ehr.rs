// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The EHR create of the admission check against a mock node (§12b.1): the
//! gateway asks for `Prefer: return=minimal`, and a node that answers with
//! the full representation anyway is read through the typed `201_EHR` body of
//! `openehr-its` (ITS-REST 1.1.0 EHR API, `ehr_create`), the `ehr_id` still
//! taken from `ETag`. A call the deadline overtook before it left is told
//! from one the node left unanswered (§11.5).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;
use std::time::{Duration, Instant};

use ferrofed_engine::dispatch::{Contact, DispatchOptions, NodeClient};
use ferrofed_engine::ehr::EhrCallError;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_testkit::mock::Server;
use openehr_its::json::from_canonical_json;
use openehr_its::rest::client::ReqwestTransport;
use openehr_rm::v1_2::ehr::ehr_status::EhrStatus;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, ResponseTemplate};

type TestResult = Result<(), Box<dyn Error>>;

/// The `ehr_id` the mock node assigns, a synthetic version-4 UUID.
const EHR_ID: &str = "3e9f1c2a-5b4d-4c6e-8f7a-9b0c1d2e3f40";

/// The `EHR_STATUS` the create carries, with no subject.
const EHR_STATUS: &str = r#"{"_type":"EHR_STATUS","name":{"_type":"DV_TEXT","value":"EHR Status"},"archetype_node_id":"openEHR-EHR-EHR_STATUS.generic.v1","subject":{"_type":"PARTY_SELF"},"is_queryable":true,"is_modifiable":true}"#;

/// The full representation of the created `EHR`, as `Prefer:
/// return=representation` asks for it.
fn representation() -> String {
    format!(
        r#"{{"_type":"EHR","system_id":{{"_type":"HIER_OBJECT_ID","value":"cdr-a.example.org"}},"ehr_id":{{"_type":"HIER_OBJECT_ID","value":"{EHR_ID}"}},"ehr_status":{{"_type":"OBJECT_REF","namespace":"local","type":"EHR_STATUS","id":{{"_type":"HIER_OBJECT_ID","value":"7f1e2d3c-4b5a-4968-8776-655443322110"}}}},"ehr_access":{{"_type":"OBJECT_REF","namespace":"local","type":"EHR_ACCESS","id":{{"_type":"HIER_OBJECT_ID","value":"8a2f3e4d-5c6b-4a79-9887-766554433221"}}}},"time_created":{{"_type":"DV_DATE_TIME","value":"2026-10-03T09:00:00Z"}}}}"#
    )
}

/// A client for one endpoint of node A at `url`.
fn client_at(url: &str) -> Result<NodeClient<ReqwestTransport>, Box<dyn Error>> {
    let document = format!(
        "[[organisation]]\nid = \"org-a\"\n\n[[node]]\nid = \"node-a\"\norganisation = \"org-a\"\nsystem_id = \"cdr-a.example.org\"\n\n[[endpoint]]\nid = \"node-a-pub\"\nnode = \"node-a\"\nurl = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n"
    );
    let snapshot = RegistrySnapshot::from_toml_str(&document)?;
    let endpoint = snapshot
        .endpoints()
        .next()
        .ok_or("the snapshot holds no endpoint")?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(10))?;
    Ok(NodeClient::new(endpoint, transport)?)
}

/// Options with a deadline ten seconds from now.
fn options() -> Result<DispatchOptions, Box<dyn Error>> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(10))
        .ok_or("the deadline is past the platform clock")?;
    Ok(DispatchOptions::new(
        deadline,
        crate::conveyed::conveyance(),
    ))
}

/// A node that answers a `return=minimal` create with `201`, the `ehr_id` in
/// `ETag`, and `body` as `application/json`.
async fn node_answering(body: String) -> Server {
    let server = Server::start().await;
    Mock::given(method("POST"))
        .and(path("/openehr/v1/ehr"))
        .and(header("prefer", "return=minimal"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("ETag", format!("\"{EHR_ID}\"").as_str())
                .set_body_raw(body.into_bytes(), "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_return_representation_answer_is_read_through_the_typed_201_ehr_body() -> TestResult {
    let server = node_answering(representation()).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let status: EhrStatus = from_canonical_json(EHR_STATUS)?;
    let ehr_id = client.create_ehr(&status, &options()?).await?;
    assert_eq!(EHR_ID, ehr_id);
    Ok(())
}

#[tokio::test]
async fn a_201_body_that_is_no_ehr_fails_the_create() -> TestResult {
    let server = node_answering(r#"{"_type":"COMPOSITION"}"#.to_owned()).await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let status: EhrStatus = from_canonical_json(EHR_STATUS)?;
    let created = client.create_ehr(&status, &options()?).await;
    assert!(
        matches!(created, Err(EhrCallError::Failed { .. })),
        "{created:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_create_whose_deadline_passed_before_it_left_is_expired_and_never_sent() -> TestResult {
    let server = Server::start().await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let status: EhrStatus = from_canonical_json(EHR_STATUS)?;
    let created = client
        .create_ehr(
            &status,
            &DispatchOptions::new(Instant::now(), crate::conveyed::conveyance()),
        )
        .await;
    let Err(error) = created else {
        return Err(format!("an expired create succeeded: {created:?}").into());
    };
    assert!(
        matches!(error, EhrCallError::Expired { .. }),
        "§11.5: the node was never asked: {error:?}"
    );
    assert_eq!(Contact::Unsent, Contact::of_ehr_call_error(&error));
    let received = server.received_requests().await.ok_or("recording is on")?;
    assert!(received.is_empty(), "nothing is sent");
    Ok(())
}

#[tokio::test]
async fn a_read_the_node_leaves_unanswered_is_a_time_out_of_a_silent_node() -> TestResult {
    let server = Server::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/openehr/v1/ehr/{EHR_ID}")))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .mount(&server)
        .await;
    let client = client_at(&format!("{}/openehr", server.uri()))?;
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(200))
        .ok_or("the deadline is past the platform clock")?;
    let read = client
        .read_ehr(
            EHR_ID,
            &DispatchOptions::new(deadline, crate::conveyed::conveyance()),
        )
        .await;
    let Err(error) = read else {
        return Err("a silent node answered".into());
    };
    assert!(
        matches!(error, EhrCallError::TimeOut { .. }),
        "§11.1: sent, and no answer in time: {error:?}"
    );
    assert_eq!(Contact::Silent, Contact::of_ehr_call_error(&error));
    Ok(())
}

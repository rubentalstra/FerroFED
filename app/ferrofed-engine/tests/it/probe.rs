// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The ask-all probe against mock members (§12.5.1 step 4, §11.5): what each
//! member's answer shows of it, a probe the overall budget overtook before it
//! left included. Asserted on what the mock members received.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use ferrofed_engine::dispatch::{Contact, NodeClients};
use ferrofed_engine::forward::ForwardError;
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_engine::probe::{self, Answer, Probe, ProbedEhrId};
use ferrofed_registry::id::{EhrId, EndpointId};
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_testkit::mock::Server;
use http::HeaderMap;
use openehr_its::rest::client::ReqwestTransport;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

type TestResult = Result<(), Box<dyn Error>>;

/// The `ehr_id` every member is asked about.
const EHR: &str = "7d44b88c-4199-4bad-97dc-d78268e01398";

/// The registry of node A at `a` and node B at `b`.
fn registry(a: &str, b: &str) -> Result<RegistrySnapshot, Box<dyn Error>> {
    let mut document = String::from("[[organisation]]\nid = \"org-a\"\n");
    for (name, url) in [("a", a), ("b", b)] {
        write!(
            document,
            "\n[[node]]\nid = \"node-{name}\"\norganisation = \"org-a\"\nsystem_id = \"cdr-{name}.example.org\"\n\n[[endpoint]]\nid = \"node-{name}-pub\"\nnode = \"node-{name}\"\nurl = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n"
        )?;
    }
    Ok(RegistrySnapshot::from_toml_str(&document)?)
}

/// A member answering the probe `200` after `delay`.
async fn member(delay: Duration) -> Server {
    let server = Server::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/ehr/{EHR}")))
        .respond_with(ResponseTemplate::new(200).set_delay(delay))
        .mount(&server)
        .await;
    server
}

/// Probes node A at `a` and node B at `b` under one deadline, `until`.
async fn probed(
    a: &Server,
    b: &Server,
    until: Instant,
) -> Result<Vec<(EndpointId, probe::Probed)>, Box<dyn Error>> {
    let snapshot = registry(&a.uri(), &b.uri())?;
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(10))?;
    let clients = NodeClients::from_snapshot(&snapshot, &transport, &BTreeMap::new())?;
    let endpoints = [
        EndpointId::new("node-a-pub")?,
        EndpointId::new("node-b-pub")?,
    ];
    let probe = Probe {
        ehr_id: ProbedEhrId::try_from(&EhrId::new(EHR)?)?,
        headers: HeaderMap::new(),
        per_node: until,
        overall: until,
        request_id: OutboundId::mint(),
        conveyance: crate::conveyed::conveyance(),
    };
    Ok(probe::ask_all(&clients, &endpoints, &probe).await?)
}

async fn received(server: &Server) -> Result<usize, Box<dyn Error>> {
    Ok(server
        .received_requests()
        .await
        .ok_or("recording is on")?
        .len())
}

#[tokio::test]
async fn a_probe_the_budget_overtook_is_unsent_at_every_member() -> TestResult {
    let a = member(Duration::ZERO).await;
    let b = member(Duration::ZERO).await;
    let answers = probed(&a, &b, Instant::now()).await?;
    assert_eq!(2, answers.len());
    for (endpoint, probed) in &answers {
        assert!(
            matches!(probed.answer, Answer::Failed(ForwardError::Expired { .. })),
            "§11.5: {endpoint} was never asked: {:?}",
            probed.answer
        );
        assert_eq!(Contact::Unsent, probed.contact(), "{endpoint}");
    }
    assert_eq!(0, received(&a).await?, "node A received nothing");
    assert_eq!(0, received(&b).await?, "node B received nothing");
    Ok(())
}

#[tokio::test]
async fn a_probe_a_member_leaves_unanswered_past_the_budget_shows_a_silent_member() -> TestResult {
    let a = member(Duration::ZERO).await;
    let b = member(Duration::from_secs(3)).await;
    let until = Instant::now()
        .checked_add(Duration::from_millis(300))
        .ok_or("the deadline is past the platform clock")?;
    let answers = probed(&a, &b, until).await?;
    let contacts: Vec<(&str, Contact)> = answers
        .iter()
        .map(|(endpoint, probed)| (endpoint.as_str(), probed.contact()))
        .collect();
    assert_eq!(
        vec![
            ("node-a-pub", Contact::Answered(http::StatusCode::OK)),
            ("node-b-pub", Contact::Silent),
        ],
        contacts,
        "§11.1: node B was sent the probe and gave no answer in time"
    );
    assert_eq!(1, received(&b).await?, "the probe left for node B");
    Ok(())
}

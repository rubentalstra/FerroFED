// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The harness care services directory held to the mCSD client of `ihe-iti`:
//! what a test publishes reads back through ITI-90, a change reads back
//! through ITI-91 alone, and an outage is an error.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;
use std::time::Duration;

use ferrofed_testkit::mcsd::{ENDPOINT_ID, HarnessDirectory, Member, ORGANISATION_ID, Outage};
use ihe_iti::mcsd::client::McsdClient;
use ihe_iti::mcsd::replica::{Refresh, Replica, Scope};
use url::Url;

type TestResult = Result<(), Box<dyn Error>>;

const PROMPT: Duration = Duration::from_secs(5);

fn member(name: &str) -> Member {
    Member {
        organisation: format!("org-{name}"),
        endpoint: format!("node-{name}-pub"),
        node: format!("node-{name}"),
        system_id: format!("cdr-{name}.example.org"),
        address: format!("https://cdr-{name}.example.org/openehr"),
    }
}

fn client(directory: &HarnessDirectory) -> Result<McsdClient, Box<dyn Error>> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    Ok(McsdClient::new(Url::parse(&directory.base())?, http)?)
}

fn scope() -> Scope {
    Scope::identified(ORGANISATION_ID, ENDPOINT_ID)
}

#[tokio::test]
async fn what_a_test_publishes_reads_back_through_iti_90() -> TestResult {
    let directory = HarnessDirectory::start().await;
    directory.publish(&[member("a"), member("b")])?;
    let replica = Replica::read(&client(&directory)?, scope(), PROMPT).await?;
    let content = replica.directory()?;
    let addresses: Vec<Option<&str>> = content
        .endpoints()
        .iter()
        .map(|endpoint| endpoint.address())
        .collect();
    assert_eq!(
        vec![
            Some("https://cdr-a.example.org/openehr"),
            Some("https://cdr-b.example.org/openehr"),
        ],
        addresses
    );
    assert_eq!(2, content.organizations().len());
    assert!(replica.since().is_some(), "the answers carry the clock");
    Ok(())
}

#[tokio::test]
async fn a_change_reads_back_through_iti_91_alone() -> TestResult {
    let directory = HarnessDirectory::start().await;
    directory.publish(&[member("a"), member("b")])?;
    let client = client(&directory)?;
    let replica = Replica::read(&client, scope(), PROMPT).await?;
    let mut moved = member("b");
    moved.address = "https://cdr-b2.example.org/openehr".to_owned();
    directory.put_endpoint(moved.endpoint()?);

    let Refresh::Changed(next) = replica.refreshed(&client, PROMPT).await? else {
        return Err("the endpoint moved".into());
    };
    let content = next.directory()?;
    assert_eq!(
        Some("https://cdr-b2.example.org/openehr"),
        content
            .endpoints()
            .iter()
            .find(|endpoint| endpoint.logical_id() == Some("node-b-pub"))
            .and_then(|endpoint| endpoint.address())
    );
    let requests = directory.requests().await;
    let searches = requests
        .iter()
        .filter(|request| !request.contains("_history"))
        .count();
    assert_eq!(
        2, searches,
        "one ITI-90 search per type, at the read: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("/fhir/Endpoint/_history?_since=")),
        "{requests:?}"
    );
    Ok(())
}

#[tokio::test]
async fn an_outage_is_an_error_and_the_end_of_it_answers_again() -> TestResult {
    let directory = HarnessDirectory::start().await;
    directory.publish(&[member("a")])?;
    let client = client(&directory)?;
    directory.outage(Some(Outage::Refusing));
    assert!(Replica::read(&client, scope(), PROMPT).await.is_err());
    directory.outage(Some(Outage::Silent));
    let silent = Replica::read(&client, scope(), Duration::from_millis(200)).await;
    assert!(
        silent.as_ref().is_err_and(|error| !error.answered()),
        "{silent:?}"
    );
    directory.outage(None);
    assert_eq!((1, 1), Replica::read(&client, scope(), PROMPT).await?.len());
    Ok(())
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The registry read from a care services directory (§15.1, N21): the
//! federation's identifier systems select its members from a shared
//! directory, the testkit's harness directory standing in for one.

use std::error::Error;
use std::time::Duration;

use ferrofed_identity::directory::mcsd::{
    DirectoryConfig, DirectoryReadError, DirectorySource, Refreshed,
};
use ferrofed_identity::directory::{ENDPOINT_ID_SYSTEM, ORGANISATION_ID_SYSTEM};
use ferrofed_registry::id::NodeId;
use ferrofed_registry::secret::SecretUrl;
use ferrofed_testkit::mcsd::{self, HarnessDirectory, Member};

type TestResult = Result<(), Box<dyn Error>>;

fn member(name: &str) -> Member {
    Member {
        organisation: format!("org-{name}"),
        endpoint: format!("node-{name}-pub"),
        node: format!("node-{name}"),
        system_id: format!("cdr-{name}.example.org"),
        address: format!("https://cdr-{name}.example.org/openehr"),
    }
}

fn source(harness: &HarnessDirectory) -> Result<DirectorySource, Box<dyn Error>> {
    Ok(DirectorySource::new(DirectoryConfig {
        base: SecretUrl::new(harness.base()),
        credentials: None,
        deadline: Duration::from_secs(5),
        pages: 200,
        bytes: 64 << 20,
        entries: 50_000,
    })?)
}

#[test]
fn the_harness_spells_the_identifier_systems_of_the_fhir_form() {
    assert_eq!(ORGANISATION_ID_SYSTEM, mcsd::ORGANISATION_ID);
    assert_eq!(ENDPOINT_ID_SYSTEM, mcsd::ENDPOINT_ID);
}

/// A directory shared with other services holds the IG's own examples, XCA
/// and DICOM endpoints among them; only the federation's resources become
/// members, and the rest never reaches the connection-type rule.
#[tokio::test]
async fn only_the_federations_resources_of_a_shared_directory_are_members() -> TestResult {
    let harness = HarnessDirectory::start().await;
    harness.publish_examples()?;
    harness.publish(&[member("a"), member("b")])?;
    let read = source(&harness)?.read().await?;
    assert_eq!((2, 2), read.content().len());
    let nodes: Vec<&NodeId> = read
        .snapshot()
        .nodes()
        .map(ferrofed_registry::snapshot::Node::id)
        .collect();
    assert_eq!(
        vec![&"node-a".parse::<NodeId>()?, &"node-b".parse::<NodeId>()?],
        nodes
    );
    Ok(())
}

#[tokio::test]
async fn a_member_relying_on_hl7_fhir_rest_is_no_registry() -> TestResult {
    let harness = HarnessDirectory::start().await;
    let a = member("a");
    harness.put_organization(a.organisation()?);
    harness.put_endpoint(a.endpoint_with_connection_type(
        "http://terminology.hl7.org/CodeSystem/endpoint-connection-type",
        "hl7-fhir-rest",
    )?);
    let refused = source(&harness)?.read().await;
    assert!(
        matches!(refused, Err(DirectoryReadError::Registry(_))),
        "{refused:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_refresh_into_a_broken_registry_is_refused_and_the_content_kept() -> TestResult {
    let harness = HarnessDirectory::start().await;
    harness.publish(&[member("a")])?;
    let source = source(&harness)?;
    let read = source.read().await?;
    harness.delete_endpoint("node-a-pub");
    let refreshed = source.refresh(read.content()).await?;
    assert!(matches!(refreshed, Refreshed::Refused(_)), "{refreshed:?}");
    assert_eq!(
        (1, 1),
        read.content().len(),
        "the content read is unchanged"
    );
    Ok(())
}

#[tokio::test]
async fn a_directory_that_does_not_answer_is_an_exchange_error() -> TestResult {
    let source = DirectorySource::new(DirectoryConfig {
        base: SecretUrl::new(format!("{}/fhir", ferrofed_testkit::unreachable::BASE)),
        credentials: None,
        deadline: Duration::from_secs(2),
        pages: 200,
        bytes: 64 << 20,
        entries: 50_000,
    })?;
    let refused = source.read().await;
    let Err(DirectoryReadError::Exchange(error)) = &refused else {
        return Err(format!("an exchange error: {refused:?}").into());
    };
    assert!(!error.answered());
    Ok(())
}

/// A refresh asks ITI-91 from one minute before the `Date` the directory
/// stamped its read with, on the directory's own clock.
#[tokio::test]
async fn a_refresh_asks_from_a_minute_before_the_directorys_clock() -> TestResult {
    let harness = HarnessDirectory::start().await;
    harness.publish(&[member("a")])?;
    let source = source(&harness)?;
    let read = source.read().await?;
    let refreshed = source.refresh(read.content()).await?;
    assert!(
        matches!(refreshed, Refreshed::Unchanged(_)),
        "{refreshed:?}"
    );
    let requests = harness.requests().await;
    assert!(
        requests
            .iter()
            .any(|request| request.ends_with("_history?_since=2026-01-01T01%3A59%3A00Z")),
        "two changes move the harness clock to 02:00: {requests:?}"
    );
    Ok(())
}

/// A directory whose answer runs past a cap gives no registry: the read is an
/// exchange error that says it ran out of its budget.
#[tokio::test]
async fn a_directory_past_a_cap_gives_no_registry() -> TestResult {
    let harness = HarnessDirectory::start().await;
    harness.publish(&[member("a"), member("b")])?;
    let source = DirectorySource::new(DirectoryConfig {
        base: SecretUrl::new(harness.base()),
        credentials: None,
        deadline: Duration::from_secs(5),
        pages: 200,
        bytes: 64 << 20,
        entries: 3,
    })?;
    let refused = source.read().await;
    let Err(DirectoryReadError::Exchange(error)) = &refused else {
        return Err(format!("an exchange error: {refused:?}").into());
    };
    assert!(error.exceeded(), "{error:?}");
    Ok(())
}

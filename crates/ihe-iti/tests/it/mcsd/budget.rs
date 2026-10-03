// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The budget every walk draws on: a faulty or hostile directory that links
//! pages in a cycle, answers with more bytes or entries than the caps allow,
//! or answers slowly page after page ends in a typed error, never in a
//! shorter answer, and never later than the deadline (no specification
//! governs the limits: our own design).

use std::time::{Duration, Instant};

use ihe_iti::mcsd::budget::{Budget, Limits};
use ihe_iti::mcsd::client::CareService;
use ihe_iti::mcsd::error::McsdError;
use ihe_iti::mcsd::replica::{Replica, Scope};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{BASE, FHIR_JSON, bundle, client, endpoint, full_url, matched, organization};

/// A budget of five seconds with `limits`.
fn within(limits: Limits) -> Budget {
    Budget::new(Instant::now() + Duration::from_secs(5), limits)
}

/// A stub answering every `GET` of `kind` with a searchset of `entries`
/// whose `next` link points back at itself, after `delay`.
async fn cycling(kind: &str, entries: usize, delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    let at = format!("{}{BASE}{kind}", server.uri());
    let matches: Vec<String> = (0..entries)
        .map(|index| {
            let id = format!("org-{index}");
            matched(
                &full_url(&server, kind, &id),
                &organization(&id, "urn:oid:2.999.10", &[]),
            )
        })
        .collect();
    let body = bundle(
        "searchset",
        &matches,
        &[format!(r#"{{"relation":"next","url":"{at}"}}"#)],
    );
    Mock::given(method("GET"))
        .and(path(format!("{BASE}{kind}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(body.into_bytes(), FHIR_JSON)
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

#[test]
fn the_default_caps_are_two_hundred_pages_sixty_four_mib_and_fifty_thousand_entries() {
    assert_eq!(
        Limits {
            pages: 200,
            bytes: 64 << 20,
            entries: 50_000,
        },
        Limits::default()
    );
}

#[tokio::test]
async fn a_link_cycle_ends_at_the_page_cap() {
    let server = cycling("Organization", 0, Duration::ZERO).await;
    let error = client(&server)
        .find(
            CareService::Organization,
            &[],
            &mut within(Limits {
                pages: 3,
                ..Limits::default()
            }),
        )
        .await
        .expect_err("the cycle never ends by itself");
    assert!(
        matches!(error, McsdError::TooManyPages { limit: 3 }),
        "{error:?}"
    );
    assert!(error.exceeded());
    let asked = server.received_requests().await.expect("recording is on");
    assert_eq!(3, asked.len(), "no page is asked past the cap");
}

#[tokio::test]
async fn a_link_cycle_ends_at_the_default_page_cap() {
    let server = cycling("Organization", 0, Duration::ZERO).await;
    let error = client(&server)
        .find(
            CareService::Organization,
            &[],
            &mut within(Limits::default()),
        )
        .await
        .expect_err("the cycle never ends by itself");
    assert!(
        matches!(error, McsdError::TooManyPages { limit: 200 }),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_bytes_of_every_page_count_against_one_cap() {
    let server = cycling("Organization", 2, Duration::ZERO).await;
    let error = client(&server)
        .find(
            CareService::Organization,
            &[],
            &mut within(Limits {
                bytes: 4096,
                ..Limits::default()
            }),
        )
        .await
        .expect_err("the pages together pass the byte cap");
    assert!(
        matches!(error, McsdError::TooLarge { limit: 4096 }),
        "{error:?}"
    );
    let asked = server.received_requests().await.expect("recording is on");
    assert!(
        asked.len() > 1,
        "each page fits, so only their sum passes the cap: {} pages",
        asked.len()
    );
}

#[tokio::test]
async fn the_entries_of_every_page_count_against_one_cap() {
    let server = cycling("Organization", 2, Duration::ZERO).await;
    let error = client(&server)
        .find(
            CareService::Organization,
            &[],
            &mut within(Limits {
                entries: 5,
                ..Limits::default()
            }),
        )
        .await
        .expect_err("the pages together pass the entry cap");
    assert!(
        matches!(error, McsdError::TooManyEntries { limit: 5 }),
        "{error:?}"
    );
}

/// Each page answers well within a per-page timeout, and the walk still ends
/// at the one deadline that bounds it whole.
#[tokio::test]
async fn one_deadline_bounds_the_whole_walk() {
    let server = cycling("Organization", 0, Duration::from_millis(150)).await;
    let started = Instant::now();
    let error = client(&server)
        .find(
            CareService::Organization,
            &[],
            &mut Budget::new(
                Instant::now() + Duration::from_millis(700),
                Limits::default(),
            ),
        )
        .await
        .expect_err("the walk outlasts its deadline");
    assert!(matches!(error, McsdError::Timeout), "{error:?}");
    assert!(error.exceeded() && !error.answered());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the walk ended at its deadline, after {:?}",
        started.elapsed()
    );
}

/// The two searches of a replica's read draw on one budget, so the caps
/// bound the whole read.
#[tokio::test]
async fn a_replica_read_spends_one_budget_on_both_searches() {
    let server = MockServer::start().await;
    for (kind, entries) in [
        (
            "Organization",
            vec![matched(
                &full_url(&server, "Organization", "org-a"),
                &organization("org-a", "urn:oid:2.999.10", &["ep-a"]),
            )],
        ),
        (
            "Endpoint",
            vec![matched(
                &full_url(&server, "Endpoint", "ep-a"),
                &endpoint("ep-a", "urn:oid:2.999.11", "org-a", "https://a.example.org"),
            )],
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("{BASE}{kind}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(bundle("searchset", &entries, &[]).into_bytes(), FHIR_JSON),
            )
            .mount(&server)
            .await;
    }
    let error = Replica::read(
        &client(&server),
        Scope::everything(),
        &mut within(Limits {
            entries: 1,
            ..Limits::default()
        }),
    )
    .await
    .expect_err("one entry per search, two in all");
    assert!(
        matches!(error, McsdError::TooManyEntries { limit: 1 }),
        "{error:?}"
    );
}

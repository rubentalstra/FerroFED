// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The replica: read in scope with ITI-90, kept in step with ITI-91 since an
//! instant the caller chooses, the newest version winning, read again whole
//! when no instant is given, and every read held to its budget.

use ihe_iti::mcsd::directory::Directory;
use ihe_iti::mcsd::replica::{Refresh, Replica, Scope};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    BASE, ENDPOINT_SYSTEM, FHIR_JSON, ORG_SYSTEM, budget, bundle, client, deletion, endpoint,
    matched, organization, version,
};

/// The `Date` of the stub's ITI-90 answers.
const READ_AT: &str = "Thu, 01 Oct 2026 12:00:00 GMT";

/// The `Date` of the stub's ITI-91 answers.
const REFRESHED_AT: &str = "Thu, 01 Oct 2026 13:00:00 GMT";

fn scope() -> Scope {
    Scope::identified(ORG_SYSTEM, ENDPOINT_SYSTEM)
}

/// A stub directory whose ITI-90 searches answer `organizations` and
/// `endpoints`, each with the `Date` `date` when one is given, and expect to
/// be asked `reads` times.
async fn directory(
    organizations: &[String],
    endpoints: &[String],
    date: Option<&str>,
    reads: u64,
) -> MockServer {
    let server = MockServer::start().await;
    for (kind, system, entries) in [
        ("Organization", ORG_SYSTEM, organizations),
        ("Endpoint", ENDPOINT_SYSTEM, endpoints),
    ] {
        let mut answer = ResponseTemplate::new(200)
            .set_body_raw(bundle("searchset", entries, &[]).into_bytes(), FHIR_JSON);
        if let Some(date) = date {
            answer = answer.insert_header("Date", date);
        }
        Mock::given(method("GET"))
            .and(path(format!("{BASE}{kind}")))
            .and(query_param("identifier", format!("{system}|")))
            .respond_with(answer)
            .expect(reads)
            .mount(&server)
            .await;
    }
    server
}

/// Answers the ITI-91 history of `kind` since `since` with `entries`.
async fn history(server: &MockServer, kind: &str, since: &str, entries: &[String]) {
    Mock::given(method("GET"))
        .and(path(format!("{BASE}{kind}/_history")))
        .and(query_param("_since", since))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(bundle("history", entries, &[]).into_bytes(), FHIR_JSON)
                .insert_header("Date", REFRESHED_AT),
        )
        .expect(1)
        .mount(server)
        .await;
}

/// The `fullUrl` of the resource `kind/id` in the directory the entries name.
fn url(kind: &str, id: &str) -> String {
    format!("https://directory.example.org/fhir/{kind}/{id}")
}

fn organization_entry(id: &str, endpoints: &[&str]) -> String {
    matched(
        &url("Organization", id),
        &organization(id, ORG_SYSTEM, endpoints),
    )
}

fn endpoint_entry(id: &str, address: &str) -> String {
    matched(
        &url("Endpoint", id),
        &endpoint(id, ENDPOINT_SYSTEM, "org-a", address),
    )
}

/// The `address` of each endpoint of `directory`, by logical id.
fn addresses(directory: &Directory) -> Vec<(Option<&str>, Option<&str>)> {
    directory
        .endpoints()
        .iter()
        .map(|endpoint| (endpoint.logical_id(), endpoint.address()))
        .collect()
}

/// A stub directory holding org-a with endpoints ep-a and ep-b, and one
/// out-of-scope Endpoint the scoped search would not have matched.
async fn seeded(reads: u64) -> MockServer {
    let organizations = vec![organization_entry("org-a", &["ep-a", "ep-b"])];
    let endpoints = vec![
        endpoint_entry("ep-a", "https://cdr-a.example.org/openehr"),
        endpoint_entry("ep-b", "https://cdr-b.example.org/openehr"),
    ];
    directory(&organizations, &endpoints, Some(READ_AT), reads).await
}

#[tokio::test]
async fn a_read_holds_the_resources_in_scope_and_keeps_the_directorys_clock_reading() {
    let organizations = vec![organization_entry("org-a", &["ep-a"])];
    let endpoints = vec![
        endpoint_entry("ep-a", "https://cdr-a.example.org/openehr"),
        matched(
            &url("Endpoint", "ep-x"),
            &endpoint("ep-x", "urn:oid:2.999.99", "org-a", "https://x.example.org"),
        ),
    ];
    let server = directory(&organizations, &endpoints, Some(READ_AT), 1).await;
    let replica = Replica::read(&client(&server), scope(), &mut budget())
        .await
        .expect("a replica");
    assert_eq!(
        (1, 1),
        replica.len(),
        "the Endpoint out of scope is left out"
    );
    assert_eq!(Some(READ_AT), replica.answered_at());
}

#[tokio::test]
async fn a_refresh_applies_only_the_changes_since_the_last_read() {
    let server = seeded(1).await;
    let client = client(&server);
    let replica = Replica::read(&client, scope(), &mut budget())
        .await
        .expect("a replica");
    let since = "2026-10-01T11:59:00Z";
    history(&server, "Organization", since, &[]).await;
    history(
        &server,
        "Endpoint",
        since,
        &[
            version(
                &url("Endpoint", "ep-a"),
                "PUT",
                "Endpoint/ep-a",
                &endpoint(
                    "ep-a",
                    ENDPOINT_SYSTEM,
                    "org-a",
                    "https://cdr-a2.example.org/openehr",
                ),
            ),
            version(
                &url("Endpoint", "ep-a"),
                "PUT",
                "Endpoint/ep-a",
                &endpoint(
                    "ep-a",
                    ENDPOINT_SYSTEM,
                    "org-a",
                    "https://cdr-a-older.example.org/openehr",
                ),
            ),
        ],
    )
    .await;

    let Refresh::Changed(next) = replica
        .refreshed(&client, Some("2026-10-01T11:59:00Z"), &mut budget())
        .await
        .expect("a refresh")
    else {
        panic!("the history changed an endpoint");
    };
    let directory = next.directory().expect("directory content");
    assert_eq!(
        vec![
            (Some("ep-a"), Some("https://cdr-a2.example.org/openehr")),
            (Some("ep-b"), Some("https://cdr-b.example.org/openehr")),
        ],
        addresses(&directory),
        "the newest version of ep-a wins and ep-b, which did not change, stays"
    );
    assert_eq!(Some(REFRESHED_AT), next.answered_at());
    let held = replica.directory().expect("directory content");
    assert_eq!(
        Some(Some("https://cdr-a.example.org/openehr")),
        addresses(&held).first().map(|(_, address)| *address),
        "the replica refreshed from is unchanged"
    );
}

#[tokio::test]
async fn a_deletion_and_a_version_leaving_the_scope_remove_their_resources() {
    let server = seeded(1).await;
    let client = client(&server);
    let replica = Replica::read(&client, scope(), &mut budget())
        .await
        .expect("a replica");
    let since = "2026-10-01T11:59:00Z";
    history(&server, "Organization", since, &[]).await;
    history(
        &server,
        "Endpoint",
        since,
        &[
            deletion("Endpoint/ep-b"),
            version(
                &url("Endpoint", "ep-a"),
                "PUT",
                "Endpoint/ep-a",
                &endpoint(
                    "ep-a",
                    "urn:oid:2.999.99",
                    "org-a",
                    "https://cdr-a.example.org",
                ),
            ),
        ],
    )
    .await;
    let next = replica
        .refreshed(&client, Some("2026-10-01T11:59:00Z"), &mut budget())
        .await
        .expect("a refresh")
        .into_replica();
    assert_eq!((1, 0), next.len());
}

#[tokio::test]
async fn a_history_of_resources_out_of_scope_changes_nothing() {
    let server = seeded(1).await;
    let client = client(&server);
    let replica = Replica::read(&client, scope(), &mut budget())
        .await
        .expect("a replica");
    let since = "2026-10-01T11:59:00Z";
    history(&server, "Organization", since, &[]).await;
    history(
        &server,
        "Endpoint",
        since,
        &[version(
            &url("Endpoint", "ep-x"),
            "POST",
            "Endpoint",
            &endpoint("ep-x", "urn:oid:2.999.99", "org-x", "https://x.example.org"),
        )],
    )
    .await;
    let refresh = replica
        .refreshed(&client, Some("2026-10-01T11:59:00Z"), &mut budget())
        .await
        .expect("a refresh");
    let Refresh::Unchanged(next) = refresh else {
        panic!("nothing in scope changed: {refresh:?}");
    };
    assert_eq!(replica.len(), next.len());
    assert_eq!(
        Some(REFRESHED_AT),
        next.answered_at(),
        "the next refresh can ask from later"
    );
}

#[tokio::test]
async fn a_refresh_with_no_instant_reads_everything_again() {
    let organizations = vec![organization_entry("org-a", &["ep-a"])];
    let endpoints = vec![endpoint_entry("ep-a", "https://cdr-a.example.org/openehr")];
    let server = directory(&organizations, &endpoints, Some(READ_AT), 2).await;
    let client = client(&server);
    let replica = Replica::read(&client, scope(), &mut budget())
        .await
        .expect("a replica");
    let refresh = replica
        .refreshed(&client, None, &mut budget())
        .await
        .expect("a refresh");
    assert!(matches!(refresh, Refresh::Unchanged(_)), "{refresh:?}");
}

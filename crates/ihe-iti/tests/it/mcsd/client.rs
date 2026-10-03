// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! ITI-90 and ITI-91 against a stub directory: every page is read, a
//! deletion is a change, and every answer the transactions do not define is
//! an error that carries no URL and no text of the directory's.

use ihe_iti::mcsd::client::{CareResource, CareService, McsdClient, Version};
use ihe_iti::mcsd::error::{Malformation, McsdError};
use ihe_iti::outcome::IssueType;
use jiff::Timestamp;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    BASE, FHIR_JSON, PROMPT, bundle, client, deletion, endpoint, full_url, matched, organization,
    version,
};

/// A stub answering `GET path` with `status`, `media` and `body`, and the
/// `Date` header `date` when one is given.
async fn answering(
    server: &MockServer,
    at: &str,
    status: u16,
    media: &str,
    body: String,
    date: Option<&str>,
) {
    let mut template = ResponseTemplate::new(status).set_body_raw(body.into_bytes(), media);
    if let Some(date) = date {
        template = template.insert_header("Date", date);
    }
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(template)
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_search_reads_every_page_and_the_first_pages_date() {
    let server = MockServer::start().await;
    let next = format!("{}{BASE}Organization?page=2", server.uri());
    Mock::given(method("GET"))
        .and(path(format!("{BASE}Organization")))
        .and(query_param("page", "2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                bundle(
                    "searchset",
                    &[matched(
                        &full_url(&server, "Organization", "org-b"),
                        &organization("org-b", "urn:oid:2.999.10", &[]),
                    )],
                    &[],
                )
                .into_bytes(),
                FHIR_JSON,
            ),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    answering(
        &server,
        &format!("{BASE}Organization"),
        200,
        FHIR_JSON,
        bundle(
            "searchset",
            &[matched(
                &full_url(&server, "Organization", "org-a"),
                &organization("org-a", "urn:oid:2.999.10", &[]),
            )],
            &[format!(r#"{{"relation":"next","url":"{next}"}}"#)],
        ),
        Some("Sun, 06 Nov 1994 08:49:37 GMT"),
    )
    .await;

    let found = client(&server)
        .find(CareService::Organization, &[], PROMPT)
        .await
        .expect("both pages read");
    let ids: Vec<Option<&str>> = found
        .matches()
        .iter()
        .map(|found| found.resource().logical_id())
        .collect();
    assert_eq!(vec![Some("org-a"), Some("org-b")], ids);
    assert_eq!(
        Some("1994-11-06T08:49:37Z".to_owned()),
        found.answered_at().map(|at| at.to_string())
    );
}

#[tokio::test]
async fn a_search_sends_its_parameters() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{BASE}Endpoint")))
        .and(query_param("identifier", "urn:oid:2.999.11|"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(bundle("searchset", &[], &[]).into_bytes(), FHIR_JSON),
        )
        .expect(1)
        .mount(&server)
        .await;
    let found = client(&server)
        .find(
            CareService::Endpoint,
            &[("identifier", "urn:oid:2.999.11|")],
            PROMPT,
        )
        .await
        .expect("an empty searchset");
    assert!(found.matches().is_empty());
}

#[tokio::test]
async fn a_history_reads_versions_and_deletions_newest_first_since_an_instant() {
    let server = MockServer::start().await;
    let since: Timestamp = "2026-10-01T12:00:00Z".parse().expect("an instant");
    let address = "https://cdr-a.example.org/openehr";
    Mock::given(method("GET"))
        .and(path(format!("{BASE}Endpoint/_history")))
        .and(query_param("_since", "2026-10-01T12:00:00Z"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                bundle(
                    "history",
                    &[
                        deletion("Endpoint/ep-b"),
                        version(
                            &full_url(&server, "Endpoint", "ep-a"),
                            "PUT",
                            "Endpoint/ep-a",
                            &endpoint("ep-a", "urn:oid:2.999.11", "org-a", address),
                        ),
                    ],
                    &[],
                )
                .into_bytes(),
                FHIR_JSON,
            ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let updates = client(&server)
        .updates(CareService::Endpoint, since, PROMPT)
        .await
        .expect("a history");
    let [deleted, current] = updates.changes() else {
        panic!("two changes: {updates:?}");
    };
    assert_eq!("ep-b", deleted.logical_id());
    assert_eq!(&Version::Deleted, deleted.version());
    assert_eq!("ep-a", current.logical_id());
    let Version::Current { resource, .. } = current.version() else {
        panic!("a current version");
    };
    let CareResource::Endpoint(found) = resource else {
        panic!("an Endpoint");
    };
    assert_eq!(Some(address), found.address.value.as_deref());
}

#[tokio::test]
async fn a_refused_request_keeps_the_issue_codes_and_drops_the_text() {
    let server = MockServer::start().await;
    answering(
        &server,
        &format!("{BASE}Organization"),
        503,
        FHIR_JSON,
        r#"{"resourceType":"OperationOutcome","issue":[{"severity":"error","code":"transient","diagnostics":"Qz7SentinelText"}]}"#.to_owned(),
        None,
    )
    .await;
    let error = client(&server)
        .find(CareService::Organization, &[], PROMPT)
        .await
        .expect_err("a 503 is no answer");
    let McsdError::Rejected { status, issues } = &error else {
        panic!("a rejection: {error:?}");
    };
    assert_eq!(http::StatusCode::SERVICE_UNAVAILABLE, *status);
    assert_eq!(&vec![IssueType::Transient], issues);
    assert!(error.answered(), "the directory answered");
    for rendering in [error.to_string(), format!("{error:?}")] {
        assert!(!rendering.contains("Qz7Sentinel"), "{rendering}");
    }
}

#[tokio::test]
async fn an_answer_that_is_not_the_interactions_bundle_is_refused() {
    let cases = [
        (
            "application/json+wrong",
            bundle("searchset", &[], &[]),
            Malformation::NotFhirJson,
        ),
        (
            FHIR_JSON,
            bundle("history", &[], &[]),
            Malformation::BundleType {
                expected: "searchset",
            },
        ),
        (
            FHIR_JSON,
            r#"{"resourceType":"OperationOutcome","issue":[{"severity":"error","code":"processing"}]}"#.to_owned(),
            Malformation::UnexpectedResource,
        ),
    ];
    for (media, body, expected) in cases {
        let server = MockServer::start().await;
        answering(&server, &format!("{BASE}Endpoint"), 200, media, body, None).await;
        let error = client(&server)
            .find(CareService::Endpoint, &[], PROMPT)
            .await
            .expect_err("refused");
        assert!(
            matches!(&error, McsdError::Malformed(found) if *found == expected),
            "{expected:?}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_match_of_another_type_or_without_a_full_url_is_refused() {
    let server = MockServer::start().await;
    answering(
        &server,
        &format!("{BASE}Organization"),
        200,
        FHIR_JSON,
        bundle(
            "searchset",
            &[matched(
                &full_url(&server, "Endpoint", "ep-a"),
                &endpoint("ep-a", "urn:oid:2.999.11", "org-a", "https://a.example.org"),
            )],
            &[],
        ),
        None,
    )
    .await;
    let error = client(&server)
        .find(CareService::Organization, &[], PROMPT)
        .await
        .expect_err("an Endpoint is no Organization");
    assert!(matches!(
        error,
        McsdError::Malformed(Malformation::UnexpectedEntry { index: 0 })
    ));

    let server = MockServer::start().await;
    answering(
        &server,
        &format!("{BASE}Organization"),
        200,
        FHIR_JSON,
        bundle(
            "searchset",
            &[format!(
                r#"{{"resource":{}}}"#,
                organization("org-a", "urn:oid:2.999.10", &[])
            )],
            &[],
        ),
        None,
    )
    .await;
    let error = client(&server)
        .find(CareService::Organization, &[], PROMPT)
        .await
        .expect_err("a match needs its fullUrl");
    assert!(matches!(
        error,
        McsdError::Malformed(Malformation::NoFullUrl { index: 0 })
    ));
}

#[tokio::test]
async fn a_history_entry_without_a_request_or_deleting_another_type_is_refused() {
    let since: Timestamp = "2026-10-01T12:00:00Z".parse().expect("an instant");
    let cases = [
        (
            format!(
                r#"{{"fullUrl":"https://a.example.org/fhir/Endpoint/ep-a","resource":{}}}"#,
                endpoint("ep-a", "urn:oid:2.999.11", "org-a", "https://a.example.org")
            ),
            Malformation::NoRequest { index: 0 },
        ),
        (
            deletion("Organization/org-a"),
            Malformation::RequestUrl { index: 0 },
        ),
        (
            r#"{"request":{"method":"GET","url":"Endpoint/ep-a"}}"#.to_owned(),
            Malformation::Method { index: 0 },
        ),
    ];
    for (entry, expected) in cases {
        let server = MockServer::start().await;
        answering(
            &server,
            &format!("{BASE}Endpoint/_history"),
            200,
            FHIR_JSON,
            bundle("history", &[entry], &[]),
            None,
        )
        .await;
        let error = client(&server)
            .updates(CareService::Endpoint, since, PROMPT)
            .await
            .expect_err("refused");
        assert!(
            matches!(&error, McsdError::Malformed(found) if *found == expected),
            "{expected:?}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_page_link_to_another_origin_is_not_followed() {
    let server = MockServer::start().await;
    answering(
        &server,
        &format!("{BASE}Organization"),
        200,
        FHIR_JSON,
        bundle(
            "searchset",
            &[],
            &[r#"{"relation":"next","url":"https://elsewhere.example.org/fhir/Organization?page=2"}"#
                .to_owned()],
        ),
        None,
    )
    .await;
    let error = client(&server)
        .find(CareService::Organization, &[], PROMPT)
        .await
        .expect_err("the link leaves the directory");
    assert!(matches!(error, McsdError::ForeignPage));
}

#[tokio::test]
async fn an_unreachable_directory_did_not_answer() {
    let base = Url::parse(&format!("http://127.0.0.1:0{BASE}")).expect("a base");
    let client = McsdClient::new(
        base,
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client"),
    )
    .expect("a client");
    let error = client
        .find(CareService::Organization, &[], PROMPT)
        .await
        .expect_err("nothing listens");
    assert!(!error.answered(), "{error:?}");
}

#[test]
fn a_base_with_a_query_or_another_scheme_is_refused() {
    for base in [
        "https://directory.example.org/fhir?x=1",
        "ftp://directory.example.org/fhir",
        "urn:oid:2.999.1",
    ] {
        let http = reqwest::Client::new();
        assert!(
            McsdClient::new(Url::parse(base).expect("a URL"), http).is_err(),
            "{base}"
        );
    }
}

#[test]
fn the_debug_of_a_client_hides_the_userinfo_of_its_base() {
    let base = Url::parse("https://user:Qz7Sentinel@directory.example.org/fhir").expect("a URL");
    let client = McsdClient::new(base, reqwest::Client::new()).expect("a client");
    let rendered = format!("{client:?}");
    assert!(!rendered.contains("Qz7Sentinel"), "{rendered}");
    assert!(rendered.contains("directory.example.org"), "{rendered}");
}

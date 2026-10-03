// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The directory content reader: a Bundle of `Organization` and `Endpoint`
//! resources, references resolved as FHIR R4 §2.36.4.1 resolves them.
#![expect(
    clippy::disallowed_types,
    reason = "the test seam: the fixtures build FHIR JSON as values"
)]

use ihe_iti::mcsd::directory::{Directory, Resolution};
use ihe_iti::mcsd::error::DirectoryError;
use serde_json::{Value, json};

const BASE: &str = "https://directory.example.org/fhir";

fn organization(id: &str, endpoints: &[&str]) -> Value {
    let mut entry = json!({
        "fullUrl": format!("{BASE}/Organization/{id}"),
        "resource": {
            "resourceType": "Organization",
            "id": id,
            "identifier": [{"system": "https://example.org/org", "value": format!("{id}-value")}],
            "name": format!("Organisation {id}"),
        }
    });
    // The FHIR JSON decoder refuses an empty array, so none is written.
    if !endpoints.is_empty() {
        entry["resource"]["endpoint"] = endpoints
            .iter()
            .map(|e| json!({"reference": e}))
            .collect::<Vec<_>>()
            .into();
    }
    entry
}

fn endpoint(id: &str, manager: &str) -> Value {
    json!({
        "fullUrl": format!("{BASE}/Endpoint/{id}"),
        "resource": {
            "resourceType": "Endpoint",
            "id": id,
            "status": "active",
            "connectionType": {"system": "https://example.org/connection-type", "code": "example"},
            "managingOrganization": {"reference": manager},
            "payloadType": [{"text": "example"}],
            "address": format!("https://{id}.example.org/openehr"),
        }
    })
}

fn bundle(kind: &str, entries: Vec<Value>) -> Vec<u8> {
    let mut bundle = json!({"resourceType": "Bundle", "type": kind});
    if !entries.is_empty() {
        bundle["entry"] = entries.into();
    }
    bundle.to_string().into_bytes()
}

#[test]
fn a_relative_reference_resolves_on_the_root_of_a_rest_full_url() {
    let body = bundle(
        "collection",
        vec![
            organization("org-a", &["Endpoint/ep-a"]),
            endpoint("ep-a", "Organization/org-a"),
        ],
    );
    let directory = Directory::from_json(&body).expect("the Bundle reads");
    let [org] = directory.organizations() else {
        panic!("one organisation");
    };
    let [ep] = directory.endpoints() else {
        panic!("one endpoint");
    };
    assert_eq!(org.entry(), 0);
    assert_eq!(ep.entry(), 1);
    assert_eq!(org.name(), Some("Organisation org-a"));
    assert_eq!(org.active(), None);
    assert_eq!(
        org.identifier_values("https://example.org/org")
            .collect::<Vec<_>>(),
        vec![Some("org-a-value")]
    );
    assert_eq!(ep.status(), Some("active"));
    assert_eq!(
        ep.connection_type_system(),
        Some("https://example.org/connection-type")
    );
    assert_eq!(ep.connection_type_code(), Some("example"));
    assert_eq!(ep.address(), Some("https://ep-a.example.org/openehr"));
    assert_eq!(ep.logical_id(), Some("ep-a"));
    assert_eq!(
        directory.managing_organization(ep),
        Some(Resolution::Found(org))
    );
    assert_eq!(
        directory.endpoints_of(org).collect::<Vec<_>>(),
        vec![Resolution::Found(ep)]
    );
}

#[test]
fn an_absolute_reference_matches_a_full_url() {
    let body = bundle(
        "searchset",
        vec![
            json!({
                "fullUrl": "urn:uuid:9b1f0c1e-5d2a-4c8e-9a51-2f0e7b3c6d10",
                "resource": {"resourceType": "Organization"}
            }),
            endpoint("ep-a", "urn:uuid:9b1f0c1e-5d2a-4c8e-9a51-2f0e7b3c6d10"),
        ],
    );
    let directory = Directory::from_json(&body).expect("the Bundle reads");
    let [org] = directory.organizations() else {
        panic!("one organisation");
    };
    let ep = directory.endpoints().first().expect("one endpoint");
    assert_eq!(
        directory.managing_organization(ep),
        Some(Resolution::Found(org))
    );
}

#[test]
fn a_relative_reference_from_an_entry_without_a_rest_full_url_has_no_meaning() {
    let mut ep = endpoint("ep-a", "Organization/org-a");
    ep["fullUrl"] = json!("urn:uuid:0d6f8a2e-1b3c-4d5e-8f90-a1b2c3d4e5f6");
    let body = bundle("collection", vec![organization("org-a", &[]), ep]);
    let directory = Directory::from_json(&body).expect("the Bundle reads");
    let ep = directory.endpoints().first().expect("one endpoint");
    assert_eq!(
        directory.managing_organization(ep),
        Some(Resolution::Outside("Organization/org-a"))
    );
}

#[test]
fn a_reference_the_bundle_does_not_hold_is_outside_it() {
    let body = bundle(
        "collection",
        vec![
            organization("org-a", &["Endpoint/ep-z"]),
            endpoint("ep-a", "Organization/org-z"),
        ],
    );
    let directory = Directory::from_json(&body).expect("the Bundle reads");
    let ep = directory.endpoints().first().expect("one endpoint");
    let org = directory.organizations().first().expect("one organisation");
    assert_eq!(
        directory.managing_organization(ep),
        Some(Resolution::Outside("Organization/org-z"))
    );
    assert_eq!(
        directory.endpoints_of(org).collect::<Vec<_>>(),
        vec![Resolution::Outside("Endpoint/ep-z")]
    );
}

#[test]
fn a_reference_by_identifier_alone_is_not_literal() {
    let mut ep = endpoint("ep-a", "unused");
    ep["resource"]["managingOrganization"] =
        json!({"identifier": {"system": "https://example.org/org", "value": "org-a-value"}});
    let body = bundle("collection", vec![organization("org-a", &[]), ep]);
    let directory = Directory::from_json(&body).expect("the Bundle reads");
    let ep = directory.endpoints().first().expect("one endpoint");
    assert_eq!(
        directory.managing_organization(ep),
        Some(Resolution::NotLiteral)
    );
}

#[test]
fn an_endpoint_without_a_managing_organisation_answers_none() {
    let mut ep = endpoint("ep-a", "unused");
    if let Some(resource) = ep["resource"].as_object_mut() {
        resource.remove("managingOrganization");
    }
    let directory = Directory::from_json(&bundle("collection", vec![ep])).expect("reads");
    let ep = directory.endpoints().first().expect("one endpoint");
    assert_eq!(directory.managing_organization(ep), None);
}

#[test]
fn an_identifier_with_the_system_and_no_value_is_reported_as_none() {
    let mut org = organization("org-a", &[]);
    org["resource"]["identifier"] = json!([
        {"system": "https://example.org/org"},
        {"system": "https://example.org/other", "value": "x"},
    ]);
    let directory = Directory::from_json(&bundle("collection", vec![org])).expect("reads");
    let org = directory.organizations().first().expect("one organisation");
    assert_eq!(
        org.identifier_values("https://example.org/org")
            .collect::<Vec<_>>(),
        vec![None]
    );
}

#[test]
fn text_that_is_not_json_is_refused() {
    assert!(matches!(
        Directory::from_json(b"{"),
        Err(DirectoryError::NotJson { .. })
    ));
}

#[test]
fn a_resource_other_than_a_bundle_is_refused() {
    let body = json!({"resourceType": "Organization"}).to_string();
    assert_eq!(
        Directory::from_json(body.as_bytes()),
        Err(DirectoryError::NotABundle)
    );
    assert_eq!(Directory::from_json(b"[]"), Err(DirectoryError::NotABundle));
}

#[test]
fn a_bundle_of_another_type_is_refused() {
    assert_eq!(
        Directory::from_json(&bundle("transaction", vec![])),
        Err(DirectoryError::BundleType {
            found: Some("transaction".to_owned())
        })
    );
}

#[test]
fn an_endpoint_that_does_not_decode_is_refused() {
    let mut ep = endpoint("ep-a", "Organization/org-a");
    if let Some(resource) = ep["resource"].as_object_mut() {
        resource.remove("address");
    }
    assert!(matches!(
        Directory::from_json(&bundle("collection", vec![ep])),
        Err(DirectoryError::Decode(_))
    ));
}

#[test]
fn an_entry_without_a_resource_is_refused() {
    let body = bundle("collection", vec![json!({"fullUrl": "urn:uuid:x"})]);
    assert_eq!(
        Directory::from_json(&body),
        Err(DirectoryError::NoResource { index: 0 })
    );
}

#[test]
fn an_entry_of_another_resource_is_refused() {
    let body = bundle(
        "collection",
        vec![
            organization("org-a", &[]),
            json!({"resource": {"resourceType": "Basic", "code": {"text": "x"}}}),
        ],
    );
    assert_eq!(
        Directory::from_json(&body),
        Err(DirectoryError::UnexpectedEntry { index: 1 })
    );
}

#[test]
fn a_modifier_extension_is_refused() {
    let mut ep = endpoint("ep-a", "Organization/org-a");
    ep["resource"]["modifierExtension"] =
        json!([{"url": "https://example.org/modifier", "valueBoolean": true}]);
    assert_eq!(
        Directory::from_json(&bundle("collection", vec![ep])),
        Err(DirectoryError::ModifierExtension { index: 0 })
    );
    let mut org = organization("org-a", &[]);
    org["resource"]["modifierExtension"] =
        json!([{"url": "https://example.org/modifier", "valueBoolean": true}]);
    assert_eq!(
        Directory::from_json(&bundle("collection", vec![org])),
        Err(DirectoryError::ModifierExtension { index: 0 })
    );
}

#[test]
fn a_repeated_full_url_is_refused() {
    let mut second = organization("org-b", &[]);
    second["fullUrl"] = json!(format!("{BASE}/Organization/org-a"));
    let body = bundle("collection", vec![organization("org-a", &[]), second]);
    assert_eq!(
        Directory::from_json(&body),
        Err(DirectoryError::DuplicateFullUrl { index: 1 })
    );
}

#[test]
fn a_repeated_logical_id_is_refused() {
    let mut second = endpoint("ep-a", "Organization/org-a");
    second["fullUrl"] = json!("urn:uuid:other");
    let body = bundle(
        "collection",
        vec![endpoint("ep-a", "Organization/org-a"), second],
    );
    assert_eq!(
        Directory::from_json(&body),
        Err(DirectoryError::DuplicateId { index: 1 })
    );
    let mut org = organization("org-b", &[]);
    org["resource"]["id"] = json!("org-a");
    let body = bundle("collection", vec![organization("org-a", &[]), org]);
    assert_eq!(
        Directory::from_json(&body),
        Err(DirectoryError::DuplicateId { index: 1 })
    );
}

#[test]
fn an_organisation_and_an_endpoint_may_share_a_logical_id() {
    let body = bundle(
        "collection",
        vec![
            organization("same", &[]),
            endpoint("same", "Organization/same"),
        ],
    );
    let directory = Directory::from_json(&body).expect("the Bundle reads");
    assert_eq!(directory.organizations().len(), 1);
    assert_eq!(directory.endpoints().len(), 1);
}

#[test]
fn a_directory_shows_no_credential() {
    const USER: &str = "Qz7user";
    const PASSWORD: &str = "Qz7password";
    const TOKEN: &str = "Qz7token";
    let mut org = organization("org-a", &[]);
    org["fullUrl"] =
        format!("https://{USER}:{PASSWORD}@directory.example.org/fhir/Organization/org-a").into();
    let mut ep = endpoint("ep-a", "Organization/org-a");
    ep["fullUrl"] =
        format!("https://{USER}:{PASSWORD}@directory.example.org/fhir/Endpoint/ep-a").into();
    ep["resource"]["address"] =
        format!("https://{USER}:{PASSWORD}@cdr-a.example.org/openehr?access_token={TOKEN}").into();
    ep["resource"]["header"] = json!([format!("Authorization: Bearer {TOKEN}")]);
    let directory =
        Directory::from_json(&bundle("collection", vec![org, ep])).expect("the Bundle reads");
    for shown in [format!("{directory:?}"), format!("{directory:#?}")] {
        for credential in [USER, PASSWORD, TOKEN] {
            assert!(!shown.contains(credential), "{shown}");
        }
        for redacted in [
            "https://***@directory.example.org/fhir/Organization/org-a",
            "https://***@directory.example.org/fhir/Endpoint/ep-a",
            "https://***@cdr-a.example.org/openehr?***",
        ] {
            assert!(shown.contains(redacted), "{shown}");
        }
    }
}

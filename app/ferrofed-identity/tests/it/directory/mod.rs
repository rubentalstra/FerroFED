// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The registry document in FHIR form (N19, N20, N21, §15.2): one synthetic
//! federation written in both forms, nothing real.
#![expect(
    clippy::disallowed_types,
    reason = "the test seam: the fixtures build FHIR JSON as values"
)]

mod equivalence;
mod mcsd;
mod refusal;

use ferrofed_identity::directory::{
    CREATING_SYSTEM_ID_SYSTEM, ENDPOINT_ID_SYSTEM, NODE_ID_SYSTEM, ORGANISATION_ID_SYSTEM,
    SYSTEM_ID_SYSTEM,
};
use ferrofed_registry::snapshot::ConnectionType;
use serde_json::{Value, json};

/// Two organisations, two nodes, three endpoints: `node-a` exposes two, one
/// managed by `org-region` (N20 allows a manager other than the operator) and
/// suspended, and one `creating_system_id` beyond the members' own.
pub(crate) const NATIVE: &str = r#"
[[organisation]]
id = "org-a"
name = "Hospital A"

[[organisation]]
id = "org-region"

[[node]]
id = "node-a"
organisation = "org-a"
system_id = "cdr-a.example.org"

[[node]]
id = "node-b"
organisation = "org-region"
system_id = "2.999.20.1"

[[endpoint]]
id = "node-a-pub"
node = "node-a"
url = "https://cdr-a.example.org/openehr"
connection_type = "openehr-rest-query"
managing_organisation = "org-a"

[[endpoint]]
id = "node-a-region"
node = "node-a"
url = "https://internal.cdr-a.example.org/openehr"
connection_type = "openehr-rest-query"
managing_organisation = "org-region"
status = "suspended"

[[endpoint]]
id = "node-b-pub"
node = "node-b"
url = "http://cdr-b.example.org:8080/openehr"
connection_type = "openehr-rest-query"
managing_organisation = "org-region"

[[creating_system]]
creating_system_id = "legacy-a.example.org"
endpoint = "node-a-pub"
"#;

const BASE: &str = "https://registry.example.org/fhir";

/// The entry positions of the [`fhir`] Bundle.
pub(crate) const ORG_A: usize = 0;
pub(crate) const ORG_REGION: usize = 1;
pub(crate) const NODE_A_PUB: usize = 2;
pub(crate) const NODE_A_REGION: usize = 3;
pub(crate) const NODE_B_PUB: usize = 4;

fn identifier(system: &str, value: &str) -> Value {
    json!({"system": system, "value": value})
}

fn organisation(id: &str, name: Option<&str>, endpoints: &[&str]) -> Value {
    let mut resource = json!({
        "resourceType": "Organization",
        "id": id,
        "identifier": [identifier(ORGANISATION_ID_SYSTEM, id)],
        "endpoint": endpoints
            .iter()
            .map(|endpoint| json!({"reference": format!("Endpoint/{endpoint}")}))
            .collect::<Vec<_>>(),
    });
    if let Some(name) = name {
        resource["name"] = json!(name);
    }
    json!({"fullUrl": format!("{BASE}/Organization/{id}"), "resource": resource})
}

struct EndpointSpec<'a> {
    id: &'a str,
    node: &'a str,
    system_id: &'a str,
    creating_systems: &'a [&'a str],
    status: &'a str,
    manager: &'a str,
    address: &'a str,
}

fn endpoint(spec: &EndpointSpec<'_>) -> Value {
    let mut identifiers = vec![
        identifier(ENDPOINT_ID_SYSTEM, spec.id),
        identifier(NODE_ID_SYSTEM, spec.node),
        identifier(SYSTEM_ID_SYSTEM, spec.system_id),
    ];
    identifiers.extend(
        spec.creating_systems
            .iter()
            .map(|id| identifier(CREATING_SYSTEM_ID_SYSTEM, id)),
    );
    json!({
        "fullUrl": format!("{BASE}/Endpoint/{}", spec.id),
        "resource": {
            "resourceType": "Endpoint",
            "id": spec.id,
            "identifier": identifiers,
            "status": spec.status,
            "connectionType": {"system": ConnectionType::SYSTEM, "code": "openehr-rest-query"},
            "managingOrganization": {"reference": format!("Organization/{}", spec.manager)},
            "payloadType": [{"text": "openEHR"}],
            "address": spec.address,
        }
    })
}

/// The federation of [`NATIVE`] as a FHIR R4 `collection` Bundle.
pub(crate) fn fhir() -> Value {
    json!({
        "resourceType": "Bundle",
        "type": "collection",
        "entry": [
            organisation("org-a", Some("Hospital A"), &["node-a-pub", "node-a-region"]),
            organisation("org-region", None, &["node-b-pub"]),
            endpoint(&EndpointSpec {
                id: "node-a-pub",
                node: "node-a",
                system_id: "cdr-a.example.org",
                creating_systems: &["legacy-a.example.org"],
                status: "active",
                manager: "org-a",
                address: "https://cdr-a.example.org/openehr",
            }),
            endpoint(&EndpointSpec {
                id: "node-a-region",
                node: "node-a",
                system_id: "cdr-a.example.org",
                creating_systems: &[],
                status: "suspended",
                manager: "org-region",
                address: "https://internal.cdr-a.example.org/openehr",
            }),
            endpoint(&EndpointSpec {
                id: "node-b-pub",
                node: "node-b",
                system_id: "2.999.20.1",
                creating_systems: &[],
                status: "active",
                manager: "org-region",
                address: "http://cdr-b.example.org:8080/openehr",
            }),
        ]
    })
}

/// The resource of entry `index`, for a test that breaks it.
pub(crate) fn resource(bundle: &mut Value, index: usize) -> &mut Value {
    &mut bundle["entry"][index]["resource"]
}

/// The bytes of a Bundle.
pub(crate) fn bytes(bundle: &Value) -> Vec<u8> {
    bundle.to_string().into_bytes()
}

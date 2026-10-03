// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The registry document in FHIR form (N19).
//!
//! The members are FHIR R4 `Organization` and `Endpoint` resources, read into
//! the same members, organisations, `system_id`s and managing organisations
//! as the native form. The document is one FHIR R4 JSON `Bundle` of type `collection` or
//! `searchset`, read by `ihe_iti`'s mCSD directory reader, so it has the
//! shape an mCSD directory delivers (§15.1). The registry's own ids travel as
//! identifiers in FerroFED's systems, because a FHIR logical id belongs to the
//! server that holds the resource (no specification governs this: our own
//! design):
//!
//! - an `Organization` carries one [`ORGANISATION_ID_SYSTEM`] identifier;
//! - an `Endpoint` carries one [`ENDPOINT_ID_SYSTEM`] identifier, its stable
//!   `endpoint_id` (N19), one [`NODE_ID_SYSTEM`] identifier and one
//!   [`SYSTEM_ID_SYSTEM`] identifier, the node it belongs to and that node's
//!   openEHR `system_id`, and one [`CREATING_SYSTEM_ID_SYSTEM`] identifier for
//!   each other `creating_system_id` it answers for (N21, §12.2);
//! - an `Endpoint`'s `connectionType` is `openehr-rest-query` in
//!   [`ConnectionType::SYSTEM`], and nothing else (N19, §15.2, CP-20);
//! - its `managingOrganization` names its one managing organisation (N20),
//!   and the one `Organization` whose `endpoint` list names it operates its
//!   node;
//! - an `Endpoint` MAY carry one [`CONSENT_REFUSAL_CODE_EXTENSION`] extension
//!   per ITS-REST `Error` `code` its node marks a consent refusal with, the
//!   `consent_refusal_codes` of the native form (§11.1, N27).
//!
//! Every endpoint of a node agrees on the node's `system_id` and operator.
//! The node's `product`, `version` and identifiers have no place in this form.
//!
//! ```
//! use ferrofed_identity::directory;
//! use ferrofed_registry::id::{NodeId, SystemId};
//!
//! let registry = directory::snapshot_from_json(br#"{
//!   "resourceType": "Bundle",
//!   "type": "collection",
//!   "entry": [
//!     {
//!       "fullUrl": "https://registry.example.org/fhir/Organization/org-a",
//!       "resource": {
//!         "resourceType": "Organization",
//!         "id": "org-a",
//!         "identifier": [{"system": "https://ferrofed.eu/fhir/sid/organisation-id", "value": "org-a"}],
//!         "endpoint": [{"reference": "Endpoint/node-a-pub"}]
//!       }
//!     },
//!     {
//!       "fullUrl": "https://registry.example.org/fhir/Endpoint/node-a-pub",
//!       "resource": {
//!         "resourceType": "Endpoint",
//!         "id": "node-a-pub",
//!         "identifier": [
//!           {"system": "https://ferrofed.eu/fhir/sid/endpoint-id", "value": "node-a-pub"},
//!           {"system": "https://ferrofed.eu/fhir/sid/node-id", "value": "node-a"},
//!           {"system": "https://ferrofed.eu/fhir/sid/system-id", "value": "cdr-a.example.org"}
//!         ],
//!         "status": "active",
//!         "connectionType": {
//!           "system": "https://ferrofed.eu/fhir/CodeSystem/connection-type",
//!           "code": "openehr-rest-query"
//!         },
//!         "managingOrganization": {"reference": "Organization/org-a"},
//!         "payloadType": [{"text": "openEHR"}],
//!         "address": "https://cdr-a.example.org/openehr"
//!       }
//!     }
//!   ]
//! }"#)?;
//!
//! let system_id: SystemId = "cdr-a.example.org".parse()?;
//! let node = registry.node_for_system_id(&system_id).map(|node| node.id());
//! assert_eq!(node, Some(&"node-a".parse::<NodeId>()?));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod error;
pub mod mcsd;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;

use ferrofed_registry::document::{
    CreatingSystemDoc, Document, EndpointDoc, NodeDoc, OrganisationDoc,
};
use ferrofed_registry::error::IdError;
use ferrofed_registry::id::{EndpointId, NodeId, OrganisationId, SystemId};
use ferrofed_registry::secret::SecretUrl;
use ferrofed_registry::snapshot::{ConnectionType, EndpointStatus, RegistrySnapshot};
use ihe_iti::mcsd::directory::{Directory, DirectoryEndpoint, Resolution};

use error::{
    ConnectionTypeFault, Entry, FhirFormError, IdentifierFault, OperatorFault, ReferenceFault,
};

/// The identifier system of an organisation's registry id.
pub const ORGANISATION_ID_SYSTEM: &str = "https://ferrofed.eu/fhir/sid/organisation-id";

/// The identifier system of an endpoint's stable `endpoint_id` (N19).
pub const ENDPOINT_ID_SYSTEM: &str = "https://ferrofed.eu/fhir/sid/endpoint-id";

/// The identifier system of the `node_id` an endpoint belongs to.
pub const NODE_ID_SYSTEM: &str = "https://ferrofed.eu/fhir/sid/node-id";

/// The identifier system of the openEHR `system_id` of an endpoint's node.
pub const SYSTEM_ID_SYSTEM: &str = "https://ferrofed.eu/fhir/sid/system-id";

/// The identifier system of a `creating_system_id` an endpoint answers for,
/// other than its node's own `system_id` (N21, §12.2).
pub const CREATING_SYSTEM_ID_SYSTEM: &str = "https://ferrofed.eu/fhir/sid/creating-system-id";

/// The extension of an `Endpoint` naming one consent refusal code.
///
/// Its `valueCode` is one ITS-REST `Error` `code` by which the endpoint's node
/// marks a `403` as a consent refusal, repeated once per code (§11.1, N27; no
/// specification governs the extension: our own design).
pub const CONSENT_REFUSAL_CODE_EXTENSION: &str =
    "https://ferrofed.eu/fhir/StructureDefinition/consent-refusal-code";

/// Reads and validates the registry document in FHIR form at `path`.
///
/// # Errors
///
/// [`FhirFormError::Read`] when the file cannot be read, and every error of
/// [`snapshot_from_json`].
pub fn read(path: &Path) -> Result<RegistrySnapshot, FhirFormError> {
    let body = std::fs::read(path).map_err(|source| FhirFormError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    snapshot_from_json(&body)
}

/// Parses and validates a registry document in FHIR form.
///
/// # Errors
///
/// [`FhirFormError::Directory`] when the text is not a Bundle of
/// `Organization` and `Endpoint` resources, the variant naming the resource
/// for an id, a connection type (N19, §15.2), a status, an address, a
/// managing organisation (N20) or an operator the form does not admit, and
/// [`FhirFormError::Registry`] for a membership rule the native form shares.
pub fn snapshot_from_json(body: &[u8]) -> Result<RegistrySnapshot, FhirFormError> {
    let directory = Directory::from_json(body).map_err(FhirFormError::Directory)?;
    snapshot_from_directory(&directory)
}

/// Validates directory content as a registry in FHIR form, whether a
/// document carried it or a care services directory answered it.
///
/// # Errors
///
/// Every error of [`snapshot_from_json`] but [`FhirFormError::Directory`],
/// which the content's reader has already decided.
pub fn snapshot_from_directory(directory: &Directory) -> Result<RegistrySnapshot, FhirFormError> {
    let document = document(directory)?;
    RegistrySnapshot::from_document(document).map_err(FhirFormError::Registry)
}

fn document(directory: &Directory) -> Result<Document, FhirFormError> {
    let mut organisations = Vec::new();
    let mut organisation_at: BTreeMap<usize, OrganisationId> = BTreeMap::new();
    for organisation in directory.organizations() {
        let id =
            single(organisation.identifier_values(ORGANISATION_ID_SYSTEM)).map_err(|fault| {
                FhirFormError::OrganisationId {
                    organisation: Entry::new(organisation.entry(), organisation.logical_id()),
                    fault,
                }
            })?;
        if organisation.active() == Some(false) {
            return Err(FhirFormError::OrganisationInactive(id));
        }
        organisation_at.insert(organisation.entry(), id.clone());
        organisations.push(OrganisationDoc {
            id,
            name: organisation.name().map(str::to_owned),
        });
    }
    let operators = operators(directory, &organisation_at)?;
    let mut nodes: Vec<NodeDoc> = Vec::new();
    let mut endpoints = Vec::new();
    let mut creating_systems = Vec::new();
    for endpoint in directory.endpoints() {
        let member = member(directory, endpoint, &organisation_at, &operators)?;
        join_node(
            &mut nodes,
            member.endpoint.node.clone(),
            member.operator,
            member.system_id,
        )?;
        endpoints.push(member.endpoint);
        creating_systems.extend(member.creating_systems);
    }
    Ok(Document {
        organisations,
        nodes,
        endpoints,
        creating_systems,
    })
}

/// What one `Endpoint` declares: the endpoint, its node's operator and
/// `system_id`, and the `creating_system_id`s it answers for.
struct Member {
    endpoint: EndpointDoc,
    operator: OrganisationId,
    system_id: SystemId,
    creating_systems: Vec<CreatingSystemDoc>,
}

fn member(
    directory: &Directory,
    endpoint: &DirectoryEndpoint,
    organisation_at: &BTreeMap<usize, OrganisationId>,
    operators: &BTreeMap<usize, Vec<OrganisationId>>,
) -> Result<Member, FhirFormError> {
    let id: EndpointId =
        single(endpoint.identifier_values(ENDPOINT_ID_SYSTEM)).map_err(|fault| {
            FhirFormError::EndpointId {
                endpoint: Entry::new(endpoint.entry(), endpoint.logical_id()),
                fault,
            }
        })?;
    let connection_type =
        connection_type(endpoint).map_err(|fault| FhirFormError::ConnectionType {
            endpoint: id.clone(),
            fault,
        })?;
    let status = match endpoint.status() {
        Some("active") => EndpointStatus::Active,
        Some("suspended") => EndpointStatus::Suspended,
        found => {
            return Err(FhirFormError::Status {
                endpoint: id,
                found: found.map(str::to_owned),
            });
        }
    };
    let Some(url) = endpoint.address() else {
        return Err(FhirFormError::Address(id));
    };
    let managing_organisation = managing_organisation(directory, endpoint, organisation_at)
        .map_err(|fault| FhirFormError::ManagingOrganisation {
            endpoint: id.clone(),
            fault,
        })?;
    let node: NodeId = single(endpoint.identifier_values(NODE_ID_SYSTEM)).map_err(|fault| {
        FhirFormError::NodeId {
            endpoint: id.clone(),
            fault,
        }
    })?;
    let system_id: SystemId =
        single(endpoint.identifier_values(SYSTEM_ID_SYSTEM)).map_err(|fault| {
            FhirFormError::SystemId {
                endpoint: id.clone(),
                fault,
            }
        })?;
    let mut creating_systems = Vec::new();
    for value in endpoint.identifier_values(CREATING_SYSTEM_ID_SYSTEM) {
        let creating_system_id =
            usable(value).map_err(|fault| FhirFormError::CreatingSystemId {
                endpoint: id.clone(),
                fault,
            })?;
        creating_systems.push(CreatingSystemDoc {
            creating_system_id,
            endpoint: id.clone(),
        });
    }
    let mut consent_refusal_codes = Vec::new();
    for code in endpoint.extension_codes(CONSENT_REFUSAL_CODE_EXTENSION) {
        let Some(code) = code else {
            return Err(FhirFormError::ConsentRefusalCode(id));
        };
        consent_refusal_codes.push(code.to_owned());
    }
    let operator = match operators.get(&endpoint.entry()).map(Vec::as_slice) {
        Some([operator]) => operator.clone(),
        Some([first, second, ..]) => {
            return Err(FhirFormError::Operator {
                endpoint: id,
                fault: OperatorFault::Several {
                    first: first.clone(),
                    second: second.clone(),
                },
            });
        }
        Some([]) | None => {
            return Err(FhirFormError::Operator {
                endpoint: id,
                fault: OperatorFault::Unlisted,
            });
        }
    };
    Ok(Member {
        endpoint: EndpointDoc {
            id,
            node,
            url: SecretUrl::new(url),
            connection_type,
            managing_organisation,
            status,
            consent_refusal_codes,
        },
        operator,
        system_id,
        creating_systems,
    })
}

/// The organisations that list each endpoint, keyed by the endpoint's entry,
/// each organisation once and in document order.
fn operators(
    directory: &Directory,
    organisation_at: &BTreeMap<usize, OrganisationId>,
) -> Result<BTreeMap<usize, Vec<OrganisationId>>, FhirFormError> {
    let mut operators: BTreeMap<usize, Vec<OrganisationId>> = BTreeMap::new();
    for organisation in directory.organizations() {
        let Some(id) = organisation_at.get(&organisation.entry()) else {
            continue;
        };
        let mut listed = BTreeSet::new();
        for resolution in directory.endpoints_of(organisation) {
            let endpoint = match resolution {
                Resolution::Found(endpoint) => endpoint,
                Resolution::Outside(reference) => {
                    return Err(FhirFormError::OrganisationEndpoint {
                        organisation: id.clone(),
                        fault: ReferenceFault::Outside(reference.to_owned()),
                    });
                }
                Resolution::NotLiteral => {
                    return Err(FhirFormError::OrganisationEndpoint {
                        organisation: id.clone(),
                        fault: ReferenceFault::NotLiteral,
                    });
                }
            };
            // NOTE: no specification governs this: our own design; a second listing
            // by the same organisation names the same one operator.
            if listed.insert(endpoint.entry()) {
                operators
                    .entry(endpoint.entry())
                    .or_default()
                    .push(id.clone());
            }
        }
    }
    Ok(operators)
}

/// Adds an endpoint's node, or checks that the node it names agrees on its
/// operator and `system_id`.
fn join_node(
    nodes: &mut Vec<NodeDoc>,
    id: NodeId,
    organisation: OrganisationId,
    system_id: SystemId,
) -> Result<(), FhirFormError> {
    let Some(node) = nodes.iter().find(|node| node.id == id) else {
        nodes.push(NodeDoc {
            id,
            organisation,
            system_id,
            product: None,
            version: None,
            identifiers: Vec::new(),
        });
        return Ok(());
    };
    if node.organisation != organisation {
        return Err(FhirFormError::NodeOperator {
            node: id,
            first: node.organisation.clone(),
            second: organisation,
        });
    }
    if node.system_id != system_id {
        return Err(FhirFormError::NodeSystemId {
            node: id,
            first: node.system_id.clone(),
            second: system_id,
        });
    }
    Ok(())
}

/// The connection type an endpoint's `connectionType` codes (N19, §15.2).
// NOTE: N19, §15.2: only a defined code is accepted, and no openEHR or HL7 system
// is registered for one, so the one bound system is FerroFED's own.
fn connection_type(endpoint: &DirectoryEndpoint) -> Result<ConnectionType, ConnectionTypeFault> {
    let Some(code) = endpoint.connection_type_code() else {
        return Err(ConnectionTypeFault::NoCode);
    };
    if code == "hl7-fhir-rest" {
        return Err(ConnectionTypeFault::FhirRest);
    }
    let Some(system) = endpoint.connection_type_system() else {
        return Err(ConnectionTypeFault::NoSystem {
            code: code.to_owned(),
        });
    };
    (system == ConnectionType::SYSTEM)
        .then(|| ConnectionType::from_code(code))
        .flatten()
        .ok_or_else(|| ConnectionTypeFault::Unbound {
            system: system.to_owned(),
            code: code.to_owned(),
        })
}

/// The organisation an endpoint's `managingOrganization` names (N20).
fn managing_organisation(
    directory: &Directory,
    endpoint: &DirectoryEndpoint,
    organisation_at: &BTreeMap<usize, OrganisationId>,
) -> Result<OrganisationId, ReferenceFault> {
    match directory.managing_organization(endpoint) {
        None => Err(ReferenceFault::Missing),
        Some(Resolution::NotLiteral) => Err(ReferenceFault::NotLiteral),
        Some(Resolution::Outside(reference)) => Err(ReferenceFault::Outside(reference.to_owned())),
        Some(Resolution::Found(organisation)) => organisation_at
            .get(&organisation.entry())
            .cloned()
            .ok_or(ReferenceFault::Missing),
    }
}

/// The one identifier value of a system, parsed as the id it carries.
fn single<'a, T>(mut values: impl Iterator<Item = Option<&'a str>>) -> Result<T, IdentifierFault>
where
    T: FromStr<Err = IdError>,
{
    let Some(value) = values.next() else {
        return Err(IdentifierFault::Missing);
    };
    if values.next().is_some() {
        return Err(IdentifierFault::Repeated);
    }
    usable(value)
}

/// An identifier value parsed as the id it carries.
fn usable<T>(value: Option<&str>) -> Result<T, IdentifierFault>
where
    T: FromStr<Err = IdError>,
{
    value
        .ok_or(IdentifierFault::NoValue)?
        .parse()
        .map_err(IdentifierFault::Malformed)
}

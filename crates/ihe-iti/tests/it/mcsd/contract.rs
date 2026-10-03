// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The client held to the vendored mCSD 4.0.0 artefacts: the interactions and
//! search parameters it uses are the ones the Directory and Query Client of
//! ITI-90 and the Directory and Update Client of ITI-91 declare, the IG's
//! example Organizations and Endpoints read as directory content, and the
//! Endpoint profile's `connectionType` binding is what a registry reads.

use ihe_iti::mcsd::client::{CareService, McsdClient};
use ihe_iti::mcsd::directory::Directory;
use serde::Deserialize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{BASE, FHIR_JSON, budget, bundle, client, matched, vendored};

/// The members of a `CapabilityStatement` the client answers to.
#[derive(Debug, Deserialize)]
struct CapabilityStatement {
    rest: Vec<Rest>,
}

#[derive(Debug, Deserialize)]
struct Rest {
    mode: String,
    resource: Vec<Resource>,
}

#[derive(Debug, Deserialize)]
struct Resource {
    #[serde(rename = "type")]
    kind: String,
    interaction: Vec<Interaction>,
    #[serde(rename = "searchParam", default)]
    search_param: Vec<SearchParam>,
}

#[derive(Debug, Deserialize)]
struct Interaction {
    code: String,
}

#[derive(Debug, Deserialize)]
struct SearchParam {
    name: String,
}

/// The `kind` resource of the one `rest` entry in `mode` of the vendored
/// capability statement `file`.
fn declared(file: &str, mode: &str, kind: CareService) -> Resource {
    let statement: CapabilityStatement =
        serde_json::from_str(&vendored(file)).expect("a CapabilityStatement");
    statement
        .rest
        .into_iter()
        .find(|rest| rest.mode == mode)
        .expect("the rest entry of the mode")
        .resource
        .into_iter()
        .find(|resource| resource.kind == kind.resource_type())
        .expect("the resource type")
}

fn interactions(resource: &Resource) -> Vec<&str> {
    resource
        .interaction
        .iter()
        .map(|interaction| interaction.code.as_str())
        .collect()
}

fn parameters(resource: &Resource) -> Vec<&str> {
    resource
        .search_param
        .iter()
        .map(|parameter| parameter.name.as_str())
        .collect()
}

#[test]
fn iti_90_searches_both_types_by_identifier_on_both_sides() {
    for kind in [CareService::Organization, CareService::Endpoint] {
        for (file, mode) in [
            ("CapabilityStatement-IHE.mCSD.Directory.json", "server"),
            ("CapabilityStatement-IHE.mCSD.QueryClient.json", "client"),
        ] {
            let resource = declared(file, mode, kind);
            assert!(
                interactions(&resource).contains(&"search-type"),
                "{file}: {kind:?}"
            );
            assert!(
                parameters(&resource).contains(&"identifier"),
                "{file}: {kind:?} declares the identifier search the scoped read uses"
            );
        }
    }
}

#[test]
fn iti_91_is_the_type_history_since_an_instant_on_both_sides() {
    for kind in [CareService::Organization, CareService::Endpoint] {
        for (file, mode) in [
            (
                "CapabilityStatement-IHE.mCSD.Directory.Update.json",
                "server",
            ),
            ("CapabilityStatement-IHE.mCSD.UpdateClient.json", "client"),
        ] {
            let resource = declared(file, mode, kind);
            assert_eq!(vec!["history-type"], interactions(&resource), "{file}");
            assert_eq!(vec!["_since"], parameters(&resource), "{file}");
        }
    }
}

/// The members of a `StructureDefinition` snapshot element.
#[derive(Debug, Deserialize)]
struct StructureDefinition {
    snapshot: Snapshot,
}

#[derive(Debug, Deserialize)]
struct Snapshot {
    element: Vec<Element>,
}

#[derive(Debug, Deserialize)]
struct Element {
    id: String,
    binding: Option<Binding>,
}

#[derive(Debug, Deserialize)]
struct Binding {
    strength: String,
    #[serde(rename = "valueSet")]
    value_set: String,
}

fn binding(file: &str, id: &str) -> Binding {
    let definition: StructureDefinition =
        serde_json::from_str(&vendored(file)).expect("a StructureDefinition");
    definition
        .snapshot
        .element
        .into_iter()
        .find(|element| element.id == id)
        .and_then(|element| element.binding)
        .expect("the element is bound")
}

/// The reading the FHIR form of the registry rests on: the mCSD `Endpoint`
/// binds `connectionType` extensibly to the HL7 endpoint connection types,
/// so a code from another system may be carried in it, while only the
/// document-sharing profile requires the IHE endpoint types (§15.2, N19).
#[test]
fn the_endpoint_binds_its_connection_type_extensibly_and_only_doc_share_requires_ihe_types() {
    let endpoint = binding(
        "StructureDefinition-IHE.mCSD.Endpoint.json",
        "Endpoint.connectionType",
    );
    assert_eq!("extensible", endpoint.strength);
    assert_eq!(
        "http://hl7.org/fhir/ValueSet/endpoint-connection-type",
        endpoint.value_set
    );
    let doc_share = binding(
        "StructureDefinition-IHE.mCSD.Endpoint.DocShare.json",
        "Endpoint.connectionType",
    );
    assert_eq!("required", doc_share.strength);
    assert!(
        doc_share
            .value_set
            .starts_with("https://profiles.ihe.net/ITI/mCSD/ValueSet/"),
        "{}",
        doc_share.value_set
    );
}

/// The IG's example Organizations and Endpoints, answered by a stub
/// directory as ITI-90 searchsets, read as directory content with the
/// references between them resolved.
#[tokio::test]
async fn the_igs_examples_read_through_iti_90() {
    let server = MockServer::start().await;
    for (kind, files) in [
        (
            CareService::Organization,
            [
                "example/Organization-ex-OrgA.json",
                "example/Organization-ex-OrgB.json",
                "example/Organization-ex-OrgC.json",
            ],
        ),
        (
            CareService::Endpoint,
            [
                "example/Endpoint-ex-endpointDicom.json",
                "example/Endpoint-ex-endpointXCAquery.json",
                "example/Endpoint-ex-endpointXCAretrieve.json",
            ],
        ),
    ] {
        let entries: Vec<String> = files
            .iter()
            .map(|file| {
                let resource = vendored(file);
                let id = file
                    .trim_start_matches("example/")
                    .trim_end_matches(".json")
                    .split_once('-')
                    .map(|(_, id)| id.to_owned())
                    .expect("the file names its resource");
                matched(
                    &format!("{}{BASE}{}/{id}", server.uri(), kind.resource_type()),
                    resource.trim(),
                )
            })
            .collect();
        Mock::given(method("GET"))
            .and(path(format!("{BASE}{}", kind.resource_type())))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(bundle("searchset", &entries, &[]).into_bytes(), FHIR_JSON),
            )
            .mount(&server)
            .await;
    }
    let client: McsdClient = client(&server);
    let organizations = client
        .find(CareService::Organization, &[], &mut budget())
        .await
        .expect("the example Organizations");
    let endpoints = client
        .find(CareService::Endpoint, &[], &mut budget())
        .await
        .expect("the example Endpoints");
    assert_eq!(3, organizations.matches().len());
    assert_eq!(3, endpoints.matches().len());
    let replica = ihe_iti::mcsd::replica::Replica::read(
        &client,
        ihe_iti::mcsd::replica::Scope::everything(),
        &mut budget(),
    )
    .await
    .expect("a replica");
    let directory: Directory = replica.directory().expect("directory content");
    let query = directory
        .endpoints()
        .iter()
        .find(|endpoint| endpoint.logical_id() == Some("ex-endpointXCAquery"))
        .expect("the XCA query endpoint");
    assert_eq!(Some("ihe-xca"), query.connection_type_code());
    let manager = directory.managing_organization(query);
    assert!(
        matches!(
            manager,
            Some(ihe_iti::mcsd::directory::Resolution::Found(organization))
                if organization.logical_id() == Some("ex-OrgA")
        ),
        "the managing organisation resolves inside the directory"
    );
}

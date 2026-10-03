// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The PIXm localizer: the members whose `ehr_id` domain holds an identifier
//! for the patient are the candidates (§14.2, "demographic-registration"),
//! read from the same ITI-83 call the resolution of the query reuses, and a
//! PIX Manager that does not answer fails closed (§14.1, N4, Annex A.1).
#![allow(
    clippy::expect_used,
    reason = "fixture builders fail the test they serve on an impossible value"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use ferrofed_identity::localizer::{Localization, Localizer, LocalizerError};
use ferrofed_identity::patient::{IdentifierNamespace, PatientRef};
use ferrofed_identity::pixm::{ManagerConfig, PixAuth, PixmResolver, SHARED_CAPACITY};
use ferrofed_identity::resolver::{Resolution, Resolver};
use ferrofed_registry::id::NodeId;
use ferrofed_registry::secret::SecretUrl;
use ferrofed_testkit::mock::Server;
use secrecy::SecretString;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::support::registry;

const SENTINEL: &str = "SENTINEL-PIX-408";
const NAMESPACE: &str = "urn:oid:2.999.1";
const DOMAIN_A: &str = "urn:oid:2.999.10";
const DOMAIN_B: &str = "urn:oid:2.999.20";
const EHR_A: &str = "2222aaaa-2222-4222-8222-222222222222";
const OPERATION: &str = "/fhir/Patient/$ihe-pix";
const FHIR_JSON: &str = "application/fhir+json";

fn node(id: &str) -> NodeId {
    NodeId::new(id).expect("a node id")
}

fn patient(value: &str) -> PatientRef {
    PatientRef::new(
        IdentifierNamespace::new(NAMESPACE).expect("a namespace"),
        SecretString::from(value),
    )
    .expect("a patient reference")
}

fn members() -> Vec<NodeId> {
    vec![node("node-a"), node("node-b")]
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

/// The PIXm resolver over one stub Manager serving both members.
fn pixm(server: &Server) -> PixmResolver {
    PixmResolver::from_config(
        vec![ManagerConfig {
            base: SecretUrl::new(format!("{}/fhir/", server.uri())),
            auth: PixAuth::None,
            members: BTreeMap::from([
                (node("node-a"), DOMAIN_A.to_owned()),
                (node("node-b"), DOMAIN_B.to_owned()),
            ]),
        }],
        BTreeMap::new(),
        &registry(),
    )
    .expect("the resolver builds")
}

/// A stub Manager answering every call with `status` and `body`, after
/// `delay`.
async fn manager(status: u16, body: &str, delay: Duration) -> Server {
    let server = Server::start().await;
    Mock::given(method("GET"))
        .and(path(OPERATION))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_raw(body.as_bytes().to_vec(), FHIR_JSON)
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

/// A `Parameters` answer holding node A's `ehr_id` only.
fn at_a() -> String {
    format!(
        r#"{{"resourceType":"Parameters","parameter":[{{"name":"targetIdentifier","valueIdentifier":{{"system":"{DOMAIN_A}","value":"{EHR_A}"}}}}]}}"#
    )
}

async fn calls(server: &Server) -> usize {
    server
        .received_requests()
        .await
        .expect("recording is on")
        .len()
}

// conformance: CP-5
#[tokio::test]
async fn the_candidates_are_the_members_whose_domain_holds_the_patient() {
    let server = manager(200, &at_a(), Duration::ZERO).await;
    let pixm = pixm(&server);
    match pixm.localize(&patient(SENTINEL), &members(), soon()).await {
        Localization::Candidates(named) => {
            assert_eq!(BTreeSet::from([node("node-a")]), named);
        }
        other => panic!("node A holds the patient (§14.2): {other:?}"),
    }
}

// conformance: CP-5
#[tokio::test]
async fn the_resolution_of_the_same_query_reuses_the_localization_call() {
    let server = manager(200, &at_a(), Duration::ZERO).await;
    let pixm = pixm(&server);
    let _named = pixm.localize(&patient(SENTINEL), &members(), soon()).await;
    let resolutions = pixm
        .resolve(&patient(SENTINEL), &[node("node-a")], soon())
        .await;
    assert!(
        matches!(resolutions.get(&node("node-a")), Some(Resolution::Resolved(ehr)) if ehr.as_str() == EHR_A)
    );
    assert_eq!(1, calls(&server).await, "one ITI-83 call per query");

    let _again = pixm
        .resolve(&patient(SENTINEL), &[node("node-a")], soon())
        .await;
    assert_eq!(
        2,
        calls(&server).await,
        "a kept answer serves one resolution"
    );
}

#[tokio::test]
async fn another_patient_never_reads_a_kept_answer() {
    let server = manager(200, &at_a(), Duration::ZERO).await;
    let pixm = pixm(&server);
    let _named = pixm.localize(&patient(SENTINEL), &members(), soon()).await;
    let _other = pixm
        .resolve(&patient("SENTINEL-OTHER"), &[node("node-a")], soon())
        .await;
    assert_eq!(2, calls(&server).await, "the other patient is asked about");
    assert!(
        !format!("{pixm:?}").contains(SENTINEL),
        "no rendering shows a kept identifier"
    );
}

#[tokio::test]
async fn a_localization_past_the_capacity_keeps_nothing_and_its_resolution_asks_again() {
    let server = manager(200, &at_a(), Duration::ZERO).await;
    let pixm = pixm(&server);
    let kept = format!("shared: {SHARED_CAPACITY}");
    for index in 0..SHARED_CAPACITY {
        let named = pixm
            .localize(
                &patient(&format!("SENTINEL-CAP-{index}")),
                &members(),
                soon(),
            )
            .await;
        assert!(matches!(named, Localization::Candidates(_)), "{named:?}");
    }
    assert!(format!("{pixm:?}").contains(&kept), "{pixm:?}");

    let named = pixm
        .localize(&patient("SENTINEL-OVER"), &members(), soon())
        .await;
    assert!(
        matches!(&named, Localization::Candidates(set) if set.contains(&node("node-a"))),
        "the localization answers past the capacity: {named:?}"
    );
    assert!(
        format!("{pixm:?}").contains(&kept),
        "the capacity holds: {pixm:?}"
    );
    let asked = calls(&server).await;
    assert_eq!(SHARED_CAPACITY + 1, asked);

    let resolutions = pixm
        .resolve(&patient("SENTINEL-OVER"), &[node("node-a")], soon())
        .await;
    assert!(
        matches!(resolutions.get(&node("node-a")), Some(Resolution::Resolved(ehr)) if ehr.as_str() == EHR_A),
        "the resolution past the capacity resolves: {resolutions:?}"
    );
    assert_eq!(
        asked + 1,
        calls(&server).await,
        "it asks the Manager itself"
    );

    let _first = pixm
        .resolve(&patient("SENTINEL-CAP-0"), &[node("node-a")], soon())
        .await;
    assert_eq!(
        asked + 1,
        calls(&server).await,
        "a kept answer still serves"
    );
}

// conformance: CP-5
#[tokio::test]
async fn a_patient_the_manager_does_not_know_has_no_records() {
    let server = manager(
        404,
        r#"{"resourceType":"OperationOutcome","issue":[{"severity":"error","code":"not-found"}]}"#,
        Duration::ZERO,
    )
    .await;
    let answer = pixm(&server)
        .localize(&patient(SENTINEL), &members(), soon())
        .await;
    assert!(matches!(answer, Localization::NoRecords), "{answer:?}");
}

#[tokio::test]
async fn a_localization_no_resolution_follows_keeps_nothing() {
    let unknown = manager(
        404,
        r#"{"resourceType":"OperationOutcome","issue":[{"severity":"error","code":"not-found"}]}"#,
        Duration::ZERO,
    )
    .await;
    let pixm = pixm(&unknown);
    for index in 0..64 {
        let answer = pixm
            .localize(
                &patient(&format!("SENTINEL-UNKNOWN-{index}")),
                &members(),
                soon(),
            )
            .await;
        assert!(matches!(answer, Localization::NoRecords), "{answer:?}");
    }
    assert!(
        format!("{pixm:?}").contains("shared: 0"),
        "a burst of unknown patients leaves the memo empty: {pixm:?}"
    );

    let failing = manager(503, "{}", Duration::ZERO).await;
    let pixm = self::pixm(&failing);
    let answer = pixm.localize(&patient(SENTINEL), &members(), soon()).await;
    assert!(matches!(answer, Localization::Unavailable(_)), "{answer:?}");
    assert!(
        format!("{pixm:?}").contains("shared: 0"),
        "a failed localization keeps nothing: {pixm:?}"
    );
}

// conformance: CP-5
#[tokio::test]
async fn a_manager_that_fails_leaves_the_localization_unavailable() {
    let failing = manager(503, "{}", Duration::ZERO).await;
    match pixm(&failing)
        .localize(&patient(SENTINEL), &members(), soon())
        .await
    {
        Localization::Unavailable(error) => {
            assert_eq!(
                Some(503),
                error.status().map(|status| status.as_u16()),
                "the status the Manager answered with"
            );
        }
        other => panic!("§14.1: no candidate set from a failing Manager: {other:?}"),
    }

    let silent = manager(200, &at_a(), Duration::from_secs(10)).await;
    let answer = pixm(&silent)
        .localize(
            &patient(SENTINEL),
            &members(),
            Instant::now() + Duration::from_millis(200),
        )
        .await;
    assert!(
        matches!(
            answer,
            Localization::Unavailable(LocalizerError::DeadlineExceeded)
        ),
        "{answer:?}"
    );
}

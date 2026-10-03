// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The fan-out against mock nodes: concurrent dispatch under the per-node and
//! overall budgets, the decision under each completion strategy, the Tier order
//! and `LIMIT` over the node answers, and every answer validated against the
//! result-set schema (§11.1 to §11.6.1, N6, N37, N38, N39, N40). The helpers
//! here serve every module below.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

mod aggregate;
mod all_or_nothing;
mod best_effort;
mod budget;
mod consent;
mod decision;
mod dedup;
mod distinct;
mod localization;
mod order;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::time::Duration;

use ferrofed_engine::dispatch::{NodeClients, NodeQuery};
use ferrofed_engine::fanout::{Budget, FederatedAnswer, Plan, fan_out};
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_testkit::mock::Server;
use openehr_federation::status::EndpointStatus;
use openehr_its::rest::client::ReqwestTransport;
use openehr_its::rest::generated::query::ResultSetColumn;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

type TestResult = Result<(), Box<dyn Error>>;

/// The time a loaded host may add to any wait a test makes, in milliseconds.
///
/// A bound on elapsed time sits this far past what the code under test
/// should take, and a node meant to be abandoned stays silent at least this
/// far past the bound, so neither side of the claim depends on how busy the
/// machine is.
const SLACK_MS: u64 = 3_000;

/// A synthetic node query, scoped to an `ehr_id` under no real system.
const NODE_AQL: &str = "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c WHERE e/ehr_id/value = '7d44b88c-4199-4bad-97dc-d78268e01398'";

/// The façade's own query text and columns, as the rewrite renders them.
const FACADE_AQL: &str = "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c";

/// An ITS-REST `RESULT_SET` whose rows are the one-column `uids`.
fn result_set(uids: &[&str]) -> String {
    let rows: Vec<String> = uids.iter().map(|uid| format!("[\"{uid}\"]")).collect();
    format!(
        r##"{{"q":"{FACADE_AQL}","columns":[{{"name":"#0","path":"c/uid/value"}}],"rows":[{}]}}"##,
        rows.join(",")
    )
}

/// A JSON answer with `status` and `body`.
fn json(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

/// A mock node answering `POST /v1/query/aql` with `answer`.
async fn node(answer: ResponseTemplate) -> Server {
    let server = Server::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/query/aql"))
        .respond_with(answer)
        .mount(&server)
        .await;
    server
}

/// A federation of one node per `(endpoint_id, url)` pair, each with its own
/// `system_id`, all managed by one organisation.
fn federation(endpoints: &[(&str, &str)]) -> Result<RegistrySnapshot, Box<dyn Error>> {
    let mut document = String::from("[[organisation]]\nid = \"org-a\"\n");
    for (index, (id, url)) in endpoints.iter().enumerate() {
        write!(
            document,
            "\n[[node]]\nid = \"node-{index}\"\norganisation = \"org-a\"\nsystem_id = \"cdr-{index}.example.org\"\n\n[[endpoint]]\nid = \"{id}\"\nnode = \"node-{index}\"\nurl = \"{url}\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n"
        )?;
    }
    Ok(RegistrySnapshot::from_toml_str(&document)?)
}

/// The clients of every endpoint of `snapshot`.
fn clients(snapshot: &RegistrySnapshot) -> Result<NodeClients<ReqwestTransport>, Box<dyn Error>> {
    let transport = ReqwestTransport::with_timeout(Duration::from_secs(10))?;
    Ok(NodeClients::from_snapshot(
        snapshot,
        &transport,
        &BTreeMap::new(),
    )?)
}

/// A plan dispatching `NODE_AQL` to every listed endpoint.
fn plan_for(endpoints: &[&str]) -> Result<Plan, Box<dyn Error>> {
    let mut plan = Plan::new();
    for id in endpoints {
        plan = plan.dispatch(EndpointId::new(*id)?, NodeQuery::new(NODE_AQL))?;
    }
    Ok(plan)
}

/// A budget of `per_node_ms` per node and `overall_ms` overall.
fn budget(per_node_ms: u64, overall_ms: u64) -> Result<Budget, Box<dyn Error>> {
    Ok(Budget::new(
        Duration::from_millis(per_node_ms),
        Duration::from_millis(overall_ms),
    )?)
}

/// Runs `plan` against `snapshot` under `budget`.
async fn run(
    snapshot: &RegistrySnapshot,
    plan: Plan,
    budget: Budget,
) -> Result<FederatedAnswer, Box<dyn Error>> {
    Ok(fan_out(
        &clients(snapshot)?,
        snapshot,
        plan,
        budget,
        (&crate::conveyed::conveyance(), Some(OutboundId::mint())),
    )
    .await?)
}

/// The status each endpoint was reported with, by endpoint id.
fn statuses(answer: &FederatedAnswer) -> BTreeMap<String, EndpointStatus> {
    answer
        .federation()
        .endpoints()
        .iter()
        .map(|record| (record.id().as_str().to_owned(), record.status()))
        .collect()
}

/// The JSON text of the answer's rows.
fn rows_text(answer: &FederatedAnswer) -> Result<String, Box<dyn Error>> {
    Ok(serde_json::to_string(answer.rows())?)
}

/// The answer as the federated `RESULT_SET` body, validated against the
/// vendored result-set schema.
fn validated_body(answer: FederatedAnswer) -> Result<String, Box<dyn Error>> {
    let columns = vec![ResultSetColumn {
        name: "#0".to_owned(),
        path: Some("c/uid/value".to_owned()),
        additional_properties: BTreeMap::new(),
    }];
    let body = answer.into_result_set(Some(FACADE_AQL.to_owned()), Some(columns))?;
    let text = serde_json::to_string(&body)?;
    schema::validate(&text)?;
    Ok(text)
}

/// Schema validation of the answer bodies against the vendored specification.
mod schema {
    #![expect(
        clippy::disallowed_types,
        reason = "the test seam: schema validation reads JSON as values, in tests only"
    )]

    use std::error::Error;

    use serde_json::Value;

    /// The vendored result-envelope schema.
    const RESULT_SET_SCHEMA: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/specs/federation-spec/modules/ROOT/attachments/federated-result-set.schema.json"
    );

    /// Validates the JSON `text` against the result-set schema, formats
    /// included.
    pub(super) fn validate(text: &str) -> Result<(), Box<dyn Error>> {
        let schema: Value = serde_json::from_str(&std::fs::read_to_string(RESULT_SET_SCHEMA)?)?;
        let validator = jsonschema::options()
            .should_validate_formats(true)
            .build(&schema)?;
        let instance: Value = serde_json::from_str(text)?;
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|error| format!("{} at {}", error, error.instance_path()))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("federated-result-set.schema.json: {}", errors.join("; ")).into())
        }
    }
}

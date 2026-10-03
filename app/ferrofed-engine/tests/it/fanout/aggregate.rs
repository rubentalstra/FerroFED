// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! An aggregate recombined across mock nodes (§11.6.3, N14, N39): the one-row
//! node answers become the federation's row, a node whose value cannot take
//! part in an exactly correct answer is `node-error` and fails the query
//! `424` under all-or-nothing, a node that did not answer fails it `504` as
//! any query does, and a recombining plan is refused under best-effort.

use std::time::Duration;

use ferrofed_engine::dispatch::NodeQuery;
use ferrofed_engine::fanout::{Completion, FanOutError, Plan, Verdict, fan_out};
use ferrofed_registry::id::EndpointId;
use ferrofed_testkit::mock::Server;
use http::StatusCode;
use openehr_federation::aggregate::{Recombination, Recombine};
use openehr_federation::status::EndpointStatus;

use super::{
    SLACK_MS, TestResult, budget, clients, federation, json, node, rows_text, run, statuses,
};

/// The node query of `SELECT COUNT(*), AVG(x)`, as the rewrite dispatches it.
const NODE_AQL: &str = "SELECT COUNT(*), SUM(o/data[at0001]/value/magnitude), COUNT(o/data[at0001]/value/magnitude) FROM EHR e CONTAINS COMPOSITION c CONTAINS OBSERVATION o WHERE e/ehr_id/value = '7d44b88c-4199-4bad-97dc-d78268e01398'";

/// A node answering the one row `row`, a JSON array, after `delay`.
async fn aggregate_node(row: &str, delay: Duration) -> Server {
    let body = format!(
        r##"{{"q":"node","columns":[{{"name":"#0"}},{{"name":"#1"}},{{"name":"#2"}}],"rows":[{row}]}}"##
    );
    node(json(200, &body).set_delay(delay)).await
}

/// `COUNT(*)` from node column 0, and `AVG(x)` from its `SUM` and `COUNT` in
/// columns 1 and 2.
fn recombination() -> Recombination {
    Recombination::new(vec![
        Recombine::Count { column: 0 },
        Recombine::Avg { sum: 1, count: 2 },
    ])
}

fn plan(endpoints: &[&str]) -> Result<Plan, Box<dyn std::error::Error>> {
    let mut plan = Plan::new().recombining(recombination());
    for id in endpoints {
        plan = plan.dispatch(EndpointId::new(*id)?, NodeQuery::new(NODE_AQL))?;
    }
    Ok(plan)
}

// conformance: CP-10 CP-32
#[tokio::test]
async fn the_node_answers_recombine_into_one_row() -> TestResult {
    let a = aggregate_node("[3, 12, 2]", Duration::ZERO).await;
    let b = aggregate_node("[2, 0.5, 1]", Duration::ZERO).await;
    let snapshot = federation(&[("node-a-pub", &a.uri()), ("node-b-pub", &b.uri())])?;
    let answer = run(
        &snapshot,
        plan(&["node-a-pub", "node-b-pub"])?,
        budget(2_000, 5_000)?,
    )
    .await?;
    assert_eq!(answer.status(), StatusCode::OK);
    assert!(answer.federation().complete());
    assert_eq!(
        rows_text(&answer)?,
        "[[5,4.166666666666667]]",
        "§11.6.3: 3 + 2 rows, and (12 + 0.5) / (2 + 1), never one row per node"
    );
    Ok(())
}

// conformance: CP-32
#[tokio::test]
async fn an_avg_over_integers_answers_an_integer() -> TestResult {
    let a = aggregate_node("[3, 12, 2]", Duration::ZERO).await;
    let b = aggregate_node("[2, 7, 1]", Duration::ZERO).await;
    let snapshot = federation(&[("node-a-pub", &a.uri()), ("node-b-pub", &b.uri())])?;
    let answer = run(
        &snapshot,
        plan(&["node-a-pub", "node-b-pub"])?,
        budget(2_000, 5_000)?,
    )
    .await?;
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(
        rows_text(&answer)?,
        "[[5,6]]",
        "AQL 1.1.0 §3.9.1.5: (12 + 7) / (2 + 1) over Integer input, the nearest integer"
    );
    Ok(())
}

// conformance: CP-10 CP-32
#[tokio::test]
async fn a_value_the_recombination_cannot_use_fails_the_query_424() -> TestResult {
    let a = aggregate_node("[3, 12, 2]", Duration::ZERO).await;
    let b = aggregate_node(r#"[2, "twelve", 1]"#, Duration::ZERO).await;
    let snapshot = federation(&[("node-a-pub", &a.uri()), ("node-b-pub", &b.uri())])?;
    let answer = run(
        &snapshot,
        plan(&["node-a-pub", "node-b-pub"])?,
        budget(2_000, 5_000)?,
    )
    .await?;
    assert_eq!(answer.verdict(), Verdict::NodeFailed);
    assert_eq!(answer.status(), StatusCode::FAILED_DEPENDENCY, "N37");
    assert!(answer.rows().is_empty(), "no guessed value, no partial one");
    let statuses = statuses(&answer);
    assert_eq!(
        statuses.get("node-b-pub"),
        Some(&EndpointStatus::NodeError),
        "§11.1"
    );
    assert_eq!(statuses.get("node-a-pub"), Some(&EndpointStatus::Active));
    Ok(())
}

// conformance: CP-10 CP-32
#[tokio::test]
async fn a_node_that_does_not_answer_fails_the_aggregate_504() -> TestResult {
    let a = aggregate_node("[3, 12, 2]", Duration::ZERO).await;
    let slow = aggregate_node("[2, 0.5, 1]", Duration::from_millis(200 + SLACK_MS)).await;
    let snapshot = federation(&[("node-a-pub", &a.uri()), ("node-b-pub", &slow.uri())])?;
    let answer = run(
        &snapshot,
        plan(&["node-a-pub", "node-b-pub"])?,
        budget(200, 2_000)?,
    )
    .await?;
    assert_eq!(answer.status(), StatusCode::GATEWAY_TIMEOUT, "§11.4");
    assert!(answer.rows().is_empty());
    assert!(!answer.federation().complete());
    Ok(())
}

// conformance: CP-10 CP-32
#[tokio::test]
async fn a_recombining_plan_is_refused_best_effort_before_dispatch() -> TestResult {
    let a = aggregate_node("[3, 12, 2]", Duration::ZERO).await;
    let snapshot = federation(&[("node-a-pub", &a.uri())])?;
    let plan = plan(&["node-a-pub"])?.completing(Completion::BestEffort);
    let outcome = fan_out(
        &clients(&snapshot)?,
        &snapshot,
        plan,
        budget(2_000, 5_000)?,
        (&crate::conveyed::conveyance(), None),
    )
    .await;
    assert!(
        matches!(outcome, Err(FanOutError::PartialAggregate)),
        "§11.6.3, §11.4: {outcome:?}"
    );
    let asked = a.received_requests().await.ok_or("recording is on")?;
    assert!(asked.is_empty(), "nothing was dispatched");
    Ok(())
}

// conformance: CP-32
#[tokio::test]
async fn a_count_past_what_a_json_number_holds_is_an_error() -> TestResult {
    let a = aggregate_node(&format!("[{}, null, 0]", u64::MAX), Duration::ZERO).await;
    let b = aggregate_node("[1, null, 0]", Duration::ZERO).await;
    let snapshot = federation(&[("node-a-pub", &a.uri()), ("node-b-pub", &b.uri())])?;
    let outcome = fan_out(
        &clients(&snapshot)?,
        &snapshot,
        plan(&["node-a-pub", "node-b-pub"])?,
        budget(2_000, 5_000)?,
        (&crate::conveyed::conveyance(), None),
    )
    .await;
    assert!(
        matches!(outcome, Err(FanOutError::Unrepresentable(_))),
        "§11.6.3: exactly correct or not answered: {outcome:?}"
    );
    Ok(())
}

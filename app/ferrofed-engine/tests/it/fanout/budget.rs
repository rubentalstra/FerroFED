// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The budget a request runs under: time spent before the dispatch counted,
//! a client wait that shortens it, the effective budget reported, and no
//! cascade between nodes (§11.5, N38, N40).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ferrofed_engine::dispatch::Contact;
use ferrofed_engine::fanout::{Completion, FederatedAnswer, TIMEOUT_POLICY, fan_out_within};
use ferrofed_testkit::mock::Server;
use http::StatusCode;
use openehr_federation::status::EndpointStatus;

use super::{
    SLACK_MS, TestResult, budget, clients, federation, json, node, plan_for, result_set, rows_text,
    statuses, validated_body,
};

/// A node answering one row after `delay_ms`.
async fn slow_node(uid: &str, delay_ms: u64) -> Server {
    node(json(200, &result_set(&[uid])).set_delay(Duration::from_millis(delay_ms))).await
}

/// The `(per_node_ms, overall_ms)` the answer reports in
/// `meta.federation.timeout`.
fn reported(answer: &FederatedAnswer) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let timeout = answer
        .federation()
        .timeout()
        .ok_or("meta.federation.timeout is missing")?;
    assert_eq!(
        timeout.policy.as_deref(),
        Some(TIMEOUT_POLICY),
        "the budget is reported with the policy it applies under"
    );
    Ok((
        timeout.per_node_ms.ok_or("per_node_ms is missing")?,
        timeout.overall_ms.ok_or("overall_ms is missing")?,
    ))
}

// conformance: CP-31
#[tokio::test]
async fn time_spent_before_the_dispatch_comes_out_of_the_overall_budget() -> TestResult {
    let spent_ms = SLACK_MS;
    let overall_ms = spent_ms + 300;
    let slow = slow_node("s1::cdr-0.example.org::1", overall_ms + SLACK_MS).await;
    let snapshot = federation(&[("node-s-pub", &slow.uri())])?;
    let arrived = Instant::now()
        .checked_sub(Duration::from_millis(spent_ms))
        .ok_or("the clock cannot go back by the time spent")?;
    let called = Instant::now();
    let answer = fan_out_within(
        &clients(&snapshot)?,
        &snapshot,
        plan_for(&["node-s-pub"])?,
        budget(10_000, overall_ms)?,
        arrived,
        (&crate::conveyed::conveyance(), None),
    )
    .await?;
    let waited = called.elapsed();
    assert!(
        waited < Duration::from_millis(overall_ms),
        "the fan-out waited {waited:?}, though {spent_ms} ms of the {overall_ms} ms budget was \
         spent before it"
    );
    assert_eq!(
        statuses(&answer),
        BTreeMap::from([("node-s-pub".to_owned(), EndpointStatus::TimeOut)])
    );
    assert_eq!(reported(&answer)?, (overall_ms, overall_ms));
    validated_body(answer)?;
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn a_shortened_budget_abandons_a_node_the_configured_one_would_wait_for() -> TestResult {
    let wait_ms = 300;
    let late_ms = wait_ms + 2 * SLACK_MS;
    let late = slow_node("l1::cdr-0.example.org::1", late_ms).await;
    let snapshot = federation(&[("node-l-pub", &late.uri())])?;
    let configured = budget(late_ms + 1_000, late_ms + 2_000)?;
    let started = Instant::now();
    let answer = fan_out_within(
        &clients(&snapshot)?,
        &snapshot,
        plan_for(&["node-l-pub"])?,
        configured.shortened_to(Duration::from_millis(wait_ms)),
        started,
        (&crate::conveyed::conveyance(), None),
    )
    .await?;
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_millis(wait_ms + SLACK_MS),
        "the fan-out waited {waited:?}, past the client's {wait_ms} ms"
    );
    assert_eq!(answer.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        statuses(&answer),
        BTreeMap::from([("node-l-pub".to_owned(), EndpointStatus::TimeOut)])
    );
    assert_eq!(
        reported(&answer)?,
        (300, 300),
        "the effective budget is reported, never the configured one"
    );
    validated_body(answer)?;
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn a_longer_wait_leaves_the_configured_budget_in_force() -> TestResult {
    let overall_ms = 400;
    let late = slow_node("l1::cdr-0.example.org::1", overall_ms + 2 * SLACK_MS).await;
    let snapshot = federation(&[("node-l-pub", &late.uri())])?;
    let started = Instant::now();
    let answer = fan_out_within(
        &clients(&snapshot)?,
        &snapshot,
        plan_for(&["node-l-pub"])?,
        budget(300, overall_ms)?.shortened_to(Duration::from_secs(60)),
        started,
        (&crate::conveyed::conveyance(), None),
    )
    .await?;
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_millis(overall_ms + SLACK_MS),
        "the fan-out waited {waited:?}: a client wait extended the budget"
    );
    assert_eq!(
        statuses(&answer),
        BTreeMap::from([("node-l-pub".to_owned(), EndpointStatus::TimeOut)])
    );
    assert_eq!(reported(&answer)?, (300, 400));
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn a_zero_wait_asks_no_node_and_reports_every_node_time_out() -> TestResult {
    let a = node(json(200, &result_set(&["a1::cdr-0.example.org::1"]))).await;
    let b = node(json(200, &result_set(&["b1::cdr-1.example.org::1"]))).await;
    let snapshot = federation(&[("node-a-pub", &a.uri()), ("node-b-pub", &b.uri())])?;
    let started = Instant::now();
    let answer = fan_out_within(
        &clients(&snapshot)?,
        &snapshot,
        plan_for(&["node-a-pub", "node-b-pub"])?,
        budget(2_000, 5_000)?.shortened_to(Duration::ZERO),
        started,
        (&crate::conveyed::conveyance(), None),
    )
    .await?;
    assert_eq!(answer.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        statuses(&answer),
        BTreeMap::from([
            ("node-a-pub".to_owned(), EndpointStatus::TimeOut),
            ("node-b-pub".to_owned(), EndpointStatus::TimeOut),
        ]),
        "a node never asked is unknown, never empty"
    );
    assert_eq!(reported(&answer)?, (0, 0));
    for server in [&a, &b] {
        let asked = server
            .received_requests()
            .await
            .ok_or("request recording is off")?;
        assert!(asked.is_empty(), "a node was asked with no budget left");
    }
    validated_body(answer)?;
    Ok(())
}

// conformance: CP-31
#[tokio::test]
async fn abandoning_one_node_never_aborts_another_in_flight() -> TestResult {
    let failing = node(json(500, r#"{"message":"synthetic failure"}"#)).await;
    let per_node_ms = SLACK_MS;
    let steady = slow_node("m1::cdr-1.example.org::1", 250).await;
    let stuck = slow_node("s1::cdr-2.example.org::1", per_node_ms + 2 * SLACK_MS).await;
    let snapshot = federation(&[
        ("node-e-pub", &failing.uri()),
        ("node-m-pub", &steady.uri()),
        ("node-s-pub", &stuck.uri()),
    ])?;
    let started = Instant::now();
    let answer = fan_out_within(
        &clients(&snapshot)?,
        &snapshot,
        plan_for(&["node-e-pub", "node-m-pub", "node-s-pub"])?.completing(Completion::BestEffort),
        budget(per_node_ms, per_node_ms + 2 * SLACK_MS)?,
        started,
        (&crate::conveyed::conveyance(), None),
    )
    .await?;
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_millis(per_node_ms + SLACK_MS),
        "the fan-out waited {waited:?}, past the per-node timeout of the stuck node"
    );
    assert_eq!(answer.status(), StatusCode::OK);
    assert_eq!(
        statuses(&answer),
        BTreeMap::from([
            ("node-e-pub".to_owned(), EndpointStatus::NodeError),
            ("node-m-pub".to_owned(), EndpointStatus::Active),
            ("node-s-pub".to_owned(), EndpointStatus::TimeOut),
        ]),
        "an early failure and an abandoned node leave the in-flight node alone"
    );
    let rows = rows_text(&answer)?;
    assert!(rows.contains("m1::cdr-1.example.org::1"), "{rows}");
    validated_body(answer)?;
    Ok(())
}

#[tokio::test]
async fn a_zero_wait_leaves_every_node_without_contact() -> TestResult {
    let a = node(json(200, &result_set(&["a1::cdr-0.example.org::1"]))).await;
    let b = node(json(200, &result_set(&["b1::cdr-1.example.org::1"]))).await;
    let snapshot = federation(&[("node-a-pub", &a.uri()), ("node-b-pub", &b.uri())])?;
    let answer = fan_out_within(
        &clients(&snapshot)?,
        &snapshot,
        plan_for(&["node-a-pub", "node-b-pub"])?,
        budget(2_000, 5_000)?.shortened_to(Duration::ZERO),
        Instant::now(),
        (&crate::conveyed::conveyance(), None),
    )
    .await?;
    let contacts: BTreeMap<String, Contact> = answer
        .contacts()
        .map(|(endpoint, contact)| (endpoint.as_str().to_owned(), contact))
        .collect();
    assert_eq!(
        BTreeMap::from([
            ("node-a-pub".to_owned(), Contact::Unsent),
            ("node-b-pub".to_owned(), Contact::Unsent),
        ]),
        contacts,
        "§11.5: the budget ran out before either request left, so neither node was asked"
    );
    Ok(())
}

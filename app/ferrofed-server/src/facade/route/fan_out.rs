// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Fan-out template upload: one template upload sent to every member the
//! request names, each independently, and answered per node (§12.6, N43,
//! CP-34).
//!
//! It is offered only where the deployment sets
//! `federation.fan_out_template_upload`, which `OPTIONS {base}/` declares
//! (§7a.2, N30), and it applies to the two template upload operations of
//! ITS-REST alone. Such an upload fans out when its
//! `openEHR-federation-endpoint` header is `*`, every active member, or when
//! its targeting headers select more than one endpoint: a distinct request,
//! never the reading of a plain upload, which still names its one node or is
//! refused (§12.6 item 1). Every other definition request routes to one node
//! ([`named`]).
//!
//! Each member is sent the client's request through the same forwarding as a
//! routed request: the operation's declared values only, the body
//! byte-identical (N22, N33). Nothing is rolled back, so a member that
//! accepted keeps its template whatever the others answered (§12.6 item 3).
//! The answer carries `meta.federation` with one `endpoints[]` entry per
//! registry member, the status of each, and where it failed the node's HTTP
//! status with an excerpt of its message, as a federated query's endpoint
//! record carries them (§9.5, §11.1, §12.6 item 2). The
//! provenance headers name the members that accepted (§7a.3, N31).
//!
//! What each member showed of itself is recorded on `GET
//! /health/dependencies` as a routed request records it, and each request
//! that left the gateway in the node request metrics ([`observed`]).
//!
//! The per-member machinery, [`each`], [`settled`], [`contact`], [`observed`],
//! [`record_meta`] and [`answer_status`], serves the stored-query registry's
//! distribution and drift check too, which §12.7 holds to these same terms
//! (N44).

use std::collections::BTreeSet;
use std::time::Instant;

use axum::Json;
use axum::response::{IntoResponse, Response};
use ferrofed_engine::dispatch::reported;
use ferrofed_engine::dispatch::{Contact, DispatchOptions, NodeClient};
use ferrofed_engine::fanout::TIMEOUT_POLICY;
use ferrofed_engine::forward::{ForwardError, Forwarded, HeldRequest};
use ferrofed_engine::hygiene::Withheld;
use ferrofed_engine::onward::conveyance::Conveyance;
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::snapshot::{Endpoint, EndpointStatus, RegistrySnapshot};
use http::{HeaderMap, StatusCode};
use openehr_federation::error::WireError;
use openehr_federation::headers;
use openehr_federation::meta::{FederationMeta, TimeoutBudget};
use openehr_federation::outcome::{EndpointOutcome, ErrorDetail, Outcome};
use openehr_federation::status;
use openehr_its::rest::client::{ErrorBody, ReqwestTransport};
use openehr_its::rest::routes::RouteMatch;
use serde::Serialize;
use tokio::task::{JoinError, JoinSet};

use super::chosen::{Chooser, named, to_named};
use super::{Arrived, DEFINITION_GROUP, Deadlines, held, unheld};
use crate::error::{self, Code};
use crate::facade::provenance::Provenance;
use crate::facade::security;
use crate::facade::target::{self, Mechanism, Selected, TargetError};
use crate::federation::Federation;

/// The ITS-REST operations that upload a template, ADL 1.4 and ADL 2
/// (§12.6).
const UPLOADS: [&str; 2] = [
    "definition_template_adl1.4_upload",
    "definition_template_adl2_upload",
];

/// The endpoint header value that names every active member (§12.6 item 1).
const EVERY_MEMBER: &str = "*";

/// Answers a request in the definition area: a template upload that names
/// several members fans out where the deployment offers it, and every other
/// request routes to the one endpoint the targeting headers name (§7a.1,
/// §12.6, N43).
pub(super) async fn definition(
    federation: &Federation,
    arrived: Arrived<'_>,
    matched: &RouteMatch,
) -> Response {
    let chooser = Chooser::Client;
    if !(federation.fans_out_template_upload() && uploads_template(matched)) {
        return named(federation, arrived, DEFINITION_GROUP, chooser).await;
    }
    let started = Instant::now();
    let logged = arrived.outbound.to_string();
    let request = match held(&arrived) {
        Ok(request) => request,
        Err(failure) => return unheld(&failure, arrived.request_id, &logged),
    };
    let several =
        |selected: &BTreeSet<EndpointId>| every_member(arrived.headers) || selected.len() > 1;
    match members(federation.snapshot(), arrived.headers) {
        Ok(Some(selected)) if several(&selected) => {
            fan_out(
                federation,
                &arrived,
                (&request, &selected),
                (started, &logged),
            )
            .await
        }
        Ok(_) => {
            let held = (request, started);
            to_named(federation, arrived, held, DEFINITION_GROUP, chooser).await
        }
        Err(refused) => error::response(refused.code(), refused.to_string(), arrived.request_id),
    }
}

/// Whether `matched` uploads a template (§12.6).
fn uploads_template(matched: &RouteMatch) -> bool {
    matched.group == DEFINITION_GROUP && UPLOADS.contains(&matched.operation_id)
}

/// The members the targeting headers of a definition request name, or
/// `None` when they name none (§12.6, §8.4).
///
/// `*` alone in the endpoint header selects every active member; an
/// organisation header beside it must select the same set (§8.4.1). Any
/// other targeting selects the endpoints it names.
///
/// # Errors
///
/// Returns the [`TargetError`] of targeting the registry cannot answer, and
/// [`TargetError::Conflict`] when an organisation header beside `*` selects
/// another set (§8.4.1, N35).
pub(crate) fn members(
    snapshot: &RegistrySnapshot,
    headers: &HeaderMap,
) -> Result<Option<BTreeSet<EndpointId>>, TargetError> {
    if !every_member(headers) {
        return target::requested(snapshot, None, headers);
    }
    let every: BTreeSet<EndpointId> = snapshot
        .endpoints()
        .filter(|endpoint| endpoint.status() == EndpointStatus::Active)
        .map(|endpoint| endpoint.id().clone())
        .collect();
    let mut others = headers.clone();
    others.remove(headers::ENDPOINT);
    match target::requested(snapshot, None, &others)? {
        Some(organisations) if organisations != every => Err(TargetError::Conflict {
            first: Selected {
                by: Mechanism::EndpointHeader,
                endpoints: every,
            },
            second: Selected {
                by: Mechanism::OrganisationHeader,
                endpoints: organisations,
            },
        }),
        _ => Ok(Some(every)),
    }
}

/// Whether the endpoint header of `headers` is `*` and nothing else, its
/// empty list elements ignored as everywhere (RFC 9110 §5.6.1).
fn every_member(headers: &HeaderMap) -> bool {
    let mut named = Vec::new();
    for line in headers.get_all(headers::ENDPOINT) {
        let Ok(text) = line.to_str() else {
            return false;
        };
        named.extend(text.split(',').map(str::trim).filter(|id| !id.is_empty()));
    }
    named == [EVERY_MEMBER]
}

/// The endpoints of `snapshot` that `selected` names, in registry order, or
/// `None` when it names none or a suspended one, which resolves to no
/// destination (§11.2).
pub(crate) fn targets<'s>(
    snapshot: &'s RegistrySnapshot,
    selected: &BTreeSet<EndpointId>,
) -> Option<Vec<&'s Endpoint>> {
    let targets: Vec<&Endpoint> = snapshot
        .endpoints()
        .filter(|endpoint| selected.contains(endpoint.id()))
        .collect();
    // NOTE: §11.1 never contacts a suspended endpoint, so a fan-out naming one
    // resolves to no destination (§11.2), as a routed request does.
    let suspended = targets
        .iter()
        .any(|endpoint| endpoint.status() == EndpointStatus::Suspended);
    (!targets.is_empty() && !suspended).then_some(targets)
}

/// What became of the request sent to one member.
#[derive(Debug)]
pub(crate) enum Asked<R> {
    /// The call ended with `R`, an answer or a failure of its own.
    Ended(R),
    /// The gateway holds no client for the member, so nothing was sent.
    Unsent,
    /// The call was under way when the overall budget ran out (§11.5).
    Abandoned,
}

/// Sends one request to each of `targets` at once through `call`, within
/// `budget`, under the gateway's `outbound` id and conveying `conveyance`,
/// and returns what became of
/// each with the gateway's measurement of its request in milliseconds, in
/// the order of `targets`.
///
/// Each member is asked independently: one that fails or is late changes
/// nothing for the others, and nothing is undone (§12.6 item 3).
///
/// # Errors
///
/// Returns [`Unfinished`] when a task panicked, so the caller fails the
/// request as the probe and the query fan-out do, and attributes nothing to
/// any member.
pub(crate) async fn each<R, F, Fut>(
    federation: &Federation,
    targets: &[&Endpoint],
    (budget, outbound, conveyance): (&Deadlines, OutboundId, &Conveyance),
    logged: &str,
    call: F,
) -> Result<Vec<(Asked<R>, u64)>, Unfinished>
where
    F: Fn(NodeClient<ReqwestTransport>, DispatchOptions) -> Fut,
    Fut: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    let options =
        DispatchOptions::new(budget.per_node(), conveyance.clone()).with_request_id(outbound);
    let until = tokio::time::Instant::from_std(budget.overall());
    let mut tasks = JoinSet::new();
    let mut sent: Vec<Option<(Asked<R>, u64)>> = targets.iter().map(|_| None).collect();
    for (index, endpoint) in targets.iter().enumerate() {
        let Some(client) = federation.clients().get(endpoint.id()).cloned() else {
            tracing::error!(
                endpoint = %endpoint.id(),
                request_id = logged,
                "a registry endpoint has no node client"
            );
            if let Some(slot) = sent.get_mut(index) {
                *slot = Some((Asked::Unsent, 0));
            }
            continue;
        };
        let asked = call(client, options.clone());
        tasks.spawn(async move {
            let started = Instant::now();
            // NOTE: tokio::time::timeout_at (docs.rs) polls the call before the budget, so a
            // request the budget overtook before it left ends unsent, never abandoned.
            let asked = match tokio::time::timeout_at(until, asked).await {
                Ok(answer) => Asked::Ended(answer),
                Err(_elapsed) => Asked::Abandoned,
            };
            (index, asked, elapsed_ms(started))
        });
    }
    while let Some(joined) = tasks.join_next().await {
        let (index, asked, latency_ms) = joined.map_err(Unfinished::Task)?;
        if let Some(slot) = sent.get_mut(index) {
            *slot = Some((asked, latency_ms));
        }
    }
    sent.into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(Unfinished::Unanswered)
}

/// A fan-out that could not tell what became of every member's request: a
/// defect on the gateway's side, never a member's answer.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Unfinished {
    /// A task panicked or was cancelled.
    #[error("a fan-out task did not finish")]
    Task(#[source] JoinError),
    /// A member was left with no record of its request.
    #[error("a fan-out member was left with no record of its request")]
    Unanswered,
}

/// The §11.1 outcome of a member's request that ended as `sent` after
/// `latency_ms`: `done` reads an ended call, and a request never sent or
/// abandoned is reported as such.
pub(crate) fn settled<R>(
    asked: Asked<R>,
    latency_ms: u64,
    done: impl FnOnce(R, u64) -> Outcome,
) -> Outcome {
    match asked {
        Asked::Ended(ended) => done(ended, latency_ms),
        Asked::Unsent => not_sent(latency_ms),
        Asked::Abandoned => Outcome::TimeOut {
            latency_ms,
            error: ErrorDetail::Text("no answer before the overall budget ran out".to_owned()),
        },
    }
}

/// What the request to a member that ended as `asked` showed of it: `read`
/// reads an ended call, an abandoned one got no answer, and one the gateway
/// holds no client for never left.
pub(crate) fn contact<R>(asked: &Asked<R>, read: impl FnOnce(&R) -> Contact) -> Contact {
    match asked {
        Asked::Ended(ended) => read(ended),
        Asked::Unsent => Contact::Unsent,
        Asked::Abandoned => Contact::Silent,
    }
}

/// Records the request to `endpoint` that showed `contact` and ended as
/// `outcome` in the per-member record: the member's state on `GET
/// /health/dependencies`, and the request in the node request metrics,
/// neither when the request never left the gateway.
pub(crate) fn observed(
    federation: &Federation,
    endpoint: &EndpointId,
    contact: Contact,
    outcome: &Outcome,
) {
    federation.dependencies().contacted(endpoint, contact);
    federation.requests().settled(endpoint, outcome, contact);
}

/// Sends the client's upload, held as `request`, to each member of
/// `selected` independently, within the request's budget, and answers per
/// node (§12.6, §11.5).
async fn fan_out(
    federation: &Federation,
    arrived: &Arrived<'_>,
    (request, selected): (&HeldRequest, &BTreeSet<EndpointId>),
    (started, logged): (Instant, &str),
) -> Response {
    let request_id = arrived.request_id;
    let snapshot = federation.snapshot();
    let Some(targets) = targets(snapshot, selected) else {
        return error::fixed(Code::NoDestination, request_id);
    };
    let Some(budget) = Deadlines::from(federation, started) else {
        tracing::error!(
            request_id = logged,
            "the fan-out upload's deadline cannot be represented"
        );
        return error::fixed(Code::Internal, request_id);
    };
    let sent = each(
        federation,
        &targets,
        (&budget, arrived.outbound, &arrived.conveyance),
        logged,
        |client, options| {
            let request = request.clone();
            async move { client.forward_held(request, &options).await }
        },
    )
    .await;
    let sent = match sent {
        Ok(sent) => sent,
        Err(unfinished) => {
            tracing::error!(
                error = %crate::chain(&unfinished),
                request_id = logged,
                "the fan-out upload could not tell what became of every member"
            );
            return error::fixed(Code::Internal, request_id);
        }
    };
    let mut outcomes = Vec::with_capacity(targets.len());
    for (endpoint, (sent, latency_ms)) in targets.iter().zip(sent) {
        let reached = contact(&sent, Contact::of_forwarded);
        let outcome = settled(sent, latency_ms, |answer, latency_ms| {
            outcome(endpoint, answer, latency_ms, logged)
        });
        observed(federation, endpoint.id(), reached, &outcome);
        outcomes.push((endpoint.id(), outcome));
    }
    tracing::info!(
        members = outcomes.len(),
        accepted = outcomes
            .iter()
            .filter(|(_, outcome)| outcome.status() == status::EndpointStatus::Active)
            .count(),
        request_id = logged,
        "a template upload fanned out"
    );
    match answer(federation, &outcomes) {
        Ok(response) => response,
        Err(refused) => {
            tracing::error!(
                error = %crate::chain(&refused),
                request_id = logged,
                "the fan-out upload's per-node record could not be built"
            );
            error::fixed(Code::Internal, request_id)
        }
    }
}

/// The milliseconds since `started`, saturating at `u64::MAX`.
fn elapsed_ms(started: Instant) -> u64 {
    whole_ms(started.elapsed())
}

/// The §11.1 outcome of what `endpoint` answered the upload, measured as
/// `latency_ms`: a failure carries the node's HTTP status and an excerpt of
/// its message (§9.5, [`reported`]).
fn outcome(
    endpoint: &Endpoint,
    answer: Result<Forwarded, ForwardError>,
    latency_ms: u64,
    logged: &str,
) -> Outcome {
    let error = |message: String| ErrorDetail::Text(message);
    // NOTE: §12.6: a template is not patient data, and no resolution precedes
    // its upload, so the gateway withholds no identifier for the request.
    let withheld = Withheld::none();
    match answer {
        Ok(forwarded) if forwarded.status().is_success() => Outcome::Active { latency_ms },
        Ok(forwarded) => {
            let (status, _, body) = forwarded.into_parts();
            Outcome::NodeError {
                latency_ms,
                error: reported::answered(status, &ErrorBody::from_bytes(body), &withheld),
            }
        }
        Err(ForwardError::Refused {
            status: refused,
            body,
            ..
        }) => Outcome::NodeError {
            latency_ms,
            error: reported::said(
                format!("the node refused the gateway's onward credentials with {refused}"),
                &body,
                &withheld,
            ),
        },
        Err(ForwardError::TimeOut { .. }) => Outcome::TimeOut {
            latency_ms,
            error: error("no answer before the deadline".to_owned()),
        },
        Err(ForwardError::Expired { .. }) => Outcome::TimeOut {
            latency_ms,
            error: error(
                "no answer before the deadline, which passed before the request was sent"
                    .to_owned(),
            ),
        },
        Err(ForwardError::Credentials { error, .. }) => Outcome::NodeError { latency_ms, error },
        Err(ForwardError::Unreachable { .. }) => Outcome::Offline {
            latency_ms,
            error: error("the node could not be reached".to_owned()),
        },
        Err(failure) => {
            if let ForwardError::Withheld {
                endpoint: withheld,
                part,
            } = &failure
            {
                security::forward_withheld(withheld, *part, logged);
            }
            tracing::error!(
                endpoint = %endpoint.id(),
                error = %crate::chain(&failure),
                request_id = logged,
                "the fan-out upload could not be sent to a member"
            );
            not_sent(latency_ms)
        }
    }
}

/// The outcome of a member the gateway could not send its request to.
// NOTE: §11.1 has no status for a request the gateway itself could not send;
// the node was not reached, so it is `offline` (our own design).
pub(crate) fn not_sent(latency_ms: u64) -> Outcome {
    Outcome::Offline {
        latency_ms,
        error: ErrorDetail::Text("the gateway could not send the request to the node".to_owned()),
    }
}

/// The body of a fan-out upload's answer: `meta.federation` alone (§9.5,
/// §12.6 item 2).
#[derive(Debug, Serialize)]
struct Uploaded {
    meta: UploadedMeta,
}

/// The `meta` of [`Uploaded`].
#[derive(Debug, Serialize)]
pub(crate) struct UploadedMeta {
    /// The per-member record.
    pub(crate) federation: FederationMeta,
}

/// The answer to a fan-out upload whose members ended as `outcomes`, in
/// registry order, with the status of [`answer_status`].
///
/// # Errors
///
/// Returns the [`WireError`] of an endpoint id the wire type refuses.
fn answer(
    federation: &Federation,
    outcomes: &[(&EndpointId, Outcome)],
) -> Result<Response, WireError> {
    let meta = record_meta(federation, outcomes)?;
    let provenance = Provenance::active(&meta);
    let code = answer_status(
        &outcomes
            .iter()
            .map(|(_, outcome)| outcome)
            .collect::<Vec<_>>(),
    );
    let response = (
        code,
        Json(Uploaded {
            meta: UploadedMeta { federation: meta },
        }),
    )
        .into_response();
    Ok(provenance.stamp(response))
}

/// The `meta.federation` record of a fan-out whose members ended as
/// `outcomes`, in registry order, with the request's budget.
///
/// Every registry member appears: one the request did not name, or a
/// suspended one `*` left out, as `excluded` (§8.1, §11.1).
///
/// # Errors
///
/// Returns the [`WireError`] of an endpoint id the wire type refuses.
pub(crate) fn record_meta(
    federation: &Federation,
    outcomes: &[(&EndpointId, Outcome)],
) -> Result<FederationMeta, WireError> {
    let snapshot = federation.snapshot();
    let mut records = Vec::new();
    for endpoint in snapshot.endpoints() {
        let outcome = outcomes
            .iter()
            .find(|(id, _)| *id == endpoint.id())
            .map_or(Outcome::Excluded { error: None }, |(_, outcome)| {
                outcome.clone()
            });
        records.push(record(snapshot, endpoint, outcome)?);
    }
    let budget = federation.budget();
    Ok(FederationMeta::new(records)?.with_timeout(TimeoutBudget {
        per_node_ms: Some(whole_ms(budget.per_node())),
        overall_ms: Some(whole_ms(budget.overall())),
        policy: Some(TIMEOUT_POLICY.to_owned()),
        ..TimeoutBudget::default()
    }))
}

/// The milliseconds of `duration`, saturating at `u64::MAX`.
fn whole_ms(duration: std::time::Duration) -> u64 {
    // NOTE: §9.5 reports latency in whole milliseconds; a duration past
    // u64::MAX ms cannot occur inside any budget, so saturating loses nothing.
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The status of a fan-out whose in-scope members ended as `outcomes`:
/// `200` when every member accepted, `207` when some did and others failed,
/// and otherwise `504` or `424` as §11.2 maps the failures.
// NOTE: §12.6 item 3 allows `207`-style reporting for a partial success; `207`
// tells it from `200` on the status line, and an all-failed upload maps as §11.2 does.
pub(crate) fn answer_status(outcomes: &[&Outcome]) -> StatusCode {
    let accepted = outcomes
        .iter()
        .filter(|outcome| outcome.status() == status::EndpointStatus::Active)
        .count();
    if accepted == outcomes.len() {
        StatusCode::OK
    } else if accepted > 0 {
        StatusCode::MULTI_STATUS
    } else if outcomes.iter().any(|outcome| {
        matches!(
            outcome.status(),
            status::EndpointStatus::Offline | status::EndpointStatus::TimeOut
        )
    }) {
        StatusCode::GATEWAY_TIMEOUT
    } else {
        StatusCode::FAILED_DEPENDENCY
    }
}

/// The `endpoints[]` entry of `endpoint` of `snapshot`: its outcome, node,
/// managing organisation, and the node's `system_id`, product and version
/// where the registry holds them (§9.5, N20, N40).
fn record(
    snapshot: &RegistrySnapshot,
    endpoint: &Endpoint,
    outcome: Outcome,
) -> Result<EndpointOutcome, WireError> {
    let id = openehr_federation::id::EndpointId::new(endpoint.id().as_str())?;
    let mut record = EndpointOutcome::new(id, outcome)
        .with_node_id(endpoint.node().as_str())
        .with_organisation(endpoint.managing_organisation().as_str());
    if let Some(node) = snapshot.node(endpoint.node()) {
        record = record.with_system_id(node.system_id().as_str());
        if let Some(product) = node.product() {
            record = record.with_product(product);
        }
        if let Some(version) = node.version() {
            record = record.with_version(version);
        }
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, HeaderValue, Method, StatusCode};
    use openehr_federation::headers;
    use openehr_federation::outcome::{ErrorDetail, Outcome};
    use openehr_its::rest::routes::{Lookup, lookup};

    use super::{answer_status, every_member, uploads_template};

    fn upload(method: &Method, path: &str) -> bool {
        matches!(lookup(method, path), Lookup::Matched(matched) if uploads_template(&matched))
    }

    #[test]
    fn only_the_two_template_uploads_fan_out() {
        assert!(upload(&Method::POST, "/definition/template/adl1.4"));
        assert!(upload(&Method::POST, "/definition/template/adl2"));
        for (method, path) in [
            (Method::GET, "/definition/template/adl1.4"),
            (Method::GET, "/definition/template/adl2/t.v1"),
            (Method::GET, "/definition/template/adl1.4/t.v1/example"),
            (Method::PUT, "/definition/query/org::q/1.0.0"),
            (Method::POST, "/ehr"),
        ] {
            assert!(!upload(&method, path), "{method} {path}");
        }
    }

    fn endpoint_header(lines: &[&'static str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for line in lines {
            map.append(headers::ENDPOINT, HeaderValue::from_static(line));
        }
        map
    }

    #[test]
    fn a_star_alone_names_every_member() {
        assert!(every_member(&endpoint_header(&["*"])));
        assert!(every_member(&endpoint_header(&[" * ,"])));
        assert!(!every_member(&endpoint_header(&["*, node-a-pub"])));
        assert!(!every_member(&endpoint_header(&["*", "*"])));
        assert!(!every_member(&endpoint_header(&["node-a-pub"])));
        assert!(!every_member(&HeaderMap::new()));
    }

    // conformance: CP-34
    #[test]
    fn a_failed_member_is_never_reported_as_overall_success() {
        let active = Outcome::Active { latency_ms: 3 };
        let failed = Outcome::NodeError {
            latency_ms: 4,
            error: ErrorDetail::Text("the node answered 422 Unprocessable Entity".to_owned()),
        };
        let late = Outcome::TimeOut {
            latency_ms: 9,
            error: ErrorDetail::Text("no answer before the deadline".to_owned()),
        };
        assert_eq!(StatusCode::OK, answer_status(&[&active, &active]));
        assert_eq!(StatusCode::MULTI_STATUS, answer_status(&[&active, &failed]));
        assert_eq!(StatusCode::MULTI_STATUS, answer_status(&[&late, &active]));
        assert_eq!(
            StatusCode::FAILED_DEPENDENCY,
            answer_status(&[&failed, &failed])
        );
        assert_eq!(
            StatusCode::GATEWAY_TIMEOUT,
            answer_status(&[&failed, &late])
        );
    }
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The answer to a federated query: its modes read from the request, the
//! patient resolved and the fan-out run under one budget, and the
//! `RESULT_SET` built with its provenance, or the error answer naming why
//! there is none (§4, §9, §11).
//!
//! A server error is logged under the gateway's own id, never the client's
//! free-text request id, and no answer quotes the query, a parameter value
//! or a header value (§5.4.3).

use std::time::{Duration, Instant};

use axum::Json;
use axum::response::{IntoResponse, Response};
use ferrofed_engine::fanout::{Budget, Completion, FanOutError, FederatedAnswer, fan_out_within};
use ferrofed_engine::onward::conveyance::Conveyance;
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_identity::binding::SessionKey;
use http::{HeaderMap, HeaderValue, StatusCode};
use openehr_federation::aql::Analysis;
use openehr_federation::aql::refusal::Refusal;
use openehr_federation::dedup::DedupMode;
use openehr_its::rest::generated::query::ResultSet;
use openehr_its::rest::runtime::ApiError;

use crate::error::{self, Code};
use crate::facade::provenance::{Dispatch, Provenance};
use crate::facade::request::{Arrived, Submitted, read};
use crate::facade::{
    cells, completeness, dedup, follow_up, intake, owner, plan, prefer, scoped, security, target,
};
use crate::federation::Federation;

/// Runs the federated query `submitted` and answers it (§7, §9, §11).
pub(crate) async fn answer(
    federation: &Federation,
    arrived: Arrived<'_>,
    submitted: Submitted<'_>,
) -> Response {
    let Arrived {
        headers,
        request_id,
        outbound,
        conveyance,
        started,
    } = arrived;
    // TODO(#412): the authenticated client session the resolution bindings belong to.
    let session: Option<SessionKey> = None;
    let completion = match completeness::of(headers, federation.best_effort()) {
        Ok(completion) => completion,
        Err(error) => return Failure::Completeness(error).respond(request_id, outbound),
    };
    let dedup = match dedup::of(headers) {
        Ok(mode) => mode,
        Err(error) => return Failure::Dedup(error).respond(request_id, outbound),
    };
    let configured = federation.budget();
    let wait = prefer::wait(headers);
    let budget = wait.map_or(configured, |wait| configured.shortened_to(wait));
    let name = match &submitted {
        Submitted::Body(_) | Submitted::Query { .. } => None,
        Submitted::Stored { name, .. } => Some((*name).to_owned()),
    };
    let query = Query {
        sent: submitted,
        headers,
        completion,
        dedup,
        budget,
        started,
        outbound,
        conveyance,
        session: session.as_ref(),
    };
    match federate(federation, query).await {
        Ok((status, mut result_set, provenance)) => {
            // NOTE: §12.7, N44: the answer to a stored query names the gateway's definition.
            result_set.name = name;
            let mut response = (status, Json(result_set)).into_response();
            if let Some(applied) = wait.filter(|_| budget != configured) {
                applied_wait(&mut response, applied);
            }
            provenance.stamp(response)
        }
        Err(failure) => failure.respond(request_id, outbound),
    }
}

/// Names the `wait` that set the budget in `Preference-Applied` (RFC 7240
/// §3).
#[expect(
    clippy::expect_used,
    reason = "`wait=` followed by decimal digits is always a valid header value"
)]
fn applied_wait(response: &mut Response, wait: Duration) {
    let value = HeaderValue::try_from(format!("wait={}", wait.as_secs()))
        .expect("`wait=` and digits should be a valid header value");
    response
        .headers_mut()
        .insert(prefer::PREFERENCE_APPLIED, value);
}

/// Why a federated query has no `RESULT_SET` to answer with.
#[derive(Debug, thiserror::Error)]
pub(super) enum Failure {
    /// The request body is not an ITS-REST `AdhocQueryExecute`.
    #[error("the request body is not an ITS-REST ad hoc query")]
    Body,
    /// The query string is not an ITS-REST ad hoc query.
    // NOTE: §5.4.3, the decoder's message may name a query parameter the client
    // chose, so the answer carries a fixed message.
    #[error("the query string is not an ITS-REST ad hoc query")]
    Query(#[source] ApiError),
    /// The completeness header is refused.
    #[error(transparent)]
    Completeness(#[from] completeness::CompletenessError),
    /// The dedup header is refused.
    #[error(transparent)]
    Dedup(#[from] dedup::DedupError),
    /// A query parameter is not an AQL literal.
    #[error(transparent)]
    Parameter(#[from] intake::IntakeError),
    /// The query is refused before anything is dispatched.
    #[error(transparent)]
    Refused(#[from] Refusal),
    /// The directive or a targeting header names what the registry does not
    /// know, or two of them select different node sets (§8.4.1).
    #[error(transparent)]
    Target(#[from] target::TargetError),
    /// The fan-out could not be planned.
    #[error("the federated query could not be planned")]
    Plan(#[source] plan::TargetsError),
    /// The query is scoped to one `ehr_id`, and the order of §12.5.1 names
    /// no one member that owns it.
    #[error(transparent)]
    Routed(#[from] scoped::Unrouted),
    /// Node selection left no registry member in scope, so the request
    /// resolves to no destination (§11.2, §11.3).
    #[error(
        "no registry member is in scope for this request, so it cannot be resolved to any destination (§11.2, §11.3)"
    )]
    NoDestination,
    /// The fan-out failed on the gateway's side.
    #[error("the federated query could not be dispatched")]
    FanOut(#[source] FanOutError),
    /// A node's rows do not match the query it was sent.
    #[error("a node answered rows that do not match the dispatched query")]
    Cells(#[source] cells::CellError),
    /// The envelope could not be encoded.
    #[error("the federated answer could not be encoded")]
    Envelope(#[source] openehr_federation::error::WireError),
}

impl Failure {
    /// The code of this failure, which names its status (§11.2).
    fn code(&self) -> Code {
        match self {
            Self::Body | Self::Query(_) => Code::BodyInvalid,
            Self::Completeness(completeness::CompletenessError::NotOffered) => {
                Code::PartialUnsupported
            }
            Self::Completeness(
                completeness::CompletenessError::Repeated | completeness::CompletenessError::Value,
            ) => Code::CompletenessInvalid,
            Self::Dedup(_) => Code::DedupInvalid,
            Self::Parameter(_) => Code::ParameterInvalid,
            Self::Refused(refusal) => Code::Refused(refusal.into()),
            Self::Target(error) => error.code(),
            Self::Plan(plan::TargetsError::Patient(_)) => Code::PatientInvalid,
            Self::Routed(unrouted) => unrouted.code(),
            Self::NoDestination => Code::NoDestination,
            // NOTE: §11.1, a node row the gateway cannot use is a node-error
            // at dispatch, so one reaching the cells is the gateway's fault.
            Self::Plan(_) | Self::FanOut(_) | Self::Cells(_) | Self::Envelope(_) => Code::Internal,
        }
    }

    /// The error answer of this failure, naming the exchange id `request_id`.
    ///
    /// The message is the failure's display text, which locates a fault and
    /// never quotes the query, a parameter value or a header value (§5.4.3).
    /// A server error is logged under `outbound`, the id the request line
    /// records, and never under the client's free-text `request_id`.
    fn respond(self, request_id: &str, outbound: OutboundId) -> Response {
        let code = self.code();
        if code.status().is_server_error() {
            tracing::error!(
                code = code.as_str(),
                error = %crate::chain(&self),
                request_id = %outbound,
                "the federated query failed"
            );
        }
        error::response(code, self.to_string(), request_id)
    }
}

/// One federated query, as the façade read it from the request.
#[derive(Debug)]
struct Query<'a> {
    /// What the request sent.
    sent: Submitted<'a>,
    /// The request headers, which may name the node set (§8.4).
    headers: &'a HeaderMap,
    /// The completion strategy the request selects (§11.4).
    completion: Completion,
    /// The dedup mode the request selects (§10).
    dedup: DedupMode,
    /// The effective budget: the configured one, shortened by the client's
    /// `Prefer: wait` (§11.5).
    budget: Budget,
    /// When the request arrived, the instant the overall budget runs from.
    started: Instant,
    /// The gateway's id of the request: the one every node receives and the
    /// security events record, never the client's.
    outbound: OutboundId,
    /// Whom the request is on behalf of, conveyed to every node (§13.1, N24).
    conveyance: &'a Conveyance,
    /// The client session the resolution bindings belong to.
    session: Option<&'a SessionKey>,
}

/// Drops the `session`'s bindings that name a member the consent pre-filter
/// denied (N27a), holds the `{node, ehr_id}` set a resolution produced as the
/// `session`'s resolution bindings (§12.5.1 step 2), teaches the `ehr_id` index where
/// each `ehr_id` is held (step 3), and records the state the resolution
/// showed of the resolver.
fn remember(federation: &Federation, session: Option<&SessionKey>, targets: &plan::Targets) {
    let resolved = &targets.resolved;
    if let Some(session) = session {
        // NOTE: N27a; a consent denial drops every `ehr_id` the session cached for a denied
        // member before the new bindings are held (no specification governs this: our own design).
        federation
            .bindings()
            .forget_denied(session, &targets.denied);
        federation.bindings().record(
            session,
            Instant::now(),
            resolved.iter().map(|(node, ehr_id)| (node, ehr_id)),
        );
    }
    for (node, ehr_id) in resolved {
        owner::learn(federation.index(), ehr_id, node);
    }
    if let Some(observed) = targets.resolver {
        federation.dependencies().resolver(observed);
    }
}

/// Records what a fan-out showed of each member it dispatched to: its last
/// state for the health surface, read from the node's own answer, and its
/// request for the metrics surface, read from its §11.1 record.
fn observed(federation: &Federation, answer: &FederatedAnswer) {
    let records = answer.federation().endpoints();
    for (endpoint, contact) in answer.contacts() {
        federation.dependencies().contacted(endpoint, contact);
        let record = records
            .iter()
            .find(|record| record.id().as_str() == endpoint.as_str());
        if let Some(record) = record {
            federation
                .requests()
                .settled(endpoint, record.outcome(), contact);
        }
    }
}

/// Runs one federated query and returns the status, the `RESULT_SET`, and
/// the endpoints the answer names as having acted for it (§7a.3, N31).
///
/// Resolution and the fan-out share one overall budget, which runs from the
/// request's arrival, so the gateway answers within its declared budget
/// (§11.5). The `{node, ehr_id}` set a resolution produces is held as the
/// session's resolution bindings (§12.5.1 step 2); without a
/// session there is nothing to scope them to, and none is held.
async fn federate(
    federation: &Federation,
    query: Query<'_>,
) -> Result<(StatusCode, ResultSet, Provenance), Failure> {
    let Query {
        sent,
        headers,
        completion,
        dedup,
        budget,
        started,
        outbound,
        conveyance,
        session,
    } = query;
    let logged = outbound.to_string();
    let request_id = logged.as_str();
    let (request, named, analysis) =
        read(federation, (sent, headers), (completion, dedup), request_id)?;
    let deadline = started
        .checked_add(budget.overall())
        .ok_or(Failure::FanOut(FanOutError::Clock))?;
    let scope = scoped::Scoped {
        headers,
        session,
        budget,
        started,
        outbound,
        conveyance,
    };
    let routed = scoped::routed(federation, &analysis, named.as_ref(), scope).await?;
    let selection = plan::Selection::of(named.as_ref(), routed.map(|owner| owner.endpoint.id()));
    let (targets, subject) = match &analysis {
        Analysis::Patient(query) => (
            plan::patient(federation, selection, query, deadline)
                .await
                .map_err(Failure::Plan)?,
            Some(query.subject()),
        ),
        Analysis::Unscoped(query) => (
            plan::unscoped(federation.snapshot(), selection, query).map_err(Failure::Plan)?,
            None,
        ),
    };
    if targets.plan.has_no_destination() {
        return Err(Failure::NoDestination);
    }
    remember(federation, session, &targets);
    let attributes = analysis.attributes();
    let mut plan = targets
        .plan
        .completing(completion)
        .ordered(analysis.order().clone())
        .deduplicating(dedup)
        .annotating(attributes.clone());
    if let Some(recombination) = analysis.recombination() {
        plan = plan.recombining(recombination.clone());
    }
    let dispatch = Dispatch::of(routed, &plan);
    let answer = fan_out_within(
        federation.clients(),
        federation.snapshot(),
        plan,
        budget,
        started,
        (conveyance, Some(outbound)),
    )
    .await
    .map_err(|error| {
        security::fan_out(&error, request_id);
        Failure::FanOut(error)
    })?;
    observed(federation, &answer);
    follow_up::observe(federation, answer.seen(), request_id);
    let mut status = answer.status();
    // NOTE: no specification governs this (§11.3 covers only an answered lookup):
    // our own design, a cross-reference that could not answer fails the query
    // 424 under all-or-nothing; under best-effort it stays reported.
    if targets.resolution_failed
        && completion == Completion::AllOrNothing
        && status == StatusCode::OK
    {
        status = StatusCode::FAILED_DEPENDENCY;
    }
    let acting = dispatch.provenance(federation.snapshot(), answer.federation(), status);
    let provenance = answer.attributes().to_vec();
    let mut result_set = answer
        .into_result_set(Some(request.q.clone()), Some(analysis.columns().to_vec()))
        .map_err(Failure::Envelope)?;
    let rows = std::mem::take(&mut result_set.rows);
    result_set.rows = if status == StatusCode::OK {
        let added = cells::Added {
            subject,
            attributes: &attributes,
            values: &provenance,
        };
        cells::reinject(rows, &targets.sources, &added).map_err(Failure::Cells)?
    } else {
        Vec::new()
    };
    Ok((status, result_set, acting))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{Failure, cells, completeness, dedup, plan, target};
    use crate::error::Code;
    use ferrofed_engine::fanout::FanOutError;
    use ferrofed_identity::patient::PatientRefError;
    use http::StatusCode;
    use openehr_federation::aql::refusal::Refusal;
    use openehr_its::rest::runtime::ApiError;

    #[test]
    fn each_failure_answers_its_code_and_status() {
        let table = [
            (Failure::Body, "body-invalid", StatusCode::BAD_REQUEST),
            (
                Failure::Query(ApiError::BadRequest(
                    "the required query parameter `q` is missing".to_owned(),
                )),
                "body-invalid",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Completeness(completeness::CompletenessError::Value),
                "completeness-invalid",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Completeness(completeness::CompletenessError::Repeated),
                "completeness-invalid",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Completeness(completeness::CompletenessError::NotOffered),
                "partial-unsupported",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Dedup(dedup::DedupError::Value),
                "dedup-invalid",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Dedup(dedup::DedupError::Repeated),
                "dedup-invalid",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Refused(Refusal::OffsetUnsupported),
                "offset-unsupported",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Target(target::TargetError::UnknownEndpoint {
                    by: target::Mechanism::EndpointHeader,
                    position: 1,
                    at: None,
                }),
                "endpoint-unknown",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Target(target::TargetError::UnknownOrganisation {
                    by: target::Mechanism::OrganisationDirective,
                    position: 1,
                    at: None,
                }),
                "organisation-unknown",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Target(target::TargetError::Empty(
                    target::Mechanism::OrganisationHeader,
                )),
                "organisation-unknown",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Target(target::TargetError::Conflict {
                    first: target::Selected {
                        by: target::Mechanism::EndpointDirective,
                        endpoints: BTreeSet::new(),
                    },
                    second: target::Selected {
                        by: target::Mechanism::OrganisationHeader,
                        endpoints: BTreeSet::new(),
                    },
                }),
                "targeting-conflict",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::Plan(plan::TargetsError::Patient(PatientRefError::EmptyNamespace)),
                "patient-invalid",
                StatusCode::BAD_REQUEST,
            ),
            (
                Failure::FanOut(FanOutError::Clock),
                "internal",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                Failure::Cells(cells::CellError::NoSubject),
                "internal",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (failure, code, status) in table {
            let answered: Code = failure.code();
            assert_eq!(code, answered.as_str(), "{failure:?}");
            assert_eq!(status, answered.status(), "{failure:?}");
        }
    }
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A federated query the client scoped to one `ehr_id`, routed to the one
//! member that owns it (N29, §12.5.1, N41).
//!
//! `WHERE e/ehr_id/value = '…'` and `FROM EHR e[ehr_id/value='…']` address
//! one EHR as `{base}/v1/ehr/{ehr_id}` does, and N29 makes the forms
//! semantically equivalent. An `ehr_id` is meaningful only at the member
//! that issued it (§12.5), so an undirected query in either form is sent to
//! that member alone, found in the order a path `ehr_id` is (§12.5.1): a
//! binding the client session holds, the `ehr_id` index, then the ask-all
//! probe, a query being a read. The targeting headers and the `FROM ENDPOINT`
//! directive name the node set themselves (§8) and are never overridden.
//! Every other member is reported `excluded` (§11.1). Two members claiming
//! the `ehr_id` are a `409` and an integrity incident, never a merged answer
//! (§12.5.2, N42), and an `ehr_id` no member holds can be routed to no
//! destination (§11.2).

use std::collections::BTreeSet;
use std::time::Instant;

use ferrofed_engine::fanout::Budget;
use ferrofed_engine::onward::conveyance::Conveyance;
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_engine::probe::{Probe, ProbedEhrId};
use ferrofed_identity::binding::SessionKey;
use ferrofed_registry::id::{EhrId, EndpointId};
use ferrofed_registry::snapshot::Endpoint;
use http::HeaderMap;
use openehr_federation::aql::Analysis;

use crate::error::Code;
use crate::facade::route::ehr;
use crate::facade::{owner, security};
use crate::federation::Federation;

/// The member an `ehr_id`-scoped query is sent to, and the step of §12.5.1
/// that named it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Owner<'a> {
    /// The endpoint the owner is asked through.
    pub(crate) endpoint: &'a Endpoint,
    /// The step that named it.
    pub(crate) step: owner::Step,
}

/// Why an `ehr_id`-scoped query names no member to send it to.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Unrouted {
    /// The `ehr_id` is no openEHR `HIER_OBJECT_ID` (§12.5).
    #[error("the ehr_id the query is scoped to is not an openEHR HIER_OBJECT_ID (§12.5, N29)")]
    Invalid,
    /// The `ehr_id` no earlier step routes is no bare UUID, so it is never
    /// probed at every member (§5.4.1, N33).
    #[error(
        "the ehr_id the query is scoped to is routed by no binding or index entry, and is probed at every member only when it is a UUID (§5.4.1, N33, §12.5.1)"
    )]
    NotProbed,
    /// The targeting headers name no one endpoint (§8.4.1).
    #[error(transparent)]
    Untargeted(#[from] owner::Untargeted),
    /// A binding or the index named several members: a collision (§12.5.2,
    /// N42).
    #[error(transparent)]
    Claimed(owner::Unsettled),
    /// The owner the binding or the index names has no endpoint in the
    /// registry (§11.2).
    #[error(
        "the member that holds the ehr_id has no endpoint, so the query has no destination (§11.2)"
    )]
    NoEndpoint,
    /// The ask-all probe named no owner.
    #[error("{message}")]
    Probe {
        /// The code the probe's failure answers with.
        code: Code,
        /// What the probe found, naming endpoints and never a value.
        message: String,
    },
}

impl Unrouted {
    /// The stable code the error body names.
    pub(crate) fn code(&self) -> Code {
        match self {
            Self::Invalid => Code::EhrIdInvalid,
            Self::NotProbed => Code::ProbeRequiresUuid,
            Self::Untargeted(untargeted) => untargeted.code(),
            Self::Claimed(unsettled) => unsettled.code(),
            Self::NoEndpoint => Code::NoDestination,
            Self::Probe { code, .. } => *code,
        }
    }
}

/// The request an `ehr_id`-scoped query arrived on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scoped<'a> {
    /// The request headers; the probe carries the ones its operation
    /// declares.
    pub(crate) headers: &'a HeaderMap,
    /// The client session the resolution bindings belong to.
    pub(crate) session: Option<&'a SessionKey>,
    /// The effective budget, which the probe and the fan-out share (§11.5).
    pub(crate) budget: Budget,
    /// When the request arrived, the instant the budget runs from.
    pub(crate) started: Instant,
    /// The gateway's id for the request, the one every member receives.
    pub(crate) outbound: OutboundId,
    /// Whom the request is on behalf of, conveyed to every member (§13.1, N24).
    pub(crate) conveyance: &'a Conveyance,
}

/// The owner an undirected query scoped to one `ehr_id` is routed to, or
/// `None` for a query the `ehr_id` order does not route: one that names a
/// patient, names its endpoints (`named`), or is not scoped to one `ehr_id`.
///
/// # Errors
///
/// Returns an [`Unrouted`] when the query is scoped to one `ehr_id` and no
/// step of §12.5.1 names exactly one member.
pub(crate) async fn routed<'a>(
    federation: &'a Federation,
    analysis: &Analysis,
    named: Option<&BTreeSet<EndpointId>>,
    scoped: Scoped<'_>,
) -> Result<Option<Owner<'a>>, Unrouted> {
    let ehr_id = match (analysis, named) {
        (Analysis::Unscoped(query), None) => query.ehr_scope(),
        (Analysis::Unscoped(_) | Analysis::Patient(_), _) => None,
    };
    let Some(ehr_id) = ehr_id else {
        return Ok(None);
    };
    let found = owner(federation, scoped, ehr_id).await?;
    tracing::debug!(
        endpoint = %found.endpoint.id(),
        step = found.step.as_str(),
        request_id = %scoped.outbound,
        "routed an ehr_id-scoped query"
    );
    Ok(Some(found))
}

/// Finds the member that owns `ehr_id`, the literal the query is scoped to,
/// in the order of §12.5.1 (N41).
///
/// A probe that finds the owner teaches the `ehr_id` index.
///
/// # Errors
///
/// Returns an [`Unrouted`] when no step names exactly one member.
async fn owner<'a>(
    federation: &'a Federation,
    scoped: Scoped<'_>,
    ehr_id: &str,
) -> Result<Owner<'a>, Unrouted> {
    let logged = scoped.outbound.to_string();
    // NOTE: §5.4.3, the parse error quotes the value, which may be a patient
    // identifier, so the refusal names the field and never the error.
    let ehr_id = EhrId::new(ehr_id).map_err(|_quoted| Unrouted::Invalid)?;
    let snapshot = federation.snapshot();
    let held = scoped.session.map(|session| owner::Held {
        bindings: federation.bindings(),
        session,
        now: Instant::now(),
    });
    match owner::located(snapshot, scoped.headers, held, federation.index(), &ehr_id)? {
        owner::Located::At { endpoint, step } => Ok(Owner { endpoint, step }),
        owner::Located::Unreachable { .. } => Err(Unrouted::NoEndpoint),
        owner::Located::Collision(claimed) => {
            owner::collided(&ehr_id, claimed.detection, &claimed.claimants);
            Err(Unrouted::Claimed(owner::Unsettled::Claimed(
                claimed.claimants,
            )))
        }
        owner::Located::Unknown => {
            let Ok(asked) = ProbedEhrId::try_from(&ehr_id) else {
                security::probe_refused(&logged);
                return Err(Unrouted::NotProbed);
            };
            let overall = scoped
                .started
                .checked_add(scoped.budget.overall())
                .ok_or_else(|| Unrouted::Probe {
                    code: Code::Internal,
                    message: Code::Internal.message().to_owned(),
                })?;
            let per_node = Instant::now()
                .checked_add(scoped.budget.per_node())
                .map_or(overall, |at| at.min(overall));
            let probe = Probe {
                ehr_id: asked,
                headers: scoped.headers.clone(),
                per_node,
                overall,
                request_id: scoped.outbound,
                conveyance: scoped.conveyance.clone(),
            };
            let (endpoint, _answer) = ehr::ask_all(federation, &probe, &logged)
                .await
                .map_err(|(code, message)| Unrouted::Probe { code, message })?;
            owner::learn(federation.index(), &ehr_id, endpoint.node());
            Ok(Owner {
                endpoint,
                step: owner::Step::AskAll,
            })
        }
    }
}

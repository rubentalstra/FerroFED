// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! EHR-area routing: a request under a path `ehr_id`, routed to the one
//! member that owns it in the order of §12.5.1, the ask-all probe included
//! (§7a.1, §12.5, N41).
//!
//! The owner is found from the targeting headers, a resolution binding of
//! the client session, the `ehr_id` index, and for a read only, the ask-all
//! probe of every member, all within the request's budget (§11.5). A write
//! none of the first three routes is a `400`, and is never probed (N41); so
//! is a read whose `ehr_id` is no bare UUID, because the probe would carry it
//! to every member (§5.4.1, N33). An `ehr_id` a binding, the index or the
//! probe finds at several members is a `409` listing the claimants, and
//! raises the integrity incident of N42; no claimant is sent the request
//! (§12.5.2). A versioned write is sent only when the node controls the
//! version it amends ([`write::controlled`]; §12.4, N23).

use std::time::Instant;

use axum::response::Response;
use ferrofed_engine::forward::{ForwardError, Forwarded};
use ferrofed_engine::probe::{self, Answer, Probe, ProbedEhrId};
use ferrofed_identity::binding::SessionKey;
use ferrofed_registry::id::EhrId;
use ferrofed_registry::incident::Detection;
use ferrofed_registry::snapshot::{Endpoint, EndpointStatus};
use http::{HeaderMap, Method};
use openehr_its::rest::routes::RouteMatch;

use super::{Arrived, Deadlines, Failure, answered, failed, forward, held, learn, unheld};
use crate::error::{self, Code};
use crate::facade::provenance::Provenance;
use crate::facade::write::{self, Write};
use crate::facade::{follow_up, owner, security};
use crate::federation::Federation;

/// The path parameter that names the EHR (§12.5).
const EHR_ID_PARAM: &str = "ehr_id";

/// Routes one request in the EHR area to the owner of its path `ehr_id` and
/// answers as the owner did.
///
/// The path `ehr_id` is parsed first, and a malformed one is a `400` before
/// any routing (§12.5); so is a query parameter the operation does not
/// declare, or a declared header or query value that does not match its
/// declared kind, before anything is sent (§5.4.1, N33). The owner is then found in
/// the order of §12.5.1 (N41): the targeting headers, a binding the client
/// session holds, the `ehr_id` index, and for a read only, the ask-all probe.
/// A write none of the first three routes is a `400` (`target-required`),
/// and so is a read whose `ehr_id` is no bare UUID (`probe-requires-uuid`):
/// nothing is probed. A new EHR is routed by the targeting headers alone,
/// and refused when another member holds its `ehr_id`; a versioned write is
/// sent only when the node controls the version it amends
/// ([`write::controlled`]; §12.4, N23), read from the headers composed for
/// the node, which are the ones it is sent.
pub(super) async fn route(
    federation: &Federation,
    arrived: Arrived<'_>,
    matched: &RouteMatch,
) -> Response {
    let started = Instant::now();
    let request_id = arrived.request_id;
    // NOTE: §5.4.1, N33: the client's id is free text that may carry an
    // identifier, so every log event names the gateway's own id instead.
    let logged = arrived.outbound.to_string();
    let Some(ehr_id) = path_ehr_id(matched) else {
        return error::fixed(Code::EhrIdInvalid, request_id);
    };
    let write = Write::of(matched);
    if let Some(refused) = write::path_refused(write, matched) {
        return error::response(refused.code(), refused.to_string(), request_id);
    }
    let request = match held(&arrived) {
        Ok(request) => request,
        Err(failure) => return unheld(&failure, request_id, &logged),
    };
    let snapshot = federation.snapshot();
    let located = match locate(federation, arrived.headers, write, &ehr_id, started) {
        Ok(located) => located,
        Err(Unlocated::Untargeted(untargeted)) => {
            return error::response(untargeted.code(), untargeted.to_string(), request_id);
        }
        Err(Unlocated::HeldElsewhere(refused)) => {
            return held_elsewhere(&refused, (request_id, &logged));
        }
    };
    let Some(budget) = Deadlines::from(federation, started) else {
        tracing::error!(
            request_id = logged,
            "the routed request's deadline cannot be represented"
        );
        return error::fixed(Code::Internal, request_id);
    };
    let (endpoint, step, probed) = match located {
        owner::Located::At { endpoint, step } => (endpoint, step, None),
        owner::Located::Collision(claimed) => return collision(&ehr_id, claimed, request_id),
        owner::Located::Unreachable { .. } => {
            return error::fixed(Code::NoDestination, request_id);
        }
        owner::Located::Unknown if arrived.method.is_safe() => {
            // NOTE: §5.4.1, N33: the probe reaches members the client never named,
            // so an ehr_id that may be a patient identifier is never probed.
            let Ok(asked) = ProbedEhrId::try_from(&ehr_id) else {
                security::probe_refused(&logged);
                return error::fixed(Code::ProbeRequiresUuid, request_id);
            };
            let probe = Probe {
                ehr_id: asked,
                headers: arrived.headers.clone(),
                per_node: budget.per_node(),
                overall: budget.overall(),
                request_id: arrived.outbound,
                conveyance: arrived.conveyance.clone(),
            };
            match ask_all(federation, &probe, &logged).await {
                Ok((endpoint, answer)) => {
                    let probed = (arrived.method == Method::GET
                        && arrived.path == probe.path()
                        && arrived.uri.query().is_none_or(str::is_empty))
                    .then_some(answer);
                    (endpoint, owner::Step::AskAll, probed)
                }
                Err((code, message)) => return error::response(code, message, request_id),
            }
        }
        // NOTE: §12a.1 route-ehr, N41: a write no earlier step routes is refused,
        // never routed by its version's creating_system_id alone.
        owner::Located::Unknown => return error::fixed(Code::TargetRequired, request_id),
    };
    if let Write::Versioned(preceding) = write
        && let Err(refused) = write::controlled(
            snapshot,
            preceding,
            (matched, request.headers(), &arrived.body),
            endpoint.node(),
        )
    {
        return not_controlled(&refused, endpoint, (request_id, &logged));
    }
    // NOTE: §11.1 never contacts a suspended endpoint, so a request that names
    // one resolves to no destination (§11.2); the suspension rule is our own design.
    if endpoint.status() == EndpointStatus::Suspended {
        return error::fixed(Code::NoDestination, request_id);
    }
    tracing::debug!(
        endpoint = %endpoint.id(),
        step = step.as_str(),
        request_id = logged,
        "routed a path ehr_id"
    );
    let provenance = Provenance::of(snapshot, endpoint);
    let forwarded = if let Some(answer) = probed {
        Ok(answer)
    } else {
        let sent = (request, arrived.outbound, &arrived.conveyance);
        forward(federation, endpoint, sent, &budget, &logged).await
    };
    match forwarded {
        Ok(forwarded) => {
            let read = follow_up::version_of(arrived.method, matched);
            learn(federation, (&ehr_id, read), endpoint, &forwarded, &logged);
            provenance.stamp(answered(forwarded))
        }
        Err(Failure::Internal) => error::fixed(Code::Internal, request_id),
        Err(Failure::Forward(failure)) => failed(&failure, provenance, (request_id, &logged)),
    }
}

/// What the first three steps of §12.5.1 say about the owner of `ehr_id`
/// for a request that writes `write`, read at `started` (N41).
///
/// A new EHR has no owner for a binding or the index to name, so only the
/// targeting headers route it (§12.4, §8.4, N23), and only while neither
/// places its `ehr_id` at another member.
///
/// # Errors
///
/// Returns [`Unlocated`] when the targeting headers name no one endpoint the
/// registry holds (§8.4.1), or another member holds a new EHR's `ehr_id`.
fn locate<'a>(
    federation: &'a Federation,
    headers: &HeaderMap,
    write: Write,
    ehr_id: &EhrId,
    started: Instant,
) -> Result<owner::Located<'a>, Unlocated> {
    let (snapshot, index) = (federation.snapshot(), federation.index());
    // TODO(#412): the authenticated client session the resolution bindings belong to.
    let session: Option<SessionKey> = None;
    let held = session.as_ref().map(|session| owner::Held {
        bindings: federation.bindings(),
        session,
        now: started,
    });
    if write == Write::NewEhr {
        let Some(endpoint) = owner::targeted(snapshot, headers)? else {
            return Ok(owner::Located::Unknown);
        };
        if let Some(refused) = owner::held_elsewhere(snapshot, held, index, ehr_id, endpoint) {
            return Err(Unlocated::HeldElsewhere(refused));
        }
        let step = owner::Step::Target;
        return Ok(owner::Located::At { endpoint, step });
    }
    Ok(owner::located(snapshot, headers, held, index, ehr_id)?)
}

/// Why a routed request is refused before its owner is located.
#[derive(Debug, thiserror::Error)]
enum Unlocated {
    /// The targeting headers name no one endpoint the registry holds.
    #[error(transparent)]
    Untargeted(#[from] owner::Untargeted),
    /// A new EHR's `ehr_id` is held at another member than the targeted one.
    #[error(transparent)]
    HeldElsewhere(owner::HeldElsewhere),
}

/// The `409` refusing a new EHR whose `ehr_id` `refused` places at another
/// member, logged with endpoint ids only (§12.4, §5.4.3).
fn held_elsewhere(refused: &owner::HeldElsewhere, (request_id, logged): (&str, &str)) -> Response {
    tracing::warn!(
        endpoint = %refused.at,
        holders = %owner::Listed(&refused.holders),
        detection = refused.detection.as_str(),
        request_id = logged,
        "a new EHR was refused: another member holds its ehr_id"
    );
    // NOTE: ITS-REST 1.1.0 ehr_create_with_id answers 409 for an ehr_id an EHR already uses,
    // and N42's ehr-id-collision with its incident is for two claimants, which a refusal never makes.
    error::response(Code::EhrIdHeld, refused.to_string(), request_id)
}

/// The refusal of a versioned write the path `ehr_id` routes to `endpoint`,
/// before anything is sent: it names no single preceding version, or the
/// node does not control that version (§10.3, §12.4, N23).
fn not_controlled(
    refused: &write::Refused,
    endpoint: &Endpoint,
    (request_id, logged): (&str, &str),
) -> Response {
    if let write::Refused::NotControlling(_) = refused {
        // NOTE: §10.3 copy-write-reject, CP-29: the controlling node is never asked, so a
        // write from a de-duplicated row is refused alike whether its owner is up or down.
        tracing::warn!(
            endpoint = %endpoint.id(),
            code = refused.code().as_str(),
            request_id = logged,
            "a versioned write was refused at a node that does not control the version it amends"
        );
    }
    error::response(refused.code(), refused.to_string(), request_id)
}

/// The `409` refusing a request whose `ehr_id` the members of `claimed`
/// claim, once its integrity incident is raised (§12.5.2, N42).
///
/// No claimant is sent the request, a read or a write.
fn collision(ehr_id: &EhrId, claimed: owner::Claimed, request_id: &str) -> Response {
    owner::collided(ehr_id, claimed.detection, &claimed.claimants);
    let refused = owner::Unsettled::Claimed(claimed.claimants);
    error::response(refused.code(), refused.to_string(), request_id)
}

/// The `ehr_id` the path segment of `matched` decodes to, or `None` when it
/// is no `HIER_OBJECT_ID`.
fn path_ehr_id(matched: &RouteMatch) -> Option<EhrId> {
    let param = matched.path_param(EHR_ID_PARAM)?;
    // NOTE: §12.5, a segment that is not UTF-8 or not a HIER_OBJECT_ID names
    // no EHR, so either failure is the malformed-ehr_id answer.
    EhrId::new(param.decoded().ok()?).ok()
}

/// Runs the ask-all probe and returns the one owner it found with its
/// answer, or the code and the message that refuse the read (§12.5.1 step
/// 4).
pub(crate) async fn ask_all<'a>(
    federation: &'a Federation,
    probe: &Probe,
    logged: &str,
) -> Result<(&'a Endpoint, Forwarded), (Code, String)> {
    let internal = || (Code::Internal, Code::Internal.message().to_owned());
    let snapshot = federation.snapshot();
    let members = owner::probed(snapshot);
    let answers = probe::ask_all(federation.clients(), &members, probe)
        .await
        .map_err(|error| {
            tracing::error!(
                error = %crate::chain(&error),
                request_id = logged,
                "the ask-all probe could not run"
            );
            internal()
        })?;
    for (endpoint, probed) in &answers {
        if let Answer::Failed(ForwardError::Withheld { part, .. }) = &probed.answer {
            security::forward_withheld(endpoint, *part, logged);
        }
        federation
            .dependencies()
            .contacted(endpoint, probed.contact());
        federation.requests().probed(endpoint, probed);
    }
    let answers = answers
        .into_iter()
        .map(|(endpoint, probed)| (endpoint, probed.answer))
        .collect();
    let (endpoint, answer) = match owner::settled(answers) {
        owner::Settled::Owner { endpoint, answer } => (endpoint, answer),
        owner::Settled::Failed(unsettled) => {
            if let owner::Unsettled::Claimed(claimants) = &unsettled {
                owner::collided(probe.ehr_id.ehr_id(), Detection::AskAll, claimants);
            }
            let code = unsettled.code();
            if code.status().is_server_error() {
                tracing::error!(
                    code = code.as_str(),
                    error = %unsettled,
                    request_id = logged,
                    "the ask-all probe named no owner"
                );
            }
            return Err((code, unsettled.to_string()));
        }
    };
    let declared = snapshot.endpoint(&endpoint).ok_or_else(|| {
        tracing::error!(
            endpoint = %endpoint,
            request_id = logged,
            "a probed endpoint left the snapshot"
        );
        internal()
    })?;
    Ok((declared, answer))
}

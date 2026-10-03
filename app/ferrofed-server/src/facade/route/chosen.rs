// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Routing a request that has no owner to find: the creation of an EHR, a
//! definition request and a DEMOGRAPHIC request each go to one endpoint that
//! is named, never found (§7a.1, §12.4, §12.6, N23, N32, N43).
//!
//! The client names it in the targeting headers, always; for the DEMOGRAPHIC
//! area the deployment declares the one endpoint the client may name.
//! Nothing is probed and no node is picked implicitly.

use std::time::Instant;

use axum::response::Response;
use ferrofed_engine::forward::HeldRequest;
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::snapshot::{Endpoint, EndpointStatus, RegistrySnapshot};
use http::HeaderMap;

use super::{Arrived, Deadlines, Failure, answered, failed, forward, held, unheld};
use crate::error::{self, Code};
use crate::facade::owner;
use crate::facade::provenance::Provenance;
use crate::federation::Federation;

/// Which endpoints the targeting headers of a request without an owner may
/// name.
#[derive(Debug, Clone, Copy)]
pub(super) enum Chooser<'a> {
    /// Any one endpoint of the registry (§8.4, §12.4, §12.6).
    Client,
    /// Only this endpoint, the one the deployment declared for the area
    /// (§7a.1, N32).
    Declared(&'a EndpointId),
}

/// Routes a request to the one endpoint `chooser` names, and answers as that
/// node did (§7a.1, §12.4, §12.6, N23, N32, N43); `area` names what was
/// routed in the debug event.
///
/// A new EHR has no owner for a binding or the index to name, and a template
/// or a stored query lives at the node it was sent to, so only the client
/// can name the node; a DEMOGRAPHIC request names the endpoint the
/// deployment declared for it. The query string and the declared values are
/// held first, as on the EHR route ([`held`]); then the
/// endpoint is [`chosen`]: nothing is probed and no node is picked
/// implicitly. The body is forwarded byte-identical, and the node's answer,
/// an error included, comes back as the node sent it with the acting
/// endpoint named; no two nodes' answers are combined (N22, N31, N33).
pub(super) async fn named(
    federation: &Federation,
    arrived: Arrived<'_>,
    area: &'static str,
    chooser: Chooser<'_>,
) -> Response {
    let started = Instant::now();
    let request = match held(&arrived) {
        Ok(request) => request,
        Err(failure) => {
            let logged = arrived.outbound.to_string();
            return unheld(&failure, arrived.request_id, &logged);
        }
    };
    to_named(federation, arrived, (request, started), area, chooser).await
}

/// Routes the client's `request`, already held to its operation at
/// `started`, to the one endpoint `chooser` names, as [`named`] does.
pub(super) async fn to_named(
    federation: &Federation,
    arrived: Arrived<'_>,
    (request, started): (HeldRequest, Instant),
    area: &'static str,
    chooser: Chooser<'_>,
) -> Response {
    let request_id = arrived.request_id;
    let logged = arrived.outbound.to_string();
    let snapshot = federation.snapshot();
    let endpoint = match chosen(snapshot, arrived.headers, chooser) {
        Ok(endpoint) => endpoint,
        Err((code, message)) => return error::response(code, message, request_id),
    };
    if endpoint.status() == EndpointStatus::Suspended {
        return error::fixed(Code::NoDestination, request_id);
    }
    let Some(budget) = Deadlines::from(federation, started) else {
        tracing::error!(
            request_id = logged,
            "the routed request's deadline cannot be represented"
        );
        return error::fixed(Code::Internal, request_id);
    };
    tracing::debug!(
        endpoint = %endpoint.id(),
        area,
        request_id = logged,
        "routed to the endpoint the targeting headers name"
    );
    let provenance = Provenance::of(snapshot, endpoint);
    let sent = (request, arrived.outbound, &arrived.conveyance);
    match forward(federation, endpoint, sent, &budget, &logged).await {
        Ok(forwarded) => provenance.stamp(answered(forwarded)),
        Err(Failure::Internal) => error::fixed(Code::Internal, request_id),
        Err(Failure::Forward(failure)) => failed(&failure, provenance, (request_id, &logged)),
    }
}

/// The one endpoint of `snapshot` the targeting headers of `headers` name,
/// where `chooser` allows it, or the code and the message refusing the
/// request (§8.4.1, §12.6).
///
/// The headers must name exactly one endpoint the registry holds: none is
/// `target-required`, several `endpoint-several`, and an unknown id or `*`
/// `endpoint-unknown`. Where the deployment declared the area's endpoint, a
/// header naming another is `targeting-conflict`, naming both.
fn chosen<'a>(
    snapshot: &'a RegistrySnapshot,
    headers: &HeaderMap,
    chooser: Chooser<'_>,
) -> Result<&'a Endpoint, (Code, String)> {
    let targeted = owner::targeted(snapshot, headers)
        .map_err(|untargeted| (untargeted.code(), untargeted.to_string()))?;
    // NOTE: §7a.1, §12.6, §12.4, N23: the request names its node, so a declared
    // endpoint only bounds what it may name and is never applied as a default.
    let Some(endpoint) = targeted else {
        let code = Code::TargetRequired;
        return Err((code, code.message().to_owned()));
    };
    match chooser {
        Chooser::Client => Ok(endpoint),
        Chooser::Declared(declared) if endpoint.id() == declared => Ok(endpoint),
        Chooser::Declared(declared) => Err((
            Code::TargetingConflict,
            format!(
                "the targeting headers select the endpoint {}, and this area is served \
                 by the declared endpoint {declared} alone (§7a.1, §8.4.1, N32)",
                endpoint.id()
            ),
        )),
    }
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The registry's definitions at the members: distribution, and the
//! per-member drift report (§12.7, N44, CP-40).
//!
//! Both are offered only where the deployment sets
//! `federation.fan_out_stored_queries` beside the registry, which
//! `OPTIONS {base}/` declares as `definition.stored_query_fan_out` (§7a.2).
//! A registry request asks for either by naming `*` or members in its
//! targeting headers: a distinct request, never the reading of a plain one,
//! which the registry answers alone (§12.7 stored-query-fanout, §12.6 item 1).
//! Where neither is offered, a targeting header on such a request is refused,
//! so a request for distribution is never answered as a plain one.
//!
//! A distributed `PUT` is stored at the registry first, and the registry's
//! copy, the canonical print it holds, is then sent to each named member
//! with ITS-REST `PUT /definition/query/{name}/{version}`, independently,
//! through the fan-out of [`fan_out::each`]. Nothing is rolled back, and a
//! failed member never removes the registry's definition. The answer is the
//! registry's `StoredQuery` with `meta.federation`, one `endpoints[]` entry
//! per registry member, on the statuses of the template fan-out (§12.6 items
//! 2 and 3). A definition carrying the `FROM ENDPOINT` or `ORGANISATION`
//! directive is refused for distribution before anything is stored (§12.7
//! fanout-endpoint-targeted-refused).
//!
//! The operator's action on the admin listener sends a version the registry
//! holds again to the members it names and leaves the registry's copy as it
//! is, so a member that missed a distribution or was admitted after it can be
//! sent the version (§12.7 stored-query-drift); a second `PUT` stays refused
//! (§12.7 stored-query-versioning). Its answer has the shape of a first
//! distribution, and its `meta.registry` says `held` where a first
//! distribution says `stored`.
//!
//! A `GET` of a version naming members reads each member's copy with ITS-REST
//! `GET /definition/query/{name}/{version}` and compares its AQL with the
//! registry's: the answer is the registry's `StoredQuery` with
//! `meta.federation`, `active` where the copy matches (§12.7
//! stored-query-drift). No node's text is copied into it.
//!
//! Each member's state is recorded on `GET /health/dependencies` from the
//! node's own answer, whatever the §11.1 record says: a refused store or a
//! copy that differs or is missing is an answer, and the member is `up`.

use std::collections::BTreeSet;
use std::time::Instant;

use axum::Json;
use axum::response::{IntoResponse, Response};
use ferrofed_engine::dispatch::definition::{DefinitionAt, NodeCopy};
use ferrofed_engine::dispatch::{Contact, DispatchError};
use ferrofed_registry::definition::StoredDefinition;
use ferrofed_registry::id::EndpointId;
use ferrofed_registry::snapshot::Endpoint;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use openehr_federation::aql::directive::FacadeQuery;
use openehr_federation::error::WireError;
use openehr_federation::headers;
use openehr_federation::meta::FederationMeta;
use openehr_federation::object::Extra;
use openehr_federation::outcome::{ErrorDetail, Outcome};
use openehr_federation::status::EndpointStatus;
use openehr_its::rest::generated::definition::StoredQuery;
use openehr_query::federation::{parse_federated, to_federated_aql};
use serde::Serialize;

use super::{Refused, its_rest};
use crate::error::Code;
use crate::facade::provenance::Provenance;
use crate::facade::route::fan_out::{
    self, Asked, Unfinished, answer_status, contact, each, not_sent, observed, record_meta, settled,
};
use crate::facade::route::{Arrived, Deadlines};
use crate::facade::security;
use crate::federation::Federation;
use ferrofed_engine::onward::conveyance::Conveyance;
use ferrofed_engine::outbound_id::OutboundId;

/// The `code` of a member whose copy differs from the registry's.
const DIFFERS: &str = "definition-differs";

/// The `code` of a member that holds no copy.
const MISSING: &str = "definition-missing";

/// The members a registry request names for distribution or a drift
/// report, or `None` when the deployment offers neither or the request names
/// none.
///
/// # Errors
///
/// The `400` of targeting the registry cannot answer, and the `404` of a
/// selection naming a suspended member (§8.4.1, §11.2). Where the deployment
/// offers neither, a request carrying a targeting header is a `400`
/// (`stored-query-fan-out-unsupported`), never answered as if it carried none.
pub(super) fn requested(
    federation: &Federation,
    headers: &HeaderMap,
) -> Result<Option<BTreeSet<EndpointId>>, Refused> {
    if !federation.fans_out_stored_queries() {
        let targeted = [headers::ENDPOINT, headers::ORGANISATION]
            .into_iter()
            .any(|name| headers.contains_key(name));
        return if targeted {
            Err(Refused::fixed(Code::StoredQueryFanOutUnsupported))
        } else {
            Ok(None)
        };
    }
    let Some(selected) = fan_out::members(federation.snapshot(), headers)
        .map_err(|refused| Refused::with(refused.code(), &refused))?
    else {
        return Ok(None);
    };
    match fan_out::targets(federation.snapshot(), &selected) {
        Some(_) => Ok(Some(selected)),
        None => Err(Refused::fixed(Code::NoDestination)),
    }
}

/// Refuses a definition whose AQL `aql` names members of the federation in
/// a `FROM ENDPOINT` or `ORGANISATION` directive, which no node can execute
/// (§12.7 fanout-endpoint-targeted-refused, §8.1).
// NOTE: §8.1 makes ORGANISATION the coarser selector of the same federation-only
// directive, so the reason §12.7 gives for FROM ENDPOINT holds for it too.
pub(super) fn distributable(aql: &str, logged: &str) -> Result<(), Refused> {
    let parsed = FacadeQuery::parse(aql).map_err(|refusal| {
        security::refused(&refusal, logged);
        Refused::with(Code::Refused((&refusal).into()), &refusal)
    })?;
    match parsed.directive() {
        Some(_) => Err(Refused::fixed(Code::DefinitionEndpointTargeted)),
        None => Ok(()),
    }
}

/// What the registry did with the version a distribution sends, which the
/// answer states as `meta.registry`.
// NOTE: no specification governs this: our own design; the admin action distributes
// a held version without storing it, so the answer says which of the two it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Registry {
    /// The request stored the version.
    Stored,
    /// The registry held the version already, and its copy is unchanged.
    Held,
}

/// The body of a distribution's or a drift report's answer: the registry's
/// definition, and `meta.federation` with each member's outcome (§9.5,
/// §12.6 item 2).
#[derive(Debug, Serialize)]
struct Reported {
    #[serde(flatten)]
    definition: StoredQuery,
    meta: ReportedMeta,
}

/// The `meta` of [`Reported`].
#[derive(Debug, Serialize)]
struct ReportedMeta {
    /// The per-member record.
    federation: FederationMeta,
    /// What the registry did with a distributed version; a drift report
    /// carries none.
    #[serde(skip_serializing_if = "Option::is_none")]
    registry: Option<Registry>,
}

/// The name and version of `definition` as a node request carries them.
fn at(definition: &StoredDefinition) -> (String, String) {
    (
        definition.name().as_str().to_owned(),
        definition.version().to_string(),
    )
}

/// The deadlines of a request to `federation` that arrived at `started`.
fn deadlines(
    federation: &Federation,
    started: Instant,
    logged: &str,
) -> Result<Deadlines, Refused> {
    Deadlines::from(federation, started).ok_or_else(|| {
        tracing::error!(
            request_id = logged,
            "the registry fan-out's deadline cannot be represented"
        );
        Refused::fixed(Code::Internal)
    })
}

/// What became of each member's request, or the internal refusal when the
/// fan-out could not tell, which is never attributed to a member.
fn finished<R>(
    sent: Result<Vec<(Asked<R>, u64)>, Unfinished>,
    logged: &str,
) -> Result<Vec<(Asked<R>, u64)>, Refused> {
    sent.map_err(|unfinished| {
        tracing::error!(
            error = %crate::chain(&unfinished),
            request_id = logged,
            "the registry fan-out could not tell what became of every member"
        );
        Refused::fixed(Code::Internal)
    })
}

/// Sends the registry's `definition` to each member of `selected` and
/// answers per node, the registry's definition standing whatever they
/// answer, with `registry` saying whether the request stored it (§12.7
/// stored-query-fanout, §12.6 item 3).
pub(super) async fn distribute(
    federation: &Federation,
    definition: &StoredDefinition,
    (registry, selected): (Registry, &BTreeSet<EndpointId>),
    (outbound, conveyance, started): (OutboundId, &Conveyance, Instant),
) -> Result<Response, Refused> {
    let logged = outbound.to_string();
    let targets = fan_out::targets(federation.snapshot(), selected)
        .ok_or_else(|| Refused::fixed(Code::NoDestination))?;
    let budget = deadlines(federation, started, &logged)?;
    let (name, version) = at(definition);
    let aql = definition.aql().to_owned();
    let sent = each(
        federation,
        &targets,
        (&budget, outbound, conveyance),
        &logged,
        |client, options| {
            let (name, version, aql) = (name.clone(), version.clone(), aql.clone());
            async move {
                let at = DefinitionAt {
                    name: &name,
                    version: &version,
                };
                client.store_definition(at, &aql, &options).await
            }
        },
    )
    .await;
    let sent = finished(sent, &logged)?;
    let mut outcomes = Vec::with_capacity(targets.len());
    for (endpoint, (sent, latency_ms)) in targets.iter().zip(sent) {
        let reached = contact(&sent, |stored| match stored {
            Ok(stored) => stored.contact,
            Err(_unsent) => Contact::Unsent,
        });
        let outcome = settled(sent, latency_ms, |stored, latency_ms| match stored {
            Ok(stored) => stored.outcome,
            Err(failure) => unsent(endpoint, &failure, latency_ms, &logged),
        });
        observed(federation, endpoint.id(), reached, &outcome);
        outcomes.push((endpoint.id(), outcome));
    }
    tracing::info!(
        members = outcomes.len(),
        accepted = accepted(&outcomes),
        registry = ?registry,
        request_id = logged,
        "a stored-query definition was distributed"
    );
    let code = answer_status(
        &outcomes
            .iter()
            .map(|(_, outcome)| outcome)
            .collect::<Vec<_>>(),
    );
    let location = HeaderValue::try_from(version).map_err(|_invalid| {
        tracing::error!("a version is not a valid header value");
        Refused::fixed(Code::Internal)
    })?;
    let answered = (code, Some(registry));
    let mut response = reported(federation, definition, &outcomes, answered, &logged)?;
    response.headers_mut().insert(header::LOCATION, location);
    Ok(response)
}

/// Reads each member of `selected`'s copy of the registry's `definition`
/// and answers with the definition and a per-member drift report (§12.7
/// stored-query-drift).
pub(super) async fn drift(
    federation: &Federation,
    definition: &StoredDefinition,
    selected: &BTreeSet<EndpointId>,
    arrived: &Arrived<'_>,
) -> Result<Response, Refused> {
    let started = Instant::now();
    let logged = arrived.outbound.to_string();
    let targets = fan_out::targets(federation.snapshot(), selected)
        .ok_or_else(|| Refused::fixed(Code::NoDestination))?;
    let budget = deadlines(federation, started, &logged)?;
    let (name, version) = at(definition);
    let sent = each(
        federation,
        &targets,
        (&budget, arrived.outbound, &arrived.conveyance),
        &logged,
        |client, options| {
            let (name, version) = (name.clone(), version.clone());
            async move {
                let at = DefinitionAt {
                    name: &name,
                    version: &version,
                };
                client.read_definition(at, &options).await
            }
        },
    )
    .await;
    let sent = finished(sent, &logged)?;
    let mut outcomes = Vec::with_capacity(targets.len());
    for (endpoint, (sent, latency_ms)) in targets.iter().zip(sent) {
        let reached = contact(&sent, |copy| match copy {
            Ok(copy) => copy.contact(),
            Err(_unsent) => Contact::Unsent,
        });
        let outcome = settled(sent, latency_ms, |copy, latency_ms| match copy {
            Ok(copy) => compared(definition.aql(), copy),
            Err(failure) => unsent(endpoint, &failure, latency_ms, &logged),
        });
        observed(federation, endpoint.id(), reached, &outcome);
        outcomes.push((endpoint.id(), outcome));
    }
    tracing::info!(
        members = outcomes.len(),
        matching = accepted(&outcomes),
        request_id = logged,
        "a stored-query definition was checked for drift"
    );
    // NOTE: no specification governs this: our own design; the registry's
    // definition is always answered, and 207 says not every member matches.
    let code = if accepted(&outcomes) == outcomes.len() {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };
    reported(federation, definition, &outcomes, (code, None), &logged)
}

/// How many of `outcomes` are `active`.
fn accepted(outcomes: &[(&EndpointId, Outcome)]) -> usize {
    outcomes
        .iter()
        .filter(|(_, outcome)| outcome.status() == EndpointStatus::Active)
        .count()
}

/// The outcome of a member's `copy` against the registry's `aql`: `active`
/// where it matches, and a `node-error` naming [`DIFFERS`] or [`MISSING`].
// NOTE: §11.1 has no drift status, and its set is closed; a copy that differs
// or is missing is node-error with a code saying which (our own design).
fn compared(aql: &str, copy: NodeCopy) -> Outcome {
    match copy {
        NodeCopy::Held {
            aql: held,
            latency_ms,
        } if same(aql, &held) => Outcome::Active { latency_ms },
        NodeCopy::Held { latency_ms, .. } => Outcome::NodeError {
            latency_ms,
            error: drifted(
                DIFFERS,
                "the node's copy of this version differs from the registry's definition",
            ),
        },
        NodeCopy::Missing { latency_ms } => Outcome::NodeError {
            latency_ms,
            error: drifted(
                MISSING,
                "the node holds no copy of this version: it answered 404 Not Found",
            ),
        },
        NodeCopy::Failed { outcome, .. } => outcome,
    }
}

/// Whether the AQL texts `registry` and `node` are one query: their
/// canonical prints are equal, so layout and comments do not count.
fn same(registry: &str, node: &str) -> bool {
    // NOTE: no specification governs this: our own design; a node copy that
    // is not AQL is legitimately not the registry's query, so it differs.
    match (parse_federated(registry), parse_federated(node)) {
        (Ok(registry), Ok(node)) => to_federated_aql(&registry) == to_federated_aql(&node),
        _ => false,
    }
}

/// The structured `error` of a drifted member, its `code` and `message`.
fn drifted(code: &str, message: &str) -> ErrorDetail {
    let mut members = Extra::new();
    // NOTE: no specification governs this: our own design; a string always
    // serializes, and the text form keeps both should it ever not.
    let built = members
        .insert_serialized("code", code)
        .and_then(|_none| members.insert_serialized("message", message));
    match built {
        Ok(_none) => ErrorDetail::Object(members),
        Err(_unserialized) => ErrorDetail::Text(format!("{code}: {message}")),
    }
}

/// The outcome of a member the gateway could not send its request to, the
/// gateway-side `failure` logged.
fn unsent(endpoint: &Endpoint, failure: &DispatchError, latency_ms: u64, logged: &str) -> Outcome {
    if let DispatchError::Withheld {
        endpoint: withheld,
        part,
    } = failure
    {
        security::forward_withheld(withheld, *part, logged);
    }
    tracing::error!(
        endpoint = %endpoint.id(),
        error = %crate::chain(failure),
        request_id = logged,
        "a stored-query definition request could not be sent to a member"
    );
    not_sent(latency_ms)
}

/// The answer `code` carrying the registry's `definition`, the record of
/// `outcomes` and, for a distribution, what the `registry` did with it, its
/// provenance naming the `active` members (§7a.3, N31).
fn reported(
    federation: &Federation,
    definition: &StoredDefinition,
    outcomes: &[(&EndpointId, Outcome)],
    (code, registry): (StatusCode, Option<Registry>),
    logged: &str,
) -> Result<Response, Refused> {
    let meta = record_meta(federation, outcomes).map_err(|refused: WireError| {
        tracing::error!(
            error = %crate::chain(&refused),
            request_id = logged,
            "the registry fan-out's per-node record could not be built"
        );
        Refused::fixed(Code::Internal)
    })?;
    let provenance = Provenance::active(&meta);
    let body = Reported {
        definition: its_rest(definition),
        meta: ReportedMeta {
            federation: meta,
            registry,
        },
    };
    Ok(provenance.stamp((code, Json(body)).into_response()))
}

#[cfg(test)]
mod tests {
    use super::same;

    #[test]
    fn layout_and_comments_do_not_make_a_copy_differ() {
        let registry = "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c";
        assert!(same(
            registry,
            "SELECT  c/uid/value\nFROM EHR e -- a comment\n  CONTAINS COMPOSITION c"
        ));
        assert!(!same(
            registry,
            "SELECT c/name/value FROM EHR e CONTAINS COMPOSITION c"
        ));
        assert!(!same(registry, "not AQL at all"));
    }
}

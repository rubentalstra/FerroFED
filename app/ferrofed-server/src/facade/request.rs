// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The query intake: one federated query request as it arrived, what it
//! submits, and its reading into the façade query, the endpoints it selects
//! and the analysis of its query (§7.2, §8.1, §8.4).
//!
//! A body or a query string that is no ITS-REST ad hoc query is refused with
//! a fixed message, because the decoder's own may quote the request
//! (§5.4.3). Every refusal and strip of the analysis is a security event,
//! by position and never by value.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::time::Instant;

use axum::response::Response;
use ferrofed_engine::declared;
use ferrofed_engine::fanout::Completion;
use ferrofed_engine::onward::conveyance::Conveyance;
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_registry::id::EndpointId;
use http::HeaderMap;
use openehr_federation::aql::directive::FacadeQuery;
use openehr_federation::aql::{Analysis, Paging, Targeting};
use openehr_federation::dedup::DedupMode;
use openehr_its::rest::generated::query::{AdhocQueryExecute, QueryExecuteAdhocQueryParams};
use openehr_its::rest::routes::RouteMatch;

use crate::facade::answer::Failure;
use crate::facade::{intake, route, security, target};
use crate::federation::Federation;

/// One federated query request, as it arrived.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Arrived<'a> {
    /// The request headers, which select the modes and may name the node set.
    pub(crate) headers: &'a HeaderMap,
    /// The exchange id the answer names, empty when the client sent none.
    pub(crate) request_id: &'a str,
    /// The gateway's id for the request, the one every node receives.
    pub(crate) outbound: OutboundId,
    /// Whom the request is on behalf of, conveyed to every node it reaches
    /// (§13.1, N24).
    pub(crate) conveyance: &'a Conveyance,
    /// When the request arrived, the instant the overall budget runs from.
    pub(crate) started: Instant,
}

/// What a federated query request submits.
#[derive(Debug)]
pub(crate) enum Submitted<'a> {
    /// The body of `POST {base}/v1/query/aql`, an ITS-REST
    /// `AdhocQueryExecute`.
    Body(&'a [u8]),
    /// The query string of `GET {base}/v1/query/aql`, the operation
    /// `matched` names, which the generated parameters decode.
    Query {
        /// The matched `query_execute_adhoc_query` operation.
        matched: &'a RouteMatch,
        /// The query string, without its `?`.
        query: Option<&'a str>,
    },
    /// A stored query invoked by name: its AQL with the client's `offset`,
    /// `fetch` and `query_parameters`, run exactly as if submitted inline,
    /// and the qualified name the answer carries (§12.7, N44).
    Stored {
        /// The expanded request.
        request: AdhocQueryExecute,
        /// The qualified name of the gateway's definition.
        name: &'a str,
    },
}

/// Analyses the façade query of `request` under the request's `completion`
/// and `dedup` mode, directed at the `named` endpoints when the query names
/// them, recording a refusal or a strip as a security event (§5.4.3).
///
/// An aggregate recombined across nodes is refused under best-effort: it is
/// exactly correct only over every node in scope (§11.6.3), and the gateway
/// never serves an all-or-nothing answer to a request that asked for
/// `partial` (§11.4).
fn analysed(
    federation: &Federation,
    (facade, named): (FacadeQuery, Option<&BTreeSet<EndpointId>>),
    request: &AdhocQueryExecute,
    (completion, dedup): (Completion, DedupMode),
    request_id: &str,
) -> Result<Analysis, Failure> {
    let parameters = intake::parameters(request.query_parameters.as_ref())?;
    let paging = Paging {
        offset: request.offset,
        fetch: request.fetch,
    };
    let mut context = federation.context().clone().with_dedup(dedup);
    if let Some(endpoints) = named.and_then(|named| NonZeroUsize::new(named.len())) {
        context = context.with_targeting(Targeting::Directed { endpoints });
    }
    let analysis = facade
        .analyse(&parameters, paging, &context)
        .and_then(|analysis| {
            if completion == Completion::BestEffort {
                analysis.admit_best_effort()?;
            }
            Ok(analysis)
        })
        .inspect_err(|refusal| security::refused(refusal, request_id))?;
    if let Analysis::Patient(query) = &analysis {
        security::stripped(query, request_id);
    }
    Ok(analysis)
}

/// The answer refusing the `Content-Type` of a request the gateway answers
/// itself, addressing `matched`, or `None` when the operation takes it
/// (ITS-REST 1.1.0; RFC 9110 §8.3).
///
/// A `Content-Type` naming no media type the operation lists is a `415`
/// (`media-type-unsupported`) naming the client's `request_id`, answered
/// before the body is read, stored or sent anywhere (RFC 9110 §15.5.16); the
/// gateway's `logged` id names any event. A `body` sent without a
/// `Content-Type` is read as the first listed media type
/// ([`declared::content_type`]): `application/json` for a query `POST`,
/// `text/plain` for a stored-query definition `PUT`.
pub(crate) fn unsupported_media(
    matched: &RouteMatch,
    (headers, body): (&HeaderMap, &[u8]),
    (request_id, logged): (&str, &str),
) -> Option<Response> {
    declared::content_type(matched, headers, body)
        .err()
        .map(|refusal| route::declared_refused(&refusal, request_id, logged))
}

/// The request `sent`, the endpoints its directive or its `headers`
/// select, and the analysis of its query under the request's modes.
pub(super) fn read(
    federation: &Federation,
    (sent, headers): (Submitted<'_>, &HeaderMap),
    modes: (Completion, DedupMode),
    request_id: &str,
) -> Result<(AdhocQueryExecute, Option<BTreeSet<EndpointId>>, Analysis), Failure> {
    let request: AdhocQueryExecute = match sent {
        // NOTE: §5.4.3, the reader's message may quote the body, so a malformed
        // body is refused with a fixed message.
        Submitted::Body(body) => serde_json::from_slice(body).map_err(|_quoted| Failure::Body)?,
        Submitted::Query { matched, query } => adhoc(matched, query)?,
        Submitted::Stored { request, .. } => request,
    };
    let (facade, named) = directed(federation, &request.q, headers, request_id)?;
    let analysis = analysed(
        federation,
        (facade, named.as_ref()),
        &request,
        modes,
        request_id,
    )?;
    Ok((request, named, analysis))
}

/// The ad hoc query the query string `query` of `matched` carries, decoded
/// by the generated parameters of `query_execute_adhoc_query`: one pair per
/// scalar, one per `query_parameters` member, each name and value
/// percent-decoded (RFC 3986 §2.1, so a `+` is a literal plus).
fn adhoc(matched: &RouteMatch, query: Option<&str>) -> Result<AdhocQueryExecute, Failure> {
    // NOTE: no specification governs this: our own design; the façade reads its
    // headers itself, as for a POST, so the decoder reads the query string alone.
    let params = QueryExecuteAdhocQueryParams::from_request(matched, query, &HeaderMap::new())
        .map_err(Failure::Query)?;
    // NOTE: §5.4.1, N33: the node is scoped by its own ehr_id, so a client's
    // `ehr_id` is dropped, as the POST body's undeclared members are.
    Ok(AdhocQueryExecute {
        q: params.q,
        offset: params.offset,
        fetch: params.fetch,
        query_parameters: params.query_parameters,
        additional_properties: BTreeMap::new(),
    })
}

/// The façade query `q`, parsed, and the endpoints its directive or the
/// targeting headers in `headers` select, or `None` for an undirected query
/// (§8.1, §8.4).
///
/// The request URI is never read: no query parameter targets anything
/// (§8.4, N35). A selection of no endpoint leaves the request no destination
/// (§11.2).
fn directed(
    federation: &Federation,
    q: &str,
    headers: &HeaderMap,
    request_id: &str,
) -> Result<(FacadeQuery, Option<BTreeSet<EndpointId>>), Failure> {
    let facade =
        FacadeQuery::parse(q).inspect_err(|refusal| security::refused(refusal, request_id))?;
    let named = target::requested(federation.snapshot(), facade.directive(), headers)?;
    if named.as_ref().is_some_and(BTreeSet::is_empty) {
        return Err(Failure::NoDestination);
    }
    Ok((facade, named))
}

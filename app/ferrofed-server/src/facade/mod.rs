// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The ITS-REST façade: `POST {base}/v1/query/aql`, and its `GET` form,
//! answered as one federated `RESULT_SET` over every member (§7, §9, §11).
//!
//! An unmodified openEHR client sends a §7.2 façade query and receives one
//! ITS-REST `RESULT_SET` with the rows of every member that answered and
//! `meta.federation` naming every endpoint (N1, N16, N17). The `query` group
//! is FerroFED's own handler over the generated DTOs, because the federated
//! `424` and `504` carry `meta.federation`, which the generated `ApiError`
//! cannot (N37, §11.4).
//!
//! One request runs the reference flow of §4: [`completeness`] reads the
//! completion strategy the request selects (§11.4), [`dedup`] the dedup mode
//! (§10), [`prefer`] reads the
//! client deadline that can shorten the budget (§11.5), [`intake`] types the query
//! parameters, [`target`] selects the node set a `FROM ENDPOINT` or
//! `ORGANISATION` directive or a targeting header names (§8.1, §8.4), the
//! rewrite analyses the query and names the patient,
//! [`plan`] resolves the patient at every member and builds one node query
//! per member that knows them, the engine fans out under the budget and the
//! strategy, and [`cells`] builds each façade row with the subject columns
//! re-injected (N5) and the ENDPOINT attributes the query selects added from
//! the registry entry of the endpoint the row came from (§9.3, N12).
//! A refused query is a `400` whose message locates the fault by byte range
//! and never quotes it (§5.4.3), and whose body names the refusal's stable
//! code ([`crate::error`]). Every strip, refusal and outbound-gate stop is
//! a [`security`] event, by position and never by value.
//!
//! A request to an EHR resource under a path `ehr_id` is routed to one node
//! instead, and passed through byte-identical ([`route`]; §7a.1, §7a.3);
//! [`owner`] finds that node in the order of §12.5.1, and a resolution here
//! teaches its `ehr_id` index which member holds each resolved `ehr_id`.
//! An undirected query the client scoped to one `ehr_id`, in either form of
//! N29, goes to the member that owns it, found in the same order (`scoped`).
//! A versioned write goes there only when that node controls the version it
//! amends, and a new EHR only to an explicit target
//! ([`write`](mod@write); §12.4).
//! A definition request goes to the one endpoint the targeting headers
//! name, never to a node picked implicitly and never merged ([`route`];
//! §7a.1, §12.6, N43), unless the stored-query registry holds it
//! ([`stored`]; §12.7).
//! The read of an EHR by subject resolves the subject at the gateway and is
//! routed by the resolved `ehr_id` alone ([`subject`]; §5.2, N33).
//! Every version a fan-out or a routed answer shows an endpoint holding
//! teaches the follow-up routing table ([`follow_up`]; §12.2, N21).
//! `OPTIONS {base}/` describes the whole surface, and `OPTIONS` on a sub-path
//! names the methods served there ([`options`]; §7a.2, N30).
//!
//! This module holds the two query handlers; `request` reads what a
//! request submits, and `answer` answers it.

mod answer;
pub mod cells;
pub mod completeness;
mod consent;
pub mod dedup;
pub mod follow_up;
pub mod intake;
mod localize;
pub mod options;
pub mod owner;
pub mod plan;
pub mod prefer;
mod provenance;
mod request;
pub mod route;
mod scoped;
pub mod security;
pub mod stored;
pub mod subject;
pub mod target;
pub mod write;

use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use ferrofed_engine::outbound_id::OutboundId;
use http::{HeaderMap, Method, Uri};
use openehr_its::rest::routes::{self, Lookup};

use crate::auth::caller::Caller;
use crate::conveyed;
use crate::error::{self, Code};
use crate::facade::request::{Arrived, Submitted};
use crate::request_id;
use crate::state::AppState;

/// The route the federated query is served at, under the ITS-REST prefix.
pub const QUERY_AQL: &str = "/v1/query/aql";

/// The ad hoc query's path relative to the ITS-REST base, as the
/// `openehr-its` route table names it.
const ADHOC_QUERY: &str = "/query/aql";

/// `POST {base}/v1/query/aql`: the federated ad hoc query.
///
/// Without a federation, the gateway federates nothing and answers as the
/// unserved ITS-REST surface does. Every node the query reaches receives the
/// request's [`OutboundId`], never the client's `x-request-id` (§5.4.1, N33);
/// the client's id names the request only in the answer.
pub async fn query_aql(
    State(state): State<Arc<AppState>>,
    outbound: Option<Extension<OutboundId>>,
    caller: Option<Extension<Caller>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let outbound = outbound.map_or_else(OutboundId::mint, |Extension(id)| id);
    federated(
        &state,
        (&headers, outbound, started, caller.as_deref()),
        Submitted::Body(&body),
    )
    .await
}

/// `GET {base}/v1/query/aql`: the federated ad hoc query, its members in the
/// query string (ITS-REST Query API, `query_execute_adhoc_query`).
///
/// The query string is decoded by the generated parameters of the
/// operation, and the request it stands for runs the pipeline of
/// [`query_aql`] unchanged (N1, CP-1). A query string the decoder refuses is
/// a `400` (`body-invalid`), the answer to a malformed `POST` body.
pub async fn query_aql_get(
    State(state): State<Arc<AppState>>,
    outbound: Option<Extension<OutboundId>>,
    caller: Option<Extension<Caller>>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let started = Instant::now();
    let outbound = outbound.map_or_else(OutboundId::mint, |Extension(id)| id);
    let Lookup::Matched(matched) = routes::lookup(&Method::GET, ADHOC_QUERY) else {
        tracing::error!(
            request_id = %outbound,
            "openehr-its declares no GET {ADHOC_QUERY}"
        );
        let request_id = request_id::of(&headers).unwrap_or_default();
        return error::fixed(Code::Internal, request_id);
    };
    let submitted = Submitted::Query {
        matched: &matched,
        query: uri.query(),
    };
    federated(
        &state,
        (&headers, outbound, started, caller.as_deref()),
        submitted,
    )
    .await
}

/// Runs `submitted` over the configured federation, or answers `501` when
/// none is configured.
///
/// A `POST` body is read only under a `Content-Type` the operation lists, and
/// is otherwise a `415` no node is asked for ([`request::unsupported_media`]).
async fn federated(
    state: &AppState,
    (headers, outbound, started, caller): (&HeaderMap, OutboundId, Instant, Option<&Caller>),
    submitted: Submitted<'_>,
) -> Response {
    let request_id = request_id::of(headers).unwrap_or_default();
    let Some(federation) = state.federation() else {
        return error::fixed(Code::NotImplemented, request_id);
    };
    let conveyance = match conveyed::of(&federation, caller) {
        Ok(conveyance) => conveyance,
        Err(unconveyed) => return unconveyed.respond(request_id, &outbound.to_string()),
    };
    if let Submitted::Body(body) = submitted {
        let logged = outbound.to_string();
        let Lookup::Matched(matched) = routes::lookup(&Method::POST, ADHOC_QUERY) else {
            tracing::error!(
                request_id = logged,
                "openehr-its declares no POST {ADHOC_QUERY}"
            );
            return error::fixed(Code::Internal, request_id);
        };
        let ids = (request_id, logged.as_str());
        if let Some(refused) = request::unsupported_media(&matched, (headers, body), ids) {
            return refused;
        }
    }
    let arrived = Arrived {
        headers,
        request_id,
        outbound,
        conveyance: &conveyance,
        started,
    };
    answer::answer(&federation, arrived, submitted).await
}

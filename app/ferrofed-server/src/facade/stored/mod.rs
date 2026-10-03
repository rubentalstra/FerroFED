// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The federated stored-query registry: definitions held at the gateway,
//! versioned immutably, and invoked by name as an ordinary fan-out (§12.7,
//! N44, CP-40).
//!
//! The gateway is authoritative for each definition (§12.7). `PUT
//! {base}/v1/definition/query/{name}/{version}` stores the AQL on ITS-REST's
//! semver segment, `GET` on the same path reads it back, and `GET
//! {base}/v1/definition/query/{name}` lists every version of every name the
//! segment starts (ITS-REST Definition API). A definition is analysed as a
//! façade query before it is held, and one that names its patient by a
//! literal is refused, so no patient identifier is held at rest (§5.4.1,
//! N33). A second `PUT` of a held name and version is a `409`, and the held
//! text stands (§12.7, N44; ITS-REST `409_StoredQuery_version`). A read-only
//! registry answers a `PUT` with `405`, and over a store several replicas
//! share, each read and invocation reads the store again first, so every
//! replica answers what any of them stored.
//!
//! `GET` and `POST {base}/v1/query/{name}[/{version}]` expand the definition
//! with the client's `offset`, `fetch` and `query_parameters`, from the query
//! string or the body, and run it through the pipeline of
//! `POST {base}/v1/query/aql`, exactly as if the client had
//! submitted the text inline: the targeting headers, the completion and dedup
//! modes and the budget apply unchanged, and the answer carries ITS-REST's
//! `name` naming the gateway's definition (§12.7, N44). Without a version,
//! the highest one is run (ITS-REST Query API). The registry's own AQL is
//! always the one run, never a member's copy (§12.7 stored-query-drift).
//!
//! Where the deployment offers it, a `PUT` naming members in its targeting
//! headers is also distributed to them, and a `GET` of a version naming them
//! reports per member whether its copy matches (`distribution`). The
//! operator sends a held version again to members that miss it through the
//! admin listener (`distribute_held`), never through a `PUT`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::response::{IntoResponse, Response};
use ferrofed_engine::declared::query;
use ferrofed_engine::outbound_id::OutboundId;
use ferrofed_registry::definition::store::{Definitions, Insertion};
use ferrofed_registry::definition::{QueryName, QueryVersion, StoredDefinition, VersionPattern};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use jiff::Timestamp;
use openehr_federation::aql::definition::{Definition, SubjectOrigin};
use openehr_its::rest::generated::definition::{
    DefinitionQueryVersionStoreYamlParams, StoredQuery,
};
use openehr_its::rest::generated::query::{
    AdhocQueryExecute, Query, QueryExecuteStoredQueryParams, QueryExecuteStoredQueryVersionParams,
};
use openehr_its::rest::routes::RouteMatch;

use crate::error::{self, Code};
use crate::facade::request::{self, Submitted};
use crate::facade::route::Arrived;
use crate::facade::stored::distribution::Registry;
use crate::facade::{answer, security};
use crate::federation::Federation;
use crate::request_id;
use crate::state::AppState;

mod distribution;

/// The path parameter that names the stored query.
const NAME_PARAM: &str = "qualified_query_name";

/// The path parameter that names its version.
const VERSION_PARAM: &str = "version";

/// The message of a query string the generated parameters refuse.
const QUERY_STRING_INVALID: &str =
    "the query string is not the one the ITS-REST operation declares";

/// The message of a held version's distribution that carries a body.
const HELD_TAKES_NO_BODY: &str =
    "the distribution of a held version takes no body: it sends the registry's copy";

/// The message of a held version's distribution that names no member.
const HELD_NAMES_MEMBERS: &str = "the distribution of a held version names its members in the \
     openEHR-federation-endpoint or openEHR-federation-organisation header";

/// The query language the registry stores, the ITS-REST `query_type`
/// default.
const AQL: &str = "AQL";

/// One registry operation a request addresses (ITS-REST Definition and Query
/// APIs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    /// `PUT /definition/query/{name}/{version}`.
    Store,
    /// `PUT /definition/query/{name}`, refused: the registry stores at a
    /// version (§12.7).
    StoreUnversioned,
    /// `GET /definition/query/{name}/{version}`.
    Read,
    /// `GET /definition/query/{name}`.
    List,
    /// `GET` or `POST` of `/query/{name}` and `/query/{name}/{version}`,
    /// its members in the `carrier`.
    Execute(Carrier),
}

/// Where a stored-query invocation carries its members (ITS-REST Query API).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carrier {
    /// The `Query` body of a `POST`.
    Body,
    /// The query string of a `GET`.
    Query,
}

/// The registry operation `matched` names, or `None` for every other
/// operation.
fn operation(matched: &RouteMatch) -> Option<Operation> {
    match matched.operation_id {
        "definition_query_version_store.yaml" => Some(Operation::Store),
        "definition_query_store.yaml" => Some(Operation::StoreUnversioned),
        "definition_query_version_get" => Some(Operation::Read),
        "definition_query_list" => Some(Operation::List),
        "query_execute_stored_query_body" | "query_execute_stored_query_version_body" => {
            Some(Operation::Execute(Carrier::Body))
        }
        "query_execute_stored_query" | "query_execute_stored_query_version" => {
            Some(Operation::Execute(Carrier::Query))
        }
        _ => None,
    }
}

/// Whether the registry answers `matched` when it is offered.
pub(crate) fn serves(matched: &RouteMatch) -> bool {
    operation(matched).is_some()
}

/// Whether a registry that is `read_only` serves `matched` with anything but
/// a `405`: every operation it answers, less the stores of a read-only one.
pub(crate) fn accepts(matched: &RouteMatch, read_only: bool) -> bool {
    operation(matched).is_some_and(|operation| !(read_only && operation.stores()))
}

impl Operation {
    /// Whether the operation stores a definition.
    fn stores(self) -> bool {
        matches!(self, Self::Store | Self::StoreUnversioned)
    }
}

/// The methods a read-only registry serves on a definition path, which a
/// `PUT` there names in `Allow` (RFC 9110 §15.5.6).
const READ_ONLY_ALLOW: &str = "GET, OPTIONS";

/// Answers `arrived` when it addresses the registry, or hands it back.
///
/// # Errors
///
/// The request, unanswered, when `matched` is no registry operation.
pub(crate) async fn serve<'a>(
    federation: &Federation,
    definitions: &Arc<Definitions>,
    matched: &RouteMatch,
    arrived: Arrived<'a>,
) -> Result<Response, Arrived<'a>> {
    let Some(operation) = operation(matched) else {
        return Err(arrived);
    };
    let request_id = arrived.request_id;
    if operation.stores() && definitions.is_read_only() {
        return Ok(read_only(request_id));
    }
    let answered = match operation {
        Operation::Store => store(federation, definitions, matched, &arrived).await,
        Operation::StoreUnversioned => Err(Refused::fixed(Code::QueryVersionRequired)),
        Operation::Read => read(federation, definitions, matched, &arrived).await,
        Operation::List => list(definitions, matched, &arrived).await,
        Operation::Execute(carrier) => {
            execute(federation, definitions, (matched, carrier), arrived).await
        }
    };
    Ok(answered.unwrap_or_else(|refused| refused.respond(request_id)))
}

/// The `405` of a `PUT` at a read-only registry, with `Allow` naming the
/// methods it serves there (§12.7; RFC 9110 §15.5.6).
fn read_only(request_id: &str) -> Response {
    let mut response = error::fixed(Code::StoredQueryReadOnly, request_id);
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(READ_ONLY_ALLOW));
    response
}

/// Reads the store again before the view answers, when other processes
/// share it: every version of `name`, or with no name every definition, so
/// each replica answers what any of them stored (§12.7, N44).
async fn refreshed(
    definitions: &Arc<Definitions>,
    name: Option<&QueryName>,
    outbound: &OutboundId,
) -> Result<(), Refused> {
    if !definitions.is_shared() {
        return Ok(());
    }
    let held = Arc::clone(definitions);
    let name = name.cloned();
    // NOTE: no specification governs this: our own design; the read waits on
    // the database, so it runs where a blocking call may wait.
    let read = tokio::task::spawn_blocking(move || match name {
        Some(name) => held.refresh_named(&name),
        None => held.refresh(),
    })
    .await;
    match read {
        Ok(Ok(())) => Ok(()),
        Ok(Err(failure)) => {
            tracing::error!(
                error = crate::chain(&failure),
                request_id = %outbound,
                "the stored-query store could not be read"
            );
            Err(Refused::fixed(Code::Internal))
        }
        Err(failure) => {
            tracing::error!(
                error = %failure,
                request_id = %outbound,
                "the stored-query read did not complete"
            );
            Err(Refused::fixed(Code::Internal))
        }
    }
}

/// A registry request refused before anything is stored or dispatched.
#[derive(Debug)]
struct Refused {
    /// The code, which names the status.
    code: Code,
    /// The message, which never quotes the request.
    message: String,
}

impl Refused {
    /// The refusal `code`, with its fixed message.
    fn fixed(code: Code) -> Self {
        Self {
            code,
            message: code.message().to_owned(),
        }
    }

    /// The refusal `code`, with `message`.
    fn with(code: Code, message: &impl std::fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }

    /// The error answer, naming the exchange id `request_id`.
    fn respond(self, request_id: &str) -> Response {
        error::response(self.code, self.message, request_id)
    }
}

/// The decoded segment `param` of `matched`, when the template declares it.
fn segment(matched: &RouteMatch, param: &str) -> Option<String> {
    // NOTE: no specification governs this: our own design; a segment that does
    // not decode to UTF-8 names nothing, and is refused as malformed.
    matched
        .path_param(param)
        .and_then(|segment| segment.decoded().ok())
}

/// The stored query's qualified name in `matched`.
fn name(matched: &RouteMatch) -> Result<QueryName, Refused> {
    let text = segment(matched, NAME_PARAM).unwrap_or_default();
    QueryName::new(&text).map_err(|refused| Refused::with(Code::QueryNameInvalid, &refused))
}

/// The version pattern in `matched`, `None` when the path names none.
fn pattern(matched: &RouteMatch) -> Result<Option<VersionPattern>, Refused> {
    if matched.path_param(VERSION_PARAM).is_none() {
        return Ok(None);
    }
    let text = segment(matched, VERSION_PARAM).unwrap_or_default();
    text.parse::<VersionPattern>()
        .map(Some)
        .map_err(|refused| Refused::with(Code::QueryVersionInvalid, &refused))
}

/// Checks the query string of a `PUT`: only the declared `query_type`, and
/// only AQL (ITS-REST Definition API).
///
/// Parameter names are held to those the operation declares, by position,
/// and the query string is decoded by the operation's generated parameters
/// (RFC 3986 §2.1, so a `+` is a literal plus).
fn query_string(matched: &RouteMatch, arrived: &Arrived<'_>) -> Result<(), Refused> {
    let query = arrived.uri.query();
    if let Some(query) = query {
        query::every_declared(matched, query).map_err(|unlisted| {
            security::query_parameter_refused(unlisted.position, &arrived.outbound.to_string());
            Refused::with(Code::QueryParameterRefused, &unlisted)
        })?;
    }
    let params =
        DefinitionQueryVersionStoreYamlParams::from_request(matched, query, &HeaderMap::new())
            .map_err(|_named| Refused::with(Code::BodyInvalid, &QUERY_STRING_INVALID))?;
    match params.query_type {
        Some(language) if !language.eq_ignore_ascii_case(AQL) => {
            Err(Refused::fixed(Code::QueryTypeUnsupported))
        }
        Some(_) | None => Ok(()),
    }
}

/// The members of a stored-query invocation: the `Query` body of a `POST`,
/// or the query string of a `GET`, which the operation's generated
/// parameters decode, a `q` key included as a query parameter, since the
/// `GET` forms declare none (ITS-REST Query API).
///
/// A decoded `ehr_id` is dropped, as the body's undeclared members are.
fn members(
    matched: &RouteMatch,
    arrived: &Arrived<'_>,
    carrier: Carrier,
) -> Result<Query, Refused> {
    let decoded = |offset, fetch, query_parameters| Query {
        offset,
        fetch,
        query_parameters,
        additional_properties: BTreeMap::new(),
    };
    let (query, headers) = (arrived.uri.query(), &HeaderMap::new());
    // NOTE: §5.4.3, the reader's and the decoder's messages may quote the
    // request, so a malformed one is refused with a fixed message.
    match carrier {
        Carrier::Body => serde_json::from_slice(&arrived.body)
            .map_err(|_quoted| Refused::fixed(Code::BodyInvalid)),
        Carrier::Query if matched.path_param(VERSION_PARAM).is_some() => {
            QueryExecuteStoredQueryVersionParams::from_request(matched, query, headers)
                .map(|p| decoded(p.offset, p.fetch, p.query_parameters))
                .map_err(|_named| Refused::with(Code::BodyInvalid, &QUERY_STRING_INVALID))
        }
        Carrier::Query => QueryExecuteStoredQueryParams::from_request(matched, query, headers)
            .map(|p| decoded(p.offset, p.fetch, p.query_parameters))
            .map_err(|_named| Refused::with(Code::BodyInvalid, &QUERY_STRING_INVALID)),
    }
}

/// `PUT {base}/v1/definition/query/{name}/{version}`: stores the definition
/// unless its name and version are held (§12.7, N44).
///
/// The body is the AQL text (ITS-REST `text/plain`), and a `Content-Type`
/// naming another media type is a `415` before anything is read or stored.
/// It is analysed as a
/// façade query, with every `$parameter` standing in for a value an
/// invocation binds, and refused with a `400` when the rewrite would refuse
/// it whatever is bound, or when it names its patient by a literal. The text
/// held is the canonical print of the parse. A stored definition answers
/// `200` with `Location` naming it, as a reference relative to the request
/// (RFC 9110 §10.2.2), because the gateway does not know its public base URL
/// (§4.1, N28).
///
/// A request naming members for distribution is refused before anything is
/// stored when its targeting cannot be answered, or when the definition
/// carries a `FROM ENDPOINT` or `ORGANISATION` directive (§12.7
/// fanout-endpoint-targeted-refused); otherwise the definition is stored and
/// then distributed ([`distribution::distribute`]).
async fn store(
    federation: &Federation,
    definitions: &Arc<Definitions>,
    matched: &RouteMatch,
    arrived: &Arrived<'_>,
) -> Result<Response, Refused> {
    let started = Instant::now();
    let logged = arrived.outbound.to_string();
    let ids = (arrived.request_id, logged.as_str());
    if let Some(response) =
        request::unsupported_media(matched, (arrived.headers, &arrived.body), ids)
    {
        return Ok(response);
    }
    let name = name(matched)?;
    let version = segment(matched, VERSION_PARAM)
        .unwrap_or_default()
        .parse::<QueryVersion>()
        .map_err(|refused| Refused::with(Code::QueryVersionInvalid, &refused))?;
    query_string(matched, arrived)?;
    let distributed = distribution::requested(federation, arrived.headers)?;
    let text =
        std::str::from_utf8(&arrived.body).map_err(|_text| Refused::fixed(Code::BodyInvalid))?;
    let admitted = Definition::admit(text, federation.context()).map_err(|refusal| {
        security::refused(&refusal, &logged);
        Refused::with(Code::Refused((&refusal).into()), &refusal)
    })?;
    if let Some(SubjectOrigin::Literal { at }) = admitted.subject() {
        security::definition_refused(at.as_ref(), &logged);
        return Err(Refused::fixed(Code::SubjectLiteral));
    }
    if distributed.is_some() {
        distribution::distributable(admitted.aql(), &logged)?;
    }
    let stored = StoredDefinition::new(name, version, admitted.aql().to_owned(), Timestamp::now());
    let copy = stored.clone();
    let held = Arc::clone(definitions);
    // NOTE: no specification governs this: our own design; the insert commits
    // to disk, so it runs where a blocking call may wait.
    let inserted = tokio::task::spawn_blocking(move || held.insert(stored)).await;
    match inserted {
        Ok(Ok(Insertion::Stored)) => match distributed {
            Some(selected) => {
                let sent = (Registry::Stored, &selected);
                let sent_as = (arrived.outbound, &arrived.conveyance, started);
                distribution::distribute(federation, &copy, sent, sent_as).await
            }
            None => stored_answer(version),
        },
        Ok(Ok(Insertion::Held)) => Err(Refused::fixed(Code::StoredQueryHeld)),
        Ok(Err(failure)) => {
            tracing::error!(
                error = crate::chain(&failure),
                request_id = logged,
                "a stored query could not be stored"
            );
            Err(Refused::fixed(Code::Internal))
        }
        Err(failure) => {
            tracing::error!(
                error = %failure,
                request_id = logged,
                "the stored-query insert did not complete"
            );
            Err(Refused::fixed(Code::Internal))
        }
    }
}

/// The operator's action on the admin listener that sends the registry's
/// held copy of `name` at `version` again to the members the targeting
/// `headers` name, and changes nothing at the registry (§12.7
/// stored-query-drift).
///
/// The answer has the shape and statuses of a first distribution, with
/// `meta.registry` saying `held` ([`distribution::distribute`]). The action
/// takes no `body`. It is refused, before any node is asked, with:
///
/// - `405` (`stored-query-read-only`) and an empty `Allow` at a read-only
///   registry, whose copies its operator publishes (RFC 9110 §10.2.1);
/// - `400` for a name or a version outside ITS-REST's forms, a body, a
///   deployment that offers no distribution, a request naming no member
///   (`target-required`), and a definition carrying a `FROM ENDPOINT` or
///   `ORGANISATION` directive (§12.7 fanout-endpoint-targeted-refused);
/// - `404` for a version the registry does not hold, or no registry.
// NOTE: no specification governs this: our own design; §12.7 gives drift repair no
// request, and a second PUT is refused, so the repair is an operator action.
pub(crate) async fn distribute_held(
    state: &AppState,
    (name, version): (&str, &str),
    (headers, body): (&HeaderMap, &[u8]),
) -> Response {
    let request_id = request_id::of(headers).unwrap_or_default();
    let Some(definitions) = state.definitions() else {
        return error::fixed(Code::StoredQueryUnknown, request_id);
    };
    if definitions.is_read_only() {
        let mut response = error::fixed(Code::StoredQueryReadOnly, request_id);
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static(""));
        return response;
    }
    let Some(federation) = state.federation() else {
        return error::fixed(Code::NotImplemented, request_id);
    };
    let at = (name, version);
    redistributed(&federation, definitions, at, (headers, body))
        .await
        .unwrap_or_else(|refused| refused.respond(request_id))
}

/// The distribution [`distribute_held`] answers, or its refusal.
async fn redistributed(
    federation: &Federation,
    definitions: &Arc<Definitions>,
    (name, version): (&str, &str),
    (headers, body): (&HeaderMap, &[u8]),
) -> Result<Response, Refused> {
    let started = Instant::now();
    let outbound = OutboundId::mint();
    let logged = outbound.to_string();
    if !body.is_empty() {
        return Err(Refused::with(Code::BodyInvalid, &HELD_TAKES_NO_BODY));
    }
    let name =
        QueryName::new(name).map_err(|refused| Refused::with(Code::QueryNameInvalid, &refused))?;
    let version = version
        .parse::<QueryVersion>()
        .map_err(|refused| Refused::with(Code::QueryVersionInvalid, &refused))?;
    if !federation.fans_out_stored_queries() {
        return Err(Refused::fixed(Code::StoredQueryFanOutUnsupported));
    }
    let selected = distribution::requested(federation, headers)?
        .ok_or_else(|| Refused::with(Code::TargetRequired, &HELD_NAMES_MEMBERS))?;
    refreshed(definitions, Some(&name), &outbound).await?;
    let held = definitions
        .find(&name, Some(&VersionPattern::Exact(version)))
        .ok_or_else(|| Refused::fixed(Code::StoredQueryUnknown))?;
    distribution::distributable(held.aql(), &logged)?;
    let sent = (Registry::Held, &selected);
    let conveyance = crate::conveyed::gateway(federation).map_err(|unconveyed| {
        tracing::error!(
            error = %unconveyed,
            request_id = logged,
            "the held stored query could not convey the gateway, so nothing was sent"
        );
        Refused::fixed(Code::Internal)
    })?;
    distribution::distribute(federation, &held, sent, (outbound, &conveyance, started)).await
}

/// The `200` of a stored definition, with `Location` relative to the request
/// path, which ends in the version.
fn stored_answer(version: QueryVersion) -> Result<Response, Refused> {
    let location = HeaderValue::try_from(version.to_string()).map_err(|_invalid| {
        tracing::error!("a version is not a valid header value");
        Refused::fixed(Code::Internal)
    })?;
    Ok((StatusCode::OK, [(header::LOCATION, location)]).into_response())
}

/// The ITS-REST `StoredQuery` of `definition`.
fn its_rest(definition: &StoredDefinition) -> StoredQuery {
    StoredQuery {
        name: definition.name().as_str().to_owned(),
        r#type: AQL.to_owned(),
        version: definition.version().to_string(),
        saved: definition.saved().to_string(),
        q: definition.aql().to_owned(),
        additional_properties: BTreeMap::new(),
    }
}

/// `GET {base}/v1/definition/query/{name}/{version}`: the definition the
/// version selects, or `404` (ITS-REST Definition API), with a per-member
/// drift report where the request names members and the deployment offers
/// it ([`distribution::drift`]).
async fn read(
    federation: &Federation,
    definitions: &Arc<Definitions>,
    matched: &RouteMatch,
    arrived: &Arrived<'_>,
) -> Result<Response, Refused> {
    let name = name(matched)?;
    let pattern = pattern(matched)?;
    let checked = distribution::requested(federation, arrived.headers)?;
    refreshed(definitions, Some(&name), &arrived.outbound).await?;
    let definition = definitions
        .find(&name, pattern.as_ref())
        .ok_or_else(|| Refused::fixed(Code::StoredQueryUnknown))?;
    match checked {
        Some(selected) => distribution::drift(federation, &definition, &selected, arrived).await,
        None => Ok((StatusCode::OK, Json(its_rest(&definition))).into_response()),
    }
}

/// `GET {base}/v1/definition/query/{name}`: every version of every
/// definition whose name starts with the segment (ITS-REST Definition API,
/// `definition_query_list`).
async fn list(
    definitions: &Arc<Definitions>,
    matched: &RouteMatch,
    arrived: &Arrived<'_>,
) -> Result<Response, Refused> {
    let pattern = segment(matched, NAME_PARAM)
        .filter(|pattern| {
            pattern
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ':'))
        })
        .ok_or_else(|| Refused::fixed(Code::QueryNameInvalid))?;
    refreshed(definitions, None, &arrived.outbound).await?;
    let listed: Vec<StoredQuery> = definitions
        .list(&pattern)
        .iter()
        .map(|definition| its_rest(definition))
        .collect();
    Ok((StatusCode::OK, Json(listed)).into_response())
}

/// `GET` or `POST {base}/v1/query/{name}[/{version}]`: the stored query, run
/// as if its AQL were submitted inline with the request's members (§12.7,
/// N44, N1).
///
/// A `POST` whose `Content-Type` names no media type the operation lists is
/// a `415`, answered before the definition is looked up.
async fn execute(
    federation: &Federation,
    definitions: &Arc<Definitions>,
    (matched, carrier): (&RouteMatch, Carrier),
    arrived: Arrived<'_>,
) -> Result<Response, Refused> {
    let started = Instant::now();
    if carrier == Carrier::Body {
        let logged = arrived.outbound.to_string();
        let ids = (arrived.request_id, logged.as_str());
        if let Some(response) =
            request::unsupported_media(matched, (arrived.headers, &arrived.body), ids)
        {
            return Ok(response);
        }
    }
    let name = name(matched)?;
    let pattern = pattern(matched)?;
    refreshed(definitions, Some(&name), &arrived.outbound).await?;
    let definition = definitions
        .find(&name, pattern.as_ref())
        .ok_or_else(|| Refused::fixed(Code::StoredQueryUnknown))?;
    let members = members(matched, &arrived, carrier)?;
    let request = AdhocQueryExecute {
        q: definition.aql().to_owned(),
        offset: members.offset,
        fetch: members.fetch,
        query_parameters: members.query_parameters,
        additional_properties: BTreeMap::new(),
    };
    let query = request::Arrived {
        headers: arrived.headers,
        request_id: arrived.request_id,
        outbound: arrived.outbound,
        conveyance: &arrived.conveyance,
        started,
    };
    let submitted = Submitted::Stored {
        request,
        name: definition.name().as_str(),
    };
    Ok(answer::answer(federation, query, submitted).await)
}

#[cfg(test)]
mod tests {
    use super::serves;
    use http::Method;
    use openehr_its::rest::routes::{Lookup, lookup};

    #[test]
    fn the_registry_answers_every_stored_query_definition_operation() {
        // NOTE: §12.7 registry-authoritative, N44: where the registry is offered,
        // no stored-query definition request falls through to a node.
        for (method, path) in [
            (Method::GET, "/definition/query/org::q"),
            (Method::PUT, "/definition/query/org::q"),
            (Method::GET, "/definition/query/org::q/1.0.0"),
            (Method::PUT, "/definition/query/org::q/1.0.0"),
        ] {
            let Lookup::Matched(matched) = lookup(&method, path) else {
                panic!("ITS-REST declares {method} {path}");
            };
            assert!(serves(&matched), "{method} {path}");
        }
    }
}

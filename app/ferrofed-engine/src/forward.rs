// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Single-node forwarding: one client request passed to one node once, and
//! the node's answer returned as the node sent it (§7a.1, §7a.3, N22, N31).
//!
//! The request leaves through `openehr-its`'s `Client::forward`, which sends
//! it once and classifies nothing. The method, the path and the body bytes are
//! the client's, unchanged: a commit body is clinical content the gateway has
//! no right to alter (§5.4 scope note, N33). Of the client's headers and query
//! string, only what the ITS-REST operation the method and path address
//! declares travels, as the outbound gate admits it
//! ([`hygiene::forwarded_headers`], [`hygiene::forwarded_query`]). Each path
//! identifier and each value that travels matches what the operation
//! declares for it, and `Accept`, `Content-Type` and `Prefer` travel as values
//! the operation lists ([`declared::held`]); a body never travels without a
//! `Content-Type` its operation's body is declared in. A [`HeldRequest`]
//! carries those headers, composed once, so a route that reads them before
//! sending sends the same ones. The
//! endpoint's onward credentials set `Authorization`, and the request's
//! minted [`OutboundId`](crate::outbound_id::OutboundId) sets `X-Request-Id`,
//! and the caller's identity, signed for the node, sets
//! [`openEHR-federation-client`](crate::onward::conveyance::HEADER).
//!
//! The answer keeps its status, its body bytes, `Location` and `ETag`; only
//! the hop-by-hop fields are removed (RFC 9110 §7.6.1). A node's `404` or
//! `500` is a [`Forwarded`] answer like any other (§11.2). A `401` is the node
//! refusing the gateway's own onward credentials, so it is
//! [`ForwardError::Refused`], carrying the node's status and body, and never
//! a challenge to the client's credentials.

use std::fmt;

use crate::declared::{self, Refusal};
use crate::dispatch::reported;
use crate::dispatch::{DispatchOptions, NodeClient, OptionsError};
use crate::hygiene::{self, Composed, Outbound, Part, UnlistedParameter};
use crate::onward::conveyance::ConveyanceError;
use ferrofed_registry::id::EndpointId;
use http::header::{CONNECTION, CONTENT_LENGTH, TE, TRAILER, TRANSFER_ENCODING, UPGRADE};
use http::{HeaderMap, HeaderName, Method, StatusCode};
use openehr_federation::outcome::ErrorDetail;
use openehr_its::rest::client::{
    ClientError, ErrorBody, Request, Transport, TransportError, path_segment,
};
use openehr_its::rest::routes::{self, Lookup, ParamLocation, RouteMatch};

/// The hop-by-hop fields RFC 9110 §7.6.1 names besides `Connection` itself,
/// which an intermediary never forwards.
const HOP_BY_HOP: [&str; 2] = ["keep-alive", "proxy-connection"];

/// A client request to forward to one node, as the client sent it.
///
/// `Debug` shows the method and the sizes, never the path, a header or the
/// body.
pub struct ClientRequest {
    /// The request method.
    pub method: Method,
    /// The path relative to the ITS-REST base, as received and still
    /// percent-encoded (`/ehr/7d44…/composition`).
    pub path: String,
    /// The query string as received, without its `?`.
    pub query: Option<String>,
    /// Every header the client sent.
    pub headers: HeaderMap,
    /// The body bytes the client sent.
    pub body: Vec<u8>,
}

impl fmt::Debug for ClientRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientRequest")
            .field("method", &self.method)
            .field("headers", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// What the node answered a forwarded request, as it answered it, less the
/// hop-by-hop fields.
///
/// `Debug` shows the status and the sizes, never a header value or the body.
pub struct Forwarded {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Forwarded {
    /// The node's status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The node's headers, `Location` and `ETag` untouched.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The node's body bytes.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The status, the headers and the body.
    #[must_use]
    pub fn into_parts(self) -> (StatusCode, HeaderMap, Vec<u8>) {
        (self.status, self.headers, self.body)
    }
}

impl fmt::Debug for Forwarded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Forwarded")
            .field("status", &self.status)
            .field("headers", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// A forwarded request that has no answer of the node's to pass on.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ForwardError {
    /// The client's query string carries a parameter the route does not
    /// forward, so nothing was sent (§5.4.1, N33).
    #[error(transparent)]
    QueryParameter(#[from] UnlistedParameter),
    /// A path, query or header value the operation declares does not match
    /// what it declares for it, or `Accept` or `Content-Type` names no media
    /// type it lists, so nothing was sent (§5.4.1, N33).
    #[error(transparent)]
    Value(#[from] Refusal),
    /// The outbound gate found a withheld patient identifier in the request,
    /// so nothing was sent (§5.4.1, N33).
    #[error(
        "the request to endpoint {endpoint} would carry a patient identifier in {part}, so it was not sent"
    )]
    Withheld {
        /// The endpoint.
        endpoint: EndpointId,
        /// The part of the request that carried it; never the value.
        part: Part,
    },
    /// No onward credential could be obtained for the endpoint, so nothing
    /// was sent (§13.1, N25).
    #[error("no onward credential could be obtained for endpoint {endpoint}")]
    Credentials {
        /// The endpoint.
        endpoint: EndpointId,
        /// The `error` the endpoint is reported with: a fixed sentence and,
        /// when the token endpoint refused with one, its registered RFC 6749
        /// §5.2 code ([`reported::unauthenticated`]).
        error: ErrorDetail,
        /// What the client runtime reported.
        #[source]
        source: Box<ClientError>,
    },
    /// The request could not be composed from its parts.
    #[error("the request to endpoint {endpoint} could not be composed")]
    Compose {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the client runtime reported.
        #[source]
        source: Box<ClientError>,
    },
    /// The caller's identity could not be signed for the node, so nothing
    /// was sent (§13.1, N24).
    #[error("the caller's identity could not be conveyed to endpoint {endpoint}")]
    Conveyance {
        /// The endpoint.
        endpoint: EndpointId,
        /// Why it could not be signed.
        #[source]
        source: ConveyanceError,
    },
    /// The deadline passed before the request left the gateway, so nothing
    /// was sent and the node was never asked (§11.5).
    #[error("the deadline for endpoint {endpoint} passed before the request was sent")]
    Expired {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the client runtime reported.
        #[source]
        source: Box<ClientError>,
    },
    /// The node was sent the request and did not answer before the deadline
    /// (§11.2).
    #[error("endpoint {endpoint} did not answer before the deadline")]
    TimeOut {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the client runtime reported.
        #[source]
        source: Box<ClientError>,
    },
    /// The node could not be reached: a refused connection or a broken
    /// stream (§11.2).
    #[error("endpoint {endpoint} could not be reached")]
    Unreachable {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the client runtime reported.
        #[source]
        source: Box<ClientError>,
    },
    /// The node refused the gateway's onward credentials.
    #[error("endpoint {endpoint} refused the gateway's onward credentials with {status}")]
    Refused {
        /// The endpoint.
        endpoint: EndpointId,
        /// The node's status.
        status: StatusCode,
        /// The node's body, as received.
        body: ErrorBody,
    },
    /// The method and path address no ITS-REST operation, so nothing names
    /// what the request may carry and nothing was sent.
    #[error("the request addresses no ITS-REST operation, so it was not sent")]
    Unrouted,
}

/// How a request's declared values are held to its operation:
/// [`declared::held`] or [`declared::fitting`].
type Compose = fn(&RouteMatch, Option<&str>, &HeaderMap, &[u8]) -> Result<HeaderMap, Refusal>;

/// A client request held to the ITS-REST operation its method and path
/// address, with the headers composed for the node once
/// ([`declared::held`]).
///
/// Only [`HeldRequest::hold`] builds one, so a held request's query string
/// names only parameters its operation declares, and its headers are the
/// ones the operation declares, each held to its kind. `Debug` shows the
/// method and the sizes, never the path, a header or the body.
#[derive(Clone)]
pub struct HeldRequest {
    method: Method,
    path: String,
    query: Option<String>,
    operation: RouteMatch,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl HeldRequest {
    /// Holds `request` to the ITS-REST operation its method and path
    /// address, and composes the headers the node receives.
    ///
    /// # Errors
    ///
    /// Returns [`ForwardError::Unrouted`] when the method and path address no
    /// operation, [`ForwardError::QueryParameter`] for a query parameter the
    /// operation does not declare or a route never forwards, and
    /// [`ForwardError::Value`] for a declared value that does not match its
    /// kind, or an `Accept` or `Content-Type` naming no listed media type.
    pub fn hold(request: ClientRequest) -> Result<Self, ForwardError> {
        Self::composed(request, declared::held)
    }

    /// Holds the gateway's own `request` to the ITS-REST operation its
    /// method and path address, leaving out a header that does not fit it
    /// ([`declared::fitting`]); the ask-all probe is held this way.
    ///
    /// # Errors
    ///
    /// Returns the [`ForwardError`] of [`HeldRequest::hold`], never one for a
    /// header.
    pub fn fit(request: ClientRequest) -> Result<Self, ForwardError> {
        Self::composed(request, declared::fitting)
    }

    /// Holds `request` to its operation, its headers composed by `compose`.
    fn composed(request: ClientRequest, compose: Compose) -> Result<Self, ForwardError> {
        let ClientRequest {
            method,
            path,
            query,
            headers,
            body,
        } = request;
        let Lookup::Matched(operation) = routes::lookup(&method, &path) else {
            return Err(ForwardError::Unrouted);
        };
        if let Some(query) = query.as_deref() {
            hygiene::forwarded_query(&operation, query)?;
        }
        let headers = compose(&operation, query.as_deref(), &headers, &body)?;
        Ok(Self {
            method,
            path,
            query,
            operation,
            headers,
            body,
        })
    }

    /// The headers composed for the node: each one the operation declares,
    /// held to its kind, with `Accept`, `Content-Type` and `Prefer` as listed
    /// values ([`declared::held`]).
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
}

impl fmt::Debug for HeldRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeldRequest")
            .field("method", &self.method)
            .field("operation", &self.operation.operation_id)
            .field("headers", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

impl<T: Transport> NodeClient<T> {
    /// Forwards `request` to the node once and returns its answer.
    ///
    /// The ITS-REST operation the method and path address decides which of
    /// the client's headers and query parameters travel, and each must match
    /// the kind the operation declares for it ([`HeldRequest::hold`]); the
    /// held request then leaves as [`NodeClient::forward_held`] sends it.
    ///
    /// # Errors
    ///
    /// Returns the [`ForwardError`] of [`HeldRequest::hold`] with nothing
    /// sent, and otherwise that of [`NodeClient::forward_held`].
    pub async fn forward(
        &self,
        request: ClientRequest,
        options: &DispatchOptions,
    ) -> Result<Forwarded, ForwardError> {
        self.forward_held(HeldRequest::hold(request)?, options)
            .await
    }

    /// Forwards the held `request` to the node once and returns its answer.
    ///
    /// The headers travel as they were composed when the request was held.
    /// The deadline and the request id of `options` apply, and the outbound
    /// gate reads the URL and every forwarded header against the identifiers
    /// `options` withholds (§5.4.1, N33).
    ///
    /// # Errors
    ///
    /// Returns [`ForwardError::Withheld`] with nothing sent,
    /// [`ForwardError::Credentials`] and [`ForwardError::Compose`] when the
    /// request could not leave, [`ForwardError::Expired`] when the deadline
    /// passed before it left, [`ForwardError::TimeOut`] and
    /// [`ForwardError::Unreachable`] when the node gave no answer, and
    /// [`ForwardError::Refused`] when it answered `401`.
    pub async fn forward_held(
        &self,
        request: HeldRequest,
        options: &DispatchOptions,
    ) -> Result<Forwarded, ForwardError> {
        let HeldRequest {
            method,
            path,
            query,
            operation,
            headers,
            body,
        } = request;
        let mut outgoing = Request::new(method, path);
        if let Some(query) = query.as_deref() {
            outgoing.raw_query(query);
        }
        outgoing.headers_mut().extend(headers);
        let call = options
            .call_options(self.endpoint())
            .map_err(|error| match error {
                OptionsError::Conveyance(source) => ForwardError::Conveyance {
                    endpoint: self.endpoint().clone(),
                    source,
                },
                OptionsError::Client(source) => ForwardError::Compose {
                    endpoint: self.endpoint().clone(),
                    source: Box::new(source),
                },
            })?;
        outgoing.apply_options(&call);
        if !body.is_empty() {
            outgoing.raw_body(body, None);
        }
        self.gate_forward(&operation, &outgoing, options)?;
        match self.client().forward(outgoing).await {
            Ok(answer) if answer.status() == StatusCode::UNAUTHORIZED => {
                Err(ForwardError::Refused {
                    endpoint: self.endpoint().clone(),
                    status: answer.status(),
                    body: answer.error_body(),
                })
            }
            Ok(answer) => {
                let status = answer.status();
                let mut headers = answer.headers().clone();
                strip_hop_by_hop(&mut headers);
                Ok(Forwarded {
                    status,
                    headers,
                    body: answer.into_body(),
                })
            }
            Err(error) => Err(self.unanswered(error, options)),
        }
    }

    /// The outbound gate over a forwarded request: its URL, the caller claims
    /// it conveys, and every header `operation` declares that the request
    /// carries (§5.4.1, N33).
    ///
    /// Those are all the headers [`hygiene::forwarded_headers`] admits. The
    /// minted `X-Request-Id` is exempt as in every node request
    /// ([`crate::hygiene`]), and a `Content-Type` the operation declares no
    /// parameter for is one of its listed media types, never the client's
    /// text.
    fn gate_forward(
        &self,
        operation: &RouteMatch,
        request: &Request,
        options: &DispatchOptions,
    ) -> Result<(), ForwardError> {
        let withheld = options.withheld();
        if withheld.is_empty() {
            return Ok(());
        }
        let base = self.base();
        let mut url = base.clone();
        url.set_path(&format!(
            "{}{}",
            base.path().trim_end_matches('/'),
            request.path()
        ));
        let query = request.query_string();
        url.set_query((!query.is_empty()).then_some(query));
        let mut sent: Vec<(&'static str, String)> = Vec::new();
        let declared = operation
            .params
            .iter()
            .filter(|param| param.location == ParamLocation::Header);
        for param in declared {
            for value in request.headers().get_all(param.name) {
                let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
                sent.push((param.name, value));
            }
        }
        let headers: Vec<(&'static str, &str)> = sent
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let conveyed = options.conveyance().carried();
        let ehr_prefix = format!("{}/ehr/", base.path().trim_end_matches('/'));
        let segment = options
            .composed_ehr_id()
            .map(|ehr_id| path_segment(&ehr_id.as_str()));
        let outbound = Outbound {
            aql: "",
            scope: None,
            paging: &[],
            url: &url,
            composed: Composed {
                ehr_prefix: &ehr_prefix,
                ehr_segment: segment.as_deref(),
            },
            headers: &headers,
            conveyed: &conveyed,
        };
        match withheld.found_in(&outbound) {
            Some(part) => Err(ForwardError::Withheld {
                endpoint: self.endpoint().clone(),
                part,
            }),
            None => Ok(()),
        }
    }

    /// The error for a forwarded request that reached no answer.
    ///
    /// `Client::forward` sends once and raises `DeadlineElapsed` only before
    /// the request is handed to the transport, so it is a request never sent.
    fn unanswered(&self, error: ClientError, options: &DispatchOptions) -> ForwardError {
        let endpoint = self.endpoint().clone();
        match error {
            ClientError::DeadlineElapsed { .. } => ForwardError::Expired {
                endpoint,
                source: Box::new(error),
            },
            ClientError::Transport {
                source: TransportError::Timeout { .. },
                ..
            } => ForwardError::TimeOut {
                endpoint,
                source: Box::new(error),
            },
            ClientError::Transport { .. } => ForwardError::Unreachable {
                endpoint,
                source: Box::new(error),
            },
            ClientError::Credentials { ref source, .. } => ForwardError::Credentials {
                error: reported::unauthenticated(source, &endpoint, options.request_id()),
                endpoint,
                source: Box::new(error),
            },
            other => ForwardError::Compose {
                endpoint,
                source: Box::new(other),
            },
        }
    }
}

/// Removes the hop-by-hop fields of `headers`: `Connection`, every field it
/// names, and the fields RFC 9110 §7.6.1 lists, plus `Content-Length`, which
/// the gateway's own framing sets for the bytes it sends.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|option| HeaderName::from_bytes(option.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        CONNECTION,
        TE,
        TRAILER,
        TRANSFER_ENCODING,
        UPGRADE,
        CONTENT_LENGTH,
    ] {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::strip_hop_by_hop;
    use http::HeaderMap;

    #[test]
    fn the_hop_by_hop_fields_go_and_location_and_etag_stay() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", "keep-alive, x-node-hop".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("x-node-hop", "1".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("content-length", "12".parse().unwrap());
        headers.insert(
            "location",
            "https://cdr-a.example.org/v1/ehr/e/composition/u::cdr-a.example.org::1"
                .parse()
                .unwrap(),
        );
        headers.insert("etag", "\"u::cdr-a.example.org::1\"".parse().unwrap());
        strip_hop_by_hop(&mut headers);
        let left: Vec<&str> = headers.keys().map(http::HeaderName::as_str).collect();
        assert_eq!(vec!["location", "etag"], left);
    }
}

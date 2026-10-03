// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Node dispatch: one ITS-REST client per registry endpoint, and the mapping
//! from what a node answered to exactly one §11.1 endpoint status (N16, N40).
//!
//! Every query to a node is the generated `query_execute_adhoc_query_body`
//! (`POST {base}/v1/query/aql`) of `openehr-its`'s `rest-client`, and every
//! stored-query definition sent to or read from a node is a generated call
//! too ([`definition`]), so the request line, the headers and the body are
//! composed by that runtime and nowhere in FerroFED (no specification governs
//! this: our own design). This
//! module adds the per-endpoint client, the call's deadline, the caller's
//! identity signed for the node ([`crate::onward::conveyance`]) and the gateway's
//! [`OutboundId`], and the classification of the answer. Every header a node
//! request carries is listed in [`crate::outbound_id`]:
//!
//! | The node | Status |
//! |---|---|
//! | answered `200` with a result set | `active` |
//! | was not reachable: a refused connection or a broken stream | `offline` |
//! | did not answer before the deadline | `time-out` |
//! | answered `403` with an ITS-REST `Error` whose `code` is one of the endpoint's consent refusal codes in the registry | `consent-denied`, with its latency |
//! | answered with any other failure: a documented error, an undocumented status, a body that is not a result set, rows shorter than the query selects | `node-error` |
//!
//! ITS-REST defines no consent signal, so a refusal is `consent-denied` only
//! where the registry names the code the node marks it with (§11.1, N27; no
//! specification governs the code: our own design). A `consent-denied` node
//! fails nothing under either completion strategy (§11.3).
//!
//! A `node-error` carries the node's own status and an excerpt of its
//! message ([`reported`], §9.5, §11.2), never folded into `offline`; a
//! refused connection still carries its reason. An onward credential that
//! could not be obtained is a `node-error` carrying the token endpoint's
//! error, with nothing sent ([`reported::unauthenticated`]; §13.1, N25). Any
//! other failure on the gateway's side before the request left (a body that
//! would not serialize) is a [`DispatchError`], never an endpoint status:
//! nothing was sent to report on.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use crate::ehr::EhrCallError;
use crate::forward::{ForwardError, Forwarded};
use crate::hygiene::{Part, Withheld};
use crate::onward::conveyance::{self, Conveyance, ConveyanceError};
use crate::outbound_id::OutboundId;
use ferrofed_registry::id::{EhrId, EndpointId};
use ferrofed_registry::snapshot::{Endpoint, RegistrySnapshot};
use http::StatusCode;
use openehr_base::v1_3::base_types::identification::hier_object_id::HierObjectId;
use openehr_federation::outcome::Outcome;
use openehr_federation::status::EndpointStatus;
use openehr_its::rest::client::{
    CallOptions, Client, ClientError, CredentialsProvider, RetryPolicy, Transport,
};
use openehr_its::rest::generated::query::client::QueryClient;
use openehr_its::rest::generated::query::{
    AdhocQueryExecute, QueryExecuteAdhocQueryBodyParams, ResultSet,
};
use url::Url;

mod classify;
pub mod definition;
mod gate;
pub mod reported;

/// The API version segment ITS-REST 1.1.0 puts every path under
/// (`{baseUrl}/v1/...`), appended to the endpoint's base URL.
pub const API_VERSION_SEGMENT: &str = "v1";

/// The header that carries the gateway's [`OutboundId`] to a node.
pub const REQUEST_ID_HEADER: &str = "X-Request-Id";

/// A shared credentials provider for one endpoint's onward grant.
pub type SharedCredentials = Arc<dyn CredentialsProvider>;

/// The query one node receives: standard AQL already scoped to that node's
/// own `ehr_id` by the rewrite (§7.1), and the page the gateway asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeQuery {
    aql: String,
    offset: Option<u32>,
    fetch: Option<u32>,
    scope: Option<String>,
    width: usize,
}

impl NodeQuery {
    /// A query of `aql` with no page bounds.
    #[must_use]
    pub fn new(aql: impl Into<String>) -> Self {
        Self {
            aql: aql.into(),
            offset: None,
            fetch: None,
            scope: None,
            width: 0,
        }
    }

    /// This query reading `width` cells of every row the node answers.
    ///
    /// A node row with fewer cells is an answer the gateway cannot use, so
    /// the endpoint is `node-error` (§11.1).
    #[must_use]
    pub fn with_width(mut self, width: usize) -> Self {
        self.width = width;
        self
    }

    /// This query as scoped by the rewrite to the node's own `ehr_id`, which
    /// the outbound gate reads past (§7.1).
    #[must_use]
    pub fn with_scope(mut self, ehr_id: &HierObjectId) -> Self {
        self.scope = Some(ehr_id.value().to_owned());
        self
    }

    /// This query starting at row `offset` of the node's answer.
    #[must_use]
    pub fn with_offset(mut self, offset: u32) -> Self {
        self.offset = Some(offset);
        self
    }

    /// This query asking the node for at most `fetch` rows.
    #[must_use]
    pub fn with_fetch(mut self, fetch: u32) -> Self {
        self.fetch = Some(fetch);
        self
    }

    /// The AQL text the node receives.
    #[must_use]
    pub fn aql(&self) -> &str {
        &self.aql
    }

    /// The ITS-REST request body for this query.
    fn body(&self) -> AdhocQueryExecute {
        AdhocQueryExecute {
            q: self.aql.clone(),
            offset: self.offset.map(i64::from),
            fetch: self.fetch.map(i64::from),
            query_parameters: None,
            additional_properties: BTreeMap::new(),
        }
    }
}

/// The per-call options of one dispatch: the instant the node must have
/// answered by, the identity the request conveys, the gateway's
/// [`OutboundId`], and the identifiers no request may carry.
#[derive(Debug, Clone)]
pub struct DispatchOptions {
    deadline: Instant,
    conveyance: Conveyance,
    request_id: Option<OutboundId>,
    withheld: Arc<Withheld>,
    composed_ehr_id: Option<EhrId>,
}

impl DispatchOptions {
    /// Options with `deadline` as the instant the node must have answered by
    /// (§11.5), every request conveying `conveyance` in
    /// [`conveyance::HEADER`] (§13.1, N24, N25).
    ///
    /// There is no dispatch without a conveyance: a request reaches a node
    /// only on behalf of a verified caller or of the gateway itself.
    #[must_use]
    pub fn new(deadline: Instant, conveyance: Conveyance) -> Self {
        Self {
            deadline,
            conveyance,
            request_id: None,
            withheld: Arc::new(Withheld::none()),
            composed_ehr_id: None,
        }
    }

    /// These options naming the node's own `ehr_id`, which the gateway
    /// composed into a forwarded request's path as `/ehr/{ehr_id}` from a
    /// resolution or the `ehr_id` index, never from the client.
    ///
    /// The outbound gate masks that one path segment, as it masks the scope
    /// literal of a dispatched query (§5.4.1, N33).
    #[must_use]
    pub fn with_composed_ehr_id(mut self, ehr_id: EhrId) -> Self {
        self.composed_ehr_id = Some(ehr_id);
        self
    }

    /// These options refusing to send a request that carries one of the
    /// identifiers `withheld` (§5.4.1, N33).
    #[must_use]
    pub fn with_withheld(mut self, withheld: Arc<Withheld>) -> Self {
        self.withheld = withheld;
        self
    }

    /// These options sending `request_id` to the node in
    /// [`REQUEST_ID_HEADER`].
    ///
    /// The id is one the gateway minted, never a client value, so no client
    /// text reaches a node in this header (§5.4.1, N33).
    #[must_use]
    pub fn with_request_id(mut self, request_id: OutboundId) -> Self {
        self.request_id = Some(request_id);
        self
    }

    /// The instant the node must have answered by.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The identifiers no request may carry.
    pub(crate) fn withheld(&self) -> &Withheld {
        &self.withheld
    }

    /// The gateway's id of the request, when the caller passed one.
    pub(crate) fn request_id(&self) -> Option<&OutboundId> {
        self.request_id.as_ref()
    }

    /// The `ehr_id` the gateway composed into a forwarded request's path.
    pub(crate) fn composed_ehr_id(&self) -> Option<&EhrId> {
        self.composed_ehr_id.as_ref()
    }

    /// The identity every request under these options conveys.
    #[must_use]
    pub fn conveyance(&self) -> &Conveyance {
        &self.conveyance
    }

    /// The `openehr-its` call options for these options toward `endpoint`:
    /// the deadline, the [`conveyance::HEADER`] signed for that endpoint,
    /// and the request id.
    pub(crate) fn call_options(&self, endpoint: &EndpointId) -> Result<CallOptions, OptionsError> {
        let conveyed = self.conveyance.signed_for(endpoint)?;
        let options = CallOptions::default()
            .with_deadline(self.deadline)
            .with_header(conveyance::HEADER, &conveyed)?;
        Ok(match self.request_id {
            Some(id) => options.with_header(REQUEST_ID_HEADER, &id.to_string())?,
            None => options,
        })
    }
}

/// Why the call options of one request could not be made, so nothing was
/// sent.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OptionsError {
    /// The caller's identity could not be signed for the node.
    #[error(transparent)]
    Conveyance(#[from] ConveyanceError),
    /// A header the options carry is not legal on the wire.
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// What one request showed of the node beside the §11.1 outcome it reports:
/// whether it left the gateway, and the node's own HTTP status where the node
/// answered.
///
/// The §11.1 record carries a node's status only as text in its `error`, so
/// this keeps it typed for the gateway's own health and metrics surfaces. No
/// specification governs those surfaces: our own design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contact {
    /// The request never left the gateway.
    Unsent,
    /// The node answered with this HTTP status.
    Answered(StatusCode),
    /// The request left and the node gave no answer: it timed out, could not
    /// be reached, or was abandoned when the overall budget ran out.
    Silent,
}

impl Contact {
    /// Returns what a forwarded request's outcome showed of the node.
    ///
    /// A refusal of the gateway's onward credentials is the node's answer,
    /// and a deadline that passed before the request left is
    /// [`Contact::Unsent`], as every other failure on the gateway's side is.
    #[must_use]
    pub fn of_forwarded(outcome: &Result<Forwarded, ForwardError>) -> Self {
        match outcome {
            Ok(answer) => Self::Answered(answer.status()),
            Err(error) => Self::of_forward_error(error),
        }
    }

    /// Returns what a forwarded request that got no answer of its own
    /// showed of the node, read as [`Contact::of_forwarded`] reads it.
    #[must_use]
    pub fn of_forward_error(error: &ForwardError) -> Self {
        match error {
            ForwardError::Refused { status, .. } => Self::Answered(*status),
            ForwardError::TimeOut { .. } | ForwardError::Unreachable { .. } => Self::Silent,
            ForwardError::QueryParameter(_)
            | ForwardError::Expired { .. }
            | ForwardError::Value(_)
            | ForwardError::Withheld { .. }
            | ForwardError::Credentials { .. }
            | ForwardError::Compose { .. }
            | ForwardError::Conveyance { .. }
            | ForwardError::Unrouted => Self::Unsent,
        }
    }

    /// Returns what an EHR call that got no usable answer showed of the node,
    /// by the same rule: an answer is the node's status, a deadline that
    /// passed before the request left is [`Contact::Unsent`].
    #[must_use]
    pub fn of_ehr_call_error(error: &EhrCallError) -> Self {
        match error {
            EhrCallError::Rejected { status, .. } | EhrCallError::Unnamed { status, .. } => {
                Self::Answered(*status)
            }
            EhrCallError::TimeOut { .. } | EhrCallError::Unreachable { .. } => Self::Silent,
            EhrCallError::Withheld { .. }
            | EhrCallError::Expired { .. }
            | EhrCallError::Conveyance { .. } => Self::Unsent,
            EhrCallError::Failed { source, .. } => Self::of_client_error(source),
        }
    }

    /// Returns what a call that ended in `error` showed of the node: the
    /// status of an answer, [`Contact::Silent`] for a request that left with
    /// no answer, and [`Contact::Unsent`] for one that never left.
    #[must_use]
    pub fn of_client_error(error: &ClientError) -> Self {
        match error {
            ClientError::Unauthorized { .. } => Self::Answered(StatusCode::UNAUTHORIZED),
            ClientError::Forbidden { .. } => Self::Answered(StatusCode::FORBIDDEN),
            ClientError::ServiceFailure { status, .. }
            | ClientError::UndocumentedStatus { status, .. }
            | ClientError::Body { status, .. } => Self::Answered(*status),
            ClientError::Transport { .. } => Self::Silent,
            ClientError::BaseUrl { .. }
            | ClientError::DeadlineElapsed { .. }
            | ClientError::Credentials { .. }
            | ClientError::Build { .. }
            | ClientError::HeaderName { .. }
            | ClientError::InvalidCredentials { .. }
            | ClientError::HeaderValue { .. }
            | ClientError::UnsupportedMediaType { .. }
            | ClientError::Serialize { .. } => Self::Unsent,
        }
    }

    /// Whether the request left the gateway.
    #[must_use]
    pub const fn sent(self) -> bool {
        !matches!(self, Self::Unsent)
    }
}

/// What one node made of one dispatched query.
#[derive(Debug, Clone)]
pub enum NodeReply {
    /// The node answered with a result set: the endpoint is `active`.
    Answered {
        /// The node's answer, as the ITS-REST `RESULT_SET` it sent.
        result_set: Box<ResultSet>,
        /// The gateway's measurement of the request, in milliseconds.
        latency_ms: u64,
    },
    /// The node was asked and gave no result set; the outcome is `offline`,
    /// `time-out` or `node-error`, always with its `error`, or the node's own
    /// `consent-denied`, with its latency (§11.1, N27, N40).
    Failed {
        /// The endpoint outcome, carrying the error and the latency.
        outcome: Outcome,
        /// What the request showed of the node.
        contact: Contact,
    },
}

impl NodeReply {
    /// The endpoint outcome this reply reports in `meta.federation`.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        match self {
            Self::Answered { latency_ms, .. } => Outcome::Active {
                latency_ms: *latency_ms,
            },
            Self::Failed { outcome, .. } => outcome.clone(),
        }
    }

    /// The §11.1 status this reply reports.
    #[must_use]
    pub fn status(&self) -> EndpointStatus {
        match self {
            Self::Answered { .. } => EndpointStatus::Active,
            Self::Failed { outcome, .. } => outcome.status(),
        }
    }

    /// What the request showed of the node: a result set is the node's `200`.
    #[must_use]
    pub fn contact(&self) -> Contact {
        match self {
            Self::Answered { .. } => Contact::Answered(StatusCode::OK),
            Self::Failed { contact, .. } => *contact,
        }
    }
}

/// A client could not be built for an endpoint of the registry snapshot.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SetupError {
    /// The endpoint's base URL cannot carry the ITS-REST paths.
    #[error("the base URL of endpoint {endpoint} cannot carry the ITS-REST paths")]
    BaseUrl {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the client runtime reported.
        #[source]
        source: Box<ClientError>,
    },
    /// Credentials were configured for an endpoint the snapshot does not hold.
    #[error("credentials are configured for {endpoint}, which is not an endpoint of the registry")]
    UnknownEndpoint {
        /// The endpoint id the credentials were keyed by.
        endpoint: EndpointId,
    },
}

/// A dispatch failed on the gateway's side before any request reached the
/// node, so there is no endpoint status to report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DispatchError {
    /// The outbound gate found a withheld patient identifier in the request
    /// the gateway composed, so the request was not sent (§5.4.1, N33).
    #[error(
        "the request to endpoint {endpoint} would carry a patient identifier in {part}, so it was not sent"
    )]
    Withheld {
        /// The endpoint.
        endpoint: EndpointId,
        /// The part of the request that carried it; never the value.
        part: Part,
    },
    /// The request could not be composed: a body that would not serialize, or
    /// another request the client runtime refuses to build.
    #[error("the request to endpoint {endpoint} could not be composed")]
    Compose {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the client runtime reported.
        #[source]
        source: Box<ClientError>,
    },
    /// The caller's identity could not be signed for the node, so the
    /// request was not sent (§13.1, N24).
    #[error("the caller's identity could not be conveyed to endpoint {endpoint}")]
    Conveyance {
        /// The endpoint.
        endpoint: EndpointId,
        /// Why it could not be signed.
        #[source]
        source: ConveyanceError,
    },
}

impl DispatchError {
    /// The error for options toward `endpoint` that could not be made.
    pub(crate) fn of_options(endpoint: &EndpointId, error: OptionsError) -> Self {
        match error {
            OptionsError::Conveyance(source) => Self::Conveyance {
                endpoint: endpoint.clone(),
                source,
            },
            OptionsError::Client(source) => Self::Compose {
                endpoint: endpoint.clone(),
                source: Box::new(source),
            },
        }
    }
}

/// The ITS-REST client of one registry endpoint.
#[derive(Debug, Clone)]
pub struct NodeClient<T> {
    endpoint: EndpointId,
    client: Client<T>,
    consent_refusal_codes: BTreeSet<String>,
}

impl<T: Transport> NodeClient<T> {
    /// The client of `endpoint` over `transport`, rooted at the endpoint's
    /// base URL with the ITS-REST version segment under it.
    ///
    /// The base URL is used as the registry holds it, with no prefix assumed
    /// (N28): `https://cdr.example.org/openehr` addresses
    /// `https://cdr.example.org/openehr/v1/query/aql`. The client sends a
    /// request once; there is no retry inside the client's budget (no
    /// specification governs this: our own design).
    ///
    /// # Errors
    ///
    /// Returns [`SetupError::BaseUrl`] when the base URL cannot carry a path.
    pub fn new(endpoint: &Endpoint, transport: T) -> Result<Self, SetupError> {
        let base = versioned_base(endpoint.url());
        let client = Client::new(transport, base)
            .map_err(|source| SetupError::BaseUrl {
                endpoint: endpoint.id().clone(),
                source: Box::new(source),
            })?
            // NOTE: openehr-its Client::execute (docs.rs) raises DeadlineElapsed before an attempt, so
            // with one attempt it is a request never sent, which every Contact reading here relies on.
            .with_retry(RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            });
        Ok(Self {
            endpoint: endpoint.id().clone(),
            client,
            consent_refusal_codes: endpoint.consent_refusal_codes().clone(),
        })
    }

    /// This client asking `provider` for the onward credentials of every
    /// request.
    #[must_use]
    pub fn with_credentials_provider(mut self, provider: SharedCredentials) -> Self {
        self.client = self.client.with_credentials_provider(provider);
        self
    }

    /// The endpoint this client dispatches to.
    #[must_use]
    pub fn endpoint(&self) -> &EndpointId {
        &self.endpoint
    }

    /// The ITS-REST service root every path is resolved under.
    #[must_use]
    pub fn base(&self) -> &Url {
        self.client.base()
    }

    /// The ITS-REST client every request to the node is sent through.
    pub(crate) fn client(&self) -> &Client<T> {
        &self.client
    }

    /// Sends `query` to the node and classifies the answer.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError`] when the request could not leave the gateway:
    /// a withheld identifier in the request ([`DispatchError::Withheld`], with
    /// nothing sent), no credential, or a body the client runtime refuses. Every answer, and every failure to reach the node, is a
    /// [`NodeReply`].
    pub async fn query(
        &self,
        query: &NodeQuery,
        options: &DispatchOptions,
    ) -> Result<NodeReply, DispatchError> {
        self.gate(query, options)?;
        let call = options
            .call_options(&self.endpoint)
            .map_err(|error| DispatchError::of_options(&self.endpoint, error))?;
        let params = QueryExecuteAdhocQueryBodyParams {
            accept: None,
            content_type: None,
        };
        let started = Instant::now();
        let answer = QueryClient::new(&self.client)
            .with_options(call)
            .query_execute_adhoc_query_body(&params, &query.body())
            .await;
        let latency_ms = classify::elapsed_ms(started);
        match answer {
            Ok(outcome) => Ok(classify::narrow(
                classify::answered(outcome, latency_ms, options.withheld()),
                query.width,
            )),
            Err(error) => classify::failed(
                (&self.endpoint, &self.consent_refusal_codes),
                error,
                latency_ms,
                options,
            ),
        }
    }
}

/// One [`NodeClient`] per endpoint of a registry snapshot.
#[derive(Debug, Clone)]
pub struct NodeClients<T> {
    clients: BTreeMap<EndpointId, NodeClient<T>>,
}

impl<T: Transport + Clone> NodeClients<T> {
    /// A client for every endpoint of `snapshot`, each over a clone of
    /// `transport` (one connection pool), with the onward credentials of
    /// `credentials` where an endpoint has them.
    ///
    /// # Errors
    ///
    /// Returns [`SetupError::BaseUrl`] when an endpoint's base URL cannot carry
    /// a path, and [`SetupError::UnknownEndpoint`] when `credentials` names an
    /// endpoint the snapshot does not hold.
    pub fn from_snapshot(
        snapshot: &RegistrySnapshot,
        transport: &T,
        credentials: &BTreeMap<EndpointId, SharedCredentials>,
    ) -> Result<Self, SetupError> {
        if let Some(stray) = credentials
            .keys()
            .find(|endpoint| snapshot.endpoint(endpoint).is_none())
        {
            return Err(SetupError::UnknownEndpoint {
                endpoint: stray.clone(),
            });
        }
        let mut clients = BTreeMap::new();
        for endpoint in snapshot.endpoints() {
            let mut client = NodeClient::new(endpoint, transport.clone())?;
            if let Some(provider) = credentials.get(endpoint.id()) {
                client = client.with_credentials_provider(Arc::clone(provider));
            }
            clients.insert(endpoint.id().clone(), client);
        }
        Ok(Self { clients })
    }

    /// The client of `endpoint`, when the snapshot holds it.
    #[must_use]
    pub fn get(&self, endpoint: &EndpointId) -> Option<&NodeClient<T>> {
        self.clients.get(endpoint)
    }

    /// Every client, in endpoint id order.
    pub fn iter(&self) -> impl Iterator<Item = &NodeClient<T>> {
        self.clients.values()
    }

    /// The number of clients.
    #[must_use]
    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// Whether the snapshot held no endpoint.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }
}

/// `base` with the ITS-REST version segment appended to its path, the base
/// itself otherwise unchanged (N28).
fn versioned_base(base: &Url) -> Url {
    let mut versioned = base.clone();
    let root = base.path().trim_end_matches('/');
    versioned.set_path(&format!("{root}/{API_VERSION_SEGMENT}"));
    versioned
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use super::{NodeClients, versioned_base};
    use ferrofed_registry::snapshot::RegistrySnapshot;
    use openehr_its::rest::client::ReqwestTransport;
    use url::Url;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    #[expect(
        clippy::panic_in_result_fn,
        reason = "a test asserts, and returns its setup errors"
    )]
    fn every_node_client_makes_one_attempt() -> TestResult {
        let snapshot = RegistrySnapshot::from_toml_str(
            "[[organisation]]\nid = \"org-a\"\n\n[[node]]\nid = \"node-a\"\norganisation = \"org-a\"\nsystem_id = \"cdr-a.example.org\"\n\n[[endpoint]]\nid = \"node-a-pub\"\nnode = \"node-a\"\nurl = \"https://cdr-a.example.org/openehr\"\nconnection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n",
        )?;
        let transport = ReqwestTransport::with_timeout(Duration::from_secs(1))?;
        let clients = NodeClients::from_snapshot(&snapshot, &transport, &BTreeMap::new())?;
        let mut built = 0_usize;
        for client in clients.iter() {
            built += 1;
            assert_eq!(
                1,
                client.client().retry().max_attempts,
                "a retry lets DeadlineElapsed follow a sent attempt, and Contact::of_client_error, \
                 Contact::of_forward_error, Contact::of_ehr_call_error, the query reply's and the \
                 stored definition's contact would all read that request as never sent"
            );
        }
        assert_eq!(1, built, "the snapshot's one endpoint has a client");
        Ok(())
    }

    #[test]
    #[expect(
        clippy::panic_in_result_fn,
        reason = "a test asserts, and returns its setup errors"
    )]
    fn the_version_segment_is_appended_once() -> TestResult {
        assert_eq!(
            versioned_base(&Url::parse("https://cdr.example.org/openehr")?).as_str(),
            "https://cdr.example.org/openehr/v1"
        );
        assert_eq!(
            versioned_base(&Url::parse("https://cdr.example.org/openehr/")?).as_str(),
            "https://cdr.example.org/openehr/v1"
        );
        assert_eq!(
            versioned_base(&Url::parse("https://cdr.example.org")?).as_str(),
            "https://cdr.example.org/v1"
        );
        Ok(())
    }
}

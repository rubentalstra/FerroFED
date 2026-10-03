// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The fan-out under one deadline and the all-or-nothing decision (§11.4, §11.5).
//!
//! One concurrent request goes to each in-scope node, the `meta.federation`
//! envelope is built from every outcome, and the decision is taken over it
//! (N37, N38, N40). A [`Plan`] names the endpoints to dispatch to, each with the node query the
//! rewrite produced for it, and the endpoints whose status was settled before
//! any request existed (`not-resolved` from the cross-reference, a
//! pre-filter's `consent-denied`, `excluded`, `not-localized`). [`fan_out`]
//! sends every query at once, each under a per-node deadline cut to the
//! overall budget, and stops waiting when the budget runs out: a node still
//! outstanding then is abandoned and reported `time-out`, abandoning it
//! touches no other request, and an answer that arrives later has nowhere to
//! go (§11.5). There is no retry and no hedging inside the budget (no
//! specification governs this: our own design).
//!
//! The envelope is built before the decision, so a failing answer still
//! carries it (§11.4, CP-30). [`decide`] is the pure decision under the
//! request's [`Completion`]. All-or-nothing, the default (N37): any in-scope
//! `offline` or `time-out` fails the query `504`, any `node-error` fails it
//! `424`, and `504` wins when both occur. Best-effort, selected per request
//! with `openEHR-federation-completeness: partial`: the same failures are
//! reported and the answering nodes' rows come back with a `200`.
//! `not-resolved` and `consent-denied` are answers and fail nothing in either
//! mode (§11.3, N6). Every in-scope status short of `active` clears
//! `complete`, which the envelope derives from the statuses and never takes as
//! an input.
//!
//! The rows of the `active` nodes are merged under the plan's Tier order, cut
//! at its `LIMIT` (§11.6.1, N39) and, for a page at `OFFSET k`, sliced from
//! row `k` (§11.6.2) by `openehr_federation::merge`. A node that returned `n`
//! rows out of the federation order is reported `node-error`, so under
//! all-or-nothing the query fails `424` (§11.4; no specification governs the
//! order check: our own design). An aggregate query's one-row answers are
//! recombined into one row instead (§11.6.3); a node whose answer cannot take
//! part in an exact value is `node-error` the same way, and such a plan is
//! refused under best-effort, since it is exact only over every node.
//!
//! Under version-identity dedup (§10.2), selected per request, the merge keeps
//! the originating copy of a version held at several endpoints, each answer
//! carrying its node's registry `system_id`, and `meta.federation.dedup`
//! records the mode on every answer, with what it suppressed beside the rows.

mod answer;
mod seen;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrofed_registry::id::EndpointId;
use ferrofed_registry::snapshot::RegistrySnapshot;
use http::StatusCode;
use openehr_base::prelude::ObjectVersionId;
use openehr_federation::aggregate::Recombination;
use openehr_federation::attribute::EndpointAttribute;
use openehr_federation::dedup::DedupMode;
use openehr_federation::envelope;
use openehr_federation::error::WireError;
use openehr_federation::merge::Unrepresentable;
use openehr_federation::meta::{FederationMeta, TimeoutBudget};
use openehr_federation::order::ResultOrder;
use openehr_federation::outcome::{EndpointOutcome, ErrorDetail, Outcome};
use openehr_federation::status::EndpointStatus;
use openehr_its::rest::client::Transport;
use openehr_its::rest::generated::query::{
    ResultSet, ResultSetColumn, ResultSetMetadata, ResultSetRow,
};
use tokio::task::{JoinError, JoinSet};

use crate::dispatch::{Contact, DispatchError, DispatchOptions, NodeClients, NodeQuery, NodeReply};
use crate::hygiene::Withheld;
use crate::onward::conveyance::Conveyance;
use crate::outbound_id::OutboundId;

/// The completion policy the budget applies under, as `OPTIONS {base}/` and
/// `meta.federation.timeout` name it (§7a.2, §11.5; the value is our own
/// design, since no specification governs it).
pub const TIMEOUT_POLICY: &str = "abandon-and-mark";

/// The `meta.federation` member that carries the failure of a configured
/// localizer, as `localization.error` (§14.1; the member name is our own
/// design, since the specification names none).
pub const LOCALIZATION_MEMBER: &str = "localization";

/// The `meta.federation` member that carries the failure of the consent
/// pre-filter, as `consent.error` (N27a; it mirrors `localization.error` of
/// §14.1, and no specification names it: our own design).
pub const CONSENT_MEMBER: &str = "consent";

/// The completion strategy a request runs under (§11.4, N37).
///
/// Both apply to reads only; a write goes to one node and succeeds or fails
/// there (§11.4, §12.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Completion {
    /// The default: an in-scope node that was asked and did not answer, or
    /// answered with an error, fails the query (`504` or `424`).
    #[default]
    AllOrNothing,
    /// Best-effort, opted into per request: the rows of the nodes that
    /// answered come back with a `200`, every other node is reported with its
    /// status, and `complete` is `false`.
    BestEffort,
}

/// The per-node timeout and the overall budget of one fan-out (§11.5, N38).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    per_node: Duration,
    overall: Duration,
}

impl Budget {
    /// A budget of `per_node` for each node's request and `overall` for the
    /// whole fan-out.
    ///
    /// # Errors
    ///
    /// Returns [`BudgetError::Zero`] when either duration is zero: a gateway
    /// MUST apply both (§11.5), and a zero budget abandons every node before
    /// it is asked.
    pub fn new(per_node: Duration, overall: Duration) -> Result<Self, BudgetError> {
        if per_node.is_zero() {
            return Err(BudgetError::Zero { which: "per-node" });
        }
        if overall.is_zero() {
            return Err(BudgetError::Zero { which: "overall" });
        }
        Ok(Self { per_node, overall })
    }

    /// The per-node timeout in force: the configured one, never longer than
    /// the overall budget.
    #[must_use]
    pub fn per_node(&self) -> Duration {
        self.per_node.min(self.overall)
    }

    /// The overall budget.
    #[must_use]
    pub fn overall(&self) -> Duration {
        self.overall
    }

    /// Returns this budget with the overall budget cut to `wait`, the client
    /// deadline of a `Prefer: wait` (§11.5).
    ///
    /// A shorter wait is honoured and a longer one changes nothing, because a
    /// client can shorten the budget and never extend it. The per-node
    /// timeout follows, since it never runs past the overall budget. A wait
    /// of zero leaves no time to ask any node, so every node is reported
    /// `time-out` and none is sent a request.
    #[must_use]
    pub fn shortened_to(self, wait: Duration) -> Self {
        Self {
            per_node: self.per_node,
            overall: self.overall.min(wait),
        }
    }

    /// The effective budget as `meta.federation.timeout` reports it (§11.5).
    fn record(&self) -> TimeoutBudget {
        TimeoutBudget {
            per_node_ms: Some(whole_ms(self.per_node())),
            overall_ms: Some(whole_ms(self.overall)),
            policy: Some(TIMEOUT_POLICY.to_owned()),
            ..TimeoutBudget::default()
        }
    }
}

/// A budget that cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BudgetError {
    /// One of the two budgets is zero.
    #[error("the {which} budget is zero")]
    Zero {
        /// `per-node` or `overall`.
        which: &'static str,
    },
}

/// What one fan-out does with each endpoint: the node queries to send, and
/// the statuses settled before any request existed.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    dispatch: BTreeMap<EndpointId, NodeQuery>,
    settled: BTreeMap<EndpointId, Outcome>,
    withheld: Arc<Withheld>,
    completion: Completion,
    order: ResultOrder,
    recombination: Option<Recombination>,
    dedup: DedupMode,
    attributes: Vec<EndpointAttribute>,
    unavailable: BTreeMap<&'static str, ErrorDetail>,
}

impl Plan {
    /// An empty plan.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// This plan refusing to send any request that carries one of the
    /// identifiers `withheld`, the ones resolution consumed (§5.4.1, N33).
    #[must_use]
    pub fn withholding(mut self, withheld: Withheld) -> Self {
        self.withheld = Arc::new(withheld);
        self
    }

    /// This plan decided under `completion` instead of the all-or-nothing
    /// default (§11.4).
    #[must_use]
    pub fn completing(mut self, completion: Completion) -> Self {
        self.completion = completion;
        self
    }

    /// This plan merging the node answers under `order`, the Tier order and
    /// `LIMIT` the rewrite wrote into the node queries, and the `OFFSET` the
    /// Tier skips (§11.6.1, §11.6.2, N39).
    #[must_use]
    pub fn ordered(mut self, order: ResultOrder) -> Self {
        self.order = order;
        self
    }

    /// This plan recombining the one-row node answers of an aggregate query
    /// into the federation's row under `recombination`, instead of merging
    /// them as rows (§11.6.3).
    ///
    /// A recombined aggregate is exactly correct only over every in-scope
    /// node, so [`fan_out`] refuses the plan under [`Completion::BestEffort`].
    #[must_use]
    pub fn recombining(mut self, recombination: Recombination) -> Self {
        self.recombination = Some(recombination);
        self
    }

    /// This plan deduplicating under `mode`, the mode the request selected
    /// (§10, N15), which `meta.federation.dedup.mode` records on the answer.
    ///
    /// The order says which node column holds the version uid
    /// ([`ResultOrder::version_key`]); with none, no row is suppressed.
    #[must_use]
    pub fn deduplicating(mut self, mode: DedupMode) -> Self {
        self.dedup = mode;
        self
    }

    /// This plan adding the ENDPOINT attributes `attributes` to the rows of
    /// every endpoint that answers, in that order, from the endpoint's
    /// registry entry (§9.3, N12): the answer carries their values beside
    /// each row ([`FederatedAnswer::attributes`]), and under `DISTINCT` they
    /// take part in which rows are equal (N13).
    #[must_use]
    pub fn annotating(mut self, attributes: Vec<EndpointAttribute>) -> Self {
        self.attributes = attributes;
        self
    }

    /// This plan reporting that the configured localizer did not answer, as
    /// `meta.federation.localization.error` on the answer, since under
    /// fail-closed neither `complete` nor the status can carry it (§14.1).
    #[must_use]
    pub fn localization_failed(mut self, error: ErrorDetail) -> Self {
        self.unavailable.insert(LOCALIZATION_MEMBER, error);
        self
    }

    /// This plan reporting that the consent pre-filter did not answer, as
    /// `meta.federation.consent.error` on the answer: every candidate was asked
    /// (N27), so neither `complete` nor the status changes.
    #[must_use]
    pub fn consent_unavailable(mut self, error: ErrorDetail) -> Self {
        self.unavailable.insert(CONSENT_MEMBER, error);
        self
    }

    /// Adds `endpoint`, to be asked `query`.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError::Duplicate`] when the plan already names the
    /// endpoint: every endpoint has exactly one status in a query (N16).
    pub fn dispatch(mut self, endpoint: EndpointId, query: NodeQuery) -> Result<Self, PlanError> {
        self.refuse_duplicate(&endpoint)?;
        self.dispatch.insert(endpoint, query);
        Ok(self)
    }

    /// Adds `endpoint` with the status `outcome`, settled with no request.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError::Duplicate`] when the plan already names the
    /// endpoint, and [`PlanError::DispatchedStatus`] when `outcome` is a
    /// status that only a request can produce (`active`, `offline`,
    /// `time-out`, `node-error`, a node's own consent refusal), since no
    /// request was made to measure it (N40).
    pub fn settle(mut self, endpoint: EndpointId, outcome: Outcome) -> Result<Self, PlanError> {
        if outcome.latency_ms().is_some() {
            return Err(PlanError::DispatchedStatus {
                endpoint,
                status: outcome.status(),
            });
        }
        self.refuse_duplicate(&endpoint)?;
        self.settled.insert(endpoint, outcome);
        Ok(self)
    }

    /// The endpoints the plan dispatches to, in endpoint id order.
    pub fn dispatched(&self) -> impl Iterator<Item = &EndpointId> {
        self.dispatch.keys()
    }

    /// Whether node selection resolved the request to no destination at all,
    /// which is a `404` (§11.2, first row; §11.3).
    ///
    /// That is a plan with no endpoint in scope (§11.1 "What in scope
    /// means") in which no endpoint is `not-localized`: every endpoint was
    /// ruled out by a decision (`excluded`), or the plan names none. A plan
    /// that `not-localized` left empty is not one: an empty candidate set
    /// from localization does not fail the query, and dispatches to no node
    /// (§14.1). An in-scope endpoint that is `not-resolved` keeps the request
    /// routed, whose answer is a `200` (§11.3).
    #[must_use]
    pub fn has_no_destination(&self) -> bool {
        self.dispatch.is_empty()
            && self.settled.values().all(|outcome| {
                let status = outcome.status();
                !status.is_in_scope() && status != EndpointStatus::NotLocalized
            })
    }

    fn refuse_duplicate(&self, endpoint: &EndpointId) -> Result<(), PlanError> {
        if self.dispatch.contains_key(endpoint) || self.settled.contains_key(endpoint) {
            return Err(PlanError::Duplicate {
                endpoint: endpoint.clone(),
            });
        }
        Ok(())
    }
}

/// A plan that names an endpoint twice, or settles a status no request made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// The endpoint is already in the plan.
    #[error("endpoint {endpoint} is named twice in one fan-out")]
    Duplicate {
        /// The endpoint.
        endpoint: EndpointId,
    },
    /// The status needs a dispatched request to be reported.
    #[error("endpoint {endpoint} cannot be settled as {status} without a request")]
    DispatchedStatus {
        /// The endpoint.
        endpoint: EndpointId,
        /// The status the plan was given.
        status: EndpointStatus,
    },
}

/// What the completion strategy makes of a fan-out (§11.4, N37).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No in-scope endpoint failed: the answer is a `200`, with
    /// `meta.federation.complete` saying whether every one was `active`.
    Answered,
    /// Best-effort: an in-scope endpoint failed, it is reported with its
    /// status, and the rows of the endpoints that answered come back with a
    /// `200` and `complete: false`.
    Partial,
    /// An in-scope endpoint was `offline` or `time-out`: `504`.
    Unanswered,
    /// An in-scope endpoint was `node-error`, and none unanswered: `424`.
    NodeFailed,
}

impl Verdict {
    /// The HTTP status of the answer (§11.2, §11.4).
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::Answered | Self::Partial => StatusCode::OK,
            Self::Unanswered => StatusCode::GATEWAY_TIMEOUT,
            Self::NodeFailed => StatusCode::FAILED_DEPENDENCY,
        }
    }

    /// Whether the query failed, so no rows are returned.
    #[must_use]
    pub fn failed(self) -> bool {
        matches!(self, Self::Unanswered | Self::NodeFailed)
    }
}

/// The decision over the reported endpoints under `completion` (§11.4, N37).
///
/// Under all-or-nothing, `504` takes precedence over `424`: an unanswered node
/// is an unknown, which a retry or a `partial` request recovers, while a node
/// error is already read. Under best-effort the same failures make the answer
/// [`Verdict::Partial`] and fail nothing. An endpoint that was not in scope
/// (`excluded`, `not-localized`) decides nothing, and `not-resolved` and
/// `consent-denied` are answers in either mode (§11.3).
#[must_use]
pub fn decide(endpoints: &[EndpointOutcome], completion: Completion) -> Verdict {
    let failing = endpoints
        .iter()
        .map(EndpointOutcome::status)
        .filter(|status| status.is_in_scope() && status.fails_all_or_nothing());
    let mut verdict = Verdict::Answered;
    for status in failing {
        if completion == Completion::BestEffort {
            return Verdict::Partial;
        }
        match status {
            EndpointStatus::Offline | EndpointStatus::TimeOut => return Verdict::Unanswered,
            _ => verdict = Verdict::NodeFailed,
        }
    }
    verdict
}

/// The answer of one fan-out: the decision, the envelope, and the rows of the
/// `active` endpoints when the query did not fail.
#[derive(Debug, Clone)]
pub struct FederatedAnswer {
    verdict: Verdict,
    federation: FederationMeta,
    rows: Vec<ResultSetRow>,
    attributes: Vec<Vec<String>>,
    seen: Vec<(EndpointId, ObjectVersionId)>,
    contacts: BTreeMap<EndpointId, Contact>,
}

impl FederatedAnswer {
    /// The decision under the plan's completion strategy.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        self.verdict
    }

    /// The HTTP status of the answer.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.verdict.status()
    }

    /// The `meta.federation` record, present on a failing answer too
    /// (§11.4).
    #[must_use]
    pub fn federation(&self) -> &FederationMeta {
        &self.federation
    }

    /// The rows of the `active` endpoints in endpoint id order, empty when the
    /// query failed: a failing query MUST NOT return the rows it did obtain,
    /// and a best-effort answer returns exactly those (§11.4). Unresponsive
    /// nodes never contribute rows (§11.1).
    #[must_use]
    pub fn rows(&self) -> &[ResultSetRow] {
        &self.rows
    }

    /// The values of the plan's ENDPOINT attributes beside each row, in
    /// [`FederatedAnswer::rows`] order, from the registry entry of the
    /// endpoint the row came from (§9.3, N12; [`Plan::annotating`]). An entry
    /// is empty when the plan adds no attribute, and for the one row of a
    /// recombined aggregate, which comes from no single endpoint.
    #[must_use]
    pub fn attributes(&self) -> &[Vec<String>] {
        &self.attributes
    }

    /// The versions the rows of each answering endpoint show it holding, one
    /// per endpoint and `creating_system_id`, in endpoint id order: what the
    /// follow-up routing table learns from (§12.2, N21).
    ///
    /// They are read from every endpoint that sent rows, a failing answer's
    /// included.
    pub fn seen(&self) -> impl Iterator<Item = (&EndpointId, &ObjectVersionId)> {
        self.seen
            .iter()
            .map(|(endpoint, version)| (endpoint, version))
    }

    /// What the request to each endpoint the plan dispatched to showed of
    /// the node, in endpoint id order: its own HTTP status where it answered,
    /// which the §11.1 record in [`FederatedAnswer::federation`] carries only
    /// as text. An endpoint settled with no request has none.
    pub fn contacts(&self) -> impl Iterator<Item = (&EndpointId, Contact)> {
        self.contacts
            .iter()
            .map(|(endpoint, contact)| (endpoint, *contact))
    }

    /// The federated ITS-REST `RESULT_SET` of this answer, with the façade's
    /// own `q` and `columns[]` (N17, §9.2) and `meta.federation` under `meta`
    /// (§9.1). A failing answer gets the same shape with no rows.
    ///
    /// # Errors
    ///
    /// Returns [`WireError`] when the envelope cannot be encoded.
    pub fn into_result_set(
        self,
        q: Option<String>,
        columns: Option<Vec<ResultSetColumn>>,
    ) -> Result<ResultSet, WireError> {
        let mut meta = ResultSetMetadata {
            _href: None,
            _type: None,
            _schema_version: None,
            _created: None,
            _generator: None,
            _executed_aql: None,
            additional_properties: BTreeMap::new(),
        };
        envelope::attach(&self.federation, &mut meta)?;
        Ok(ResultSet {
            meta: Some(meta),
            name: None,
            q,
            columns,
            rows: self.rows,
            additional_properties: BTreeMap::new(),
        })
    }
}

/// A fan-out that could not produce an answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FanOutError {
    /// The plan names an endpoint the registry snapshot or its clients do
    /// not hold.
    #[error("endpoint {endpoint} is not in the registry snapshot")]
    UnknownEndpoint {
        /// The endpoint.
        endpoint: EndpointId,
    },
    /// A request could not leave the gateway (no credential, a request the
    /// client runtime refused), so there is no endpoint status to report: a
    /// gateway internal error (§11.2).
    #[error("a request could not leave the gateway")]
    Dispatch(#[from] DispatchError),
    /// A dispatch task ended without a reply.
    #[error("a dispatch task ended without a reply")]
    Task(#[from] JoinError),
    /// The overall budget runs past what the platform clock can represent.
    #[error("the overall budget runs past the platform clock")]
    Clock,
    /// An endpoint record could not be written to the envelope.
    #[error("the record of endpoint {endpoint} could not be written to the envelope")]
    Record {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the wire types refused.
        #[source]
        source: WireError,
    },
    /// The envelope could not be built from the endpoint records.
    #[error("the meta.federation envelope could not be built")]
    Envelope(#[source] WireError),
    /// The plan recombines an aggregate under best-effort completion, which
    /// would answer a value over the nodes that answered as the federation's
    /// (§11.6.3, §11.4). Nothing was dispatched.
    #[error("an aggregate recombined across nodes cannot be answered best-effort")]
    PartialAggregate,
    /// The recombined aggregate cannot be written exactly (§11.6.3).
    #[error("the recombined aggregate cannot be written exactly")]
    Unrepresentable(#[source] Unrepresentable),
}

/// Sends every query of `plan` to its node at once and decides the answer
/// (§11.4, §11.5).
///
/// Each request carries a per-node deadline, the budget's per-node timeout
/// from now and never past the overall deadline; the `conveyance` and the
/// minted `request_id` go to every node. When the overall budget runs out,
/// every node still outstanding is abandoned and reported `time-out` with
/// the time it was given, its task dropped: a late answer adds nothing.
///
/// # Errors
///
/// Returns [`FanOutError::UnknownEndpoint`] when `plan` names an endpoint
/// `snapshot` or `clients` lack, [`FanOutError::Dispatch`] when a request
/// could not leave the gateway, [`FanOutError::Task`] when a dispatch task
/// panicked, [`FanOutError::Clock`] when the budget overflows the clock, and
/// [`FanOutError::Record`] or [`FanOutError::Envelope`] when the envelope
/// cannot be written, [`FanOutError::PartialAggregate`] for a plan that
/// recombines an aggregate under best-effort completion, and
/// [`FanOutError::Unrepresentable`] when the recombined aggregate of an
/// answered query cannot be written exactly.
pub async fn fan_out<T>(
    clients: &NodeClients<T>,
    snapshot: &RegistrySnapshot,
    plan: Plan,
    budget: Budget,
    (conveyance, request_id): (&Conveyance, Option<OutboundId>),
) -> Result<FederatedAnswer, FanOutError>
where
    T: Transport + Clone + 'static,
{
    fan_out_within(
        clients,
        snapshot,
        plan,
        budget,
        Instant::now(),
        (conveyance, request_id),
    )
    .await
}

/// Sends every query of `plan` to its node at once, inside the overall budget
/// of a request that started at `started` (§11.5).
///
/// The overall deadline runs from `started`, so the time the request spent
/// before the dispatch, resolving the patient, comes out of the same budget:
/// the gateway answers within its declared overall budget. The per-node
/// deadline runs from the dispatch and never past the overall deadline, and a
/// node's latency is measured from the dispatch (§9.5, N40). A node whose
/// deadline passed before its request could be sent is `time-out` with no
/// request sent. Everything else is as [`fan_out`].
///
/// # Errors
///
/// As [`fan_out`].
pub async fn fan_out_within<T>(
    clients: &NodeClients<T>,
    snapshot: &RegistrySnapshot,
    plan: Plan,
    budget: Budget,
    started: Instant,
    (conveyance, request_id): (&Conveyance, Option<OutboundId>),
) -> Result<FederatedAnswer, FanOutError>
where
    T: Transport + Clone + 'static,
{
    let deadline = started
        .checked_add(budget.overall())
        .ok_or(FanOutError::Clock)?;
    let dispatched = Instant::now();
    let node_deadline = dispatched
        .checked_add(budget.per_node())
        .map_or(deadline, |at| at.min(deadline));
    let Plan {
        dispatch,
        settled,
        withheld,
        completion,
        order: result_order,
        recombination,
        dedup,
        attributes,
        unavailable,
    } = plan;
    if recombination.is_some() && completion == Completion::BestEffort {
        return Err(FanOutError::PartialAggregate);
    }
    let order: Vec<EndpointId> = dispatch.keys().cloned().collect();
    let until = tokio::time::Instant::from_std(deadline);
    let mut tasks = JoinSet::new();
    for (index, (endpoint, query)) in dispatch.into_iter().enumerate() {
        let client = clients
            .get(&endpoint)
            .ok_or_else(|| FanOutError::UnknownEndpoint {
                endpoint: endpoint.clone(),
            })?
            .clone();
        let mut options = DispatchOptions::new(node_deadline, conveyance.clone())
            .with_withheld(Arc::clone(&withheld));
        if let Some(id) = request_id {
            options = options.with_request_id(id);
        }
        // NOTE: tokio::time::timeout_at (docs.rs) polls the query before the budget, so a request
        // the budget overtook before it left ends unsent, never abandoned.
        tasks.spawn(async move {
            let reply = tokio::time::timeout_at(until, client.query(&query, &options)).await;
            (index, reply)
        });
    }
    let mut replies: Vec<Option<NodeReply>> = vec![None; order.len()];
    while let Some(joined) = tasks.join_next().await {
        let (index, reply) = joined?;
        if let (Ok(reply), Some(slot)) = (reply, replies.get_mut(index)) {
            *slot = Some(reply?);
        }
    }
    let abandoned_ms = whole_ms(dispatched.elapsed());
    let mut contacts = BTreeMap::new();
    let mut records: BTreeMap<EndpointId, (Outcome, Option<Vec<ResultSetRow>>)> = settled
        .into_iter()
        .map(|(endpoint, outcome)| (endpoint, (outcome, None)))
        .collect();
    for (endpoint, reply) in order.into_iter().zip(replies) {
        let contact = reply.as_ref().map_or(Contact::Silent, NodeReply::contact);
        let record = match reply {
            Some(NodeReply::Answered {
                result_set,
                latency_ms,
            }) => (Outcome::Active { latency_ms }, Some(result_set.rows)),
            Some(NodeReply::Failed { outcome, .. }) => (outcome, None),
            None => (abandoned(abandoned_ms, budget.overall()), None),
        };
        contacts.insert(endpoint.clone(), contact);
        records.insert(endpoint, record);
    }
    let shaping = answer::Shaping {
        order: &result_order,
        recombination: recombination.as_ref(),
        dedup,
        attributes: &attributes,
    };
    let mut answer = answer::answer(snapshot, records, shaping, budget, completion)?;
    for (member, error) in unavailable {
        answer::report_unavailable(&mut answer.federation, member, error)?;
    }
    answer.contacts = contacts;
    Ok(answer)
}

/// The `time-out` of a node still outstanding when the overall budget ran out
/// (§11.5).
fn abandoned(latency_ms: u64, overall: Duration) -> Outcome {
    Outcome::TimeOut {
        latency_ms,
        error: ErrorDetail::Text(format!(
            "abandoned with no answer when the overall budget of {} ms ran out",
            whole_ms(overall)
        )),
    }
}

/// `duration` in whole milliseconds, saturating at `u64::MAX`.
fn whole_ms(duration: Duration) -> u64 {
    // NOTE: §9.5 reports latency in whole milliseconds; no budget runs past
    // u64::MAX ms, so saturating loses nothing.
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

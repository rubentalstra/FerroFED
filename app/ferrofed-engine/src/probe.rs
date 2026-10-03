// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The ask-all probe of §12.5.1 step 4: `GET {base}/v1/ehr/{ehr_id}` at every
//! member, for a read whose owner no earlier routing step named.
//!
//! Each probe is the one request, held once per member with
//! [`HeldRequest::fit`] and sent with
//! [`NodeClient::forward_held`](crate::dispatch::NodeClient::forward_held), so it passes
//! the same outbound gate as every routed request: the path carries the
//! node-local `ehr_id` and nothing else, and of the client's headers only the
//! ones `GET {base}/v1/ehr/{ehr_id}` declares travel, each composed for that
//! operation and left out when it does not fit it (§5.4.1, N33). The
//! `ehr_id` is a [`ProbedEhrId`], a bare UUID, because the probe carries it
//! to members the client never named and any other form may be a patient
//! identifier (§5.4.1, N33). Every
//! member is asked at once, each under the per-node deadline and all under
//! the overall budget (§11.5, N38): a slow member is abandoned, never waited
//! on past the budget, and abandoning it aborts no other probe.
//!
//! The probe reports what each member answered and decides nothing. A member
//! that answered neither a success nor `404` has not said whether it holds
//! the `ehr_id`, and the caller must read it as unknown, never as absent
//! (§11.5: a `time-out` means unknown, never no data).

use std::time::{Duration, Instant};

use ferrofed_registry::id::{EhrId, EndpointId};
use http::{HeaderMap, Method, StatusCode};
use openehr_its::rest::client::Transport;
use tokio::task::{JoinError, JoinSet};

use crate::dispatch::{Contact, DispatchOptions, NodeClients};
use crate::forward::{ClientRequest, ForwardError, Forwarded, HeldRequest};
use crate::onward::conveyance::Conveyance;
use crate::outbound_id::OutboundId;

/// What one member answered the probe.
#[derive(Debug)]
pub enum Answer {
    /// A success: the member holds the EHR, and this is its answer.
    Holds(Forwarded),
    /// `404 Not Found`: the member does not hold the EHR.
    Absent,
    /// Any other status, which says nothing about whether the member holds
    /// the EHR.
    Erred(StatusCode),
    /// The probe got no answer of the member's: it timed out, could not
    /// reach the member, was refused the onward credentials, or was never
    /// sent, its deadline passed before it left included
    /// ([`ForwardError::Expired`]).
    Failed(ForwardError),
    /// The probe was sent and the overall budget ran out before the member
    /// answered (§11.5).
    Abandoned,
}

/// What one member answered the probe, and how long the gateway waited for
/// it.
#[derive(Debug)]
pub struct Probed {
    /// What the member answered.
    pub answer: Answer,
    /// The gateway's measurement of the probe: from sending it to the
    /// member's answer or failure, or, for an [`Answer::Abandoned`] probe,
    /// to the moment the overall budget ran out.
    pub latency: Duration,
}

impl Probed {
    /// Returns what the probe showed of the member: its status where it
    /// answered, [`Contact::Silent`] where it gave no answer or was
    /// abandoned, and [`Contact::Unsent`] where the probe never left.
    #[must_use]
    pub fn contact(&self) -> Contact {
        match &self.answer {
            Answer::Holds(forwarded) => Contact::Answered(forwarded.status()),
            Answer::Absent => Contact::Answered(StatusCode::NOT_FOUND),
            Answer::Erred(status) => Contact::Answered(*status),
            Answer::Failed(error) => Contact::of_forward_error(error),
            Answer::Abandoned => Contact::Silent,
        }
    }
}

/// A probe that could not be run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProbeError {
    /// An endpoint to probe has no node client.
    #[error("endpoint {endpoint} has no node client")]
    UnknownEndpoint {
        /// The endpoint.
        endpoint: EndpointId,
    },
    /// A probe task failed to complete.
    #[error("a probe task failed")]
    Task(#[from] JoinError),
}

/// The `ehr_id` an ask-all probe asks every member about: a bare UUID, the
/// form §12b.2 asks a member to mint its `ehr_id`s in (N42a).
///
/// Every other `HIER_OBJECT_ID` form admits a value the gateway cannot tell
/// from a patient identifier, a bare national number parsing as a one-arc
/// ISO OID, and the probe would carry it to every member (§5.4.1, N33).
#[derive(Debug, Clone)]
pub struct ProbedEhrId(EhrId);

impl ProbedEhrId {
    /// The `ehr_id`, a bare UUID.
    #[must_use]
    pub fn ehr_id(&self) -> &EhrId {
        &self.0
    }
}

impl TryFrom<&EhrId> for ProbedEhrId {
    type Error = NotUuid;

    fn try_from(ehr_id: &EhrId) -> Result<Self, Self::Error> {
        if ehr_id.is_uuid() {
            Ok(Self(ehr_id.clone()))
        } else {
            Err(NotUuid)
        }
    }
}

/// An `ehr_id` that is no bare UUID, which no member is ever probed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("only an ehr_id that is a UUID is probed at every member (§5.4.1, N33)")]
pub struct NotUuid;

/// One probe: the `ehr_id` it asks about, the client headers it may carry,
/// and its budget.
#[derive(Debug)]
pub struct Probe {
    /// The `ehr_id` every member is asked about.
    pub ehr_id: ProbedEhrId,
    /// The client's headers; the operation decides which travel.
    pub headers: HeaderMap,
    /// The instant each member must have answered by: the per-node timeout,
    /// never past `overall`.
    pub per_node: Instant,
    /// The instant the overall budget runs out.
    pub overall: Instant,
    /// The gateway's id for the request, the one every member receives.
    pub request_id: OutboundId,
    /// The identity every member is told the probe is asked on behalf of
    /// (§13.1, N24).
    pub conveyance: Conveyance,
}

impl Probe {
    /// The path the probe asks every member for, relative to the ITS-REST
    /// base.
    ///
    /// A UUID is written in hexadecimal digits and hyphens, so the `ehr_id`
    /// is its own path segment.
    #[must_use]
    pub fn path(&self) -> String {
        format!("/ehr/{}", self.ehr_id.ehr_id())
    }
}

/// Asks every endpoint of `endpoints` for the EHR `probe` names, at once,
/// and returns each answer with its latency, in the order of `endpoints`.
///
/// # Errors
///
/// Returns [`ProbeError::UnknownEndpoint`] when an endpoint has no client in
/// `clients`, before anything is sent, and [`ProbeError::Task`] when a probe
/// task panicked.
pub async fn ask_all<T>(
    clients: &NodeClients<T>,
    endpoints: &[EndpointId],
    probe: &Probe,
) -> Result<Vec<(EndpointId, Probed)>, ProbeError>
where
    T: Transport + Clone + 'static,
{
    let mut asked = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        let client = clients
            .get(endpoint)
            .ok_or_else(|| ProbeError::UnknownEndpoint {
                endpoint: endpoint.clone(),
            })?
            .clone();
        asked.push(client);
    }
    let deadline = probe.per_node.min(probe.overall);
    let options =
        DispatchOptions::new(deadline, probe.conveyance.clone()).with_request_id(probe.request_id);
    let path = probe.path();
    let mut tasks = JoinSet::new();
    let asked_at = Instant::now();
    let until = tokio::time::Instant::from_std(probe.overall);
    for (index, client) in asked.into_iter().enumerate() {
        // NOTE: no specification governs this: our own design; the probe is the gateway's own read, so
        // a client value that does not fit its operation's declared kind is left out, never refused.
        let request = HeldRequest::fit(ClientRequest {
            method: Method::GET,
            path: path.clone(),
            query: None,
            headers: probe.headers.clone(),
            body: Vec::new(),
        });
        let options = options.clone();
        tasks.spawn(async move {
            let started = Instant::now();
            let forwarded = async {
                match request {
                    Ok(request) => client.forward_held(request, &options).await,
                    Err(refused) => Err(refused),
                }
            };
            // NOTE: tokio::time::timeout_at (docs.rs) polls the call before the budget, so a probe
            // the budget overtook before it left ends `Expired`, never abandoned.
            let answer = match tokio::time::timeout_at(until, forwarded).await {
                Ok(forwarded) => classified(forwarded),
                Err(_elapsed) => Answer::Abandoned,
            };
            (
                index,
                Probed {
                    answer,
                    latency: started.elapsed(),
                },
            )
        });
    }
    let mut answers: Vec<Option<Probed>> = endpoints.iter().map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        let (index, probed) = joined?;
        if let Some(slot) = answers.get_mut(index) {
            *slot = Some(probed);
        }
    }
    let abandoned_after = asked_at.elapsed();
    Ok(endpoints
        .iter()
        .cloned()
        .zip(answers)
        .map(|(endpoint, probed)| {
            let probed = probed.unwrap_or(Probed {
                answer: Answer::Abandoned,
                latency: abandoned_after,
            });
            (endpoint, probed)
        })
        .collect())
}

/// What a member's reply says about whether it holds the EHR.
fn classified(forwarded: Result<Forwarded, ForwardError>) -> Answer {
    match forwarded {
        Ok(answer) if answer.status().is_success() => Answer::Holds(answer),
        Ok(answer) if answer.status() == StatusCode::NOT_FOUND => Answer::Absent,
        Ok(answer) => Answer::Erred(answer.status()),
        Err(failure) => Answer::Failed(failure),
    }
}

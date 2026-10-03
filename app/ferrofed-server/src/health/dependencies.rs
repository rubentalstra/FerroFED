// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The last state the gateway observed of each member endpoint, of the
//! resolver, of the consent pre-filter and of the localizer, which
//! `GET /health/dependencies` reports.
//!
//! Nothing here sends a request: the states come from the requests the
//! gateway already makes for its clients, so a dependency nobody has asked
//! since boot is [`Observed::Unknown`]. The record holds one slot per
//! endpoint of the registry snapshot, and one each for the resolver, the
//! consent pre-filter and the localizer, fixed when
//! the federation is built, so it never grows. A dependency's state never
//! gates readiness: under §11 a node outage is reported per query, and
//! readiness that followed it would turn one CDR outage into a total
//! outage. A member's state is its reachability and health, never whether
//! a request to it was valid: every call that sends a member a request reads
//! the node's own answer the same way ([`Observed::of_contact`]), whatever
//! the §11.1 record of that call says. No specification governs health
//! probes: our own design.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};

use ferrofed_engine::dispatch::Contact;
use ferrofed_identity::consent::ConsentDecision;
use ferrofed_identity::localizer::{Localization, LocalizerError};
use ferrofed_identity::resolver::Resolution;
use ferrofed_registry::id::{EndpointId, NodeId};
use serde::Serialize;

/// The last state observed of one dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Observed {
    /// No request has reached it since the federation was built.
    Unknown,
    /// It answered the last request.
    Up,
    /// It was reached and answered the last request with a failure.
    Failing,
    /// The last request could not reach it or got no answer in time.
    Down,
}

impl Observed {
    /// Returns the byte this state is stored as.
    const fn code(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Up => 1,
            Self::Failing => 2,
            Self::Down => 3,
        }
    }

    /// Returns the state stored as `code`.
    const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Unknown,
            1 => Self::Up,
            2 => Self::Failing,
            _ => Self::Down,
        }
    }

    /// Returns what a request's contact with a node says of it, or `None`
    /// when the request never left the gateway.
    ///
    /// The state is the node's reachability and health, never whether the
    /// request was valid: any answer below `500` is [`Observed::Up`], a `5xx`
    /// is [`Observed::Failing`], and no answer is [`Observed::Down`].
    #[must_use]
    pub fn of_contact(contact: Contact) -> Option<Self> {
        match contact {
            Contact::Unsent => None,
            Contact::Answered(status) => Some(Self::of_answer(status)),
            Contact::Silent => Some(Self::Down),
        }
    }

    /// Returns what a node's HTTP answer says of it: a server error is a
    /// failure, and any other answer is an answer.
    // NOTE: no specification governs this: our own design; a 4xx says the
    // request was refused, never that the node is unwell, so the node is up.
    #[must_use]
    pub fn of_answer(status: http::StatusCode) -> Self {
        if status.is_server_error() {
            Self::Failing
        } else {
            Self::Up
        }
    }

    /// Returns what a consent pre-filter's decision says of its service, by
    /// the rule the members follow: a decision, or any answer below `500`, is
    /// [`Observed::Up`], a `5xx` is [`Observed::Failing`], and no answer is
    /// [`Observed::Down`].
    #[must_use]
    pub fn of_consent(decision: &ConsentDecision) -> Self {
        match decision {
            ConsentDecision::Denied(_) | ConsentDecision::NoSignal => Self::Up,
            ConsentDecision::Unavailable(error) => {
                error.status().map_or(Self::Down, Self::of_answer)
            }
        }
    }

    /// Returns what a localizer's answer says of its service, by the rule the
    /// members follow: an answer, or a failure answered below `500`, is
    /// [`Observed::Up`], a `5xx` is [`Observed::Failing`], and no answer is
    /// [`Observed::Down`]; an exchange that could not be audited is
    /// [`Observed::Failing`]; `None` when no localizer was asked.
    #[must_use]
    pub fn of_localization(localization: &Localization) -> Option<Self> {
        match localization {
            Localization::NotConfigured => None,
            Localization::Candidates(_) | Localization::NoRecords => Some(Self::Up),
            // NOTE: no specification governs this: our own design; an exchange the
            // gateway could not audit is a failure of the localization path itself.
            Localization::Unavailable(LocalizerError::AuditFailed(_)) => Some(Self::Failing),
            Localization::Unavailable(error) => {
                Some(error.status().map_or(Self::Down, Self::of_answer))
            }
        }
    }

    /// Returns what a resolution says of the resolver: down when it could not
    /// answer for some member, up when it answered for each, and `None` when
    /// it was not asked.
    #[must_use]
    pub fn of_resolutions(resolutions: &BTreeMap<NodeId, Resolution>) -> Option<Self> {
        if resolutions.is_empty() {
            None
        } else if resolutions
            .values()
            .any(|resolution| matches!(resolution, Resolution::Unavailable(_)))
        {
            Some(Self::Down)
        } else {
            Some(Self::Up)
        }
    }
}

/// One slot per member endpoint and one for the resolver, each holding the
/// last state observed.
#[derive(Debug)]
pub struct Dependencies {
    /// The endpoints of the registry snapshot, fixed at construction.
    endpoints: BTreeMap<EndpointId, AtomicU8>,
    /// The resolver's slot, when one is configured.
    resolver: Option<AtomicU8>,
    /// The consent pre-filter's slot, when one is configured.
    consent: Option<AtomicU8>,
    /// The localizer's slot, when one is configured.
    localizer: Option<AtomicU8>,
}

impl Dependencies {
    /// Returns a record of `endpoints` and, when `resolver` is `true`, of the
    /// resolver, every state [`Observed::Unknown`].
    #[must_use]
    pub fn new<'a>(endpoints: impl IntoIterator<Item = &'a EndpointId>, resolver: bool) -> Self {
        Self {
            endpoints: endpoints
                .into_iter()
                .map(|endpoint| (endpoint.clone(), AtomicU8::new(Observed::Unknown.code())))
                .collect(),
            resolver: resolver.then(|| AtomicU8::new(Observed::Unknown.code())),
            consent: None,
            localizer: None,
        }
    }

    /// Returns this record with a slot for the consent pre-filter when
    /// `configured` is `true`, its state [`Observed::Unknown`].
    #[must_use]
    pub fn with_consent(mut self, configured: bool) -> Self {
        self.consent = configured.then(|| AtomicU8::new(Observed::Unknown.code()));
        self
    }

    /// Returns this record with a slot for the localizer when `configured` is
    /// `true`, its state [`Observed::Unknown`].
    #[must_use]
    pub fn with_localizer(mut self, configured: bool) -> Self {
        self.localizer = configured.then(|| AtomicU8::new(Observed::Unknown.code()));
        self
    }

    /// Records `observed` as the last state of `endpoint`.
    ///
    /// An endpoint outside the record is ignored, so the record never grows.
    pub fn endpoint(&self, endpoint: &EndpointId, observed: Observed) {
        if let Some(slot) = self.endpoints.get(endpoint) {
            slot.store(observed.code(), Ordering::Relaxed);
        }
    }

    /// Records what a request to `endpoint` showed of it, when the request
    /// left the gateway ([`Observed::of_contact`]).
    pub fn contacted(&self, endpoint: &EndpointId, contact: Contact) {
        if let Some(observed) = Observed::of_contact(contact) {
            self.endpoint(endpoint, observed);
        }
    }

    /// Records `observed` as the resolver's last state, when one is
    /// configured.
    pub fn resolver(&self, observed: Observed) {
        if let Some(slot) = &self.resolver {
            slot.store(observed.code(), Ordering::Relaxed);
        }
    }

    /// Records `observed` as the consent pre-filter's last state, when one is
    /// configured.
    pub fn consent(&self, observed: Observed) {
        if let Some(slot) = &self.consent {
            slot.store(observed.code(), Ordering::Relaxed);
        }
    }

    /// Records `observed` as the localizer's last state, when one is
    /// configured.
    pub fn localizer(&self, observed: Observed) {
        if let Some(slot) = &self.localizer {
            slot.store(observed.code(), Ordering::Relaxed);
        }
    }

    /// Returns the report `GET /health/dependencies` answers with.
    #[must_use]
    pub fn report(&self) -> Report {
        Report {
            endpoints: self
                .endpoints
                .iter()
                .map(|(endpoint, slot)| {
                    (
                        endpoint.as_str().to_owned(),
                        Observed::from_code(slot.load(Ordering::Relaxed)),
                    )
                })
                .collect(),
            resolver: self
                .resolver
                .as_ref()
                .map(|slot| Observed::from_code(slot.load(Ordering::Relaxed))),
            consent: self
                .consent
                .as_ref()
                .map(|slot| Observed::from_code(slot.load(Ordering::Relaxed))),
            localizer: self
                .localizer
                .as_ref()
                .map(|slot| Observed::from_code(slot.load(Ordering::Relaxed))),
            directory: None,
        }
    }
}

/// What `GET /health/dependencies` answers with: endpoint ids and states,
/// and nothing else.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    /// Every member endpoint by id, in id order.
    pub endpoints: BTreeMap<String, Observed>,
    /// The resolver's state, absent when no resolver is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolver: Option<Observed>,
    /// The consent pre-filter's state, absent when no pre-filter is
    /// configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consent: Option<Observed>,
    /// The localizer's state, absent when no localizer is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localizer: Option<Observed>,
    /// The state of the care services directory the registry is read from,
    /// absent when the registry is a document: `up` after its last answer,
    /// `failing` after a `5xx` or an answer that breaks ITI-90 or ITI-91, and
    /// `down` when it did not answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<Observed>,
}

#[cfg(test)]
mod tests {
    use super::{Dependencies, Observed};
    use ferrofed_engine::dispatch::Contact;
    use ferrofed_engine::dispatch::definition::NodeCopy;
    use ferrofed_registry::id::EndpointId;
    use http::StatusCode;
    use openehr_federation::outcome::{ErrorDetail, Outcome};

    #[test]
    fn every_slot_starts_unknown_and_keeps_the_last_state() {
        let a = EndpointId::new("node-a-pub").expect("a valid id");
        let b = EndpointId::new("node-b-pub").expect("a valid id");
        let record = Dependencies::new([&a, &b], true);
        let report = record.report();
        assert_eq!(Some(&Observed::Unknown), report.endpoints.get("node-a-pub"));
        assert_eq!(Some(Observed::Unknown), report.resolver);
        record.endpoint(&a, Observed::Down);
        record.endpoint(&a, Observed::Up);
        record.endpoint(&b, Observed::Failing);
        record.resolver(Observed::Down);
        let report = record.report();
        assert_eq!(Some(&Observed::Up), report.endpoints.get("node-a-pub"));
        assert_eq!(Some(&Observed::Failing), report.endpoints.get("node-b-pub"));
        assert_eq!(Some(Observed::Down), report.resolver);
    }

    #[test]
    fn an_endpoint_outside_the_record_is_never_added() {
        let a = EndpointId::new("node-a-pub").expect("a valid id");
        let stranger = EndpointId::new("node-z-pub").expect("a valid id");
        let record = Dependencies::new([&a], false);
        record.endpoint(&stranger, Observed::Down);
        record.resolver(Observed::Up);
        let report = record.report();
        assert_eq!(1, report.endpoints.len());
        assert_eq!(None, report.resolver, "no resolver, no slot");
    }

    #[test]
    fn only_a_request_that_left_the_gateway_is_an_observation() {
        assert_eq!(None, Observed::of_contact(Contact::Unsent));
        assert_eq!(Some(Observed::Down), Observed::of_contact(Contact::Silent));
    }

    #[test]
    fn any_answer_below_500_is_up_and_a_5xx_is_failing() {
        for status in [
            StatusCode::OK,
            StatusCode::CREATED,
            StatusCode::MOVED_PERMANENTLY,
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
            StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            assert_eq!(
                Some(Observed::Up),
                Observed::of_contact(Contact::Answered(status)),
                "{status}"
            );
        }
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert_eq!(
                Some(Observed::Failing),
                Observed::of_contact(Contact::Answered(status)),
                "{status}"
            );
        }
    }

    #[test]
    fn a_copy_held_or_missing_is_an_answer_whether_or_not_it_matches() {
        let held = NodeCopy::Held {
            aql: "SELECT c FROM EHR e CONTAINS COMPOSITION c".to_owned(),
            latency_ms: 3,
        };
        assert_eq!(Some(Observed::Up), Observed::of_contact(held.contact()));
        let missing = NodeCopy::Missing { latency_ms: 3 };
        assert_eq!(Some(Observed::Up), Observed::of_contact(missing.contact()));
        let failed = NodeCopy::Failed {
            outcome: Outcome::NodeError {
                latency_ms: 3,
                error: ErrorDetail::Text("synthetic".to_owned()),
            },
            contact: Contact::Answered(StatusCode::BAD_GATEWAY),
        };
        assert_eq!(
            Some(Observed::Failing),
            Observed::of_contact(failed.contact())
        );
    }
}

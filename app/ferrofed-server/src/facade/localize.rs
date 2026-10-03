// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Localization as every undirected patient route applies it: the federated
//! query and the read of an EHR by subject (N4, N10, §14.1).
//!
//! The configured localizer is asked which members might hold the patient's
//! data, within its own budget. A member it does not name is no candidate. A
//! localizer that does not answer leaves no candidate under the default
//! fail-closed policy, and every member under a declared `ask-all`; either
//! way its failure is kept, for the route to report. Each call is recorded
//! as the localizer's state on `GET /health/dependencies` and in the
//! localizer call metrics.

use std::collections::BTreeSet;
use std::time::Instant;

use ferrofed_identity::localizer::{Localization, LocalizerError, OnFailure};
use ferrofed_identity::patient::PatientRef;
use ferrofed_registry::id::NodeId;
use openehr_federation::outcome::{ErrorDetail, Outcome};

use crate::federation::Federation;
use crate::health::dependencies::Observed;

/// What localization left of the members asked (§14.1).
#[derive(Debug)]
pub(crate) struct Localized {
    /// The members localization named, or `None` when every member is a
    /// candidate.
    candidates: Option<BTreeSet<NodeId>>,
    /// The error every member it did not name carries: the localizer's
    /// failure, under fail-closed.
    error: Option<ErrorDetail>,
    /// The localizer's failure, carried in `meta.federation` too.
    pub(crate) failure: Option<ErrorDetail>,
}

impl Localized {
    /// Every member a candidate: no localizer, or one that is not consulted.
    pub(crate) fn everyone() -> Self {
        Self {
            candidates: None,
            error: None,
            failure: None,
        }
    }

    /// No member a candidate, each carrying `error` when there is one.
    fn nobody(error: Option<ErrorDetail>) -> Self {
        Self {
            candidates: Some(BTreeSet::new()),
            failure: error.clone(),
            error,
        }
    }

    /// Whether `member` is a candidate.
    pub(crate) fn admits(&self, member: &NodeId) -> bool {
        self.candidates
            .as_ref()
            .is_none_or(|candidates| candidates.contains(member))
    }

    /// Whether the localizer failed and the deployment kept to fail-closed,
    /// so no member is a candidate because of the failure.
    pub(crate) fn failed_closed(&self) -> bool {
        self.error.is_some()
    }

    /// The status of a member that is not a candidate (§11.1).
    pub(crate) fn not_localized(&self) -> Outcome {
        Outcome::NotLocalized {
            error: self.error.clone(),
        }
    }
}

/// Asks the federation's localizer which of `members` might hold
/// `patient`'s data, within the localizer's budget and before `deadline`
/// (§14.1, N4), and records what it showed of itself.
///
/// Without a configured localizer, or with no member, every member is a
/// candidate. A localizer still silent at the end of its budget did not
/// answer, and the failure policy applies as to any other failure.
pub(crate) async fn localize(
    federation: &Federation,
    patient: &PatientRef,
    members: &[NodeId],
    deadline: Instant,
) -> Localized {
    let policy = federation.localization();
    let Some(localizer) = policy.localizer() else {
        return Localized::everyone();
    };
    if members.is_empty() {
        return Localized::everyone();
    }
    let until = Instant::now()
        .checked_add(policy.timeout())
        .map_or(deadline, |at| at.min(deadline));
    let answer = tokio::time::timeout_at(
        tokio::time::Instant::from_std(until),
        localizer.localize(patient, members, until),
    )
    .await
    .unwrap_or(Localization::Unavailable(LocalizerError::DeadlineExceeded));
    if let Some(observed) = Observed::of_localization(&answer) {
        federation.dependencies().localizer(observed);
    }
    federation.requests().localized(&answer);
    match answer {
        Localization::NotConfigured => Localized::everyone(),
        Localization::Candidates(named) => Localized {
            candidates: Some(named),
            error: None,
            failure: None,
        },
        Localization::NoRecords => Localized::nobody(None),
        Localization::Unavailable(error @ LocalizerError::AuditFailed(_)) => {
            // NOTE: §14.1, ITI TF-2 §3.55.5.1: ask-all covers a localizer outage, never an
            // exchange the gateway could not audit, so this fails closed under every policy.
            tracing::error!(
                error = %crate::chain(&error),
                "the localization exchange could not be audited"
            );
            Localized::nobody(Some(ErrorDetail::Text(format!(
                "the localization exchange could not be audited, so its answer is not used: {}",
                client_text(&error)
            ))))
        }
        Localization::Unavailable(error) => {
            tracing::warn!(
                error = %crate::chain(&error),
                on_failure = %policy.on_failure(),
                "the localizer did not answer"
            );
            let failure = ErrorDetail::Text(format!(
                "the localizer could not answer: {}",
                client_text(&error)
            ));
            match policy.on_failure() {
                OnFailure::Closed => Localized::nobody(Some(failure)),
                OnFailure::AskAll => Localized {
                    candidates: None,
                    error: None,
                    failure: Some(failure),
                },
            }
        }
    }
}

/// The text a client reads of a localizer's `error`: its cause chain, with
/// the status the localization service answered named once.
///
/// A binding's own error often restates that status, so a link of the chain
/// that names it again is left out here. The log keeps the whole chain.
fn client_text(error: &LocalizerError) -> String {
    let status = error.status().map(|status| status.to_string());
    let mut line = error.to_string();
    let mut cause = std::error::Error::source(error);
    while let Some(link) = cause {
        let text = link.to_string();
        // NOTE: no specification governs this text: our own design; the rule
        // reads the rendering only, and the outcome stays typed.
        if status
            .as_deref()
            .is_none_or(|status| !text.contains(status))
        {
            line.push_str(": ");
            line.push_str(&text);
        }
        cause = link.source();
    }
    line
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use ferrofed_identity::localizer::LocalizerError;
    use http::StatusCode;

    use super::client_text;

    #[derive(Debug)]
    struct Link(&'static str, Option<Box<Link>>);

    impl fmt::Display for Link {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Link {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1
                .as_deref()
                .map(|link| -> &(dyn std::error::Error + 'static) { link })
        }
    }

    #[test]
    fn an_answered_status_is_named_once_with_the_binding_reason() {
        let error = LocalizerError::Answered {
            status: StatusCode::SERVICE_UNAVAILABLE,
            source: Box::new(Link(
                "the service could not cross-reference the patient",
                Some(Box::new(Link(
                    "the service answered 503 Service Unavailable",
                    None,
                ))),
            )),
        };
        assert_eq!(
            "the localization service answered 503 Service Unavailable: the service could not cross-reference the patient",
            client_text(&error)
        );
        assert_eq!(
            "the localization service answered 503 Service Unavailable: the service could not cross-reference the patient: the service answered 503 Service Unavailable",
            crate::chain(&error),
            "the log keeps the whole chain"
        );
    }

    #[test]
    fn a_failure_without_a_status_keeps_its_whole_chain() {
        let error = LocalizerError::Backend(Box::new(Link(
            "the service could not be reached",
            Some(Box::new(Link("connection refused", None))),
        )));
        assert_eq!(crate::chain(&error), client_text(&error));
    }
}

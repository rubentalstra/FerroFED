// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The classification of what a node answered into exactly one §11.1
//! endpoint status, or into a [`DispatchError`] when the failure was the
//! gateway's own and nothing reached the node.

use std::collections::BTreeSet;
use std::time::Instant;

use ferrofed_registry::id::EndpointId;
use http::StatusCode;
use openehr_federation::outcome::{ConsentRefusal, ErrorDetail, Outcome};
use openehr_its::rest::client::{ClientError, ErrorBody, TransportError};
use openehr_its::rest::generated::query::client::QueryExecuteAdhocQueryBodyOutcome;

use super::reported::{self, chain, excerpt_of};
use super::{Contact, DispatchError, DispatchOptions, NodeReply};
use crate::hygiene::Withheld;
use crate::hygiene::mask::MASK;

/// `reply`, or a `node-error` when one of its rows has fewer than `width`
/// cells (§11.1).
pub(super) fn narrow(reply: NodeReply, width: usize) -> NodeReply {
    let NodeReply::Answered {
        result_set,
        latency_ms,
    } = reply
    else {
        return reply;
    };
    match result_set
        .rows
        .iter()
        .map(Vec::len)
        .find(|found| *found < width)
    {
        Some(found) => NodeReply::Failed {
            outcome: Outcome::NodeError {
                latency_ms,
                error: text(format!(
                    "the node answered a row with {found} cells where the dispatched query selects {width}"
                )),
            },
            contact: Contact::Answered(StatusCode::OK),
        },
        None => NodeReply::Answered {
            result_set,
            latency_ms,
        },
    }
}

/// The milliseconds since `started`, saturating at `u64::MAX`.
pub(super) fn elapsed_ms(started: Instant) -> u64 {
    // NOTE: §9.5 reports latency in whole milliseconds; a duration past
    // u64::MAX ms cannot occur inside any budget, so saturating loses nothing.
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The reply for a documented answer of `POST /query/aql`, its `error`
/// holding no identifier of `withheld`.
pub(super) fn answered(
    outcome: QueryExecuteAdhocQueryBodyOutcome,
    latency_ms: u64,
    withheld: &Withheld,
) -> NodeReply {
    match outcome {
        QueryExecuteAdhocQueryBodyOutcome::Ok { body, .. } => NodeReply::Answered {
            result_set: Box::new(body),
            latency_ms,
        },
        QueryExecuteAdhocQueryBodyOutcome::BadRequest { body } => node_error(
            (latency_ms, StatusCode::BAD_REQUEST),
            reported::answered(StatusCode::BAD_REQUEST, &body, withheld),
        ),
        QueryExecuteAdhocQueryBodyOutcome::RequestTimeout { body } => node_error(
            (latency_ms, StatusCode::REQUEST_TIMEOUT),
            reported::answered(StatusCode::REQUEST_TIMEOUT, &body, withheld),
        ),
    }
}

/// The reply, or the gateway-side error, for a call to `endpoint` that
/// reached no documented answer, its `error` holding no identifier of
/// `options` withholds.
///
/// A `403` whose body carries one of `refusal_codes`, the endpoint's
/// consent refusal codes, is `consent-denied` ([`refused_on_consent`]).
pub(super) fn failed(
    (endpoint, refusal_codes): (&EndpointId, &BTreeSet<String>),
    error: ClientError,
    latency_ms: u64,
    options: &DispatchOptions,
) -> Result<NodeReply, DispatchError> {
    let withheld = options.withheld();
    let failure = |outcome, contact| Ok(NodeReply::Failed { outcome, contact });
    match error {
        ClientError::DeadlineElapsed { .. } => failure(
            Outcome::TimeOut {
                latency_ms,
                error: text(
                    "no answer before the deadline, which passed before the request was sent",
                ),
            },
            Contact::Unsent,
        ),
        ClientError::Transport {
            source: TransportError::Timeout { source },
            ..
        } => failure(
            Outcome::TimeOut {
                latency_ms,
                error: reported::followed_by(
                    "no answer before the deadline".to_owned(),
                    &chain(&*source),
                    withheld,
                ),
            },
            Contact::Silent,
        ),
        ClientError::Transport {
            source: TransportError::Send { source },
            ..
        } => failure(
            Outcome::Offline {
                latency_ms,
                error: reported::followed_by(
                    "the node could not be reached".to_owned(),
                    &chain(&*source),
                    withheld,
                ),
            },
            Contact::Silent,
        ),
        ClientError::Unauthorized { body, .. } => Ok(node_error(
            (latency_ms, StatusCode::UNAUTHORIZED),
            reported::answered(StatusCode::UNAUTHORIZED, &body, withheld),
        )),
        ClientError::Forbidden { body, .. } => {
            Ok(forbidden(latency_ms, &body, refusal_codes, withheld))
        }
        ClientError::ServiceFailure { status, body, .. }
        | ClientError::UndocumentedStatus { status, body, .. } => Ok(node_error(
            (latency_ms, status),
            reported::answered(status, &body, withheld),
        )),
        ClientError::Body { status, source, .. } => failure(
            Outcome::NodeError {
                latency_ms,
                error: text(format!(
                    "the node answered {status} with a body that is not an ITS-REST RESULT_SET (at `{}`, a {:?} defect)",
                    excerpt_of(&source.path().to_string(), withheld)
                        .unwrap_or_else(|| MASK.to_owned()),
                    source.inner().classify(),
                )),
            },
            Contact::Answered(status),
        ),
        ClientError::Credentials { source, .. } => failure(
            Outcome::NodeError {
                latency_ms,
                error: reported::unauthenticated(&source, endpoint, options.request_id()),
            },
            Contact::Unsent,
        ),
        other => Err(DispatchError::Compose {
            endpoint: endpoint.clone(),
            source: Box::new(other),
        }),
    }
}

/// The reply of a node that answered `403` with `body`: `consent-denied`
/// when the body names one of `refusal_codes`, `node-error` otherwise.
fn forbidden(
    latency_ms: u64,
    body: &ErrorBody,
    refusal_codes: &BTreeSet<String>,
    withheld: &Withheld,
) -> NodeReply {
    let status = StatusCode::FORBIDDEN;
    let Some(code) = refused_on_consent(body, refusal_codes) else {
        return node_error(
            (latency_ms, status),
            reported::answered(status, body, withheld),
        );
    };
    let lead = format!("the node answered {status} with the consent refusal code {code}");
    NodeReply::Failed {
        outcome: Outcome::ConsentDenied {
            refused_by: ConsentRefusal::Node { latency_ms },
            error: Some(reported::said(lead, body, withheld)),
        },
        contact: Contact::Answered(status),
    }
}

/// The consent refusal code `body` carries, when it is an ITS-REST `Error`
/// whose `code` member is one of `refusal_codes`.
// NOTE: §11.1, N27; ITS-REST 1.1.0 defines no consent signal, so only a code the registry
// lists for the endpoint marks a refusal (no specification governs this: our own design).
pub(super) fn refused_on_consent<'a>(
    body: &'a ErrorBody,
    refusal_codes: &BTreeSet<String>,
) -> Option<&'a str> {
    body.error()?
        .additional_properties
        .get(reported::CODE_MEMBER)?
        .as_str()
        .filter(|code| refusal_codes.contains(*code))
}

/// A `node-error` reply carrying `error`, for a node that answered `status`.
fn node_error((latency_ms, status): (u64, StatusCode), error: ErrorDetail) -> NodeReply {
    NodeReply::Failed {
        outcome: Outcome::NodeError { latency_ms, error },
        contact: Contact::Answered(status),
    }
}

/// An `error` message; every message here starts with fixed text, so it is
/// never empty.
fn text(message: impl Into<String>) -> ErrorDetail {
    ErrorDetail::Text(message.into())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::failed;
    use crate::dispatch::{DispatchError, DispatchOptions};
    use ferrofed_registry::id::EndpointId;
    use http::Method;
    use openehr_its::rest::client::ClientError;
    use url::Url;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Whether the classification of `error` is a gateway-side compose
    /// failure, the one place a call no node answered can land.
    fn is_compose(error: ClientError) -> Result<bool, Box<dyn std::error::Error>> {
        let endpoint = EndpointId::new("node-a-pub")?;
        Ok(matches!(
            failed(
                (&endpoint, &BTreeSet::new()),
                error,
                0,
                &DispatchOptions::new(
                    std::time::Instant::now(),
                    crate::onward::conveyance::tests::conveyance(),
                ),
            ),
            Err(DispatchError::Compose { .. })
        ))
    }

    #[test]
    #[expect(
        clippy::panic_in_result_fn,
        reason = "a test asserts, and returns its setup errors"
    )]
    fn the_client_errors_no_node_can_cause_are_compose_failures() -> TestResult {
        let base = Url::parse("data:text/plain,not-a-base")?;
        assert!(is_compose(ClientError::BaseUrl { base })?);
        let build = http::Request::builder().uri("http://[::1").body(());
        let Err(source) = build else {
            return Err("an unparsable URI built a request".into());
        };
        assert!(is_compose(ClientError::Build {
            method: Method::POST,
            path: "/query/aql".to_owned(),
            source,
        })?);
        let Err(source) = http::HeaderName::from_bytes(b"not a name") else {
            return Err("an illegal header name parsed".into());
        };
        assert!(is_compose(ClientError::HeaderName {
            header: "not a name".to_owned(),
            source,
        })?);
        let Err(source) = http::HeaderValue::from_str("line\nbreak") else {
            return Err("a line break parsed as a header value".into());
        };
        assert!(is_compose(ClientError::HeaderValue {
            header: "X-Request-Id".to_owned(),
            source,
        })?);
        assert!(is_compose(ClientError::UnsupportedMediaType {
            requested: "application/xml".to_owned(),
        })?);
        let Err(source) = serde_json::from_str::<u8>("not json") else {
            return Err("an illegal JSON text parsed".into());
        };
        assert!(is_compose(ClientError::Serialize { source })?);
        Ok(())
    }
}

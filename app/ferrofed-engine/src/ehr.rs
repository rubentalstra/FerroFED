// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! EHR creation and retrieval on one node, the two calls the admission check
//! of §12b.1 makes: `POST {base}/v1/ehr` and `GET {base}/v1/ehr/{ehr_id}`,
//! as openEHR ITS-REST 1.1.0 names them.
//!
//! Both go through the generated `EhrClient` of `openehr-its`'s
//! `rest-client`, with the endpoint's onward credentials, the call's deadline,
//! the caller's identity signed for the node and the gateway's minted
//! `X-Request-Id`, and both pass the outbound gate
//! first: the URL and every header the gateway sets are read against the
//! identifiers the options withhold (§5.4.1, N33). The `EHR_STATUS` a create
//! carries is a write body, which the gate never reads (§5.4 scope note).
//!
//! An error names the endpoint and the node's status, never the node's body,
//! which may echo the `EHR_STATUS` it was sent.

use openehr_its::rest::client::{ClientError, ErrorBody, Transport, TransportError};
use openehr_its::rest::generated::ehr::client::{EhrClient, EhrCreateOutcome, EhrGetByIdOutcome};
use openehr_its::rest::generated::ehr::{EhrCreateParams, EhrGetByIdParams};
use openehr_rm::v1_2::ehr::ehr::Ehr;
use openehr_rm::v1_2::ehr::ehr_status::EhrStatus;

use crate::dispatch::{DispatchOptions, NodeClient, OptionsError};
use crate::hygiene::{Composed, Outbound, Part};
use crate::onward::conveyance::ConveyanceError;
use ferrofed_registry::id::EndpointId;
use http::StatusCode;

/// The `Prefer` value a create sends: the node answers with the `ehr_id` in
/// `ETag` and `Location` and no body (ITS-REST 1.1.0 EHR API, `Prefer`).
const PREFER_MINIMAL: &str = "return=minimal";

/// An EHR call that got no usable answer from the node.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EhrCallError {
    /// The outbound gate found a withheld identifier in the request, so
    /// nothing was sent (§5.4.1, N33).
    #[error(
        "the request to endpoint {endpoint} would carry a patient identifier in {part}, so it was not sent"
    )]
    Withheld {
        /// The endpoint.
        endpoint: EndpointId,
        /// The part of the request that carried it; never the value.
        part: Part,
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
    /// The node was sent the request and did not answer before the deadline.
    #[error("endpoint {endpoint} did not answer before the deadline")]
    TimeOut {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the client runtime reported.
        #[source]
        source: Box<ClientError>,
    },
    /// The node could not be reached: a refused connection or a broken
    /// stream.
    #[error("endpoint {endpoint} could not be reached")]
    Unreachable {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the client runtime reported.
        #[source]
        source: Box<ClientError>,
    },
    /// The node answered with an error status the operation documents
    /// (`400` or `409` for a create, `404` for a read).
    #[error("endpoint {endpoint} answered {status}")]
    Rejected {
        /// The endpoint.
        endpoint: EndpointId,
        /// The node's status.
        status: StatusCode,
        /// The node's body, as received; never displayed.
        body: ErrorBody,
    },
    /// The node answered a create with success and named no `ehr_id` in
    /// `ETag` or `Location`.
    #[error("endpoint {endpoint} created an EHR and named no ehr_id in ETag or Location")]
    Unnamed {
        /// The endpoint.
        endpoint: EndpointId,
        /// The node's success status, `201` or `204`.
        status: StatusCode,
    },
    /// Any other failure: no credential, a request that could not be
    /// composed, the node refusing the credentials, a `5xx`, a status the
    /// operation does not document, or a body that is not an `EHR`.
    #[error("the call to endpoint {endpoint} failed")]
    Failed {
        /// The endpoint.
        endpoint: EndpointId,
        /// What the client runtime reported, the node's status included.
        #[source]
        source: Box<ClientError>,
    },
}

impl<T: Transport> NodeClient<T> {
    /// Creates an EHR on the node with `status` as its `EHR_STATUS`, and
    /// returns the `ehr_id` the node assigned, as the node wrote it.
    ///
    /// The `ehr_id` is read from `ETag`, the `ehr_id` in double quotes, or
    /// else from the last segment of `Location` (ITS-REST 1.1.0 EHR API,
    /// `201_EHR`).
    ///
    /// # Errors
    ///
    /// Returns [`EhrCallError::Withheld`] with nothing sent,
    /// [`EhrCallError::Expired`] when the deadline passed before it left,
    /// [`EhrCallError::TimeOut`] and [`EhrCallError::Unreachable`] when the
    /// node gave no answer, [`EhrCallError::Rejected`] for a `400` or `409`,
    /// [`EhrCallError::Unnamed`] for a success naming no `ehr_id`, and
    /// [`EhrCallError::Failed`] for every other failure.
    pub async fn create_ehr(
        &self,
        status: &EhrStatus,
        options: &DispatchOptions,
    ) -> Result<String, EhrCallError> {
        let params = EhrCreateParams {
            prefer: Some(PREFER_MINIMAL.to_owned()),
            accept: None,
            content_type: None,
            openehr_version: None,
            openehr_audit_details: None,
        };
        self.gate_ehr("/ehr", &[("Prefer", PREFER_MINIMAL)], options)?;
        let call = options
            .call_options(self.endpoint())
            .map_err(|error| self.options_failure(error))?;
        let answer = EhrClient::new(self.client())
            .with_options(call)
            .ehr_create(&params, Some(status))
            .await
            .map_err(|source| self.ehr_failure(source))?;
        let (status, etag, location) = match answer {
            EhrCreateOutcome::Created { headers, .. } => {
                (StatusCode::CREATED, headers.etag, headers.location)
            }
            EhrCreateOutcome::NoContent { headers } => {
                (StatusCode::NO_CONTENT, headers.etag, headers.location)
            }
            EhrCreateOutcome::BadRequest { body } => {
                return Err(self.rejected(StatusCode::BAD_REQUEST, body));
            }
            EhrCreateOutcome::Conflict { body } => {
                return Err(self.rejected(StatusCode::CONFLICT, body));
            }
        };
        etag.as_deref()
            .and_then(from_etag)
            .or_else(|| location.as_deref().and_then(from_location))
            .ok_or_else(|| EhrCallError::Unnamed {
                endpoint: self.endpoint().clone(),
                status,
            })
    }

    /// Reads the EHR `ehr_id` names from the node.
    ///
    /// # Errors
    ///
    /// Returns [`EhrCallError::Withheld`] with nothing sent,
    /// [`EhrCallError::Expired`] when the deadline passed before it left,
    /// [`EhrCallError::TimeOut`] and [`EhrCallError::Unreachable`] when the
    /// node gave no answer, [`EhrCallError::Rejected`] for a `404`, and
    /// [`EhrCallError::Failed`] for every other failure, a body that is not an
    /// `EHR` included.
    pub async fn read_ehr(
        &self,
        ehr_id: &str,
        options: &DispatchOptions,
    ) -> Result<Ehr, EhrCallError> {
        let params = EhrGetByIdParams {
            ehr_id: ehr_id.to_owned(),
            accept: None,
        };
        let path = format!(
            "/ehr/{}",
            openehr_its::rest::client::path_segment(&params.ehr_id)
        );
        self.gate_ehr(&path, &[], options)?;
        let call = options
            .call_options(self.endpoint())
            .map_err(|error| self.options_failure(error))?;
        let answer = EhrClient::new(self.client())
            .with_options(call)
            .ehr_get_by_id(&params)
            .await
            .map_err(|source| self.ehr_failure(source))?;
        match answer {
            EhrGetByIdOutcome::Ok { body, .. } => Ok(body),
            EhrGetByIdOutcome::NotFound { body } => Err(self.rejected(StatusCode::NOT_FOUND, body)),
        }
    }

    /// The outbound gate over an EHR call to `path` carrying `headers`
    /// (§5.4.1, N33).
    fn gate_ehr(
        &self,
        path: &str,
        headers: &[(&'static str, &str)],
        options: &DispatchOptions,
    ) -> Result<(), EhrCallError> {
        let withheld = options.withheld();
        if withheld.is_empty() {
            return Ok(());
        }
        let base = self.base();
        let mut url = base.clone();
        url.set_path(&format!("{}{path}", base.path().trim_end_matches('/')));
        let conveyed = options.conveyance().carried();
        let outbound = Outbound {
            aql: "",
            scope: None,
            paging: &[],
            url: &url,
            composed: Composed::default(),
            headers,
            conveyed: &conveyed,
        };
        match withheld.found_in(&outbound) {
            Some(part) => Err(EhrCallError::Withheld {
                endpoint: self.endpoint().clone(),
                part,
            }),
            None => Ok(()),
        }
    }

    /// The error for a documented error answer of the node's.
    fn rejected(&self, status: StatusCode, body: ErrorBody) -> EhrCallError {
        EhrCallError::Rejected {
            endpoint: self.endpoint().clone(),
            status,
            body,
        }
    }

    /// The error for call options that could not be made, so nothing was
    /// sent.
    fn options_failure(&self, error: OptionsError) -> EhrCallError {
        match error {
            OptionsError::Conveyance(source) => EhrCallError::Conveyance {
                endpoint: self.endpoint().clone(),
                source,
            },
            OptionsError::Client(source) => self.ehr_failure(source),
        }
    }

    /// The error for a call that reached no documented answer.
    ///
    /// The client makes one attempt, so `DeadlineElapsed` comes before the
    /// request reaches the transport: a request never sent.
    fn ehr_failure(&self, error: ClientError) -> EhrCallError {
        let endpoint = self.endpoint().clone();
        match error {
            ClientError::DeadlineElapsed { .. } => EhrCallError::Expired {
                endpoint,
                source: Box::new(error),
            },
            ClientError::Transport {
                source: TransportError::Timeout { .. },
                ..
            } => EhrCallError::TimeOut {
                endpoint,
                source: Box::new(error),
            },
            ClientError::Transport { .. } => EhrCallError::Unreachable {
                endpoint,
                source: Box::new(error),
            },
            other => EhrCallError::Failed {
                endpoint,
                source: Box::new(other),
            },
        }
    }
}

/// The `ehr_id` an `ETag` carries: the value in double quotes, weak or
/// strong (RFC 9110 §8.8.3), or `None` for an empty or malformed tag.
fn from_etag(etag: &str) -> Option<String> {
    let tag = etag.trim();
    let tag = tag.strip_prefix("W/").unwrap_or(tag);
    tag.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The `ehr_id` a `Location` names: its last non-empty path segment,
/// percent-decoded, or `None` when it has none.
fn from_location(location: &str) -> Option<String> {
    let url = url::Url::parse(location.trim()).ok()?;
    let segment = url.path_segments()?.rfind(|segment| !segment.is_empty())?;
    let decoded = crate::hygiene::decode::percent_decoded(segment);
    (!decoded.is_empty()).then_some(decoded)
}

#[cfg(test)]
mod tests {
    use super::{from_etag, from_location};

    #[test]
    fn an_etag_names_the_ehr_id_weak_or_strong() {
        let id = "7d44b88c-4199-4bad-97dc-d78268e01398";
        assert_eq!(Some(id.to_owned()), from_etag(&format!("\"{id}\"")));
        assert_eq!(Some(id.to_owned()), from_etag(&format!("W/\"{id}\"")));
        assert_eq!(None, from_etag(id), "an unquoted tag is malformed");
        assert_eq!(None, from_etag("\"\""), "an empty tag names nothing");
    }

    #[test]
    fn a_location_names_the_ehr_id_in_its_last_segment() {
        let id = "7d44b88c-4199-4bad-97dc-d78268e01398";
        assert_eq!(
            Some(id.to_owned()),
            from_location(&format!("https://cdr.example.org/openehr/v1/ehr/{id}"))
        );
        assert_eq!(
            Some(id.to_owned()),
            from_location(&format!("https://cdr.example.org/v1/ehr/{id}/"))
        );
        assert_eq!(None, from_location("not a url"));
    }
}

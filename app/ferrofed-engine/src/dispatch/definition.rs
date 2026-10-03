// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A stored-query definition at a node, distributed or read back (§12.7,
//! N44).
//!
//! The registry's copy is sent with the generated
//! `definition_query_version_store_yaml` (`PUT
//! {base}/v1/definition/query/{name}/{version}`), and the node's copy is read
//! back with `definition_query_version_get` for the drift check. Both are
//! composed by `openehr-its`'s `rest-client` and pass the outbound
//! gate first (§5.4.1, N33). What the node made of the call is one §11.1
//! status, as for a dispatched query, and a `node-error` carries what one
//! does: the node's HTTP status and an excerpt of its message
//! ([`super::reported`], §9.5, §12.6 item 2).

use std::time::Instant;

use http::StatusCode;
use openehr_federation::outcome::{ErrorDetail, Outcome};
use openehr_its::rest::client::{ClientError, Transport, TransportError, path_segment};
use openehr_its::rest::generated::definition::client::{
    DefinitionClient, DefinitionQueryVersionGetOutcome, DefinitionQueryVersionStoreYamlOutcome,
};
use openehr_its::rest::generated::definition::{
    DefinitionQueryVersionGetParams, DefinitionQueryVersionStoreYamlParams,
};

use super::{Contact, DispatchError, DispatchOptions, NodeClient, classify, reported};
use crate::hygiene::{Composed, Outbound};

/// The query language a distributed definition is stored as, the ITS-REST
/// `query_type`.
const AQL: &str = "AQL";

/// One stored-query definition at one version, as the gateway sends it to a
/// node or asks a node for it.
#[derive(Debug, Clone, Copy)]
pub struct DefinitionAt<'a> {
    /// The qualified name, `[{namespace}::]{query-name}`.
    pub name: &'a str,
    /// The version, `major.minor.patch`.
    pub version: &'a str,
}

/// What a node holds at a definition's name and version.
#[derive(Debug, Clone)]
pub enum NodeCopy {
    /// The node answered `200` with its copy.
    Held {
        /// The AQL of the node's copy.
        aql: String,
        /// The gateway's measurement of the request, in milliseconds.
        latency_ms: u64,
    },
    /// The node answered `404`: it holds no copy.
    Missing {
        /// The gateway's measurement of the request, in milliseconds.
        latency_ms: u64,
    },
    /// The node was asked and gave no copy; the outcome is `offline`,
    /// `time-out` or `node-error`, always with its `error`.
    Failed {
        /// The endpoint outcome.
        outcome: Outcome,
        /// What the request showed of the node.
        contact: Contact,
    },
}

impl NodeCopy {
    /// Returns what the read showed of the node: a copy held is the
    /// node's `200`, and a copy missing its `404`.
    #[must_use]
    pub fn contact(&self) -> Contact {
        match self {
            Self::Held { .. } => Contact::Answered(StatusCode::OK),
            Self::Missing { .. } => Contact::Answered(StatusCode::NOT_FOUND),
            Self::Failed { contact, .. } => *contact,
        }
    }
}

/// What a node made of a definition the gateway sent it.
#[derive(Debug, Clone)]
pub struct Stored {
    /// The endpoint outcome: `active` when the node answered `200`.
    pub outcome: Outcome,
    /// What the request showed of the node.
    pub contact: Contact,
}

impl<T: Transport> NodeClient<T> {
    /// Stores `aql` at the node as `definition` and reports what the node
    /// made of it: `active` when it answered `200`, with the node's own
    /// status beside the outcome.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError`] when the request could not leave the
    /// gateway: a withheld identifier in it, no credential, or a request the
    /// client runtime refuses to build.
    pub async fn store_definition(
        &self,
        definition: DefinitionAt<'_>,
        aql: &str,
        options: &DispatchOptions,
    ) -> Result<Stored, DispatchError> {
        self.gate_definition(definition, aql, options)?;
        let params = DefinitionQueryVersionStoreYamlParams {
            qualified_query_name: definition.name.to_owned(),
            version: definition.version.to_owned(),
            query_type: Some(AQL.to_owned()),
            accept: None,
        };
        let started = Instant::now();
        let answer = DefinitionClient::new(self.client())
            .with_options(self.definition_call(options)?)
            .definition_query_version_store_yaml(&params, aql)
            .await;
        let latency_ms = classify::elapsed_ms(started);
        match answer {
            Ok(DefinitionQueryVersionStoreYamlOutcome::Ok { .. }) => Ok(Stored {
                outcome: Outcome::Active { latency_ms },
                contact: Contact::Answered(StatusCode::OK),
            }),
            Ok(DefinitionQueryVersionStoreYamlOutcome::BadRequest { body }) => Ok(node_error(
                (latency_ms, StatusCode::BAD_REQUEST),
                reported::answered(StatusCode::BAD_REQUEST, &body, options.withheld()),
            )),
            Ok(DefinitionQueryVersionStoreYamlOutcome::Conflict { body }) => Ok(node_error(
                (latency_ms, StatusCode::CONFLICT),
                reported::answered(StatusCode::CONFLICT, &body, options.withheld()),
            )),
            Err(error) => self.definition_failure(error, latency_ms, options),
        }
    }

    /// Asks the node for its copy of `definition`.
    ///
    /// # Errors
    ///
    /// Returns [`DispatchError`] when the request could not leave the
    /// gateway: a withheld identifier in it, no credential, or a request the
    /// client runtime refuses to build.
    pub async fn read_definition(
        &self,
        definition: DefinitionAt<'_>,
        options: &DispatchOptions,
    ) -> Result<NodeCopy, DispatchError> {
        self.gate_definition(definition, "", options)?;
        let params = DefinitionQueryVersionGetParams {
            qualified_query_name: definition.name.to_owned(),
            version: definition.version.to_owned(),
            accept: None,
        };
        let started = Instant::now();
        let answer = DefinitionClient::new(self.client())
            .with_options(self.definition_call(options)?)
            .definition_query_version_get(&params)
            .await;
        let latency_ms = classify::elapsed_ms(started);
        match answer {
            Ok(DefinitionQueryVersionGetOutcome::Ok { body, .. }) => Ok(NodeCopy::Held {
                aql: body.q,
                latency_ms,
            }),
            Ok(DefinitionQueryVersionGetOutcome::NotFound { .. }) => {
                Ok(NodeCopy::Missing { latency_ms })
            }
            Err(error) => self
                .definition_failure(error, latency_ms, options)
                .map(|Stored { outcome, contact }| NodeCopy::Failed { outcome, contact }),
        }
    }

    /// The `openehr-its` call options of `options`.
    fn definition_call(
        &self,
        options: &DispatchOptions,
    ) -> Result<openehr_its::rest::client::CallOptions, DispatchError> {
        options
            .call_options(&self.endpoint)
            .map_err(|error| DispatchError::of_options(&self.endpoint, error))
    }

    /// The outcome of a call that reached no documented answer, carrying the
    /// node's status and an excerpt of its message with no identifier of
    /// `options` withholds, or the gateway-side error when nothing left.
    fn definition_failure(
        &self,
        error: ClientError,
        latency_ms: u64,
        options: &DispatchOptions,
    ) -> Result<Stored, DispatchError> {
        let withheld = options.withheld();
        let failed = |message: String| ErrorDetail::Text(message);
        let late = || Outcome::TimeOut {
            latency_ms,
            error: failed("no answer before the deadline".to_owned()),
        };
        match error {
            ClientError::DeadlineElapsed { .. } => Ok(Stored {
                outcome: late(),
                contact: Contact::Unsent,
            }),
            ClientError::Transport {
                source: TransportError::Timeout { .. },
                ..
            } => Ok(Stored {
                outcome: late(),
                contact: Contact::Silent,
            }),
            ClientError::Transport {
                source: TransportError::Send { .. },
                ..
            } => Ok(Stored {
                outcome: Outcome::Offline {
                    latency_ms,
                    error: failed("the node could not be reached".to_owned()),
                },
                contact: Contact::Silent,
            }),
            ClientError::Unauthorized { body, .. } => Ok(node_error(
                (latency_ms, StatusCode::UNAUTHORIZED),
                reported::said(
                    format!(
                        "the node refused the gateway's onward credentials with {}",
                        StatusCode::UNAUTHORIZED
                    ),
                    &body,
                    withheld,
                ),
            )),
            ClientError::Forbidden { body, .. } => Ok(node_error(
                (latency_ms, StatusCode::FORBIDDEN),
                reported::answered(StatusCode::FORBIDDEN, &body, withheld),
            )),
            ClientError::ServiceFailure { status, body, .. }
            | ClientError::UndocumentedStatus { status, body, .. } => Ok(node_error(
                (latency_ms, status),
                reported::answered(status, &body, withheld),
            )),
            ClientError::Body { status, .. } => Ok(node_error(
                (latency_ms, status),
                failed(format!(
                    "the node answered {status} with a body that is not an ITS-REST StoredQuery"
                )),
            )),
            ClientError::Credentials { source, .. } => Ok(Stored {
                outcome: Outcome::NodeError {
                    latency_ms,
                    error: reported::unauthenticated(&source, &self.endpoint, options.request_id()),
                },
                contact: Contact::Unsent,
            }),
            other => Err(DispatchError::Compose {
                endpoint: self.endpoint.clone(),
                source: Box::new(other),
            }),
        }
    }

    /// The outbound gate over a definition request: its path and the AQL it
    /// carries (§5.4.1, N33).
    fn gate_definition(
        &self,
        definition: DefinitionAt<'_>,
        aql: &str,
        options: &DispatchOptions,
    ) -> Result<(), DispatchError> {
        if options.withheld.is_empty() {
            return Ok(());
        }
        let base = self.client.base();
        let mut url = base.clone();
        url.set_path(&format!(
            "{}/definition/query/{}/{}",
            base.path().trim_end_matches('/'),
            path_segment(&definition.name),
            path_segment(&definition.version)
        ));
        let headers: [(&'static str, &str); 0] = [];
        let conveyed = options.conveyance().carried();
        let outbound = Outbound {
            aql,
            scope: None,
            paging: &[],
            url: &url,
            composed: Composed::default(),
            headers: &headers,
            conveyed: &conveyed,
        };
        match options.withheld.found_in(&outbound) {
            Some(part) => Err(DispatchError::Withheld {
                endpoint: self.endpoint.clone(),
                part,
            }),
            None => Ok(()),
        }
    }
}

/// The `node-error` of a node that answered `status`, carrying `error`.
fn node_error((latency_ms, status): (u64, StatusCode), error: ErrorDetail) -> Stored {
    Stored {
        outcome: Outcome::NodeError { latency_ms, error },
        contact: Contact::Answered(status),
    }
}

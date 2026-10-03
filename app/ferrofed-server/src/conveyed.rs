// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! What every request to a node is told about the caller: the verified
//! [`Caller`] of the request, signed for each node by the gateway's
//! [`Signer`] (§13.1, N24, N25, §12.4, CP-16).
//!
//! The gateway names itself to a node it holds an OAuth 2.0 grant at by the
//! `client_id` of that grant, the `iss` of its client assertions there, and
//! to every other node by the federation's identifier (§7a.2, N30). A request
//! that reaches dispatch without a verified caller is the gateway's own
//! failure, answered `500` with nothing sent: the guard puts a caller on
//! every request it admits, so none should arrive without one. The
//! gateway's own requests for its operator, the admission check and the
//! distribution of a held stored-query version, convey the gateway itself.

use std::sync::Arc;

use axum::response::Response;
use ferrofed_engine::onward::conveyance::{
    self, Conveyance, Principal, Purpose, Signer, Verification,
};
use openehr_federation::id::FederationId;

use crate::auth::caller::{Caller, VerifiedBy};
use crate::config::settings::{Scheme, Settings};
use crate::error::{self, Code};
use crate::federation::{Federation, FederationError};

/// A request that cannot convey whom it is on behalf of, so nothing is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Unconveyed {
    /// The request reached dispatch with no verified caller.
    #[error("the request reached dispatch with no verified caller")]
    NoCaller,
    /// The federation holds no key to sign the conveyance with.
    #[error("the federation holds no key to sign the caller's identity with")]
    Unsigned,
}

impl Unconveyed {
    /// The answer to the request: `500`, logged under the gateway's `logged`
    /// id.
    #[must_use]
    pub fn respond(self, request_id: &str, logged: &str) -> Response {
        tracing::error!(
            error = %self,
            request_id = logged,
            "a request to a node could not convey its caller, so nothing was sent"
        );
        error::fixed(Code::Internal, request_id)
    }
}

/// The signer of `settings`'s `[signing]` key, naming the gateway `id` to
/// every node and the `client_id` of its grant to a node it holds one at.
///
/// # Errors
///
/// Returns [`FederationError::Unsigned`] when `[signing]` is not set.
pub(crate) fn signer(settings: &Settings, id: &FederationId) -> Result<Signer, FederationError> {
    let Some(signing) = &settings.signing else {
        return Err(FederationError::Unsigned);
    };
    let mut signer = Signer::new(Arc::clone(&signing.keys), id.as_str());
    for (endpoint, scheme) in &settings.credentials {
        if let Scheme::OAuth2(grant) = scheme {
            signer = signer.with_issuer_at(endpoint.clone(), grant.client_id());
        }
    }
    Ok(signer)
}

/// The conveyance of `caller` to the nodes of `federation`.
///
/// # Errors
///
/// Returns [`Unconveyed::NoCaller`] when there is no verified caller, and
/// [`Unconveyed::Unsigned`] when the federation holds no signer.
pub fn of(federation: &Federation, caller: Option<&Caller>) -> Result<Conveyance, Unconveyed> {
    let signer = federation.signer().ok_or(Unconveyed::Unsigned)?;
    let caller = caller.ok_or(Unconveyed::NoCaller)?;
    Ok(Conveyance::new(
        Arc::clone(signer),
        Principal::Caller(conveyed(caller)),
    ))
}

/// The conveyance of the gateway itself, for its operator, to the nodes of
/// `federation`.
///
/// # Errors
///
/// Returns [`Unconveyed::Unsigned`] when the federation holds no signer.
pub fn gateway(federation: &Federation) -> Result<Conveyance, Unconveyed> {
    let signer = federation.signer().ok_or(Unconveyed::Unsigned)?;
    Ok(Conveyance::new(Arc::clone(signer), Principal::Gateway))
}

/// What a node is told of `caller`; its `client_id` and anything about a
/// patient are not among it (§5.4.1, N33).
fn conveyed(caller: &Caller) -> conveyance::Caller {
    conveyance::Caller {
        issuer: caller.issuer().to_owned(),
        subject: caller.subject().to_owned(),
        organisation: caller.organisation().map(str::to_owned),
        purposes: caller
            .purposes()
            .iter()
            .map(|purpose| Purpose {
                system: purpose.system.clone(),
                code: purpose.code.clone(),
            })
            .collect(),
        scope: caller.granted().to_owned(),
        verified_by: match caller.verified_by() {
            VerifiedBy::Signature => Verification::Signature,
            VerifiedBy::Introspection => Verification::Introspection,
            VerifiedBy::Edge => Verification::Edge,
        },
    }
}

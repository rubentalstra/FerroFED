// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! What a node is told about the caller: the [`HEADER`] every request to a
//! node carries, a compact JWS the gateway signs for that one node (§13.1,
//! N24, N25, §12.4, CP-16).
//!
//! The token is signed with the current key of the gateway's
//! [`KeyRing`], the key whose public half the gateway publishes as its JWK
//! Set and declares as `federation.auth.jwks_uri` (N30), so a node verifies
//! it the way it verifies the gateway's client assertions. Its JOSE header
//! names [`ALGORITHM`], the key's `kid` and the type [`TYPE`] (RFC 8725
//! §3.11). Its claims:
//!
//! | Claim | Value |
//! |---|---|
//! | `iss` | the gateway, as the node knows it ([`Signer::issuer_at`]) |
//! | `aud` | the node's `endpoint_id` |
//! | `iat`, `exp` | whole seconds of the wall clock, `exp` [`LIFETIME`] after `iat` |
//! | `jti` | a fresh version 4 UUID |
//! | `sub` | the caller's subject; the gateway's `iss` for the gateway's own request |
//! | `iss_upstream` | the issuer that vouched for the caller |
//! | `verified_by` | how the gateway verified the caller: `signature`, `introspection` or `edge` |
//! | `subject_organization_id` | the caller's organisation, when its token names one (IHE IUA) |
//! | `purpose_of_use` | the caller's purposes of use, each a `system` and `code` (IHE IUA, HL7 v3 `PurposeOfUse`) |
//! | `scope` | the caller's scopes as granted |
//!
//! It never carries `person_id` or any other patient identifier (§5.4.1,
//! N33): a [`Caller`] holds no field for one, and the outbound gate reads
//! every caller claim against the identifiers a request withholds
//! ([`crate::hygiene`]). The caller's own token is never in it. §13.1 leaves
//! end-user conveyance open, so the header and its claims are FerroFED's own
//! design.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use ferrofed_registry::id::EndpointId;
use jsonwebtoken::Header;
use serde::Serialize;

use crate::onward::keys::{ALGORITHM, KeyRing};

/// The header every request to a node carries the caller's identity in.
pub const HEADER: &str = "openEHR-federation-client";

/// The JOSE `typ` of the token (RFC 8725 §3.11), so a node cannot take it
/// for an access token or a client assertion.
pub const TYPE: &str = "openehr-federation-client+jwt";

/// How long a token is valid after it is signed.
// NOTE: no specification governs this: our own design; one minute outlasts any
// per-node budget and keeps a captured token short-lived.
pub const LIFETIME: Duration = Duration::from_secs(60);

/// How the gateway verified a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verification {
    /// A signed access token, against its issuer's key set (RFC 9068).
    Signature,
    /// An access token its issuer's introspection endpoint called active
    /// (RFC 7662).
    Introspection,
    /// The edge's signed assertion: the edge authenticated the caller, and
    /// the gateway verified what the edge asserted.
    Edge,
}

impl Verification {
    /// The `verified_by` claim's value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Signature => "signature",
            Self::Introspection => "introspection",
            Self::Edge => "edge",
        }
    }
}

/// One purpose of use: a code and the system that defines it, the HL7 v3
/// `PurposeOfUse` coding IHE IUA conveys (§13.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Purpose {
    /// The code system, when the caller's token names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// The code.
    pub code: String,
}

/// The caller as the gateway verified it, the facts a node is told.
///
/// It holds no patient identifier and no credential. `Debug` shows how the
/// caller was verified and no value.
#[derive(Clone, PartialEq, Eq)]
pub struct Caller {
    /// The issuer that vouched for the caller.
    pub issuer: String,
    /// The caller's subject.
    pub subject: String,
    /// The caller's organisation, when its token names one.
    pub organisation: Option<String>,
    /// The caller's purposes of use.
    pub purposes: Vec<Purpose>,
    /// The caller's scopes as granted, space-separated.
    pub scope: String,
    /// How the gateway verified the caller.
    pub verified_by: Verification,
}

impl fmt::Debug for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Caller")
            .field("verified_by", &self.verified_by)
            .finish_non_exhaustive()
    }
}

/// On whose behalf a request reaches a node.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Principal {
    /// A caller the gateway verified.
    Caller(Caller),
    /// The gateway itself, for its operator, with no client caller: the
    /// admission check and the operator's stored-query distribution.
    Gateway,
}

/// The gateway's signer of conveyances: its keys, and the `iss` each node
/// knows it by.
#[derive(Debug)]
pub struct Signer {
    keys: Arc<KeyRing>,
    issuer: String,
    issuers: BTreeMap<EndpointId, String>,
}

impl Signer {
    /// A signer with the current key of `keys`, naming the gateway `issuer`
    /// to every node.
    #[must_use]
    pub fn new(keys: Arc<KeyRing>, issuer: impl Into<String>) -> Self {
        Self {
            keys,
            issuer: issuer.into(),
            issuers: BTreeMap::new(),
        }
    }

    /// This signer, naming the gateway `issuer` to `endpoint`.
    ///
    /// An endpoint the gateway obtains an OAuth 2.0 token from knows the
    /// gateway as the `client_id` of its client assertions, so the two name
    /// the gateway the same way.
    #[must_use]
    pub fn with_issuer_at(mut self, endpoint: EndpointId, issuer: impl Into<String>) -> Self {
        self.issuers.insert(endpoint, issuer.into());
        self
    }

    /// The `iss` `endpoint` receives.
    #[must_use]
    pub fn issuer_at(&self, endpoint: &EndpointId) -> &str {
        self.issuers.get(endpoint).unwrap_or(&self.issuer)
    }

    /// The keys the signer signs with.
    #[must_use]
    pub fn keys(&self) -> &KeyRing {
        &self.keys
    }
}

/// A conveyance that could not be signed, so nothing was sent.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConveyanceError {
    /// The token could not be signed.
    #[error("the caller's identity could not be signed for the node")]
    Sign(#[source] jsonwebtoken::errors::Error),
}

/// The identity one client request conveys to every node it reaches: the
/// signer and the principal.
///
/// `Debug` names the principal's kind and no value.
#[derive(Debug, Clone)]
pub struct Conveyance {
    signer: Arc<Signer>,
    principal: Arc<Principal>,
}

/// The claims of one token.
#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
    jti: String,
    sub: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    iss_upstream: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verified_by: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_organization_id: Option<&'a str>,
    #[serde(skip_serializing_if = "<[Purpose]>::is_empty")]
    purpose_of_use: &'a [Purpose],
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

impl Conveyance {
    /// The conveyance of `principal`, signed by `signer`.
    #[must_use]
    pub fn new(signer: Arc<Signer>, principal: Principal) -> Self {
        Self {
            signer,
            principal: Arc::new(principal),
        }
    }

    /// On whose behalf the request reaches a node.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The [`HEADER`] value for `endpoint`: a compact JWS, its `aud` the
    /// endpoint's id, valid for [`LIFETIME`] from now.
    ///
    /// # Errors
    ///
    /// Returns [`ConveyanceError::Sign`] when the key cannot sign.
    pub fn signed_for(&self, endpoint: &EndpointId) -> Result<String, ConveyanceError> {
        let iss = self.signer.issuer_at(endpoint);
        let iat = jiff::Timestamp::now().as_second();
        let lifetime = i64::try_from(LIFETIME.as_secs()).unwrap_or(i64::MAX);
        let mut claims = Claims {
            iss,
            aud: endpoint.as_str(),
            iat,
            exp: iat.saturating_add(lifetime),
            jti: uuid::Uuid::new_v4().to_string(),
            sub: iss,
            iss_upstream: None,
            verified_by: None,
            subject_organization_id: None,
            purpose_of_use: &[],
            scope: None,
        };
        if let Principal::Caller(caller) = self.principal.as_ref() {
            claims.sub = &caller.subject;
            claims.iss_upstream = Some(&caller.issuer);
            claims.verified_by = Some(caller.verified_by.as_str());
            claims.subject_organization_id = caller.organisation.as_deref();
            claims.purpose_of_use = &caller.purposes;
            claims.scope = Some(caller.scope.as_str()).filter(|scope| !scope.is_empty());
        }
        let key = self.signer.keys.current();
        let mut header = Header::new(ALGORITHM);
        header.typ = Some(TYPE.to_owned());
        header.kid = Some(key.kid().to_owned());
        jsonwebtoken::encode(&header, &claims, key.private()).map_err(ConveyanceError::Sign)
    }

    /// Every claim value that comes from the caller's credential, which the
    /// outbound gate reads against the identifiers a request withholds.
    ///
    /// The gateway's own `iss`, the node's `aud` and the minted `iat`,
    /// `exp` and `jti` come from no request, and are not among them.
    #[must_use]
    pub fn carried(&self) -> Vec<&str> {
        let Principal::Caller(caller) = self.principal.as_ref() else {
            return Vec::new();
        };
        let mut carried = vec![
            caller.subject.as_str(),
            caller.issuer.as_str(),
            caller.scope.as_str(),
        ];
        carried.extend(caller.organisation.as_deref());
        for purpose in &caller.purposes {
            carried.extend(purpose.system.as_deref());
            carried.push(&purpose.code);
        }
        carried
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, LazyLock};
    use std::time::Duration;

    use ferrofed_registry::id::EndpointId;
    use secrecy::SecretString;

    use super::{Caller, Conveyance, Principal, Purpose, Signer, Verification};
    use crate::onward::SystemClock;
    use crate::onward::keys::{KeyRing, SigningKey};

    /// The keys of every unit test's conveyance, generated once.
    static KEYS: LazyLock<Arc<KeyRing>> = LazyLock::new(|| {
        let pem = ferrofed_testkit::oauth::es384_pem().expect("a test key should generate");
        let key = SigningKey::from_pem(&SecretString::from(pem)).expect("an ES384 key");
        Arc::new(
            KeyRing::new(key, None, Duration::ZERO, Arc::new(SystemClock))
                .expect("one key is a ring"),
        )
    });

    fn caller() -> Caller {
        Caller {
            issuer: "https://issuer.example.test".to_owned(),
            subject: "clinician-0042".to_owned(),
            organisation: Some("urn:oid:2.999.7".to_owned()),
            purposes: vec![Purpose {
                system: Some("http://terminology.hl7.org/CodeSystem/v3-ActReason".to_owned()),
                code: "TREAT".to_owned(),
            }],
            scope: "user/aql-*.s".to_owned(),
            verified_by: Verification::Edge,
        }
    }

    fn signer() -> Arc<Signer> {
        Arc::new(Signer::new(Arc::clone(&KEYS), "urn:example:gateway"))
    }

    /// A conveyance of a synthetic verified caller, for a unit test that
    /// dispatches.
    pub(crate) fn conveyance() -> Conveyance {
        Conveyance::new(signer(), Principal::Caller(caller()))
    }

    #[test]
    fn every_caller_claim_is_carried_to_the_gate_and_no_gateway_claim_is() {
        let conveyance = conveyance();
        let carried = conveyance.carried();
        for claim in [
            "clinician-0042",
            "https://issuer.example.test",
            "urn:oid:2.999.7",
            "http://terminology.hl7.org/CodeSystem/v3-ActReason",
            "TREAT",
            "user/aql-*.s",
        ] {
            assert!(carried.contains(&claim), "{claim} in {carried:?}");
        }
        let gateway = Conveyance::new(signer(), Principal::Gateway);
        assert!(gateway.carried().is_empty(), "the gateway's own claims");
    }

    #[test]
    fn an_endpoint_without_an_issuer_of_its_own_gets_the_gateways() {
        let a = EndpointId::new("node-a-pub").expect("an id");
        let b = EndpointId::new("node-b-pub").expect("an id");
        let signer = Signer::new(Arc::clone(&KEYS), "urn:example:gateway")
            .with_issuer_at(a.clone(), "client-a");
        assert_eq!("client-a", signer.issuer_at(&a));
        assert_eq!("urn:example:gateway", signer.issuer_at(&b));
    }

    #[test]
    fn a_conveyance_shows_no_caller_value_in_debug() {
        let shown = format!("{:?}", conveyance());
        assert!(!shown.contains("clinician-0042"), "{shown}");
        assert!(shown.contains("Edge"), "{shown}");
    }
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The identity every test request conveys: a synthetic verified caller,
//! signed with a key generated once per test process, and the reading of a
//! conveyed token against the published JWK Set (§13.1, N24, N25).

use std::error::Error;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use ferrofed_engine::onward::SystemClock;
use ferrofed_engine::onward::conveyance::{
    Caller, Conveyance, Principal, Purpose, Signer, TYPE, Verification,
};
use ferrofed_engine::onward::keys::{ALGORITHM, KeyRing, SigningKey};
use ferrofed_testkit::oauth;
use jsonwebtoken::{DecodingKey, Validation};
use secrecy::SecretString;
use serde::Deserialize;

/// The `iss` the test gateway names itself by.
pub(crate) const GATEWAY: &str = "urn:example:ferrofed-under-test";

/// The issuer that vouched for the synthetic caller.
pub(crate) const UPSTREAM: &str = "https://issuer.example.test";

/// The synthetic caller's subject.
pub(crate) const SUBJECT: &str = "clinician-0042";

/// The synthetic caller's organisation.
pub(crate) const ORGANISATION: &str = "urn:oid:2.999.7";

/// The synthetic caller's scopes as granted.
pub(crate) const SCOPE: &str = "user/aql-*.s user/composition-*.cru";

/// The HL7 v3 `ActReason` code system the purpose of use is coded in.
pub(crate) const ACT_REASON: &str = "http://terminology.hl7.org/CodeSystem/v3-ActReason";

/// The signer of every test conveyance.
#[expect(
    clippy::expect_used,
    reason = "a test process that cannot generate a key pair cannot test anything"
)]
static SIGNER: LazyLock<Arc<Signer>> =
    LazyLock::new(|| Arc::new(signer().expect("a test key pair should generate")));

/// A signer over a fresh ES384 key, naming the gateway [`GATEWAY`].
pub(crate) fn signer() -> Result<Signer, Box<dyn Error>> {
    let key = SigningKey::from_pem(&SecretString::from(oauth::es384_pem()?))?;
    let keys = KeyRing::new(key, None, Duration::ZERO, Arc::new(SystemClock))?;
    Ok(Signer::new(Arc::new(keys), GATEWAY))
}

/// The shared test signer.
pub(crate) fn shared() -> Arc<Signer> {
    Arc::clone(&SIGNER)
}

/// The synthetic caller, verified by signature, treating.
pub(crate) fn caller() -> Caller {
    Caller {
        issuer: UPSTREAM.to_owned(),
        subject: SUBJECT.to_owned(),
        organisation: Some(ORGANISATION.to_owned()),
        purposes: vec![Purpose {
            system: Some(ACT_REASON.to_owned()),
            code: "TREAT".to_owned(),
        }],
        scope: SCOPE.to_owned(),
        verified_by: Verification::Signature,
    }
}

/// The conveyance of [`caller`] by the shared signer.
pub(crate) fn conveyance() -> Conveyance {
    Conveyance::new(shared(), Principal::Caller(caller()))
}

/// The conveyance of `caller` by the shared signer.
pub(crate) fn conveyance_of(caller: Caller) -> Conveyance {
    Conveyance::new(shared(), Principal::Caller(caller))
}

/// One purpose of use as a node reads it.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadPurpose {
    /// The code system.
    pub(crate) system: Option<String>,
    /// The code.
    pub(crate) code: String,
}

/// The claims of a conveyed token as a node reads them; any claim not named
/// here fails the read.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Read {
    pub(crate) iss: String,
    pub(crate) aud: String,
    pub(crate) iat: i64,
    pub(crate) exp: i64,
    pub(crate) jti: String,
    pub(crate) sub: String,
    pub(crate) iss_upstream: Option<String>,
    pub(crate) verified_by: Option<String>,
    pub(crate) subject_organization_id: Option<String>,
    #[serde(default)]
    pub(crate) purpose_of_use: Vec<ReadPurpose>,
    pub(crate) scope: Option<String>,
}

/// Verifies `token` as a node does: its `typ` and algorithm, its signature
/// against `keys`'s published JWK Set by `kid`, its `iss`, its `aud`
/// `audience` and its `exp`.
pub(crate) fn verified(
    token: &str,
    keys: &KeyRing,
    (issuer, audience): (&str, &str),
) -> Result<Read, Box<dyn Error>> {
    let header = jsonwebtoken::decode_header(token)?;
    if header.typ.as_deref() != Some(TYPE) || header.alg != ALGORITHM {
        return Err(format!("typ {:?}, alg {:?}", header.typ, header.alg).into());
    }
    let kid = header.kid.ok_or("the token names its key")?;
    let published = keys.published();
    let jwk = published.find(&kid).ok_or("the key is published")?;
    let mut validation = Validation::new(ALGORITHM);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    Ok(jsonwebtoken::decode::<Read>(token, &DecodingKey::from_jwk(jwk)?, &validation)?.claims)
}

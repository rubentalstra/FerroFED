// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The registry read from an mCSD care services directory (§15.1, N21,
//! Annex A.5).
//!
//! The members are the directory's `Organization`s that carry an
//! [`ORGANISATION_ID_SYSTEM`] identifier and its `Endpoint`s that carry an
//! [`ENDPOINT_ID_SYSTEM`] identifier, read with ITI-90 and kept in step with
//! ITI-91 through `ihe_iti`'s [`Replica`]. The rest of the directory is not
//! the federation's. The content then passes the same mapping and checks as
//! the registry document in FHIR form ([`snapshot_from_directory`]): the
//! connection-type rule of §15.2 (N19, CP-20), one managing organisation per
//! endpoint (N20), and every membership rule the native form holds. No
//! specification governs the selection by identifier system: our own design.

use std::fmt;
use std::time::{Duration, Instant};

use ferrofed_registry::secret::SecretUrl;
use ferrofed_registry::snapshot::RegistrySnapshot;
use http::header::{AUTHORIZATION, HeaderMap};
use ihe_iti::mcsd::budget::{Budget, Limits};
use ihe_iti::mcsd::client::McsdClient;
use ihe_iti::mcsd::error::{InvalidBase, McsdError};
use ihe_iti::mcsd::replica::{Refresh, Replica, Scope};
use jiff::fmt::rfc2822::DateTimeParser;
use jiff::{SignedDuration, Timestamp};
use openehr_its::rest::client::{Credentials, InvalidCredentials};
use thiserror::Error;
use url::Url;

use super::error::FhirFormError;
use super::{ENDPOINT_ID_SYSTEM, ORGANISATION_ID_SYSTEM, snapshot_from_directory};

/// The directory a registry is read from, as the configuration names it.
#[derive(Debug)]
pub struct DirectoryConfig {
    /// The directory's FHIR base URL, which `Debug` shows without its
    /// userinfo.
    pub base: SecretUrl,
    /// The credentials the gateway sends, when the transport does not
    /// authenticate it (ITI TF-2 Appendix Z.8).
    pub credentials: Option<Credentials>,
    /// How long one whole read or refresh may take, over every page of both
    /// resource types.
    pub deadline: Duration,
    /// The most pages one read or refresh may read.
    pub pages: usize,
    /// The most bytes of answer bodies one read or refresh may read.
    pub bytes: usize,
    /// The most Bundle entries one read or refresh may read.
    pub entries: usize,
}

/// A directory source that cannot be built.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DirectoryConfigError {
    /// The base URL does not parse as a URL.
    #[error("the directory base URL is not a URL")]
    BaseUrl(#[source] url::ParseError),
    /// The base URL is not an `http(s)` URL without a query or a fragment.
    #[error("the directory base URL is not an http(s) URL without a query or a fragment")]
    Base(#[source] InvalidBase),
    /// A credential does not form an `Authorization` value (RFC 7617 §2,
    /// RFC 6750 §2.1).
    #[error("the credentials of the directory cannot be sent in the Authorization header")]
    Credentials(#[source] InvalidCredentials),
    /// The HTTP client could not be built.
    #[error("the HTTP client for the directory could not be built")]
    Client(#[source] reqwest::Error),
}

/// Why the directory gave no registry.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DirectoryReadError {
    /// The exchange with the directory failed.
    #[error(transparent)]
    Exchange(ExchangeError),
    /// The directory's content is not a registry the gateway admits.
    #[error("the care services directory does not hold a valid registry")]
    Registry(#[source] FhirFormError),
}

/// An exchange with the directory that failed: ITI-90 or ITI-91 did not end
/// in directory content.
#[derive(Debug, Error)]
#[error("the care services directory could not be read")]
pub struct ExchangeError(#[source] McsdError);

impl ExchangeError {
    /// The HTTP status the directory answered with, when the gateway refused
    /// that status.
    #[must_use]
    pub fn status(&self) -> Option<http::StatusCode> {
        self.0.status()
    }

    /// Whether the directory answered at all.
    #[must_use]
    pub fn answered(&self) -> bool {
        self.0.answered()
    }

    /// Whether the read ran out of its budget: its deadline, its pages, its
    /// bytes or its entries.
    #[must_use]
    pub fn exceeded(&self) -> bool {
        self.0.exceeded()
    }
}

/// The directory content a registry was read from: where the next refresh
/// starts.
#[derive(Debug, Clone)]
pub struct Content(Replica);

impl Content {
    /// How many `Organization`s and `Endpoint`s of the federation the
    /// directory holds.
    #[must_use]
    pub fn len(&self) -> (usize, usize) {
        self.0.len()
    }

    /// Whether the directory holds no resource of the federation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The directory content a registry was read from, with the snapshot it
/// makes.
#[derive(Debug, Clone)]
pub struct Materialised {
    content: Content,
    snapshot: RegistrySnapshot,
}

impl Materialised {
    /// The directory content, which a refresh starts from.
    #[must_use]
    pub fn content(&self) -> &Content {
        &self.content
    }

    /// The registry the content makes.
    #[must_use]
    pub fn snapshot(&self) -> &RegistrySnapshot {
        &self.snapshot
    }

    /// The directory content and the registry.
    #[must_use]
    pub fn into_parts(self) -> (Content, RegistrySnapshot) {
        (self.content, self.snapshot)
    }
}

/// What a refresh of the directory found.
#[derive(Debug)]
pub enum Refreshed {
    /// No member changed; the content asks from a later instant next time.
    Unchanged(Content),
    /// Some member changed, and the content makes this registry.
    Changed(Box<Materialised>),
    /// Some member changed, and the content makes no registry the gateway
    /// admits; the registry in place stays.
    Refused(FhirFormError),
}

/// The care services directory a registry is read from.
///
/// `Debug` shows the base without its userinfo and leaves out the HTTP
/// client, whose default headers hold the credentials.
pub struct DirectorySource {
    client: McsdClient,
    deadline: Duration,
    limits: Limits,
}

impl DirectorySource {
    /// Builds the source `config` names.
    ///
    /// # Errors
    /// A [`DirectoryConfigError`] for a base URL that does not parse or that
    /// the client refuses, credentials no header can carry, or an HTTP client
    /// that cannot be built.
    pub fn new(config: DirectoryConfig) -> Result<Self, DirectoryConfigError> {
        let mut headers = HeaderMap::new();
        if let Some(credentials) = config.credentials {
            let header = credentials
                .header_value()
                .map_err(DirectoryConfigError::Credentials)?;
            headers.insert(AUTHORIZATION, header);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()
            .map_err(DirectoryConfigError::Client)?;
        let base = Url::parse(config.base.expose()).map_err(DirectoryConfigError::BaseUrl)?;
        let client = McsdClient::new(base, http).map_err(DirectoryConfigError::Base)?;
        Ok(Self {
            client,
            deadline: config.deadline,
            limits: Limits {
                pages: config.pages,
                bytes: config.bytes,
                entries: config.entries,
            },
        })
    }

    /// Reads the members from the directory with ITI-90 and checks them as a
    /// registry, within the deadline and the caps.
    ///
    /// # Errors
    /// [`DirectoryReadError::Exchange`] when the directory cannot be read,
    /// and [`DirectoryReadError::Registry`] when its content is no registry
    /// the gateway admits.
    pub async fn read(&self) -> Result<Materialised, DirectoryReadError> {
        let replica = Replica::read(&self.client, scope(), &mut self.budget())
            .await
            .map_err(|error| DirectoryReadError::Exchange(ExchangeError(error)))?;
        let snapshot = snapshot_of(&replica).map_err(DirectoryReadError::Registry)?;
        Ok(Materialised {
            content: Content(replica),
            snapshot,
        })
    }

    /// Asks the directory what changed since `held` was read, with ITI-91,
    /// and checks the changed content as a registry, within the deadline and
    /// the caps.
    ///
    /// It asks from [`OVERLAP`] before the `Date` the directory stamped the
    /// first answer of that read with, and reads everything again with ITI-90
    /// when the directory sent no `Date` it can read.
    ///
    /// # Errors
    /// The [`ExchangeError`] of an exchange that failed; `held` stays as it
    /// was.
    pub async fn refresh(&self, held: &Content) -> Result<Refreshed, ExchangeError> {
        match held
            .0
            .refreshed(
                &self.client,
                since(held.0.answered_at()).as_deref(),
                &mut self.budget(),
            )
            .await
            .map_err(ExchangeError)?
        {
            Refresh::Unchanged(replica) => Ok(Refreshed::Unchanged(Content(replica))),
            refresh => {
                let replica = refresh.into_replica();
                Ok(match snapshot_of(&replica) {
                    Ok(snapshot) => Refreshed::Changed(Box::new(Materialised {
                        content: Content(replica),
                        snapshot,
                    })),
                    Err(error) => Refreshed::Refused(error),
                })
            }
        }
    }
}

impl DirectorySource {
    /// A budget for one read or refresh, starting now.
    fn budget(&self) -> Budget {
        Budget::new(Instant::now() + self.deadline, self.limits)
    }
}

impl fmt::Debug for DirectorySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectorySource")
            .field("client", &self.client)
            .field("deadline", &self.deadline)
            .field("limits", &self.limits)
            .finish()
    }
}

/// How far before the directory's own clock reading a refresh asks from.
///
/// `_since` includes every version created at or after the instant (FHIR R4
/// history), and a version applied twice changes nothing, so the overlap
/// costs a re-read of recent versions and covers one committed while the
/// previous answer was written.
// NOTE: no specification governs this: our own design; ITI-91 leaves the
// instant to the Update Client (§3.91.4.1.1, "business rules").
pub const OVERLAP: SignedDuration = SignedDuration::from_secs(60);

/// The FHIR `instant` [`OVERLAP`] before the HTTP `Date` `answered_at`
/// (RFC 9110 §5.6.7), or `None` when there is none to read.
// NOTE: RFC 9110 §6.6.1: a Date that does not parse is treated as absent, which
// only makes the next refresh a full read, never a narrower one.
fn since(answered_at: Option<&str>) -> Option<String> {
    static PARSER: DateTimeParser = DateTimeParser::new();
    let at = PARSER.parse_timestamp(answered_at?).ok()?;
    Some(
        at.checked_sub(OVERLAP)
            .unwrap_or(Timestamp::MIN)
            .to_string(),
    )
}

/// The directory resources that are the federation's.
fn scope() -> Scope {
    Scope::identified(ORGANISATION_ID_SYSTEM, ENDPOINT_ID_SYSTEM)
}

/// The registry `replica` makes.
fn snapshot_of(replica: &Replica) -> Result<RegistrySnapshot, FhirFormError> {
    let directory = replica.directory().map_err(FhirFormError::Directory)?;
    snapshot_from_directory(&directory)
}

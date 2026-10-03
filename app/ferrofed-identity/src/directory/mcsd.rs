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
use std::time::Duration;

use ferrofed_registry::secret::SecretUrl;
use ferrofed_registry::snapshot::RegistrySnapshot;
use http::header::{AUTHORIZATION, HeaderMap};
use ihe_iti::mcsd::client::McsdClient;
use ihe_iti::mcsd::error::{InvalidBase, McsdError};
use ihe_iti::mcsd::replica::{Refresh, Replica, Scope};
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
    /// How long one page of an ITI-90 or ITI-91 answer may take.
    pub timeout: Duration,
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
    timeout: Duration,
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
            timeout: config.timeout,
        })
    }

    /// Reads the members from the directory with ITI-90 and checks them as a
    /// registry.
    ///
    /// # Errors
    /// [`DirectoryReadError::Exchange`] when the directory cannot be read,
    /// and [`DirectoryReadError::Registry`] when its content is no registry
    /// the gateway admits.
    pub async fn read(&self) -> Result<Materialised, DirectoryReadError> {
        let replica = Replica::read(&self.client, scope(), self.timeout)
            .await
            .map_err(|error| DirectoryReadError::Exchange(ExchangeError(error)))?;
        let snapshot = snapshot_of(&replica).map_err(DirectoryReadError::Registry)?;
        Ok(Materialised {
            content: Content(replica),
            snapshot,
        })
    }

    /// Asks the directory what changed since `held` was read, with ITI-91,
    /// and checks the changed content as a registry.
    ///
    /// # Errors
    /// The [`ExchangeError`] of an exchange that failed; `held` stays as it
    /// was.
    pub async fn refresh(&self, held: &Content) -> Result<Refreshed, ExchangeError> {
        match held
            .0
            .refreshed(&self.client, self.timeout)
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

impl fmt::Debug for DirectorySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectorySource")
            .field("client", &self.client)
            .field("timeout", &self.timeout)
            .finish()
    }
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

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The Query Client of ITI-90 and the Update Client of ITI-91 over the
//! `Organization` and `Endpoint` resources of a care services directory
//! (mCSD 4.0.0, ITI TF-2 §3.90, §3.91).
//!
//! [`McsdClient::find`] is ITI-90, Find Matching Care Services: a FHIR
//! `search-type` interaction, `GET [base]/[type]?[parameters]`, answered with
//! a `searchset` Bundle (`CapabilityStatement-IHE.mCSD.QueryClient`).
//! [`McsdClient::updates`] is ITI-91, Request Care Services Updates: a FHIR
//! `history-type` interaction, `GET [base]/[type]/_history?_since=[instant]`,
//! answered with a `history` Bundle, newest version first
//! (`CapabilityStatement-IHE.mCSD.UpdateClient`; FHIR R4 history,
//! <http://hl7.org/fhir/R4/http.html#history>). Both follow every `next` link
//! on the directory's own origin, so a caller gets the whole answer or an
//! error.

mod response;

use fhir_types::r4::endpoint::Endpoint;
use fhir_types::r4::organization::Organization;
use http::header::{ACCEPT, CONTENT_TYPE, DATE};
use std::fmt;
use url::Url;

use super::budget::Budget;
use super::error::{InvalidBase, McsdError};
use crate::redact::RedactedUrl;
use crate::search;

/// The media type ITI-90 and ITI-91 ask for and read (ITI TF-2 Appendix Z.6).
const FHIR_JSON: &str = "application/fhir+json";

/// The care service resource types a federation registry reads from a
/// directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum CareService {
    /// `Organization`.
    Organization,
    /// `Endpoint`.
    Endpoint,
}

impl CareService {
    /// The FHIR resource type name.
    #[must_use]
    pub const fn resource_type(self) -> &'static str {
        match self {
            Self::Organization => "Organization",
            Self::Endpoint => "Endpoint",
        }
    }
}

/// One `Organization` or `Endpoint` a directory answered with.
///
/// `Debug` shows the type and the logical id only: an `Endpoint`'s `header`
/// list may hold a credential.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CareResource {
    /// An `Organization`.
    Organization(Box<Organization>),
    /// An `Endpoint`.
    Endpoint(Box<Endpoint>),
}

impl CareResource {
    /// The resource's type.
    #[must_use]
    pub fn kind(&self) -> CareService {
        match self {
            Self::Organization(_) => CareService::Organization,
            Self::Endpoint(_) => CareService::Endpoint,
        }
    }

    /// The resource's logical id.
    #[must_use]
    pub fn logical_id(&self) -> Option<&str> {
        match self {
            Self::Organization(resource) => resource.id.as_deref(),
            Self::Endpoint(resource) => resource.id.as_deref(),
        }
    }

    /// Whether the resource carries an identifier in `system`.
    #[must_use]
    pub fn identified_in(&self, system: &str) -> bool {
        let identifiers = match self {
            Self::Organization(resource) => &resource.identifier,
            Self::Endpoint(resource) => &resource.identifier,
        };
        identifiers.iter().any(|identifier| {
            identifier
                .system
                .as_ref()
                .and_then(|uri| uri.value.as_deref())
                == Some(system)
        })
    }
}

impl fmt::Debug for CareResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CareResource")
            .field("kind", &self.kind())
            .field("logical_id", &self.logical_id())
            .finish()
    }
}

/// One resource an ITI-90 search matched, with the `fullUrl` its entry
/// carried.
#[derive(Clone, PartialEq, Eq)]
pub struct Match {
    full_url: String,
    resource: CareResource,
}

impl Match {
    /// The entry's `fullUrl`.
    #[must_use]
    pub fn full_url(&self) -> &str {
        &self.full_url
    }

    /// The resource.
    #[must_use]
    pub fn resource(&self) -> &CareResource {
        &self.resource
    }

    /// The `fullUrl` and the resource.
    #[must_use]
    pub fn into_parts(self) -> (String, CareResource) {
        (self.full_url, self.resource)
    }
}

impl fmt::Debug for Match {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Match")
            .field("full_url", &RedactedUrl(&self.full_url))
            .field("resource", &self.resource)
            .finish()
    }
}

/// What an ITI-90 search found, over every page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    matches: Vec<Match>,
    answered_at: Option<String>,
}

impl Found {
    /// Every match, in the order the pages listed them.
    #[must_use]
    pub fn matches(&self) -> &[Match] {
        &self.matches
    }

    /// Every match, by value.
    #[must_use]
    pub fn into_matches(self) -> Vec<Match> {
        self.matches
    }

    /// The `Date` the directory stamped its first page with (RFC 9110
    /// §6.6.1), as written, on the directory's own clock; `None` when it
    /// sent none.
    #[must_use]
    pub fn answered_at(&self) -> Option<&str> {
        self.answered_at.as_deref()
    }
}

/// What one ITI-91 history entry says of a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Version {
    /// The resource was created or updated to this version, with the
    /// `fullUrl` its entry carried.
    Current {
        /// The entry's `fullUrl`.
        full_url: String,
        /// The resource.
        resource: CareResource,
    },
    /// The resource was deleted.
    Deleted,
}

/// One entry of an ITI-91 history: a resource's logical id and what became
/// of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    logical_id: String,
    version: Version,
}

impl Change {
    /// The logical id of the resource the entry is about.
    #[must_use]
    pub fn logical_id(&self) -> &str {
        &self.logical_id
    }

    /// What became of it.
    #[must_use]
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// The logical id and the version.
    #[must_use]
    pub fn into_parts(self) -> (String, Version) {
        (self.logical_id, self.version)
    }
}

/// What an ITI-91 history answered, over every page: newest version first
/// (FHIR R4 history, "sorted with oldest versions last").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updates {
    changes: Vec<Change>,
    answered_at: Option<String>,
}

impl Updates {
    /// Every change, newest first.
    #[must_use]
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// Every change, by value, newest first.
    #[must_use]
    pub fn into_changes(self) -> Vec<Change> {
        self.changes
    }

    /// The `Date` the directory stamped its first page with (RFC 9110
    /// §6.6.1), as written, on the directory's own clock; `None` when it
    /// sent none.
    #[must_use]
    pub fn answered_at(&self) -> Option<&str> {
        self.answered_at.as_deref()
    }
}

/// A Query Client and Update Client bound to one care services directory.
///
/// `Debug` shows the base with its userinfo and query replaced by `***`, and
/// leaves out the HTTP client, whose default headers may hold a credential.
#[derive(Clone)]
pub struct McsdClient {
    base: Url,
    organization: Interactions,
    endpoint: Interactions,
    http: reqwest::Client,
}

/// The URLs of the two interactions on one resource type.
#[derive(Clone)]
struct Interactions {
    /// `[base]/[type]`, the `search-type` interaction.
    search: Url,
    /// `[base]/[type]/_history`, the `history-type` interaction.
    history: Url,
}

impl Interactions {
    /// The interactions on `kind` under the FHIR base `base`.
    fn of(base: &Url, kind: CareService) -> Result<Self, InvalidBase> {
        let search = search::under_base(base.clone(), kind.resource_type()).ok_or(InvalidBase)?;
        let history =
            search::under_base(base.clone(), &format!("{}/_history", kind.resource_type()))
                .ok_or(InvalidBase)?;
        Ok(Self { search, history })
    }
}

impl McsdClient {
    /// Creates a client for the directory whose FHIR base URL is `base`.
    ///
    /// `http` carries the transport the caller chose: TLS, client
    /// certificates, default authorization headers (ITI TF-2 Appendix Z.8).
    /// Build it with `redirect::Policy::none()`, so a `3xx` is an error like
    /// any other unexpected status and never takes the credentials elsewhere.
    ///
    /// # Errors
    /// [`InvalidBase`] when `base` is not an `http` or `https` URL without a
    /// query or a fragment.
    pub fn new(base: Url, http: reqwest::Client) -> Result<Self, InvalidBase> {
        let base = search::under_base(base, "").ok_or(InvalidBase)?;
        Ok(Self {
            organization: Interactions::of(&base, CareService::Organization)?,
            endpoint: Interactions::of(&base, CareService::Endpoint)?,
            base,
            http,
        })
    }

    /// The interactions on `kind`.
    fn interactions(&self, kind: CareService) -> &Interactions {
        match kind {
            CareService::Organization => &self.organization,
            CareService::Endpoint => &self.endpoint,
        }
    }

    /// Returns the directory's FHIR base URL, ending in `/`.
    #[must_use]
    pub fn base(&self) -> &Url {
        &self.base
    }

    /// Asks the directory for every resource of type `kind` that matches
    /// `parameters`, each a search parameter name and its value as FHIR search
    /// writes it (ITI-90, §3.90.4.1).
    ///
    /// Every page draws on `budget`: its deadline bounds the whole walk, and
    /// its pages, bytes and entries are spent as the pages are read.
    ///
    /// # Errors
    /// A [`McsdError`] for a page that is not a `searchset` Bundle of `kind`,
    /// a page link off the directory's origin, a budget that runs out, and a
    /// failure to get a page at all.
    pub async fn find(
        &self,
        kind: CareService,
        parameters: &[(&str, &str)],
        budget: &mut Budget,
    ) -> Result<Found, McsdError> {
        let mut url = self.interactions(kind).search.clone();
        if !parameters.is_empty() {
            url.query_pairs_mut().extend_pairs(parameters);
        }
        let mut matches = Vec::new();
        let mut answered_at = None;
        let mut next = Some(url);
        let mut first = true;
        while let Some(url) = next.take() {
            let page = self.page(url, budget).await?;
            if first {
                answered_at = page.date;
                first = false;
            }
            let read = response::searchset(&page.body, kind, &self.base)?;
            budget.entries(read.entries)?;
            matches.extend(read.matches);
            next = read.next;
        }
        Ok(Found {
            matches,
            answered_at,
        })
    }

    /// Asks the directory for every version of a resource of type `kind`
    /// created at or after `since`, a FHIR `instant` as written (ITI-91,
    /// §3.91.4.1; FHIR R4 history, `_since`).
    ///
    /// Every page draws on `budget`, as [`McsdClient::find`]'s do.
    ///
    /// # Errors
    /// A [`McsdError`] for a page that is not a `history` Bundle of `kind`,
    /// a page link off the directory's origin, a budget that runs out, and a
    /// failure to get a page at all.
    pub async fn updates(
        &self,
        kind: CareService,
        since: &str,
        budget: &mut Budget,
    ) -> Result<Updates, McsdError> {
        let mut url = self.interactions(kind).history.clone();
        url.query_pairs_mut().append_pair("_since", since);
        let mut changes = Vec::new();
        let mut answered_at = None;
        let mut next = Some(url);
        let mut first = true;
        while let Some(url) = next.take() {
            let page = self.page(url, budget).await?;
            if first {
                answered_at = page.date;
                first = false;
            }
            let read = response::history(&page.body, kind, &self.base)?;
            budget.entries(read.changes.len())?;
            changes.extend(read.changes);
            next = read.next;
        }
        Ok(Updates {
            changes,
            answered_at,
        })
    }

    /// Reads one page from `url`, within what is left of `budget`.
    async fn page(&self, url: Url, budget: &mut Budget) -> Result<response::Page, McsdError> {
        budget.page()?;
        // NOTE: no specification governs this: our own design, so a directory's
        // link cannot send this client's credentials to another origin.
        if url.origin() != self.base.origin() {
            return Err(McsdError::ForeignPage);
        }
        let answer = self
            .http
            .get(url)
            .header(ACCEPT, FHIR_JSON)
            .timeout(budget.remaining()?)
            .send()
            .await
            .map_err(response::transport)?;
        let status = answer.status();
        let media = answer
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let date = answer
            .headers()
            .get(DATE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response::body(answer, budget).await?;
        response::page(status, media.as_deref(), body, date)
    }
}

impl fmt::Debug for McsdClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McsdClient")
            .field("base", &RedactedUrl(self.base.as_str()))
            .finish_non_exhaustive()
    }
}

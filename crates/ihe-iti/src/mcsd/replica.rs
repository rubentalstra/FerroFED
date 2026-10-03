// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A replica of a directory's `Organization` and `Endpoint` resources, read
//! with ITI-90 and kept in step with ITI-91.
//!
//! [`Replica::read`] finds every resource in scope with ITI-90.
//! [`Replica::refreshed`] asks ITI-91 for the versions created since the last
//! read and applies them: for each logical id the newest version wins, a
//! deletion removes the resource, and a version that has left the scope
//! removes it too. A replica that does not know since when to ask reads
//! everything again. A replica is a value: a refresh returns a new one and
//! leaves the old one as it was, so a caller keeps the content it trusts until
//! it has checked the new content.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use fhir_types::r4::endpoint::Endpoint;
use fhir_types::r4::organization::Organization;
use jiff::{SignedDuration, Timestamp};

use super::client::{CareResource, CareService, Found, McsdClient, Updates, Version};
use super::directory::Directory;
use super::error::{DirectoryError, McsdError};
use crate::search::escape;

/// How far before the directory's own clock reading the next ITI-91 request
/// asks from.
///
/// `_since` includes every version created at or after the instant (FHIR R4
/// history), and a version applied twice leaves the replica unchanged, so the
/// overlap costs a re-read of recent versions and covers a version committed
/// while the previous answer was being written.
// NOTE: no specification governs this: our own design; ITI-91 leaves the
// instant to the Update Client (§3.91.4.1.1, "business rules").
pub const OVERLAP: SignedDuration = SignedDuration::from_secs(60);

/// Which resources of the directory the replica holds.
///
/// A resource type with an identifier system holds only the resources that
/// carry an identifier in that system; ITI-90 asks for them with the
/// `identifier` search parameter (`[system]|`, FHIR R4 token search), and an
/// ITI-91 version without one takes the resource out of the replica. A type
/// with none holds every resource.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    organizations: Option<String>,
    endpoints: Option<String>,
}

impl Scope {
    /// Every `Organization` and every `Endpoint` of the directory.
    #[must_use]
    pub fn everything() -> Self {
        Self::default()
    }

    /// The `Organization`s carrying an identifier in `organization_system`
    /// and the `Endpoint`s carrying one in `endpoint_system`.
    #[must_use]
    pub fn identified(
        organization_system: impl Into<String>,
        endpoint_system: impl Into<String>,
    ) -> Self {
        Self {
            organizations: Some(organization_system.into()),
            endpoints: Some(endpoint_system.into()),
        }
    }

    /// The identifier system `kind` is scoped to.
    fn system(&self, kind: CareService) -> Option<&str> {
        match kind {
            CareService::Organization => self.organizations.as_deref(),
            CareService::Endpoint => self.endpoints.as_deref(),
        }
    }

    /// Whether `resource` is in scope.
    fn holds(&self, resource: &CareResource) -> bool {
        self.system(resource.kind())
            .is_none_or(|system| resource.identified_in(system))
    }
}

/// The resources of one directory in scope, keyed by logical id, and the
/// instant the next ITI-91 request asks from.
#[derive(Clone, PartialEq, Eq)]
pub struct Replica {
    scope: Scope,
    organizations: BTreeMap<String, (String, Organization)>,
    endpoints: BTreeMap<String, (String, Endpoint)>,
    since: Option<Timestamp>,
}

/// What a refresh found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refresh {
    /// No resource in scope changed; the replica asks from a later instant.
    Unchanged(Replica),
    /// Some resource in scope changed.
    Changed(Replica),
}

impl Refresh {
    /// The refreshed replica.
    #[must_use]
    pub fn into_replica(self) -> Replica {
        match self {
            Self::Unchanged(replica) | Self::Changed(replica) => replica,
        }
    }
}

impl Replica {
    /// Reads every resource in `scope` from the directory `client` asks,
    /// with ITI-90; `timeout` bounds each page.
    ///
    /// # Errors
    /// The [`McsdError`] of the first search that fails.
    pub async fn read(
        client: &McsdClient,
        scope: Scope,
        timeout: Duration,
    ) -> Result<Self, McsdError> {
        let organizations = find(client, &scope, CareService::Organization, timeout).await?;
        let endpoints = find(client, &scope, CareService::Endpoint, timeout).await?;
        let since = earliest(organizations.answered_at(), endpoints.answered_at());
        let mut replica = Self {
            scope,
            organizations: BTreeMap::new(),
            endpoints: BTreeMap::new(),
            since: since.map(before),
        };
        for found in organizations
            .into_matches()
            .into_iter()
            .chain(endpoints.into_matches())
        {
            let (full_url, resource) = found.into_parts();
            replica.put(full_url, resource);
        }
        Ok(replica)
    }

    /// Asks the directory for what changed since this replica was read, with
    /// ITI-91, or reads everything again with ITI-90 when the replica does
    /// not know since when to ask; `timeout` bounds each page.
    ///
    /// # Errors
    /// The [`McsdError`] of the first request that fails; this replica is
    /// unchanged either way.
    pub async fn refreshed(
        &self,
        client: &McsdClient,
        timeout: Duration,
    ) -> Result<Refresh, McsdError> {
        let Some(since) = self.since else {
            let read = Self::read(client, self.scope.clone(), timeout).await?;
            return Ok(if read.same_content(self) {
                Refresh::Unchanged(read)
            } else {
                Refresh::Changed(read)
            });
        };
        let organizations = client
            .updates(CareService::Organization, since, timeout)
            .await?;
        let endpoints = client
            .updates(CareService::Endpoint, since, timeout)
            .await?;
        let mut next = self.clone();
        next.since = earliest(organizations.answered_at(), endpoints.answered_at()).map(before);
        next.apply(CareService::Organization, organizations);
        next.apply(CareService::Endpoint, endpoints);
        Ok(if next.same_content(self) {
            Refresh::Unchanged(next)
        } else {
            Refresh::Changed(next)
        })
    }

    /// The replica's resources as directory content: the organisations, then
    /// the endpoints, each in logical id order.
    ///
    /// # Errors
    /// A [`DirectoryError`] for a resource that carries a `modifierExtension`
    /// or repeats another's `fullUrl`, as a directory Bundle would be refused.
    pub fn directory(&self) -> Result<Directory, DirectoryError> {
        Directory::from_resources(
            self.organizations.values().cloned(),
            self.endpoints.values().cloned(),
        )
    }

    /// The instant the next ITI-91 request asks from, on the directory's
    /// clock; `None` when the next refresh reads everything again.
    #[must_use]
    pub fn since(&self) -> Option<Timestamp> {
        self.since
    }

    /// How many `Organization`s and `Endpoint`s the replica holds.
    #[must_use]
    pub fn len(&self) -> (usize, usize) {
        (self.organizations.len(), self.endpoints.len())
    }

    /// Whether the replica holds no resource.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.organizations.is_empty() && self.endpoints.is_empty()
    }

    /// Applies the history of `kind`, newest version first: the first
    /// entry for each logical id is that resource's state now.
    fn apply(&mut self, kind: CareService, updates: Updates) {
        let mut decided = BTreeSet::new();
        for change in updates.into_changes() {
            let (logical_id, version) = change.into_parts();
            if !decided.insert(logical_id.clone()) {
                continue;
            }
            match version {
                Version::Current { full_url, resource } => {
                    if self.scope.holds(&resource) {
                        self.put(full_url, resource);
                    } else {
                        self.remove(resource.kind(), &logical_id);
                    }
                }
                Version::Deleted => self.remove(kind, &logical_id),
            }
        }
    }

    /// Holds `resource` under its logical id, when it is in scope.
    fn put(&mut self, full_url: String, resource: CareResource) {
        if !self.scope.holds(&resource) {
            return;
        }
        let Some(id) = resource.logical_id().map(str::to_owned) else {
            return;
        };
        match resource {
            CareResource::Organization(resource) => {
                self.organizations.insert(id, (full_url, *resource));
            }
            CareResource::Endpoint(resource) => {
                self.endpoints.insert(id, (full_url, *resource));
            }
        }
    }

    /// Drops the resource of `kind` with `logical_id`.
    fn remove(&mut self, kind: CareService, logical_id: &str) {
        match kind {
            CareService::Organization => {
                self.organizations.remove(logical_id);
            }
            CareService::Endpoint => {
                self.endpoints.remove(logical_id);
            }
        }
    }

    /// Whether `other` holds the same resources, whatever it asks from next.
    fn same_content(&self, other: &Self) -> bool {
        self.organizations == other.organizations && self.endpoints == other.endpoints
    }
}

impl fmt::Debug for Replica {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Replica")
            .field("scope", &self.scope)
            .field("organizations", &self.organizations.len())
            .field("endpoints", &self.endpoints.len())
            .field("since", &self.since)
            .finish()
    }
}

/// ITI-90 over `kind`, asking for the resources in `scope`.
async fn find(
    client: &McsdClient,
    scope: &Scope,
    kind: CareService,
    timeout: Duration,
) -> Result<Found, McsdError> {
    match scope.system(kind) {
        Some(system) => {
            let token = format!("{}|", escape(system));
            client
                .find(kind, &[("identifier", token.as_str())], timeout)
                .await
        }
        None => client.find(kind, &[], timeout).await,
    }
}

/// The earlier of two clock readings, or `None` when either is missing.
fn earliest(first: Option<Timestamp>, second: Option<Timestamp>) -> Option<Timestamp> {
    Some(first?.min(second?))
}

/// The instant [`OVERLAP`] before `at`, or the earliest instant there is.
fn before(at: Timestamp) -> Timestamp {
    at.checked_sub(OVERLAP).unwrap_or(Timestamp::MIN)
}

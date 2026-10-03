// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! A replica of a directory's `Organization` and `Endpoint` resources, read
//! with ITI-90 and kept in step with ITI-91.
//!
//! [`Replica::read`] finds every resource in scope with ITI-90.
//! [`Replica::refreshed`] asks ITI-91 for the versions created since an instant
//! the caller chooses, usually from the directory's own clock reading at the
//! last read ([`Replica::answered_at`]), and applies them: for each logical id
//! the newest version wins, a deletion removes the resource, and a version
//! that has left the scope removes it too. With no instant it reads everything
//! again. Every read draws on one [`Budget`], and a read that runs out of it
//! is an error, never a shorter replica. A replica is a value: a refresh returns a new one and
//! leaves the old one as it was, so a caller keeps the content it trusts until
//! it has checked the new content.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use fhir_types::r4::endpoint::Endpoint;
use fhir_types::r4::organization::Organization;

use super::budget::Budget;
use super::client::{CareResource, CareService, Found, McsdClient, Updates, Version};
use super::directory::Directory;
use super::error::{DirectoryError, McsdError};
use crate::search::escape;

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
    answered_at: Option<String>,
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
    /// with ITI-90, both searches drawing on `budget`.
    ///
    /// # Errors
    /// The [`McsdError`] of the first search that fails.
    pub async fn read(
        client: &McsdClient,
        scope: Scope,
        budget: &mut Budget,
    ) -> Result<Self, McsdError> {
        let organizations = find(client, &scope, CareService::Organization, budget).await?;
        let endpoints = find(client, &scope, CareService::Endpoint, budget).await?;
        let answered_at = organizations.answered_at().map(str::to_owned);
        let mut replica = Self {
            scope,
            organizations: BTreeMap::new(),
            endpoints: BTreeMap::new(),
            answered_at,
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

    /// Asks the directory for what changed at or after `since`, a FHIR
    /// `instant` as written, with ITI-91, or reads everything again with
    /// ITI-90 when `since` is `None`; every request draws on `budget`.
    ///
    /// # Errors
    /// The [`McsdError`] of the first request that fails; this replica is
    /// unchanged either way.
    pub async fn refreshed(
        &self,
        client: &McsdClient,
        since: Option<&str>,
        budget: &mut Budget,
    ) -> Result<Refresh, McsdError> {
        let Some(since) = since else {
            let read = Self::read(client, self.scope.clone(), budget).await?;
            return Ok(if read.same_content(self) {
                Refresh::Unchanged(read)
            } else {
                Refresh::Changed(read)
            });
        };
        let organizations = client
            .updates(CareService::Organization, since, budget)
            .await?;
        let endpoints = client.updates(CareService::Endpoint, since, budget).await?;
        let mut next = self.clone();
        next.answered_at = organizations.answered_at().map(str::to_owned);
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

    /// The `Date` the directory stamped the first answer of the last read or
    /// refresh with, as written: the directory's own clock reading before any
    /// of that read's content; `None` when it sent none.
    #[must_use]
    pub fn answered_at(&self) -> Option<&str> {
        self.answered_at.as_deref()
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
            .field("answered_at", &self.answered_at)
            .finish()
    }
}

/// ITI-90 over `kind`, asking for the resources in `scope`.
async fn find(
    client: &McsdClient,
    scope: &Scope,
    kind: CareService,
    budget: &mut Budget,
) -> Result<Found, McsdError> {
    match scope.system(kind) {
        Some(system) => {
            let token = format!("{}|", escape(system));
            client
                .find(kind, &[("identifier", token.as_str())], budget)
                .await
        }
        None => client.find(kind, &[], budget).await,
    }
}

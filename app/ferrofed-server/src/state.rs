// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! What every handler shares: the phase of the process, the health registry,
//! the federation, and the stored-query registry.

use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use ferrofed_registry::definition::store::{DefinitionStore, Definitions, StoreError};
use ferrofed_registry::snapshot::RegistrySnapshot;

use crate::config::settings::Settings;
use crate::config::stored_queries::{Backend, Store};
use crate::config::transport::{self, CleartextError, ProtectedSite};
use crate::directory::DirectoryRegistry;
use crate::federation::{Federation, FederationError, read_registry};
use crate::health::lifecycle::Lifecycle;
use crate::health::{Built, HealthIndicator, Registry};
use crate::metrics::{Metrics, MetricsError};
use crate::stored;

/// The state the router is built over.
///
/// Every state starts booting, so readiness answers `503` until the run path
/// marks boot complete ([`Lifecycle::booted`]).
#[derive(Debug, Default)]
pub struct AppState {
    /// Where the process is in its life, which gates readiness.
    lifecycle: Lifecycle,
    /// The indicators readiness runs.
    health: Registry,
    /// The federation the ITS-REST façade queries, when a registry is set.
    ///
    /// A registry reload replaces it whole; a request takes the `Arc` once
    /// and keeps it to its end.
    federation: RwLock<Option<Arc<Federation>>>,
    /// The stored-query registry, when it is offered (§12.7). It sits beside
    /// the federation, which holds no store handle.
    definitions: Option<Arc<Definitions>>,
    /// The metrics surface every recording site and the admin listener
    /// share; it outlives every federation a reload builds.
    metrics: Arc<Metrics>,
    /// The care services directory the registry is kept in step with, when
    /// it is read from one; it outlives every federation a refresh builds.
    directory: Option<Arc<DirectoryRegistry>>,
}

/// A state that cannot be built from the settings.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StateError {
    /// The federation cannot be built.
    #[error(transparent)]
    Federation(#[from] FederationError),
    /// The stored-query registry's store cannot be opened or read.
    #[error(
        "the {backend} stored-query store{} could not be opened",
        path.as_ref().map(|path| format!(" {}", path.display())).unwrap_or_default()
    )]
    StoredQueries {
        /// The backend `stored_queries.backend` names.
        backend: Backend,
        /// The file or directory `stored_queries.path` names, for a backend
        /// that reads one; a connection string is never named.
        path: Option<PathBuf>,
        /// What the store reported.
        #[source]
        source: StoreError,
    },
    /// The metrics surface cannot be built.
    #[error(transparent)]
    Metrics(#[from] MetricsError),
    /// The gateway federates, and `[auth]` trusts no issuer, so no caller
    /// could ever be admitted (§13.1, N25).
    #[error(
        "the gateway federates, and [auth] names no [[auth.issuer]]: every caller authenticates (§13.1, N25)"
    )]
    NoIssuer,
    /// A credential would travel over a URL that is not `https`, outside the
    /// development profile.
    #[error(transparent)]
    Cleartext(#[from] CleartextError),
}

impl AppState {
    /// Returns the state `settings` describe.
    ///
    /// The registry document is loaded, the outbound clients built, and the
    /// stored-query store opened with every definition it holds read, before
    /// the gateway serves. Each subsystem built gets a [`Built`] indicator,
    /// and no member node or identity source gets one. The state is booting
    /// until the run path marks boot complete. The metrics surface is built
    /// with an OTLP push when `metrics.otlp_endpoint` is set, which needs a
    /// Tokio runtime context, and the federation records its node requests
    /// through it. Every credential is held to the transport rule of
    /// [`transport::check`] before anything that could send one is built, and
    /// under the development profile each one that travels unencrypted
    /// is logged as a warning.
    ///
    /// # Errors
    /// Returns a [`StateError`] when the federation `settings` describe
    /// cannot be built, a credential would travel in cleartext outside the
    /// development profile, the stored-query store cannot be opened or read,
    /// or the metrics surface cannot be built.
    pub fn build(settings: &Settings) -> Result<Self, StateError> {
        Self::build_read(settings, read_registry(settings))
    }

    /// Returns the state `settings` describe, over `document`, the registry
    /// document [`read_registry`] read from the same settings.
    ///
    /// The boot reads the document once, before the startup banner, and
    /// builds over that read here ([`Federation::load_read`]).
    ///
    /// # Errors
    /// Returns the [`StateError`] [`AppState::build`] returns, the read's own
    /// error included.
    pub fn build_read(
        settings: &Settings,
        document: Option<Result<RegistrySnapshot, FederationError>>,
    ) -> Result<Self, StateError> {
        settings.log_summary();
        let federation = Federation::load_read(settings, document)?;
        let cleartext = transport::check(settings, federation.as_ref().map(Federation::snapshot))?;
        transport::warn(&cleartext);
        let metrics = Arc::new(Metrics::new(&settings.metrics)?);
        let federation = federation.map(|federation| federation.metered(metrics.nodes()));
        let definitions = definitions(settings, federation.as_ref())?;
        let mut built: Vec<Arc<dyn HealthIndicator>> = vec![Arc::new(Built("configuration"))];
        if federation.is_some() {
            built.push(Arc::new(Built("registry")));
            built.push(Arc::new(Built("outbound_clients")));
        }
        if definitions.is_some() {
            built.push(Arc::new(Built("stored_queries")));
        }
        Ok(Self {
            lifecycle: Lifecycle::default(),
            health: Registry::new(built),
            federation: RwLock::new(federation.map(Arc::new)),
            definitions: definitions.map(Arc::new),
            metrics,
            directory: None,
        })
    }

    /// Checks what `settings` describe without serving: the federation
    /// builds, every credential travels over `https` unless the profile is
    /// development ([`transport::check`]), and a read-only stored-query
    /// directory loads. A shared or embedded store is not opened, so the
    /// check reaches no database and takes no file lock.
    ///
    /// Returns the credentials that travel unencrypted under the
    /// development profile, so the caller can say so.
    ///
    /// # Errors
    /// Returns the [`StateError`] [`AppState::build`] would return for the
    /// federation, the transport of a credential, or the definition files.
    pub fn check(settings: &Settings) -> Result<Vec<ProtectedSite>, StateError> {
        let federation = Federation::load(settings)?;
        let cleartext = transport::check(settings, federation.as_ref().map(Federation::snapshot))?;
        if let (Some(store @ Store::Files(_)), Some(federation)) =
            (&settings.stored_queries, federation.as_ref())
        {
            opened(store, federation)?;
        }
        Ok(cleartext)
    }

    /// Returns a booting state with `health` as its registry and no
    /// federation.
    #[must_use]
    pub fn with_health(health: Registry) -> Self {
        Self {
            lifecycle: Lifecycle::default(),
            health,
            federation: RwLock::new(None),
            definitions: None,
            metrics: Arc::default(),
            directory: None,
        }
    }

    /// Returns a booting state that serves the federated query over
    /// `federation`.
    #[must_use]
    pub fn with_federation(federation: Federation) -> Self {
        let metrics = Arc::new(Metrics::default());
        Self {
            lifecycle: Lifecycle::default(),
            health: Registry::default(),
            federation: RwLock::new(Some(Arc::new(federation.metered(metrics.nodes())))),
            definitions: None,
            metrics,
            directory: None,
        }
    }

    /// Returns this state keeping its registry in step with `directory`,
    /// whose last observed state `/health/dependencies` reports.
    #[must_use]
    pub fn watching(mut self, directory: Arc<DirectoryRegistry>) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Returns the report `GET /health/dependencies` answers with: the last
    /// observed state of each member endpoint, of the resolver, of the
    /// consent pre-filter, of the localizer and of the care services
    /// directory.
    #[must_use]
    pub fn dependencies(&self) -> crate::health::dependencies::Report {
        let mut report = self
            .federation()
            .map(|federation| federation.dependencies().report())
            .unwrap_or_default();
        report.directory = self.directory().map(|directory| directory.observed());
        report
    }

    /// Returns the care services directory the registry is kept in step
    /// with, when it is read from one.
    #[must_use]
    pub fn directory(&self) -> Option<&Arc<DirectoryRegistry>> {
        self.directory.as_ref()
    }

    /// Returns where the process is in its life, which gates readiness.
    #[must_use]
    pub const fn lifecycle(&self) -> &Lifecycle {
        &self.lifecycle
    }

    /// Returns the indicators readiness runs.
    #[must_use]
    pub const fn health(&self) -> &Registry {
        &self.health
    }

    /// Returns the federation, when the gateway federates.
    ///
    /// A request takes it once and keeps it to its end, so a registry reload
    /// never changes the membership under a running request.
    #[must_use]
    pub fn federation(&self) -> Option<Arc<Federation>> {
        // NOTE: no specification governs this: our own design; the lock guards
        // one `Arc` swap or clone, so a poisoned lock still holds a whole value.
        self.federation
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Puts `federation` in place of the running one, which the requests that
    /// already took it keep, and returns the running one.
    pub(crate) fn replace_federation(
        &self,
        federation: Arc<Federation>,
    ) -> Option<Arc<Federation>> {
        self.federation
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(federation)
    }

    /// Returns the metrics surface.
    #[must_use]
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    /// Returns the stored-query registry, when it is offered (§12.7).
    #[must_use]
    pub fn definitions(&self) -> Option<&Arc<Definitions>> {
        self.definitions.as_ref()
    }
}

/// Refuses a federating gateway whose `[auth]` trusts no issuer, which would
/// refuse every caller (§13.1, N25).
///
/// # Errors
/// Returns [`StateError::NoIssuer`] when `federates` and no issuer is
/// trusted.
pub fn admits_callers(settings: &Settings, federates: bool) -> Result<(), StateError> {
    if federates && settings.server.auth.issuers.is_empty() {
        return Err(StateError::NoIssuer);
    }
    Ok(())
}

/// The stored-query registry `settings` offer, over `federation`, which
/// executes its queries and admits the definitions a read-only directory
/// holds.
fn definitions(
    settings: &Settings,
    federation: Option<&Federation>,
) -> Result<Option<Definitions>, StateError> {
    // NOTE: no specification governs this: our own design; configuration
    // refuses a registry without a registry document, so both are set together.
    let (Some(store), Some(federation)) = (settings.stored_queries.as_ref(), federation) else {
        return Ok(None);
    };
    Definitions::open(opened(store, federation)?)
        .map(Some)
        .map_err(|source| refused(store, source))
}

/// The store `store` names, opened.
fn opened(store: &Store, federation: &Federation) -> Result<Box<dyn DefinitionStore>, StateError> {
    stored::open(store, federation.context()).map_err(|source| refused(store, source))
}

/// The refusal to open `store`, for `source`.
fn refused(store: &Store, source: StoreError) -> StateError {
    let path = match store {
        Store::Redb(path) | Store::Files(path) => Some(path.clone()),
        Store::Postgres(_) => None,
    };
    StateError::StoredQueries {
        backend: store.backend(),
        path,
        source,
    }
}

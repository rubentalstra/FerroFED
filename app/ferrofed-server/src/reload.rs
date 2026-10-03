// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Reloading the registry while the gateway serves.
//!
//! On `SIGHUP` the server reads its configuration again from the source it
//! started from, the file `--config` or `FERROFED_CONFIG` names and the
//! `FERROFED__` environment, and checks it exactly as at boot:
//! [`Config::load`], [`Config::resolve`], then
//! [`Federation::reloaded`](crate::federation::Federation::reloaded),
//! which is [`Federation::load`](crate::federation::Federation::load) over
//! what the process has learned, and [`transport::check`] under the profile
//! the process started with. A valid
//! registry replaces the running one at once; a request that already took
//! the running one finishes on it. Learned `creating_system_id` routes the
//! new document contradicts are withdrawn with their incidents, and index
//! entries and resolution bindings naming a member that left are dropped.
//! A configuration that does not load leaves the running registry in place.
//!
//! The sections in [`RELOADABLE`] take effect on a reload. A changed
//! `profile` refuses the reload, so every decision the development profile
//! admits reads the profile the process started with. Every other setting is
//! compared with the value the process started with, and a change is logged
//! as needing a restart while the rest of the reload applies. No
//! specification governs this: our own design.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use ferrofed_engine::dispatch::SetupError;
use ferrofed_identity::directory::error::FhirFormError;
use ferrofed_registry::error::LoadError;
use ferrofed_registry::id::{EndpointId, NodeId};

use crate::config::settings::Settings;
use crate::config::transport::{self, CleartextError, ProtectedSite};
use crate::config::{CONFIG_PATH_ENV, Config};
use crate::federation::{FederationError, Reconciled};
use crate::metrics::ReloadResult;
use crate::state::AppState;

/// The configuration sections a reload applies.
///
/// `registry` is the registry document, its path and its form, `credentials`
/// the outbound credentials of each endpoint, `dev` and `pixm` the
/// resolver, and `xcpd` the localizer, both of which name the members.
pub const RELOADABLE: [&str; 5] = ["registry", "credentials", "dev", "pixm", "xcpd"];

/// Reloads the registry the server started with.
///
/// It holds the settings the process started with, so a reload applies only
/// the [`RELOADABLE`] sections and reports a change to any other one.
pub struct Reloader {
    config: Option<PathBuf>,
    boot: Settings,
    state: Arc<AppState>,
    serial: Mutex<()>,
}

/// What an applied reload changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Applied {
    /// How many members the registry now holds.
    pub members: usize,
    /// The endpoints the new registry adds, in id order.
    pub endpoints_added: Vec<EndpointId>,
    /// The endpoints the new registry no longer holds, in id order: no
    /// request that starts after the reload calls them.
    pub endpoints_removed: Vec<EndpointId>,
    /// The members the new registry no longer holds, in id order.
    pub members_removed: Vec<NodeId>,
    /// What the reload did to what the process had learned.
    pub reconciled: Reconciled,
    /// The changed settings that take effect only on a restart, by key.
    pub needs_restart: Vec<&'static str>,
    /// The credentials that travel unencrypted, which only the
    /// development profile allows ([`transport::check`]).
    pub cleartext: Vec<ProtectedSite>,
}

/// A reload that was refused, leaving the running registry in place.
///
/// Its message names the failure and never a value of the configuration or
/// the document.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReloadError {
    /// The configuration could not be read or resolved.
    #[error("the configuration could not be loaded")]
    Config(#[source] crate::config::error::Error),
    /// The registry the configuration describes could not be built.
    #[error("the registry could not be loaded")]
    Federation {
        /// The registry document the configuration names.
        document: Option<PathBuf>,
        /// What the federation reported.
        #[source]
        source: Box<FederationError>,
    },
    /// `registry.document` was set or unset since the process started.
    #[error("registry.document was set or unset, which takes a restart")]
    RegistryPresence,
    /// `profile` differs from the profile the process started with. Every
    /// decision the development profile admits reads the boot profile, so a
    /// change takes a restart.
    #[error("profile was changed, which takes a restart")]
    Profile,
    /// A credential or a patient identifier would travel over a URL that is
    /// not `https`, outside the development profile the process started with.
    #[error("a credential or a patient identifier would travel in cleartext")]
    Cleartext(#[source] CleartextError),
}

impl ReloadError {
    /// The class of the failure, the field the refusal is logged under.
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            Self::Config(_) => "configuration",
            Self::Federation { source, .. } => federation_class(source),
            Self::RegistryPresence => "registry-presence",
            Self::Profile => "profile",
            Self::Cleartext(_) => "cleartext",
        }
    }

    /// The registry document the refusal is about, when it is about one.
    #[must_use]
    pub fn document(&self) -> Option<&std::path::Path> {
        match self {
            Self::Federation { document, .. } => document.as_deref(),
            Self::Config(_) | Self::RegistryPresence | Self::Profile | Self::Cleartext(_) => None,
        }
    }
}

impl Reloader {
    /// A reloader that reads the configuration from `config`, or from what
    /// `FERROFED_CONFIG` names when `config` is `None`, as the process did at
    /// boot with `boot` as the outcome, and swaps the federation of `state`.
    #[must_use]
    pub fn new(config: Option<PathBuf>, boot: Settings, state: Arc<AppState>) -> Self {
        Self {
            config,
            boot,
            state,
            serial: Mutex::new(()),
        }
    }

    /// Reloads the registry, logs the outcome, and returns it.
    ///
    /// The signal handler calls this; one reload runs at a time.
    ///
    /// # Errors
    /// Returns a [`ReloadError`] when the configuration or the registry does
    /// not load, or `registry.document` was set or unset; the running
    /// registry then stays in place.
    pub fn reload(&self) -> Result<Applied, ReloadError> {
        let serial = self.serial.lock().unwrap_or_else(PoisonError::into_inner);
        let outcome = self.apply();
        drop(serial);
        self.log(&outcome);
        outcome
    }

    /// Reads the configuration again and swaps in the registry it describes.
    fn apply(&self) -> Result<Applied, ReloadError> {
        let fresh = Config::load(self.config.as_deref())
            .and_then(|config| config.resolve())
            .map_err(ReloadError::Config)?;
        if fresh.registry_document.is_some() != self.boot.registry_document.is_some() {
            return Err(ReloadError::RegistryPresence);
        }
        // NOTE: no specification governs this: our own design; a reload under
        // another profile could admit what only development allows, so it is refused.
        if fresh.profile != self.boot.profile {
            return Err(ReloadError::Profile);
        }
        // NOTE: no specification governs this: our own design; a setting that
        // waits for a restart is held now, so the restart cannot stop on it.
        transport::check(&fresh, None).map_err(ReloadError::Cleartext)?;
        let needs_restart = needs_restart(&self.boot, &fresh);
        let effective = effective(&self.boot, fresh);
        let Some(running) = self.state.federation() else {
            return Ok(Applied {
                needs_restart,
                ..Applied::default()
            });
        };
        let next = running
            .reloaded(&effective)
            .map_err(|source| ReloadError::Federation {
                document: effective.registry_document.clone(),
                source: Box::new(source),
            })?
            .ok_or(ReloadError::RegistryPresence)?;
        let cleartext =
            transport::check(&effective, Some(next.snapshot())).map_err(ReloadError::Cleartext)?;
        let next = Arc::new(next);
        let (before, after) = (running.snapshot(), next.snapshot());
        let members_removed: Vec<NodeId> = before
            .nodes()
            .map(|node| node.id().clone())
            .filter(|node| after.node(node).is_none())
            .collect();
        let endpoints_removed = before
            .endpoints()
            .map(|endpoint| endpoint.id().clone())
            .filter(|endpoint| after.endpoint(endpoint).is_none())
            .collect();
        let endpoints_added = after
            .endpoints()
            .map(|endpoint| endpoint.id().clone())
            .filter(|endpoint| before.endpoint(endpoint).is_none())
            .collect();
        self.state.replace_federation(Arc::clone(&next));
        let departed: BTreeSet<NodeId> = members_removed.iter().cloned().collect();
        let reconciled = next.reconcile(&departed);
        Ok(Applied {
            members: after.nodes().count(),
            endpoints_added,
            endpoints_removed,
            members_removed,
            reconciled,
            needs_restart,
            cleartext,
        })
    }

    /// Logs an outcome, ids and counts, never a value of the configuration,
    /// the document, a credential or a header, and counts it on the metrics
    /// surface.
    fn log(&self, outcome: &Result<Applied, ReloadError>) {
        match outcome {
            Ok(applied) => {
                self.state.metrics().reloaded(ReloadResult::Applied);
                tracing::info!(
                    members = applied.members,
                    endpoints_added = joined(&applied.endpoints_added, EndpointId::as_str),
                    endpoints_removed = joined(&applied.endpoints_removed, EndpointId::as_str),
                    members_removed = joined(&applied.members_removed, NodeId::as_str),
                    incidents = applied.reconciled.incidents.len(),
                    index_dropped = applied.reconciled.index_dropped,
                    bindings_dropped = applied.reconciled.bindings_dropped,
                    "registry reloaded"
                );
                transport::warn(&applied.cleartext);
                if !applied.needs_restart.is_empty() {
                    tracing::warn!(
                        settings = applied.needs_restart.join(","),
                        "changed settings take effect only on a restart; the running values stay"
                    );
                }
            }
            Err(error) => {
                self.state.metrics().reloaded(ReloadResult::Refused);
                tracing::error!(
                    class = error.class(),
                    config = self.source().map(|path| path.display().to_string()),
                    document = error.document().map(|path| path.display().to_string()),
                    "registry reload refused, the running registry stays; `ferrofed config check` names the fault"
                );
            }
        }
    }

    /// The configuration file a reload reads, when one is named.
    fn source(&self) -> Option<PathBuf> {
        self.config
            .clone()
            .or_else(|| std::env::var_os(CONFIG_PATH_ENV).map(PathBuf::from))
    }
}

impl std::fmt::Debug for Reloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reloader")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Reloads the registry each time the process receives `SIGHUP`.
///
/// The reload reads files, so it runs on the blocking pool. A failure to
/// install the handler is logged, and the registry then changes only on a
/// restart.
pub async fn on_hangup(reloader: Arc<Reloader>) {
    use tokio::signal::unix::{SignalKind, signal};

    // NOTE: no specification governs this: our own design; SIGHUP is the
    // daemon reload signal, and a file watch could read a half-written document.
    let mut hangup = match signal(SignalKind::hangup()) {
        Ok(hangup) => hangup,
        Err(error) => {
            tracing::error!(%error, "cannot listen for SIGHUP; the registry reloads only on a restart");
            return;
        }
    };
    while hangup.recv().await.is_some() {
        tracing::info!("SIGHUP received, reloading the registry");
        let reloader = Arc::clone(&reloader);
        if let Err(error) = tokio::task::spawn_blocking(move || reloader.reload()).await {
            tracing::error!(
                panicked = error.is_panic(),
                "the registry reload did not finish; the running registry stays"
            );
        }
    }
}

/// The settings a reload builds the federation from: the [`RELOADABLE`]
/// sections of `fresh`, and every other setting as the process started.
fn effective(boot: &Settings, fresh: Settings) -> Settings {
    Settings {
        profile: boot.profile,
        server: boot.server.clone(),
        telemetry: boot.telemetry.clone(),
        registry_document: fresh.registry_document,
        registry_format: fresh.registry_format,
        federation: boot.federation.clone(),
        credentials: fresh.credentials,
        dev: fresh.dev,
        pixm: fresh.pixm,
        xcpd: fresh.xcpd,
        stored_queries: boot.stored_queries.clone(),
        metrics: boot.metrics.clone(),
        signing: boot.signing.clone(),
    }
}

/// Whether `fresh` names other signing keys, another JWK Set location or
/// another assertion lifetime than the process started with; the keys are
/// compared by `kid`, never by their material.
fn signing_changed(boot: &Settings, fresh: &Settings) -> bool {
    let shape = |settings: &Settings| {
        settings.signing.as_ref().map(|signing| {
            (
                signing.keys.current().kid().to_owned(),
                signing.keys.retiring().map(|key| key.kid().to_owned()),
                signing.jwks_uri.as_str().to_owned(),
                signing.assertion_lifetime,
            )
        })
    };
    shape(boot) != shape(fresh)
}

/// The keys outside [`RELOADABLE`] whose value in `fresh` differs from the
/// one the process started with.
fn needs_restart(boot: &Settings, fresh: &Settings) -> Vec<&'static str> {
    let (was, now) = (&boot.federation, &fresh.federation);
    [
        ("signing", signing_changed(boot, fresh)),
        ("server.listen", boot.server.listen != fresh.server.listen),
        (
            "server.base_path",
            boot.server.base_path != fresh.server.base_path,
        ),
        (
            "server.request_timeout_ms",
            boot.server.request_timeout != fresh.server.request_timeout,
        ),
        (
            "server.shutdown_timeout_ms",
            boot.server.shutdown_timeout != fresh.server.shutdown_timeout,
        ),
        (
            "server.body_limit_bytes",
            boot.server.body_limit != fresh.server.body_limit,
        ),
        (
            "telemetry.format",
            boot.telemetry.format != fresh.telemetry.format,
        ),
        (
            "telemetry.filter",
            boot.telemetry.filter != fresh.telemetry.filter,
        ),
        ("federation.id", was.id != now.id),
        ("federation.timeouts", was.budget != now.budget),
        (
            "federation.default_namespace",
            was.default_namespace != now.default_namespace,
        ),
        (
            "federation.binding_ttl_ms",
            was.binding_ttl != now.binding_ttl,
        ),
        (
            "federation.ehr_index_capacity",
            was.ehr_index_capacity != now.ehr_index_capacity,
        ),
        (
            "federation.node_selection",
            was.node_selection != now.node_selection,
        ),
        ("federation.best_effort", was.best_effort != now.best_effort),
        (
            "federation.fan_out_template_upload",
            was.fan_out_template_upload != now.fan_out_template_upload,
        ),
        (
            "federation.fan_out_stored_queries",
            was.fan_out_stored_queries != now.fan_out_stored_queries,
        ),
        ("federation.offset", was.offset != now.offset),
        (
            "federation.decomposable_aggregates",
            was.decomposable != now.decomposable,
        ),
        (
            "federation.demographic_endpoint",
            was.demographic_endpoint != now.demographic_endpoint,
        ),
        (
            "stored_queries",
            match (&boot.stored_queries, &fresh.stored_queries) {
                (Some(was), Some(now)) => !was.same_as(now),
                (None, None) => false,
                (Some(_), None) | (None, Some(_)) => true,
            },
        ),
        (
            "metrics.listen",
            boot.metrics.listen != fresh.metrics.listen,
        ),
        (
            "metrics.otlp_endpoint",
            boot.metrics.otlp_endpoint != fresh.metrics.otlp_endpoint,
        ),
    ]
    .into_iter()
    .filter_map(|(key, changed)| changed.then_some(key))
    .collect()
}

/// The class a refused federation is logged under.
fn federation_class(error: &FederationError) -> &'static str {
    match error {
        FederationError::Registry { source, .. } => match **source {
            LoadError::Read { .. } => "registry-unreadable",
            _ => "registry-invalid",
        },
        FederationError::FhirRegistry { source, .. } => match **source {
            FhirFormError::Read { .. } => "registry-unreadable",
            _ => "registry-invalid",
        },
        FederationError::DevWithoutRegistry
        | FederationError::DevTable(_)
        | FederationError::DevCrossRef(_) => "dev-cross-reference",
        FederationError::PixmWithoutRegistry
        | FederationError::PixmMember { .. }
        | FederationError::PixmNamespace(_)
        | FederationError::Pixm(_) => "pixm",
        FederationError::TwoResolvers => "resolvers",
        FederationError::Localization(_) => "localization",
        FederationError::NodeSelectionUndeclared | FederationError::IdUndeclared => "federation",
        FederationError::DemographicWithoutRegistry
        | FederationError::DemographicEndpointUnknown { .. } => "demographic-endpoint",
        FederationError::Describe(_) => "self-description",
        FederationError::Clients(SetupError::UnknownEndpoint { .. })
        | FederationError::Grant { .. } => "credentials",
        FederationError::Clients(_) => "node-clients",
        FederationError::Transport(_) => "http-client",
        FederationError::Unsigned => "signing",
    }
}

/// The ids in `ids`, comma-separated.
fn joined<T>(ids: &[T], name: impl Fn(&T) -> &str) -> String {
    ids.iter().map(name).collect::<Vec<_>>().join(",")
}

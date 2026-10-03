// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The registry read from an mCSD care services directory and kept in step
//! with it (`[registry.mcsd]`; §15.1, N21, Annex A.5).
//!
//! At boot the members are read with ITI-90 and checked as the registry
//! document in FHIR form is, and a directory that cannot be read, or holds
//! no valid registry, stops the boot. Every `refresh_interval_s` the changes
//! since the last read are asked for with ITI-91 off the clinical path, and
//! a changed registry goes through the same checks a reload does
//! ([`Reloader::directory_changed`]): the connection-type rule of §15.2
//! (N19, CP-20), one managing organisation per endpoint (N20), unique node,
//! endpoint and `system_id`s, the resolver's members and every other boot
//! check. A refresh that breaks one is refused: the running registry stays,
//! the refusal is logged and counted as a refused reload, and the next
//! refresh asks again from the same instant. Each read and refresh ends at
//! its deadline and its caps on pages, bytes and entries; one that runs past
//! them, or whose answer breaks ITI-91, is refused and counted the same way.
//! A directory that cannot be reached leaves the running registry in place;
//! every outcome shows on `/health/dependencies` as `directory`. A query never waits on the
//! directory. No specification governs the refresh policy: our own design.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ferrofed_identity::directory::mcsd::{
    Content, DirectoryConfig, DirectoryConfigError, DirectoryReadError, DirectorySource,
    ExchangeError, Materialised, Refreshed,
};
use ferrofed_registry::snapshot::RegistrySnapshot;
use openehr_its::rest::client::Credentials;

use crate::config::settings::{DirectorySettings, Scheme, Settings};
use crate::federation::{FederationError, read_registry};
use crate::health::dependencies::Observed;
use crate::reload::{Applied, ReloadError, Reloader};

/// The directory a running gateway keeps its registry in step with.
#[derive(Debug)]
pub struct DirectoryRegistry {
    source: DirectorySource,
    held: Mutex<Content>,
    observed: Mutex<Observed>,
    interval: Duration,
}

/// What one refresh of the directory did.
#[derive(Debug)]
#[non_exhaustive]
pub enum RefreshOutcome {
    /// No member changed.
    Unchanged,
    /// The changed registry replaced the running one.
    Applied(Applied),
    /// The refresh was refused, and the running registry stays: the changed
    /// registry breaks a rule, or the directory's answer ran past the deadline
    /// or a cap, or does not hold to ITI-91.
    Refused(ReloadError),
    /// The directory could not be reached or refused the request, and the
    /// running registry stays.
    Unreachable(ExchangeError),
}

impl DirectoryRegistry {
    /// Reads the members from the directory `settings` name with ITI-90, and
    /// returns the directory with the registry it holds.
    ///
    /// The read runs on a runtime of its own, so it blocks the caller.
    ///
    /// # Errors
    /// [`FederationError::Directory`] for a directory that cannot be asked as
    /// configured, a read that cannot run, and a directory that
    /// cannot be read or holds no valid registry.
    pub fn open(settings: &DirectorySettings) -> Result<(Self, RegistrySnapshot), FederationError> {
        let source = source(settings)?;
        let materialised = blocking(&source)?;
        let (content, snapshot) = materialised.into_parts();
        let registry = Self {
            source,
            held: Mutex::new(content),
            observed: Mutex::new(Observed::Up),
            interval: settings.refresh_interval,
        };
        Ok((registry, snapshot))
    }

    /// The last state observed of the directory, which
    /// `/health/dependencies` reports.
    #[must_use]
    pub fn observed(&self) -> Observed {
        *self.observed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks the directory for the changes since the registry in place was
    /// read and, when there are some, hands the registry they make to
    /// `reloader`, which replaces the running one or refuses it.
    ///
    /// The directory content advances only with a registry that was applied,
    /// so a refused or failed refresh asks again from the same instant.
    pub async fn refresh(&self, reloader: &Reloader) -> RefreshOutcome {
        let held = self.held().clone();
        let refreshed = match self.source.refresh(&held).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                self.observe(observed(&error));
                tracing::warn!(
                    answered = error.answered(),
                    status = error.status().map(|status| status.as_u16()),
                    error = crate::chain(&error),
                    "the care services directory could not be read; the running registry stays"
                );
                // NOTE: no specification governs this: our own design; an answer past
                // the budget, or one that breaks ITI-91, is refused as a broken registry is.
                if error.exceeded() || (error.answered() && error.status().is_none()) {
                    return RefreshOutcome::Refused(reloader.directory_refused(directory_error(
                        DirectoryFailure::Read(DirectoryReadError::Exchange(error)),
                    )));
                }
                return RefreshOutcome::Unreachable(error);
            }
        };
        self.observe(Observed::Up);
        match refreshed {
            Refreshed::Unchanged(replica) => {
                *self.held() = replica;
                RefreshOutcome::Unchanged
            }
            Refreshed::Changed(materialised) => {
                let (replica, snapshot) = materialised.into_parts();
                match reloader.directory_changed(snapshot) {
                    Ok(applied) => {
                        *self.held() = replica;
                        RefreshOutcome::Applied(applied)
                    }
                    Err(error) => RefreshOutcome::Refused(error),
                }
            }
            Refreshed::Refused(error) => RefreshOutcome::Refused(reloader.directory_refused(
                directory_error(DirectoryFailure::Read(DirectoryReadError::Registry(error))),
            )),
        }
    }

    /// Refreshes the registry every interval until the process ends.
    pub async fn keep_in_step(self: Arc<Self>, reloader: Arc<Reloader>) {
        let start = tokio::time::Instant::now() + self.interval;
        let mut ticks = tokio::time::interval_at(start, self.interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let outcome = self.refresh(&reloader).await;
            tracing::debug!(outcome = ?outcome, "care services directory refreshed");
        }
    }

    fn held(&self) -> MutexGuard<'_, Content> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn observe(&self, observed: Observed) {
        *self.observed.lock().unwrap_or_else(PoisonError::into_inner) = observed;
    }
}

/// Why the registry could not be read from the care services directory.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DirectoryFailure {
    /// The directory could not be read, or its content is no registry the
    /// gateway admits.
    #[error(transparent)]
    Read(DirectoryReadError),
    /// The directory cannot be asked as `[registry.mcsd]` describes it.
    #[error("the care services directory of [registry.mcsd] cannot be asked")]
    Source(#[source] DirectoryConfigError),
    /// The runtime a read outside the server runs on could not be built.
    #[error("the runtime for reading the care services directory could not be built")]
    Runtime(#[source] std::io::Error),
}

/// `failure` as the federation error a boot or a reload reports.
fn directory_error(failure: DirectoryFailure) -> FederationError {
    FederationError::Directory(Box::new(failure))
}

/// The registry's first read and, when it comes from a care services
/// directory, the directory the gateway then keeps it in step with.
///
/// A document is read as [`read_registry`] reads it; a directory is read
/// once here, so the banner, the build and the refreshes share one read.
#[must_use]
pub fn read_source(
    settings: &Settings,
) -> (
    Option<Result<RegistrySnapshot, FederationError>>,
    Option<Arc<DirectoryRegistry>>,
) {
    match &settings.registry_directory {
        None => (read_registry(settings), None),
        Some(directory) => match DirectoryRegistry::open(directory) {
            Ok((registry, snapshot)) => (Some(Ok(snapshot)), Some(Arc::new(registry))),
            Err(error) => (Some(Err(error)), None),
        },
    }
}

/// Reads the registry from the directory `settings` name, blocking the
/// caller, for a command that only checks it.
///
/// # Errors
/// The [`FederationError`] [`DirectoryRegistry::open`] returns.
pub fn read(settings: &DirectorySettings) -> Result<RegistrySnapshot, FederationError> {
    DirectoryRegistry::open(settings).map(|(_, snapshot)| snapshot)
}

/// The source `settings` name.
fn source(settings: &DirectorySettings) -> Result<DirectorySource, FederationError> {
    let credentials = match &settings.credentials {
        None => None,
        Some(Scheme::Bearer(token)) => Some(Credentials::bearer(token.to_secret_string())),
        Some(Scheme::Basic { user, password }) => Some(Credentials::basic(
            user.as_str(),
            password.to_secret_string(),
        )),
        // NOTE: no specification governs this: our own design; configuration
        // refuses a grant here, and a refusal is safer than sending nothing.
        Some(Scheme::OAuth2(_)) => {
            return Err(FederationError::Grant {
                section: String::from("registry.mcsd.credentials"),
            });
        }
    };
    DirectorySource::new(DirectoryConfig {
        base: settings.url.clone(),
        credentials,
        deadline: settings.deadline,
        pages: settings.max_pages,
        bytes: settings.max_bytes,
        entries: settings.max_entries,
    })
    .map_err(|source| directory_error(DirectoryFailure::Source(source)))
}

/// Reads `source` on a current-thread runtime of a thread of its own, so the
/// read works whether or not the caller runs on a runtime.
fn blocking(source: &DirectorySource) -> Result<Materialised, FederationError> {
    std::thread::scope(|scope| {
        let reading = scope.spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|source| directory_error(DirectoryFailure::Runtime(source)))?;
            runtime
                .block_on(source.read())
                .map_err(|error| directory_error(DirectoryFailure::Read(error)))
        });
        match reading.join() {
            Ok(read) => read,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

/// What a failed exchange says of the directory, by the rule the members
/// follow: an answer below `500` is up, a `5xx` or an answer that breaks the
/// transaction is failing, and no answer is down.
fn observed(error: &ExchangeError) -> Observed {
    match error.status() {
        Some(status) => Observed::of_answer(status),
        None if error.answered() => Observed::Failing,
        None => Observed::Down,
    }
}

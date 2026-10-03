// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The registry read from an mCSD care services directory (`[registry.mcsd]`,
//! §15.1, N21, Annex A.5), against the harness directory of the testkit: it
//! routes as the bootstrap document does, a refresh applies only what
//! changed, a refresh that breaks an integrity rule is refused with the
//! running registry kept, and a directory that does not answer keeps the
//! registry and shows on `/health/dependencies`.

mod config;
mod refresh;
mod routing;

use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;

use axum::Router;
use ferrofed_server::config::Config;
use ferrofed_server::config::settings::Settings;
use ferrofed_server::directory::DirectoryRegistry;
use ferrofed_server::reload::Reloader;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mcsd::{HarnessDirectory, Member};

use crate::facade::{EHR_A, EHR_B, crossref, settings_with_room};

/// The two members of the facade's registry document, node A at `a` and
/// node B at `b`, as the harness directory publishes them.
pub(crate) fn members(a: &str, b: &str) -> [Member; 2] {
    [member("a", a), member("b", b)]
}

/// The member `name` at `address`, with the ids the facade's document uses.
pub(crate) fn member(name: &str, address: &str) -> Member {
    Member {
        organisation: format!("org-{name}"),
        endpoint: format!("node-{name}-pub"),
        node: format!("node-{name}"),
        system_id: format!("cdr-{name}.example.org"),
        address: address.to_owned(),
    }
}

/// The development configuration reading its registry from the directory at
/// `base`, with a deadline of `deadline_ms` per read, resolving the patient at both nodes.
pub(crate) fn config(base: &str, deadline_ms: u64) -> String {
    format!(
        "profile = \"development\"\n\n[registry.mcsd]\nurl = \"{base}\"\nrefresh_interval_s = 3600\ndeadline_ms = {deadline_ms}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{}",
        crossref(&[("node-a", EHR_A), ("node-b", EHR_B)])
    )
}

/// A gateway serving the registry of a directory, with what a test drives
/// its refreshes through.
pub(crate) struct Gateway {
    pub(crate) state: Arc<AppState>,
    pub(crate) directory: Arc<DirectoryRegistry>,
    pub(crate) reloader: Reloader,
}

impl Gateway {
    /// Boots over the directory at `harness`, as `serve` does: one read of
    /// the directory, the state built over it, and a reloader beside it.
    pub(crate) fn boot(
        harness: &HarnessDirectory,
        deadline_ms: u64,
    ) -> Result<Self, Box<dyn Error>> {
        Self::boot_from(&config(&harness.base(), deadline_ms))
    }

    /// Boots as [`Gateway::boot`] does, from the configuration `text`.
    pub(crate) fn boot_from(text: &str) -> Result<Self, Box<dyn Error>> {
        let settings = settings(text)?;
        let directory_settings = settings
            .registry_directory
            .as_ref()
            .ok_or("the registry is read from a directory")?;
        let (directory, snapshot) = DirectoryRegistry::open(directory_settings)?;
        let directory = Arc::new(directory);
        let state = Arc::new(
            AppState::build_read(&settings, Some(Ok(snapshot)))?.watching(Arc::clone(&directory)),
        );
        let reloader = Reloader::new(None, settings, Arc::clone(&state));
        Ok(Self {
            state,
            directory,
            reloader,
        })
    }

    /// The router over the state.
    pub(crate) fn router(&self) -> Router {
        ferrofed_server::router(Arc::clone(&self.state), &settings_with_room())
    }

    /// The URL of each endpoint of the running registry, by endpoint id.
    pub(crate) fn addresses(&self) -> Result<BTreeMap<String, String>, Box<dyn Error>> {
        let federation = self.state.federation().ok_or("the gateway federates")?;
        Ok(federation
            .snapshot()
            .endpoints()
            .map(|endpoint| {
                (
                    endpoint.id().as_str().to_owned(),
                    endpoint.url().as_str().to_owned(),
                )
            })
            .collect())
    }
}

/// The settings `text` resolves to.
pub(crate) fn settings(text: &str) -> Result<Settings, Box<dyn Error>> {
    Ok(Config::from_sources(Some(text), &BTreeMap::new())?.resolve()?)
}

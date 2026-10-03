// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Reloading the registry while the gateway serves, driven through
//! [`Reloader::reload`], the function the `SIGHUP` handler calls: a valid
//! document replaces the running registry for the requests that start after
//! it, a request in flight finishes on the registry it took, learned state is
//! held to the new document, and a document that does not load is refused
//! with the running registry kept. No specification governs reloading: our
//! own design.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

mod learned;
mod refusals;
mod registry;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use axum::Router;
use ferrofed_registry::id::EhrId;
use ferrofed_registry::snapshot::RegistrySnapshot;
use ferrofed_server::config::Config;
use ferrofed_server::reload::Reloader;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::StatusCode;
use wiremock::{Request, Respond, ResponseTemplate};

use crate::facade::{EHR_A, body, patient_query, post, settings_with_room};
use crate::path_ehr_id::holder;
use crate::support::call;

/// A version uid node A minted, held by [`holder`].
const VERSION_A: &str = "8849182c-82ad-4088-a07f-48ead4180515::cdr-a.example.org::1";

type TestResult = Result<(), Box<dyn Error>>;

/// The `ehr_id` of the patient at node C.
const EHR_C: &str = "3333cccc-3333-4333-8333-333333333333";

/// A synthetic `creating_system_id` no member carries as its own.
const LEGACY: &str = "legacy-x.example.org";

/// A synthetic bearer token for node C, which no log line may carry.
const TOKEN_C: &str = "synthetic-reload-token-c";

/// One member `name` whose endpoint is at `url`: `org-<name>`,
/// `node-<name>` with `system_id` `cdr-<name>.example.org`, and
/// `node-<name>-pub`.
fn member(name: &str, url: &str) -> String {
    format!(
        r#"
[[organisation]]
id = "org-{name}"

[[node]]
id = "node-{name}"
organisation = "org-{name}"
system_id = "cdr-{name}.example.org"

[[endpoint]]
id = "node-{name}-pub"
node = "node-{name}"
url = "{url}"
connection_type = "openehr-rest-query"
managing_organisation = "org-{name}"
"#
    )
}

/// A gateway serving over a configuration file and a registry document in a
/// temporary directory, with the reloader its `SIGHUP` handler would call.
struct Gateway {
    _dir: tempfile::TempDir,
    config: PathBuf,
    document: PathBuf,
    state: Arc<AppState>,
    reloader: Reloader,
    app: Router,
}

impl Gateway {
    /// Starts a development gateway over `registry`, with `top` as its
    /// top-level keys and `tables` appended.
    fn start(registry: &str, top: &str, tables: &str) -> Result<Self, Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("ferrofed.toml");
        let document = dir.path().join("registry.toml");
        std::fs::write(&document, registry)?;
        std::fs::write(&config, configuration(&document, top, tables))?;
        let settings = Config::load(Some(&config))?.resolve()?;
        let state = Arc::new(AppState::build(&settings)?);
        let app = ferrofed_server::router(Arc::clone(&state), &settings_with_room());
        let reloader = Reloader::new(Some(config.clone()), settings, Arc::clone(&state));
        Ok(Self {
            _dir: dir,
            config,
            document,
            state,
            reloader,
            app,
        })
    }

    /// Rewrites the registry document.
    fn write_registry(&self, registry: &str) -> std::io::Result<()> {
        std::fs::write(&self.document, registry)
    }

    /// Rewrites the configuration file.
    fn write_config(&self, top: &str, tables: &str) -> std::io::Result<()> {
        std::fs::write(&self.config, configuration(&self.document, top, tables))
    }

    /// The running federation.
    fn federation(&self) -> Result<Arc<ferrofed_server::federation::Federation>, &'static str> {
        self.state.federation().ok_or("a registry is configured")
    }

    /// Asks the patient query and returns the status and the answer text.
    async fn ask(&self) -> Result<(StatusCode, String), Box<dyn Error>> {
        call(self.app.clone(), post(body(&patient_query())?)?).await
    }
}

/// The configuration text over `document`, a development profile with
/// `top`, and `tables` appended after the `[federation]` keys, so keys before
/// its first table header extend `[federation]`.
fn configuration(document: &Path, top: &str, tables: &str) -> String {
    let document = toml::Value::String(document.display().to_string());
    crate::support::signed(&format!(
        "profile = \"development\"\n{top}\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = 5000\noverall_timeout_ms = 6000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{tables}"
    ))
}

fn ids<T: std::str::FromStr>(values: &[&str]) -> Result<Vec<T>, T::Err> {
    values.iter().map(|value| value.parse()).collect()
}

/// How many requests `server` received.
async fn hits(server: &Server) -> Result<usize, Box<dyn Error>> {
    Ok(server
        .received_requests()
        .await
        .ok_or("recording is on")?
        .len())
}

/// The registry of node A and node B, at addresses nothing listens on.
fn snapshot_of_a_and_b() -> Result<RegistrySnapshot, Box<dyn Error>> {
    Ok(RegistrySnapshot::from_toml_str(
        &(member("a", "http://127.0.0.1:9/a") + &member("b", "http://127.0.0.1:9/b")),
    )?)
}

/// The gateway over node A, a holder of [`EHR_A`], after a reload that removed
/// node B, with an index entry learned after the reload naming both: the
/// entry a request in flight on the old registry could leave behind.
async fn indexed_at_a_departed_member() -> Result<(Gateway, Server), Box<dyn Error>> {
    let a = holder().await;
    let b = holder().await;
    let gateway = Gateway::start(&(member("a", &a.uri()) + &member("b", &b.uri())), "", "")?;
    gateway.write_registry(&member("a", &a.uri()))?;
    gateway.reloader.reload()?;
    let federation = gateway.federation()?;
    let ehr_a: EhrId = EHR_A.parse()?;
    federation.index().learn(&ehr_a, &"node-a".parse()?);
    federation.index().learn(&ehr_a, &"node-b".parse()?);
    Ok((gateway, a))
}

/// A node that holds every request until the test releases it, and says
/// when one arrived.
struct Holding {
    arrived: tokio::sync::mpsc::UnboundedSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    answer: String,
}

impl Respond for Holding {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        // NOTE: a closed channel means the test already failed, so the send
        // result has nothing to report.
        let _sent: Result<(), _> = self.arrived.send(());
        let released = self
            .release
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(Duration::from_secs(10));
        match released {
            Ok(()) => ResponseTemplate::new(200)
                .set_body_raw(self.answer.clone().into_bytes(), "application/json"),
            Err(_) => ResponseTemplate::new(503),
        }
    }
}

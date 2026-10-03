// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The four `its_rest` declarations of `OPTIONS {base}/`, held to what the
//! gateway does, in every configuration mode against two mock nodes (§7a.1,
//! §7a.2, N30, N32; CP-23, CP-25). A mode is one combination of the
//! DEMOGRAPHIC endpoint, the template fan-out, the stored-query registry and
//! its distribution to the members; the `GET` forms of query execution are
//! served in every mode. Each test
//! reads the declaration of each mode, checks that it states a behaviour,
//! and exercises that behaviour, reading what each node received from the
//! node's own capture (§16, track 10).
//!
//! This module holds the modes and the gateway each runs; one child per
//! area holds its tests, and `modes` the test over all four.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

mod definition;
mod demographic;
mod ehr;
mod modes;
mod query;

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use ferrofed_server::config::Config;
use ferrofed_server::state::AppState;
use ferrofed_testkit::mock::Server;
use http::{Method, Request, StatusCode, header};
use openehr_federation::headers::ENDPOINT;
use openehr_federation::options::OptionsRoot;
use tempfile::TempDir;
use wiremock::ResponseTemplate;

use crate::facade::{NAMESPACE, crossref, node_answering, registry, schema, settings_with_room};
use crate::path_ehr_id::answer;
use crate::support::{asked, call, error_body, mount};

type TestResult = Result<(), Box<dyn Error>>;

/// The endpoints of node A and node B in [`registry`].
const ENDPOINT_A: &str = "node-a-pub";
const ENDPOINT_B: &str = "node-b-pub";

/// The qualified name of the stored query the registry modes hold.
const NAME: &str = "org.example::declared_areas";

/// An `ehr_id` node A holds and nothing else names, found by the probe.
const PROBED: &str = "4444aaaa-4444-4444-8444-444444444444";

/// One configuration mode: each setting an `its_rest` declaration varies
/// with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Mode {
    /// `federation.demographic_endpoint` names node B's endpoint.
    demographic: bool,
    /// `federation.fan_out_template_upload` is set.
    fan_out: bool,
    /// Where stored queries live.
    stored: Stored,
}

/// The stored-query settings a configuration admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stored {
    /// No registry: a definition request routes to one node.
    Routed,
    /// The registry is offered (`[stored_queries]`).
    Held,
    /// The registry is offered and `federation.fan_out_stored_queries` is
    /// set, which only the registry admits.
    Distributed,
}

impl Mode {
    /// Every combination of the settings a configuration admits.
    fn all() -> Vec<Self> {
        let mut modes = Vec::new();
        for demographic in [false, true] {
            for fan_out in [false, true] {
                for stored in [Stored::Routed, Stored::Held, Stored::Distributed] {
                    modes.push(Self {
                        demographic,
                        fan_out,
                        stored,
                    });
                }
            }
        }
        modes
    }

    /// Whether the stored-query registry is offered.
    fn registry(self) -> bool {
        self.stored != Stored::Routed
    }

    /// Whether the registry distributes its definitions to the members.
    fn distribution(self) -> bool {
        self.stored == Stored::Distributed
    }

    /// The `[federation]` lines this mode adds.
    fn federation(self) -> String {
        let mut lines = String::new();
        if self.demographic {
            lines.push_str("demographic_endpoint = \"node-b-pub\"\n");
        }
        if self.fan_out {
            lines.push_str("fan_out_template_upload = true\n");
        }
        if self.distribution() {
            lines.push_str("fan_out_stored_queries = true\n");
        }
        lines
    }
}

/// What one request did: its status, the acting endpoint, the body, and the
/// method and path of each request node A and node B received for it.
#[derive(Debug)]
struct Outcome {
    status: StatusCode,
    acting: Option<String>,
    text: String,
    a: Vec<(String, String)>,
    b: Vec<(String, String)>,
}

impl Outcome {
    /// Whether no node received anything for the request.
    fn reached_nobody(&self) -> bool {
        self.a.is_empty() && self.b.is_empty()
    }

    /// The stable error code of the answer.
    fn code(&self) -> Result<String, Box<dyn Error>> {
        Ok(error_body(&self.text)?.code)
    }
}

/// A gateway in one mode over node A and node B, with the directory its
/// state lives in.
struct Setup {
    app: Router,
    a: Server,
    b: Server,
    _dir: TempDir,
}

impl Setup {
    /// The gateway in `mode`, resolving the patient at the members `rows`
    /// name. Both nodes answer a query; node A holds [`PROBED`].
    async fn new(mode: Mode, rows: &[(&str, &str)]) -> Result<Self, Box<dyn Error>> {
        let a = node_answering("uid-at-a::cdr-a.example.org::1").await;
        mount(
            &a,
            "GET",
            format!("/v1/ehr/{PROBED}"),
            ResponseTemplate::new(200).set_body_raw(
                format!(r#"{{"ehr_id":{{"value":"{PROBED}"}}}}"#).into_bytes(),
                "application/json",
            ),
        )
        .await;
        let b = node_answering("uid-at-b::cdr-b.example.org::1").await;
        let dir = tempfile::tempdir()?;
        let text = settings_text(dir.path(), (&a, &b), mode, rows)?;
        let settings =
            Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?
                .resolve()?;
        let app =
            ferrofed_server::router(Arc::new(AppState::build(&settings)?), &settings_with_room());
        Ok(Self {
            app,
            a,
            b,
            _dir: dir,
        })
    }

    /// The `OPTIONS {base}/` body, checked against the vendored schema.
    async fn declared(&self) -> Result<OptionsRoot, Box<dyn Error>> {
        let (status, text) =
            call(self.app.clone(), Request::options("/").body(Body::empty())?).await?;
        assert_eq!(StatusCode::OK, status, "{text}");
        schema::validate_options(&text)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Sends `request` and reads what it did.
    async fn send(&self, request: Request<Body>) -> Result<Outcome, Box<dyn Error>> {
        let before = (asked(&self.a).await?.len(), asked(&self.b).await?.len());
        let (status, acting, text) = answer(self.app.clone(), request).await?;
        Ok(Outcome {
            status,
            acting,
            text,
            a: asked(&self.a).await?.into_iter().skip(before.0).collect(),
            b: asked(&self.b).await?.into_iter().skip(before.1).collect(),
        })
    }
}

/// The settings text of a gateway in `mode` over node A and node B, its
/// registry document and store in `dir`, resolving the patient at the
/// members `rows` name.
fn settings_text(
    dir: &Path,
    (a, b): (&Server, &Server),
    mode: Mode,
    rows: &[(&str, &str)],
) -> Result<String, Box<dyn Error>> {
    let document = dir.join("registry.toml");
    std::fs::write(&document, registry(&a.uri(), &b.uri(), ""))?;
    let document = toml::Value::String(document.display().to_string());
    let store = if mode.registry() {
        let path = toml::Value::String(dir.join("definitions.redb").display().to_string());
        format!("[stored_queries]\npath = {path}\n")
    } else {
        String::new()
    };
    Ok(format!(
        "profile = \"development\"\n\n[registry]\ndocument = {document}\n\n\
         [federation]\nid = \"example-federation\"\nnode_selection = \"ask-all\"\n\
         per_node_timeout_ms = 2000\noverall_timeout_ms = 3000\n{}\n{store}{}",
        mode.federation(),
        crossref(rows)
    ))
}

/// A request of `verb` to `uri`, naming `target` in the endpoint header and
/// carrying `sent` as `(media type, body)` when given.
fn request(
    verb: &Method,
    uri: &str,
    target: Option<&str>,
    sent: Option<(&str, String)>,
) -> Result<Request<Body>, http::Error> {
    let mut request = Request::builder().method(verb.clone()).uri(uri);
    if let Some(target) = target {
        request = request.header(ENDPOINT, target);
    }
    match sent {
        Some((media, text)) => request
            .header(header::CONTENT_TYPE, media)
            .body(Body::from(text)),
        None => request.body(Body::empty()),
    }
}

/// The method and path of one request a node received.
fn at(verb: &Method, path: &str) -> (String, String) {
    (verb.as_str().to_owned(), path.to_owned())
}

/// The `PUT` storing `version` of [`NAME`], a definition naming the patient
/// through `$patient`, naming `target` when given.
fn store(version: &str, target: Option<&str>) -> Result<Request<Body>, http::Error> {
    let definition = format!(
        "SELECT c/uid/value FROM EHR e CONTAINS COMPOSITION c \
         WHERE e/ehr_status/subject/external_ref/id/value = $patient \
         AND e/ehr_status/subject/external_ref/namespace = '{NAMESPACE}'"
    );
    request(
        &Method::PUT,
        &format!("/v1/definition/query/{NAME}/{version}"),
        target,
        Some(("text/plain", definition)),
    )
}

/// Asserts that `declared` states `clause`.
fn states(declared: &str, clause: &str, mode: Mode) {
    assert!(
        declared.contains(clause),
        "§7a.2, N30: {mode:?} declares {clause:?}: {declared}"
    );
}

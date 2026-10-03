// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The registry reload counter: an applied reload and a refused one each
//! count under their `result`, and the node request instruments outlive the
//! federation a reload replaces.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;
use std::sync::Arc;

use axum::body::Body;
use ferrofed_server::config::Config;
use ferrofed_server::reload::Reloader;
use ferrofed_server::state::AppState;
use http::{Request, StatusCode, header};

use crate::facade::{
    EHR_A, EHR_B, body, crossref, node_answering, patient_query, registry, settings_with_room,
};
use crate::metrics::{Sample, count, parse};
use crate::support::call;

type TestResult = Result<(), Box<dyn Error>>;

/// The counter's Prometheus name.
const RELOADS: &str = "ferrofed_registry_reloads_total";

/// The reload count of `result` in `samples`.
fn reloads(samples: &[Sample], result: &str) -> Option<String> {
    count(samples, RELOADS, &[("result", result)])
}

#[tokio::test]
async fn an_applied_and_a_refused_reload_each_count_under_their_result() -> TestResult {
    let a = node_answering("uid-a::cdr-a.example.org::1").await;
    let b = node_answering("uid-b::cdr-b.example.org::1").await;
    let dir = tempfile::tempdir()?;
    let document = dir.path().join("registry.toml");
    let config = dir.path().join("ferrofed.toml");
    std::fs::write(&document, registry(&a.uri(), &b.uri(), ""))?;
    let named = toml::Value::String(document.display().to_string());
    std::fs::write(
        &config,
        crate::support::signed(&format!(
            "profile = \"development\"\n\n[registry]\ndocument = {named}\n\n[federation]\nper_node_timeout_ms = 2000\noverall_timeout_ms = 3000\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{}",
            crossref(&[("node-a", EHR_A), ("node-b", EHR_B)])
        )),
    )?;
    let settings = Config::load(Some(&config))?.resolve()?;
    let state = Arc::new(AppState::build(&settings)?);
    let reloader = Reloader::new(Some(config), settings, Arc::clone(&state));
    let scraped =
        || -> Result<Vec<Sample>, Box<dyn Error>> { Ok(parse(&state.metrics().render()?)?) };

    reloader.reload()?;
    let samples = scraped()?;
    assert_eq!(Some("1".to_owned()), reloads(&samples, "applied"));
    assert_eq!(Some("0".to_owned()), reloads(&samples, "refused"));

    std::fs::write(&document, "[[node]]\nid = \"node-without-organisation\"\n")?;
    assert!(reloader.reload().is_err(), "the document does not load");
    let samples = scraped()?;
    assert_eq!(Some("1".to_owned()), reloads(&samples, "applied"));
    assert_eq!(Some("1".to_owned()), reloads(&samples, "refused"));

    let app = ferrofed_server::router(Arc::clone(&state), &settings_with_room());
    let request = Request::post("/v1/query/aql")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body(&patient_query())?))?;
    let (status, text) = call(app, request).await?;
    assert_eq!(StatusCode::OK, status, "{text}");
    assert_eq!(
        Some("1".to_owned()),
        count(
            &scraped()?,
            "ferrofed_node_requests_total",
            &[("endpoint", "node-a-pub"), ("outcome", "active")]
        ),
        "the reloaded federation records through the same instruments"
    );
    Ok(())
}

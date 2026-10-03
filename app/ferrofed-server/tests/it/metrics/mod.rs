// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The metrics surface: the Prometheus text exposition of `GET /metrics` on
//! the admin listener, off by default and on loopback unless allowed; the
//! integrity incident counter by kind; the node request counter and duration
//! histogram by endpoint and outcome; the registry reload counter; and no
//! label value a request can set (§5.4.1, N33). No specification governs
//! metrics: our own design.
//!
//! This module holds the exposition reader every test parses with and the
//! metered gateway the request tests drive.

mod exposition;
mod hygiene;
mod incidents;
mod nodes;
mod panicked;
mod reload;
mod unsent;

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use ferrofed_server::config::Config;
use ferrofed_server::state::AppState;

use crate::facade::settings_with_room;

/// One sample of the text exposition: its name, its labels and its value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sample {
    pub(crate) name: String,
    pub(crate) labels: BTreeMap<String, String>,
    pub(crate) value: f64,
}

/// Reads `text` as the Prometheus text exposition format 0.0.4
/// (<https://prometheus.io/docs/instrumenting/exposition_formats/>), refusing
/// any line the format does not admit and any sample whose family no `TYPE`
/// line declared.
pub(crate) fn parse(text: &str) -> Result<Vec<Sample>, String> {
    let mut families: BTreeMap<String, String> = BTreeMap::new();
    let mut samples = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix("# ") {
            let mut words = comment.splitn(3, ' ');
            match (words.next(), words.next(), words.next()) {
                (Some("HELP"), Some(name), _) if is_name(name) => {}
                (Some("TYPE"), Some(name), Some(kind)) if is_name(name) => {
                    if !["counter", "gauge", "histogram", "summary", "untyped"].contains(&kind) {
                        return Err(format!("an unknown type: {line}"));
                    }
                    families.insert(name.to_owned(), kind.to_owned());
                }
                _ => return Err(format!("a comment the format does not admit: {line}")),
            }
            continue;
        }
        let sample = sample(line)?;
        if !declared(&families, &sample.name) {
            return Err(format!("a sample of no declared family: {line}"));
        }
        samples.push(sample);
    }
    Ok(samples)
}

/// Whether `name` is a sample of a family in `families`: the family itself,
/// or the `_bucket`, `_sum` or `_count` series of a histogram.
fn declared(families: &BTreeMap<String, String>, name: &str) -> bool {
    if families.contains_key(name) {
        return true;
    }
    ["_bucket", "_sum", "_count"].iter().any(|suffix| {
        name.strip_suffix(suffix)
            .and_then(|family| families.get(family))
            .is_some_and(|kind| kind == "histogram")
    })
}

/// Whether `name` is a metric name: `[a-zA-Z_:][a-zA-Z0-9_:]*`.
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_' || first == ':')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// Whether `name` is a label name: `[a-zA-Z_][a-zA-Z0-9_]*`.
fn is_label(name: &str) -> bool {
    is_name(name) && !name.contains(':')
}

/// Reads one sample line: `name{label="value",…} value`.
fn sample(line: &str) -> Result<Sample, String> {
    let refused = || format!("a sample the format does not admit: {line}");
    let (head, value) = line.rsplit_once(' ').ok_or_else(refused)?;
    let value = match value {
        "+Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        other => other.parse::<f64>().map_err(|_number| refused())?,
    };
    let (name, labels) = match head.split_once('{') {
        None => (head, BTreeMap::new()),
        Some((name, rest)) => (name, labels(rest.strip_suffix('}').ok_or_else(refused)?)?),
    };
    if !is_name(name) {
        return Err(refused());
    }
    Ok(Sample {
        name: name.to_owned(),
        labels,
        value,
    })
}

/// Reads the labels between the braces: `name="value"` pairs, each value
/// with `\\`, `\"` and `\n` escapes, separated by commas.
fn labels(mut rest: &str) -> Result<BTreeMap<String, String>, String> {
    let mut labels = BTreeMap::new();
    while !rest.is_empty() {
        let (name, after) = rest
            .split_once("=\"")
            .ok_or_else(|| format!("a label without a quoted value: {rest}"))?;
        if !is_label(name) {
            return Err(format!("not a label name: {name}"));
        }
        let mut value = String::new();
        let mut chars = after.char_indices();
        let end = loop {
            match chars.next() {
                Some((_, '\\')) => match chars.next() {
                    Some((_, '\\')) => value.push('\\'),
                    Some((_, '"')) => value.push('"'),
                    Some((_, 'n')) => value.push('\n'),
                    _ => return Err(format!("an escape the format does not admit: {after}")),
                },
                Some((at, '"')) => break at,
                Some((_, c)) => value.push(c),
                None => return Err(format!("an unterminated label value: {after}")),
            }
        };
        labels.insert(name.to_owned(), value);
        rest = after.get(end + 1..).unwrap_or_default();
        rest = rest.strip_prefix(',').unwrap_or(rest);
    }
    Ok(labels)
}

/// The value of the sample named `name` whose labels include every pair of
/// `labels`, when there is exactly one.
pub(crate) fn value(samples: &[Sample], name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    let mut found = samples.iter().filter(|sample| {
        sample.name == name
            && labels
                .iter()
                .all(|(key, value)| sample.labels.get(*key).map(String::as_str) == Some(*value))
    });
    let first = found.next()?;
    found.next().is_none().then_some(first.value)
}

/// The value of [`value`] as the exposition writes a whole count, so a test
/// compares counts as text and never compares floats.
pub(crate) fn count(samples: &[Sample], name: &str, labels: &[(&str, &str)]) -> Option<String> {
    value(samples, name, labels).map(|value| value.to_string())
}

/// A gateway with its own metrics, built through the real configuration
/// path from `top` and `tables` over the registry document `registry`.
pub(crate) struct Metered {
    pub(crate) state: Arc<AppState>,
    pub(crate) app: Router,
}

impl Metered {
    /// Builds the gateway with the registry written into `dir`, a per-node
    /// timeout of `per_node_ms` and an overall budget of `overall_ms`.
    pub(crate) fn start(
        dir: &Path,
        registry: &str,
        (top, tables): (&str, &str),
        (per_node_ms, overall_ms): (u64, u64),
    ) -> Result<Self, Box<dyn Error>> {
        let document = dir.join("registry.toml");
        std::fs::write(&document, registry)?;
        let document = toml::Value::String(document.display().to_string());
        let text = format!(
            "{top}\n\n[registry]\ndocument = {document}\n\n[federation]\nper_node_timeout_ms = {per_node_ms}\noverall_timeout_ms = {overall_ms}\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{tables}"
        );
        let settings =
            Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?
                .resolve()?;
        let state = Arc::new(AppState::build(&settings)?);
        let app = ferrofed_server::router(Arc::clone(&state), &settings_with_room());
        Ok(Self { state, app })
    }

    /// The exposition of this gateway's metrics, parsed.
    pub(crate) fn scraped(&self) -> Result<Vec<Sample>, Box<dyn Error>> {
        Ok(parse(&self.state.metrics().render()?)?)
    }
}

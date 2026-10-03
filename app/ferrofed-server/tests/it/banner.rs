// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The startup banner: rendered from fixed inputs, printed only before the
//! terminal rendering of the log, and never carrying a secret.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsStr;
use std::io::Write as _;
use std::net::TcpListener;
use std::process::Command;

use ferrofed_identity::dev::Profile;
use ferrofed_server::banner::{DEVELOPMENT_NOTICE, Deployment, Registry, WORDMARK, prints, render};
use ferrofed_server::config::Config;
use ferrofed_server::config::stored_queries::{Backend, Store};
use ferrofed_server::federation::{FederationError, read_registry};
use ferrofed_server::state::{AppState, StateError};
use ferrofed_server::telemetry::Format;

use crate::facade::{NAMESPACE, PATIENT, registry};
use crate::run::binary;

type TestResult = Result<(), Box<dyn Error>>;

/// The first line of text under the wordmark.
const TAGLINE: &str = "openEHR federation gateway";

/// A production deployment under a base path, with a registry of two members
/// and three endpoints.
fn deployment() -> Result<Deployment, Box<dyn Error>> {
    Ok(Deployment {
        base_path: "/federation".parse()?,
        listen: "127.0.0.1:8080".parse()?,
        registry: Registry::Read {
            members: 2,
            endpoints: 3,
        },
        stored_queries: Some(Backend::Redb),
        development: false,
        cleartext: Vec::new(),
    })
}

/// The value the aligned line labelled `label` carries, if the banner has one.
fn value_of<'a>(banner: &'a str, label: &str) -> Option<&'a str> {
    banner.lines().find_map(|line| {
        line.trim_start()
            .strip_prefix(label)
            .filter(|rest| rest.starts_with("  "))
            .map(str::trim)
    })
}

#[test]
fn the_banner_carries_the_wordmark_the_version_and_the_maintainer() -> TestResult {
    let banner = render("9.9.9", &deployment()?, false);
    assert!(banner.contains(WORDMARK), "{banner}");
    assert!(
        banner.contains(&format!("{TAGLINE} · v9.9.9")),
        "the version is substituted: {banner}"
    );
    assert!(banner.contains("Maintained by Ruben Talstra"), "{banner}");
    assert!(
        banner.contains("https://github.com/FerroHEALTH/FerroFED"),
        "{banner}"
    );
    Ok(())
}

#[test]
fn every_pin_is_read_from_its_constant() -> TestResult {
    let banner = render("9.9.9", &deployment()?, false);
    for (label, pin) in [
        ("Federation Tier", openehr_federation::FEDERATION_SPEC),
        ("ITS-REST", ferrofed_engine::ITS_REST),
        ("AQL", openehr_federation::AQL),
        ("openehr-*", ferrofed_server::OPENEHR_FAMILY),
    ] {
        assert_eq!(Some(pin), value_of(&banner, label), "{label}: {banner}");
    }
    Ok(())
}

#[test]
fn the_deployment_lines_name_the_base_the_address_the_registry_and_the_store() -> TestResult {
    let banner = render("9.9.9", &deployment()?, false);
    assert_eq!(Some("/federation"), value_of(&banner, "Base path"));
    assert_eq!(Some("127.0.0.1:8080"), value_of(&banner, "Listen"));
    assert_eq!(
        Some("2 members, 3 endpoints"),
        value_of(&banner, "Registry")
    );
    assert_eq!(Some("on, redb"), value_of(&banner, "Stored queries"));
    for (backend, shown) in [
        (Backend::Postgres, "on, postgres"),
        (Backend::Files, "on, files, read-only"),
    ] {
        let mut stored = deployment()?;
        stored.stored_queries = Some(backend);
        let banner = render("9.9.9", &stored, false);
        assert_eq!(
            Some(shown),
            value_of(&banner, "Stored queries"),
            "the backend kind, never its location"
        );
    }

    let mut bare = deployment()?;
    bare.registry = Registry::Unset;
    bare.stored_queries = None;
    let banner = render("9.9.9", &bare, false);
    assert_eq!(
        Some("none, the gateway federates nothing"),
        value_of(&banner, "Registry")
    );
    assert_eq!(Some("off"), value_of(&banner, "Stored queries"));

    bare.registry = Registry::Unreadable;
    let banner = render("9.9.9", &bare, false);
    assert_eq!(
        Some("does not load; the log says why"),
        value_of(&banner, "Registry")
    );

    bare.registry = Registry::Read {
        members: 1,
        endpoints: 1,
    };
    let banner = render("9.9.9", &bare, false);
    assert_eq!(Some("1 member, 1 endpoint"), value_of(&banner, "Registry"));
    Ok(())
}

#[test]
fn the_development_notice_prints_in_red_and_in_plain_words() -> TestResult {
    let mut development = deployment()?;
    development.development = true;

    let plain = render("9.9.9", &development, false);
    let words = plain.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(words.contains(DEVELOPMENT_NOTICE), "{plain}");
    assert!(words.contains("must not hold or reach real patient data"));
    assert!(!plain.contains('\x1b'), "no escape code without colour");

    let coloured = render("9.9.9", &development, true);
    let notice: Vec<&str> = coloured.lines().filter(|l| l.contains('\x1b')).collect();
    assert!(!notice.is_empty(), "{coloured}");
    for line in notice {
        assert!(
            line.starts_with("\x1b[1;31m") && line.ends_with("\x1b[0m"),
            "every notice line is red: {line:?}"
        );
    }
    let stripped = coloured.replace("\x1b[1;31m", "").replace("\x1b[0m", "");
    assert_eq!(plain, stripped, "colour changes no word");
    Ok(())
}

#[test]
fn a_production_deployment_prints_no_notice() -> TestResult {
    for colour in [false, true] {
        let banner = render("9.9.9", &deployment()?, colour);
        assert!(!banner.contains("DEVELOPMENT"), "{banner}");
        assert!(!banner.contains('\x1b'), "{banner}");
    }
    Ok(())
}

#[test]
fn every_line_fits_80_columns() -> TestResult {
    let wordmark: Vec<&str> = WORDMARK.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(5, wordmark.len(), "the standard font is five lines high");
    let mut development = deployment()?;
    development.development = true;
    let banner = render(env!("CARGO_PKG_VERSION"), &development, false);
    for line in banner.lines() {
        assert!(
            line.chars().count() <= 80,
            "{} columns: {line:?}",
            line.chars().count()
        );
    }
    Ok(())
}

/// The banner precedes only the terminal rendering: `auto` with stdout piped
/// to a collector renders JSON, and no banner may precede it.
#[test]
fn the_banner_prints_only_before_the_terminal_rendering() {
    assert!(prints(Format::Auto, true));
    assert!(!prints(Format::Auto, false));
    for stdout_is_terminal in [false, true] {
        assert!(prints(Format::Pretty, stdout_is_terminal));
        assert!(!prints(Format::Json, stdout_is_terminal));
    }
}

#[test]
fn colour_follows_the_terminal_unless_pretty_is_asked_for() {
    for unset in [None, Some(OsStr::new(""))] {
        assert!(Format::Auto.colour(true, unset));
        assert!(!Format::Auto.colour(false, unset));
        assert!(Format::Pretty.colour(false, unset));
        assert!(Format::Json.colour(true, unset));
        assert!(!Format::Json.colour(false, unset));
    }
}

/// A `NO_COLOR` that is set and not empty switches colour off, whatever its
/// value, the format and the terminal (<https://no-color.org>).
#[test]
fn no_color_switches_colour_off_whatever_the_format_and_the_terminal() {
    for value in ["1", "0", "false"] {
        for format in [Format::Auto, Format::Json, Format::Pretty] {
            for stdout_is_terminal in [false, true] {
                assert!(
                    !format.colour(stdout_is_terminal, Some(OsStr::new(value))),
                    "NO_COLOR={value} {format:?} terminal={stdout_is_terminal}"
                );
            }
        }
    }
}

/// Runs `serve` on a taken address with `format = "pretty"`, the development
/// profile, and `NO_COLOR` set to `no_color`, and returns its stdout.
fn served_with_no_color(no_color: &str) -> Result<String, Box<dyn Error>> {
    let taken = TcpListener::bind("127.0.0.1:0")?;
    let listen = taken.local_addr()?;
    let mut file = tempfile::NamedTempFile::new()?;
    write!(
        file,
        "profile = \"development\"\n\n[server]\nlisten = \"{listen}\"\n\n[telemetry]\nformat = \"pretty\"\n"
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_ferrofed"))
        .args(["serve", "--config"])
        .arg(file.path())
        .env_remove("FERROFED_CONFIG")
        .env("NO_COLOR", no_color)
        .output()?;
    drop(taken);
    assert_eq!(Some(1), output.status.code(), "the address is taken");
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn no_color_prints_the_banner_and_its_notice_without_colour() -> TestResult {
    let stdout = served_with_no_color("1")?;
    let banner = stdout
        .split_once("  Stored queries")
        .map(|(_, rest)| rest)
        .ok_or("the banner prints")?;
    let notice: String = banner
        .lines()
        .take_while(|line| !line.starts_with(|c: char| c.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join(" ");
    let words = notice.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(words.contains(DEVELOPMENT_NOTICE), "{stdout}");
    assert!(
        !notice.contains('\x1b'),
        "no colour in the banner: {stdout}"
    );

    let coloured = served_with_no_color("")?;
    assert!(
        coloured.contains("\x1b[1;31m"),
        "an empty NO_COLOR leaves the notice red: {coloured}"
    );
    Ok(())
}

#[test]
fn no_color_writes_the_pretty_log_without_colour() -> TestResult {
    let stdout = served_with_no_color("1")?;
    let log = stdout
        .split_once("patient data.")
        .map(|(_, rest)| rest)
        .ok_or("the banner prints before the log")?;
    assert!(log.contains("cannot serve"), "the log follows: {stdout}");
    assert!(!log.contains('\x1b'), "no colour in the log: {stdout}");

    let coloured = served_with_no_color("")?;
    let log = coloured
        .split_once("patient data.")
        .map(|(_, rest)| rest)
        .ok_or("the banner prints before the log")?;
    assert!(
        log.contains('\x1b'),
        "an empty NO_COLOR leaves an explicit pretty log coloured: {coloured}"
    );
    Ok(())
}

/// The banner and the gateway share one read of the registry document: a
/// document removed after the read still builds the state the banner counted.
#[test]
fn the_banner_and_the_build_share_one_read_of_the_registry() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("registry.toml");
    std::fs::write(
        &path,
        registry("http://127.0.0.1:9/a", "http://127.0.0.1:9/b", ""),
    )?;
    let document = toml::Value::String(path.display().to_string());
    let text = format!(
        "[registry]\ndocument = {document}\n\n[federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let read = read_registry(&settings);
    let deployment = Deployment::of(
        settings.server.base_path.clone(),
        settings.server.listen,
        read.as_ref().map(Result::as_ref),
        settings.stored_queries.as_ref().map(Store::backend),
        settings.profile == Profile::Development,
    );
    std::fs::remove_file(&path)?;

    let state = AppState::build_read(&settings, read)?;
    let federation = state.federation().ok_or("the gateway federates")?;
    assert_eq!(
        Registry::Read {
            members: federation.snapshot().nodes().count(),
            endpoints: federation.snapshot().endpoints().count(),
        },
        deployment.registry,
        "the banner counts the registry the gateway serves"
    );
    assert!(
        AppState::build(&settings).is_err(),
        "a second read would find no document"
    );
    Ok(())
}

/// A registry document that does not load stops the build over the same read
/// on the typed error a fresh boot reports.
#[test]
fn a_failed_read_stops_the_build_on_the_same_typed_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let document = toml::Value::String(dir.path().join("absent.toml").display().to_string());
    let text = format!(
        "[registry]\ndocument = {document}\n\n[federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let read = read_registry(&settings);
    assert!(matches!(read, Some(Err(FederationError::Registry { .. }))));
    let Err(StateError::Federation(FederationError::Registry { path, .. })) =
        AppState::build_read(&settings, read)
    else {
        return Err("the build stops on the registry error".into());
    };
    assert_eq!(dir.path().join("absent.toml"), path);
    Ok(())
}

/// A configuration carrying a credential, a password, a PIX Manager token and
/// a patient identifier shows none of them, and no endpoint URL either.
#[test]
fn no_secret_from_the_configuration_reaches_the_banner() -> TestResult {
    const SECRET: &str = "Qz7SentinelSecret";
    let dir = tempfile::tempdir()?;
    let document = dir.path().join("registry.toml");
    std::fs::write(
        &document,
        registry(
            "https://node-a.example.org/ehrbase/rest/openehr",
            "https://node-b.example.org/ehrbase/rest/openehr",
            "",
        ),
    )?;
    let document = toml::Value::String(document.display().to_string());
    let text = format!(
        "profile = \"development\"\n\n\
         [registry]\ndocument = {document}\n\n\
         [federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n\
         [credentials.\"node-a-pub\"]\nbearer_token = \"{SECRET}-bearer\"\n\n\
         [credentials.\"node-b-pub\"]\nuser = \"{SECRET}-user\"\npassword = \"{SECRET}-password\"\n\n\
         [[dev.crossref]]\nnamespace = \"{NAMESPACE}\"\nvalue = \"{PATIENT}\"\nmember = \"node-a\"\nehr_id = \"7d44b88c-4199-4bad-97dc-d78268e01398\"\n"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let document = read_registry(&settings);
    let deployment = Deployment::of(
        settings.server.base_path.clone(),
        settings.server.listen,
        document.as_ref().map(Result::as_ref),
        settings.stored_queries.as_ref().map(Store::backend),
        settings.profile == Profile::Development,
    );
    assert_eq!(
        Registry::Read {
            members: 2,
            endpoints: 2
        },
        deployment.registry,
        "the counts come from the document"
    );
    assert!(deployment.development);
    for colour in [false, true] {
        let banner = render("9.9.9", &deployment, colour);
        for leak in [SECRET, PATIENT, "example.org", "ehrbase", "https://node"] {
            assert!(!banner.contains(leak), "{leak} leaked: {banner}");
        }
    }
    Ok(())
}

#[test]
fn a_registry_document_that_does_not_load_is_named_unreadable() -> TestResult {
    let dir = tempfile::tempdir()?;
    let document = toml::Value::String(dir.path().join("absent.toml").display().to_string());
    let text = format!(
        "[registry]\ndocument = {document}\n\n[federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
    );
    let settings =
        Config::from_sources(Some(&crate::support::signed(&text)), &BTreeMap::new())?.resolve()?;
    let document = read_registry(&settings);
    let deployment = Deployment::of(
        settings.server.base_path.clone(),
        settings.server.listen,
        document.as_ref().map(Result::as_ref),
        settings.stored_queries.as_ref().map(Store::backend),
        settings.profile == Profile::Development,
    );
    assert_eq!(Registry::Unreadable, deployment.registry);
    Ok(())
}

/// `serve` with a terminal rendering prints the banner first; with JSON it
/// prints none, and `config check` never does. The listen address is taken,
/// so `serve` stops right after the boot.
#[test]
fn serve_prints_the_banner_before_a_pretty_log_and_never_before_json() -> TestResult {
    let taken = TcpListener::bind("127.0.0.1:0")?;
    let listen = taken.local_addr()?;
    let config = |format: &str| {
        format!("[server]\nlisten = \"{listen}\"\n\n[telemetry]\nformat = \"{format}\"\n")
    };

    let pretty = binary(&["serve"], &config("pretty"))?;
    assert_eq!(Some(1), pretty.status.code(), "the address is taken");
    let stdout = String::from_utf8_lossy(&pretty.stdout);
    assert!(
        stdout.starts_with(WORDMARK),
        "the banner comes first: {stdout}"
    );
    assert!(stdout.contains(TAGLINE), "{stdout}");

    let json = binary(&["serve"], &config("json"))?;
    assert_eq!(Some(1), json.status.code(), "the address is taken");
    let stdout = String::from_utf8_lossy(&json.stdout);
    assert!(!stdout.contains(TAGLINE), "no banner before JSON: {stdout}");
    assert!(
        stdout
            .lines()
            .next()
            .is_some_and(|line| line.starts_with('{')),
        "the first line is a JSON log line: {stdout}"
    );

    let check = binary(&["config", "check"], &config("pretty"))?;
    assert_eq!(Some(0), check.status.code());
    let stdout = String::from_utf8_lossy(&check.stdout);
    assert!(
        !stdout.contains(TAGLINE),
        "config check prints no banner: {stdout}"
    );
    drop(taken);
    Ok(())
}

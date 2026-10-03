// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The transport of every configured credential: outside `profile =
//! "development"`, `config check`, `serve`, `admission check` and every
//! reload refuse a credential sent to a URL that is not `https`, naming its
//! key; under the development profile the same configuration starts, and the
//! banner, the log and `config check` name each credential that travels
//! unencrypted. A URL no credential is sent to stays allowed over `http`.
//! §2.2 assumes transport security bound through §13; no specification
//! governs this check: our own design.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

#[cfg(feature = "postgres")]
mod postgres;

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferrofed_identity::dev::Profile;
use ferrofed_server::banner::{Deployment, render};
use ferrofed_server::config::Config;
use ferrofed_server::config::settings::Settings;
use ferrofed_server::config::stored_queries::Store;
use ferrofed_server::config::transport::{self, CleartextError, Encryption, ProtectedSite};
use ferrofed_server::federation::read_registry;
use ferrofed_server::reload::{ReloadError, Reloader};
use ferrofed_server::state::{AppState, StateError};
use ferrofed_server::telemetry::{Rendering, subscriber};
use ferrofed_testkit::oauth;

use crate::facade::registry;
use crate::run::binary;
use crate::support::Logs;

type TestResult = Result<(), Box<dyn Error>>;

/// A synthetic secret no refusal, warning or banner may carry.
const SECRET: &str = "synthetic-transport-secret-Qz7";

/// Node A over TLS.
const HTTPS_A: &str = "https://node-a.example.org/openehr";

/// Node A in cleartext.
const HTTP_A: &str = "http://node-a.example.org/openehr";

/// Node B over TLS; no test sends it a credential.
const HTTPS_B: &str = "https://node-b.example.org/openehr";

/// The key of node A's URL in a refusal.
const ENDPOINT_URL: &str = "the url of endpoint node-a-pub in registry.document";

/// The credentials section of node A.
const SECTION: &str = "credentials.node-a-pub";

/// A bearer token for node A.
fn bearer() -> String {
    format!("[credentials.\"node-a-pub\"]\nbearer_token = \"{SECRET}\"\n")
}

/// Basic credentials for node A.
fn basic() -> String {
    format!("[credentials.\"node-a-pub\"]\nuser = \"gateway\"\npassword = \"{SECRET}\"\n")
}

/// An OAuth 2.0 grant for node A at `token_endpoint`, with the `[signing]`
/// key its assertion needs, written under `dir`.
fn grant(dir: &Path, token_endpoint: &str) -> Result<String, Box<dyn Error>> {
    let key = dir.join("signing.pem");
    std::fs::write(&key, oauth::es384_pem()?)?;
    let key = toml::Value::String(key.display().to_string());
    Ok(format!(
        "[signing]\nkey_file = {key}\njwks_uri = \"https://gw.example.org/.well-known/jwks.json\"\n\n\
         [credentials.\"node-a-pub\".oauth2]\ngrant = \"client_credentials\"\nclient_auth = \"private_key_jwt\"\n\
         token_endpoint = \"{token_endpoint}\"\nclient_id = \"ferrofed-test-gateway\"\nscope = \"system/aql-*.s\"\n"
    ))
}

/// A PIX Manager at `url` with a bearer token, resolving node A.
fn pix(url: &str) -> String {
    format!(
        "[[pixm.manager]]\nurl = \"{url}\"\n\n[pixm.manager.members]\n\"node-a\" = \"urn:oid:2.999.10\"\n\"node-b\" = \"urn:oid:2.999.20\"\n\n\
         [pixm.manager.credentials]\nbearer_token = \"{SECRET}\"\n"
    )
}

/// The configuration text of a gateway under `profile` over the registry
/// document `document`, with `tables` appended.
fn configuration(profile: &str, document: &Path, tables: &str) -> String {
    let document = toml::Value::String(document.display().to_string());
    crate::support::signed(&format!(
        "profile = \"{profile}\"\n\n[registry]\ndocument = {document}\n\n\
         [federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n\n{tables}"
    ))
}

/// Writes the registry of node A at `a` and node B under `dir`, and returns
/// its path.
fn document(dir: &Path, a: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = dir.join("registry.toml");
    std::fs::write(&path, registry(a, HTTPS_B, ""))?;
    Ok(path)
}

/// The settings of a gateway under `profile`, node A at `a`, with `tables`.
fn resolved(dir: &Path, profile: &str, a: &str, tables: &str) -> Result<Settings, Box<dyn Error>> {
    let text = configuration(profile, &document(dir, a)?, tables);
    Ok(Config::from_sources(Some(&text), &BTreeMap::new())?.resolve()?)
}

/// The site `url_key` sends `payload` to.
fn site(url_key: &str, payload: &str) -> ProtectedSite {
    ProtectedSite {
        url_key: url_key.to_owned(),
        payload: payload.to_owned(),
        requires: Encryption::Https,
    }
}

/// The site of PIX Manager 0 with its credentials.
fn pix_site() -> ProtectedSite {
    site(
        "pixm.manager[0].url",
        "pixm.manager[0].credentials and patient identifiers",
    )
}

/// A PIX Manager at `url` with no credentials, resolving node A and node B.
fn bare_pix(url: &str) -> String {
    format!(
        "[[pixm.manager]]\nurl = \"{url}\"\n\n[pixm.manager.members]\n\"node-a\" = \"urn:oid:2.999.10\"\n\"node-b\" = \"urn:oid:2.999.20\"\n"
    )
}

/// Asserts that `config check` and the boot both refuse `settings` on `site`,
/// and that the refusal carries no secret.
fn refused_on(settings: &Settings, site: &ProtectedSite) -> TestResult {
    let expected = CleartextError { site: site.clone() };
    match AppState::check(settings) {
        Err(StateError::Cleartext(refused)) => {
            assert_eq!(expected, refused, "config check names the site");
        }
        other => return Err(format!("config check refuses {site:?}: {other:?}").into()),
    }
    match AppState::build(settings) {
        Err(StateError::Cleartext(refused)) => {
            assert_eq!(expected, refused, "the boot names the site");
            let text = refused.to_string();
            assert!(text.contains(&site.url_key), "{text}");
            assert!(text.contains(&site.payload), "{text}");
            assert!(!text.contains(SECRET), "{text}");
            assert!(!text.contains("example.org"), "{text}");
        }
        other => return Err(format!("serve refuses {site:?}: {other:?}").into()),
    }
    Ok(())
}

/// Asserts that `config check` passes `settings` and reports exactly `sites`.
fn reported(settings: &Settings, sites: &[ProtectedSite]) -> TestResult {
    assert_eq!(
        sites,
        AppState::check(settings)?.as_slice(),
        "config check passes and reports each cleartext credential"
    );
    Ok(())
}

#[test]
fn a_bearer_token_to_a_plain_http_endpoint_is_refused_outside_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    let settings = resolved(dir.path(), "production", HTTP_A, &bearer())?;
    refused_on(&settings, &site(ENDPOINT_URL, SECTION))
}

#[test]
fn basic_credentials_to_a_plain_http_endpoint_are_refused_outside_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    let settings = resolved(dir.path(), "production", HTTP_A, &basic())?;
    refused_on(&settings, &site(ENDPOINT_URL, SECTION))
}

#[test]
fn a_granted_token_to_a_plain_http_endpoint_is_refused_outside_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    let tables = grant(dir.path(), "https://idp.example.org/token")?;
    let settings = resolved(dir.path(), "production", HTTP_A, &tables)?;
    refused_on(&settings, &site(ENDPOINT_URL, SECTION))
}

#[test]
fn a_plain_http_token_endpoint_is_refused_outside_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    let tables = grant(dir.path(), "http://idp.example.org/token")?;
    let settings = resolved(dir.path(), "production", HTTPS_A, &tables)?;
    refused_on(
        &settings,
        &site(
            "credentials.node-a-pub.oauth2.token_endpoint",
            "credentials.node-a-pub.oauth2",
        ),
    )
}

#[test]
fn a_pix_manager_with_credentials_over_plain_http_is_refused_outside_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    let tables = pix("http://pix.example.org/fhir/");
    let settings = resolved(dir.path(), "production", HTTPS_A, &tables)?;
    refused_on(&settings, &pix_site())
}

#[test]
fn a_pix_manager_without_credentials_still_carries_identifiers_and_needs_https() -> TestResult {
    let dir = tempfile::tempdir()?;
    let identifiers = site(
        "pixm.manager[0].url",
        "the patient identifiers asked of pixm.manager[0]",
    );
    let http = bare_pix("http://pix.example.org/fhir/");
    let settings = resolved(dir.path(), "production", HTTPS_A, &http)?;
    refused_on(&settings, &identifiers)?;
    let settings = resolved(dir.path(), "development", HTTPS_A, &http)?;
    reported(&settings, &[identifiers])?;
    let https = bare_pix("https://pix.example.org/fhir/");
    let settings = resolved(dir.path(), "production", HTTPS_A, &https)?;
    reported(&settings, &[])
}

#[test]
fn an_otlp_endpoint_carrying_userinfo_is_refused_outside_development() -> TestResult {
    let text = format!("[metrics]\notlp_endpoint = \"http://gateway:{SECRET}@127.0.0.1:4317\"\n");
    let settings = Config::from_sources(Some(&text), &BTreeMap::new())?.resolve()?;
    let expected = site(
        "metrics.otlp_endpoint",
        "the userinfo of metrics.otlp_endpoint",
    );
    match AppState::check(&settings) {
        Err(StateError::Cleartext(refused)) => {
            assert_eq!(CleartextError { site: expected }, refused);
            assert!(!refused.to_string().contains(SECRET), "{refused}");
            Ok(())
        }
        other => Err(format!("an OTLP push with userinfo is refused: {other:?}").into()),
    }
}

/// An XCPD responding gateway at `url`, every request carrying a synthetic
/// XUA assertion; `[xcpd]` itself refuses `http` outside development.
fn xcpd(url: &str) -> String {
    format!(
        "[xcpd]\nsender_device = \"2.999.40.1\"\naudit = \"log\"\nassertion = \"{SECRET}\"\n\n\
         [[xcpd.gateway]]\nurl = \"{url}\"\ndevice = \"2.999.50.1\"\n\n\
         [xcpd.communities]\n\"2.999.50\" = \"node-a\"\n\"2.999.60\" = \"node-b\"\n"
    )
}

#[test]
fn an_xcpd_gateway_the_assertion_reaches_over_http_is_named_under_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    let settings = resolved(
        dir.path(),
        "development",
        HTTPS_A,
        &xcpd("http://xcpd.example.org/rg"),
    )?;
    assert_eq!(
        vec![site(
            "xcpd.gateway[0].url",
            "xcpd.assertion and patient identifiers"
        )],
        transport::check(&settings, None)?
    );
    let settings = resolved(
        dir.path(),
        "development",
        HTTPS_A,
        &xcpd("https://xcpd.example.org/rg"),
    )?;
    assert_eq!(
        Vec::<ProtectedSite>::new(),
        transport::check(&settings, None)?
    );
    let bare =
        xcpd("http://xcpd.example.org/rg").replace(&format!("assertion = \"{SECRET}\"\n"), "");
    let settings = resolved(dir.path(), "development", HTTPS_A, &bare)?;
    assert_eq!(
        vec![site(
            "xcpd.gateway[0].url",
            "the patient identifiers asked of xcpd.gateway[0]"
        )],
        transport::check(&settings, None)?,
        "a gateway is sent the patient identifier with or without an assertion"
    );
    Ok(())
}

#[test]
fn every_site_starts_under_development_and_is_reported() -> TestResult {
    let dir = tempfile::tempdir()?;
    let tables = format!(
        "{}\n{}",
        grant(dir.path(), "http://idp.example.org/token")?,
        pix("http://pix.example.org/fhir/")
    );
    let settings = resolved(dir.path(), "development", HTTP_A, &tables)?;
    reported(
        &settings,
        &[
            site(ENDPOINT_URL, SECTION),
            site(
                "credentials.node-a-pub.oauth2.token_endpoint",
                "credentials.node-a-pub.oauth2",
            ),
            pix_site(),
        ],
    )?;
    let otlp = format!(
        "profile = \"development\"\n\n[metrics]\notlp_endpoint = \"http://gateway:{SECRET}@127.0.0.1:4317\"\n"
    );
    let otlp = Config::from_sources(Some(&otlp), &BTreeMap::new())?.resolve()?;
    reported(
        &otlp,
        &[site(
            "metrics.otlp_endpoint",
            "the userinfo of metrics.otlp_endpoint",
        )],
    )
}

#[test]
fn the_boot_under_development_logs_a_warning_per_cleartext_credential() -> TestResult {
    let dir = tempfile::tempdir()?;
    let settings = resolved(dir.path(), "development", HTTP_A, &bearer())?;
    let logs = Logs::default();
    let capture = subscriber(Rendering::Json, "info", false, logs.clone())?;
    let built = tracing::subscriber::with_default(capture, || AppState::build(&settings));
    built?;
    let text = logs.text();
    let warnings: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("travels unencrypted"))
        .collect();
    assert_eq!(1, warnings.len(), "{text}");
    let warning = warnings.first().ok_or("one warning")?;
    assert!(warning.contains("\"WARN\""), "{warning}");
    assert!(warning.contains(SECTION), "{warning}");
    assert!(warning.contains(ENDPOINT_URL), "{warning}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains("node-a.example.org"), "{text}");
    Ok(())
}

#[test]
fn the_banner_names_each_cleartext_credential_by_key() -> TestResult {
    let dir = tempfile::tempdir()?;
    let settings = resolved(dir.path(), "development", HTTP_A, &bearer())?;
    let read = read_registry(&settings);
    let described = read.as_ref().map(Result::as_ref);
    let checked = transport::check(&settings, described.and_then(Result::ok));
    let deployment = Deployment::of(
        settings.server.base_path.clone(),
        settings.server.listen,
        described,
        settings.stored_queries.as_ref().map(Store::backend),
        settings.profile == Profile::Development,
    )
    .with_cleartext(checked.as_deref());
    let banner = render("9.9.9", &deployment, false);
    let line = banner
        .lines()
        .find(|line| line.trim_start().starts_with("Unencrypted"))
        .ok_or("the banner names the cleartext credential")?;
    assert!(line.contains(SECTION), "{banner}");
    assert!(!banner.contains(SECRET), "{banner}");
    assert!(!banner.contains("node-a.example.org"), "{banner}");
    Ok(())
}

#[test]
fn a_production_banner_names_no_cleartext_credential() -> TestResult {
    let dir = tempfile::tempdir()?;
    let settings = resolved(dir.path(), "production", HTTP_A, &bearer())?;
    let read = read_registry(&settings);
    let described = read.as_ref().map(Result::as_ref);
    let checked = transport::check(&settings, described.and_then(Result::ok));
    assert!(checked.is_err(), "the boot after the banner stops on it");
    let deployment = Deployment::of(
        settings.server.base_path.clone(),
        settings.server.listen,
        described,
        None,
        false,
    )
    .with_cleartext(checked.as_deref());
    assert!(deployment.cleartext.is_empty());
    Ok(())
}

#[test]
fn every_site_over_https_starts_outside_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    let tables = format!(
        "{}\n{}",
        grant(dir.path(), "https://idp.example.org/token")?,
        pix("https://pix.example.org/fhir/")
    );
    let settings = resolved(dir.path(), "production", HTTPS_A, &tables)?;
    reported(&settings, &[])?;
    AppState::build(&settings)?;
    for credentials in [bearer(), basic()] {
        let settings = resolved(dir.path(), "production", HTTPS_A, &credentials)?;
        reported(&settings, &[])?;
    }
    Ok(())
}

#[test]
fn a_plain_http_url_no_credential_is_sent_to_starts_outside_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    let settings = resolved(dir.path(), "production", HTTP_A, "")?;
    reported(&settings, &[])?;
    AppState::build(&settings)?;
    let otlp = "[metrics]\notlp_endpoint = \"http://127.0.0.1:4317\"\n";
    let settings = Config::from_sources(Some(otlp), &BTreeMap::new())?.resolve()?;
    reported(&settings, &[])
}

#[test]
fn config_check_refuses_a_cleartext_credential_naming_its_key() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = configuration("production", &document(dir.path(), HTTP_A)?, &bearer());
    let output = binary(&["config", "check"], &text)?;
    assert_eq!(Some(78), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(ENDPOINT_URL), "{stderr}");
    assert!(stderr.contains(SECTION), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    Ok(())
}

#[test]
fn config_check_under_development_passes_and_warns_by_key() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = configuration("development", &document(dir.path(), HTTP_A)?, &bearer());
    let output = binary(&["config", "check"], &text)?;
    assert_eq!(Some(0), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning"), "{stderr}");
    assert!(stderr.contains(SECTION), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    Ok(())
}

#[test]
fn admission_check_refuses_a_cleartext_credential() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = configuration("production", &document(dir.path(), HTTP_A)?, &bearer());
    let output = binary(&["admission", "check", "--endpoint", "node-a-pub"], &text)?;
    assert_eq!(Some(78), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(SECTION), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    Ok(())
}

/// A gateway under a profile serving node A over `https` with a bearer
/// token, with its configuration file, its registry document and its
/// reloader.
struct Gateway {
    _dir: tempfile::TempDir,
    config: PathBuf,
    document: PathBuf,
    state: Arc<AppState>,
    reloader: Reloader,
}

impl Gateway {
    fn start(profile: &str) -> Result<Self, Box<dyn Error>> {
        Self::start_with(|document| configuration(profile, document, &bearer()))
    }

    /// Starts over the configuration `text` writes for the registry document.
    fn start_with(text: impl Fn(&Path) -> String) -> Result<Self, Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let document = document(dir.path(), HTTPS_A)?;
        let config = dir.path().join("ferrofed.toml");
        std::fs::write(&config, text(&document))?;
        let settings = Config::load(Some(&config))?.resolve()?;
        let state = Arc::new(AppState::build(&settings)?);
        let reloader = Reloader::new(Some(config.clone()), settings, Arc::clone(&state));
        Ok(Self {
            _dir: dir,
            config,
            document,
            state,
            reloader,
        })
    }
}

#[test]
fn a_reload_that_sends_a_credential_over_plain_http_is_refused() -> TestResult {
    let gateway = Gateway::start("production")?;
    let running = gateway
        .state
        .federation()
        .ok_or("a registry is configured")?;
    std::fs::write(&gateway.document, registry(HTTP_A, HTTPS_B, ""))?;

    let refused = gateway
        .reloader
        .reload()
        .err()
        .ok_or("the reload is refused, as the boot would be")?;
    let ReloadError::Cleartext(cause) = &refused else {
        return Err(format!("a cleartext refusal: {refused:?}").into());
    };
    assert_eq!(site(ENDPOINT_URL, SECTION), cause.site);
    assert_eq!("cleartext", refused.class());
    let after = gateway
        .state
        .federation()
        .ok_or("a registry is configured")?;
    assert!(Arc::ptr_eq(&running, &after), "the running registry stays");
    Ok(())
}

#[test]
fn a_reload_cannot_switch_to_development_to_send_a_credential_in_cleartext() -> TestResult {
    let gateway = Gateway::start("production")?;
    let running = gateway
        .state
        .federation()
        .ok_or("a registry is configured")?;
    std::fs::write(&gateway.document, registry(HTTP_A, HTTPS_B, ""))?;
    std::fs::write(
        &gateway.config,
        configuration("development", &gateway.document, &bearer()),
    )?;

    let refused = gateway
        .reloader
        .reload()
        .err()
        .ok_or("the profile the process started with decides")?;
    assert!(matches!(refused, ReloadError::Profile), "{refused:?}");
    let after = gateway
        .state
        .federation()
        .ok_or("a registry is configured")?;
    assert!(Arc::ptr_eq(&running, &after), "the running registry stays");
    Ok(())
}

/// A gateway under `profile` localized by one XCPD responding gateway at
/// `url`, which carries no assertion, over the registry `document`.
fn xcpd_configuration(profile: &str, document: &Path, url: &str) -> String {
    let document = toml::Value::String(document.display().to_string());
    crate::support::signed(&format!(
        "profile = \"{profile}\"\n\n[registry]\ndocument = {document}\n\n\
         [federation]\nnode_selection = \"localized\"\nid = \"example-federation\"\n\n\
         [xcpd]\nsender_device = \"2.999.40.1\"\naudit = \"log\"\n\n\
         [[xcpd.gateway]]\nurl = \"{url}\"\ndevice = \"2.999.50.1\"\n\n\
         [xcpd.communities]\n\"2.999.50\" = \"node-a\"\n\"2.999.60\" = \"node-b\"\n"
    ))
}

#[test]
fn a_reload_never_admits_an_xcpd_gateway_over_plain_http() -> TestResult {
    let gateway = Gateway::start_with(|document| {
        xcpd_configuration("production", document, "https://xcpd.example.org/rg")
    })?;
    let running = gateway
        .state
        .federation()
        .ok_or("a registry is configured")?;
    for profile in ["production", "development"] {
        std::fs::write(
            &gateway.config,
            xcpd_configuration(profile, &gateway.document, "http://xcpd.example.org/rg"),
        )?;
        let refused = gateway
            .reloader
            .reload()
            .err()
            .ok_or_else(|| format!("an http gateway in a {profile} file is refused"))?;
        match (profile, &refused) {
            ("production", ReloadError::Config(cause)) => {
                let mut text = String::new();
                let mut next: Option<&dyn Error> = Some(cause);
                while let Some(error) = next {
                    text.push_str(&error.to_string());
                    next = error.source();
                }
                assert!(text.contains("xcpd.gateway[0].url"), "{text}");
            }
            ("development", ReloadError::Profile) => {}
            _ => {
                return Err(format!("{profile}: refused for the wrong reason: {refused:?}").into());
            }
        }
        let after = gateway
            .state
            .federation()
            .ok_or("a registry is configured")?;
        assert!(Arc::ptr_eq(&running, &after), "the running registry stays");
    }
    Ok(())
}

#[test]
fn a_reload_that_changes_the_profile_is_refused_logged_and_counted() -> TestResult {
    for (boot, file) in [("production", "development"), ("development", "production")] {
        let gateway = Gateway::start(boot)?;
        let running = gateway
            .state
            .federation()
            .ok_or("a registry is configured")?;
        std::fs::write(
            &gateway.config,
            configuration(file, &gateway.document, &bearer()),
        )?;

        let logs = Logs::default();
        let capture = subscriber(Rendering::Json, "info", false, logs.clone())?;
        let reloaded = tracing::subscriber::with_default(capture, || gateway.reloader.reload());
        let Err(refused) = reloaded else {
            return Err(format!("{boot} to {file} is refused, as a restart decides it").into());
        };
        assert!(matches!(refused, ReloadError::Profile), "{refused:?}");
        assert_eq!("profile", refused.class());
        let after = gateway
            .state
            .federation()
            .ok_or("a registry is configured")?;
        assert!(Arc::ptr_eq(&running, &after), "the running registry stays");
        let text = logs.text();
        assert!(
            text.lines()
                .any(|line| line.contains("\"ERROR\"") && line.contains("\"profile\"")),
            "{text}"
        );
        let metrics = gateway.state.metrics().render()?;
        assert!(
            metrics.lines().any(|line| {
                line.starts_with("ferrofed_registry_reloads_total")
                    && line.contains("result=\"refused\"")
                    && line.ends_with(" 1")
            }),
            "{metrics}"
        );
    }
    Ok(())
}

#[test]
fn a_reload_under_development_applies_and_names_the_cleartext_credential() -> TestResult {
    let gateway = Gateway::start("development")?;
    std::fs::write(&gateway.document, registry(HTTP_A, HTTPS_B, ""))?;

    let logs = Logs::default();
    let capture = subscriber(Rendering::Json, "info", false, logs.clone())?;
    let applied = tracing::subscriber::with_default(capture, || gateway.reloader.reload())?;
    assert_eq!(vec![site(ENDPOINT_URL, SECTION)], applied.cleartext);
    let text = logs.text();
    assert!(
        text.lines()
            .any(|line| line.contains("travels unencrypted") && line.contains(SECTION)),
        "{text}"
    );
    assert!(!text.contains(SECRET), "{text}");
    Ok(())
}

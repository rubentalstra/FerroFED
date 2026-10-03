// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! `[registry.mcsd]`: `config check` reads the registry from the directory
//! and refuses what the registry's FHIR form refuses, a directory that does
//! not answer refuses the boot, and the section refuses a second registry
//! source, a credential in its URL and an OAuth 2.0 grant.
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

use std::error::Error;

use ferrofed_server::EXIT_CONFIG;
use ferrofed_server::config::error::Error as ConfigError;
use ferrofed_testkit::mcsd::HarnessDirectory;
use ferrofed_testkit::unreachable;

use super::{Gateway, member, members, settings};
use crate::run::binary;

type TestResult = Result<(), Box<dyn Error>>;

/// The `[registry.mcsd]` section over `base`, with the federation it needs.
fn section(base: &str, extra: &str) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:1\"\n\n[registry.mcsd]\nurl = \"{base}\"\ndeadline_ms = 2000\n{extra}\n\n[federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
    )
}

#[tokio::test]
async fn config_check_reads_the_registry_from_the_directory() -> TestResult {
    let harness = HarnessDirectory::start().await;
    harness.publish(&members(
        "http://127.0.0.1:9/openehr",
        "http://127.0.0.1:10/openehr",
    ))?;
    let output = binary(&["config", "check"], &section(&harness.base(), ""))?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !harness.requests().await.is_empty(),
        "the check read the directory"
    );
    Ok(())
}

#[tokio::test]
async fn config_check_refuses_a_directory_endpoint_relying_on_hl7_fhir_rest() -> TestResult {
    let harness = HarnessDirectory::start().await;
    let a = member("a", "http://127.0.0.1:9/openehr");
    harness.put_organization(a.organisation()?);
    harness.put_endpoint(a.endpoint_with_connection_type(
        "http://terminology.hl7.org/CodeSystem/endpoint-connection-type",
        "hl7-fhir-rest",
    )?);
    let output = binary(&["config", "check"], &section(&harness.base(), ""))?;
    assert_eq!(Some(i32::from(EXIT_CONFIG)), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("endpoint node-a-pub") && stderr.contains("hl7-fhir-rest"),
        "the endpoint and the refused code are named: {stderr}"
    );
    Ok(())
}

#[test]
fn a_directory_that_does_not_answer_refuses_the_boot() -> TestResult {
    let output = binary(
        &["config", "check"],
        &section(&format!("{}/fhir", unreachable::BASE), ""),
    )?;
    assert_eq!(Some(i32::from(EXIT_CONFIG)), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("care services directory could not be read"),
        "{stderr}"
    );
    Ok(())
}

#[tokio::test]
async fn the_credentials_reach_the_directory_and_no_rendering() -> TestResult {
    const TOKEN: &str = "Qz7SentinelDirectoryToken";
    let harness = HarnessDirectory::start().await;
    harness.publish(&members(
        "http://127.0.0.1:9/openehr",
        "http://127.0.0.1:10/openehr",
    ))?;
    let text = format!(
        "{}\n[registry.mcsd.credentials]\nbearer_token = \"{TOKEN}\"\n",
        super::config(&harness.base(), 5_000)
    );
    let resolved = settings(&text)?;
    let rendered = format!("{resolved:?}");
    assert!(!rendered.contains(TOKEN), "{rendered}");
    let directory = resolved
        .registry_directory
        .as_ref()
        .ok_or("a directory is configured")?;
    let (registry, _snapshot) = ferrofed_server::directory::DirectoryRegistry::open(directory)?;
    assert!(!format!("{registry:?}").contains(TOKEN));
    let sent = harness.authorizations().await;
    assert!(!sent.is_empty());
    assert!(
        sent.iter()
            .all(|value| value.as_deref() == Some(format!("Bearer {TOKEN}").as_str())),
        "every request carries the token"
    );
    let _booted = Gateway::boot(&harness, 5_000)?;
    Ok(())
}

#[test]
fn a_document_and_a_directory_together_are_refused() -> TestResult {
    let refused = settings(
        "[registry]\ndocument = \"/nonexistent/registry.toml\"\n\n[registry.mcsd]\nurl = \"https://directory.example.org/fhir\"\n",
    );
    let error = refused.err().ok_or("two sources are refused")?;
    assert!(
        matches!(
            error.downcast_ref::<ConfigError>(),
            Some(ConfigError::TwoRegistrySources)
        ),
        "{error}"
    );
    Ok(())
}

#[test]
fn a_directory_url_with_a_credential_or_another_scheme_is_refused() -> TestResult {
    for url in [
        "https://user:Qz7Sentinel@directory.example.org/fhir",
        "ftp://directory.example.org/fhir",
    ] {
        let refused = settings(&format!("[registry.mcsd]\nurl = \"{url}\"\n"));
        let error = refused.err().ok_or("the URL is refused")?;
        assert!(
            matches!(
                error.downcast_ref::<ConfigError>(),
                Some(ConfigError::HttpUrl { key }) if key == "registry.mcsd.url"
            ),
            "{url}: {error}"
        );
        assert!(!error.to_string().contains("Qz7Sentinel"), "{error}");
    }
    Ok(())
}

#[test]
fn a_grant_and_a_zero_interval_are_refused() {
    let grant = settings(
        "[registry.mcsd]\nurl = \"https://directory.example.org/fhir\"\n\n[registry.mcsd.credentials.oauth2]\ngrant = \"client_credentials\"\nclient_auth = \"private_key_jwt\"\ntoken_endpoint = \"https://auth.example.org/token\"\nclient_id = \"ferrofed\"\nscope = \"system/aql-*.s\"\n",
    );
    assert!(
        matches!(
            grant
                .err()
                .as_deref()
                .and_then(|error| error.downcast_ref::<ConfigError>()),
            Some(ConfigError::GrantNotHere { .. })
        ),
        "a grant is refused"
    );
    let zero = settings(
        "[registry.mcsd]\nurl = \"https://directory.example.org/fhir\"\nrefresh_interval_s = 0\n",
    );
    assert!(
        matches!(
            zero.err().as_deref().and_then(|error| error.downcast_ref::<ConfigError>()),
            Some(ConfigError::Zero { key }) if key == "registry.mcsd.refresh_interval_s"
        ),
        "a zero interval is refused"
    );
}

/// The directory's credentials travel over `https` only, outside the
/// development profile, and the refusal comes before the directory is asked;
/// under that profile the site is reported (no specification governs this:
/// our own design).
#[test]
fn directory_credentials_over_plain_http_are_refused_outside_development() -> TestResult {
    let section = "[registry.mcsd]\nurl = \"http://directory.example.org/fhir\"\n\n[registry.mcsd.credentials]\nbearer_token = \"synthetic-directory-token\"\n";
    let refused = settings(section);
    let error = refused.err().ok_or("cleartext credentials are refused")?;
    let text = error.to_string();
    assert!(
        text.contains("registry.mcsd.url") && text.contains("registry.mcsd.credentials"),
        "{text}"
    );
    assert!(!text.contains("synthetic-directory-token"), "{text}");
    let development = settings(&format!("profile = \"development\"\n\n{section}"))?;
    let reported = ferrofed_server::config::transport::check(&development, None)?;
    assert_eq!(
        vec!["registry.mcsd.url"],
        reported
            .iter()
            .map(|site| site.url_key.as_str())
            .collect::<Vec<_>>()
    );
    let without = settings("[registry.mcsd]\nurl = \"http://directory.example.org/fhir\"\n")?;
    assert!(
        ferrofed_server::config::transport::check(&without, None)?.is_empty(),
        "a directory asked with no credential sends nothing protected"
    );
    Ok(())
}

#[test]
fn a_zero_deadline_or_cap_is_refused_and_the_defaults_hold() -> TestResult {
    for key in ["deadline_ms", "max_pages", "max_bytes", "max_entries"] {
        let refused = settings(&format!(
            "[registry.mcsd]\nurl = \"https://directory.example.org/fhir\"\n{key} = 0\n"
        ));
        let full = format!("registry.mcsd.{key}");
        assert!(
            matches!(
                refused.err().as_deref().and_then(|error| error.downcast_ref::<ConfigError>()),
                Some(ConfigError::Zero { key }) if *key == full
            ),
            "a zero {key} is refused"
        );
    }
    let resolved = settings("[registry.mcsd]\nurl = \"https://directory.example.org/fhir\"\n")?;
    let directory = resolved
        .registry_directory
        .as_ref()
        .ok_or("a directory is configured")?;
    assert_eq!(std::time::Duration::from_secs(30), directory.deadline);
    assert_eq!(
        (200, 64 << 20, 50_000),
        (
            directory.max_pages,
            directory.max_bytes,
            directory.max_entries
        )
    );
    Ok(())
}

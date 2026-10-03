// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The stored-query store's PostgreSQL connection under the
//! encrypted-connection policy: outside the development profile, a
//! connection string that carries a password to a networked host must set
//! `sslmode=require`, refused by `config check`, `serve` and a reload, naming
//! `stored_queries.url`; under the development profile it starts and is
//! named. No specification governs this: our own design.

use std::sync::Arc;

use ferrofed_server::config::transport::{Encryption, ProtectedSite};
use ferrofed_server::reload::ReloadError;

use super::{
    Gateway, HTTPS_A, SECRET, TestResult, binary, configuration, document, refused_on, reported,
    resolved,
};

/// The site the store's connection string is held as.
fn store_site() -> ProtectedSite {
    ProtectedSite {
        url_key: String::from("stored_queries.url"),
        payload: String::from("the password in stored_queries.url"),
        requires: Encryption::Tls,
    }
}

/// The `[stored_queries]` table of a PostgreSQL store at `url`.
fn store(url: &str) -> String {
    format!("[stored_queries]\nbackend = \"postgres\"\nurl = \"{url}\"\n")
}

/// A connection to a networked host with a password and `query` appended.
fn networked(query: &str) -> String {
    format!("postgres://ferrofed:{SECRET}@db.example.org:5432/ferrofed{query}")
}

#[test]
fn a_password_without_required_tls_is_refused_outside_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    for query in ["", "?sslmode=prefer", "?sslmode=disable"] {
        let settings = resolved(dir.path(), "production", HTTPS_A, &store(&networked(query)))?;
        refused_on(&settings, &store_site())?;
    }
    let pairs = format!("host=db.example.org user=ferrofed password={SECRET} dbname=ferrofed");
    let settings = resolved(dir.path(), "production", HTTPS_A, &store(&pairs))?;
    refused_on(&settings, &store_site())
}

#[test]
fn required_tls_no_password_or_a_unix_socket_starts_outside_development() -> TestResult {
    let dir = tempfile::tempdir()?;
    for url in [
        networked("?sslmode=require"),
        String::from("postgres://ferrofed@db.example.org:5432/ferrofed"),
        format!("host=/var/run/postgresql user=ferrofed password={SECRET} dbname=ferrofed"),
    ] {
        let settings = resolved(dir.path(), "production", HTTPS_A, &store(&url))?;
        reported(&settings, &[])?;
    }
    Ok(())
}

#[test]
fn a_unix_socket_named_beside_a_hostaddr_is_networked() -> TestResult {
    let dir = tempfile::tempdir()?;
    let url = format!(
        "host=/var/run/postgresql hostaddr=192.0.2.10 user=ferrofed password={SECRET} dbname=ferrofed"
    );
    let settings = resolved(dir.path(), "production", HTTPS_A, &store(&url))?;
    refused_on(&settings, &store_site())
}

#[test]
fn the_development_profile_starts_and_names_the_unencrypted_store() -> TestResult {
    let dir = tempfile::tempdir()?;
    let settings = resolved(dir.path(), "development", HTTPS_A, &store(&networked("")))?;
    reported(&settings, &[store_site()])
}

#[test]
fn config_check_refuses_the_store_naming_its_key_and_never_its_password() -> TestResult {
    let dir = tempfile::tempdir()?;
    let text = configuration(
        "production",
        &document(dir.path(), HTTPS_A)?,
        &store(&networked("?sslmode=prefer")),
    );
    let output = binary(&["config", "check"], &text)?;
    assert_eq!(Some(78), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("stored_queries.url"), "{stderr}");
    assert!(stderr.contains("sslmode=require"), "{stderr}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    assert!(!stderr.contains("db.example.org"), "{stderr}");
    Ok(())
}

#[test]
fn a_reload_that_adds_an_unencrypted_store_is_refused() -> TestResult {
    let gateway = Gateway::start("production")?;
    let running = gateway
        .state
        .federation()
        .ok_or("a registry is configured")?;
    std::fs::write(
        &gateway.config,
        configuration(
            "production",
            &gateway.document,
            &format!("{}\n{}", super::bearer(), store(&networked(""))),
        ),
    )?;

    let refused = gateway
        .reloader
        .reload()
        .err()
        .ok_or("a store the restart would refuse is refused now")?;
    let ReloadError::Cleartext(cause) = &refused else {
        return Err(format!("a cleartext refusal: {refused:?}").into());
    };
    assert_eq!(store_site(), cause.site);
    assert_eq!("cleartext", refused.class());
    let after = gateway
        .state
        .federation()
        .ok_or("a registry is configured")?;
    assert!(Arc::ptr_eq(&running, &after), "the running registry stays");
    Ok(())
}

// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The binary is thin over the library, so a test drives the real run path,
//! and the boot refusals are checked on the real binary too.

use ferrofed_server::{EXIT_CONFIG, EXIT_USAGE};
use std::error::Error as StdError;
use std::io::Write;
use std::process::{Command, ExitCode, Output};

/// Runs the library entry point with `argv`.
fn run(argv: &[&str]) -> ExitCode {
    ferrofed_server::run(argv.iter().map(|word| (*word).to_owned()))
}

/// Renders `code` the way `ExitCode` renders itself, so two are comparable.
fn rendered(code: ExitCode) -> String {
    format!("{code:?}")
}

/// Runs the real binary with `args` and a configuration file holding `toml`,
/// with the suite's `[auth]` added when `toml` names none, and its
/// `[signing]` when `toml` configures a registry and names none.
pub(crate) fn binary(args: &[&str], toml: &str) -> Result<Output, Box<dyn StdError>> {
    let toml = crate::support::signed(toml);
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(toml.as_bytes())?;
    if !toml.contains("[auth") {
        file.write_all(crate::support::auth_toml()?.as_bytes())?;
    }
    let output = Command::new(env!("CARGO_BIN_EXE_ferrofed"))
        .args(args)
        .arg("--config")
        .arg(file.path())
        .env_remove("FERROFED_CONFIG")
        .output()?;
    Ok(output)
}

#[test]
fn no_job_at_all_is_a_usage_refusal() {
    assert_eq!(
        rendered(ExitCode::from(EXIT_USAGE)),
        rendered(run(&["ferrofed"])),
        "the binary does nothing without a job"
    );
}

#[test]
fn help_and_version_are_printed_and_exit_zero() {
    assert_eq!(
        rendered(ExitCode::SUCCESS),
        rendered(run(&["ferrofed", "--help"]))
    );
    assert_eq!(
        rendered(ExitCode::SUCCESS),
        rendered(run(&["ferrofed", "--version"]))
    );
}

#[test]
fn a_configuration_file_that_does_not_exist_exits_seventy_eight() {
    assert_eq!(
        rendered(ExitCode::from(EXIT_CONFIG)),
        rendered(run(&[
            "ferrofed",
            "serve",
            "--config",
            "/nonexistent/ferrofed.toml"
        ])),
        "a refused configuration is EX_CONFIG, never a generic failure"
    );
}

#[test]
#[expect(
    clippy::panic_in_result_fn,
    reason = "a test asserts, and returns its setup errors"
)]
fn config_check_accepts_a_valid_file_and_binds_nothing() -> Result<(), Box<dyn StdError>> {
    let output = binary(
        &["config", "check"],
        "[server]\nlisten = \"127.0.0.1:1\"\n[credentials.\"a\"]\nbearer_token = \"synthetic\"\n",
    )?;
    assert_eq!(Some(0), output.status.code());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("valid"), "{stdout}");
    assert!(
        !stdout.contains("synthetic"),
        "a secret is never printed: {stdout}"
    );
    Ok(())
}

#[test]
#[expect(
    clippy::panic_in_result_fn,
    reason = "a test asserts, and returns its setup errors"
)]
fn the_binary_refuses_to_boot_on_an_unknown_key_and_on_a_doubly_set_secret()
-> Result<(), Box<dyn StdError>> {
    let mut secret = tempfile::NamedTempFile::new()?;
    secret.write_all(b"synthetic-from-file")?;
    let doubled = format!(
        "[credentials.\"a\"]\nbearer_token = \"synthetic-inline\"\nbearer_token_file = {:?}\n",
        secret.path()
    );
    for (toml, names) in [
        ("[server]\nlisten_port = 8080\n", "listen_port"),
        (doubled.as_str(), "credentials.a.bearer_token"),
    ] {
        for job in [&["serve"][..], &["config", "check"][..]] {
            let output = binary(job, toml)?;
            assert_eq!(
                Some(i32::from(EXIT_CONFIG)),
                output.status.code(),
                "{job:?} refuses {names}"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(names), "{job:?} names {names}: {stderr}");
            assert!(
                !stderr.contains("synthetic-inline") && !stderr.contains("synthetic-from-file"),
                "the refusal quotes no secret: {stderr}"
            );
        }
    }
    Ok(())
}

/// §11.5: `serve` and `config check` both refuse a request timeout that does
/// not exceed the overall fan-out budget plus the combining margin.
#[test]
#[expect(
    clippy::panic_in_result_fn,
    reason = "a test asserts, and returns its setup errors"
)]
fn the_binary_refuses_a_request_timeout_inside_the_fan_out_budget() -> Result<(), Box<dyn StdError>>
{
    let toml = "[server]\nlisten = \"127.0.0.1:1\"\nrequest_timeout_ms = 25000\n\n\
                [registry]\ndocument = \"/nonexistent/registry.toml\"\n";
    for job in [&["serve"][..], &["config", "check"][..]] {
        let output = binary(job, toml)?;
        assert_eq!(
            Some(i32::from(EXIT_CONFIG)),
            output.status.code(),
            "{job:?} refuses the timeouts"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("server.request_timeout_ms")
                && stderr.contains("federation.overall_timeout_ms"),
            "{job:?} names both keys: {stderr}"
        );
    }
    Ok(())
}

/// N21, §12.2: `config check` refuses a registry document whose
/// `[[creating_system]]` mapping names an undeclared endpoint, naming the key.
#[test]
#[expect(
    clippy::panic_in_result_fn,
    reason = "a test asserts, and returns its setup errors"
)]
fn config_check_refuses_a_creating_system_mapping_to_an_undeclared_endpoint()
-> Result<(), Box<dyn StdError>> {
    let mut document = tempfile::NamedTempFile::new()?;
    document.write_all(
        b"[[organisation]]\nid = \"org-a\"\n\n\
          [[node]]\nid = \"node-a\"\norganisation = \"org-a\"\nsystem_id = \"cdr-a.example.org\"\n\n\
          [[endpoint]]\nid = \"node-a-pub\"\nnode = \"node-a\"\nurl = \"http://127.0.0.1:9/openehr\"\n\
          connection_type = \"openehr-rest-query\"\nmanaging_organisation = \"org-a\"\n\n\
          [[creating_system]]\ncreating_system_id = \"legacy-a.example.org\"\nendpoint = \"node-z-pub\"\n",
    )?;
    let path = toml::Value::String(document.path().display().to_string());
    let toml = format!(
        "[server]\nlisten = \"127.0.0.1:1\"\n\n[registry]\ndocument = {path}\n\n\
         [federation]\nnode_selection = \"ask-all\"\nid = \"example-federation\"\n"
    );
    let output = binary(&["config", "check"], &toml)?;
    assert_eq!(Some(i32::from(EXIT_CONFIG)), output.status.code());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("creating_system legacy-a.example.org")
            && stderr.contains("endpoint node-z-pub"),
        "the mapping and its endpoint are named: {stderr}"
    );
    Ok(())
}

/// The compose quickstart's configuration and registry document pass `config
/// check` together, so its four members, their credentials and the static
/// cross-reference rows the seed script reads agree with one another. The
/// suite's key stands in for the one `scripts/quickstart/signing-key.sh`
/// writes before the first `docker compose up`.
#[test]
#[expect(
    clippy::panic_in_result_fn,
    reason = "a test asserts, and returns its setup errors"
)]
fn the_quickstart_configuration_passes_config_check() -> Result<(), Box<dyn StdError>> {
    let quickstart =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker/quickstart");
    let output = Command::new(env!("CARGO_BIN_EXE_ferrofed"))
        .args(["config", "check", "--config"])
        .arg(quickstart.join("ferrofed.toml"))
        .env_remove("FERROFED_CONFIG")
        .env(
            "FERROFED__REGISTRY__DOCUMENT",
            quickstart.join("registry.toml"),
        )
        .env(
            "FERROFED__SIGNING__KEY_FILE",
            crate::support::signing_key_file(),
        )
        .output()?;
    assert_eq!(
        Some(0),
        output.status.code(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("valid"), "{stdout}");
    Ok(())
}

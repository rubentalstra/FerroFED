// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The startup banner `ferrofed serve` prints before its log starts.
//!
//! The wordmark, the product version, the maintainer and the repository, the
//! releases the gateway serves, and the deployment facts an operator checks
//! first. The wordmark is committed text, so the boot path loads no font and
//! carries no dependency for it. Every pin is read from the crate constant
//! `scripts/checks/versions.sh` holds to the pin matrix, never typed here.
//!
//! The banner prints only when the console renders the terminal form, so a
//! log pipeline reading JSON never receives it. It shows counts, an address,
//! a path, switches, and the configuration keys of each credential or
//! patient identifier that travels unencrypted: never a credential, a
//! URL, a header value or anything from a request. No specification governs the banner: our own design.

use std::fmt::Write as _;
use std::net::SocketAddr;

use ferrofed_registry::snapshot::RegistrySnapshot;

use crate::base_path::BasePath;
use crate::config::stored_queries::Backend;
use crate::config::transport::{CleartextError, ProtectedSite};
use crate::federation::FederationError;
use crate::telemetry::{Format, Rendering};

/// The `FerroFED` wordmark in the `FIGlet` "standard" font.
///
/// Rendered once with `figlet -f standard FerroFED`, trailing blanks
/// trimmed: five lines, at most 43 columns.
pub const WORDMARK: &str = r"
 _____                   _____ _____ ____
|  ___|__ _ __ _ __ ___ |  ___| ____|  _ \
| |_ / _ \ '__| '__/ _ \| |_  |  _| | | | |
|  _|  __/ |  | | | (_) |  _| | |___| |_| |
|_|  \___|_|  |_|  \___/|_|   |_____|____/";

/// The words of the development notice, before wrapping.
pub const DEVELOPMENT_NOTICE: &str = "DEVELOPMENT: this deployment runs the development profile, \
     which may resolve patients from a static development table. It must not hold or reach \
     real patient data.";

/// The column every value of the aligned list starts at, after its label.
const LABEL_WIDTH: usize = 17;

/// The width the development notice is wrapped to, inside 80 columns.
const NOTICE_WIDTH: usize = 76;

/// The start of the red the development notice prints in, on a terminal
/// with colour.
const RED: &str = "\x1b[1;31m";

/// The end of the red.
const RESET: &str = "\x1b[0m";

/// The registry document, as the banner reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registry {
    /// No registry document is configured, so the gateway federates nothing.
    Unset,
    /// The document read, with how many members and endpoints it declares.
    Read {
        /// The member nodes.
        members: usize,
        /// The endpoints of those members.
        endpoints: usize,
    },
    /// The document does not load; the boot that follows says why and stops.
    Unreadable,
}

/// The deployment facts the banner prints, each safe to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    /// The path every route sits under (§4.1, N28).
    pub base_path: BasePath,
    /// The socket address the server binds.
    pub listen: SocketAddr,
    /// The registry document.
    pub registry: Registry,
    /// The backend of the stored-query registry, when it is offered
    /// (§12.7).
    pub stored_queries: Option<Backend>,
    /// Whether the configuration declares the development profile.
    pub development: bool,
    /// Each credential or patient identifier that travels unencrypted,
    /// which only the development profile allows, by key.
    pub cleartext: Vec<ProtectedSite>,
}

impl Deployment {
    /// Returns the deployment the gateway serves under `base_path` on
    /// `listen`, with the registry `document`
    /// [`read_registry`](crate::federation::read_registry) read, the backend
    /// of the `stored_queries` registry when it is offered, and whether the
    /// profile is `development`.
    ///
    /// The boot builds the gateway over the same read, so the counts are
    /// those of the registry it serves.
    #[must_use]
    pub fn of(
        base_path: BasePath,
        listen: SocketAddr,
        document: Option<Result<&RegistrySnapshot, &FederationError>>,
        stored_queries: Option<Backend>,
        development: bool,
    ) -> Self {
        // NOTE: no specification governs this: our own design; the banner takes
        // only the values it displays, so no struct carrying a credential reaches stdout.
        let registry = match document {
            None => Registry::Unset,
            Some(Ok(snapshot)) => Registry::Read {
                members: snapshot.nodes().count(),
                endpoints: snapshot.endpoints().count(),
            },
            // NOTE: no specification governs this: our own design; the build over
            // this same read stops the boot on the typed error the banner omits.
            Some(Err(_)) => Registry::Unreadable,
        };
        Self {
            base_path,
            listen,
            registry,
            stored_queries,
            development,
            cleartext: Vec::new(),
        }
    }

    /// Returns this deployment with the credentials and patient identifiers
    /// [`check`](crate::config::transport::check) found travelling over plain
    /// `http`.
    ///
    /// A refusal shows none: the build over the same settings stops the boot
    /// on it.
    #[must_use]
    pub fn with_cleartext(mut self, checked: Result<&[ProtectedSite], &CleartextError>) -> Self {
        // NOTE: no specification governs this: our own design; the build over
        // these settings stops the boot on the typed error the banner omits.
        self.cleartext = match checked {
            Ok(sites) => sites.to_vec(),
            Err(_refused) => Vec::new(),
        };
        self
    }
}

/// Decides whether `serve` prints the banner.
///
/// It prints only when the console renders the terminal form: `auto` with
/// stdout piped to a collector renders JSON, and no banner may precede it.
/// The caller passes whether stdout is a terminal, so a test fixes the
/// decision without owning one.
#[must_use]
pub fn prints(format: Format, stdout_is_terminal: bool) -> bool {
    format.resolve(stdout_is_terminal) == Rendering::Pretty
}

/// Renders the banner for the product `version` and `deployment`, the
/// development notice in red when `colour` is set.
///
/// Parameterized so a test fixes every input; [`print()`] supplies the
/// running version.
#[must_use]
pub fn render(version: &str, deployment: &Deployment, colour: bool) -> String {
    let mut out = format!(
        "{WORDMARK}\n\n  openEHR federation gateway · v{version}\n  Maintained by {} · {}\n\n",
        env!("CARGO_PKG_AUTHORS"),
        env!("CARGO_PKG_REPOSITORY"),
    );
    let pins = [
        ("Federation Tier", openehr_federation::FEDERATION_SPEC),
        ("ITS-REST", ferrofed_engine::ITS_REST),
        ("AQL", openehr_federation::AQL),
        ("openehr-*", crate::OPENEHR_FAMILY),
    ];
    for (label, pin) in pins {
        line(&mut out, label, pin);
    }
    out.push('\n');
    line(&mut out, "Base path", deployment.base_path.as_str());
    line(&mut out, "Listen", &deployment.listen.to_string());
    let registry = match deployment.registry {
        Registry::Unset => "none, the gateway federates nothing".to_owned(),
        Registry::Read { members, endpoints } => format!(
            "{}, {}",
            counted(members, "member"),
            counted(endpoints, "endpoint")
        ),
        Registry::Unreadable => "does not load; the log says why".to_owned(),
    };
    line(&mut out, "Registry", &registry);
    let stored_queries = match deployment.stored_queries {
        None => "off",
        Some(Backend::Redb) => "on, redb",
        Some(Backend::Postgres) => "on, postgres",
        Some(Backend::Files) => "on, files, read-only",
    };
    line(&mut out, "Stored queries", stored_queries);
    for (index, site) in deployment.cleartext.iter().enumerate() {
        let label = if index == 0 { "Unencrypted" } else { "" };
        line(&mut out, label, &site.payload);
    }
    if deployment.development {
        // The same words with and without colour, because colour is the first
        // thing a scraped log loses.
        let (on, off) = if colour { (RED, RESET) } else { ("", "") };
        out.push('\n');
        for words in wrap(DEVELOPMENT_NOTICE, NOTICE_WIDTH) {
            for part in [on, "  ", &words, off, "\n"] {
                out.push_str(part);
            }
        }
    }
    out
}

/// Prints the banner for the running version to stdout.
///
/// `serve` calls it before the log subscriber exists, so no formatter
/// touches the art.
#[expect(
    clippy::print_stdout,
    reason = "the startup banner is console output, printed before any log subscriber exists"
)]
pub fn print(deployment: &Deployment, colour: bool) {
    println!("{}", render(env!("CARGO_PKG_VERSION"), deployment, colour));
}

/// Appends one aligned `label value` line to `out`.
fn line(out: &mut String, label: &str, value: &str) {
    // NOTE: no specification governs this: our own design; writing to a
    // String cannot fail.
    let _written: std::fmt::Result = writeln!(out, "  {label:<LABEL_WIDTH$}{value}");
}

/// Returns `count` with `noun`, plural unless the count is one.
fn counted(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Wraps `text` greedily at `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

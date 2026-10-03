// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The refusals of a configuration the server does not start on.

use std::fmt;
use std::path::{Path, PathBuf};

use ferrofed_registry::error::IdError;
use openehr_its::rest::client::InvalidCredentials;

use crate::config::ENV_PREFIX;
use crate::config::stored_queries::Backend;
use crate::config::transport::{CleartextError, TrustAnchorError};

/// A configuration the server refuses to start on.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The configuration file could not be read.
    #[error("the configuration file {} could not be read", path.display())]
    Read {
        /// The path that was tried.
        path: PathBuf,
        /// What the file system reported.
        #[source]
        source: std::io::Error,
    },
    /// The configuration does not parse, names an unknown key, or holds a
    /// value of the wrong type.
    ///
    /// It carries a [`ParseFault`] in place of the TOML reader's error: that
    /// error quotes the offending source line, and a line of `[dev]` holds a
    /// patient identifier, a line of `[credentials]` a secret (§5.4.1, N33).
    #[error("{fault}")]
    Parse {
        /// Where the fault is and what kind it is, with no source text.
        fault: ParseFault,
    },
    /// The merged configuration could not be written back for re-reading.
    #[error("the configuration could not be assembled")]
    Assemble {
        /// What the TOML writer reported.
        #[source]
        source: toml::ser::Error,
    },
    /// An environment override names no key under the prefix.
    #[error("{name} names no configuration key; use {ENV_PREFIX}<SECTION>__<KEY>")]
    EnvName {
        /// The variable that was read.
        name: String,
    },
    /// An environment override addresses a key under a value that is not a
    /// section.
    #[error("{name} addresses a key under a value that is not a section")]
    EnvShape {
        /// The variable that was read.
        name: String,
    },
    /// A value and its `_file` sibling are both set.
    #[error("{key} is set together with {key}_file; set one of them")]
    Conflict {
        /// The inline key.
        key: String,
    },
    /// A `_file` sibling could not be read.
    #[error("{key} names {}, which could not be read", path.display())]
    Secret {
        /// The `_file` key.
        key: String,
        /// The path it named.
        path: PathBuf,
        /// What the file system reported.
        #[source]
        source: std::io::Error,
    },
    /// A secret read from a `_file` sibling is empty.
    #[error("{key} names {}, which holds no secret", path.display())]
    EmptySecret {
        /// The `_file` key.
        key: String,
        /// The path it named.
        path: PathBuf,
    },
    /// Stored-query definitions would be distributed with no stored-query
    /// registry to distribute from (§12.7, N44).
    #[error(
        "federation.fan_out_stored_queries distributes the stored-query registry's definitions, \
         and no registry is offered: configure [stored_queries], or turn the setting off (§12.7)"
    )]
    StoredQueryFanOutWithoutRegistry,
    /// Stored-query definitions would be distributed from a read-only
    /// registry, which stores none to distribute (§12.7, N44).
    #[error(
        "federation.fan_out_stored_queries distributes the definitions a PUT stores, \
         and stored_queries.backend = \"files\" refuses every PUT: turn the setting off, \
         or choose a backend that stores (§12.7)"
    )]
    StoredQueryFanOutReadOnly,
    /// A `[stored_queries]` key the chosen backend does not read is set.
    #[error("{key} does not apply to stored_queries.backend = \"{backend}\"; remove it")]
    StoreKey {
        /// The key that is set.
        key: String,
        /// The backend chosen.
        backend: Backend,
    },
    /// The chosen stored-query backend is not built into this binary.
    #[error(
        "stored_queries.backend = \"{backend}\" is not built into this binary; build ferrofed-server with its {backend} feature"
    )]
    StoreBackendUnavailable {
        /// The backend chosen.
        backend: Backend,
    },
    /// The PostgreSQL connection string does not parse.
    ///
    /// The parser's message is not kept: it may quote a character of the
    /// string, which holds a password.
    #[error("{key} is not a PostgreSQL connection string, a URL or libpq key/value pairs")]
    StoreUrl {
        /// The key the string was read from: the inline key or its `_file`
        /// sibling.
        key: String,
    },
    /// A key a section needs is not set.
    #[error("{key} is not set, and its section needs it")]
    Missing {
        /// The key that carries no value.
        key: String,
    },
    /// `[auth]` is refused: the key and why, never a value it holds.
    #[error("{key} {fault}")]
    Auth {
        /// The key that carries the fault.
        key: String,
        /// Why it is refused.
        fault: crate::config::auth::AuthFault,
    },
    /// A URL does not parse.
    #[error("{key} is not a URL")]
    Url {
        /// The key that holds it.
        key: String,
        /// What the URL parser reported.
        #[source]
        source: url::ParseError,
    },
    /// A URL carries a user name or a password in its userinfo, where the
    /// credentials belong in their own section.
    #[error("{key} carries a user name or password; set them in {section} instead")]
    UrlCredentials {
        /// The key that holds the URL.
        key: String,
        /// The section the credentials belong in.
        section: String,
    },
    /// A socket address does not parse.
    #[error("{key} is not a socket address")]
    Listen {
        /// The key that holds it.
        key: String,
        /// What the address parser reported.
        #[source]
        source: std::net::AddrParseError,
    },
    /// The base path is not one a request can be served under (§4.1, N28).
    #[error("{key} is not a base path")]
    BasePath {
        /// The key that holds it.
        key: String,
        /// Why the path is refused.
        #[source]
        source: crate::base_path::BasePathError,
    },
    /// A duration or a size that must be positive is zero.
    #[error("{key} is zero; it must be positive")]
    Zero {
        /// The key that holds it.
        key: String,
    },
    /// The log filter does not parse.
    #[error("telemetry.filter is not a valid tracing filter")]
    Filter {
        /// What the filter parser reported.
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
    /// A credentials section is keyed by something that is not an endpoint id.
    #[error("credentials.{key:?} is not an endpoint id")]
    EndpointId {
        /// The key that was given.
        key: String,
        /// What the registry's endpoint id rule reported.
        #[source]
        source: IdError,
    },
    /// `federation.demographic_endpoint` is not an endpoint id.
    #[error("federation.demographic_endpoint is not an endpoint id")]
    DemographicEndpoint {
        /// What the registry's endpoint id rule reported.
        #[source]
        source: IdError,
    },
    /// A credentials section names more than one scheme: a bearer token, a
    /// basic user, an OAuth 2.0 grant.
    #[error("{section} names more than one credentials scheme; set one")]
    Scheme {
        /// The credentials section.
        section: String,
    },
    /// A credentials section names no scheme at all.
    #[error("{section} names no credentials; remove the section or set one scheme")]
    NoScheme {
        /// The credentials section.
        section: String,
    },
    /// A credential does not form the `Authorization` value the node client
    /// sends, as a bearer token that is not the `b64token` of RFC 6750 §2.1
    /// does not.
    ///
    /// The source is the node client's refusal, which quotes nothing.
    #[error("{key} cannot be sent in the Authorization header")]
    Authorization {
        /// The key the credential was read from: the inline key or its
        /// `_file` sibling.
        key: String,
        /// What the node client reported.
        #[source]
        source: InvalidCredentials,
    },
    /// A basic user or password holds a character RFC 7617 §2 forbids.
    #[error("{key} cannot be sent: it holds {fault}, which RFC 7617 §2 forbids")]
    Basic {
        /// The key the value was read from: the inline key or its `_file`
        /// sibling.
        key: String,
        /// The kind of character found, never the character's position.
        fault: BasicFault,
        /// What the node client reported, which quotes nothing.
        #[source]
        source: InvalidCredentials,
    },
    /// A signing key cannot be used: it is no ES384 private key in PKCS#8
    /// PEM, or the previous key is the current one.
    #[error("{key} is not a usable signing key")]
    SigningKey {
        /// The key the file was named by.
        key: String,
        /// Why the key is refused; it quotes no part of the key.
        #[source]
        source: ferrofed_engine::onward::keys::KeyError,
    },
    /// `signing.assertion_lifetime_s` is zero or longer than the five minutes
    /// a client assertion may live.
    #[error(
        "signing.assertion_lifetime_s is {seconds}; a client assertion lives 1 to {max} seconds"
    )]
    AssertionLifetime {
        /// The lifetime given.
        seconds: u64,
        /// The longest lifetime allowed.
        max: u64,
    },
    /// The rotation overlap is shorter than an assertion's lifetime plus the
    /// time a node caches the JWK Set, so a node could meet an assertion signed
    /// by a key the set no longer publishes.
    #[error(
        "signing.rotation_overlap_s ({overlap_s}) must be at least signing.assertion_lifetime_s ({lifetime_s}) plus signing.node_jwks_cache_s ({cache_s})"
    )]
    RotationOverlap {
        /// The overlap window.
        overlap_s: u64,
        /// The assertion lifetime.
        lifetime_s: u64,
        /// How long a node caches the JWK Set.
        cache_s: u64,
    },
    /// A URL that must be absolute `http` or `https` is not.
    #[error("{key} must be an http or https URL with no user name or password")]
    HttpUrl {
        /// The key that holds it.
        key: String,
    },
    /// An OAuth 2.0 grant cannot be built from its section.
    #[error("{section} is not a usable OAuth 2.0 client-credentials grant")]
    Grant {
        /// The section.
        section: String,
        /// What the grant refused.
        #[source]
        source: ferrofed_engine::onward::GrantError,
    },
    /// A scope is not one the gateway may request onward.
    #[error("{key} is not a SMART on openEHR system scope")]
    Scope {
        /// The key that holds it.
        key: String,
        /// What the scope grammar refused.
        #[source]
        source: ferrofed_engine::onward::ScopeError,
    },
    /// An OAuth 2.0 grant is configured, but no signing key signs its client
    /// assertion.
    #[error("{section} needs [signing], whose key signs its client assertion (RFC 7523 §2.2)")]
    GrantWithoutSigning {
        /// The section.
        section: String,
    },
    /// A credentials section that takes a bearer token or basic credentials
    /// names an OAuth 2.0 grant.
    #[error("{section} takes a bearer token or basic credentials, not an oauth2 grant")]
    GrantNotHere {
        /// The section.
        section: String,
    },
    /// The `[dev]` table does not have the shape of `[[dev.crossref]]` rows.
    ///
    /// The reader's own message is not kept: it may quote a row's value, and
    /// a row's value is a patient identifier.
    #[error(
        "the [dev] table is not valid: every [[dev.crossref]] row names namespace, value, member and ehr_id, and nothing else"
    )]
    DevTable,
    /// `metrics.listen` names an address other than a loopback one, and
    /// `metrics.allow_remote` does not allow it.
    #[error(
        "metrics.listen is {address}, which is not a loopback address; the metrics listener has no authentication, so set metrics.allow_remote = true to serve it beyond this host"
    )]
    MetricsRemote {
        /// The address `metrics.listen` names.
        address: std::net::SocketAddr,
    },
    /// `metrics.listen` names the address `server.listen` binds.
    #[error("metrics.listen is {address}, the address server.listen binds; give it its own")]
    MetricsShared {
        /// The address both keys name.
        address: std::net::SocketAddr,
    },
    /// `metrics.otlp_endpoint` is not an `http://` URL.
    #[error(
        "metrics.otlp_endpoint must be an http:// URL: the OTLP push speaks gRPC without TLS, to a collector beside the gateway"
    )]
    OtlpScheme,
    /// The request timeout does not exceed the overall fan-out budget plus
    /// the combining margin, so it could cut the answer and the `504`
    /// envelope the budget produces when it expires (§11.4, §11.5).
    #[error(
        "server.request_timeout_ms ({request_ms}) must exceed federation.overall_timeout_ms ({overall_ms}) plus {margin_ms} ms for combining the answers (§11.5)"
    )]
    Budget {
        /// The overall fan-out budget.
        overall_ms: u64,
        /// The combining margin the request timeout must leave past it.
        margin_ms: u64,
        /// The request timeout.
        request_ms: u64,
    },
    /// Audit messages are turned off outside a configuration marked for
    /// development.
    #[error(
        "{key} = \"off\" records no ITI-55 audit message, which only profile = \"development\" accepts; set it to \"log\" (ITI TF-2 §3.55.5.1)"
    )]
    AuditOff {
        /// The key that turned the audit off.
        key: String,
    },
    /// A URL that carries a patient identifier or a credential is not
    /// `https`, outside a configuration marked for development.
    #[error(transparent)]
    Cleartext(#[from] CleartextError),
    /// A URL the gateway verifies its callers against is plain `http` to a
    /// host that is not loopback.
    #[error(transparent)]
    TrustAnchor(#[from] TrustAnchorError),
    /// Both a registry document and a directory are configured, and the
    /// registry has one source.
    #[error("set registry.document or [registry.mcsd], not both: the registry has one source")]
    TwoRegistrySources,
    /// The localizer's budget does not end before the overall budget, so the
    /// localizer could leave no time to resolve and ask the members (§11.5,
    /// §14.1).
    #[error(
        "federation.localization.timeout_ms ({timeout_ms}) must be below federation.overall_timeout_ms ({overall_ms}), of which it is a part (§11.5)"
    )]
    LocalizationBudget {
        /// The localizer's budget.
        timeout_ms: u64,
        /// The overall fan-out budget.
        overall_ms: u64,
    },
}

impl Error {
    /// Builds an [`Error::Parse`] from what the TOML reader reported about
    /// `text`, keeping positions and key names and dropping the quoted source.
    pub(crate) fn parse(error: &toml::de::Error, text: &str, stage: Stage) -> Self {
        Self::Parse {
            fault: ParseFault::from_toml(error, text, stage),
        }
    }

    /// Names `path` as the file a parse fault sits in; any other error is
    /// returned unchanged.
    #[must_use]
    pub(crate) fn in_file(self, path: &Path) -> Self {
        match self {
            Self::Parse { mut fault } => {
                fault.file = Some(path.to_path_buf());
                Self::Parse { fault }
            }
            other => other,
        }
    }
}

/// What RFC 7617 §2 forbids in a basic user-id or password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BasicFault {
    /// A control character (`CTL` of RFC 5234 Appendix B.1), forbidden in
    /// both the user-id and the password.
    ControlCharacter,
    /// A colon, forbidden in the user-id.
    Colon,
}

impl fmt::Display for BasicFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlCharacter => f.write_str("a control character"),
            Self::Colon => f.write_str("a colon"),
        }
    }
}

/// Which text a [`ParseFault`] was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The configuration file as written: the position points into it.
    File,
    /// The tree after the environment overrides, which has no lines of its
    /// own: the fault is located by its key alone.
    Merged,
}

/// What kind of fault the TOML reader met, without any text it quoted.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Problem {
    /// The text is not TOML.
    Syntax,
    /// A key the configuration does not define.
    UnknownKey,
    /// A key its section needs is absent.
    MissingKey {
        /// The key, as the configuration's own schema names it.
        key: String,
    },
    /// A key is set twice.
    DuplicateKey,
    /// A value of the wrong type, or outside the values the key admits.
    InvalidValue {
        /// What the configuration expects there, in its own schema's words.
        expected: String,
    },
    /// Any other refusal of the reader, whose message is not kept.
    Other,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax => f.write_str("the text is not TOML"),
            Self::UnknownKey => f.write_str("an unknown key"),
            Self::MissingKey { key } => write!(f, "the key `{key}` is missing"),
            Self::DuplicateKey => f.write_str("a key set twice"),
            Self::InvalidValue { expected } => {
                write!(f, "a value it does not admit, expected {expected}")
            }
            Self::Other => f.write_str("a value the reader refused"),
        }
    }
}

/// Where a configuration parse fault sits and what kind it is.
///
/// It holds a position, key names and the schema's own words only, never a
/// value from the configuration, so it is safe in a log, an error chain and
/// `Debug` (§5.4.1, N33).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFault {
    /// The file the fault is in, when the configuration came from one.
    pub file: Option<PathBuf>,
    /// The text the fault was found in.
    pub stage: Stage,
    /// The 1-based line and column in the file, for [`Stage::File`].
    pub position: Option<(usize, usize)>,
    /// The dotted key path of the offending line, or `dev` alone for any line
    /// of the `[dev]` table, whose rows are not described.
    pub key: Option<String>,
    /// What kind of fault it is.
    pub problem: Problem,
}

impl ParseFault {
    fn from_toml(error: &toml::de::Error, text: &str, stage: Stage) -> Self {
        let offset = error.span().map(|span| span.start);
        Self {
            file: None,
            stage,
            position: match stage {
                Stage::File => offset.and_then(|at| line_column(text, at)),
                Stage::Merged => None,
            },
            key: offset.and_then(|at| key_path(text, at)),
            problem: classify(error.message(), stage),
        }
    }
}

impl fmt::Display for ParseFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the configuration")?;
        if let Some(file) = &self.file {
            write!(f, " file {}", file.display())?;
        }
        f.write_str(" is not valid")?;
        if self.stage == Stage::Merged {
            f.write_str(" after the environment overrides")?;
        }
        write!(f, ": {}", self.problem)?;
        if let Some(key) = &self.key {
            write!(f, " at {key}")?;
        }
        if let Some((line, column)) = self.position {
            write!(f, " (line {line}, column {column})")?;
        }
        Ok(())
    }
}

/// Sorts the reader's message into a [`Problem`], keeping only the key a
/// missing-field refusal names and the schema text after `expected`.
fn classify(message: &str, stage: Stage) -> Problem {
    let first = message.lines().next().unwrap_or_default();
    if let Some(rest) = first.strip_prefix("missing field `") {
        return match rest.split_once('`') {
            Some((key, _)) if is_schema_key(key) => Problem::MissingKey {
                key: key.to_owned(),
            },
            _ => Problem::Other,
        };
    }
    if first.starts_with("unknown field") {
        return Problem::UnknownKey;
    }
    if first.starts_with("duplicate key") || first.starts_with("duplicate field") {
        return Problem::DuplicateKey;
    }
    let refused_value = [
        "invalid type:",
        "invalid value:",
        "invalid length",
        "unknown variant",
    ]
    .iter()
    .any(|prefix| first.starts_with(prefix));
    if refused_value {
        // NOTE: serde writes the schema's expectation last, after any quoted
        // value, so the text after the last `, expected ` is the schema's.
        return first
            .rsplit_once(", expected ")
            .map_or(Problem::Other, |(_, expected)| Problem::InvalidValue {
                expected: expected.to_owned(),
            });
    }
    match stage {
        Stage::File => Problem::Syntax,
        Stage::Merged => Problem::Other,
    }
}

/// Whether `key` reads as a key the configuration's schema defines.
fn is_schema_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The 1-based line and column of byte `offset` in `text`.
fn line_column(text: &str, offset: usize) -> Option<(usize, usize)> {
    let before = text.get(..offset)?;
    let line = before.matches('\n').count().saturating_add(1);
    let column = before
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count()
        .saturating_add(1);
    Some((line, column))
}

/// The dotted key path of the line holding byte `offset`: its table header
/// and the key before `=`, or `dev` alone for any line of the `[dev]` table.
fn key_path(text: &str, offset: usize) -> Option<String> {
    let before = text.get(..offset)?;
    let start = before.rfind('\n').map_or(0, |at| at.saturating_add(1));
    let line = text
        .get(start..)?
        .split('\n')
        .next()
        .unwrap_or_default()
        .trim();
    let path = if line.starts_with('[') {
        header_name(line)
    } else {
        let table = text
            .get(..start)?
            .lines()
            .rev()
            .map(str::trim)
            .find(|candidate| candidate.starts_with('['))
            .map(header_name);
        let key = line
            .split_once('=')
            .map(|(key, _)| key.trim().to_owned())
            .filter(|key| !key.is_empty());
        match (table, key) {
            (Some(table), Some(key)) => format!("{table}.{key}"),
            (Some(table), None) => table,
            (None, Some(key)) => key,
            (None, None) => return None,
        }
    };
    let root = path
        .split('.')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches('"');
    if root == "dev" {
        return Some(String::from("dev"));
    }
    Some(path)
}

/// The name a `[table]` or `[[array.of.tables]]` header line declares.
fn header_name(line: &str) -> String {
    line.trim_start_matches('[')
        .split(']')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

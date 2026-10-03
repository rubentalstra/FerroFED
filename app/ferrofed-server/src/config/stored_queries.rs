// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The `[stored_queries]` table: where the federated stored-query registry
//! keeps its definitions (§12.7, N44).
//!
//! Setting the table offers the registry, which `OPTIONS {base}/` declares
//! (§7a.2), over one of three backends:
//!
//! - `redb`, the default: the embedded store file `path` names, which one
//!   gateway process opens at a time;
//! - `postgres`: one PostgreSQL database every replica shares, reached at the
//!   connection string `url` or `url_file` holds, which is a secret and never
//!   logged; only a build with the `postgres` feature offers it;
//! - `files`: read-only, one file per definition under the directory `path`
//!   names, loaded at start, so a `PUT` is refused.
//!
//! No specification governs the store or its configuration: our own design.

use std::fmt;
use std::path::PathBuf;

use ferrofed_registry::secret::SecretUrl;
use serde::Deserialize;

use crate::config::Config;
use crate::config::error::Error;
use crate::config::secrets::secret;

/// The backend of the stored-query registry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// The embedded `redb` store file, for one gateway process.
    #[default]
    Redb,
    /// A PostgreSQL database several gateway replicas share.
    Postgres,
    /// Read-only definition files loaded at start.
    Files,
}

impl Backend {
    /// The backend as `stored_queries.backend` spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Redb => "redb",
            Self::Postgres => "postgres",
            Self::Files => "files",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The federated stored-query registry, as written.
///
/// `Debug` shows the connection string as [`SecretUrl`] redacts it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoredQueries {
    /// The backend; `redb` when the table sets none.
    pub backend: Option<Backend>,
    /// The `redb` store file, created when it does not exist, or the
    /// directory the `files` backend reads.
    pub path: Option<PathBuf>,
    /// The PostgreSQL connection string of the `postgres` backend, a URL or
    /// libpq key/value pairs; a secret.
    pub url: Option<SecretUrl>,
    /// A file holding [`StoredQueries::url`], read at boot.
    pub url_file: Option<PathBuf>,
}

/// The store the stored-query registry is opened over, resolved.
///
/// `Debug` redacts the connection string, because [`SecretUrl`] does.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Store {
    /// The `redb` store file.
    Redb(PathBuf),
    /// The PostgreSQL connection string.
    Postgres(SecretUrl),
    /// The directory of read-only definition files.
    Files(PathBuf),
}

impl Store {
    /// The backend this store is opened with.
    #[must_use]
    pub fn backend(&self) -> Backend {
        match self {
            Self::Redb(_) => Backend::Redb,
            Self::Postgres(_) => Backend::Postgres,
            Self::Files(_) => Backend::Files,
        }
    }

    /// Whether `other` opens the same store, so a reload that reads it
    /// needs no restart.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Redb(a), Self::Redb(b)) | (Self::Files(a), Self::Files(b)) => a == b,
            (Self::Postgres(a), Self::Postgres(b)) => a == b,
            _ => false,
        }
    }
}

/// Resolves `[stored_queries]` of `config`: the store, when the registry is
/// offered.
///
/// # Errors
///
/// [`Error::Missing`] naming `stored_queries.path` or `stored_queries.url`
/// when the backend needs it and it is unset or empty, and naming
/// `registry.document` when the registry is offered with neither a registry
/// document nor a directory, and so without the federation that executes
/// its queries; [`Error::StoreKey`] for a key the backend does
/// not read; [`Error::StoreBackendUnavailable`] for `postgres` in a build
/// without it; [`Error::StoreUrl`] for a connection string that does not
/// parse; and the secret errors of a `url_file`.
pub fn resolve(config: &Config) -> Result<Option<Store>, Error> {
    let section = &config.stored_queries;
    let offered = section.backend.is_some()
        || section.path.is_some()
        || section.url.is_some()
        || section.url_file.is_some();
    if !offered {
        return Ok(None);
    }
    let backend = section.backend.unwrap_or_default();
    let store = match backend {
        Backend::Redb => Store::Redb(path(section, backend)?),
        Backend::Files => Store::Files(path(section, backend)?),
        Backend::Postgres => Store::Postgres(url(section)?),
    };
    if !config.registry.configured() {
        return Err(Error::Missing {
            key: String::from("registry.document"),
        });
    }
    Ok(Some(store))
}

/// The `path` a path backend reads, refusing a connection string beside it.
fn path(section: &StoredQueries, backend: Backend) -> Result<PathBuf, Error> {
    for (key, set) in [
        ("stored_queries.url", section.url.is_some()),
        ("stored_queries.url_file", section.url_file.is_some()),
    ] {
        if set {
            return Err(Error::StoreKey {
                key: String::from(key),
                backend,
            });
        }
    }
    match &section.path {
        Some(path) if !path.as_os_str().is_empty() => Ok(path.clone()),
        Some(_) | None => Err(Error::Missing {
            key: String::from("stored_queries.path"),
        }),
    }
}

/// The connection string of the `postgres` backend, checked to parse.
fn url(section: &StoredQueries) -> Result<SecretUrl, Error> {
    if section.path.is_some() {
        return Err(Error::StoreKey {
            key: String::from("stored_queries.path"),
            backend: Backend::Postgres,
        });
    }
    if !cfg!(feature = "postgres") {
        return Err(Error::StoreBackendUnavailable {
            backend: Backend::Postgres,
        });
    }
    let key = "stored_queries.url";
    let url = secret(key, section.url.as_ref(), section.url_file.as_deref())?
        .filter(|url: &SecretUrl| !url.expose().trim().is_empty())
        .ok_or_else(|| Error::Missing {
            key: String::from(key),
        })?;
    #[cfg(feature = "postgres")]
    if !crate::stored::postgres::parses(&url) {
        let key = if section.url_file.is_some() {
            "stored_queries.url_file"
        } else {
            key
        };
        return Err(Error::StoreUrl {
            key: String::from(key),
        });
    }
    Ok(url)
}

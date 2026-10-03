// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Test support for the FerroFED suites, consumed as a path-only
//! dev-dependency so `cargo package` strips it.
//!
//! It reads the pin matrix, so a crate's version constant can be asserted
//! against the single source of truth, and it holds the harness of the
//! conformance tracks (§16):
//!
//! - [`containers`]: the two CDR products behind the `FERROFED_E2E` gate,
//!   pinned by digest;
//! - [`proxy`]: the capturing and fault proxy in front of each node, whose
//!   journal the tests read;
//! - [`leak`]: the track 10 oracle over that journal, which searches every
//!   carrier for an identifier and its fragments, raw and percent-decoded;
//! - [`issuer`]: a test issuer that mints RFC 9068 access tokens and serves
//!   its key set (#80);
//! - [`mcsd`]: the harness care services directory, a test device that
//!   answers ITI-90 and ITI-91 over the registry members a test puts in it
//!   (#86);
//! - [`mock`]: the wiremock server every suite stands its nodes up with,
//!   dropped outside the test's runtime;
//! - [`oauth`]: the harness OAuth 2.0 token endpoint, which verifies the
//!   gateway's client assertion against its published JWK Set and issues
//!   the token a mock node then requires (#81);
//! - [`pix`]: the harness PIX Manager, a test device that answers ITI-83 from
//!   what an ITI-104 feed delivered (#47);
//! - [`unreachable`](mod@unreachable): a base URL no connection can
//!   reach, the unreachable node of a test;
//! - [`seed`]: the synthetic seed builder, which writes over ITS-REST alone,
//!   feeds the PIX Manager over ITI-104, and names patients only inside the
//!   `urn:oid:2.999` example arc;
//! - [`xcpd`]: a stub XCPD Responding Gateway answering ITI-55 (#85).
//!
//! The consent pre-filter fake arrives with the issue that first needs it.
#![doc(test(attr(deny(warnings))))]

pub mod containers;
pub mod issuer;
pub mod leak;
pub mod mcsd;
pub mod mock;
pub mod oauth;
pub mod pix;
pub mod proxy;
pub mod seed;
pub mod unreachable;
pub mod xcpd;

use std::fmt;
use std::path::PathBuf;

/// The pin matrix at `docs/VERSIONS.md`, relative to this crate's manifest.
const MATRIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/VERSIONS.md");

/// A pin could not be read from the matrix.
#[derive(Debug)]
pub enum PinError {
    /// The matrix file could not be read.
    Read {
        /// The path that was tried.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// No table row has the requested item in its first cell.
    Missing {
        /// The item that was looked up.
        item: String,
    },
}

impl fmt::Display for PinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, .. } => {
                write!(f, "cannot read the pin matrix at {}", path.display())
            }
            Self::Missing { item } => write!(f, "the pin matrix has no row for {item}"),
        }
    }
}

impl std::error::Error for PinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Missing { .. } => None,
        }
    }
}

/// Returns the first token of the `Pin` cell of the matrix row whose first
/// cell is `item`, with backticks removed.
///
/// This mirrors what `scripts/checks/versions.sh` reads, so a crate constant
/// and the guard agree on the same value.
///
/// # Errors
///
/// Returns [`PinError::Read`] when `docs/VERSIONS.md` cannot be read and
/// [`PinError::Missing`] when no row carries `item`.
pub fn matrix_pin(item: &str) -> Result<String, PinError> {
    let path = PathBuf::from(MATRIX);
    let text = std::fs::read_to_string(&path).map_err(|source| PinError::Read {
        path: path.clone(),
        source,
    })?;
    text.lines()
        .find_map(|line| {
            let mut cells = line
                .split('|')
                .skip(1)
                .map(|c| c.replace('`', "").trim().to_owned());
            let key = cells.next()?;
            let pin = cells.next()?;
            (key == item).then(|| pin.split_whitespace().next().unwrap_or_default().to_owned())
        })
        .ok_or_else(|| PinError::Missing {
            item: item.to_owned(),
        })
}

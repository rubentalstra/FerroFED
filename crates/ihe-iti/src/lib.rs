// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The IHE IT Infrastructure (ITI) profiles a federation gateway or a patient
//! index binds, one feature per profile:
//!
//! - `pixm`: Patient Identifier Cross-reference for Mobile, ITI-83.
//! - `pdqm`: Patient Demographics Query for Mobile, ITI-78.
//! - `mcsd`: Mobile Care Services Discovery, ITI-90 and ITI-91.
//! - `pmir`: Patient Master Identity Registry, ITI-93 and ITI-94.
//! - `xcpd`: Cross-Community Patient Discovery, ITI-55, the one profile on
//!   SOAP 1.2 with HL7 v3 and SAML XUA.
//!
//! The profiles are published at <https://profiles.ihe.net/ITI/>. The crate
//! depends on no application: it is the profiles' transactions as Rust, for
//! any caller. The profile modules land with their FerroFED issues (Annex A).
#![doc(test(attr(deny(warnings))))]

#[cfg(feature = "mcsd")]
pub mod mcsd;
#[cfg(any(feature = "pixm", feature = "pdqm", feature = "mcsd"))]
pub mod outcome;
#[cfg(feature = "pdqm")]
pub mod pdqm;
#[cfg(feature = "pixm")]
pub mod pixm;
#[cfg(feature = "pmir")]
pub mod pmir;
#[cfg(any(feature = "pixm", feature = "pdqm", feature = "mcsd", feature = "xcpd"))]
mod redact;
#[cfg(any(feature = "pixm", feature = "pdqm", feature = "mcsd"))]
mod search;
#[cfg(feature = "xcpd")]
pub mod xcpd;

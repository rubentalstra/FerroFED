// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The identity roles of the Federation Tier as traits (resolver, localizer,
//! directory, consent pre-filter, onward authentication) and the patient
//! reference carrier, with no FHIR and no transport.
//!
//! The gateway core depends on these traits only; each binding implements
//! one in a crate of its own, so it can move without a change to the core
//! (§2.4, N27, N27a). This crate holds:
//!
//! - [`patient`]: [`PatientRef`](patient::PatientRef), the patient
//!   identifier as the gateway carries it, redacted everywhere (§5.4, N33);
//! - [`resolver`]: the [`Resolver`](resolver::Resolver) seam (N3, §5.2);
//! - [`localizer`]: the [`Localizer`](localizer::Localizer) seam, which
//!   members might hold a patient's data (N4, §14.1);
//! - [`consent`]: the optional Step-1
//!   [`ConsentPrefilter`](consent::ConsentPrefilter) seam (N27a, §13.2.1);
//! - [`pixm`]: the [`Resolver`](resolver::Resolver) over PIXm ITI-83 (#43);
//! - [`xcpd`]: the [`Localizer`](localizer::Localizer) over XCPD ITI-55
//!   (Annex A.3);
//! - [`binding`]: the resolution bindings of §12.5.1 step 2, in memory and
//!   scoped to the client session (§12.5.1 step 2);
//! - [`dev`]: the static development cross-reference and consent pre-filter,
//!   FerroFED's own testing devices, enabled only under the development
//!   profile;
//! - [`directory`]: the registry document in FHIR form, `Organization` and
//!   `Endpoint` resources read through `ihe_iti`'s mCSD reader (N19, N20),
//!   and the registry read from an mCSD directory and kept in step with it
//!   ([`directory::mcsd`], §15.1, N21).
//!
//! The onward-authentication seam lands with its issue.
#![doc(test(attr(deny(warnings))))]

pub mod binding;
pub mod consent;
pub mod dev;
pub mod directory;
pub mod localizer;
pub mod patient;
pub mod pixm;
pub mod resolver;
pub mod xcpd;

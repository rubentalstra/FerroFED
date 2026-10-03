// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! Integration tests: the patient reference stays redacted, the static
//! development cross-reference resolves only under the development profile,
//! the PIXm resolver reads each member's `ehr_id` from its domain and fails
//! closed, and the resolution bindings stay in their session (§5.2, §5.4,
//! §12.5.1, N3, N6, N33).
#![allow(
    clippy::panic_in_result_fn,
    reason = "test assertions in tests that return their setup errors"
)]

mod binding;
#[cfg(test)]
mod directory;
mod localizer;
mod patient;
mod pixm;
mod pixm_localizer;
mod static_consent;
mod static_resolver;
mod support;
mod xcpd;

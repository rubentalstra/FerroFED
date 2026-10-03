// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! What one read of a directory may cost: a deadline for the whole walk over
//! every page, and caps on the pages, the bytes and the entries it reads.
//!
//! A directory answer runs to as many pages as its `next` links say, so a
//! faulty or hostile directory could hold a reader for ever or fill its
//! memory. Every ITI-90 and ITI-91 walk of [`McsdClient`] draws on one
//! [`Budget`], and running out of any part of it is an error, never a shorter
//! answer: a partial directory is never directory content. No specification
//! governs these limits: our own design.
//!
//! [`McsdClient`]: super::client::McsdClient

use std::time::{Duration, Instant};

use super::error::McsdError;

/// The caps of a [`Budget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most pages one budget reads, over every walk it is used for.
    pub pages: usize,
    /// The most bytes of answer bodies one budget reads.
    pub bytes: usize,
    /// The most Bundle entries one budget reads.
    pub entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pages: 200,
            bytes: 64 << 20,
            entries: 50_000,
        }
    }
}

/// What is left of a read's deadline, pages, bytes and entries.
#[derive(Debug, Clone)]
pub struct Budget {
    deadline: Instant,
    limits: Limits,
    pages: usize,
    bytes: usize,
    entries: usize,
}

impl Budget {
    /// A budget that ends at `deadline`, with the caps of `limits`.
    #[must_use]
    pub fn new(deadline: Instant, limits: Limits) -> Self {
        Self {
            deadline,
            limits,
            pages: 0,
            bytes: 0,
            entries: 0,
        }
    }

    /// The time left before the deadline.
    ///
    /// # Errors
    /// [`McsdError::Timeout`] when the deadline has passed.
    pub(crate) fn remaining(&self) -> Result<Duration, McsdError> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(McsdError::Timeout)
        } else {
            Ok(left)
        }
    }

    /// Takes one page.
    ///
    /// # Errors
    /// [`McsdError::TooManyPages`] when the cap is reached.
    pub(crate) fn page(&mut self) -> Result<(), McsdError> {
        if self.pages >= self.limits.pages {
            return Err(McsdError::TooManyPages {
                limit: self.limits.pages,
            });
        }
        self.pages = self.pages.saturating_add(1);
        Ok(())
    }

    /// Takes `bytes` of an answer body.
    ///
    /// # Errors
    /// [`McsdError::TooLarge`] when the cap would be passed.
    pub(crate) fn bytes(&mut self, bytes: usize) -> Result<(), McsdError> {
        let total = self.bytes.saturating_add(bytes);
        if total > self.limits.bytes {
            return Err(McsdError::TooLarge {
                limit: self.limits.bytes,
            });
        }
        self.bytes = total;
        Ok(())
    }

    /// Takes `entries` Bundle entries.
    ///
    /// # Errors
    /// [`McsdError::TooManyEntries`] when the cap would be passed.
    pub(crate) fn entries(&mut self, entries: usize) -> Result<(), McsdError> {
        let total = self.entries.saturating_add(entries);
        if total > self.limits.entries {
            return Err(McsdError::TooManyEntries {
                limit: self.limits.entries,
            });
        }
        self.entries = total;
        Ok(())
    }
}

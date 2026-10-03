// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The withheld identifiers replaced in text the gateway writes but did not
//! compose, such as a node's error message, by the rule the outbound gate
//! holds a request to (§5.4.1, N33).

use openehr_query::printer::escape_string;
use secrecy::ExposeSecret;

use super::Withheld;
use super::decode::percent_decoded;

/// The text that stands in for a withheld identifier ([`Withheld::masked`]).
pub const MASK: &str = "[withheld]";

impl Withheld {
    /// `text` with every withheld identifier replaced by [`MASK`], raw or as
    /// an AQL string literal, or `None` when one survives the replacement in
    /// a form the gate reads (raw, or percent-decoded).
    ///
    /// The forms are those [`Withheld::found_in`] reads. The longest
    /// identifier is replaced first, so one that contains another is never
    /// cut in two.
    #[must_use]
    pub fn masked(&self, text: &str) -> Option<String> {
        let mut values: Vec<&str> = self.0.iter().map(ExposeSecret::expose_secret).collect();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        let mut out = text.to_owned();
        for value in &values {
            out = out.replace(escape_string(value).as_str(), MASK);
            out = out.replace(value, MASK);
        }
        let survives = values
            .iter()
            .any(|value| out.contains(value) || percent_decoded(&out).contains(value));
        (!survives).then_some(out)
    }
}

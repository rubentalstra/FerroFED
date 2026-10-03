// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The percent-decoded form of a text, the second form the gate reads every
//! part of a request in (RFC 3986 §2.1).

/// `text` with every `%XX` escape decoded, invalid UTF-8 replaced; an escape
/// that is not two hex digits is kept as written.
pub(crate) fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let escape = (byte == b'%')
            .then(|| {
                let high = hex(*bytes.get(index.saturating_add(1))?)?;
                let low = hex(*bytes.get(index.saturating_add(2))?)?;
                Some((high << 4) | low)
            })
            .flatten();
        if let Some(decoded) = escape {
            out.push(decoded);
            index = index.saturating_add(3);
        } else {
            out.push(byte);
            index = index.saturating_add(1);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::percent_decoded;

    #[test]
    fn percent_decoding_keeps_a_broken_escape() {
        assert_eq!("a'b%zz%4", percent_decoded("a%27b%zz%4"));
    }
}

//! Lowercase hex encoding for digests.
//!
//! Its own module rather than a helper on either caller: `bootstrap` hashes
//! downloads and `cache` hashes source identity, and neither has any business
//! reaching into the other. It stays in `app` rather than moving to `core`,
//! whose stated remit is the editing model — a hex encoder is not part of that
//! vocabulary, and deduplicating into the wrong crate is worse than leaving a
//! copy behind.

/// Lowercase, zero-padded, two characters per byte.
pub fn encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Zero bytes must render as two digits, not one: an encoder that dropped
    // leading zeroes would make different digests collide.
    #[test]
    fn is_lowercase_and_padded() {
        assert_eq!(encode(&[0x0f, 0xa0, 0x00]), "0fa000");
        assert_eq!(encode(&[0xff, 0x10]), "ff10");
        assert_eq!(encode(&[]), "");
    }
}

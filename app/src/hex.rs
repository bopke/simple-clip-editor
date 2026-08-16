//! Lowercase hex encoding for digests.
//!
//! Shared by `bootstrap` (download checksums) and `cache` (source identity),
//! so it belongs to neither.

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

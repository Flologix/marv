//! Small text helpers shared by the MCP modules: SHA-256 as hex, and re-exports of the shared
//! `printable()` from `marv-core`. Not a port of one TS file.

use sha2::{Digest, Sha256};

/// SHA-256 of `bytes` as lowercase hex (what `createHash("sha256")...digest("hex")` gave in Marv).
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub(crate) use marv_core::printable::{is_bidi, printable};

/// The first `max` chars of `text`, with `…` when it was longer (char-safe, unlike byte slicing).
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_strips_escapes_and_bidi() {
        assert_eq!(printable("a\u{1b}[31mb\u{202e}c\n"), "abc\n");
    }

    #[test]
    fn sha256_matches_node() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}

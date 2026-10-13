//! The plain-text fallback for a page too complex to convert in time, and the limits both conversions share.
//!
//! Ports Marv's `src/tools/web/plain.ts`.
//!
//! This runs in Marv's own process (after the converter was killed), on HTML that just proved hostile or
//! pathological, and an attacker can make a page slow on purpose to land here, so:
//! - It's one linear pass. Tags are read by hand, character by character, and searches only move forward; never a
//!   pattern that could backtrack over the rest of the page.
//! - It hides what the Markdown path hides: hidden elements, form controls, scripts. Quoted attribute values are
//!   skipped as a browser does, so `<div title="x>" hidden>` is still one hidden tag.

use crate::web::html::Markdown;

/// About ten of web_fetch's 30k-character pages: more is rarely read.
pub const MAX_CHARS: usize = 300_000;
pub const MAX_TITLE: usize = 200;

/// One line (a newline in the title could forge a line of web_fetch's `Fetched <url> · <title>` header), at most
/// 200 characters, cut by code point.
pub fn one_line(text: Option<&str>) -> Option<String> {
    let text: String = text.unwrap_or("").chars().take(MAX_TITLE * 10).collect();
    let mut line = String::new();
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() {
            if !in_space {
                line.push(' ');
            }
            in_space = true;
        } else {
            line.push(c);
            in_space = false;
        }
    }
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line.chars().count() > MAX_TITLE {
        let cut: String = line.chars().take(MAX_TITLE - 1).collect();
        return Some(format!("{}…", cut.trim_end()));
    }
    Some(line.to_string())
}

/// Whether an inline style hides the element (`display:none`, `visibility:hidden|collapse`, `opacity:0`,
/// `font-size:0`), CSS comments removed first by hand.
pub fn is_hidden_style(_style: &str) -> bool {
    todo!("port: isHiddenStyle (comment stripping by hand, then the HIDDEN_STYLE pattern)")
}

/// The page's text: tags, comments, scripts, styles and hidden elements removed, entities decoded, whitespace
/// collapsed, at most `max` characters. Linear in the size of the page.
pub fn plain_text(_html: &str, _max: usize) -> Markdown {
    todo!("port: plainText (the quote-aware tag scanner)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_one_short_line() {
        assert_eq!(one_line(Some("  Hello\n\tworld  ")), Some("Hello world".into()));
        assert_eq!(one_line(Some("   ")), None);
        let long = "x".repeat(500);
        assert_eq!(one_line(Some(&long)).unwrap().chars().count(), MAX_TITLE);
    }
}

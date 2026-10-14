//! Text made safe for the terminal. Ports Marv's `src/printable.ts`, plus bidi stripping (new, see
//! `docs/SECURITY.md`).
//!
//! Text written by a model (or read from a file, a web page, an MCP server, the mesh) is shown in
//! the terminal. A raw escape sequence in it would be executed by the terminal, not shown: it could
//! write the clipboard (OSC 52), retitle the window or redraw the screen. Everything that isn't
//! printable is dropped at render time (the model's own conversation is untouched). This applies to
//! headless text output too: Marv 0.3.0 wrote `marv -p` replies raw, which this fixes.
//!
//! - ESC-introduced sequences: CSI (ends at its final byte, or the end of the text so a half-streamed
//!   one doesn't flicker); strings (OSC/DCS/SOS/PM/APC, until BEL, ST or the end); the short forms
//!   (`ESC ( B`, `ESC 7`, `ESC =`…); and the 8-bit CSI `\x9b`.
//! - The other 8-bit introducers (`\x9d` etc.) are deliberately not swallowed with a payload: UTF-8
//!   text decoded as Latin-1 ("“" -> "â\x80\x9c") contains them, and eating up to a terminator would
//!   delete real text. Only the introducer goes (it's a control); the rest is harmless text.
//! - Any other C0/C1 control and DEL, except `\t` and `\n` (`\r` would overwrite the line).
//! - Bidi overrides and isolates, so a Trojan-Source string can't make an approval preview look
//!   like something else.

const ESC: char = '\u{1b}';
const BEL: char = '\u{7}';
const CSI8: char = '\u{9b}';

fn is_param(c: char) -> bool {
    ('0'..='?').contains(&c)
}
fn is_intermediate(c: char) -> bool {
    (' '..='/').contains(&c)
}
fn is_final(c: char) -> bool {
    ('@'..='~').contains(&c)
}

/// Bidi controls: ALM, LRM, RLM, the embeddings/overrides (U+202A–202E) and isolates (U+2066–2069).
pub fn is_bidi(c: char) -> bool {
    matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn is_dropped_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// `text` with escape sequences, controls (except tab and newline) and bidi controls removed.
pub fn printable(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ESC {
            if let Some(end) = escape_end(&chars, i + 1) {
                i = end;
                continue;
            }
        } else if c == CSI8 {
            let mut j = i + 1;
            while j < chars.len() && is_param(chars[j]) {
                j += 1;
            }
            while j < chars.len() && is_intermediate(chars[j]) {
                j += 1;
            }
            if j < chars.len() && is_final(chars[j]) {
                j += 1;
            }
            i = j;
            continue;
        }
        if !is_dropped_control(c) && !is_bidi(c) {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Where the sequence after an ESC (at `start`) ends, or `None` if it isn't one (the lone ESC is
/// then dropped as a control).
fn escape_end(chars: &[char], start: usize) -> Option<usize> {
    let next = *chars.get(start)?;
    match next {
        '[' => {
            let mut j = start + 1;
            while j < chars.len() && is_param(chars[j]) {
                j += 1;
            }
            while j < chars.len() && is_intermediate(chars[j]) {
                j += 1;
            }
            match chars.get(j) {
                None => Some(j),
                Some(&f) if is_final(f) => Some(j + 1),
                Some(_) => None,
            }
        }
        ']' | 'P' | 'X' | '^' | '_' => {
            let mut j = start + 1;
            while j < chars.len() {
                if chars[j] == BEL {
                    return Some(j + 1);
                }
                if chars[j] == ESC && chars.get(j + 1) == Some(&'\\') {
                    return Some(j + 2);
                }
                j += 1;
            }
            Some(j)
        }
        _ => {
            let mut j = start;
            while j < chars.len() && is_intermediate(chars[j]) {
                j += 1;
            }
            match chars.get(j) {
                Some(&f) if ('0'..='~').contains(&f) => Some(j + 1),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::printable;

    #[test]
    fn strips_csi_and_keeps_text() {
        assert_eq!(printable("a\x1b[31mred\x1b[0m b"), "ared b");
    }

    #[test]
    fn strips_osc52_clipboard_and_title() {
        assert_eq!(printable("x\x1b]52;c;ZXZpbA==\x07y"), "xy");
        assert_eq!(printable("x\x1b]0;title\x1b\\y"), "xy");
    }

    #[test]
    fn half_streamed_sequences_vanish() {
        assert_eq!(printable("ok\x1b[3"), "ok");
        assert_eq!(printable("ok\x1b]52;c;abc"), "ok");
    }

    #[test]
    fn short_forms_and_8bit_csi() {
        assert_eq!(printable("a\x1b(Bb\x1b7c"), "abc");
        assert_eq!(printable("a\u{9b}31mb"), "ab");
    }

    #[test]
    fn keeps_tab_newline_drops_cr_and_lone_esc() {
        assert_eq!(printable("a\tb\nc\rd\x1b"), "a\tb\ncd");
    }

    #[test]
    fn latin1_mojibake_keeps_its_text() {
        // "“" decoded as Latin-1: only the C1 control goes, the rest stays.
        assert_eq!(printable("â\u{80}\u{9c}quote"), "âquote");
    }

    #[test]
    fn strips_bidi_overrides() {
        assert_eq!(printable("rm -rf /\u{202e}gpj.txt"), "rm -rf /gpj.txt");
    }
}

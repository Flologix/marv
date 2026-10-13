//! All colours live here so the look can be changed in one place. Ports `src/ui/theme.ts` (fully).
//!
//! Exact shades, not the terminal's "gray" (ANSI bright black): terminals pick that shade
//! themselves, and GNOME Console's is ~2.5:1 against its background, hard to read. Contrast figures
//! below are on a dark background (#1e1e1e). The one named colour, `assistant: "white"`, stays a
//! named ANSI colour as in Marv (the terminal's own white).

use ratatui::style::Color;

const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// `#2dd4bf`: the accent (borders, the martian, markers).
pub const ACCENT: Color = hex(0x2dd4bf);
/// `#d4d4d8`: your messages: ~11:1, visible but distinct from replies.
pub const USER: Color = hex(0xd4d4d8);
/// `white`: the model's replies.
pub const ASSISTANT: Color = Color::White;
/// `#a1a1aa`: reasoning, hints, status: ~6.5:1, quieter but readable.
pub const DIM: Color = hex(0xa1a1aa);
/// `#f87171`
pub const ERROR: Color = hex(0xf87171);
/// `#fbbf24`
pub const WARNING: Color = hex(0xfbbf24);
/// `#7dd3fc`: inline code and links in replies.
pub const CODE: Color = hex(0x7dd3fc);
/// `#cbd5e1`: fenced code blocks.
pub const CODE_BLOCK: Color = hex(0xcbd5e1);
/// `#4ade80`: added lines in a change.
pub const DIFF_ADD: Color = hex(0x4ade80);
/// `#f87171`: removed lines.
pub const DIFF_DEL: Color = hex(0xf87171);
/// Top-to-bottom gradient for the splash logo (one colour per line): teal fading into a gold accent.
pub const GRADIENT: [Color; 6] = [hex(0x5eead4), hex(0x2dd4bf), hex(0x14b8a6), hex(0x22d3ee), hex(0x38bdf8), hex(0xfacc15)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colours_match_marv_theme() {
        assert_eq!(ACCENT, Color::Rgb(0x2d, 0xd4, 0xbf));
        assert_eq!(DIM, Color::Rgb(0xa1, 0xa1, 0xaa));
        assert_eq!(GRADIENT[5], Color::Rgb(0xfa, 0xcc, 0x15));
    }
}

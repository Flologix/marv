//! Mouse support. Ports `src/mouse.ts` (tests: `tests/mouse.test.ts`).
//!
//! Marv turns on the terminal's mouse reporting itself:
//! - 1002 = report presses, releases, and motion while a button is held (drags); the wheel counts
//!   as buttons 64/65.
//! - 1006 = SGR encoding: `ESC [ < button ; column ; row M` (press/motion) or `m` (release).
//!
//! Mouse mode is terminal-wide, so it's turned off on every exit path (`render_options`).
//!
//! What changes in Rust: Ink only understood keys, so Marv wrapped `stdin.read()` to cut mouse
//! codes out of each chunk before Ink saw them (and held a code split across reads, or it would be
//! lost or typed into the prompt). crossterm parses SGR mouse codes itself, including split ones,
//! so that wrapper (`filterMouseInput`, `extractMouse`) has no Rust counterpart; this module maps
//! crossterm's events onto Marv's smaller [`MouseEvent`] (wheel notches and the left button only;
//! middle/right are ignored, modifiers stripped). The SGR cases in `mouse.test.ts` become tests of
//! [`from_crossterm`].

/// Turns on 1002 + 1006 (written raw by `render_options::TerminalGuard`; see why there).
pub const MOUSE_ON: &str = "\x1b[?1002h\x1b[?1006h";
/// Turns them off again, in reverse order.
pub const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l";

/// How many transcript rows one wheel notch scrolls (Marv's `WHEEL_ROWS`, in ScrollView).
pub const WHEEL_ROWS: i32 = 3;

/// Cells are 0-based: `x` is the column, `y` the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    /// One wheel notch: -1 up, +1 down.
    Scroll {
        step: i8,
    },
    Press {
        x: u16,
        y: u16,
    },
    Drag {
        x: u16,
        y: u16,
    },
    Release {
        x: u16,
        y: u16,
    },
}

/// Marv's view of a crossterm mouse event: wheel notches and the left button; None for the rest
/// (middle/right buttons, plain motion, horizontal wheel).
pub fn from_crossterm(event: crossterm::event::MouseEvent) -> Option<MouseEvent> {
    let _ = event;
    todo!("port: decode() from mouse.ts over crossterm::event::MouseEventKind (left button only, wheel = Scroll)")
}

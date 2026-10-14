//! How Marv asks the terminal to draw. Ports `src/render-options.ts` (and the terminal set-up and
//! tear-down that `src/cli.tsx` and Ink did around it).
//!
//! Marv's Ink options, and what each became:
//! - `exitOnCtrlC: false`: ctrl+c is handled inside the app (interrupt / clear / confirm exit).
//!   Here: raw mode, so ctrl+c arrives as a key event.
//! - `alternateScreen: true`: draw on the terminal's separate full-screen buffer (like vim or
//!   htop); the shell's screen is restored untouched on exit. There is no terminal scrollback, so
//!   the transcript scrolls itself (`ui::scroll_view`).
//! - `incrementalRendering: true`: rewrite only the lines that changed (Marv's
//!   `.marv/checks/render-options.yaml` guards it: a full redraw flickers in terminals that ignore
//!   synchronized output). ratatui does better natively: it diffs the previous and next buffer and
//!   writes only the changed *cells*. We also wrap each frame in synchronized output (DEC 2026).
//! - `maxFps: 60`: [`MAX_FPS`]; the app never draws more often than [`FRAME_INTERVAL`].
//! - `transformOutput`: Marv's Ink patch through which the selection store saw each frame and drew
//!   its highlight. Here the app hands the drawn `Buffer` to `selection::SelectionStore` after the
//!   widgets render (no patch needed).
//!
//! [`TerminalGuard`] restores the terminal on drop *and* on panic: mouse reporting and raw mode are
//! terminal-wide, and a crash that left them on would leave the user's shell unusable (Marv turned
//! mouse mode off in a `process.on("exit")` handler for the same reason).

use std::io::{self, Write};
use std::sync::Once;
use std::time::Duration;

use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::{cursor, execute};

use crate::mouse::{MOUSE_OFF, MOUSE_ON};

/// Frames are cheap enough to draw at 60/s, which makes scrolling glide instead of step.
pub const MAX_FPS: u32 = 60;
/// The shortest gap between two draws.
pub const FRAME_INTERVAL: Duration = Duration::from_micros(1_000_000 / MAX_FPS as u64);

/// What to turn on when the UI starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub alternate_screen: bool,
    /// SGR mouse reporting (`mouse::MOUSE_ON`): wheel scrolling and selection.
    pub mouse: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { alternate_screen: true, mouse: true }
    }
}

/// Holds the terminal in raw mode (+ alternate screen, + mouse) until dropped.
#[derive(Debug)]
pub struct TerminalGuard {
    options: RenderOptions,
}

impl TerminalGuard {
    /// Enters raw mode, the alternate screen and mouse reporting as asked, and installs a panic
    /// hook that undoes them before the panic message prints.
    pub fn enter(options: RenderOptions) -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        let guard = Self { options };
        let mut out = io::stdout();
        if options.alternate_screen {
            execute!(out, EnterAlternateScreen)?;
        }
        if options.mouse {
            // Written raw, not crossterm's EnableMouseCapture: that also turns on 1003 (report
            // every motion, buttons up or not), which would wake the event loop on each mouse
            // move. Marv asks only for 1002 (presses, drags, wheel) + 1006 (SGR encoding).
            out.write_all(MOUSE_ON.as_bytes())?;
        }
        execute!(out, cursor::Hide)?;
        out.flush()?;
        Ok(guard)
    }

    pub fn options(&self) -> RenderOptions {
        self.options
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Best-effort: everything off, whatever was on. Safe to call twice.
pub fn restore() {
    let mut out = io::stdout();
    let _ = out.write_all(MOUSE_OFF.as_bytes());
    let _ = execute!(out, cursor::Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// Chains a hook that restores the terminal before the previous hook prints the panic, so the
/// message lands on the user's normal screen instead of vanishing with the alternate one.
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_interval_is_60_fps() {
        assert_eq!(FRAME_INTERVAL, Duration::from_micros(16_666));
        assert_eq!(RenderOptions::default(), RenderOptions { alternate_screen: true, mouse: true });
    }
}

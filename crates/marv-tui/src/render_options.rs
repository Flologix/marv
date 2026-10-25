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
//! [`TerminalGuard`] restores the terminal on drop *and* on panic: mouse reporting, bracketed paste
//! and raw mode are terminal-wide, and a crash that left them on would leave the user's shell
//! unusable (Marv turned mouse mode off in a `process.on("exit")` handler for the same reason).
//!
//! Bracketed paste (marv-rs #214; Ink turned it on for Marv): without it the terminal types a paste
//! as keystrokes, so a pasted newline is Enter and a multi-line paste sent its first line as a
//! message. With it, the paste arrives whole as one `Event::Paste`, which never submits. It is on
//! in every mode (the alternate screen or inline) and off in [`restore`]. The TUI has no suspend
//! (ctrl+z) or external-editor path that leaves and re-enters the terminal; one that does must call
//! [`restore`] on the way out and [`TerminalGuard::enter`] (or [`write_enter`]) on the way back.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::{cursor, execute};

use crate::mouse::{MOUSE_OFF, MOUSE_ON};

/// Turns bracketed paste on (`ESC [ ? 2004 h`): a paste arrives between `ESC [200~` and `ESC [201~`,
/// which crossterm reports as one `Event::Paste`.
pub const BRACKETED_PASTE_ON: &str = "\x1b[?2004h";
/// Turns it off again.
pub const BRACKETED_PASTE_OFF: &str = "\x1b[?2004l";

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
    /// Enters raw mode, the alternate screen, mouse reporting and bracketed paste as asked, and
    /// installs a panic hook that undoes them before the panic message prints.
    pub fn enter(options: RenderOptions) -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        let guard = Self { options };
        write_enter(&mut io::stdout(), options)?;
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

/// Writes what [`TerminalGuard::enter`] turns on (everything but raw mode, which isn't a sequence).
pub fn write_enter(out: &mut impl Write, options: RenderOptions) -> io::Result<()> {
    if options.alternate_screen {
        execute!(out, EnterAlternateScreen)?;
    }
    if options.mouse {
        // Written raw, not crossterm's EnableMouseCapture: that also turns on 1003 (report
        // every motion, buttons up or not), which would wake the event loop on each mouse
        // move. Marv asks only for 1002 (presses, drags, wheel) + 1006 (SGR encoding).
        out.write_all(MOUSE_ON.as_bytes())?;
    }
    // Always, inline too: a paste must never type its newlines as Enter (#214).
    out.write_all(BRACKETED_PASTE_ON.as_bytes())?;
    execute!(out, cursor::Hide)?;
    out.flush()
}

/// Writes what [`restore`] turns off: everything, whatever was on, so it's safe to call twice.
pub fn write_restore(out: &mut impl Write) -> io::Result<()> {
    out.write_all(BRACKETED_PASTE_OFF.as_bytes())?;
    out.write_all(MOUSE_OFF.as_bytes())?;
    execute!(out, cursor::Show, LeaveAlternateScreen)?;
    out.flush()
}

/// Best-effort: everything off, whatever was on. Safe to call twice. Also what the panic hook runs.
pub fn restore() {
    let _ = write_restore(&mut io::stdout());
    let _ = disable_raw_mode();
}

/// Runs `work` (terminal writes) on a thread of its own and waits at most `limit` for it: `true` if it finished.
/// For the way out after a hangup or SIGTERM (#196): a closed terminal fails writes with EIO, which [`restore`]
/// ignores, but a terminal that stopped reading (XOFF, a frozen emulator) would block them for good. A write still
/// blocked is left behind on its thread, which ends with the process.
/// Without a thread (none could be started), `work` runs here, unbounded.
pub fn within(limit: Duration, work: impl FnOnce() + Send + 'static) -> bool {
    let (done, finished) = std::sync::mpsc::channel();
    let work = Arc::new(Mutex::new(Some(work)));
    let on_thread = work.clone();
    let spawned = std::thread::Builder::new().name("marv-terminal".into()).spawn(move || {
        if let Some(work) = on_thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            work();
        }
        let _ = done.send(());
    });
    match spawned {
        Ok(_) => finished.recv_timeout(limit).is_ok(),
        Err(_) => {
            if let Some(work) = work.lock().unwrap_or_else(|e| e.into_inner()).take() {
                work();
            }
            true
        }
    }
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

    fn entered(options: RenderOptions) -> String {
        let mut out = Vec::new();
        write_enter(&mut out, options).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn entering_turns_bracketed_paste_on_in_every_mode() {
        let full = entered(RenderOptions::default());
        assert!(full.contains("\x1b[?1049h"), "alternate screen: {full:?}");
        assert!(full.contains(MOUSE_ON));
        assert!(full.contains(BRACKETED_PASTE_ON));
        // Inline (no alternate screen) and without the mouse: a paste still mustn't type Enter.
        let inline = entered(RenderOptions { alternate_screen: false, mouse: false });
        assert!(!inline.contains("\x1b[?1049h"));
        assert!(!inline.contains(MOUSE_ON));
        assert!(inline.contains(BRACKETED_PASTE_ON), "{inline:?}");
    }

    #[test]
    fn restoring_turns_bracketed_paste_and_the_mouse_off() {
        let mut out = Vec::new();
        write_restore(&mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains(BRACKETED_PASTE_OFF), "{out:?}");
        assert!(out.contains(MOUSE_OFF));
        assert!(out.contains("\x1b[?1049l"), "leaves the alternate screen: {out:?}");
        assert!(out.contains("\x1b[?25h"), "shows the cursor: {out:?}");
    }
}

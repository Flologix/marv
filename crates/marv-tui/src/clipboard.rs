//! Copy text to the system clipboard. Ports `src/clipboard.ts` (tests: `tests/clipboard.test.ts`).
//!
//! Terminals have no portable clipboard API, so try the platform's clipboard tool first
//! (wl-copy / xclip / xsel / pbcopy / clip), each reading the text on stdin with stdout and stderr
//! ignored so the tool can't write over the TUI. The fallback is OSC 52, an escape code that asks
//! the terminal itself to set the clipboard; it also works over ssh, but not every terminal
//! supports it (GNOME Console doesn't).
//!
//! OSC 52 is the one escape sequence Marv writes with content in it. It's safe: the payload is
//! base64, so nothing in the copied text can end the sequence early. (We chose this over the
//! `arboard` crate: no X/Wayland libraries linked into the static binary, and it works over ssh.)

/// Clipboard commands to try, in order, each reading the text on stdin.
/// `platform` is `std::env::consts::OS`; `env` is looked up for `WAYLAND_DISPLAY` and `DISPLAY`.
pub fn clipboard_commands(platform: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<Vec<&'static str>> {
    let _ = (platform, env);
    todo!("port: clipboardCommands (darwin: pbcopy; windows: clip; wl-copy if WAYLAND_DISPLAY; xclip, xsel if DISPLAY)")
}

/// `ESC ] 52 ; c ; <base64> BEL`: asks the terminal to set the clipboard.
pub fn osc52(text: &str) -> String {
    let _ = text;
    todo!("port: osc52 with base64::engine::general_purpose::STANDARD")
}

/// How the text was copied: a command name, or "osc52" as a best effort.
pub async fn copy_to_clipboard(text: &str) -> String {
    let _ = text;
    todo!("port: copyToClipboard (tokio::process, first command that exits 0; else write osc52 to stdout)")
}

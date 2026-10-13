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

use std::io::Write;
use std::process::Stdio;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Clipboard commands to try, in order, each reading the text on stdin.
/// `platform` is `std::env::consts::OS`; `env` is looked up for `WAYLAND_DISPLAY` and `DISPLAY`.
pub fn clipboard_commands(platform: &str, env: &dyn Fn(&str) -> Option<String>) -> Vec<Vec<&'static str>> {
    match platform {
        "macos" | "darwin" => return vec![vec!["pbcopy"]],
        "windows" | "win32" => return vec![vec!["clip"]],
        _ => {}
    }
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    let mut commands = Vec::new();
    if set("WAYLAND_DISPLAY") {
        commands.push(vec!["wl-copy"]);
    }
    if set("DISPLAY") {
        commands.push(vec!["xclip", "-selection", "clipboard"]);
        commands.push(vec!["xsel", "--clipboard", "--input"]);
    }
    commands
}

/// `ESC ] 52 ; c ; <base64> BEL`: asks the terminal to set the clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text))
}

/// How the text was copied: a command name, or "osc52" as a best effort.
pub async fn copy_to_clipboard(text: &str) -> String {
    let env = |name: &str| std::env::var(name).ok();
    for command in clipboard_commands(std::env::consts::OS, &env) {
        // stdout/stderr ignored so the tool can't write over the TUI.
        let child = Command::new(command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        // Not installed: try the next one.
        let Ok(mut child) = child else { continue };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes()).await;
            // Dropping stdin closes it, so the tool sees the end of the text.
        }
        if child.wait().await.is_ok_and(|status| status.success()) {
            return command[0].to_string();
        }
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(text).as_bytes());
    let _ = out.flush();
    "osc52".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn uses_pbcopy_on_macos() {
        assert_eq!(clipboard_commands("macos", &env_of(&[])), vec![vec!["pbcopy"]]);
    }

    #[test]
    fn prefers_wl_copy_on_wayland_then_the_x11_tools() {
        assert_eq!(
            clipboard_commands("linux", &env_of(&[("WAYLAND_DISPLAY", "wayland-1"), ("DISPLAY", ":0")])),
            vec![vec!["wl-copy"], vec!["xclip", "-selection", "clipboard"], vec!["xsel", "--clipboard", "--input"]]
        );
    }

    #[test]
    fn has_nothing_to_run_without_a_display() {
        // e.g. over ssh
        assert!(clipboard_commands("linux", &env_of(&[])).is_empty());
    }

    #[test]
    fn osc52_base64_encodes_the_text_into_the_escape_sequence() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
        // Nothing in the text can end the sequence early.
        assert!(!osc52("\x07\x1b\\").contains("\x07\x1b"));
    }
}

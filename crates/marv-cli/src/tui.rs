//! Launching the TUI: the interactive half of Marv's `src/cli.tsx`.
//!
//! Startup, in Marv's order: the project root is the current folder; read AGENTS.md once (cut at 20k, the cut notice
//! goes to the UI); load skills and agents (project + personal; problems shown at startup); load the MCP config and
//! start the manager in the background (always one, even with no servers, so `/mcp add` can start the first);
//! memory; migrate `~/.ekko` (default location only); load the config file (a broken one exits 1 with "Fix the file,
//! or delete it to run setup again."); hidden PATH folders for the startup notice; Herdr reporting in a Herdr pane;
//! then the UI. On the way out: `session.flush()` (≤ 2 s), MCP close (≤ 2 s), Herdr release (≤ 1 s).
//!
//! The TUI (`marv-tui`) doesn't depend on marv-session: it drives a `marv_tui::view::SessionPort` and renders
//! `marv_tui::view::UiEvent`s. The bridge (a `SessionPort` over `MarvSession`, mapping `SessionEvent` → `UiEvent`,
//! applying `CommandAction`s, holding the approval queue behind a `marv_tools::Approver`) belongs here, in marv-cli.

/// Runs the interactive session. `resume`: `Some(true)` for -c (latest), `Some(false)` for -r (pick).
pub async fn run(_resume: Option<bool>) -> anyhow::Result<i32> {
    todo!(
        "port: cli.tsx startup + the SessionPort bridge, then marv_tui's entry point (the TUI crate's run function, once it exists)"
    )
}

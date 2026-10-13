//! Runs `html_to_markdown` in a separate process. The conversion is bounded, but it's still seconds of CPU on a page
//! someone else wrote; a process can be killed at any point, so a page that takes too long is cut off and its plain
//! text used instead.
//!
//! Ports Marv's `src/tools/web/convert.ts`.
//!
//! - One process per page (a fresh one carries nothing over and gives all its memory back: a 5 MB page can take a
//!   gigabyte while it's converted).
//! - The child gets JSON on stdin, an empty environment, and a working folder that isn't the project (Marv: so Bun
//!   wouldn't read the project's `bunfig.toml` / `.env`; marv-rs is one static binary, but the rule stays).
//! - SIGKILLed on timeout ([`CONVERT_TIMEOUT_MS`] = 15 s), abort, completion or Marv's exit; on timeout or failure the
//!   page comes back as plain text (`plain: true`, reason `timeout`/`error`).
//! - Port plan: re-exec the marv binary itself (`std::env::current_exe()`) with a hidden `__html-convert` subcommand
//!   that calls [`crate::web::html_convert_cli::main`]; marv-cli wires the subcommand. Threads can't be killed in Rust
//!   either, which is why this stays a process.

use crate::web::html::Markdown;

/// The slowest bounded conversion measured takes ~5.5 s on a fast machine; this leaves room for a slower one.
pub const CONVERT_TIMEOUT_MS: u64 = 15_000;

/// The page as Markdown, or its plain text if converting took too long or failed. Errors only if the run was stopped
/// or the process couldn't start; either way the child has exited when this returns.
pub async fn convert_html(_html: &str, _url: &str, _cancel: &tokio_util::sync::CancellationToken) -> Result<Markdown, crate::types::ToolError> {
    todo!("port: convertHtml (spawn current_exe __html-convert, JSON over stdin/stdout, kill_on_drop, timeout → plain_text)")
}

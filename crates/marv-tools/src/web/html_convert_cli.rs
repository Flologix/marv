//! The converter process that `convert_html` starts for each page: reads `{ html, url }` as JSON on stdin, runs
//! `html_to_markdown`, writes a [`Reply`] as JSON on stdout, then exits.
//!
//! Ports Marv's `src/tools/web/html-convert-cli.ts`. In marv-rs it's the hidden `marv __html-convert` subcommand
//! (marv-cli calls [`main`]).
//!
//! A conversion that fails (a stack overflow, a library bug on odd markup) is an answer, not a crash: the page was
//! downloaded fine, and `convert_html` can still give its text.

use serde::{Deserialize, Serialize};

use crate::web::html::{Markdown, html_to_markdown};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub html: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Reply {
    Ok { ok: bool, result: Markdown },
    Err { ok: bool, error: String },
}

/// The subcommand's body: stdin → stdout. The reply is one line, the last one written (a test harness may print a
/// banner before it).
pub fn main() -> std::io::Result<()> {
    use std::io::{Read, Write};
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let reply = match serde_json::from_str::<Request>(&input) {
        Ok(Request { html, url }) => {
            // A conversion that panics is an answer too; the default hook would print it, and stderr is ignored.
            std::panic::set_hook(Box::new(|_| {}));
            match std::panic::catch_unwind(|| html_to_markdown(&html, &url)) {
                Ok(result) => Reply::Ok { ok: true, result },
                Err(panic) => {
                    let error = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "the conversion failed".into());
                    Reply::Err { ok: false, error }
                }
            }
        }
        Err(err) => Reply::Err { ok: false, error: err.to_string() },
    };
    let mut out = std::io::stdout().lock();
    out.write_all(format!("\n{}\n", serde_json::to_string(&reply)?).as_bytes())?;
    out.flush()
}

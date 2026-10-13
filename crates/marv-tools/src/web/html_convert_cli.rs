//! The converter process that `convert_html` starts for each page: reads `{ html, url }` as JSON on stdin, runs
//! `html_to_markdown`, writes a [`Reply`] as JSON on stdout, then exits.
//!
//! Ports Marv's `src/tools/web/html-convert-cli.ts`. In marv-rs it's the hidden `marv __html-convert` subcommand
//! (marv-cli calls [`main`]).
//!
//! A conversion that fails (a stack overflow, a library bug on odd markup) is an answer, not a crash: the page was
//! downloaded fine, and `convert_html` can still give its text.

use serde::{Deserialize, Serialize};

use crate::web::html::Markdown;

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

/// The subcommand's body: stdin → stdout.
pub fn main() -> std::io::Result<()> {
    todo!("port: read Request from stdin, catch_unwind(html_to_markdown), write Reply")
}

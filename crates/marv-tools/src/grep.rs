//! `grep`: search file contents with a regular expression.
//!
//! Ports Marv's `src/tools/grep.ts`.
//!
//! - Output `path:line:text`, at most [`MAX_MATCHES`] = 100 lines (then "N more matches not shown"), each cut at
//!   [`MAX_LINE_CHARS`] = 300; files over 1 MB and binary files are skipped; the summary counts matches and files.
//! - A glob without a slash matches the file name anywhere (`*.ts`); with one, the project path.
//! - Never follows a symlink out of the project (each listed file goes through `resolve_in_project`); a FIFO in a
//!   folder is skipped, one named explicitly is refused; a missing path says it doesn't exist. Esc stops the scan.
//! - Port plan: ripgrep's crates (`grep-searcher` + `grep-regex` over the `files_under` list, or `ignore`'s walker
//!   outside git) instead of a line loop. Regex dialect: Marv's is JavaScript's; Rust's `regex` has no lookaround or
//!   backreferences, so either use `fancy-regex` behind the same API or say so in the description and the error.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::repair::repair_path_field;
use crate::types::{Repaired, Tool, ToolContext, ToolError, ToolOutput};

pub const MAX_MATCHES: usize = 100;
pub const MAX_FILE_BYTES: u64 = 1_000_000;
pub const MAX_LINE_CHARS: usize = 300;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrepInput {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    #[serde(rename = "ignoreCase")]
    pub ignore_case: Option<bool>,
}

pub struct Grep;

/// "1 match", "2 matches", "1 file", "3 files".
pub fn plural(n: usize, word: &str) -> String {
    let suffix = if n == 1 {
        ""
    } else if word.ends_with('h') {
        "es"
    } else {
        "s"
    };
    format!("{n} {word}{suffix}")
}

#[async_trait]
impl Tool for Grep {
    type Input = GrepInput;

    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents in the project with a regular expression. Returns matching lines as path:line: text. Respects .gitignore. Use this to find where something is defined or used."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression (JavaScript syntax) to search for, e.g. \"function\\\\s+greet\"."},
                "path": {"type": "string", "description": "File or directory to search, relative to the project root. Default: the whole project."},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. \"*.ts\" or \"src/**/*.tsx\"."},
                "ignoreCase": {"type": "boolean", "description": "Case-insensitive search."}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    fn label(&self, input: &GrepInput) -> String {
        match &input.path {
            Some(path) => format!("\"{}\" in {path}", input.pattern),
            None => format!("\"{}\"", input.pattern),
        }
    }
    fn repair(&self, raw: &Value, ctx: &ToolContext) -> Option<Repaired> {
        repair_path_field(raw, &ctx.root)
    }

    async fn run(&self, _input: &GrepInput, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        todo!(
            "port: compile regex ('Invalid regex: …'), list files (symlinks out and special files skipped), scan with ripgrep's searcher, cap output, summary '<n> matches in <m> files'"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_summaries() {
        assert_eq!(plural(1, "match"), "1 match");
        assert_eq!(plural(3, "match"), "3 matches");
        assert_eq!(plural(2, "file"), "2 files");
    }
}

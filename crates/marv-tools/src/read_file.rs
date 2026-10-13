//! `read_file`: a text file from the project, numbered, in pages.
//!
//! Ports Marv's `src/tools/read-file.ts` (fully).
//!
//! - Pages of [`DEFAULT_LIMIT`] = 400 lines; `offset`/`limit` page through longer files and the result says the next
//!   offset.
//! - Lines are numbered `   12→text` ([`numbered_line`]), not with `cat -n`'s tab: in a tab-indented file models took
//!   the separator for indentation (first line's tab missing, one extra on the others: every failed edit of the
//!   eval's tab task, 2026-10-06). `edit_file`'s closest-lines error and GitHub blobs use the same form.
//! - A line is cut at 500 chars in a long read, but a read of a few lines gives each a share of 30k
//!   ([`LINE_BUDGET_CHARS`]), so `limit: 1` shows a long line whole (an AGENTS.md bullet, issue #40); the cut says so.
//! - Binary files (a NUL in the first 8 KB) aren't dumped; special files (FIFOs) are refused.
//! - Port note: lengths are `char` counts (Marv: UTF-16 units).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::files::{is_directory, looks_binary, project_path, require_regular_file, resolve_in_project};
use crate::repair::repair_path_field;
use crate::types::{Repaired, Tool, ToolContext, ToolError, ToolOutput};

pub const DEFAULT_LIMIT: usize = 400;
pub const MAX_LINE_CHARS: usize = 500;
/// A read of a few lines gives each a share of this; a long read stays at [`MAX_LINE_CHARS`] per line (a minified
/// file would otherwise flood it).
pub const LINE_BUDGET_CHARS: usize = 30_000;

/// A line as read_file shows it: its number (padded to 5), →, then the line exactly as in the file.
pub fn numbered_line(n: usize, line: &str) -> String {
    format!("{n:>5}→{line}")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileInput {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

pub struct ReadFile;

/// The page of `text` the model reads, and its summary. `shown` names the file in errors.
pub fn render(text: &str, offset: usize, limit: usize, shown: &str) -> Result<ToolOutput, ToolError> {
    if text.is_empty() {
        return Ok(ToolOutput::ok("(empty file)", "empty"));
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = body.split('\n').collect();
    let total = lines.len();
    if offset > total {
        return Err(ToolError(format!("{shown} has only {total} lines; offset {offset} is past the end.")));
    }
    let end = total.min(offset + limit - 1);
    let cap = MAX_LINE_CHARS.max(LINE_BUDGET_CHARS / (end - offset + 1));
    let numbered: Vec<String> = lines[offset - 1..end]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let n = offset + i;
            let len = line.chars().count();
            if len <= cap {
                return numbered_line(n, line);
            }
            let whole = if len <= LINE_BUDGET_CHARS { "reads it whole".to_string() } else { format!("reads {LINE_BUDGET_CHARS} of them") };
            let head: String = line.chars().take(cap).collect();
            numbered_line(n, &format!("{head}… (line truncated: {len} characters; offset={n} and limit=1 {whole})"))
        })
        .collect();
    let numbered = numbered.join("\n");
    let partial = offset > 1 || end < total;
    let range = format!("lines {offset}-{end} of {total}");
    let more = if end < total { format!(" Use offset={} to read more.", end + 1) } else { String::new() };
    Ok(if partial {
        ToolOutput::ok(format!("{numbered}\n\n(Showing {range}.{more})"), range)
    } else {
        ToolOutput::ok(numbered, format!("{total} line{}", if total == 1 { "" } else { "s" }))
    })
}

#[async_trait]
impl Tool for ReadFile {
    type Input = ReadFileInput;

    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a text file from the project. Returns each line as its line number, →, then the line exactly as in the file (what to copy into edit_file's old_string starts after the →). Reads up to 400 lines at a time; use offset and limit to page through longer files. A line longer than 500 characters is cut short in a long read; limit=1 shows it whole."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the project root (e.g. src/app.ts)."},
                "offset": {"type": "integer", "minimum": 1, "maximum": 1_000_000, "description": "First line to read (1-based). Default 1."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "description": format!("How many lines to read. Default {DEFAULT_LIMIT}.")}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn check(&self, input: &ReadFileInput) -> Result<(), String> {
        if input.offset.is_some_and(|o| !(1..=1_000_000).contains(&o)) {
            return Err("offset: must be between 1 and 1000000".into());
        }
        if input.limit.is_some_and(|l| !(1..=2000).contains(&l)) {
            return Err("limit: must be between 1 and 2000".into());
        }
        Ok(())
    }
    fn label(&self, input: &ReadFileInput) -> String {
        input.path.clone()
    }
    fn repair(&self, raw: &Value, ctx: &ToolContext) -> Option<Repaired> {
        repair_path_field(raw, &ctx.root)
    }

    async fn run(&self, input: &ReadFileInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let absolute = resolve_in_project(ctx, &input.path)?;
        let shown = project_path(&ctx.root, &absolute);
        if is_directory(&absolute) {
            return Err(ToolError(format!("\"{shown}\" is a directory. Use glob to list its files.")));
        }
        if !absolute.exists() {
            return Err(ToolError(format!("File not found: {}", input.path)));
        }
        require_regular_file(&absolute, &shown)?;
        let bytes = tokio::fs::read(&absolute).await.map_err(|e| ToolError(format!("Couldn't read {shown}: {e}")))?;
        if looks_binary(&bytes) {
            return Ok(ToolOutput::ok(format!("Binary file ({} bytes), not shown.", bytes.len()), "binary file"));
        }
        render(&String::from_utf8_lossy(&bytes), input.offset.unwrap_or(1), input.limit.unwrap_or(DEFAULT_LIMIT), &shown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ToolRef, run_tool};
    use crate::test_support::TempDir;
    use marv_core::ToolCall;
    use std::sync::Arc;

    async fn run(root: &TempDir, args: Value) -> crate::registry::ToolOutcome {
        let tools: Vec<ToolRef> = vec![Arc::new(ReadFile)];
        let call = ToolCall { id: "c".into(), name: "read_file".into(), arguments: args.to_string() };
        run_tool(&call, &ToolContext { home: "/nonexistent-home".into(), ..ToolContext::new(root.path()) }, &tools).await
    }

    #[tokio::test]
    async fn returns_numbered_lines_and_a_summary() {
        let root = TempDir::new("read");
        root.write("src/greet.ts", "export function greet(name: string) {\n  return `Hello, ${name}!`;\n}\n");
        let out = run(&root, json!({"path": "src/greet.ts"})).await;
        assert_eq!(out.result.output, "    1→export function greet(name: string) {\n    2→  return `Hello, ${name}!`;\n    3→}");
        assert_eq!(out.result.summary, "3 lines");
        assert_eq!(out.label, "src/greet.ts");
    }

    #[tokio::test]
    async fn reads_a_slice_and_says_how_to_continue() {
        let root = TempDir::new("read-slice");
        root.write("long.txt", (1..=50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"));
        let out = run(&root, json!({"path": "long.txt", "offset": 10, "limit": 3})).await;
        assert!(out.result.output.starts_with("   10→line 10\n   11→line 11\n   12→line 12"));
        assert!(out.result.output.contains("offset=13"));
        assert_eq!(out.result.summary, "lines 10-12 of 50");
    }

    #[tokio::test]
    async fn a_long_line_is_cut_in_a_long_read_and_whole_with_a_small_limit() {
        // Issue #40: AGENTS.md bullets of several thousand characters on one line were unreadable past 500.
        let root = TempDir::new("read-long");
        let long = format!("- **Topic**: {}end", "word ".repeat(1600));
        root.write("notes.md", format!("# Notes\n{long}\n{}", "short\n".repeat(300)));
        let many = run(&root, json!({"path": "notes.md"})).await.result.output;
        assert!(!many.contains("end"));
        assert!(many.contains(&format!("(line truncated: {} characters; offset=2 and limit=1 reads it whole)", long.len())));
        let one = run(&root, json!({"path": "notes.md", "offset": 2, "limit": 1})).await.result.output;
        assert!(one.contains(&format!("    2→{long}\n")));
    }

    #[tokio::test]
    async fn refuses_outside_missing_directories_symlinks_out_and_dumps_no_binary() {
        let root = TempDir::new("read-refuse");
        let outside = TempDir::new("read-outside");
        outside.write("secret.txt", "top secret\n");
        root.write("src/a.ts", "x");
        root.write("logo.png", b"\x89PNG\0\0\0");
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.path().join("link.txt")).unwrap();
        for (path, message) in [("../outside.txt", "outside the project"), ("/etc/hostname", "outside the project"), ("missing.ts", "File not found: missing.ts"), ("src", "is a directory")] {
            let out = run(&root, json!({ "path": path })).await;
            assert!(out.result.is_error && out.result.output.contains(message), "{path}: {}", out.result.output);
        }
        let link = run(&root, json!({"path": "link.txt"})).await;
        assert!(link.result.is_error && !link.result.output.contains("top secret"));
        assert_eq!(run(&root, json!({"path": "logo.png"})).await.result.output, "Binary file (7 bytes), not shown.");
        let abs = run(&root, json!({"path": root.path().join("src/a.ts").display().to_string()})).await;
        assert_eq!(abs.label, "src/a.ts"); // an absolute label is shown relative
    }

    #[tokio::test]
    async fn repairs_a_leading_slash_path() {
        let root = TempDir::new("read-repair");
        root.write("src/pagination.js", "\tconst start = 0;\n");
        let out = run(&root, json!({"path": "/src/pagination.js"})).await;
        assert!(!out.result.is_error);
        assert!(out.result.output.contains("Marv: the path was written with a leading slash"));
    }
}

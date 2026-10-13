//! `grep`: search file contents with a regular expression.
//!
//! Ports Marv's `src/tools/grep.ts`.
//!
//! - Output `path:line:text`, at most [`MAX_MATCHES`] = 100 lines (then "N more matches not shown"), each cut at
//!   [`MAX_LINE_CHARS`] = 300; files over 1 MB and binary files are skipped; the summary counts matches and files.
//! - A glob without a slash matches the file name anywhere (`*.ts`); with one, the project path.
//! - Never follows a symlink out of the project (each listed file goes through `resolve_in_project`); a FIFO in a
//!   folder is skipped, one named explicitly is refused; a missing path says it doesn't exist. Esc stops the scan.
//! - The search is ripgrep's engine (`grep-regex` + `grep-searcher`) over the `files_under` list (git ls-files, or
//!   the `ignore` walker outside git), in place of Marv's line loop; the caps are Marv's. A file is read whole (at
//!   most 1 MB), checked for a NUL in its first 8 KB like `read_file`, then searched line by line.
//! - marv-rs: the regex dialect is Rust's (`regex`), not JavaScript's: the same syntax for everything a search
//!   usually needs, but no lookaround and no backreferences. The schema keeps Marv's words ("JavaScript syntax"),
//!   so the requests match Marv's (the oracle, #11); the error for a pattern that uses them says so instead, and the
//!   model rewrites it.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, SearcherBuilder, sinks};

use crate::files::{
    files_under, is_directory, is_regular_file, looks_binary, project_path, require_regular_file, resolve_in_project,
};
use crate::glob::glob_matcher;
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

    async fn run(&self, input: &GrepInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(input.ignore_case.unwrap_or(false))
            .build(&input.pattern)
            .map_err(|e| ToolError(invalid_regex(&e.to_string())))?;
        let path = input.path.as_deref().unwrap_or(".");
        let target = resolve_in_project(ctx, path)?;
        let base = project_path(&ctx.root, &target);
        // A glob without a slash matches the file name anywhere ("*.ts"); with one, the project path.
        let filter = input.glob.as_deref().map(|g| Ok::<_, ToolError>((glob_matcher(g)?, g.contains('/')))).transpose()?;
        let wanted = |file: &str| match &filter {
            None => true,
            Some((glob, true)) => glob.is_match(file),
            Some((glob, false)) => glob.is_match(&file[file.rfind('/').map_or(0, |i| i + 1)..]),
        };

        let files: Vec<String> = if is_directory(&target) {
            files_under(&ctx.root, &base, &ctx.git_env)
                .await?
                .into_iter()
                // A symlink in the project can point outside it; resolve_in_project refuses those, as read_file does.
                .filter(|file| resolve_in_project(ctx, file).is_ok_and(|p| is_regular_file(&p)))
                .collect()
        } else {
            require_regular_file(&target, &base)?;
            vec![base]
        };
        let files: Vec<String> = files.into_iter().filter(|f| wanted(f)).collect();

        let root = ctx.root.clone();
        let cancel = ctx.cancel.clone();
        let found = tokio::task::spawn_blocking(move || search(&root, &files, &matcher, &cancel))
            .await
            .map_err(|e| ToolError(format!("The search failed: {e}")))?;

        if found.count == 0 {
            return Ok(ToolOutput::ok("No matches.", "0 matches"));
        }
        let more = if found.count > MAX_MATCHES {
            format!("\n\n({} more matches not shown; narrow the search.)", found.count - MAX_MATCHES)
        } else {
            String::new()
        };
        Ok(ToolOutput::ok(
            format!("{}{more}", found.lines.join("\n")),
            format!("{} in {}", plural(found.count, "match"), plural(found.files, "file")),
        ))
    }
}

/// What the scan found: the first [`MAX_MATCHES`] lines, and how many matches in how many files in all.
#[derive(Debug, Default)]
struct Found {
    lines: Vec<String>,
    count: usize,
    files: usize,
}

/// Searches the files (project-relative) one by one; Esc stops between files.
fn search(
    root: &std::path::Path,
    files: &[String],
    matcher: &grep_regex::RegexMatcher,
    cancel: &tokio_util::sync::CancellationToken,
) -> Found {
    let mut searcher: Searcher = SearcherBuilder::new().line_number(true).build();
    let mut found = Found::default();
    for file in files {
        if cancel.is_cancelled() {
            break;
        }
        let path = root.join(file);
        if std::fs::metadata(&path).map_or(true, |m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if looks_binary(&bytes) {
            continue;
        }
        let before = found.count;
        let sink = sinks::Lossy(|line_number, line| {
            found.count += 1;
            if found.lines.len() < MAX_MATCHES {
                let line = line.strip_suffix('\n').unwrap_or(line);
                let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
                found.lines.push(format!("{file}:{line_number}:{cut}"));
            }
            Ok(true)
        });
        let _ = searcher.search_slice(matcher, &bytes, sink);
        if found.count > before {
            found.files += 1;
        }
    }
    found
}

/// The error for a pattern that doesn't compile; a hint when it looks like JavaScript-only syntax.
fn invalid_regex(err: &str) -> String {
    let hint = if err.contains("look-around") || err.contains("backreference") {
        " Lookaround ((?=…), (?!…), (?<=…), (?<!…)) and backreferences (\\1) aren't supported: rewrite the pattern without them."
    } else {
        ""
    };
    format!("Invalid regex: {err}{hint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::registry::{ToolOutcome, builtin_tools, run_tool};
    use crate::test_support::{TempDir, git, mkfifo, project};
    use marv_core::ToolCall;

    async fn run(root: &TempDir, args: Value) -> ToolOutcome {
        let call = ToolCall { id: "g".into(), name: "grep".into(), arguments: args.to_string() };
        run_tool(&call, &ToolContext::new(root.path()), builtin_tools()).await
    }

    #[tokio::test]
    async fn finds_matching_lines_with_file_and_line_number() {
        let root = project("grep");
        let out = run(&root, json!({"pattern": "greet\\("})).await.result;
        assert_eq!(out.output, "src/app.ts:2:  return greet('world');\nsrc/greet.ts:1:export function greet(name: string) {");
        assert_eq!(out.summary, "2 matches in 2 files");
    }

    #[tokio::test]
    async fn filters_files_with_a_glob_and_can_ignore_case() {
        let root = project("grep-glob");
        let out = run(&root, json!({"pattern": "todo", "glob": "*.tsx", "ignoreCase": true})).await.result;
        assert_eq!(out.output, "src/ui/view.tsx:1:// TODO: render the greeting");
        let pathed = run(&root, json!({"pattern": "greet", "glob": "src/*.ts"})).await.result;
        assert_eq!(pathed.summary, "2 matches in 2 files"); // not src/ui/view.tsx
    }

    #[tokio::test]
    async fn reports_no_matches_and_bad_patterns_clearly() {
        let root = project("grep-bad");
        assert_eq!(run(&root, json!({"pattern": "nothing-like-this"})).await.result.output, "No matches.");
        let bad = run(&root, json!({"pattern": "("})).await.result;
        assert!(bad.is_error && bad.output.contains("Invalid regex"));
        // JavaScript-only syntax says what to do instead.
        let lookahead = run(&root, json!({"pattern": "greet(?=\\()"})).await.result;
        assert!(lookahead.is_error && lookahead.output.contains("Lookaround"), "{}", lookahead.output);
    }

    #[tokio::test]
    async fn caps_matches_and_long_lines_and_skips_binary_and_big_files() {
        let root = TempDir::new("grep-caps");
        root.write("many.txt", "hit\n".repeat(120));
        root.write("long.txt", format!("hit {}\n", "x".repeat(400)));
        root.write("bin.dat", b"hit\0hit\n");
        root.write("big.txt", format!("hit\n{}", "y".repeat(1_100_000)));
        let out = run(&root, json!({"pattern": "hit"})).await.result;
        assert_eq!(out.summary, "121 matches in 2 files");
        assert!(out.output.ends_with("\n\n(21 more matches not shown; narrow the search.)"));
        assert_eq!(out.output.lines().filter(|l| l.contains(":hit")).count(), 100);
        let long = run(&root, json!({"pattern": "hit", "path": "long.txt"})).await.result;
        assert_eq!(long.output.chars().count(), "long.txt:1:".len() + MAX_LINE_CHARS);
    }

    #[tokio::test]
    async fn a_folder_wide_grep_doesnt_follow_a_symlink_out_of_the_project() {
        let root = project("grep-symlink");
        let outside = TempDir::new("grep-outside");
        outside.write("secret.txt", "top secret\n");
        git(root.path(), &["init", "-q"]); // the git listing includes symlinks
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.path().join("notes.txt")).unwrap();
        let out = run(&root, json!({"pattern": "top secret|greet"})).await.result;
        assert!(!out.output.contains("top secret"));
        assert!(!out.output.contains("notes.txt"));
        assert!(out.output.contains("src/greet.ts"));
    }

    #[tokio::test]
    async fn skips_a_fifo_in_a_directory_and_refuses_one_named_explicitly() {
        let root = project("grep-fifo");
        mkfifo(&root.path().join("src/pipe"));
        let dir = run(&root, json!({"pattern": "greet", "path": "src"})).await.result;
        assert!(!dir.is_error);
        assert!(dir.output.contains("src/greet.ts"));
        let named = run(&root, json!({"pattern": "x", "path": "src/pipe"})).await.result;
        assert!(named.is_error && named.output.contains("not a regular file"));
    }

    #[tokio::test]
    async fn a_path_that_doesnt_exist_says_so_not_that_its_a_special_file() {
        // A model was told a missing file was "a pipe, socket or device" (trajectory, 2026-10-05).
        let root = project("grep-missing");
        let out = run(&root, json!({"pattern": "x", "path": "src/missing.ts"})).await.result;
        assert!(out.is_error && out.output.contains("doesn't exist"));
        assert!(!out.output.contains("not a regular file"));
    }

    #[tokio::test]
    async fn esc_stops_the_scan() {
        let root = project("grep-esc");
        let ctx = ToolContext::new(root.path());
        ctx.cancel.cancel();
        let out = Grep.run(&GrepInput { pattern: "greet".into(), path: None, glob: None, ignore_case: None }, &ctx).await;
        assert_eq!(out.unwrap().output, "No matches.");
    }

    #[test]
    fn plural_summaries() {
        assert_eq!(plural(1, "match"), "1 match");
        assert_eq!(plural(3, "match"), "3 matches");
        assert_eq!(plural(2, "file"), "2 files");
    }
}

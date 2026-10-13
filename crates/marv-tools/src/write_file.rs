//! `write_file`: create a file, replace its contents, or append to it.
//!
//! Ports Marv's `src/tools/write-file.ts`.
//!
//! - `append: true` adds content at the end of an existing file instead of replacing it: the cut-off notes tell the
//!   model to write a long file in parts that way. A file that doesn't exist is just created; a last line without a
//!   newline gets one first, and the result says so.
//! - A CRLF file keeps CRLF: a model writes `\n`, and rewriting a CRLF file would otherwise change every line.
//! - Folders are created as needed. Read and written as one step per file (`with_file_lock`), so a parallel append or
//!   edit can't be lost.
//! - Like `edit_file`: preview starts `before_change`, run awaits it (Esc meanwhile writes nothing), `.git` re-checked
//!   at apply time when confined, syntax note after the change, protected paths ask even in yolo (marv-rs), formatter
//!   after the write planned (#67).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::diff::{Diff, added_lines, diff_text};
use crate::edit_file::{protected_target, start_before_change};
use crate::edit_match::{line_ending, with_line_ending};
use crate::files::{is_directory, project_path, refuse_git, require_regular_file, resolve_in_project, touches_git};
use crate::repair::repair_path_field;
use crate::types::{Preview, Repaired, Scope, Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteFileInput {
    pub path: String,
    pub content: String,
    pub append: Option<bool>,
}

/// The file as it will be written, and the diff that shows the change. `before` is None for a new file.
#[derive(Debug, Clone)]
pub struct Plan {
    pub absolute: std::path::PathBuf,
    pub shown: String,
    pub before: Option<String>,
    /// What's written (or appended), with CRLF and the added newline applied.
    pub body: String,
    pub after: String,
    pub diff: Diff,
    pub new_line_missing: bool,
    pub crlf: bool,
}

/// The pure part of the plan: what to write given what's there.
pub fn plan_text(before: Option<&str>, content: &str, append: bool) -> (String, String, bool, bool) {
    let new_line_missing = append && before.is_some_and(|b| !b.is_empty() && !b.ends_with('\n'));
    let crlf = before.is_some_and(|b| line_ending(b) == "\r\n");
    let mut body = if crlf { with_line_ending(content, "\r\n") } else { content.to_string() };
    if new_line_missing {
        body = format!("{}{body}", if crlf { "\r\n" } else { "\n" });
    }
    let after = match before {
        None => body.clone(),
        Some(b) if append => format!("{b}{body}"),
        Some(_) => body.clone(),
    };
    (body, after, new_line_missing, crlf)
}

pub async fn plan(input: &WriteFileInput, ctx: &ToolContext) -> Result<Plan, ToolError> {
    let absolute = resolve_in_project(ctx, &input.path)?;
    if ctx.confined {
        refuse_git(&ctx.root, &absolute)?;
    }
    let shown = project_path(&ctx.root, &absolute);
    if is_directory(&absolute) {
        return Err(ToolError(format!("\"{shown}\" is a directory.")));
    }
    let before = if absolute.exists() {
        require_regular_file(&absolute, &shown)?;
        Some(tokio::fs::read_to_string(&absolute).await.map_err(|e| ToolError(format!("Couldn't read {shown}: {e}")))?)
    } else {
        None
    };
    let (body, after, new_line_missing, crlf) = plan_text(before.as_deref(), &input.content, input.append.unwrap_or(false));
    let diff = match &before {
        None => Diff { lines: added_lines(&body), added: 0, removed: 0 },
        Some(b) => diff_text(b, &after, 3),
    };
    Ok(Plan { absolute, shown, before, body, after, diff, new_line_missing, crlf })
}

/// Lines in a text, not counting a final newline.
pub fn line_count(text: &str) -> usize {
    if text.is_empty() { 0 } else { text.strip_suffix('\n').unwrap_or(text).split('\n').count() }
}

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    type Input = WriteFileInput;

    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create a new file, or replace a file's entire contents. With append: true, add content at the end of an existing file instead (to write a long file in parts: write the first part, then append the rest). For changing part of a file another way, use edit_file instead. The user may be asked to approve each change."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to write, relative to the project root. Folders are created as needed."},
                "content": {"type": "string", "description": "The complete contents of the file, or with append: true the part to add at the end."},
                "append": {"type": "boolean", "description": "Add content at the end of an existing file instead of replacing it. A file that doesn't exist is just created; if the file's last line has no newline, one is added first."}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Write
    }
    fn label(&self, input: &WriteFileInput) -> String {
        input.path.clone()
    }
    fn scope(&self, _input: &WriteFileInput) -> Option<Scope> {
        Some(Scope { key: "files".into(), description: "file changes".into() })
    }
    fn auto_safe(&self, input: &WriteFileInput, ctx: &ToolContext) -> bool {
        !touches_git(ctx, &input.path)
    }
    fn protected_target(&self, input: &WriteFileInput, ctx: &ToolContext) -> Option<marv_sandbox::ProtectedRule> {
        protected_target(&input.path, ctx)
        // TODO(port): package.json → marv_sandbox::protected::lifecycle_scripts_changed(before, after) (content rule).
    }
    fn repair(&self, raw: &Value, ctx: &ToolContext) -> Option<Repaired> {
        repair_path_field(raw, &ctx.root)
    }

    async fn preview(&self, input: &WriteFileInput, ctx: &ToolContext) -> Result<Preview, ToolError> {
        start_before_change(ctx);
        let plan = plan(input, ctx).await?;
        let verb = match (&plan.before, input.append.unwrap_or(false)) {
            (None, _) => "Create",
            (Some(_), true) => "Append",
            (Some(_), false) => "Overwrite",
        };
        // All of it: the prompt shows what fits and counts the rest (a preview is never saved).
        Ok(Preview { title: format!("{verb} {}", plan.shown), diff: Some(plan.diff.lines), ..Default::default() })
    }

    async fn run(&self, _input: &WriteFileInput, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        todo!(
            "port: await before_change; aborted → nothing written; with_file_lock(plan, mkdir -p, write); broken_by note; \
             'Created <shown> (N lines).' / 'Wrote|Appended to <shown> (+a −r lines[; added a newline…][; kept its CRLF…]).'"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crlf_file_keeps_crlf_and_an_append_adds_the_missing_newline() {
        assert_eq!(plan_text(Some("a\r\nb\r\n"), "x\ny\n", false), ("x\r\ny\r\n".into(), "x\r\ny\r\n".into(), false, true));
        assert_eq!(plan_text(Some("a"), "b\n", true), ("\nb\n".into(), "a\nb\n".into(), true, false));
        assert_eq!(plan_text(None, "new\n", true), ("new\n".into(), "new\n".into(), false, false));
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count(""), 0);
    }
}

//! `edit_file`: replace text that matches exactly once (or every occurrence with `replace_all`).
//!
//! Ports Marv's `src/tools/edit-file.ts`. Matching is [`crate::edit_match`].
//!
//! - Planned twice: in the preview (so a call that can't succeed fails before anyone is asked) and again at apply
//!   time, under the file lock, in case the file changed while the user decided.
//! - Not found: the error shows the file's closest lines numbered like `read_file`, so the model can copy the real text
//!   in its next call; it suggests `write_file` only when the parent folder exists (a mistyped folder would become a
//!   stray copy).
//! - A forgiving match says which one applied ([`fuzzy_note`]), the Unicode tier (#60) included.
//! - After the change, `syntax::broken_by` adds a `Marv:` note when the change broke the file's syntax.
//! - Yolo: `auto_safe` outside `.git` (`touches_git`, through symlinks), re-checked at apply time (`refuse_git`).
//! - marv-rs: a protected path (`marv_sandbox::protected_rule`) asks even in yolo, and so does a `package.json` edit
//!   that changes a lifecycle script (the content rule, `marv_sandbox::protected::lifecycle_scripts_changed`).
//! - `run` awaits `before_change` (or Esc), then plans again, writes and formats under [`with_file_lock`], so a
//!   parallel edit or append on the same file can't be lost, and the formatter can't be formatted over (#67).
//! - #67: the project's formatter runs on the changed file after the write, inside the sandbox; the diff and the
//!   summary describe the formatted file, and the note says so when formatting changed lines beyond the edit.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::diff::{change_summary, diff_text, shown_diff};
use crate::edit_match::{EditResult, Fuzzy, apply_edit, closest_lines};
use crate::file_lock::with_file_lock;
use crate::files::{is_directory, project_path, refuse_git, require_regular_file, resolve_in_project, touches_git};
use crate::read_file::numbered_line;
use crate::repair::repair_path_field;
use crate::syntax::{broken_by, syntax_note};
use crate::types::{Preview, Repaired, Scope, Tool, ToolContext, ToolError, ToolOutput};

const MAX_CLOSEST_LINES: usize = 30;
const MAX_CLOSEST_CHARS: usize = 500;

/// What the result says after a forgiving match.
pub fn fuzzy_note(fuzzy: Fuzzy) -> &'static str {
    match fuzzy {
        Fuzzy::Unicode => {
            "old_string matched once typographic characters (curly quotes, dashes, …, Unicode spaces) were read as ASCII."
        }
        Fuzzy::TrailingWhitespace => "old_string matched once trailing whitespace was ignored.",
        Fuzzy::Indentation => "old_string matched only with different indentation; new_string was re-indented to match the file.",
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditFileInput {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: Option<bool>,
}

/// The file before and after.
#[derive(Debug, Clone)]
pub struct Plan {
    pub absolute: std::path::PathBuf,
    pub shown: String,
    pub before: String,
    pub after: String,
    pub count: usize,
    pub fuzzy: Option<Fuzzy>,
}

/// The not-found error, with the file's closest lines as they are now.
pub fn not_found(shown: &str, text: &str, old_string: &str) -> String {
    let head = format!("old_string was not found in {shown}, not even ignoring differences in whitespace and indentation.");
    let Some((start, end)) = closest_lines(text, old_string) else {
        return format!("{head} It must match the file exactly. Use read_file to see the current text, then try again.");
    };
    let last = end.min(start + MAX_CLOSEST_LINES - 1);
    let lines: Vec<String> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .skip(start - 1)
        .take(last - start + 1)
        .enumerate()
        .map(|(i, line)| {
            if line.chars().count() > MAX_CLOSEST_CHARS {
                numbered_line(
                    start + i,
                    &format!("{}… (line truncated)", line.chars().take(MAX_CLOSEST_CHARS).collect::<String>()),
                )
            } else {
                numbered_line(start + i, line)
            }
        })
        .collect();
    format!(
        "{head} The closest text is lines {start}-{end}, as it is now:\n{}\nIf that's the part you meant, copy old_string from these lines exactly (what follows each →). Otherwise use read_file to find it.",
        lines.join("\n")
    )
}

/// The file before and after, or a ToolError the model can act on. Used for the preview and again when applying.
pub async fn plan(input: &EditFileInput, ctx: &ToolContext) -> Result<Plan, ToolError> {
    let absolute = resolve_in_project(ctx, &input.path)?;
    if ctx.confined {
        refuse_git(&ctx.root, &absolute)?;
    }
    let shown = project_path(&ctx.root, &absolute);
    if is_directory(&absolute) {
        return Err(ToolError(format!("\"{shown}\" is a directory.")));
    }
    if !absolute.exists() {
        let hint = if absolute.parent().is_some_and(is_directory) { " Use write_file to create a new file." } else { "" };
        return Err(ToolError(format!("File not found: {shown}.{hint}")));
    }
    require_regular_file(&absolute, &shown)?;
    let before = tokio::fs::read_to_string(&absolute).await.map_err(|e| ToolError(format!("Couldn't read {shown}: {e}")))?;
    match apply_edit(&before, &input.old_string, &input.new_string, input.replace_all.unwrap_or(false)) {
        EditResult::Ambiguous { count } => Err(ToolError(format!(
            "old_string appears {count} times in {shown}. Include more of the surrounding lines so it matches exactly once, or set replace_all to change all {count}."
        ))),
        EditResult::NotFound => Err(ToolError(not_found(&shown, &before, &input.old_string))),
        EditResult::Applied { text, count, fuzzy } => Ok(Plan { absolute, shown, before, after: text, count, fuzzy }),
    }
}

/// Starts the session's before-change hook without waiting (the typecheck baseline runs while the user decides).
pub(crate) fn start_before_change(ctx: &ToolContext) {
    if let Some(hook) = ctx.before_change.clone() {
        tokio::spawn(async move { hook.before_change().await });
    }
}

/// What a change returns when Esc came while it waited for the baseline.
pub(crate) const INTERRUPTED_BEFORE_WRITE: &str = "Interrupted before the change was written: nothing changed.";

/// Awaits the session's before-change hook, or Esc. True: stopped, so nothing may be written (the hook resolves on
/// abort, but the user said stop).
pub(crate) async fn await_before_change(ctx: &ToolContext) -> bool {
    if let Some(hook) = &ctx.before_change {
        tokio::select! {
            () = hook.before_change() => {}
            () = ctx.cancel.cancelled() => {}
        }
    }
    ctx.aborted()
}

pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    type Input = EditFileInput;

    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Change part of a file by replacing exact text: old_string must match the file exactly once (copy it from read_file, with its whitespace and indentation, and include enough lines to be unique), or set replace_all. Prefer this over write_file for existing files. The user may be asked to approve each change."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit, relative to the project root."},
                "old_string": {"type": "string", "minLength": 1, "description": "The exact text to replace, copied from the file including whitespace and indentation."},
                "new_string": {"type": "string", "description": "What to replace it with."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of exactly one."}
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Write
    }
    fn check(&self, input: &EditFileInput) -> Result<(), String> {
        if input.old_string.is_empty() { Err("old_string: must not be empty".into()) } else { Ok(()) }
    }
    fn label(&self, input: &EditFileInput) -> String {
        input.path.clone()
    }
    fn scope(&self, _input: &EditFileInput) -> Option<Scope> {
        Some(Scope { key: "files".into(), description: "file changes".into() })
    }
    fn auto_safe(&self, input: &EditFileInput, ctx: &ToolContext) -> bool {
        !touches_git(ctx, &input.path)
    }
    fn protected_target(&self, input: &EditFileInput, ctx: &ToolContext) -> Option<marv_sandbox::ProtectedRule> {
        protected_target(&input.path, ctx)
            .or_else(|| package_scripts_changed(input, ctx).then_some(marv_sandbox::ProtectedRule::PackageScripts))
    }
    fn repair(&self, raw: &Value, ctx: &ToolContext) -> Option<Repaired> {
        repair_path_field(raw, &ctx.root)
    }

    async fn preview(&self, input: &EditFileInput, ctx: &ToolContext) -> Result<Preview, ToolError> {
        start_before_change(ctx);
        let plan = plan(input, ctx).await?;
        Ok(Preview {
            title: format!("Edit {}", plan.shown),
            diff: Some(diff_text(&plan.before, &plan.after, 3).lines),
            ..Default::default()
        })
    }

    async fn run(&self, input: &EditFileInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if await_before_change(ctx).await {
            return Ok(ToolOutput::ok(INTERRUPTED_BEFORE_WRITE, "interrupted"));
        }
        // Planned again: the file may have changed while the user was deciding. Read, written and formatted as one
        // step per file, so a parallel edit (or append) can't be lost, and the formatter can't be formatted over.
        let (planned, formatted) = with_file_lock(&ctx.root, &input.path, || async {
            let planned = plan(input, ctx).await?;
            tokio::fs::write(&planned.absolute, &planned.after)
                .await
                .map_err(|e| ToolError(format!("Couldn't write {}: {e}", planned.shown)))?;
            // #67: the project's formatter runs before anything is said; its failure is a note, not an error.
            let formatted = crate::formatter::after_write(ctx, &planned.shown, &planned.absolute, &planned.after).await;
            Ok::<_, ToolError>((planned, formatted))
        })
        .await?;
        let diff = diff_text(&planned.before, &formatted.content, 3);
        let fuzzy = planned.fuzzy.map(|f| format!(" {}", fuzzy_note(f))).unwrap_or_default();
        let notes = crate::formatter::notes(&formatted, &planned.after);
        let broken = match broken_by(&planned.shown, Some(&planned.before), &formatted.content).await {
            Some(problem) => format!("\n\n{}", syntax_note(&planned.shown, &problem, &formatted.content, true)),
            None => String::new(),
        };
        let plural = if planned.count == 1 { "" } else { "s" };
        let summary = change_summary(&diff);
        let mut result = ToolOutput::ok(
            format!(
                "Edited {}: replaced {} occurrence{} ({summary} lines).{fuzzy}{notes}{broken}",
                planned.shown, planned.count, plural
            ),
            summary,
        );
        result.diff = Some(shown_diff(&diff.lines));
        result.changed = Some(planned.shown.clone());
        Ok(result)
    }
}

/// marv-rs: an edit to a `package.json` that changes a lifecycle script asks even in yolo (`docs/SECURITY.md`). The
/// edit is planned on the file as it is now; one that can't apply asks nothing here (its preview fails).
fn package_scripts_changed(input: &EditFileInput, ctx: &ToolContext) -> bool {
    let Some(absolute) = package_json(&input.path, ctx) else { return false };
    let Ok(before) = std::fs::read_to_string(&absolute) else { return false };
    match apply_edit(&before, &input.old_string, &input.new_string, input.replace_all.unwrap_or(false)) {
        EditResult::Applied { text, .. } => marv_sandbox::protected::lifecycle_scripts_changed(Some(&before), &text),
        _ => false,
    }
}

/// The resolved path when `path` names a `package.json` inside the project.
pub(crate) fn package_json(path: &str, ctx: &ToolContext) -> Option<std::path::PathBuf> {
    let absolute = resolve_in_project(ctx, path).ok()?;
    (absolute.file_name()? == "package.json").then_some(absolute)
}

/// The protected rule a project path falls under (marv-rs); a path outside the project is left to `resolve_in_project`.
/// When the project is the home folder (or holds it), the home's dotfiles and git hooks folders are protected too
/// (`marv_sandbox::protected::home_rule`): this tool writes outside the sandbox.
pub(crate) fn protected_target(path: &str, ctx: &ToolContext) -> Option<marv_sandbox::ProtectedRule> {
    let absolute = resolve_in_project(ctx, path).ok()?;
    marv_sandbox::protected_rule(&ctx.root, &absolute)
        .unwrap_or(Some(marv_sandbox::ProtectedRule::GitDir))
        .or_else(|| marv_sandbox::protected::home_rule(&ctx.root, &ctx.home, &absolute))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn input(path: &str, old: &str, new: &str) -> EditFileInput {
        EditFileInput { path: path.into(), old_string: old.into(), new_string: new.into(), replace_all: None }
    }

    #[tokio::test]
    async fn not_found_suggests_write_file_only_when_the_folder_exists() {
        let root = TempDir::new("edit");
        root.write("src/a.ts", "x");
        let ctx = ToolContext::new(root.path());
        assert_eq!(
            plan(&input("src/new.ts", "a", "b"), &ctx).await.unwrap_err().0,
            "File not found: src/new.ts. Use write_file to create a new file."
        );
        assert_eq!(plan(&input("srcx/new.ts", "a", "b"), &ctx).await.unwrap_err().0, "File not found: srcx/new.ts.");
    }

    #[tokio::test]
    async fn nothing_matches_shows_the_closest_lines() {
        let root = TempDir::new("edit-closest");
        root.write("greet.js", "function a() {\n  return 1;\n}\n\nfunction greet(name) {\n  return `Hello, ${name}!`;\n}\n");
        let err = plan(
            &input("greet.js", "function greet(person) {\n  return `Hi, ${person}!`;\n}", "x"),
            &ToolContext::new(root.path()),
        )
        .await
        .unwrap_err();
        assert!(err.0.contains("The closest text is lines 5-7, as it is now:\n    5→function greet(name) {"), "{}", err.0);
    }

    #[tokio::test]
    async fn ambiguous_matches_say_how_many() {
        let root = TempDir::new("edit-ambiguous");
        root.write("a.txt", "x\nx\n");
        let err = plan(&input("a.txt", "x", "y"), &ToolContext::new(root.path())).await.unwrap_err();
        assert!(err.0.starts_with("old_string appears 2 times in a.txt."));
    }

    #[test]
    fn protected_files_are_flagged() {
        let root = TempDir::new("edit-protected");
        let ctx = ToolContext::new(root.path());
        assert_eq!(protected_target("AGENTS.md", &ctx), Some(marv_sandbox::ProtectedRule::AgentInstructions));
        assert_eq!(protected_target("src/a.ts", &ctx), None);
    }
}

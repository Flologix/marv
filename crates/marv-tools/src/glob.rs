//! `glob`: find files in the project by name pattern.
//!
//! Ports Marv's `src/tools/glob.ts`.
//!
//! - The file list is `files::files_under` (git ls-files, so `.gitignore` applies; the walk outside a repo skips
//!   node_modules, dist, …). The pattern matches paths relative to `path` (`**/*.ts`, `src/*.tsx` only directly in
//!   src). Sorted; at most [`MAX_RESULTS`] = 200, then a "narrow the pattern" note.
//! - Matching is `globset` with `literal_separator` (`*` stays within one folder, as in Bun.Glob); `**`, braces,
//!   classes and dotfiles behave like Bun.Glob's (checked against Bun in [`tests`]). An invalid pattern is an error
//!   the model can fix (Bun.Glob matched nothing instead).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::files::{files_under, is_directory, project_path, resolve_in_project};
use crate::repair::repair_path_field;
use crate::types::{Repaired, Tool, ToolContext, ToolError, ToolOutput};

pub const MAX_RESULTS: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobInput {
    pub pattern: String,
    pub path: Option<String>,
}

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    type Input = GlobInput;

    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files in the project by name pattern. Returns matching paths, sorted. Respects .gitignore, so dependencies and build output are skipped. Use this to explore the project's structure."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, e.g. \"**/*.ts\" (all .ts files) or \"src/*.tsx\" (only directly in src)."},
                "path": {"type": "string", "description": "Directory to search in, relative to the project root. Default: the whole project."}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Read
    }
    /// The project's file names.
    fn provenance(&self) -> marv_core::Provenance {
        marv_core::Provenance::Project
    }
    fn label(&self, input: &GlobInput) -> String {
        match &input.path {
            Some(path) => format!("{} in {path}", input.pattern),
            None => input.pattern.clone(),
        }
    }
    fn repair(&self, raw: &Value, ctx: &ToolContext) -> Option<Repaired> {
        repair_path_field(raw, &ctx.root)
    }

    async fn run(&self, input: &GlobInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path = input.path.as_deref().unwrap_or(".");
        let dir = resolve_in_project(ctx, path)?;
        if !is_directory(&dir) {
            return Err(ToolError(format!("\"{path}\" is not a directory.")));
        }
        let base = project_path(&ctx.root, &dir);
        let matcher = glob_matcher(&input.pattern)?;
        let matches: Vec<String> = files_under(&ctx.root, &base, &ctx.git_env)
            .await?
            .into_iter()
            .filter(|file| matcher.is_match(if base == "." { file.as_str() } else { &file[base.len() + 1..] }))
            .collect();
        if matches.is_empty() {
            return Ok(ToolOutput::ok("No files match.", "0 files"));
        }
        let shown = matches[..matches.len().min(MAX_RESULTS)].join("\n");
        let more = if matches.len() > MAX_RESULTS {
            format!("\n\n({} more not shown; narrow the pattern.)", matches.len() - MAX_RESULTS)
        } else {
            String::new()
        };
        let n = matches.len();
        Ok(ToolOutput::ok(format!("{shown}{more}"), format!("{n} file{}", if n == 1 { "" } else { "s" })))
    }
}

/// A glob as Bun.Glob matches it: `*` doesn't cross `/`, `**` does, dotfiles match, case matters.
pub fn glob_matcher(pattern: &str) -> Result<globset::GlobMatcher, ToolError> {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| ToolError(format!("Invalid glob pattern: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ToolOutcome, builtin_tools, run_tool};
    use crate::test_support::{TempDir, git, project};
    use marv_core::ToolCall;

    async fn run(root: &TempDir, args: Value) -> ToolOutcome {
        let call = ToolCall { id: "g".into(), name: "glob".into(), arguments: args.to_string() };
        run_tool(&call, &ToolContext::new(root.path()), builtin_tools()).await
    }

    #[test]
    fn matches_like_bun_glob() {
        // The same cases, checked with Bun: `new Bun.Glob(p).match(s)`.
        let m = |p: &str, s: &str| glob_matcher(p).unwrap().is_match(s);
        assert!(m("*", ".env"));
        assert!(m("**/*.ts", "a/.x/b.ts"));
        assert!(!m("*.ts", "a/b.ts"));
        assert!(m("**/*.ts", "b.ts"));
        assert!(m("src/{a,b}.ts", "src/b.ts"));
        assert!(m("[ab].ts", "a.ts"));
        assert!(m("**", "a/b"));
        assert!(!m("*.TS", "a.ts"));
        assert!(m("src/**", "src/a/b"));
    }

    #[tokio::test]
    async fn finds_files_by_pattern_sorted_relative_to_the_project() {
        let root = project("glob");
        let out = run(&root, json!({"pattern": "**/*.ts"})).await.result;
        assert_eq!(out.output, "src/app.ts\nsrc/greet.ts");
        assert_eq!(out.summary, "2 files");
    }

    #[tokio::test]
    async fn searches_inside_a_directory() {
        let root = project("glob-dir");
        assert_eq!(run(&root, json!({"pattern": "**/*", "path": "src/ui"})).await.result.output, "src/ui/view.tsx");
        let not_dir = run(&root, json!({"pattern": "*", "path": "src/app.ts"})).await.result;
        assert!(not_dir.is_error && not_dir.output.contains("is not a directory"));
    }

    #[tokio::test]
    async fn skips_node_modules_even_outside_git() {
        let root = project("glob-nm");
        assert_eq!(run(&root, json!({"pattern": "**/*.js"})).await.result.output, "No files match.");
    }

    #[tokio::test]
    async fn respects_gitignore_inside_a_git_repo() {
        let root = project("glob-git");
        git(root.path(), &["init", "-q"]);
        root.write(".gitignore", "src/ui/\n");
        assert_eq!(run(&root, json!({"pattern": "**/*.tsx"})).await.result.output, "No files match.");
    }

    #[tokio::test]
    async fn caps_the_list_and_says_how_many_more() {
        let root = TempDir::new("glob-many");
        for i in 0..205 {
            root.write(&format!("f{i:03}.txt"), "x");
        }
        let out = run(&root, json!({"pattern": "*.txt"})).await.result;
        assert!(out.output.ends_with("\n\n(5 more not shown; narrow the pattern.)"));
        assert_eq!(out.summary, "205 files");
    }

    #[tokio::test]
    async fn a_bad_pattern_is_an_error_the_model_can_read() {
        let root = project("glob-bad");
        let out = run(&root, json!({"pattern": "src/[a"})).await.result;
        assert!(out.is_error && out.output.contains("Invalid glob pattern"), "{}", out.output);
    }
}

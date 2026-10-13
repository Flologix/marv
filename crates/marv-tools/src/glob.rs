//! `glob`: find files in the project by name pattern.
//!
//! Ports Marv's `src/tools/glob.ts`.
//!
//! - The file list is `files::files_under` (git ls-files, so `.gitignore` applies; the walk outside a repo skips
//!   node_modules, dist, …). The pattern matches paths relative to `path` (`**/*.ts`, `src/*.tsx` only directly in
//!   src). Sorted; at most [`MAX_RESULTS`] = 200, then a "narrow the pattern" note.
//! - Port plan: `globset` for Bun.Glob's matching (check `**`, braces and dotfiles behave the same with tests).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

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
    fn label(&self, input: &GlobInput) -> String {
        match &input.path {
            Some(path) => format!("{} in {path}", input.pattern),
            None => input.pattern.clone(),
        }
    }
    fn repair(&self, raw: &Value, ctx: &ToolContext) -> Option<Repaired> {
        repair_path_field(raw, &ctx.root)
    }

    async fn run(&self, _input: &GlobInput, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        todo!("port: resolve dir (not a directory → error), files_under, globset match relative to base, cap at MAX_RESULTS, 'No files match.' / 'N files'")
    }
}

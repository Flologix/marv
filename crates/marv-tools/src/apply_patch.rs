//! `apply_patch` (planned, Marv #66): several file changes in one call, in the patch format models trained on
//! Codex-style tools already write.
//!
//! No Marv file yet; this module holds the plan so the shape is fixed before the code.
//!
//! Plan:
//! - Input: one `patch` string (`*** Begin Patch` … `*** End Patch`, with `*** Add File:`, `*** Update File:`
//!   (`@@` context hunks, `-`/`+`/` ` lines), `*** Delete File:`, `*** Move to:`).
//! - Parsed fully before anything is written; every file goes through `resolve_in_project`, `refuse_git` when
//!   confined, and the protected-path rules; any failure fails the whole call (nothing half-applied).
//! - Each hunk is located with `edit_match::apply_edit` (exact, trailing-whitespace, indentation tiers; CRLF kept), so
//!   it forgives exactly what `edit_file` forgives and reports the same closest-lines error.
//! - One preview with every file's diff; one approval (scope `files`, like the edit tools); `auto_safe` like
//!   `edit_file` for each path. Every existing file must pass `file_versions`' check like `edit_file`'s (read, and
//!   unchanged since), in the preview and again at apply time. Applied under `with_file_lock` per file, taken in the
//!   order of `file_lock::lock_key` (the real-path key both locks use, so no lock-order deadlocks).
//! - Results: one line per file plus syntax notes (`syntax::broken_by`), `diff` concatenated, `changed` per file
//!   (needs `ToolOutput::changed` to become a list, which marv-session's checks then iterate).
//! - Not registered in `builtin_tools` until it is built and has an eval.

/// One file operation in a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOp {
    Add { path: String, content: String },
    Update { path: String, move_to: Option<String>, hunks: Vec<Hunk> },
    Delete { path: String },
}

/// One `@@` hunk: the old text (context and removed lines) and the new text (context and added lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub context: Option<String>,
    pub old: String,
    pub new: String,
}

/// Parses a patch into operations, or says what's wrong with it (line number and expectation).
pub fn parse_patch(_patch: &str) -> Result<Vec<PatchOp>, String> {
    todo!("port: #66 apply_patch parser")
}

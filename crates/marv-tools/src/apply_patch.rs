//! `apply_patch` (Marv #66): several file changes in one call, in the patch format GPT models trained on (Codex's
//! `*** Begin Patch`). Spec: `docs/specs/2026-10-14-model-profiles.md` §3.3 and §5. No Marv (TypeScript) file: new.
//!
//! - **The parser** ([`parse_patch`]) is ported from OpenAI Codex's `codex-rs/apply-patch` (`parser.rs`,
//!   `streaming_parser.rs`; Apache-2.0, copyright OpenAI), with its leniencies: whitespace around the markers, a
//!   heredoc wrapper (`apply_patch <<'EOF'` … `EOF`, which models trained on Codex's shell form emit), a bare empty line
//!   in an Update as an empty context line, blank lines after `*** End of File`. marv-rs adds: a missing
//!   `*** End Patch` at the very end is accepted with a note (spec §3.3), and errors name the patch's line.
//! - **Hunks** ([`apply_hunks`]) follow Codex's semantics: per file a cursor starts at 0, an `@@ <line>` header
//!   moves it past the first line at or after it that matches, and the hunk's old lines (context and `-`) are the
//!   first match at or after the cursor, not a unique one (spec §9, decision 3). Matching is `edit_file`'s, through
//!   [`edit_match::find_from`]: exact, Unicode look-alikes, trailing whitespace, indentation (the added lines are
//!   re-indented), so a patch forgives what `edit_file` forgives. Codex's `seek_sequence` has the same tiers but
//!   folds fewer characters; Marv's are kept so the two edit tools agree. Like Codex, a hunk whose last old line is
//!   empty (the final newline written as a line) is tried again without it.
//!   - Spec note: matching is on whole lines (`find_from` takes lines, not a byte offset): a hunk is lines. And
//!     context lines keep the **file's** text (as Codex keeps them), so a forgiving match never rewrites a context
//!     line it only looked like: only `-` lines go and only `+` lines come.
//!   - A pure addition (no old lines) goes right after its `@@` line, or at the end with `*** End of File`; with
//!     neither it's an error asking for context (Codex appends it to the file: a guess Marv doesn't make).
//!   - Line endings: a file whose breaks are all CRLF keeps CRLF ([`edit_match::line_ending`]); a final newline is
//!     kept as it was (a new or empty file gets one).
//! - **All or nothing** ([`plan`]): the patch is parsed, every path resolved and checked, and every file's new text
//!   computed before anything is written; any failure fails the whole call. The tool plans in its preview and again
//!   under the locks ([`crate::file_lock::with_file_locks`], every source and destination, in key order).
//! - **Paths**: each goes through `resolve_in_project` (no way out of the project, symlinks included), `refuse_git`
//!   when confined, `require_regular_file` for an Update or Delete. An Add of an existing file and a Move onto an
//!   existing file are errors: a patch never overwrites by adding or moving. Every file is the target of at most one
//!   operation, a Move's source included, compared by [`target_key`] (the real path of the deepest existing folder
//!   plus the rest), so `a.ts`, `./src/../a.ts` and `link/a.ts` through a symlinked folder are one file whether or
//!   not it exists yet.
//! - **Stale files**: an Update, Delete or Move source must be as this session last read or wrote it
//!   ([`crate::file_versions`], `check_showing` like `edit_file`), in the plan, so in the preview and again under the
//!   locks.
//! - Input over [`MAX_PATCH_BYTES`] is refused before parsing.

use std::path::{Path, PathBuf};

use crate::edit_match::{Fuzzy, LineMatch, find_from, line_ending};
use crate::files::{is_directory, project_path, refuse_git, require_regular_file, resolve_in_project};
use crate::read_file::numbered_line;
use crate::types::{ToolContext, ToolError};

/// A patch over this is refused before it's parsed (spec §5: a guard against pathological input).
pub const MAX_PATCH_BYTES: usize = 1024 * 1024;

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";
const CONTEXT: &str = "@@ ";
const EMPTY_CONTEXT: &str = "@@";

const VALID_HEADERS: &str =
    "Valid headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}', '*** End Patch'";
const VALID_LINE: &str = "Every line of an update should start with ' ' (context line), '+' (added line) or '-' (removed line)";

/// One file operation in a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchOp {
    /// `content` is every `+` line with its newline.
    Add {
        path: String,
        content: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
    Delete {
        path: String,
    },
}

/// One hunk of an Update: an optional `@@ <line>` header, its lines, and whether it is anchored at the file's end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hunk {
    pub context: Option<String>,
    pub lines: Vec<HunkLine>,
    pub end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

impl Hunk {
    /// The lines the file has now: context and removed.
    pub fn old_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(t) | HunkLine::Remove(t) => Some(t.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect()
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// A parsed patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub ops: Vec<PatchOp>,
    /// The patch ended without `*** End Patch` (accepted, with a note: spec §3.3).
    pub missing_end: bool,
}

/// What's wrong with a patch: the 1-based line of the patch as given (0: the patch as a whole).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line == 0 {
            write!(f, "Invalid patch: {}", self.message)
        } else {
            write!(f, "Invalid patch at line {}: {}", self.line, self.message)
        }
    }
}

fn error(line: usize, message: impl Into<String>) -> PatchError {
    PatchError { line, message: message.into() }
}

/// The lines between a heredoc wrapper's first and last line, and how many lines precede them in the input (for
/// line numbers); the input's own lines when it isn't wrapped. Codex accepts `<<EOF`, `<<'EOF'` and `<<"EOF"`
/// (lenient mode); a leading `apply_patch` word is accepted too, as models write the whole shell command.
fn unwrap_heredoc<'a>(lines: &'a [&'a str]) -> (&'a [&'a str], usize) {
    if let [first, .., last] = lines
        && lines.len() >= 4
        && last.trim() == "EOF"
        && first.trim() != BEGIN_PATCH
    {
        let opener = first.trim().strip_prefix("apply_patch").unwrap_or(first.trim()).trim_start();
        if matches!(opener, "<<EOF" | "<<'EOF'" | "<<\"EOF\"") {
            return (&lines[1..lines.len() - 1], 1);
        }
    }
    (lines, 0)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Start,
    Headers,
    Add,
    Delete,
    Update { header_line: usize },
    Ended,
}

/// Parses a patch into operations, or says what's wrong with it (line number and expectation). Nothing is checked
/// against the files here: [`plan`] does that.
pub fn parse_patch(patch: &str) -> Result<Patch, PatchError> {
    // Leading blank lines are trimmed but counted, so line numbers are the patch's own.
    let skipped = patch.len() - patch.trim_start().len();
    let first_line = patch[..skipped].matches('\n').count() + 1;
    let all: Vec<&str> = patch.trim().split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect::<Vec<_>>();
    let (lines, wrapped) = unwrap_heredoc(&all);
    let offset = first_line + wrapped;

    let mut ops: Vec<PatchOp> = Vec::new();
    let mut mode = Mode::Start;
    for (i, &line) in lines.iter().enumerate() {
        let n = offset + i;
        let trimmed = line.trim();
        if mode == Mode::Start {
            if trimmed != BEGIN_PATCH {
                return Err(error(n, "the first line of the patch must be '*** Begin Patch'"));
            }
            mode = Mode::Headers;
            continue;
        }
        if mode == Mode::Ended {
            if trimmed.is_empty() {
                continue;
            }
            return Err(error(n, "nothing may follow '*** End Patch'"));
        }
        // A header (or the end) closes the operation before it; an Update must not be left empty.
        let header = if let Mode::Update { .. } = mode { line.trim_end() } else { trimmed };
        if header == END_PATCH
            || header.starts_with(ADD_FILE)
            || header.starts_with(DELETE_FILE)
            || header.starts_with(UPDATE_FILE)
        {
            if let Mode::Update { header_line } = mode {
                close_update(&ops, header_line, n)?;
            }
            if header == END_PATCH {
                mode = Mode::Ended;
            } else if let Some(path) = header.strip_prefix(ADD_FILE) {
                ops.push(PatchOp::Add { path: path_of(path, n)?, content: String::new() });
                mode = Mode::Add;
            } else if let Some(path) = header.strip_prefix(DELETE_FILE) {
                ops.push(PatchOp::Delete { path: path_of(path, n)? });
                mode = Mode::Delete;
            } else if let Some(path) = header.strip_prefix(UPDATE_FILE) {
                ops.push(PatchOp::Update { path: path_of(path, n)?, move_to: None, hunks: Vec::new() });
                mode = Mode::Update { header_line: n };
            }
            continue;
        }
        match mode {
            Mode::Headers | Mode::Delete => {
                return Err(error(n, format!("'{trimmed}' is not a valid header. {VALID_HEADERS}")));
            }
            Mode::Add => {
                let Some(added) = line.strip_prefix('+') else {
                    return Err(error(
                        n,
                        format!("every line of an added file must start with '+', got '{trimmed}'. {VALID_HEADERS}"),
                    ));
                };
                if let Some(PatchOp::Add { content, .. }) = ops.last_mut() {
                    content.push_str(added);
                    content.push('\n');
                }
            }
            Mode::Update { .. } => {
                let Some(PatchOp::Update { move_to, hunks, .. }) = ops.last_mut() else { unreachable!() };
                update_line(line, n, move_to, hunks)?;
            }
            Mode::Start | Mode::Ended => unreachable!(),
        }
    }
    match mode {
        Mode::Start => Err(error(0, "the first line of the patch must be '*** Begin Patch'")),
        Mode::Ended if ops.is_empty() => Err(error(0, "the patch has no operations")),
        Mode::Ended => Ok(Patch { ops, missing_end: false }),
        _ => {
            if let Mode::Update { header_line } = mode {
                close_update(&ops, header_line, offset + lines.len())?;
            }
            if ops.is_empty() {
                return Err(error(0, "the patch has no operations"));
            }
            Ok(Patch { ops, missing_end: true })
        }
    }
}

/// A header's path, which must not be empty.
fn path_of(path: &str, n: usize) -> Result<String, PatchError> {
    let path = path.trim();
    if path.is_empty() { Err(error(n, "the path is missing")) } else { Ok(path.to_string()) }
}

/// An Update that ends (at line `n`, a header, the end, or the patch's end) must have a hunk, and its last hunk lines.
fn close_update(ops: &[PatchOp], header_line: usize, n: usize) -> Result<(), PatchError> {
    if let Some(PatchOp::Update { path, hunks, .. }) = ops.last() {
        if hunks.is_empty() {
            return Err(error(header_line, format!("the update of '{path}' is empty")));
        }
        if hunks.last().is_some_and(Hunk::is_empty) {
            return Err(error(n, "an update hunk has no lines"));
        }
    }
    Ok(())
}

/// One line inside an Update (Codex's `process_line` for `UpdateFile`).
fn update_line(line: &str, n: usize, move_to: &mut Option<String>, hunks: &mut Vec<Hunk>) -> Result<(), PatchError> {
    let marker = line.trim_end();
    if hunks.last().is_some_and(|h| h.end_of_file) {
        if marker.is_empty() {
            return Ok(());
        }
        if marker != EMPTY_CONTEXT && !marker.starts_with(CONTEXT) {
            return Err(error(n, format!("after '*** End of File' a new hunk must start with @@, got '{line}'")));
        }
    }
    if hunks.is_empty()
        && move_to.is_none()
        && let Some(to) = marker.strip_prefix(MOVE_TO)
    {
        *move_to = Some(path_of(to, n)?);
        return Ok(());
    }
    if marker == EMPTY_CONTEXT || marker.starts_with(CONTEXT) {
        if hunks.last().is_some_and(Hunk::is_empty) {
            return Err(error(n, format!("unexpected line in an update: '{line}'. {VALID_LINE}")));
        }
        let context = marker.strip_prefix(CONTEXT).map(str::to_string);
        hunks.push(Hunk { context, ..Hunk::default() });
        return Ok(());
    }
    if marker == END_OF_FILE {
        match hunks.last_mut() {
            Some(hunk) if !hunk.is_empty() => hunk.end_of_file = true,
            _ => return Err(error(n, "'*** End of File' must follow a hunk's lines")),
        }
        return Ok(());
    }
    if let Some(unknown) = marker.strip_prefix("*** ") {
        return Err(error(n, format!("unknown marker '*** {unknown}' in an update")));
    }
    let hunk_line = if line.is_empty() {
        HunkLine::Context(String::new())
    } else if let Some(text) = line.strip_prefix(' ') {
        HunkLine::Context(text.to_string())
    } else if let Some(text) = line.strip_prefix('+') {
        HunkLine::Add(text.to_string())
    } else if let Some(text) = line.strip_prefix('-') {
        HunkLine::Remove(text.to_string())
    } else if hunks.last().is_some_and(|h| !h.is_empty()) {
        return Err(error(n, format!("'{line}' doesn't start with ' ', '+' or '-'; a new hunk starts with @@")));
    } else {
        return Err(error(n, format!("unexpected line in an update: '{line}'. {VALID_LINE}")));
    };
    if hunks.is_empty() {
        hunks.push(Hunk::default());
    }
    hunks.last_mut().expect("just pushed").lines.push(hunk_line);
    Ok(())
}

const MAX_CLOSEST_LINES: usize = 30;

/// A file's text after its hunks, and the forgiving matches that applied; or why a hunk didn't apply (the message
/// names `shown` and the hunk's number, with the file's closest lines when it wasn't found).
pub fn apply_hunks(shown: &str, before: &str, hunks: &[Hunk]) -> Result<(String, Vec<Fuzzy>), String> {
    let eol = line_ending(before);
    let mut lines: Vec<&str> = if before.is_empty() { Vec::new() } else { before.split(eol).collect() };
    let final_newline = before.is_empty() || before.ends_with('\n');
    if final_newline && lines.last() == Some(&"") {
        lines.pop();
    }
    let mut cursor = 0;
    let mut fuzzies = Vec::new();
    // (first line, how many lines go, what comes instead), in file order: the cursor only moves forward.
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    for (i, hunk) in hunks.iter().enumerate() {
        let number = i + 1;
        if let Some(context) = &hunk.context {
            let Some(found) = find_from(&lines, &[context.as_str()], cursor) else {
                let earlier = find_from(&lines, &[context.as_str()], 0).is_some();
                let why = if earlier { ", only before the previous hunk (hunks must be in file order)" } else { "" };
                return Err(format!("Hunk {number} of {shown}: the @@ line \"{context}\" wasn't found{why}."));
            };
            fuzzies.extend(found.fuzzy);
            cursor = found.start + 1;
        }
        let mut old = hunk.old_lines();
        let adds = || hunk.lines.iter().filter_map(|l| if let HunkLine::Add(t) = l { Some(t.clone()) } else { None });
        if old.is_empty() {
            let at = if hunk.context.is_some() {
                cursor
            } else if hunk.end_of_file {
                lines.len()
            } else {
                return Err(format!(
                    "Hunk {number} of {shown} only adds lines and says nowhere where: give it an @@ line or context lines from the file, or end it with *** End of File to add at the end."
                ));
            };
            replacements.push((at, 0, adds().collect()));
            cursor = at;
            continue;
        }
        let mut lines_of_hunk: Vec<&HunkLine> = hunk.lines.iter().collect();
        let mut found = locate(&lines, &old, cursor, hunk.end_of_file);
        // Codex: the file's final newline written as an empty last line; try again without it (on both sides).
        if found.is_none() && old.last() == Some(&"") {
            old.pop();
            if let Some(at) = lines_of_hunk.iter().rposition(|l| !matches!(l, HunkLine::Add(_))) {
                lines_of_hunk.remove(at);
            }
            // And on the new side, when it ends the same way.
            if let Some(at) = lines_of_hunk.iter().rposition(|l| !matches!(l, HunkLine::Remove(_)))
                && lines_of_hunk[at] == &HunkLine::Add(String::new())
            {
                lines_of_hunk.remove(at);
            }
            found = locate(&lines, &old, cursor, hunk.end_of_file);
        }
        let Some(found) = found else {
            return Err(not_found(shown, number, before, &old, &lines, cursor));
        };
        fuzzies.extend(found.fuzzy);
        let mut block = Vec::new();
        let mut j = 0;
        for line in lines_of_hunk {
            match line {
                HunkLine::Context(_) => {
                    block.push(lines[found.start + j].to_string());
                    j += 1;
                }
                HunkLine::Remove(_) => j += 1,
                HunkLine::Add(text) => match &found.reindent {
                    Some(reindent) => block.push(reindent(text, block.len())),
                    None => block.push(text.clone()),
                },
            }
        }
        replacements.push((found.start, old.len(), block));
        cursor = found.start + old.len();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut at = 0;
    for (start, len, block) in replacements {
        out.extend(lines[at..start].iter().map(|l| l.to_string()));
        out.extend(block);
        at = start + len;
    }
    out.extend(lines[at..].iter().map(|l| l.to_string()));
    let mut after = out.join(eol);
    if final_newline && !out.is_empty() {
        after.push_str(eol);
    }
    fuzzies.sort_by_key(|f| *f as u8);
    fuzzies.dedup();
    Ok((after, fuzzies))
}

/// A hunk's old lines at or after `cursor`; with `*** End of File`, at the file's end first (Codex's `eof`).
fn locate(lines: &[&str], old: &[&str], cursor: usize, end_of_file: bool) -> Option<LineMatch> {
    if end_of_file && lines.len() >= old.len() && lines.len() - old.len() >= cursor {
        let at_end = find_from(lines, old, lines.len() - old.len());
        if at_end.is_some() {
            return at_end;
        }
    }
    find_from(lines, old, cursor)
}

/// The not-found error, with the file's closest lines as they are now (`edit_file::not_found`'s shape).
fn not_found(shown: &str, number: usize, text: &str, old: &[&str], lines: &[&str], cursor: usize) -> String {
    let joined = old.join("\n");
    let earlier = find_from(lines, old, 0).is_some_and(|m| m.start < cursor);
    if earlier {
        return format!("Hunk {number} of {shown}: its lines are only before the previous hunk. Hunks must be in file order.");
    }
    let head = format!(
        "Hunk {number} of {shown}: its context and removed lines weren't found, not even ignoring differences in whitespace and indentation."
    );
    let Some((start, end)) = crate::edit_match::closest_lines(text, &joined) else {
        return format!("{head} They must match the file. Use read_file to see the current text, then try again.");
    };
    let last = end.min(start + MAX_CLOSEST_LINES - 1);
    let shown_lines: Vec<String> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .skip(start - 1)
        .take(last - start + 1)
        .enumerate()
        .map(|(i, line)| numbered_line(start + i, line))
        .collect();
    format!(
        "{head} The closest text is lines {start}-{end}, as it is now:\n{}\nIf that's the part you meant, copy the hunk's lines from these exactly (what follows each →). Otherwise use read_file to find it.",
        shown_lines.join("\n")
    )
}

/// What a file of the patch becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Add,
    Update,
    Delete,
    /// An Update with `*** Move to:` (its hunks may be empty of changes: a plain rename).
    Move,
}

/// One file of a planned patch.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub kind: ChangeKind,
    /// The path as the patch wrote it, resolved, and as the transcript shows it (project-relative).
    pub path: String,
    pub absolute: PathBuf,
    pub shown: String,
    /// A Move's destination: as written, resolved, shown.
    pub dest: Option<(String, PathBuf, String)>,
    /// The file as it is now (None: added).
    pub before: Option<String>,
    /// What gets written (None: deleted). At the destination for a Move.
    pub after: Option<String>,
    pub fuzzy: Vec<Fuzzy>,
}

impl FileChange {
    /// Where the result lands: the destination of a Move, else the path.
    pub fn target(&self) -> (&str, &Path, &str) {
        match &self.dest {
            Some((path, absolute, shown)) => (path, absolute, shown),
            None => (&self.path, &self.absolute, &self.shown),
        }
    }
}

/// A patch, planned: every file's change, computed; nothing written.
#[derive(Debug, Clone)]
pub struct PatchPlan {
    pub files: Vec<FileChange>,
    pub missing_end: bool,
}

impl PatchPlan {
    /// Every path a lock must cover, as written: sources and destinations.
    pub fn lock_paths(&self) -> Vec<&str> {
        self.files.iter().flat_map(|f| std::iter::once(f.path.as_str()).chain(f.dest.as_ref().map(|d| d.0.as_str()))).collect()
    }
}

/// The key two paths of a patch are the same file by: the real path of the deepest folder of it that exists, then the
/// rest as written (spec §3.3), so a file that doesn't exist yet is caught through a symlinked folder too. `absolute`
/// is `resolve_in_project`'s (normalized, inside the project). Case-folded where names are case-insensitive, like
/// `file_lock::lock_key`.
pub fn target_key(absolute: &Path) -> PathBuf {
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut at = absolute.to_path_buf();
    let real = loop {
        if let Ok(real) = std::fs::canonicalize(&at) {
            break real;
        }
        match (at.file_name().map(|n| n.to_os_string()), at.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                at = parent.to_path_buf();
            }
            _ => break at,
        }
    };
    let mut key = real;
    for name in rest.into_iter().rev() {
        key.push(name);
    }
    if cfg!(any(target_os = "macos", windows)) { PathBuf::from(key.to_string_lossy().to_lowercase()) } else { key }
}

/// A path of the patch, resolved for writing: inside the project, outside `.git` when confined, not a folder.
fn resolve_target(ctx: &ToolContext, path: &str) -> Result<(PathBuf, String), ToolError> {
    let absolute = resolve_in_project(ctx, path)?;
    if ctx.confined {
        refuse_git(&ctx.root, &absolute)?;
    }
    let shown = project_path(&ctx.root, &absolute);
    if is_directory(&absolute) {
        return Err(ToolError(format!("\"{shown}\" is a directory.")));
    }
    Ok((absolute, shown))
}

async fn read_text(absolute: &Path, shown: &str) -> Result<String, ToolError> {
    tokio::fs::read_to_string(absolute).await.map_err(|e| ToolError(format!("Couldn't read {shown}: {e}")))
}

/// The patch parsed and every file's change computed, or a ToolError the model can act on, with nothing written. Used
/// for the preview and again under the locks when applying.
pub async fn plan(patch: &str, ctx: &ToolContext) -> Result<PatchPlan, ToolError> {
    if patch.len() > MAX_PATCH_BYTES {
        return Err(ToolError(format!(
            "The patch is {} bytes; the limit is {MAX_PATCH_BYTES}. Split it into several apply_patch calls.",
            patch.len()
        )));
    }
    let parsed = parse_patch(patch).map_err(|e| ToolError(e.to_string()))?;
    // Every file a patch touches (a Move's source and destination both), by target_key: at most one operation each.
    // Checked first, so two operations on one file are called that, whatever else is wrong with them.
    let mut touched: Vec<(PathBuf, String)> = Vec::new();
    for path in parsed.ops.iter().flat_map(|op| match op {
        PatchOp::Add { path, .. } | PatchOp::Delete { path } => vec![path],
        PatchOp::Update { path, move_to, .. } => std::iter::once(path).chain(move_to).collect(),
    }) {
        let (absolute, shown) = resolve_target(ctx, path)?;
        let key = target_key(&absolute);
        if let Some((_, first)) = touched.iter().find(|(k, _)| *k == key) {
            let same = if *first == shown { String::new() } else { format!(" (as {first})") };
            return Err(ToolError(format!(
                "{shown} is in more than one operation of the patch{same}. Each file may be added, updated, moved or deleted once per patch: combine the changes."
            )));
        }
        touched.push((key, shown));
    }
    let mut files = Vec::with_capacity(parsed.ops.len());
    for op in &parsed.ops {
        let change = match op {
            PatchOp::Add { path, content } => {
                let (absolute, shown) = resolve_target(ctx, path)?;
                if absolute.exists() || std::fs::symlink_metadata(&absolute).is_ok() {
                    return Err(ToolError(format!(
                        "*** Add File: {shown} already exists. Use *** Update File to change it (read it first)."
                    )));
                }
                FileChange {
                    kind: ChangeKind::Add,
                    path: path.clone(),
                    absolute,
                    shown,
                    dest: None,
                    before: None,
                    after: Some(content.clone()),
                    fuzzy: Vec::new(),
                }
            }
            PatchOp::Delete { path } => {
                let (absolute, shown) = resolve_target(ctx, path)?;
                require_regular_file(&absolute, &shown)?;
                let before = read_text(&absolute, &shown).await?;
                ctx.file_versions.check_showing(&absolute, &shown, &before, None)?;
                FileChange {
                    kind: ChangeKind::Delete,
                    path: path.clone(),
                    absolute,
                    shown,
                    dest: None,
                    before: Some(before),
                    after: None,
                    fuzzy: Vec::new(),
                }
            }
            PatchOp::Update { path, move_to, hunks } => {
                let (absolute, shown) = resolve_target(ctx, path)?;
                if !absolute.exists() {
                    let hint =
                        if absolute.parent().is_some_and(is_directory) { " Use *** Add File to create a new file." } else { "" };
                    return Err(ToolError(format!("*** Update File: {shown} doesn't exist.{hint}")));
                }
                require_regular_file(&absolute, &shown)?;
                let before = read_text(&absolute, &shown).await?;
                let near = hunks.iter().map(|h| h.old_lines().join("\n")).find(|s| !s.trim().is_empty());
                ctx.file_versions.check_showing(&absolute, &shown, &before, near.as_deref())?;
                let (after, fuzzy) = apply_hunks(&shown, &before, hunks).map_err(ToolError)?;
                let dest = match move_to {
                    Some(to) => {
                        let (dest_abs, dest_shown) = resolve_target(ctx, to)?;
                        if dest_abs.exists() || std::fs::symlink_metadata(&dest_abs).is_ok() {
                            return Err(ToolError(format!(
                                "*** Move to: {dest_shown} already exists; delete it first or update it instead."
                            )));
                        }
                        Some((to.clone(), dest_abs, dest_shown))
                    }
                    None => None,
                };
                FileChange {
                    kind: if dest.is_some() { ChangeKind::Move } else { ChangeKind::Update },
                    path: path.clone(),
                    absolute,
                    shown,
                    dest,
                    before: Some(before),
                    after: Some(after),
                    fuzzy,
                }
            }
        };
        files.push(change);
    }
    Ok(PatchPlan { files, missing_end: parsed.missing_end })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn ops(patch: &str) -> Vec<PatchOp> {
        parse_patch(patch).unwrap_or_else(|e| panic!("{e}")).ops
    }
    fn parse_error(patch: &str) -> PatchError {
        parse_patch(patch).expect_err("a bad patch")
    }
    fn update(path: &str, hunks: Vec<Hunk>) -> PatchOp {
        PatchOp::Update { path: path.into(), move_to: None, hunks }
    }
    fn ctx_line(t: &str) -> HunkLine {
        HunkLine::Context(t.into())
    }
    fn add(t: &str) -> HunkLine {
        HunkLine::Add(t.into())
    }
    fn del(t: &str) -> HunkLine {
        HunkLine::Remove(t.into())
    }

    #[test]
    fn every_operation_parses() {
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+one\n+two\n*** Delete File: gone.txt\n*** Update File: a.ts\n*** Move to: b.ts\n@@ fn main\n ctx\n-old\n+new\n*** End Patch\n";
        assert_eq!(
            ops(patch),
            vec![
                PatchOp::Add { path: "new.txt".into(), content: "one\ntwo\n".into() },
                PatchOp::Delete { path: "gone.txt".into() },
                PatchOp::Update {
                    path: "a.ts".into(),
                    move_to: Some("b.ts".into()),
                    hunks: vec![Hunk {
                        context: Some("fn main".into()),
                        lines: vec![ctx_line("ctx"), del("old"), add("new")],
                        end_of_file: false
                    }],
                },
            ]
        );
    }

    #[test]
    fn several_hunks_headers_end_of_file_and_blank_context_lines() {
        let patch = "*** Begin Patch\n*** Update File: a\n@@\n-x\n+y\n\n@@ second\n a\n-b\n*** End of File\n*** End Patch";
        assert_eq!(
            ops(patch),
            vec![update(
                "a",
                vec![
                    Hunk { context: None, lines: vec![del("x"), add("y"), ctx_line("")], end_of_file: false },
                    Hunk { context: Some("second".into()), lines: vec![ctx_line("a"), del("b")], end_of_file: true },
                ]
            )]
        );
    }

    #[test]
    fn whitespace_a_heredoc_wrapper_and_crlf_are_forgiven() {
        let plain = "*** Begin Patch\n*** Delete File: a\n*** End Patch";
        assert_eq!(ops(&format!("\n\n  {plain}  \n\n")), vec![PatchOp::Delete { path: "a".into() }]);
        for opener in ["<<EOF", "<<'EOF'", "<<\"EOF\"", "apply_patch <<'EOF'"] {
            assert_eq!(ops(&format!("{opener}\n{plain}\nEOF\n")), vec![PatchOp::Delete { path: "a".into() }], "{opener}");
        }
        assert_eq!(ops(&plain.replace('\n', "\r\n")), vec![PatchOp::Delete { path: "a".into() }]);
    }

    #[test]
    fn a_missing_end_patch_at_the_very_end_is_accepted_with_a_note() {
        let parsed = parse_patch("*** Begin Patch\n*** Update File: a\n-x\n+y\n").unwrap();
        assert!(parsed.missing_end);
        assert_eq!(parsed.ops.len(), 1);
        assert!(!parse_patch("*** Begin Patch\n*** Delete File: a\n*** End Patch").unwrap().missing_end);
    }

    #[test]
    fn errors_name_the_line() {
        assert_eq!(parse_error("hello\n*** End Patch").line, 1);
        // Line numbers are the patch's own, blank lines before it and a wrapper included.
        let e = parse_error("\n*** Begin Patch\n*** Frobnicate: a\n*** End Patch");
        assert_eq!(e.line, 3);
        assert!(e.message.contains("not a valid header"), "{}", e.message);
        let e = parse_error("*** Begin Patch\n*** Add File: a\nno plus\n*** End Patch");
        assert_eq!(e.line, 3);
        assert!(e.message.contains("must start with '+'"), "{}", e.message);
        let e = parse_error("*** Begin Patch\n*** Update File: a\n*** End Patch");
        assert_eq!((e.line, e.message.as_str()), (2, "the update of 'a' is empty"));
        let e = parse_error("*** Begin Patch\n*** Update File: a\n-x\nbad\n*** End Patch");
        assert_eq!(e.line, 4);
        let e = parse_error("*** Begin Patch\n*** Update File: a\n-x\n*** Nope\n*** End Patch");
        assert!(e.message.contains("unknown marker"), "{}", e.message);
        let e = parse_error("*** Begin Patch\n*** Delete File: a\n*** End Patch\nmore");
        assert_eq!(e.line, 4);
        assert_eq!(parse_error("*** Begin Patch\n*** Delete File: \n*** End Patch").line, 2);
        assert_eq!(parse_error("*** Begin Patch\n*** End Patch").message, "the patch has no operations");
    }

    fn hunk(context: Option<&str>, lines: Vec<HunkLine>) -> Hunk {
        Hunk { context: context.map(String::from), lines, end_of_file: false }
    }

    #[test]
    fn hunks_take_the_first_match_after_the_cursor_and_the_header_narrows() {
        let before = "fn a() {\n    x();\n}\nfn b() {\n    x();\n}\n";
        let (after, fuzzy) = apply_hunks("f", before, &[hunk(Some("fn b() {"), vec![del("    x();"), add("    y();")])]).unwrap();
        assert_eq!(after, "fn a() {\n    x();\n}\nfn b() {\n    y();\n}\n");
        assert!(fuzzy.is_empty());
        let (after, _) = apply_hunks(
            "f",
            before,
            &[hunk(None, vec![del("    x();"), add("    1();")]), hunk(None, vec![del("    x();"), add("    2();")])],
        )
        .unwrap();
        assert_eq!(after, "fn a() {\n    1();\n}\nfn b() {\n    2();\n}\n");
    }

    #[test]
    fn a_hunk_before_the_previous_one_is_an_error() {
        let before = "a\nb\nc\n";
        let err = apply_hunks("f", before, &[hunk(None, vec![del("c")]), hunk(None, vec![del("a")])]).unwrap_err();
        assert!(err.contains("Hunk 2 of f") && err.contains("file order"), "{err}");
    }

    #[test]
    fn crlf_is_kept_and_context_lines_keep_the_files_text() {
        let before = "one  \r\ntwo\r\nthree\r\n";
        // Trailing whitespace forgiven on the context line, which stays as the file has it.
        let (after, fuzzy) = apply_hunks("f", before, &[hunk(None, vec![ctx_line("one"), del("two"), add("2")])]).unwrap();
        assert_eq!(after, "one  \r\n2\r\nthree\r\n");
        assert_eq!(fuzzy, [Fuzzy::TrailingWhitespace]);
    }

    #[test]
    fn indentation_is_reflowed_and_unicode_forgiven() {
        let before = "\tif x {\n\t\ty();\n\t}\n";
        let (after, fuzzy) =
            apply_hunks("f", before, &[hunk(None, vec![ctx_line("    if x {"), del("        y();"), add("        z();")])])
                .unwrap();
        assert_eq!(after, "\tif x {\n\t\tz();\n\t}\n");
        assert_eq!(fuzzy, [Fuzzy::Indentation]);
        let (after, fuzzy) =
            apply_hunks("f", "say(\u{201C}hi\u{201D});\n", &[hunk(None, vec![del("say(\"hi\");"), add("say(\"yo\");")])])
                .unwrap();
        assert_eq!(after, "say(\"yo\");\n");
        assert_eq!(fuzzy, [Fuzzy::Unicode]);
    }

    #[test]
    fn pure_additions_need_a_place() {
        let before = "a\nb\n";
        let (after, _) = apply_hunks("f", before, &[hunk(Some("a"), vec![add("inserted")])]).unwrap();
        assert_eq!(after, "a\ninserted\nb\n");
        let at_end = Hunk { context: None, lines: vec![add("z")], end_of_file: true };
        assert_eq!(apply_hunks("f", before, &[at_end]).unwrap().0, "a\nb\nz\n");
        let err = apply_hunks("f", before, &[hunk(None, vec![add("z")])]).unwrap_err();
        assert!(err.contains("only adds lines"), "{err}");
    }

    #[test]
    fn end_of_file_anchors_at_the_end_and_the_final_newline_is_kept_as_it_was() {
        let before = "x\ny\nx\n";
        let tail = Hunk { context: None, lines: vec![del("x"), add("last")], end_of_file: true };
        assert_eq!(apply_hunks("f", before, &[tail]).unwrap().0, "x\ny\nlast\n");
        assert_eq!(apply_hunks("f", "a\nb", &[hunk(None, vec![del("b"), add("c")])]).unwrap().0, "a\nc");
        // The final newline written as an empty old line (Codex's retry).
        assert_eq!(apply_hunks("f", "a\nb\n", &[hunk(None, vec![del("b"), del(""), add("c"), add("")])]).unwrap().0, "a\nc\n");
    }

    #[test]
    fn not_found_shows_the_closest_lines() {
        let before = "function greet(name) {\n  return `Hello, ${name}!`;\n}\n";
        let err = apply_hunks("greet.js", before, &[hunk(None, vec![del("function greet(person) {"), add("x")])]).unwrap_err();
        assert!(err.starts_with("Hunk 1 of greet.js: its context and removed lines weren't found"), "{err}");
        assert!(err.contains("    1→function greet(name) {"), "{err}");
        let err = apply_hunks("f", "a\n", &[hunk(Some("nope"), vec![del("a")])]).unwrap_err();
        assert!(err.contains("the @@ line \"nope\" wasn't found"), "{err}");
    }

    async fn plan_in(root: &TempDir, patch: &str) -> Result<PatchPlan, ToolError> {
        let ctx = ToolContext::new(root.path());
        for entry in walk(root.path()) {
            ctx.file_versions.saw(&entry, &std::fs::read(&entry).unwrap());
        }
        plan(patch, &ctx).await
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                out.extend(walk(&path));
            } else if path.is_file() {
                out.push(path);
            }
        }
        out
    }

    fn patch_of(body: &str) -> String {
        format!("*** Begin Patch\n{body}\n*** End Patch")
    }

    #[tokio::test]
    async fn a_plan_computes_every_file_and_a_bad_hunk_anywhere_fails_it_all() {
        let root = TempDir::new("patch-plan");
        root.write("a.txt", "a\n");
        root.write("b.txt", "b\n");
        root.write("c.txt", "c\n");
        let good = patch_of("*** Update File: a.txt\n-a\n+A\n*** Add File: d.txt\n+d\n*** Delete File: c.txt");
        let planned = plan_in(&root, &good).await.unwrap();
        let kinds: Vec<_> = planned.files.iter().map(|f| (f.kind, f.after.clone())).collect();
        assert_eq!(
            kinds,
            [(ChangeKind::Update, Some("A\n".into())), (ChangeKind::Add, Some("d\n".into())), (ChangeKind::Delete, None)]
        );
        let bad = patch_of("*** Update File: a.txt\n-a\n+A\n*** Update File: b.txt\n-nope\n+B\n*** Delete File: c.txt");
        let err = plan_in(&root, &bad).await.unwrap_err();
        assert!(err.0.contains("Hunk 1 of b.txt"), "{}", err.0);
        assert_eq!(std::fs::read_to_string(root.path().join("a.txt")).unwrap(), "a\n");
    }

    #[tokio::test]
    async fn adds_and_moves_never_overwrite_and_updates_need_a_file() {
        let root = TempDir::new("patch-exists");
        root.write("a.txt", "a\n");
        root.write("b.txt", "b\n");
        let err = plan_in(&root, &patch_of("*** Add File: a.txt\n+x")).await.unwrap_err();
        assert!(err.0.contains("already exists. Use *** Update File"), "{}", err.0);
        let err = plan_in(&root, &patch_of("*** Update File: a.txt\n*** Move to: b.txt\n-a\n+A")).await.unwrap_err();
        assert_eq!(err.0, "*** Move to: b.txt already exists; delete it first or update it instead.");
        let err = plan_in(&root, &patch_of("*** Update File: nope.txt\n-a\n+A")).await.unwrap_err();
        assert!(err.0.contains("doesn't exist. Use *** Add File"), "{}", err.0);
        let err = plan_in(&root, &patch_of("*** Delete File: nope.txt")).await.unwrap_err();
        assert!(err.0.contains("doesn't exist"), "{}", err.0);
    }

    #[tokio::test]
    async fn a_file_is_in_one_operation_only_whatever_its_spelling() {
        let root = TempDir::new("patch-dupes");
        root.write("a.txt", "a\n");
        root.write("src/s.ts", "s\n");
        std::os::unix::fs::symlink(root.path().join("src"), root.path().join("link")).unwrap();
        for body in [
            "*** Update File: a.txt\n*** Move to: b.txt\n-a\n+A\n*** Add File: b.txt\n+b",
            "*** Update File: a.txt\n*** Move to: b.txt\n-a\n+A\n*** Update File: src/s.ts\n*** Move to: b.txt\n-s\n+S",
            "*** Update File: a.txt\n*** Move to: src/s.ts\n-a\n+A\n*** Delete File: src/s.ts",
            "*** Update File: a.txt\n-a\n+A\n*** Update File: ./a.txt\n-a\n+B",
            "*** Update File: src/s.ts\n-s\n+S\n*** Update File: link/s.ts\n-s\n+T",
            "*** Add File: src/new.ts\n+n\n*** Add File: link/new.ts\n+m",
            "*** Update File: src/s.ts\n*** Move to: link/b.ts\n-s\n+S\n*** Add File: src/b.ts\n+b",
            "*** Add File: deep/er/x.ts\n+1\n*** Add File: deep/er/../er/x.ts\n+2",
        ] {
            let err = plan_in(&root, &patch_of(body)).await.unwrap_err();
            assert!(err.0.contains("in more than one operation of the patch"), "{body}: {}", err.0);
        }
    }

    #[tokio::test]
    async fn stale_and_unread_files_are_refused() {
        let root = TempDir::new("patch-stale");
        root.write("a.txt", "a\n");
        let ctx = ToolContext::new(root.path());
        let err = plan(&patch_of("*** Update File: a.txt\n-a\n+A"), &ctx).await.unwrap_err();
        assert!(err.0.contains("a.txt"), "{}", err.0);
        // check_showing recorded what it showed: the same patch now plans.
        assert!(plan(&patch_of("*** Update File: a.txt\n-a\n+A"), &ctx).await.is_ok());
        std::fs::write(root.path().join("a.txt"), "changed\n").unwrap();
        assert!(plan(&patch_of("*** Update File: a.txt\n-changed\n+A"), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn no_path_leads_out_of_the_project() {
        let root = TempDir::new("patch-out");
        let outside = TempDir::new("patch-outside");
        outside.write("x.txt", "x\n");
        root.write("a.txt", "a\n");
        std::os::unix::fs::symlink(outside.path(), root.path().join("out")).unwrap();
        for body in [
            "*** Add File: ../escape.txt\n+x".to_string(),
            format!("*** Add File: {}/new.txt\n+x", outside.path().display()),
            "*** Update File: out/x.txt\n-x\n+y".to_string(),
            "*** Update File: a.txt\n*** Move to: ../moved.txt\n-a\n+A".to_string(),
            "*** Delete File: out/x.txt".to_string(),
        ] {
            assert!(plan_in(&root, &patch_of(&body)).await.is_err(), "{body}");
        }
    }

    #[tokio::test]
    async fn an_oversized_patch_is_refused_before_parsing() {
        let root = TempDir::new("patch-big");
        let err = plan_in(&root, &"x".repeat(MAX_PATCH_BYTES + 1)).await.unwrap_err();
        assert!(err.0.contains("the limit is"), "{}", err.0);
    }
}

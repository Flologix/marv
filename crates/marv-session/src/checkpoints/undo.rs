//! /undo (Marv #16): what a turn changed, between its before and after checkpoints, and putting those paths back.
//! Ports Marv's `src/checkpoints/undo.ts`.
//!
//! Marv writes each file itself rather than through `git checkout`, which would run the repository's checkout
//! filters and follow symlinks; every path is checked to stay inside the project as it is now.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use marv_tools::diff::{added_lines, diff_text, shown_diff};
use marv_tools::{ApprovalRequest, DiffShown, Preview, PreviewFile, Scope};

use super::store::{Checkpoint, CheckpointKind, CheckpointStore, SnapshotOptions, SnapshotResult};

/// What /undo does to one path: put the old content back, delete a file the turn created, or recreate one it deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoAction {
    Restore,
    Delete,
    Recreate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoFile {
    /// Relative to the project root.
    pub path: String,
    pub action: UndoAction,
    /// The file isn't as the turn left it (the user edited it since): it's restored anyway, the pre-undo snapshot
    /// keeps the edit.
    pub changed_since: bool,
    /// The change undoing makes (now → before), for the preview; None for a binary or very large file.
    pub diff: Option<DiffShown>,
    pub binary: bool,
}

/// A path's git mode and content (a symlink's target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub mode: String,
    pub content: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoPlan {
    pub turn: u32,
    /// The turn's request ("add the parser").
    pub label: String,
    pub before: Checkpoint,
    pub after: Checkpoint,
    pub files: Vec<UndoFile>,
    /// Paths the turn's tools reported changing that no snapshot holds (git ignores them): left as they are.
    pub not_restored: Vec<String>,
    /// The chain's newest checkpoint when the plan was made: if it moved, the plan is stale.
    pub head: String,
    /// What each path goes back to: its mode and content in the before snapshot (None: delete it).
    pub targets: HashMap<String, Option<Target>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UndoResult {
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    pub recreated: Vec<String>,
    /// Paths left alone, and why (a parent that now leads outside the project).
    pub refused: Vec<Refused>,
    /// The snapshot taken first, so the undo itself can be undone by hand, and how long it took.
    pub pre_undo: String,
    pub pre_undo_ms: u64,
    pub pre_undo_files: usize,
    /// Why the `undo` marker wasn't written (the turn is kept as undone in memory, for this session only).
    pub marker_failed: Option<String>,
}

/// Why there's no plan: nothing to undo, or (`failed`) the checkpoints couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoRefusal {
    pub reason: String,
    pub failed: bool,
}

impl UndoRefusal {
    fn failed(reason: impl Into<String>) -> Self {
        Self { reason: reason.into(), failed: true }
    }
}

/// Larger files are named in the preview, not diffed.
const MAX_DIFFED_BYTES: usize = 1024 * 1024;

/// The newest turn that has a pair and hasn't been undone (by a marker on the chain, or in `also_undone`): (before, after).
pub fn next_undoable(chain: &[Checkpoint], also_undone: &HashSet<u32>) -> Option<(Checkpoint, Checkpoint)> {
    let undone: HashSet<u32> =
        chain.iter().filter(|c| c.kind == CheckpointKind::Undo).map(|c| c.turn).chain(also_undone.iter().copied()).collect();
    for (i, after) in chain.iter().enumerate() {
        if after.kind != CheckpointKind::After || undone.contains(&after.turn) {
            continue;
        }
        if let Some(before) = chain[i + 1..].iter().find(|c| c.kind == CheckpointKind::Before && c.turn == after.turn) {
            return Some((before.clone(), after.clone()));
        }
    }
    None
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

/// The path as it is now: its git mode and content (a symlink's target), or None when it's missing (or a folder).
fn current(absolute: &Path) -> Option<Target> {
    let info = std::fs::symlink_metadata(absolute).ok()?;
    if info.file_type().is_symlink() {
        let target = std::fs::read_link(absolute).ok()?;
        return Some(Target { mode: "120000".into(), content: target.as_os_str().as_bytes().to_vec() });
    }
    if !info.is_file() {
        return None;
    }
    let mode = if info.permissions().mode() & 0o111 != 0 { "100755" } else { "100644" };
    Some(Target { mode: mode.into(), content: std::fs::read(absolute).ok()? })
}

/// The newest turn to undo, what undoing it does to each path, and whether a path was edited since the turn ended.
/// `reported`: the paths a turn's tools said they changed, so the ones git ignores can be listed as not restored.
pub async fn plan_undo(
    store: &CheckpointStore,
    reported: impl Fn(u32) -> Vec<String>,
    root: Option<&Path>,
) -> Result<UndoPlan, UndoRefusal> {
    let where_ = store.locate().await.map_err(|e| UndoRefusal::failed(format!("checkpoints can't be read: {e}")))?;
    let head = store.head().await;
    let chain = store.chain().await.map_err(UndoRefusal::failed)?;
    let (Some(head), Some((before, after))) = (head, next_undoable(&chain, &store.undone_turns())) else {
        return Err(UndoRefusal {
            reason: "nothing to undo: no turn in this session has changed files yet (or they're all undone)".into(),
            failed: false,
        });
    };
    // Resolved as apply_undo resolves it, so the plan and the apply compare paths against the same root.
    let given = root.map_or_else(|| where_.top.join(&where_.prefix), Path::to_path_buf);
    let project_root = std::fs::canonicalize(&given).unwrap_or(given);
    let spec = store.pathspec().await;
    let listed =
        store.git(&["diff-tree", "-r", "-z", "--no-renames", &before.sha, &after.sha, "--", &spec], None, None, None).await;
    if !listed.ok {
        return Err(UndoRefusal::failed(format!("git diff-tree failed: {}", listed.out.trim())));
    }
    let fields: Vec<&str> = listed.out.split('\0').collect();
    let mut files = Vec::new();
    let mut targets = HashMap::new();
    let mut not_restored: Vec<String> = Vec::new();
    for pair in fields.chunks(2) {
        let [meta, top_path] = pair else { continue };
        if top_path.is_empty() {
            continue;
        }
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_sha, new_sha, status] = parts[..] else { continue };
        let path = top_path.strip_prefix(where_.prefix.as_str()).unwrap_or(top_path).to_string();
        // A submodule's commit isn't a file Marv can write back.
        if old_mode == "160000" || new_mode == "160000" {
            not_restored.push(path);
            continue;
        }
        // A copy that can't be read (pruned, corrupt) stops the plan: read as empty, it would be written back as an
        // empty file.
        let blob = |sha: &str| {
            let sha = sha.to_string();
            async move {
                let read = store.git(&["cat-file", "blob", &sha], None, None, None).await;
                read.ok.then_some(read.bytes)
            }
        };
        let unreadable = || UndoRefusal::failed(format!("the saved copy of {path} can't be read from the checkpoints"));
        let before_target = if status == "A" {
            None
        } else {
            Some(Target { mode: old_mode.into(), content: blob(old_sha).await.ok_or_else(unreadable)? })
        };
        let after_target = if status == "D" {
            None
        } else {
            Some(Target { mode: new_mode.into(), content: blob(new_sha).await.ok_or_else(unreadable)? })
        };
        let now = current(&project_root.join(&path));
        let changed_since = match (&after_target, &now) {
            (None, now) => now.is_some(),
            (Some(_), None) => true,
            (Some(after), Some(now)) => now != after,
        };
        let action = match status {
            "A" => UndoAction::Delete,
            "D" => UndoAction::Recreate,
            _ => UndoAction::Restore,
        };
        let from = now.as_ref().map_or(&[][..], |n| &n.content[..]);
        let to = before_target.as_ref().map_or(&[][..], |b| &b.content[..]);
        let binary = is_binary(from) || is_binary(to);
        let large = from.len() > MAX_DIFFED_BYTES || to.len() > MAX_DIFFED_BYTES;
        let diff = (!binary && !large).then(|| {
            let (from, to) = (String::from_utf8_lossy(from), String::from_utf8_lossy(to));
            if now.is_none() { shown_diff(&added_lines(&to)) } else { shown_diff(&diff_text(&from, &to, 3).lines) }
        });
        files.push(UndoFile { path: path.clone(), action, changed_since, diff, binary });
        targets.insert(path, before_target);
    }
    // Reported by a tool but in neither snapshot: git ignores it, so there's nothing to put back.
    let in_tree = |tree: &str, path: &str| {
        let object = format!("{tree}:{}{path}", where_.prefix);
        async move { store.git(&["cat-file", "-e", &object], None, None, None).await.ok }
    };
    let mut seen = HashSet::new();
    for path in reported(after.turn) {
        if !seen.insert(path.clone()) || targets.contains_key(&path) || not_restored.contains(&path) {
            continue;
        }
        if !in_tree(&before.sha, &path).await && !in_tree(&after.sha, &path).await {
            not_restored.push(path);
        }
    }
    Ok(UndoPlan { turn: after.turn, label: after.label.clone(), before, after, files, not_restored, head, targets })
}

fn inside(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

/// Why `path`'s folder can't be written now (it leads outside the project, or isn't a folder), or None.
fn unsafe_parent(root: &Path, path: &str) -> Option<String> {
    let joined = root.join(path);
    let mut folder = joined.parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf());
    // The deepest folder that exists: it must really be inside the project, symlinks resolved.
    while std::fs::symlink_metadata(&folder).is_err() {
        match folder.parent() {
            Some(parent) => folder = parent.to_path_buf(),
            None => break,
        }
    }
    let Some(real) = std::fs::canonicalize(&folder).ok().filter(|real| inside(root, real)) else {
        return Some("its folder now leads outside the project".into());
    };
    if !std::fs::symlink_metadata(&real).is_ok_and(|m| m.is_dir()) {
        return Some("its folder is now a file".into());
    }
    None
}

/// Removes the folders above `path` that became empty, up to (never including) the project root.
fn remove_empty_folders(root: &Path, path: &str) {
    let joined = root.join(path);
    let mut folder = joined.parent().map(Path::to_path_buf);
    while let Some(dir) = folder {
        if dir == root || !inside(root, &dir) || std::fs::remove_dir(&dir).is_err() {
            return;
        }
        folder = dir.parent().map(Path::to_path_buf);
    }
}

/// Writes `target` at `absolute` as its git mode says (a file, an executable, a symlink), replacing what's there
/// without following it.
fn put(root: &Path, absolute: &Path, target: &Target) -> Result<(), String> {
    let folder = absolute.parent().ok_or("no folder")?;
    std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    // Checked again now that the folders exist: one swapped for a symlink since unsafe_parent looked is refused.
    if !std::fs::canonicalize(folder).is_ok_and(|real| inside(root, &real)) {
        return Err("its folder now leads outside the project".into());
    }
    let temp: PathBuf = folder.join(format!(".marv-undo-{}-{}", std::process::id(), marv_fs::random_hex()));
    if target.mode == "120000" {
        std::os::unix::fs::symlink(OsStr::from_bytes(&target.content), &temp).map_err(|e| e.to_string())?;
    } else {
        let mode = if target.mode == "100755" { 0o755 } else { 0o644 };
        std::fs::write(&temp, &target.content).map_err(|e| e.to_string())?;
        // Set after writing: the umask may have taken bits off.
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    }
    if std::fs::symlink_metadata(absolute).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::remove_file(&temp);
        return Err("a folder is there now".into());
    }
    std::fs::rename(&temp, absolute).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        e.to_string()
    })
}

/// Applies a plan the user said yes to: a pre-undo snapshot first (so the undo can be undone by hand), then each
/// path, created files deleted before anything is written (a file the turn put where a folder was), and an `undo`
/// marker on the chain, so the next /undo goes one turn further back. Fails if the snapshot fails or the chain moved
/// since the plan was made; a path that can't be written safely is refused and reported, not written.
pub async fn apply_undo(store: &CheckpointStore, plan: &UndoPlan, root: Option<&Path>) -> Result<UndoResult, String> {
    let where_ = store.locate().await?;
    if store.head().await.as_deref() != Some(plan.head.as_str()) {
        return Err("the checkpoints changed since this undo was planned: run /undo again".into());
    }
    let given = root.map_or_else(|| where_.top.join(&where_.prefix), Path::to_path_buf);
    let project_root = std::fs::canonicalize(&given).map_err(|e| e.to_string())?;
    let snapshot = |kind| store.snapshot(SnapshotOptions { turn: plan.turn, kind, label: &plan.label, cancel: None });
    let mut result = match snapshot(CheckpointKind::PreUndo).await {
        SnapshotResult::Ok { sha, files, ms, .. } => {
            UndoResult { pre_undo: sha, pre_undo_ms: ms, pre_undo_files: files, ..Default::default() }
        }
        SnapshotResult::Failed { message, .. } => return Err(format!("couldn't save a snapshot before undoing ({message})")),
    };
    let mut ordered: Vec<&UndoFile> = plan.files.iter().collect();
    ordered.sort_by_key(|f| f.action != UndoAction::Delete); // stable: deletes first, the rest in order
    for file in ordered {
        let refuse = |result: &mut UndoResult, reason: String| result.refused.push(Refused { path: file.path.clone(), reason });
        if let Some(why) = unsafe_parent(&project_root, &file.path) {
            refuse(&mut result, why);
            continue;
        }
        let absolute = project_root.join(&file.path);
        match plan.targets.get(&file.path).cloned().flatten() {
            None => {
                let info = std::fs::symlink_metadata(&absolute).ok();
                if info.as_ref().is_some_and(|i| i.is_dir()) {
                    refuse(&mut result, "a folder is there now".into());
                    continue;
                }
                if info.is_some()
                    && let Err(err) = std::fs::remove_file(&absolute)
                {
                    refuse(&mut result, err.to_string());
                    continue;
                }
                remove_empty_folders(&project_root, &file.path);
                result.deleted.push(file.path.clone());
            }
            Some(target) => match put(&project_root, &absolute, &target) {
                Ok(()) if file.action == UndoAction::Recreate => result.recreated.push(file.path.clone()),
                Ok(()) => result.restored.push(file.path.clone()),
                Err(reason) => refuse(&mut result, reason),
            },
        }
    }
    if let SnapshotResult::Failed { message, .. } = snapshot(CheckpointKind::Undo).await {
        // The files are already written: the turn is undone either way, so the next /undo must not pick it again.
        store.mark_undone(plan.turn);
        result.marker_failed = Some(message);
    }
    Ok(result)
}

/// "a.ts", "a.ts and b.ts", "a.ts, b.ts and c.ts".
fn listed(paths: &[String]) -> String {
    match paths {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn first_line(label: &str, max: usize) -> String {
    label.split('\n').next().unwrap_or("").chars().take(max).collect()
}

/// What the model is told at the start of the next message after an undo: the files it changed aren't as it left
/// them, so what it remembers of them is wrong. None when nothing was written.
pub fn undo_note(plan: &UndoPlan, result: &UndoResult) -> Option<String> {
    let back: Vec<String> = result.restored.iter().chain(&result.recreated).cloned().collect();
    let mut parts = Vec::new();
    if !back.is_empty() {
        let verb = if back.len() == 1 { "is back as it was" } else { "are back as they were" };
        parts.push(format!("{} {verb} before it", listed(&back)));
    }
    if !result.deleted.is_empty() {
        parts.push(format!("{} {} removed", listed(&result.deleted), if result.deleted.len() == 1 { "was" } else { "were" }));
    }
    if parts.is_empty() {
        return None;
    }
    let label = first_line(&plan.label, 80);
    let label = if label.is_empty() { String::new() } else { format!(" (\"{label}\")") };
    Some(format!(
        "Marv: the user undid your changes from an earlier turn{label}: {}. Re-read files before editing them.",
        parts.join(", ")
    ))
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn verb(action: UndoAction) -> &'static str {
    match action {
        UndoAction::Restore => "Restore",
        UndoAction::Delete => "Delete",
        UndoAction::Recreate => "Recreate",
    }
}

/// /undo's question: each file and what undoing does to it (its diff, now → before), what's left alone, yes or no.
pub fn undo_request(plan: &UndoPlan) -> ApprovalRequest {
    let label = first_line(&plan.label, 60);
    let changed_since: Vec<&str> = plan.files.iter().filter(|f| f.changed_since).map(|f| f.path.as_str()).collect();
    let mut text = Vec::new();
    if !changed_since.is_empty() {
        text.push(format!("Changed since the turn: {}", changed_since.join(", ")));
    }
    if !plan.not_restored.is_empty() {
        text.push(format!("Not restored (ignored by git): {}", plan.not_restored.join(", ")));
    }
    let files = plan
        .files
        .iter()
        .map(|f| PreviewFile {
            title: format!("{} {}", verb(f.action), f.path),
            diff: f.diff.clone(),
            note: if f.binary {
                Some("binary".into())
            } else if f.diff.is_none() {
                Some("too large to show".into())
            } else if f.changed_since {
                Some("changed since".into())
            } else {
                None
            },
        })
        .collect();
    let quoted = if label.is_empty() { String::new() } else { format!(" (\"{label}\")") };
    ApprovalRequest {
        reason: false,
        persists: false,
        tool: "undo".into(),
        label: format!("turn {}", plan.turn),
        preview: Preview {
            title: format!("Undo turn {}{quoted}? {}", plan.turn, plural(plan.files.len(), "file")),
            files: Some(files),
            text: (!text.is_empty()).then(|| text.join("\n")),
            warning: (!changed_since.is_empty()).then(|| {
                format!(
                    "{} changed since the turn: those edits are undone too (a snapshot taken first keeps them)",
                    plural(changed_since.len(), "file")
                )
            }),
            note: Some("nothing is written before you say yes".into()),
            ..Preview::default()
        },
        scope: Scope { key: "undo".into(), description: "undo".into() },
        agent: None,
        network: false,
        once: true,
        must_ask: false,
        protected: None,
        door: None,
    }
}

/// The transcript's line after an undo: "↶ Undid turn 3: 4 files restored", and anything left alone.
pub fn undo_summary(plan: &UndoPlan, result: &UndoResult) -> String {
    let done = result.restored.len() + result.recreated.len() + result.deleted.len();
    let mut parts = vec![format!("↶ Undid turn {}: {} restored", plan.turn, plural(done, "file"))];
    if !result.refused.is_empty() {
        let refused: Vec<String> = result.refused.iter().map(|r| format!("{} ({})", r.path, r.reason)).collect();
        parts.push(format!("not written: {}", refused.join(", ")));
    }
    if !plan.not_restored.is_empty() {
        parts.push(format!("left as they are (ignored by git): {}", plan.not_restored.join(", ")));
    }
    if let Some(why) = &result.marker_failed {
        parts.push(format!("the undo couldn't be recorded ({why}): after a restart /undo may offer this turn again"));
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    // Ports tests/checkpoints.test.ts "undo".
    use super::*;
    use crate::checkpoints::store::{SnapshotFailure, StoreOptions};
    use crate::checkpoints::test_support::{Temp, git, make_repo, read, temp, write};
    use std::sync::Arc;

    fn store(root: &Path, own: &Temp) -> CheckpointStore {
        CheckpointStore::new(StoreOptions::new(root, Some(own.0.clone()), "s1"))
    }

    /// A turn: snapshot before, `change` the files, snapshot after.
    async fn turn(s: &CheckpointStore, n: u32, change: impl FnOnce(), label: &str) {
        let label = if label.is_empty() { format!("turn {n}") } else { label.to_string() };
        assert!(s.snapshot(SnapshotOptions { turn: n, kind: CheckpointKind::Before, label: &label, cancel: None }).await.ok());
        change();
        assert!(s.snapshot(SnapshotOptions { turn: n, kind: CheckpointKind::After, label: &label, cancel: None }).await.ok());
    }

    async fn plan(s: &CheckpointStore) -> UndoPlan {
        plan_undo(s, |_| Vec::new(), None).await.unwrap_or_else(|r| panic!("{}", r.reason))
    }

    fn lines(diff: &Option<DiffShown>) -> Vec<String> {
        diff.as_ref()
            .unwrap()
            .lines
            .iter()
            .map(|l| format!("{}:{}", serde_json::to_value(l.kind).unwrap().as_str().unwrap(), l.text))
            .collect()
    }

    #[tokio::test]
    async fn modified_created_deleted_mode_change_and_symlink_are_put_back_empty_folders_go() {
        let repo = make_repo(true);
        let r = repo.0.clone();
        write(r.join("gone.txt"), "keep me\n");
        write(r.join("deep/gone2.txt"), "deep\n");
        write(r.join("run.sh"), "echo hi\n");
        std::os::unix::fs::symlink("a.txt", r.join("link")).unwrap();
        let own = temp("cp-own");
        let s = store(&r, &own);
        turn(
            &s,
            1,
            || {
                write(r.join("a.txt"), "two\n");
                write(r.join("new/nested/made.txt"), "made\n");
                std::fs::remove_file(r.join("gone.txt")).unwrap();
                std::fs::remove_dir_all(r.join("deep")).unwrap();
                std::fs::set_permissions(r.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
                std::fs::remove_file(r.join("link")).unwrap();
                std::os::unix::fs::symlink("gone.txt", r.join("link")).unwrap();
            },
            "add the parser",
        )
        .await;
        let plan = plan(&s).await;
        assert_eq!((plan.turn, plan.label.as_str()), (1, "add the parser"));
        let by_path: HashMap<&str, UndoAction> = plan.files.iter().map(|f| (f.path.as_str(), f.action)).collect();
        assert_eq!(
            by_path,
            HashMap::from([
                ("a.txt", UndoAction::Restore),
                ("new/nested/made.txt", UndoAction::Delete),
                ("gone.txt", UndoAction::Recreate),
                ("deep/gone2.txt", UndoAction::Recreate),
                ("run.sh", UndoAction::Restore),
                ("link", UndoAction::Restore),
            ])
        );
        assert_eq!(lines(&plan.files.iter().find(|f| f.path == "a.txt").unwrap().diff), ["del:two", "add:one"]);
        assert!(plan.files.iter().all(|f| !f.changed_since));

        let result = apply_undo(&s, &plan, None).await.unwrap();
        assert!(result.refused.is_empty(), "{:?}", result.refused);
        assert_eq!(read(r.join("a.txt")), "one\n");
        assert!(!r.join("new").exists());
        assert_eq!(read(r.join("gone.txt")), "keep me\n");
        assert_eq!(read(r.join("deep/gone2.txt")), "deep\n");
        assert_eq!(std::fs::metadata(r.join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
        assert!(std::fs::symlink_metadata(r.join("link")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(r.join("link")).unwrap(), Path::new("a.txt"));
        assert_eq!(result.restored.len() + result.deleted.len() + result.recreated.len(), 6);
    }

    // The working file's own bytes, not what git's filters store (Greptile on Marv #71).

    /// Commits `files` and `.gitattributes` in a fresh repository with `config` set first.
    fn repo_with(config: &[(&str, &str)], attributes: &str, files: &[(&str, &str)]) -> Temp {
        let repo = make_repo(true);
        for (key, value) in config {
            git(&repo.0, &["config", key, value]);
        }
        write(repo.0.join(".gitattributes"), attributes);
        for (path, content) in files {
            write(repo.0.join(path), content);
        }
        git(&repo.0, &["add", "-A"]);
        git(&repo.0, &["commit", "-q", "-m", "attributes"]);
        repo
    }

    async fn undo_back(s: &CheckpointStore) -> UndoPlan {
        let plan = plan(s).await;
        assert!(plan.files.iter().all(|f| !f.changed_since), "{:?}", plan.files);
        assert!(apply_undo(s, &plan, None).await.unwrap().refused.is_empty());
        plan
    }

    #[tokio::test]
    async fn eol_crlf_a_crlf_file_comes_back_crlf_and_isnt_called_changed_since() {
        let crlf = "one\r\ntwo\r\n";
        let repo = repo_with(&[], "*.txt text eol=crlf\n", &[("w.txt", crlf)]);
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(&s, 1, || write(repo.0.join("w.txt"), "one\r\nTWO\r\n"), "").await;
        let plan = undo_back(&s).await;
        assert_eq!(lines(&plan.files.iter().find(|f| f.path == "w.txt").unwrap().diff), ["ctx:one\r", "del:TWO\r", "add:two\r"]);
        assert_eq!(read(repo.0.join("w.txt")), crlf);
    }

    #[tokio::test]
    async fn core_autocrlf_lf_stays_lf_crlf_stays_crlf_also_when_deleted() {
        let repo = repo_with(&[("core.autocrlf", "true")], "", &[("lf.txt", "lf\n"), ("crlf.txt", "crlf\r\n")]);
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(
            &s,
            1,
            || {
                write(repo.0.join("lf.txt"), "changed\n");
                std::fs::remove_file(repo.0.join("crlf.txt")).unwrap();
            },
            "",
        )
        .await;
        undo_back(&s).await;
        assert_eq!(read(repo.0.join("lf.txt")), "lf\n");
        assert_eq!(read(repo.0.join("crlf.txt")), "crlf\r\n");
    }

    #[tokio::test]
    async fn a_clean_filter_the_files_own_content_comes_back_no_smudge_run() {
        let markers = temp("cp-marker");
        let marker = markers.0.join("smudged");
        let smudge = format!("touch {}; tr A-Z a-z", marker.display());
        let repo = repo_with(
            &[("filter.fake.clean", "tr a-z A-Z"), ("filter.fake.smudge", &smudge)],
            "*.bin filter=fake\n",
            &[("asset.bin", "real content\n")],
        );
        let _ = std::fs::remove_file(&marker);
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(&s, 1, || write(repo.0.join("asset.bin"), "other bytes!\n"), "").await;
        undo_back(&s).await;
        assert_eq!(read(repo.0.join("asset.bin")), "real content\n");
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn outside_a_repository_too_the_projects_gitattributes_applies() {
        let plain = temp("cp-plain");
        write(plain.0.join(".gitattributes"), "*.txt text eol=crlf\n");
        write(plain.0.join("w.txt"), "x\r\ny\r\n");
        let cp_own = temp("cp-own");
        let s = store(&plain.0, &cp_own);
        turn(&s, 1, || write(plain.0.join("w.txt"), "z\r\n"), "").await;
        undo_back(&s).await;
        assert_eq!(read(plain.0.join("w.txt")), "x\r\ny\r\n");
        // A second turn reuses what the first hashed: still the file's own bytes.
        turn(&s, 2, || write(plain.0.join("w.txt"), "q\r\n"), "").await;
        undo_back(&s).await;
        assert_eq!(read(plain.0.join("w.txt")), "x\r\ny\r\n");
    }

    #[tokio::test]
    async fn a_file_edited_since_is_marked_and_restored_reported_paths_in_neither_tree_arent_restored() {
        let repo = make_repo(true);
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(
            &s,
            1,
            || {
                write(repo.0.join("a.txt"), "two\n");
                write(repo.0.join("ignored.txt"), "secret\n");
            },
            "",
        )
        .await;
        write(repo.0.join("a.txt"), "three\n");
        let plan = plan_undo(&s, |_| vec!["a.txt".into(), "ignored.txt".into()], None).await.unwrap();
        let files: Vec<(&str, bool)> = plan.files.iter().map(|f| (f.path.as_str(), f.changed_since)).collect();
        assert_eq!(files, [("a.txt", true)]);
        assert_eq!(plan.not_restored, ["ignored.txt"]);
        apply_undo(&s, &plan, None).await.unwrap();
        assert_eq!(read(repo.0.join("a.txt")), "one\n");
    }

    #[tokio::test]
    async fn a_parent_that_now_leads_outside_the_project_is_refused() {
        let repo = make_repo(true);
        let outside = temp("cp-outside");
        write(repo.0.join("sub/f.txt"), "f\n");
        git(&repo.0, &["add", "-A"]);
        git(&repo.0, &["commit", "-q", "-m", "sub"]);
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(&s, 1, || write(repo.0.join("sub/f.txt"), "changed\n"), "").await;
        std::fs::remove_dir_all(repo.0.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside.0, repo.0.join("sub")).unwrap();
        let plan = plan(&s).await;
        let result = apply_undo(&s, &plan, None).await.unwrap();
        let refused: Vec<&str> = result.refused.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(refused, ["sub/f.txt"]);
        assert!(!outside.0.join("f.txt").exists());
    }

    #[tokio::test]
    async fn a_binary_file_is_named_not_diffed() {
        let repo = make_repo(true);
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(&s, 1, || write(repo.0.join("img.bin"), [0u8, 1, 2, 0]), "").await;
        let plan = plan(&s).await;
        assert_eq!(plan.files.len(), 1);
        let f = &plan.files[0];
        assert_eq!((f.path.as_str(), f.action, f.binary), ("img.bin", UndoAction::Delete, true));
        assert!(f.diff.is_none());
    }

    #[tokio::test]
    async fn pre_undo_snapshot_and_undo_marker_two_undos_walk_back_two_turns() {
        let repo = make_repo(true);
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(&s, 1, || write(repo.0.join("a.txt"), "two\n"), "").await;
        turn(&s, 2, || write(repo.0.join("a.txt"), "three\n"), "").await;
        let first = plan(&s).await;
        assert_eq!(first.turn, 2);
        let applied = apply_undo(&s, &first, None).await.unwrap();
        assert!(!applied.pre_undo.is_empty());
        assert_eq!(git(&repo.0, &["show", &format!("{}:a.txt", applied.pre_undo)]), "three");
        assert_eq!(read(repo.0.join("a.txt")), "two\n");
        let chain = s.chain().await.unwrap();
        let kinds: Vec<(u32, CheckpointKind)> = chain.iter().take(2).map(|c| (c.turn, c.kind)).collect();
        assert_eq!(kinds, [(2, CheckpointKind::Undo), (2, CheckpointKind::PreUndo)]);
        assert_eq!(next_undoable(&chain, &HashSet::new()).map(|(_, after)| after.turn), Some(1));

        let second = plan(&s).await;
        assert_eq!(second.turn, 1);
        apply_undo(&s, &second, None).await.unwrap();
        assert_eq!(read(repo.0.join("a.txt")), "one\n");
        let refusal = plan_undo(&s, |_| Vec::new(), None).await.unwrap_err();
        assert!(refusal.reason.contains("nothing") && !refusal.failed);
    }

    #[tokio::test]
    async fn a_failed_undo_marker_still_counts_the_next_undo_goes_further_back_and_the_summary_says_so() {
        let repo = make_repo(true);
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(&s, 1, || write(repo.0.join("a.txt"), "two\n"), "").await;
        turn(&s, 2, || write(repo.0.join("a.txt"), "three\n"), "").await;
        let first = plan(&s).await;
        s.fake.lock().unwrap().snapshot = Some(Arc::new(|kind| {
            (kind == CheckpointKind::Undo).then(|| SnapshotResult::Failed {
                reason: SnapshotFailure::Failed,
                message: "disk full".into(),
                ms: 0,
            })
        }));
        let applied = apply_undo(&s, &first, None).await.unwrap();
        s.fake.lock().unwrap().snapshot = None;
        assert_eq!(applied.marker_failed.as_deref(), Some("disk full"));
        assert!(undo_summary(&first, &applied).contains("couldn't be recorded (disk full)"));
        write(repo.0.join("a.txt"), "edited by the user\n");
        assert_eq!(plan(&s).await.turn, 1);
    }

    #[tokio::test]
    async fn a_saved_copy_that_cant_be_read_stops_the_plan_rather_than_writing_the_file_back_empty() {
        let repo = make_repo(true);
        write(repo.0.join("lost.txt"), "only copy in the checkpoints\n");
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(&s, 1, || write(repo.0.join("lost.txt"), "changed\n"), "").await;
        let before = s.chain().await.unwrap().into_iter().find(|c| c.kind == CheckpointKind::Before).unwrap();
        let blob = git(&repo.0, &["rev-parse", &format!("{}:lost.txt", before.sha)]);
        std::fs::remove_file(repo.0.join(".git/objects").join(&blob[..2]).join(&blob[2..])).unwrap();
        let refusal = plan_undo(&s, |_| Vec::new(), None).await.unwrap_err();
        assert!(refusal.failed && refusal.reason.contains("lost.txt"), "{refusal:?}");
        assert_eq!(read(repo.0.join("lost.txt")), "changed\n");
    }

    #[tokio::test]
    async fn a_plan_made_before_another_checkpoint_is_refused() {
        let repo = make_repo(true);
        let cp_own = temp("cp-own");
        let s = store(&repo.0, &cp_own);
        turn(&s, 1, || write(repo.0.join("a.txt"), "two\n"), "").await;
        let stale = plan(&s).await;
        turn(&s, 2, || write(repo.0.join("a.txt"), "three\n"), "").await;
        assert!(apply_undo(&s, &stale, None).await.unwrap_err().contains("changed"));
        assert_eq!(read(repo.0.join("a.txt")), "three\n");
    }

    #[tokio::test]
    async fn a_subfolder_projects_paths_are_relative_to_it() {
        let repo = make_repo(true);
        std::fs::create_dir(repo.0.join("pkg")).unwrap();
        let cp_own = temp("cp-own");
        let s = store(&repo.0.join("pkg"), &cp_own);
        turn(&s, 1, || write(repo.0.join("pkg/x.txt"), "x\n"), "").await;
        let plan = plan(&s).await;
        let paths: Vec<&str> = plan.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["x.txt"]);
        apply_undo(&s, &plan, None).await.unwrap();
        assert!(!repo.0.join("pkg/x.txt").exists());
        assert!(repo.0.join("pkg").exists()); // the project's own folder is never removed
    }

    fn a_plan(files: Vec<UndoFile>, not_restored: Vec<String>) -> UndoPlan {
        let c = |kind| Checkpoint { sha: "s".into(), tree: "t".into(), turn: 3, kind, label: "add the parser\nmore".into() };
        UndoPlan {
            turn: 3,
            label: "add the parser\nmore".into(),
            before: c(CheckpointKind::Before),
            after: c(CheckpointKind::After),
            files,
            not_restored,
            head: "h".into(),
            targets: HashMap::new(),
        }
    }

    #[test]
    fn the_note_the_request_and_the_summary_read_as_marvs() {
        let file = |path: &str, action, changed_since, diff: bool| UndoFile {
            path: path.into(),
            action,
            changed_since,
            diff: diff.then(DiffShown::default),
            binary: false,
        };
        let plan = a_plan(
            vec![file("a.ts", UndoAction::Restore, true, true), file("b.ts", UndoAction::Delete, false, false)],
            vec!["x.log".into()],
        );
        let result = UndoResult { restored: vec!["a.ts".into()], deleted: vec!["b.ts".into()], ..Default::default() };
        assert_eq!(
            undo_note(&plan, &result).unwrap(),
            "Marv: the user undid your changes from an earlier turn (\"add the parser\"): a.ts is back as it was before it, b.ts was removed. Re-read files before editing them."
        );
        assert_eq!(undo_note(&plan, &UndoResult::default()), None);
        let two =
            UndoResult { restored: vec!["a.ts".into(), "c.ts".into()], recreated: vec!["d.ts".into()], ..Default::default() };
        assert!(undo_note(&plan, &two).unwrap().contains("a.ts, c.ts and d.ts are back as they were before it."));

        let request = undo_request(&plan);
        assert_eq!((request.tool.as_str(), request.label.as_str(), request.once), ("undo", "turn 3", true));
        assert_eq!(request.preview.title, "Undo turn 3 (\"add the parser\")? 2 files");
        let files = request.preview.files.unwrap();
        assert_eq!(files[0].title, "Restore a.ts");
        assert_eq!(files[0].note.as_deref(), Some("changed since"));
        assert_eq!(files[1].note.as_deref(), Some("too large to show"));
        assert_eq!(request.preview.text.as_deref(), Some("Changed since the turn: a.ts\nNot restored (ignored by git): x.log"));
        assert_eq!(
            request.preview.warning.as_deref(),
            Some("1 file changed since the turn: those edits are undone too (a snapshot taken first keeps them)")
        );
        assert_eq!(request.preview.note.as_deref(), Some("nothing is written before you say yes"));

        let refused = UndoResult { refused: vec![Refused { path: "z".into(), reason: "why".into() }], ..result };
        assert_eq!(
            undo_summary(&plan, &refused),
            "↶ Undid turn 3: 2 files restored; not written: z (why); left as they are (ignored by git): x.log"
        );
    }
}

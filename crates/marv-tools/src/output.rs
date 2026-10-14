//! Cutting long tool output for the model: the start and the end are kept, the middle is dropped, and the whole
//! output is saved to a file the model can read.
//!
//! New module (no Marv file of its own): the cut from `formatOutput` in Marv's `src/tools/bash.ts`, shared because
//! MCP results (`src/mcp/tool.ts`) are capped at the same 30k characters.
//!
//! - 30k characters reach the model: the first 10k and the last 20k (errors and summaries are usually at the end).
//! - Counted in `char`s (Marv: UTF-16 units), and never split inside a character.
//! - #61 (new in marv-rs, after opencode's `tool-output.ts`): a cut output of bash, an MCP tool or a subagent is saved
//!   whole to `~/.marv/tool-output/<session>/<call id>.txt` (0600, in 0700 folders, like sessions) and the marker names
//!   it, so the model can `read_file` or `grep` the part it needs instead of re-running the command (slow, side
//!   effects, a different result). `read_file` and `grep` may read exactly the session's folder
//!   ([`crate::files::resolve_readable`]); bash can't see it (the sandbox has its own home), and doesn't need to.
//! - Call ids aren't unique (Ollama reuses `call_0`, and a subagent's ids are its own): a taken name gets `-1`, `-2`….
//! - A save that fails only loses the path: the cut is the same, so the call never fails over it.
//! - Trajectories record the model's text, so the path, never the saved content.
//! - [`clean_tool_output`] runs at a session's first turn (after any resume): sessions' folders older than [`KEEP_DAYS`] go, then the oldest
//!   until the whole store is under [`MAX_TOTAL_BYTES`]. web_fetch isn't cut but paged (`offset`), so it saves nothing.
//! - A live session holds its folder ([`hold`]: a shared lock on its `.live` file), and the cleanup never deletes a
//!   held one: not the session that just started, not the one it then resumed, not another Marv's that's running.
//! - The source is kept: an output from a source other than the project (an MCP server, a subagent) is saved as
//!   `<call id>.<source>.txt`, and reading it back with `read_file` or `grep` gives it that source again
//!   ([`saved_provenance`]), so an MCP server's text is fenced and taints when read back as when it first came.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use marv_core::Provenance;

use crate::types::ToolContext;

/// What the model sees of a long output.
pub const MAX_MODEL_CHARS: usize = 30_000;
/// How much of the start is kept; the rest of the budget goes to the end.
pub const HEAD_CHARS: usize = 10_000;
/// The store's folder under Marv's config folder (`~/.marv`).
pub const TOOL_OUTPUT_DIR: &str = "tool-output";
/// A session's saved outputs are deleted this long after its folder last changed.
pub const KEEP_DAYS: u64 = 7;
/// The whole store's cap: past it, the oldest sessions' folders go first.
pub const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// The file a live session holds a shared lock on, in its folder ([`hold`]).
pub const LIVE_FILE: &str = ".live";

/// `text` with its middle cut when longer than `max` characters: `head` characters, a marker, then the last
/// `max - head`.
pub fn cut_middle(text: &str, max: usize, head: usize) -> String {
    cut_middle_saved(text, max, head, None)
}

/// [`cut_middle`], the marker naming the file that holds the whole text.
fn cut_middle_saved(text: &str, max: usize, head: usize, saved: Option<&Path>) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let byte_at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    let tail = max - head;
    let omitted = total - max;
    let marker = match saved {
        Some(path) => {
            format!("… ({omitted} characters omitted; full output: {} — read_file it, or grep it) …", path.display())
        }
        None => format!("… ({omitted} characters omitted) …"),
    };
    format!("{}\n\n{marker}\n\n{}", &text[..byte_at(head)], &text[byte_at(total - tail)..])
}

/// [`cut_middle`] with the model's budget.
pub fn cut_for_model(text: &str) -> String {
    cut_middle(text, MAX_MODEL_CHARS, HEAD_CHARS)
}

/// [`cut_for_model`], and when it cuts, `text` saved whole in `dir` (a session's folder) under the call's id and
/// named in the marker.
pub fn spill_cut(text: &str, dir: &Path, call_id: &str, source: Provenance) -> std::io::Result<String> {
    if text.chars().count() <= MAX_MODEL_CHARS {
        return Ok(text.to_string());
    }
    let path = save(text, dir, call_id, source)?;
    Ok(cut_middle_saved(text, MAX_MODEL_CHARS, HEAD_CHARS, Some(&path)))
}

/// What a call's long result goes through: [`spill_cut`] into the session's folder (`ToolContext::tool_output`),
/// or the plain cut where there's none or the save fails. `source`: where the text came from (the tool's provenance),
/// kept in the file's name so a read gives it back ([`saved_provenance`]).
pub fn cut_for_call(text: &str, ctx: &ToolContext, source: Provenance) -> String {
    match &ctx.tool_output {
        Some(dir) => {
            spill_cut(text, dir, ctx.call_id.as_deref().unwrap_or("call"), source).unwrap_or_else(|_| cut_for_model(text))
        }
        None => cut_for_model(text),
    }
}

/// The source's tag in a saved output's name: none for the project, its serde name (`mcp`, `subagent`…) otherwise.
fn source_tag(source: Provenance) -> Option<String> {
    (source != Provenance::Project)
        .then(|| serde_json::to_value(source).ok().and_then(|v| v.as_str().map(str::to_string)))
        .flatten()
}

/// The source of a saved output, from its name (`<call id>[-n][.<source>].txt`; a call id has no dots). A name with
/// a tag Marv doesn't know is MCP's: the strictest reading (fenced, taints).
pub fn saved_provenance(path: &Path) -> Provenance {
    let Some(stem) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".txt")) else {
        return Provenance::Mcp;
    };
    match stem.split_once('.') {
        None => Provenance::Project,
        Some((_, tag)) => serde_json::from_value(serde_json::Value::String(tag.into())).unwrap_or(Provenance::Mcp),
    }
}

/// Writes `text` to a new `<call id>[.<source>].txt` in `dir` (0600; `-1`, `-2`… when the name is taken). Returns its
/// path.
fn save(text: &str, dir: &Path, call_id: &str, source: Provenance) -> std::io::Result<PathBuf> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // A provider's id is the model's text as far as a file name goes: only what's safe in one.
    let mut name: String =
        call_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(100).collect();
    if name.is_empty() {
        name = "call".into();
    }
    let tag = source_tag(source).map_or(String::new(), |t| format!(".{t}"));
    for n in 0.. {
        let path = if n == 0 { dir.join(format!("{name}{tag}.txt")) } else { dir.join(format!("{name}-{n}{tag}.txt")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("some name is free")
}

/// A live session's hold on its folder (a shared lock on its [`LIVE_FILE`]): while it's kept, [`clean_tool_output`]
/// leaves the folder alone, in this process or another. Dropping it lets the folder age out as usual.
#[derive(Debug)]
pub struct LiveFolder {
    _lock: std::fs::File,
}

/// Holds `dir` (a session's folder, made if missing) for a live session, and marks it as just used, so a resumed
/// session's outputs are kept 7 days from now. Waits while a cleanup is deleting it, then makes it again.
pub fn hold(dir: &Path) -> std::io::Result<LiveFolder> {
    use std::os::unix::fs::MetadataExt;
    loop {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let path = dir.join(LIVE_FILE);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&path)?;
        file.lock_shared()?;
        // A cleanup that held it exclusively has deleted the folder meanwhile: the lock is then on a file that's gone.
        let (held, now) = (file.metadata()?, std::fs::symlink_metadata(&path));
        if now.is_ok_and(|m| m.dev() == held.dev() && m.ino() == held.ino()) {
            let _ = std::fs::File::open(dir).and_then(|d| d.set_modified(SystemTime::now()));
            return Ok(LiveFolder { _lock: file });
        }
    }
}

/// Deletes a session's folder unless a live session holds it ([`hold`]); the exclusive lock is kept while deleting,
/// so a session starting to hold it waits and makes it again. Whether it was deleted.
fn remove_unless_held(dir: &Path) -> bool {
    let live = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(dir.join(LIVE_FILE));
    match live {
        Ok(file) => file.try_lock().is_ok() && std::fs::remove_dir_all(dir).is_ok(),
        Err(_) => false,
    }
}

/// Startup cleanup of the store (`~/.marv/tool-output`): sessions' folders not changed for `keep` go, then the
/// oldest ones until the rest is under `max_bytes`. A folder a live session holds ([`hold`]) is never deleted, nor
/// `current` (the session starting). Cheap: one stat per file; errors are skipped (a missing store is nothing to
/// clean).
pub fn clean_tool_output(base: &Path, keep: Duration, max_bytes: u64, current: Option<&str>, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(base) else { return };
    let mut sessions: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries.filter_map(Result::ok) {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if !meta.is_dir() {
            continue;
        }
        if current.is_some_and(|c| entry.file_name() == c) {
            total += folder_bytes(&entry.path());
            continue;
        }
        let changed = meta.modified().unwrap_or(now);
        if now.duration_since(changed).is_ok_and(|age| age > keep) && remove_unless_held(&entry.path()) {
            continue;
        }
        let bytes = folder_bytes(&entry.path());
        total += bytes;
        sessions.push((changed, bytes, entry.path()));
    }
    sessions.sort();
    for (_, bytes, path) in sessions {
        if total <= max_bytes {
            break;
        }
        if remove_unless_held(&path) {
            total -= bytes;
        }
    }
}

/// The size of the files directly in a session's folder (it has no subfolders).
fn folder_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(Result::ok).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn keeps_the_start_and_the_end_of_very_long_output() {
        let output: String = (0..5000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let text = cut_for_model(&output);
        assert!(text.chars().count() < 32_000);
        assert!(text.starts_with("line 0\n"));
        assert!(text.ends_with("line 4999"));
        assert!(text.contains(&format!("… ({} characters omitted) …", output.len() - MAX_MODEL_CHARS)));
    }

    #[test]
    fn short_output_is_untouched_and_multibyte_text_is_cut_on_char_boundaries() {
        assert_eq!(cut_middle("abc", 5, 2), "abc");
        assert_eq!(cut_middle("ééééééé", 4, 2), "éé\n\n… (3 characters omitted) …\n\néé");
    }

    #[test]
    fn a_cut_output_is_saved_whole_privately_and_named_in_the_marker() {
        let store = TempDir::new("spill");
        let dir = store.path().join("session-1");
        let output = format!("start\n{}\nend", "é".repeat(50_000));
        let text = spill_cut(&output, &dir, "call_0", Provenance::Project).unwrap();
        let path = dir.join("call_0.txt");
        assert!(text.starts_with("start\n") && text.ends_with("\nend"));
        let omitted = output.chars().count() - MAX_MODEL_CHARS;
        assert!(text.contains(&format!(
            "… ({omitted} characters omitted; full output: {} — read_file it, or grep it) …",
            path.display()
        )));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), output);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);

        // Ollama reuses call_0; an id that isn't a file name is made one.
        let again = spill_cut(&output, &dir, "call_0", Provenance::Project).unwrap();
        assert!(again.contains(&dir.join("call_0-1.txt").display().to_string()));
        let odd = spill_cut(&output, &dir, "../x/y", Provenance::Project).unwrap();
        assert!(odd.contains(&dir.join("___x_y.txt").display().to_string()));

        // Short output: untouched, nothing saved.
        assert_eq!(spill_cut("short", &dir, "call_9", Provenance::Mcp).unwrap(), "short");
        assert!(!dir.join("call_9.txt").exists());
    }

    #[test]
    fn without_a_store_or_when_the_save_fails_the_cut_is_the_plain_one() {
        let output = "x".repeat(40_000);
        let plain = cut_for_model(&output);
        assert_eq!(cut_for_call(&output, &ToolContext::new("/tmp"), Provenance::Project), plain);
        let store = TempDir::new("spill-fails");
        store.write("file", "not a folder");
        let ctx = ToolContext { tool_output: Some(store.path().join("file/s")), ..ToolContext::new("/tmp") };
        assert_eq!(cut_for_call(&output, &ctx, Provenance::Project), plain);
    }

    #[test]
    fn a_saved_outputs_name_keeps_its_source_and_a_read_gives_it_back() {
        let store = TempDir::new("spill-source");
        let dir = store.path().join("s");
        let output = "m".repeat(40_000);
        for (id, source, name) in [
            ("m1", Provenance::Mcp, "m1.mcp.txt"),
            ("m1", Provenance::Mcp, "m1-1.mcp.txt"),
            ("a1", Provenance::Subagent, "a1.subagent.txt"),
            ("b1", Provenance::Project, "b1.txt"),
            // A provider's id can't pass for a source: its dots are made underscores.
            ("x.project", Provenance::Mcp, "x_project.mcp.txt"),
        ] {
            let text = spill_cut(&output, &dir, id, source).unwrap();
            let path = dir.join(name);
            assert!(text.contains(&path.display().to_string()), "{name}: {text:.200}");
            assert_eq!(saved_provenance(&path), source, "{name}");
        }
        // A tag Marv doesn't know reads as the strictest source.
        assert_eq!(saved_provenance(Path::new("/s/y.what.txt")), Provenance::Mcp);
        assert_eq!(saved_provenance(Path::new("/s/.live")), Provenance::Mcp);
    }

    #[test]
    fn a_folder_a_live_session_holds_is_never_cleaned_up() {
        let store = TempDir::new("clean-held");
        for name in ["resumed", "running", "ended"] {
            store.write(&format!("{name}/call_0.txt"), "x".repeat(400));
        }
        let held: Vec<LiveFolder> = ["resumed", "running"].iter().map(|n| hold(&store.path().join(n)).unwrap()).collect();
        for name in ["resumed", "running", "ended"] {
            age(&store.path().join(name), 30);
        }
        let week = Duration::from_secs(KEEP_DAYS * 86_400);
        clean_tool_output(store.path(), week, 0, Some("new"), SystemTime::now());
        let left = |name: &str| store.path().join(name).join("call_0.txt").exists();
        assert!(left("resumed") && left("running"), "held folders survive both age and size");
        assert!(!left("ended"));

        // Once let go, they age out as any other; holding one marks it as just used.
        drop(held);
        let resumed = store.path().join("resumed");
        age(&resumed, 30);
        let _again = hold(&resumed).unwrap();
        let fresh = std::fs::metadata(&resumed).unwrap().modified().unwrap();
        assert!(SystemTime::now().duration_since(fresh).unwrap() < Duration::from_secs(60));
        clean_tool_output(store.path(), week, u64::MAX, None, SystemTime::now());
        assert!(left("resumed"));
        assert!(!store.path().join("running").exists(), "no longer held, 30 days old");

        // A folder held by this process too: the cleanup's lock is refused.
        let gone = store.path().join("gone");
        let _held = hold(&gone).unwrap();
        assert!(gone.join(LIVE_FILE).exists() && !remove_unless_held(&gone));
    }

    fn age(dir: &Path, days: u64) {
        let when = SystemTime::now() - Duration::from_secs(days * 86_400);
        std::fs::File::open(dir).unwrap().set_modified(when).unwrap();
    }

    #[test]
    fn old_sessions_are_cleaned_up_then_the_oldest_until_under_the_cap() {
        let store = TempDir::new("clean");
        for (name, days, bytes) in [("old", 8, 10), ("week", 6, 400), ("days", 3, 400), ("new", 0, 400), ("cur", 30, 400)] {
            store.write(&format!("{name}/call_0.txt"), "x".repeat(bytes));
            age(&store.path().join(name), days);
        }
        let week = Duration::from_secs(KEEP_DAYS * 86_400);
        clean_tool_output(store.path(), week, 1000, Some("cur"), SystemTime::now());
        let left = |name: &str| store.path().join(name).exists();
        // 8 days old: gone. Then 1600 bytes > 1000: the oldest ("week") goes, 1200 still over: "days" too.
        assert!(!left("old") && !left("week") && !left("days"));
        assert!(left("new"));
        assert!(left("cur"), "the session starting is never deleted, however old");

        clean_tool_output(&store.path().join("missing"), week, 1000, None, SystemTime::now()); // nothing to clean
    }
}

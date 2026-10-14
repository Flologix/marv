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
//! - [`clean_tool_output`] runs when a session starts: sessions' folders older than [`KEEP_DAYS`] go, then the oldest
//!   until the whole store is under [`MAX_TOTAL_BYTES`]. web_fetch isn't cut but paged (`offset`), so it saves nothing.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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
pub fn spill_cut(text: &str, dir: &Path, call_id: &str) -> std::io::Result<String> {
    if text.chars().count() <= MAX_MODEL_CHARS {
        return Ok(text.to_string());
    }
    let path = save(text, dir, call_id)?;
    Ok(cut_middle_saved(text, MAX_MODEL_CHARS, HEAD_CHARS, Some(&path)))
}

/// What a call's long result goes through: [`spill_cut`] into the session's folder (`ToolContext::tool_output`),
/// or the plain cut where there's none or the save fails.
pub fn cut_for_call(text: &str, ctx: &ToolContext) -> String {
    match &ctx.tool_output {
        Some(dir) => spill_cut(text, dir, ctx.call_id.as_deref().unwrap_or("call")).unwrap_or_else(|_| cut_for_model(text)),
        None => cut_for_model(text),
    }
}

/// Writes `text` to a new `<call id>.txt` in `dir` (0600; `-1`, `-2`… when the name is taken). Returns its path.
fn save(text: &str, dir: &Path, call_id: &str) -> std::io::Result<PathBuf> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // A provider's id is the model's text as far as a file name goes: only what's safe in one.
    let mut name: String =
        call_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(100).collect();
    if name.is_empty() {
        name = "call".into();
    }
    for n in 0.. {
        let path = if n == 0 { dir.join(format!("{name}.txt")) } else { dir.join(format!("{name}-{n}.txt")) };
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

/// Startup cleanup of the store (`~/.marv/tool-output`): sessions' folders not changed for `keep` go, then the
/// oldest ones until the rest is under `max_bytes`. `current` (the session starting) is never deleted. Cheap: one
/// stat per file; errors are skipped (a missing store is nothing to clean).
pub fn clean_tool_output(base: &Path, keep: Duration, max_bytes: u64, current: Option<&str>, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(base) else { return };
    let mut sessions: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if !meta.is_dir() || current.is_some_and(|c| entry.file_name() == c) {
            continue;
        }
        let changed = meta.modified().unwrap_or(now);
        if now.duration_since(changed).is_ok_and(|age| age > keep) {
            let _ = std::fs::remove_dir_all(entry.path());
            continue;
        }
        sessions.push((changed, folder_bytes(&entry.path()), entry.path()));
    }
    let current_bytes = current.map_or(0, |c| folder_bytes(&base.join(c)));
    let mut total: u64 = current_bytes + sessions.iter().map(|(_, bytes, _)| bytes).sum::<u64>();
    sessions.sort();
    for (_, bytes, path) in sessions {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
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
        let text = spill_cut(&output, &dir, "call_0").unwrap();
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
        let again = spill_cut(&output, &dir, "call_0").unwrap();
        assert!(again.contains(&dir.join("call_0-1.txt").display().to_string()));
        let odd = spill_cut(&output, &dir, "../x/y").unwrap();
        assert!(odd.contains(&dir.join("___x_y.txt").display().to_string()));

        // Short output: untouched, nothing saved.
        assert_eq!(spill_cut("short", &dir, "call_9").unwrap(), "short");
        assert!(!dir.join("call_9.txt").exists());
    }

    #[test]
    fn without_a_store_or_when_the_save_fails_the_cut_is_the_plain_one() {
        let output = "x".repeat(40_000);
        let plain = cut_for_model(&output);
        assert_eq!(cut_for_call(&output, &ToolContext::new("/tmp")), plain);
        let store = TempDir::new("spill-fails");
        store.write("file", "not a folder");
        let ctx = ToolContext { tool_output: Some(store.path().join("file/s")), ..ToolContext::new("/tmp") };
        assert_eq!(cut_for_call(&output, &ctx), plain);
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

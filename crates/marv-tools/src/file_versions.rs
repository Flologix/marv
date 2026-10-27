//! The version of each file a session has seen, so `edit_file` and `write_file` never write over a change it hasn't
//! seen (marv-rs; Marv has no equivalent).
//!
//! Why: the edit tools read a file, change it and write the whole file back. When something else changed the file
//! after the model last read it (another Marv session in the same folder, a subagent, the person in their editor, a
//! `bash` command), the model's change is built on text that's no longer there, and writing it silently drops the
//! other change. Marv's in-process file lock ([`crate::file_lock`]) only orders calls; it can't know what the model
//! has seen.
//!
//! The rule (compare-and-swap on content):
//! - a session remembers a content hash (SHA-256) per file it read with `read_file` (any page of it; a `grep` hit
//!   doesn't count) or wrote with `edit_file`/`write_file` (the file as written, after the formatter);
//! - `edit_file` and `write_file` on an **existing** file check the file's hash now against the session's, in the
//!   preview (so a stale call fails before anyone is asked) and again at apply time under the file lock, which also
//!   locks across processes, so two writers can't both pass the check. A file the session never read is refused the
//!   same way. A new file needs no read;
//! - a refusal writes nothing and tells the model to read the file again. marv-rs: when the session never read the
//!   file at all, the edit tools' refusal ([`FileVersions::check_showing`]) shows it as it is now instead (the whole
//!   file up to [`crate::read_file::DEFAULT_LIMIT`] lines, else that many around `old_string`), numbered like
//!   `read_file`, and that shown version is recorded as seen: the model now has it, as if it had called `read_file`,
//!   so its next call can succeed without one. In a benchmark run, 50 of 86 tool errors in Marv's
//!   evals were this refusal, for files the model had read with `bash` (`cat`, `sed -n`), each costing a step plus a
//!   `read_file`. Only the never-read case: a file that changed since the session saw it is still refused with no
//!   text, and must be read again. A secret's name (`.env*`, `*.pem`, `id_*`, which need approval to read) and a
//!   binary file are never shown: the plain refusal stands. Counting a `bash` read as seen was the other option; it
//!   was left out because proving what a command printed is harder than recording what Marv itself shows. When the session's own last write may not
//!   have landed as planned (it failed, or the file changed right after it), the refusal says so instead of blaming
//!   someone else ([`FileVersions::unsure`]).
//!
//! A partial read (`offset`/`limit`) counts as seen: the hash is of the whole file, so a change anywhere in it is still
//! caught, and an edit built on lines the model didn't page to fails as not found (a `write_file` replaces the whole file
//! either way, which is the model's own choice).
//! At most [`MAX_FILES`] files are remembered (least recently used first out); a forgotten one must be read again.
//!
//! The map is the conversation's, kept across its turns (in memory; /clear and a resume start empty). A subagent has its
//! own, so the parent's next write to a file its subagent changed is refused until the parent reads it again. After
//! /undo the files undone no longer match, which is what the undo note asks for ("Re-read files before editing
//! them").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::read_file::DEFAULT_LIMIT;
use crate::types::ToolError;

type Hash = [u8; 32];

/// At most this many files are remembered; past it the least recently used is forgotten (and must be read again
/// before it's written). A long session in a big repository stays bounded.
pub const MAX_FILES: usize = 10_000;

/// How the session came to hold a file's hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Read it, or wrote it and saw the result land.
    Seen,
    /// Its own write may not have landed as planned (the write failed, or something changed the file between the
    /// write and the formatter's run): the hash is what it meant to write, and a mismatch isn't someone else's fault.
    Unsure,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    hash: Hash,
    origin: Origin,
    /// When it was last used, for the cap.
    used: u64,
}

#[derive(Debug)]
struct Inner {
    files: HashMap<PathBuf, Entry>,
    clock: u64,
    cap: usize,
}

/// What the session last saw of each file, by real path.
#[derive(Debug)]
pub struct FileVersions(Mutex<Inner>);

impl Default for FileVersions {
    fn default() -> Self {
        Self::with_cap(MAX_FILES)
    }
}

/// A file's content hash.
pub fn content_hash(content: &[u8]) -> Hash {
    Sha256::digest(content).into()
}

/// The key a file is remembered by: its real path when it exists (a symlink and its target are one file).
fn key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| marv_sandbox::paths::normalize(path))
}

impl FileVersions {
    fn with_cap(cap: usize) -> Self {
        Self(Mutex::new(Inner { files: HashMap::new(), clock: 0, cap }))
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, path: &Path, content: &[u8], origin: Origin) {
        let mut inner = self.inner();
        inner.clock += 1;
        let used = inner.clock;
        inner.files.insert(key(path), Entry { hash: content_hash(content), origin, used });
        if inner.files.len() > inner.cap
            && let Some(oldest) = inner.files.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone())
        {
            inner.files.remove(&oldest);
        }
    }

    /// The session read or wrote `path`, which now holds `content`.
    pub fn saw(&self, path: &Path, content: &[u8]) {
        self.record(path, content, Origin::Seen);
    }

    /// The session's own write to `path`, meant to leave `content`, may not have landed as planned: a later mismatch
    /// is refused without blaming anyone else.
    pub fn unsure(&self, path: &Path, content: &[u8]) {
        self.record(path, content, Origin::Unsure);
    }

    /// After the session's write of `wrote` to `path` and the formatter's run: the version it may now write over.
    pub fn wrote(&self, path: &Path, wrote: &str, formatted: &crate::formatter::Formatted) {
        match formatted.seen(wrote) {
            (version, true) => self.saw(path, version.as_bytes()),
            (version, false) => self.unsure(path, version.as_bytes()),
        }
    }

    /// A new conversation: nothing has been seen.
    pub fn forget_all(&self) {
        self.inner().files.clear();
    }

    /// Whether the session has seen `path` as it is now (`current`).
    pub fn is_current(&self, path: &Path, current: &[u8]) -> bool {
        self.inner().files.get(&key(path)).is_some_and(|e| e.hash == content_hash(current))
    }

    /// Whether the session may write over `path`, which holds `current`.
    fn verdict(&self, path: &Path, current: &[u8]) -> Verdict {
        let mut inner = self.inner();
        inner.clock += 1;
        let now = inner.clock;
        let Some(entry) = inner.files.get_mut(&key(path)) else { return Verdict::NeverRead };
        entry.used = now;
        match (entry.hash == content_hash(current), entry.origin) {
            (true, _) => Verdict::Current,
            (false, Origin::Seen) => Verdict::Changed,
            (false, Origin::Unsure) => Verdict::Unsure,
        }
    }

    /// Ok when the session may write over `path` (shown as `shown`), which exists and holds `current`; otherwise
    /// the refusal the model reads.
    pub fn check(&self, path: &Path, shown: &str, current: &[u8]) -> Result<(), ToolError> {
        match self.verdict(path, current) {
            Verdict::Current => Ok(()),
            Verdict::NeverRead => Err(ToolError(never_read(shown))),
            Verdict::Changed => Err(ToolError(changed_since_read(shown))),
            Verdict::Unsure => Err(ToolError(not_as_you_left_it(shown))),
        }
    }

    /// [`Self::check`] for the edit tools: a file the session never read is refused all the same (nothing is written),
    /// but the refusal shows it as it is now ([`unread_page`]: around `near`, edit_file's `old_string`, when the file
    /// is long), and that version counts as seen, as a `read_file` of it would. Only what is shown is recorded, from the
    /// same `current` the check looked at, so a change after this is still refused. A secret or a binary file isn't
    /// shown, and a file that changed since the session saw it gets the ordinary refusal: the stale-write rule is
    /// unchanged (docs/SECURITY.md).
    pub fn check_showing(&self, path: &Path, shown: &str, current: &str, near: Option<&str>) -> Result<(), ToolError> {
        if self.verdict(path, current.as_bytes()) != Verdict::NeverRead {
            return self.check(path, shown, current.as_bytes());
        }
        match unread_page(path, shown, current, near) {
            Some(page) => {
                self.saw(path, current.as_bytes());
                Err(ToolError(never_read_shown(shown, &page)))
            }
            None => Err(ToolError(never_read(shown))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Current,
    NeverRead,
    Changed,
    Unsure,
}

/// The page of a never-read file its refusal shows, numbered like `read_file`: the whole file up to
/// [`DEFAULT_LIMIT`] lines; a longer one from a quarter page before `near` (its exact match, else its closest lines),
/// or from the top. None for a secret's name or a binary file: those get the plain refusal.
fn unread_page(path: &Path, shown: &str, current: &str, near: Option<&str>) -> Option<String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if marv_sandbox::protected::is_secret_name(&name) || current.contains('\0') {
        return None;
    }
    let total = crate::write_file::line_count(current);
    let line = near.filter(|s| !s.is_empty()).and_then(|near| match current.find(near) {
        Some(at) => Some(current[..at].matches('\n').count() + 1),
        None => crate::edit_match::closest_lines(current, near).map(|(start, _)| start),
    });
    let offset = match line {
        Some(line) if total > DEFAULT_LIMIT => {
            line.saturating_sub(DEFAULT_LIMIT / 4).clamp(1, total.saturating_sub(DEFAULT_LIMIT) + 1)
        }
        _ => 1,
    };
    crate::read_file::render(current, offset, DEFAULT_LIMIT, shown).ok().map(|out| out.output)
}

/// The refusal for an existing file the session never read, with the file as it is now (marv-rs).
pub fn never_read_shown(shown: &str, page: &str) -> String {
    format!(
        "{shown} already exists and you hadn't read it in this conversation, so nothing was written. Here it is as it is now (this counts as reading it):\n{page}\n\nMake your change on this text: if your call already matches it, send the same call again."
    )
}

/// The refusal for an existing file the session never read (or read so long ago it was forgotten).
pub fn never_read(shown: &str) -> String {
    format!(
        "{shown} already exists and you haven't read it in this conversation, so nothing was written. Read it with read_file first, then make your change."
    )
}

/// The refusal for a file that changed since the session last read or wrote it.
pub fn changed_since_read(shown: &str) -> String {
    format!(
        "{shown} changed since you last read it (another Marv session, a command or the user changed it), so nothing was written: your change would have dropped theirs. Read it again with read_file, then make your change on the current text."
    )
}

/// The refusal for a file the session's own last change may not have left as planned.
pub fn not_as_you_left_it(shown: &str) -> String {
    format!(
        "{shown} isn't as your last change meant to leave it (that write may not have landed whole, or the file changed right after it), so nothing was written. Read it again with read_file, then make your change on the current text."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn a_file_is_current_only_as_it_was_seen() {
        let root = TempDir::new("versions");
        root.write("a.txt", "one\n");
        let path = root.path().join("a.txt");
        let seen = FileVersions::default();
        assert_eq!(seen.check(&path, "a.txt", b"one\n").unwrap_err().0, never_read("a.txt"));
        seen.saw(&path, b"one\n");
        assert!(seen.check(&path, "a.txt", b"one\n").is_ok());
        assert_eq!(seen.check(&path, "a.txt", b"two\n").unwrap_err().0, changed_since_read("a.txt"));
        // The same file by another spelling of its path.
        assert!(seen.is_current(&root.path().join("./x/../a.txt"), b"one\n"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_and_its_target_are_one_file() {
        let root = TempDir::new("versions-link");
        root.write("a.txt", "one\n");
        std::os::unix::fs::symlink(root.path().join("a.txt"), root.path().join("b.txt")).unwrap();
        let seen = FileVersions::default();
        seen.saw(&root.path().join("b.txt"), b"one\n");
        assert!(seen.is_current(&root.path().join("a.txt"), b"one\n"));
    }

    #[test]
    fn the_session_s_own_unsure_write_doesnt_blame_anyone() {
        let root = TempDir::new("versions-unsure");
        let path = root.path().join("a.txt");
        let seen = FileVersions::default();
        seen.unsure(&path, b"planned\n");
        assert!(seen.check(&path, "a.txt", b"planned\n").is_ok(), "it landed after all");
        assert_eq!(seen.check(&path, "a.txt", b"half\n").unwrap_err().0, not_as_you_left_it("a.txt"));
    }

    #[test]
    fn a_never_read_file_is_refused_with_its_text_and_then_counts_as_seen() {
        let root = TempDir::new("versions-shown");
        root.write("a.txt", "one\ntwo\n");
        let path = root.path().join("a.txt");
        let seen = FileVersions::default();
        let err = seen.check_showing(&path, "a.txt", "one\ntwo\n", Some("two")).unwrap_err().0;
        assert_eq!(err, never_read_shown("a.txt", "    1→one\n    2→two"));
        // What was shown is the version the session has now: the retry passes.
        assert!(seen.check_showing(&path, "a.txt", "one\ntwo\n", Some("two")).is_ok());
    }

    #[test]
    fn a_file_changed_after_it_was_shown_or_read_is_still_refused_without_its_text() {
        // The stale-write rule is unchanged: only the never-read case shows the file.
        let root = TempDir::new("versions-shown-stale");
        let path = root.path().join("a.txt");
        let seen = FileVersions::default();
        assert!(seen.check_showing(&path, "a.txt", "one\n", None).is_err());
        assert_eq!(seen.check_showing(&path, "a.txt", "theirs\n", None).unwrap_err().0, changed_since_read("a.txt"));
        seen.unsure(&path, b"planned\n");
        assert_eq!(seen.check_showing(&path, "a.txt", "half\n", None).unwrap_err().0, not_as_you_left_it("a.txt"));
    }

    #[test]
    fn a_secret_or_a_binary_file_is_never_shown() {
        let root = TempDir::new("versions-shown-secret");
        let seen = FileVersions::default();
        for name in [".env", ".env.local", "server.pem", "id_ed25519"] {
            let path = root.path().join(name);
            let err = seen.check_showing(&path, name, "TOKEN=abc\n", None).unwrap_err().0;
            assert_eq!(err, never_read(name));
            assert!(!err.contains("TOKEN"));
            assert_eq!(seen.check_showing(&path, name, "TOKEN=abc\n", None).unwrap_err().0, never_read(name), "not seen");
        }
        let path = root.path().join("blob.bin");
        assert_eq!(seen.check_showing(&path, "blob.bin", "a\0b", None).unwrap_err().0, never_read("blob.bin"));
    }

    #[test]
    fn a_long_file_is_shown_around_old_string() {
        let root = TempDir::new("versions-shown-long");
        let path = root.path().join("long.txt");
        let text: String = (1..=1000).map(|n| format!("line {n}\n")).collect();
        let seen = FileVersions::default();
        let err = seen.check_showing(&path, "long.txt", &text, Some("line 700\n")).unwrap_err().0;
        assert!(err.contains("  700→line 700") && err.contains("  600→line 600"), "{err}");
        assert!(!err.contains("    1→line 1\n") && err.contains("(Showing lines 600-999 of 1000."), "{err}");
        // Near the end the page still holds DEFAULT_LIMIT lines; with nothing to go by, the top.
        let seen = FileVersions::default();
        let err = seen.check_showing(&path, "long.txt", &text, Some("line 1000")).unwrap_err().0;
        assert!(err.contains("(Showing lines 601-1000 of 1000.)"), "{err}");
        let seen = FileVersions::default();
        let err = seen.check_showing(&path, "long.txt", &text, None).unwrap_err().0;
        assert!(err.contains("(Showing lines 1-400 of 1000."), "{err}");
    }

    #[test]
    fn past_the_cap_the_least_recently_used_file_is_forgotten() {
        let root = TempDir::new("versions-cap");
        let seen = FileVersions::with_cap(2);
        let (a, b, c) = (root.path().join("a"), root.path().join("b"), root.path().join("c"));
        seen.saw(&a, b"a");
        seen.saw(&b, b"b");
        assert!(seen.check(&a, "a", b"a").is_ok()); // a is used again: b is now the oldest
        seen.saw(&c, b"c");
        assert_eq!(seen.check(&b, "b", b"b").unwrap_err().0, never_read("b"));
        assert!(seen.check(&a, "a", b"a").is_ok() && seen.check(&c, "c", b"c").is_ok());
    }
}

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
//! - a refusal writes nothing and tells the model to read the file again. When the session's own last write may not
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

    /// Ok when the session may write over `path` (shown as `shown`), which exists and holds `current`; otherwise
    /// the refusal the model reads.
    pub fn check(&self, path: &Path, shown: &str, current: &[u8]) -> Result<(), ToolError> {
        let mut inner = self.inner();
        inner.clock += 1;
        let now = inner.clock;
        let Some(entry) = inner.files.get_mut(&key(path)) else { return Err(ToolError(never_read(shown))) };
        entry.used = now;
        match (entry.hash == content_hash(current), entry.origin) {
            (true, _) => Ok(()),
            (false, Origin::Seen) => Err(ToolError(changed_since_read(shown))),
            (false, Origin::Unsure) => Err(ToolError(not_as_you_left_it(shown))),
        }
    }
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

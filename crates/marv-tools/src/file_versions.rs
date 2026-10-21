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
//! - a refusal writes nothing and tells the model to read the file again.
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

/// What the session last saw of each file, by real path.
#[derive(Debug, Default)]
pub struct FileVersions(Mutex<HashMap<PathBuf, Hash>>);

/// A file's content hash.
pub fn content_hash(content: &[u8]) -> Hash {
    Sha256::digest(content).into()
}

/// The key a file is remembered by: its real path when it exists (a symlink and its target are one file).
fn key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| marv_sandbox::paths::normalize(path))
}

impl FileVersions {
    /// The session read or wrote `path`, which now holds `content`.
    pub fn saw(&self, path: &Path, content: &[u8]) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(key(path), content_hash(content));
    }

    /// A new conversation: nothing has been seen.
    pub fn forget_all(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Whether the session has seen `path` as it is now (`current`).
    pub fn is_current(&self, path: &Path, current: &[u8]) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&key(path)) == Some(&content_hash(current))
    }

    /// Ok when the session may write over `path` (shown as `shown`), which exists and holds `current`; otherwise
    /// the refusal the model reads.
    pub fn check(&self, path: &Path, shown: &str, current: &[u8]) -> Result<(), ToolError> {
        let seen = self.0.lock().unwrap_or_else(|e| e.into_inner()).get(&key(path)).copied();
        match seen {
            Some(hash) if hash == content_hash(current) => Ok(()),
            Some(_) => Err(ToolError(changed_since_read(shown))),
            None => Err(ToolError(never_read(shown))),
        }
    }
}

/// The refusal for an existing file the session never read.
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
}

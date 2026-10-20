//! The file system as the sandbox sees it, injectable for tests: what every backend checks paths against before it
//! decides what a command may touch (`FsProbe`), and the real one (`RealFs`).
//!
//! Shared by every [`crate::backend::SandboxBackend`]: the policy ([`crate::plan`]) and each renderer take a
//! `&dyn FsProbe`, so the bubblewrap arguments and (later) the Seatbelt profile are tested against a fake file system
//! on any platform.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// What a path itself is, not following a final symlink (`lstatKind`). Folders and special files are `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Symlink,
}

/// What a folder entry is, not following a symlink: what [`FsProbe::entries`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    /// A Unix socket or a FIFO: a way to talk to a process outside the sandbox, which a read-only mount doesn't close.
    Channel,
    /// A device or anything else.
    Other,
}

/// The file system, injectable for tests.
pub trait FsProbe {
    /// A folder's entries with what each is (not following symlinks).
    fn entries(&self, dir: &Path) -> io::Result<Vec<(std::ffi::OsString, EntryKind)>>;
    /// What a path itself is (not following a final symlink); None if it doesn't exist.
    fn entry_kind(&self, path: &Path) -> Option<EntryKind>;
    /// Whether a path exists (following symlinks).
    fn exists(&self, path: &Path) -> bool;
    /// What the path itself is.
    fn kind(&self, path: &Path) -> Option<FileKind>;
    /// Where it really leads (an error if it leads nowhere).
    fn realpath(&self, path: &Path) -> io::Result<PathBuf>;
}

/// The real file system.
pub struct RealFs;

fn entry_kind_of(file_type: std::fs::FileType) -> EntryKind {
    use std::os::unix::fs::FileTypeExt;
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_socket() || file_type.is_fifo() {
        EntryKind::Channel
    } else {
        EntryKind::Other
    }
}

impl FsProbe for RealFs {
    fn entries(&self, dir: &Path) -> io::Result<Vec<(OsString, EntryKind)>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            out.push((entry.file_name(), entry.file_type().map(entry_kind_of)?));
        }
        out.sort();
        Ok(out)
    }
    fn entry_kind(&self, path: &Path) -> Option<EntryKind> {
        std::fs::symlink_metadata(path).ok().map(|m| entry_kind_of(m.file_type()))
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn kind(&self, path: &Path) -> Option<FileKind> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() {
            Some(FileKind::Symlink)
        } else if meta.is_file() {
            Some(FileKind::File)
        } else {
            None
        }
    }
    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
}

//! A fake file system for the sandbox's unit tests (bwrap's arguments, the plan's policy and, later, the Seatbelt
//! profile): `exists` by predicate, `kind` by table, `realpath` by table (identity otherwise), folder listings.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use crate::fs::{EntryKind, FileKind, FsProbe};

/// A fake file system: `exists` by predicate, `kind` by table, `realpath` by table (identity otherwise).
pub(crate) struct Fake {
    pub(crate) exists: Box<dyn Fn(&str) -> bool>,
    pub(crate) kinds: HashMap<String, FileKind>,
    pub(crate) links: HashMap<String, String>,
    pub(crate) dangling: bool,
    /// Folder listings (a folder not listed is empty), and what's in them.
    pub(crate) dirs: HashMap<String, Vec<(String, EntryKind)>>,
    pub(crate) unlistable: bool,
}
impl Fake {
    pub(crate) fn new(exists: impl Fn(&str) -> bool + 'static) -> Self {
        Self {
            exists: Box::new(exists),
            kinds: HashMap::new(),
            links: HashMap::new(),
            dangling: false,
            dirs: HashMap::new(),
            unlistable: false,
        }
    }
    /// A file system from listings alone: every listed entry exists, as what it's listed as.
    pub(crate) fn listed(dirs: &[(&str, &[(&str, EntryKind)])]) -> Self {
        let mut all: HashMap<String, EntryKind> = HashMap::new();
        for (dir, list) in dirs {
            all.insert((*dir).into(), EntryKind::Dir);
            for (name, kind) in *list {
                all.insert(format!("{dir}/{name}"), *kind);
            }
        }
        let names: Vec<String> = all.keys().cloned().collect();
        let mut fs = Fake::new(move |p| names.iter().any(|n| n == p));
        for (path, kind) in &all {
            match kind {
                EntryKind::File => drop(fs.kinds.insert(path.clone(), FileKind::File)),
                EntryKind::Symlink => drop(fs.kinds.insert(path.clone(), FileKind::Symlink)),
                _ => {}
            }
        }
        fs.dirs = dirs.iter().map(|(d, l)| ((*d).into(), l.iter().map(|(n, k)| ((*n).into(), *k)).collect())).collect();
        fs
    }
    fn listed_kind(&self, path: &str) -> Option<EntryKind> {
        let (dir, name) = path.rsplit_once('/')?;
        let dir = if dir.is_empty() { "/" } else { dir };
        self.dirs.get(dir)?.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
    }
}
impl FsProbe for Fake {
    fn entries(&self, dir: &Path) -> io::Result<Vec<(OsString, EntryKind)>> {
        if self.unlistable {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(self.dirs.get(&*dir.to_string_lossy()).cloned().unwrap_or_default().into_iter().map(|(n, k)| (n.into(), k)).collect())
    }
    fn entry_kind(&self, path: &Path) -> Option<EntryKind> {
        let p = path.to_string_lossy();
        if let Some(kind) = self.listed_kind(&p) {
            return Some(kind);
        }
        match self.kinds.get(&*p) {
            Some(FileKind::File) => Some(EntryKind::File),
            Some(FileKind::Symlink) => Some(EntryKind::Symlink),
            None => self.exists(path).then_some(EntryKind::Dir),
        }
    }
    fn exists(&self, path: &Path) -> bool {
        (self.exists)(&path.to_string_lossy())
    }
    fn kind(&self, path: &Path) -> Option<FileKind> {
        self.kinds.get(&*path.to_string_lossy()).copied()
    }
    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        let p = path.to_string_lossy();
        if self.dangling && self.kinds.get(&*p) == Some(&FileKind::Symlink) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(PathBuf::from(self.links.get(&*p).cloned().unwrap_or_else(|| p.into_owned())))
    }
}

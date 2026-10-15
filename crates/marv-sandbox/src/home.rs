//! The stand-in home: the home folder is an empty tmpfs inside the sandbox, so secrets there (`~/.ssh`, `~/.marv`
//! with the API key, browser profiles…) can't be read, except a few toolchain folders mounted read-only, with the
//! credential files inside them hidden.
//!
//! Ports `TOOLCHAINS`, `hiddenPathFolders`, `CREDENTIALS` and `credentialMasks` from Marv's `src/sandbox.ts`.
//!
//! - Tokens inside whole config files (`~/.gitconfig`, `~/.config/git/config`, `~/.cargo/config.toml`) can't be
//!   hidden this way: the sandbox needs those files whole.
//! - bwrap can't mount on a symlink (and fails before the command starts), so for a symlinked credential file
//!   (stow, chezmoi) the file it leads to is masked instead, wherever a toolchain mount makes it visible.
//! - A `PATH` folder under the hidden home that no toolchain shows is reported at startup and by `/sandbox`: a tool
//!   whose folder is hidden (`env: 'node': No such file or directory`, exit 127) is otherwise a silent mystery, and
//!   silently skips Marv's own post-edit typecheck.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::bwrap::{FileKind, FsProbe};

/// Folders from the hidden home folder a command may need, mounted read-only.
pub const TOOLCHAINS: &[&str] = &[
    ".bun",
    ".cargo",
    ".rustup",
    ".local/bin",
    ".local/share/pnpm",
    ".local/share/mise",
    ".asdf",
    ".nvm",
    ".volta",
    ".deno",
    ".pyenv",
    ".gitconfig",
    ".config/git",
    // Personal skills, so their scripts can run (the rest of ~/.marv, with the API key, stays hidden).
    ".marv/skills",
];

/// Credential files inside the toolchain folders, replaced by `/dev/null`: an approved (or auto-approved) command
/// could otherwise print them into the conversation, which goes to the model provider.
pub const CREDENTIALS: &[&str] = &[".cargo/credentials", ".cargo/credentials.toml", ".config/git/credentials"];

/// Home entries that never show in the sandbox, even when the project is the home folder (or holds it) and the
/// home is shown read-only: Marv's config, credentials, and agent and tool state with sockets in it. Every hidden
/// top-level entry of the home is already hidden in that case ([`crate::bwrap`]); this list keeps these hidden even
/// if a toolchain mount (or a later change to [`TOOLCHAINS`]) would show them again.
pub const HOME_MASKS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".config/gh",
    ".config/herdr",
    ".claude",
    ".claude.json",
    ".docker",
    ".kube",
    ".local/share/keyrings",
];

/// Whether the project is the home folder or a parent of it (`/`, `/home`), judged on real paths. Then the sandbox
/// can't hide the home behind a stand-in without hiding the project too: it shows it read-only, with Marv's config,
/// credentials and every hidden top-level entry masked ([`crate::bwrap::sandbox_args`]).
pub fn home_project(root: &Path, home: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    contains(&real(root), &real(home))
}

/// Whether `outer` is `inner` or a parent of it.
pub(crate) fn contains(outer: &Path, inner: &Path) -> bool {
    inner.starts_with(outer)
}

/// PATH folders under the hidden home folder that no toolchain mount shows. Not the mounted ones, not inside the
/// project, not folders that don't exist (nothing to miss). Sorted, so a notice lists them in a stable order.
pub fn hidden_path_folders(path: &str, home: &Path, root: Option<&Path>, exists: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut found = BTreeSet::new();
    for dir in path.split(':') {
        let dir = Path::new(dir);
        if !dir.is_absolute() || !contains(home, dir) || !exists(dir) {
            continue;
        }
        // The project is mounted (writable): its node_modules/.bin or bin/ aren't hidden.
        if root.is_some_and(|root| contains(root, dir)) {
            continue;
        }
        if TOOLCHAINS.iter().any(|t| contains(&home.join(t), dir)) {
            continue;
        }
        found.insert(dir.to_path_buf());
    }
    found.into_iter().collect()
}

/// Where to put `/dev/null` so the sandbox can't read a credential file, as paths inside the sandbox. A plain file is
/// masked where it is; for a symlink, the file it leads to, wherever a toolchain mount shows it (the real path has no
/// symlinks below the mount point, so the mask is never on a symlink). A link leading anywhere else already reads as
/// missing, because the home is hidden. Anything else (dangling, a folder) is left alone.
pub fn credential_masks(file: &Path, mounts: &[PathBuf], fs: &dyn FsProbe) -> Vec<PathBuf> {
    match fs.kind(file) {
        Some(FileKind::File) => return vec![file.to_path_buf()],
        Some(FileKind::Symlink) => {}
        None => return vec![],
    }
    let Ok(target) = fs.realpath(file) else { return vec![] };
    if fs.kind(&target) != Some(FileKind::File) {
        return vec![];
    }
    let mut masks = Vec::new();
    for mount in mounts {
        let Ok(source) = fs.realpath(mount) else { continue };
        if let Ok(rel) = target.strip_prefix(&source) {
            masks.push(mount.join(rel));
        }
    }
    masks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_path_folders_are_the_ones_no_toolchain_shows() {
        // mise's installs, asdf's shims and .bun's bin are mounted (TOOLCHAINS); rbenv's and a private tools folder
        // aren't, and a missing folder has nothing to miss.
        let home = Path::new("/home/me");
        let path = "/home/me/.rbenv/shims:/home/me/.local/share/mise/installs/node/26.8.2/bin:/home/me/.asdf/shims:/home/me/.bun/bin:/home/me/.tools/bin:/usr/bin:/home/me/gone/bin";
        let not_gone = |p: &Path| !p.to_string_lossy().contains("gone");
        assert_eq!(
            hidden_path_folders(path, home, None, &not_gone),
            vec![PathBuf::from("/home/me/.rbenv/shims"), PathBuf::from("/home/me/.tools/bin")]
        );
        assert!(hidden_path_folders("/usr/bin:/bin", home, None, &|_| true).is_empty());
        assert!(hidden_path_folders("relative/bin:/home/me/.bun/bin/node", home, None, &|_| true).is_empty());
        // The project is mounted: its own bin folders aren't hidden.
        assert_eq!(
            hidden_path_folders(
                "/home/me/app/node_modules/.bin:/home/me/app/bin:/home/me/.tools/bin",
                home,
                Some(Path::new("/home/me/app")),
                &|_| true
            ),
            vec![PathBuf::from("/home/me/.tools/bin")]
        );
    }
}

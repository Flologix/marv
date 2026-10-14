//! Git checkpoints and /undo (Marv #16). Ports Marv's `src/checkpoints/`.
//!
//! Every turn that changes files gets a *before* and an *after* snapshot ([`turn::CheckpointTurn`]), written by
//! [`store::CheckpointStore`] as git commits under a hidden ref, and `/undo` ([`undo`]) puts the paths that differ
//! between them back. The user's index, HEAD, branches and stash are never touched.

pub mod store;
pub mod turn;
pub mod undo;

#[cfg(test)]
mod session_tests;

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    pub(crate) use crate::worktree::tests::{Temp, sh_git as git};

    /// A temp folder, removed on drop.
    pub(crate) fn temp(tag: &str) -> Temp {
        Temp::new(tag)
    }

    /// A repository with one commit (`a.txt` "one", `.gitignore` ignoring ignored.txt and build/); `email: false`
    /// leaves user.email unset (the commit gets it from -c).
    pub(crate) fn make_repo(email: bool) -> Temp {
        let dir = temp("cp-repo");
        git(&dir.0, &["init", "-q", "-b", "main"]);
        if email {
            git(&dir.0, &["config", "user.email", "t@example.com"]);
            git(&dir.0, &["config", "user.name", "T"]);
        }
        git(&dir.0, &["config", "commit.gpgSign", "false"]);
        std::fs::write(dir.0.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.0.join(".gitignore"), "ignored.txt\nbuild/\n").unwrap();
        git(&dir.0, &["add", "-A"]);
        git(&dir.0, &["-c", "user.email=t@example.com", "-c", "user.name=T", "commit", "-q", "-m", "first"]);
        dir
    }

    pub(crate) fn write(path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    pub(crate) fn read(path: impl AsRef<Path>) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    pub(crate) fn tree_files(cwd: &Path, sha: &str) -> Vec<String> {
        let mut files: Vec<String> =
            git(cwd, &["ls-tree", "-r", "--name-only", sha]).lines().filter(|l| !l.is_empty()).map(String::from).collect();
        files.sort();
        files
    }
}

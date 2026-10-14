//! marv-sandbox: where commands run and which paths an agent may touch.
//!
//! Ports `src/sandbox.ts` (the bubblewrap argument builder, the stand-in home, credential masks, `/run`), the
//! path rules of `src/tools/files.ts` (`resolveInProject`, `touchesGit`, `refuseGit`), the `.git` guard and the
//! notes of `src/tools/bash.ts` (`gitGuard`, `worktreeGitDirs`, `HOME_NOTE`, `NETWORK_NOTE`, `GH_NOTE`), plus the
//! marv-rs security model from `docs/SECURITY.md`: protected paths and taint.
//!
//! Approval decides *whether* a command runs; the sandbox limits *what an approved command can touch*.
//!
//! Modules:
//! - [`paths`]: every path is checked after `realpath`; nothing follows a symlink out of its root (skills included).
//! - [`protected`]: paths that ask even in yolo and are read-only inside the sandbox.
//! - [`home`]: the stand-in home: toolchain folders, credential masks, hidden PATH folders.
//! - [`host_env`]: the environment of what runs outside the sandbox (host commands, gh), secrets stripped.
//! - [`bwrap`]: the bubblewrap argument builder (and the mesh plug at `/run/marv/mesh.sock`).
//! - [`git_guard`]: `.git` read-only or a placeholder for commands that run without asking; worktree records.
//! - [`notes`]: the `Marv:` notes bash appends (stand-in home, missing network, gh, read-only .git).
//! - [`taint`]: once web/MCP/mesh/image text is in the context, egress always asks.

pub mod bwrap;
pub mod git_guard;
pub mod home;
pub mod host_env;
pub mod notes;
pub mod paths;
pub mod protected;
pub mod taint;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub use bwrap::{Bind, FileKind, FsProbe, MESH_SOCKET, RealFs, SandboxOptions, sandbox_args, sandbox_available};
pub use paths::{project_path, resolve_in_project, resolve_within, touches_git};
pub use protected::{PROTECTED_PATHS, ProtectedPath, ProtectedRule, is_protected, protected_rule};
pub use taint::TaintState;

/// Whether every sandbox hides `dir`: it is (by its real path) under `/run`, `/tmp` or the home folder, which each
/// command sees as empty tmpfs. The mesh keeps its sockets only in such a folder: a socket a sandboxed command could
/// see is one it could connect to (a read-only mount doesn't stop `connect()`), and a plug's socket carries that
/// plug's identity and wires. A runtime dir elsewhere (`XDG_RUNTIME_DIR=/var/tmp/…`) is refused, not used.
pub fn hidden_in_sandbox(dir: &std::path::Path, home: &std::path::Path) -> bool {
    let real = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let dir = real(dir);
    if !dir.is_absolute() {
        return false;
    }
    let home = real(home);
    let under = |base: &std::path::Path| dir.starts_with(base) && dir != base;
    under(std::path::Path::new("/run"))
        || under(std::path::Path::new("/tmp"))
        || (home != std::path::Path::new("/") && under(&home))
}

/// Why a path or a mount was refused. The messages go to the model as they are (tools wrap them in a
/// `ToolError`), so they say what to do instead.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// Ported message from `resolveInProject`.
    #[error("\"{path}\" is outside the project ({root}). Use a path relative to the project root, like \"src/app.ts\".")]
    OutsideProject { path: String, root: String },
    /// A file outside a folder it must stay in (a skill's folder), lexically or through a symlink.
    #[error("\"{path}\" is outside {within}.")]
    OutsideDir { path: String, within: String },
    #[error("\"{0}\" doesn't exist.")]
    Missing(String),
    /// `refuseGit`: a change that runs without asking leads into `.git`.
    #[error("\"{0}\" leads into .git, which changes only with the user's approval. Try again; they'll be asked.")]
    IntoGit(String),
    /// An extra folder that can't be mounted safely (`resolveExtra`).
    #[error("{0}")]
    Mount(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The session's sandbox: whether it's on, and what every command's bwrap call shares. Cheap to clone; held by
/// `marv_tools::ToolContext` behind an `Arc`.
#[derive(Debug, Clone)]
pub struct Sandbox {
    /// `/sandbox on|off`. Off: commands run directly under `setsid` (still approved, with a warning).
    pub enabled: bool,
    /// The real home folder, replaced by an empty tmpfs inside the sandbox.
    pub home: PathBuf,
    /// `PATH` inside the sandbox (the host's).
    pub path: String,
    /// `LANG` inside the sandbox (the host's, else `C.UTF-8`).
    pub lang: String,
    /// The mesh plug's host-side socket, bound at [`MESH_SOCKET`] (`docs/MESH.md`). None: no mesh.
    pub mesh_socket: Option<PathBuf>,
    /// Folders every command sees empty: Marv's runtime folder (`$XDG_RUNTIME_DIR/marv`, see `SandboxOptions::hidden`).
    pub hidden: Vec<PathBuf>,
}

/// One command's mounts beyond the session's: see [`SandboxOptions`] for what each means.
#[derive(Debug, Clone, Default)]
pub struct CommandMounts {
    pub network: bool,
    pub read_only: Vec<PathBuf>,
    pub writable: Vec<PathBuf>,
    pub placeholders: Vec<PathBuf>,
    pub project_read_only: bool,
    /// Mounts of their own (the process door's relay, Marv #51).
    pub binds: Vec<Bind>,
}

impl Sandbox {
    /// From the real environment (`HOME`, `PATH`, `LANG`), never from a file in the project.
    pub fn from_env(enabled: bool) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        Self {
            enabled,
            home,
            path: std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            lang: std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()),
            mesh_socket: None,
            hidden: std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|d| d.is_absolute())
                .map(|d| vec![d.join("marv")])
                .unwrap_or_default(),
        }
    }

    /// Whether commands really run sandboxed: on, and bwrap works here.
    pub fn active(&self) -> bool {
        self.enabled && sandbox_available()
    }

    /// The full argv for `script` in the project: `bwrap <args> -- bash -c script`. Protected paths that exist
    /// are mounted read-only (`protected::protected_mounts`).
    pub fn command(&self, root: &Path, mounts: &CommandMounts, script: &str) -> Result<Vec<OsString>, SandboxError> {
        self.command_argv(root, mounts, &["bash".into(), "-c".into(), script.into()])
    }

    /// The full argv for `inner` in the project: `bwrap <args> -- inner…` (the process door runs its relay there).
    pub fn command_argv(&self, root: &Path, mounts: &CommandMounts, inner: &[OsString]) -> Result<Vec<OsString>, SandboxError> {
        let options = SandboxOptions {
            root: root.to_path_buf(),
            home: self.home.clone(),
            network: mounts.network,
            path: self.path.clone(),
            lang: self.lang.clone(),
            read_only: mounts.read_only.clone(),
            writable: mounts.writable.clone(),
            placeholders: mounts.placeholders.clone(),
            project_read_only: mounts.project_read_only,
            protected: protected::protected_mounts(root),
            mesh_socket: self.mesh_socket.clone(),
            binds: mounts.binds.clone(),
            hidden: self.hidden.clone(),
        };
        let mut argv: Vec<OsString> = vec!["bwrap".into()];
        argv.extend(sandbox_args(&options, &RealFs)?);
        argv.push("--".into());
        argv.extend(inner.iter().cloned());
        Ok(argv)
    }
}

#[cfg(test)]
mod hidden_tests {
    use super::*;

    #[test]
    fn only_folders_the_sandbox_hides_hold_the_mesh() {
        let home = std::path::Path::new("/home/me");
        assert!(hidden_in_sandbox(std::path::Path::new("/run/user/1000"), home));
        assert!(hidden_in_sandbox(std::path::Path::new("/tmp/marv-1000"), home));
        assert!(hidden_in_sandbox(std::path::Path::new("/home/me/.run"), home));
        for visible in ["/var/tmp/xdg", "/srv/run", "/run", "/tmp", "/home/me", "relative/dir"] {
            assert!(!hidden_in_sandbox(std::path::Path::new(visible), home), "{visible}");
        }
        assert!(!hidden_in_sandbox(std::path::Path::new("/anything"), std::path::Path::new("/")), "a home of / hides nothing");
    }
}

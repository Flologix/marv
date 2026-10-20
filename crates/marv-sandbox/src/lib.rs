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
//! - [`plan`]: what one command's sandbox allows ([`Plan`]), decided once for every backend.
//! - [`backend`]: [`SandboxBackend`], how a plan becomes a sandboxed command, its clean-up and its kill.
//! - [`bwrap`]: the bubblewrap backend and argument builder (and the mesh plug at `/run/marv/mesh.sock`).
//! - [`seatbelt`]: the Seatbelt (macOS `sandbox-exec`) profile builder, pure.
//! - [`seatbelt_backend`]: the Seatbelt backend (macOS): `sandbox-exec`, `marv __guard`, the private `TMPDIR` (#166).
//! - [`fs`]: the file system as the sandbox checks it ([`FsProbe`]), injectable for tests.
//! - [`git_guard`]: `.git` read-only or a placeholder for commands that run without asking; worktree records.
//! - [`notes`]: the `Marv:` notes bash appends (stand-in home, missing network, gh, read-only .git).
//! - [`runtime`]: Marv's runtime folder (mesh and process sockets): one rule per platform, `MARV_RUNTIME_DIR`.
//! - [`taint`]: once web/MCP/mesh/image text is in the context, egress always asks.

pub mod backend;
pub mod bwrap;
pub mod fs;
pub mod git_guard;
pub mod home;
pub mod host_env;
pub mod notes;
pub mod paths;
pub mod plan;
pub mod protected;
pub mod runtime;
pub mod seatbelt;
pub mod seatbelt_backend;
pub mod taint;
pub mod writable;

#[cfg(test)]
mod test_fs;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub use backend::{Prepared, SandboxBackend, SandboxCommand, backend, kill_group, wait_exited};
pub use bwrap::{Bwrap, MESH_SOCKET, SandboxOptions, sandbox_args, sandbox_available};
pub use fs::{EntryKind, FileKind, FsProbe, RealFs};
pub use home::home_project;
pub use paths::{project_path, resolve_in_project, resolve_within, touches_git};
pub use plan::{Bind, Extra, MESH_SOCKET_ENV, Plan};
pub use protected::{PROTECTED_PATHS, ProtectedPath, ProtectedRule, is_protected, protected_rule};
pub use runtime::{RUNTIME_DIR_RULE, override_refusal, runtime_dir, runtime_env, runtime_paths};
pub use taint::TaintState;
pub use writable::resolve as resolve_writable;

/// Whether every sandbox hides `dir`: it is (by its real path) under `/run`, `/tmp` (by their real paths too: macOS's
/// `/tmp` is `/private/tmp`) or a hidden top-level entry of the
/// home folder (`~/.<anything>`), which each command sees as empty tmpfs — the home-folder project too, which shows
/// the rest of the home read-only. The mesh keeps its sockets only in such a folder: a socket a sandboxed command could
/// see is one it could connect to (a read-only mount doesn't stop `connect()`), and a plug's socket carries that
/// plug's identity and wires. A runtime dir elsewhere (`XDG_RUNTIME_DIR=/var/tmp/…`) is refused, not used.
pub fn hidden_in_sandbox(dir: &std::path::Path, home: &std::path::Path) -> bool {
    let dir = real_or_planned(dir);
    // A `..` left over (under a missing folder) could climb out of whichever folder the prefix test matches.
    if !dir.is_absolute() || dir.components().any(|c| c == std::path::Component::ParentDir) {
        return false;
    }
    let home = real_or_planned(home);
    let under = |base: &std::path::Path| dir.starts_with(base) && dir != base;
    under(&real_or_planned(std::path::Path::new("/run")))
        || under(&real_or_planned(std::path::Path::new("/tmp")))
        || (home != std::path::Path::new("/")
            && dir
                .strip_prefix(&home)
                .ok()
                .and_then(|rel| rel.components().next())
                .is_some_and(|first| first.as_os_str().to_string_lossy().starts_with('.'))
            && !home::TOOLCHAINS.iter().any(|t| dir.starts_with(home.join(t))))
}

/// `path` with its deepest existing ancestor resolved and the rest appended: where a folder that doesn't exist yet
/// will be once made. A plain `canonicalize` fails there and would leave macOS's `/tmp/marv-<uid>` (before its first
/// use) unresolved, so it wouldn't match `/tmp`'s real path, `/private/tmp`.
pub(crate) fn real_or_planned(path: &std::path::Path) -> std::path::PathBuf {
    let mut rest = Vec::new();
    let mut base = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(base) {
            return rest.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (base.parent(), base.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                base = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Marv's config folder as the session finds it (`default_config_dir`): `MARV_CONFIG_DIR`, a relative one taken from
/// the folder Marv started in (where the session reads it), else `~/.marv`. Never `~/.marv` in place of a relative
/// value: the sandbox would then hide the wrong folder and show the real one.
fn config_dir(value: Option<OsString>, cwd: Option<&Path>, home: &Path) -> PathBuf {
    match value.map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => cwd.map(|c| c.join(&dir)).unwrap_or(dir),
        None => home.join(".marv"),
    }
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
    /// Folders every command sees empty: Marv's runtime folder (`runtime::runtime_paths`: the default and any override, see `SandboxOptions::hidden`).
    pub hidden: Vec<PathBuf>,
    /// Extra folders outside the project every command may change (#98): the config's `"writable"`, already
    /// checked (`writable::resolve`) and canonical. None of the refusal happens here — a path in this list was
    /// accepted at startup.
    pub writable: Vec<PathBuf>,
    /// Marv's config folder (`$MARV_CONFIG_DIR` from the real environment, else `~/.marv`): hidden in every sandbox,
    /// wherever it is (`SandboxOptions::config_dir`).
    pub config_dir: Option<PathBuf>,
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
            home: home.clone(),
            path: std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            lang: std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()),
            mesh_socket: None,
            hidden: runtime_paths(&runtime_env()),
            writable: Vec::new(),
            config_dir: Some(config_dir(std::env::var_os("MARV_CONFIG_DIR"), std::env::current_dir().ok().as_deref(), &home)),
        }
    }

    /// The backend this platform's commands run in ([`backend()`]).
    pub fn backend(&self) -> &'static dyn SandboxBackend {
        backend()
    }

    /// Whether commands really run sandboxed: on, and the backend works here.
    pub fn active(&self) -> bool {
        self.enabled && self.backend().available()
    }

    /// `inner` in the project, sandboxed by the backend: spawn [`SandboxCommand::to_command`] (only the backend's
    /// environment, in `root`), and [`Prepared::finish`] it once it has ended (dropping it does too). Protected paths
    /// that exist are read-only (`protected::protected_mounts`).
    pub fn prepare(&self, root: &Path, mounts: &CommandMounts, inner: &[OsString]) -> Result<Prepared, SandboxError> {
        let backend = self.backend();
        Ok(Prepared::new(backend.command(&self.plan(root, mounts), inner, &RealFs)?, backend))
    }

    /// The [`Plan`] for one command in the project: the session's sandbox merged with the command's own mounts, plus
    /// the protected paths that exist (`protected::protected_mounts`).
    pub fn plan(&self, root: &Path, mounts: &CommandMounts) -> Plan {
        Plan {
            root: root.to_path_buf(),
            home: self.home.clone(),
            network: mounts.network,
            path: self.path.clone(),
            lang: self.lang.clone(),
            read_only: mounts.read_only.clone(),
            // The session's extra writable folders (#98) with this command's own on top.
            writable: self
                .writable
                .iter()
                .chain(mounts.writable.iter())
                .filter(|d| !mounts.read_only.contains(d))
                .cloned()
                .collect(),
            placeholders: mounts.placeholders.clone(),
            project_read_only: mounts.project_read_only,
            // In the home folder the project is read-only (bwrap.rs), so there's nothing to protect on top of it, and
            // nothing to walk the whole home for.
            protected: if home_project(root, &self.home) { Vec::new() } else { protected::protected_mounts(root) },
            mesh_socket: self.mesh_socket.clone(),
            binds: mounts.binds.clone(),
            hidden: self.hidden.clone(),
            config_dir: self.config_dir.clone(),
        }
    }
}

#[cfg(test)]
mod hidden_tests {
    use super::*;

    /// A relative `MARV_CONFIG_DIR` is where the session reads it (from the folder Marv started in), so that's what
    /// the sandbox hides, not `~/.marv`.
    #[test]
    fn the_config_folder_is_the_sessions() {
        let (cwd, home) = (Some(Path::new("/work")), Path::new("/home/me"));
        assert_eq!(config_dir(Some("cfg".into()), cwd, home), Path::new("/work/cfg"));
        assert_eq!(config_dir(Some("/srv/cfg".into()), cwd, home), Path::new("/srv/cfg"));
        assert_eq!(config_dir(None, cwd, home), Path::new("/home/me/.marv"));
    }

    #[test]
    fn only_folders_the_sandbox_hides_hold_the_mesh() {
        let home = std::path::Path::new("/home/me");
        assert!(hidden_in_sandbox(std::path::Path::new("/run/user/1000"), home));
        assert!(hidden_in_sandbox(std::path::Path::new("/tmp/marv-1000"), home));
        assert!(hidden_in_sandbox(std::path::Path::new("/home/me/.run"), home));
        // A visible folder of the home shows in a home-folder project; a toolchain's shows everywhere.
        assert!(!hidden_in_sandbox(std::path::Path::new("/home/me/run"), home));
        assert!(!hidden_in_sandbox(std::path::Path::new("/home/me/.cargo/run"), home));
        for visible in ["/var/tmp/xdg", "/srv/run", "/run", "/tmp", "/home/me", "relative/dir"] {
            assert!(!hidden_in_sandbox(std::path::Path::new(visible), home), "{visible}");
        }
        assert!(!hidden_in_sandbox(std::path::Path::new("/anything"), std::path::Path::new("/")), "a home of / hides nothing");
        assert!(!hidden_in_sandbox(std::path::Path::new("/tmp/no-such-marv/../../etc"), home));
    }

    /// macOS's `/tmp` is a symlink to `/private/tmp`: a runtime folder not made yet must still resolve through it.
    #[test]
    fn a_folder_not_made_yet_resolves_through_a_symlinked_parent() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap().join(format!("marv-rp-{}", std::process::id()));
        std::fs::create_dir_all(base.join("private")).unwrap();
        let link = base.join("tmp");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(base.join("private"), &link).unwrap();
        assert_eq!(super::real_or_planned(&link.join("marv-501/p")), base.join("private/marv-501/p"));
        assert_eq!(super::real_or_planned(&link), base.join("private"));
        std::fs::remove_dir_all(&base).unwrap();
    }
}

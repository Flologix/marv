//! What one command's sandbox allows, decided once for every backend (#163, `docs/specs/2026-10-17-macos-sandbox.md`
//! §5).
//!
//! A [`Plan`] is built by [`crate::Sandbox::plan`] from the session's sandbox and one command's
//! [`crate::CommandMounts`]. It says *what* the command may touch: the project (writable or not), the extra read-only
//! and writable folders, the protected paths and placeholders, Marv's own folders to hide, the network, the mesh plug,
//! the relay's binds and the environment. A [`crate::backend::SandboxBackend`] decides *how*: bubblewrap
//! ([`crate::bwrap`]) shows something else at a path (an empty tmpfs, `/dev/null`, a read-only bind); Seatbelt (#164,
//! macOS) will refuse the path instead.
//!
//! The policy both need lives here, as methods over a [`FsProbe`], so it's decided in one place and tested against a
//! fake file system: which extras are safe to show and whether they hold the project ([`Plan::read_only_extras`],
//! [`Plan::writable_extras`]), whether the project is the home folder ([`Plan::home_project`]), the toolchains shown
//! again, which of Marv's folders exist to hide ([`Plan::hidden_dirs`], [`Plan::config_dir_hidden`]), the credential
//! files ([`Plan::credentials`]) and the environment ([`Plan::env`]).
//!
//! Port note: the spec's sketch stores these resolved values as fields. They're methods instead, so a `Plan` stays
//! what the caller decided (the bwrap tests build one by hand, unchanged since it was `SandboxOptions`) and the
//! resolved view can't drift from it.

use std::path::{Path, PathBuf};

use crate::SandboxError;
use crate::backend::SandboxBackend;
use crate::fs::{EntryKind, FsProbe};
use crate::home::{CREDENTIALS, HOME_MASKS, TOOLCHAINS, contains, credential_masks};

/// The environment variable that tells a sandboxed command where its mesh plug is (`docs/MESH.md`): bubblewrap shows
/// the plug at [`crate::bwrap::MESH_SOCKET`], Seatbelt can't move it from its host path. Set only when plugged.
pub const MESH_SOCKET_ENV: &str = "MARV_MESH_SOCKET";

/// One command's sandbox: see the module doc. `SandboxOptions` is bubblewrap's older name for it.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub root: PathBuf,
    pub home: PathBuf,
    pub network: bool,
    /// PATH inside the sandbox.
    pub path: String,
    /// LANG inside the sandbox (`C.UTF-8` when empty).
    pub lang: String,
    /// Extra folders the command may read but not change: a worktree's shared `.git` (so git status/diff/log work),
    /// or the worktree around a subagent working in one of its subfolders (mounted *before* the project, which stays
    /// writable on top).
    pub read_only: Vec<PathBuf>,
    /// Extra folders outside the project the command may change, for an approved command: a git worktree's record
    /// and its repository's `.git` (in the hidden home), so an approved `git commit` works there.
    pub writable: Vec<PathBuf>,
    /// Paths inside the project that don't exist yet and must stay that way: each gets an empty read-only folder
    /// (a project without `.git`, so a command can't create a repository whose config git would run outside). bwrap
    /// leaves the empty mount point behind; [`SandboxBackend::after`] removes it.
    pub placeholders: Vec<PathBuf>,
    /// The project read-only too: Marv's own checks, which run the project's code without asking.
    pub project_read_only: bool,
    /// Protected paths inside the project, mounted read-only on top of it (marv-rs).
    pub protected: Vec<PathBuf>,
    /// The mesh plug's host socket, bound at [`crate::bwrap::MESH_SOCKET`] (marv-rs). See [`Plan::mesh_bind`].
    pub mesh_socket: Option<PathBuf>,
    /// Files or folders mounted at a path of their own, after `/run` is emptied (so a destination under `/run`
    /// works): the process door's relay (Marv #51) needs Marv's own binary at `/run/marv-bin` (read-only) and its
    /// socket folder at `/run/marv` (writable for the process that serves it, read-only for a command that only
    /// connects). Where each really appears inside is [`SandboxBackend::guest_path`].
    pub binds: Vec<Bind>,
    /// Marv's own runtime folders (`runtime::runtime_paths`, the default and any `MARV_RUNTIME_DIR`: the mesh's plug
    /// and discovery sockets, the process door's relays), each an empty tmpfs in the sandbox wherever it is (marv-rs).
    pub hidden: Vec<PathBuf>,
    /// Marv's config folder (`~/.marv` or `MARV_CONFIG_DIR`): an empty tmpfs in the sandbox wherever it is, its
    /// `skills/` shown again read-only (marv-rs). Inside the stand-in home it's hidden already.
    pub config_dir: Option<PathBuf>,
}

/// One of [`Plan::binds`]: `source` on the host, at `dest` in the sandbox (for bubblewrap; a backend without mounts
/// shows it at `source`, see [`SandboxBackend::guest_path`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub writable: bool,
}

/// An extra folder of [`Plan::read_only`] or [`Plan::writable`] after [`resolve_extra`]: the path the caller gave,
/// where it really leads (what was checked, and what a backend must use), and whether it holds the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extra {
    pub given: PathBuf,
    pub real: PathBuf,
    pub around_project: bool,
}

/// The real path of an extra folder after checking that showing it is safe, and whether it holds the project (then
/// bwrap mounts it before the project). bwrap follows symlinks, so the check must too.
pub fn resolve_extra(dir: &Path, home: &Path, root: &Path, fs: &dyn FsProbe) -> Result<(PathBuf, bool), SandboxError> {
    if !dir.is_absolute() {
        return Err(SandboxError::Mount(format!("Extra sandbox folder {} must be an absolute path.", dir.display())));
    }
    let real =
        fs.realpath(dir).map_err(|_| SandboxError::Mount(format!("Extra sandbox folder {} doesn't exist.", dir.display())))?;
    let real_home = fs.realpath(home).unwrap_or_else(|_| home.to_path_buf());
    let real_root = fs.realpath(root).unwrap_or_else(|_| root.to_path_buf());
    // Mounting the home folder or a parent of it (even read-only) would expose the hidden home, with the API key in it.
    if real == Path::new("/") || contains(&real, &real_home) {
        return Err(SandboxError::Mount(format!(
            "Refusing to make {} visible in the sandbox: it is, or leads to, the home folder or one of its parents.",
            dir.display()
        )));
    }
    // Read-only over the project itself would leave nothing writable.
    if real == real_root {
        return Err(SandboxError::Mount(format!(
            "Refusing to make {} read-only in the sandbox: it is the project folder.",
            dir.display()
        )));
    }
    let around = contains(&real, &real_root);
    Ok((real, around))
}

impl Plan {
    /// The project's real path (as given when it leads nowhere).
    pub fn real_root(&self, fs: &dyn FsProbe) -> PathBuf {
        fs.realpath(&self.root).unwrap_or_else(|_| self.root.clone())
    }

    /// The home folder's real path (as given when it leads nowhere).
    pub fn real_home(&self, fs: &dyn FsProbe) -> PathBuf {
        fs.realpath(&self.home).unwrap_or_else(|_| self.home.clone())
    }

    /// Whether the project is the home folder or a parent of it, on real paths (`crate::home_project`). Then nothing
    /// of the project is writable: showing it writable would show the whole real home.
    pub fn home_project(&self, fs: &dyn FsProbe) -> bool {
        contains(&self.real_root(fs), &self.real_home(fs))
    }

    /// Whether the command may change nothing in the project: Marv's own checks, or a home-folder project.
    pub fn project_read_only(&self, fs: &dyn FsProbe) -> bool {
        self.project_read_only || self.home_project(fs)
    }

    /// In a home-folder project, [`HOME_MASKS`] under the home's real path: the entries kept hidden even where a
    /// toolchain would show them again (bwrap masks each that exists; Seatbelt denies each, there or not). Nothing
    /// otherwise: the whole home is hidden then.
    pub fn home_masks(&self, fs: &dyn FsProbe) -> Vec<PathBuf> {
        if !self.home_project(fs) {
            return Vec::new();
        }
        let home = self.real_home(fs);
        HOME_MASKS.iter().map(|name| home.join(name)).collect()
    }

    /// The [`TOOLCHAINS`] that exist under the home, as paths under the home (not resolved): shown again, read-only.
    pub fn toolchains(&self, fs: &dyn FsProbe) -> Vec<PathBuf> {
        TOOLCHAINS.iter().map(|dir| self.home.join(dir)).filter(|full| fs.exists(full)).collect()
    }

    /// [`Plan::read_only`], each checked by [`resolve_extra`] (the first refusal stops the command).
    pub fn read_only_extras(&self, fs: &dyn FsProbe) -> Result<Vec<Extra>, SandboxError> {
        self.extras(&self.read_only, fs)
    }

    /// [`Plan::writable`], each checked by [`resolve_extra`] (the first refusal stops the command).
    pub fn writable_extras(&self, fs: &dyn FsProbe) -> Result<Vec<Extra>, SandboxError> {
        self.extras(&self.writable, fs)
    }

    fn extras(&self, dirs: &[PathBuf], fs: &dyn FsProbe) -> Result<Vec<Extra>, SandboxError> {
        dirs.iter()
            .map(|dir| {
                resolve_extra(dir, &self.home, &self.root, fs).map(|(real, around_project)| Extra {
                    given: dir.clone(),
                    real,
                    around_project,
                })
            })
            .collect()
    }

    /// Marv's runtime folders ([`Plan::hidden`]) that exist, by their real (absolute) paths: what every backend hides.
    /// A folder that doesn't exist yet has nothing in it to hide.
    pub fn hidden_dirs(&self, fs: &dyn FsProbe) -> Vec<PathBuf> {
        self.hidden.iter().filter_map(|dir| fs.realpath(dir).ok()).filter(|real| fs.exists(real) && real.is_absolute()).collect()
    }

    /// Marv's config folder by its real path, when it's a folder to hide: it exists, and it doesn't hold the project
    /// (hiding it would hide the project). Its config.json holds the API key and the session-start hooks, which run
    /// outside the sandbox.
    pub fn config_dir_hidden(&self, fs: &dyn FsProbe) -> Option<PathBuf> {
        let real = fs.realpath(self.config_dir.as_ref()?).ok()?;
        (fs.entry_kind(&real) == Some(EntryKind::Dir) && !contains(&real, &self.real_root(fs))).then_some(real)
    }

    /// The [`CREDENTIALS`] files to hide, as paths inside the sandbox, without duplicates: each that exists, or for a
    /// symlinked one the file it leads to wherever one of `shown` (the toolchain folders) shows it
    /// (`home::credential_masks`).
    pub fn credentials(&self, shown: &[PathBuf], fs: &dyn FsProbe) -> Vec<PathBuf> {
        let mut masks: Vec<PathBuf> = Vec::new();
        for file in CREDENTIALS {
            for mask in credential_masks(&self.home.join(file), shown, fs) {
                if !masks.contains(&mask) {
                    masks.push(mask);
                }
            }
        }
        masks
    }

    /// The mesh plug as a bind: its host socket, at [`crate::bwrap::MESH_SOCKET`] for a backend that can mount it.
    pub fn mesh_bind(&self) -> Option<Bind> {
        self.mesh_socket.as_ref().map(|socket| Bind {
            source: socket.clone(),
            dest: crate::bwrap::MESH_SOCKET.into(),
            writable: true,
        })
    }

    /// The command's whole environment, nothing inherited (no API keys): `PATH`, `HOME`, `LANG`, `TERM=dumb`, `TMPDIR`
    /// (`tmp`, the backend's private temporary folder), a throwaway `BUN_INSTALL_CACHE_DIR` in it, and
    /// [`MESH_SOCKET_ENV`] where `backend` shows the plug, when there is one, then the backend's own
    /// ([`SandboxBackend::extra_env`]). `bun install` needs a writable cache; not
    /// the user's, nor any cache that outlives the command: a shared one would let one sandboxed command (an install's
    /// lifecycle scripts) plant packages for another context.
    pub fn env(&self, tmp: &str, backend: &dyn SandboxBackend) -> Vec<(String, String)> {
        let lang = if self.lang.is_empty() { "C.UTF-8" } else { &self.lang };
        let mut env: Vec<(String, String)> = [
            ("PATH", self.path.clone()),
            ("HOME", self.home.to_string_lossy().into_owned()),
            ("LANG", lang.to_string()),
            ("TERM", "dumb".to_string()),
            ("TMPDIR", tmp.to_string()),
            ("BUN_INSTALL_CACHE_DIR", format!("{tmp}/bun-cache")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        if let Some(plug) = self.mesh_bind() {
            env.push((MESH_SOCKET_ENV.into(), backend.guest_path(&plug).to_string_lossy().into_owned()));
        }
        // What this backend needs on top (Seatbelt: caches in the private TMPDIR, as the home can't be written).
        env.extend(backend.extra_env(tmp));
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bwrap::Bwrap;
    use crate::fs::FileKind;
    use crate::test_fs::Fake;

    fn base() -> Plan {
        Plan { root: "/home/me/proj".into(), home: "/home/me".into(), path: "/usr/bin".into(), ..Default::default() }
    }

    #[test]
    fn extras_are_resolved_checked_and_placed_around_the_project() {
        let mut fs = Fake::new(|_| true);
        fs.links.insert("/srv/link".into(), "/srv/tree".into());
        let plan = Plan { root: "/srv/tree/pkg".into(), read_only: vec!["/srv/link".into(), "/srv/other".into()], ..base() };
        let extras = plan.read_only_extras(&fs).unwrap();
        assert_eq!(
            extras,
            vec![
                Extra { given: "/srv/link".into(), real: "/srv/tree".into(), around_project: true },
                Extra { given: "/srv/other".into(), real: "/srv/other".into(), around_project: false },
            ]
        );
        // The home, a parent of it, / and (read-only) the project itself are refused, as is a relative path.
        for bad in ["/home/me", "/home", "/", "/srv/tree/pkg", "relative"] {
            assert!(Plan { read_only: vec![bad.into()], ..plan.clone() }.read_only_extras(&fs).is_err(), "{bad}");
            assert!(Plan { writable: vec![bad.into()], ..plan.clone() }.writable_extras(&fs).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_home_folder_or_a_parent_is_a_read_only_project() {
        let fs = Fake::new(|_| true);
        assert!(!base().home_project(&fs) && !base().project_read_only(&fs));
        for root in ["/home/me", "/home", "/"] {
            let plan = Plan { root: root.into(), ..base() };
            assert!(plan.home_project(&fs) && plan.project_read_only(&fs), "{root}");
        }
        assert!(Plan { project_read_only: true, ..base() }.project_read_only(&fs));
        // Judged on real paths: a link to the home is the home.
        let mut fs = Fake::new(|_| true);
        fs.links.insert("/work".into(), "/home/me".into());
        assert!(Plan { root: "/work".into(), ..base() }.home_project(&fs));
    }

    #[test]
    fn only_existing_folders_are_hidden_and_never_one_holding_the_project() {
        let mut fs = Fake::new(|p| p.contains("marv") || p == "/home");
        fs.links.insert("/run/user/1000/marv".into(), "/srv/xdg/marv".into());
        let plan = Plan {
            hidden: vec!["/run/user/1000/marv".into(), "/missing".into()],
            config_dir: Some("/srv/marv-config".into()),
            ..base()
        };
        assert_eq!(plan.hidden_dirs(&fs), vec![PathBuf::from("/srv/xdg/marv")]);
        assert_eq!(plan.config_dir_hidden(&fs), Some(PathBuf::from("/srv/marv-config")));
        assert_eq!(Plan { config_dir: Some("/home".into()), ..plan.clone() }.config_dir_hidden(&fs), None, "holds the project");
        assert_eq!(Plan { config_dir: None, ..plan }.config_dir_hidden(&fs), None);
    }

    #[test]
    fn toolchains_and_credentials() {
        let mut fs = Fake::new(|p| p.ends_with(".cargo") || p.ends_with(".bun"));
        fs.kinds.insert("/home/me/.cargo/credentials.toml".into(), FileKind::File);
        let plan = base();
        let shown = plan.toolchains(&fs);
        assert_eq!(shown, vec![PathBuf::from("/home/me/.bun"), PathBuf::from("/home/me/.cargo")]);
        assert_eq!(plan.credentials(&shown, &fs), vec![PathBuf::from("/home/me/.cargo/credentials.toml")]);
    }

    #[test]
    fn the_environment_is_cleared_and_names_the_plug_only_when_plugged() {
        let env = base().env("/tmp", &Bwrap);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["PATH", "HOME", "LANG", "TERM", "TMPDIR", "BUN_INSTALL_CACHE_DIR"]);
        assert!(env.contains(&("LANG".into(), "C.UTF-8".into())));
        assert!(env.contains(&("BUN_INSTALL_CACHE_DIR".into(), "/tmp/bun-cache".into())));
        let plugged = Plan { mesh_socket: Some("/run/user/1000/marv/plugs/s/a.sock".into()), ..base() }.env("/tmp", &Bwrap);
        assert_eq!(plugged.last(), Some(&(MESH_SOCKET_ENV.to_string(), crate::bwrap::MESH_SOCKET.to_string())));
    }
}

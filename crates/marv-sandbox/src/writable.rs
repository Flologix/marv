//! Extra writable sandbox folders (#98): the config's `"writable": ["~/Projects/marv/.beads"]`, bind-mounted
//! read-write into every command's sandbox so a tool that keeps its state outside the project (bd's database,
//! a shared cache) runs inside the sandbox and never asks.
//!
//! This is the one way a sandboxed command writes outside the project, so the entries are refused, at startup,
//! unless they are plainly safe (docs/SECURITY.md). Every entry is canonicalised first and the rules are judged
//! on the canonical path — a symlink (`~/beads -> ~/.ssh`) is refused by where it leads:
//! - `/`, the home folder or a parent of it;
//! - **any hidden top-level entry of the home** (`~/.<anything>`) — the entry, anything inside it, or a parent of
//!   it: `.local/bin` (a planted binary on PATH runs on the host), `.config/autostart` and
//!   `.config/systemd/user` (run at the next login), `.claude` (its hooks run on the host), `~/.ssh`, `~/.marv`
//!   (the API key, the MCP servers), `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.cargo`, `~/.config/git`,
//!   `~/.config/gh`, and the credential files — the names `PROTECTED_HOME_PATHS` spells out are only the ones the
//!   error message names specifically; the rule doesn't depend on the list. A *visible* top-level folder's tree
//!   (`~/Projects/…`) is the one thing under the home that stays allowed;
//! - **a git repository** — the entry is one, sits under a `.git` path component, or directly holds a `.git`
//!   entry (the worktree kind, a file, included): a planted `.git/hooks` script runs on the host's next git
//!   command there. Known limit, accepted with the user's eyes open: a repository planted *deeper inside* an
//!   accepted folder later (`~/Projects/x/cache/repo/.git` created by a command) is not scanned — the refusal
//!   happens once, at startup, on what the entry held then;
//! - the project's parents (a writable `~/Projects` would make every other project writable);
//! - a Marv runtime folder (the mesh's plug sockets, the `runtime` argument's `marv/` under it), inside it or holding it;
//! - Marv's config folder (`config_dir`: `~/.marv`, or a visible `MARV_CONFIG_DIR`), inside it or holding it — the
//!   config's hooks, the API key, the tool-output files;
//! - anything missing or not a folder: bwrap can't mount it.
//!
//! What survives is passed to [`crate::Sandbox`] (`writable`), which bwrap binds read-write; the per-command
//! writable mounts (an approved `git_write` command's worktree records, under `~/.marv/worktrees`) keep their own
//! path, which must stay allowed — that's why the refusal happens here and not for every bwrap mount.

use std::path::{Path, PathBuf};

use crate::paths::expand_home;

/// The home's protected paths the error message names specifically, relative to the home. The *rule* doesn't
/// depend on this list — every hidden top-level home entry (`~/.<anything>`) is refused — these are only the ones
/// worth spelling out (the credential and instruction/config locations, a superset of what the stand-in home
/// hides or masks).
pub const PROTECTED_HOME_PATHS: &[&str] = &[
    ".ssh",
    ".marv",
    ".gnupg",
    ".aws",
    ".kube",
    ".cargo",
    ".config/git",
    ".config/gh",
    ".config",
    ".local/bin",
    ".netrc",
    ".gitconfig",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
];

/// Whether `real` (the canonical entry) can't be mounted writable, and why. Fail closed: the rules say no more
/// than what's plainly safe outside the project, and anything under the home is refused unless it's a *visible*
/// top-level folder's tree (`~/Projects/…`), a git repository or the mesh's runtime dir excluded.
fn refuse(real: &Path, home: &Path, root: &Path, runtime: Option<&Path>, config_dir: Option<&Path>) -> Option<String> {
    if real == Path::new("/") {
        return Some("it is the root of the file system".into());
    }
    let real_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if real == real_home || real_home.starts_with(real) {
        return Some(format!("it is, or holds, your home folder {}", real_home.display()));
    }
    // A parent of the project would make every other project writable too (theirs `.git` included).
    let real_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if real != real_root && real_root.starts_with(real) {
        return Some(format!("it holds the project folder {}", real_root.display()));
    }
    // Any hidden top-level entry of the home (~/.<anything>): the entry, anything inside it, a parent of it. That's
    // every dotfile's tree at once — `.local/bin` (a planted binary on PATH runs on the host), `.config/autostart`
    // and `.config/systemd/user` (run at your next login), `.claude` (its hooks run on the host) — not only the
    // names below, which are just the ones the message spells out.
    if let Some(first) = real.strip_prefix(&real_home).ok().and_then(|rel| rel.components().next()) {
        let name = first.as_os_str().to_string_lossy();
        if name.starts_with('.') {
            let protected = PROTECTED_HOME_PATHS.iter().find(|p| {
                let path = real_home.join(p);
                real == path || real.starts_with(&path) || path.starts_with(real)
            });
            return Some(match protected {
                Some(p) => format!("it is, or holds, a protected path ({})", real_home.join(p).display()),
                None => format!(
                    "it is, or holds, a hidden folder in your home ({}): a planted file there runs outside the sandbox",
                    real_home.join(name.as_ref()).display()
                ),
            });
        }
    }
    // A git repository — the entry itself, under a `.git` component, or one directly holding a `.git` entry (the
    // worktree kind too): a planted `.git/hooks` script runs on the host's next git command there. (A nested
    // repository planted *deeper inside* an accepted folder is not scanned — see the module doc.) Only folders can
    // directly hold a `.git`: a plain file entry falls through to the folder check.
    if real.components().any(|c| c.as_os_str() == ".git") {
        return Some("it is, or is inside, a git repository's .git: git runs its hooks and config outside the sandbox".into());
    }
    if real.is_dir() && real.join(".git").try_exists().unwrap_or(true) {
        return Some("it is, or holds, a git repository: git runs its hooks and config outside the sandbox".into());
    }
    // The mesh's sockets live in the runtime dir's `marv` folder: a writable mount over them (or over their
    // parent) would let a command reach a plug's socket. (`hidden_in_sandbox` is broader — anything under /run,
    // /tmp or the home — but a writable folder under the home is exactly the point of #98, so only the runtime
    // dir itself is out. `runtime` is the caller's XDG_RUNTIME_DIR, from the real environment.)
    // Inside it counts too (`$XDG_RUNTIME_DIR/marv/mesh`, where the sockets themselves are). Judged on the real path,
    // like the entry.
    if let Some(sockets) = runtime.map(|base| real_or_given(&base.join("marv")))
        && (real.starts_with(&sockets) || sockets.starts_with(real))
    {
        return Some("the mesh's runtime sockets live there".into());
    }
    // Marv's own config folder (`~/.marv`, or wherever `MARV_CONFIG_DIR` puts it — a visible folder escapes the
    // hidden-entry rule): its config.json holds the API key and the session-start hooks (run outside the sandbox at
    // the next start), and it holds the tool-output files, the worktrees and the checkpoints.
    if let Some(config) = config_dir.map(real_or_given)
        && (real.starts_with(&config) || config.starts_with(real))
    {
        return Some(format!("it is, or holds, or is inside Marv's config folder {}", config.display()));
    }
    None
}

/// The canonical path where it exists, else as given (made absolute by the caller's paths already).
fn real_or_given(path: &Path) -> PathBuf {
    crate::paths::real_with_missing(path).unwrap_or_else(|_| path.to_path_buf())
}

/// One entry: canonical path, or why it was refused. The message names the entry as written.
fn resolve_entry(
    entry: &str,
    home: &Path,
    root: &Path,
    runtime: Option<&Path>,
    config_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let raw = expand_home(entry, home);
    if !raw.is_absolute() {
        return Err(format!("Writable sandbox folder \"{entry}\" must be an absolute path (or start with ~)."));
    }
    // Where it really leads, symlinks followed through the existing parts (a missing tail stays as written): the
    // rules are judged on the real path, so `~/beads -> ~/.ssh` is refused wherever it exists or not.
    let real = crate::paths::real_with_missing(&raw).map_err(|e| format!("Writable sandbox folder \"{entry}\": {e}."))?;
    if let Some(why) = refuse(&real, home, root, runtime, config_dir) {
        return Err(format!("Refusing writable sandbox folder \"{entry}\": {why}."));
    }
    if !raw.exists() {
        return Err(format!("Writable sandbox folder \"{entry}\" doesn't exist (tried {}); create it first.", raw.display()));
    }
    if !real.is_dir() {
        return Err(format!("Writable sandbox folder \"{entry}\" isn't a folder ({}).", real.display()));
    }
    Ok(real)
}

/// The config's `"writable"` entries resolved once at startup: the canonical paths to bind read-write into every
/// sandbox, and the problems (refused entries) for the startup notice. Duplicates collapse. `runtime` is the
/// process's `XDG_RUNTIME_DIR` (the mesh sockets live under `marv/` in it), from the real environment; `config_dir` is
/// Marv's config folder (`default_config_dir`: `~/.marv` or `MARV_CONFIG_DIR`).
pub fn resolve(
    entries: &[String],
    home: &Path,
    root: &Path,
    runtime: Option<&Path>,
    config_dir: Option<&Path>,
) -> (Vec<PathBuf>, Vec<String>) {
    let (mut accepted, mut problems) = (Vec::new(), Vec::new());
    for entry in entries {
        match resolve_entry(entry, home, root, runtime, config_dir) {
            Ok(real) if !accepted.contains(&real) => accepted.push(real),
            Ok(_) => {}
            Err(problem) => problems.push(problem),
        }
    }
    (accepted, problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_dir::TempDir;

    fn in_home(home: &Path, root: &Path, entry: &str) -> Result<PathBuf, String> {
        resolve_entry(entry, home, root, None, None)
    }

    fn with_runtime(home: &Path, root: &Path, entry: &str, runtime: &Path) -> Result<PathBuf, String> {
        resolve_entry(entry, home, root, Some(runtime), None)
    }

    #[test]
    fn ordinary_folders_are_accepted_and_canonicalised() {
        let home = TempDir::new("writable-home");
        let dir = TempDir::new("writable-ok");
        let path = dir.path().to_str().unwrap().to_string();
        let root = TempDir::new("writable-root");
        let (ok, problems) = resolve(&[path.clone(), format!("{path}/.")], home.path(), root.path(), None, None);
        assert_eq!(ok, [dir.path()], "duplicates and `.` paths collapse to the canonical path");
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn the_root_the_home_and_the_projects_parents_are_refused() {
        let home = TempDir::new("writable-home2");
        let project = TempDir::new("writable-proj");
        for entry in ["/", home.path().to_str().unwrap(), &format!("{}/..", home.path().display()), "~/"] {
            let why = in_home(home.path(), project.path(), entry).unwrap_err();
            assert!(why.contains("Refusing"), "{entry}: {why}");
        }
        // A folder strictly containing the project (but outside the home) is refused: it holds other projects.
        let work = TempDir::new("writable-work");
        let nested = work.path().join("project");
        std::fs::create_dir_all(&nested).unwrap();
        let why = in_home(home.path(), &nested, work.path().to_str().unwrap()).unwrap_err();
        assert!(why.contains("holds the project folder"), "{why}");
        assert!(why.contains("holds the project folder"), "{why}");
    }

    #[test]
    fn protected_home_paths_and_anything_holding_them_are_refused() {
        let home = TempDir::new("writable-home3");
        let project = TempDir::new("writable-proj3");
        for entry in [
            "~/.ssh",
            "~/.ssh/keys",
            "~/.marv",
            "~/.marv/skills",
            "~/.gnupg",
            "~/.aws",
            "~/.cargo",
            "~/.config/git",
            "~/.config/gh",
            "~/.config",
            "~/.netrc",
            "~/.gitconfig",
            "~/.npmrc",
        ] {
            let why = in_home(home.path(), project.path(), entry).unwrap_err();
            assert!(why.contains("protected path"), "{entry}: {why}");
        }
    }

    #[test]
    fn any_hidden_top_level_home_entry_is_refused_with_what_it_is() {
        let home = TempDir::new("writable-home-hidden");
        let project = TempDir::new("writable-proj-hidden");
        // Anything the message doesn't name specifically: a hidden top-level entry, inside it, is refused all
        // the same — .local/bin (planted binary on PATH), .config/autostart, .claude (its hooks run on the host).
        std::fs::create_dir_all(home.path().join(".whatever/bin")).unwrap();
        std::fs::create_dir_all(home.path().join(".local/bin")).unwrap();
        std::fs::create_dir_all(home.path().join(".claude/hooks")).unwrap();
        // `.local/bin` and `~/.config/*` get their specific message (a known name, or one they hold);
        // everything else gets the general hidden-folder one.
        for entry in ["~/.whatever", "~/.whatever/bin", "~/.claude/hooks", "~/.claude"] {
            let why = in_home(home.path(), project.path(), entry).unwrap_err();
            assert!(why.contains("hidden folder in your home"), "{entry}: {why}");
        }
        for entry in ["~/.local/bin", "~/.local", "~/.config/autostart"] {
            let why = in_home(home.path(), project.path(), entry).unwrap_err();
            assert!(why.contains("protected path"), "{entry}: {why}");
        }
        // The known ones keep their specific message.
        let why = in_home(home.path(), project.path(), "~/.ssh").unwrap_err();
        assert!(why.contains("protected path"), "{why}");
    }

    #[test]
    fn visible_top_level_home_trees_stay_allowed() {
        let home = TempDir::new("writable-home-visible");
        let project = TempDir::new("writable-proj-visible");
        // The task's own example: bd's database in a visible tree, hidden dot-entries beside it.
        std::fs::create_dir_all(home.path().join("Projects/marv/.beads")).unwrap();
        std::fs::create_dir_all(home.path().join("Projects/marv/.git")).unwrap(); // a repo beside it
        std::fs::create_dir_all(home.path().join("visible")).unwrap();
        let ok = in_home(home.path(), project.path(), "~/Projects/marv/.beads").unwrap();
        assert_eq!(ok, home.path().join("Projects/marv/.beads"));
        let ok = in_home(home.path(), project.path(), "~/visible").unwrap();
        assert_eq!(ok, home.path().join("visible"));
    }

    #[test]
    fn git_repositories_are_refused_their_interiors_with_them() {
        let home = TempDir::new("writable-home-git");
        let project = TempDir::new("writable-proj-git");
        // A repository the entry is, or directly holds — folder form and worktree file form, inside the home and
        // outside it.
        std::fs::create_dir_all(home.path().join("repo/.git/hooks")).unwrap();
        std::fs::create_dir_all(home.path().join("wt")).unwrap();
        std::fs::write(home.path().join("wt/.git"), "gitdir: /nowhere\n").unwrap();
        let outside = TempDir::new("writable-git-out");
        std::fs::create_dir_all(outside.path().join("repo/.git")).unwrap();
        for entry in ["~/repo", "~/repo/.git/hooks", "~/wt", "~/srv/git/repo"] {
            std::fs::create_dir_all(home.path().join("srv/git/repo/.git")).unwrap();
            let why = in_home(home.path(), project.path(), entry).unwrap_err();
            assert!(why.contains("git repository"), "{entry}: {why}");
        }
        let why =
            resolve_entry(&outside.path().join("repo").to_string_lossy(), Path::new("/nowhere-home"), project.path(), None, None)
                .unwrap_err();
        assert!(why.contains("git repository"), "{why}");
        // A folder inside a repo — but not a `.git` interior and not directly holding one — is the documented
        // known limit (nested repos planted deeper inside an accepted folder are not scanned), so it's allowed.
        std::fs::create_dir_all(home.path().join("repo/src")).unwrap();
        assert!(in_home(home.path(), project.path(), "~/repo/src").is_ok());
    }

    #[test]
    fn a_symlink_is_judged_by_where_it_leads() {
        let home = TempDir::new("writable-home4");
        std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
        std::fs::create_dir_all(home.path().join("beads")).unwrap();
        std::os::unix::fs::symlink(home.path().join(".ssh"), home.path().join("innocent")).unwrap();
        let root = TempDir::new("writable-root4");
        // The symlink to a protected folder is refused; the real folder next to it is fine.
        let why = in_home(home.path(), root.path(), "~/innocent").unwrap_err();
        assert!(why.contains("protected path"), "{why}");
        let ok = in_home(home.path(), root.path(), "~/beads").unwrap();
        assert_eq!(ok, home.path().join("beads"));
    }

    #[test]
    fn missing_and_non_folders_are_refused_with_what_to_do() {
        let home = TempDir::new("writable-home5");
        let root = TempDir::new("writable-root5");
        let why = in_home(home.path(), root.path(), "~/nope").unwrap_err();
        assert!(why.contains("doesn't exist") && why.contains("create it first"), "{why}");
        // A visible (non-hidden) file: the folder rule. A hidden one is refused by its hidden-entry rule first.
        std::fs::write(home.path().join("notes.txt"), "x").unwrap();
        let why = in_home(home.path(), root.path(), "~/notes.txt").unwrap_err();
        assert!(why.contains("isn't a folder"), "{why}");
        std::fs::write(home.path().join(".bashrc"), "x").unwrap();
        let why = in_home(home.path(), root.path(), "~/.bashrc").unwrap_err();
        assert!(why.contains("hidden folder in your home"), "{why}");
        let why = in_home(home.path(), root.path(), "rel/ative").unwrap_err();
        assert!(why.contains("absolute path"), "{why}");
    }

    #[test]
    fn the_mesh_runtime_dir_is_refused() {
        let home = TempDir::new("writable-home6");
        let root = TempDir::new("writable-root6");
        let runtime = home.path().join("xdg");
        std::fs::create_dir_all(runtime.join("marv")).unwrap();
        let why = with_runtime(home.path(), root.path(), "~/xdg/marv", &runtime).unwrap_err();
        assert!(why.contains("runtime sockets"), "{why}");
        let why = with_runtime(home.path(), root.path(), "~/xdg", &runtime).unwrap_err();
        assert!(why.contains("runtime sockets"), "{why}");
        // An unrelated folder next to it is fine.
        std::fs::create_dir_all(home.path().join("other")).unwrap();
        assert!(with_runtime(home.path(), root.path(), "~/other", &runtime).is_ok());
    }

    // #98 review: the sockets are in `marv/mesh` under the runtime dir, so a folder *inside* `marv/` is refused too.
    #[test]
    fn a_folder_inside_the_mesh_runtime_dir_is_refused() {
        let home = TempDir::new("writable-home7");
        let root = TempDir::new("writable-root7");
        let runtime = home.path().join("xdg");
        std::fs::create_dir_all(runtime.join("marv/mesh")).unwrap();
        let why = with_runtime(home.path(), root.path(), "~/xdg/marv/mesh", &runtime).unwrap_err();
        assert!(why.contains("runtime sockets"), "{why}");
    }

    // #98 review: Marv's config folder is refused wherever MARV_CONFIG_DIR puts it — a visible one isn't caught by the
    // hidden-entry rule, and its config.json's hooks run outside the sandbox (its tool-output files are there too).
    #[test]
    fn marvs_config_folder_is_refused_wherever_it_is() {
        let home = TempDir::new("writable-home8");
        let root = TempDir::new("writable-root8");
        let config = home.path().join("marv-config");
        std::fs::create_dir_all(config.join("tool-output")).unwrap();
        std::fs::create_dir_all(home.path().join("holder/inner")).unwrap();
        let nested = home.path().join("holder/inner/cfg");
        std::fs::create_dir_all(&nested).unwrap();
        for (entry, config) in [("~/marv-config", &config), ("~/marv-config/tool-output", &config), ("~/holder", &nested)] {
            let why = resolve_entry(entry, home.path(), root.path(), None, Some(config)).unwrap_err();
            assert!(why.contains("Marv's config folder"), "{entry}: {why}");
        }
        // Next to it is fine.
        std::fs::create_dir_all(home.path().join("beads")).unwrap();
        assert!(resolve_entry("~/beads", home.path(), root.path(), None, Some(&config)).is_ok());
    }
}

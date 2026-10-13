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
    ProtectedPath { rule: ProtectedRule::EnvFile, pattern: ".env*", why: "dotenv loaders read it in your shell, and it often holds secrets" },
    ProtectedPath { rule: ProtectedRule::Envrc, pattern: ".envrc", why: "direnv runs it in your shell" },
    ProtectedPath { rule: ProtectedRule::Bunfig, pattern: "bunfig.toml", why: "bun runs its preload scripts for every bun command" },
    ProtectedPath { rule: ProtectedRule::McpJson, pattern: ".mcp.json", why: "it names programs Marv starts outside the sandbox" },
    ProtectedPath { rule: ProtectedRule::AgentInstructions, pattern: "AGENTS.md, CLAUDE.md", why: "every future session follows them" },
    ProtectedPath { rule: ProtectedRule::MarvDir, pattern: ".marv/**", why: "Marv's project config, skills, agents and checks" },
    ProtectedPath { rule: ProtectedRule::EditorConfig, pattern: ".vscode/, .idea/", why: "your editor runs its tasks" },
    ProtectedPath { rule: ProtectedRule::PackageScripts, pattern: "package.json scripts", why: "install and lifecycle scripts run on your next install" },
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

/// The rule protecting `path` (absolute, or relative to `root`), checked on the path as written and on where it
/// really leads. A path that leads outside the project is an error (`resolve_in_project` refuses it anyway).
pub fn protected_rule(root: &Path, path: &Path) -> Result<Option<ProtectedRule>, SandboxError> {
    let absolute = normalize(&root.join(path));
    let real_root = std::fs::canonicalize(root)?;
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
    let config = std::fs::read_to_string(root.join(".git/config")).ok()?;
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
            found = Some(if path.is_absolute() { path } else { root.join(path) }); // the last one wins, as in git
        }
    }
    found
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
        Some(LIFECYCLE_SCRIPTS.iter().filter_map(|k| map.and_then(|m| m.get(*k)).map(|v| ((*k).to_string(), v.clone()))).collect())
    };
    let Some(now) = scripts(after) else { return true };
    match before.map(scripts) {
        None => !now.is_empty(),
        Some(None) => true,
        Some(Some(was)) => was != now,
    }
}

/// Protected paths that exist at the project's top level (and the hooks folder), mounted read-only on top of the
/// writable project. `.git` is left to `git_guard` (an approved `git_write` command must write it).
///
/// TODO(port): bwrap can only mount what exists, so a command can still *create* `.envrc` or a nested `AGENTS.md`.
/// Planned: placeholders for the top-level names (like `.git`'s) and a post-command scan for new protected files
/// anywhere, which then needs the user's approval to keep.
pub fn protected_mounts(root: &Path) -> Vec<PathBuf> {
    let mut mounts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        let mut names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
        names.sort();
        for name in names {
            let rel = Path::new(&name);
            match rule_for_relative(rel) {
                Some(ProtectedRule::GitDir) | None => {}
                Some(_) => {
                    let path = root.join(rel);
                    // bwrap can't mount on a symlink; its target is checked by the path rules instead.
                    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.file_type().is_symlink()) {
                        mounts.push(path);
                    }
                }
            }
        }
    }
    if let Some(hooks) = hooks_path(root)
        && hooks.starts_with(root)
        && hooks.is_dir()
        && !mounts.contains(&hooks)
    {
        mounts.push(hooks);
    }
    mounts
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
}

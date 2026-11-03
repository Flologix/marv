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

/// Desktop and app settings and state, shown again read-only (bubblewrap) when the project is the home folder (or
/// holds it): asked about the desktop, the window manager or an app's settings, a model otherwise had to leave the
/// sandbox (`sandbox: false`, asked each time) just to read `~/.config/hypr`. What's private in them stays hidden:
/// [`HOME_MASKS`] names the folders (browsers, password managers, chat apps, agents' logins), and files named like a
/// secret ([`secret_name`]) and every socket and FIFO found in them are `/dev/null` (`bwrap::home_masks`). Never in
/// any other project, where the home is a stand-in.
pub const HOME_SHOWN: &[&str] = &[".config", ".local/share", ".local/state"];

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
    // Inside HOME_SHOWN: logins, cookies, keys, message stores and shell history.
    ".config/chromium",
    ".config/google-chrome",
    ".config/google-chrome-beta",
    ".config/google-chrome-unstable",
    ".config/BraveSoftware",
    ".config/microsoft-edge",
    ".config/vivaldi",
    ".config/opera",
    ".mozilla",
    ".librewolf",
    ".zen",
    ".thunderbird",
    ".config/1Password",
    ".config/op",
    ".config/Bitwarden",
    ".config/bitwarden",
    ".config/keepassxc",
    ".local/share/keepassxc",
    ".password-store",
    ".config/Signal",
    ".config/discord",
    ".config/Slack",
    ".config/Element",
    ".local/share/TelegramDesktop",
    ".config/rclone",
    ".config/hub",
    ".config/glab-cli",
    ".config/doctl",
    ".config/heroku",
    ".config/netlify",
    ".config/vercel",
    ".config/configstore",
    ".config/github-copilot",
    ".config/opencode",
    ".local/share/opencode",
    ".config/marv",
    ".local/share/marv",
    ".local/state/marv",
    ".config/Code/User/globalStorage",
    ".config/Cursor/User/globalStorage",
    ".config/zed/credentials",
    ".local/share/zed/db",
    ".local/share/atuin",
    ".local/share/fish/fish_history",
    ".local/state/zsh",
    ".local/share/recently-used.xbel",
    ".local/share/Trash",
    ".config/Docker Desktop",
    ".config/tailscale",
    ".config/wireguard",
    ".config/NetworkManager",
    ".config/evolution",
    ".local/share/evolution",
    ".config/goa-1.0",
    ".config/kwalletrc",
    ".local/share/kwalletd",
    ".config/chromium-headless",
    ".config/microsoft-edge-dev",
    ".config/microsoft-edge-beta",
    ".config/ai.flologix.marv",
    ".local/share/ai.flologix.marv",
    ".config/harbor",
    ".local/share/pki",
    ".local/share/Steam/config/htmlcache",
];

/// Folders found in a [`HOME_SHOWN`] folder that hold an app's web storage, where Electron apps (VS Code, Obsidian,
/// Slack) and embedded browsers keep login tokens: hidden whole, by name (lowercase), wherever they are.
pub fn secret_dir_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "local storage" | "session storage" | "indexeddb" | "databases" | "webstorage")
}

/// Whether a file found in a [`HOME_SHOWN`] folder is named like a secret, and is masked: a login token, a private
/// key, a cookie jar, a password database, a dotenv file. Lowercase match on the name only, so it errs toward hiding
/// (a `tokens.css` is hidden too).
pub fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    const EXACT: &[&str] = &[
        "auth.json",
        "cookies",
        "cookies.sqlite",
        "login data",
        "web data",
        "hosts.yml",
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        ".env",
        ".netrc",
        "netrc",
        ".pgpass",
        "keychain",
    ];
    const PARTS: &[&str] =
        &["token", "secret", "credential", "password", "passwd", "apikey", "api_key", "api-key", "private", "oauth", "session"];
    const EXTENSIONS: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".kdbx", ".gpg", ".asc", ".age", ".jks", ".keystore"];
    EXACT.contains(&name.as_str())
        || name.starts_with(".env.")
        || PARTS.iter().any(|p| name.contains(p))
        || EXTENSIONS.iter().any(|e| name.ends_with(e))
}

/// Whether the project is the home folder or a parent of it (`/`, `/home`), judged on real paths. Then the sandbox
/// can't hide the home behind a stand-in without hiding the project too: it shows it read-only, with Marv's config,
/// credentials and every hidden top-level entry masked ([`crate::bwrap::sandbox_args`]).
pub fn home_project(root: &Path, home: &Path) -> bool {
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    contains(&real(root), &real(home))
}

/// In a home-folder project, the visible two-level folder of the home (`Projects/shop`) a file the edit tools would
/// change lies in, lexically and after realpath alike, so a person can answer "always" for that one folder. None
/// (the change asks every time) for a file higher up (`~/notes.md`, `~/Desktop/a.txt`), under a hidden entry at
/// either level, led elsewhere by a symlink, or in a folder that is on `path` or holds a folder that is (a script
/// written there would run as a command).
pub fn home_subfolder(root: &Path, home: &Path, absolute: &Path, path: &str) -> Option<PathBuf> {
    if !home_project(root, home) {
        return None;
    }
    let real_home = std::fs::canonicalize(home).ok()?;
    let folder = |base: &Path, file: &Path| -> Option<PathBuf> {
        let parts: Vec<_> = file.strip_prefix(base).ok()?.components().collect();
        // Two folders and at least the file below them.
        if parts.len() < 3 || parts[..2].iter().any(|p| p.as_os_str().to_string_lossy().starts_with('.')) {
            return None;
        }
        Some(parts[..2].iter().collect())
    };
    let lexical = folder(&crate::paths::normalize(home), absolute)?;
    let real = folder(&real_home, &crate::paths::real_with_missing(absolute).ok()?)?;
    if lexical != real {
        return None;
    }
    let full = real_home.join(&real);
    let on_path = std::env::split_paths(path).any(|entry| {
        let entry = std::fs::canonicalize(&entry).unwrap_or(entry);
        entry.starts_with(&full)
    });
    (!on_path).then_some(real)
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

    #[test]
    fn a_home_file_change_gets_its_two_level_visible_folder_or_nothing() {
        let home = crate::paths::test_dir::TempDir::new("home-subfolder");
        let h = home.path();
        std::fs::create_dir_all(h.join("Projects/shop/site")).unwrap();
        std::fs::create_dir_all(h.join("Projects/.hidden")).unwrap();
        std::fs::create_dir_all(h.join("go/bin")).unwrap();
        std::fs::create_dir_all(h.join("Documents/other")).unwrap();
        std::os::unix::fs::symlink(h.join("Documents/other"), h.join("Projects/shop/away")).unwrap();
        std::os::unix::fs::symlink(h.join("Documents"), h.join("Projects/linked")).unwrap();
        let at = |rel: &str, path: &str| home_subfolder(h, h, &h.join(rel), path);
        assert_eq!(at("Projects/shop/site/index.html", ""), Some(PathBuf::from("Projects/shop")));
        assert_eq!(at("Projects/shop/new/deeper/a.css", "/usr/bin"), Some(PathBuf::from("Projects/shop")));
        // Too high up, hidden, through a symlink, or a folder on PATH.
        assert_eq!(at("notes.md", ""), None);
        assert_eq!(at("Projects/readme.md", ""), None);
        assert_eq!(at("Projects/shop", ""), None);
        assert_eq!(at("Projects/.hidden/x/a", ""), None);
        assert_eq!(at(".config/app/a", ""), None);
        assert_eq!(at("Projects/shop/away/a.txt", ""), None);
        assert_eq!(at("Projects/linked/other/a.txt", ""), None);
        let path = format!("/usr/bin:{}", h.join("go/bin").display());
        assert_eq!(at("go/bin/tool", &path), None);
        let path = format!("/usr/bin:{}", h.join("Projects/shop/site").display());
        assert_eq!(at("Projects/shop/a.txt", &path), None);
        // Not a home-folder project: nothing to scope.
        assert_eq!(home_subfolder(&h.join("Projects/shop"), h, &h.join("Projects/shop/site/a"), ""), None);
    }
}

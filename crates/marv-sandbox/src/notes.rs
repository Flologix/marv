//! The `Marv:` notes bash appends to a sandboxed command's output, and the guesses that decide when.
//!
//! Ports `GIT_HINT`, `HOME_NOTE`, `NETWORK_NOTE`, `GH_NOTE`, `touchesHiddenHome` and `looksLikeNetworkFailure` from
//! Marv's `src/tools/bash.ts`. They live here because each one explains the sandbox to the model.
//!
//! - The home note: the prompt said "hidden", but `ls ~/.marv` showed `skills/` (mounted back in), so a model decided
//!   home was readable, took the missing config and trajectories for real, and searched for them for 21 steps.
//! - The network note (issue #23): after a fresh `gh auth login`, `gh auth status` without network said the token was
//!   invalid, and the model walked the user through logging out and in again.
//! - The gh note (issue #27): the GitHub MCP server is the way to GitHub; never mount gh's config into the sandbox. Where
//!   the session has the github tool (Marv #47), `GH_TOOL_NOTE` points there instead.
//! - Rust's `regex` has no lookahead: Marv's `(?=…)` word ends are consuming groups here (only `is_match` is used), and
//!   `touches_hidden_home` checks the character after `~` by hand.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::home::{TOOLCHAINS, contains};
use crate::paths::normalize;

/// Appended when a confined command fails on the read-only `.git`, so the model learns how to do it.
pub const GIT_HINT: &str = "Marv: .git is read-only for commands that run without asking. To change the repository, run the command again with git_write: true (the user is asked).";

/// Appended when a sandboxed command looks into the hidden home folder.
pub const HOME_NOTE: &str = "Marv: in the sandbox the home folder is an empty stand-in: only toolchain folders (like ~/.bun, ~/.local/share/mise, ~/.asdf), git config and ~/.marv/skills show through, read-only. Anything else there (Marv's own config, sessions and trajectories in ~/.marv included) is invisible, so a missing file or an empty folder under ~ says nothing about the real home folder.";

/// Appended when a sandboxed command without network fails in a way that looks like the network's missing.
pub const NETWORK_NOTE: &str = "Marv: this command ran in the sandbox without network, and its failure looks like it may come from that: there, every connection and DNS lookup fails, so logins look invalid and servers look down. Before drawing conclusions, run it again with network: true.";

/// Added to the network note when the failed command is `gh`.
pub const GH_NOTE: &str = "For GitHub, don't suggest gh auth login, and never mount gh's config into the sandbox (a token there must not reach command output): suggest the GitHub MCP server instead, which the user connects in ~/.marv/mcp.json — see Marv's README, \"Connect GitHub\".";

/// [`GH_NOTE`] where the session has the github tool (Marv #47): gh with the user's login is one call away.
pub const GH_TOOL_NOTE: &str = "For GitHub, use the github tool instead: it runs gh with the user's own login, outside the sandbox (in bash gh has no login, and that's on purpose). Don't suggest gh auth login, and never mount gh's config into the sandbox.";

fn network_errors() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)could not resolve (?:host|hostname|proxy)|temporary failure in name resolution|name or service not known|getaddrinfo|\b(?:ENOTFOUND|EAI_AGAIN|ECONNREFUSED|ECONNRESET|ENETUNREACH|EHOSTUNREACH|ETIMEDOUT|ETIMEOUT)\b|ConnectionRefused|network is unreachable|connection refused|connection timed out|failed to connect|unable to connect|dns error|no such host|error connecting to").unwrap()
    })
}

fn network_commands() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|[\s;&|(`])(?:gh|curl|wget|ssh|scp|rsync|ping|git\s+(?:fetch|pull|push|clone|ls-remote|submodule\s+update)|(?:npm|pnpm|yarn|bun)\s+(?:install|i|add|ci|update|upgrade|publish|view|info|outdated|create)|pip3?\s+install|uv\s+(?:sync|add|pip\s+install)|cargo\s+(?:install|fetch|update|add)|go\s+(?:get|mod\s+download)|docker\s+(?:pull|push|login))(?:$|[\s;&|)`])").unwrap()
    })
}

fn gh_command() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|[\s;&|(`])gh(?:$|[\s;&|)`])").unwrap())
}

/// Whether a failed command's failure may come from the missing network: its output has a network error, or the
/// command is one that needs the network. A guess, for the note only.
pub fn looks_like_network_failure(command: &str, output: &str) -> bool {
    network_errors().is_match(output) || network_commands().is_match(command)
}

/// Whether the command runs `gh` (the gh note follows the network note).
pub fn runs_gh(command: &str) -> bool {
    gh_command().is_match(command)
}

/// Whether a command refers to a part of the home folder the sandbox hides: not the project (often inside the home
/// folder) and not the toolchain folders it mounts. A guess from the command's text: `~`, `$HOME`, `${HOME}` or the
/// home's path at the start of a word.
pub fn touches_hidden_home(command: &str, root: &Path, home: &Path) -> bool {
    let home_text = home.to_string_lossy();
    let refs = Regex::new(&format!(r#"(?:^|[\s=:'"(<>;|&])(?:~|\$HOME|\$\{{HOME\}}|{})"#, regex::escape(&home_text))).unwrap();
    let word_end = |c: char| c == '/' || c.is_whitespace() || "'\");|&<>".contains(c);
    let path_stop = |c: char| c.is_whitespace() || "'\";|&<>()".contains(c);
    let mut at = 0;
    while let Some(m) = refs.find_at(command, at) {
        let after = &command[m.end()..];
        at = m.end();
        if !after.chars().next().is_none_or(word_end) {
            continue; // `~other`, `/home/meow`: not the home folder
        }
        let rest: String = after.chars().take_while(|c| !path_stop(*c)).collect();
        at += rest.len();
        let path = normalize(&home.join(rest.trim_start_matches('/')));
        if !contains(root, &path) && !TOOLCHAINS.iter().any(|dir| contains(&home.join(dir), &path)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touches_the_home_folder_or_anything_hidden_in_it() {
        let touches = |c: &str| touches_hidden_home(c, Path::new("/home/me/proj"), Path::new("/home/me"));
        assert!(touches("ls -la ~/.marv/"));
        assert!(touches("find ~ -maxdepth 4 -name trajectories"));
        assert!(touches("cd ~ && ls"));
        assert!(touches("cat \"$HOME/.ssh/config\""));
        assert!(touches("ls ${HOME}"));
        assert!(touches("ls /home/me/.config 2>/dev/null"));
        assert!(touches("echo x; ls ~/.marv/config.json|head"));
    }

    #[test]
    fn not_the_project_the_shown_folders_or_another_tilde() {
        let touches = |c: &str| touches_hidden_home(c, Path::new("/home/me/proj"), Path::new("/home/me"));
        assert!(!touches("ls -la ~/.bun/bin/ | grep marv"));
        assert!(!touches("ls ~/.marv/skills/notes"));
        assert!(!touches("cat ~/proj/src/a.ts"));
        assert!(!touches("cat /home/me/proj/package.json"));
        assert!(!touches("git log HEAD~1 && git diff main~2"));
        assert!(!touches("ls /home/meow ~other"));
        assert!(!touches("bun test"));
    }

    #[test]
    fn network_errors_in_the_output() {
        assert!(looks_like_network_failure("./fetch.sh", "curl: (6) Could not resolve host: example.com"));
        assert!(looks_like_network_failure("bun run x", "TypeError: getaddrinfo ETIMEOUT example.com"));
        assert!(looks_like_network_failure("bun run x", "code: \"ConnectionRefused\""));
        assert!(looks_like_network_failure("make", "dns error: Temporary failure in name resolution"));
        assert!(looks_like_network_failure("python3 a.py", "OSError: [Errno 101] Network is unreachable"));
        assert!(looks_like_network_failure("node a.js", "Error: connect ECONNREFUSED 1.2.3.4:443"));
    }

    #[test]
    fn commands_that_need_the_network() {
        assert!(looks_like_network_failure("gh auth status", "The token in default is invalid."));
        assert!(looks_like_network_failure("cd app && git pull --rebase", ""));
        assert!(looks_like_network_failure("git clone https://github.com/a/b", ""));
        assert!(looks_like_network_failure("bun install", "error: lockfile had changes"));
        assert!(looks_like_network_failure("npm i zod", ""));
        assert!(looks_like_network_failure("ls; curl -sS https://x.dev", ""));
        assert!(runs_gh("gh auth status") && !runs_gh("ls src/gh"));
    }

    #[test]
    fn not_ordinary_failures() {
        assert!(!looks_like_network_failure("bun test", "1 fail\nexpect(received).toBe(expected)"));
        assert!(!looks_like_network_failure("git status", "fatal: not a git repository"));
        assert!(!looks_like_network_failure("git commit -m x", "nothing to commit"));
        assert!(!looks_like_network_failure("cat ghost.txt", "No such file or directory"));
        assert!(!looks_like_network_failure("bun run build", "error: Could not resolve: './missing.ts'"));
        assert!(!looks_like_network_failure("ls src/gh", ""));
        assert!(!looks_like_network_failure(
            "npx tsc --noEmit",
            "src/a.ts(3,7): error TS2322: Type 'string' is not assignable to type 'number'."
        ));
        assert!(looks_like_network_failure("bunx cowsay hi", "error: getaddrinfo EAI_AGAIN registry.npmjs.org"));
    }
}

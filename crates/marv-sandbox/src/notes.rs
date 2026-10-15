//! The `Marv:` notes bash appends to a sandboxed command's output, and the guesses that decide when.
//!
//! Ports `GIT_HINT`, `HOME_NOTE`, `NETWORK_NOTE`, `GH_NOTE`, `BACKGROUND_NOTE` (and its process-door variant),
//! `touchesHiddenHome`, `backgroundsProcess` and `looksLikeNetworkFailure` from
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

/// Appended to a sandboxed command in a project that is the home folder (or holds it) when it failed or looked at a
/// hidden part of the home (marv-rs): there the home is shown read-only with its private parts hidden, so a write
/// fails with "Read-only file system" and `~/.ssh` reads as empty, and a model that doesn't know why retries.
pub const HOME_PROJECT_NOTE: &str = "Marv: the project is your home folder, so in the sandbox it is read-only, and its hidden entries (~/.marv, ~/.ssh, ~/.config, ~/.local and the like, apart from toolchain folders and git config) and any socket in it are hidden: an empty or missing one there says nothing about the real home folder. Commands can write only to /tmp. To change a file, use edit_file or write_file (the user is asked); for more, ask the user to start Marv in a project folder.";

/// Appended when a sandboxed command without network fails in a way that looks like the network's missing.
pub const NETWORK_NOTE: &str = "Marv: this command ran in the sandbox without network, and its failure looks like it may come from that: there, every connection and DNS lookup fails, so logins look invalid and servers look down. Before drawing conclusions, run it again with network: true.";

/// Added to the result of a retry that was run with network because the same command (its tail aside) already failed
/// without it the way the network note describes (#81): a model ignored that note and re-sent the command 11 times.
pub const ESCALATED_NOTE: &str = "Marv: this command already failed without network in a way that looked like the missing network, so this time Marv ran it with network: true, which the user has to approve. If network isn't granted, don't run it again: do without it (look for what's already on this machine) or tell the user it needs the network.";

/// Added to the network note when the failed command is `gh`.
pub const GH_NOTE: &str = "For GitHub, don't suggest gh auth login, and never mount gh's config into the sandbox (a token there must not reach command output): suggest the GitHub MCP server instead, which the user connects in ~/.marv/mcp.json — see Marv's README, \"Connect GitHub\".";

/// [`GH_NOTE`] where the session has the github tool (Marv #47): gh with the user's login is one call away.
pub const GH_TOOL_NOTE: &str = "For GitHub, use the github tool instead: it runs gh with the user's own login, outside the sandbox (in bash gh has no login, and that's on purpose). Don't suggest gh auth login, and never mount gh's config into the sandbox.";

/// Appended when a sandboxed command leaves processes running in the background: each command runs in its own PID
/// namespace, so everything it started is killed when it ends, and (without network) its localhost isn't the user's
/// anyway. A model started a dev server detached, curled it successfully inside the sandbox, and told the user "the
/// server's up" with a dead link (Marv issue #50).
pub const BACKGROUND_NOTE: &str = "Marv: anything this command started in the background was stopped when the command ended, and a server in the sandbox can't be reached from the user's browser (the sandbox has its own network namespace; with network: true its localhost is the host's). Don't say a server is running for the user: to check one yourself, run it in the foreground within one command, query it there, and let the command end; to let the user try it, give them the command to run in their own terminal.";

/// The same, where the session has the process door (Marv #51): the way to keep one running is the process tool.
pub const BACKGROUND_NOTE_PROCESS: &str = "Marv: anything this command started in the background was stopped when the command ended, and a server in the sandbox can't be reached from the user's browser (the sandbox has its own network namespace; with network: true its localhost is the host's). Don't say a server is running for the user: to check one yourself, run it in the foreground within one command, query it there, and let the command end; to keep a server or watcher running for the user, use the process tool instead.";

/// Whether a command leaves processes running in the background, as far as its text can say: a background `&` (an
/// unquoted one, outside `&&`), `nohup`/`setsid`/`disown`, or a command whose very job is to keep serving (`dev`,
/// `start`, `serve` — as the command or a `run`/package-runner script, not as an argument like `echo start`). A guess,
/// for [`BACKGROUND_NOTE`] only. Marv's `backgroundsProcess`; its `(?=$|[\s#])` lookahead is a consuming group here.
pub fn backgrounds_process(command: &str) -> bool {
    static RES: OnceLock<[Regex; 4]> = OnceLock::new();
    let [quoted, comment, redirect, amp] = RES.get_or_init(|| {
        [
            Regex::new(r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'"#).unwrap(),
            Regex::new(r"(^|\s)#[^\n]*").unwrap(),
            Regex::new(r"(\d)>&(\d)").unwrap(),
            Regex::new(r"(?:^|[\s;|])&(?:$|[\s#])").unwrap(),
        ]
    });
    let clean = quoted.replace_all(command, " "); // quoted text, including any & in it
    let clean = comment.replace_all(&clean, "$1"); // comments
    let clean = clean.replace("&&", " ");
    let clean = redirect.replace_all(&clean, "$1=$2"); // 2>&1 isn't backgrounding
    if amp.is_match(&clean) {
        return true;
    }
    let serving = |word: Option<&&str>| word.is_some_and(|w| matches!(*w, "dev" | "start" | "serve"));
    clean.split([';', '|', '\n', '&']).any(|segment| {
        let words: Vec<&str> = segment.split_whitespace().collect();
        let Some(head) = words.first() else { return false };
        if matches!(*head, "nohup" | "setsid" | "disown") || serving(Some(head)) {
            return true;
        }
        if matches!(*head, "npm" | "pnpm" | "yarn" | "bun" | "npx" | "bunx") {
            return if words.get(1) == Some(&"run") { serving(words.get(2)) } else { serving(words.get(1)) };
        }
        false
    })
}

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

/// Whether a command that exited 0 hid a failure like the missing network's behind `||` (`git clone … || true`, #81):
/// it has a `||`, it's a command that needs the network, and its output has a network error. Stricter than
/// [`looks_like_network_failure`], since nothing failed as far as the exit code says.
pub fn hidden_network_failure(command: &str, output: &str) -> bool {
    command.contains("||") && network_commands().is_match(command) && network_errors().is_match(output)
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

/// Whether [`HOME_PROJECT_NOTE`] goes after a command in a home-folder project: it failed, or its text names the home
/// (`~`, `$HOME`, its path) or a hidden entry (a word starting with `.` and a letter: `.ssh`, `./.config`). A guess.
pub fn needs_home_project_note(command: &str, home: &Path, failed: bool) -> bool {
    if failed {
        return true;
    }
    let refs = Regex::new(&format!(
        r#"(?:^|[\s=:'"(<>;|&])(?:~|\$HOME|\$\{{HOME\}}|{})|(?:^|[\s=:'"(<>;|&/])\.[A-Za-z_]"#,
        regex::escape(&home.to_string_lossy())
    ))
    .unwrap();
    refs.is_match(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backgrounds_process_sees_a_trailing_amp_nohup_setsid_disown_and_serve_dev_start_commands() {
        for command in [
            "node server.cjs &",
            "nohup node server.cjs > log 2>&1",
            "setsid nohup node server.cjs &",
            "bun run dev & disown",
            "bunx serve -l 4789",
            "npm run dev",
        ] {
            assert!(backgrounds_process(command), "{command}");
        }
    }

    #[test]
    fn backgrounds_process_not_for_ordinary_foreground_commands() {
        for command in ["bun test", "git push origin main", "echo 'run it with &'", "cat a && b", "echo start; ls"] {
            assert!(!backgrounds_process(command), "{command}");
        }
        // `wait` alone, or a comment after &, doesn't background anything; `wait` after a real & does.
        assert!(!backgrounds_process("echo done # not &"));
        assert!(backgrounds_process("bun run dev & wait"));
        // 2>&1 isn't backgrounding.
        assert!(!backgrounds_process("cargo build 2>&1"));
    }

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
    fn the_home_project_note_after_a_failure_or_a_look_at_the_home() {
        let home = Path::new("/home/me");
        assert!(needs_home_project_note("touch notes.txt", home, true));
        for command in ["ls ~/.ssh", "cat .marv/config.json", "ls -a ./.config", "ls $HOME", "cat /home/me/x"] {
            assert!(needs_home_project_note(command, home, false), "{command}");
        }
        for command in ["ls Projects", "cat notes.txt", "git log HEAD~1", "ls ./src"] {
            assert!(!needs_home_project_note(command, home, false), "{command}");
        }
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

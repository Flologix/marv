//! A shell command's "what it really runs" key, so retries that only differ in their tail compare equal (#81), and
//! a guess at whether a command only reads.
//!
//! New in marv-rs. A model stuck on a command that can't succeed (a `git clone` in the sandbox without network)
//! re-sent it 11 times, each time with another tail: `&& du -sh`, `|| true`, `&& command -v tokei`, `| tail -2`.
//! The key keeps everything that can change whether the work fails, and drops only what can't:
//! - the text is split into its top-level parts at `&&`, `||`, `;`, `|`, `&` and newlines (not inside quotes, not
//!   after a backslash);
//! - setup before the work is kept (`cd a && make` and `cd b && make` are different work), and so is the first
//!   part that isn't setup (`cd`, `pushd`, `set`, `export`, a bare `VAR=value`);
//! - after it, the key ends at `&&` (what follows never runs when the work fails), `;`, `&` or a newline (what
//!   follows runs anyway and doesn't change whether the work failed);
//! - `|| true` and `|| :` are dropped (they only hide the failure); any other `|| …` is kept;
//! - pipe stages that only show part of the output are dropped (`| tail -2`, `| head`, `| wc -l`, `| cat`, `| less`);
//!   any other stage is kept (`| grep missing` is different work from `| tail`);
//! - fd redirections to the terminal or nowhere (`2>&1`, `>/dev/null`, `2>/dev/null`, `&>/dev/null`) are dropped,
//!   and whitespace is collapsed.
//!
//! A guess from the text, used by the bash tool's network escalation and the agent loop's failing-command guard;
//! never for a security decision.

/// How a part was joined to the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sep {
    Start,
    And,
    Or,
    Pipe,
    /// `;` or a newline.
    Semi,
    Amp,
}

impl Sep {
    fn text(self) -> &'static str {
        match self {
            Sep::Start => "",
            Sep::And => "&&",
            Sep::Or => "||",
            Sep::Pipe => "|",
            Sep::Semi => ";",
            Sep::Amp => "&",
        }
    }
}

/// The key of `command` (see the module notes).
pub fn command_key(command: &str) -> String {
    let mut out = String::new();
    let mut working = false;
    for (sep, part) in parts(command) {
        if working {
            match sep {
                Sep::And | Sep::Semi | Sep::Amp | Sep::Start => break,
                Sep::Pipe if is_display_stage(&part) => continue,
                Sep::Or if matches!(part.as_str(), "true" | ":") => continue,
                Sep::Pipe | Sep::Or => {}
            }
        }
        working |= !is_setup(&part);
        if !out.is_empty() {
            out.push(' ');
            out.push_str(sep.text());
            out.push(' ');
        }
        out.push_str(&part);
    }
    out
}

/// Whether every part of `command` only reads: setup, or a command from a short list of readers (`ls`, `cat`,
/// `grep`, `echo`, `whoami`, `git status`, …), with no output redirected to a file. A guess from the text, for the
/// failing-command guard only (a successful command that may have changed things resets it); never for a security
/// decision.
pub fn is_read_only_command(command: &str) -> bool {
    const READERS: [&str; 46] = [
        "echo", "printf", "pwd", "whoami", "id", "groups", "ls", "cat", "head", "tail", "wc", "grep", "egrep", "fgrep", "rg",
        "ag", "which", "type", "command", "env", "printenv", "date", "uname", "hostname", "stat", "file", "du", "df", "tree",
        "realpath", "readlink", "basename", "dirname", "test", "[", "true", "false", ":", "sleep", "sort", "uniq", "cut", "tr",
        "diff", "cmp", "less",
    ];
    const GIT_READERS: [&str; 10] =
        ["status", "log", "diff", "show", "rev-parse", "ls-files", "describe", "blame", "grep", "remote"];
    let all = parts(command);
    !all.is_empty()
        && all.iter().all(|(_, part)| {
            if part.contains('>') {
                return false;
            }
            if is_setup(part) {
                return true;
            }
            let mut words = part.split_whitespace().skip_while(|w| is_assignment(w));
            let Some(head) = words.next() else { return true };
            match head {
                "git" => words.next().is_some_and(|sub| GIT_READERS.contains(&sub)),
                "find" => !part.contains("-delete") && !part.contains("-exec"),
                _ => READERS.contains(&head),
            }
        })
}

/// The cleaned, non-empty top-level parts with the separator before each.
fn parts(command: &str) -> Vec<(Sep, String)> {
    split_parts(command)
        .into_iter()
        .map(|(sep, part)| (sep, clean(&part)))
        .filter(|(_, part)| !part.is_empty())
        .enumerate()
        .map(|(i, (sep, part))| (if i == 0 { Sep::Start } else { sep }, part))
        .collect()
}

/// Top-level parts, each with the separator before it.
fn split_parts(command: &str) -> Vec<(Sep, String)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut sep = Sep::Start;
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut chars = command.chars().peekable();
    let push = |current: &mut String, parts: &mut Vec<(Sep, String)>, sep: &mut Sep, next: Sep| {
        parts.push((*sep, std::mem::take(current)));
        *sep = next;
    };
    while let Some(c) = chars.next() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if !single => {
                current.push(c);
                escaped = true;
            }
            '\'' if !double => {
                single = !single;
                current.push(c);
            }
            '"' if !single => {
                double = !double;
                current.push(c);
            }
            _ if single || double => current.push(c),
            '|' if chars.peek() == Some(&'|') => {
                chars.next();
                push(&mut current, &mut parts, &mut sep, Sep::Or);
            }
            '|' => push(&mut current, &mut parts, &mut sep, Sep::Pipe),
            // `2>&1`, `>&2`, `&>file`: a redirection, not a separator.
            '&' if current.ends_with('>') || chars.peek() == Some(&'>') => current.push(c),
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                push(&mut current, &mut parts, &mut sep, Sep::And);
            }
            '&' => push(&mut current, &mut parts, &mut sep, Sep::Amp),
            ';' | '\n' => push(&mut current, &mut parts, &mut sep, Sep::Semi),
            _ => current.push(c),
        }
    }
    parts.push((sep, current));
    parts
}

/// Whitespace collapsed, redirections to the terminal or nowhere dropped.
fn clean(part: &str) -> String {
    const DROPPED: [&str; 6] = ["2>&1", ">&2", "1>&2", ">/dev/null", "2>/dev/null", "&>/dev/null"];
    part.split_whitespace().filter(|word| !DROPPED.contains(word)).collect::<Vec<_>>().join(" ")
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Setup before the work: `cd`, `pushd`, `set`, `export`, or nothing but `VAR=value` assignments.
fn is_setup(part: &str) -> bool {
    let head = part.split_whitespace().next().unwrap_or("");
    matches!(head, "cd" | "pushd" | "set" | "export") || part.split_whitespace().all(is_assignment)
}

/// A pipe stage that only shows part of what came before.
fn is_display_stage(part: &str) -> bool {
    let head = part.split_whitespace().next().unwrap_or("");
    matches!(head, "tail" | "head" | "wc" | "cat" | "less" | "more")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLONE: &str = "git clone https://github.com/FlologixAI/marv /tmp/marv";

    #[test]
    fn the_tails_from_issue_81_all_have_the_clones_key() {
        for command in [
            CLONE.to_string(),
            format!("{CLONE} 2>&1 | tail -2"),
            format!("{CLONE} 2>&1 | tail -2 && du -sh /tmp/marv"),
            format!("{CLONE} && du -sh /tmp/marv"),
            format!("{CLONE} || true"),
            format!("{CLONE} || :"),
            format!("{CLONE}; true"),
            format!("{CLONE}   2>&1 && command -v tokei"),
            format!("{CLONE}; ls"),
            format!("{CLONE} >/dev/null 2>&1 & wait"),
            format!("{CLONE} | head -5 | wc -l"),
        ] {
            assert_eq!(command_key(&command), CLONE, "{command}");
        }
    }

    // Greptile on #111: a key too wide blocked other work.
    #[test]
    fn different_work_has_different_keys() {
        assert_ne!(command_key("cd a && make"), command_key("cd b && make"));
        assert_eq!(command_key("cd a && make && make install"), "cd a && make");
        assert_ne!(command_key("cat build.log | grep missing"), command_key("cat build.log | tail"));
        assert_eq!(command_key("cat build.log | grep missing | head -3"), "cat build.log | grep missing");
        assert_ne!(command_key("make || make clean"), command_key("make"));
        assert_ne!(command_key("CI=1 npm test"), command_key("npm test"));
        assert_ne!(command_key("cargo test"), command_key("cargo build"));
        assert_ne!(command_key("npm test -- -t foo"), command_key("npm test -- -t bar"));
        assert_eq!(command_key("echo test"), "echo test");
    }

    #[test]
    fn separators_inside_quotes_dont_split() {
        assert_eq!(command_key("echo 'a && b | c'; ls"), "echo 'a && b | c'");
        assert_eq!(command_key(r#"grep "x|y" f | head -1"#), r#"grep "x|y" f"#);
        assert_eq!(command_key(r"echo a\;b; ls"), r"echo a\;b");
    }

    #[test]
    fn a_command_of_setup_only_is_its_own_key() {
        assert_eq!(command_key("cd  src"), "cd src");
        assert_eq!(command_key("export A=1; cd src"), "export A=1 ; cd src");
        assert_eq!(command_key("true"), "true");
        assert_eq!(command_key(""), "");
    }

    #[test]
    fn readers_are_read_only_and_anything_that_may_change_things_isnt() {
        for command in [
            "echo test",
            "whoami",
            "ls -la && pwd",
            "cd src && grep -rn x . | head",
            "git status",
            "git log -3",
            "find . -name '*.rs'",
        ] {
            assert!(is_read_only_command(command), "{command}");
        }
        for command in [
            "npm install",
            "cargo build",
            "echo x > file",
            "git checkout main",
            "find . -delete",
            "ls && rm -rf build",
            "pip install -r requirements.txt",
            "",
        ] {
            assert!(!is_read_only_command(command), "{command}");
        }
    }
}

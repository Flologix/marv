//! A shell command's "what it really runs" key, so retries that only differ in their tail compare equal (#81).
//!
//! New in marv-rs. A model stuck on a command that can't succeed (a `git clone` in the sandbox without network)
//! re-sent it 11 times, each time with another tail: `&& du -sh`, `|| true`, `&& command -v tokei`, `| tail -2`.
//! The key is the first command of the chain that does the work:
//! - the text is split into its top-level parts at `&&`, `||`, `;`, `|`, `&` and newlines (not inside quotes, not
//!   after a backslash);
//! - stages after a `|` are dropped (`| tail -2`, `| head`, `| grep x`): the first stage is what runs;
//! - parts that only set things up are skipped: `cd …`, `pushd …`, `set …`, `export …`, `true`, `:`;
//! - fd redirections to the terminal or nowhere (`2>&1`, `>/dev/null`, `2>/dev/null`, `&>/dev/null`) are dropped,
//!   and whitespace is collapsed.
//!
//! The key of a command that is nothing but setup is the whole command, collapsed. A guess from the text, used by
//! the bash tool's network escalation and the agent loop's failing-command guard; never for a security decision.

/// The key of `command` (see the module notes).
pub fn command_key(command: &str) -> String {
    let mut first_stage = true;
    for (part, after_pipe) in split_parts(command) {
        let part = clean(&part);
        if part.is_empty() {
            continue;
        }
        // A stage after `|` filters what the first one printed.
        if after_pipe && !first_stage {
            continue;
        }
        first_stage = false;
        if is_setup(&part) {
            continue;
        }
        return part;
    }
    clean(command)
}

/// Top-level parts, each with whether a single `|` came right before it.
fn split_parts(command: &str) -> Vec<(String, bool)> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut after_pipe = false;
    let (mut single, mut double, mut escaped) = (false, false, false);
    let mut chars = command.chars().peekable();
    let push = |current: &mut String, parts: &mut Vec<(String, bool)>, after: bool| {
        parts.push((std::mem::take(current), after));
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
                push(&mut current, &mut parts, after_pipe);
                after_pipe = false;
            }
            '|' => {
                push(&mut current, &mut parts, after_pipe);
                after_pipe = true;
            }
            // `2>&1`, `>&2`, `&>file`: a redirection, not a separator.
            '&' if current.ends_with('>') || chars.peek() == Some(&'>') => current.push(c),
            '&' => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                push(&mut current, &mut parts, after_pipe);
                after_pipe = false;
            }
            ';' | '\n' => {
                push(&mut current, &mut parts, after_pipe);
                after_pipe = false;
            }
            _ => current.push(c),
        }
    }
    push(&mut current, &mut parts, after_pipe);
    parts
}

/// Whitespace collapsed, redirections to the terminal or nowhere dropped.
fn clean(part: &str) -> String {
    const DROPPED: [&str; 6] = ["2>&1", ">&2", "1>&2", ">/dev/null", "2>/dev/null", "&>/dev/null"];
    part.split_whitespace().filter(|word| !DROPPED.contains(word)).collect::<Vec<_>>().join(" ")
}

fn is_setup(part: &str) -> bool {
    let head = part.split_whitespace().next().unwrap_or("");
    matches!(head, "cd" | "pushd" | "set" | "export" | "true" | ":")
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
            format!("{CLONE} && du -sh /tmp/marv"),
            format!("{CLONE} || true"),
            format!("{CLONE}   2>&1 && command -v tokei"),
            format!("cd /tmp && {CLONE}; ls"),
            format!("{CLONE} >/dev/null 2>&1 & wait"),
        ] {
            assert_eq!(command_key(&command), CLONE, "{command}");
        }
    }

    #[test]
    fn different_commands_have_different_keys() {
        assert_ne!(command_key("cargo test"), command_key("cargo build"));
        assert_ne!(command_key("npm test -- -t foo"), command_key("npm test -- -t bar"));
        assert_ne!(command_key("cd a && make"), command_key("cd b && make test"));
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
        assert_eq!(command_key("true"), "true");
        assert_eq!(command_key(""), "");
    }
}

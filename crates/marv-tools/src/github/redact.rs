//! A second line of defense behind the github tool's refusals (#47): whatever gh prints goes to the model, the
//! transcript and the trajectory, so GitHub's token shapes are replaced before it reaches any of them. The refusals
//! keep gh from printing a token (auth, config, --verbose); this catches one that turns up anyway (a token pasted
//! into an issue, a CI log that echoed one). Bash's host commands (#52) go through it too.
//!
//! Ports Marv's `src/tools/github/redact.ts`.

use std::sync::LazyLock;

use regex::Regex;

/// Personal, OAuth, user-to-server, server-to-server and refresh tokens; fine-grained PATs; installation tokens.
static TOKENS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36,}|\bgithub_pat_[A-Za-z0-9_]{22,}|\bv1\.[0-9a-f]{40}\b").unwrap());

pub const REDACTED: &str = "[redacted token]";

/// The text with every token replaced, and how many there were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub count: usize,
}

pub fn redact(text: &str) -> Redacted {
    let count = TOKENS.find_iter(text).count();
    let text = if count == 0 { text.to_string() } else { TOKENS.replace_all(text, REDACTED).into_owned() };
    Redacted { text, count }
}

/// Marv's "Marv: N token(s) was/were redacted from this output." note, or nothing.
pub fn redacted_note(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => "Marv: 1 token was redacted from this output.".into(),
        n => format!("Marv: {n} tokens were redacted from this output."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a36() -> String {
        format!("{}B1c2D3", "a".repeat(30))
    }

    #[test]
    fn replaces_each_github_token_shape_and_counts_them() {
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let got = redact(&format!("token {prefix}{} here", a36()));
            assert_eq!(got, Redacted { text: "token [redacted token] here".into(), count: 1 });
        }
        let pat = format!("github_pat_{}_{}", "x".repeat(22), "y".repeat(59));
        assert_eq!(redact(&pat), Redacted { text: REDACTED.into(), count: 1 });
        let v1 = format!("v1.{}01234567", "0123456789abcdef".repeat(2));
        assert_eq!(redact(&v1), Redacted { text: REDACTED.into(), count: 1 });
        assert_eq!(redact(&format!("a ghp_{}\nb gho_{}", a36(), a36())).count, 2);
    }

    #[test]
    fn leaves_look_alikes_and_short_strings_alone() {
        for text in [
            format!("ghx_{}", a36()),
            "ghp_short".into(),
            "github_pat_short".into(),
            "v1.abc".into(),
            "the ghp_ prefix".into(),
            format!("xghp_{}", a36()),
        ] {
            assert_eq!(redact(&text), Redacted { text: text.clone(), count: 0 });
        }
    }
}

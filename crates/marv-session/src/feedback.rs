//! Implicit feedback: what the user's next message says about the turn before it. Ports Marv's `src/feedback.ts`.
//!
//! A plain keyword match, on purpose: free, instant, predictable, and the matched phrase is logged with the score, so
//! the data can be re-scored later with something smarter. Explicit labels (`/good`, `/bad`, `/label`) are stronger
//! and logged separately. The next message is often praise plus the next task ("Perfect. Next, fix the broken
//! link"), so:
//! - a correction must point back at what was done ("it's broken", "revert that", "you broke"): a bare "broken",
//!   "undo" or "wrong" usually describes the next task. Corrections count anywhere in the message.
//! - praise counts only in the first sentence, and not in a question ("that works? no").
//!
//! Port note: Rust's `regex` has no lookaround. Marv's `(?<![\w-])…(?![\w-])` word edges become a consumed
//! `(?:^|[^A-Za-z0-9_-])` / `(?:$|[^A-Za-z0-9_-])` around a capture group (only the first match is used, so the
//! consumed edge never hides one), and the sentence split is done by hand.

use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplySignal {
    /// 1 good, -1 bad.
    pub score: i8,
    /// The words that decided it, as the user wrote them.
    pub phrase: String,
}

/// Whole words or phrases only: "great" mustn't fire inside "great-grandparent".
fn words(alternatives: &[&str], emoji: &[&str]) -> Regex {
    let emoji = if emoji.is_empty() { String::new() } else { format!("|({})", emoji.join("|")) };
    Regex::new(&format!(r"(?i)(?:^|[^A-Za-z0-9_-])({})(?:$|[^A-Za-z0-9_-]){emoji}", alternatives.join("|"))).unwrap()
}

// Checked first, over the whole message: a correction is often polite ("thanks, but it's still broken").
static NEGATIVE: LazyLock<Regex> = LazyLock::new(|| {
    words(
        &[
            "that'?s (?:not|wrong)",
            "not what i",
            "(?:it'?s|that'?s|this is|is|are) (?:wrong|broken)",
            "(?:doesn'?t|does not|didn'?t|did not|isn'?t|is not|still not|not) work(?:ing|s)?",
            "still (?:broken|failing|fails|wrong|not working)",
            "(?:is|are) (?:still )?failing",
            "fails? now",
            "now fails?",
            "you broke",
            r"broke (?:it|that|the \w+)",
            "revert (?:that|it|this|those|the (?:last )?changes?)",
            "undo (?:that|it|this|those|the (?:last )?(?:change|changes|edit|edits)|your (?:change|changes|edit|edits))",
            "why did you",
            "not (?:great|good|right|correct|quite)",
        ],
        &["👎"],
    )
});

static POSITIVE: LazyLock<Regex> = LazyLock::new(|| {
    words(
        &[
            "thanks?(?: you)?",
            "thx",
            "ty",
            "perfect",
            "great",
            "awesome",
            "amazing",
            "brilliant",
            "excellent",
            "nice(?: one)?",
            "well done",
            "good job",
            "lgtm",
            "looks good(?: to me)?",
            r"^\s*(?:it|that|this|everything) works(?: now| great| perfectly| fine)?",
            "works (?:now|great|perfectly)",
            r"^\s*exactly",
            "exactly what i (?:wanted|needed|asked for)",
            "love it",
        ],
        &["👍", "🎉", "🙏"],
    )
});

fn phrase(re: &Regex, text: &str) -> Option<String> {
    let caps = re.captures(text)?;
    let m = caps.get(1).or_else(|| caps.get(2))?;
    Some(m.as_str().trim().to_string())
}

/// The first sentence: where praise for the last turn goes, before the next request.
fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    let mut prev = None;
    for (i, c) in text.char_indices() {
        if c == '\n' || (c.is_whitespace() && matches!(prev, Some('.' | '!' | '?'))) {
            return &text[..i];
        }
        prev = Some(c);
    }
    text
}

/// Whether a message praises or corrects the turn before it, or neither.
pub fn classify_reply(raw: &str) -> Option<ReplySignal> {
    let text = raw.replace(['‘', '’'], "'");
    if let Some(phrase) = phrase(&NEGATIVE, &text) {
        return Some(ReplySignal { score: -1, phrase });
    }
    let first = first_sentence(&text);
    if first.ends_with('?') {
        return None; // "that works?" asks, it doesn't praise
    }
    phrase(&POSITIVE, first).map(|phrase| ReplySignal { score: 1, phrase })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(text: &str) -> Option<i8> {
        classify_reply(text).map(|s| s.score)
    }

    #[test]
    fn positive() {
        for text in [
            "thanks!",
            "Thank you, that works",
            "perfect",
            "great, now add tests",
            "nice one",
            "lgtm",
            "looks good to me",
            "awesome 🎉",
            "👍",
            "it works now",
            "exactly what I wanted",
        ] {
            assert_eq!(score(text), Some(1), "{text}");
        }
    }

    #[test]
    fn negative() {
        for text in [
            "that's wrong",
            "no, that's not what I asked",
            "it doesn't work",
            "still broken",
            "the tests are still failing",
            "please revert that",
            "undo the last change",
            "why did you delete the file?",
            "not great",
            "👎",
        ] {
            assert_eq!(score(text), Some(-1), "{text}");
        }
    }

    #[test]
    fn no_signal() {
        for text in [
            "now add a README",
            "what does this function do?",
            "no worries, can you also add tests",
            "great-grandparent class names",
            "fix the broken test",
            "undo stack implementation please",
            "explain how the cache works",
            "make sure it works on windows",
            "write exactly 3 retries",
            "that works? no",
        ] {
            assert_eq!(score(text), None, "{text}");
        }
    }

    #[test]
    fn praise_then_the_next_task() {
        for (text, s) in [
            ("Perfect. Next, add an undo button", 1),
            ("great, now fix the broken link", 1),
            ("Thanks! Now handle the wrong password case", 1),
            ("nice. can you add a revert endpoint?", 1),
            ("Perfect. But the tests fail now.", -1),
            ("it still doesn’t work", -1),
            ("this isn’t working", -1),
        ] {
            assert_eq!(score(text), Some(s), "{text}");
        }
    }

    #[test]
    fn the_phrase_is_kept() {
        assert_eq!(classify_reply("Thanks! next").unwrap().phrase, "Thanks");
    }
}

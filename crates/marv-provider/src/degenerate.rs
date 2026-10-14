//! Marv #96 (marv-rs only, not in Marv yet): a guard against a degenerate stream.
//!
//! GLM-5.3-flash once streamed newline-only reasoning for six minutes (91,481 deltas, 2.95M characters, 7,341 of them
//! not `\n`) after the work was done, until the eval harness killed it. Nothing in the reply was worth keeping, and a
//! user would just have seen Marv "thinking" forever.
//!
//! The adapters feed every text and reasoning delta to a [`DegenerateGuard`]. Once the last [`TAIL`] bytes of either
//! one are (almost) only whitespace, or one short piece (at most [`MAX_PERIOD`] bytes) repeated over and over, the
//! stream counts as degenerate: the adapter aborts the request and asks once more with [`DEGENERATE_NOTE`]; a second
//! degenerate reply ends the turn with an error.
//!
//! - "Almost" whitespace: at most 1% of the tail is anything else, so a few stray characters in a sea of newlines
//!   (the GLM stream's 7,341) don't hide it. Real prose or code is never 99% whitespace over 16 KB.
//! - Tool-call arguments aren't looked at: a file being written may well repeat itself.
//! - The tail is checked once per [`CHECK_EVERY`] new bytes, so a long healthy reply costs next to nothing.
//! - A text loop that repeats a longer passage is the agent loop's job (`detect_text_repetition`).

use marv_core::ProviderEvent;

/// How much of the latest text (and, separately, reasoning) is looked at, in bytes.
pub const TAIL: usize = 16 * 1024;
/// The longest piece whose repetition counts as degenerate, in bytes.
pub const MAX_PERIOD: usize = 64;
/// The tail is checked again after this many new bytes.
pub const CHECK_EVERY: usize = 1024;
/// At most this share of a tail may be other than whitespace for it to count as whitespace.
pub const MAX_NON_WHITESPACE: f64 = 0.01;

/// Sent (as a user turn after the history) with the one retry after a degenerate reply.
pub const DEGENERATE_NOTE: &str = "(Automatic note from Marv, the program running you) Your previous response was discarded: it had turned into whitespace or the same short piece repeated over and over. Answer again, without repeating yourself.";

/// Whether `tail` is degenerate: (almost) only whitespace, or one piece of at most [`MAX_PERIOD`] bytes repeated.
pub fn is_degenerate(tail: &[u8]) -> bool {
    if tail.is_empty() {
        return false;
    }
    let other = tail.iter().filter(|b| !b.is_ascii_whitespace()).count();
    if other as f64 <= tail.len() as f64 * MAX_NON_WHITESPACE {
        return true;
    }
    (1..=MAX_PERIOD).any(|period| tail.len() >= 4 * period && tail[period..].iter().zip(tail).all(|(a, b)| a == b))
}

/// The latest bytes of one kind of output.
#[derive(Debug, Default)]
struct Tail {
    bytes: Vec<u8>,
    unchecked: usize,
}

impl Tail {
    /// Adds a delta; true when the tail has degenerated.
    fn push(&mut self, delta: &str) -> bool {
        self.bytes.extend_from_slice(delta.as_bytes());
        if self.bytes.len() > 2 * TAIL {
            self.bytes.drain(..self.bytes.len() - TAIL);
        }
        self.unchecked += delta.len();
        if self.bytes.len() < TAIL || self.unchecked < CHECK_EVERY {
            return false;
        }
        self.unchecked = 0;
        is_degenerate(&self.bytes[self.bytes.len() - TAIL..])
    }
}

/// Watches one attempt's text and reasoning. One per response.
#[derive(Debug, Default)]
pub struct DegenerateGuard {
    text: Tail,
    reasoning: Tail,
    streamed: u64,
}

impl DegenerateGuard {
    /// Looks at one event from the stream; true when the reply has degenerated.
    pub fn check(&mut self, event: &ProviderEvent) -> bool {
        match event {
            ProviderEvent::TextDelta(t) => {
                self.streamed += t.len() as u64;
                self.text.push(t)
            }
            ProviderEvent::ThinkingDelta(t) => self.check_reasoning(t),
            _ => false,
        }
    }

    /// Looks at reasoning that arrived some other way (OpenRouter's `reasoning_details` blocks, when the chunk has no
    /// plain `reasoning`); true when it has degenerated.
    pub fn check_reasoning(&mut self, text: &str) -> bool {
        self.streamed += text.len() as u64;
        self.reasoning.push(text)
    }

    /// How many bytes of text and reasoning this attempt streamed (all thrown away when it degenerates).
    pub fn streamed(&self) -> u64 {
        self.streamed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thinking(t: &str) -> ProviderEvent {
        ProviderEvent::ThinkingDelta(t.into())
    }

    /// Feeds `delta` until the guard trips; how many bytes that took (`None`: never, within `limit` bytes).
    fn trips_after(delta: impl Fn(usize) -> String, limit: usize) -> Option<usize> {
        let mut guard = DegenerateGuard::default();
        let mut n = 0;
        let mut i = 0;
        while n < limit {
            let d = delta(i);
            n += d.len();
            i += 1;
            if guard.check(&thinking(&d)) {
                return Some(n);
            }
        }
        None
    }

    #[test]
    fn whitespace_tails_and_short_repeats_are_degenerate() {
        let table: &[(&str, &[u8], bool)] = &[
            ("newlines", &[b'\n'; TAIL], true),
            ("spaces and newlines", &b" \n\t \r\n".repeat(TAIL / 6), true),
            ("one short chunk", &b"ok. ".repeat(TAIL / 4), true),
            ("a 64-byte line", &[&[b'-'; 63][..], b"\n"].concat().repeat(TAIL / 64), true),
            ("prose", b"The quick brown fox jumps over the lazy dog, then naps in the sun until evening comes.", false),
            (
                "a longer repeated passage (the agent loop's job)",
                &[b'x'; 65].iter().copied().chain(*b"y").collect::<Vec<_>>().repeat(250),
                false,
            ),
            ("empty", b"", false),
        ];
        for (name, tail, expected) in table {
            assert_eq!(is_degenerate(tail), *expected, "{name}");
        }
        // Mostly newlines with a stray character every 200 bytes (0.5%): still whitespace (the GLM stream).
        let sparse: Vec<u8> = (0..TAIL).map(|i| if i % 200 == 0 { b'a' } else { b'\n' }).collect();
        assert!(is_degenerate(&sparse));
        // Code is indented, but nowhere near 99% whitespace.
        assert!(!is_degenerate(b"    if x {\n        y += 1;\n    }\n"));
    }

    #[test]
    fn the_guard_trips_on_a_newline_flood_within_its_tail() {
        let at = trips_after(|_| "\n\n\n\n".into(), 1 << 20).expect("never tripped");
        assert!(at <= TAIL + CHECK_EVERY, "{at}");
    }

    #[test]
    fn a_long_healthy_reply_never_trips() {
        let words = ["alpha ", "beta, ", "gamma\n", "delta; ", "epsilon ", "zeta "];
        let healthy = |i: usize| format!("{}{} ", words[i % words.len()], i);
        assert_eq!(trips_after(healthy, 512 * 1024), None);
        // Indented code with blank lines.
        let code = |i: usize| format!("    fn f{i}() {{\n        return {i};\n    }}\n\n");
        assert_eq!(trips_after(code, 512 * 1024), None);
    }

    #[test]
    fn text_and_reasoning_are_watched_separately_and_tool_calls_not_at_all() {
        let mut guard = DegenerateGuard::default();
        for _ in 0..(TAIL / 8) {
            // Half a tail of newlines in each never fills either tail.
            assert!(!guard.check(&ProviderEvent::TextDelta("\n\n\n\n".into())));
            assert!(!guard.check(&thinking("\n\n\n\n")));
            let call = ProviderEvent::ToolCallDelta { index: 0, name: "write_file".into(), text: "\n".repeat(64) };
            assert!(!guard.check(&call));
        }
        assert_eq!(guard.streamed(), 2 * 4 * (TAIL / 8) as u64);
    }
}

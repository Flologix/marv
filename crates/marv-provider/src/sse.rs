//! Server-sent events: how LLM APIs stream a reply over plain HTTP. Ports `src/provider/sse.ts`.
//!
//! The response body is text made of events separated by blank lines:
//!
//! ```text
//! data: {"choices":[{"delta":{"content":"Hel"}}]}
//!
//! data: {"choices":[{"delta":{"content":"lo"}}]}
//!
//! data: [DONE]
//! ```
//!
//! - Lines starting with `:` are comments (keep-alives: OpenRouter sends `: OPENROUTER PROCESSING`).
//! - Network chunks can split anywhere, even mid-line or mid-UTF-8 character, so bytes are
//!   buffered until a full event is seen. Boundaries are ASCII, so splitting the byte buffer at
//!   one never cuts a character in half.
//! - Both `\n\n` and CRLF boundaries count (`/\r?\n\r?\n/` in Marv).
//! - The stream may end without a trailing blank line: the rest is still an event.
//! - Several `data:` lines in one event are joined with `\n`; one leading space is dropped.

use futures::{Stream, StreamExt};

/// Incremental SSE decoder: feed it network chunks, get each event's `data` payload.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a chunk and returns the data of every event it completed, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((start, end)) = event_boundary(&self.buffer) {
            let event = String::from_utf8_lossy(&self.buffer[..start]).into_owned();
            self.buffer.drain(..end);
            if let Some(data) = data_of(&event) {
                out.push(data);
            }
        }
        out
    }

    /// The stream ended: whatever is buffered is the last event (it may lack a blank line).
    pub fn finish(self) -> Option<String> {
        data_of(&String::from_utf8_lossy(&self.buffer))
    }
}

/// The leftmost match of `\r?\n\r?\n` in `buf`, as (start, end) byte offsets.
fn event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        if buf[i] != b'\n' {
            continue;
        }
        let start = if i > 0 && buf[i - 1] == b'\r' { i - 1 } else { i };
        match (buf.get(i + 1), buf.get(i + 2)) {
            (Some(b'\n'), _) => return Some((start, i + 2)),
            (Some(b'\r'), Some(b'\n')) => return Some((start, i + 3)),
            _ => {}
        }
    }
    None
}

/// The `data` of one event, or `None` when it has no data lines (a comment, a keep-alive).
fn data_of(event: &str) -> Option<String> {
    let lines: Vec<&str> = event
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Yields the `data` payload of each event in a byte stream (e.g. `reqwest::Response::bytes_stream()`).
/// A transport error is passed through and ends nothing by itself: the adapter decides (a drop
/// mid-stream is retriable, see [`crate::retry`]).
pub fn parse_sse<S, B, E>(body: S) -> impl Stream<Item = Result<String, E>> + Send
where
    S: Stream<Item = Result<B, E>> + Send + Unpin,
    B: AsRef<[u8]> + Send,
    E: Send,
{
    futures::stream::unfold(
        (body, Some(SseDecoder::new()), std::collections::VecDeque::<Result<String, E>>::new()),
        |(mut body, mut decoder, mut ready)| async move {
            loop {
                if let Some(item) = ready.pop_front() {
                    return Some((item, (body, decoder, ready)));
                }
                let dec = decoder.as_mut()?;
                match body.next().await {
                    Some(Ok(chunk)) => ready.extend(dec.push(chunk.as_ref()).into_iter().map(Ok)),
                    Some(Err(err)) => ready.push_back(Err(err)),
                    None => ready.extend(decoder.take()?.finish().map(Ok)),
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/sse.test.ts.

    /// A body that arrives in the given pieces, the way network chunks split arbitrarily.
    fn collect(pieces: &[&str]) -> Vec<String> {
        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        for piece in pieces {
            out.extend(decoder.push(piece.as_bytes()));
        }
        out.extend(decoder.finish());
        out
    }

    #[test]
    fn yields_the_data_of_each_event() {
        assert_eq!(collect(&["data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n"]), ["{\"a\":1}", "{\"b\":2}", "[DONE]"]);
    }

    #[test]
    fn reassembles_events_split_across_network_chunks() {
        assert_eq!(collect(&["da", "ta: {\"a\"", ":1}\n", "\ndata: x\n\n"]), ["{\"a\":1}", "x"]);
    }

    #[test]
    fn skips_comments_and_keep_alives() {
        assert_eq!(collect(&[": OPENROUTER PROCESSING\n\ndata: hi\n\n"]), ["hi"]);
    }

    #[test]
    fn handles_crlf_line_endings_and_a_final_event_without_a_blank_line() {
        assert_eq!(collect(&["data: one\r\n\r\ndata: two"]), ["one", "two"]);
    }

    #[test]
    fn joins_multi_line_data_with_newlines() {
        assert_eq!(collect(&["data: line 1\ndata: line 2\n\n"]), ["line 1\nline 2"]);
    }

    #[test]
    fn a_character_split_across_chunks_survives() {
        let bytes = "data: café\n\n".as_bytes();
        let (a, b) = bytes.split_at(10); // inside the two-byte "é"
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(a).is_empty());
        assert_eq!(decoder.push(b), ["café"]);
    }

    #[tokio::test]
    async fn parse_sse_streams_over_a_byte_stream() {
        let chunks: Vec<Result<&[u8], ()>> = vec![Ok(b"data: a\n"), Ok(b"\ndata: b")];
        let out: Vec<_> = parse_sse(futures::stream::iter(chunks)).collect().await;
        assert_eq!(out, vec![Ok("a".to_string()), Ok("b".to_string())]);
    }
}

//! Renders the model's Markdown as structured terminal output. Ports `src/ui/Markdown.tsx`
//! (tests: `tests/markdown.test.tsx`).
//!
//! - Lists with hanging indents (markers in fixed-width columns: `•`/`◦`/`▪` by depth, right-aligned
//!   numbers, `☑`/`☐` for tasks); tight lists have no gap, loose lists one blank line.
//! - Inline code coloured (`theme::CODE`) without backticks; bold/italic/strikethrough; a link shows
//!   its text then ` (href)` dimmed, a bare URL once, underlined; images as `[image: alt]`.
//! - Code blocks get their language on a dim line and a left rule; headings bold in the accent;
//!   blockquotes a `▎` bar; tables aligned as plain text with a `─` rule; `hr` a rule.
//! - Marv used `marked`; here `pulldown-cmark` (GFM tables, strikethrough, task lists).
//!
//! **Streaming:** a reply is re-rendered many times a second while it streams, and parsing grows
//! faster than the text (~20 ms at 11 KB with marked). [`StreamLexer`] caches the blocks before the
//! last safe boundary and re-parses only the unfinished end. A cut is a blank line outside any fence
//! (tracked by its character and length: ```` ```sh ```` inside ````` ````markdown ````` doesn't
//! close it, and a backtick "fence" with backticks after it is inline code) or HTML comment, before
//! a *complete* line that starts a new block and isn't a list item (a loose list cut there would
//! render as several lists, and jump when the reply is parsed whole). An unclosed fence renders
//! fine mid-stream. Finished messages are parsed whole once. Known limit: a reference link and its
//! definition in different chunks render as text until the reply is finished. The test property:
//! every streamed prefix of random replies renders the same as parsing it whole.
//!
//! **Security:** [`render`] takes the raw text and runs `printable()` on the whole of it before
//! lexing, so the lexer's cache sees one consistent text (an unfinished escape sequence at the end
//! is dropped until it completes; the lexer starts over if the text no longer extends its cache).

use marv_core::printable;
use ratatui::text::Line;

/// List bullets by depth.
pub const BULLETS: [&str; 3] = ["•", "◦", "▪"];

/// A parsed block (paragraph, list, code, ...), kept owned so streamed prefixes can be cached.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    // TODO(port): the block tree built from pulldown-cmark events.
    _private: (),
}

/// Parses the whole text into blocks (Marv's `lex`).
pub fn lex(text: &str) -> Vec<Block> {
    let _ = text;
    todo!("port: lex with pulldown-cmark (GFM options) into Block")
}

/// Incremental parser for a reply that's still streaming (Marv's `createStreamLexer`).
#[derive(Debug, Clone, Default)]
pub struct StreamLexer {
    /// The parsed prefix: always ends at a block boundary, outside any fence.
    done: String,
    done_blocks: Vec<Block>,
}

impl StreamLexer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The blocks for `text`, which normally extends the last text seen.
    pub fn parse(&mut self, text: &str) -> Vec<Block> {
        let _ = (text, &self.done, &self.done_blocks);
        todo!("port: createStreamLexer's boundary scan (FENCE, LIST_ITEM, HTML comments)")
    }
}

/// Lays out blocks as lines `width` columns wide (hanging indents need the width).
pub fn layout(blocks: &[Block], width: u16) -> Vec<Line<'static>> {
    let _ = (blocks, width);
    todo!("port: Blocks/Block/List/Table/inline from Markdown.tsx")
}

/// Raw Markdown from a model or tool → terminal lines. `streaming`: the reply's lexer while it
/// streams in; None for a finished message (parsed whole).
pub fn render(text: &str, width: u16, streaming: Option<&mut StreamLexer>) -> Vec<Line<'static>> {
    let clean = printable(text);
    let blocks = match streaming {
        Some(lexer) => lexer.parse(&clean),
        None => lex(&clean),
    };
    layout(&blocks, width)
}

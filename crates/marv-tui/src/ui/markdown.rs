//! Renders the model's Markdown as structured terminal output. Ports `src/ui/Markdown.tsx`
//! (tests: `tests/markdown.test.tsx`).
//!
//! - Lists with hanging indents (markers in fixed-width columns: `•`/`◦`/`▪` by depth, right-aligned
//!   numbers, `☑`/`☐` for tasks). Within an item, a loose list puts one blank line between its
//!   blocks, a tight one none; items themselves follow each other directly, as in Marv.
//! - Inline code coloured (`theme::CODE`) without backticks; bold/italic/strikethrough; a link shows
//!   its text then ` (href)` dimmed, a bare URL once, underlined; images as `[image: alt]`.
//! - Code blocks get their language on a dim line and a left rule; headings bold in the accent;
//!   blockquotes a `▎` bar; tables aligned as plain text with a `─` rule; `hr` a rule.
//! - Marv used `marked`; here `pulldown-cmark` (GFM tables, strikethrough, task lists). pulldown-cmark
//!   has no GFM "extended autolinks", so a bare `http(s)://` URL in text is found here
//!   ([`autolink`]) and drawn like marked's autolink (once, underlined, in `theme::CODE`).
//! - Wrapping is Ink's (wrap-ansi `hard`, `trim: false`): words break at spaces, a word longer than
//!   the line is cut, leading spaces are kept (code keeps its indentation). One difference: Ink
//!   started the row after an exactly full one with the separating space; here that space goes.
//!   Tabs are drawn as two spaces (a cell holds one column; a raw tab would move the cursor).
//! - The quote bar sits in a 2-column marker beside the quote's first row, as Ink drew it (the
//!   marker box stretched, its one-line text didn't).
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
//! The stream lexer also keeps each finished block's lines at the last width (Marv relied on
//! React's memo for that), so a streaming frame wraps only the unfinished end.
//!
//! **Security:** [`render`] takes the raw text and runs `printable()` on the whole of it before
//! lexing, so the lexer's cache sees one consistent text (an unfinished escape sequence at the end
//! is dropped until it completes; the lexer starts over if the text no longer extends its cache).

use std::borrow::Cow;

use marv_core::printable;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::theme;

/// List bullets by depth.
pub const BULLETS: [&str; 3] = ["•", "◦", "▪"];

/// A piece of inline text in one style (marked's inline tokens, already resolved to how they look).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: Style,
}

impl Run {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Self { text: text.into(), style }
    }
}

/// One list item: its task box (if a task), whether the list is loose, and its blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub task: Option<bool>,
    pub loose: bool,
    pub blocks: Vec<Block>,
}

/// A parsed block (marked's block tokens), kept owned so streamed prefixes can be cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// A paragraph, or a tight list item's text (marked's "text" block).
    Paragraph(Vec<Run>),
    Heading(Vec<Run>),
    Code {
        lang: Option<String>,
        text: String,
    },
    List {
        ordered: bool,
        start: u64,
        items: Vec<ListItem>,
    },
    Quote(Vec<Block>),
    /// Plain cell text, the header row first.
    Table(Vec<Vec<String>>),
    Rule,
    Html(String),
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// Parses the whole text into blocks (Marv's `lex`).
pub fn lex(text: &str) -> Vec<Block> {
    let mut events = Parser::new_ext(text, options());
    Lexer::default().blocks(&mut events).blocks
}

/// What a run of blocks held besides the blocks.
#[derive(Default)]
struct Read {
    blocks: Vec<Block>,
    /// A real paragraph (not a tight item's text): the item is loose.
    paragraphs: bool,
}

/// Walks pulldown-cmark's events into [`Block`]s.
#[derive(Default)]
struct Lexer {
    /// The task marker of the item being read (it can arrive inside the item's paragraph).
    task: Option<bool>,
}

impl Lexer {
    /// Blocks until the enclosing container's end (or the end of the text).
    fn blocks<'a>(&mut self, events: &mut impl Iterator<Item = Event<'a>>) -> Read {
        let mut read = Read::default();
        let mut text: Vec<Run> = Vec::new();
        let flush = |text: &mut Vec<Run>, read: &mut Read| {
            if !text.is_empty() {
                read.blocks.push(Block::Paragraph(std::mem::take(text)));
            }
        };
        while let Some(event) = events.next() {
            match event {
                Event::End(_) => break,
                Event::Start(tag) => match tag {
                    Tag::Paragraph => {
                        flush(&mut text, &mut read);
                        read.paragraphs = true;
                        let mut runs = Vec::new();
                        self.inlines(events, Style::new(), false, &mut runs);
                        read.blocks.push(Block::Paragraph(runs));
                    }
                    Tag::Heading { .. } => {
                        flush(&mut text, &mut read);
                        let mut runs = Vec::new();
                        self.inlines(events, Style::new(), false, &mut runs);
                        read.blocks.push(Block::Heading(runs));
                    }
                    Tag::BlockQuote(_) => {
                        flush(&mut text, &mut read);
                        read.blocks.push(Block::Quote(self.blocks(events).blocks));
                    }
                    Tag::CodeBlock(kind) => {
                        flush(&mut text, &mut read);
                        let lang = match kind {
                            CodeBlockKind::Fenced(info) => Some(info.trim().to_string()).filter(|l| !l.is_empty()),
                            CodeBlockKind::Indented => None,
                        };
                        let mut code = plain(events);
                        if code.ends_with('\n') {
                            code.pop();
                        }
                        read.blocks.push(Block::Code { lang, text: code });
                    }
                    Tag::HtmlBlock => {
                        flush(&mut text, &mut read);
                        let html = plain(events);
                        read.blocks.push(Block::Html(html.trim_end_matches('\n').to_string()));
                    }
                    Tag::List(start) => {
                        flush(&mut text, &mut read);
                        let list = self.list(events, start);
                        read.blocks.push(list);
                    }
                    Tag::Table(_) => {
                        flush(&mut text, &mut read);
                        read.blocks.push(table(events));
                    }
                    Tag::Emphasis
                    | Tag::Strong
                    | Tag::Strikethrough
                    | Tag::Superscript
                    | Tag::Subscript
                    | Tag::Link { .. }
                    | Tag::Image { .. } => self.inline_tag(events, tag, Style::new(), &mut text),
                    // Not enabled (footnotes, definition lists, metadata) or out of place: keep the text.
                    _ => {
                        flush(&mut text, &mut read);
                        let inner = self.blocks(events);
                        read.blocks.extend(inner.blocks);
                    }
                },
                Event::Rule => {
                    flush(&mut text, &mut read);
                    read.blocks.push(Block::Rule);
                }
                event => self.inline(event, Style::new(), false, &mut text),
            }
        }
        flush(&mut text, &mut read);
        read
    }

    fn list<'a>(&mut self, events: &mut impl Iterator<Item = Event<'a>>, start: Option<u64>) -> Block {
        let mut items = Vec::new();
        while let Some(event) = events.next() {
            match event {
                Event::Start(Tag::Item) => {
                    let outer = self.task.take();
                    let read = self.blocks(events);
                    items.push(ListItem { task: self.task.take(), loose: read.paragraphs, blocks: read.blocks });
                    self.task = outer;
                }
                Event::End(_) => break,
                _ => {}
            }
        }
        Block::List { ordered: start.is_some(), start: start.unwrap_or(1), items }
    }

    /// Inline events until the enclosing tag's end.
    fn inlines<'a>(&mut self, events: &mut impl Iterator<Item = Event<'a>>, style: Style, link: bool, runs: &mut Vec<Run>) {
        while let Some(event) = events.next() {
            match event {
                Event::End(_) => break,
                Event::Start(tag) => self.inline_tag(events, tag, style, runs),
                event => self.inline(event, style, link, runs),
            }
        }
    }

    fn inline_tag<'a>(&mut self, events: &mut impl Iterator<Item = Event<'a>>, tag: Tag<'a>, style: Style, runs: &mut Vec<Run>) {
        match tag {
            Tag::Emphasis => self.inlines(events, style.add_modifier(Modifier::ITALIC), false, runs),
            Tag::Strong => self.inlines(events, style.add_modifier(Modifier::BOLD), false, runs),
            Tag::Strikethrough => self.inlines(events, style.add_modifier(Modifier::CROSSED_OUT), false, runs),
            Tag::Link { dest_url, .. } => {
                let mut inner = Vec::new();
                self.inlines(events, style, true, &mut inner);
                let text: String = inner.iter().map(|r| r.text.as_str()).collect();
                // A bare URL (autolink) is shown once; otherwise the text, then the target.
                if text == *dest_url || format!("mailto:{text}") == *dest_url {
                    push(runs, Run::new(text, style.fg(theme::CODE).add_modifier(Modifier::UNDERLINED)));
                } else {
                    for run in inner {
                        push(runs, Run { style: run.style.add_modifier(Modifier::UNDERLINED), ..run });
                    }
                    push(runs, Run::new(format!(" ({dest_url})"), style.fg(theme::DIM)));
                }
            }
            Tag::Image { .. } => {
                let mut inner = Vec::new();
                self.inlines(events, style, true, &mut inner);
                let alt: String = inner.iter().map(|r| r.text.as_str()).collect();
                push(runs, Run::new(format!("[image: {alt}]"), style.fg(theme::DIM)));
            }
            _ => self.inlines(events, style, false, runs),
        }
    }

    fn inline(&mut self, event: Event<'_>, style: Style, link: bool, runs: &mut Vec<Run>) {
        match event {
            Event::Text(text) if link => push(runs, Run::new(text.to_string(), style)),
            Event::Text(text) => autolink(&text, style, runs),
            Event::Code(code) => push(runs, Run::new(code.to_string(), style.fg(theme::CODE))),
            // html, and anything unexpected: show the text as-is.
            Event::Html(text) | Event::InlineHtml(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                push(runs, Run::new(text.to_string(), style));
            }
            Event::SoftBreak | Event::HardBreak => push(runs, Run::new("\n", style)),
            Event::FootnoteReference(name) => push(runs, Run::new(format!("[^{name}]"), style)),
            // Shown as the list marker instead.
            Event::TaskListMarker(checked) => self.task = Some(checked),
            Event::Start(_) | Event::End(_) | Event::Rule => {}
        }
    }
}

/// Appends a run, merging it into the last one when they look the same.
fn push(runs: &mut Vec<Run>, run: Run) {
    if run.text.is_empty() {
        return;
    }
    match runs.last_mut() {
        Some(last) if last.style == run.style => last.text.push_str(&run.text),
        _ => runs.push(run),
    }
}

/// Text until the enclosing tag's end, without styles (code, HTML blocks, table cells: marked's `plain`).
fn plain<'a>(events: &mut impl Iterator<Item = Event<'a>>) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for event in events.by_ref() {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) if depth == 0 => break,
            Event::End(_) => depth -= 1,
            Event::Text(t)
            | Event::Code(t)
            | Event::Html(t)
            | Event::InlineHtml(t)
            | Event::InlineMath(t)
            | Event::DisplayMath(t) => {
                out.push_str(&t);
            }
            _ => {}
        }
    }
    out
}

fn table<'a>(events: &mut impl Iterator<Item = Event<'a>>) -> Block {
    let mut rows: Vec<Vec<String>> = Vec::new();
    while let Some(event) = events.next() {
        match event {
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                let mut cells = Vec::new();
                while let Some(event) = events.next() {
                    match event {
                        Event::Start(Tag::TableCell) => cells.push(plain(events)),
                        Event::End(_) => break,
                        _ => {}
                    }
                }
                rows.push(cells);
            }
            Event::End(_) => break,
            _ => {}
        }
    }
    // Every row as wide as the header, as marked makes them.
    let columns = rows.first().map_or(0, Vec::len);
    for row in &mut rows {
        row.resize(columns, String::new());
    }
    Block::Table(rows)
}

/// Text with its bare `http(s)://` URLs as links (GFM's extended autolinks, which marked had).
fn autolink(text: &str, style: Style, runs: &mut Vec<Run>) {
    let mut rest = text;
    let mut offset = 0;
    while let Some(found) = ["https://", "http://"].iter().filter_map(|s| rest[offset..].find(s).map(|i| i + offset)).min() {
        // Only at a word's start (or after `(`, `*`, `_`, `~`), as GFM says.
        let before = rest[..found].chars().next_back();
        if before.is_some_and(|c| !c.is_whitespace() && !"(*_~".contains(c)) {
            offset = found + 1;
            continue;
        }
        let end = rest[found..].find(|c: char| c.is_whitespace() || c == '<').map_or(rest.len(), |i| found + i);
        let mut url = &rest[found..end];
        // Trailing punctuation isn't part of it, nor an unbalanced `)`.
        loop {
            let trimmed = url.trim_end_matches(['?', '!', '.', ',', ':', ';', '*', '_', '~', '\'', '"']);
            let trimmed = if trimmed.ends_with(')') && trimmed.matches(')').count() > trimmed.matches('(').count() {
                &trimmed[..trimmed.len() - 1]
            } else {
                trimmed
            };
            if trimmed.len() == url.len() {
                break;
            }
            url = trimmed;
        }
        if !url.contains('.') || url.ends_with("://") {
            offset = found + 1;
            continue;
        }
        push(runs, Run::new(&rest[..found], style));
        push(runs, Run::new(url, style.fg(theme::CODE).add_modifier(Modifier::UNDERLINED)));
        rest = &rest[found + url.len()..];
        offset = 0;
    }
    push(runs, Run::new(rest, style));
}

// ---------------------------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------------------------

/// A fence line: its run of ``` or ~~~ (3 or more), then the rest (a language, or nothing when it
/// closes). Marv's `FENCE`: `/^ {0,3}(`{3,}|~{3,})(.*)$/`.
fn fence(line: &str) -> Option<(char, usize, &str)> {
    let body = strip_indent(line)?;
    let c = body.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = body.len() - body.trim_start_matches(c).len();
    (run >= 3).then(|| (c, run, &body[run..]))
}

/// The line without up to 3 leading spaces, or None if it's indented more.
fn strip_indent(line: &str) -> Option<&str> {
    let body = line.trim_start_matches(' ');
    (line.len() - body.len() <= 3).then_some(body)
}

/// A list item's first line: "- ", "* ", "+ ", "1. " or "1) ". Marv's `LIST_ITEM`:
/// `/^ {0,3}([-*+]|\d{1,9}[.)])(\s|$)/`.
fn is_list_item(line: &str) -> bool {
    let Some(body) = strip_indent(line) else { return false };
    let rest = if let Some(rest) = body.strip_prefix(['-', '*', '+']) {
        rest
    } else {
        let digits = body.len() - body.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if !(1..=9).contains(&digits) {
            return false;
        }
        match body[digits..].strip_prefix(['.', ')']) {
            Some(rest) => rest,
            None => return false,
        }
    };
    rest.chars().next().is_none_or(char::is_whitespace)
}

/// Incremental parser for a reply that's still streaming (Marv's `createStreamLexer`).
///
/// Parses a reply as it streams in without re-parsing all of it each time: parsing grows faster
/// than the text (~20 ms at 11 KB, ~110 ms at 30 KB with marked), and streaming re-renders many
/// times a second. Text only ever grows at the end, so everything before the last finished block
/// is parsed once and cached; each update parses just the unfinished end.
#[derive(Debug, Clone, Default)]
pub struct StreamLexer {
    /// The parsed prefix: always ends at a block boundary, outside any fence.
    done: String,
    done_blocks: Vec<Block>,
    /// The finished blocks' lines at `width`, one entry per block in `done_blocks` laid out so far.
    width: u16,
    laid: Vec<Vec<Line<'static>>>,
}

impl StreamLexer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The blocks for `text`, which normally extends the last text seen.
    pub fn parse(&mut self, text: &str) -> Vec<Block> {
        self.advance(text);
        let mut blocks = self.done_blocks.clone();
        blocks.extend(lex(&text[self.done.len()..]));
        blocks
    }

    /// Lays out `text` like `layout(&self.parse(text), width)`, reusing the finished blocks' lines.
    pub fn render(&mut self, text: &str, width: u16) -> Vec<Line<'static>> {
        self.advance(text);
        if self.width != width {
            self.width = width;
            self.laid.clear();
        }
        while self.laid.len() < self.done_blocks.len() {
            let block = &self.done_blocks[self.laid.len()];
            self.laid.push(layout_block(block, width, 0));
        }
        let tail = lex(&text[self.done.len()..]);
        let mut lines = Vec::new();
        let mut first = true;
        let laid_tail = tail.iter().map(|b| Cow::Owned(layout_block(b, width, 0)));
        for block in self.laid.iter().map(|l| Cow::Borrowed(l.as_slice())).chain(laid_tail) {
            if !first {
                lines.push(Line::default());
            }
            first = false;
            lines.extend(block.iter().cloned());
        }
        lines
    }

    /// Moves the cut to the last block boundary in `text` and parses what it passed.
    fn advance(&mut self, text: &str) {
        if !text.starts_with(&self.done) {
            self.done.clear();
            self.done_blocks.clear();
            self.laid.clear();
        }
        // Find the last block boundary after the parsed prefix: a blank line outside any fence or HTML comment,
        // before a line that starts a new block (not an indented continuation, and not another item of a list: a
        // loose list split there would render as separate lists, and jump when the reply is parsed whole).
        let mut boundary = self.done.len();
        let mut open: Option<(char, usize)> = None;
        let mut in_comment = false;
        let mut line_start = self.done.len();
        while let Some(nl) = text[line_start..].find('\n').map(|i| i + line_start) {
            let line = &text[line_start..nl];
            let marker = fence(line);
            if let Some((c, length)) = open {
                // Closed only by the same character, at least as long, with nothing after it (```sh inside
                // ````markdown, or ```js after ```js, doesn't close it).
                if marker.is_some_and(|(mc, ml, rest)| mc == c && ml >= length && rest.trim().is_empty()) {
                    open = None;
                }
            } else if in_comment {
                if line.contains("-->") {
                    in_comment = false;
                }
            } else if let Some((c, length, _)) = marker.filter(|(c, _, rest)| !(*c == '`' && rest.contains('`'))) {
                // (A backtick fence's info string can't contain a backtick: "```x``` is…" is inline code, not a fence.)
                open = Some((c, length));
            } else if strip_indent(line).is_some_and(|l| l.starts_with("<!--")) && !line.contains("-->") {
                in_comment = true;
            }
            // Only a complete next line can decide (cuts are cached for good): "3" still arriving may become "3. third".
            let next = text[nl + 1..].find('\n').map(|end| &text[nl + 1..nl + 1 + end]);
            if line.trim().is_empty()
                && open.is_none()
                && !in_comment
                && next.is_some_and(|n| !n.is_empty() && !n.starts_with(char::is_whitespace) && !is_list_item(n))
            {
                boundary = nl + 1;
            }
            line_start = nl + 1;
        }
        if boundary > self.done.len() {
            self.done_blocks.extend(lex(&text[self.done.len()..boundary]));
            self.done = text[..boundary].to_string();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------------------------

/// Lays out blocks as lines `width` columns wide (hanging indents need the width).
pub fn layout(blocks: &[Block], width: u16) -> Vec<Line<'static>> {
    layout_blocks(blocks, width, 0, 1)
}

/// A run of blocks, separated by `gap` blank lines (one between paragraphs; none in tight lists).
fn layout_blocks(blocks: &[Block], width: u16, depth: usize, gap: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            lines.extend(std::iter::repeat_n(Line::default(), gap));
        }
        lines.extend(layout_block(block, width, depth));
    }
    lines
}

fn layout_block(block: &Block, width: u16, depth: usize) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme::DIM);
    match block {
        Block::Paragraph(runs) => wrap_runs(runs, Style::new(), width),
        Block::Heading(runs) => wrap_runs(runs, Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD), width),
        Block::Code { lang, text } => {
            let mut lines = lang.as_deref().map(|lang| wrap_text(lang, dim, width)).unwrap_or_default();
            let body = wrap_text(text, Style::new().fg(theme::CODE_BLOCK), width.saturating_sub(2));
            lines.extend(beside(body, Span::styled("│ ", dim), Span::styled("│ ", dim)));
            lines
        }
        Block::List { ordered, start, items } => {
            let number_width = format!("{}.", start + items.len().max(1) as u64 - 1).len();
            let marker_width = if *ordered { number_width + 1 } else { 2 };
            let marker_style = if *ordered { Style::new() } else { dim };
            let mut lines = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let marker = match item.task {
                    Some(true) => "☑".to_string(),
                    Some(false) => "☐".to_string(),
                    None if *ordered => format!("{:>number_width$}", format!("{}.", start + i as u64)),
                    None => BULLETS[depth % BULLETS.len()].to_string(),
                };
                let content =
                    layout_blocks(&item.blocks, width.saturating_sub(marker_width as u16), depth + 1, usize::from(item.loose));
                let first = Span::styled(format!("{marker:<marker_width$}"), marker_style);
                lines.extend(beside(content, first, Span::raw(" ".repeat(marker_width))));
            }
            lines
        }
        Block::Quote(blocks) => {
            let content = layout_blocks(blocks, width.saturating_sub(2), depth, 1);
            beside(content, Span::styled("▎ ", dim), Span::raw("  "))
        }
        Block::Table(rows) => {
            let Some(header) = rows.first() else { return Vec::new() };
            let widths: Vec<usize> =
                (0..header.len()).map(|col| rows.iter().map(|row| row[col].width()).max().unwrap_or(0)).collect();
            let line = |cells: &[String]| {
                let padded: Vec<String> =
                    cells.iter().zip(&widths).map(|(cell, w)| format!("{cell}{}", " ".repeat(w - cell.width()))).collect();
                padded.join("  ").trim_end().to_string()
            };
            let mut lines = wrap_text(&line(header), Style::new().add_modifier(Modifier::BOLD), width);
            let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            lines.extend(wrap_text(&rule.join("  "), dim, width));
            for row in &rows[1..] {
                lines.extend(wrap_text(&line(row), Style::new(), width));
            }
            lines
        }
        Block::Rule => vec![Line::styled("─".repeat(width as usize), dim)],
        Block::Html(text) => wrap_text(text, Style::new(), width),
    }
}

/// `lines` beside a fixed-width marker column: `first` before the first line, `rest` before the
/// others (a hanging indent). An empty block still shows its marker.
pub fn beside(lines: Vec<Line<'static>>, first: Span<'static>, rest: Span<'static>) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return vec![Line::from(first)];
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let lead = if i == 0 { first.clone() } else { rest.clone() };
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(lead);
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Plain text in one style, wrapped (newlines break lines).
pub fn wrap_text(text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    wrap_runs(&[Run::new(text, Style::new())], style, width)
}

/// One grapheme of a run, for wrapping.
#[derive(Clone, Copy)]
struct Cell {
    run: usize,
    start: usize,
    end: usize,
    width: usize,
    space: bool,
}

/// Runs word-wrapped to `width` columns like Ink's Text (see the module docs); each run's style
/// is patched over `base`.
pub fn wrap_runs(runs: &[Run], base: Style, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    // A cell holds one column: a raw tab would move the cursor past what the layout knows.
    let texts: Vec<Cow<str>> = runs
        .iter()
        .map(|r| if r.text.contains('\t') { Cow::Owned(r.text.replace('\t', "  ")) } else { Cow::Borrowed(&*r.text) })
        .collect();
    let styles: Vec<Style> = runs.iter().map(|r| base.patch(r.style)).collect();
    let mut lines = Vec::new();
    let mut cells: Vec<Cell> = Vec::new();
    for (run, text) in texts.iter().enumerate() {
        for (start, g) in text.grapheme_indices(true) {
            if g == "\n" || g == "\r\n" {
                wrap_line(&cells, &texts, &styles, width, &mut lines);
                cells.clear();
                continue;
            }
            cells.push(Cell { run, start, end: start + g.len(), width: g.width(), space: g == " " });
        }
    }
    wrap_line(&cells, &texts, &styles, width, &mut lines);
    lines
}

/// Starts a new row; the space just added before the word that moved goes.
fn new_row(rows: &mut Vec<Vec<Cell>>, row_width: &mut usize, spaced: bool) {
    if spaced {
        rows.last_mut().expect("never empty").pop();
    }
    rows.push(Vec::new());
    *row_width = 0;
}

/// One logical line (no newlines) as rows (wrap-ansi's `exec` with `hard: true`).
fn wrap_line(cells: &[Cell], texts: &[Cow<str>], styles: &[Style], width: usize, out: &mut Vec<Line<'static>>) {
    let mut rows: Vec<Vec<Cell>> = vec![Vec::new()];
    let mut row_width = 0;
    let mut word_start = 0;
    let mut first = true;
    while word_start <= cells.len() {
        let word_end = cells[word_start..].iter().position(|c| c.space).map_or(cells.len(), |i| word_start + i);
        let word = &cells[word_start..word_end];
        let length: usize = word.iter().map(|c| c.width).sum();
        let mut spaced = false;
        if !first {
            // The space before this word: it ends a full row instead of starting the next.
            if row_width >= width {
                rows.push(Vec::new());
                row_width = 0;
            } else {
                rows.last_mut().expect("never empty").push(cells[word_start - 1]);
                row_width += 1;
                spaced = true;
            }
        }
        first = false;
        if length > width {
            // A word longer than the line is cut; it starts on this row unless that costs an extra break.
            let remaining = width - row_width;
            let this_line = 1 + (length - remaining).saturating_sub(1) / width;
            let next_line = (length - 1) / width;
            if next_line < this_line && row_width > 0 {
                new_row(&mut rows, &mut row_width, spaced);
            }
            for cell in word {
                if row_width + cell.width > width && row_width > 0 {
                    rows.push(Vec::new());
                    row_width = 0;
                }
                rows.last_mut().expect("never empty").push(*cell);
                row_width += cell.width;
            }
        } else {
            if row_width + length > width && row_width > 0 && length > 0 {
                new_row(&mut rows, &mut row_width, spaced);
            }
            rows.last_mut().expect("never empty").extend_from_slice(word);
            row_width += length;
        }
        word_start = word_end + 1;
    }
    for row in rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut i = 0;
        while i < row.len() {
            // Consecutive cells of one run are one slice of its text.
            let mut j = i + 1;
            while j < row.len() && row[j].run == row[i].run && row[j].start == row[j - 1].end {
                j += 1;
            }
            let run = row[i].run;
            let text = &texts[run][row[i].start..row[j - 1].end];
            match spans.last_mut() {
                Some(last) if last.style == styles[run] => last.content.to_mut().push_str(text),
                _ => spans.push(Span::styled(text.to_string(), styles[run])),
            }
            i = j;
        }
        out.push(Line::from(spans));
    }
}

/// Raw Markdown from a model or tool → terminal lines. `streaming`: the reply's lexer while it
/// streams in; None for a finished message (parsed whole).
pub fn render(text: &str, width: u16, streaming: Option<&mut StreamLexer>) -> Vec<Line<'static>> {
    // Cleaned as a whole before lexing, so the lexer's cache sees one consistent text.
    let clean = printable(text);
    match streaming {
        Some(lexer) => lexer.render(&clean, width),
        None => layout(&lex(&clean), width),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders markdown at a given width and returns the plain lines, trailing spaces trimmed.
    fn show_at(text: &str, width: u16) -> Vec<String> {
        render(text, width, None).iter().map(|l| l.to_string().trim_end().to_string()).collect()
    }

    fn show(text: &str) -> Vec<String> {
        show_at(text, 60)
    }

    #[test]
    fn paragraphs_are_separated_by_one_blank_line() {
        assert_eq!(show("First paragraph.\n\nSecond paragraph."), ["First paragraph.", "", "Second paragraph."]);
    }

    #[test]
    fn inline_code_and_emphasis_lose_their_markers() {
        assert_eq!(show("Run `bun test` with **care** and *style*."), ["Run bun test with care and style."]);
        let lines = render("Run `bun test` with **care** and *style*.", 60, None);
        let span = |text: &str| lines[0].spans.iter().find(|s| s.content == text).unwrap().style;
        assert_eq!(span("bun test").fg, Some(theme::CODE));
        assert!(span("care").add_modifier.contains(Modifier::BOLD));
        assert!(span("style").add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn bullet_lists_wrap_with_a_hanging_indent_and_nest() {
        let lines = show_at(
            "- Root: `package.json`, `tsconfig.json`, `bun.lock`, plus a patch at `patches/ink@8.0.0.patch`\n\
             - `src/` — the app itself:\n  \
             - Core: `agent.ts`, `app.tsx`, `cli.tsx`, `prompt.ts`, `paths.ts`, `mouse.ts`",
            44,
        );
        assert_eq!(
            lines,
            [
                "• Root: package.json, tsconfig.json,",
                "  bun.lock, plus a patch at",
                "  patches/ink@8.0.0.patch",
                "• src/ — the app itself:",
                "  ◦ Core: agent.ts, app.tsx, cli.tsx,",
                "    prompt.ts, paths.ts, mouse.ts",
            ]
        );
    }

    #[test]
    fn numbered_lists_keep_their_numbers_aligned() {
        let items: Vec<String> = (1..=10).map(|i| format!("{i}. item {i}")).collect();
        let lines = show(&items.join("\n"));
        assert_eq!(lines[0], " 1. item 1");
        assert_eq!(lines[9], "10. item 10");
        assert_eq!(show("3. three\n4. four"), ["3. three", "4. four"]);
    }

    #[test]
    fn loose_items_space_their_blocks_but_not_the_items() {
        assert_eq!(show("1. first\n\n2. second"), ["1. first", "2. second"]);
        assert_eq!(show("- one\n\n  more\n- two"), ["• one", "", "  more", "• two"]);
    }

    #[test]
    fn task_lists_show_checkboxes() {
        assert_eq!(show("- [x] done\n- [ ] todo"), ["☑ done", "☐ todo"]);
    }

    #[test]
    fn headings_stand_on_their_own_line() {
        assert_eq!(show("## Layout\nSome text."), ["Layout", "", "Some text."]);
        let lines = render("## Layout", 60, None);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::ACCENT));
    }

    #[test]
    fn code_blocks_keep_their_lines_behind_a_rule_with_the_language() {
        let lines = show("Before:\n\n```ts\nconst x = 1;\n  indented();\n```\n\nAfter.");
        assert_eq!(lines, ["Before:", "", "ts", "│ const x = 1;", "│   indented();", "", "After."]);
    }

    #[test]
    fn an_unfinished_code_block_mid_stream_still_renders() {
        assert_eq!(show("```ts\nconst x ="), ["ts", "│ const x ="]);
    }

    #[test]
    fn tables_line_up_in_columns() {
        assert_eq!(
            show("| Tool | Does |\n|---|---|\n| glob | finds files |\n| read_file | reads one |"),
            ["Tool       Does", "─────────  ───────────", "glob       finds files", "read_file  reads one"]
        );
    }

    #[test]
    fn links_show_their_target_when_it_differs_from_the_text() {
        assert_eq!(
            show("See [the docs](https://example.com) or https://bun.sh."),
            ["See the docs (https://example.com) or https://bun.sh."]
        );
        let lines = render("See https://bun.sh. or <https://x.dev>", 60, None);
        let span = |text: &str| lines[0].spans.iter().find(|s| s.content == text).unwrap().style;
        assert!(span("https://bun.sh").add_modifier.contains(Modifier::UNDERLINED));
        assert!(span("https://x.dev").add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(
            show("not a link: xhttps://a.b and (https://c.d/x(y))."),
            ["not a link: xhttps://a.b and (https://c.d/x(y))."]
        );
    }

    #[test]
    fn blockquotes_get_a_bar() {
        assert_eq!(show("> careful here"), ["▎ careful here"]);
    }

    #[test]
    fn escaped_characters_come_out_plain() {
        assert_eq!(show("2 \\* 3 and a < b & c"), ["2 * 3 and a < b & c"]);
    }

    #[test]
    fn images_and_rules() {
        assert_eq!(show_at("![a cat](cat.png)\n\n---", 10), ["[image: a", "cat]", "", "──────────"]);
    }

    #[test]
    fn wrapping_cuts_long_words_and_never_overflows() {
        assert_eq!(show_at("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(show_at("ab cdefghij", 4), ["ab", "cdef", "ghij"]);
        // A space that would overflow a full row ends it.
        assert_eq!(show_at("abcd efgh", 4), ["abcd", "efgh"]);
        for width in [1, 3, 7, 20] {
            for line in render("- a list item with `code` and a\tlong_word_that_goes_on_and_on", width, None) {
                assert!(line.width() <= (width as usize).max(3), "{line:?} at {width}");
            }
        }
    }

    #[test]
    fn escape_sequences_never_reach_the_lines() {
        let lines = render("before \x1b]52;c;aGk=\x07after `x\x1b[2J`", 60, None);
        let text: String = lines.iter().map(|l| l.to_string()).collect();
        assert!(!text.contains('\x1b') && !text.contains('\x07'));
        assert_eq!(text, "before after x");
    }

    // --- the stream lexer ---

    /// Feeds `text` in chunks, like a stream, and returns the final blocks.
    fn stream(text: &str, chunk: usize) -> Vec<Block> {
        let mut lexer = StreamLexer::new();
        let mut blocks = lexer.parse("");
        let mut i = chunk;
        while i < text.len() + chunk {
            blocks = lexer.parse(&text[..i.min(text.len())]);
            i += chunk;
        }
        blocks
    }

    fn kinds(blocks: &[Block]) -> Vec<&'static str> {
        blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph(_) => "paragraph",
                Block::Heading(_) => "heading",
                Block::Code { .. } => "code",
                Block::List { .. } => "list",
                Block::Quote(_) => "blockquote",
                Block::Table(_) => "table",
                Block::Rule => "hr",
                Block::Html(_) => "html",
            })
            .collect()
    }

    #[test]
    fn gives_the_same_blocks_as_parsing_the_whole_text() {
        let text = "Intro paragraph.\n\n## Heading\n\nSome `code` here.\n\n- one\n- two\n\nOutro.";
        assert_eq!(kinds(&stream(text, 7)), kinds(&lex(text)));
        assert_eq!(stream(text, 7), lex(text));
    }

    #[test]
    fn never_splits_a_code_block_at_a_blank_line_inside_it() {
        let text = "Before.\n\n```ts\nconst a = 1;\n\nconst b = 2;\n```\n\nAfter.";
        let code: Vec<Block> = stream(text, 7).into_iter().filter(|b| matches!(b, Block::Code { .. })).collect();
        assert_eq!(code, [Block::Code { lang: Some("ts".into()), text: "const a = 1;\n\nconst b = 2;".into() }]);
    }

    #[test]
    fn keeps_indented_continuations_with_their_list_item() {
        let text = "- item one\n\n  more about item one\n\n- item two";
        let blocks = stream(text, 7);
        assert_eq!(kinds(&blocks), ["list"]);
        assert!(format!("{blocks:?}").contains("more about item one"));
    }

    #[test]
    fn while_it_streams_every_partial_reply_parses_like_the_same_text_parsed_whole() {
        // Random replies built from tricky blocks, streamed in random chunks. (Left out: a reference link whose
        // definition comes later, which no incremental parse can know; finished replies are parsed whole anyway.)
        const BLOCKS: [&str; 13] = [
            "Some paragraph text with `code` and **bold**.",
            "- item one\n- item two\n  - nested\n- item three",
            "1. first\n\n2. second\n\n3. third", // a loose list: one list, not three
            "- loose item\n\n  continued para\n\n- next",
            "```ts\nconst a = 1;\n\nconst b = 2;\n```",
            "~~~\nraw ``` inside\n\nmore\n~~~",
            "```js\nconsole.log(1)\n```js\n\n# not a heading\n\nstill code\n```", // ```js doesn't close it
            "````markdown\n## Install\n\n```sh\nbun install\n\nbun run dev\n```\n\n- list in md\n````", // a shorter fence inside
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "## Heading",
            "> quote line\n>\n> more quote",
            "<!--\n\nhidden comment\n\n-->",
            "```const x``` is inline code at line start",
        ];
        let mut seed: u64 = 777;
        let mut random = move || {
            seed = (seed.wrapping_mul(1_103_515_245).wrapping_add(12_345)) & 0x7fff_ffff;
            seed as f64 / 0x7fff_ffff as f64
        };
        for _ in 0..120 {
            let count = 2 + (random() * 4.0) as usize;
            let parts: Vec<&str> = (0..count).map(|_| BLOCKS[(random() * BLOCKS.len() as f64) as usize % BLOCKS.len()]).collect();
            let text = parts.join("\n\n");
            let mut lexer = StreamLexer::new();
            let mut renderer = StreamLexer::new();
            let mut at = 0;
            while at < text.len() {
                at = text.len().min(at + 1 + (random() * 12.0) as usize);
                let prefix = &text[..at];
                assert_eq!(lexer.parse(prefix), lex(prefix), "prefix: {prefix:?}");
                assert_eq!(renderer.render(prefix, 40), layout(&lex(prefix), 40), "prefix: {prefix:?}");
            }
        }
    }

    #[test]
    fn starts_over_when_the_text_is_replaced_a_new_reply() {
        let mut lexer = StreamLexer::new();
        lexer.parse("First reply.\n\nSecond paragraph.");
        assert_eq!(kinds(&lexer.parse("Something else entirely.")), ["paragraph"]);
        lexer.render("First reply.\n\nSecond paragraph.", 40);
        assert_eq!(lexer.render("Other.\n\nText.", 40), layout(&lex("Other.\n\nText."), 40));
    }

    #[test]
    fn a_new_width_lays_the_finished_blocks_out_again() {
        let text = "A paragraph that is long enough to wrap.\n\nAnd the end.";
        let mut lexer = StreamLexer::new();
        assert_eq!(lexer.render(text, 60), layout(&lex(text), 60));
        assert_eq!(lexer.render(text, 12), layout(&lex(text), 12));
    }

    #[test]
    fn re_parses_only_the_unfinished_end_so_long_replies_stay_fast() {
        let sample = "Some text with `code` and **bold**.\n\n- a list item\n- another one\n\n";
        let text = sample.repeat(300); // ~20 KB
        let mut lexer = StreamLexer::new();
        let mut times = Vec::new();
        let mut i = 50;
        while i <= text.len() {
            let t0 = std::time::Instant::now();
            lexer.render(&text[..i], 80);
            times.push(t0.elapsed());
            i += 50;
        }
        // Re-parsing (and re-wrapping) the whole reply on each update grows with it: a typical update near the end
        // would cost about 9 times one near the start. Medians, not the slowest update, and no fixed time: one
        // scheduler hiccup on a busy CI runner (a 70 ms update on macOS Intel) shouldn't fail it.
        let median = |part: &[std::time::Duration]| {
            let mut part = part.to_vec();
            part.sort();
            part[part.len() / 2]
        };
        let fifth = times.len() / 5;
        let (start, end) = (median(&times[..fifth]), median(&times[times.len() - fifth..]));
        assert!(
            end < start * 4 + std::time::Duration::from_millis(1),
            "a typical update took {start:?} at the start, {end:?} at the end"
        );
    }
}

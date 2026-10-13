//! HTML to Markdown for `web_fetch`: the page's main content (Readability, the code behind Firefox's Reader View),
//! written as Markdown with GitHub's tables.
//!
//! Ports Marv's `src/tools/web/html.ts` (linkedom + @mozilla/readability + turndown with turndown-plugin-gfm).
//!
//! Everything here is CPU work on a stranger's page, so two rules:
//! - It's bounded: the page is cut to [`crate::web::plain::MAX_CHARS`] and [`MAX_ELEMENTS`] before Readability and
//!   the Markdown writer see it, and Readability is skipped for pages nested too deep (unbounded, a 5 MB page of links
//!   took over 4 minutes, 5 MB of `<br>` 126 s, 2,000 nested divs 23 s). It still runs only in the converter process.
//! - What the model gets is what a reader sees: hidden elements (inline `display:none`/`visibility:hidden`/
//!   `opacity:0`, `[hidden]`, `aria-hidden`), form controls, svg and scripts are removed (hidden text is the classic
//!   prompt-injection channel); links and images are kept only for http/https/mailto after resolving (`<base href>`
//!   honoured); the title is one line of at most 200 chars.
//! - Port plan: `html5ever`/`scraper` for the DOM, a Readability port (`readability` crate or `dom_smoothie`), and a
//!   Markdown writer with GFM tables (`htmd`); keep the same limits and the same hidden-element rules, with Marv's
//!   tests as fixtures.

/// With less text than this, Readability didn't find the page's content.
pub const MIN_ARTICLE_CHARS: usize = 200;
/// The Markdown writer's cost grows with an element's children times the output so far; 20k elements take under a second.
pub const MAX_ELEMENTS: usize = 20_000;
/// Deeper than this, elements keep only their text.
pub const MAX_DEPTH: usize = 200;
/// Readability's cost grows with depth, measured best by the sum of every element's depth.
pub const MAX_READABILITY_DEPTH_SUM: usize = 500_000;
/// …and with the square of list items without text.
pub const MAX_READABILITY_EMPTY_ITEMS: usize = 2_000;
pub const MAX_ALT: usize = 200;
pub const MAX_WHITESPACE_RUN: usize = 200;

/// Why the page wasn't converted from its main content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackReason {
    NoArticle,
    TooLarge,
    TooDeep,
    Timeout,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Markdown {
    /// One line, at most 200 characters (it goes into the `Fetched <url> · <title>` header).
    pub title: Option<String>,
    pub markdown: String,
    /// Readability found nothing (or the page was too deep or big for it), so this is the whole page.
    pub fallback: bool,
    /// Why it fell back; set exactly when `fallback` is.
    pub reason: Option<FallbackReason>,
    /// The page had more than MAX_CHARS of content (or MAX_ELEMENTS elements), and the rest was cut.
    pub truncated: bool,
    /// Converting took too long (or failed), so this is the page's text with the tags stripped.
    pub plain: bool,
}

/// The page as Markdown. Call it only inside the converter process (`html_convert_cli`).
pub fn html_to_markdown(_html: &str, _url: &str) -> Markdown {
    todo!("port: htmlToMarkdown (bounded prune, hidden removal, Readability, GFM Markdown)")
}

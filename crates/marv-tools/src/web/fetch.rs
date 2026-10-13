//! Downloading for `web_fetch`: plain GETs only (the model chooses the URL, nothing else), each redirect checked
//! again (a public page could redirect to a private address), with a time and a size limit, text only.
//!
//! Ports Marv's `src/tools/web/fetch.ts`.
//!
//! - [`TIMEOUT_MS`] = 20 s, [`MAX_BYTES`] = 5 MB, at most [`MAX_REDIRECTS`] = 5, followed by hand; error pages read
//!   up to 64 KB for their message. gzip/deflate/brotli decoded; binary sniffed (a NUL in the first 8 KB).
//! - `Accept` prefers Markdown (some sites send it when asked, which beats converting their HTML).
//! - Headers (a GitHub token) go only to the first URL's origin and only over https ([`sends_headers`]): `URL.host`
//!   ignores the scheme, so a redirect from https to http on the same host would otherwise send a token in clear.
//! - A refused redirect returns the 3xx with `location`, and [`redirect_note`] tells the model where it leads.
//! - 403/429/503 say the site may block automated readers; only a name that doesn't exist says "check the address".
//! - Port plan: `reqwest` with redirects disabled, rustls, and a resolver pinned to the address `check_address`
//!   approved (closes Marv's DNS-rebinding gap).

use url::Url;

use crate::types::ToolError;

pub const TIMEOUT_MS: u64 = 20_000;
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_REDIRECTS: usize = 5;
/// Some sites send Markdown when asked for it.
pub const ACCEPT: &str = "text/markdown, text/html;q=0.9, text/plain;q=0.8, application/json;q=0.8, */*;q=0.5";
/// An error page: enough for its message, never worth failing over.
pub const ERROR_BODY_BYTES: usize = 64 * 1024;

/// A fetched page as `web_fetch` pages it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Where it ended up, after redirects.
    pub url: String,
    pub title: Option<String>,
    pub text: String,
    /// Said under the header, e.g. that Readability found nothing.
    pub note: Option<String>,
}

/// One GET's answer (error statuses are returned, not thrown: the caller knows what they mean).
#[derive(Debug, Clone)]
pub struct Got {
    pub url: String,
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// The model's URL as a URL: without a scheme (`github.com/o/r`), https is meant. `example.com:8080/x` and
/// `localhost:3000` are hosts with a port, not schemes: only `://` or a known scheme without slashes counts.
pub fn parse_url(text: &str) -> Result<Url, ToolError> {
    let text = text.trim();
    let has_scheme = {
        let mut chars = text.char_indices();
        let first_alpha = chars.next().is_some_and(|(_, c)| c.is_ascii_alphabetic());
        let scheme_end = text.find("://");
        let slashes = first_alpha
            && scheme_end.is_some_and(|end| text[..end].chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)));
        let lower = text.to_ascii_lowercase();
        slashes || ["mailto:", "file:", "data:", "javascript:"].iter().any(|s| lower.starts_with(s))
    };
    let with_scheme = if has_scheme { text.to_string() } else { format!("https://{text}") };
    Url::parse(&with_scheme).map_err(|_| ToolError(format!("Not a valid URL: {text}. Give the full address, e.g. https://example.com/page.")))
}

/// Credentials go only where the user's URL pointed, and only over https.
pub fn sends_headers(first: &Url, current: &Url, allow_http: bool) -> bool {
    first.origin() == current.origin() && (current.scheme() == "https" || allow_http)
}

/// Said when a redirect wasn't followed: where it leads, and what to do about it.
pub fn redirect_note(got: &Got) -> Option<String> {
    if !(300..400).contains(&got.status) {
        return None;
    }
    let location = got.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location"))?.1.as_str();
    let target = Url::parse(&got.url).ok()?.join(location).ok()?;
    Some(format!("{} redirects to {target}, on another site: call web_fetch with that URL to follow it (the user may be asked).", got.url))
}

/// GETs a URL, following redirects by hand so each one is checked.
pub async fn get(_url: &str) -> Result<Got, ToolError> {
    todo!("port: get (GetOptions: cancel, allow_private, allow_http, headers, timeout, max_bytes, follow_redirect)")
}

/// A page as text: HTML converted (in a child process), plain text and JSON as they are, binary refused.
pub async fn fetch_page(_url: &str) -> Result<Page, ToolError> {
    todo!("port: fetchPage with FALLBACK_NOTES and the 403/429/503 hints")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_without_a_scheme_is_https_and_a_port_is_not_a_scheme() {
        assert_eq!(parse_url("github.com/o/r").unwrap().as_str(), "https://github.com/o/r");
        assert_eq!(parse_url("localhost:3000").unwrap().as_str(), "https://localhost:3000/");
        assert_eq!(parse_url("http://example.com").unwrap().scheme(), "http");
        assert_eq!(parse_url("file:///etc/passwd").unwrap().scheme(), "file"); // refused later by check_address
        assert!(parse_url("http://").is_err());
    }

    #[test]
    fn headers_only_to_the_first_origin_over_https() {
        let first = Url::parse("https://api.github.com/x").unwrap();
        assert!(sends_headers(&first, &Url::parse("https://api.github.com/y").unwrap(), false));
        assert!(!sends_headers(&first, &Url::parse("http://api.github.com/y").unwrap(), false));
        assert!(!sends_headers(&first, &Url::parse("https://evil.com/").unwrap(), false));
    }
}

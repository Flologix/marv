//! Web reading for `web_fetch`: where it may connect, how it downloads, GitHub through its API, and HTML to Markdown.
//!
//! Ports Marv's `src/tools/web/` (one module per file).
//!
//! - [`address`]: public internet only (no loopback, private, link-local, CGNAT, multicast; IPv4-mapped and NAT64 too).
//! - [`fetch`]: plain GETs, redirects followed by hand (at most 5) and each checked again, 20 s, 5 MB, text only.
//! - [`github`]: repo / tree / blob links through the API; the token goes to the API host only, over https.
//! - [`convert`] + [`html_convert_cli`]: the conversion runs in a killable child process (15 s), never in-process.
//! - [`html`]: main content (Readability), Markdown with GFM tables, hidden elements removed (the classic
//!   prompt-injection channel), bounded work.
//! - [`plain`]: the linear, quote-aware plain-text fallback.
//!
//! - [`hidden`] (marv-rs): hiding classes, invisible characters, Markdown comments.
//!
//! marv-rs closes two of Marv's known limits: the connection is pinned to the checked address (DNS rebinding), and
//! the common frameworks' hiding classes are detected (stylesheets still aren't evaluated). Still as in Marv: the
//! budget is spent in document order.

/// What a web call says when the user stopped the run (Esc) during it.
pub const STOPPED: &str = "Stopped by the user.";

pub mod address;
pub mod convert;
pub mod fetch;
pub mod github;
pub mod hidden;
pub mod html;
pub mod html_convert_cli;
pub mod plain;
#[cfg(test)]
pub(crate) mod test_server;

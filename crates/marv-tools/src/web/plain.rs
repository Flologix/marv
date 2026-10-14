//! The plain-text fallback for a page too complex to convert in time, and the limits both conversions share.
//!
//! Ports Marv's `src/tools/web/plain.ts`.
//!
//! This runs in Marv's own process (after the converter was killed), on HTML that just proved hostile or
//! pathological, and an attacker can make a page slow on purpose to land here, so:
//! - It's one linear pass. Tags are read by hand, character by character, and searches only move forward; never a
//!   pattern that could backtrack over the rest of the page.
//! - It hides what the Markdown path hides: hidden elements, form controls, scripts. Quoted attribute values are
//!   skipped as a browser does, so `<div title="x>" hidden>` is still one hidden tag.
//! - marv-rs: "hidden" also means a hiding class ([`crate::web::hidden::is_hidden_class`]) and an off-screen or
//!   zero-size inline style ([`is_hidden_style`]); invisible characters are dropped as the text comes in.

use crate::web::hidden::{is_hidden_class, is_invisible};
use crate::web::html::Markdown;

/// About ten of web_fetch's 30k-character pages: more is rarely read.
pub const MAX_CHARS: usize = 300_000;
pub const MAX_TITLE: usize = 200;

/// One line (a newline in the title could forge a line of web_fetch's `Fetched <url> · <title>` header), at most
/// 200 characters, cut by code point.
pub fn one_line(text: Option<&str>) -> Option<String> {
    let text: String = text.unwrap_or("").chars().take(MAX_TITLE * 10).collect();
    let mut line = String::new();
    let mut in_space = false;
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() {
            if !in_space {
                line.push(' ');
            }
            in_space = true;
        } else {
            line.push(c);
            in_space = false;
        }
    }
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line.chars().count() > MAX_TITLE {
        let cut: String = line.chars().take(MAX_TITLE - 1).collect();
        return Some(format!("{}…", cut.trim_end()));
    }
    Some(line.to_string())
}

/// Whether an inline style (its entities already decoded) hides the element. CSS comments are removed first, since a
/// comment between `display:` and `none` hides as well; by hand, since a lazy regex for comments is quadratic on a
/// style full of unclosed ones. Stylesheets aren't evaluated (see [`crate::web::hidden::is_hidden_class`]).
///
/// Marv's rules: `display:none`, `visibility:hidden|collapse`, `opacity:0` (`.0` too), `font-size:0`, any one of them
/// hiding even if another declaration says otherwise. marv-rs adds the off-screen and zero-size tricks: a large
/// negative `left`/`top`/`right`/`bottom`/`margin-*`/`text-indent` or `translate`, `clip: rect(0 0 0 0)`,
/// `clip-path: inset(50%)`, a 0 or 1 px box with `overflow: hidden`, `transform: scale(0)`, `color: transparent`.
pub fn is_hidden_style(style: &str) -> bool {
    let mut css = String::new();
    let mut at = 0;
    while at < style.len() {
        let Some(open) = style[at..].find("/*").map(|i| at + i) else {
            css.push_str(&style[at..]);
            break;
        };
        css.push_str(&style[at..open]);
        match style[open + 2..].find("*/") {
            Some(close) => at = open + 2 + close + 2,
            None => break, // an unclosed comment runs to the end
        }
    }
    let css = css.to_ascii_lowercase();
    let decls: Vec<(&str, &str)> = css
        .split(';')
        .filter_map(|d| {
            let (prop, value) = d.split_once(':')?;
            let value = value.trim();
            let value = value.strip_suffix("!important").unwrap_or(value).trim();
            Some((prop.trim(), value))
        })
        .collect();
    let any = |f: &dyn Fn(&str, &str) -> bool| decls.iter().any(|(p, v)| f(p, v));
    let zero = |v: &str| length(v).is_some_and(|(n, _)| n == 0.0);
    let tiny = |v: &str| length(v).is_some_and(|(n, unit)| n <= 1.0 && matches!(unit, "" | "px"));
    let clipped = any(&|p, v| p.starts_with("overflow") && (v == "hidden" || v == "clip"));
    any(&|p, v| p == "display" && v == "none")
        || any(&|p, v| p == "visibility" && (v == "hidden" || v == "collapse"))
        || any(&|p, v| p == "opacity" && zero(v))
        || any(&|p, v| p == "font-size" && zero(v))
        || any(&|p, v| {
            matches!(p, "left" | "right" | "top" | "bottom" | "margin-left" | "margin-top" | "text-indent") && off_screen(v)
        })
        || any(&|p, v| p == "transform" && transform_hides(v))
        || any(&|p, v| p == "clip" && clip_hides(v))
        || any(&|p, v| p == "clip-path" && clip_path_hides(v))
        || any(&|p, v| p == "color" && v == "transparent")
        || (clipped && any(&|p, v| matches!(p, "width" | "height" | "max-width" | "max-height") && tiny(v)))
}

/// A CSS length: its number and unit (`-9999px` → -9999, "px"). None for anything else.
fn length(value: &str) -> Option<(f64, &str)> {
    let end = value.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+')).unwrap_or(value.len());
    let n: f64 = value[..end].parse().ok()?;
    Some((n, value[end..].trim()))
}

/// Far off to the top or left: where "visually hidden" text is parked.
fn off_screen(value: &str) -> bool {
    length(value).is_some_and(|(n, unit)| match unit {
        "" | "px" | "pt" => n <= -500.0,
        "em" | "rem" | "ch" | "ex" => n <= -30.0,
        "%" | "vw" | "vh" => n <= -50.0,
        _ => false,
    })
}

/// The numbers inside the first `(…)` after `name`.
fn args<'a>(value: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let start = value.find(name)? + name.len();
    let inner = value[start..].trim_start().strip_prefix('(')?;
    let inner = &inner[..inner.find(')')?];
    Some(inner.split([',', ' ']).filter(|a| !a.is_empty()).collect())
}

fn transform_hides(value: &str) -> bool {
    let scaled_away = ["scale(", "scalex(", "scaley("]
        .iter()
        .any(|f| args(value, &f[..f.len() - 1]).is_some_and(|a| a.iter().any(|x| length(x).is_some_and(|(n, _)| n == 0.0))));
    let moved_away =
        ["translate", "translatex", "translatey"].iter().any(|f| args(value, f).is_some_and(|a| a.iter().any(|x| off_screen(x))));
    scaled_away || moved_away
}

fn clip_hides(value: &str) -> bool {
    args(value, "rect").is_some_and(|a| !a.is_empty() && a.iter().all(|x| length(x).is_some_and(|(n, _)| n.abs() <= 1.0)))
}

fn clip_path_hides(value: &str) -> bool {
    args(value, "inset").is_some_and(|a| a.first().and_then(|x| length(x)).is_some_and(|(n, u)| u == "%" && n >= 50.0))
        || args(value, "circle").is_some_and(|a| a.first().and_then(|x| length(x)).is_some_and(|(n, _)| n == 0.0))
}

/// Raw text in HTML: no tags inside, only the end tag ends it (an unclosed `<script>` swallows the rest of the page in
/// a browser too).
const RAW_TEXT: &[&str] = &["script", "style", "title", "textarea", "noscript", "iframe", "noembed", "noframes", "xmp"];
/// Never text a reader sees: skipped with everything in them, like a hidden element.
const SKIPPED: &[&str] = &["template", "svg", "math", "select", "button", "object", "datalist"];
/// No end tag, so a hidden `<img>` hides nothing after it.
const VOID: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"];
/// A blank line before and after these, so paragraphs and list items don't run together.
const BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tr",
    "ul",
];

const NAMED: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", "\u{a0}"),
    ("shy", ""),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("sbquo", "‚"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("bdquo", "„"),
    ("laquo", "«"),
    ("raquo", "»"),
    ("ndash", "–"),
    ("mdash", "—"),
    ("hellip", "…"),
    ("bull", "•"),
    ("middot", "·"),
    ("prime", "′"),
    ("ensp", "\u{2002}"),
    ("emsp", "\u{2003}"),
    ("thinsp", "\u{2009}"),
    ("copy", "©"),
    ("reg", "®"),
    ("trade", "™"),
    ("deg", "°"),
    ("plusmn", "±"),
    ("times", "×"),
    ("divide", "÷"),
    ("micro", "µ"),
    ("para", "¶"),
    ("sect", "§"),
    ("euro", "€"),
    ("pound", "£"),
    ("yen", "¥"),
    ("cent", "¢"),
    ("larr", "←"),
    ("rarr", "→"),
    ("uarr", "↑"),
    ("darr", "↓"),
    ("aacute", "á"),
    ("agrave", "à"),
    ("acirc", "â"),
    ("auml", "ä"),
    ("aring", "å"),
    ("ccedil", "ç"),
    ("eacute", "é"),
    ("egrave", "è"),
    ("ecirc", "ê"),
    ("euml", "ë"),
    ("iacute", "í"),
    ("iuml", "ï"),
    ("ntilde", "ñ"),
    ("oacute", "ó"),
    ("ocirc", "ô"),
    ("ouml", "ö"),
    ("oslash", "ø"),
    ("uacute", "ú"),
    ("uuml", "ü"),
    ("szlig", "ß"),
    ("Aacute", "Á"),
    ("Auml", "Ä"),
    ("Eacute", "É"),
    ("Ouml", "Ö"),
    ("Uuml", "Ü"),
];

struct Tag {
    name: String,
    closing: bool,
    hidden: bool,
    /// Just after the tag's `>`, or None if the page ends inside the tag (an unclosed quote).
    end: Option<usize>,
}

/// The page's text: tags, comments, scripts, styles and hidden elements removed, entities decoded, whitespace
/// collapsed, at most `max` characters. Linear in the size of the page.
pub fn plain_text(html: &str, max: usize) -> Markdown {
    let mut out = Text::new(max);
    let mut title: Option<Option<String>> = None;
    // Inside an element whose content is skipped (hidden, or one of SKIPPED): its name, and how many are open.
    let mut skipping: Option<(String, usize)> = None;
    let bytes = html.as_bytes();

    let mut at = 0;
    while at < html.len() && !out.truncated {
        let lt = html[at..].find('<').map(|i| at + i);
        let text_end = lt.unwrap_or(html.len());
        if skipping.is_none() && text_end > at {
            out.text(&decode(&html[at..text_end]));
        }
        let Some(lt) = lt else { break };
        at = lt;
        if html[at..].starts_with("<!--") {
            at = html[at + 4..].find("-->").map_or(html.len(), |i| at + 4 + i + 3);
            continue;
        }
        if matches!(bytes.get(at + 1), Some(b'!' | b'?')) {
            // a doctype, <![CDATA[, <?xml: they end at the first ">"
            at = html[at..].find('>').map_or(html.len(), |i| at + i + 1);
            continue;
        }
        let Some(tag) = read_tag(html, at) else {
            if skipping.is_none() {
                out.text("<"); // a lone "<" (as in "a < b") is text
            }
            at += 1;
            continue;
        };
        let Some(end) = tag.end else { break }; // the page ends inside a tag: a browser shows none of it
        at = end;
        let name = tag.name.as_str();
        let closing = tag.closing;

        if let Some((skip_name, depth)) = &mut skipping {
            // A <p> can't contain a block, so one starting ends it (HTML lets a page leave </p> out).
            if !closing && skip_name == "p" && BLOCK.contains(&name) {
                skipping = None;
            } else if name == skip_name && !VOID.contains(&name) {
                if closing {
                    *depth -= 1;
                } else {
                    *depth += 1;
                }
                if *depth == 0 {
                    skipping = None;
                }
            }
        }
        if skipping.is_none() {
            if BLOCK.contains(&name) {
                out.paragraph();
            } else if name == "br" {
                out.line();
            } else if name == "td" || name == "th" {
                out.text(" ");
            }
        }
        if closing || VOID.contains(&name) {
            continue;
        }
        if RAW_TEXT.contains(&name) {
            let raw_end = end_of_raw_text(html, name, at);
            if name == "title" && title.is_none() && skipping.is_none() {
                title = Some(one_line(Some(&decode(&html[at..raw_end.map_or(html.len(), |(start, _)| start)]))));
            }
            let Some((_, after)) = raw_end else { break };
            at = after;
        } else if skipping.is_none() && (SKIPPED.contains(&name) || tag.hidden) {
            // Without an end tag, everything after it is skipped: that's safe, and still one pass.
            skipping = Some((name.to_string(), 1));
        }
    }
    let truncated = out.truncated;
    Markdown { title: title.flatten(), markdown: out.into_string(), fallback: true, reason: None, truncated, plain: true }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0c | b'\r')
}

/// The tag starting at `html[at] == '<'`, read as a browser does: its name, then attributes whose quoted values may
/// contain `>`. None if this `<` doesn't start a tag. Each character is looked at once.
fn read_tag(html: &str, at: usize) -> Option<Tag> {
    let b = html.as_bytes();
    let n = b.len();
    let mut i = at + 1;
    let closing = b.get(i) == Some(&b'/');
    if closing {
        i += 1;
    }
    if !b.get(i).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let name_start = i;
    while i < n && !is_space(b[i]) && b[i] != b'/' && b[i] != b'>' {
        i += 1;
    }
    let name = html[name_start..i].to_ascii_lowercase();
    let mut hidden = false;
    let tag = |hidden, end| Some(Tag { name: name.clone(), closing, hidden, end });
    loop {
        while i < n && (is_space(b[i]) || b[i] == b'/') {
            i += 1;
        }
        if i >= n {
            return tag(hidden, None);
        }
        if b[i] == b'>' {
            return tag(hidden, Some(i + 1));
        }
        let attr_start = i; // an attribute's name can start with anything, even "=" or a quote
        i += 1;
        while i < n && !is_space(b[i]) && b[i] != b'/' && b[i] != b'>' && b[i] != b'=' {
            i += 1;
        }
        let attr = html[attr_start..i].to_ascii_lowercase();
        while i < n && is_space(b[i]) {
            i += 1;
        }
        let mut value = "";
        if b.get(i) == Some(&b'=') {
            i += 1;
            while i < n && is_space(b[i]) {
                i += 1;
            }
            match b.get(i) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let Some(close) = html[i + 1..].find(quote as char).map(|c| i + 1 + c) else {
                        return tag(hidden, None);
                    };
                    value = &html[i + 1..close];
                    i = close + 1;
                }
                _ => {
                    let start = i;
                    while i < n && !is_space(b[i]) && b[i] != b'>' {
                        i += 1;
                    }
                    value = &html[start..i];
                }
            }
        }
        if !hidden && !closing {
            hidden = attr == "hidden"
                || (attr == "aria-hidden" && decode(value).trim().eq_ignore_ascii_case("true"))
                || (attr == "style" && is_hidden_style(&decode(value)))
                || (attr == "class" && is_hidden_class(&decode(value)));
        }
    }
}

/// Where raw text opened just before `from` ends: the start and end of its end tag, or None if it never ends. Only
/// the name is matched, then the `>` found with `find` (a pattern running on to the `>` would rescan the rest of the
/// page for each `</script` without one).
fn end_of_raw_text(html: &str, name: &str, from: usize) -> Option<(usize, usize)> {
    let b = html.as_bytes();
    let mut at = from;
    while let Some(i) = html[at..].find("</").map(|i| at + i) {
        let name_end = i + 2 + name.len();
        if b.len() >= name_end
            && html.is_char_boundary(name_end)
            && html[i + 2..name_end].eq_ignore_ascii_case(name)
            && b.get(name_end).is_some_and(|c| is_space(*c) || *c == b'/' || *c == b'>')
        {
            let end = html[name_end..].find('>').map_or(html.len(), |g| name_end + g + 1);
            return Some((i, end));
        }
        at = i + 2;
    }
    None
}

/// The text so far, whitespace collapsed as it comes in, so a run of spaces or empty blocks doesn't use the budget.
struct Text {
    out: String,
    /// In characters.
    length: usize,
    max: usize,
    /// The last character written is whitespace (or nothing is written yet).
    space: bool,
    newlines: usize,
    truncated: bool,
}

impl Text {
    fn new(max: usize) -> Self {
        Self { out: String::new(), length: 0, max, space: true, newlines: 2, truncated: false }
    }

    fn text(&mut self, raw: &str) {
        let mut text = String::with_capacity(raw.len());
        let mut in_space = false;
        for c in raw.chars() {
            if c.is_whitespace() || c == '\u{feff}' {
                if !in_space {
                    text.push(' ');
                }
                in_space = true;
            } else if !is_invisible(c) {
                text.push(c);
                in_space = false;
            }
        }
        let mut text = text.as_str();
        if self.space && text.starts_with(' ') {
            text = &text[1..];
        }
        if text.is_empty() {
            return;
        }
        let count = text.chars().count();
        let text = if self.length + count > self.max {
            self.truncated = true;
            let keep = self.max - self.length;
            &text[..text.char_indices().nth(keep).map_or(text.len(), |(i, _)| i)]
        } else {
            text
        };
        if text.is_empty() {
            return;
        }
        self.out.push_str(text);
        self.length += text.chars().count();
        self.space = text.ends_with(' ');
        self.newlines = 0;
    }

    /// A blank line, unless there is one already.
    fn paragraph(&mut self) {
        while self.newlines < 2 {
            self.line();
        }
    }

    fn line(&mut self) {
        if self.newlines >= 2 {
            return;
        }
        if self.space && self.newlines == 0 && self.length > 0 {
            self.out.pop(); // no space at the end of a line
            self.length -= 1;
        }
        self.out.push('\n');
        self.length += 1;
        self.space = true;
        self.newlines += 1;
    }

    fn into_string(self) -> String {
        self.out.trim().to_string()
    }
}

fn decode(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    static ENTITY: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"&(?:#(\d{1,7});?|#[xX]([0-9a-fA-F]{1,6});?|([a-zA-Z]{2,8});)").unwrap());
    ENTITY
        .replace_all(text, |caps: &regex::Captures| {
            if let Some(named) = caps.get(3) {
                let named = named.as_str();
                let lower = named.to_ascii_lowercase();
                return NAMED
                    .iter()
                    .find(|(k, _)| *k == named)
                    .or_else(|| NAMED.iter().find(|(k, _)| *k == lower))
                    .map_or_else(|| caps[0].to_string(), |(_, v)| v.to_string());
            }
            let code = match (caps.get(1), caps.get(2)) {
                (Some(d), _) => d.as_str().parse::<u32>().unwrap_or(0),
                (_, Some(h)) => u32::from_str_radix(h.as_str(), 16).unwrap_or(0),
                _ => 0,
            };
            match char::from_u32(code) {
                Some(c) if code > 0 => c.to_string(),
                _ => "\u{fffd}".to_string(),
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_one_short_line() {
        assert_eq!(one_line(Some("  Hello\n\tworld  ")), Some("Hello world".into()));
        assert_eq!(one_line(Some("   ")), None);
        let long = "x".repeat(500);
        assert_eq!(one_line(Some(&long)).unwrap().chars().count(), MAX_TITLE);
    }

    #[test]
    fn plain_text_drops_tags_scripts_styles_comments_and_hidden_elements_and_decodes_entities() {
        let html = "<!doctype html><html><head><title>A &amp; B\nC</title><style>p { color: red }</style></head><body>
<!-- a comment --><h1>Head</h1><p>One &lt;two&gt; &#51; &#x34; &nbsp;five</p>
<div style=\"display:none\">HIDDEN-STYLE<div>still hidden</div></div><p aria-hidden=\"true\">HIDDEN-ARIA</p><img hidden src=\"x\">
<select><option>OPT</option></select><script>evil()</script><svg><text>SVG</text></svg>
<ul><li>first</li><li>second</li></ul><p>x<br>y</p><table><tr><td>a</td><td>b</td></tr></table>a < b</body></html>";
        let page = plain_text(html, MAX_CHARS);
        assert!(page.plain);
        assert_eq!(page.title.as_deref(), Some("A & B C"));
        assert_eq!(page.markdown, "Head\n\nOne <two> 3 4 five\n\nfirst\n\nsecond\n\nx\ny\n\na b\n\na < b");
    }

    #[test]
    fn plain_text_reads_quoted_attributes_as_a_browser_does() {
        let text = |html: &str| plain_text(html, MAX_CHARS).markdown;
        assert_eq!(text(r#"<div title="x>" hidden>SECRET</div>visible"#), "visible");
        assert_eq!(text(r#"<img alt="x > y" src="/a.png"> visible"#), "visible");
        assert_eq!(text(r#"<div data-x='a"' hidden=hidden>H</div><div aria-hidden=TRUE>A</div>ok"#), "ok");
        assert_eq!(text(r#"text <div title="never closed>hidden</div> more"#), "text"); // the rest is inside the tag
    }

    #[test]
    fn plain_text_hidden_styles_however_written_and_a_hidden_p_ends_where_a_block_starts() {
        let styles = [
            "display:none",
            "DISPLAY: NONE !important",
            "opacity:0.0",
            "opacity: .0",
            "font-size:0px",
            "visibility : hidden",
            "display:none&#59;",
            "display:/**/none",
            "display:block;display:none",
        ];
        let html: String = styles.iter().enumerate().map(|(i, s)| format!(r#"<span style="{s}">H{i}</span>"#)).collect();
        assert_eq!(plain_text(&format!(r#"{html}<span style="opacity:0.01">shown</span>"#), MAX_CHARS).markdown, "shown");
        assert_eq!(plain_text("<p hidden>secret<p>visible one<p>visible two", MAX_CHARS).markdown, "visible one\n\nvisible two");
    }

    #[test]
    fn plain_text_decodes_the_common_named_entities() {
        assert_eq!(
            plain_text("Caf&eacute; &mdash; &rsquo;quoted&rsquo; &ldquo;q&rdquo; &copy; 2026&hellip; &#39 &unknown;", MAX_CHARS)
                .markdown,
            "Café — ’quoted’ “q” © 2026… ' &unknown;"
        );
    }

    #[test]
    fn plain_text_collapses_whitespace_before_it_counts_and_stops_at_the_budget() {
        assert_eq!(plain_text(&format!("{}<p>real content</p>", " ".repeat(400_000)), MAX_CHARS).markdown, "real content");
        assert_eq!(
            plain_text(&format!("{}<p>real content</p>", "<div> </div>".repeat(60_000)), MAX_CHARS).markdown,
            "real content"
        );
        let cut = plain_text(&format!("<p>{}</p>", "word ".repeat(1000)), 100);
        assert!(cut.markdown.chars().count() <= 100);
        assert!(cut.truncated);
        assert!(!plain_text("<p>short</p>", 100).truncated);
    }

    #[test]
    fn plain_text_stays_linear_on_input_built_to_make_a_scanner_backtrack() {
        // The fastest of three runs, so a pause doesn't count.
        let time = |html: &str| {
            (0..3)
                .map(|_| {
                    let start = std::time::Instant::now();
                    plain_text(html, MAX_CHARS);
                    start.elapsed().as_secs_f64() * 1000.0
                })
                .fold(f64::INFINITY, f64::min)
        };
        for unit in [
            "<script",
            "<select>",
            "<div hidden>",
            "</script ",
            "<a<b<c",
            "<!--",
            "<x y='",
            "<a ",
            "<b hidden></b>",
            "&amp;",
            "<p style='/*/*/*'>",
            "<p class='sr-only'>",
        ] {
            let n = 50_000;
            let once = time(&unit.repeat(n));
            let twice = time(&unit.repeat(2 * n));
            assert!(twice < once * 3.0 + 50.0, "{unit}: {once} ms, then {twice} ms"); // linear doubles; quadratic quadruples
        }
    }

    // ---- marv-rs: hidden-text cases Marv let through (docs/SECURITY.md) ----------------------------------------

    #[test]
    fn off_screen_and_zero_size_styles_hide() {
        for style in [
            "position:absolute;left:-9999px",
            "position: absolute; top: -10000px",
            "text-indent:-9999px",
            "margin-left: -100em",
            "position:absolute;clip:rect(0 0 0 0);width:1px;height:1px;overflow:hidden",
            "clip: rect(1px, 1px, 1px, 1px)",
            "clip-path: inset(50%)",
            "width:0;height:0;overflow:hidden",
            "max-height:0; overflow: hidden",
            "transform: scale(0)",
            "transform:translateX(-9999px)",
            "color: transparent",
        ] {
            assert!(is_hidden_style(style), "{style}");
        }
        for style in [
            "position:absolute;left:10px",
            "margin-left:-1em",
            "width:1px",
            "overflow:hidden",
            "clip-path: inset(10%)",
            "transform: scale(1.2)",
            "color: red",
        ] {
            assert!(!is_hidden_style(style), "{style}");
        }
    }

    #[test]
    fn plain_text_drops_hiding_classes_off_screen_text_and_invisible_characters() {
        let html = r#"<p class="sr-only">SR-ONLY</p><p class="visually-hidden">VH</p><span class="d-none">DNONE</span>
<div style="position:absolute;left:-9999px">OFFSCREEN</div><p>Read\u{200B}me\u{E0041}\u{E0042}</p><p class="hidden md:block">wide</p>"#
            .replace("\\u{200B}", "\u{200B}")
            .replace("\\u{E0041}", "\u{E0041}")
            .replace("\\u{E0042}", "\u{E0042}");
        assert_eq!(plain_text(&html, MAX_CHARS).markdown, "Readme\n\nwide");
    }
}

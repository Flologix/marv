//! Files a `bash` command showed the model whole, which then count as read ([`crate::file_versions`]).
//!
//! marv-rs only (no Marv counterpart). The edit tools refuse to write over an existing file the session never read
//! (docs/SECURITY.md, "No write over a change the session hasn't seen"). Models often read with the shell instead of
//! `read_file` (`cat src/a.js`, `for f in *.js; do cat "$f"; done`), and each such file's first edit was refused and
//! retried: 65 of Claude Haiku 5.5's 69 tool errors on the bench, 78 of DeepSeek V4.1 Flash's (2026-10-31), and the
//! extra steps pushed `long-haul` over the step limit. What the model was shown is as good as a `read_file` when it
//! holds the file's whole current content: that version is then recorded as seen, exactly as `read_file` records it.
//!
//! The rule only adds a way to have read a file; the check is the same:
//! - Judged on the text the model was shown (after Marv's cut of long output): a file the cut left out wasn't seen.
//! - The file's **whole** current content must appear verbatim (CRLF read as LF, a final newline optional), or with
//!   each line numbered as `cat -n` and `nl` number them (Claude Haiku 5.5 read with `cat -n`, 2026-10-31). A partial
//!   view (`head`, `grep`, `sed -n`) doesn't count, unlike a partial `read_file`: there's no telling which part.
//! - Read after the command, so it's the version on disk that the model saw; a change after this is refused as before.
//! - Candidates are paths named in the command or its output, inside the project (`resolve_in_project`: no symlink out),
//!   regular text files up to [`MAX_FILE_BYTES`], at most [`MAX_CANDIDATES`] of them, also relative to a `cd` in the
//!   command. A name the output alone gives (a loop's `== a.js` header) needs [`MIN_OUTPUT_ONLY_BYTES`] of content, so
//!   a tiny file whose few bytes appear in unrelated output by chance isn't taken as seen.
//! - A secret's name (`.env*`, `*.pem`, `id_*`) never counts, as `check_showing` never shows one.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::files::{is_regular_file, resolve_in_project};
use crate::types::ToolContext;

/// Files bigger than this aren't checked (a command that printed one whole is rare, and reading it costs).
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
/// At most this many paths are tried per command.
pub const MAX_CANDIDATES: usize = 200;
/// A file named only in the output (not the command) must have at least this much content to count.
pub const MIN_OUTPUT_ONLY_BYTES: usize = 24;

fn cd_dirs(command: &str) -> Vec<String> {
    static CD: OnceLock<Regex> = OnceLock::new();
    let re = CD.get_or_init(|| Regex::new(r#"(?:^|[;&|(\n]\s*)cd\s+(?:"([^"]+)"|'([^']+)'|([^\s;&|)]+))"#).unwrap());
    re.captures_iter(command).filter_map(|c| c.get(1).or(c.get(2)).or(c.get(3)).map(|m| m.as_str().to_string())).collect()
}

/// The path-like words of a text: split on whitespace, quotes and shell punctuation, trimmed of trailing punctuation.
fn words(text: &str, path_like_only: bool) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || "'\"`;|&<>(){}=,".contains(c))
        .map(|w| w.trim_end_matches([':', '.', ',', ')', ']']).trim_start_matches(['[', '(']))
        .filter(move |w| {
            !w.is_empty()
                && w.len() <= 4096
                && !w.starts_with('-')
                && w.chars().any(|c| c.is_alphanumeric())
                && (!path_like_only || w.contains('.') || w.contains('/'))
        })
}

/// Whether `shown` holds `content` whole (CRLF as LF; a final newline optional), as it is or line-numbered.
fn holds(shown: &str, content: &str, unnumbered: &str) -> bool {
    let content = content.replace("\r\n", "\n");
    let body = content.strip_suffix('\n').unwrap_or(&content);
    !body.trim().is_empty() && (shown.contains(body) || unnumbered.contains(body))
}

/// `shown` with the line numbers `cat -n` and `nl` put in front of each line (`     12\t`) taken off; other lines as
/// they are.
fn without_line_numbers(shown: &str) -> String {
    static NUMBERED: OnceLock<Regex> = OnceLock::new();
    let re = NUMBERED.get_or_init(|| Regex::new(r"^ *\d+\t").unwrap());
    shown.split('\n').map(|line| re.replace(line, "")).collect::<Vec<_>>().join("\n")
}

/// Records as seen each project file whose whole current content `shown` (what the model was shown of `command`'s
/// output) holds. Returns the files recorded, project-relative (for tests and the trajectory).
pub fn record_files_shown(ctx: &ToolContext, command: &str, shown: &str) -> Vec<String> {
    let shown = shown.replace("\r\n", "\n");
    let unnumbered = without_line_numbers(&shown);
    let mut bases: Vec<PathBuf> = vec![PathBuf::new()];
    bases.extend(cd_dirs(command).into_iter().map(PathBuf::from));
    let mut tried: HashSet<PathBuf> = HashSet::new();
    let mut recorded = Vec::new();
    let named = words(command, false).map(|w| (w, true));
    let printed = words(&shown, true).map(|w| (w, false));
    for (word, in_command) in named.chain(printed) {
        if tried.len() >= MAX_CANDIDATES {
            break;
        }
        for base in &bases {
            let relative = if Path::new(word).is_absolute() { PathBuf::from(word) } else { base.join(word) };
            let Ok(absolute) = resolve_in_project(ctx, &relative.to_string_lossy()) else { continue };
            if !tried.insert(absolute.clone()) {
                continue;
            }
            if let Some(shown_path) = shown_whole(ctx, &absolute, &shown, &unnumbered, in_command) {
                recorded.push(shown_path);
            }
        }
    }
    recorded
}

/// The file's project path when `shown` holds its whole current content (and it's one that may count); it's then
/// recorded as seen.
fn shown_whole(ctx: &ToolContext, absolute: &Path, shown: &str, unnumbered: &str, in_command: bool) -> Option<String> {
    let name = absolute.file_name()?.to_string_lossy().into_owned();
    if !is_regular_file(absolute) || marv_sandbox::protected::is_secret_name(&name) {
        return None;
    }
    if std::fs::metadata(absolute).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(absolute).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let content = String::from_utf8(bytes).ok()?;
    if !in_command && content.trim().len() < MIN_OUTPUT_ONLY_BYTES {
        return None;
    }
    if !holds(shown, &content, unnumbered) {
        return None;
    }
    ctx.file_versions.saw(absolute, content.as_bytes());
    Some(crate::files::project_path(&ctx.root, absolute))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    const A: &str = "export function a() {\n  return 1;\n}\n";
    const B: &str = "export function b() {\n  return 2;\n}\n";

    fn ctx(root: &TempDir) -> ToolContext {
        ToolContext::new(root.path())
    }
    fn is_seen(ctx: &ToolContext, root: &TempDir, rel: &str) -> bool {
        let path = root.path().join(rel);
        ctx.file_versions.is_current(&path, &std::fs::read(&path).unwrap())
    }

    #[test]
    fn a_file_catted_whole_counts_as_read() {
        let root = TempDir::new("shown-cat");
        root.write("src/a.js", A);
        let ctx = ctx(&root);
        assert_eq!(record_files_shown(&ctx, "cat src/a.js", &format!("{A}\n[exit code 0]")), ["src/a.js"]);
        assert!(is_seen(&ctx, &root, "src/a.js"));
    }

    #[test]
    fn a_loop_after_cd_with_headers_counts_each_file() {
        let root = TempDir::new("shown-loop");
        root.write("src/units/a.js", A);
        root.write("src/units/b.js", B);
        let ctx = ctx(&root);
        let shown = format!("== a.js\n{A}\n== b.js\n{B}\n\n[exit code 0]");
        let mut got = record_files_shown(&ctx, r#"cd src/units && for f in *.js; do echo "== $f"; cat "$f"; echo; done"#, &shown);
        got.sort();
        assert_eq!(got, ["src/units/a.js", "src/units/b.js"]);
    }

    #[test]
    fn a_partial_view_a_cut_or_a_changed_file_doesnt_count() {
        let root = TempDir::new("shown-partial");
        root.write("src/a.js", A);
        let ctx = ctx(&root);
        assert!(record_files_shown(&ctx, "head -n 2 src/a.js", "export function a() {\n  return 1;\n").is_empty());
        assert!(record_files_shown(&ctx, "grep return src/a.js", "  return 1;").is_empty());
        let cut = "export function a() {\n… (output cut: 1 more line, saved to /tmp/x)";
        assert!(record_files_shown(&ctx, "cat src/a.js", cut).is_empty());
        assert!(!is_seen(&ctx, &root, "src/a.js"));
        // What was shown is an older version: the file on disk isn't the one the model saw.
        assert!(record_files_shown(&ctx, "cat src/a.js", B).is_empty());
    }

    #[test]
    fn a_tiny_file_named_only_in_the_output_doesnt_count_but_one_in_the_command_does() {
        let root = TempDir::new("shown-tiny");
        root.write("n.txt", "1\n");
        let ctx = ctx(&root);
        assert!(record_files_shown(&ctx, "ls", "n.txt\n1 file\n").is_empty());
        assert_eq!(record_files_shown(&ctx, "cat n.txt", "1\n"), ["n.txt"]);
    }

    #[test]
    fn secrets_paths_outside_and_binaries_never_count() {
        let root = TempDir::new("shown-secret");
        let outside = TempDir::new("shown-outside");
        root.write(".env", "API_KEY=sk-1234567890abcdef1234567890\n");
        root.write("logo.bin", [0u8, 1, 2, 3]);
        outside.write("x.js", A);
        std::os::unix::fs::symlink(outside.path(), root.path().join("out")).unwrap();
        let ctx = ctx(&root);
        assert!(record_files_shown(&ctx, "cat .env", "API_KEY=sk-1234567890abcdef1234567890\n").is_empty());
        assert!(record_files_shown(&ctx, "cat out/x.js", A).is_empty());
        let absolute = format!("cat {}/x.js", outside.path().display());
        assert!(record_files_shown(&ctx, &absolute, A).is_empty());
    }

    #[test]
    fn a_line_numbered_cat_counts_but_numbered_part_of_it_doesnt() {
        let root = TempDir::new("shown-numbered");
        root.write("src/a.js", A);
        let ctx = ctx(&root);
        let numbered = "     1\texport function a() {\n     2\t  return 1;\n     3\t}\n";
        assert!(record_files_shown(&ctx, "cat -n src/a.js", &numbered[..numbered.find("     3").unwrap()]).is_empty());
        assert_eq!(record_files_shown(&ctx, "cat -n src/a.js", numbered), ["src/a.js"]);
    }

    #[test]
    fn crlf_files_match_their_lf_output() {
        let root = TempDir::new("shown-crlf");
        root.write("w.txt", "line one of the file\r\nline two of the file\r\n");
        let ctx = ctx(&root);
        assert_eq!(record_files_shown(&ctx, "cat w.txt", "line one of the file\nline two of the file\n"), ["w.txt"]);
    }
}

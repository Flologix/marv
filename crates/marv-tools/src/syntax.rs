//! After a file change, does the file still parse? `edit_file` and `write_file` add a `Marv:` note when a change
//! broke it, so the model finds out in the same step instead of a few steps later from a failing test (or never: in a
//! third of the eval runs the model didn't run anything after its last edit).
//!
//! Ports Marv's `src/tools/syntax.ts`.
//!
//! - **Parsing must never run the file's code.** Marv learned this with `Bun.Transpiler` (its `transformSync` and
//!   `scan` execute macros, so checking a file would run it unsandboxed). Every parser here only parses.
//! - Kinds: JS/TS/JSX (Marv: `@babel/parser`; port plan: tree-sitter's javascript/typescript/tsx grammars, an
//!   `ERROR`/`MISSING` node is the problem), JSON (not JSONC: `tsconfig*.json`, `jsconfig*.json`, `.jsonc`,
//!   `.vscode/`), YAML, Python (`python3 -I -c 'ast.parse'` from `/` with an empty environment, 5 s timeout).
//! - Files over [`MAX_CHARS`] = 2M chars are skipped (a parse should take milliseconds; that size is likely generated).
//! - [`broken_by`]: a file that already didn't parse isn't blamed (and a parser that disagrees with the project would
//!   otherwise complain about every edit).

use std::path::Path;

use crate::read_file::numbered_line;

/// Bigger than this isn't checked.
pub const MAX_CHARS: usize = 2_000_000;
pub const PYTHON_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxProblem {
    pub message: String,
    /// 1-based.
    pub line: Option<usize>,
    /// 1-based.
    pub column: Option<usize>,
}

/// JSON files that may have comments and trailing commas (JSONC), which a JSON parser rejects.
pub fn is_jsonc(path: &str) -> bool {
    let base = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let config = ["tsconfig", "jsconfig"].iter().any(|stem| base == format!("{stem}.json") || (base.starts_with(&format!("{stem}.")) && base.ends_with(".json")));
    config || path.ends_with(".jsonc") || path.starts_with(".vscode/") || path.contains("/.vscode/")
}

/// The file's syntax error, by its extension; None if it parses, or isn't a kind Marv checks.
pub async fn syntax_error(_path: &str, text: &str) -> Option<SyntaxProblem> {
    if text.chars().count() > MAX_CHARS {
        return None;
    }
    todo!("port: syntaxError: JS/TS via tree-sitter, JSON via serde_json (unless is_jsonc), YAML, Python via python3 -I")
}

/// A syntax error the change introduced: the file parsed before (or is new) and doesn't now.
pub async fn broken_by(path: &str, before: Option<&str>, after: &str) -> Option<SyntaxProblem> {
    let now = syntax_error(path, after).await?;
    if let Some(before) = before
        && syntax_error(path, before).await.is_some()
    {
        return None;
    }
    Some(now)
}

/// What the model is told, after the edit's own result.
pub fn syntax_note(shown: &str, problem: &SyntaxProblem, after: &str, existed: bool) -> String {
    let at = match (problem.line, problem.column) {
        (Some(line), Some(column)) => format!(" at line {line}, column {column}"),
        (Some(line), None) => format!(" at line {line}"),
        _ => String::new(),
    };
    let what = if existed { "doesn't parse any more after this change" } else { "doesn't parse" };
    let quoted = problem
        .line
        .and_then(|n| after.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).nth(n - 1).map(|line| (n, line)))
        .map(|(n, line)| {
            let shown_line = if line.chars().count() > 500 { format!("{}…", line.chars().take(500).collect::<String>()) } else { line.to_string() };
            format!("\n{}", numbered_line(n, &shown_line))
        })
        .unwrap_or_default();
    format!(
        "Marv: {shown} {what}: {}{at}.{quoted}\nFix it before going on (if you're sure the syntax is valid for this project, ignore this).",
        problem.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_files_are_recognised() {
        assert!(is_jsonc("tsconfig.json"));
        assert!(is_jsonc("packages/a/tsconfig.build.json"));
        assert!(is_jsonc(".vscode/settings.json"));
        assert!(is_jsonc("a.jsonc"));
        assert!(!is_jsonc("package.json"));
    }

    #[test]
    fn the_note_quotes_the_broken_line() {
        let problem = SyntaxProblem { message: "Unexpected token".into(), line: Some(2), column: Some(5) };
        let note = syntax_note("a.ts", &problem, "ok\nbad (\n", true);
        assert_eq!(note, "Marv: a.ts doesn't parse any more after this change: Unexpected token at line 2, column 5.\n    2→bad (\nFix it before going on (if you're sure the syntax is valid for this project, ignore this).");
    }
}

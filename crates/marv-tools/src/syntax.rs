//! After a file change, does the file still parse? `edit_file` and `write_file` add a `Marv:` note when a change
//! broke it, so the model finds out in the same step instead of a few steps later from a failing test (or never: in a
//! third of the eval runs the model didn't run anything after its last edit).
//!
//! Ports Marv's `src/tools/syntax.ts`.
//!
//! - **Parsing must never run the file's code.** Marv learned this with `Bun.Transpiler` (its `transformSync` and
//!   `scan` execute macros, so checking a file would run it unsandboxed). Every parser here only parses.
//! - Kinds: JS/TS/JSX, JSON (not JSONC: `tsconfig*.json`, `jsconfig*.json`, `.jsonc`, `.vscode/`), YAML (every
//!   document in the file), Python (`python3 -I -c 'ast.parse'` from `/` with an empty environment, 5 s timeout).
//! - Port note: JS/TS is checked with oxc (its parser plus the semantic pass's syntax checks), in place of Marv's
//!   `@babel/parser`. Like Babel it reports what a parser can know (a redeclared `const`, TypeScript in a `.js` file),
//!   accepts scripts, CommonJS (`return` at the top) and modules alike (unambiguous), JSX in `.js` files, decorators
//!   and `using`. Messages are oxc's wording, not Babel's; the line and the 1-based column (in chars) are where the
//!   error's primary label starts. JSON and YAML messages are serde_json's and serde_yaml_ng's.
//! - Files over [`MAX_CHARS`] = 2M chars are skipped (a parse should take milliseconds; that size is likely generated).
//! - [`broken_by`]: a file that already didn't parse isn't blamed (and a parser that disagrees with the project would
//!   otherwise complain about every edit).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use oxc_allocator::Allocator;
use oxc_parser::{ParseOptions, Parser};
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::read_file::numbered_line;

/// Bigger than this isn't checked.
pub const MAX_CHARS: usize = 2_000_000;
pub const PYTHON_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    let config = ["tsconfig", "jsconfig"]
        .iter()
        .any(|stem| base == format!("{stem}.json") || (base.starts_with(&format!("{stem}.")) && base.ends_with(".json")));
    config || path.ends_with(".jsonc") || path.starts_with(".vscode/") || path.contains("/.vscode/")
}

/// The file's syntax error, by its extension; None if it parses, or isn't a kind Marv checks.
pub async fn syntax_error(path: &str, text: &str) -> Option<SyntaxProblem> {
    if text.len() > MAX_CHARS && text.chars().count() > MAX_CHARS {
        return None;
    }
    let ext = path.rsplit_once('.').map_or(path, |(_, ext)| ext).to_lowercase();
    if let Some(source_type) = js_source_type(&ext) {
        return parse_js(text, source_type);
    }
    let message = |e: &dyn std::fmt::Display| Some(SyntaxProblem { message: e.to_string(), line: None, column: None });
    match ext.as_str() {
        "json" if !is_jsonc(path) => serde_json::from_str::<serde::de::IgnoredAny>(text).err().and_then(|e| message(&e)),
        "yaml" | "yml" => {
            for document in serde_yaml_ng::Deserializer::from_str(text) {
                if let Err(e) = serde_yaml_ng::Value::deserialize(document) {
                    return message(&e);
                }
            }
            None
        }
        "py" => parse_python(text).await,
        _ => None,
    }
}

/// How each JS/TS extension is parsed. JSX in `.js` files is common (React projects), and allowing it costs nothing
/// for plain JavaScript. Unambiguous: scripts, CommonJS and module fragments are all fine.
fn js_source_type(ext: &str) -> Option<SourceType> {
    Some(match ext {
        "ts" | "mts" | "cts" => SourceType::ts(),
        "tsx" => SourceType::tsx(),
        "js" | "mjs" | "cjs" | "jsx" => SourceType::jsx().with_unambiguous(true),
        _ => return None,
    })
}

fn parse_js(text: &str, source_type: SourceType) -> Option<SyntaxProblem> {
    let allocator = Allocator::default();
    let options = ParseOptions { allow_return_outside_function: true, ..ParseOptions::default() };
    let parsed = Parser::new(&allocator, text, source_type).with_options(options).parse();
    let is_error = |d: &oxc_diagnostics::OxcDiagnostic| d.severity == oxc_diagnostics::Severity::Error;
    let error = match parsed.diagnostics.into_iter().find(is_error) {
        Some(error) => error,
        // What only the semantic pass knows: a redeclared binding, a misplaced `super`.
        None => {
            SemanticBuilder::new().with_check_syntax_error(true).build(&parsed.program).diagnostics.into_iter().find(is_error)?
        }
    };
    let label = error.labels.iter().find(|l| l.primary()).or(error.labels.last());
    let (line, column) = match label {
        Some(label) => {
            let (line, column) = line_column(text, label.offset() as usize);
            (Some(line), Some(column))
        }
        None => (None, None),
    };
    Some(SyntaxProblem { message: error.message.to_string(), line, column })
}

/// The 1-based line and column (in chars) of a byte offset.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..text.floor_char_boundary(offset.min(text.len()))];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (before.matches('\n').count() + 1, before[line_start..].chars().count() + 1)
}

/// `python3` on the host's PATH.
fn python3() -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join("python3")).find(|p| p.is_file())
}

async fn parse_python(text: &str) -> Option<SyntaxProblem> {
    let python = python3()?;
    // -I: no environment variables, no user site-packages, not the current folder on the path. ast.parse only builds
    // a tree; the file's code never runs.
    let script = "import ast, json, sys\ntry:\n    ast.parse(sys.stdin.read())\nexcept SyntaxError as e:\n    print(json.dumps({'message': e.msg, 'line': e.lineno, 'column': e.offset}))\n";
    let mut child = tokio::process::Command::new(python)
        .args(["-I", "-c", script])
        .current_dir("/")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let input = text.to_string();
    // Written alongside the wait, so a big file can't block on a full pipe.
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(input.as_bytes()).await;
    });
    let output = tokio::time::timeout(Duration::from_millis(PYTHON_TIMEOUT_MS), child.wait_with_output()).await.ok()?.ok()?;
    let _ = writer.await;
    let out = String::from_utf8_lossy(&output.stdout);
    let out = out.trim();
    if out.is_empty() { None } else { serde_json::from_str(out).ok() }
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
            let shown_line = if line.chars().count() > 500 {
                format!("{}…", line.chars().take(500).collect::<String>())
            } else {
                line.to_string()
            };
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

    async fn error(path: &str, text: &str) -> Option<SyntaxProblem> {
        syntax_error(path, text).await
    }

    #[tokio::test]
    async fn finds_a_js_ts_syntax_error_with_its_line_and_column_1_based() {
        let ts = error("a.ts", "export function f(x: number) {\n  return x +;\n}\n").await.unwrap();
        assert_eq!((ts.line, ts.column), (Some(2), Some(13)), "{ts:?}");
        let js = error("a.js", "const a = 1;\nconst a = 2;\n").await.unwrap();
        assert!(js.message.contains("already been declared"), "{js:?}");
        assert_eq!(js.line, Some(2));
    }

    #[tokio::test]
    async fn typescript_in_a_js_file_is_an_error() {
        // A model wrote types into JavaScript.
        assert!(error("server.js", "export function createServer(options: {port: number}) {}\n").await.is_some());
    }

    #[tokio::test]
    async fn valid_files_are_fine() {
        for (path, text) in [
            ("a.ts", "import data from \"./d.json\" with { type: \"json\" };\nexport const x = data satisfies object;\n"),
            ("a.ts", "await using res = getResource();\n"),
            (
                "a.ts",
                "@Component({})\nexport class A { @Input() accessor name = 1; #p = 1; has(o: object) { return #p in o; } }\n",
            ),
            ("a.ts", "enum E { A = 1 }\nnamespace N { export const a = 1; }\nexport = N;\n"),
            ("a.tsx", "export const A = () => <div>{1}</div>;\nconst f = <T,>(x: T) => x;\n"),
            ("a.js", "export const A = () => <div className='a'>{1}</div>;\n"),
            ("a.cjs", "#!/usr/bin/env node\nmodule.exports = {};\nreturn;\n"),
            ("a.json", "{ \"a\": [1, 2] }\n"),
            ("a.yaml", "a: [1, 2]\nb: { c: 3 }\n"),
            ("multi.yaml", "a: 1\n---\nb: 2\n"),
            ("notes.md", "# anything `goes`"),
        ] {
            assert_eq!(error(path, text).await, None, "{path}: {text}");
        }
    }

    #[tokio::test]
    async fn json_and_yaml_errors() {
        assert!(error("a.json", "{\n  \"a\": 1,\n}\n").await.is_some());
        assert!(error("a.yml", "a: [1, 2\nb: 3\n").await.is_some());
        assert!(error("multi.yml", "a: 1\n---\nb: [\n").await.is_some());
    }

    #[tokio::test]
    async fn json_with_comments_isnt_checked() {
        // tsconfig.json, .jsonc and VS Code's settings allow comments.
        assert_eq!(error("tsconfig.json", "{ // comment\n  \"a\": 1, }\n").await, None);
        assert_eq!(error("tsconfig.build.json", "{ /* c */ }").await, None);
        assert_eq!(error(".vscode/settings.json", "{ // c\n}").await, None);
        assert_eq!(error("x.jsonc", "{ // c\n}").await, None);
    }

    #[tokio::test]
    async fn python_parsed_without_running_it() {
        if python3().is_none() {
            eprintln!("skipped: no python3");
            return;
        }
        let marker = std::env::temp_dir().join(format!("marv-should-not-run-{}", std::process::id()));
        assert_eq!(error("a.py", "def f(:\n    pass\n").await.unwrap().line, Some(1));
        let code = format!("import os\nos.system('touch {}')\n", marker.display());
        assert_eq!(error("a.py", &code).await, None);
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn a_file_too_big_to_check_quickly_isnt_checked() {
        assert_eq!(error("big.js", &format!("{} +", "x".repeat(3_000_000))).await, None);
    }

    #[tokio::test]
    async fn broken_by_blames_only_what_the_change_broke() {
        assert!(broken_by("a.js", Some("const a = 1;\n"), "const a = ;\n").await.is_some());
        // A file that didn't parse before isn't blamed on the change.
        assert!(broken_by("a.js", Some("const a = ;\n"), "const a = ;\nconst b = ;\n").await.is_none());
        // A new file is checked.
        assert!(broken_by("a.js", None, "const a = ;\n").await.is_some());
    }

    #[test]
    fn the_note_quotes_the_broken_line() {
        let problem = SyntaxProblem { message: "Unexpected token".into(), line: Some(2), column: Some(5) };
        let note = syntax_note("a.ts", &problem, "ok\nbad (\n", true);
        assert_eq!(
            note,
            "Marv: a.ts doesn't parse any more after this change: Unexpected token at line 2, column 5.\n    2→bad (\nFix it before going on (if you're sure the syntax is valid for this project, ignore this)."
        );
    }
}

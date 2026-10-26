//! Python's checker, basedpyright or pyright `--outputjson`, after a step's file changes (marv-rs, #187;
//! `docs/specs/2026-10-25-lsp-checker.md` §4 "Python", §6.5). New in marv-rs: TypeScript Marv has only tsc; nothing
//! here is a port.
//!
//! - **Applies** when the project root has a `pyrightconfig.json`, `pyproject.toml`, `setup.py`, `setup.cfg` or
//!   `requirements.txt`. Only looks: whether a checker is installed is learned from the sandboxed run itself.
//! - **Which checker** is chosen inside the sandbox, by the command ([`check_command`]), never on the host: the
//!   project's own `.venv/bin/basedpyright`, then its `node_modules/.bin/pyright`, then `basedpyright`, then `pyright`
//!   on the sandbox's PATH. A project-local one is the project's own code, as `node_modules/.bin/tsc` is: allowed,
//!   because it runs sandboxed, with the project read-only and no network. Nothing a project file names ever runs on
//!   the host (`docs/SECURITY.md`; the no-host-spawn test pins it). basedpyright comes first because its PyPI wheel
//!   brings its own node and works offline; the PyPI `pyright` wrapper downloads node on its first run, which fails
//!   without network (unreadable, said once).
//! - **What the project steers**, inside the sandbox only: pyright *runs* a Python interpreter to find import paths
//!   (`venvPath`/`venv` in its config pick which), and that venv's `.pth` files run at its startup.
//! - **Reading it** ([`parse_pyright`]): errors only (`severity: "error"`), the whole project; `file` is absolute, so
//!   it's made relative to the root, and a file outside the root is dropped; lines and columns from 1. The run is
//!   unreadable unless it exited 0 or 1 (2 is a fatal error, 3 a config file it couldn't parse), its JSON is the last
//!   thing in the output (text before it, such as a node warning on stderr, is skipped; anything after it isn't),
//!   and the summary's `errorCount` matches the errors listed. Messages are cut at [`super::cargo::MAX_MESSAGE`]
//!   (basedpyright's no-break-space indents made plain spaces first).
//! - **Not installed:** none of the four in the sandbox (the command exits 127 having printed nothing) turns the kind
//!   off for the session quietly: most Python projects don't have pyright installed, and that isn't worth a notice.
//! - **Hidden interpreters**, said once (§6.5): `env: 'node': No such file or directory` (npm's pyright with a node
//!   the sandbox hides), a `bad interpreter` or `required file not found` (a `.venv/bin/basedpyright` whose shebang
//!   names a Python the sandbox can't see), and, before anything runs, a `.venv/bin/python` linking into a folder the
//!   sandbox hides ([`hidden_venv_python`], read with `read_link`, never run). The usual one is uv's: its venvs link
//!   to `~/.local/share/uv/python`, which isn't a sandbox toolchain folder (yet: that's its own change).
//! - **Output size:** an unconfigured project can have thousands of baseline errors; past `MAX_CAPTURE` the run is
//!   unreadable, and two unreadable runs in a row turn the kind off for the session.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::Diagnostic;
use super::cargo::cut;
use super::tsc::{MAX_REPORTED, error_list, missing_interpreter, ts_error_key};
use super::turn::{CheckKind, CheckPass, MAX_UNREADABLE, low_priority};

/// Its name in notices, `/diagnostics` and the note (for basedpyright too).
pub const LABEL: &str = "pyright";
/// The files that make a folder a Python project, at its root.
pub const MARKERS: &[&str] = &["pyrightconfig.json", "pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"];

/// Whether the project is a Python project: one of [`MARKERS`] at its root. Only looks.
pub fn applies(root: &Path) -> bool {
    MARKERS.iter().any(|m| root.join(m).is_file())
}

/// The command, run in the sandbox with the project as its folder: the first checker there, in the module doc's order,
/// or exit 127 with nothing printed when there's none. Resolved by the sandbox's shell, never on the host.
pub fn check_command() -> String {
    let cmd = "if [ -x .venv/bin/basedpyright ]; then exec .venv/bin/basedpyright --outputjson; \
               elif [ -x node_modules/.bin/pyright ]; then exec node_modules/.bin/pyright --outputjson; \
               elif command -v basedpyright >/dev/null 2>&1; then exec basedpyright --outputjson; \
               elif command -v pyright >/dev/null 2>&1; then exec pyright --outputjson; \
               else exit 127; fi";
    low_priority(&[], cmd)
}

/// `file` relative to the project, or `None` when it's outside it. pyright names files by their real paths (node's
/// `process.cwd()`), so a root reached through a symlink is matched by its real path too.
fn relative(file: &str, root: &Path, real_root: Option<&Path>) -> Option<String> {
    let file = Path::new(file);
    let rel = file.strip_prefix(root).ok().or_else(|| real_root.and_then(|r| file.strip_prefix(r).ok()))?;
    let rel = rel.to_str()?;
    (!rel.is_empty()).then(|| rel.to_string())
}

/// The errors in pyright's `--outputjson` output, or `None` when the run can't be read (see the module doc).
pub fn parse_pyright(root: &Path, output: &str, exit_code: Option<i32>) -> Option<Vec<Diagnostic>> {
    if !matches!(exit_code, Some(0 | 1)) {
        return None;
    }
    // The JSON starts at the first line that starts with `{`; stderr's lines may come before it.
    let start = if output.starts_with('{') { 0 } else { output.find("\n{")? + 1 };
    let value: Value = serde_json::from_str(&output[start..]).ok()?;
    let listed = value.get("generalDiagnostics")?.as_array()?;
    let error_count = value.get("summary")?.get("errorCount")?.as_u64()?;
    let real_root = std::fs::canonicalize(root).ok();
    let mut errors = Vec::new();
    let mut counted = 0u64;
    for d in listed {
        if d.get("severity")?.as_str()? != "error" {
            continue;
        }
        counted += 1;
        let message = d.get("message")?.as_str()?;
        let start = d.get("range").and_then(|r| r.get("start"));
        let at = |k: &str| start.and_then(|s| s.get(k)).and_then(Value::as_u64).map(|n| n + 1);
        let file = match d.get("file").and_then(Value::as_str).filter(|f| !f.is_empty()) {
            Some(f) => match relative(f, root, real_root.as_deref()) {
                Some(rel) => Some(rel),
                None => continue, // outside the project: not the model's to fix
            },
            None => None,
        };
        errors.push(Diagnostic {
            line: at("line").filter(|_| file.is_some()),
            column: at("character").filter(|_| file.is_some()),
            file,
            code: d.get("rule").and_then(Value::as_str).unwrap_or("").to_string(),
            // basedpyright indents a message's detail lines with no-break spaces; pyright with plain ones.
            message: cut(message.replace('\u{a0}', " ")),
        });
    }
    // The summary disagreeing with the list, or the exit code with both: something else is going on.
    if counted != error_count || (exit_code == Some(1)) != (counted > 0) {
        return None;
    }
    Some(errors)
}

static BAD_INTERPRETER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m): ([^:\n]+): bad interpreter: No such file or directory\s*$").unwrap());
static REQUIRED_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^(?:\S+: )?(?:line \d+: )?(.+?): cannot execute: required file not found\s*$").unwrap());

/// `path` with the home folder written `~`, for a notice.
fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && home != "/" && path.starts_with(&format!("{home}/")) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

/// The interpreter a failed run couldn't find: npm pyright's `node`, or the Python a launcher's shebang names
/// (`bad interpreter`, exit 126; newer bash says `required file not found`, exit 127, naming only the launcher).
pub fn missing(output: &str, exit_code: Option<i32>) -> Option<String> {
    if let Some(node) = missing_interpreter(output, exit_code) {
        return Some(node);
    }
    if !matches!(exit_code, Some(126 | 127)) {
        return None;
    }
    if let Some(c) = BAD_INTERPRETER.captures(output) {
        return Some(tilde(c[1].trim()));
    }
    REQUIRED_FILE.captures(output).map(|c| format!("the interpreter of {}", tilde(c[1].trim())))
}

/// A `.venv/bin/python` in the project that links (through any chain of links) into a folder the sandbox hides,
/// written for the notice: "this project's Python (~/.local/share/uv/python/…)". Reads links only; never runs it. A
/// link inside the project is followed on, since the project itself is shown.
pub fn hidden_venv_python(root: &Path, hides: &dyn Fn(&Path) -> bool) -> Option<String> {
    let real_root = std::fs::canonicalize(root).ok();
    let inside = |p: &Path| p.starts_with(root) || real_root.as_deref().is_some_and(|r| p.starts_with(r));
    let mut at: PathBuf = root.join(".venv/bin/python");
    for _ in 0..16 {
        let target = std::fs::read_link(&at).ok()?;
        let next = at.parent()?.join(target);
        if !inside(&next) && hides(&next) {
            return Some(format!("this project's Python ({})", tilde(&next.to_string_lossy())));
        }
        at = next;
    }
    None
}

/// What the model is told: Marv's sentence, then the new errors fenced as the project's data.
pub fn pyright_note(added: &[Diagnostic]) -> String {
    pyright_note_within(added, MAX_REPORTED)
}

/// [`pyright_note`] listing at most `limit` errors: a step's kinds share one budget.
pub fn pyright_note_within(added: &[Diagnostic], limit: usize) -> String {
    let n = added.len();
    crate::provenance::project_note(
        &if n == 1 {
            "pyright found 1 error that wasn't in the previous check.".to_string()
        } else {
            format!("pyright found {n} errors that weren't in the previous check.")
        },
        LABEL,
        &error_list(added, limit),
    )
}

/// The pyright kind.
pub struct Pyright;

impl CheckKind<Diagnostic> for Pyright {
    fn label(&self) -> &'static str {
        LABEL
    }
    fn source(&self) -> Option<&'static str> {
        Some("pyright")
    }
    fn max_unreadable(&self) -> Option<u32> {
        Some(MAX_UNREADABLE)
    }
    fn prepare(&self, root: &Path) -> bool {
        applies(root)
    }
    /// Every pass is the whole project (a changed signature breaks callers in other files), so the whole baseline is
    /// re-checked.
    fn pass(&self, _root: &Path, _cache: Option<&Path>, _changed: Option<&[String]>) -> Option<CheckPass<Diagnostic>> {
        Some(CheckPass { command: check_command(), covers: None, read_only: Vec::new() })
    }
    /// Without the root its paths can't be made relative: unreadable (the machinery calls [`Self::parse_in`]).
    fn parse(&self, _output: &str, _exit_code: Option<i32>) -> Option<Vec<Diagnostic>> {
        None
    }
    fn parse_in(&self, root: &Path, output: &str, exit_code: Option<i32>) -> Option<Vec<Diagnostic>> {
        parse_pyright(root, output, exit_code)
    }
    fn missing(&self, output: &str, exit_code: Option<i32>) -> Option<String> {
        missing(output, exit_code)
    }
    fn hidden(&self, root: &Path) -> Option<String> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let backend = marv_sandbox::backend();
        hidden_venv_python(root, &|p| backend.hides(p, &home))
    }
    /// None of the four is in the sandbox: the command's own `exit 127`, with nothing printed.
    fn not_installed(&self, output: &str, exit_code: Option<i32>) -> bool {
        exit_code == Some(127) && output.trim().is_empty()
    }
    /// File, rule and message; not the line, which edits move.
    fn key(&self, problem: &Diagnostic) -> String {
        ts_error_key(problem)
    }
    fn note(&self, added: &[Diagnostic]) -> String {
        pyright_note(added)
    }
    fn note_within(&self, added: &[Diagnostic], limit: usize) -> String {
        pyright_note_within(added, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tsc::format_error;
    use super::*;

    /// Real `basedpyright --outputjson` output (basedpyright 1.40.2, on pyright 1.1.414) on a package with an error in
    /// `pkg/lib.py` and four in `pkg/use.py`, which calls it; basedpyright's default mode adds warnings. Paths moved
    /// to `/work/fixture`.
    const BASED: &str = include_str!("fixtures/basedpyright-errors.json");
    /// The same project under npm's `pyright --outputjson` (1.1.414): errors only.
    const PYRIGHT: &str = include_str!("fixtures/pyright-errors.json");
    /// pyright on the project once fixed (real path left in: no file is named).
    const CLEAN: &str = include_str!("fixtures/pyright-clean.json");

    fn root() -> &'static Path {
        Path::new("/work/fixture")
    }

    const EXPECTED: [&str; 5] = [
        "pkg/lib.py:5:12 reportReturnType Type \"Literal['forty-two']\" is not assignable to return type \"int\"\n    \"Literal['forty-two']\" is not assignable to \"int\"",
        "pkg/use.py:1:28 reportAttributeAccessIssue \"missing_fn\" is unknown import symbol",
        "pkg/use.py:3:10 reportAssignmentType Type \"str\" is not assignable to declared type \"int\"\n    \"str\" is not assignable to \"int\"",
        "pkg/use.py:3:16 reportArgumentType Argument of type \"Literal[3]\" cannot be assigned to parameter \"name\" of type \"str\" in function \"greet\"\n    \"Literal[3]\" is not assignable to \"str\"",
        "pkg/use.py:4:5 reportUndefinedVariable \"undefined_name\" is not defined",
    ];

    #[test]
    fn errors_only_from_basedpyright_and_pyright_relative_and_from_one() {
        assert!(BASED.contains("\"severity\": \"warning\""));
        for (name, out) in [("basedpyright", BASED), ("pyright", PYRIGHT)] {
            let errors = parse_pyright(root(), out, Some(1)).unwrap_or_else(|| panic!("{name}"));
            let shown: Vec<String> = errors.iter().map(format_error).collect();
            assert_eq!(shown, EXPECTED, "{name}");
        }
        assert_eq!(parse_pyright(root(), CLEAN, Some(0)), Some(vec![]));
    }

    #[test]
    fn a_file_outside_the_project_is_dropped() {
        let out = PYRIGHT.replace("/work/fixture/pkg/lib.py", "/usr/lib/python3/site.py");
        let errors = parse_pyright(root(), &out, Some(1)).unwrap();
        assert_eq!(errors.len(), 4);
        assert!(errors.iter().all(|e| e.file.as_deref() == Some("pkg/use.py")));
        // A sibling folder that only starts with the root's name isn't inside it.
        let out = PYRIGHT.replace("/work/fixture/pkg/lib.py", "/work/fixture-other/lib.py");
        assert_eq!(parse_pyright(root(), &out, Some(1)).unwrap().len(), 4);
    }

    #[test]
    fn text_before_the_json_is_skipped_anything_else_is_unreadable() {
        let warned = format!("(node:12) Warning: something\nNo configuration file found.\n{PYRIGHT}");
        assert_eq!(parse_pyright(root(), &warned, Some(1)).unwrap().len(), 5);
        assert_eq!(parse_pyright(root(), &format!("{PYRIGHT}\nSegmentation fault\n"), Some(1)), None);
        assert_eq!(parse_pyright(root(), "", Some(0)), None);
        assert_eq!(parse_pyright(root(), "Error: something broke\n", Some(1)), None);
        // Cut off mid-list.
        assert_eq!(parse_pyright(root(), &PYRIGHT[..PYRIGHT.len() / 2], Some(1)), None);
    }

    #[test]
    fn exit_codes_other_than_0_and_1_and_disagreeing_counts_are_unreadable() {
        // 3: a config file it couldn't parse (the JSON then lists nothing); 2: a fatal error.
        let config = format!("Config file \"/work/fixture/pyrightconfig.json\" could not be parsed.\n{CLEAN}");
        assert_eq!(parse_pyright(root(), &config, Some(3)), None);
        assert_eq!(parse_pyright(root(), PYRIGHT, Some(2)), None);
        assert_eq!(parse_pyright(root(), PYRIGHT, None), None);
        // Errors listed with a clean exit, or a failed exit with none.
        assert_eq!(parse_pyright(root(), PYRIGHT, Some(0)), None);
        assert_eq!(parse_pyright(root(), CLEAN, Some(1)), None);
        let miscounted = PYRIGHT.replace("\"errorCount\": 5", "\"errorCount\": 6");
        assert_eq!(parse_pyright(root(), &miscounted, Some(1)), None);
    }

    #[test]
    fn a_syntax_error_has_no_rule_and_long_messages_are_cut() {
        let out = PYRIGHT.replace("\"rule\": \"reportUndefinedVariable\"", "\"rule\": null");
        let errors = parse_pyright(root(), &out, Some(1)).unwrap();
        assert_eq!(errors[4].code, "");
        let long = PYRIGHT.replace("is unknown import symbol", &"é".repeat(600));
        let errors = parse_pyright(root(), &long, Some(1)).unwrap();
        assert_eq!(errors[1].message.chars().count(), super::super::cargo::MAX_MESSAGE + 1);
        assert!(errors[1].message.ends_with('…'));
    }

    #[test]
    fn the_key_is_file_rule_and_message_not_the_line() {
        let errors = parse_pyright(root(), PYRIGHT, Some(1)).unwrap();
        let moved = Diagnostic { line: Some(40), column: Some(1), ..errors[1].clone() };
        assert_eq!(Pyright.key(&errors[1]), Pyright.key(&moved));
        assert_ne!(Pyright.key(&errors[2]), Pyright.key(&errors[3]));
    }

    #[test]
    fn the_note_is_marvs_sentence_then_the_errors_fenced() {
        let errors = parse_pyright(root(), PYRIGHT, Some(1)).unwrap();
        let note = pyright_note(&errors[..1]);
        assert!(
            note.starts_with(
                "(Automatic note from Marv, the program running you) pyright found 1 error that wasn't in the previous check."
            ),
            "{note}"
        );
        assert!(note.find("pkg/lib.py:5:12").unwrap() > note.find("<untrusted-data").unwrap());
        let within = pyright_note_within(&errors, 2);
        assert!(within.contains("found 5 errors") && within.contains("\nand 3 more.\n"), "{within}");
        assert!(!within.contains("reportAssignmentType"), "{within}");
    }

    #[test]
    fn the_command_tries_the_projects_own_checker_first_inside_the_sandbox() {
        let command = check_command().replace("'\\''", "'");
        let order: Vec<usize> = [
            "[ -x .venv/bin/basedpyright ]",
            "[ -x node_modules/.bin/pyright ]",
            "command -v basedpyright",
            "command -v pyright",
            "else exit 127",
        ]
        .iter()
        .map(|part| command.find(part).unwrap_or_else(|| panic!("{part}: {command}")))
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{command}");
        assert_eq!(command.matches("--outputjson").count(), 4, "{command}");
        assert!(command.contains("nice -n 10"), "{command}");
    }

    /// The command picks the project's own checker, then PATH's, with a stand-in shell environment.
    #[test]
    fn choosing_between_the_projects_checker_and_paths() {
        let dir = crate::checks::Scratch::new();
        let bin = dir.0.join("pathbin");
        let script = |name: &str| format!("#!/bin/sh\necho {name}\n");
        let exe = |p: &Path, body: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
            std::fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        };
        let project = dir.0.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let run = || {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(check_command())
                .current_dir(&project)
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .output()
                .unwrap();
            (String::from_utf8_lossy(&out.stdout).trim().to_string(), out.status.code())
        };
        assert_eq!(run(), (String::new(), Some(127)));
        assert!(Pyright.not_installed("", Some(127)));
        exe(&bin.join("pyright"), &script("path-pyright"));
        assert_eq!(run().0, "path-pyright");
        exe(&bin.join("basedpyright"), &script("path-basedpyright"));
        assert_eq!(run().0, "path-basedpyright");
        exe(&project.join("node_modules/.bin/pyright"), &script("project-pyright"));
        assert_eq!(run().0, "project-pyright");
        exe(&project.join(".venv/bin/basedpyright"), &script("project-basedpyright"));
        assert_eq!(run().0, "project-basedpyright");
    }

    #[test]
    fn it_applies_with_a_python_marker_at_the_root() {
        let dir = crate::checks::Scratch::new();
        assert!(!Pyright.prepare(&dir.0));
        dir.put("src/pyproject.toml", "");
        assert!(!Pyright.prepare(&dir.0));
        for marker in MARKERS {
            let one = crate::checks::Scratch::new();
            one.put(marker, "");
            assert!(Pyright.prepare(&one.0), "{marker}");
        }
    }

    #[test]
    fn hidden_interpreters_are_named_a_plain_not_found_is_quiet() {
        assert_eq!(missing("env: ‘node’: No such file or directory\n", Some(127)).as_deref(), Some("node"));
        let home = std::env::var("HOME").unwrap();
        let bad = format!(
            "bash: line 1: .venv/bin/basedpyright: {home}/.local/share/uv/python/cpython-3.12-linux/bin/python3.12: bad interpreter: No such file or directory\n"
        );
        assert_eq!(missing(&bad, Some(126)).as_deref(), Some("~/.local/share/uv/python/cpython-3.12-linux/bin/python3.12"));
        assert_eq!(
            missing("bash: line 1: .venv/bin/basedpyright: cannot execute: required file not found\n", Some(127)).as_deref(),
            Some("the interpreter of .venv/bin/basedpyright")
        );
        assert_eq!(missing(&bad, Some(1)), None);
        assert_eq!(missing("bash: line 1: pyright: command not found\n", Some(127)), None);
        assert_eq!(missing("", Some(127)), None);
        assert!(!Pyright.not_installed("env: ‘node’: No such file or directory\n", Some(127)));
    }

    #[test]
    fn a_venv_python_linking_into_a_hidden_folder_is_found_by_reading_links() {
        let dir = crate::checks::Scratch::new();
        let home = std::env::var("HOME").unwrap();
        let hidden_dir = format!("{home}/.local/share/uv/python/cpython-3.12.15-linux-x86_64-gnu/bin/python3.12");
        let hides = |p: &Path| p.starts_with(format!("{home}/.local/share/uv"));
        assert_eq!(hidden_venv_python(&dir.0, &hides), None);
        std::fs::create_dir_all(dir.0.join(".venv/bin")).unwrap();
        // uv's layout: python -> python3 (in the venv, shown) -> the hidden interpreter.
        std::os::unix::fs::symlink("python3", dir.0.join(".venv/bin/python")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/python3", dir.0.join(".venv/bin/python3")).unwrap();
        assert_eq!(hidden_venv_python(&dir.0, &hides), None);
        std::fs::remove_file(dir.0.join(".venv/bin/python3")).unwrap();
        std::os::unix::fs::symlink(&hidden_dir, dir.0.join(".venv/bin/python3")).unwrap();
        assert_eq!(
            hidden_venv_python(&dir.0, &hides).as_deref(),
            Some("this project's Python (~/.local/share/uv/python/cpython-3.12.15-linux-x86_64-gnu/bin/python3.12)")
        );
        // The real backend agrees that uv's folder is hidden (a dot folder of the home no toolchain shows).
        assert!(Pyright.hidden(&dir.0).is_some_and(|t| t.contains("~/.local/share/uv/python/")));
        // A loop of links ends.
        std::fs::remove_file(dir.0.join(".venv/bin/python3")).unwrap();
        std::os::unix::fs::symlink("python", dir.0.join(".venv/bin/python3")).unwrap();
        assert_eq!(hidden_venv_python(&dir.0, &hides), None);
    }
}

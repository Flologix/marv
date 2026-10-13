//! Runs the project's checks inside the sandbox. Ports Marv's `src/checks/runner.ts`.
//!
//! Prints one JSON line per violation (`{check, file?, line?, message, why?}`), `{check, error, detail?}` for a check
//! that failed, and `{done: true}` last, so output that stops early (a crash, the capture limit) can be told apart.
//! A script's stdout is read as it arrives (at most 1 MB, at most 500 violations kept per check, stderr's first 16k
//! chars), 30 s per script. Pattern checks are matched in the sandbox, so a hostile pattern can only time out.
//!
//! **Port note (redesign):** Marv's runner was TypeScript source sent to `bun -e`, self-contained because the sandbox
//! can't see Marv's files. marv-rs is one binary with no TypeScript of its own: the runner is the hidden
//! `marv __run-checks <spec>` subcommand, the binary bind-mounted read-only into the sandbox
//! ([`super::kind::runner_command`]). The spec names the checks to run, and the runner loads them from the
//! (read-only) project itself with [`load_checks`], so the command stays short however long the rules are. Patterns
//! are matched with `regress` (ECMAScript semantics, as Marv's `RegExp`); `.ts` scripts still run with the project's
//! bun, found on the sandbox's `PATH`. Without bun the run ends as Marv's `bun -e` did: `bun: command not found`, exit
//! 127, which [`super::kind::ProjectChecksKind`] reports as the missing interpreter.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::load::{CheckBody, CheckRule, ProjectCheck, compile, glob_set, load_checks};

/// Per check: violations past this are dropped.
pub const MAX_VIOLATIONS_PER_CHECK: usize = 500;
/// Larger files aren't read by pattern checks.
pub const MAX_FILE_BYTES: u64 = 2_000_000;
/// A script's stdout; past it the script is killed and the check failed.
pub const MAX_SCRIPT_OUTPUT: usize = 1_000_000;
pub const MAX_STDERR_CHARS: usize = 16_384;
/// What the runner is called with: the subcommand that follows the binary.
pub const RUNNER_SUBCOMMAND: &str = "__run-checks";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSpec {
    /// The names of the checks to run (loaded from `.marv/checks/` by the runner).
    pub checks: Vec<String>,
    /// Per script check.
    pub timeout_ms: u64,
}

/// `marv __run-checks <spec>`: runs the checks in the current folder (the project, in the sandbox). Exit 127 when a
/// script check needs bun and there's none; 1 for a spec it can't read.
pub fn main(spec: &str) -> ExitCode {
    let Ok(spec) = serde_json::from_str::<RunnerSpec>(spec) else {
        eprintln!("marv __run-checks: the spec isn't valid");
        return ExitCode::from(1);
    };
    let root = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut out = std::io::stdout().lock();
    match run_checks(&root, &spec, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(RunError::NoBun) => {
            let _ = out.flush();
            eprintln!("bun: command not found");
            ExitCode::from(127)
        }
        Err(RunError::Io(_)) => ExitCode::from(1),
    }
}

#[derive(Debug)]
pub enum RunError {
    /// A script check needs bun, and it isn't on the `PATH`.
    NoBun,
    Io(std::io::Error),
}

impl From<std::io::Error> for RunError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// A violation as a check reports it (the runner adds its check's name).
#[derive(Debug, Default)]
struct Found {
    file: Option<String>,
    line: Option<u64>,
    message: String,
    why: Option<String>,
}

/// Why a check failed: `message` stays the same from run to run (it's part of what makes a problem "the same one");
/// what stderr said, which can vary, goes in `detail`.
struct Failure {
    message: String,
    detail: Option<String>,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), detail: None }
    }
}

/// The runner's body: runs each named check (in `root`), prints the JSON lines and `{"done":true}` last.
pub fn run_checks(root: &Path, spec: &RunnerSpec, out: &mut impl Write) -> Result<(), RunError> {
    let (all, _) = load_checks(root);
    let mut print = |record: Value| -> std::io::Result<()> { writeln!(out, "{record}") };
    for check in all.iter().filter(|c| spec.checks.contains(&c.name)) {
        let found = match &check.body {
            CheckBody::Pattern { rule, flags, message, why } => pattern_check(root, check, rule, flags, message, why.as_deref()),
            CheckBody::Script => script_check(root, check, spec.timeout_ms)?,
        };
        match found {
            Ok(found) => {
                for v in found.into_iter().take(MAX_VIOLATIONS_PER_CHECK) {
                    let mut record = Map::new();
                    record.insert("check".into(), json!(check.name));
                    if let Some(file) = v.file {
                        record.insert("file".into(), json!(file));
                    }
                    if let Some(line) = v.line {
                        record.insert("line".into(), json!(line));
                    }
                    record.insert("message".into(), json!(v.message));
                    if let Some(why) = v.why {
                        record.insert("why".into(), json!(why));
                    }
                    print(Value::Object(record))?;
                }
            }
            Err(failure) => {
                let mut record = json!({"check": check.name, "error": failure.message});
                if let Some(detail) = failure.detail {
                    record["detail"] = json!(detail);
                }
                print(record)?;
            }
        }
    }
    print(json!({"done": true}))?;
    out.flush()?;
    Ok(())
}

/// Whether a project-relative path is in `node_modules` or `.git` (never checked).
fn skipped(path: &str) -> bool {
    path.split('/').any(|part| part == "node_modules" || part == ".git")
}

/// The project's files (project-relative, `/`-separated, sorted) that `glob` matches and `exclude` doesn't. Symlinks
/// aren't followed, nor read: a check reads the project's own files.
fn files(root: &Path, glob: &[String], exclude: &[String]) -> Vec<String> {
    let (Some(glob), exclude) = (glob_set(glob), glob_set(exclude)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if skipped(&path) {
                continue;
            }
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() && glob.is_match(&path) && !exclude.as_ref().is_some_and(|e| e.is_match(&path)) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// A declarative check: each match of `forbid` is a violation at its line; a file without a match of `require` is
/// one.
fn pattern_check(
    root: &Path,
    check: &ProjectCheck,
    rule: &CheckRule,
    flags: &str,
    message: &str,
    why: Option<&str>,
) -> Result<Vec<Found>, Failure> {
    let pattern = match rule {
        CheckRule::Forbid(p) | CheckRule::Require(p) => p,
    };
    let regex = compile(pattern, flags).map_err(|e| Failure::new(e.to_string()))?;
    let mut found = Vec::new();
    let violation = |file: &str, line: Option<u64>| Found {
        file: Some(file.to_string()),
        line,
        message: message.to_string(),
        why: why.map(String::from),
    };
    for file in files(root, &check.glob, &check.exclude) {
        let path = root.join(&file);
        if std::fs::metadata(&path).map_or(true, |m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        if let CheckRule::Require(_) = rule {
            if regex.find(&text).is_none() {
                found.push(violation(&file, None));
            }
            continue;
        }
        let (mut line, mut at) = (1u64, 0usize);
        for m in regex.find_iter(&text) {
            line += text.as_bytes()[at..m.start()].iter().filter(|&&b| b == b'\n').count() as u64;
            at = m.start();
            found.push(violation(&file, Some(line)));
            if found.len() > MAX_VIOLATIONS_PER_CHECK {
                return Ok(found);
            }
        }
    }
    Ok(found)
}

/// A line of a script's output as a violation (`{file?, line?, message, why?}`), or `None` when it isn't one.
fn as_violation(raw: &str) -> Option<Found> {
    let Value::Object(v) = serde_json::from_str::<Value>(raw).ok()? else {
        return None;
    };
    let text = |key: &str| match v.get(key) {
        None => Some(None),
        Some(Value::String(s)) => Some(Some(s.clone())),
        Some(_) => None,
    };
    let line = match v.get("line") {
        None => None,
        // JavaScript's `typeof line === "number"`: any number; a fraction or a negative can't be a line, so it's dropped.
        Some(Value::Number(n)) => n.as_u64(),
        Some(_) => return None,
    };
    Some(Found { message: text("message")??, file: text("file")?, line, why: text("why")? })
}

/// The line that says what went wrong: the first that mentions an error (bun ends a crash with its version), else
/// the last.
fn last_line(text: &str) -> String {
    let lines: Vec<&str> = text.trim().lines().filter(|l| !l.trim().is_empty()).collect();
    let line = lines.iter().find(|l| l.to_lowercase().contains("error")).or(lines.last()).copied().unwrap_or("");
    line.chars().take(200).collect()
}

/// A `.ts` check, run with bun from the project root; it prints violations as JSON lines.
fn script_check(root: &Path, check: &ProjectCheck, timeout_ms: u64) -> Result<Result<Vec<Found>, Failure>, RunError> {
    let mut child = match Command::new("bun")
        .arg(&check.file)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunError::NoBun),
        Err(e) => return Ok(Err(Failure::new(format!("couldn't start: {e}")))),
    };
    let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        return Ok(Err(Failure::new("couldn't start: no output pipes")));
    };
    // Read as it arrives, never more than MAX_SCRIPT_OUTPUT: a script printing in a loop for its 30 s would otherwise
    // fill memory. Only the violations that will be printed are kept.
    let too_much = Arc::new(AtomicBool::new(false));
    let reader = {
        let too_much = too_much.clone();
        std::thread::spawn(move || {
            let mut violations: Vec<Found> = Vec::new();
            let mut not_violation: Option<String> = None;
            let mut reader = BufReader::new(stdout);
            let (mut bytes, mut raw) = (0usize, Vec::new());
            loop {
                raw.clear();
                // A line is read in pieces no larger than what's left of the limit, so one endless line can't either.
                let left = (MAX_SCRIPT_OUTPUT + 1).saturating_sub(bytes) as u64;
                match (&mut reader).take(left).read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => bytes += n,
                }
                if bytes > MAX_SCRIPT_OUTPUT {
                    too_much.store(true, Ordering::SeqCst);
                    break;
                }
                let line = String::from_utf8_lossy(&raw);
                if line.trim().is_empty() || not_violation.is_some() {
                    continue;
                }
                match as_violation(line.trim_end_matches(['\n', '\r'])) {
                    Some(v) if violations.len() < MAX_VIOLATIONS_PER_CHECK => violations.push(v),
                    Some(_) => {}
                    None => not_violation = Some(line.trim_end().chars().take(200).collect()),
                }
            }
            (violations, not_violation)
        })
    };
    // Only stderr's start is kept (where bun says what went wrong); the rest is read and dropped.
    let errors = std::thread::spawn(move || {
        let (mut kept, mut chunk) = (Vec::new(), [0u8; 8192]);
        while let Ok(n) = stderr.read(&mut chunk) {
            if n == 0 {
                break;
            }
            if kept.len() < MAX_STDERR_CHARS * 4 {
                kept.extend_from_slice(&chunk[..n]);
            }
        }
        String::from_utf8_lossy(&kept).chars().take(MAX_STDERR_CHARS).collect::<String>()
    });
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if too_much.load(Ordering::SeqCst) {
            let _ = child.kill();
            break child.wait().ok();
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (violations, not_violation) = reader.join().unwrap_or_default();
    let stderr = errors.join().unwrap_or_default();
    if timed_out {
        return Ok(Err(Failure::new(format!("timed out after {} s", timeout_ms as f64 / 1000.0))));
    }
    if too_much.load(Ordering::SeqCst) {
        return Ok(Err(Failure::new(format!("printed more than {} MB", MAX_SCRIPT_OUTPUT / 1_000_000))));
    }
    let code = status.and_then(|s| s.code());
    if code != Some(0) {
        let code = code.map_or_else(|| "null".to_string(), |c| c.to_string());
        let detail = (!stderr.trim().is_empty()).then(|| last_line(&stderr));
        return Ok(Err(Failure { message: format!("exited with code {code}"), detail }));
    }
    if let Some(line) = not_violation {
        return Ok(Err(Failure::new(format!("printed a line that isn't a violation ({{file, line, message, why}}): {line}"))));
    }
    Ok(Ok(violations))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::Scratch;
    use crate::checks::kind::parse_runner;
    use crate::checks::load::Violation;

    const NO_DEFAULT: &str =
        "glob: 'src/**/*.ts'\nforbid: 'export default'\nmessage: no default exports\nwhy: named exports are easier to find\n";

    /// Runs every check in `dir`, the way the session does but in-process and outside the sandbox (the logic, not the
    /// confinement, is tested here).
    fn run_all(dir: &Path) -> Option<Vec<Violation>> {
        let names = load_checks(dir).0.into_iter().map(|c| c.name).collect();
        let mut out = Vec::new();
        run_checks(dir, &RunnerSpec { checks: names, timeout_ms: 60_000 }, &mut out).unwrap();
        parse_runner(&String::from_utf8(out).unwrap(), Some(0))
    }

    fn v(check: &str, file: &str, line: Option<u64>, message: &str, why: Option<&str>) -> Violation {
        Violation {
            check: check.into(),
            file: Some(file.into()),
            line,
            message: message.into(),
            why: why.map(String::from),
            failed: false,
        }
    }

    fn bun() -> bool {
        Command::new("bun").arg("--version").stdout(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    #[test]
    fn forbidden_at_each_matchs_line_required_when_a_file_lacks_it_excluded_files_arent_read() {
        let dir = Scratch::new();
        dir.put(".marv/checks/no-default.yaml", NO_DEFAULT);
        dir.put(
            ".marv/checks/header.yaml",
            "glob: 'src/**/*.ts'\nexclude: 'src/gen/**'\nrequire: '^// header'\nflags: m\nmessage: no header\n",
        );
        dir.put("src/a.ts", "// header\nexport const a = 1;\n\nexport default a;\n");
        dir.put("src/b.ts", "export default 2;\n");
        dir.put("src/gen/c.ts", "export default 3;\n");
        dir.put("README.md", "export default\n");
        dir.put("node_modules/x/src/d.ts", "export default 4;\n");
        let why = Some("named exports are easier to find");
        assert_eq!(
            run_all(&dir.0).unwrap(),
            vec![
                v("header", "src/b.ts", None, "no header", None),
                v("no-default", "src/a.ts", Some(4), "no default exports", why),
                v("no-default", "src/b.ts", Some(1), "no default exports", why),
                v("no-default", "src/gen/c.ts", Some(1), "no default exports", why),
            ]
        );
    }

    #[test]
    fn several_matches_on_one_line_and_lines_after() {
        let dir = Scratch::new();
        dir.put(".marv/checks/todo.yaml", "glob: '*.txt'\nforbid: 'TODO'\nmessage: m\n");
        dir.put("a.txt", "TODO TODO\n\n\nTODO\n");
        let lines: Vec<_> = run_all(&dir.0).unwrap().into_iter().map(|v| v.line.unwrap()).collect();
        assert_eq!(lines, [1, 1, 4]);
    }

    #[test]
    fn regular_expressions_and_messages_with_quotes_survive() {
        let dir = Scratch::new();
        dir.put(
            ".marv/checks/quotes.yaml",
            "glob: \"*.txt\"\nforbid: \"it's \\\"here\\\"\"\nmessage: \"don't say it's \\\"here\\\"\"\n",
        );
        dir.put("a.txt", "it's \"here\"\n");
        assert_eq!(run_all(&dir.0).unwrap(), vec![v("quotes", "a.txt", Some(1), "don't say it's \"here\"", None)]);
    }

    #[test]
    fn only_the_named_checks_run_and_the_report_ends_with_done() {
        let dir = Scratch::new();
        dir.put(".marv/checks/a.yaml", "glob: '*.txt'\nforbid: x\nmessage: a\n");
        dir.put(".marv/checks/b.yaml", "glob: '*.txt'\nforbid: x\nmessage: b\n");
        dir.put("f.txt", "x\n");
        let mut out = Vec::new();
        run_checks(&dir.0, &RunnerSpec { checks: vec!["b".into()], timeout_ms: 1000 }, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"check\":\"b\",\"file\":\"f.txt\",\"line\":1,\"message\":\"b\"}\n{\"done\":true}\n"
        );
    }

    #[test]
    fn past_500_violations_the_rest_are_dropped() {
        let dir = Scratch::new();
        dir.put(".marv/checks/x.yaml", "glob: '*.txt'\nforbid: x\nmessage: m\n");
        dir.put("a.txt", &"x\n".repeat(600));
        assert_eq!(run_all(&dir.0).unwrap().len(), 500);
    }

    #[test]
    fn script_output_lines_are_read_as_violations() {
        assert!(as_violation(r#"{"file":"src/a.ts","line":2,"message":"bad","why":"because"}"#).is_some());
        assert!(as_violation(r#"{"message":"project-wide"}"#).is_some());
        assert!(as_violation("checking…").is_none());
        assert!(as_violation(r#"{"file":3,"message":"m"}"#).is_none());
        assert!(as_violation(r#"{"line":"2","message":"m"}"#).is_none());
        assert_eq!(last_line("at x\nError: boom\nBun v1.2\n"), "Error: boom");
        assert_eq!(last_line("a\nb\n"), "b");
    }

    #[test]
    fn scripts_json_lines_become_violations_and_a_crash_or_chatter_is_a_failed_check() {
        if !bun() {
            eprintln!("skipped: no bun");
            return;
        }
        let dir = Scratch::new();
        dir.put(
            ".marv/checks/good.ts",
            "console.log(JSON.stringify({ file: \"src/a.ts\", line: 2, message: \"bad\", why: \"because\" }));\nconsole.log(JSON.stringify({ message: \"project-wide\" }));\n",
        );
        dir.put(".marv/checks/crash.ts", "throw new Error(\"boom\");\n");
        dir.put(".marv/checks/chatty.ts", "console.log(\"checking…\");\n");
        let found = run_all(&dir.0).unwrap();
        assert!(found.contains(&v("good", "src/a.ts", Some(2), "bad", Some("because"))));
        assert!(found.iter().any(|f| f.check == "good" && f.message == "project-wide" && f.file.is_none()));
        let crash = found.iter().find(|f| f.check == "crash").unwrap();
        assert_eq!(crash.message, "the check failed: exited with code 1");
        assert!(crash.failed && crash.why.as_deref().unwrap().contains("boom"));
        let chatty = found.iter().find(|f| f.check == "chatty").unwrap();
        assert!(chatty.failed && chatty.message.contains("printed a line that isn't a violation"));
    }

    #[test]
    fn a_script_that_keeps_failing_is_the_same_problem_each_time_however_its_stderr_varies() {
        if !bun() {
            eprintln!("skipped: no bun");
            return;
        }
        let dir = Scratch::new();
        dir.put(".marv/checks/flaky.ts", "console.error(`at ${Date.now()}.${Math.random()}`);\nprocess.exit(3);\n");
        let first = run_all(&dir.0).unwrap().remove(0);
        let second = run_all(&dir.0).unwrap().remove(0);
        assert_eq!(first.message, "the check failed: exited with code 3");
        assert!(first.why.as_deref().unwrap().starts_with("its stderr ended: at "));
        assert_eq!(crate::checks::kind::violation_key(&first), crate::checks::kind::violation_key(&second));
    }

    #[test]
    fn a_scripts_output_is_bounded() {
        if !bun() {
            eprintln!("skipped: no bun");
            return;
        }
        let dir = Scratch::new();
        dir.put(
            ".marv/checks/flood.ts",
            "const line = JSON.stringify({ message: \"x\".repeat(1000) });\nfor (let i = 0; i < 2000; i++) console.log(line);\n",
        );
        dir.put(
            ".marv/checks/many.ts",
            "for (let i = 0; i < 600; i++) console.log(JSON.stringify({ file: `f${i}.ts`, message: \"m\" }));\n",
        );
        let found = run_all(&dir.0).unwrap();
        let flood: Vec<_> = found.iter().filter(|v| v.check == "flood").collect();
        assert_eq!(flood.len(), 1);
        assert_eq!(flood[0].message, "the check failed: printed more than 1 MB");
        assert_eq!(found.iter().filter(|v| v.check == "many").count(), 500);
    }

    #[test]
    fn a_script_past_its_timeout_is_a_failed_check() {
        if !bun() {
            eprintln!("skipped: no bun");
            return;
        }
        let dir = Scratch::new();
        dir.put(".marv/checks/slow.ts", "await Bun.sleep(5000);\n");
        let mut out = Vec::new();
        run_checks(&dir.0, &RunnerSpec { checks: vec!["slow".into()], timeout_ms: 300 }, &mut out).unwrap();
        let found = parse_runner(&String::from_utf8(out).unwrap(), Some(0)).unwrap();
        assert_eq!(found[0].message, "the check failed: timed out after 0.3 s");
    }
}

//! `docs/SECURITY.md` "Sandbox": pyright after the model's edits (#187, `marv-session`'s `diagnostics::pyright`) runs
//! the project's own checker when it has one (`.venv/bin/basedpyright`, `node_modules/.bin/pyright`) and a Python
//! interpreter the project's config can choose, so it runs only in the real sandbox: the project read-only, no
//! network, the hidden home hidden, nothing writable. These run in the real sandbox (bubblewrap on Linux, Seatbelt
//! on macOS); the one with a real checker skips where neither basedpyright nor pyright is installed.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use marv_guards::TempDir;
use marv_session::diagnostics::Diagnostic;
use marv_session::diagnostics::pyright::Pyright;
use marv_session::diagnostics::turn::{
    CheckOutcome, CheckRun, Checker, CommandResult, RunCheck, TurnChecks, TurnOptions, sandboxed, sandboxed_options,
};
use tokio_util::sync::CancellationToken;

const PYPROJECT: &str = "[project]\nname = \"marv_guard_fixture\"\nversion = \"0.1.0\"\n";
const LIB: &str = "def answer() -> int:\n    return 42\n";
const USE_CLEAN: &str = "from pkg.lib import answer\n\nx: int = answer()\n";
const USE_BROKEN: &str = "from pkg.lib import answer\n\nx: str = answer()\n";

/// Whether this machine has basedpyright or pyright on PATH (the test's own look; Marv never looks on the host).
fn host_has_pyright() -> bool {
    ["basedpyright", "pyright"]
        .iter()
        .any(|bin| std::process::Command::new(bin).arg("--version").output().is_ok_and(|o| o.status.success()))
}

/// The real runner, keeping each run and its result.
fn recording(seen: &Arc<Mutex<Vec<(CheckRun, CommandResult)>>>) -> RunCheck {
    let (kept, real) = (seen.clone(), sandboxed());
    Arc::new(move |run: CheckRun, cancel| {
        let (kept, future) = (kept.clone(), real(run.clone(), cancel));
        Box::pin(async move {
            let result = future.await;
            kept.lock().unwrap().push((run, result.clone()));
            result
        })
    })
}

/// The Python checker on `project`, in the real sandbox, with a long timeout (a cold pyright on a loaded runner).
fn turn(project: &Path, run: RunCheck) -> (TurnChecks, Arc<Mutex<Vec<CheckOutcome<Diagnostic>>>>) {
    let mut checker = Checker::new(Arc::new(Pyright), Some(run));
    checker.timeout = Duration::from_secs(300);
    let told: Arc<Mutex<Vec<CheckOutcome<Diagnostic>>>> = Default::default();
    let kept = told.clone();
    let checks = Arc::new(checker)
        .turn(TurnOptions {
            root: project.to_path_buf(),
            sandbox: true,
            cancel: CancellationToken::new(),
            on_status: Arc::new(|_| {}),
            on_result: Arc::new(move |r| kept.lock().unwrap().push(r)),
        })
        .expect("a Python project gets pyright");
    (checks, told)
}

/// A step that edited `path`.
fn edited(path: &str) -> marv_agent::ToolRun {
    let mut run =
        marv_agent::ToolRun::new(marv_core::ToolResult { output: "Edited".into(), summary: None, is_error: false }, path);
    run.changed = Some(path.into());
    run.display = Some(Arc::new(marv_session::runner::ToolDisplay { diff: Some(Default::default()), plan: None }));
    run
}

fn project() -> TempDir {
    let project = TempDir::new("pyright-check");
    project.write("pyproject.toml", PYPROJECT);
    project.write("pkg/__init__.py", "");
    project.write("pkg/lib.py", LIB);
    project.write("pkg/use.py", USE_CLEAN);
    project
}

/// **pyright tells an edit's new error once, by its path in the project, from the sandbox.** Baseline clean, an
/// edit adds a type error (told), the same again (nothing), the fix (nothing). Every run is the guarded one: the
/// project read-only, no network, nothing writable.
///
/// Proven: made `parse_pyright` keep absolute paths → failed (the note named the project's temp folder).
#[tokio::test(flavor = "multi_thread")]
async fn pyright_tells_a_new_error_once_from_the_sandbox() {
    marv_guards::require_sandbox!();
    if !host_has_pyright() {
        eprintln!("skipped: no basedpyright or pyright here");
        return;
    }
    let project = project();
    let seen: Arc<Mutex<Vec<(CheckRun, CommandResult)>>> = Default::default();
    let (checks, told) = turn(project.path(), recording(&seen));
    checks.before_change.before_change().await;
    let runs = || seen.lock().unwrap().clone();
    assert!(told.lock().unwrap().is_empty(), "{:?} {:?}", told.lock().unwrap(), runs());

    project.write("pkg/use.py", USE_BROKEN);
    let note = (checks.after_step)(vec![edited("pkg/use.py")]).await;
    let note = note.unwrap_or_else(|| panic!("nothing told: {:?} {:?}", told.lock().unwrap(), runs()));
    assert!(note.contains("pyright found 1 error that wasn't in the previous check."), "{note}");
    assert!(note.contains("\npkg/use.py:3:10 reportAssignmentType "), "{note}");
    assert!(!note.contains(&project.path().display().to_string()), "{note}");
    assert_eq!((checks.after_step)(vec![edited("pkg/use.py")]).await, None, "told twice");
    project.write("pkg/use.py", USE_CLEAN);
    assert_eq!((checks.after_step)(vec![edited("pkg/use.py")]).await, None, "a fix was told");

    for (run, _) in runs() {
        let options = sandboxed_options(&run);
        assert!(options.sandbox && options.project_read_only && !options.network, "not the guarded run");
        assert!(options.writable.is_empty(), "{:?}", options.writable);
    }
}

/// **A project's own `.venv/bin/basedpyright` runs only sandboxed** (it's chosen first, and it's the project's
/// code): it can't write into the project or the real home, can't read a hidden home file, and can't reach a
/// listener on the host's loopback; and what it prints is still read as a check. Needs no real pyright.
///
/// Proven (Linux): made `sandboxed_options` drop `project_read_only` and set `network: true` → failed (its report
/// said "project: ALLOWED" and "loopback: ALLOWED").
#[tokio::test(flavor = "multi_thread")]
async fn a_projects_own_checker_runs_sandboxed() {
    marv_guards::require_sandbox!();
    let project = project();
    let real_home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
    let in_home = real_home.join(format!("marv-guard-pyright-planted-{}", std::process::id()));
    let secret_dir = real_home.join(format!(".marv-guard-pyright-{}", std::process::id()));
    std::fs::create_dir_all(&secret_dir).unwrap();
    let secret = secret_dir.join("secret");
    std::fs::write(&secret, "HIDDEN-HOME-SECRET\n").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let planted = project.path().join("planted.txt");
    let clean = r#"{"version": "x", "generalDiagnostics": [], "summary": {"filesAnalyzed": 1, "errorCount": 0, "warningCount": 0, "informationCount": 0, "timeInSec": 0.1}}"#;
    // POSIX sh only: `/dev/tcp` is bash's, so the loopback attempt goes through python3 when there is one.
    project.write(
        ".venv/bin/basedpyright",
        &format!(
            "#!/bin/sh\n\
             (echo pwned > '{planted}') 2>/dev/null && echo 'project: ALLOWED' >&2 || echo 'project: denied' >&2\n\
             (echo pwned > '{home}') 2>/dev/null && echo 'home: written' >&2 || echo 'home: denied' >&2\n\
             cat '{secret}' >/dev/null 2>&1 && echo 'secret: ALLOWED' >&2 || echo 'secret: denied' >&2\n\
             if command -v python3 >/dev/null 2>&1; then\n\
               python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {port}), 3)' 2>/dev/null \
                 && echo 'loopback: ALLOWED' >&2 || echo 'loopback: denied' >&2\n\
             fi\n\
             echo '{clean}'\n",
            planted = planted.display(),
            home = in_home.display(),
            secret = secret.display(),
        ),
    );
    std::fs::set_permissions(project.path().join(".venv/bin/basedpyright"), std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .unwrap();
    let seen: Arc<Mutex<Vec<(CheckRun, CommandResult)>>> = Default::default();
    let (checks, told) = turn(project.path(), recording(&seen));
    checks.before_change.before_change().await;
    let runs = seen.lock().unwrap().clone();
    let (_, result) = runs.last().expect("the baseline ran");
    let _ = std::fs::remove_dir_all(&secret_dir);
    assert!(result.output.contains("project: denied"), "{result:?}");
    assert!(result.output.contains("secret: denied"), "{result:?}");
    assert!(!result.output.contains("ALLOWED"), "{result:?}");
    assert!(!planted.exists(), "the project's checker wrote into the project");
    // The home is the sandbox's throwaway stand-in under bwrap (a write there can succeed inside), refused outright
    // under Seatbelt: either way, nothing reaches the host.
    assert!(!in_home.exists(), "the project's checker wrote into the real home");
    let _ = std::fs::remove_file(&in_home);
    // Its output was read as a clean check: nothing told, nothing failed.
    assert!(told.lock().unwrap().is_empty(), "{:?} {runs:?}", told.lock().unwrap());
    let options = sandboxed_options(&runs[0].0);
    assert!(options.sandbox && options.project_read_only && !options.network && options.writable.is_empty());
}

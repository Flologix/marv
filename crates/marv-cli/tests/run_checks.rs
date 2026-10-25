//! `marv __run-checks` in the real sandbox, as the session runs it (`marv_session::diagnostics::turn::sandboxed`): the
//! binary mounted read-only, the project read-only, no network. Skipped where bwrap doesn't work.

use std::path::{Path, PathBuf};

use marv_session::checks::kind::{parse_runner, runner_command};
use marv_session::checks::load_checks;
use marv_session::diagnostics::turn::{CHECK_TIMEOUT, CheckRun, sandboxed};
use tokio_util::sync::CancellationToken;

fn bwrap_works() -> bool {
    std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-net", "true"])
        .status()
        .is_ok_and(|s| s.success())
}

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("marv-run-checks-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn put(&self, path: &str, content: &str) {
        let full = self.0.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn run_all(root: &Path) -> (String, Option<Vec<marv_session::checks::Violation>>) {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_marv"));
    let (checks, problems) = load_checks(root);
    assert!(problems.is_empty(), "{problems:?}");
    let run = CheckRun {
        command: runner_command(&binary, &checks, 30_000),
        root: root.to_path_buf(),
        read_only: vec![binary],
        writable: vec![],
        timeout: CHECK_TIMEOUT,
    };
    let result = sandboxed()(run, CancellationToken::new()).await;
    let parsed = parse_runner(&result.output, result.exit_code);
    (result.output, parsed)
}

#[tokio::test]
async fn the_runner_runs_in_the_sandbox_and_cant_change_the_project() {
    if !bwrap_works() {
        eprintln!("skipped: no working bwrap");
        return;
    }
    let project = Project::new();
    project.put(".marv/checks/no-default.yaml", "glob: 'src/**/*.ts'\nforbid: 'export default'\nmessage: no default exports\n");
    project.put("src/a.ts", "export const a = 1;\n\nexport default a;\n");
    let (output, found) = run_all(&project.0).await;
    let found = found.unwrap_or_else(|| panic!("unreadable: {output}"));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!((found[0].file.as_deref(), found[0].line), (Some("src/a.ts"), Some(3)));

    // A script check: it may read, but neither write the project nor reach the network.
    let has_bun = std::process::Command::new("bun").arg("--version").output().is_ok_and(|o| o.status.success());
    if !has_bun {
        eprintln!("skipped the script check: no bun");
        return;
    }
    project.put(
        ".marv/checks/escape.ts",
        r#"import { writeFileSync } from "node:fs";
let wrote = "no";
try { writeFileSync("planted.txt", "x"); wrote = "yes"; } catch {}
let net = "no";
try { await fetch("http://1.1.1.1", { signal: AbortSignal.timeout(2000) }); net = "yes"; } catch {}
console.log(JSON.stringify({ message: `wrote=${wrote} net=${net}` }));
"#,
    );
    let (output, found) = run_all(&project.0).await;
    if output.contains("bun: command not found") {
        eprintln!("skipped the script check: bun isn't visible in the sandbox");
        return;
    }
    let found = found.unwrap_or_else(|| panic!("unreadable: {output}"));
    let escape = found.iter().find(|v| v.check == "escape").unwrap_or_else(|| panic!("{found:?}"));
    assert_eq!(escape.message, "wrote=no net=no", "{found:?}");
    assert!(!project.0.join("planted.txt").exists());
}

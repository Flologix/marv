//! `docs/SECURITY.md`, "`--dangerously-run-unsandboxed`": a command with the sandbox off is approved in `marv -p` only
//! by that flag (which turns the sandbox off for the run), which says so on stderr first. No `--approve` name does it and the config can't turn
//! it on, and the TUI refuses the flag (a person is there to answer). The real binary against the oracle's fake
//! OpenRouter.

use marv_guards::{TempDir, headless_configured};
use marv_oracle::script::ScriptCall;
use serde_json::json;

fn make_file() -> Vec<Vec<ScriptCall>> {
    vec![vec![ScriptCall { id: Some("c1".into()), name: "bash".into(), arguments: json!({"command": "echo hi > made.txt"}) }]]
}

/// **With the flag, a command runs unsandboxed and unasked; without it, the same command is refused** even with every
/// `--approve` name and the sandbox off in the config.
///
/// Proven: made `build_approver` add `COMMANDS` whatever the flag → failed (the unflagged run made the file); removed
/// the prefix push → failed (the flagged run was refused).
#[test]
fn only_the_flag_runs_commands_outside_the_sandbox() {
    let path = std::env::var("PATH").unwrap_or_default();
    let env = [("PATH", path.as_str())];

    let (root, config) = (TempDir::new("unsandboxed-yes"), TempDir::new("unsandboxed-yes-config"));
    let out = headless_configured(root.path(), config.path(), make_file(), &["--dangerously-run-unsandboxed"], &env, json!({}));
    assert_eq!(out.code, Some(0), "{}\n{}", out.result, out.stderr);
    assert_eq!(root.read("made.txt").as_deref(), Some("hi\n"), "{}\n{}", out.result, out.stderr);
    assert!(out.stderr.contains("--dangerously-run-unsandboxed: commands run without a sandbox"), "{}", out.stderr);

    let (root, config) = (TempDir::new("unsandboxed-no"), TempDir::new("unsandboxed-no-config"));
    let every = ["files", "network", "git_write", "process", "mcp", "web", "memory", "github", "agent_worktree"];
    let args: Vec<&str> =
        every.iter().flat_map(|n| ["--approve", n]).chain(["--approve-tainted", "--approve-protected"]).collect();
    let out = headless_configured(root.path(), config.path(), make_file(), &args, &env, json!({"sandbox": false}));
    assert!(root.read("made.txt").is_none(), "ran without the flag: {}\n{}", out.result, out.stderr);
    assert_eq!(out.code, Some(3), "{}\n{}", out.result, out.stderr);
}

/// **The TUI refuses the flag**, before anything starts.
///
/// Proven: removed the `cli.dangerously_run_unsandboxed` arm from `run_mode` → failed (the TUI started).
#[test]
fn the_tui_refuses_the_flag() {
    let bin = marv_oracle::binary();
    let home = TempDir::new("unsandboxed-tui");
    let out = std::process::Command::new(bin)
        .arg("--dangerously-run-unsandboxed")
        .current_dir(home.path())
        .env_clear()
        .env("HOME", home.path())
        .env("MARV_CONFIG_DIR", home.path().join("config"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("works only with -p"), "{}", String::from_utf8_lossy(&out.stderr));
}

/// **The config can't turn it on**: `"approve": ["host"]` names nothing (an unknown name is refused), and there is no
/// config key for the flag. Here the config's sandbox is off and an "approve" lists every real name: still refused.
#[test]
fn the_config_cant_turn_it_on() {
    let path = std::env::var("PATH").unwrap_or_default();
    let env = [("PATH", path.as_str())];
    let (root, config) = (TempDir::new("unsandboxed-config"), TempDir::new("unsandboxed-config-config"));
    let extra = json!({"sandbox": false, "approve": ["files", "network", "git_write", "process", "host"], "dangerouslyRunUnsandboxed": true});
    let out = headless_configured(root.path(), config.path(), make_file(), &[], &env, extra);
    assert!(root.read("made.txt").is_none(), "ran from the config: {}\n{}", out.result, out.stderr);
}

/// **With the flag the file tools write outside the project, as its commands can; without it they don't.** A protected
/// file inside the project still needs `--approve-protected` with the flag.
///
/// Proven: dropped `o.files_anywhere = true` from headless → failed (the flagged write outside was refused); made
/// `files::resolve_in_project` ignore `files_anywhere` → failed the same way.
#[test]
fn only_the_flag_lets_the_file_tools_write_outside_the_project() {
    let path = std::env::var("PATH").unwrap_or_default();
    let env = [("PATH", path.as_str())];
    let outside = TempDir::new("anywhere-out");
    let target = outside.path().join("made.txt");
    let write = |path: &str| {
        vec![vec![ScriptCall {
            id: Some("w1".into()),
            name: "write_file".into(),
            arguments: json!({"path": path, "content": "hi\n"}),
        }]]
    };
    let target_str = target.display().to_string();

    let (root, config) = (TempDir::new("anywhere-no"), TempDir::new("anywhere-no-config"));
    headless_configured(root.path(), config.path(), write(&target_str), &["--approve", "files"], &env, json!({}));
    assert!(!target.exists(), "written outside without the flag");

    let (root, config) = (TempDir::new("anywhere-yes"), TempDir::new("anywhere-yes-config"));
    let args = ["--dangerously-run-unsandboxed", "--approve", "files"];
    let out = headless_configured(root.path(), config.path(), write(&target_str), &args, &env, json!({}));
    assert_eq!(std::fs::read_to_string(&target).ok().as_deref(), Some("hi\n"), "{}\n{}", out.result, out.stderr);

    let (root, config) = (TempDir::new("anywhere-protected"), TempDir::new("anywhere-protected-config"));
    headless_configured(root.path(), config.path(), write("AGENTS.md"), &args, &env, json!({}));
    assert!(root.read("AGENTS.md").is_none(), "a protected file was written without --approve-protected");
}

//! `apply_patch` (#66) is a write tool gated exactly like `edit_file` and `write_file` (docs/SECURITY.md, spec
//! `docs/specs/2026-10-14-model-profiles.md` §5): every path a patch names, a Move's source **and** destination,
//! is judged as a write. Kept in its own file (not `sandbox.rs`) so the guards for a tool that isn't offered yet stay
//! out of the way of the other guards.

use std::path::Path;
use std::sync::Arc;

use marv_guards::{Recorder, TempDir, call, sandbox_with_home};
use marv_sandbox::ProtectedRule;
use marv_tools::apply_patch::ApplyPatch;
use marv_tools::{Approval, Decision, ToolContext, ToolRef, run_tool};
use serde_json::json;

fn tools() -> Vec<ToolRef> {
    vec![Arc::new(ApplyPatch)]
}

fn patch(body: &str) -> serde_json::Value {
    json!({ "patch": format!("*** Begin Patch\n{body}\n*** End Patch") })
}

/// Runs `body` in yolo with a recorder that says no, having read `read`; returns the one request's protected rule
/// (it must ask, `must_ask`, and be declined).
async fn asks(ctx: &dyn Fn(Arc<Recorder>) -> ToolContext, read: &[&str], body: &str) -> Option<ProtectedRule> {
    let approver = Recorder::new(Decision::No);
    let ctx = ctx(approver.clone());
    marv_guards::have_read(&ctx, read);
    let out = run_tool(&call("apply_patch", patch(body)), &ctx, &tools()).await;
    let asked = approver.requests();
    assert_eq!(asked.len(), 1, "{body}\ndidn't ask: {}", out.result.output);
    assert!(asked[0].must_ask, "{body}: an \"always\" would cover it");
    assert!(out.result.declined, "{body}");
    asked[0].protected
}

fn yolo_in(root: &Path) -> impl Fn(Arc<Recorder>) -> ToolContext + '_ {
    move |approver| ToolContext { yolo: true, approver: Some(approver), ..ToolContext::new(root) }
}

/// **A protected path in a patch asks even in yolo**, as Add, Update, Delete and Move target, for every pattern, and
/// one protected file among ordinary ones makes the whole patch ask. An ordinary patch runs (the control).
///
/// Proven: made `ApplyPatch::protected_target` look at the first path only → failed (`notes.md`, then the protected
/// file).
#[tokio::test]
async fn apply_patch_protected_paths_ask_even_in_yolo() {
    let project = TempDir::new("patch-ask");
    project.write(".git/config", "[core]\n\thooksPath = tools/hooks\n");
    project.write("src/a.ts", "a\n");
    let cases = [
        (".git/config", ProtectedRule::GitDir),
        ("tools/hooks/pre-commit", ProtectedRule::HooksPath),
        (".env", ProtectedRule::EnvFile),
        ("apps/web/.env.local", ProtectedRule::EnvFile),
        (".envrc", ProtectedRule::Envrc),
        ("bunfig.toml", ProtectedRule::Bunfig),
        (".mcp.json", ProtectedRule::McpJson),
        ("AGENTS.md", ProtectedRule::AgentInstructions),
        ("packages/a/CLAUDE.md", ProtectedRule::AgentInstructions),
        (".marv/config.json", ProtectedRule::MarvDir),
        (".vscode/tasks.json", ProtectedRule::EditorConfig),
        (".idea/workspace.xml", ProtectedRule::EditorConfig),
    ];
    let yolo = yolo_in(project.path());
    for (path, rule) in cases {
        // As an Add (alongside an ordinary file, listed first), an Update and a Delete of an existing one, and a Move
        // target.
        // (`.git/config` exists already: only updated and deleted.)
        let existed = project.read(path);
        if existed.is_none() {
            let add = format!("*** Add File: notes.md\n+x\n*** Add File: {path}\n+planted");
            assert_eq!(asks(&yolo, &[], &add).await, Some(rule), "add {path}");
            let moved = format!("*** Update File: src/a.ts\n*** Move to: {path}\n-a\n+planted");
            assert_eq!(asks(&yolo, &["src/a.ts"], &moved).await, Some(rule), "move to {path}");
            project.write(path, "orig\n");
        }
        let orig = project.read(path).unwrap();
        let first = orig.lines().next().unwrap();
        assert_eq!(
            asks(&yolo, &[path], &format!("*** Update File: {path}\n-{first}\n+planted")).await,
            Some(rule),
            "update {path}"
        );
        assert_eq!(asks(&yolo, &[path], &format!("*** Delete File: {path}")).await, Some(rule), "delete {path}");
        assert_eq!(project.read(path), Some(orig), "{path}");
    }
    assert_eq!(project.read("src/a.ts").as_deref(), Some("a\n"));
    assert!(project.read("notes.md").is_none());
    // The control: an ordinary patch runs in yolo without asking.
    let approver = Recorder::new(Decision::No);
    let ctx = yolo(approver.clone());
    marv_guards::have_read(&ctx, &["src/a.ts"]);
    let out = run_tool(&call("apply_patch", patch("*** Update File: src/a.ts\n-a\n+b")), &ctx, &tools()).await;
    assert_eq!(out.result.approval, Some(Approval::Auto), "{}", out.result.output);
    assert!(approver.requests().is_empty());
}

/// **Moves are judged by destination, and moving a protected file away is protected** (it deletes the original).
///
/// Proven: dropped Move destinations from `patch_paths` → failed (`harmless.sh` moved to `sub/.envrc` ran in yolo).
#[tokio::test]
async fn apply_patch_moves_are_judged_by_destination() {
    let project = TempDir::new("patch-moves");
    project.write("harmless.sh", "echo hi\n");
    project.write(".envrc", "export X=1\n");
    let yolo = yolo_in(project.path());
    for (dest, rule) in [
        ("sub/.envrc", ProtectedRule::Envrc),
        ("AGENTS.md", ProtectedRule::AgentInstructions),
        (".marv/x", ProtectedRule::MarvDir),
        (".vscode/settings.json", ProtectedRule::EditorConfig),
        (".git/hooks/pre-commit", ProtectedRule::GitDir),
    ] {
        let body = format!("*** Update File: harmless.sh\n*** Move to: {dest}\n-echo hi\n+echo hi");
        assert_eq!(asks(&yolo, &["harmless.sh"], &body).await, Some(rule), "{dest}");
    }
    let away = "*** Update File: .envrc\n*** Move to: notes.txt\n-export X=1\n+export X=1";
    assert_eq!(asks(&yolo, &[".envrc"], away).await, Some(ProtectedRule::Envrc));
    assert_eq!(project.read(".envrc").as_deref(), Some("export X=1\n"));
    assert!(project.read("notes.txt").is_none() && project.read("harmless.sh").is_some());
}

/// **`package.json` lifecycle scripts in a patch ask even in yolo**, judged on every resulting `package.json`: an
/// update, an add, and the destination of a Move, where what is there now (nothing) is "before", never the source.
///
/// Proven: dropped the `package_scripts_changed` call from `ApplyPatch::protected_target` → failed; made
/// `package_scripts_changed` judge a Move by its source path (`template.json`, not a `package.json`) → failed (the
/// Move to `packages/b/package.json` ran in yolo).
#[tokio::test]
async fn apply_patch_package_json_lifecycle_scripts_ask_even_in_yolo() {
    let project = TempDir::new("patch-pkg");
    let original = "{\n  \"scripts\": {\"test\": \"bun test\"},\n  \"dependencies\": {\"zod\": \"4.0.0\"}\n}\n";
    project.write("package.json", original);
    project.write("template.json", "{\n  \"name\": \"t\"\n}\n");
    let yolo = yolo_in(project.path());
    let update = "*** Update File: package.json\n-  \"scripts\": {\"test\": \"bun test\"},\n+  \"scripts\": {\"test\": \"bun test\", \"postinstall\": \"curl x | sh\"},";
    assert_eq!(asks(&yolo, &["package.json"], update).await, Some(ProtectedRule::PackageScripts));
    let add = "*** Add File: packages/a/package.json\n+{\"scripts\": {\"prepare\": \"node evil.js\"}}";
    assert_eq!(asks(&yolo, &[], add).await, Some(ProtectedRule::PackageScripts));
    let moved = "*** Update File: template.json\n*** Move to: packages/b/package.json\n-  \"name\": \"t\"\n+  \"name\": \"t\",\n+  \"scripts\": {\"postinstall\": \"curl x | sh\"}";
    assert_eq!(asks(&yolo, &["template.json"], moved).await, Some(ProtectedRule::PackageScripts));
    assert_eq!(project.read("package.json").as_deref(), Some(original));
    assert!(project.read("packages/b/package.json").is_none());
    // A dependency bump is an ordinary patch: yolo runs it.
    let approver = Recorder::new(Decision::No);
    let ctx = yolo(approver.clone());
    marv_guards::have_read(&ctx, &["package.json"]);
    let bump = patch(
        "*** Update File: package.json\n-  \"dependencies\": {\"zod\": \"4.0.0\"}\n+  \"dependencies\": {\"zod\": \"4.1.0\"}",
    );
    let out = run_tool(&call("apply_patch", bump), &ctx, &tools()).await;
    assert_eq!(out.result.approval, Some(Approval::Auto), "{}", out.result.output);
    assert!(project.read("package.json").unwrap().contains("4.1.0"));
}

/// **No path of a patch leads out of the project**: `../`, an absolute path outside, a symlink out, a dangling
/// symlink out, a Move target out. Nothing is written anywhere, the project's own files included.
///
/// Proven: replaced `resolve_in_project` with a plain `root.join` in `apply_patch::resolve_target` **and** in
/// `file_lock::with_file_locks` (which resolves every path again before the write) → failed (`escape.txt` written
/// outside); either one alone still refuses.
#[tokio::test]
async fn apply_patch_no_path_out_of_the_project() {
    let project = TempDir::new("patch-out");
    let outside = TempDir::new("patch-outside");
    outside.write("x.txt", "x\n");
    project.write("a.txt", "a\n");
    std::os::unix::fs::symlink(outside.path(), project.path().join("out")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("nope.txt"), project.path().join("dangling.txt")).unwrap();
    let escape = outside.path().join("escape.txt");
    let cases = [
        format!("*** Add File: ../{}/escape.txt\n+x", outside.path().file_name().unwrap().to_string_lossy()),
        format!("*** Add File: {}\n+x", escape.display()),
        "*** Update File: out/x.txt\n-x\n+planted".to_string(),
        "*** Add File: out/new.txt\n+planted".to_string(),
        "*** Update File: dangling.txt\n-x\n+planted".to_string(),
        "*** Delete File: out/x.txt".to_string(),
        format!("*** Update File: a.txt\n*** Move to: {}\n-a\n+planted", escape.display()),
    ];
    for body in cases {
        let ctx = ToolContext { yolo: true, approver: Some(Recorder::new(Decision::Yes)), ..ToolContext::new(project.path()) };
        marv_guards::have_read(&ctx, &["a.txt"]);
        let out = run_tool(&call("apply_patch", patch(&body)), &ctx, &tools()).await;
        assert!(out.result.is_error, "{body}\nran: {}", out.result.output);
    }
    assert!(!escape.exists() && !outside.path().join("new.txt").exists() && !outside.path().join("nope.txt").exists());
    assert_eq!(outside.read("x.txt").as_deref(), Some("x\n"));
    assert_eq!(project.read("a.txt").as_deref(), Some("a\n"));
}

/// **In the home folder, a patch that writes a dotfile or a hooks folder asks every time**, Add, Update and Move-to,
/// even in yolo, and each call there is asked `once` (no "always"), as for the edit tools.
///
/// Proven: made `ApplyPatch::protected_target` use `marv_sandbox::protected_rule` instead of
/// `edit_file::protected_target` (so no `home_rule`) → failed.
#[tokio::test]
async fn apply_patch_in_the_home_folder_dotfiles_and_hooks_ask_even_in_yolo() {
    let home = TempDir::new("patch-home");
    let h = home.path();
    home.write(".bashrc", "# rc\n");
    home.write("notes.txt", "x\n");
    home.write("Projects/app/.git/config", "[core]\n\thooksPath = tools/hooks\n");
    let context = |approver: Arc<Recorder>| {
        let mut ctx = ToolContext { yolo: true, approver: Some(approver), ..ToolContext::new(h) };
        ctx.home = h.to_path_buf();
        ctx.sandbox = Arc::new(sandbox_with_home(h));
        ctx
    };
    let cases = [
        "*** Add File: .profile\n+planted",
        "*** Update File: .bashrc\n-# rc\n+curl x|sh",
        "*** Update File: notes.txt\n*** Move to: .config/autostart/x.desktop\n-x\n+planted",
        "*** Add File: Projects/app/tools/hooks/pre-commit\n+planted",
    ];
    for body in cases {
        assert_eq!(asks(&context, &[".bashrc", "notes.txt"], body).await, Some(ProtectedRule::HomeDotfile), "{body}");
    }
    assert_eq!(home.read(".bashrc").as_deref(), Some("# rc\n"));
    assert!(!h.join(".profile").exists() && !h.join(".config/autostart").exists());
    // An ordinary file of the home is asked too, `once`.
    let approver = Recorder::new(Decision::No);
    let ctx = context(approver.clone());
    marv_guards::have_read(&ctx, &["notes.txt"]);
    let out = run_tool(&call("apply_patch", patch("*** Update File: notes.txt\n-x\n+y")), &ctx, &tools()).await;
    assert!(out.result.declined, "{}", out.result.output);
    let asked = approver.requests();
    assert!(asked.len() == 1 && asked[0].once && asked[0].protected.is_none(), "{asked:?}");
}

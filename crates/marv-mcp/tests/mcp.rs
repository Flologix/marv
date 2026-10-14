//! Ports Marv's `tests/mcp.test.ts`: the manager against real servers (the fixture over stdio, the test server and
//! the oracle's tracker over Streamable HTTP). Approval and yolo are `marv-session`'s (`run_tool`); these check the
//! manager and the tools it hands out.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use marv_core::ToolKind;
use marv_mcp::config::ServerSource;
use marv_mcp::manager::McpState;
use marv_mcp::trust::McpTrust;
use serde_json::json;
use support::*;
use tokio_util::sync::CancellationToken;

fn names(m: &marv_mcp::manager::McpManager) -> Vec<String> {
    m.tools().iter().map(|t| t.name.clone()).collect()
}

#[tokio::test]
async fn a_local_servers_tools_become_marv_tools_with_the_servers_schema() {
    let m = manager(vec![stdio("test")], Opts::new(temp_dir("root")));
    assert!(!m.settled(), "a manager with servers isn't settled before start() runs");
    m.start().await;
    assert!(m.settled());
    let status = &m.status()[0];
    assert_eq!(status.state, McpState::Connected);
    assert_eq!(
        status.tools,
        ["echo", "lookup", "fail", "picture", "slow", "env", "progress", "cwd"].map(|t| format!("mcp__test__{t}"))
    );
    let specs = m.specs();
    let echo = specs.iter().find(|s| s.name == "mcp__test__echo").unwrap();
    assert_eq!(echo.description, "(MCP server \"test\") Echo the text back.");
    assert_eq!(echo.parameters["properties"], json!({"text": {"type": "string"}}));
    assert_eq!(echo.parameters["required"], json!(["text"]));
    assert!(echo.parameters.get("$schema").is_none());
    m.close().await;
}

#[tokio::test]
async fn an_empty_manager_is_settled_at_once() {
    let m = manager(vec![], Opts::new(temp_dir("root")));
    assert!(m.settled());
    m.ready().await;
}

#[tokio::test]
async fn calls_return_the_servers_text_and_kinds_follow_the_hint_and_the_taint() {
    let m = manager(vec![stdio("test")], Opts::new(temp_dir("root")));
    m.start().await;
    assert_eq!(call(&m, "mcp__test__echo", json!({"text": "hi"})).await, ("echo: hi".into(), false));
    let tools = m.tools();
    let echo = tools.iter().find(|t| t.name == "mcp__test__echo").unwrap();
    let lookup = tools.iter().find(|t| t.name == "mcp__test__lookup").unwrap();
    // A tool the server marks read-only runs without asking, until the session is tainted (marv-rs).
    assert_eq!((echo.kind(false), lookup.kind(false), lookup.kind(true)), (ToolKind::Execute, ToolKind::Read, ToolKind::Egress));
    assert!(!echo.auto_safe(), "yolo never skips an MCP call");
    let preview = echo.preview(json!({"text": "hi"}).as_object().unwrap());
    assert_eq!(preview.title, "test: echo");
    assert_eq!(preview.text.as_deref(), Some("{\n  \"text\": \"hi\"\n}"));
    assert_eq!(preview.note, "MCP server \"test\" (yours) · runs outside the sandbox");
    assert_eq!(echo.scope(), ("mcp:test:echo".into(), "test's echo".into()));
    assert_eq!(call(&m, "mcp__test__lookup", json!({"word": "mars"})).await.0, "mars: a word");
    m.close().await;
}

#[tokio::test]
async fn errors_images_and_what_the_servers_environment_holds() {
    let m = manager(vec![stdio("test")], Opts::new(temp_dir("root")));
    m.start().await;
    assert_eq!(call(&m, "mcp__test__fail", json!({})).await, ("it broke".into(), true));
    assert_eq!(
        call(&m, "mcp__test__picture", json!({})).await.0,
        "here it is\n[image (image/png), 0 KB, not shown: Marv passes text only]"
    );
    m.close().await;
    // API keys don't reach it; its own env does.
    let mut o = Opts::new(temp_dir("root"));
    o.env.insert("OPENROUTER_API_KEY".into(), "sk-or-secret".into());
    let m = manager(vec![stdio("test")], o);
    m.start().await;
    assert_eq!(call(&m, "mcp__test__env", json!({})).await.0, "key= custom=yes");
    m.close().await;
}

#[tokio::test]
async fn a_long_call_that_reports_progress_isnt_cut_off_by_the_timeout() {
    // 1 s, but the tool takes 1.5 s, reporting every 0.3 s.
    let server = config("test", ServerSource::Personal, json!({"command": FIXTURE, "timeout": 1}));
    let m = manager(vec![server], Opts::new(temp_dir("root")));
    m.start().await;
    assert_eq!(call(&m, "mcp__test__progress", json!({})).await, ("made it".into(), false));
    // Without progress, the timeout holds.
    let (text, error) = call(&m, "mcp__test__slow", json!({})).await;
    assert!(error && text.contains("Request timed out"), "{text}");
    m.close().await;
}

#[tokio::test]
async fn esc_stops_a_call_thats_taking_long() {
    let m = manager(vec![stdio("test")], Opts::new(temp_dir("root")));
    m.start().await;
    let cancel = CancellationToken::new();
    let later = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        later.cancel();
    });
    let started = Instant::now();
    let (_, error) = call_with(&m, "mcp__test__slow", json!({}), cancel).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(error);
    // The server is still usable.
    assert_eq!(call(&m, "mcp__test__echo", json!({"text": "after"})).await.0, "echo: after");
    m.close().await;
}

#[tokio::test]
async fn yours_start_in_your_home_folder_a_projects_in_the_project() {
    let root = temp_dir("root");
    let mine = manager(vec![stdio("test")], Opts::new(root.clone()));
    let proj = config("proj", ServerSource::Project, json!({"command": FIXTURE}));
    let theirs = manager(vec![proj], Opts::new(root.clone()));
    tokio::join!(mine.start(), theirs.start());
    let home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
    assert_eq!(call(&mine, "mcp__test__cwd", json!({})).await.0, home.display().to_string());
    assert_eq!(call(&theirs, "mcp__proj__cwd", json!({})).await.0, root.display().to_string());
    mine.close().await;
    theirs.close().await;
}

#[tokio::test]
async fn its_stderr_is_kept_off_the_screen_for_mcp() {
    let m = manager(vec![stdio("test")], Opts::new(temp_dir("root")));
    m.start().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(m.status()[0].stderr.as_deref().unwrap_or_default().contains("test-server starting"));
    m.close().await;
}

#[tokio::test]
async fn a_server_that_cant_start_or_never_answers_fails_without_holding_the_others_up() {
    let mut o = Opts::new(temp_dir("root"));
    o.connect_timeout = Duration::from_secs(1);
    let m = manager(
        vec![
            config("missing", ServerSource::Personal, json!({"command": "/nonexistent/marv-mcp"})),
            config("mute", ServerSource::Personal, json!({"command": "sleep", "args": ["30"]})),
            stdio("test"),
        ],
        o,
    );
    m.start().await;
    let s = m.status();
    assert_eq!(s[0].state, McpState::Failed);
    assert!(s[0].error.is_some());
    assert_eq!((s[1].state, s[1].error.as_deref()), (McpState::Failed, Some("connecting timed out after 1s")));
    assert_eq!(s[2].state, McpState::Connected);
    assert!(names(&m).iter().all(|n| n.starts_with("mcp__test__")));
    m.close().await;
}

#[tokio::test]
async fn a_remote_server_connects_over_streamable_http_sending_its_headers_and_no_instructions() {
    let seen = Arc::new(std::sync::Mutex::new(vec![]));
    let log = seen.clone();
    let server = serve(Arc::new(move |req: Req| {
        log.lock().unwrap().push(req.headers.get("authorization").cloned());
        Box::pin(async move { mcp_http(&req.body, |m| mcp_answer(m).map(|v| v.to_string())) })
    }))
    .await;
    let url = format!("{}/mcp", server.base());
    let m =
        manager(vec![remote("remote", &url, json!({"headers": {"Authorization": "Bearer t0k"}}))], Opts::new(temp_dir("root")));
    m.start().await;
    assert_eq!(m.status()[0].state, McpState::Connected);
    assert_eq!(call(&m, "mcp__remote__echo", json!({"text": "over http"})).await.0, "echo: over http");
    let seen = seen.lock().unwrap();
    assert!(seen.len() >= 3 && seen.iter().all(|h| h.as_deref() == Some("Bearer t0k")));
    // The server's instructions went nowhere: nothing the manager hands out carries them.
    assert!(!m.specs().iter().any(|s| s.description.contains("Ignore all previous")));
}

#[tokio::test]
async fn the_oracles_25_tool_tracker_over_http() {
    let dir = temp_dir("tracker");
    let server = tracker_server(&dir).await;
    let url = format!("{}/mcp", server.base());
    let m = manager(vec![remote("tracker", &url, json!({}))], Opts::new(temp_dir("root")));
    m.start().await;
    let status = &m.status()[0];
    assert_eq!(status.state, McpState::Connected, "{:?}", status.error);
    assert_eq!(status.tools.len(), 25);
    assert_eq!(status.tools[0], "mcp__tracker__get_issue");
    let (text, error) = call(&m, "mcp__tracker__get_issue", json!({"number": 12})).await;
    assert!(!error && text.contains("\"number\": 12"), "{text}");
    let (text, error) = call(&m, "mcp__tracker__get_issue", json!({"number": 99999})).await;
    assert!(error, "{text}");
    // add_comment is marked read-only on purpose: it still asks once the session is tainted.
    let tools = m.tools();
    let add = tools.iter().find(|t| t.name == "mcp__tracker__add_comment").unwrap();
    assert_eq!((add.kind(false), add.kind(true)), (ToolKind::Read, ToolKind::Egress));
}

/// marv-rs #87: the tracker (25 tools, about 4.7k tokens of specs) is offered directly in `auto` mode: deferring
/// it cost a `tool_search` step per tool group for a prompt that is small anyway. Under Marv's old 4000-token limit
/// it was deferred.
#[tokio::test]
async fn the_trackers_25_tools_are_offered_directly() {
    use marv_mcp::deferred::{DEFER_TOKENS, McpMode, McpToolsSetting, choose_mode};
    let (dir, root) = (temp_dir("tracker-direct"), temp_dir("root"));
    let server = tracker_server(&dir).await;
    let m = manager(vec![remote("tracker", &format!("{}/mcp", server.base()), json!({}))], Opts::new(root.clone()));
    m.start().await;
    let specs = m.specs();
    assert_eq!(specs.len(), 25);
    let choice = choose_mode(McpToolsSetting::Auto, &specs, Some(128_000));
    assert!(choice.spec_tokens > 4000 && (choice.spec_tokens as f64) < DEFER_TOKENS, "{choice:?}");
    assert_eq!(choice.mode, McpMode::Eager);
    m.close().await;
    for d in [dir, root] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[tokio::test]
async fn a_tool_list_that_never_ends_doesnt_hang_startup() {
    let paging = config("paging", ServerSource::Personal, json!({"command": FIXTURE, "args": ["paging"]}));
    let m = manager(vec![paging], Opts::new(temp_dir("root")));
    let started = Instant::now();
    m.start().await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let s = &m.status()[0];
    assert_eq!((s.state, s.tools.clone()), (McpState::Connected, vec!["mcp__paging__only".to_string()]));
    m.close().await;
}

#[tokio::test]
async fn two_servers_tools_with_one_name_the_second_is_reported_not_silently_dropped() {
    let m = manager(vec![stdio("test"), stdio("test")], Opts::new(temp_dir("root")));
    m.start().await;
    let s = m.status();
    assert!(s[0].tools.contains(&"mcp__test__echo".to_string()));
    assert!(s[1].skipped.contains(&"mcp__test__echo".to_string()));
    assert_eq!(names(&m).iter().filter(|n| *n == "mcp__test__echo").count(), 1);
    m.close().await;
}

#[tokio::test]
async fn trust_while_servers_are_still_starting_doesnt_start_yours_twice() {
    let root = temp_dir("root");
    let mut o = Opts::new(root.clone());
    o.trust = Some(McpTrust::new(root.join("trust.json")));
    let proj = config("proj", ServerSource::Project, json!({"command": FIXTURE}));
    let m = manager(vec![stdio("test"), proj], o);
    let starting = tokio::spawn({
        let m = m.clone();
        async move { m.start().await }
    });
    assert!(m.untrusted().is_empty(), "still checking: nothing is untrusted yet");
    let started = m.trust_all().await.unwrap();
    assert_eq!(started.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["proj"]);
    starting.await.unwrap();
    // Each server started once: one process per server.
    assert_eq!(m.status().iter().map(|s| s.state).collect::<Vec<_>>(), [McpState::Connected, McpState::Connected]);
    m.close().await;
}

#[tokio::test]
async fn a_projects_server_waits_until_its_trusted_then_starts() {
    let root = temp_dir("root");
    let mut o = Opts::new(root.clone());
    let trust = McpTrust::new(root.join("trust.json"));
    o.trust = Some(trust.clone());
    let proj = config("proj", ServerSource::Project, json!({"command": FIXTURE}));
    let m = manager(vec![proj.clone()], o);
    m.start().await;
    assert_eq!(m.status()[0].state, McpState::Untrusted);
    assert!(m.tools().is_empty());
    assert_eq!(m.untrusted()[0].name, "proj");
    let started = m.trust_all().await.unwrap();
    assert_eq!((started[0].name.as_str(), started[0].state), ("proj", McpState::Connected));
    assert!(!m.tools().is_empty());
    assert!(trust.is_trusted(&format!("proj-{}", root.display()), &proj));
    m.close().await;
}

#[tokio::test]
async fn a_broken_trust_file_cant_crash_startup() {
    let root = temp_dir("root");
    std::fs::write(root.join("trust.json"), "null").unwrap();
    let mut o = Opts::new(root.clone());
    o.trust = Some(McpTrust::new(root.join("trust.json")));
    let m = manager(vec![config("proj", ServerSource::Project, json!({"command": FIXTURE})), stdio("test")], o);
    m.start().await;
    let states: Vec<_> = m.status().iter().map(|s| (s.name.clone(), s.state)).collect();
    assert_eq!(states, [("proj".to_string(), McpState::Untrusted), ("test".to_string(), McpState::Connected)]);
    m.close().await;
}

fn running(marker: &str) -> bool {
    let out = std::process::Command::new("pgrep").args(["-f", marker]).output().unwrap();
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

#[tokio::test]
async fn close_right_after_start_leaves_no_server_connected_or_running() {
    let marker = format!("close-race-{}", marv_fs::random_hex());
    let racy = config("racy", ServerSource::Personal, json!({"command": FIXTURE, "args": [marker]}));
    let m = manager(vec![racy], Opts::new(temp_dir("root")));
    let ready = m.start();
    m.close().await;
    ready.await;
    let s = &m.status()[0];
    assert_eq!((s.state, s.error.as_deref()), (McpState::Failed, Some("closed")));
    for _ in 0..50 {
        if !running(&marker) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!running(&marker));
}

#[tokio::test]
async fn close_and_kill_stop_a_connected_server() {
    let marker = format!("close-{}", marv_fs::random_hex());
    let m = manager(
        vec![config("x", ServerSource::Personal, json!({"command": FIXTURE, "args": [marker]}))],
        Opts::new(temp_dir("root")),
    );
    m.start().await;
    assert!(running(&marker));
    m.kill();
    for _ in 0..50 {
        if !running(&marker) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!running(&marker));
    // A server that exits later: its tools stay listed and calls fail clearly.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(m.status()[0].state, McpState::Failed);
    let (text, error) = call(&m, "mcp__x__echo", json!({"text": "hi"})).await;
    assert!(error && text.contains("the server isn't available"), "{text}");
}

#[tokio::test]
async fn an_added_server_starts_and_a_turn_starting_meanwhile_waits_for_it() {
    let m = manager(vec![], Opts::new(temp_dir("root")));
    m.start().await;
    assert!(m.settled());
    let adding = m.add(stdio("added"));
    assert!(!m.settled(), "unsettled as soon as add() is called");
    let waiting = tokio::spawn({
        let m = m.clone();
        async move { m.ready().await }
    });
    let status = adding.await.unwrap();
    waiting.await.unwrap();
    assert!(m.settled());
    assert_eq!((status.name.as_str(), status.state), ("added", McpState::Connected));
    assert!(names(&m).contains(&"mcp__added__echo".to_string()));
    assert!(m.specs().iter().any(|s| s.name == "mcp__added__echo"));
    assert_eq!(call(&m, "mcp__added__echo", json!({"text": "hi"})).await.0, "echo: hi");
    // It starts in the home folder, like your other servers.
    let home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
    assert_eq!(call(&m, "mcp__added__cwd", json!({})).await.0, home.display().to_string());
    m.close().await;
}

#[tokio::test]
async fn a_taken_name_is_refused_a_failing_server_says_so() {
    let m = manager(vec![stdio("a")], Opts::new(temp_dir("root")));
    m.start().await;
    assert!(m.add(stdio("a")).await.unwrap_err().to_string().contains("already an MCP server named \"a\""));
    let failed = m.add(config("broken", ServerSource::Personal, json!({"command": "/nonexistent/server"}))).await.unwrap();
    assert_eq!(failed.state, McpState::Failed);
    assert!(m.settled());
    m.close().await;
}

#[tokio::test]
async fn remove_stops_it_its_tools_stay_listed_and_fail_a_new_conversation_forgets_them() {
    let m = manager(vec![], Opts::new(temp_dir("root")));
    m.add(stdio("gone")).await.unwrap();
    let specs = m.specs().len();
    m.remove("gone").await.unwrap();
    assert_eq!(m.status()[0].state, McpState::Removed);
    assert_eq!(m.specs().len(), specs);
    let (text, error) = call(&m, "mcp__gone__echo", json!({"text": "hi"})).await;
    assert!(error && text.contains("removed with /mcp remove"), "{text}");
    assert!(m.remove("gone").await.unwrap_err().to_string().contains("no MCP server named \"gone\""));
    // Removed in this conversation: can't come back until the next one.
    assert!(m.add(stdio("gone")).await.unwrap_err().to_string().contains("removed in this conversation"));
    m.forget_removed();
    assert!(m.specs().is_empty() && m.status().is_empty());
    assert_eq!(m.add(stdio("gone")).await.unwrap().state, McpState::Connected);
    m.close().await;
}

#[tokio::test]
async fn a_server_removed_while_its_still_connecting_stays_removed() {
    let m = manager(vec![], Opts::new(temp_dir("root")));
    let adding = tokio::spawn({
        let m = m.clone();
        async move { m.add(stdio("quick")).await }
    });
    tokio::task::yield_now().await;
    let _ = m.remove("quick").await;
    let _ = adding.await.unwrap();
    let state = m.status().iter().find(|s| s.name == "quick").map(|s| s.state);
    assert!(matches!(state, Some(McpState::Removed) | None), "{state:?}");
    if state == Some(McpState::Removed) {
        assert!(call(&m, "mcp__quick__echo", json!({"text": "hi"})).await.1);
    }
    m.close().await;
}

#[tokio::test]
async fn after_close_nothing_more_is_added() {
    let m = manager(vec![], Opts::new(temp_dir("root")));
    m.add(stdio("x")).await.unwrap();
    m.close().await;
    assert!(m.add(stdio("y")).await.unwrap_err().to_string().contains("closing"));
}

#[tokio::test]
async fn two_marv_processes_updating_one_token_store_every_entry_survives() {
    let store = marv_mcp::oauth_store::McpTokenStore::new(temp_dir("store").join("mcp-tokens.json"));
    let run = |prefix: &str| {
        std::process::Command::new(FIXTURE)
            .args(["token-writer", &store.path.display().to_string(), prefix, "40"])
            .spawn()
            .unwrap()
    };
    let children = [run("a"), run("b"), run("c")];
    for i in 0..40 {
        let server = format!("d{i}");
        store
            .update(&format!("d https://x.test/{i}"), move |e| {
                let mut e = e.unwrap_or_else(|| marv_mcp::oauth_store::Login::new(""));
                e.server = server;
                Ok(Some(e))
            })
            .await
            .unwrap();
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(store.all().len(), 160);
    assert!(!std::path::PathBuf::from(format!("{}.lock", store.path.display())).exists());
}

#[tokio::test]
async fn two_marv_processes_adding_servers_at_once_every_server_survives() {
    let d = temp_dir("file");
    let run = |prefix: &str| {
        std::process::Command::new(FIXTURE).args(["file-writer", &d.display().to_string(), prefix, "25"]).spawn().unwrap()
    };
    let children = [run("a"), run("b")];
    for i in 0..25 {
        marv_mcp::personal_file::add_personal_server(&d, &format!("c{i}"), json!({"command": "npx", "args": ["-y", "x@1.0.0"]}))
            .await
            .unwrap();
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(marv_mcp::personal_file::personal_server_names(&d).len(), 75);
    assert!(!d.join("mcp.json.lock").exists());
}

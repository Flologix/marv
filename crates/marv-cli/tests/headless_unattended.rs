//! `marv -p` tells the model it runs unattended (`headless::HEADLESS_NOTE`): nobody can answer a question, so it
//! shouldn't stop to ask. On Terminal-Bench a run ended on "Want me to keep going…?" (2026-10-31). The note is in every
//! request's system prompt, after Marv's own, the same bytes each time (the prompt cache).

use std::process::{Command, Stdio};

use marv_oracle::script::Reply;
use marv_oracle::server::FakeServer;
use serde_json::{Value, json};

#[test]
fn every_request_of_marv_p_says_nobody_can_answer() {
    let dir = std::env::temp_dir().join(format!("marv-unattended-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["proj", "home", "config", "run"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join("proj/a.txt"), "a\n").unwrap();
    let replies: Vec<Reply> = vec![
        serde_json::from_value(json!({"tool_calls": [{"id": "c1", "name": "read_file", "arguments": {"path": "a.txt"}}]}))
            .unwrap(),
        serde_json::from_value(json!({"text": "Done."})).unwrap(),
    ];
    let server = FakeServer::start(replies, Vec::new()).unwrap();
    let settings = Value::Object(marv_oracle::runner::default_config(&server.base_url()));
    std::fs::write(dir.join("config/config.json"), settings.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_marv"))
        .args(["-p", "read a.txt"])
        .current_dir(dir.join("proj"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.join("home"))
        .env("MARV_CONFIG_DIR", dir.join("config"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("MARV_GITHUB", "off")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let systems: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|r| r.method == "POST")
        .map(|r| serde_json::from_str::<Value>(&r.body).unwrap())
        .map(|body| {
            body["messages"][0]["content"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| body["messages"][0]["content"].to_string())
        })
        .collect();
    assert_eq!(systems.len(), 2);
    assert!(systems[0].contains("# Running unattended"), "{}", systems[0]);
    assert!(systems[0].contains("nobody can answer a question"), "{}", systems[0]);
    // The checklist before the final reply, and no unsandboxed facts in a sandboxed run.
    assert!(systems[0].contains("Before your final reply, re-read the task and list every explicit requirement"));
    assert!(systems[0].contains("Never look online for the task's own source, hidden tests or reference answers."));
    assert!(!systems[0].contains(marv_session::prompt::UNSANDBOXED_FACTS), "{}", systems[0]);
    assert_eq!(systems[0], systems[1], "the system prompt changed between requests");
}

/// `--time-limit` puts the budget in the system prompt (the same bytes on every request) and each tool's result (a
/// command's, and a file write's too) says how much time is left.
#[test]
fn a_time_limit_is_in_the_prompt_and_in_each_command_result() {
    let dir = std::env::temp_dir().join(format!("marv-time-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["proj", "home", "config", "run"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let replies: Vec<Reply> = vec![
        serde_json::from_value(json!({"tool_calls": [{"id": "c1", "name": "bash", "arguments": {"command": "echo hi"}}]}))
            .unwrap(),
        serde_json::from_value(
            json!({"tool_calls": [{"id": "c2", "name": "write_file", "arguments": {"path": "out.txt", "content": "hi\n"}}]}),
        )
        .unwrap(),
        serde_json::from_value(json!({"text": "Done."})).unwrap(),
    ];
    let server = FakeServer::start(replies, Vec::new()).unwrap();
    let settings = Value::Object(marv_oracle::runner::default_config(&server.base_url()));
    std::fs::write(dir.join("config/config.json"), settings.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_marv"))
        .args(["-p", "say hi", "--time-limit", "600", "--dangerously-run-unsandboxed"])
        .current_dir(dir.join("proj"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.join("home"))
        .env("MARV_CONFIG_DIR", dir.join("config"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("MARV_GITHUB", "off")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let bodies: Vec<Value> =
        server.requests().into_iter().filter(|r| r.method == "POST").map(|r| serde_json::from_str(&r.body).unwrap()).collect();
    assert_eq!(bodies.len(), 3);
    let system = |b: &Value| b["messages"][0]["content"].to_string();
    assert!(system(&bodies[0]).contains("# Time limit") && system(&bodies[0]).contains("10 minutes"), "{}", system(&bodies[0]));
    assert_eq!(system(&bodies[0]), system(&bodies[1]));
    for body in &bodies[1..] {
        let last = body["messages"].as_array().unwrap().last().unwrap().to_string();
        assert!(last.contains("time left in this run: 9m") || last.contains("time left in this run: 10m"), "{last}");
    }
    assert!(bodies[2]["messages"].as_array().unwrap().last().unwrap()["role"] == "tool");
}

/// `--dangerously-run-unsandboxed`: the access facts say commands run on this machine, not in a sandbox (0.15.0 told
/// such a run it was sandboxed, and on Terminal-Bench it handed the user scripts instead of configuring the machine).
#[test]
fn an_unsandboxed_run_is_told_it_is_not_sandboxed() {
    let dir = std::env::temp_dir().join(format!("marv-unsandboxed-facts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["proj", "home", "config", "run"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    let replies: Vec<Reply> = vec![serde_json::from_value(json!({"text": "Done."})).unwrap()];
    let server = FakeServer::start(replies, Vec::new()).unwrap();
    let settings = Value::Object(marv_oracle::runner::default_config(&server.base_url()));
    std::fs::write(dir.join("config/config.json"), settings.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_marv"))
        .args(["-p", "say hi", "--dangerously-run-unsandboxed"])
        .current_dir(dir.join("proj"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.join("home"))
        .env("MARV_CONFIG_DIR", dir.join("config"))
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("MARV_GITHUB", "off")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let body: Value =
        server.requests().into_iter().find(|r| r.method == "POST").map(|r| serde_json::from_str(&r.body).unwrap()).unwrap();
    let system = body["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(system.contains(marv_session::prompt::UNSANDBOXED_FACTS), "{system}");
    assert!(!system.contains("Sandboxed commands can write only there"), "{system}");
    assert!(!system.contains("# Sandbox limits"), "{system}");
}

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
    assert_eq!(systems[0], systems[1], "the system prompt changed between requests");
}

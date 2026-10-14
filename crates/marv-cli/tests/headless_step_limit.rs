//! `marv -p` at the step limit end to end (Marv #59), with the real binary and a scripted fake model (the oracle's
//! server): with no one to say "keep going", the last request asks for a wrap-up with `tool_choice: "none"` (the
//! tools still sent, so the cached prefix holds), the summary is what `-p` prints, and it still exits 2.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use marv_oracle::script::Reply;
use marv_oracle::server::FakeServer;
use serde_json::{Value, json};

fn reply(v: Value) -> Reply {
    serde_json::from_value(v).unwrap()
}
fn call(id: &str, name: &str, args: Value) -> Reply {
    reply(json!({"tool_calls": [{"id": id, "name": name, "arguments": args}]}))
}

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const SUMMARY: &str = "Read a.txt and b.txt. Left: c.txt. Next step: read c.txt.";

#[test]
fn marv_p_at_the_step_limit_prints_a_wrap_up_and_still_exits_2() {
    let dir = std::env::temp_dir().join(format!("marv-step-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["proj", "home", "config", "run"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = Dir(dir.canonicalize().unwrap());
    let project = dir.0.join("proj");
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(project.join(name), name).unwrap();
    }
    let server = FakeServer::start(
        vec![
            call("c1", "read_file", json!({"path": "a.txt"})),
            call("c2", "read_file", json!({"path": "b.txt"})),
            reply(json!({"text": SUMMARY})),
        ],
        Vec::new(),
    )
    .unwrap();
    let settings = Value::Object(marv_oracle::runner::default_config(&server.base_url()));
    std::fs::write(dir.0.join("config/config.json"), settings.to_string()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_marv"))
        .args(["-p", "read every file", "--max-steps", "2"])
        .current_dir(&project)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.0.join("home"))
        .env("MARV_CONFIG_DIR", dir.0.join("config"))
        .env("XDG_RUNTIME_DIR", dir.0.join("run"))
        .env("MARV_GITHUB", "off")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(2), "{stdout}\n{stderr}");
    assert_eq!(stdout, format!("{SUMMARY}\n"));
    assert!(stderr.contains("Stopped after 2 steps"), "{stderr}");

    let bodies: Vec<Value> =
        server.requests().into_iter().filter(|r| r.method == "POST").map(|r| serde_json::from_str(&r.body).unwrap()).collect();
    assert_eq!(bodies.len(), 3);
    let (before, wrap_up) = (&bodies[1], &bodies[2]);
    assert!(before.get("tool_choice").is_none());
    assert_eq!(wrap_up["tool_choice"], "none");
    // Same tools, same system prompt and history so far: only the note is new.
    assert_eq!(wrap_up["tools"], before["tools"]);
    let (old, new) = (before["messages"].as_array().unwrap(), wrap_up["messages"].as_array().unwrap());
    assert_eq!(&new[..old.len()], &old[..]);
    let note = new.last().unwrap();
    assert_eq!(note["role"], "user");
    assert!(note["content"].as_str().unwrap().contains("You've reached the step limit. Don't call tools."), "{note}");
}

//! Mesh v2 end to end (#16): two `marv -p` processes on one machine, each with a scripted fake model (the oracle's
//! server) and real bwrap, exchange messages over cross-process wires that both ends approved
//! (`--approve mesh:<receptacle>`). Each message reaches the other model fenced, from `peer-<name>`. Without the far
//! end's approval nothing arrives and the asking agent reads why. Skipped where bwrap doesn't work.
//!
//! The models can't wait for mail on their own, so each script parks in a sandboxed `bash` loop until the test
//! touches a file in its project; the inbox is read right after that step (`with_inbox`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use marv_oracle::script::Reply;
use marv_oracle::server::FakeServer;
use serde_json::{Value, json};

fn bwrap_works() -> bool {
    Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-net", "true"])
        .status()
        .is_ok_and(|s| s.success())
}

/// Waits in the sandbox until the test touches `go` in the project.
const WAIT: &str = "for i in $(seq 1 900); do [ -e go ] && break; sleep 0.1; done; echo ready";

fn reply(v: Value) -> Reply {
    serde_json::from_value(v).unwrap()
}
fn call(id: &str, name: &str, args: Value) -> Reply {
    reply(json!({"tool_calls": [{"id": id, "name": name, "arguments": args}]}))
}
fn text(t: &str) -> Reply {
    reply(json!({"text": t}))
}

/// Where the test machines go: on macOS a short `/tmp`, as macOS's temp folder (`/var/folders/…/T`) leaves too little
/// of `sun_path`'s 104 bytes for the sockets (#150).
fn temp_base() -> PathBuf {
    if cfg!(target_os = "macos") { PathBuf::from("/tmp") } else { std::env::temp_dir() }
}

/// The variable that puts a Marv's runtime folder at `<run>/marv`: `XDG_RUNTIME_DIR=<run>` on Linux,
/// `MARV_RUNTIME_DIR=<run>/marv` on macOS, which has no `XDG_RUNTIME_DIR`.
fn runtime_var(run: &Path) -> (&'static str, PathBuf) {
    if cfg!(target_os = "macos") { ("MARV_RUNTIME_DIR", run.join("marv")) } else { ("XDG_RUNTIME_DIR", run.to_path_buf()) }
}

/// A throwaway machine: a private runtime dir, a home, and a project and config per Marv.
struct Machine(PathBuf);

impl Machine {
    fn new(label: &str) -> Self {
        let dir = temp_base().join(format!("mx-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        for sub in ["run", "home"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }

    fn run_dir(&self) -> PathBuf {
        self.0.join("run")
    }

    fn command(&self, name: &str, args: &[&str]) -> Command {
        let (runtime_var, runtime_dir) = runtime_var(&self.run_dir());
        let mut command = Command::new(env!("CARGO_BIN_EXE_marv"));
        command
            .args(args)
            .current_dir(self.0.join(format!("proj-{name}")))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.0.join("home"))
            .env("MARV_CONFIG_DIR", self.0.join(format!("config-{name}")))
            .env(runtime_var, runtime_dir)
            .env("MARV_MESH_NAME", name)
            // No GitHub probe (`gh auth status`), which this test doesn't need: where `gh` is a wrapper that installs
            // gh on first use (mise), the probe's timeout kills the wrapper and not the installer, which keeps writing
            // ~40 MB into the fake home after it was removed (#89).
            .env("MARV_GITHUB", "off")
            .env("TERM", "dumb")
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// `marv -p` as `name` against its own fake model.
    fn marv(&self, name: &str, server: &FakeServer, prompt: &str, approve: &[&str]) -> Child {
        let (project, config) = (self.0.join(format!("proj-{name}")), self.0.join(format!("config-{name}")));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        let settings = Value::Object(marv_oracle::runner::default_config(&server.base_url()));
        std::fs::write(config.join("config.json"), settings.to_string()).unwrap();
        let mut args = vec!["-p", prompt];
        // A name is an --approve scope; a flag ("--approve-tainted") goes as it is.
        for a in approve {
            if a.starts_with("--") {
                args.push(a);
            } else {
                args.extend(["--approve", a]);
            }
        }
        self.command(name, &args).spawn().unwrap()
    }

    fn touch(&self, name: &str) {
        std::fs::write(self.0.join(format!("proj-{name}")).join("go"), "").unwrap();
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Polls `ok` for up to a minute.
fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every message's text in a chat request, joined: what the model was shown.
fn shown(body: &str) -> String {
    let body: Value = serde_json::from_str(body).unwrap();
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| match &m["content"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn chats(server: &FakeServer) -> Vec<String> {
    server.requests().into_iter().filter(|r| r.method == "POST").map(|r| r.body).collect()
}

/// Ends a child and returns (exit code, stdout, stderr).
fn finish(child: Child) -> (Option<i32>, String, String) {
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

/// Kills what's still running if an assertion fails midway.
struct Reap(Vec<Child>);
impl Drop for Reap {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn socket(machine: &Machine, name: &str) -> PathBuf {
    machine.run_dir().join("marv").join("mesh").join(format!("{name}.sock"))
}

fn ls(machine: &Machine) -> String {
    let out = machine.command("alpha", &["mesh", "ls"]).current_dir("/").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into()
}

#[test]
fn two_marvs_exchange_messages_over_wires_both_ends_approved() {
    if !bwrap_works() {
        eprintln!("skipped: no working bwrap");
        return;
    }
    let machine = Machine::new("both");
    let alpha_model = FakeServer::start(
        vec![
            call("a1", "mesh_send", json!({"receptacle": "peer.beta.agent.parent.inbox", "text": "alpha: the API is ready"})),
            call("a2", "bash", json!({"command": WAIT})),
            text("alpha done"),
        ],
        vec![],
    )
    .unwrap();
    let beta_model = FakeServer::start(
        vec![
            call("b1", "bash", json!({"command": WAIT})),
            call(
                "b2",
                "mesh_send",
                json!({"receptacle": "peer.alpha.agent.parent.inbox", "text": "beta: thanks, wiring the UI"}),
            ),
            text("beta done"),
        ],
        vec![],
    )
    .unwrap();

    // Each pre-approves its wire to the other and the other's wire to it. beta replies after alpha's message tainted
    // its session, and `marv -p` approves tainted egress only with --approve-tainted.
    let beta = machine.marv(
        "beta",
        &beta_model,
        "Wait for alpha.",
        &["mesh:agent.parent.inbox", "mesh:peer.alpha.agent.parent.inbox", "--approve-tainted"],
    );
    let mut reap = Reap(vec![beta]);
    wait_for("beta on the mesh", || socket(&machine, "beta").exists());
    let listed = ls(&machine);
    assert!(listed.starts_with("beta  radio off  pane -  agents parent  "), "{listed}");
    assert!(listed.contains("(peer.beta.agent.parent.inbox)"), "{listed}");

    let alpha = machine.marv(
        "alpha",
        &alpha_model,
        "Tell beta the API is ready.",
        &["mesh:peer.beta.agent.parent.inbox", "mesh:agent.parent.inbox"],
    );
    reap.0.push(alpha);
    // alpha sent: its second request carries the send's result.
    wait_for("alpha's send", || chats(&alpha_model).len() >= 2);
    let after_send = shown(&chats(&alpha_model)[1]);
    assert!(after_send.contains("Sent to peer.beta.agent.parent.inbox."), "{after_send}");
    assert_eq!(ls(&machine).lines().count(), 2, "both on the mesh");

    // beta's wait ends; its next request has alpha's message, fenced, from alpha's plug on beta's switchboard.
    machine.touch("beta");
    wait_for("beta's reply", || chats(&beta_model).len() >= 3);
    let beta_saw = shown(&chats(&beta_model)[1]);
    assert!(beta_saw.contains("<mesh-message from=\"peer-alpha\" receptacle=\"agent.parent.inbox\">"), "{beta_saw}");
    assert!(beta_saw.contains("alpha: the API is ready"), "{beta_saw}");
    assert!(shown(&chats(&beta_model)[2]).contains("Sent to peer.alpha.agent.parent.inbox."));

    // And back.
    machine.touch("alpha");
    let alpha = reap.0.pop().unwrap();
    let (code, out, err) = finish(alpha);
    assert_eq!(code, Some(0), "alpha: {err}");
    assert_eq!(out.trim(), "alpha done");
    let alpha_saw = shown(&chats(&alpha_model)[2]);
    assert!(alpha_saw.contains("<mesh-message from=\"peer-beta\" receptacle=\"agent.parent.inbox\">"), "{alpha_saw}");
    assert!(alpha_saw.contains("beta: thanks, wiring the UI"), "{alpha_saw}");
    let (code, out, err) = finish(reap.0.pop().unwrap());
    assert_eq!(code, Some(0), "beta: {err}");
    assert_eq!(out.trim(), "beta done");
    // Both left the mesh.
    assert!(!socket(&machine, "alpha").exists() && !socket(&machine, "beta").exists());
    assert_eq!(ls(&machine).trim(), "No Marv is on the mesh here.");
}

#[test]
fn without_the_far_ends_approval_nothing_arrives() {
    if !bwrap_works() {
        eprintln!("skipped: no working bwrap");
        return;
    }
    let machine = Machine::new("one");
    let alpha_model = FakeServer::start(
        vec![
            call("a1", "mesh_send", json!({"receptacle": "peer.beta.agent.parent.inbox", "text": "alpha: hello?"})),
            text("alpha done"),
        ],
        vec![],
    )
    .unwrap();
    let beta_model = FakeServer::start(vec![call("b1", "bash", json!({"command": WAIT})), text("beta done")], vec![]).unwrap();
    // beta is on the mesh (MARV_MESH_NAME) but approves no wire to its inbox.
    let beta = machine.marv("beta", &beta_model, "Wait.", &[]);
    let mut reap = Reap(vec![beta]);
    wait_for("beta on the mesh", || socket(&machine, "beta").exists());
    let alpha = machine.marv("alpha", &alpha_model, "Say hello to beta.", &["mesh:peer.beta.agent.parent.inbox"]);
    reap.0.push(alpha);
    let (code, out, err) = finish(reap.0.pop().unwrap());
    assert_eq!((code, out.trim()), (Some(0), "alpha done"), "{err}");
    let alpha_saw = shown(&chats(&alpha_model)[1]);
    assert!(alpha_saw.contains("the Marv \"beta\" didn't wire it: its user said no"), "{alpha_saw}");
    machine.touch("beta");
    let (code, out, err) = finish(reap.0.pop().unwrap());
    assert_eq!((code, out.trim()), (Some(0), "beta done"), "{err}");
    let beta_saw = shown(&chats(&beta_model)[1]);
    assert!(!beta_saw.contains("mesh-message") && !beta_saw.contains("hello?"), "{beta_saw}");
}

//! The mesh radio (#124), end to end: real `marv` TUIs, each in a pseudo-terminal (a small python3 helper), on one
//! machine's mesh. Phase 1: two with `"mesh": {"radio": true}` in their own config see each other in the strip above
//! the prompt and `marv mesh ls` says their radio is on; a third without it is on the mesh but not on the radio. One
//! killed with SIGKILL shows `left (lost)` on the other's strip, and its dead door socket is swept at once. Phase 2: a
//! person's `/tx` reaches the other Marv's model fenced, wakes it (wake on), and never reaches the sender's own model.
//! Skipped without python3.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use marv_oracle::server::FakeServer;
use serde_json::{Value, json};

/// Runs `argv` in a pseudo-terminal of 40×160, appends what it draws to `log`, types each line read from stdin
/// (with a carriage return), and prints the child's pid first.
const PTY: &str = r#"
import fcntl, os, pty, select, signal, struct, sys, termios
log, argv = sys.argv[1], sys.argv[2:]
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
    os.execv(argv[0], argv)
print(pid, flush=True)
inputs = [fd, sys.stdin]
with open(log, "ab", buffering=0) as out:
    while True:
        ready, _, _ = select.select(inputs, [], [], 1.0)
        if fd in ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            out.write(data)
        if sys.stdin in ready:
            line = sys.stdin.readline()
            if not line:
                inputs.remove(sys.stdin)
            else:
                os.write(fd, line.rstrip("\n").encode() + b"\r")
"#;

fn python() -> bool {
    Command::new("python3").arg("-c").arg("import pty").status().is_ok_and(|s| s.success())
}

/// A throwaway machine: a private runtime dir, a home, and a project and config per Marv.
struct Machine(PathBuf);

impl Machine {
    fn new() -> Self {
        // One per test: the tests of this file run at once, in one process.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mx-radio-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["run", "home"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let dir = dir.canonicalize().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }

    fn socket(&self, name: &str) -> PathBuf {
        self.0.join("run/marv/mesh").join(format!("{name}.sock"))
    }

    fn command(&self, name: &str, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(self.0.join(format!("proj-{name}")))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.0.join("home"))
            .env("MARV_CONFIG_DIR", self.0.join(format!("config-{name}")))
            .env("XDG_RUNTIME_DIR", self.0.join("run"))
            .env("MARV_MESH_NAME", name)
            // No GitHub probe (see tests/mesh.rs, #89).
            .env("MARV_GITHUB", "off")
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8");
        command
    }

    /// The TUI as `name`, in a pseudo-terminal; `radio` is what its own config says.
    fn tui(&self, name: &str, server: &FakeServer, radio: bool) -> Tui {
        self.tui_with(name, server, if radio { json!({"radio": true}) } else { Value::Null })
    }

    /// [`Self::tui`] with `mesh` as its own config's `"mesh"` (null: none).
    fn tui_with(&self, name: &str, server: &FakeServer, mesh: Value) -> Tui {
        let (project, config) = (self.0.join(format!("proj-{name}")), self.0.join(format!("config-{name}")));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        let mut settings = Value::Object(marv_oracle::runner::default_config(&server.base_url()));
        if !mesh.is_null() {
            settings["mesh"] = mesh;
        }
        std::fs::write(config.join("config.json"), settings.to_string()).unwrap();
        let screen = self.0.join(format!("screen-{name}.log"));
        let mut child = self
            .command(name, "python3")
            .arg("-c")
            .arg(PTY)
            .arg(&screen)
            .arg(env!("CARGO_BIN_EXE_marv"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut pid = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut pid).unwrap();
        let stdin = child.stdin.take();
        Tui { child, stdin, pid: pid.trim().parse().unwrap(), screen }
    }

    fn ls(&self) -> String {
        let out = self.command("ls", env!("CARGO_BIN_EXE_marv")).current_dir("/").args(["mesh", "ls"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `marv` TUI in its pseudo-terminal.
struct Tui {
    child: Child,
    stdin: Option<ChildStdin>,
    /// marv's own pid (the helper's child).
    pid: i32,
    screen: PathBuf,
}

impl Tui {
    /// Everything it drew so far, escape sequences and blanks out: the terminal gets only the cells that changed, so a
    /// space already on screen isn't sent again.
    fn screen(&self) -> String {
        let raw = String::from_utf8_lossy(&std::fs::read(&self.screen).unwrap_or_default()).into_owned();
        let mut text = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                match chars.next() {
                    Some('[') => while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {},
                    Some(']') => while chars.next().is_some_and(|c| c != '\x07' && c != '\x1b') {},
                    _ => {}
                }
            } else if !c.is_control() && !c.is_whitespace() {
                text.push(c);
            }
        }
        text
    }

    fn type_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    fn kill(&self) {
        let _ = Command::new("kill").args(["-KILL", &self.pid.to_string()]).status();
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.kill();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls `ok` for up to a minute.
fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn two_marvs_on_the_radio_see_each_other_and_a_killed_one_is_lost() {
    if !python() {
        eprintln!("skipped: no python3 with pty");
        return;
    }
    let machine = Machine::new();
    let model = FakeServer::start(vec![], vec![]).unwrap();
    let alpha = machine.tui("alpha", &model, true);
    wait_for("alpha on the mesh", || machine.socket("alpha").exists());
    let beta = machine.tui("beta", &model, true);
    let mut gamma = machine.tui("gamma", &model, false);
    wait_for("three Marvs on the mesh", || machine.ls().lines().count() == 3);
    let listed = machine.ls();
    assert!(listed.contains("alpha  radio on   pane -"), "{listed}");
    assert!(listed.contains("beta   radio on   pane -"), "{listed}");
    assert!(listed.contains("gamma  radio off  pane -"), "{listed}");

    // Each of the two shows the other in its strip; neither shows gamma, which isn't on the radio. (A strip that changes
    // is matched by what changed in it: the terminal gets only those cells.)
    wait_for("alpha's strip", || alpha.screen().contains("◉radio·"));
    wait_for("beta in alpha's strip", || alpha.screen().contains("betaidle"));
    wait_for("alpha in beta's strip", || beta.screen().contains("alphaidle"));
    assert!(!alpha.screen().contains("gamma"), "{}", alpha.screen());
    assert!(!gamma.screen().contains("◉radio"), "gamma's radio is off: no strip");

    // gamma turns it on itself (`/radio on`, typed): it joins at once, and its config says so from now on.
    gamma.type_line("/radio on");
    wait_for("gamma in alpha's strip", || alpha.screen().contains("gammaidle"));
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(machine.0.join("config-gamma/config.json")).unwrap()).unwrap();
    assert_eq!(saved["mesh"], json!({"radio": true}));

    // beta is killed: no goodbye. alpha shows it lost, and its dead socket is swept.
    beta.kill();
    wait_for("beta lost on alpha's strip", || alpha.screen().contains("left(lost)"));
    wait_for("beta's socket swept", || !machine.socket("beta").exists());
    assert!(machine.socket("alpha").exists() && machine.socket("gamma").exists());
    drop((alpha, beta, gamma));
}

/// Phase 2: alpha's person sends beta a message with `/tx`. alpha's transcript gets Marv's one line and alpha's model
/// nothing; beta (wake on, idle) is woken by it, its model reads it fenced as `<radio-message … by="user">` with the
/// radio wake note after it, and its tools include `radio_tx` (its conversation started on the radio).
#[test]
fn a_tx_reaches_the_other_marvs_model_fenced_and_wakes_it() {
    if !python() {
        eprintln!("skipped: no python3 with pty");
        return;
    }
    use marv_oracle::script::Reply;
    let machine = Machine::new();
    let alpha_model = FakeServer::start(vec![], vec![]).unwrap();
    let beta_model =
        FakeServer::start(vec![Reply { text: "alpha says tests pass.".into(), ..Reply::default() }], vec![]).unwrap();
    let mut alpha = machine.tui("alpha", &alpha_model, true);
    let beta = machine.tui_with("beta", &beta_model, json!({"radio": true, "wake": true}));
    wait_for("beta in alpha's strip", || alpha.screen().contains("betaidle"));
    wait_for("alpha in beta's strip", || beta.screen().contains("alphaidle"));

    alpha.type_line("/tx @beta tests pass on main");
    // The screen log holds the terminal's incremental redraws: a cell already showing the right letter isn't written
    // again, so the line's full text may never appear in one piece (with /peers in the palette, "tests pass" came out
    // as "testspss"). Its start is enough here: what reached beta is checked in full below.
    wait_for("alpha's line", || alpha.screen().contains("✻Radio→@beta:"));
    wait_for("beta's model asked", || beta_model.requests().iter().any(|r| r.path.ends_with("/chat/completions")));
    let request = beta_model.requests().into_iter().find(|r| r.path.ends_with("/chat/completions")).unwrap();
    let body: Value = serde_json::from_str(&request.body).unwrap();
    let sent = body["messages"].to_string();
    assert!(
        sent.contains(
            r#"<radio-message from=\"alpha\" kind=\"msg\" to=\"you\" by=\"user\">\ntests pass on main\n</radio-message>"#
        ),
        "{sent}"
    );
    assert!(sent.contains("the radio message(s) above arrived while you were idle"), "{sent}");
    let tools: Vec<&str> = body["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert!(tools.contains(&"radio_tx"), "{tools:?}");
    wait_for("beta's wake line", || beta.screen().contains("✻Radiomessagefromalpha(user)."));
    assert!(!alpha_model.requests().iter().any(|r| r.path.ends_with("/chat/completions")), "nothing reached alpha's own model");
    drop((alpha, beta));
}

//! The mesh radio (#124), end to end: real `marv` TUIs, each in a pseudo-terminal (a small python3 helper), on one
//! machine's mesh. Phase 1: two with `"mesh": {"radio": true}` in their own config see each other in the strip above
//! the prompt and `marv mesh ls` says their radio is on; a third without it is on the mesh but not on the radio. One
//! killed with SIGKILL shows `left (lost)` on the other's strip, and its dead door socket is swept at once. Phase 2: a
//! person's `/tx` reaches the other Marv's model fenced, wakes it (wake on), and never reaches the sender's own model.
//! Skipped without python3.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use marv_oracle::server::FakeServer;
use serde_json::{Value, json};

/// Runs `argv` in a pseudo-terminal of 40×160, appends what it draws to `log`, types each line read from stdin
/// (with a carriage return), and prints the child's pid first. Once the child has exited (and been reaped), it waits
/// for everything left in the child's session (`pty.fork` makes it a session leader) to end too, killing it after
/// 10 s, prints `left <pids>` (what was still running then: nothing, after a clean exit) and `survived <pids>` (what
/// was still there 5 s after the SIGKILLs), and exits; `sweep failed: …` if it couldn't list the processes.
/// A process Marv started in a group of its own (a checkpoint's `git gc`, #191) is still in its session: one that
/// outlived a killed Marv kept writing into the machine's folder while the test removed it.
const PTY: &str = r#"
import fcntl, os, pty, select, signal, struct, subprocess, sys, termios, time
log, argv = sys.argv[1], sys.argv[2:]
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
    os.execv(argv[0], argv)
print(pid, flush=True)

def session():
    # Not `pgrep -s`: macOS's pgrep has no -s. A listing that fails raises: the check never passes for want of one.
    listed = subprocess.run(["ps", "-A", "-o", "pid="], stdout=subprocess.PIPE, text=True, check=True).stdout.split()
    if not listed:
        raise RuntimeError("ps listed no processes")
    found = []
    for p in map(int, listed):
        try:
            if p != pid and os.getsid(p) == pid:
                found.append(p)
        except (ProcessLookupError, PermissionError):
            pass
    return found

# Whether getsid sees marv's session (it drew something: it has exec'd, after pty.fork's setsid). If it can't, the
# sweep would find nothing whatever is running: it fails instead.
inputs, exited, seen = [fd, sys.stdin], False, None
with open(log, "ab", buffering=0) as out:
    while not exited:
        exited = os.waitpid(pid, os.WNOHANG)[0] == pid
        ready, _, _ = select.select(inputs, [], [], 0 if exited else 0.2)
        if fd in ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                data = b""
            if data:
                out.write(data)
                if seen is None:
                    try:
                        seen = os.getsid(pid) == pid
                    except OSError:
                        seen = False
            else:
                inputs.remove(fd)
        if sys.stdin in ready:
            line = sys.stdin.readline()
            if not line:
                inputs.remove(sys.stdin)
            elif not exited:
                os.write(fd, line.rstrip("\n").encode() + b"\r")
try:
    if not seen:
        raise RuntimeError("couldn't read marv's session id")
    deadline = time.monotonic() + 10
    while session() and time.monotonic() < deadline:
        time.sleep(0.1)
    left = session()
    for p in left:
        try:
            os.kill(p, signal.SIGKILL)
        except OSError:
            pass
    deadline = time.monotonic() + 5
    while session() and time.monotonic() < deadline:
        time.sleep(0.1)
    print("left", *left, flush=True)
    print("survived", *session(), flush=True)
except Exception as e:
    print("sweep failed:", e, flush=True)
    sys.exit(1)
"#;

fn python() -> bool {
    Command::new("python3").arg("-c").arg("import pty").status().is_ok_and(|s| s.success())
}

/// Where the test machines go: on macOS a short `/tmp`, as macOS's temp folder (`/var/folders/…/T`) leaves too little
/// of `sun_path`'s 104 bytes for the sockets (#150).
fn temp_base() -> PathBuf {
    if cfg!(target_os = "macos") { PathBuf::from("/tmp") } else { std::env::temp_dir() }
}

/// The variable that puts a Marv's runtime folder at `<run>/marv`: `XDG_RUNTIME_DIR=<run>` on Linux,
/// `MARV_RUNTIME_DIR=<run>/marv` on macOS, which has no `XDG_RUNTIME_DIR` (as `/tmp/…`, not the real `/private/tmp/…`:
/// 8 bytes fewer for the sockets).
fn runtime_var(run: &Path) -> (&'static str, PathBuf) {
    if cfg!(target_os = "macos") {
        let run = run.strip_prefix("/private").map_or_else(|_| run.to_path_buf(), |r| Path::new("/").join(r));
        ("MARV_RUNTIME_DIR", run.join("marv"))
    } else {
        ("XDG_RUNTIME_DIR", run.to_path_buf())
    }
}

/// A throwaway machine: a private runtime dir, a home, and a project and config per Marv.
struct Machine(PathBuf);

impl Machine {
    fn new() -> Self {
        // One per test: the tests of this file run at once, in one process.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = temp_base().join(format!("mx-r{n}-{}", std::process::id()));
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
        let (runtime_var, runtime_dir) = runtime_var(&self.0.join("run"));
        let mut command = Command::new(program);
        command
            .current_dir(self.0.join(format!("proj-{name}")))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.0.join("home"))
            .env("MARV_CONFIG_DIR", self.0.join(format!("config-{name}")))
            .env(runtime_var, runtime_dir)
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
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut pid = String::new();
        stdout.read_line(&mut pid).unwrap();
        let stdin = child.stdin.take();
        Tui { child, stdin, stdout, pid: pid.trim().parse().unwrap(), screen, killed: false, done: false }
    }

    fn ls(&self) -> String {
        let out = self.command("ls", env!("CARGO_BIN_EXE_marv")).current_dir("/").args(["mesh", "ls"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into()
    }
}

impl Drop for Machine {
    /// After every Tui has ended ([`Tui::exit`]), with nothing left running in its sessions: a process still writing
    /// in the folder makes the removal fail midway (`Directory not empty`), leaving the rest behind (#191).
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0)
            && !std::thread::panicking()
        {
            panic!("couldn't remove {}: {e}", self.0.display());
        }
    }
}

/// A `marv` TUI in its pseudo-terminal.
struct Tui {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The helper's: marv's pid came first; `left …` comes last.
    stdout: BufReader<ChildStdout>,
    /// marv's own pid (the helper's child).
    pid: i32,
    screen: PathBuf,
    /// Killed on purpose: no `/exit`.
    killed: bool,
    /// The helper has exited.
    done: bool,
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

    /// Whether the prompt is up: past the splash, which a key only dismisses (it ate the `/` of a `/radio on` typed too
    /// early on a slow macOS runner, #191), and in raw mode, where the pty's `\n` for a `\r` typed before it would be
    /// ctrl+j, not Enter.
    fn ready(&self) -> bool {
        self.screen().contains(PROMPT)
    }

    /// Types `line` and Enter, once the prompt is up.
    fn type_line(&mut self, line: &str) {
        wait_for("the prompt", || self.ready());
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// SIGKILL: no goodbye, no cleanup. What it started in a process group of its own may outlive it; the helper
    /// waits for (and then kills) what's left in its session.
    fn kill(&mut self) {
        self.killed = true;
        let _ = Command::new("kill").args(["-KILL", &self.pid.to_string()]).status();
    }

    /// Ends it and waits until marv and everything in its session has exited. Unless it was killed (or the test is
    /// already failing: no slow goodbyes then), it's asked to leave (`/exit`, once its prompt is up) and given a minute,
    /// as a person would. `Ok`: the pids still running 10 s after it exited (then killed); `Err`: the helper's sweep
    /// failed, or something survived the kill.
    fn exit(&mut self) -> Result<Vec<i32>, String> {
        if self.done {
            return Ok(Vec::new());
        }
        if std::thread::panicking() {
            self.kill();
        }
        if !self.killed {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !self.ready() && Instant::now() < deadline && self.child.try_wait().ok().flatten().is_none() {
                std::thread::sleep(Duration::from_millis(100));
            }
            if let Some(stdin) = self.stdin.as_mut() {
                let _ = stdin.write_all(b"/exit\n").and_then(|()| stdin.flush());
            }
        }
        // Its EOF: the helper types nothing more, and exits once marv has.
        self.stdin = None;
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.child.try_wait().ok().flatten().is_none() {
            if Instant::now() >= deadline {
                // It didn't leave: killed, and the helper still sweeps its session.
                self.kill();
                let _ = self.child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.done = true;
        let mut report = String::new();
        let _ = self.stdout.read_to_string(&mut report);
        let pids = |prefix: &str| -> Option<Vec<i32>> {
            let line = report.lines().find_map(|l| l.strip_prefix(prefix))?;
            Some(line.split_whitespace().filter_map(|p| p.parse().ok()).collect())
        };
        match (pids("left"), pids("survived")) {
            (Some(left), Some(survived)) if survived.is_empty() => Ok(left),
            (Some(_), Some(survived)) => Err(format!("still running after SIGKILL: {survived:?}")),
            _ => Err(format!("the helper's sweep didn't finish: {report:?}")),
        }
    }

    /// [`Self::exit`], asserting that a Marv that wasn't killed left nothing running.
    fn quit(mut self) {
        let left = self.exit().unwrap_or_else(|e| panic!("marv {}: {e}", self.pid));
        assert!(left.is_empty(), "still running 10 s after marv {} exited: {left:?}", self.pid);
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.exit();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The empty prompt's placeholder, blanks out (see [`Tui::screen`]).
const PROMPT: &str = "Typeamessage,or/forcommands";

/// Polls `ok` for up to two minutes: a slow runner's Marvs take a while to start, and a radio that missed a join
/// re-lists every 30 s.
fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
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
    let mut beta = machine.tui("beta", &model, true);
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
    beta.exit().unwrap();
    alpha.quit();
    gamma.quit();
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
    // beta's turn started a checkpoint prune, whose `git gc` (in a process group of its own) outlived a SIGKILLed beta
    // and kept writing in config-beta while the folder was removed (#191): a Marv that's asked to leave takes it along.
    alpha.quit();
    beta.quit();
}

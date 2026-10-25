//! #196: closing the terminal ends the TUI as `/exit` does. A real `marv` in a pseudo-terminal (a small python3
//! helper) runs a turn, whose checkpoint prune starts a `git gc` in a process group of its own; a `git` on PATH that
//! stands in for the real one makes that gc hang. The helper then closes the terminal (the pty's master): the kernel
//! hangs it up and sends marv SIGHUP. Marv must exit at once with 129, and take the gc along: afterwards nothing from
//! its session is still running. Before #196 SIGHUP's default ended marv on the spot and the gc lived on. SIGTERM, the
//! terminal still open, does the same and restores the terminal. Skipped without python3 or git.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use marv_oracle::script::Reply;
use marv_oracle::server::FakeServer;

/// Runs `argv` in a pseudo-terminal of 40×160, appends what it draws to `log`, types each line read from stdin (with
/// a carriage return), and prints the child's pid first. The line `HANGUP` closes the terminal instead, `TERM` sends
/// the child SIGTERM. Once the
/// child has exited (and been reaped), it prints `exit <code>` (-n: signal n ended it), waits up to 10 s for
/// everything left in the child's session (`pty.fork` makes it a session leader; a separate process group stays in
/// it) to end too, prints `left <pids>` (what was still running then), kills those and exits.
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
    found = []
    for p in map(int, listed):
        try:
            if p != pid and os.getsid(p) == pid:
                found.append(p)
        except (ProcessLookupError, PermissionError):
            pass
    return found

inputs, status = [fd, sys.stdin], None
with open(log, "ab", buffering=0) as out:
    while status is None:
        done, raw = os.waitpid(pid, os.WNOHANG)
        if done == pid:
            status = os.waitstatus_to_exitcode(raw)
        ready, _, _ = select.select(inputs, [], [], 0 if status is not None else 0.2)
        if fd in ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                data = b""
            if data:
                out.write(data)
            else:
                inputs.remove(fd)
        if sys.stdin in ready:
            line = sys.stdin.readline()
            if not line:
                inputs.remove(sys.stdin)
            elif line.strip() == "TERM":
                os.kill(pid, signal.SIGTERM)
            elif line.strip() == "HANGUP":
                if fd in inputs:
                    inputs.remove(fd)
                os.close(fd)
            elif status is None and fd in inputs:
                os.write(fd, line.rstrip("\n").encode() + b"\r")
print("exit", status, flush=True)
deadline = time.monotonic() + 10
while session() and time.monotonic() < deadline:
    time.sleep(0.1)
left = session()
for p in left:
    try:
        os.kill(p, signal.SIGKILL)
    except OSError:
        pass
print("left", *left, flush=True)
"#;

fn python() -> bool {
    Command::new("python3").arg("-c").arg("import pty").status().is_ok_and(|s| s.success())
}

/// The real git, from PATH.
fn real_git() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join("git")).find(|git| git.is_file())
}

/// The empty prompt's placeholder, blanks out (as tests/radio.rs reads the screen).
const PROMPT: &str = "Typeamessage,or/forcommands";

/// What was drawn, escape sequences and blanks out.
fn screen(log: &Path) -> String {
    let raw = String::from_utf8_lossy(&std::fs::read(log).unwrap_or_default()).into_owned();
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

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Whether process `pid` is still there (a zombie counts: it's no one's to reap here but the helper's).
fn running(pid: i32) -> bool {
    Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

#[test]
fn closing_the_terminal_during_a_prune_leaves_nothing_running() {
    quit_during_a_prune("HANGUP", "129");
}

/// SIGTERM, with the terminal still there: the same way out, and the terminal is restored (bracketed paste off, the
/// alternate screen left).
#[test]
fn a_sigterm_during_a_prune_leaves_nothing_running_and_restores_the_terminal() {
    let log = quit_during_a_prune("TERM", "143");
    if log.is_empty() {
        return; // skipped
    }
    let log = String::from_utf8_lossy(&log);
    // The last word on each: off.
    for mode in ["\x1b[?2004", "\x1b[?1049"] {
        let last = log.rfind(mode).map(|i| &log[i + mode.len()..i + mode.len() + 1]);
        assert_eq!(last, Some("l"), "{mode:?} not turned off at the end");
    }
}

/// A test's folder (home, project, config, a `bin` first on PATH) and what it started: on drop, even after a failed
/// assertion, the processes are SIGKILLed and the folder removed.
struct Machine {
    dir: PathBuf,
    /// marv, the stand-in gc, a hook.
    pids: Vec<i32>,
    children: Vec<std::process::Child>,
}

impl Machine {
    /// `name` keeps the tests' folders apart (they run at once); `config` is merged into the fake server's config.
    fn new(name: &str, server: &FakeServer, config: serde_json::Value) -> Self {
        // A short folder on macOS, as tests/radio.rs: the runtime folder's sockets need room in sun_path.
        let base = if cfg!(target_os = "macos") { PathBuf::from("/tmp") } else { std::env::temp_dir() };
        let dir = base.join(format!("mx-h{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["run", "home", "proj", "config", "bin"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let machine = Self { dir: dir.canonicalize().unwrap(), pids: Vec::new(), children: Vec::new() };
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(machine.dir.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut settings = serde_json::Value::Object(marv_oracle::runner::default_config(&server.base_url()));
        if let serde_json::Value::Object(extra) = config {
            settings.as_object_mut().unwrap().extend(extra);
        }
        std::fs::write(machine.dir.join("config/config.json"), settings.to_string()).unwrap();
        machine
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// `program` in the project, with only this machine's environment.
    fn command(&self, program: &str) -> Command {
        let mut path = vec![self.dir.join("bin")];
        path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        let (runtime_var, runtime_dir) = if cfg!(target_os = "macos") {
            let run = self.dir.join("run");
            let run = run.strip_prefix("/private").map_or_else(|_| run.clone(), |r| Path::new("/").join(r));
            ("MARV_RUNTIME_DIR", run.join("marv"))
        } else {
            ("XDG_RUNTIME_DIR", self.dir.join("run"))
        };
        let mut command = Command::new(program);
        command
            .current_dir(self.dir.join("proj"))
            .env_clear()
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("HOME", self.dir.join("home"))
            .env("MARV_CONFIG_DIR", self.dir.join("config"))
            .env(runtime_var, runtime_dir)
            .env("MARV_GITHUB", "off")
            .env("TERM", "xterm-256color")
            .env("LANG", "C.UTF-8");
        command
    }

    /// The pid a script wrote to `file` (once it has).
    fn pid_in(&self, file: &str) -> Option<i32> {
        std::fs::read_to_string(self.path(file)).ok()?.trim().parse().ok()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        for pid in &self.pids {
            let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).stderr(Stdio::null()).status();
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Starts marv in a pty, runs a turn whose prune's gc hangs, then sends `how` (a helper line) and checks marv exited
/// with `code` and left nothing running. Returns what marv wrote to the terminal.
fn quit_during_a_prune(how: &str, code: &str) -> Vec<u8> {
    let (true, Some(git)) = (python(), real_git()) else {
        eprintln!("skipped: no python3 with pty, or no git");
        return Vec::new();
    };
    let server = FakeServer::start(vec![Reply { text: "hello there".into(), ..Reply::default() }], vec![]).unwrap();
    let mut machine = Machine::new(&how.to_lowercase(), &server, serde_json::Value::Null);

    // The stand-in git: a gc writes its pid and hangs (it stays git's process, in git's group); anything else is git.
    let marker = machine.path("gc.pid");
    let shim = machine.path("bin/git");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = gc ]; then echo $$ > '{}'; exec sleep 600; fi\ndone\nexec '{}' \"$@\"\n",
            marker.display(),
            git.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

    let log = machine.path("screen.log");
    let mut helper = machine
        .command("python3")
        .arg("-c")
        .arg(PTY)
        .arg(&log)
        .arg(env!("CARGO_BIN_EXE_marv"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(helper.stdout.take().unwrap());
    let mut stdin = helper.stdin.take().unwrap();
    machine.children.push(helper);
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let marv: i32 = line.trim().parse().unwrap();
    machine.pids.push(marv);

    wait_for("the prompt", || screen(&log).contains(PROMPT));
    stdin.write_all(b"hi\n").unwrap();
    stdin.flush().unwrap();
    wait_for("the prune's gc", || machine.pid_in("gc.pid").is_some());
    let gc = machine.pid_in("gc.pid").unwrap();
    machine.pids.push(gc);
    assert!(running(gc), "the stand-in gc is running");

    let hung_up = Instant::now();
    stdin.write_all(format!("{how}\n").as_bytes()).unwrap();
    stdin.flush().unwrap();
    drop(stdin);
    let mut report = String::new();
    stdout.read_to_string(&mut report).unwrap();
    let field = |prefix: &str| report.lines().find_map(|l| l.strip_prefix(prefix)).map(str::trim).map(String::from);
    let exit = field("exit").unwrap_or_else(|| panic!("the helper didn't finish: {report:?}"));
    let left = field("left").unwrap_or_else(|| panic!("the helper didn't sweep: {report:?}"));

    assert_eq!(exit, code, "marv {marv} ended by the signal's default (-n), or not through its way out: {report:?}");
    assert!(left.is_empty(), "still running 10 s after marv {marv} exited: {left:?}");
    assert!(!running(gc), "the prune's gc {gc} outlived marv");
    // Well within the 10 s deadline plus the helper's 10 s sweep: marv didn't wait for the 600 s gc.
    assert!(hung_up.elapsed() < Duration::from_secs(14), "took {:?}", hung_up.elapsed());
    std::fs::read(&log).unwrap_or_default()
}

/// `marv -p` with a session-start hook that writes its pid to `hook.pid` and then sleeps `sleep` seconds, started by
/// `sh -c '<prefix> exec marv -p hi'` (so marv keeps the shell's pid and its ignored signals). Waits for the hook,
/// sends marv SIGHUP, and returns marv's exit code, its stdout, how long it took after the signal, and the hook's pid.
fn sighup_during_a_hook(name: &str, prefix: &str, sleep: u32) -> (Option<i32>, String, Duration, i32, Machine) {
    let server = FakeServer::start(vec![Reply { text: "hello there".into(), ..Reply::default() }], vec![]).unwrap();
    let hook = format!("echo $$ > hook.pid; sleep {sleep}");
    let mut machine =
        Machine::new(name, &server, serde_json::json!({"hooks": {"sessionStart": [{"command": hook, "timeout": 60}]}}));
    let script = format!("{prefix} exec '{}' -p hi", env!("CARGO_BIN_EXE_marv"));
    let child = machine
        .command("sh")
        .args(["-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let marv = child.id() as i32;
    machine.pids.push(marv);
    machine.children.push(child);
    wait_for("the hook", || machine.pid_in("proj/hook.pid").is_some());
    let hook = machine.pid_in("proj/hook.pid").unwrap();
    machine.pids.push(hook);
    let signalled = Instant::now();
    let _ = Command::new("kill").args(["-HUP", &marv.to_string()]).status();
    let child = machine.children.last_mut().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "marv -p didn't exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    let took = signalled.elapsed();
    let mut out = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut out);
    (status.code(), out, took, hook, machine)
}

/// `marv -p` gets SIGHUP during a slow session-start hook: it starts no turn, kills the hook, and exits 129 at once.
#[test]
fn marv_p_leaves_at_once_on_sighup_during_a_hook() {
    let (code, out, took, hook, _machine) = sighup_during_a_hook("p", "", 30);
    assert_eq!(code, Some(129), "stdout: {out:?}");
    assert!(!out.contains("hello there"), "no turn: {out:?}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while running(hook) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!running(hook), "the hook {hook} outlived marv");
}

/// Under `nohup` (SIGHUP inherited as ignored), SIGHUP stays ignored: the run finishes and answers.
#[test]
fn marv_p_under_an_ignored_sighup_finishes_its_run() {
    let (code, out, _, _, _machine) = sighup_during_a_hook("n", "trap '' HUP;", 2);
    assert_eq!(code, Some(0), "stdout: {out:?}");
    assert!(out.contains("hello there"), "{out:?}");
}

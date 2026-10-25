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

/// Starts marv in a pty, runs a turn whose prune's gc hangs, then sends `how` (a helper line) and checks marv exited
/// with `code` and left nothing running. Returns what marv wrote to the terminal.
fn quit_during_a_prune(how: &str, code: &str) -> Vec<u8> {
    let (true, Some(git)) = (python(), real_git()) else {
        eprintln!("skipped: no python3 with pty, or no git");
        return Vec::new();
    };
    // A short folder on macOS, as tests/radio.rs: the runtime folder's sockets need room in sun_path.
    let base = if cfg!(target_os = "macos") { PathBuf::from("/tmp") } else { std::env::temp_dir() };
    let dir = base.join(format!("mx-h{}-{}", how.to_lowercase(), std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["run", "home", "proj", "config", "bin"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = dir.canonicalize().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("run"), std::fs::Permissions::from_mode(0o700)).unwrap();

    // The stand-in git: a gc writes its pid and hangs (it stays git's process, in git's group); anything else is git.
    let marker = dir.join("gc.pid");
    let shim = dir.join("bin/git");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = gc ]; then echo $$ > '{}'; exec sleep 600; fi\ndone\nexec '{}' \"$@\"\n",
            marker.display(),
            git.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

    let server = FakeServer::start(vec![Reply { text: "hello there".into(), ..Reply::default() }], vec![]).unwrap();
    let settings = serde_json::Value::Object(marv_oracle::runner::default_config(&server.base_url()));
    std::fs::write(dir.join("config/config.json"), settings.to_string()).unwrap();
    let mut path = vec![dir.join("bin")];
    path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    let (runtime_var, runtime_dir) = if cfg!(target_os = "macos") {
        let run = dir.join("run");
        let run = run.strip_prefix("/private").map_or_else(|_| run.clone(), |r| Path::new("/").join(r));
        ("MARV_RUNTIME_DIR", run.join("marv"))
    } else {
        ("XDG_RUNTIME_DIR", dir.join("run"))
    };
    let log = dir.join("screen.log");
    let mut helper = Command::new("python3")
        .arg("-c")
        .arg(PTY)
        .arg(&log)
        .arg(env!("CARGO_BIN_EXE_marv"))
        .current_dir(dir.join("proj"))
        .env_clear()
        .env("PATH", std::env::join_paths(path).unwrap())
        .env("HOME", dir.join("home"))
        .env("MARV_CONFIG_DIR", dir.join("config"))
        .env(runtime_var, runtime_dir)
        .env("MARV_GITHUB", "off")
        .env("TERM", "xterm-256color")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(helper.stdout.take().unwrap());
    let mut stdin = helper.stdin.take().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let marv: i32 = line.trim().parse().unwrap();

    wait_for("the prompt", || screen(&log).contains(PROMPT));
    stdin.write_all(b"hi\n").unwrap();
    stdin.flush().unwrap();
    wait_for("the prune's gc", || std::fs::read_to_string(&marker).is_ok_and(|s| s.trim().parse::<i32>().is_ok()));
    let gc: i32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    assert!(running(gc), "the stand-in gc is running");

    let hung_up = Instant::now();
    stdin.write_all(format!("{how}\n").as_bytes()).unwrap();
    stdin.flush().unwrap();
    drop(stdin);
    let mut report = String::new();
    stdout.read_to_string(&mut report).unwrap();
    let _ = helper.wait();
    let field = |prefix: &str| report.lines().find_map(|l| l.strip_prefix(prefix)).map(str::trim).map(String::from);
    let exit = field("exit").unwrap_or_else(|| panic!("the helper didn't finish: {report:?}"));
    let left = field("left").unwrap_or_else(|| panic!("the helper didn't sweep: {report:?}"));
    let still_gc = running(gc);
    if still_gc {
        let _ = Command::new("kill").args(["-KILL", &gc.to_string()]).status();
    }
    let drawn = std::fs::read(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(exit, code, "marv {marv} ended by the signal's default (-n), or not through its way out: {report:?}");
    assert!(left.is_empty(), "still running 10 s after marv {marv} exited: {left:?}");
    assert!(!still_gc, "the prune's gc {gc} outlived marv");
    // Well within the 5 s deadline plus the helper's 10 s sweep: marv didn't wait for the 600 s gc.
    assert!(hung_up.elapsed() < Duration::from_secs(14), "took {:?}", hung_up.elapsed());
    drawn
}

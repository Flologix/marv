//! `marv __guard` (#165) as the real binary runs it, outside any sandbox (so on Linux and macOS alike): the command's
//! exit code comes back, it runs in a process group of the guard's own, a stop is passed on to everything in that
//! group, and nothing in it outlives the guard's parent, even when that parent is SIGKILLed.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

fn guard(command: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marv"));
    cmd.arg("__guard").arg(std::process::id().to_string()).arg("--").args(command);
    cmd
}

/// The first line the child prints, as numbers.
fn first_line(child: &mut Child) -> Vec<i32> {
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    line.split_whitespace().map(|n| n.parse().unwrap()).collect()
}

/// Whether `pid` is still running (a zombie counts as gone), waiting up to 5 s for it to end.
fn ends(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let zombie = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with('Z'))
            .unwrap_or(false);
        if kill(Pid::from_raw(pid), None).is_err() || zombie {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    false
}

#[test]
fn the_commands_exit_code_comes_back_and_a_signal_is_128_plus_n() {
    assert_eq!(guard(&["bash", "-c", "exit 7"]).status().unwrap().code(), Some(7));
    assert_eq!(guard(&["bash", "-c", "kill -9 $$"]).status().unwrap().code(), Some(137));
    let out = guard(&["no-such-command-marv-guard"]).output().unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("couldn't start"));
}

#[test]
fn the_command_runs_in_the_guards_own_process_group_and_session() {
    let out = guard(&["bash", "-c", "echo $PPID $(ps -o pgid= -p $$) $(ps -o pgid= -p $PPID)"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let ids: Vec<i32> = String::from_utf8_lossy(&out.stdout).split_whitespace().map(|n| n.parse().unwrap()).collect();
    let (guard, group, guards_group) = (ids[0], ids[1], ids[2]);
    assert_eq!(group, guard, "the command's group is the guard's pid");
    assert_eq!(guards_group, guard, "the guard leads it");
    assert_ne!(group, nix::unistd::getpgrp().as_raw(), "not the test's group");
}

#[test]
fn a_parent_that_isnt_the_guards_runs_nothing() {
    let out = Command::new(env!("CARGO_BIN_EXE_marv"))
        .args(["__guard", &(std::process::id() + 1).to_string(), "--", "echo", "ran"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn a_stop_reaches_everything_in_the_group_and_the_guard_exits_with_the_commands_code() {
    let mut child =
        guard(&["bash", "-c", "sleep 600 & echo $!; wait"]).stdout(Stdio::piped()).stdin(Stdio::null()).spawn().unwrap();
    let sleeper = first_line(&mut child)[0];
    // To the guard alone, not its group: passing it on is the guard's job.
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = nix::sys::signal::killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
            panic!("the guard didn't exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(128 + 15));
    assert!(ends(sleeper), "the background sleeper outlived the stop");
}

#[test]
fn nothing_in_the_group_outlives_a_parent_killed_with_sigkill() {
    // The guard's parent is this bash; the command prints the guard's pid and its background sleeper's.
    let script = r#""$1" __guard $$ -- bash -c 'sleep 600 & echo $PPID $!; wait' & wait"#;
    let mut parent = Command::new("bash")
        .args(["-c", script, "parent", env!("CARGO_BIN_EXE_marv")])
        .stdout(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let ids = first_line(&mut parent);
    let (guard, sleeper) = (ids[0], ids[1]);
    parent.kill().unwrap();
    parent.wait().unwrap();
    let took = Instant::now();
    assert!(ends(sleeper), "the sleeper outlived the guard's parent");
    assert!(ends(guard), "the guard outlived its parent");
    assert!(took.elapsed() < Duration::from_secs(3), "noticed only after {:?}", took.elapsed());
}

#[test]
fn a_stop_is_passed_on_once_not_again_for_the_guards_own_copy() {
    // The command survives SIGTERM and counts how many it got until the test creates the marker, 1 s after the stop.
    let marker = std::env::temp_dir().join(format!("marv-guard-once-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = r#"n=0; trap 'n=$((n+1))' TERM; echo 0; while [ ! -e "$1" ]; do sleep 0.1; done; echo $n"#;
    let marker_arg = marker.to_str().unwrap();
    let mut child =
        guard(&["bash", "-c", script, "count", marker_arg]).stdout(Stdio::piped()).stdin(Stdio::null()).spawn().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    std::fs::write(&marker, "").unwrap();
    line.clear();
    out.read_line(&mut line).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert_eq!(line.trim(), "1", "the command got the stop {} times", line.trim());
}

#[test]
fn a_guard_that_cant_have_a_session_of_its_own_runs_nothing() {
    use std::os::unix::process::CommandExt;
    // A group leader can't `setsid`: in a group of its own it would still be in the test's session.
    let out = guard(&["echo", "ran"]).process_group(0).output().unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert!(String::from_utf8_lossy(&out.stderr).contains("session"));
}

#[test]
fn other_signals_that_would_end_the_guard_are_passed_on_too() {
    // Not SIGQUIT here: bash starts a background job with SIGINT and SIGQUIT ignored.
    for signal in [Signal::SIGUSR1, Signal::SIGUSR2, Signal::SIGALRM] {
        let code = 128 + signal as i32;
        let mut child =
            guard(&["bash", "-c", "sleep 600 & echo $!; wait"]).stdout(Stdio::piped()).stdin(Stdio::null()).spawn().unwrap();
        let sleeper = first_line(&mut child)[0];
        kill(Pid::from_raw(child.id() as i32), signal).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = nix::sys::signal::killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
                panic!("the guard didn't exit after {signal}");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // First, so a failure leaves nothing running: `ends` kills a sleeper that outlives its 5 s.
        assert!(ends(sleeper), "the background sleeper outlived {signal}");
        // The guard's own code is the command's: it was passed on, not the guard's death.
        assert_eq!(status.code(), Some(code), "{signal}");
    }
}

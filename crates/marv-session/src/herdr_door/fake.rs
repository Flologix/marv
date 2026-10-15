//! A fake `herdr` for tests (this crate's and the guard suite's): never the user's real Herdr. See `fake_herdr.sh` for
//! what it answers. Its state and log live in the folder it's written to, so each test has its own Herdr.

use std::path::{Path, PathBuf};
use std::time::Duration;

const SCRIPT: &str = include_str!("fake_herdr.sh");

/// Writes an executable `herdr` into `dir` and waits until it can run; returns its path.
pub fn install_fake_herdr(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let herdr = dir.join("herdr");
    std::fs::write(&herdr, SCRIPT).unwrap();
    std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o755)).unwrap();
    wait_until_runnable(&herdr);
    herdr
}

/// Each call the fake took, as its arguments joined by spaces, in order.
pub fn fake_herdr_log(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("log")).unwrap_or_default().lines().map(str::to_string).collect()
}

/// Sets the state Herdr reports for the agent in a pane the door opened (`working`, `blocked`…).
pub fn set_pane_state(dir: &Path, pane: &str, state: &str) {
    std::fs::write(dir.join(format!("state-{pane}")), format!("{state}\n")).unwrap();
}

/// Closes a pane behind Marv's back (the user closed it, its program exited): Herdr no longer has it.
pub fn close_pane_outside(dir: &Path, pane: &str) {
    let _ = std::fs::remove_file(dir.join(format!("opened-{pane}")));
    let _ = std::fs::remove_file(dir.join(format!("agent-{pane}")));
}

/// The door's setup with the fake in `dir`: Marv in pane w1:p1, Marv's binary a stand-in path.
pub fn fake_setup(dir: &Path, max_peers: u32) -> super::HerdrSetup {
    super::HerdrSetup {
        program: install_fake_herdr(dir),
        own_pane: Some("w1:p1".into()),
        marv: PathBuf::from("/opt/marv/bin/marv"),
        max_peers,
        runs: true,
    }
}

/// Waits until a script just written can be run (#92): another test's thread may have forked while it was open for
/// writing, and Linux refuses to run a file open for writing ("Text file busy") until that child execs.
fn wait_until_runnable(program: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match std::process::Command::new(program).arg("--ready").status() {
            Ok(status) => return assert!(status.success(), "fake herdr --ready: {status}"),
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => panic!("fake herdr didn't run: {e}"),
        }
    }
}

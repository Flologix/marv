//! `marv __relay` as the published binary runs it (Marv's "the relay's source still runs after bundling, as in the
//! published CLI": its bundled `require` once broke the relay). Here the relay is compiled in, so the check is that the
//! real binary answers the hidden subcommand, listens, runs the command and exits with its code.

use std::process::Command;

#[test]
fn the_marv_binary_runs_the_relay_listens_and_exits_with_the_commands_code() {
    let dir = std::env::temp_dir().join(format!("marv-relay-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("r.sock");
    let spec = serde_json::json!({
        "listen": socket,
        "argv": ["bash", "-c", format!("test -S {} && echo relay-ran; exit 7", socket.display())],
    });
    let out = Command::new(env!("CARGO_BIN_EXE_marv")).args(["__relay", &spec.to_string()]).output().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "relay-ran\n");
    assert_eq!(out.status.code(), Some(7));
}

#[test]
fn a_bad_spec_is_refused() {
    let out = Command::new(env!("CARGO_BIN_EXE_marv")).args(["__relay", "nope"]).output().unwrap();
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("the spec isn't valid"));
}

//! `docs/SECURITY.md`, "Process and launch".

use std::collections::BTreeMap;
use std::path::Path;

use marv_core::printable;
use marv_guards::TempDir;
use marv_session::config::default_config_dir;

/// **One static binary**: no interpreter or loader reads the project's `.env`, `bunfig.toml` or `node_modules`. Bun
/// loaded a cloned repository's `.env` and `bunfig.toml` (preload scripts) before Marv's first line ran; the Rust
/// binary has no such loader, and this keeps one from arriving as a dependency.
///
/// Proven: added a `name = "dotenvy"` package to `Cargo.lock` → this test failed.
#[test]
fn no_dotenv_loader_is_linked_in() {
    let lock = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock")).unwrap();
    for loader in ["dotenv", "dotenvy", "dotenv-flow", "envfile"] {
        assert!(!lock.contains(&format!("name = \"{loader}\"")), "{loader} would read the project's .env");
    }
}

/// **Config only from `~/.marv` or `$MARV_CONFIG_DIR` in the real environment**, never from a file in the project:
/// a project `.env` naming `MARV_CONFIG_DIR` (which Bun loaded for Marv 0.3.0) points nowhere.
///
/// Proven: changed `default_config_dir` to ignore `env` (always `~/.marv`) → this test failed; there is no project
/// argument to revert to, which is the guard.
#[test]
fn the_config_folder_comes_only_from_the_real_environment() {
    let project = TempDir::new("launch");
    project.write(".env", "MARV_CONFIG_DIR=./evil\n");
    project.write("bunfig.toml", "preload = [\"./evil.ts\"]\n");
    let home = Path::new("/home/me");
    // What the binary builds from the process environment: the project's files aren't in it.
    let clean: BTreeMap<String, String> = BTreeMap::new();
    assert_eq!(default_config_dir(&clean, home), home.join(".marv"));
    let real: BTreeMap<String, String> = [("MARV_CONFIG_DIR".to_string(), "/srv/marv".to_string())].into();
    assert_eq!(default_config_dir(&real, home), Path::new("/srv/marv"));
}

/// **Model output is never written to the terminal raw**: `printable()` strips CSI, OSC (OSC 52 writes the
/// clipboard), DCS, C0/C1 controls (except tab and newline) and bidi overrides (Trojan Source in an approval).
///
/// Proven: made `is_bidi` return false → this test failed; made `printable` keep `ESC` sequences (skipped the
/// `escape_end` branch) → this test failed.
#[test]
fn printable_strips_escapes_controls_and_bidi() {
    let hostile = "ok\x1b]52;c;Y3VybCB4fHNo\x07 \x1b[2J\x1b[31mred\x1b[0m \x1bPq#0\x1b\\ \u{9b}1;1H\r\x08\x7f\u{202e}gnp.exe\u{2066}x\u{2069}\tend\n";
    let shown = printable(hostile);
    assert_eq!(shown, "ok red  gnp.exex\tend\n");
    for c in shown.chars() {
        assert!(c == '\t' || c == '\n' || !c.is_control(), "{c:?} left in");
        assert!(!marv_core::printable::is_bidi(c), "{c:?} left in");
    }
}

/// **Headless text mode too**: Marv 0.3.0 wrote `marv -p` replies raw. A scripted reply carrying OSC 52 and a bidi
/// override reaches stdout only as printable text.
///
/// Proven: not yet (needs #11, `marv -p` end to end); the collector's `text_output` is unit-tested in marv-cli.
#[test]
#[ignore = "needs #11 (headless marv -p)"]
fn headless_text_output_is_printable() {
    use marv_oracle::runner::{MODEL, default_config};
    use marv_oracle::script::Reply;
    use marv_oracle::server::FakeServer;

    let reply = Reply { text: "done\x1b]52;c;ZXZpbA==\x07\u{202e}txt.exe".into(), ..Reply::default() };
    let server = FakeServer::start(vec![reply], vec![serde_json::json!({"id": MODEL})]).unwrap();
    let dirs = TempDir::new("headless");
    let (home, config, project) = (dirs.path().join("home"), dirs.path().join("config"), dirs.path().join("project"));
    for d in [&home, &config, &project] {
        std::fs::create_dir_all(d).unwrap();
    }
    let cfg = serde_json::Value::Object(default_config(&server.base_url()));
    std::fs::write(config.join("config.json"), cfg.to_string()).unwrap();
    let out = std::process::Command::new(marv_oracle::binary())
        .args(["-p", "hi", "--output", "text"])
        .current_dir(&project)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("MARV_CONFIG_DIR", &config)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("done"), "{stdout}");
    assert!(!stdout.contains('\x1b') && !stdout.contains('\x07') && !stdout.contains('\u{202e}'), "{stdout:?}");
}

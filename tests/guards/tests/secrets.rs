//! `docs/SECURITY.md`, "Marv's own secrets never come back in a tool result": the provider keys Marv was configured
//! with (and its MCP servers' tokens) are taken out of every tool result in `run_tool`, before the result reaches the
//! model, the stream, the saved session or the trajectory. On Terminal-Bench (2026-11-06) a model in a container
//! `cat`ed Marv's config.json and the OpenRouter key went into all four. Fake keys only.

use marv_core::REDACTED_SECRET;
use marv_guards::{TempDir, call};
use marv_oracle::script::{Reply, ScriptCall};
use serde_json::json;

const FAKE_KEY: &str = "sk-or-v1-guard-fake-0123456789abcdef";
const FAKE_MCP_TOKEN: &str = "ghp-guard-fake-mcp-token-0123456789";

/// Every file under `dir`, recursively, as text (unreadable ones skipped).
fn texts_under(dir: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(texts_under(&path));
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            out.push((path, text));
        }
    }
    out
}

/// **A tool result never carries a configured secret**: `run_tool` replaces it, whatever the tool (here read_file of
/// a project file holding the key and an MCP token), in the output and the label alike.
///
/// Proven: made `registry::redacted` return the outcome unchanged → failed (the key was in the output).
#[tokio::test]
async fn run_tool_redacts_configured_secrets_from_every_result() {
    let root = TempDir::new("secrets-run-tool");
    root.write("leak.json", &format!("{{\"apiKey\": \"{FAKE_KEY}\", \"token\": \"{FAKE_MCP_TOKEN}\"}}\n"));
    let mut ctx = marv_tools::ToolContext::new(root.path());
    ctx.secrets = std::sync::Arc::new(marv_core::Secrets::new([FAKE_KEY.to_string(), FAKE_MCP_TOKEN.to_string()]));
    let out = marv_tools::run_tool(&call("read_file", json!({"path": "leak.json"})), &ctx, marv_tools::builtin_tools()).await;
    assert!(!out.result.output.contains(FAKE_KEY) && !out.result.output.contains(FAKE_MCP_TOKEN), "{}", out.result.output);
    assert_eq!(out.result.output.matches(REDACTED_SECRET).count(), 2, "{}", out.result.output);
    // A grep for the key itself: its label (the pattern) is redacted too.
    let grep = marv_tools::run_tool(&call("grep", json!({"pattern": FAKE_KEY})), &ctx, marv_tools::builtin_tools()).await;
    assert!(!grep.label.contains(FAKE_KEY) && !grep.result.output.contains(FAKE_KEY), "{} {}", grep.label, grep.result.output);
}

/// **End to end, `marv -p --dangerously-run-unsandboxed` that `cat`s Marv's own config**: the OpenRouter key from
/// config.json and an MCP server's token from mcp.json never reach the model (the requests the fake OpenRouter got),
/// the stream (stdout), the saved session or the trajectory; the model reads the redaction instead.
///
/// Proven: made `registry::redacted` return the outcome unchanged → failed (the key was in the request); dropped the
/// `ctx.secrets = …` line from the session's turn → failed the same way; dropped `mcp_secrets` from it → failed (the
/// MCP token was in the request).
#[test]
fn an_unsandboxed_run_that_cats_marvs_config_never_sends_its_keys() {
    let (root, config_dir) = (TempDir::new("secrets-e2e"), TempDir::new("secrets-e2e-config"));
    let config_path = config_dir.path().join("config.json");
    let mcp_path = config_dir.path().join("mcp.json");
    let command = format!("cat {} {}", config_path.display(), mcp_path.display());
    let replies = vec![
        Reply {
            tool_calls: vec![ScriptCall { id: Some("c1".into()), name: "bash".into(), arguments: json!({"command": command}) }],
            ..Reply::default()
        },
        Reply { text: "Done.".into(), ..Reply::default() },
    ];
    let server = marv_oracle::server::FakeServer::start(replies, vec![json!({"id": marv_oracle::runner::MODEL})]).unwrap();
    let mut config = marv_oracle::runner::default_config(&server.base_url());
    config.insert("apiKey".into(), json!(FAKE_KEY));
    config.insert("trajectories".into(), json!(true));
    std::fs::write(&config_path, serde_json::Value::Object(config).to_string()).unwrap();
    // A personal MCP server that exits at once: its config (and token) is loaded whether or not it connects.
    let mcp = json!({"mcpServers": {"gh": {"command": "true", "env": {"GITHUB_TOKEN": FAKE_MCP_TOKEN}}}});
    std::fs::write(&mcp_path, mcp.to_string()).unwrap();
    let home = config_dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = std::process::Command::new(marv_oracle::binary())
        .args(["-p", "do it", "--output", "stream-json", "--dangerously-run-unsandboxed"])
        .current_dir(root.path())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("MARV_CONFIG_DIR", config_dir.path())
        .env("NO_COLOR", "1")
        .env("MARV_GITHUB", "off")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let bodies: Vec<String> = server.requests().into_iter().filter(|r| r.method == "POST").map(|r| r.body).collect();
    assert!(bodies.len() >= 2, "the command's result was never sent: {stdout}\n{stderr}");
    let sent = bodies.join("\n");
    // The command ran and its result was sent, redacted.
    assert!(sent.contains(REDACTED_SECRET), "no redaction in what was sent:\n{sent}\n{stderr}");
    for secret in [FAKE_KEY, FAKE_MCP_TOKEN] {
        assert!(!sent.contains(secret), "{secret} was sent to the model");
        assert!(!stdout.contains(secret), "{secret} is in the stream");
        for sub in ["trajectories", "sessions", "tool-output"] {
            for (path, text) in texts_under(&config_dir.path().join(sub)) {
                assert!(!text.contains(secret), "{secret} is in {}", path.display());
            }
        }
    }
    // The trajectory was written (so the check above looked at something).
    assert!(!texts_under(&config_dir.path().join("trajectories")).is_empty(), "no trajectory written\n{stderr}");
}

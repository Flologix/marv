//! `marv mesh ls|wire|plug`, and the process's mesh (new in marv-rs; docs/MESH.md topology 2, docs/subsystems/mesh.md).
//!
//! - [`start`]: the switchboard of a `marv` process (the TUI's, or `marv -p`'s when asked for), named by
//!   `marv_mesh::discovery::mesh_name`, with Herdr's agent panes as receptacles when Marv runs in a Herdr pane. Its
//!   door ([`Mesh::open_door`]) puts it on the mesh: `$XDG_RUNTIME_DIR/marv/mesh/<name>.sock`. No `XDG_RUNTIME_DIR`, no
//!   mesh (never a `/tmp` fallback).
//! - `ls`: the live switchboards in `$XDG_RUNTIME_DIR/marv/mesh/` (owner and mode checked), one line each, and in a
//!   Herdr pane the agent panes it can reach. Everything printed goes through `printable()`.
//! - `wire 'alpha -> beta:inbox'`: asks `alpha` for a wire from its `parent` to `beta`'s parent inbox. `alpha`'s user
//!   approves, then `beta`'s (both ends, `Grant::remote`). Ends are `<name>[/<agent>]`; the target can also be one of
//!   `alpha`'s receptacles by name (`alpha -> herdr.pane.w1-p2.send:call`).
//! - `plug <name> [--as <plug>]`: the stdio↔socket shim. Another agent (Claude Code, opencode) runs it as an MCP
//!   server and gets a plug of its own on `<name>`'s switchboard, with no wires until the user wires it.
//!
//! **marv-rs difference from MESH.md rule 1:** running `marv mesh wire` asks, it doesn't approve. Any process of the
//! user's can run it, an unsandboxed agent in another pane included, so each end's Marv still asks its person (or
//! its `--approve mesh:<receptacle>` list headless).

use std::path::Path;
use std::sync::Arc;

use marv_core::printable;
use marv_mesh::discovery::{CONNECT, DoorClient, PLUG, PeerSwitchboard, check_socket, mesh_dir, peer_receptacle, session_socket};
use marv_mesh::herdr::{HerdrAdapter, PaneAction, herdr_target, pane_receptacle};
use marv_mesh::plug::{MODE_META, MODES_META};
use marv_mesh::{PlugId, ReceptacleName, WireMode};
use marv_session::mesh::PARENT;
use serde_json::{Map, Value, json};

pub use marv_session::mesh_process::{herdr_program, start};

fn env() -> marv_mcp::Env {
    std::env::vars().collect()
}

fn no_runtime_dir(command: &str) -> i32 {
    eprintln!(
        "marv mesh {command} needs XDG_RUNTIME_DIR under /run, /tmp or your home folder: Marv puts the mesh only where sandboxed commands can't see it."
    );
    1
}

/// `~/x` for a folder under the home folder.
fn short(path: &str) -> String {
    let home = marv_session::paths::home_dir();
    match Path::new(path).strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.to_string(),
    }
}

/// One line per live switchboard: name, whether its radio is on (#124), pane, agents, folder, and the receptacle
/// another Marv reaches its parent by.
fn ls_lines(peers: &[PeerSwitchboard]) -> Vec<String> {
    let width = peers.iter().map(|p| p.session.len()).max().unwrap_or(0);
    peers
        .iter()
        .map(|p| {
            let inbox = ReceptacleName::inbox(&PlugId::new(PARENT).expect("a plug id"));
            let reach = peer_receptacle(&p.session, &inbox).map(|r| r.to_string()).unwrap_or_default();
            printable(&format!(
                "{:width$}  radio {:<3}  pane {}  agents {}  {}  ({})",
                p.session,
                if p.info.radio { "on" } else { "off" },
                p.info.pane.as_deref().unwrap_or("-"),
                p.info.agents.join(","),
                short(&p.info.cwd),
                reach,
            ))
        })
        .collect()
}

/// `marv mesh ls`. Returns the exit code.
pub async fn ls() -> i32 {
    let env = env();
    let Some(dir) = mesh_dir(&env) else { return no_runtime_dir("ls") };
    let peers = match marv_mesh::discovery::list(&dir).await {
        Ok(peers) => peers,
        Err(e) => {
            eprintln!("{}", printable(&e.to_string()));
            return 1;
        }
    };
    if peers.is_empty() {
        println!("No Marv is on the mesh here.");
    }
    for line in ls_lines(&peers) {
        println!("{line}");
    }
    // In Herdr: the agent panes a Marv here can be wired to (reading Herdr's list changes nothing).
    // Never a herdr from the folder `marv mesh ls` runs in (a project's node_modules/.bin, `.` on PATH).
    let here: Vec<std::path::PathBuf> = std::env::current_dir().ok().into_iter().collect();
    if let Some(target) = herdr_target(&env)
        && let Some(program) = herdr_program(&env, &here)
        && let Ok(panes) = HerdrAdapter::new(program, None).agents().await
        && !panes.is_empty()
    {
        println!("\nHerdr agent panes:");
        for pane in panes {
            let marv = peers.iter().find(|p| p.info.pane.as_deref() == Some(pane.pane_id.as_str())).map(|p| p.session.as_str());
            let here = if pane.pane_id == target.pane_id { " (this pane)" } else { "" };
            let names = [PaneAction::Send, PaneAction::Status]
                .into_iter()
                .filter_map(|a| pane_receptacle(&pane.pane_id, a))
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            println!(
                "{}",
                printable(&format!(
                    "  {}{here}  {} {}{}  {names}",
                    pane.pane_id,
                    pane.agent.as_deref().unwrap_or("?"),
                    pane.status.as_deref().unwrap_or("?"),
                    marv.map(|m| format!("  marv {m}")).unwrap_or_default(),
                ))
            );
        }
    }
    0
}

/// A `marv mesh wire` spec: which Marv asks, as which of its agents, for which of its receptacles, in which mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliWire {
    pub session: String,
    pub from: PlugId,
    pub to: ReceptacleName,
    pub mode: WireMode,
}

const WIRE_USAGE: &str = "write a wire as <marv>[/<agent>] -> <marv>[/<agent>]:inbox (e.g. alpha -> beta:inbox), or \
     <marv>[/<agent>] -> <receptacle>[:<mode>] (e.g. alpha -> herdr.pane.w1-p2.send:call); marv mesh ls shows the names";

/// `<marv>[/<agent>]`, the agent `parent` when left out.
fn end(text: &str) -> Result<(String, PlugId), String> {
    let (session, agent) = text.trim().split_once('/').unwrap_or((text.trim(), PARENT));
    let session_ok = !session.is_empty() && PlugId::new(session).is_ok();
    if !session_ok {
        return Err(format!("\"{session}\" isn't a Marv's name: {WIRE_USAGE}"));
    }
    Ok((session.to_string(), PlugId::new(agent).map_err(|e| e.to_string())?))
}

/// Parses `alpha -> beta:inbox`, `alpha/worker-1 -> alpha:inbox`, `alpha -> herdr.pane.w1-p2.send:call`.
pub fn parse_cli_wire(spec: &str) -> Result<CliWire, String> {
    let (from, to) = spec.split_once("->").ok_or_else(|| format!("\"{spec}\": {WIRE_USAGE}"))?;
    let (session, from) = end(from)?;
    let to = to.trim();
    if let Some(target) = to.strip_suffix(":inbox").filter(|t| !t.contains('.')) {
        let (to_session, agent) = end(target)?;
        let inbox = ReceptacleName::inbox(&agent);
        let to = if to_session == session { inbox } else { peer_receptacle(&to_session, &inbox).map_err(|e| e.to_string())? };
        return Ok(CliWire { session, from, to, mode: WireMode::Send });
    }
    let (name, mode) = match to.rsplit_once(':') {
        Some((name, mode)) => (name, mode.parse::<WireMode>().map_err(|e| e.to_string())?),
        None => (to, WireMode::Call),
    };
    if !name.contains('.') {
        return Err(format!("\"{spec}\": {WIRE_USAGE}"));
    }
    Ok(CliWire { session, from, to: ReceptacleName::new(name).map_err(|e| e.to_string())?, mode })
}

/// Connects to a live Marv's door by name.
async fn door(dir: &Path, session: &str) -> Result<DoorClient, String> {
    let socket = session_socket(dir, session).ok_or_else(|| format!("\"{session}\" isn't a Marv's name"))?;
    DoorClient::connect(&socket).await.map_err(|_| format!("No Marv named \"{session}\" is on the mesh (marv mesh ls)."))
}

/// `marv mesh wire <spec>`. Returns the exit code.
pub async fn wire(spec: &str) -> i32 {
    let wire = match parse_cli_wire(spec) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{}", printable(&e));
            return 1;
        }
    };
    let env = env();
    let Some(dir) = mesh_dir(&env) else { return no_runtime_dir("wire") };
    let mut client = match door(&dir, &wire.session).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}", printable(&e));
            return 1;
        }
    };
    eprintln!(
        "{}",
        printable(&format!("Asking for {}/{} -> {} ({}): approve it in that Marv.", wire.session, wire.from, wire.to, wire.mode))
    );
    let answer = client
        .request(CONNECT, json!({"from": wire.from, "to": wire.to, "mode": wire.mode}))
        .await
        .unwrap_or_else(|e| json!({"wired": false, "reason": e.to_string()}));
    if answer.get("wired").and_then(Value::as_bool) == Some(true) {
        println!("{}", printable(&format!("Wired {}/{} -> {} ({}).", wire.session, wire.from, wire.to, wire.mode)));
        0
    } else {
        let reason = answer.get("reason").and_then(Value::as_str).unwrap_or("refused");
        eprintln!("{}", printable(&format!("Not wired: {reason}")));
        1
    }
}

/// The shim's translation between an MCP client and the switchboard. MCP clients (and the models behind them) take
/// tool names of `[A-Za-z0-9_-]` only, so `agent.parent.inbox` is shown as `agent_parent_inbox` (a number added if two
/// collide) and mapped back on `tools/call`. A client doesn't know about wire modes: a call to a receptacle wired for
/// `send` only goes as a send.
#[derive(Debug, Default)]
pub struct ShimNames {
    /// Shown name → (receptacle, its wired modes).
    shown: Vec<(String, String, Vec<WireMode>)>,
    /// `tools/list` requests waiting for their answer.
    listing: Vec<Value>,
}

impl ShimNames {
    /// A line from the client, as the switchboard gets it.
    pub fn outgoing(&mut self, line: &str) -> String {
        let Ok(mut message) = serde_json::from_str::<Map<String, Value>>(line.trim_end()) else { return line.to_string() };
        match message.get("method").and_then(Value::as_str) {
            Some("tools/list") => {
                if let Some(id) = message.get("id") {
                    self.listing.push(id.clone());
                }
                line.to_string()
            }
            Some("tools/call") => {
                let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else { return line.to_string() };
                let shown = params.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                if let Some((_, real, modes)) = self.shown.iter().find(|(s, _, _)| *s == shown) {
                    params.insert("name".into(), Value::from(real.as_str()));
                    let asked = params.get("_meta").and_then(|m| m.get(MODE_META)).is_some();
                    if !asked && !modes.contains(&WireMode::Call) && modes.contains(&WireMode::Send) {
                        let meta = params.entry("_meta").or_insert_with(|| json!({}));
                        if let Some(meta) = meta.as_object_mut() {
                            meta.insert(MODE_META.into(), json!(WireMode::Send));
                        }
                    }
                }
                format!("{}\n", Value::Object(message))
            }
            _ => line.to_string(),
        }
    }

    /// A line from the switchboard, as the client gets it.
    pub fn incoming(&mut self, line: &str) -> String {
        let Ok(mut message) = serde_json::from_str::<Map<String, Value>>(line.trim_end()) else { return line.to_string() };
        let Some(at) = message.get("id").and_then(|id| self.listing.iter().position(|l| l == id)) else {
            return line.to_string();
        };
        self.listing.remove(at);
        let Some(tools) = message.get_mut("result").and_then(|r| r.get_mut("tools")).and_then(Value::as_array_mut) else {
            return line.to_string();
        };
        self.shown.clear();
        for tool in tools.iter_mut() {
            let Some(real) = tool.get("name").and_then(Value::as_str).map(str::to_string) else { continue };
            let modes: Vec<WireMode> = tool
                .pointer(&format!("/_meta/{}", MODES_META.replace('/', "~1")))
                .cloned()
                .and_then(|m| serde_json::from_value(m).ok())
                .unwrap_or_default();
            let base: String = real.chars().map(|c| if c == '.' { '_' } else { c }).collect();
            let mut shown = base.clone();
            let mut n = 2;
            while self.shown.iter().any(|(s, _, _)| *s == shown) {
                shown = format!("{base}_{n}");
                n += 1;
            }
            tool["name"] = Value::from(shown.as_str());
            self.shown.push((shown, real, modes));
        }
        format!("{}\n", Value::Object(message))
    }
}

/// `marv mesh plug <marv> [--as <plug>]`: stdin and stdout become an MCP connection on a new plug of that Marv's
/// switchboard. The plug lives while this runs. Returns the exit code.
pub async fn plug(session: &str, name: Option<&str>) -> i32 {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let env = env();
    let Some(dir) = mesh_dir(&env) else { return no_runtime_dir("plug") };
    let name = name.map_or_else(|| format!("ext-{}", std::process::id()), str::to_string);
    let fail = |e: String| {
        eprintln!("marv mesh plug: {}", printable(&e));
        1
    };
    // Held while the shim runs: the switchboard unplugs it when this connection closes.
    let mut client = match door(&dir, session).await {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let answer = match client.request(PLUG, json!({"name": name})).await {
        Ok(a) => a,
        Err(e) => return fail(e.to_string()),
    };
    let socket = std::path::PathBuf::from(answer.get("socket").and_then(Value::as_str).unwrap_or_default());
    if let Err(e) = check_socket(&socket) {
        return fail(e.to_string());
    }
    let stream = match tokio::net::UnixStream::connect(&socket).await {
        Ok(s) => s,
        Err(e) => return fail(format!("can't reach the plug: {e}")),
    };
    eprintln!("{}", printable(&format!("marv mesh plug: plugged into {session} as {name}; the user wires it (marv mesh wire).")));
    let (read, mut write) = stream.into_split();
    let names = Arc::new(std::sync::Mutex::new(ShimNames::default()));
    let up = {
        let names = names.clone();
        async move {
            let mut stdin = BufReader::new(tokio::io::stdin());
            loop {
                let mut line = String::new();
                match stdin.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = names.lock().unwrap_or_else(|e| e.into_inner()).outgoing(&line);
                if write.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    };
    let down = async move {
        let mut from = BufReader::new(read);
        let mut stdout = tokio::io::stdout();
        loop {
            let mut line = String::new();
            match from.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = names.lock().unwrap_or_else(|e| e.into_inner()).incoming(&line);
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    };
    // Either side closing ends the shim: the client left, or the Marv did.
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    drop(client);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_specs() {
        let w = parse_cli_wire("alpha -> beta:inbox").unwrap();
        assert_eq!(
            (w.session.as_str(), w.from.as_str(), w.to.as_str(), w.mode),
            ("alpha", "parent", "peer.beta.agent.parent.inbox", WireMode::Send)
        );
        let w = parse_cli_wire("alpha/worker-1 -> alpha:inbox").unwrap();
        assert_eq!((w.from.as_str(), w.to.as_str()), ("worker-1", "agent.parent.inbox"));
        let w = parse_cli_wire("alpha -> beta/claude:inbox").unwrap();
        assert_eq!(w.to.as_str(), "peer.beta.agent.claude.inbox");
        let w = parse_cli_wire("alpha -> herdr.pane.w1-p2.send:call").unwrap();
        assert_eq!((w.to.as_str(), w.mode), ("herdr.pane.w1-p2.send", WireMode::Call));
        assert_eq!(parse_cli_wire("alpha -> herdr.pane.w1-p2.status").unwrap().mode, WireMode::Call);
        for bad in
            ["alpha beta", "alpha -> beta", "Alpha -> beta:inbox", "alpha -> beta:everything", "/x -> b:inbox", "a -> x.y:all"]
        {
            assert!(parse_cli_wire(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ls_shows_one_line_each_and_how_to_reach_it() {
        let peer = |session: &str, pane: Option<&str>| PeerSwitchboard {
            session: session.into(),
            socket: format!("/run/x/{session}.sock").into(),
            info: marv_mesh::discovery::PeerInfo {
                name: session.into(),
                pid: 1,
                cwd: "/srv/api\x1b[2J".into(),
                pane: pane.map(String::from),
                agents: vec!["parent".into(), "worker-1".into()],
                version: "0".into(),
                radio: pane.is_some(),
            },
        };
        let lines = ls_lines(&[peer("alpha", Some("w1:p2")), peer("b", None)]);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("alpha  radio on   pane w1:p2  agents parent,worker-1  /srv/api"), "{}", lines[0]);
        assert!(lines[0].ends_with("(peer.alpha.agent.parent.inbox)"));
        assert!(!lines[0].contains('\x1b'), "printable");
        assert!(lines[1].starts_with("b      radio off  pane -"), "{}", lines[1]);
    }

    #[test]
    fn the_shim_shows_mcp_safe_names_and_sends_to_inboxes() {
        let mut names = ShimNames::default();
        let list = r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;
        assert_eq!(names.outgoing(list), list);
        let answer = json!({"jsonrpc": "2.0", "id": 7, "result": {"tools": [
            {"name": "agent.parent.inbox", "_meta": {"marv/modes": ["send"]}},
            {"name": "agent_parent.inbox", "_meta": {"marv/modes": ["call"]}},
        ]}});
        let shown: Value = serde_json::from_str(&names.incoming(&answer.to_string())).unwrap();
        let tools = shown["result"]["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "agent_parent_inbox");
        assert_eq!(tools[1]["name"], "agent_parent_inbox_2", "collisions get a number");
        let call = json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {"name": "agent_parent_inbox", "arguments": {"text": "hi"}}});
        let sent: Value = serde_json::from_str(&names.outgoing(&call.to_string())).unwrap();
        assert_eq!(sent["params"]["name"], "agent.parent.inbox");
        assert_eq!(sent["params"]["_meta"]["marv/mode"], "send");
        let call2 = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "agent_parent_inbox_2"}});
        let sent: Value = serde_json::from_str(&names.outgoing(&call2.to_string())).unwrap();
        assert_eq!(sent["params"]["name"], "agent_parent.inbox");
        assert!(sent["params"].get("_meta").is_none(), "a call wire stays a call");
        // Anything else passes as is, unknown names included (the switchboard refuses them).
        let other = r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"nope"}}"#;
        assert_eq!(serde_json::from_str::<Value>(&names.outgoing(other)).unwrap()["params"]["name"], "nope");
        assert_eq!(names.incoming("not json\n"), "not json\n");
    }
}

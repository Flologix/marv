import { ask } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useState } from "react";
import { type Lookup, type ServerEntry, desktop } from "../lib/backend";
import type { ServerStatus } from "../lib/chat";

const STATE_TEXT: Record<ServerStatus["state"], string> = {
  connected: "connected",
  connecting: "starting…",
  needs_login: "needs login",
  failed: "failed",
  untrusted: "not trusted yet",
  removed: "removed",
};

/**
 * The MCP servers a chat in `folder` gets: yours (~/.marv/mcp.json, every folder) and the folder's (.mcp.json).
 * Each has a switch for chats; adding and removing write your config. Status, login and trust need an open chat,
 * since that's where the servers run.
 */
export function Servers({
  folder,
  servers,
  chatOpen,
  mcpOff,
  onToggle,
  onAction,
  onChanged,
}: {
  folder: string;
  servers: ServerStatus[];
  chatOpen: boolean;
  mcpOff: string[];
  onToggle: (name: string, on: boolean) => void;
  onAction: (action: string, name?: string) => void;
  onChanged: () => void;
}) {
  const [entries, setEntries] = useState<ServerEntry[]>([]);
  const [adding, setAdding] = useState(false);
  const [source, setSource] = useState("");
  const [name, setName] = useState("");
  const [lookup, setLookup] = useState<Lookup | null>(null);
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    desktop.mcpServers(folder).then(setEntries, (e) => setError(String(e)));
  }, [folder]);

  useEffect(load, [load]);

  // Servers start (and log in) in the background: ask the open chat how they're doing while this view shows.
  useEffect(() => {
    if (!chatOpen) return;
    onAction("status");
    const timer = setInterval(() => onAction("status"), 3000);
    return () => clearInterval(timer);
  }, [chatOpen, onAction]);

  async function look() {
    setError(null);
    setLookup(null);
    setWorking(true);
    try {
      setLookup(await desktop.mcpLookup(source, name || undefined));
    } catch (e) {
      setError(String(e));
    } finally {
      setWorking(false);
    }
  }

  async function install(serverName: string) {
    setError(null);
    try {
      await desktop.mcpInstall(serverName);
      setAdding(false);
      setSource("");
      setName("");
      setLookup(null);
      load();
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  }

  async function remove(serverName: string) {
    if (!(await ask(`Remove ${serverName} from ~/.marv/mcp.json? It stops being offered in every folder.`, { title: "Marv", kind: "warning" })))
      return;
    try {
      await desktop.mcpRemove(serverName);
      load();
      onChanged();
    } catch (e) {
      setError(String(e));
    }
  }

  const status = (n: string) => servers.find((s) => s.name === n);
  const untrusted = servers.some((s) => s.state === "untrusted");

  function row(entry: ServerEntry) {
    const on = !mcpOff.includes(entry.name);
    const s = status(entry.name);
    const state = !on ? "off" : s ? s.state : "idle";
    return (
      <div key={entry.name} className="server" title={entry.target}>
        <span className={`server-dot ${state}`} />
        <span className="server-name">{entry.name}</span>
        <span className="server-meta">
          {!on ? "off" : s ? (s.state === "connected" ? `${s.tools} tools` : STATE_TEXT[s.state]) : ""}
        </span>
        <span className="server-actions">
          {on && s?.canLogin && chatOpen && (
            <button className="icon-btn" title={s.loggedIn ? "Log out" : "Log in (opens your browser)"} onClick={() => onAction(s.loggedIn ? "logout" : "login", entry.name)}>
              {s.loggedIn ? "⏏" : "⇥"}
            </button>
          )}
          {entry.scope === "global" && (
            <button className="icon-btn danger" title="Remove from ~/.marv/mcp.json" onClick={() => void remove(entry.name)}>
              ×
            </button>
          )}
        </span>
        <button
          type="button"
          role="switch"
          aria-checked={on}
          title={on ? "On for chats: switch off" : "Off for chats: switch on"}
          className={`toggle small${on ? " on" : ""}`}
          onClick={() => onToggle(entry.name, !on)}
        />
      </div>
    );
  }

  const global = entries.filter((e) => e.scope === "global");
  const local = entries.filter((e) => e.scope === "folder");

  return (
    <div className="servers">
      <div className="section-title">
        <span>global · every folder</span>
      </div>
      {global.length === 0 && <p className="faint pad">none</p>}
      {global.map(row)}
      <div className="section-title">
        <span>this folder · .mcp.json</span>
        {untrusted && chatOpen && (
          <button className="link" onClick={() => onAction("trust")}>
            trust
          </button>
        )}
      </div>
      {local.length === 0 && <p className="faint pad">none</p>}
      {local.map(row)}

      {!chatOpen && entries.length > 0 && <p className="faint pad">Open a chat to see status and log in.</p>}

      {adding ? (
        <div className="add-server">
          <input autoFocus placeholder="npm package, URL or GitHub repo" value={source} spellCheck={false} onChange={(e) => setSource(e.target.value)} onKeyDown={(e) => e.key === "Enter" && source.trim() && void look()} />
          <input placeholder="name (optional)" value={name} spellCheck={false} onChange={(e) => setName(e.target.value)} />
          <div className="add-buttons">
            <button className="btn primary" disabled={!source.trim() || working} onClick={() => void look()}>
              {working ? "Looking up…" : "Look up"}
            </button>
            <button className="btn plain" onClick={() => (setAdding(false), setLookup(null), setError(null))}>
              Cancel
            </button>
          </div>
          {lookup && "choices" in lookup && (
            <div className="lookup">
              <p>That README lists several servers. Pick one:</p>
              {lookup.choices.map((c) => (
                <button key={c} className="link mono" onClick={() => (setSource(c.replace(/^\/mcp add\s+/, "")), setLookup(null))}>
                  {c}
                </button>
              ))}
            </div>
          )}
          {lookup && "name" in lookup && (
            <div className="lookup">
              <b>{lookup.title}</b>
              {lookup.command && <pre className="code">$ {lookup.command}</pre>}
              <pre className="code">{lookup.text}</pre>
              {lookup.warning && <p className="warning">{lookup.warning}</p>}
              <p className="faint">Written to ~/.marv/mcp.json: every folder gets it, and you can switch it off per chat.</p>
              <div className="add-buttons">
                <button className="btn primary" onClick={() => void install(lookup.name)}>
                  Add {lookup.name}
                </button>
              </div>
            </div>
          )}
        </div>
      ) : (
        <button className="add-link" onClick={() => setAdding(true)}>
          + add server
        </button>
      )}
      {error && <p className="notice error pad">{error}</p>}
    </div>
  );
}

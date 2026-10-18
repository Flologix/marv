import { ask } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useState } from "react";
import { type ChatRecord, type Entry, type LiveChat, desktop } from "../lib/backend";
import { type ServerStatus, forget } from "../lib/chat";
import { Servers } from "./Servers";
import { ago } from "../lib/format";

type Editing = { mode: "new"; parent: string } | { mode: "rename"; rel: string; name: string } | null;

const join = (parent: string, name: string) => (parent ? `${parent}/${name}` : name);
const parentOf = (rel: string) => (rel.includes("/") ? rel.slice(0, rel.lastIndexOf("/")) : "");

/** The root's folder tree (folders open on click; files listed, dimmed) and the selected folder's chats. */
export function Sidebar({
  rootName,
  folder,
  onFolder,
  chats,
  chatId,
  onChat,
  onNewChat,
  onChatsChanged,
  onChangeRoot,
  onRootUp,
  onEnter,
  servers,
  chatOpen,
  mcpOff,
  onToggleServer,
  onServerAction,
  onServersChanged,
  onBrowseApps,
  live,
  onStop,
  waiting,
  onJump,
}: {
  rootName: string;
  folder: string;
  onFolder: (rel: string) => void;
  chats: ChatRecord[];
  chatId: string | null;
  onChat: (id: string) => void;
  onNewChat: () => void;
  onChatsChanged: () => void;
  onChangeRoot: () => void;
  onRootUp: () => void;
  onEnter: (rel: string) => void;
  servers: ServerStatus[];
  chatOpen: boolean;
  mcpOff: string[];
  onToggleServer: (name: string, on: boolean) => void;
  onServerAction: (action: string, name?: string) => void;
  onServersChanged: () => void;
  onBrowseApps: () => void;
  /** Chats running now, here or elsewhere (#155). */
  live: LiveChat[];
  onStop: (id: string) => void;
  /** Chats elsewhere waiting for an approval. */
  waiting: ChatRecord[];
  onJump: (chat: ChatRecord) => void;
}) {
  const [children, setChildren] = useState<Record<string, Entry[]>>({});
  const [open, setOpen] = useState<Set<string>>(new Set([""]));
  const [editing, setEditing] = useState<Editing>(null);
  const [error, setError] = useState<string | null>(null);
  const [view, setView] = useState<"folders" | "servers">("folders");

  const load = useCallback(async (rel: string) => {
    try {
      const entries = await desktop.listDir(rel);
      setChildren((c) => ({ ...c, [rel]: entries }));
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    void load("");
  }, [load]);

  // Marv changes files too: refresh what's open every few seconds while the window shows.
  useEffect(() => {
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") open.forEach((rel) => void load(rel));
    }, 4000);
    return () => clearInterval(timer);
  }, [open, load]);

  function toggle(rel: string) {
    setOpen((o) => {
      const next = new Set(o);
      if (next.has(rel) && rel !== "") next.delete(rel);
      else {
        next.add(rel);
        void load(rel);
      }
      return next;
    });
  }

  async function run(action: () => Promise<unknown>, refresh: string[]) {
    setError(null);
    try {
      await action();
      for (const rel of refresh) await load(rel);
    } catch (e) {
      setError(String(e));
    }
  }

  async function finishEdit(value: string) {
    const edit = editing;
    setEditing(null);
    const name = value.trim();
    if (!edit || !name) return;
    if (edit.mode === "new") {
      await run(async () => {
        const made = await desktop.createFolder(edit.parent, name);
        setOpen((o) => new Set(o).add(edit.parent));
        onFolder(made);
      }, [edit.parent]);
    } else if (name !== edit.name) {
      await run(async () => {
        const renamed = await desktop.rename(edit.rel, name);
        if (folder === edit.rel || folder.startsWith(`${edit.rel}/`)) onFolder(renamed + folder.slice(edit.rel.length));
        onChatsChanged();
      }, [parentOf(edit.rel)]);
    }
  }

  async function remove(rel: string) {
    if (!(await ask(`Move ${rel} to the trash?`, { title: "Marv", kind: "warning" }))) return;
    await run(async () => {
      await desktop.trash(rel);
      if (folder === rel || folder.startsWith(`${rel}/`)) onFolder(parentOf(rel));
    }, [parentOf(rel)]);
  }

  async function removeChat(chat: ChatRecord) {
    if (!(await ask(`Delete the chat "${chat.title}"? The folder's files stay.`, { title: "Marv", kind: "warning" }))) return;
    await desktop.deleteChat(chat.id);
    forget(chat.id);
    onChatsChanged();
  }

  function rows(parent: string, depth: number): React.ReactNode[] {
    const out: React.ReactNode[] = [];
    if (editing?.mode === "new" && editing.parent === parent) {
      out.push(<EditRow key={`new:${parent}`} depth={depth} initial="" onDone={finishEdit} />);
    }
    for (const entry of children[parent] ?? []) {
      const rel = join(parent, entry.name);
      const pad = { paddingLeft: 8 + depth * 14 };
      if (editing?.mode === "rename" && editing.rel === rel) {
        out.push(<EditRow key={`edit:${rel}`} depth={depth} initial={entry.name} onDone={finishEdit} />);
        continue;
      }
      if (!entry.dir) {
        out.push(
          <div key={rel} className="row file" style={pad}>
            <span className="twisty" />
            <span className="name">{entry.name}</span>
          </div>,
        );
        continue;
      }
      out.push(
        <div
          key={rel}
          className={`row${folder === rel ? " selected" : ""}`}
          style={pad}
          title="Click to work here · double-click to open"
          onClick={() => onFolder(rel)}
          onDoubleClick={() => onEnter(rel)}
        >
          <span
            className="twisty"
            onClick={(e) => {
              e.stopPropagation();
              toggle(rel);
            }}
          >
            {open.has(rel) ? "▾" : "▸"}
          </span>
          <span className="name">{entry.name}</span>
          <span className="actions" onClick={(e) => e.stopPropagation()}>
            <button className="icon-btn" title="New folder inside" onClick={() => (setOpen((o) => new Set(o).add(rel)), void load(rel), setEditing({ mode: "new", parent: rel }))}>
              +
            </button>
            <button className="icon-btn" title="Rename" onClick={() => setEditing({ mode: "rename", rel, name: entry.name })}>
              ✎
            </button>
            <button className="icon-btn danger" title="Move to trash" onClick={() => void remove(rel)}>
              ×
            </button>
          </span>
        </div>,
      );
      if (open.has(rel)) out.push(...rows(rel, depth + 1));
    }
    return out;
  }

  return (
    <aside className="side">
      <div className="side-tabs" role="tablist">
        <button role="tab" aria-selected={view === "folders"} className={view === "folders" ? "active" : ""} onClick={() => setView("folders")}>
          Folders
        </button>
        <button role="tab" aria-selected={view === "servers"} className={view === "servers" ? "active" : ""} onClick={() => setView("servers")}>
          Servers
        </button>
      </div>
      {view === "servers" ? (
        <div className="tree">
          <Servers
            folder={folder}
            servers={servers}
            chatOpen={chatOpen}
            mcpOff={mcpOff}
            onToggle={onToggleServer}
            onAction={onServerAction}
            onChanged={onServersChanged}
            onBrowse={onBrowseApps}
          />
        </div>
      ) : (
      <>
      <div className="side-head">
        <span className="label" title="Your Marv folder">
          {rootName}
        </span>
        <button className="icon-btn" title="Up a level" onClick={onRootUp}>
          ↑
        </button>
        <button className="icon-btn" title="New folder" onClick={() => setEditing({ mode: "new", parent: "" })}>
          +
        </button>
      </div>
      <div className="tree">
        <div className={`row${folder === "" ? " selected" : ""}`} style={{ paddingLeft: 8 }} onClick={() => onFolder("")}>
          <span className="twisty">~</span>
          <span className="name">{rootName}</span>
        </div>
        {rows("", 0)}
        {error && <p className="notice error" style={{ padding: "6px 12px" }}>{error}</p>}
      </div>
      </>
      )}
      <div className="chats">
        <div className="section-title">
          <span>chats · {folder || rootName}</span>
          <button className="icon-btn" title="New chat here" onClick={onNewChat}>
            +
          </button>
        </div>
        {waiting.map((chat) => (
          <button key={chat.id} className="waiting-elsewhere" onClick={() => onJump(chat)} title={chat.folder}>
            <span className="dot" />
            <span className="title">{chat.title}</span>
            <span className="faint">needs an answer</span>
          </button>
        ))}
        {chats.length === 0 && <p className="faint" style={{ padding: "2px 12px" }}>none yet</p>}
        {chats.map((chat) => {
          const state = live.find((l) => l.id === chat.id);
          const mark = !state ? "" : state.waiting > 0 ? " waiting" : state.busy ? " busy" : " live";
          const tip = !state
            ? "Not running: opening it continues the conversation"
            : state.waiting > 0
              ? "Running · waiting for your answer"
              : state.busy
                ? "Running · working"
                : "Running · on the mesh, idle";
          return (
            <div key={chat.id} className={`chat-row${chat.id === chatId ? " selected" : ""}${mark}`} onClick={() => onChat(chat.id)}>
              <span className="dot" title={tip} />
              <span className="title">{chat.title}</span>
              <span className="when">{ago(chat.created)}</span>
              <span className="actions" onClick={(e) => e.stopPropagation()}>
                {state && chat.id !== chatId && (
                  <button className="icon-btn" title="Stop: it leaves the mesh (opening it again continues it)" onClick={() => onStop(chat.id)}>
                    ■
                  </button>
                )}
                <button className="icon-btn danger" title="Delete chat" onClick={() => void removeChat(chat)}>
                  ×
                </button>
              </span>
            </div>
          );
        })}
      </div>
      <div className="side-foot">
        <img className="flai" src="/flai.png" alt="FloLogix AI" />
        <span className="grow">marv</span>
        <button className="icon-btn" title="Choose another Marv folder" onClick={onChangeRoot}>
          ⇄
        </button>
      </div>
    </aside>
  );
}

function EditRow({ depth, initial, onDone }: { depth: number; initial: string; onDone: (value: string) => void }) {
  const [value, setValue] = useState(initial);
  return (
    <div className="row" style={{ paddingLeft: 8 + depth * 14 }}>
      <span className="twisty">▸</span>
      <input
        autoFocus
        value={value}
        placeholder="folder name"
        spellCheck={false}
        onChange={(e) => setValue(e.target.value)}
        onBlur={() => onDone(value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") onDone(value);
          if (e.key === "Escape") onDone("");
        }}
        onFocus={(e) => e.target.select()}
      />
    </div>
  );
}

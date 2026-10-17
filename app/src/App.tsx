import { message, open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useMemo, useState } from "react";
import { ChatView } from "./components/ChatView";
import { Onboarding } from "./components/Onboarding";
import { Gallery } from "./components/Gallery";
import { Martian } from "./components/Martian";
import { SpacesBar } from "./components/SpacesBar";
import { Sidebar } from "./components/Sidebar";
import { type AppInfo, type ChatRecord, type ChatSettings, type LiveChat, type Model, desktop, isDesktop, loadModels } from "./lib/backend";
import { useChat } from "./lib/chat";

const SETTINGS_KEY = "marv-settings";
const DEFAULT_SETTINGS: ChatSettings = { model: "z-ai/glm-5.3-flash", maxSteps: 50, yolo: true, mcpOff: [] };

function stored<T>(key: string, fallback: T): T {
  try {
    return JSON.parse(localStorage.getItem(key) ?? "null") ?? fallback;
  } catch {
    return fallback;
  }
}

function keep(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {}
}

function useSettings() {
  const [settings, setSettings] = useState<ChatSettings>(() => ({ ...DEFAULT_SETTINGS, ...stored(SETTINGS_KEY, DEFAULT_SETTINGS) }));
  const [models, setModels] = useState<Model[]>([]);
  useEffect(() => keep(SETTINGS_KEY, settings), [settings]);
  useEffect(() => {
    loadModels().then(setModels, () => setModels([]));
  }, []);
  return { settings, setSettings, models };
}

export default function App() {
  return isDesktop ? <Desktop /> : <Browser />;
}

/** Marv desktop: the root folder's tree on the left, a chat on the selected folder on the right. */
function Desktop() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [folder, setFolder] = useState(() => stored("marv-folder", ""));
  const [chatId, setChatId] = useState<string | null>(null);
  const [chats, setChats] = useState<ChatRecord[]>([]);
  const [collapsed, setCollapsed] = useState(false);
  const { settings, setSettings, models } = useSettings();
  const [reload, setReload] = useState(0);
  const [setup, setSetup] = useState(false);
  const [gallery, setGallery] = useState(false);
  const [installed, setInstalled] = useState<string[]>([]);
  // The active space's switched-off apps go with every chat in it.
  const appsOff = info?.spaces.find((s) => s.id === info.active)?.appsOff ?? [];
  const appsOffKey = appsOff.join(",");
  const chatSettings = useMemo(() => ({ ...settings, mcpOff: appsOffKey ? appsOffKey.split(",") : [] }), [settings, appsOffKey]);
  const chat = useChat(chatId, chatSettings, reload);

  // First run (or a step still missing): the onboarding, through to connecting apps.
  useEffect(() => {
    if (info && (!info.root || !(info.hasKey || info.provider === "ollama"))) setSetup(true);
  }, [info]);

  // Opened as `marv-desktop <dir>`: that folder; else where the last session left off.
  useEffect(() => {
    desktop.takeLaunch().then(
      (launch) => {
        if (!launch) return desktop.info().then(setInfo);
        setInfo(launch.info);
        setFolder(launch.folder);
        setChatId(null);
      },
      (e) => {
        void message(String(e), { title: "Marv", kind: "error" });
        desktop.info().then(setInfo);
      },
    );
  }, []);

  useEffect(() => keep("marv-folder", folder), [folder]);

  // The running chats (#155): the sidebar marks them, and one waiting for an answer anywhere is shown.
  const [live, setLive] = useState<LiveChat[]>([]);
  const [allChats, setAllChats] = useState<ChatRecord[]>([]);
  useEffect(() => {
    const poll = () => desktop.liveChats().then(setLive, () => {});
    void poll();
    const timer = setInterval(poll, 2000);
    return () => clearInterval(timer);
  }, []);
  const waitingIds = live.filter((c) => c.waiting > 0).map((c) => c.id).join(",");
  useEffect(() => {
    if (waitingIds) desktop.chats().then(setAllChats, () => {});
  }, [waitingIds]);

  const refreshChats = useCallback(() => {
    desktop.chats(folder).then(setChats, () => setChats([]));
  }, [folder]);

  useEffect(() => {
    if (info?.root) refreshChats();
  }, [info?.root, refreshChats]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key === "b") {
        e.preventDefault();
        setCollapsed((c) => !c);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  if (!info) return null;
  if (setup)
    return (
      <Onboarding info={info} onInfo={setInfo} onModel={(model) => setSettings({ ...settings, model })} onFinish={() => setSetup(false)} />
    );
  const rootName = info.rootName ?? "marv";

  // Another space: its own folder, chats and apps, back where you left it (renaming one keeps the view), or at
  // `target`.
  function toSpace(next: AppInfo, target?: { folder: string; chatId: string }) {
    const was = info?.active;
    setInfo(next);
    if (next.active === was) return;
    const views = stored<Record<string, { folder: string; chatId: string | null }>>("marv-space-views", {});
    if (was) views[was] = { folder, chatId };
    keep("marv-space-views", views);
    if (target) {
      setFolder(target.folder);
      setChatId(target.chatId);
      return;
    }
    const view = next.active ? views[next.active] : undefined;
    setFolder(view?.folder ?? "");
    setChatId(null);
    // Its chat again, unless it was deleted since.
    const id = view?.chatId;
    if (id)
      desktop.chats(view.folder).then(
        (list) => list.some((c) => c.id === id) && setChatId(id),
        () => {},
      );
  }

  // A chat anywhere: its space (the one whose folder holds it most closely), its folder, the chat.
  function jumpTo(target: ChatRecord) {
    const space = info!.spaces
      .filter((s) => target.folder === s.root || target.folder.startsWith(`${s.root}/`))
      .sort((a, b) => b.root.length - a.root.length)[0];
    if (!space) return;
    const rel = target.folder.slice(space.root.length + 1);
    if (space.id === info!.active && info!.root === space.root) {
      setFolder(rel);
      setChatId(target.id);
    } else {
      desktop.switchSpace(space.id).then((next) => toSpace(next, { folder: rel, chatId: target.id }), (e) => void message(String(e), { title: "Marv" }));
    }
  }
  const waiting = allChats.filter((c) => c.id !== chatId && live.some((l) => l.id === c.id && l.waiting > 0));

  function openGallery() {
    desktop.mcpServers(folder).then((s) => setInstalled(s.map((e) => e.name)), () => {});
    setGallery(true);
  }

  function selectFolder(rel: string) {
    if (rel !== folder) setChatId(null);
    setFolder(rel);
  }

  async function send(text: string) {
    if (chatId) return chat.send(text);
    // A draft: the chat exists from its first message, titled by it.
    const title = text.split("\n")[0].slice(0, 60);
    const created = await desktop.createChat(folder, title);
    chat.startWith(created.id, text);
    setChatId(created.id);
    refreshChats();
  }

  async function changeRoot() {
    const path = await open({ directory: true, title: "Choose your Marv folder" });
    if (typeof path !== "string") return;
    try {
      setInfo(await desktop.setRoot(path));
      setFolder("");
      setChatId(null);
    } catch (e) {
      void message(String(e), { title: "Marv", kind: "error" });
    }
  }

  // Double-click: into that folder.
  async function enter(rel: string) {
    try {
      setInfo(await desktop.enterFolder(rel));
      setFolder("");
      setChatId(null);
    } catch (e) {
      void message(String(e), { title: "Marv", kind: "info" });
    }
  }

  // ↑: the Marv folder's parent becomes the Marv folder; the selected folder stays selected.
  async function rootUp() {
    try {
      const was = rootName;
      setInfo(await desktop.rootUp());
      setFolder(folder ? `${was}/${folder}` : was);
      setChatId(null);
    } catch (e) {
      void message(String(e), { title: "Marv", kind: "info" });
    }
  }

  const parts = folder ? folder.split("/") : [];
  const crumbs = (
    <>
      <span>{rootName}</span>
      {parts.map((p, i) => (
        <span key={i}>
          / {i === parts.length - 1 ? <b>{p}</b> : p}
        </span>
      ))}
      {chatId && <span className="faint">· {chats.find((c) => c.id === chatId)?.title}</span>}
    </>
  );

  return (
    <div className="frame">
    <SpacesBar info={info} onInfo={toSpace} />
    <div className={`shell${collapsed ? " collapsed" : ""}`}>
      <Sidebar
        key={info.root}
        rootName={rootName}
        onRootUp={() => void rootUp()}
        onEnter={(rel) => void enter(rel)}
        servers={chat.servers}
        chatOpen={chatId !== null && chat.status === "ready"}
        mcpOff={chatSettings.mcpOff}
        onToggleServer={(name, on) =>
          void desktop.setAppsOff(on ? appsOff.filter((n) => n !== name) : [...appsOff, name]).then(setInfo)
        }
        onBrowseApps={openGallery}
        onServerAction={chat.mcp}
        onServersChanged={() => setReload((r) => r + 1)}
        folder={folder}
        onFolder={selectFolder}
        chats={chats}
        chatId={chatId}
        onChat={setChatId}
        onNewChat={() => setChatId(null)}
        onChatsChanged={refreshChats}
        live={live}
        onStop={(id) => void desktop.stopChat(id).then(() => desktop.liveChats().then(setLive))}
        waiting={waiting}
        onJump={jumpTo}
        onChangeRoot={() => void changeRoot()}
      />
      <ChatView
        key={chatId ?? `draft:${folder}`}
        chat={chat}
        crumbs={crumbs}
        settings={chatSettings}
        onSettings={setSettings}
        models={models}
        hasKey={info.hasKey}
        onSend={(text) => void send(text)}
        left={
          <button className="icon-btn" title={collapsed ? "Show folders (ctrl+b)" : "Hide folders (ctrl+b)"} onClick={() => setCollapsed((c) => !c)}>
            {collapsed ? "»" : "«"}
          </button>
        }
        empty={
          <section className="empty">
            <Martian size={56} className="toon" />
            <span className="where">
              {rootName}
              {folder && ` / ${folder}`}
            </span>
            <h1>What should Marv do here?</h1>
            <p>
              Marv can read and change everything in this folder and below it. Sandboxed commands and edits run on their own; it asks
              before anything leaves the sandbox.
            </p>
          </section>
        }
      />
    </div>
    {gallery && (
      <div className="modal" onClick={() => setGallery(false)}>
        <div className="modal-card" onClick={(e) => e.stopPropagation()}>
          <div className="modal-head">
            <h2>Apps</h2>
            <button className="icon-btn" title="Close" onClick={() => setGallery(false)}>
              ×
            </button>
          </div>
          <p className="help">Connected apps are on in every space; switch one off in a space from Servers.</p>
          <Gallery
            installed={installed}
            onInstalled={() => {
              openGallery();
              setReload((r) => r + 1);
            }}
          />
        </div>
      </div>
    )}
    </div>
  );
}

/** In a plain browser: one chat with marv-web, in a workspace of its own on the server. */
function Browser() {
  const [chatId, setChatId] = useState(() => stored<string | null>("marv-web-chat", null) ?? crypto.randomUUID());
  const { settings, setSettings, models } = useSettings();
  const chat = useChat(chatId, settings);
  useEffect(() => keep("marv-web-chat", chatId), [chatId]);
  return (
    <ChatView
      chat={chat}
      crumbs={
        <>
          <b>marv</b>
          <button className="icon-btn" title="New chat" onClick={() => setChatId(crypto.randomUUID())}>
            +
          </button>
        </>
      }
      settings={settings}
      onSettings={setSettings}
      models={models}
      onSend={chat.send}
      empty={
        <section className="empty">
          <Martian size={56} className="toon" />
          <h1>What should Marv build?</h1>
          <p>This chat runs in a fresh workspace on the marv-web server.</p>
        </section>
      }
    />
  );
}

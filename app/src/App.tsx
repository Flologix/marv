import { message, open } from "@tauri-apps/plugin-dialog";
import { useCallback, useEffect, useState } from "react";
import { ChatView } from "./components/ChatView";
import { Onboarding } from "./components/Onboarding";
import { Martian } from "./components/Martian";
import { Sidebar } from "./components/Sidebar";
import { type AppInfo, type ChatRecord, type ChatSettings, type Model, desktop, isDesktop, loadModels } from "./lib/backend";
import { useChat } from "./lib/chat";

const SETTINGS_KEY = "marv-settings";
const DEFAULT_SETTINGS: ChatSettings = { model: "z-ai/glm-5.3-flash", maxSteps: 50, yolo: true };

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
  const [settings, setSettings] = useState<ChatSettings>(() => stored(SETTINGS_KEY, DEFAULT_SETTINGS));
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
  const chat = useChat(chatId, settings);

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
  if (!info.root || !info.hasKey) return <Onboarding info={info} onDone={setInfo} />;
  const rootName = info.rootName ?? "marv";

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
    <div className={`shell${collapsed ? " collapsed" : ""}`}>
      <Sidebar
        key={info.root}
        rootName={rootName}
        onRootUp={() => void rootUp()}
        onEnter={(rel) => void enter(rel)}
        folder={folder}
        onFolder={selectFolder}
        chats={chats}
        chatId={chatId}
        onChat={setChatId}
        onNewChat={() => setChatId(null)}
        onChatsChanged={refreshChats}
        onChangeRoot={() => void changeRoot()}
      />
      <ChatView
        key={chatId ?? `draft:${folder}`}
        chat={chat}
        crumbs={crumbs}
        settings={settings}
        onSettings={setSettings}
        models={models}
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

// What the page talks to: Marv desktop's Tauri commands, or marv-web's WebSocket in a plain browser. A chat looks
// the same either way: messages out (send, interrupt, approval, configure), the same JSON events back.
import { Channel, invoke } from "@tauri-apps/api/core";

export const isDesktop = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export interface AppInfo {
  root: string | null;
  rootName: string | null;
  hasKey: boolean;
}

export interface Entry {
  name: string;
  dir: boolean;
}

export interface ChatRecord {
  id: string;
  /** Relative to the root; "" is the root. */
  folder: string;
  title: string;
  sessionId?: string | null;
  created: number;
}

export interface ChatSettings {
  model: string;
  maxSteps: number;
  /** Sandboxed commands and edits in the folder run without asking. */
  yolo: boolean;
}

export type ServerEvent = Record<string, unknown> & { type: string };

export interface Connection {
  send(message: object): void;
  close(): void;
}

export const desktop = {
  info: () => invoke<AppInfo>("app_info"),
  setRoot: (path: string) => invoke<AppInfo>("set_root", { path }),
  rootUp: () => invoke<AppInfo>("root_up"),
  /** `marv-desktop <dir>`: where to open, once. */
  takeLaunch: () => invoke<{ info: AppInfo; folder: string } | null>("take_launch"),
  setApiKey: (key: string) => invoke<AppInfo>("set_api_key", { key }),
  listDir: (rel: string) => invoke<Entry[]>("list_dir", { rel }),
  createFolder: (parent: string, name: string) => invoke<string>("create_folder", { parent, name }),
  rename: (rel: string, name: string) => invoke<string>("rename_entry", { rel, name }),
  trash: (rel: string) => invoke<void>("trash_entry", { rel }),
  chats: (folder?: string) => invoke<ChatRecord[]>("list_chats", { folder }),
  createChat: (folder: string, title: string) => invoke<ChatRecord>("create_chat", { folder, title }),
  renameChat: (id: string, title: string) => invoke<void>("rename_chat", { id, title }),
  deleteChat: (id: string) => invoke<void>("delete_chat", { id }),
};

/** Opens a chat's session; `onEvent` gets its events until `close()`. */
export async function connect(id: string, settings: ChatSettings, onEvent: (e: ServerEvent) => void): Promise<Connection> {
  if (isDesktop) {
    const channel = new Channel<ServerEvent>();
    channel.onmessage = onEvent;
    await invoke("open_chat", { id, ...settings, onEvent: channel });
    return {
      send: (message) => void invoke("chat_message", { id, message }).catch((e) => onEvent({ type: "error", message: String(e) })),
      close: () => void invoke("close_chat", { id }),
    };
  }
  // A browser: marv-web, one WebSocket per chat.
  const url = import.meta.env.VITE_MARV_WS ?? `ws://${location.hostname}:8787/ws`;
  const ws = new WebSocket(url);
  const queue: string[] = [];
  ws.onopen = () => {
    ws.send(JSON.stringify({ type: "hello", chatId: id, ...settings }));
    queue.splice(0).forEach((m) => ws.send(m));
  };
  ws.onmessage = (m) => onEvent(JSON.parse(m.data));
  ws.onclose = () => onEvent({ type: "closed" });
  return {
    send: (message) => {
      const text = JSON.stringify(message);
      if (ws.readyState === WebSocket.OPEN) ws.send(text);
      else queue.push(text);
    },
    close: () => ws.close(),
  };
}

export interface Model {
  id: string;
  name: string;
  context: number;
  /** Dollars per million tokens. */
  input: number;
  output: number;
  /** Runs on this machine (Ollama). */
  local?: boolean;
}

/** The models pulled into the local Ollama, as `ollama/<name>`; none when it isn't running. */
async function localModels(): Promise<Model[]> {
  try {
    const response = await fetch("http://127.0.0.1:11434/api/tags", { signal: AbortSignal.timeout(1500) });
    const { models } = await response.json();
    return (models ?? []).map((m: { name: string; size: number }) => ({
      id: `ollama/${m.name}`,
      name: `${m.name} · local, ${(m.size / 1e9).toFixed(1)} GB`,
      context: 0,
      input: 0,
      output: 0,
      local: true,
    }));
  } catch {
    return [];
  }
}

const MODELS_KEY = "marv-models";

/** Local Ollama models first, then OpenRouter's that can call tools, cheapest first (cached for an hour). */
export async function loadModels(): Promise<Model[]> {
  const [local, remote] = await Promise.all([localModels(), remoteModels().catch(() => [] as Model[])]);
  return [...local, ...remote];
}

async function remoteModels(): Promise<Model[]> {
  try {
    const cached = JSON.parse(localStorage.getItem(MODELS_KEY) ?? "null");
    if (cached && Date.now() - cached.at < 3_600_000) return cached.models;
  } catch {}
  const response = await fetch("https://openrouter.ai/api/v1/models");
  const { data } = await response.json();
  const models: Model[] = (data ?? [])
    .filter((m: { supported_parameters?: string[] }) => m.supported_parameters?.includes("tools"))
    .map((m: { id: string; name: string; context_length: number; pricing: { prompt: string; completion: string } }) => ({
      id: m.id,
      name: m.name,
      context: m.context_length,
      input: Number(m.pricing.prompt) * 1e6,
      output: Number(m.pricing.completion) * 1e6,
    }))
    // Routers price per request (-1): no fixed price to show.
    .filter((m: Model) => m.input >= 0 && m.output >= 0)
    .sort((a: Model, b: Model) => a.input - b.input);
  try {
    localStorage.setItem(MODELS_KEY, JSON.stringify({ at: Date.now(), models }));
  } catch {}
  return models;
}

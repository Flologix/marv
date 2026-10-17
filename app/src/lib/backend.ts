// What the page talks to: Marv desktop's Tauri commands, or marv-web's WebSocket in a plain browser. A chat looks
// the same either way: messages out (send, interrupt, approval, configure), the same JSON events back.
import { Channel, invoke } from "@tauri-apps/api/core";

export const isDesktop = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export interface Space {
  id: string;
  name: string;
  root: string;
  /** Apps (MCP servers) switched off in this space. */
  appsOff: string[];
}

export interface AppInfo {
  root: string | null;
  rootName: string | null;
  hasKey: boolean;
  /** "openrouter" or "ollama", once chosen at onboarding. */
  provider: string | null;
  spaces: Space[];
  active: string | null;
}

export interface LocalModel {
  id: string;
  tools: boolean;
}

export interface Entry {
  name: string;
  dir: boolean;
}

/** A chat running in the background, or on screen: whether a turn runs, and how many approvals wait. */
export interface LiveChat {
  id: string;
  busy: boolean;
  waiting: number;
}

export interface ChatRecord {
  id: string;
  /** The full path of the folder the chat runs in. */
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
  /** MCP servers switched off for chats (by name). */
  mcpOff: string[];
}

/** An MCP server as the config files have it. */
export interface ServerEntry {
  name: string;
  scope: "global" | "folder";
  target: string;
}

/** A looked-up server to add, shown before it's written; or several to choose from. */
export type Lookup =
  | { name: string; title: string; command?: string | null; text: string; warning?: string | null }
  | { choices: string[] };

export type ServerEvent = Record<string, unknown> & { type: string };

export interface Connection {
  send(message: object): void;
  close(): void;
}

export const desktop = {
  info: () => invoke<AppInfo>("app_info"),
  setRoot: (path: string) => invoke<AppInfo>("set_root", { path }),
  rootUp: () => invoke<AppInfo>("root_up"),
  /** Go into a folder: it becomes the tree's top folder. */
  enterFolder: (rel: string) => invoke<AppInfo>("enter_folder", { rel }),
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
  /** Ends a chat running in the background: it leaves the mesh. Opening it again continues it. */
  stopChat: (id: string) => invoke<void>("stop_chat", { id }),
  /** The running chats (#155), shown or not. */
  liveChats: () => invoke<LiveChat[]>("live_chats"),
  mcpServers: (folder: string) => invoke<ServerEntry[]>("mcp_servers", { folder }),
  mcpLookup: (source: string, name?: string, vars?: string[], headers?: Record<string, string>) =>
    invoke<Lookup>("mcp_lookup", { source, name, vars, headers }),
  /** Writes the looked-up server, its `${VAR}`s filled from `values`. */
  mcpInstall: (name: string, values?: Record<string, string>) => invoke<void>("mcp_install", { name, values }),
  createSpace: (name: string, path: string) => invoke<AppInfo>("create_space", { name, path }),
  switchSpace: (id: string) => invoke<AppInfo>("switch_space", { id }),
  renameSpace: (id: string, name: string) => invoke<AppInfo>("rename_space", { id, name }),
  removeSpace: (id: string) => invoke<AppInfo>("remove_space", { id }),
  setAppsOff: (off: string[]) => invoke<AppInfo>("set_apps_off", { off }),
  setProvider: (provider: "openrouter" | "ollama") => invoke<AppInfo>("set_provider", { provider }),
  localModels: () => invoke<LocalModel[]>("local_models"),
  pullModel: (name: string, onProgress: (line: string) => void) => {
    const channel = new Channel<string>();
    channel.onmessage = onProgress;
    return invoke<void>("pull_model", { name, onProgress: channel });
  },
  mcpRemove: (name: string) => invoke<void>("mcp_remove", { name }),
  /** Voxtype dictation on or off: it types into the focused field. */
  dictate: () => invoke<void>("dictate"),
  /** An https link, in the person's browser. */
  openUrl: (url: string) => invoke<void>("open_url", { url }),
};

/** Opens a chat's session; `onEvent` gets its events until `close()`. */
export async function connect(
  id: string,
  settings: ChatSettings,
  resume: { generation: number; after: number },
  onEvent: (e: ServerEvent) => void,
): Promise<Connection> {
  if (isDesktop) {
    const channel = new Channel<ServerEvent>();
    channel.onmessage = onEvent;
    // A chat still running in the background is shown again from event `after` on; it isn't started twice.
    await invoke("open_chat", { id, ...settings, ...resume, onEvent: channel });
    return {
      send: (message) => void invoke("chat_message", { id, message }).catch((e) => onEvent({ type: "error", message: String(e) })),
      // The chat keeps running (on the mesh, its turn going on); stop_chat ends it.
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
/** The models in the local Ollama (asked by the app, not the page: no browser rules in the way). */
export async function localModels(): Promise<Model[]> {
  if (!isDesktop) return [];
  try {
    return (await desktop.localModels()).map((m) => ({
      id: m.id,
      name: m.tools ? m.id.slice(7) : `${m.id.slice(7)} · no tool calls`,
      context: 0,
      input: 0,
      output: 0,
      local: true,
    }));
  } catch {
    return [];
  }
}

const MODELS_KEY = "marv-models-2";

/** Local Ollama models first, then OpenRouter's that can call tools, cheapest first (cached for an hour). */
export async function loadModels(): Promise<Model[]> {
  const [local, remote] = await Promise.all([localModels(), remoteModels().catch(() => [] as Model[])]);
  return [...local, ...remote];
}

export async function remoteModels(): Promise<Model[]> {
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
    // Batch variants only take the batch API, not chat.
    .filter((m: Model) => !m.id.endsWith(":batch"))
    .sort((a: Model, b: Model) => a.input - b.input);
  try {
    localStorage.setItem(MODELS_KEY, JSON.stringify({ at: Date.now(), models }));
  } catch {}
  return models;
}

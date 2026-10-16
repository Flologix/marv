"use client";

import { useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import styles from "./page.module.css";

const SCOPES = [
  { id: "web", label: "Web", hint: "fetch pages and search" },
  { id: "network", label: "Network", hint: "commands that use the network (npm install…)" },
  { id: "git_write", label: "Git writes", hint: "commits and other repository changes" },
  { id: "process", label: "Processes", hint: "background servers and watchers" },
  { id: "files", label: "Files", hint: "edits outside what yolo allows" },
  { id: "memory", label: "Memory", hint: "save notes across chats" },
];

interface Model {
  id: string;
  name: string;
  context: number;
  input: number;
  output: number;
}

interface Settings {
  model: string;
  approve: string[];
  maxSteps: number;
}

interface Usage {
  promptTokens: number;
  cachedTokens: number;
  completionTokens: number;
  cost: number;
}

type Item =
  | { kind: "user"; text: string }
  | { kind: "text"; text: string }
  | { kind: "thinking"; text: string }
  | { kind: "tool"; tool: string; label: string; result?: string; error?: boolean; approval?: string; done: boolean }
  | { kind: "notice"; text: string; tone: "info" | "warn" | "error" }
  | { kind: "result"; reason: string; steps: number; usage?: Usage; refused?: { tool: string; label: string }[] };

const SETTINGS_KEY = "marv-web-settings";

function loadSettings(fallbackModel: string): Settings {
  try {
    const saved = JSON.parse(localStorage.getItem(SETTINGS_KEY) ?? "null");
    if (saved?.model) return saved;
  } catch {}
  return { model: fallbackModel, approve: [], maxSteps: 50 };
}

/** Folds one of Marv's stream-json events into the transcript. */
function apply(items: Item[], event: Record<string, unknown>): Item[] {
  const last = items[items.length - 1];
  const next = items.slice();
  switch (event.type) {
    case "text_delta":
    case "thinking_delta": {
      const kind = event.type === "text_delta" ? "text" : "thinking";
      if (last?.kind === kind) next[next.length - 1] = { ...last, text: last.text + event.text };
      else next.push({ kind, text: String(event.text) });
      return next;
    }
    case "assistant":
      // The reply's whole text; already shown if it streamed in.
      if (last?.kind !== "text" && event.text) next.push({ kind: "text", text: String(event.text) });
      return next;
    case "tool_start":
      next.push({ kind: "tool", tool: String(event.tool), label: String(event.label ?? ""), done: false });
      return next;
    case "tool_end":
    case "tool_skipped": {
      // Read-only calls run in parallel, so match the oldest open call of that tool.
      const i = next.findIndex((it) => it.kind === "tool" && !it.done && it.tool === event.tool);
      const done = {
        kind: "tool" as const,
        tool: String(event.tool),
        label: String(event.label ?? (i >= 0 ? (next[i] as { label: string }).label : "")),
        result: String(event.result ?? event.reason ?? ""),
        error: event.type === "tool_skipped" || event.error === true,
        approval: event.approval as string | undefined,
        done: true,
      };
      if (i >= 0) next[i] = done;
      else next.push(done);
      return next;
    }
    case "status":
      return event.status === "compacting" ? [...next, { kind: "notice", text: "Compacting the conversation…", tone: "info" }] : items;
    case "compaction":
      return [...next, { kind: "notice", text: event.compacted ? "Conversation compacted." : `Compaction failed (${event.reason}).`, tone: event.compacted ? "info" : "warn" }];
    case "step_limit":
      return [...next, { kind: "notice", text: `Stopped at the step limit (${event.steps}).`, tone: "warn" }];
    case "cut_off":
    case "text_loop":
    case "tool_loop":
      return [...next, { kind: "notice", text: `Turn stopped: ${String(event.type).replace("_", " ")}.`, tone: "warn" }];
    case "error":
      return [...next, { kind: "notice", text: String(event.message), tone: "error" }];
    case "stderr":
      return [...next, { kind: "notice", text: String(event.text), tone: "warn" }];
    case "result":
      return [...next, { kind: "result", reason: String(event.reason), steps: Number(event.steps), usage: event.usage as Usage, refused: event.refused as { tool: string; label: string }[] }];
    default:
      return items;
  }
}

export default function Chat() {
  const [chatId, setChatId] = useState<string>("");
  const [items, setItems] = useState<Item[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [models, setModels] = useState<Model[]>([]);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [showSettings, setShowSettings] = useState(false);
  const abort = useRef<AbortController | null>(null);
  const bottom = useRef<HTMLDivElement>(null);

  useEffect(() => {
    setChatId(crypto.randomUUID());
    fetch("/api/models")
      .then((r) => r.json())
      .then((data) => {
        setModels(data.models);
        setSettings(loadSettings(data.default));
      });
  }, []);

  useEffect(() => {
    if (settings) {
      try {
        localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings));
      } catch {}
    }
  }, [settings]);

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "end" });
  }, [items]);

  const model = models.find((m) => m.id === settings?.model);
  const totalCost = items.reduce((sum, it) => sum + (it.kind === "result" ? (it.usage?.cost ?? 0) : 0), 0);

  async function send() {
    const prompt = input.trim();
    if (!prompt || busy || !settings) return;
    const resume = items.some((it) => it.kind === "result");
    setInput("");
    setItems((it) => [...it, { kind: "user", text: prompt }]);
    setBusy(true);
    abort.current = new AbortController();
    try {
      const response = await fetch("/api/chat", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ chatId, prompt, resume, ...settings }),
        signal: abort.current.signal,
      });
      if (!response.ok || !response.body) {
        const { error } = await response.json().catch(() => ({ error: response.statusText }));
        setItems((it) => [...it, { kind: "notice", text: error, tone: "error" }]);
        return;
      }
      const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
      let buffer = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += value;
        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";
        const events = lines.filter(Boolean).flatMap((line) => {
          try {
            return [JSON.parse(line)];
          } catch {
            return [];
          }
        });
        if (events.length) setItems((it) => events.reduce(apply, it));
      }
    } catch (error) {
      if ((error as Error).name === "AbortError") setItems((it) => [...it, { kind: "notice", text: "Stopped.", tone: "warn" }]);
      else setItems((it) => [...it, { kind: "notice", text: String(error), tone: "error" }]);
    } finally {
      setBusy(false);
      abort.current = null;
    }
  }

  function newChat() {
    abort.current?.abort();
    setChatId(crypto.randomUUID());
    setItems([]);
  }

  return (
    <div className={styles.shell}>
      <header className={styles.header}>
        <div className={styles.brand}>
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img src="/marv.svg" alt="" width={28} height={28} />
          <span>Marv</span>
        </div>
        <button className={styles.chip} onClick={() => setShowSettings((s) => !s)} title="Settings">
          {settings?.model ?? "…"}
          {settings && settings.approve.length > 0 && <span className={styles.badge}>+{settings.approve.length}</span>}
        </button>
        <span className={styles.cost}>${totalCost.toFixed(4)}</span>
        <button className={styles.ghost} onClick={newChat}>New chat</button>
      </header>

      {showSettings && settings && (
        <section className={styles.settings}>
          <label className={styles.field}>
            <span>Model</span>
            <input
              list="models"
              value={settings.model}
              onChange={(e) => setSettings({ ...settings, model: e.target.value.trim() })}
              spellCheck={false}
            />
            <datalist id="models">
              {models.map((m) => (
                <option key={m.id} value={m.id}>{`${m.name} · $${m.input.toFixed(2)}/$${m.output.toFixed(2)} per M`}</option>
              ))}
            </datalist>
            <small>
              {model
                ? `${Math.round(model.context / 1000)}k context · $${model.input.toFixed(2)} in / $${model.output.toFixed(2)} out per million tokens`
                : "Any OpenRouter model id that supports tools."}
            </small>
          </label>
          <label className={styles.field}>
            <span>Step limit</span>
            <input
              type="number"
              min={0}
              max={500}
              value={settings.maxSteps}
              onChange={(e) => setSettings({ ...settings, maxSteps: Math.max(0, Math.min(500, Number(e.target.value) || 0)) })}
            />
            <small>0 never stops.</small>
          </label>
          <fieldset className={styles.field}>
            <span>Allow without asking</span>
            <small>Sandboxed commands and edits in the workspace always run. Anything else is refused unless allowed here (there&apos;s no one to ask).</small>
            <div className={styles.scopes}>
              {SCOPES.map((s) => (
                <label key={s.id} title={s.hint}>
                  <input
                    type="checkbox"
                    checked={settings.approve.includes(s.id)}
                    onChange={(e) =>
                      setSettings({
                        ...settings,
                        approve: e.target.checked ? [...settings.approve, s.id] : settings.approve.filter((a) => a !== s.id),
                      })
                    }
                  />
                  {s.label}
                </label>
              ))}
            </div>
          </fieldset>
        </section>
      )}

      <main className={styles.transcript}>
        {items.length === 0 && (
          <div className={styles.empty}>
            {/* eslint-disable-next-line @next/next/no-img-element */}
            <img src="/marv.svg" alt="" width={96} height={96} />
            <p>Ask Marv to build something. Each chat gets its own sandboxed workspace.</p>
          </div>
        )}
        {items.map((item, i) => (
          <ItemView key={i} item={item} />
        ))}
        {busy && <div className={styles.working}>Marv is working…</div>}
        <div ref={bottom} />
      </main>

      <form
        className={styles.composer}
        onSubmit={(e) => {
          e.preventDefault();
          send();
        }}
      >
        <textarea
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              send();
            }
          }}
          placeholder="Message Marv (Enter to send, Shift+Enter for a new line)"
          rows={2}
          autoFocus
        />
        {busy ? (
          <button type="button" className={styles.stop} onClick={() => abort.current?.abort()}>Stop</button>
        ) : (
          <button type="submit" disabled={!input.trim() || !settings}>Send</button>
        )}
      </form>
    </div>
  );
}

function ItemView({ item }: { item: Item }) {
  switch (item.kind) {
    case "user":
      return <div className={styles.user}>{item.text}</div>;
    case "text":
      return (
        <div className={styles.reply}>
          <Markdown remarkPlugins={[remarkGfm]}>{item.text}</Markdown>
        </div>
      );
    case "thinking":
      return (
        <details className={styles.thinking}>
          <summary>Thinking</summary>
          <div>{item.text}</div>
        </details>
      );
    case "tool":
      return (
        <details className={`${styles.tool} ${item.error ? styles.toolError : ""}`}>
          <summary>
            <span className={styles.toolName}>{item.tool}</span>
            <span className={styles.toolLabel}>{item.label}</span>
            {!item.done && <span className={styles.spinner} aria-label="running" />}
            {item.approval && <span className={styles.approval}>{item.approval}</span>}
          </summary>
          {item.result !== undefined && <pre>{item.result || "(no output)"}</pre>}
        </details>
      );
    case "notice":
      return <div className={`${styles.notice} ${styles[item.tone]}`}>{item.text}</div>;
    case "result":
      return (
        <div className={styles.result}>
          {item.reason} · {item.steps} step{item.steps === 1 ? "" : "s"}
          {item.usage &&
            ` · ${item.usage.promptTokens.toLocaleString()} in (${item.usage.cachedTokens.toLocaleString()} cached) / ${item.usage.completionTokens.toLocaleString()} out · $${item.usage.cost.toFixed(4)}`}
          {item.refused && item.refused.length > 0 && (
            <div className={styles.refused}>
              Refused (allow in settings): {item.refused.map((r) => r.label || r.tool).join(", ")}
            </div>
          )}
        </div>
      );
  }
}

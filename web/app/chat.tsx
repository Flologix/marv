"use client";

import { useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { type ApprovalRequest, type DiffLine, type Item, type Settings, useMarv } from "@/lib/useMarv";
import styles from "./chat.module.css";

interface Model {
  id: string;
  name: string;
  context: number;
  input: number;
  output: number;
}

const SETTINGS_KEY = "marv-web-settings";
const CHAT_KEY = "marv-web-chat";

const SUGGESTIONS = [
  "Build a tiny CLI that converts CSV to JSON, with tests",
  "Write a Python script that prints the first 20 primes, then run it",
  "Set up a minimal Express server with a /health route",
];

function stored<T>(key: string): T | null {
  try {
    return JSON.parse(localStorage.getItem(key) ?? "null");
  } catch {
    return null;
  }
}

function keep(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {}
}

const tokens = (n: number) =>
  n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : n >= 1000 ? `${(n / 1000).toFixed(n >= 100_000 ? 0 : 1)}k` : String(n);

export default function Chat() {
  const [chatId, setChatId] = useState("");
  const [settings, setSettings] = useState<Settings | null>(null);
  const [models, setModels] = useState<Model[]>([]);
  const [open, setOpen] = useState(false);
  const [input, setInput] = useState("");
  const marv = useMarv(chatId, settings);
  const bottom = useRef<HTMLDivElement>(null);
  const prompt = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    fetch("/api/models")
      .then((r) => r.json())
      .then((data) => {
        setChatId(stored<string>(CHAT_KEY) ?? crypto.randomUUID());
        setModels(data.models);
        setSettings(stored<Settings>(SETTINGS_KEY) ?? { model: data.default, maxSteps: 50, yolo: true });
      });
  }, []);

  useEffect(() => {
    if (chatId) keep(CHAT_KEY, chatId);
  }, [chatId]);

  useEffect(() => {
    if (settings) keep(SETTINGS_KEY, settings);
  }, [settings]);

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "end", behavior: "smooth" });
  }, [marv.items]);

  const cost = marv.items.reduce((sum, it) => sum + (it.kind === "turn" ? (it.usage?.cost ?? 0) : 0), 0);
  const lastUsage = [...marv.items].reverse().find((it) => it.kind === "turn" && it.usage);
  const context = lastUsage?.kind === "turn" ? lastUsage.usage : undefined;
  const waiting = marv.items.some((it) => it.kind === "approval" && !it.decision);

  function submit(text = input) {
    const message = text.trim();
    if (!message || marv.busy) return;
    if (marv.send(message)) setInput("");
  }

  function newChat() {
    setChatId(crypto.randomUUID());
    setInput("");
    prompt.current?.focus();
  }

  return (
    <div className={styles.app}>
      <header className={styles.bar}>
        <div className={styles.brand}>
          <i aria-hidden />
          Marv
        </div>
        <span className={`${styles.dot} ${styles[marv.status]}`} title={marv.status} />
        <div className={styles.spacer} />
        {cost > 0 && <span className={styles.meta}>${cost.toFixed(4)}</span>}
        <button className={`${styles.btn} ${styles.chip}`} onClick={() => setOpen((o) => !o)} aria-expanded={open}>
          {settings?.model ?? "…"}
        </button>
        <button className={`${styles.btn} ${styles.ghost}`} onClick={newChat}>
          New chat
        </button>
        {open && settings && <SettingsPanel settings={settings} models={models} onChange={setSettings} onClose={() => setOpen(false)} />}
      </header>

      <main className={styles.scroll}>
        <div className={styles.column}>
          {marv.items.length === 0 ? (
            <section className={styles.empty}>
              <p className={styles.eyebrow}>Coding agent · sandboxed</p>
              <h1>What should Marv build?</h1>
              <p className={styles.lede}>
                Marv works in its own workspace. It runs sandboxed commands and edits on its own, and asks you before anything
                leaves the sandbox.
              </p>
              <div className={styles.suggestions}>
                {SUGGESTIONS.map((s) => (
                  <button key={s} className={`${styles.btn} ${styles.secondary}`} onClick={() => submit(s)} disabled={marv.status !== "ready"}>
                    {s}
                  </button>
                ))}
              </div>
            </section>
          ) : (
            marv.items.map((item, i) => <ItemView key={i} item={item} onAnswer={marv.answer} />)
          )}
          {marv.busy && !waiting && (
            <div className={styles.working}>
              <span className={styles.spinner} /> Working
            </div>
          )}
          {marv.error && <div className={`${styles.alert} ${styles.alertErr}`}>{marv.error}</div>}
          <div ref={bottom} />
        </div>
      </main>

      <footer className={styles.dock}>
        <form
          className={styles.composer}
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <textarea
            ref={prompt}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                submit();
              }
            }}
            placeholder={marv.status === "ready" ? "Ask Marv to build, fix or explain something" : "Connecting to marv-web…"}
            rows={1}
            autoFocus
          />
          {marv.busy ? (
            <button type="button" className={`${styles.btn} ${styles.secondary}`} onClick={marv.interrupt}>
              Stop
            </button>
          ) : (
            <button type="submit" className={`${styles.btn} ${styles.primary}`} disabled={!input.trim() || marv.status !== "ready"}>
              Send
            </button>
          )}
        </form>
        <p className={styles.hint}>
          {context?.context && context.contextLength
            ? `Context ${tokens(context.context)} of ${tokens(context.contextLength)}`
            : "Enter to send · Shift+Enter for a new line"}
        </p>
      </footer>
    </div>
  );
}

function SettingsPanel({
  settings,
  models,
  onChange,
  onClose,
}: {
  settings: Settings;
  models: Model[];
  onChange: (s: Settings) => void;
  onClose: () => void;
}) {
  const [model, setModel] = useState(settings.model);
  const info = models.find((m) => m.id === model);
  return (
    <div className={styles.panel} role="dialog" aria-label="Settings">
      <div className={styles.field}>
        <label htmlFor="model">Model</label>
        <input
          id="model"
          list="models"
          value={model}
          spellCheck={false}
          onChange={(e) => setModel(e.target.value.trim())}
          onBlur={() => model && onChange({ ...settings, model })}
          onKeyDown={(e) => e.key === "Enter" && model && onChange({ ...settings, model })}
        />
        <datalist id="models">
          {models.map((m) => (
            <option key={m.id} value={m.id}>{`${m.name} · $${m.input.toFixed(2)} / $${m.output.toFixed(2)}`}</option>
          ))}
        </datalist>
        <span className={styles.help}>
          {info
            ? `${tokens(info.context)} context · $${info.input.toFixed(2)} in, $${info.output.toFixed(2)} out per million tokens`
            : "Any OpenRouter model that supports tools."}
        </span>
      </div>
      <div className={styles.field}>
        <label htmlFor="steps">Steps before asking to continue</label>
        <input
          id="steps"
          type="number"
          min={0}
          max={500}
          value={settings.maxSteps}
          onChange={(e) => onChange({ ...settings, maxSteps: Math.max(0, Math.min(500, Number(e.target.value) || 0)) })}
        />
        <span className={styles.help}>0 never asks.</span>
      </div>
      <div className={styles.switchRow}>
        <button
          type="button"
          role="switch"
          aria-checked={settings.yolo}
          aria-labelledby="yolo-label"
          className={`${styles.toggle} ${settings.yolo ? styles.on : ""}`}
          onClick={() => onChange({ ...settings, yolo: !settings.yolo })}
        />
        <span>
          <b id="yolo-label">Run sandboxed work without asking</b>
          <span className={styles.help}>Off: approve every edit and command.</span>
        </span>
      </div>
      <button className={`${styles.btn} ${styles.ghost} ${styles.done}`} onClick={onClose}>
        Done
      </button>
    </div>
  );
}

function ItemView({ item, onAnswer }: { item: Item; onAnswer: (id: number, d: "yes" | "always" | "no") => void }) {
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
          <summary>Thought</summary>
          <p>{item.text}</p>
        </details>
      );
    case "tool":
      return (
        <details className={`${styles.tool} ${item.agent ? styles.sub : ""}`}>
          <summary>
            {!item.done ? <span className={styles.spinner} /> : <span className={`${styles.mark} ${item.error ? styles.markErr : styles.markOk}`} />}
            <span className={styles.toolName}>{item.tool}</span>
            <span className={styles.toolLabel}>{item.label}</span>
            {item.summary && <span className={styles.toolSummary}>{item.summary}</span>}
          </summary>
          {item.diff && item.diff.lines.length > 0 && <Diff lines={item.diff.lines} more={item.diff.more} />}
          {item.result && !item.diff && <pre className={styles.code}>{item.result}</pre>}
        </details>
      );
    case "approval":
      return <Approval request={item.request} decision={item.decision} onAnswer={onAnswer} />;
    case "notice":
      return <p className={`${styles.notice} ${styles[item.tone]}`}>{item.text}</p>;
    case "turn":
      return (
        <p className={styles.turn}>
          {item.reason === "end" ? "Done" : item.reason}
          {item.usage && ` · ${tokens(item.usage.promptTokens)} in · ${tokens(item.usage.completionTokens)} out · $${item.usage.cost.toFixed(4)}`}
        </p>
      );
  }
}

function Approval({
  request: r,
  decision,
  onAnswer,
}: {
  request: ApprovalRequest;
  decision?: string;
  onAnswer: (id: number, d: "yes" | "always" | "no") => void;
}) {
  if (decision) {
    const word = decision === "no" ? "Declined" : decision === "always" ? "Allowed for the session" : "Allowed";
    return (
      <p className={`${styles.decided} ${decision === "no" ? styles.declined : ""}`}>
        {word} · <span>{r.command ?? r.label}</span>
      </p>
    );
  }
  return (
    <section className={styles.approval} aria-live="assertive">
      <p className={styles.eyebrow}>{r.agent ? `${r.agent} asks` : "Marv asks"}</p>
      <h3>{r.title}</h3>
      {r.command && <pre className={styles.code}>$ {r.command}</pre>}
      {r.diff && r.diff.length > 0 && <Diff lines={r.diff} more={0} />}
      {r.text && <pre className={styles.code}>{r.text}</pre>}
      {r.warning && <div className={`${styles.alert} ${styles.alertWarn}`}>{r.warning}</div>}
      {r.note && <p className={styles.help}>{r.note}</p>}
      <div className={styles.actions}>
        <button className={`${styles.btn} ${styles.primary}`} onClick={() => onAnswer(r.id, "yes")} autoFocus>
          Allow
        </button>
        {!r.once && (
          <button className={`${styles.btn} ${styles.secondary}`} onClick={() => onAnswer(r.id, "always")}>
            Allow {r.scope} this session
          </button>
        )}
        <button className={`${styles.btn} ${styles.ghost}`} onClick={() => onAnswer(r.id, "no")}>
          Decline
        </button>
      </div>
    </section>
  );
}

function Diff({ lines, more }: { lines: DiffLine[]; more: number }) {
  return (
    <pre className={`${styles.code} ${styles.diff}`}>
      {lines.map((l, i) => (
        <span key={i} className={styles[l.kind]}>
          {l.kind === "gap" ? "…" : `${l.kind === "add" ? "+" : l.kind === "del" ? "−" : " "} ${l.text}`}
          {"\n"}
        </span>
      ))}
      {more > 0 && <span className={styles.gap}>… {more} more lines</span>}
    </pre>
  );
}

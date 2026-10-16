import { type ReactNode, useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ChatSettings, Model } from "../lib/backend";
import type { ApprovalRequest, DiffLine, Item, useChat } from "../lib/chat";
import { tokens } from "../lib/format";
import { Martian } from "./Martian";
import { Settings } from "./Settings";

type Chat = ReturnType<typeof useChat>;

export function ChatView({
  chat,
  crumbs,
  empty,
  settings,
  onSettings,
  models,
  onSend,
  left,
}: {
  chat: Chat;
  crumbs: ReactNode;
  empty: ReactNode;
  settings: ChatSettings;
  onSettings: (s: ChatSettings) => void;
  models: Model[];
  onSend: (text: string) => void;
  left?: ReactNode;
}) {
  const [input, setInput] = useState("");
  const [open, setOpen] = useState(false);
  const bottom = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "end" });
  }, [chat.items]);

  const cost = chat.items.reduce((sum, it) => sum + (it.kind === "turn" ? (it.usage?.cost ?? 0) : 0), 0);
  const last = [...chat.items].reverse().find((it) => it.kind === "turn" && it.usage);
  const usage = last?.kind === "turn" ? last.usage : undefined;
  const asking = chat.items.some((it) => it.kind === "approval" && !it.decision);

  function submit() {
    const text = input.trim();
    if (!text || chat.busy) return;
    onSend(text);
    setInput("");
  }

  return (
    <div className="main">
      <header className="topbar">
        {left}
        <div className="crumbs">{crumbs}</div>
        {chat.status !== "idle" && <span className={`status-dot ${chat.status}`} title={chat.status} />}
        {cost > 0 && <span className="faint">${cost.toFixed(4)}</span>}
        <button className="chip" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
          {settings.model}
        </button>
        {open && <Settings settings={settings} models={models} onChange={onSettings} onClose={() => setOpen(false)} />}
      </header>

      <main className="scroll">
        <div className="column">
          {chat.items.length === 0 ? empty : chat.items.map((item, i) => <ItemView key={i} item={item} onAnswer={chat.answer} />)}
          {chat.busy && !asking && (
            <div className="working">
              <Martian size={12} className="toon bob" /> working
            </div>
          )}
          {chat.error && <p className="notice error">{chat.error}</p>}
          <div ref={bottom} />
        </div>
      </main>

      <footer className="dock">
        <form
          className="composer"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <textarea
            value={input}
            rows={1}
            autoFocus
            placeholder="Ask Marv to build, fix or explain something"
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                submit();
              }
            }}
          />
          {chat.busy ? (
            <button type="button" className="btn" onClick={chat.interrupt}>
              Stop
            </button>
          ) : (
            <button type="submit" className="btn primary" disabled={!input.trim()}>
              Send
            </button>
          )}
        </form>
        <div className="hint">
          <span>enter to send · shift+enter for a new line</span>
          {usage?.context && usage.contextLength ? <span>context {tokens(usage.context)} / {tokens(usage.contextLength)}</span> : null}
        </div>
      </footer>
    </div>
  );
}

function ItemView({ item, onAnswer }: { item: Item; onAnswer: (id: number, d: "yes" | "always" | "no") => void }) {
  switch (item.kind) {
    case "user":
      return <div className="user">{item.text}</div>;
    case "text":
      return (
        <div className="reply">
          <Markdown remarkPlugins={[remarkGfm]}>{item.text}</Markdown>
        </div>
      );
    case "thinking":
      return item.text.trim() ? (
        <details className="thinking">
          <summary>thought</summary>
          <p>{item.text.trim()}</p>
        </details>
      ) : null;
    case "tool":
      return (
        <details className={`tool${item.agent ? " sub" : ""}`}>
          <summary>
            {item.done ? <span className={`mark${item.error ? " err" : ""}`} /> : <span className="spinner" />}
            <span className="name">{item.tool}</span>
            <span className="label">{item.label}</span>
            {item.summary && <span className="summary">{item.summary}</span>}
          </summary>
          {item.diff && item.diff.lines.length > 0 ? (
            <Diff lines={item.diff.lines} more={item.diff.more} />
          ) : (
            item.result && <pre className="code">{item.result}</pre>
          )}
        </details>
      );
    case "approval":
      return <Approval request={item.request} decision={item.decision} onAnswer={onAnswer} />;
    case "notice":
      return <p className={`notice ${item.tone}`}>{item.text}</p>;
    case "turn":
      return (
        <p className="turn">
          {item.reason === "end" ? "done" : item.reason}
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
    const word = decision === "no" ? "declined" : decision === "always" ? "allowed for this chat" : "allowed";
    return (
      <p className={`decided${decision === "no" ? " no" : ""}`}>
        {word} · <span>{r.command ?? r.label}</span>
      </p>
    );
  }
  return (
    <section className="approval" aria-live="assertive">
      <span className="who">{r.agent ? `${r.agent} asks` : "Marv asks"}</span>
      <h3>{r.title}</h3>
      {r.command && <pre className="code">$ {r.command}</pre>}
      {r.diff && r.diff.length > 0 && <Diff lines={r.diff} more={0} />}
      {r.text && <pre className="code">{r.text}</pre>}
      {r.warning && <div className="warning">{r.warning}</div>}
      {r.note && <span className="note">{r.note}</span>}
      <div className="buttons">
        <button className="btn primary" autoFocus onClick={() => onAnswer(r.id, "yes")}>
          Allow
        </button>
        {!r.once && (
          <button className="btn" onClick={() => onAnswer(r.id, "always")}>
            Allow {r.scope} for this chat
          </button>
        )}
        <button className="btn plain" onClick={() => onAnswer(r.id, "no")}>
          Decline
        </button>
      </div>
    </section>
  );
}

function Diff({ lines, more }: { lines: DiffLine[]; more: number }) {
  return (
    <pre className="code diff">
      {lines.map((l, i) => (
        <span key={i} className={l.kind}>
          {l.kind === "gap" ? "…" : `${l.kind === "add" ? "+" : l.kind === "del" ? "-" : " "} ${l.text}`}
        </span>
      ))}
      {more > 0 && <span className="gap">… {more} more lines</span>}
    </pre>
  );
}

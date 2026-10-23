import { type ReactNode, useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { type ChatSettings, type Model, desktop, isDesktop } from "../lib/backend";
import { type Answer, type ApprovalRequest, type Command, type DiffLine, type Item, knownCommands, type useChat } from "../lib/chat";
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
  hasKey,
  onSetupPlan,
  onSend,
  left,
}: {
  chat: Chat;
  crumbs: ReactNode;
  empty: ReactNode;
  settings: ChatSettings;
  onSettings: (s: ChatSettings) => void;
  models: Model[];
  hasKey?: boolean;
  /** #159: opens the plan setup (only when this build offers a plan). */
  onSetupPlan?: () => void;
  onSend: (text: string) => void;
  left?: ReactNode;
}) {
  const [input, setInput] = useState("");
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [dismissed, setDismissed] = useState(false);
  const [listening, setListening] = useState(false);
  const promptRef = useRef<HTMLTextAreaElement>(null);

  // Voxtype types into the focused field: the prompt, then.
  function dictate() {
    promptRef.current?.focus();
    desktop.dictate().then(
      () => setListening((l) => !l),
      () => setListening(false),
    );
  }
  const all: Command[] = chat.commands.length ? chat.commands : knownCommands();
  // The / menu: while the first word is still being typed.
  const typing = input.startsWith("/") && !input.includes(" ") && !dismissed;
  const matches = typing ? all.filter((c) => c.name.startsWith(input.slice(1).toLowerCase())).slice(0, 50) : [];

  function pick(c: Command) {
    setInput(`/${c.name} `);
    setActive(0);
  }
  const bottom = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "end" });
  }, [chat.items]);

  const cost = chat.items.reduce((sum, it) => sum + (it.kind === "turn" ? (it.usage?.cost ?? 0) : 0), 0);
  const last = [...chat.items].reverse().find((it) => it.kind === "turn" && it.usage);
  const usage = last?.kind === "turn" ? last.usage : undefined;
  const asking = chat.items.some((it) => it.kind === "approval" && !it.decision && !it.restored);

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
        {chat.meshName && (
          <span className="faint" title={`On the mesh as ${chat.meshName}: other Marvs see it (marv mesh ls) and can ask you for a wire`}>
            mesh · {chat.meshName}
          </span>
        )}
        {chat.status !== "idle" && <span className={`status-dot ${chat.status}`} title={chat.status} />}
        {cost > 0 && <span className="faint">${cost.toFixed(4)}</span>}
        <button className="chip" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
          {settings.model}
        </button>
        {open && <Settings settings={settings} models={models} hasKey={hasKey} onSetupPlan={onSetupPlan} onChange={onSettings} onClose={() => setOpen(false)} />}
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
        <div className="composer-wrap">
        {matches.length > 0 && (
          <div className="slash" role="listbox">
            {matches.map((c, i) => (
              <button
                key={c.name}
                type="button"
                role="option"
                aria-selected={i === active}
                className={i === Math.min(active, matches.length - 1) ? "active" : ""}
                onMouseDown={(e) => {
                  e.preventDefault();
                  pick(c);
                }}
              >
                <span className={`cmd${c.skill ? " skill" : ""}`}>/{c.name}</span>
                <span className="desc">{c.description}</span>
              </button>
            ))}
          </div>
        )}
        <form
          className="composer"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <textarea
            ref={promptRef}
            value={input}
            rows={1}
            autoFocus
            placeholder="Ask Marv to build, fix or explain something, or type / for commands"
            onChange={(e) => {
              setInput(e.target.value);
              setActive(0);
              setDismissed(false);
            }}
            onKeyDown={(e) => {
              if (matches.length > 0) {
                const current = Math.min(active, matches.length - 1);
                if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                  e.preventDefault();
                  setActive((current + (e.key === "ArrowDown" ? 1 : matches.length - 1)) % matches.length);
                  return;
                }
                if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey && `/${matches[current].name}` !== input)) {
                  e.preventDefault();
                  pick(matches[current]);
                  return;
                }
                if (e.key === "Escape") {
                  setDismissed(true);
                  return;
                }
              }
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                submit();
              }
            }}
          />
          {isDesktop && (
            <button
              type="button"
              className={`mic${listening ? " on" : ""}`}
              title={listening ? "Stop dictation (Super+Ctrl+X)" : "Dictate (Super+Ctrl+X)"}
              onClick={dictate}
            >
              {listening ? "● rec" : "mic"}
            </button>
          )}
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
        </div>
        <div className="hint">
          <span>enter to send · shift+enter for a new line · / for commands and skills</span>
          {usage?.context && usage.contextLength ? <span>context {tokens(usage.context)} / {tokens(usage.contextLength)}</span> : null}
        </div>
      </footer>
    </div>
  );
}

function ItemView({ item, onAnswer }: { item: Item; onAnswer: (id: number, d: Answer) => void }) {
  switch (item.kind) {
    case "user":
      return <div className="user">{item.text}</div>;
    case "text":
      return (
        <div className="reply">
          <Markdown remarkPlugins={[remarkGfm]}>{item.text}</Markdown>
          <CopyButton text={item.text} />
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
      return <Approval request={item.request} decision={item.decision} reason={item.reason} restored={item.restored} onAnswer={onAnswer} />;
    case "notice":
      return <p className={`notice ${item.tone}`}>{item.text}</p>;
    case "output":
      return (
        <div className="output">
          <Markdown remarkPlugins={[remarkGfm]}>{item.text}</Markdown>
        </div>
      );
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
  reason,
  restored,
  onAnswer,
}: {
  request: ApprovalRequest;
  decision?: string;
  reason?: string;
  /** From the saved transcript, not yet known to still wait: no buttons until the session says. */
  restored?: boolean;
  onAnswer: (id: number, d: Answer) => void;
}) {
  // #185: "Decline and say why" opens a one-line reason; the turn goes on with it. Only for a model's call
  // (`reason`): the engine says which.
  const [why, setWhy] = useState<string | null>(null);
  if (decision || restored) {
    // "expired": the session that asked ended first (Marv restarted), so there's nothing left to answer.
    const idle = !decision || decision === "expired";
    const no = decision === "no" || decision === "declined";
    const word = !decision
      ? "waiting for Marv"
      : decision === "expired"
        ? "not answered (Marv restarted)"
        : decision === "no" || decision === "declined"
          ? "declined"
          : decision === "always"
            ? "allowed for this chat"
            : "allowed";
    return (
      <p className={`decided${idle ? " idle" : no ? " no" : ""}`}>
        {word} · <span>{r.command ?? r.label}</span>
        {reason && <span> · “{reason}”</span>}
      </p>
    );
  }
  const sendWhy = () => {
    const text = (why ?? "").trim();
    if (text) onAnswer(r.id, { declined: text });
  };
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
        {r.reason && why === null && (
          <button className="btn plain" onClick={() => setWhy("")}>
            Decline and say why
          </button>
        )}
      </div>
      {why !== null && (
        <form
          className="why"
          onSubmit={(e) => {
            e.preventDefault();
            sendWhy();
          }}
        >
          <input
            autoFocus
            value={why}
            maxLength={500} /* the engine's MAX_DECLINE_REASON; it cuts there too */
            placeholder="Why not? Marv reads this and carries on."
            onChange={(e) => setWhy(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setWhy(null);
            }}
          />
          <button type="submit" className="btn" disabled={!why.trim()}>
            Send
          </button>
        </form>
      )}
    </section>
  );
}

/** Copies a reply as Markdown, the way it was written. */
function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // Older webviews: through a hidden text box.
      const area = Object.assign(document.createElement("textarea"), { value: text });
      document.body.append(area);
      area.select();
      document.execCommand("copy");
      area.remove();
    }
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  }
  return (
    <button className="copy" title="Copy this reply" onClick={() => void copy()}>
      {copied ? "copied" : "copy"}
    </button>
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

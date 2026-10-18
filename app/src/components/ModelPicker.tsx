import { useEffect, useState } from "react";
import { type Model, desktop, isDesktop, localModels } from "../lib/backend";
import { tokens } from "../lib/format";

/** Models Marv runs well on this machine: tool calls, and they fit a 12 GB GPU. */
const SUGGESTED = [
  { id: "gemma4:12b", note: "8 GB · tools, thinking" },
  { id: "qwen3:14b", note: "9 GB · tools, thinking" },
];

/** Where Marv's model comes from: OpenRouter (cloud, a key), a plan set up here (Z.ai Coding Plan, ChatGPT), or Ollama
 * (this machine, free). */
export function ModelPicker({
  model,
  remote,
  hasKey,
  onPick,
}: {
  model: string;
  remote: Model[];
  hasKey: boolean;
  onPick: (id: string) => void;
}) {
  const plans = remote.filter((m) => m.plan);
  const [tab, setTab] = useState<"openrouter" | "plan" | "ollama">(
    model.startsWith("ollama/") ? "ollama" : plans.some((m) => m.id === model) ? "plan" : "openrouter",
  );
  const [query, setQuery] = useState("");
  const [local, setLocal] = useState<Model[] | null>(null);
  const [pulling, setPulling] = useState<string | null>(null);
  const [progress, setProgress] = useState("");
  const [error, setError] = useState<string | null>(null);

  const refresh = () =>
    desktop.localModels().then(
      () => localModels().then(setLocal),
      (e) => {
        setLocal([]);
        setError(String(e));
      },
    );

  useEffect(() => {
    if (tab === "ollama" && isDesktop) void refresh();
  }, [tab]);

  async function pull(name: string) {
    setError(null);
    setPulling(name);
    setProgress("starting…");
    try {
      await desktop.pullModel(name, setProgress);
      await refresh();
      onPick(`ollama/${name}`);
    } catch (e) {
      setError(String(e));
    } finally {
      setPulling(null);
    }
  }

  const q = query.trim().toLowerCase();
  const shown = remote
    .filter((m) => !m.plan)
    .filter((m) => !q || m.id.toLowerCase().includes(q) || m.name.toLowerCase().includes(q))
    .slice(0, 80);

  return (
    <div className="picker">
      <div className="picker-tabs" role="tablist">
        <button role="tab" aria-selected={tab === "openrouter"} className={tab === "openrouter" ? "active" : ""} onClick={() => setTab("openrouter")}>
          OpenRouter
        </button>
        {plans.length > 0 && (
          <button role="tab" aria-selected={tab === "plan"} className={tab === "plan" ? "active" : ""} onClick={() => setTab("plan")}>
            Your plan
          </button>
        )}
        {isDesktop && (
          <button role="tab" aria-selected={tab === "ollama"} className={tab === "ollama" ? "active" : ""} onClick={() => setTab("ollama")}>
            Ollama · local
          </button>
        )}
      </div>

      {tab === "plan" ? (
        <>
          <p className="help">Paid by your plan, within its usage limits. When a limit is reached Marv stops and says so.</p>
          <div className="model-list">
            {plans.map((m) => (
              <button key={m.id} className={`model${m.id === model ? " active" : ""}`} onClick={() => onPick(m.id)} title={m.id}>
                <span className="model-id">{m.name}</span>
                <span className="model-meta">
                  {m.context ? `${tokens(m.context)} · ` : ""}
                  {m.plan}
                </span>
              </button>
            ))}
          </div>
        </>
      ) : tab === "openrouter" ? (
        <>
          {!hasKey && <p className="help warn">Add an OpenRouter key (first-run setup) to use these.</p>}
          <input className="search mono" placeholder="search models" value={query} spellCheck={false} onChange={(e) => setQuery(e.target.value)} />
          <div className="model-list">
            {shown.map((m) => (
              <button key={m.id} className={`model${m.id === model ? " active" : ""}`} onClick={() => onPick(m.id)} title={m.name}>
                <span className="model-id">{m.id}</span>
                <span className="model-meta">
                  {tokens(m.context)} · ${m.input.toFixed(2)}/${m.output.toFixed(2)}
                </span>
              </button>
            ))}
            {shown.length === 0 && <p className="help">No match.</p>}
          </div>
        </>
      ) : (
        <>
          <p className="help">Runs on this machine: free, private, and works offline. The first message loads it into memory.</p>
          <div className="model-list">
            {local?.map((m) => (
              <button key={m.id} className={`model${m.id === model ? " active" : ""}`} onClick={() => onPick(m.id)}>
                <span className="model-id">{m.name}</span>
                <span className="model-meta">local</span>
              </button>
            ))}
            {local && local.length === 0 && !error && <p className="help">No models yet: download one below.</p>}
          </div>
          <div className="pull">
            <span className="help">Download</span>
            {SUGGESTED.filter((s) => !local?.some((m) => m.id === `ollama/${s.id}`)).map((s) => (
              <button key={s.id} className="btn" disabled={pulling !== null} onClick={() => void pull(s.id)}>
                {s.id}&nbsp;<span className="faint">{s.note}</span>
              </button>
            ))}
          </div>
          {pulling && (
            <p className="help mono">
              {pulling}: {progress}
            </p>
          )}
        </>
      )}
      {error && <p className="notice error">{error}</p>}
    </div>
  );
}

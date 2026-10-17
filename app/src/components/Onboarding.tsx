import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useState } from "react";
import { type AppInfo, type LocalModel, desktop } from "../lib/backend";
import { Gallery } from "./Gallery";
import { Martian } from "./Martian";

const SUGGESTED = ["gemma4:12b", "qwen3:14b"];

/**
 * First run, three steps: how Marv thinks (Ollama on this machine, or OpenRouter in the cloud), the first space's
 * folder, and the apps to connect (skippable). Each step shows only while it's needed.
 */
export function Onboarding({
  info,
  onInfo,
  onModel,
  onFinish,
}: {
  info: AppInfo;
  onInfo: (info: AppInfo) => void;
  onModel: (model: string) => void;
  onFinish: () => void;
}) {
  const step = !info.hasKey && info.provider !== "ollama" ? "brain" : !info.root ? "folder" : "apps";
  const [installed, setInstalled] = useState<string[]>([]);
  const refresh = () => desktop.mcpServers("").then((s) => setInstalled(s.map((e) => e.name)), () => {});

  useEffect(() => {
    if (step === "apps") void refresh();
  }, [step]);

  return (
    <div className="onboarding">
      <div className={`card${step === "apps" ? " wide" : ""}`}>
        <Martian size={56} className="toon" />
        <div className="brand">
          <img className="flai" src="/flai.png" alt="FloLogix AI" /> marv desktop
          <span className="steps">
            {["brain", "folder", "apps"].map((s) => (
              <i key={s} className={s === step ? "on" : ""} />
            ))}
          </span>
        </div>
        {step === "brain" && <Brain onInfo={onInfo} onModel={onModel} />}
        {step === "folder" && <Folder onInfo={onInfo} />}
        {step === "apps" && (
          <>
            <h1>Connect your apps</h1>
            <p>Marv can work in them for you: answer mail, post to Slack, review pull requests, check a board. Add them now or later from Servers.</p>
            <Gallery installed={installed} onInstalled={() => void refresh()} />
            <div>
              <button className="btn primary" onClick={onFinish}>
                {installed.length ? "Done" : "Skip for now"}
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function Brain({ onInfo, onModel }: { onInfo: (info: AppInfo) => void; onModel: (model: string) => void }) {
  const [local, setLocal] = useState<LocalModel[] | null>(null);
  const [ollamaError, setOllamaError] = useState<string | null>(null);
  const [key, setKey] = useState("");
  const [pulling, setPulling] = useState<string | null>(null);
  const [progress, setProgress] = useState("");
  const [error, setError] = useState<string | null>(null);

  const check = () =>
    desktop.localModels().then(
      (m) => (setLocal(m), setOllamaError(null)),
      (e) => (setLocal(null), setOllamaError(String(e))),
    );

  useEffect(() => {
    void check();
  }, []);

  async function useLocal(id: string) {
    onModel(id);
    onInfo(await desktop.setProvider("ollama"));
  }

  async function pull(name: string) {
    setPulling(name);
    try {
      await desktop.pullModel(name, setProgress);
      await useLocal(`ollama/${name}`);
    } catch (e) {
      setError(String(e));
      setPulling(null);
    }
  }

  async function saveKey() {
    try {
      await desktop.setApiKey(key);
      onInfo(await desktop.setProvider("openrouter"));
    } catch (e) {
      setError(String(e));
    }
  }

  const usable = local?.filter((m) => m.tools) ?? [];
  return (
    <>
      <h1>How should Marv think?</h1>
      <p>Pick where its model runs. You can switch any time from the model menu.</p>
      <div className="choices">
        <section className="choice">
          <b>On this computer · Ollama</b>
          <span className="help">Free and private; nothing leaves your machine. Needs a good GPU for speed.</span>
          {ollamaError ? (
            <>
              <span className="help warn">Ollama isn't running. Install it, then start it:</span>
              <pre className="code">sudo systemctl enable --now ollama</pre>
              <button className="btn" onClick={() => void check()}>
                Check again
              </button>
            </>
          ) : (
            <>
              {usable.map((m) => (
                <button key={m.id} className="btn primary" onClick={() => void useLocal(m.id)}>
                  Use {m.id.slice(7)}
                </button>
              ))}
              {SUGGESTED.filter((s) => !local?.some((m) => m.id === `ollama/${s}`)).map((s) => (
                <button key={s} className="btn" disabled={pulling !== null} onClick={() => void pull(s)}>
                  Download {s}
                </button>
              ))}
              {pulling && (
                <span className="help mono">
                  {pulling}: {progress}
                </span>
              )}
            </>
          )}
        </section>
        <section className="choice">
          <b>In the cloud · OpenRouter</b>
          <span className="help">Hundreds of models, pay per use. Fast on any computer.</span>
          <input className="mono" type="password" placeholder="sk-or-…" value={key} onChange={(e) => setKey(e.target.value)} />
          <span className="help">
            From{" "}
            <button className="link" onClick={() => void desktop.openUrl("https://openrouter.ai/settings/keys")}>
              openrouter.ai/settings/keys ↗
            </button>
            . Stays on this machine.
          </span>
          <button className="btn primary" disabled={!key.trim()} onClick={() => void saveKey()}>
            Use OpenRouter
          </button>
        </section>
      </div>
      {error && <p className="error">{error}</p>}
    </>
  );
}

function Folder({ onInfo }: { onInfo: (info: AppInfo) => void }) {
  const [error, setError] = useState<string | null>(null);
  async function choose() {
    setError(null);
    const path = await open({ directory: true, title: "Choose your first space's folder" });
    if (typeof path !== "string") return;
    try {
      onInfo(await desktop.setRoot(path));
    } catch (e) {
      setError(String(e));
    }
  }
  return (
    <>
      <h1>Choose a folder</h1>
      <p>
        Your first space: Marv can read and change everything in this folder, and asks before anything outside it. Make more spaces later
        (a project, your work inbox…) with the + at the top.
      </p>
      <p className="faint mono" style={{ fontSize: 12 }}>
        e.g. ~/Projects or a new ~/Marv. Not your home folder itself.
      </p>
      <div>
        <button className="btn primary" onClick={() => void choose()}>
          Choose folder…
        </button>
      </div>
      {error && <p className="error">{error}</p>}
    </>
  );
}

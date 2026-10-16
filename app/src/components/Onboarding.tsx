import { open } from "@tauri-apps/plugin-dialog";
import { useState } from "react";
import { type AppInfo, desktop } from "../lib/backend";

/** First run: the Marv folder (everything Marv works on lives under it) and, if there's none yet, a key. */
export function Onboarding({ info, onDone }: { info: AppInfo; onDone: (info: AppInfo) => void }) {
  const [error, setError] = useState<string | null>(null);
  const [key, setKey] = useState("");

  async function choose() {
    setError(null);
    const path = await open({ directory: true, title: "Choose your Marv folder" });
    if (typeof path !== "string") return;
    try {
      onDone(await desktop.setRoot(path));
    } catch (e) {
      setError(String(e));
    }
  }

  async function saveKey() {
    setError(null);
    try {
      onDone(await desktop.setApiKey(key));
    } catch (e) {
      setError(String(e));
    }
  }

  return (
    <div className="onboarding">
      <div className="card">
        <div className="brand">
          <img src="/marv.svg" alt="" /> marv desktop
        </div>
        {!info.root ? (
          <>
            <h1>Choose your Marv folder</h1>
            <p>
              Your projects live in this folder. The left pane shows what&apos;s inside; start Marv on any folder there, and it can
              read and change everything below it. It asks you before anything outside.
            </p>
            <p className="faint mono" style={{ fontSize: 12 }}>
              e.g. ~/Projects or a new ~/Marv. Not your home folder itself.
            </p>
            <div>
              <button className="btn primary" onClick={() => void choose()}>
                Choose folder…
              </button>
            </div>
          </>
        ) : (
          <>
            <h1>Add an OpenRouter key</h1>
            <p>Marv reaches its models through OpenRouter. The key stays on this machine, readable only by you.</p>
            <label className="field">
              <span>API key</span>
              <input className="mono" type="password" value={key} placeholder="sk-or-…" onChange={(e) => setKey(e.target.value)} />
              <span className="help">From openrouter.ai/settings/keys.</span>
            </label>
            <div>
              <button className="btn primary" disabled={!key.trim()} onClick={() => void saveKey()}>
                Save key
              </button>
            </div>
          </>
        )}
        {error && <p className="error">{error}</p>}
      </div>
    </div>
  );
}

import { useState } from "react";
import { APPS, type App } from "../lib/apps";
import { type Lookup, desktop } from "../lib/backend";

/** Apps to connect: a tile each; Connect asks for what it needs, shows exactly what will be added, then adds it. */
export function Gallery({ installed, onInstalled }: { installed: string[]; onInstalled: () => void }) {
  const [open, setOpen] = useState<string | null>(null);
  return (
    <div className="gallery">
      {APPS.map((app) =>
        open === app.id ? (
          <Connect
            key={app.id}
            app={app}
            onDone={() => {
              setOpen(null);
              onInstalled();
            }}
            onCancel={() => setOpen(null)}
          />
        ) : (
          <div key={app.id} className="tile">
            <span className="mark">{app.mark}</span>
            <div className="tile-text">
              <b>{app.name}</b>
              <span>{app.blurb}</span>
            </div>
            {installed.includes(app.id) ? (
              <span className="connected">connected</span>
            ) : (
              <button className="btn" onClick={() => setOpen(app.id)}>
                Connect
              </button>
            )}
          </div>
        ),
      )}
    </div>
  );
}

function Connect({ app, onDone, onCancel }: { app: App; onDone: () => void; onCancel: () => void }) {
  const [values, setValues] = useState<Record<string, string>>(() =>
    Object.fromEntries(app.fields.map((f) => [f.name, f.value ?? ""])),
  );
  const [review, setReview] = useState<Extract<Lookup, { name: string }> | null>(null);
  const [working, setWorking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ready = app.fields.every((f) => values[f.name]?.trim());

  async function look() {
    setError(null);
    setWorking(true);
    try {
      const found = await desktop.mcpLookup(app.source, app.id, app.headers ? [] : app.fields.map((f) => f.name), app.headers);
      if ("choices" in found) setError("That source lists several servers; add it from Servers → + add server.");
      else setReview(found);
    } catch (e) {
      setError(String(e));
    } finally {
      setWorking(false);
    }
  }

  async function add() {
    setError(null);
    setWorking(true);
    try {
      const filled = Object.fromEntries(Object.entries(values).map(([k, v]) => [k, v.trim()]));
      await desktop.mcpInstall(review!.name, filled);
      onDone();
    } catch (e) {
      setError(String(e));
      setWorking(false);
    }
  }

  return (
    <div className="tile open">
      <div className="tile-head">
        <span className="mark">{app.mark}</span>
        <b>Connect {app.name}</b>
      </div>
      {app.setup && (
        <p className="help">
          {app.setup.text}{" "}
          <button className="link" onClick={() => void desktop.openUrl(app.setup!.url)}>
            Open ↗
          </button>
        </p>
      )}
      {app.fields.map((f) => (
        <label key={f.name} className="field">
          <span>{f.label}</span>
          <input
            className="mono"
            type={f.secret ? "password" : "text"}
            placeholder={f.placeholder}
            value={values[f.name]}
            spellCheck={false}
            disabled={review !== null}
            onChange={(e) => setValues((v) => ({ ...v, [f.name]: e.target.value }))}
          />
        </label>
      ))}
      {review && (
        <div className="lookup">
          <b>{review.title}</b>
          {review.command && <pre className="code">$ {review.command}</pre>}
          <pre className="code">{review.text}</pre>
          {review.warning && <p className="warning">{review.warning}</p>}
          <p className="help">
            Added to ~/.marv/mcp.json (yours only{app.fields.some((f) => f.secret) ? ", with the keys above" : ""}). Every space gets it; switch it off
            in any space.
          </p>
        </div>
      )}
      <div className="add-buttons">
        {review ? (
          <button className="btn primary" disabled={working} onClick={() => void add()}>
            {working ? "Adding…" : `Add ${app.name}`}
          </button>
        ) : (
          <button className="btn primary" disabled={!ready || working} onClick={() => void look()}>
            {working ? "Checking…" : "Continue"}
          </button>
        )}
        <button className="btn plain" onClick={onCancel}>
          Cancel
        </button>
      </div>
      {error && <p className="notice error">{error}</p>}
    </div>
  );
}

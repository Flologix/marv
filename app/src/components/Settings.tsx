import { useState } from "react";
import type { ChatSettings, Model } from "../lib/backend";
import { tokens } from "../lib/format";

export function Settings({
  settings,
  models,
  onChange,
  onClose,
}: {
  settings: ChatSettings;
  models: Model[];
  onChange: (s: ChatSettings) => void;
  onClose: () => void;
}) {
  const [model, setModel] = useState(settings.model);
  const info = models.find((m) => m.id === model);
  const commit = () => model && model !== settings.model && onChange({ ...settings, model });
  return (
    <div className="popover" role="dialog" aria-label="Settings">
      <label className="field">
        <span>Model</span>
        <input
          className="mono"
          list="models"
          value={model}
          spellCheck={false}
          onChange={(e) => setModel(e.target.value.trim())}
          onBlur={commit}
          onKeyDown={(e) => e.key === "Enter" && commit()}
        />
        <datalist id="models">
          {models.map((m) => (
            <option key={m.id} value={m.id}>{`${m.name} · $${m.input.toFixed(2)} / $${m.output.toFixed(2)}`}</option>
          ))}
        </datalist>
        <span className="help">
          {info
            ? `${tokens(info.context)} context · $${info.input.toFixed(2)} in, $${info.output.toFixed(2)} out per million tokens`
            : "Any OpenRouter model that can call tools."}
        </span>
      </label>
      <label className="field">
        <span>Steps before asking to continue</span>
        <input
          type="number"
          min={0}
          max={500}
          style={{ width: 96 }}
          value={settings.maxSteps}
          onChange={(e) => onChange({ ...settings, maxSteps: Math.max(0, Math.min(500, Number(e.target.value) || 0)) })}
        />
        <span className="help">0 never asks.</span>
      </label>
      <div className="switch">
        <button
          type="button"
          role="switch"
          aria-checked={settings.yolo}
          aria-labelledby="yolo"
          className={`toggle${settings.yolo ? " on" : ""}`}
          onClick={() => onChange({ ...settings, yolo: !settings.yolo })}
        />
        <div>
          <div id="yolo">Run sandboxed work without asking</div>
          <div className="help">Off: approve every edit and command, too.</div>
        </div>
      </div>
      <div style={{ display: "flex", justifyContent: "flex-end" }}>
        <button className="btn" onClick={onClose}>
          Done
        </button>
      </div>
    </div>
  );
}

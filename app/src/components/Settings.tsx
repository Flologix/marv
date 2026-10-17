import type { ChatSettings, Model } from "../lib/backend";
import { ModelPicker } from "./ModelPicker";

export function Settings({
  settings,
  models,
  hasKey = true,
  onChange,
  onClose,
}: {
  settings: ChatSettings;
  models: Model[];
  hasKey?: boolean;
  onChange: (s: ChatSettings) => void;
  onClose: () => void;
}) {
  return (
    <div className="popover" role="dialog" aria-label="Settings">
      <div className="field">
        <span>Model</span>
        <ModelPicker
          model={settings.model}
          remote={models.filter((m) => !m.local)}
          hasKey={hasKey}
          onPick={(model) => model !== settings.model && onChange({ ...settings, model })}
        />
      </div>
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

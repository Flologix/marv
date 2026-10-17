import { ask, open } from "@tauri-apps/plugin-dialog";
import { useState } from "react";
import { type AppInfo, desktop } from "../lib/backend";

/** The spaces, as tabs across the top: each its own folder, chats and apps. */
export function SpacesBar({ info, onInfo }: { info: AppInfo; onInfo: (info: AppInfo) => void }) {
  const [renaming, setRenaming] = useState<string | null>(null);
  const [name, setName] = useState("");

  async function create() {
    const path = await open({ directory: true, title: "The new space's folder" });
    if (typeof path !== "string") return;
    try {
      onInfo(await desktop.createSpace(path.split("/").filter(Boolean).pop() ?? "space", path));
    } catch (e) {
      alertError(e);
    }
  }

  async function remove(id: string, spaceName: string) {
    if (!(await ask(`Remove the space "${spaceName}"? Its folder, files and chats stay.`, { title: "Marv", kind: "warning" }))) return;
    try {
      onInfo(await desktop.removeSpace(id));
    } catch (e) {
      alertError(e);
    }
  }

  async function rename(id: string) {
    setRenaming(null);
    if (name.trim()) onInfo(await desktop.renameSpace(id, name));
  }

  return (
    <nav className="spaces" aria-label="Spaces">
      {info.spaces.map((space) =>
        renaming === space.id ? (
          <input
            key={space.id}
            className="space-edit"
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            onBlur={() => void rename(space.id)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void rename(space.id);
              if (e.key === "Escape") setRenaming(null);
            }}
          />
        ) : (
          <div
            key={space.id}
            className={`space${space.id === info.active ? " active" : ""}`}
            title={`${space.root} · double-click to rename`}
            onClick={() => space.id !== info.active && void desktop.switchSpace(space.id).then(onInfo, alertError)}
            onDoubleClick={() => (setRenaming(space.id), setName(space.name))}
          >
            {space.name}
            {info.spaces.length > 1 && (
              <button
                className="space-x"
                title="Remove this space"
                onClick={(e) => {
                  e.stopPropagation();
                  void remove(space.id, space.name);
                }}
              >
                ×
              </button>
            )}
          </div>
        ),
      )}
      <button className="space-add" title="New space: a folder with its own chats and apps" onClick={() => void create()}>
        +
      </button>
    </nav>
  );
}

function alertError(e: unknown) {
  void import("@tauri-apps/plugin-dialog").then(({ message }) => message(String(e), { title: "Marv", kind: "error" }));
}

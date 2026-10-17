//! A process's place on the mesh (moved from marv-cli's `mesh_cmd` so Marv desktop can use it too): its switchboard,
//! named by `marv_mesh::discovery::mesh_name`, with Herdr's agent panes as receptacles when Marv runs in a Herdr
//! pane, and its door ([`Mesh::open_door`]) at `$XDG_RUNTIME_DIR/marv/mesh/<name>.sock`. No `XDG_RUNTIME_DIR`, no
//! mesh (never a `/tmp` fallback).

use std::path::Path;
use std::sync::Arc;

use marv_mesh::discovery::{DoorGuard, DoorInfo, mesh_dir, mesh_name, open_door, session_socket};
use marv_mesh::herdr::{HerdrAdapter, herdr_target};
use marv_mesh::radio::{Presence, Radio, RadioTimings};
use tokio_util::sync::CancellationToken;

use crate::mesh::{DoorWires, MeshHandle, WireApprover};

/// The process's mesh while it runs: its switchboard, served until this drops, and its door once opened.
pub struct Mesh {
    pub handle: MeshHandle,
    pub name: String,
    root: std::path::PathBuf,
    pane: Option<String>,
    dir: std::path::PathBuf,
    door: Option<DoorGuard>,
    /// The radio (#124), the TUI's only ([`Mesh::start_radio`]).
    radio: Option<Arc<Radio>>,
    cancel: CancellationToken,
}

impl Drop for Mesh {
    fn drop(&mut self) {
        if let Some(radio) = self.radio.take() {
            radio.stop();
        }
        self.door.take();
        self.cancel.cancel();
    }
}

/// This process's switchboard (`None` without `XDG_RUNTIME_DIR`). An error names what's wrong (a bad
/// `MARV_MESH_NAME`, a name a live Marv has); the caller carries on without the mesh. Needs a Tokio runtime.
/// `outside`: the project and the sandbox's writable folders, where the `herdr` the pane receptacles run is never taken
/// from (`herdr_program`).
pub fn start(env: &marv_mcp::Env, root: &Path, outside: &[std::path::PathBuf]) -> Result<Option<Mesh>, String> {
    let Some(dir) = mesh_dir(env) else { return Ok(None) };
    let chosen = env.contains_key("MARV_MESH_NAME");
    let mut name = mesh_name(env, root).map_err(|e| e.to_string())?;
    // A generated name that a live Marv already has: try another (the suffix changes with the clock).
    for _ in 0..5 {
        let live = session_socket(&dir, &name).is_some_and(|s| std::os::unix::net::UnixStream::connect(s).is_ok());
        if !live {
            break;
        }
        if chosen {
            return Err(format!("a Marv named \"{name}\" is already on the mesh: set another MARV_MESH_NAME"));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        name = mesh_name(env, root).map_err(|e| e.to_string())?;
    }
    let cancel = CancellationToken::new();
    let handle = crate::mesh::start(&name, env, cancel.clone()).ok_or("no XDG_RUNTIME_DIR")?;
    let pane = herdr_target(env).map(|t| t.pane_id);
    // No herdr found the safe way: no Herdr receptacles, rather than a PATH lookup that could run a planted one.
    if let (Some(own), Some(program)) = (&pane, herdr_program(env, outside)) {
        handle.set_herdr(Arc::new(HerdrAdapter::new(program, Some(own.clone()))));
    }
    Ok(Some(Mesh { handle, name, root: root.to_path_buf(), pane, dir, door: None, radio: None, cancel }))
}

impl Mesh {
    /// Puts this Marv on the mesh: other processes find it (`marv mesh ls`) and ask it for wires, which `wires`
    /// answers (the session's person, or `--approve mesh:<r>`).
    pub fn open_door(&mut self, wires: Arc<dyn WireApprover>) -> Result<(), String> {
        let info = DoorInfo { cwd: self.root.clone(), pane: self.pane.clone() };
        let door = open_door(self.handle.clone(), &self.dir, info, Arc::new(DoorWires(wires))).map_err(|e| e.to_string())?;
        self.door = Some(door);
        Ok(())
    }

    /// The radio (#124) for this Marv's open door, off until the caller turns it on. The TUI's alone: `marv -p` never
    /// makes one, so its door refuses every join.
    pub fn start_radio(&mut self, wake: bool) -> Arc<Radio> {
        let me = Presence::new(&self.name, &self.root, self.pane.clone(), wake);
        let radio = Radio::new(&self.handle, &self.dir, me, RadioTimings::default());
        radio.start();
        self.radio = Some(radio.clone());
        radio
    }
}

/// The `herdr` the mesh runs (pane receptacles, `marv mesh ls`), the doors' way: from an absolute PATH entry only, and
/// never from inside `outside` (`marv_sandbox::host_env::host_program`). None: Marv offers no Herdr panes.
pub fn herdr_program(env: &marv_mcp::Env, outside: &[std::path::PathBuf]) -> Option<std::path::PathBuf> {
    let outside: Vec<&Path> = outside.iter().map(std::path::PathBuf::as_path).collect();
    marv_sandbox::host_env::host_program("herdr", env.get("PATH").map(String::as_str), &outside)
}

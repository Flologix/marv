//! `marv mesh ls|wire` (new in marv-rs; docs/MESH.md, docs/subsystems/mesh.md).
//!
//! - `ls` (mesh v2): the live switchboards in `$XDG_RUNTIME_DIR/marv/mesh/` (owner and mode checked; no `/tmp`
//!   fallback).
//! - `wire 'a -> b:inbox'` (mesh v1): a person grants a wire (rule 1: only the user wires; `Approver::Cli`). The spec is
//!   parsed here; sending the grant to the running Marv's switchboard is still to port.

use marv_mesh::approval::parse_wire_spec;

/// `marv mesh ls`. Returns the exit code.
pub async fn ls() -> i32 {
    let env: marv_mcp::Env = std::env::vars().collect();
    let Some(dir) = marv_mesh::discovery::mesh_dir(&env) else {
        eprintln!("marv mesh ls needs XDG_RUNTIME_DIR: Marv never lists switchboards in a world-readable folder.");
        return 1;
    };
    let _ = dir;
    todo!("port: mesh v2: marv_mesh::discovery::list(&dir), one line per session (printable)")
}

/// `marv mesh wire <spec>`. Returns the exit code.
pub async fn wire(spec: &str) -> i32 {
    let (from, to, mode) = match parse_wire_spec(spec) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("{}", marv_core::printable(&e.to_string()));
            return 1;
        }
    };
    let _ = (from, to, mode);
    todo!("port: mesh v1: grant the wire (Grant::local(Approver::Cli)) on the running Marv's switchboard socket")
}

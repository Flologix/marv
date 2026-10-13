//! Adding and removing servers in your own `~/.marv/mcp.json` (`/mcp add`,
//! `/mcp remove`). Ports `src/mcp/personal-file.ts`.
//!
//! - The file is yours and may hold more than Marv wrote: every other key and
//!   server is kept as it was.
//! - A file that can't be read is **never overwritten** (it's yours to fix).
//! - Never the project's `.mcp.json`: that one is shared with the repository.
//! - One change at a time, across Marv processes too: the whole
//!   read-change-write holds `mcp.json.lock`, so two terminals adding servers at
//!   once both land, and a remove can't undo an add.
//! - The entry is written exactly as approved (the approval is the security
//!   boundary, see `install::proposal`), 0600: it may hold header references.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::PERSONAL_FILE;

pub fn personal_file(config_dir: &Path) -> PathBuf {
    config_dir.join(PERSONAL_FILE)
}

/// The file as it is right now, with its key order; missing is empty. Anything
/// Marv can't safely change is an error that says so.
///
/// Key order: serde_json sorts maps unless `preserve_order` is on, so a write
/// would reorder the user's file. Port note: write through
/// `config::OrderedEntries`-style ordered values before enabling writes.
pub fn read(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("Couldn't read {}: {e}", path.display())),
    };
    let shown = path.display();
    let data: Value = serde_json::from_str(&text)
        .map_err(|_| format!("Couldn't read {shown} (it isn't valid JSON): fix it by hand first, Marv won't overwrite it."))?;
    match data {
        Value::Object(map) if map.get("mcpServers").is_none_or(Value::is_object) => Ok(map),
        _ => Err(format!(
            "Couldn't read {shown} (it should be {{\"mcpServers\": {{…}}}}): fix it by hand first, Marv won't overwrite it."
        )),
    }
}

/// Adds `entry` as server `name`, exactly as given. Refuses a taken name, an
/// unreadable file and an entry `config::check_entry` rejects.
pub async fn add_personal_server(_config_dir: &Path, _name: &str, _entry: Value) -> crate::Result<()> {
    todo!("port: addPersonalServer (withFileLock; check_entry; read; refuse taken name; writePrivate with trailing newline)")
}

/// Removes server `name`, keeping everything else.
pub async fn remove_personal_server(_config_dir: &Path, _name: &str) -> crate::Result<()> {
    todo!("port: removePersonalServer (withFileLock; read; refuse missing; writePrivate)")
}

/// The names in your `mcp.json`, or none when it can't be read (for `/mcp remove`'s check).
pub fn personal_server_names(config_dir: &Path) -> Vec<String> {
    read(&personal_file(config_dir))
        .ok()
        .and_then(|mut m| m.remove("mcpServers"))
        .and_then(|v| if let Value::Object(s) = v { Some(s.keys().cloned().collect()) } else { None })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_marv_cant_read_is_an_error_not_empty() {
        let dir = std::env::temp_dir().join(format!("marv-personal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = personal_file(&dir);
        let _ = std::fs::remove_file(&path);
        assert!(read(&path).unwrap().is_empty());
        for bad in ["nope", "[]", "{\"mcpServers\": []}"] {
            std::fs::write(&path, bad).unwrap();
            assert!(read(&path).unwrap_err().contains("Marv won't overwrite it"), "{bad}");
            assert!(personal_server_names(&dir).is_empty());
        }
        std::fs::write(&path, r#"{"other": 1, "mcpServers": {"a": {}, "b": {}}}"#).unwrap();
        assert_eq!(personal_server_names(&dir), vec!["a", "b"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}

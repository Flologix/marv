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

use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

use crate::config::{PERSONAL_FILE, check_entry};

pub fn personal_file(config_dir: &Path) -> PathBuf {
    config_dir.join(PERSONAL_FILE)
}

/// JSON with every object's key order kept (marv-rs): serde_json's `Value`
/// sorts keys (its `preserve_order` feature would change hashing elsewhere, the
/// trust keys among them), and a write must not reorder the user's file.
#[derive(Debug, Clone, PartialEq)]
pub enum Ordered {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Ordered>),
    Object(Vec<(String, Ordered)>),
}

impl Ordered {
    fn from_value(v: Value) -> Ordered {
        match v {
            Value::Null => Ordered::Null,
            Value::Bool(b) => Ordered::Bool(b),
            Value::Number(n) => Ordered::Number(n),
            Value::String(s) => Ordered::String(s),
            Value::Array(a) => Ordered::Array(a.into_iter().map(Ordered::from_value).collect()),
            Value::Object(o) => Ordered::Object(o.into_iter().map(|(k, v)| (k, Ordered::from_value(v))).collect()),
        }
    }
}

impl Serialize for Ordered {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Ordered::Null => s.serialize_unit(),
            Ordered::Bool(b) => s.serialize_bool(*b),
            Ordered::Number(n) => n.serialize(s),
            Ordered::String(t) => s.serialize_str(t),
            Ordered::Array(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Ordered::Object(fields) => {
                let mut map = s.serialize_map(Some(fields.len()))?;
                for (k, v) in fields {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<Ordered, E> {
                Ok(Ordered::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Ordered, E> {
                Ok(Ordered::Number(n.into()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Ordered, E> {
                Ok(Ordered::Number(n.into()))
            }
            fn visit_f64<E>(self, n: f64) -> Result<Ordered, E> {
                Ok(Number::from_f64(n).map_or(Ordered::Null, Ordered::Number))
            }
            fn visit_str<E>(self, t: &str) -> Result<Ordered, E> {
                Ok(Ordered::String(t.to_string()))
            }
            fn visit_string<E>(self, t: String) -> Result<Ordered, E> {
                Ok(Ordered::String(t))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Ordered, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Ordered::Array(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut fields: Vec<(String, Ordered)> = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Ordered>()? {
                    // A repeated key: the last one counts, as in JSON.parse.
                    match fields.iter_mut().find(|(key, _)| *key == k) {
                        Some(slot) => slot.1 = v,
                        None => fields.push((k, v)),
                    }
                }
                Ok(Ordered::Object(fields))
            }
        }
        d.deserialize_any(V)
    }
}

/// The file's top-level object, in its key order.
pub type McpFile = Vec<(String, Ordered)>;

fn servers_of(file: &McpFile) -> Option<&Vec<(String, Ordered)>> {
    file.iter().find(|(k, _)| k == "mcpServers").and_then(|(_, v)| if let Ordered::Object(s) = v { Some(s) } else { None })
}

/// The file as it is right now, with its key order; missing is empty. Anything
/// Marv can't safely change is an error that says so.
pub fn read(path: &Path) -> Result<McpFile, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Couldn't read {}: {e}", path.display())),
    };
    let shown = path.display();
    let data: Ordered = serde_json::from_str(&text)
        .map_err(|_| format!("Couldn't read {shown} (it isn't valid JSON): fix it by hand first, Marv won't overwrite it."))?;
    match data {
        Ordered::Object(fields) if fields.iter().all(|(k, v)| k != "mcpServers" || matches!(v, Ordered::Object(_))) => Ok(fields),
        _ => Err(format!(
            "Couldn't read {shown} (it should be {{\"mcpServers\": {{…}}}}): fix it by hand first, Marv won't overwrite it."
        )),
    }
}

async fn write(path: &Path, file: McpFile) -> crate::Result<()> {
    let text = serde_json::to_string_pretty(&Ordered::Object(file))?;
    marv_fs::write_private(path, &format!("{text}\n")).await?;
    Ok(())
}

/// One change at a time, across Marv processes too: the whole
/// read-change-write holds `mcp.json.lock` (`marv_fs::with_file_lock`), so two
/// terminals adding servers at once both land, and a remove can't undo an add.
async fn locked<F, Fut>(config_dir: &Path, work: F) -> crate::Result<()>
where
    F: FnOnce(PathBuf) -> Fut + Send,
    Fut: std::future::Future<Output = crate::Result<()>> + Send,
{
    let path = personal_file(config_dir);
    let held = path.clone();
    marv_fs::with_file_lock(&held, marv_fs::LockOptions::default(), || work(path))
        .await
        .map_err(crate::oauth_store::lock_error)?
}

/// Adds `entry` as server `name`, exactly as given (it's what the user
/// approved). Refuses a taken name, an unreadable file and an entry
/// `config::check_entry` rejects. Written 0600: it may hold header references.
pub async fn add_personal_server(config_dir: &Path, name: &str, entry: Value) -> crate::Result<()> {
    locked(config_dir, |path| async move {
        check_entry(name, &entry).map_err(crate::McpError::Config)?;
        let mut file = read(&path).map_err(crate::McpError::Config)?;
        if servers_of(&file).is_some_and(|s| s.iter().any(|(k, _)| k == name)) {
            return Err(crate::McpError::Config(format!(
                "You already have a server named \"{name}\" in {}: pick another with --name, or /mcp remove {name} first.",
                path.display()
            )));
        }
        let added = (name.to_string(), Ordered::from_value(entry));
        match file.iter_mut().find(|(k, _)| k == "mcpServers") {
            Some((_, Ordered::Object(servers))) => servers.push(added),
            _ => file.push(("mcpServers".into(), Ordered::Object(vec![added]))),
        }
        write(&path, file).await
    })
    .await
}

/// Removes server `name`, keeping everything else.
pub async fn remove_personal_server(config_dir: &Path, name: &str) -> crate::Result<()> {
    locked(config_dir, |path| async move {
        let mut file = read(&path).map_err(crate::McpError::Config)?;
        let servers = file.iter_mut().find_map(|(k, v)| match v {
            Ordered::Object(s) if k == "mcpServers" => Some(s),
            _ => None,
        });
        let Some(servers) = servers.filter(|s| s.iter().any(|(k, _)| k == name)) else {
            return Err(crate::McpError::Config(format!("There's no server \"{name}\" in {}.", path.display())));
        };
        servers.retain(|(k, _)| k != name);
        write(&path, file).await
    })
    .await
}

/// The names in your `mcp.json`, in file order, or none when it can't be read (for `/mcp remove`'s check).
pub fn personal_server_names(config_dir: &Path) -> Vec<String> {
    read(&personal_file(config_dir))
        .ok()
        .and_then(|file| servers_of(&file).map(|s| s.iter().map(|(k, _)| k.clone()).collect()))
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
        std::fs::write(&path, r#"{"other": 1, "mcpServers": {"b": {}, "a": {}}}"#).unwrap();
        assert_eq!(personal_server_names(&dir), vec!["b", "a"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    use crate::config::{Include, McpTransport, load_mcp_config};
    use crate::testutil::temp_dir;
    use serde_json::json;

    fn entry() -> Value {
        json!({"command": "npx", "args": ["-y", "weather@1.2.0"], "env": {"API_KEY": "${API_KEY}"}})
    }
    fn file(d: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(personal_file(d)).unwrap()).unwrap()
    }
    fn err(r: crate::Result<()>) -> String {
        r.unwrap_err().to_string()
    }

    #[tokio::test]
    async fn adds_beside_whats_there_0600_in_order_and_loads_back() {
        use std::os::unix::fs::PermissionsExt;
        let d = temp_dir("personal");
        std::fs::write(
            personal_file(&d),
            r#"{"zother": true, "mcpServers": {"docs": {"url": "https://x/mcp", "type": "http"}}}"#,
        )
        .unwrap();
        add_personal_server(&d, "weather", entry()).await.unwrap();
        let text = std::fs::read_to_string(personal_file(&d)).unwrap();
        // The user's key order is kept (zother first, url before type), and the file ends with a newline.
        assert!(text.starts_with("{\n  \"zother\": true,\n  \"mcpServers\": {\n    \"docs\": {\n      \"url\""), "{text}");
        assert!(text.ends_with("}\n"));
        let written = file(&d);
        assert_eq!(written["mcpServers"]["weather"], entry());
        assert_eq!(std::fs::metadata(personal_file(&d)).unwrap().permissions().mode() & 0o777, 0o600);
        let env: crate::Env = [("API_KEY".to_string(), "k".to_string())].into();
        let loaded = load_mcp_config(&d, &d, &env, Include { personal: true, project: false });
        assert_eq!(loaded.problems, Vec::<String>::new());
        assert_eq!(loaded.servers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["docs", "weather"]);
        assert_eq!(
            loaded.servers[1].transport,
            McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "weather@1.2.0".into()],
                env: [("API_KEY".to_string(), "k".to_string())].into()
            }
        );
    }

    #[tokio::test]
    async fn a_missing_file_is_created_and_an_unset_var_doesnt_stop_the_write() {
        let d = temp_dir("personal");
        add_personal_server(&d, "weather", entry()).await.unwrap();
        assert_eq!(file(&d), json!({"mcpServers": {"weather": entry()}}));
        add_personal_server(&d, "w2", json!({"command": "npx", "args": ["${NOT_SET_ANYWHERE_123}"]})).await.unwrap();
        assert_eq!(file(&d)["mcpServers"]["w2"]["args"], json!(["${NOT_SET_ANYWHERE_123}"]));
    }

    #[tokio::test]
    async fn refuses_a_taken_name_a_broken_file_and_an_entry_marv_couldnt_load() {
        let d = temp_dir("personal");
        add_personal_server(&d, "weather", entry()).await.unwrap();
        assert!(err(add_personal_server(&d, "weather", entry()).await).contains("--name"));
        std::fs::write(personal_file(&d), "{ broken").unwrap();
        assert!(err(add_personal_server(&d, "x", entry()).await).contains("Couldn't read"));
        assert_eq!(std::fs::read_to_string(personal_file(&d)).unwrap(), "{ broken"); // never overwritten
        std::fs::write(personal_file(&d), r#"{"mcpServers": []}"#).unwrap();
        assert!(err(add_personal_server(&d, "x", entry()).await).contains("Couldn't read"));
        assert!(
            err(add_personal_server(&temp_dir("personal"), "x", json!({"type": "sse", "url": "https://x"})).await)
                .contains("sse")
        );
        assert!(err(add_personal_server(&temp_dir("personal"), "a__b", entry()).await).contains("__"));
    }

    #[tokio::test]
    async fn two_adds_at_once_both_land_and_remove_keeps_the_rest() {
        let d = temp_dir("personal");
        let (a, b) = tokio::join!(add_personal_server(&d, "a", entry()), add_personal_server(&d, "b", entry()));
        a.unwrap();
        b.unwrap();
        assert_eq!(personal_server_names(&d), ["a", "b"]);
        remove_personal_server(&d, "a").await.unwrap();
        assert_eq!(personal_server_names(&d), ["b"]);
        assert!(err(remove_personal_server(&d, "nope").await).contains("no server \"nope\""));
    }
}

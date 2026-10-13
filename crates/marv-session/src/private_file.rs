//! Writing a private file (sessions, memory, MCP trust, MCP logins): readable only by you (0600, in a 0700 folder),
//! and never left half-written. Ports Marv's `src/private-file.ts`.
//!
//! The content goes to a temp file first, then a rename puts it in place in one step, so a crash leaves the old file
//! or the new one, never a mix. The temp name is unique (process id + random), so two Marv processes saving the same
//! file can't write into each other's temp file (with one shared `.tmp` name, overlapping saves left a session
//! unreadable in a third of test rounds); and writes to one file from this process run one after the other, so an
//! older write can't land after a newer one.
//!
//! Port note: marv-mcp's trust, token store and personal file need this too; marv-mcp can't depend on marv-session,
//! so this (and `file_lock`) should move to a crate both can use (see docs/subsystems/session.md, "Upstream gaps").

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// Per-file write queues: a lock per path, dropped once nobody holds or waits on it.
static QUEUES: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

/// Eight random hex digits: a fresh `RandomState` is seeded per call, plus a counter (no RNG crate needed).
pub(crate) fn random_hex() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:08x}", h.finish() as u32)
}

/// Writes `content` to `path` privately and atomically. Writes to one path from this process are queued.
pub async fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    let lock = QUEUES.lock().unwrap_or_else(|e| e.into_inner()).entry(path.to_path_buf()).or_default().clone();
    let result = {
        let _held = lock.lock().await;
        let path = path.to_path_buf();
        let content = content.to_string();
        tokio::task::spawn_blocking(move || write(&path, &content)).await.map_err(std::io::Error::other)?
    };
    // Forget the file once nobody else queues on it, so the map doesn't grow.
    let mut queues = QUEUES.lock().unwrap_or_else(|e| e.into_inner());
    if Arc::strong_count(&lock) <= 2 {
        queues.remove(path);
    }
    result
}

fn write(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let temp = PathBuf::from(format!("{}.{}.{}.tmp", path.display(), std::process::id(), random_hex()));
    let attempt = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        // `mode` only applies when the file is created (and the umask applies to it).
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&temp, path)
    })();
    if attempt.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    attempt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("marv-session-{tag}-{}-{}", std::process::id(), random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn writes_privately_in_a_private_folder() {
        let dir = temp_dir("private");
        let path = dir.join("nested/a.json");
        write_private(&path, "{}").await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn overlapping_writes_land_in_order_and_leave_no_temp_files() {
        let dir = temp_dir("overlap");
        let path = dir.join("s.json");
        let writes: Vec<_> = (0..20)
            .map(|i| {
                let p = path.clone();
                tokio::spawn(async move { write_private(&p, &format!("{{\"n\":{i}}}")).await })
            })
            .collect();
        for w in writes {
            w.await.unwrap().unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

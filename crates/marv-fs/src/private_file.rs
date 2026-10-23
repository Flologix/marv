//! Writing a private file (sessions, memory, MCP trust, MCP logins): readable only by you (0600, in a 0700 folder),
//! and never left half-written. Ports Marv's `src/private-file.ts`.
//!
//! The content goes to a temp file first, then a rename puts it in place in one step, so a crash leaves the old file
//! or the new one, never a mix. The temp name is unique (process id + random), so two Marv processes saving the same
//! file can't write into each other's temp file (with one shared `.tmp` name, overlapping saves left a session
//! unreadable in a third of test rounds); and writes to one file from this process run one after the other, so an
//! older write can't land after a newer one.
//!
//! Lives in marv-fs (not marv-session, where it started) because marv-mcp's trust, token store and personal file need
//! it too, and marv-mcp can't depend on marv-session.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// Per-file write queues: a lock per path, dropped once nobody holds or waits on it.
static QUEUES: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);

/// A random `u64`: a fresh `RandomState` is seeded per call, plus a counter (no RNG crate needed). Unique names and
/// jitter, never secrets.
pub(crate) fn random_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.finish()
}

/// `bytes` random bytes as hex (Marv's `randomBytes(bytes).toString("hex")`).
pub(crate) fn random_hex_bytes(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes * 2);
    while out.len() < bytes * 2 {
        out.push_str(&format!("{:016x}", random_u64()));
    }
    out.truncate(bytes * 2);
    out
}

/// Eight random hex digits (four bytes, as Marv's temp names): unique temp file and folder names.
pub fn random_hex() -> String {
    random_hex_bytes(4)
}

/// Writes `content` to `path` privately and atomically. Writes to one path from this process are queued.
pub async fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    write_queued(path, content, None).await
}

/// [`write_private`], creating folders only below `anchor`, which must exist (it is never created): a write that
/// lands after its folder was removed (a session's last save, after a test removed its temp folder) fails instead of
/// making the folders again. Each folder below it is made one level at a time, never recursively.
pub async fn write_private_under(anchor: &Path, path: &Path, content: &str) -> std::io::Result<()> {
    write_queued(path, content, Some(anchor)).await
}

async fn write_queued(path: &Path, content: &str, anchor: Option<&Path>) -> std::io::Result<()> {
    let lock = QUEUES.lock().unwrap_or_else(|e| e.into_inner()).entry(path.to_path_buf()).or_default().clone();
    let result = {
        let _held = lock.lock().await;
        let path = path.to_path_buf();
        let content = content.to_string();
        let anchor = anchor.map(Path::to_path_buf);
        tokio::task::spawn_blocking(move || write(&path, &content, anchor.as_deref())).await.map_err(std::io::Error::other)?
    };
    // Forget the file once nobody else queues on it, so the map doesn't grow.
    let mut queues = QUEUES.lock().unwrap_or_else(|e| e.into_inner());
    if Arc::strong_count(&lock) <= 2 {
        queues.remove(path);
    }
    result
}

fn write(path: &Path, content: &str, anchor: Option<&Path>) -> std::io::Result<()> {
    use std::io::Write;
    match (path.parent(), anchor) {
        (Some(dir), None) => std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?,
        (Some(dir), Some(anchor)) => {
            let below = dir.strip_prefix(anchor).map_err(|_| std::io::Error::other("the file isn't under its anchor"))?;
            if !anchor.is_dir() {
                return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "the anchor folder is gone"));
            }
            let mut at = anchor.to_path_buf();
            for part in below.components() {
                at.push(part);
                match std::fs::DirBuilder::new().mode(0o700).create(&at) {
                    Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
                    _ => {}
                }
            }
        }
        (None, _) => {}
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

    // Ported from tests/private-file.test.ts.

    /// Removed when the guard drops, so a failing test doesn't leave its folder behind (#89).
    fn temp_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new().prefix(&format!("marv-private-{tag}-")).tempdir().unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect()
    }

    // A write landing after its folders were removed (marv-rs #212's CI: a session's last save after the test's temp
    // folder was gone) fails, and makes nothing; with the anchor there, the folders below it are made.
    #[tokio::test]
    async fn a_write_under_a_missing_anchor_makes_no_folders() {
        let tmp = temp_dir("anchor");
        let gone = tmp.path().join("gone");
        let file = gone.join("store").join("project").join("s.json");
        assert!(write_private_under(&gone.join("store"), &file, "x").await.is_err());
        assert!(!gone.exists());
        std::fs::create_dir(&gone).unwrap();
        write_private_under(&gone, &file, "x").await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
    }

    #[test]
    fn random_hex_has_the_asked_length() {
        assert_eq!(random_hex().len(), 8);
        assert_eq!(random_hex_bytes(9).len(), 18);
        assert!(random_hex_bytes(9).chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(random_hex(), random_hex());
    }

    #[tokio::test]
    async fn writes_are_private_and_overlapping_writes_never_corrupt_it_the_last_one_wins() {
        let tmp = temp_dir("overlap");
        let dir = tmp.path();
        let file = dir.join("sub/session.json");
        // Big enough that the writes overlap. `join_all` polls them in order, so they queue in order.
        let contents: Vec<String> =
            (0..20).map(|i| serde_json::json!({"n": i, "pad": "x".repeat(200_000 * ((i % 3) + 1))}).to_string()).collect();
        let writes = contents.iter().map(|c| write_private(&file, c));
        for result in futures::future::join_all(writes).await {
            result.unwrap();
        }
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["n"], 19);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dir.join("sub")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(names(&dir.join("sub")), ["session.json"]); // no temp files left
        assert!(QUEUES.lock().unwrap().get(&file).is_none()); // the queue is forgotten
    }

    /// The other Marv process of the next test: the test binary run again, for this test only.
    const WRITER: &str = "MARV_FS_TEST_WRITER";

    #[tokio::test]
    #[ignore = "a child process of two_marv_processes_writing_the_same_file_never_corrupt_it"]
    async fn private_file_writer_child() {
        let Ok(spec) = std::env::var(WRITER) else { return };
        let (id, file) = spec.split_once(' ').unwrap();
        for i in 0..40 {
            let content = serde_json::json!({"id": id, "i": i, "pad": "y".repeat(300_000)}).to_string();
            write_private(Path::new(file), &content).await.unwrap();
        }
    }

    #[test]
    fn two_marv_processes_writing_the_same_file_never_corrupt_it() {
        let tmp = temp_dir("procs");
        let dir = tmp.path();
        let file = dir.join("s.json");
        let writer = |id: u32| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["private_file::tests::private_file_writer_child", "--exact", "--ignored", "--quiet"])
                .env(WRITER, format!("{id} {}", file.display()))
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let procs = [writer(1), writer(2)];
        for mut p in procs {
            assert!(p.wait().unwrap().success());
        }
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["i"], 39);
        assert_eq!(names(dir), ["s.json"]);
    }
}

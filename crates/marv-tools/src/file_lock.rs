//! Per-file serialisation of read-change-write.
//!
//! Ports Marv's `src/tools/file-lock.ts` (the in-process queue; the cross-process `src/file-lock.ts` with O_EXCL lock
//! files belongs to marv-session's private files).
//!
//! `write_file` and `edit_file` read a file, change it, and write the whole file back. Two calls on one file at the
//! same time (parallel tool calls, subagents sharing the project folder) would both read the same original, and the
//! later write would drop the earlier one's change: two appends would keep only one. Each file's read-change-write runs
//! after the previous one on the same path has finished. Entries are forgotten once nothing is queued behind them.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use marv_sandbox::paths::normalize;

fn locks() -> &'static Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(Default::default)
}

/// Runs `f` after every earlier `with_file_lock` on the same path (relative to `root`) has finished.
pub async fn with_file_lock<T, F: Future<Output = T>>(root: &Path, path: &str, f: impl FnOnce() -> F) -> T {
    let key = normalize(&root.join(path));
    let lock = locks().lock().unwrap_or_else(|e| e.into_inner()).entry(key.clone()).or_default().clone();
    let result = {
        let _held = lock.lock().await;
        f().await
    };
    // Forget the path once nothing else holds or waits for it (the map's copy and ours).
    let mut map = locks().lock().unwrap_or_else(|e| e.into_inner());
    if Arc::strong_count(&lock) == 2 {
        map.remove(&key);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn calls_on_one_path_run_one_after_another() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let root = Path::new("/tmp/marv-lock-test");
        let a = {
            let order = order.clone();
            with_file_lock(root, "a.txt", move || async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                order.lock().unwrap().push("first");
            })
        };
        let b = {
            let order = order.clone();
            with_file_lock(root, "./a.txt", move || async move { order.lock().unwrap().push("second") })
        };
        tokio::join!(a, b);
        assert_eq!(*order.lock().unwrap(), ["first", "second"]);
        assert!(locks().lock().unwrap().is_empty());
    }
}

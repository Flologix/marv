//! `docs/SECURITY.md`, "File changes": `edit_file` and `write_file` never write over a change the session
//! hasn't seen. Each session remembers a content hash of every file it read or wrote
//! (`marv_tools::file_versions`); an existing file that changed since, or that it never read, is refused with "read
//! it again", and nothing is written. The check and the write are one step under a short cross-process lock
//! (`marv_tools::file_lock`, an advisory `flock` in the runtime folder), held for that call only.
//!
//! The other writer is a real second process where it can be: the real `marv -p` binary against the oracle's fake
//! OpenRouter, next to this test's own tool context (a second session). Where a lock must be held at a chosen moment,
//! this test holds it on the lock file Marv uses: `flock` belongs to an open file, so a second open file conflicts
//! exactly as another process does.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use marv_guards::{TempDir, call, headless_steps};
use marv_oracle::script::ScriptCall;
use marv_tools::file_lock::{LOCK_FOLDER, lock_path};
use marv_tools::{ToolContext, builtin_tools, run_tool};
use serde_json::{Value, json};

fn script(id: &str, name: &str, arguments: Value) -> ScriptCall {
    ScriptCall { id: Some(id.into()), name: name.into(), arguments }
}

/// A session in `root` that says yes to every change (the person approved it: only the stale check can stop it).
fn session(root: &Path) -> ToolContext {
    ToolContext { yolo: true, ..ToolContext::new(root) }
}

async fn run(ctx: &ToolContext, name: &str, args: Value) -> marv_tools::ToolOutput {
    run_tool(&call(name, args), ctx, builtin_tools()).await.result
}

/// **An edit after another session's write is refused until the file is read again.** This session reads `a.txt`;
/// another Marv process (`marv -p`) reads it and writes it; this session's edit, built on the old text, is refused
/// and writes nothing, so the other's change stays. After reading the file again, the same edit goes through.
///
/// Proven: removed the `file_versions.check` from `edit_file::plan` → failed (the edit dropped the other process's
/// line).
#[tokio::test]
async fn an_edit_after_another_sessions_write_is_refused_until_reread() {
    let (root, config) = (TempDir::new("stale-edit"), TempDir::new("stale-edit-config"));
    root.write("a.txt", "one\ntwo\n");
    let ctx = session(root.path());
    assert!(!run(&ctx, "read_file", json!({"path": "a.txt"})).await.is_error);

    let other = vec![
        vec![script("r1", "read_file", json!({"path": "a.txt"}))],
        vec![script("w1", "write_file", json!({"path": "a.txt", "content": "one\ntwo\ntheirs\n"}))],
    ];
    let out = headless_steps(root.path(), config.path(), other, &["--approve", "files"], &[("MARV_GITHUB", "off")]);
    assert_eq!(out.code, Some(0), "{}\n{}", out.result, out.stderr);
    assert_eq!(root.read("a.txt").as_deref(), Some("one\ntwo\ntheirs\n"), "the other process wrote");

    let edit = json!({"path": "a.txt", "old_string": "two", "new_string": "TWO"});
    let refused = run(&ctx, "edit_file", edit.clone()).await;
    assert!(refused.is_error, "{}", refused.output);
    assert!(refused.output.contains("changed since you last read it") && refused.output.contains("read_file"));
    assert_eq!(root.read("a.txt").as_deref(), Some("one\ntwo\ntheirs\n"), "nothing written over theirs");

    assert!(!run(&ctx, "read_file", json!({"path": "a.txt"})).await.is_error);
    let edited = run(&ctx, "edit_file", edit).await;
    assert!(!edited.is_error, "{}", edited.output);
    assert_eq!(root.read("a.txt").as_deref(), Some("one\nTWO\ntheirs\n"));
}

/// **An existing file the session never read can't be overwritten** (nor edited), in `marv -p` as in any session;
/// a new file needs no read, and once written the session may write it again.
///
/// Proven: let `FileVersions::check` pass a file it has no hash for → failed (`marv -p` overwrote `keep.txt`).
#[test]
fn an_existing_file_never_read_cant_be_overwritten() {
    let (root, config) = (TempDir::new("stale-unread"), TempDir::new("stale-unread-config"));
    root.write("keep.txt", "the user's text\n");
    let steps = vec![
        vec![script("w1", "write_file", json!({"path": "keep.txt", "content": "clobbered\n"}))],
        vec![script("e1", "edit_file", json!({"path": "keep.txt", "old_string": "user", "new_string": "model"}))],
        vec![script("w2", "write_file", json!({"path": "new.txt", "content": "first\n"}))],
        vec![script("w3", "write_file", json!({"path": "new.txt", "content": "second\n"}))],
    ];
    let out = headless_steps(root.path(), config.path(), steps, &["--approve", "files"], &[("MARV_GITHUB", "off")]);
    assert_eq!(out.code, Some(0), "{}\n{}", out.result, out.stderr);
    assert_eq!(root.read("keep.txt").as_deref(), Some("the user's text\n"), "the unread file is untouched");
    assert_eq!(root.read("new.txt").as_deref(), Some("second\n"), "a new file, then its own write again");
}

/// **The check and the write are one step across processes.** While another process holds the lock on `new.txt`
/// (this test, on the lock file in the runtime folder `marv -p` uses), `marv -p`'s `write_file` of the new file
/// waits; the other process creates the file meanwhile and lets go. Marv then checks the file as it is now, finds an
/// existing file it never read, and writes nothing: it didn't pass a check made before the other's write.
///
/// Proven: dropped the cross-process lock from `with_file_lock` (`file_locks` ignored) → failed (`marv -p` created
/// `new.txt` while the lock was held).
#[test]
fn the_check_and_write_are_atomic_across_processes() {
    let (root, config, runtime) =
        (TempDir::new("stale-atomic"), TempDir::new("stale-atomic-config"), TempDir::short("stale-atomic-rt"));
    let (runtime_var, runtime_dir) = marv_guards::runtime_var(runtime.path());
    // The child's runtime folder is `<runtime>/marv` on every platform (`runtime_var`).
    // Made private, as Marv makes it (it refuses a runtime folder others could change).
    let locks = runtime.path().join("marv").join(LOCK_FOLDER);
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700).create(&locks).unwrap();
    let theirs = File::create(lock_path(&locks, &root.path().join("new.txt"))).unwrap();
    theirs.lock().unwrap();

    let marv = {
        let (root, config) = (root.path().to_path_buf(), config.path().to_path_buf());
        let runtime_dir = runtime_dir.to_string_lossy().to_string();
        std::thread::spawn(move || {
            let steps = vec![vec![script("w1", "write_file", json!({"path": "new.txt", "content": "marv's\n"}))]];
            let env = [("MARV_GITHUB", "off"), (runtime_var, runtime_dir.as_str())];
            headless_steps(&root, &config, steps, &["--approve", "files"], &env)
        })
    };
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(root.read("new.txt"), None, "marv -p wrote while another process held the lock");
    std::fs::write(root.path().join("new.txt"), "theirs\n").unwrap();
    theirs.unlock().unwrap();

    let out = marv.join().unwrap();
    assert_eq!(out.code, Some(0), "{}\n{}", out.result, out.stderr);
    assert_eq!(root.read("new.txt").as_deref(), Some("theirs\n"), "the other process's file stays");
}

/// **No lock outlives one call.** Right after an edit (written), a refused edit (stale) and a refused write (never
/// read), another process can take the file's lock at once: none is held across a step, let alone a turn.
///
/// Proven: leaked the lock's file handle in `with_file_lock` (`std::mem::forget`) → failed (the lock was still held
/// after the call).
#[tokio::test]
async fn no_lock_outlives_one_call() {
    let (root, locks) = (TempDir::new("stale-nolock"), TempDir::new("stale-nolock-rt"));
    root.write("a.txt", "one\n");
    root.write("b.txt", "bee\n");
    let ctx = ToolContext { file_locks: Some(locks.path().to_path_buf()), ..session(root.path()) };
    // Free within a moment: a command another test starts meanwhile shares the lock's open file between its fork and
    // its exec (which closes it). A lock a call kept stays held.
    let free = |file: &str| {
        let lock = File::open(lock_path(locks.path(), &root.path().join(file))).expect("the call made its lock file");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if lock.try_lock().is_ok() {
                return true;
            }
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    assert!(!run(&ctx, "read_file", json!({"path": "a.txt"})).await.is_error);
    let edited = run(&ctx, "edit_file", json!({"path": "a.txt", "old_string": "one", "new_string": "ONE"})).await;
    assert!(!edited.is_error, "{}", edited.output);
    assert!(free("a.txt"), "held after a written edit");

    std::fs::write(root.path().join("a.txt"), "changed\n").unwrap();
    let stale = run(&ctx, "edit_file", json!({"path": "a.txt", "old_string": "changed", "new_string": "x"})).await;
    assert!(stale.is_error, "{}", stale.output);
    assert!(free("a.txt"), "held after a refused edit");

    // Refused in the preview, before anyone was asked: no lock was even taken. Held at apply time, it's let go.
    let unread = run(&ctx, "write_file", json!({"path": "b.txt", "content": "x\n"})).await;
    assert!(unread.is_error, "{}", unread.output);
    let created = run(&ctx, "write_file", json!({"path": "c.txt", "content": "new\n"})).await;
    assert!(!created.is_error, "{}", created.output);
    assert!(free("c.txt"), "held after a created file");
}

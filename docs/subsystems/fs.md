# marv-fs

The file helpers that marv-mcp and marv-session both need, in a crate below both (marv-mcp can't depend on
marv-session). Depends on nothing in the workspace.

Spec: the **Private files** bullet and the `withFileLock` part of the **OAuth logins** bullet in Marv's `CLAUDE.md`,
`tests/private-file.test.ts`, `tests/file-lock.test.ts` and `tests/fixtures/file-lock-contender.ts`.

## Modules

| Marv (TS) | Rust module | Status |
|---|---|---|
| `src/private-file.ts` | `private_file.rs` | **Ported, with tests** (moved from marv-session). `write_private`: 0600 in a 0700 folder, a unique temp file (pid + random) renamed into place, writes of one path queued in-process. `random_hex` for unique names. |
| `src/file-lock.ts` | `file_lock.rs` | **Ported, with tests.** `with_file_lock(path, LockOptions, work)`: queued per path in-process; across processes `<file>.lock` (O_EXCL, a `<pid> <random>` token, mtime refreshed every `stale / 3`), stale takeover and release only under the `<lock>.takeover` guard; `take_over_stale`, `release_lock`. |

## Port notes

- **Cross-process tests re-run the test binary** as the other process (`--exact <child test> --ignored`, with the
  arguments in an env var), where Marv spawned `bun`. The "slow holder" test contends through `hold` directly (no
  in-process queue), which is what Marv's second process was for.
- **A dropped hold releases its lock** (marv-rs): if the future running `with_file_lock` is dropped mid-work (a
  cancelled turn), a guard stops the heartbeat and releases the lock from a spawned task. Marv's `finally` covered a
  rejected promise; a dropped Rust future runs no `finally`, so without this the lock would sit until it went stale.
- **`LockOptions::stale(d)`** gives Marv's default `waitMs` (`stale + 5 s`); `Default` is `STALE_LOCK` (10 s).
- Randomness comes from a seeded `RandomState` plus a counter (no RNG crate): unique names and jitter, never secrets.

## Interfaces other crates use

```rust
marv_fs::write_private(&Path, &str).await -> io::Result<()>
marv_fs::with_file_lock(&Path, LockOptions::default(), || async { … }).await -> Result<T, LockError> // Busy | Io
marv_fs::random_hex() -> String  // 8 hex digits: temp names
```

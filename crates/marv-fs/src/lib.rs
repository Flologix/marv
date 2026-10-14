//! marv-fs: the file helpers marv-mcp and marv-session both need, in a crate below both.
//!
//! - [`private_file`]: `write_private`, 0600 in a 0700 folder, atomic (Marv's `src/private-file.ts`);
//! - [`file_lock`]: `with_file_lock`, one read-change-write of a shared file at a time across Marv processes
//!   (Marv's `src/file-lock.ts`).

pub mod file_lock;
pub mod private_file;

pub use file_lock::{LockError, LockOptions, STALE_LOCK, with_file_lock};
pub use private_file::{random_hex, write_private};

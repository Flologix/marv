//! Project checks (`src/checks/`, issue #41): a project's rules as code, `.marv/checks/<name>.yaml|yml|json|ts`,
//! run in the sandbox after the model's edits through the diagnostics machinery ([`crate::diagnostics::turn`]).
//!
//! - [`load`]: reading the rule files (`load.ts`);
//! - [`kind`]: the check kind, the runner's output, the note for the model (`kind.ts`);
//! - [`runner`]: what runs inside the sandbox (`runner.ts`).

pub mod kind;
pub mod load;
pub mod runner;

pub use kind::{ProjectCheckResult, checks_note, project_check_notice};
pub use load::{CHECKS_DIR, ProjectCheck, Violation, load_checks};

/// A scratch project folder for tests, removed on drop.
#[cfg(test)]
pub(crate) struct Scratch(pub std::path::PathBuf);

#[cfg(test)]
impl Scratch {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("marv-checks-{}-{}", std::process::id(), marv_fs::random_hex()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    /// Writes `content` at `path` (project-relative), creating its folders.
    pub fn put(&self, path: &str, content: &str) {
        let full = self.0.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
}

#[cfg(test)]
impl Drop for Scratch {
    fn drop(&mut self) {
        // A test that ends while a background blocking task (the check cache's sizing, say) is still writing into the
        // folder gets the folder back after the removal: inside a runtime, look again a moment later until it stays gone.
        let in_runtime = tokio::runtime::Handle::try_current().is_ok();
        for _ in 0..20 {
            let _ = std::fs::remove_dir_all(&self.0);
            if !in_runtime {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
            if !self.0.exists() {
                return;
            }
        }
    }
}

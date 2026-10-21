//! marv-tools: the [`Tool`] trait, the registry and [`run_tool`] (approval, yolo, repair), and Marv's built-in tools.
//!
//! Ports Marv's `src/tools/` (one module per TypeScript file; `index.ts` is [`registry`]). The sandbox and the path
//! rules live in marv-sandbox; tools call them through [`files`].
//!
//! Public API other crates use:
//! - [`Tool`] (implement it), [`DynTool`] (what the registry holds), [`ToolRef`] = `Arc<dyn DynTool>`;
//! - [`ToolContext`] (built per session/subagent), [`Approver`], [`ApprovalRequest`], [`Decision`], [`AgentHost`],
//!   [`BeforeChange`];
//! - [`run_tool`] → [`ToolOutcome`] (never fails), [`builtin_tools`], [`tool_specs_for`], [`is_parallel_call`].
//!
//! Planned across the crate: `apply_patch` (#66, [`apply_patch`]). The formatter run after edits (#67) is in
//! [`formatter`].

pub mod agent;
pub mod apply_patch;
pub mod bash;
pub mod diff;
pub mod edit_file;
pub mod edit_match;
pub mod file_lock;
pub mod files;
pub mod formatter;
pub mod github;
pub mod glob;
pub mod grep;
pub mod guard;
pub mod memory;
pub mod output;
pub mod plan;
pub mod process;
pub mod read_file;
pub mod registry;
pub mod relay;
pub mod repair;
pub mod schema_repair;
pub mod skill;
pub mod syntax;
pub mod table;
pub mod types;
pub mod web;
pub mod web_fetch;
pub mod write_file;

pub use formatter::{FormatOutcome, Formatter, FormatterSetting, ProjectFormatter};
pub use marv_core::MAX_DECLINE_REASON;
pub use registry::{
    DECLINED, INTERRUPTED, REMOVED_TOOLS, Tombstone, ToolOutcome, ToolRef, builtin_tools, declined_because, is_parallel_call,
    run_tool, schema_repaired, tool_specs, tool_specs_for, unknown_tool,
};
pub use schema_repair::RepairMode;
pub use types::{
    AgentHost, AgentProgress, Approval, ApprovalRequest, Approver, BeforeChange, Checkpoint, Decision, DiffKind, DiffLine,
    DiffShown, DynTool, MemoryPaths, NetworkFailures, PlanHost, PlanItem, PlanStatus, Preview, PreviewFile, ProcessForward,
    ProcessForwards, ProcessHost, ProcessInfo, ProcessState, Repaired, Scope, Skill, StartResult, Tool, ToolContext, ToolError,
    ToolOutput,
};

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh temp folder (canonical path), removed on drop.
    pub struct TempDir(PathBuf);
    impl TempDir {
        pub fn new(tag: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("marv-tools-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(std::fs::canonicalize(dir).unwrap())
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
        pub fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The project Marv's `tests/tools.test.ts` starts from.
    pub fn project(tag: &str) -> TempDir {
        let root = TempDir::new(tag);
        root.write("package.json", "{ \"name\": \"demo\" }\n");
        root.write("src/app.ts", "export function main() {\n  return greet('world');\n}\n");
        root.write("src/greet.ts", "export function greet(name: string) {\n  return `Hello, ${name}!`;\n}\n");
        root.write("src/ui/view.tsx", "// TODO: render the greeting\nexport const View = () => null;\n");
        root.write("node_modules/dep/index.js", "function greet() {}\n");
        root.write("logo.png", [0x89, 0x50, 0x4e, 0x47, 0x00, 0x00, 0x01]);
        root
    }

    /// Plain git, without Marv's protections (the tests' own setup, and their negative controls).
    pub fn git(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git").args(args).current_dir(cwd).status().unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Runs the test `name` (its path in this crate, e.g. `bash::tests::x`) again in a child process with `env`
    /// added, asserts it passed there, and returns false; in that child it returns true, and the test's body runs.
    /// A test needs a variable in Marv's own environment (a provider key that must not leak), and Rust can't change
    /// its own environment without `unsafe`.
    pub fn in_child_with_env(name: &str, env: &[(&str, &str)]) -> bool {
        if std::env::var("MARV_TEST_CHILD").as_deref() == Ok(name) {
            return true;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--nocapture", "--test-threads=1"])
            .env("MARV_TEST_CHILD", name)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && stdout.contains("1 passed"), "{name} in a child:\n{stdout}\n{stderr}");
        false
    }

    pub fn mkfifo(path: &Path) {
        assert!(std::process::Command::new("mkfifo").arg(path).status().unwrap().success());
    }
}

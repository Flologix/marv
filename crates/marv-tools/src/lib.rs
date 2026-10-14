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
//! Planned across the crate: spill cut output to a file the model can page through (#61, [`output`]),
//! schema-guided argument repair (#65, [`repair`]), a formatter run after edits (#67, [`edit_file`]/[`write_file`]),
//! `apply_patch` (#66, [`apply_patch`]).

pub mod agent;
pub mod apply_patch;
pub mod bash;
pub mod diff;
pub mod edit_file;
pub mod edit_match;
pub mod file_lock;
pub mod files;
pub mod glob;
pub mod grep;
pub mod memory;
pub mod output;
pub mod process;
pub mod read_file;
pub mod registry;
pub mod relay;
pub mod repair;
pub mod skill;
pub mod syntax;
pub mod types;
pub mod web;
pub mod web_fetch;
pub mod write_file;

pub use registry::{
    DECLINED, INTERRUPTED, ToolOutcome, ToolRef, builtin_tools, is_parallel_call, run_tool, tool_specs, tool_specs_for,
};
pub use types::{
    AgentHost, AgentProgress, Approval, ApprovalRequest, Approver, BeforeChange, Decision, DiffKind, DiffLine, DiffShown,
    DynTool, MemoryPaths, Preview, ProcessForward, ProcessForwards, ProcessHost, ProcessInfo, ProcessState, Repaired, Scope,
    Skill, StartResult, Tool, ToolContext, ToolError, ToolOutput,
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

    pub fn mkfifo(path: &Path) {
        assert!(std::process::Command::new("mkfifo").arg(path).status().unwrap().success());
    }
}

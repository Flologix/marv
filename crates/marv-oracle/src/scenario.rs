//! A scenario on disk: `run.json`, `script.jsonl`, `project/` and the recorded `expect/`.
//!
//! The layout is the one `tests/oracle/README.md` plans. `run.json` replaces Marv's eval `task.md`
//! (`evals/run.ts`): besides the prompt it carries the `marv -p` flags and config, because a
//! scripted model needs no hidden check; `expect/` is the check.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::script::{Reply, parse_script};

/// `run.json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    /// What the scenario checks, for whoever reads a failure.
    #[serde(default)]
    pub description: String,
    /// The `-p` prompt.
    pub prompt: String,
    /// `--max-steps`.
    pub max_steps: Option<u32>,
    /// `--approve` names, one flag each.
    #[serde(default)]
    pub approve: Vec<String>,
    /// Fields laid over the default `config.json` (see [`crate::runner::default_config`]).
    #[serde(default)]
    pub config: Map<String, Value>,
    /// Make `project/` a git repository with one commit first, as Marv's evals do (glob and grep
    /// list files with `git ls-files`). Default true.
    #[serde(default = "yes")]
    pub git: bool,
    /// Kill the binary after this long (default [`DEFAULT_TIMEOUT_SECS`]).
    pub timeout_secs: Option<u64>,
    /// What `GET /models` lists, in OpenRouter's format (`{"id", "context_length",
    /// "supported_parameters": ["tools"]}`). Empty by default: Marv then knows no context window,
    /// and never compacts.
    #[serde(default)]
    pub models: Vec<Value>,
    /// An MCP server for the run, as Marv's eval tasks' `mcp.json` names it.
    pub mcp: Option<McpFixture>,
    /// 1-based requests allowed to break the prefix rule (see [`crate::prefix`]): the first request
    /// after a compaction, which starts the cache over on purpose.
    #[serde(default)]
    pub prefix_resets: Vec<usize>,
}

/// `{"server": "tracker", "data": "tracker.json"}`: the tracker ([`crate::tracker`]) serving a data
/// file from `tests/oracle/mcp/`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpFixture {
    pub server: String,
    pub data: String,
}

fn yes() -> bool {
    true
}

/// Long enough for a retry's backoff (1 s, 4 s) and Bun's startup, short enough to notice a hang.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    pub dir: PathBuf,
    pub run: RunSpec,
    pub replies: Vec<Reply>,
}

impl Scenario {
    pub fn load(dir: &Path) -> anyhow::Result<Scenario> {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let read = |file: &str| std::fs::read_to_string(dir.join(file)).map_err(|err| anyhow::anyhow!("{name}/{file}: {err}"));
        let run: RunSpec = serde_json::from_str(&read("run.json")?).map_err(|err| anyhow::anyhow!("{name}/run.json: {err}"))?;
        let replies = parse_script(&read("script.jsonl")?).map_err(|err| anyhow::anyhow!("{name}/{err}"))?;
        Ok(Scenario { name, dir: dir.to_path_buf(), run, replies })
    }

    pub fn project(&self) -> PathBuf {
        self.dir.join("project")
    }

    pub fn expect(&self) -> PathBuf {
        self.dir.join("expect")
    }
}

/// Every scenario under `root` (each subdirectory with a `run.json`), by name.
pub fn load_all(root: &Path) -> anyhow::Result<Vec<Scenario>> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|err| anyhow::anyhow!("{}: {err}", root.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.join("run.json").is_file())
        .collect();
    dirs.sort();
    dirs.iter().map(|dir| Scenario::load(dir)).collect()
}

/// `tests/oracle/scenarios` in this workspace.
pub fn default_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/oracle/scenarios")
}

/// `tests/oracle/mcp` in this workspace: the MCP fixtures' data.
pub fn mcp_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/oracle/mcp")
}

/// Files stored under another name in `project/` and `expect/files/`: a `.gitignore` there would
/// also apply to this repository, and hide the very files the scenario needs.
const RENAMED: [(&str, &str); 1] = [(".gitignore", "gitignore.oracle")];

/// A path as a project has it, from the name it's stored under.
pub fn real_path(stored: &str) -> String {
    map_name(stored, |name| RENAMED.iter().find(|(_, s)| *s == name).map(|(real, _)| *real))
}

/// A path as it's stored, from the name a project has.
pub fn stored_path(real: &str) -> String {
    map_name(real, |name| RENAMED.iter().find(|(r, _)| *r == name).map(|(_, stored)| *stored))
}

fn map_name(path: &str, map: impl Fn(&str) -> Option<&'static str>) -> String {
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(d, n)| (d, n));
    let name = map(name).unwrap_or(name);
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitignore_is_stored_under_another_name() {
        assert_eq!(stored_path(".gitignore"), "gitignore.oracle");
        assert_eq!(stored_path("sub/.gitignore"), "sub/gitignore.oracle");
        assert_eq!(real_path("sub/gitignore.oracle"), "sub/.gitignore");
        assert_eq!(real_path("src/a.ts"), "src/a.ts");
    }
}

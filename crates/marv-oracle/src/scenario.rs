//! A scenario on disk: `run.json`, `script.jsonl`, `project/` and the recorded `expect/`.
//!
//! The layout is the one `tests/oracle/README.md` plans. `run.json` replaces Marv's eval `task.md`
//! (`evals/run.ts`): besides the prompt it carries the `marv -p` flags and config, because a
//! scripted model needs no hidden check; `expect/` is the check.
//!
//! A deliberate marv-rs difference (a security fix, a Sprint 5 item) is declared in `run.json`
//! (`deliberate`: the issue and why). Only then does checking marv-rs ([`Target::MarvRs`]) read
//! `expect.marv-rs/` instead of `expect/`, and serve `script.marv-rs.jsonl` when the difference
//! changes what the model is asked (#64 asks it to continue, so its scripted answer differs too).
//! TypeScript Marv is always held to `expect/`.

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
    /// marv-rs differs from Marv here on purpose: checked against `expect.marv-rs/`.
    pub deliberate: Option<Deliberate>,
}

/// `{"issue": 64, "why": "…"}`: the Marv issue the difference implements, and what it changes. A marv-rs security
/// rule that no Marv issue asked for cites its `docs/SECURITY.md` section instead:
/// `{"security": "Provenance and taint", "why": "…"}`. Exactly one of the two.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deliberate {
    #[serde(default)]
    pub issue: Option<u32>,
    /// The `docs/SECURITY.md` section (its heading) the difference follows.
    #[serde(default)]
    pub security: Option<String>,
    pub why: String,
}

impl Deliberate {
    /// Names exactly one reason (a Marv issue, or a SECURITY.md section) and says why.
    pub fn well_formed(&self) -> bool {
        let reason = match (&self.issue, &self.security) {
            (Some(n), None) => *n > 0,
            (None, Some(section)) => !section.trim().is_empty(),
            _ => false,
        };
        reason && !self.why.trim().is_empty()
    }
}

/// Which Marv a run checks or records: TypeScript Marv (the recordings in `expect/`), or marv-rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Marv,
    MarvRs,
}

impl Target {
    /// `marv` or `marv-rs`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "marv" => Some(Self::Marv),
            "marv-rs" => Some(Self::MarvRs),
            _ => None,
        }
    }

    /// `MARV_ORACLE_TARGET`; without it, checking means marv-rs and recording means TypeScript Marv.
    pub fn from_env(record: bool) -> anyhow::Result<Self> {
        match std::env::var("MARV_ORACLE_TARGET") {
            Ok(name) => {
                Self::parse(&name).ok_or_else(|| anyhow::anyhow!("MARV_ORACLE_TARGET must be marv or marv-rs, not {name:?}"))
            }
            Err(_) => Ok(if record { Self::Marv } else { Self::MarvRs }),
        }
    }
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
    /// `script.marv-rs.jsonl`, for a deliberate difference that changes what the model is asked.
    pub marv_rs_replies: Option<Vec<Reply>>,
}

impl Scenario {
    pub fn load(dir: &Path) -> anyhow::Result<Scenario> {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        let read = |file: &str| std::fs::read_to_string(dir.join(file)).map_err(|err| anyhow::anyhow!("{name}/{file}: {err}"));
        let run: RunSpec = serde_json::from_str(&read("run.json")?).map_err(|err| anyhow::anyhow!("{name}/run.json: {err}"))?;
        let replies = parse_script(&read("script.jsonl")?).map_err(|err| anyhow::anyhow!("{name}/{err}"))?;
        let marv_rs_replies = match dir.join(MARV_RS_SCRIPT).is_file() {
            true => Some(parse_script(&read(MARV_RS_SCRIPT)?).map_err(|err| anyhow::anyhow!("{name}/{err}"))?),
            false => None,
        };
        let has_override = dir.join(MARV_RS_EXPECT).exists() || marv_rs_replies.is_some();
        match (&run.deliberate, has_override) {
            (Some(_), false) => {
                anyhow::bail!("{name}: run.json declares a deliberate difference, but there's no {MARV_RS_EXPECT}/")
            }
            (None, true) => anyhow::bail!("{name}: {MARV_RS_EXPECT}/ or {MARV_RS_SCRIPT} without a `deliberate` in run.json"),
            _ => {}
        }
        Ok(Scenario { name, dir: dir.to_path_buf(), run, replies, marv_rs_replies })
    }

    /// The scenario as `target` runs it: marv-rs gets its own script for a deliberate difference.
    pub fn for_target(&self, target: Target) -> Scenario {
        let mut scenario = self.clone();
        if let (Target::MarvRs, Some(replies)) = (target, &self.marv_rs_replies) {
            scenario.replies = replies.clone();
        }
        scenario
    }

    /// Where `target`'s expected outcome is: `expect.marv-rs/` for marv-rs where a difference is declared.
    pub fn expect_for(&self, target: Target) -> PathBuf {
        match (target, &self.run.deliberate) {
            (Target::MarvRs, Some(_)) => self.dir.join(MARV_RS_EXPECT),
            _ => self.expect(),
        }
    }

    pub fn project(&self) -> PathBuf {
        self.dir.join("project")
    }

    pub fn expect(&self) -> PathBuf {
        self.dir.join("expect")
    }
}

/// marv-rs's expected outcome where it differs on purpose, and its script when the model is asked something else.
pub const MARV_RS_EXPECT: &str = "expect.marv-rs";
pub const MARV_RS_SCRIPT: &str = "script.marv-rs.jsonl";

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
    fn a_deliberate_difference_names_one_reason() {
        let d = |v: serde_json::Value| serde_json::from_value::<Deliberate>(v).unwrap().well_formed();
        assert!(d(serde_json::json!({"issue": 64, "why": "w"})));
        assert!(d(serde_json::json!({"security": "Provenance and taint", "why": "w"})));
        assert!(!d(serde_json::json!({"issue": 64, "security": "Provenance and taint", "why": "w"})));
        assert!(!d(serde_json::json!({"why": "w"})));
        assert!(!d(serde_json::json!({"issue": 0, "why": "w"})));
        assert!(!d(serde_json::json!({"security": " ", "why": "w"})));
        assert!(!d(serde_json::json!({"issue": 64, "why": " "})));
    }

    #[test]
    fn gitignore_is_stored_under_another_name() {
        assert_eq!(stored_path(".gitignore"), "gitignore.oracle");
        assert_eq!(stored_path("sub/.gitignore"), "sub/gitignore.oracle");
        assert_eq!(real_path("sub/gitignore.oracle"), "sub/.gitignore");
        assert_eq!(real_path("src/a.ts"), "src/a.ts");
    }
}

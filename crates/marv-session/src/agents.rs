//! Agent types: the kinds of subagent Marv can start, as Markdown files. Ports Marv's `src/agents.ts`.
//!
//! ```text
//! .marv/agents/<name>.md      the project's (win a clash)
//! ~/.marv/agents/<name>.md    your personal ones
//! ```
//!
//! YAML frontmatter (`name`, `description`, optionally `tools` and `model`), then the agent's own instructions. The
//! format is Claude Code's, so files written for it (superpowers' `code-reviewer.md`) work unchanged: its tool names
//! are mapped, its model shorthands mean the session's model, and `findAgent` tries a `plugin:`-prefixed name bare.
//! Only names and descriptions go into the main system prompt.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::frontmatter::parse_frontmatter;
use crate::skills::{cap_body, valid_name};

/// What subagents may use. Never `agent` (no subagents of subagents: no runaway recursion) or `memory` (memory
/// outlives the session, so only the main agent changes it, with the user's approval). Every other built-in tool is
/// in `marv_tools::MAIN_AGENT_ONLY`, which `run_tool` refuses in a subagent's call (`marv_tools::floor`).
pub const SUBAGENT_TOOLS: [&str; 9] =
    ["read_file", "glob", "grep", "web_fetch", "github", "skill", "edit_file", "write_file", "bash"];

/// Claude Code's model shorthands: not ids on Marv's providers, so they mean the session's model, like `inherit`.
const CLAUDE_CODE_MODELS: [&str; 4] = ["inherit", "sonnet", "opus", "haiku"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    BuiltIn,
    Project,
    Personal,
}

impl AgentSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Project => "project",
            Self::Personal => "personal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentType {
    pub name: String,
    pub description: String,
    /// Its own instructions; empty for general-purpose.
    pub body: String,
    /// The tools it may use (Marv's names).
    pub tools: Vec<String>,
    /// A model on the same provider; `None` means the session's model.
    pub model: Option<String>,
    pub source: AgentSource,
}

/// Claude Code's tool names, so agent files written for it work here.
fn alias(name: &str) -> &str {
    match name {
        "Read" => "read_file",
        "Glob" | "LS" => "glob",
        "Grep" => "grep",
        "WebFetch" => "web_fetch",
        "Edit" | "MultiEdit" => "edit_file",
        "Write" => "write_file",
        "Bash" => "bash",
        "Skill" => "skill",
        other => other,
    }
}

pub fn general_purpose() -> AgentType {
    AgentType {
        name: "general-purpose".into(),
        description:
            "Any self-contained task: research across many files, implementing one planned task, running and fixing tests.".into(),
        body: String::new(),
        tools: SUBAGENT_TOOLS.iter().map(|t| t.to_string()).collect(),
        model: None,
        source: AgentSource::BuiltIn,
    }
}

/// The built-in `explore` type's tools: reading and searching only. No edits, no bash, so nothing it does changes the
/// project, whatever the parent's mode (yolo included): the tools it isn't given aren't offered to it, and a call to
/// one is an unknown tool. web_fetch taints it as it does any agent (`TaintState::for_subagent`).
pub const EXPLORE_TOOLS: [&str; 4] = ["read_file", "grep", "glob", "web_fetch"];

/// How the system prompt lists `explore`. The oracle drops its line (`marv_oracle::runner::MARV_RS_TEXT`).
pub const EXPLORE_DESCRIPTION: &str = "Read-only search: finding where something is defined or used, how a part of the code works, or answering a question across many files. Can't change files or run commands. Prefer it to general-purpose for any search.";

const EXPLORE_BODY: &str = "You are a read-only exploration agent. Find what the task asks for (where something is defined or used, how a part of the code works, which files matter) and report it with file paths and line numbers, quoting only the lines that matter. You can't change files or run commands: if the task needs a change, say what and where instead.";

/// marv-rs (#186, not in Marv): a read-only agent for searches. Defaults to the session's model; a project or personal
/// `.marv/agents/explore.md` replaces it whole (with its own `model:` for a cheaper one, say), as for general-purpose.
pub fn explore() -> AgentType {
    AgentType {
        name: "explore".into(),
        description: EXPLORE_DESCRIPTION.into(),
        body: EXPLORE_BODY.into(),
        tools: EXPLORE_TOOLS.iter().map(|t| t.to_string()).collect(),
        model: None,
        source: AgentSource::BuiltIn,
    }
}

/// The built-in types, sorted by name: what every session starts with before its agent files.
pub fn builtin_agents() -> Vec<AgentType> {
    vec![explore(), general_purpose()]
}

pub fn agents_dir(base: &Path) -> PathBuf {
    base.join(".marv").join("agents")
}

/// `tools:` as a list or a comma-separated string. Anything outside [`SUBAGENT_TOOLS`] is a problem, not silently
/// dropped.
pub fn parse_tools(value: Option<&Value>, shown: &str) -> Result<Vec<String>, String> {
    let list: Vec<String> = match value {
        None | Some(Value::Null) => {
            return Ok(SUBAGENT_TOOLS.iter().map(|t| t.to_string()).collect());
        }
        Some(Value::Array(items)) => {
            items.iter().map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).collect()
        }
        Some(Value::String(s)) => s.split(',').map(String::from).collect(),
        Some(_) => return Err(format!("{shown}: tools should be a list of tool names.")),
    };
    let mut tools = Vec::new();
    for raw in list.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
        let name = alias(raw);
        if !SUBAGENT_TOOLS.contains(&name) {
            return Err(format!("{shown}: subagents can't use \"{raw}\". They can use: {}.", SUBAGENT_TOOLS.join(", ")));
        }
        if !tools.iter().any(|t| t == name) {
            tools.push(name.to_string());
        }
    }
    Ok(tools)
}

/// Parses an agent file's text.
pub fn parse_agent(text: &str, file: &str, source: AgentSource, shown: &str) -> Result<AgentType, String> {
    let parsed = parse_frontmatter(text, shown)?;
    let field = |k: &str| parsed.fields.get(k).and_then(Value::as_str).map(|s| s.trim().to_string());
    let name = field("name").unwrap_or_else(|| file.trim_end_matches(".md").to_string());
    let description = field("description").unwrap_or_default();
    if !valid_name(&name) {
        return Err(format!("{shown}: the name \"{name}\" should be lowercase letters, digits and dashes."));
    }
    if description.is_empty() {
        return Err(format!("{shown} needs a description: it's how the model knows when to use the agent."));
    }
    let tools = parse_tools(parsed.fields.get("tools"), shown)?;
    let model = field("model").filter(|m| !m.is_empty() && !CLAUDE_CODE_MODELS.contains(&m.as_str()));
    Ok(AgentType { name, description, body: cap_body(parsed.body, file), tools, model, source })
}

/// Built-in, then personal (under `home`), then project agents (under `root`; later wins). Sorted by name.
pub fn load_agents(root: Option<&Path>, home: Option<&Path>) -> (Vec<AgentType>, Vec<String>) {
    let mut by_name: std::collections::BTreeMap<String, AgentType> =
        builtin_agents().into_iter().map(|a| (a.name.clone(), a)).collect();
    let mut problems = Vec::new();
    for (base, source, prefix) in [(home, AgentSource::Personal, "~/"), (root, AgentSource::Project, "")] {
        let Some(base) = base else { continue };
        let dir = agents_dir(base);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<String> =
            entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|f| f.ends_with(".md")).collect();
        files.sort();
        for file in files {
            let shown = format!("{prefix}.marv/agents/{file}");
            // marv-rs: never read through a symlink leading out of the agents folder.
            let result = marv_sandbox::resolve_within(&dir, &file)
                .map_err(|_| format!("{shown}: leads outside .marv/agents, so it wasn't loaded."))
                .and_then(|path| std::fs::read_to_string(path).map_err(|e| format!("{shown}: couldn't be read ({e}).")))
                .and_then(|text| parse_agent(&text, &file, source, &shown));
            match result {
                Ok(agent) => {
                    by_name.insert(agent.name.clone(), agent);
                }
                Err(problem) => problems.push(problem),
            }
        }
    }
    (by_name.into_values().collect(), problems)
}

/// Finds a type by name; `superpowers:code-reviewer` is also tried without its prefix.
pub fn find_agent<'a>(agents: &'a [AgentType], name: &str) -> Option<&'a AgentType> {
    if let Some(exact) = agents.iter().find(|a| a.name == name) {
        return Some(exact);
    }
    let (_, bare) = name.rsplit_once(':')?;
    agents.iter().find(|a| a.name == bare)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every built-in tool is either a subagent's or the main agent's alone, never both.
    #[test]
    fn every_builtin_tool_is_a_subagents_or_the_main_agents_alone() {
        for tool in marv_tools::builtin_tools() {
            let name = tool.name();
            assert!(
                SUBAGENT_TOOLS.contains(&name) != marv_tools::MAIN_AGENT_ONLY.contains(&name),
                "{name}: in both lists or neither"
            );
        }
    }

    #[test]
    fn claude_code_files_work_unchanged() {
        let a = parse_agent(
            "---\nname: code-reviewer\ndescription: Reviews.\ntools: Read, Grep, MultiEdit, LS\nmodel: sonnet\n---\nYou review.",
            "code-reviewer.md",
            AgentSource::Project,
            "x",
        )
        .unwrap();
        assert_eq!(a.tools, vec!["read_file", "grep", "edit_file", "glob"]);
        assert_eq!(a.model, None);
        assert_eq!(a.body, "You review.");
        let b = parse_agent("---\nname: fast\ndescription: d\nmodel: qwen3.5:9b\n---\n", "fast.md", AgentSource::Personal, "y")
            .unwrap();
        assert_eq!(b.model.as_deref(), Some("qwen3.5:9b"));
    }

    #[test]
    fn tools_outside_the_subagent_set_are_a_problem() {
        assert!(parse_tools(Some(&json!(["agent"])), "f").unwrap_err().contains("subagents can't use \"agent\""));
        assert!(parse_tools(Some(&json!("memory")), "f").is_err());
        assert!(parse_tools(Some(&json!(3)), "f").unwrap_err().contains("should be a list"));
        assert_eq!(parse_tools(None, "f").unwrap().len(), 9);
    }

    #[test]
    fn find_agent_tries_a_plugin_prefix_bare() {
        let agents = vec![general_purpose(), AgentType { name: "code-reviewer".into(), ..general_purpose() }];
        assert!(find_agent(&agents, "superpowers:code-reviewer").is_some());
        assert!(find_agent(&agents, "general-purpose").is_some());
        assert!(find_agent(&agents, "nope").is_none());
    }

    #[test]
    fn explore_is_built_in_with_only_the_read_tools() {
        let (agents, problems) = load_agents(None, None);
        assert!(problems.is_empty());
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["explore", "general-purpose"]);
        let explore = find_agent(&agents, "explore").unwrap();
        assert_eq!(explore.tools, ["read_file", "grep", "glob", "web_fetch"]);
        assert_eq!(explore.source, AgentSource::BuiltIn);
        assert_eq!(explore.model, None, "the session's model unless an agent file says otherwise");
        assert!(!explore.body.is_empty());
        for tool in &explore.tools {
            assert!(SUBAGENT_TOOLS.contains(&tool.as_str()));
            assert!(!["edit_file", "write_file", "bash"].contains(&tool.as_str()));
        }
    }

    #[test]
    fn an_explore_file_replaces_the_built_in_project_over_personal() {
        let dir = std::env::temp_dir().join(format!("marv-explore-{}-{}", std::process::id(), line!()));
        let (root, home) = (dir.join("root"), dir.join("home"));
        for base in [&root, &home] {
            std::fs::create_dir_all(agents_dir(base)).unwrap();
        }
        std::fs::write(
            agents_dir(&home).join("explore.md"),
            "---\nname: explore\ndescription: Mine.\ntools: Read, Grep\nmodel: small-model\n---\nSearch.",
        )
        .unwrap();
        let (agents, _) = load_agents(None, Some(&home));
        let explore = find_agent(&agents, "explore").unwrap();
        assert_eq!(
            (explore.source, explore.description.as_str(), explore.model.as_deref()),
            (AgentSource::Personal, "Mine.", Some("small-model"))
        );
        assert_eq!(explore.tools, ["read_file", "grep"]);
        std::fs::write(agents_dir(&root).join("explore.md"), "---\ndescription: The project's.\n---\n").unwrap();
        let (agents, _) = load_agents(Some(&root), Some(&home));
        let explore = find_agent(&agents, "explore").unwrap();
        assert_eq!((explore.source, explore.description.as_str()), (AgentSource::Project, "The project's."));
        assert_eq!(agents.iter().filter(|a| a.name == "explore").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! What a session can read from disk, by where it lives (the SDK's `sources`). Ports Marv's `src/sources.ts`.
//!
//! - `User`: your `~/.marv`: personal skills and agents, memory (personal and this project's: both your own files,
//!   never in the repository), and your MCP servers (`mcp.json`).
//! - `Project`: the repository: `AGENTS.md`, `.marv/skills`, `.marv/agents`, and `.mcp.json` (whose servers still
//!   start only once trusted, with `/mcp trust` in the CLI).
//!
//! The CLI loads both. A program using the SDK loads neither unless it asks: a library mustn't quietly read your
//! files, or start a repository's servers.

use std::path::Path;

use marv_mcp::config::{Include, McpServerConfig, load_mcp_config};

use crate::agents::{AgentType, load_agents};
use crate::config::Env;
use crate::memory::{Memories, MemoryPaths, load_memory, memory_paths};
use crate::prompt::{Instructions, read_instructions};
use crate::skills::{LoadedSkill, load_skills};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    User,
    Project,
}

#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub instructions: Option<Instructions>,
    pub skills: Vec<LoadedSkill>,
    /// Always has the built-in general-purpose type.
    pub agents: Vec<AgentType>,
    pub memory: Option<(MemoryPaths, Memories)>,
    pub mcp_servers: Vec<McpServerConfig>,
    /// Files that couldn't be read, and why.
    pub problems: Vec<String>,
}

/// Loads only the `sources` asked for. `config_dir` holds memory and `mcp.json`; personal skills and agents live in
/// `<home>/.marv` (even with another config dir, as the CLI does).
pub async fn load_sources(root: &Path, sources: &[Source], config_dir: &Path, home: &Path, env: &Env) -> Loaded {
    let user = sources.contains(&Source::User);
    let project = sources.contains(&Source::Project);
    let (root_base, home_base) = (project.then_some(root), user.then_some(home));
    let (skills, mut problems) = load_skills(root_base, home_base);
    let (agents, agent_problems) = load_agents(root_base, home_base);
    problems.extend(agent_problems);
    let mcp_servers = if user || project {
        let loaded = load_mcp_config(root, config_dir, env, Include { personal: user, project });
        problems.extend(loaded.problems);
        loaded.servers
    } else {
        Vec::new()
    };
    let memory = if user {
        let paths = memory_paths(config_dir, root);
        let initial = load_memory(&paths).await.unwrap_or_default();
        Some((paths, initial))
    } else {
        None
    };
    Loaded { instructions: if project { read_instructions(root) } else { None }, skills, agents, memory, mcp_servers, problems }
}

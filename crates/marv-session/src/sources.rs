//! What a session can read from disk, by where it lives (the SDK's `sources`). Ports Marv's `src/sources.ts`.
//!
//! - `User`: your `~/.marv`: your own `AGENTS.md` (#62), personal skills and agents, memory (personal and this
//!   project's: both your own files, never in the repository), and your MCP servers (`mcp.json`).
//! - `Project`: the repository: `AGENTS.md` (or `CLAUDE.md`) and the parent folders' `AGENTS.md` up to the git root
//!   (#62), `.marv/skills`, `.marv/agents`, and `.mcp.json` (whose servers still start only once trusted, with
//!   `/mcp trust` in the CLI). Nested `AGENTS.md` files come with the tool results that touch their folders. The
//!   instruction files, skills and agent types count as the person's only in a project they trusted
//!   (`crate::project_trust`); otherwise the instruction files are quoted as the project's data, and the skills and
//!   agent types aren't loaded.
//!
//! The CLI loads both. A program using the SDK loads neither unless it asks: a library mustn't quietly read your
//! files, or start a repository's servers.

use std::path::Path;
use std::sync::Arc;

use marv_mcp::config::{Include, McpServerConfig, load_mcp_config};

use crate::agents::AgentType;
use crate::config::Env;
use crate::memory::{Memories, MemoryPaths, load_memory, memory_paths};
use crate::project_trust::{NestedTrust, ProjectFiles, TrustStatus, load_project};
use crate::prompt::Instructions;
use crate::skills::LoadedSkill;

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
    /// The project's root files as loaded, where they stand, and whether they count as the person's
    /// (`crate::project_trust`).
    pub project_files: ProjectFiles,
    pub project_status: Option<TrustStatus>,
    pub trusted: bool,
    /// Which subfolder instruction files count as the person's.
    pub nested: Arc<NestedTrust>,
}

/// Loads only the `sources` asked for. `config_dir` holds memory, `mcp.json` and the project trust; personal skills
/// and agents live in `<home>/.marv` (even with another config dir, as the CLI does). `trust`: whether the project's
/// instructions, skills and agent types count as the person's (`crate::project_trust::load_project`: `None` uses the
/// stored trust); when not, its instruction files are quoted as its data and its skills and agent types aren't loaded.
pub async fn load_sources(
    root: &Path,
    sources: &[Source],
    config_dir: &Path,
    home: &Path,
    env: &Env,
    trust: Option<bool>,
) -> Loaded {
    let user = sources.contains(&Source::User);
    let project = sources.contains(&Source::Project);
    let read = load_project(root, home, config_dir, user, project, trust);
    let mut problems = read.skill_problems;
    problems.extend(read.agent_problems);
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
    problems.extend(read.instruction_problems);
    Loaded {
        instructions: read.instructions,
        skills: read.skills,
        agents: read.agents,
        memory,
        mcp_servers,
        problems,
        project_files: read.files,
        project_status: project.then_some(read.status),
        trusted: read.trusted,
        nested: read.nested,
    }
}

//! Memory: short notes Marv keeps across sessions, in two scopes. Ports Marv's `src/memory.ts` (loading, paths and
//! the prompt section; the file edits live in `marv_tools::memory`, which the memory tool and `/remember` share).
//!
//! ```text
//! ~/.marv/memory/personal.md              everywhere (your preferences)
//! ~/.marv/memory/projects/<project>.md    one project (facts not obvious from its code)
//! ```
//!
//! Plain Markdown, one top-level bullet (`- `, `* `, `+ `) per memory. Headings, notes and nested bullets are the
//! user's and left alone. Loaded into the system prompt when a conversation starts. The model changes memory only
//! through the memory tool, approved each time: memory persists, so an instruction planted by a malicious file and
//! saved there would come back in every session (a persistent prompt injection).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::paths::{legacy_project_key, project_key};

pub use marv_tools::memory::{add_memory, find_memory, remove_memory};

pub const MAX_ENTRY_CHARS: usize = 500;
pub const MAX_ENTRIES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    Personal,
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryPaths {
    pub personal: PathBuf,
    pub project: PathBuf,
    /// Where the project file was before project keys had a hash: moved to `project` on load.
    pub legacy_project: Option<PathBuf>,
}

impl MemoryPaths {
    /// What the memory tool needs.
    pub fn for_tools(&self) -> marv_tools::MemoryPaths {
        marv_tools::MemoryPaths { personal: self.personal.clone(), project: self.project.clone() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Memories {
    pub personal: Vec<String>,
    pub project: Vec<String>,
}

pub fn header(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Personal => "# Marv's memory: personal (applies to all projects)",
        MemoryScope::Project => "# Marv's memory: this project",
    }
}

pub fn memory_paths(config_dir: &Path, root: &Path) -> MemoryPaths {
    let dir = config_dir.join("memory");
    MemoryPaths {
        personal: dir.join("personal.md"),
        project: dir.join("projects").join(format!("{}.md", project_key(root))),
        legacy_project: Some(dir.join("projects").join(format!("{}.md", legacy_project_key(root)))),
    }
}

static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-*+] (.*)$").unwrap());

/// The memories in a file's text: its top-level bullets.
pub fn entries_of(text: &str) -> Vec<String> {
    text.split('\n')
        .filter_map(|line| BULLET.captures(line.trim_end()).map(|c| c[1].trim().to_string()))
        .filter(|e| !e.is_empty())
        .collect()
}

async fn read_entries(path: &Path) -> Vec<String> {
    tokio::fs::read_to_string(path).await.map(|t| entries_of(&t)).unwrap_or_default()
}

pub async fn load_memory(paths: &MemoryPaths) -> std::io::Result<Memories> {
    // The old key wasn't unique, so the first project to load it claims it (colliding projects already shared it).
    if let Some(legacy) = &paths.legacy_project
        && !paths.project.exists()
        && legacy.exists()
    {
        tokio::fs::rename(legacy, &paths.project).await?;
    }
    Ok(Memories { personal: read_entries(&paths.personal).await, project: read_entries(&paths.project).await })
}

/// The system prompt's Memory section.
pub fn memory_section(memories: &Memories) -> String {
    let list = |entries: &[String]| {
        if entries.is_empty() {
            "(nothing yet)".to_string()
        } else {
            entries.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n")
        }
    };
    format!(
        "# Memory\n\nYou keep a memory across sessions, in two parts: personal (applies everywhere: the user's preferences and how they like you to work) and project (facts about this project that aren't obvious from its code or instructions). When you learn something worth knowing next time, especially a preference or a correction from the user, save it with the memory tool; the user approves each change. Keep each memory to one short sentence. Don't save temporary details of the current task, things already written in the code, or secrets. If a memory turns out to be wrong, remove it.\n\nPersonal (all projects):\n{}\n\nThis project:\n{}",
        list(&memories.personal),
        list(&memories.project)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_top_level_bullets_are_memories() {
        let text = "# Marv's memory\n\n- uses tabs\n* likes short replies  \n+ third\n  - nested, the user's\nprose\n- \n";
        assert_eq!(entries_of(text), vec!["uses tabs", "likes short replies", "third"]);
    }

    #[test]
    fn the_section_lists_both_scopes() {
        let s = memory_section(&Memories { personal: vec!["a".into()], project: vec![] });
        assert!(s.contains("Personal (all projects):\n- a\n\nThis project:\n(nothing yet)"));
    }

    #[test]
    fn project_memory_is_keyed_by_project() {
        let p = memory_paths(Path::new("/c"), Path::new("/home/me/proj"));
        assert_eq!(p.personal, Path::new("/c/memory/personal.md"));
        assert!(p.project.to_string_lossy().starts_with("/c/memory/projects/-home-me-proj-"));
    }
}

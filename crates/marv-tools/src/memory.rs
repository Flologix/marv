//! `memory`: save or remove a one-line memory (personal or project).
//!
//! Ports Marv's `src/tools/memory.ts` (the tool) and the bullet edits it calls from `src/memory.ts` (`addMemory`,
//! `findMemory`, `removeMemory`), which live here so marv-session can reuse them for `/remember` and `/forget`.
//!
//! - Memories are top-level Markdown bullets (`- `, `* `, `+ `); headings, notes and nested bullets are the user's and
//!   are left alone: adding appends one line, removing deletes the one matching line. Files are 0600
//!   (`~/.marv/memory/personal.md`, `~/.marv/memory/projects/<project>.md`).
//! - `kind: Write` and no `auto_safe`: every change is approved, even in yolo. Memory persists, and an instruction
//!   planted by a malicious file and saved there would return in every session.
//! - A removal must match exactly one memory, or it fails in the preview, before anyone is asked.
//! - Approval previews show the memory as plain text, with the file (home shortened to `~`) as the note.

use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{Preview, Scope, Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryAction {
    Add,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    Personal,
    Project,
}

impl MemoryScope {
    fn where_(self) -> &'static str {
        match self {
            Self::Personal => "personal, all projects",
            Self::Project => "this project",
        }
    }
    fn word(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryInput {
    pub action: MemoryAction,
    pub scope: MemoryScope,
    pub text: String,
}

/// `~/x` for a path in the home folder.
pub fn shorten_home(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Appends a bullet unless it's already there. Ok(false): already remembered.
pub async fn add_memory(_path: &Path, _text: &str) -> Result<bool, ToolError> {
    todo!("port: addMemory (src/memory.ts): one '- text' line, 0600 via write_private, already-there check")
}

/// The memories (top-level bullets) containing `text`.
pub async fn find_memory(_path: &Path, _text: &str) -> Vec<String> {
    todo!("port: findMemory (src/memory.ts)")
}

/// Removes the one memory containing `text`; returns it.
pub async fn remove_memory(_path: &Path, _text: &str) -> Result<String, ToolError> {
    todo!("port: removeMemory (src/memory.ts): exactly one match, else an error naming the count")
}

fn path_for(scope: MemoryScope, ctx: &ToolContext) -> Result<&Path, ToolError> {
    let paths = ctx.memory.as_ref().ok_or_else(|| ToolError::new("Memory isn't available here."))?;
    Ok(match scope {
        MemoryScope::Personal => &paths.personal,
        MemoryScope::Project => &paths.project,
    })
}

pub struct Memory;

#[async_trait]
impl Tool for Memory {
    type Input = MemoryInput;

    fn name(&self) -> &str {
        "memory"
    }
    fn description(&self) -> &str {
        "Save something to remember in future sessions, or remove a memory that's wrong. Use it for the user's preferences and corrections (personal) and for project facts that aren't obvious from the code (project). Not for temporary task details or secrets. The user approves each change."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "remove"], "description": "add a memory, or remove one that's wrong or outdated"},
                "scope": {"type": "string", "enum": ["personal", "project"], "description": "personal: about the user, applies in every project. project: about this project only."},
                "text": {"type": "string", "minLength": 1, "description": "add: the memory, one short sentence. remove: text from the memory to remove (must match exactly one)."}
            },
            "required": ["action", "scope", "text"],
            "additionalProperties": false
        })
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Write
    }
    fn check(&self, input: &MemoryInput) -> Result<(), String> {
        if input.text.is_empty() { Err("text: must not be empty".into()) } else { Ok(()) }
    }
    fn label(&self, input: &MemoryInput) -> String {
        let verb = if input.action == MemoryAction::Add { "remember" } else { "forget" };
        format!("{verb}: {}", input.text)
    }
    fn scope(&self, _input: &MemoryInput) -> Option<Scope> {
        Some(Scope { key: "memory".into(), description: "memory changes".into() })
    }

    async fn preview(&self, input: &MemoryInput, ctx: &ToolContext) -> Result<Preview, ToolError> {
        let path = path_for(input.scope, ctx)?;
        let note = Some(shorten_home(path, &ctx.home));
        if input.action == MemoryAction::Add {
            return Ok(Preview {
                title: format!("Remember ({})", input.scope.where_()),
                text: Some(input.text.clone()),
                note,
                ..Default::default()
            });
        }
        // Check the match now, so the user isn't asked to approve a removal that can't happen.
        let found = find_memory(path, &input.text).await;
        if found.len() != 1 {
            return Err(ToolError(if found.is_empty() {
                format!("No {} memory matches \"{}\".", input.scope.word(), input.text)
            } else {
                format!("{} memories match \"{}\"; use more of the text.", found.len(), input.text)
            }));
        }
        Ok(Preview {
            title: format!("Forget ({})", input.scope.where_()),
            text: found.into_iter().next(),
            note,
            ..Default::default()
        })
    }

    async fn run(&self, input: &MemoryInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path = path_for(input.scope, ctx)?;
        let scope = input.scope.word();
        if input.action == MemoryAction::Add {
            return Ok(if add_memory(path, &input.text).await? {
                ToolOutput::ok(format!("Saved to {scope} memory: {}", input.text), format!("saved to {scope} memory"))
            } else {
                ToolOutput::ok(format!("Already in {scope} memory."), "already remembered")
            });
        }
        let removed = remove_memory(path, &input.text).await?;
        Ok(ToolOutput::ok(format!("Removed from {scope} memory: {removed}"), format!("removed from {scope} memory")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_shortened_and_labels_say_what_happens() {
        assert_eq!(
            shorten_home(Path::new("/home/me/.marv/memory/personal.md"), Path::new("/home/me")),
            "~/.marv/memory/personal.md"
        );
        assert_eq!(shorten_home(Path::new("/srv/x"), Path::new("/home/me")), "/srv/x");
        let input = MemoryInput { action: MemoryAction::Remove, scope: MemoryScope::Project, text: "uses bun".into() };
        assert_eq!(Memory.label(&input), "forget: uses bun");
    }
}

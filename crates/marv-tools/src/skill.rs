//! `skill`: load a skill's instructions, or read one of its files.
//!
//! Ports Marv's `src/tools/skill.ts` (fully). Skills are loaded by marv-session (`src/skills.ts`); only names and
//! descriptions are in the system prompt (stable all session, so the cache holds), and the model loads the full
//! instructions here. Offered only when skills exist (`tool_specs_for`).
//!
//! marv-rs security fix (docs/SECURITY.md): `file` is resolved with `marv_sandbox::resolve_within`, after realpath.
//! Marv compared only the lexical path, so a symlink in a skill's folder (`key -> ~/.ssh/id_ed25519`) let the model
//! read any file, outside the sandbox. Skills run as `Provenance::Project` text (a project skill is project content).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{Skill, Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillInput {
    pub name: String,
    pub file: Option<String>,
}

pub struct SkillTool;

/// Reads one of a skill's files, confined to its folder after realpath.
pub fn read_skill_file(skill: &Skill, file: &str) -> Result<String, ToolError> {
    let path = marv_sandbox::resolve_within(&skill.dir, file).map_err(|err| match err {
        marv_sandbox::SandboxError::Missing(_) => {
            let files = if skill.files.is_empty() { "none".to_string() } else { skill.files.join(", ") };
            ToolError(format!("The {} skill has no file \"{file}\". Its files: {files}.", skill.name))
        }
        marv_sandbox::SandboxError::OutsideDir { .. } => {
            ToolError(format!("\"{file}\" is outside the {} skill's folder.", skill.name))
        }
        other => ToolError(other.to_string()),
    })?;
    if !crate::files::is_regular_file(&path) {
        return Err(ToolError(format!("\"{file}\" is not a regular file in the {} skill's folder.", skill.name)));
    }
    std::fs::read_to_string(&path).map_err(|e| ToolError(format!("Couldn't read \"{file}\": {e}")))
}

#[async_trait]
impl Tool for SkillTool {
    type Input = SkillInput;

    fn name(&self) -> &str {
        "skill"
    }
    fn description(&self) -> &str {
        "Load a skill: detailed instructions for a specific kind of task (the available skills are listed in the system prompt). Load the matching skill before starting such a task, then follow it. Use `file` to read one of the skill's extra files."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The skill's name, from the list in the system prompt."},
                "file": {"type": "string", "description": "A file from the skill's folder to read instead, e.g. reference.md."}
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
    fn label(&self, input: &SkillInput) -> String {
        match &input.file {
            Some(file) => format!("{}/{file}", input.name),
            None => input.name.clone(),
        }
    }

    async fn run(&self, input: &SkillInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(found) = ctx.skills.iter().find(|s| s.name == input.name) else {
            let names: Vec<&str> = ctx.skills.iter().map(|s| s.name.as_str()).collect();
            let names = if names.is_empty() { "none".to_string() } else { names.join(", ") };
            return Err(ToolError(format!("There's no skill called \"{}\". Available skills: {names}.", input.name)));
        };
        if let Some(file) = &input.file {
            let text = read_skill_file(found, file)?;
            let lines = text.split('\n').count();
            return Ok(ToolOutput::ok(text, format!("{lines} lines")));
        }
        let files = if found.files.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n---\nThis skill's folder is {}. Its files: {}. Read one with skill {{\"name\": \"{}\", \"file\": \"...\"}}; scripts can be run with bash from that folder.",
                found.dir.display(),
                found.files.join(", "),
                found.name
            )
        };
        Ok(ToolOutput::ok(format!("{}{files}", found.body), "loaded"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::sync::Arc;

    fn pdf(dir: &TempDir) -> Skill {
        Skill {
            name: "pdf".into(),
            description: "PDFs".into(),
            dir: dir.path().to_path_buf(),
            body: "Use pdftotext.".into(),
            files: vec!["reference.md".into()],
        }
    }

    #[tokio::test]
    async fn loads_instructions_and_points_to_files() {
        let dir = TempDir::new("skill");
        dir.write("reference.md", "ref\n");
        let ctx = ToolContext { skills: Arc::new(vec![pdf(&dir)]), ..ToolContext::new("/tmp") };
        let out = SkillTool.run(&SkillInput { name: "pdf".into(), file: None }, &ctx).await.unwrap();
        assert!(out.output.starts_with("Use pdftotext."));
        assert!(out.output.contains(&dir.path().display().to_string()));
        let file = SkillTool.run(&SkillInput { name: "pdf".into(), file: Some("reference.md".into()) }, &ctx).await.unwrap();
        assert_eq!(file.output, "ref\n");
        let none = SkillTool.run(&SkillInput { name: "nope".into(), file: None }, &ctx).await.unwrap_err();
        assert_eq!(none.0, "There's no skill called \"nope\". Available skills: pdf.");
    }

    #[test]
    fn reads_nothing_outside_the_folder_even_through_a_symlink() {
        let dir = TempDir::new("skill-link");
        let outside = TempDir::new("skill-secret");
        outside.write("id_ed25519", "PRIVATE KEY");
        std::os::unix::fs::symlink(outside.path().join("id_ed25519"), dir.path().join("key")).unwrap();
        let skill = pdf(&dir);
        let escape = read_skill_file(&skill, "../../../config.json").unwrap_err();
        assert_eq!(escape.0, "\"../../../config.json\" is outside the pdf skill's folder.");
        let link = read_skill_file(&skill, "key").unwrap_err();
        assert!(link.0.contains("outside the pdf skill's folder"), "{}", link.0);
        assert!(!link.0.contains("PRIVATE"));
        assert_eq!(
            read_skill_file(&skill, "missing.md").unwrap_err().0,
            "The pdf skill has no file \"missing.md\". Its files: reference.md."
        );
    }
}

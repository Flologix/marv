//! Skills: reusable instructions for specific tasks, as Markdown files. Ports Marv's `src/skills.ts`.
//!
//! ```text
//! .marv/skills/<name>/SKILL.md      the project's skills (win a name clash)
//! ~/.marv/skills/<name>/SKILL.md    your personal skills
//! ```
//!
//! Progressive disclosure: only each skill's name and description go into the system prompt (stable all session, so
//! the cache holds); the model loads the full instructions with the `skill` tool. Broken ones are skipped and
//! reported (startup notice and `/skills`).
//!
//! marv-rs change (docs/SECURITY.md): a skill folder or `SKILL.md` that is a symlink leading out of its skills folder
//! is refused (`marv_sandbox::resolve_within`); Marv followed it, so a cloned repository could make Marv read any
//! file of yours into the prompt.

use std::path::{Path, PathBuf};

use crate::frontmatter::parse_frontmatter;

pub use marv_tools::Skill;

pub const MAX_BODY_CHARS: usize = 50_000;
pub const MAX_FILES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Project,
    Personal,
}

/// A loaded skill: what the `skill` tool needs, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    pub skill: Skill,
    pub source: SkillSource,
}

pub fn skills_dir(base: &Path) -> PathBuf {
    base.join(".marv").join("skills")
}

/// `^[a-z0-9][a-z0-9-]{0,63}$`: skill and agent names.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Cuts a body at `max` chars with a note naming the file.
pub(crate) fn cap_body(body: String, file: &str) -> String {
    if body.chars().count() <= MAX_BODY_CHARS {
        return body;
    }
    let cut: String = body.chars().take(MAX_BODY_CHARS).collect();
    format!("{cut}\n\n({file} was cut off here: it's longer than {MAX_BODY_CHARS} characters.)")
}

/// Everything in a skill folder except SKILL.md, relative and sorted (at most [`MAX_FILES`]; symlinks not followed).
fn list_files(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, current: &Path, files: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(current) else {
            return;
        };
        let mut names: Vec<_> = entries.flatten().collect();
        names.sort_by_key(|e| e.file_name());
        for entry in names {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => walk(root, &path, files),
                Ok(_) if path != root.join("SKILL.md") => {
                    files.push(path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned());
                }
                _ => {}
            }
            if files.len() >= MAX_FILES {
                return;
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort();
    files
}

/// Parses one skill folder's SKILL.md: the skill, or a problem to report.
pub fn read_skill(dir: &Path, folder: &str, source: SkillSource, shown: &str) -> Result<LoadedSkill, String> {
    let text = std::fs::read_to_string(dir.join("SKILL.md")).map_err(|e| format!("{shown}: couldn't be read ({e})."))?;
    let parsed = parse_frontmatter(&text, shown)?;
    let name =
        parsed.fields.get("name").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).unwrap_or_else(|| folder.to_string());
    let description = parsed.fields.get("description").and_then(|v| v.as_str()).map(|s| s.trim().to_string()).unwrap_or_default();
    if !valid_name(&name) {
        return Err(format!("{shown}: the name \"{name}\" should be lowercase letters, digits and dashes."));
    }
    if description.is_empty() {
        return Err(format!("{shown} needs a description: it's how the model knows when to use the skill."));
    }
    let body = cap_body(parsed.body, "SKILL.md");
    Ok(LoadedSkill { skill: Skill { name, description, dir: dir.to_path_buf(), body, files: list_files(dir) }, source })
}

/// Loads personal skills (under `home`), then project skills (under `root`; they win a name clash). Either can be
/// left out. Skips broken ones, saying why. Sorted by name.
pub fn load_skills(root: Option<&Path>, home: Option<&Path>) -> (Vec<LoadedSkill>, Vec<String>) {
    let mut by_name: std::collections::BTreeMap<String, LoadedSkill> = Default::default();
    let mut problems = Vec::new();
    for (base, source, prefix) in [(home, SkillSource::Personal, "~/"), (root, SkillSource::Project, "")] {
        let Some(base) = base else { continue };
        let dir = skills_dir(base);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut folders: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        folders.sort();
        for folder in folders {
            let shown = format!("{prefix}.marv/skills/{folder}/SKILL.md");
            // marv-rs: the folder and its SKILL.md must stay inside the skills folder after realpath.
            let skill_dir = match marv_sandbox::resolve_within(&dir, &folder) {
                Ok(d) if d.is_dir() => d,
                Ok(_) => continue,
                Err(_) => {
                    problems.push(format!("{shown}: its folder leads outside .marv/skills, so it wasn't loaded."));
                    continue;
                }
            };
            if !skill_dir.join("SKILL.md").exists() {
                continue;
            }
            if marv_sandbox::resolve_within(&skill_dir, "SKILL.md").is_err() {
                problems.push(format!("{shown} leads outside its folder, so it wasn't loaded."));
                continue;
            }
            match read_skill(&skill_dir, &folder, source, &shown) {
                Ok(skill) => {
                    by_name.insert(skill.skill.name.clone(), skill);
                }
                Err(problem) => problems.push(problem),
            }
        }
    }
    (by_name.into_values().collect(), problems)
}

/// The user ran `/<skill> <args>`: one message with the skill's instructions and what they asked.
pub fn skill_message(skill: &Skill, args: &str) -> String {
    let files = if skill.files.is_empty() {
        String::new()
    } else {
        format!("\n\n(Its files are in {}: {}.)", skill.dir.display(), skill.files.join(", "))
    };
    let request = if args.is_empty() { "They gave no further details.".to_string() } else { format!("Their request: {args}") };
    format!("The user ran the \"{}\" skill. Follow its instructions:\n\n{}{files}\n\n{request}", skill.name, skill.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("marv-skills-{tag}-{}-{}", std::process::id(), marv_fs::random_hex()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(d).unwrap()
    }
    fn write(path: PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn project_skills_win_a_clash_and_broken_ones_are_reported() {
        let (root, home) = (temp("root"), temp("home"));
        write(skills_dir(&home).join("notes/SKILL.md"), "---\nname: notes\ndescription: Personal.\n---\nP");
        write(skills_dir(&root).join("notes/SKILL.md"), "---\nname: notes\ndescription: Project.\n---\nQ");
        write(skills_dir(&root).join("notes/ref.md"), "x");
        write(skills_dir(&root).join("bad/SKILL.md"), "---\nname: Bad Name\ndescription: x\n---\n");
        let (skills, problems) = load_skills(Some(&root), Some(&home));
        assert_eq!(skills.len(), 1);
        assert_eq!((skills[0].skill.description.as_str(), skills[0].source), ("Project.", SkillSource::Project));
        assert_eq!(skills[0].skill.files, vec!["ref.md"]);
        assert!(problems[0].contains("should be lowercase"));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_skill_folder_symlinked_out_of_the_project_is_refused() {
        let (root, outside) = (temp("root2"), temp("outside"));
        write(outside.join("SKILL.md"), "---\nname: evil\ndescription: Reads your files.\n---\n");
        std::fs::create_dir_all(skills_dir(&root)).unwrap();
        std::os::unix::fs::symlink(&outside, skills_dir(&root).join("evil")).unwrap();
        let (skills, problems) = load_skills(Some(&root), None);
        assert!(skills.is_empty());
        assert!(problems[0].contains("outside"));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn names() {
        assert!(valid_name("code-review"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("A"));
        assert!(!valid_name(&"a".repeat(65)));
    }
}

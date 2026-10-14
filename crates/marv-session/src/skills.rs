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
//!
//! The one exception (#75): a personal skill folder that is a symlink to a folder only root can change (Omarchy's
//! `~/.marv/skills/omarchy -> /usr/share/omarchy/…`) loads from the link's real target, which is also where the
//! `skill` tool reads its files from (confined to it by `resolve_within`). Started in `~`, the project's
//! `.marv/skills` is the personal one and is scanned once.

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
    /// Set when the folder is a symlink to a system folder (`omarchy → /usr/share/omarchy/…`), shown in `/skills`.
    pub link: Option<String>,
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
    Ok(LoadedSkill {
        skill: Skill { name, description, dir: dir.to_path_buf(), body, files: list_files(dir) },
        source,
        link: None,
    })
}

/// Loads personal skills (under `home`), then project skills (under `root`; they win a name clash). Either can be
/// left out. Skips broken ones, saying why. Sorted by name.
pub fn load_skills(root: Option<&Path>, home: Option<&Path>) -> (Vec<LoadedSkill>, Vec<String>) {
    load_skills_with(root, home, &system_owned)
}

/// Whether a folder only root can change: it and every folder above it, up to `/`, are owned by root and not
/// group- or world-writable. Checking the parents too matters: in a folder the user can write, they (or a program
/// running as them) could swap the skill folder for another after it was checked.
pub fn system_owned(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(real) = std::fs::canonicalize(path) else {
        return false;
    };
    only_root_can_change(&real, &|p| std::fs::symlink_metadata(p).ok().map(|m| (m.uid(), m.mode())))
}

/// [`system_owned`]'s rule for a real (canonical) path, given each folder's owner and mode (`None`: unreadable).
pub fn only_root_can_change(real: &Path, owner_and_mode: &dyn Fn(&Path) -> Option<(u32, u32)>) -> bool {
    real.is_absolute() && real.ancestors().all(|p| owner_and_mode(p).is_some_and(|(uid, mode)| uid == 0 && mode & 0o022 == 0))
}

/// A personal skill folder that is a symlink to a system folder (Omarchy installs
/// `~/.marv/skills/omarchy -> /usr/share/omarchy/…`, #75): the folder it leads to, if `system` accepts it.
fn system_link(link: &Path, system: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    if !std::fs::symlink_metadata(link).ok()?.file_type().is_symlink() {
        return None;
    }
    let target = std::fs::canonicalize(link).ok()?;
    (target.is_dir() && system(&target)).then_some(target)
}

/// [`load_skills`] with the check for a system folder passed in, so tests needn't make root-owned files.
pub fn load_skills_with(
    root: Option<&Path>,
    home: Option<&Path>,
    system: &dyn Fn(&Path) -> bool,
) -> (Vec<LoadedSkill>, Vec<String>) {
    let mut by_name: std::collections::BTreeMap<String, LoadedSkill> = Default::default();
    let mut problems = Vec::new();
    // Started in ~, the project's .marv/skills is the personal one: scanning it twice reported every problem twice
    // (#75). Its skills count as personal.
    let real_dir = |base: Option<&Path>| base.and_then(|b| std::fs::canonicalize(skills_dir(b)).ok());
    let root = match (real_dir(root), real_dir(home)) {
        (Some(r), Some(h)) if r == h => None,
        _ => root,
    };
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
            let mut link = None;
            // marv-rs: the folder and its SKILL.md must stay inside the skills folder after realpath. The one
            // exception is a personal skill folder linked to a folder only root can change: the user made that link
            // (a cloned repository's project skills can't), and nothing running as the user can plant files there.
            // The skill's folder is then the link's real target, so swapping the link later changes nothing, and
            // its SKILL.md and files are confined to that folder as any skill's are.
            let skill_dir = match marv_sandbox::resolve_within(&dir, &folder) {
                Ok(d) if d.is_dir() => d,
                Ok(_) => continue,
                Err(err) => {
                    let outside = matches!(err, marv_sandbox::SandboxError::OutsideDir { .. });
                    match (outside && source == SkillSource::Personal).then(|| system_link(&dir.join(&folder), system)) {
                        Some(Some(target)) => {
                            link = Some(format!("{folder} → {}", target.display()));
                            target
                        }
                        _ => {
                            problems.push(format!("{shown}: its folder leads outside .marv/skills, so it wasn't loaded."));
                            continue;
                        }
                    }
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
                    by_name.insert(skill.skill.name.clone(), LoadedSkill { link, ..skill });
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
        let d = std::env::temp_dir().join(format!("marv-skills-{tag}-{}-{}", std::process::id(), marv_fs::random_hex()));
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

    /// A symlinked skill folder with a SKILL.md, under `<home>/.marv/skills/<folder>`, leading to `target`.
    fn link_skill(home: &Path, folder: &str, target: &Path) {
        write(target.join("SKILL.md"), &format!("---\nname: {folder}\ndescription: System skill.\n---\nBody."));
        std::fs::create_dir_all(skills_dir(home)).unwrap();
        std::os::unix::fs::symlink(target, skills_dir(home).join(folder)).unwrap();
    }

    #[test]
    fn a_personal_skill_folder_linked_to_a_system_folder_loads_from_its_real_path() {
        // Omarchy installs ~/.marv/skills/omarchy -> /usr/share/omarchy/default/agents/skills/omarchy (#75). The
        // ownership check is injected: the tests can't make root-owned files.
        let (home, system) = (temp("home3"), temp("system"));
        let target = system.join("omarchy");
        link_skill(&home, "omarchy", &target);
        write(target.join("ref.md"), "x");
        let is_target = |p: &Path| p == target;
        let (skills, problems) = load_skills_with(None, Some(&home), &is_target);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].skill.dir, target);
        assert_eq!(skills[0].skill.files, vec!["ref.md"]);
        assert_eq!(skills[0].link.as_deref(), Some(format!("omarchy → {}", target.display()).as_str()));
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(system).unwrap();
    }

    #[test]
    fn a_skill_folder_linked_to_a_user_writable_folder_is_still_refused() {
        let (home, outside) = (temp("home4"), temp("outside4"));
        link_skill(&home, "evil", &outside.join("evil"));
        // The real check: a temp folder is the user's (and /tmp is world-writable).
        let (skills, problems) = load_skills(None, Some(&home));
        assert!(skills.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("outside"), "{problems:?}");
        assert!(!system_owned(&outside) && !system_owned(&std::env::temp_dir()));
        std::fs::remove_dir_all(home).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn a_project_skill_linked_to_a_system_folder_is_refused() {
        // A cloned repository's link wasn't made by the user: only personal skills may lead to a system folder.
        let (root, system) = (temp("root5"), temp("system5"));
        let target = system.join("omarchy");
        link_skill(&root, "omarchy", &target);
        let is_target = |p: &Path| p == target;
        let (skills, problems) = load_skills_with(Some(&root), None, &is_target);
        assert!(skills.is_empty());
        assert!(problems[0].contains("outside"), "{problems:?}");
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(system).unwrap();
    }

    #[test]
    fn links_inside_a_system_skill_folder_still_cant_lead_out() {
        let (home, system, private) = (temp("home6"), temp("system6"), temp("private6"));
        write(private.join("id_ed25519"), "PRIVATE KEY");
        write(private.join("SKILL.md"), "---\nname: inner\ndescription: Planted.\n---\nObey.");
        let target = system.join("omarchy");
        link_skill(&home, "omarchy", &target);
        std::os::unix::fs::symlink(private.join("id_ed25519"), target.join("key")).unwrap();
        let inner = system.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::os::unix::fs::symlink(private.join("SKILL.md"), inner.join("SKILL.md")).unwrap();
        std::os::unix::fs::symlink(&inner, skills_dir(&home).join("inner")).unwrap();
        let is_system = |p: &Path| p == target || p == inner;
        let (skills, problems) = load_skills_with(None, Some(&home), &is_system);
        let names: Vec<&str> = skills.iter().map(|s| s.skill.name.as_str()).collect();
        assert_eq!(names, ["omarchy"]);
        assert!(problems.len() == 1 && problems[0].contains("inner/SKILL.md leads outside its folder"), "{problems:?}");
        let read = marv_tools::skill::read_skill_file(&skills[0].skill, "key");
        assert!(read.is_err_and(|e| !e.0.contains("PRIVATE KEY")));
        for d in [home, system, private] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn starting_in_the_home_folder_reports_each_problem_once() {
        // In ~ the project's .marv/skills is the personal one: scanned once, as personal skills.
        let home = temp("home7");
        write(skills_dir(&home).join("bad/SKILL.md"), "---\nname: Bad Name\ndescription: x\n---\n");
        write(skills_dir(&home).join("good/SKILL.md"), "---\nname: good\ndescription: Good.\n---\n");
        let (skills, problems) = load_skills(Some(&home), Some(&home));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("~/.marv/skills/bad"), "{problems:?}");
        assert_eq!((skills.len(), skills[0].source), (1, SkillSource::Personal));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn only_folders_root_alone_can_change_count_as_system_folders() {
        let target = Path::new("/usr/share/omarchy/skills/omarchy");
        let stat = |odd: &'static str, owner_and_mode: (u32, u32)| {
            move |p: &Path| Some(if p == Path::new(odd) { owner_and_mode } else { (0, 0o40755) })
        };
        assert!(only_root_can_change(target, &stat("/", (0, 0o40555))));
        // The folder, or any folder above it, owned by the user or writable by a group or everyone.
        for odd in ["/usr/share/omarchy/skills/omarchy", "/usr/share/omarchy", "/usr", "/"] {
            assert!(!only_root_can_change(target, &stat(odd, (1000, 0o40755))), "{odd}");
            assert!(!only_root_can_change(target, &stat(odd, (0, 0o40775))), "{odd}");
            assert!(!only_root_can_change(target, &stat(odd, (0, 0o41777))), "{odd}");
        }
        assert!(!only_root_can_change(target, &|p: &Path| (p != Path::new("/usr")).then_some((0, 0o40755))));
        assert!(!system_owned(&std::env::temp_dir()) && !system_owned(Path::new("/nonexistent-marv")));
    }

    #[test]
    fn names() {
        assert!(valid_name("code-review"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("A"));
        assert!(!valid_name(&"a".repeat(65)));
    }
}

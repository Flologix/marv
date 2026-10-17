//! The folder tree under the user's root: listing, new folders, renames and moving to the trash. Every path from
//! the page is relative to the root and checked to stay under it (no absolute paths, no `..`, no symlink out).

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
}

/// `rel` under `root`, or why not. `""` is the root itself.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("\"{rel}\" isn't a path inside your Marv folder."));
    }
    let path = root.join(rel_path);
    // A symlink inside the root may point anywhere: what counts is where it really is.
    if let Ok(real) = std::fs::canonicalize(&path)
        && !real.starts_with(root)
    {
        return Err(format!("\"{rel}\" leads outside your Marv folder."));
    }
    Ok(path)
}

/// A new name for something in a folder: one plain name, no separators.
pub fn check_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(format!("\"{name}\" can't be a name: use letters, numbers, dashes or dots, no slashes."));
    }
    Ok(name)
}

/// A folder's entries, folders first, `.git` left out.
pub fn list(root: &Path, rel: &str) -> Result<Vec<Entry>, String> {
    let dir = resolve(root, rel)?;
    let mut entries: Vec<Entry> = std::fs::read_dir(&dir)
        .map_err(|e| format!("Couldn't read {}: {e}", display(rel)))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name != ".git").then(|| Entry { dir: e.path().is_dir(), name })
        })
        .collect();
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(entries)
}

pub fn create_folder(root: &Path, parent: &str, name: &str) -> Result<String, String> {
    let name = check_name(name)?;
    let path = resolve(root, parent)?.join(name);
    if path.exists() {
        return Err(format!("{name} already exists."));
    }
    std::fs::create_dir(&path).map_err(|e| format!("Couldn't make {name}: {e}"))?;
    Ok(join(parent, name))
}

pub fn rename(root: &Path, rel: &str, name: &str) -> Result<String, String> {
    if rel.is_empty() {
        return Err("Your Marv folder itself can't be renamed here.".into());
    }
    let name = check_name(name)?;
    let from = resolve(root, rel)?;
    let parent = Path::new(rel).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let to = from.with_file_name(name);
    if to.exists() {
        return Err(format!("{name} already exists."));
    }
    std::fs::rename(&from, &to).map_err(|e| format!("Couldn't rename {}: {e}", display(rel)))?;
    Ok(join(&parent, name))
}

pub fn trash(root: &Path, rel: &str) -> Result<(), String> {
    if rel.is_empty() {
        return Err("Your Marv folder itself can't be deleted here.".into());
    }
    let path = resolve(root, rel)?;
    trash::delete(&path).map_err(|e| format!("Couldn't move {} to the trash: {e}", display(rel)))
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") }
}

fn display(rel: &str) -> &str {
    if rel.is_empty() { "your Marv folder" } else { rel }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> (tempfile_dir::Dir, PathBuf) {
        let dir = tempfile_dir::Dir::new();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    /// A temp folder removed on drop (no extra dependency for three tests).
    mod tempfile_dir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!("marv-desktop-test-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn paths_stay_under_the_root() {
        let (_dir, root) = root();
        assert!(resolve(&root, "").is_ok());
        assert!(resolve(&root, "a/b").is_ok());
        assert!(resolve(&root, "../x").is_err());
        assert!(resolve(&root, "a/../../x").is_err());
        assert!(resolve(&root, "/etc").is_err());
        std::os::unix::fs::symlink("/etc", root.join("out")).unwrap();
        assert!(resolve(&root, "out").is_err());
    }

    #[test]
    fn folders_are_made_listed_renamed() {
        let (_dir, root) = root();
        assert_eq!(create_folder(&root, "", "site").unwrap(), "site");
        assert_eq!(create_folder(&root, "site", "src").unwrap(), "site/src");
        std::fs::write(root.join("site/README.md"), "hi").unwrap();
        std::fs::create_dir(root.join("site/.git")).unwrap();
        assert_eq!(
            list(&root, "site").unwrap(),
            vec![Entry { name: "src".into(), dir: true }, Entry { name: "README.md".into(), dir: false }]
        );
        assert!(create_folder(&root, "", "site").is_err());
        assert!(create_folder(&root, "", "a/b").is_err());
        assert!(create_folder(&root, "", "..").is_err());
        assert_eq!(rename(&root, "site/src", "lib").unwrap(), "site/lib");
        assert!(root.join("site/lib").is_dir());
        assert!(rename(&root, "", "x").is_err());
        assert!(trash(&root, "").is_err());
    }
}

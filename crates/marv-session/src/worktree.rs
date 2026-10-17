//! Git worktrees for subagents: a second checkout on its own branch, so agents working in parallel don't edit the
//! same files. Ports Marv's `src/worktree.ts`.
//!
//! ```text
//! git worktree add -b marv/<task>-<id> ~/.marv/worktrees/<project>/<task>-<id> HEAD
//! ```
//!
//! Inside the sandbox a subagent can change only its worktree; the repository's `.git` is read-only, so it can't plant
//! hooks or config that git would later run *outside* the sandbox. When it ends (however it ends) **Marv commits for
//! it** ([`finish_worktree`]): one commit `marv: <description>` (`(interrupted)` unless it ended with an answer),
//! through a temporary git dir Marv writes itself (HEAD → the branch, `commondir` → the real `.git`, a copy of the
//! worktree's index, without which force-added or skip-worktree files would be dropped), so nothing the worktree's
//! own `.git` file or record says is trusted. It refuses to commit if the worktree holds another `.git` outside
//! ignored folders (git would use that repository's config). Then it deletes only that worktree's folder and admin
//! dir (never `git worktree prune`, which would drop the user's other worktrees) and keeps the branch (deleted if it
//! has no commits). Commits are unsigned: a pinentry prompt can't show in the TUI and would block every subagent.
//!
//! Known limits: split-index and reftable repositories fail safe (git errors, the folder is kept).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use marv_tools::ToolError;

use crate::git::{GIT_TIMEOUT, run_git};

pub const NOT_A_REPO: &str =
    "isolation: \"worktree\" needs a git repository with at least one commit. Start the agent without isolation instead.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub dir: PathBuf,
    pub branch: String,
    /// Short hash of the commit it started from.
    pub base: String,
    /// The main checkout (the project root).
    pub repo: PathBuf,
    /// The repository's shared `.git` (the sandbox shows it read-only).
    pub git_dir: PathBuf,
    /// This worktree's own folder inside it (`.git/worktrees/<id>`): its HEAD and index.
    pub admin_dir: PathBuf,
    /// Where the parent's project sits in the repository ("pkg/", or "" at the top).
    pub prefix: String,
    /// Where the subagent works: `dir` + `prefix` (a session in `repo/pkg` gives its subagent `pkg`).
    pub work_dir: PathBuf,
}

/// Where git must look for a worktree a subagent has used: never its `.git` file, which was within the subagent's
/// reach and could point at a fake repository with its own config.
pub fn worktree_env(wt: &Worktree) -> Vec<(String, String)> {
    vec![
        ("GIT_DIR".into(), wt.admin_dir.to_string_lossy().into_owned()),
        ("GIT_COMMON_DIR".into(), wt.git_dir.to_string_lossy().into_owned()),
        ("GIT_WORK_TREE".into(), wt.dir.to_string_lossy().into_owned()),
    ]
}

/// "Task 2: Parser errors" → "marv/task-2-parser-errors-ab12".
pub fn branch_name(description: &str, id: &str) -> String {
    let mut slug = String::new();
    for c in description.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(40).collect();
    let slug = slug.trim_end_matches('-');
    format!("marv/{}-{id}", if slug.is_empty() { "task" } else { slug })
}

/// Runs git for the worktree code (60 s timeout); `out` is trimmed.
async fn git(cwd: &Path, args: &[&str], pinned: &[(String, String)]) -> (bool, String) {
    let result = run_git(cwd, args, pinned, GIT_TIMEOUT).await;
    if result.timed_out {
        return (false, format!("git timed out after {} s", GIT_TIMEOUT.as_secs()));
    }
    (result.ok, result.out.trim().to_string())
}

/// A git dir of Marv's own for the finishing commit: HEAD on the worktree's branch, the repository's real `.git` as
/// `commondir`, and a copy of the index.
///
/// The copy is required, not just faster: the index is what says a file is tracked even though it matches
/// `.gitignore` (added with `add -f`) or is marked skip-worktree, and `add -A` from an empty index would silently
/// leave those files out of the commit, i.e. delete them from the branch. So without it, this fails and nothing is
/// committed. Pinning `GIT_DIR` to the worktree's record (`.git/worktrees/<id>`) isn't enough: git finds branches
/// through that folder's `commondir` file even when `GIT_COMMON_DIR` is set, follows its HEAD, and, in a repository
/// with per-worktree config, reads its `config.worktree`, where a filter could run a program. The sandbox shows that
/// folder read-only; this is the second layer.
async fn commit_git_dir(wt: &Worktree) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("marv-commit-{}", marv_fs::random_hex()));
    tokio::fs::create_dir(&dir).await?;
    let filled = async {
        tokio::fs::write(dir.join("HEAD"), format!("ref: refs/heads/{}\n", wt.branch)).await?;
        tokio::fs::write(dir.join("commondir"), format!("{}\n", wt.git_dir.display())).await?;
        tokio::fs::copy(wt.admin_dir.join("index"), dir.join("index")).await
    };
    if let Err(err) = filled.await {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Err(err);
    }
    Ok(dir)
}

/// Another git repository inside the worktree (e.g. in a submodule's folder): when git looks into it, it uses that
/// repository's own config, which could make it run a program outside the sandbox. Found at any depth, except in
/// `skip`: folders git ignores, which `git add -A` never looks into (so a `node_modules/x/.git` or a
/// `.venv/src/<pkg>/.git` is no reason to refuse, and walking them is most of the work after an install). Symlinks
/// aren't followed (git doesn't either; following them would leave the worktree and go round loops). Async, so a big
/// tree doesn't hold up the runtime. Fails if a folder can't be read.
async fn nested_repo(root: &Path, skip: &HashSet<String>) -> std::io::Result<Option<String>> {
    let mut pending = vec![String::new()];
    while let Some(folder) = pending.pop() {
        let mut entries = tokio::fs::read_dir(root.join(&folder)).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if folder.is_empty() { name.clone() } else { format!("{folder}/{name}") };
            if name == ".git" {
                if !folder.is_empty() {
                    return Ok(Some(path)); // the top-level one is the worktree's own pointer, which Marv's git never reads
                }
            } else if entry.file_type().await?.is_dir() && !skip.contains(&path) {
                pending.push(path);
            }
        }
    }
    Ok(None)
}

/// The folders git ignores in the worktree, relative to its top ("node_modules"), listed by the same git (same git
/// dir, index and ignore rules) that then runs `add -A`. Empty if git can't list them: then everything is walked
/// (slower, never less safe).
async fn ignored_folders(dir: &Path, pinned: &[(String, String)]) -> HashSet<String> {
    let (ok, out) = git(dir, &["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"], pinned).await;
    if !ok {
        return HashSet::new();
    }
    out.split('\0').filter_map(|path| path.strip_suffix('/')).map(str::to_string).collect()
}

/// What a worktree would start from, how many uncommitted changes it would leave behind (in the whole repository,
/// even when `root` is a subfolder of it), and where `root` sits in it ("pkg/", or "" at the top).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub base: String,
    pub dirty: usize,
    pub prefix: String,
}

/// [`RepoState`], or `None` outside a repository with a commit.
pub async fn inspect_repo(root: &Path) -> Option<RepoState> {
    let (ok, base) = git(root, &["rev-parse", "--short", "HEAD"], &[]).await;
    if !ok {
        return None;
    }
    let (ok, prefix) = git(root, &["rev-parse", "--show-prefix"], &[]).await;
    if !ok {
        return None;
    }
    let (_, status) = git(root, &["status", "--porcelain"], &[]).await;
    let dirty = if status.is_empty() { 0 } else { status.split('\n').count() };
    Some(RepoState { base, dirty, prefix })
}

/// Creates the worktree and its branch from HEAD (`ToolError(NOT_A_REPO)` outside a repository with a commit).
pub async fn create_worktree(root: &Path, base_dir: &Path, description: &str) -> Result<Worktree, ToolError> {
    let repo = inspect_repo(root).await.ok_or_else(|| ToolError::new(NOT_A_REPO))?;
    let id: String = marv_fs::random_hex().chars().take(4).collect();
    let branch = branch_name(description, &id);
    let dir = base_dir.join(&branch["marv/".len()..]);
    tokio::fs::create_dir_all(base_dir).await.map_err(|e| ToolError::new(format!("Couldn't create a worktree: {e}")))?;
    let dir_text = dir.to_string_lossy().into_owned();
    let (ok, out) = git(root, &["worktree", "add", "-q", "-b", &branch, &dir_text, "HEAD"], &[]).await;
    if !ok {
        return Err(ToolError::new(format!("Couldn't create a worktree: {out}")));
    }
    let absolute = |path: String| {
        let path = PathBuf::from(path);
        if path.is_absolute() { path } else { dir.join(path) }
    };
    let git_dir = absolute(git(&dir, &["rev-parse", "--git-common-dir"], &[]).await.1);
    let admin_dir = absolute(git(&dir, &["rev-parse", "--git-dir"], &[]).await.1);
    // No trailing slash: "pkg/" → <dir>/pkg.
    let sub = repo.prefix.trim_end_matches('/');
    let work_dir = if sub.is_empty() { dir.clone() } else { dir.join(sub) };
    // A folder with no tracked files (e.g. a new package) isn't checked out: start it empty.
    tokio::fs::create_dir_all(&work_dir).await.map_err(|e| ToolError::new(format!("Couldn't create a worktree: {e}")))?;
    Ok(Worktree { dir, branch, base: repo.base, repo: root.to_path_buf(), git_dir, admin_dir, prefix: repo.prefix, work_dir })
}

/// Commits everything in the worktree to its branch, unless it holds another repository ([`nested_repo`]). Returns
/// why it couldn't, or `None`.
async fn commit_changes(wt: &Worktree, message: &str) -> Option<String> {
    let git_dir = match commit_git_dir(wt).await {
        Ok(dir) => dir,
        Err(err) => return Some(format!("couldn't read the worktree's index ({err}).")),
    };
    let problem = commit_with(wt, &git_dir, message).await;
    let _ = tokio::fs::remove_dir_all(&git_dir).await;
    problem
}

async fn commit_with(wt: &Worktree, git_dir: &Path, message: &str) -> Option<String> {
    let pinned = vec![
        ("GIT_DIR".to_string(), git_dir.to_string_lossy().into_owned()),
        ("GIT_COMMON_DIR".to_string(), wt.git_dir.to_string_lossy().into_owned()),
        ("GIT_WORK_TREE".to_string(), wt.dir.to_string_lossy().into_owned()),
    ];
    let run = |args: &'static [&'static str]| git(&wt.dir, args, &pinned);
    match nested_repo(&wt.dir, &ignored_folders(&wt.dir, &pinned).await).await {
        Err(err) => return Some(format!("couldn't look through the worktree ({err}).")),
        Ok(Some(nested)) => {
            return Some(format!(
                "the worktree contains another git repository ({nested}), so Marv didn't commit it automatically."
            ));
        }
        Ok(None) => {}
    }
    let (ok, status) = run(&["status", "--porcelain"]).await;
    if !ok {
        return Some(format!("couldn't read the changes ({status})."));
    }
    if status.is_empty() {
        return None;
    }
    let (ok, out) = run(&["add", "-A"]).await;
    if !ok {
        return Some(format!("couldn't stage the changes ({out})."));
    }
    // Unsigned, even if the repository signs commits: the full-screen UI can't host a pinentry prompt, and every
    // parallel subagent would wait on it. The parent's merge can sign.
    let (ok, out) = git(&wt.dir, &["-c", "commit.gpgSign=false", "commit", "-q", "-m", message], &pinned).await;
    if !ok {
        return Some(format!("couldn't commit the changes ({out})."));
    }
    if !run(&["status", "--porcelain"]).await.1.is_empty() {
        return Some("some changes couldn't be committed.".into());
    }
    None
}

/// Deletes a folder (asynchronously: a node_modules can take seconds); false if it couldn't. A missing one is gone.
async fn remove(path: &Path) -> bool {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => true,
        Err(err) => err.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Commits the subagent's changes, removes the worktree and keeps the branch (or deletes it if nothing was
/// committed). Returns the line the parent agent reads. If anything can't be committed, the folder is kept and its
/// path reported, so no work is lost. Never fails: every problem becomes the line.
pub async fn finish_worktree(wt: &Worktree, description: &str, interrupted: bool) -> String {
    let keep = |why: String| format!("Branch {}: {why} The changes are still in {}.", wt.branch, wt.dir.display());
    let message = format!("marv: {description}{}", if interrupted { " (interrupted)" } else { "" });
    if let Some(problem) = commit_changes(wt, &message).await {
        return keep(problem);
    }
    let range = format!("{}..{}", wt.base, wt.branch);
    let (ok, out) = git(&wt.repo, &["rev-list", "--count", &range], &[]).await;
    if !ok {
        return keep(format!("couldn't count its commits ({out})."));
    }
    let count: u64 = out.parse().unwrap_or(0);
    // Delete the folder and this worktree's own record directly. `git worktree remove` would read inside the
    // worktree, and `git worktree prune` would also drop the user's worktrees whose folders are missing right now.
    // Everything is committed by now, so a folder that can't be removed (e.g. one the subagent made read-only) only
    // needs mentioning.
    let (dir_removed, admin_removed) = tokio::join!(remove(&wt.dir), remove(&wt.admin_dir));
    let note: String = [(dir_removed, &wt.dir), (admin_removed, &wt.admin_dir)]
        .into_iter()
        .filter(|(removed, _)| !removed)
        .map(|(_, path)| format!(" The folder couldn't be removed: {}.", path.display()))
        .collect();
    if count == 0 {
        git(&wt.repo, &["branch", "-D", &wt.branch], &[]).await;
        return format!("No changes (branch {} removed).{note}", wt.branch);
    }
    format!(
        "Branch {}: {count} commit{} on {}. Review it with `git diff {}...{}`, then merge it.{note}",
        wt.branch,
        if count == 1 { "" } else { "s" },
        wt.base,
        wt.base,
        wt.branch
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Runs git for a test (not Marv's git: hooks and config apply), stdout trimmed.
    pub(crate) fn sh_git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").args(args).current_dir(cwd).output().expect("git runs");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A temp folder, deleted on drop (after making everything in it writable again).
    pub(crate) struct Temp(pub PathBuf);
    impl Temp {
        pub(crate) fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("marv-{tag}-{}-{}", std::process::id(), marv_fs::random_hex()));
            std::fs::create_dir_all(&dir).unwrap();
            // The real path: on macOS the temp folder is under /var, a link to /private/var, and git and the code
            // under test report the resolved one.
            Self(dir.canonicalize().unwrap())
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::process::Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.0).output();
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A repository with one commit (`a.txt`) on `main`.
    pub(crate) fn init_repo(repo: &Path) {
        sh_git(repo, &["init", "-q", "-b", "main"]);
        sh_git(repo, &["config", "user.email", "test@example.com"]);
        sh_git(repo, &["config", "user.name", "Test"]);
        sh_git(repo, &["config", "commit.gpgSign", "false"]);
        if !repo.join("a.txt").exists() {
            std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        }
        sh_git(repo, &["add", "-A"]);
        sh_git(repo, &["commit", "-q", "-m", "first"]);
    }

    fn setup() -> (Temp, Temp) {
        let (repo, trees) = (Temp::new("wt-repo"), Temp::new("wt-trees"));
        init_repo(&repo.0);
        (repo, trees)
    }

    fn write(path: impl AsRef<Path>, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    fn is_id_branch(branch: &str, slug: &str) -> bool {
        branch.strip_prefix(&format!("marv/{slug}-")).is_some_and(|id| id.len() == 4 && id.chars().all(|c| c.is_ascii_hexdigit()))
    }

    // Ported from tests/worktree.test.ts.

    #[tokio::test]
    async fn inspect_repo_the_base_commit_and_how_many_uncommitted_changes_would_be_left_behind() {
        let (repo, trees) = setup();
        let base = sh_git(&repo.0, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(inspect_repo(&repo.0).await, Some(RepoState { base, dirty: 0, prefix: "".into() }));
        write(repo.0.join("b.txt"), "new\n");
        assert_eq!(inspect_repo(&repo.0).await.unwrap().dirty, 1);
        assert_eq!(inspect_repo(&trees.0).await, None, "None outside a git repo");
    }

    #[test]
    fn branch_names_are_slugs() {
        assert_eq!(branch_name("Task 2: Parser errors!", "ab12"), "marv/task-2-parser-errors-ab12");
        assert_eq!(branch_name("!!!", "ab12"), "marv/task-ab12");
        assert_eq!(branch_name(&"x".repeat(80), "ab12"), format!("marv/{}-ab12", "x".repeat(40)));
        let long = branch_name(&"word ".repeat(20), "x");
        assert!(!long.contains("--") && long.len() <= "marv/".len() + 40 + 2);
    }

    #[tokio::test]
    async fn a_new_branch_in_its_own_folder_sharing_the_repos_git() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Fix parser").await.unwrap();
        assert!(is_id_branch(&wt.branch, "fix-parser"), "{}", wt.branch);
        assert!(wt.dir.starts_with(&trees.0));
        assert!(wt.dir.join("a.txt").exists());
        assert!(sh_git(&repo.0, &["branch", "--list", &wt.branch]).contains(&wt.branch));
        assert_eq!(wt.git_dir, repo.0.join(".git"));
        assert!(wt.admin_dir.starts_with(repo.0.join(".git").join("worktrees")));
        assert_eq!(wt.base, sh_git(&repo.0, &["rev-parse", "--short", "HEAD"]));
    }

    #[tokio::test]
    async fn from_a_subfolder_it_works_in_the_same_subfolder_and_the_commit_covers_the_whole_worktree() {
        let (repo, trees) = setup();
        std::fs::create_dir(repo.0.join("pkg")).unwrap();
        write(repo.0.join("pkg/p.txt"), "p\n");
        sh_git(&repo.0, &["add", "-A"]);
        sh_git(&repo.0, &["commit", "-q", "-m", "pkg"]);
        let wt = create_worktree(&repo.0.join("pkg"), &trees.0, "Sub").await.unwrap();
        assert_eq!(wt.prefix, "pkg/");
        assert_eq!(wt.work_dir, wt.dir.join("pkg"));
        assert!(!wt.work_dir.to_string_lossy().ends_with('/'));
        assert!(wt.work_dir.join("p.txt").exists());
        write(wt.work_dir.join("new.txt"), "n\n");
        assert!(finish_worktree(&wt, "Sub", false).await.contains("1 commit"));
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:pkg/new.txt", wt.branch)]), "n");
        assert!(!wt.dir.exists());
    }

    #[tokio::test]
    async fn from_an_untracked_subfolder_that_folder_is_created_in_the_worktree() {
        let (repo, trees) = setup();
        std::fs::create_dir(repo.0.join("fresh")).unwrap();
        let wt = create_worktree(&repo.0.join("fresh"), &trees.0, "Fresh").await.unwrap();
        assert_eq!(wt.work_dir, wt.dir.join("fresh"));
        assert!(wt.work_dir.exists());
    }

    #[tokio::test]
    async fn at_the_top_of_the_repository_it_works_in_the_worktree_itself() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Top").await.unwrap();
        assert_eq!(wt.prefix, "");
        assert_eq!(wt.work_dir, wt.dir);
    }

    #[tokio::test]
    async fn marvs_own_git_ignores_a_planted_hook_and_redirected_git_pointers() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Sneaky").await.unwrap();
        let pwned = trees.0.join("PWNED");
        // Plant what a subagent might, if it could write there. (In real use the sandbox shows .git read-only; this
        // is the second layer.)
        std::fs::create_dir_all(wt.git_dir.join("hooks")).unwrap();
        let hook = wt.git_dir.join("hooks/pre-commit");
        marv_tools::github::fake::write_executable(&hook, &format!("#!/bin/sh\ntouch {}\n", pwned.display()));
        // The worktree's .git file pointed at a fake repository whose config runs a program.
        let fake = trees.0.join("fake.git");
        sh_git(&trees.0, &["init", "-q", "--bare", &fake.to_string_lossy()]);
        sh_git(&fake, &["config", "core.fsmonitor", &format!("touch {}", pwned.display())]);
        write(wt.dir.join(".git"), &format!("gitdir: {}\n", fake.display()));
        write(wt.admin_dir.join("commondir"), &format!("{}\n", fake.display()));
        write(wt.dir.join("b.txt"), "bee\n");

        finish_worktree(&wt, "Sneaky", false).await;
        assert!(!pwned.exists());
        // Committed to the real branch, not the fake repo.
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:b.txt", wt.branch)]), "bee");
    }

    #[tokio::test]
    async fn a_redirected_head_in_the_worktrees_record_doesnt_move_the_commit_to_another_branch() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Head").await.unwrap();
        write(wt.admin_dir.join("HEAD"), "ref: refs/heads/main\n");
        write(wt.dir.join("b.txt"), "bee\n");
        assert!(finish_worktree(&wt, "Head", false).await.contains("1 commit"));
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:b.txt", wt.branch)]), "bee");
        assert_eq!(sh_git(&repo.0, &["log", "--format=%s", "main"]), "first");
    }

    #[tokio::test]
    async fn per_worktree_config_planted_in_the_worktrees_record_doesnt_run_a_program() {
        let (repo, trees) = setup();
        // A repository that uses per-worktree config reads .git/worktrees/<id>/config.worktree.
        sh_git(&repo.0, &["config", "core.repositoryformatversion", "1"]);
        sh_git(&repo.0, &["config", "extensions.worktreeConfig", "true"]);
        let wt = create_worktree(&repo.0, &trees.0, "Config").await.unwrap();
        let pwned = trees.0.join("PWNED");
        write(wt.admin_dir.join("config.worktree"), &format!("[filter \"x\"]\n\tclean = touch {} && cat\n", pwned.display()));
        write(wt.dir.join(".gitattributes"), "* filter=x\n");
        write(wt.dir.join("b.txt"), "bee\n");
        assert!(finish_worktree(&wt, "Config", false).await.contains("1 commit"));
        assert!(!pwned.exists());
    }

    #[tokio::test]
    async fn a_nested_repository_isnt_committed_automatically() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Nested").await.unwrap();
        std::fs::create_dir_all(wt.dir.join("sub/.git")).unwrap();
        write(wt.dir.join("c.txt"), "c\n");
        let line = finish_worktree(&wt, "Nested", false).await;
        assert!(line.contains("another git repository (sub/.git)"), "{line}");
        assert!(line.contains(&format!("still in {}", wt.dir.display())));
        assert!(wt.dir.join("c.txt").exists());
        assert_eq!(sh_git(&repo.0, &["rev-list", "--count", &format!("main..{}", wt.branch)]), "0");
    }

    #[tokio::test]
    async fn a_repository_inside_an_ignored_folder_doesnt_block_the_commit() {
        let (repo, trees) = setup();
        write(repo.0.join(".gitignore"), "node_modules/\n");
        sh_git(&repo.0, &["add", "-A"]);
        sh_git(&repo.0, &["commit", "-q", "-m", "ignore"]);
        let wt = create_worktree(&repo.0, &trees.0, "Deps").await.unwrap();
        std::fs::create_dir_all(wt.dir.join("node_modules/x/.git")).unwrap();
        write(wt.dir.join("node_modules/x/index.js"), "x\n");
        write(wt.dir.join("c.txt"), "c\n");
        assert!(finish_worktree(&wt, "Deps", false).await.contains("1 commit"));
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:c.txt", wt.branch)]), "c");
        assert!(!sh_git(&repo.0, &["ls-tree", "-r", "--name-only", &wt.branch]).contains("node_modules"));
        assert!(!wt.dir.exists());
    }

    // Marv's "doesn't block the event loop" test: here the walk and the removal are async (tokio::fs) on a
    // current-thread runtime, so a ticking task proves the runtime ran while it worked.
    #[tokio::test]
    async fn finishing_doesnt_block_the_runtime_while_it_deletes_the_folder() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Big").await.unwrap();
        for d in 0..40 {
            let folder = wt.dir.join(format!("node_modules/p{d}"));
            std::fs::create_dir_all(&folder).unwrap();
            for f in 0..50 {
                write(folder.join(format!("f{f}.js")), "x");
            }
        }
        write(wt.dir.join(".gitignore"), "node_modules/\n");
        let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ticker = tokio::spawn({
            let ticks = ticks.clone();
            async move {
                loop {
                    ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tokio::task::yield_now().await;
                }
            }
        });
        let line = finish_worktree(&wt, "Big", false).await;
        ticker.abort();
        assert!(line.contains("1 commit"), "{line}");
        assert!(ticks.load(std::sync::atomic::Ordering::Relaxed) > 0, "the runtime ran while it worked");
        assert!(!wt.dir.exists());
    }

    #[tokio::test]
    async fn the_search_for_nested_repositories_doesnt_follow_symlinks_out_of_the_worktree() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Links").await.unwrap();
        std::fs::create_dir(wt.dir.join("d")).unwrap();
        std::os::unix::fs::symlink("..", wt.dir.join("d/loop")).unwrap();
        // A folder with a .git inside, but not part of the worktree.
        std::os::unix::fs::symlink(&repo.0, wt.dir.join("elsewhere")).unwrap();
        assert!(finish_worktree(&wt, "Links", false).await.contains("1 commit"));
        // Committed as a link.
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:elsewhere", wt.branch)]), repo.0.to_string_lossy());
    }

    fn is_root() -> bool {
        std::process::Command::new("id").arg("-u").output().is_ok_and(|o| o.stdout.starts_with(b"0\n"))
    }

    #[tokio::test]
    async fn a_folder_marv_cant_read_keeps_the_worktree_and_reports_it() {
        if is_root() {
            return; // root reads it anyway
        }
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Locked").await.unwrap();
        let locked = wt.dir.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let line = finish_worktree(&wt, "Locked", false).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(line.contains(&format!("still in {}", wt.dir.display())), "{line}");
        assert!(wt.dir.exists());
    }

    #[tokio::test]
    async fn only_this_worktrees_record_is_removed_not_the_users_other_worktrees() {
        let (repo, trees) = setup();
        let other = trees.0.join("users-own");
        sh_git(&repo.0, &["worktree", "add", "-q", "-b", "mine", &other.to_string_lossy()]);
        std::fs::remove_dir_all(&other).unwrap(); // e.g. on a drive that isn't mounted right now
        let wt = create_worktree(&repo.0, &trees.0, "Tidy").await.unwrap();
        finish_worktree(&wt, "Tidy", false).await;
        let list = sh_git(&repo.0, &["worktree", "list"]);
        assert!(list.contains(&*other.to_string_lossy()));
        assert!(!list.contains(&*wt.dir.to_string_lossy()));
    }

    // Marv's "inherited GIT_* variables" test sets GIT_INDEX_FILE in the process; here run_git takes the outer
    // environment as a parameter (git::git_environment, tested in git.rs), so it's tested there without touching the
    // test process's environment.

    #[tokio::test]
    async fn leftover_changes_are_committed_the_folder_removed_the_branch_kept() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Add b").await.unwrap();
        write(wt.dir.join("b.txt"), "bee\n");
        let line = finish_worktree(&wt, "Add b", false).await;
        assert!(line.contains(&format!("Branch {}: 1 commit on {}", wt.branch, wt.base)), "{line}");
        assert!(!wt.dir.exists());
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:b.txt", wt.branch)]), "bee");
        assert_eq!(sh_git(&repo.0, &["log", "-1", "--format=%s", &wt.branch]), "marv: Add b");
        assert!(!repo.0.join("b.txt").exists(), "the main checkout is untouched");
    }

    #[tokio::test]
    async fn tracked_files_that_match_gitignore_stay_on_the_branch() {
        let (repo, trees) = setup();
        write(repo.0.join(".gitignore"), "*.env\n");
        write(repo.0.join("keep.env"), "secret=1\n");
        sh_git(&repo.0, &["add", "-f", ".gitignore", "keep.env"]);
        sh_git(&repo.0, &["commit", "-q", "-m", "tracked but ignored"]);
        let wt = create_worktree(&repo.0, &trees.0, "Ignored").await.unwrap();
        write(wt.dir.join("a.txt"), "two\n");
        assert!(finish_worktree(&wt, "Ignored", false).await.contains("1 commit"));
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:keep.env", wt.branch)]), "secret=1");
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:a.txt", wt.branch)]), "two");
    }

    #[tokio::test]
    async fn without_the_worktrees_index_nothing_is_committed_and_the_folder_is_kept() {
        // Staging from an empty index would silently drop tracked files that match .gitignore.
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "No index").await.unwrap();
        std::fs::remove_file(wt.admin_dir.join("index")).unwrap();
        write(wt.dir.join("b.txt"), "bee\n");
        let line = finish_worktree(&wt, "No index", false).await;
        assert!(line.contains(&format!("still in {}", wt.dir.display())), "{line}");
        assert!(wt.dir.join("b.txt").exists());
        assert_eq!(sh_git(&repo.0, &["rev-list", "--count", &format!("main..{}", wt.branch)]), "0");
    }

    #[tokio::test]
    async fn a_repository_that_signs_commits_gets_an_unsigned_commit_so_nothing_prompts_or_hangs() {
        let (repo, trees) = setup();
        let gpg = trees.0.join("gpg.sh");
        marv_tools::github::fake::write_executable(&gpg, "#!/bin/sh\nexit 1\n");
        sh_git(&repo.0, &["config", "commit.gpgSign", "true"]);
        sh_git(&repo.0, &["config", "gpg.program", &gpg.to_string_lossy()]);
        let wt = create_worktree(&repo.0, &trees.0, "Signed").await.unwrap();
        write(wt.dir.join("b.txt"), "bee\n");
        assert!(finish_worktree(&wt, "Signed", false).await.contains("1 commit"));
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:b.txt", wt.branch)]), "bee");
    }

    #[tokio::test]
    async fn a_folder_that_cant_be_removed_after_the_commit_still_gets_the_branch_line() {
        if is_root() {
            return;
        }
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Stuck").await.unwrap();
        let ro = wt.dir.join("ro");
        std::fs::create_dir(&ro).unwrap();
        write(ro.join("f.txt"), "f\n");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let line = finish_worktree(&wt, "Stuck", false).await;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(line.contains(&format!("Branch {}: 1 commit on {}", wt.branch, wt.base)), "{line}");
        assert!(line.contains(&format!("The folder couldn't be removed: {}.", wt.dir.display())), "{line}");
        assert_eq!(sh_git(&repo.0, &["show", &format!("{}:ro/f.txt", wt.branch)]), "f");
    }

    #[tokio::test]
    async fn an_interrupted_subagents_work_is_committed_and_marked() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Half done").await.unwrap();
        write(wt.dir.join("c.txt"), "c\n");
        finish_worktree(&wt, "Half done", true).await;
        assert_eq!(sh_git(&repo.0, &["log", "-1", "--format=%s", &wt.branch]), "marv: Half done (interrupted)");
    }

    #[tokio::test]
    async fn no_changes_deletes_the_branch_too() {
        let (repo, trees) = setup();
        let wt = create_worktree(&repo.0, &trees.0, "Nothing").await.unwrap();
        assert_eq!(finish_worktree(&wt, "Nothing", false).await, format!("No changes (branch {} removed).", wt.branch));
        assert_eq!(sh_git(&repo.0, &["branch", "--list", &wt.branch]), "");
    }

    #[tokio::test]
    async fn outside_a_git_repo_a_tool_error_the_model_can_act_on() {
        let trees = Temp::new("wt-none");
        let err = create_worktree(&trees.0, &trees.0, "x").await.unwrap_err();
        assert_eq!(err.0, NOT_A_REPO);
    }

    #[test]
    fn worktree_env_pins_git_to_the_record_never_the_git_file() {
        let wt = Worktree {
            dir: "/trees/x".into(),
            branch: "marv/x".into(),
            base: "abc".into(),
            repo: "/proj".into(),
            git_dir: "/proj/.git".into(),
            admin_dir: "/proj/.git/worktrees/x".into(),
            prefix: "".into(),
            work_dir: "/trees/x".into(),
        };
        let env = worktree_env(&wt);
        assert_eq!(env[0], ("GIT_DIR".into(), "/proj/.git/worktrees/x".into()));
        assert_eq!(env[2], ("GIT_WORK_TREE".into(), "/trees/x".into()));
    }

    use std::sync::Arc;
}

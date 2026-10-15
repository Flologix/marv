//! The environment of what runs on the host, outside the sandbox: bash with `sandbox: false` (Marv #52) and the
//! github tool's gh (#47). They need the user's own environment (systemctl --user needs XDG_RUNTIME_DIR and D-Bus,
//! ssh its agent, a tool under the home folder its config), but what they print reaches the model, the transcript
//! and the trajectory, so Marv's keys and anything that looks like a secret stay behind.
//!
//! Ports `secretVariable` and `hostEnv` from Marv's `src/tools/bash.ts` (Sprint 4, the TS review's fix: host commands'
//! secret variables are stripped).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

/// Names that hold a secret: tokens, passwords, API and access keys, credentials (SSH_AUTH_SOCK is a socket: kept).
static SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|_)(?:TOKEN|SECRET|PASSWORD|PASSWD|PASS|PWD|API_?KEY|KEY|KEY_ID|PRIVATE_KEY|CREDENTIALS?)(?:_|$)|PASSWORD",
    )
    .unwrap()
});
/// A URL with a password in it (postgres://user:pass@host).
static CREDENTIAL_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://[^/\s:@]+:[^/\s@]+@").unwrap());

/// Whether a variable must stay out of a command that runs outside the sandbox: Marv's own (MARV_*, the provider
/// key) and anything that looks like a secret.
pub fn secret_variable(name: &str, value: &str) -> bool {
    name == "OPENROUTER_API_KEY"
        || name.starts_with("MARV_")
        || (name != "PWD" && name != "OLDPWD" && SECRET_NAME.is_match(name))
        || CREDENTIAL_URL.is_match(value)
}

/// The environment of a command on the host: `env` (the user's own) minus Marv's keys and anything secret.
pub fn host_env(env: impl IntoIterator<Item = (String, String)>) -> BTreeMap<String, String> {
    env.into_iter().filter(|(name, value)| !secret_variable(name, value)).collect()
}

/// [`host_env`] of this process's environment (variables that aren't UTF-8 are left out).
pub fn host_env_now() -> BTreeMap<String, String> {
    host_env(std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))))
}

/// The program a door runs on the host (gh, herdr), found on `path` the safe way: only absolute PATH entries (a
/// relative one, like `node_modules/.bin` or `.`, means the folder Marv runs in), and never a program whose real path
/// is inside one of `outside` (the project, the sandbox's other writable folders), where a sandboxed command could
/// have put it. Such a candidate is skipped, not run. Returns the program's real path.
pub fn host_program(name: &str, path: Option<&str>, outside: &[&std::path::Path]) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let outside: Vec<std::path::PathBuf> = outside.iter().filter_map(|d| std::fs::canonicalize(d).ok()).collect();
    path?.split(':').filter(|d| d.starts_with('/')).map(|d| std::path::Path::new(d).join(name)).find_map(|candidate| {
        let real = std::fs::canonicalize(&candidate).ok()?;
        let runnable = std::fs::metadata(&real).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
        (runnable && !outside.iter().any(|d| real.starts_with(d))).then_some(real)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Marv's `tests/host-env.test.ts` environment.
    fn env() -> Vec<(String, String)> {
        [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("SSH_AUTH_SOCK", "/run/user/1000/ssh"),
            ("OPENROUTER_API_KEY", "sk-or"),
            ("MARV_MODEL", "m"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("GITHUB_TOKEN", "ghp_x"),
            ("GH_TOKEN", "ghp_y"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("AWS_ACCESS_KEY_ID", "AKIA"),
            ("NPM_TOKEN", "npm"),
            ("PGPASSWORD", "pg"),
            ("DB_PASSWORD", "db"),
            ("STRIPE_KEY", "sk_live"),
            ("CLIENT_SECRET", "cs"),
            ("GOOGLE_APPLICATION_CREDENTIALS", "/k.json"),
            ("DATABASE_URL", "postgres://user:hunter2@db:5432/app"),
            ("REDIS_URL", "redis://localhost:6379"),
            ("KEYBOARD_LAYOUT", "us"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn keeps_what_host_tools_need_and_drops_secrets() {
        let kept: Vec<String> = host_env(env()).into_keys().collect();
        assert_eq!(
            kept,
            ["DBUS_SESSION_BUS_ADDRESS", "HOME", "KEYBOARD_LAYOUT", "PATH", "REDIS_URL", "SSH_AUTH_SOCK", "XDG_RUNTIME_DIR"]
        );
    }

    #[test]
    fn pwd_is_a_folder_not_a_password() {
        assert!(!secret_variable("PWD", "/home/u/p"));
        assert!(!secret_variable("OLDPWD", "/home/u"));
        assert!(secret_variable("DB_PWD", "x"));
    }

    #[test]
    fn a_door_program_comes_from_an_absolute_path_entry_outside_the_project() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("marv-host-program-{}", std::process::id()));
        let (project, bin) = (base.join("project"), base.join("bin"));
        for dir in [project.join("node_modules/.bin"), bin.clone()] {
            std::fs::create_dir_all(&dir).unwrap();
            let tool = dir.join("tool");
            std::fs::write(&tool, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let real_bin = std::fs::canonicalize(&bin).unwrap();
        let planted = project.join("node_modules/.bin");
        // A relative entry is never used, and an absolute one inside the project is skipped for the next.
        let path = format!("node_modules/.bin:.:{}:{}", planted.display(), bin.display());
        assert_eq!(host_program("tool", Some(&path), &[&project]), Some(real_bin.join("tool")));
        assert_eq!(host_program("tool", Some("node_modules/.bin:."), &[&project]), None);
        assert_eq!(host_program("tool", Some(&planted.to_string_lossy()), &[&project]), None);
        // A link from outside into the project counts as the project's.
        let linked = base.join("lnk");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(planted.join("tool"), linked.join("tool")).unwrap();
        assert_eq!(host_program("tool", Some(&linked.to_string_lossy()), &[&project]), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}

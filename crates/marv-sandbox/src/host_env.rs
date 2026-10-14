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
    Regex::new(r"(?i)(?:^|_)(?:TOKEN|SECRET|PASSWORD|PASSWD|PASS|PWD|API_?KEY|KEY|KEY_ID|PRIVATE_KEY|CREDENTIALS?)(?:_|$)|PASSWORD")
        .unwrap()
});
/// A URL with a password in it (postgres://user:pass@host).
static CREDENTIAL_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://[^/\s:@]+:[^/\s@]+@").unwrap());

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
}

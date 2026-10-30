//! Update notices and `marv update` (marv-rs only, not in Marv).
//!
//! - **The check:** at most once a day, the TUI asks npm's registry for the newest published version
//!   ([`LATEST_URL`]) in the background, and keeps the answer in `<cache>/marv/update-check.json`. A newer version
//!   than this one shows one line in the transcript, with the command that updates this install. It never delays
//!   startup, says nothing when the network is down, and never runs in `marv -p` (its stdout is a script's).
//! - **What it sends:** an unauthenticated GET for a public version number, with `marv/<version>` as the User-Agent.
//!   No identifier, no usage data. npm sees the request and the IP, like any `npm view`.
//! - **What it shows:** only a version that parses as `x.y.z` ([`Version::parse`]). Nothing else from the response
//!   reaches the screen, so the registry can't put text (or escape sequences) in front of the person.
//! - **Off:** `"updateCheck": false` in `~/.marv/config.json`, `MARV_NO_UPDATE_CHECK=1`, `NO_UPDATE_NOTIFIER` (the
//!   convention npm's tools share) or `CI` in the environment.
//! - **`marv update`:** runs the update for how this binary was installed ([`Install`]): `npm install -g` for the npm
//!   package, the release's `install.sh` (which checks the binary's SHA-256) with the same prefix for an install.sh
//!   install; for a build from source it says how. Never automatic: Marv doesn't replace itself behind your back.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use marv_session::config::Env;

/// This build's version.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// npm's record of the launcher package's newest version (`dist-tags.latest`): a small JSON object with `version`.
pub const LATEST_URL: &str = "https://registry.npmjs.org/@flologixai/marv/latest";

/// The newest release's installer (install.sh is a release asset with no version in its name).
pub const INSTALL_SH_URL: &str = "https://github.com/Flologix/marv/releases/latest/download/install.sh";

/// The npm package.
pub const NPM_PACKAGE: &str = "@flologixai/marv";

/// How often the TUI asks.
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a check may take before it's given up (the TUI never waits for it).
const TIMEOUT: Duration = Duration::from_secs(5);

/// The cache file's name, in Marv's cache folder.
const CACHE_FILE: &str = "update-check.json";

/// A release version, `major.minor.patch`. Anything else (a pre-release, a build tag, text) doesn't parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let mut next = || -> Option<u64> {
            let part = parts.next()?;
            // Digits only: no sign, no space, no leading zero but "0" itself, at most 9 digits.
            let plain = !part.is_empty()
                && part.len() <= 9
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'));
            if plain { part.parse().ok() } else { None }
        };
        let version = Version(next()?, next()?, next()?);
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// This build's version, parsed.
pub fn current() -> Version {
    Version::parse(CURRENT).expect("the crate's version is x.y.z")
}

/// Whether the TUI may check: the config's `updateCheck` and the environment's opt-outs.
pub fn enabled(config_on: bool, env: &Env) -> bool {
    let set = |name: &str| env.get(name).is_some_and(|v| !v.trim().is_empty());
    let no_check = env.get("MARV_NO_UPDATE_CHECK").is_some_and(|v| !matches!(v.trim(), "" | "0" | "false"));
    config_on && !no_check && !set("NO_UPDATE_NOTIFIER") && !set("CI")
}

/// Marv's cache folder: `~/Library/Caches/marv` on macOS; `$XDG_CACHE_HOME/marv` (when it's an absolute path) or
/// `~/.cache/marv` elsewhere. The check cache (`diagnostics::cache`) lives in its `checks` folder.
pub fn cache_dir(env: &Env, home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        return home.join("Library/Caches/marv");
    }
    let xdg = env.get("XDG_CACHE_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    xdg.unwrap_or_else(|| home.join(".cache")).join("marv")
}

/// What the last check found, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checked {
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// The newest version then, or `None` when that check failed.
    pub latest: Option<Version>,
}

impl Checked {
    /// Read from the cache file; anything unreadable is no record (the check runs again).
    pub fn load(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join(CACHE_FILE)).ok()?;
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        let at = value.get("checkedAt")?.as_u64()?;
        let latest = value.get("latest").and_then(|v| v.as_str()).and_then(Version::parse);
        Some(Self { at, latest })
    }

    /// Written beside and renamed over, so a reader never sees half a file. Failures are ignored: the check just runs
    /// again next time.
    pub fn save(&self, dir: &Path) {
        let value = serde_json::json!({ "checkedAt": self.at, "latest": self.latest.map(|v| v.to_string()) });
        let _ = std::fs::create_dir_all(dir);
        let tmp = dir.join(format!("{CACHE_FILE}.{}", std::process::id()));
        if std::fs::write(&tmp, value.to_string()).is_ok() && std::fs::rename(&tmp, dir.join(CACHE_FILE)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Whether it's recent enough not to ask again. A clock set back (`at` in the future) counts as stale.
    pub fn fresh(&self, now: u64) -> bool {
        now >= self.at && now - self.at < CHECK_EVERY.as_secs()
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The newest version npm has, from `url` (a JSON object with `version`). `None` on any failure.
pub async fn fetch_latest(url: &str) -> Option<Version> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(format!("marv/{CURRENT}"))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .ok()?;
    let response = client.get(url).header("Accept", "application/json").send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    // The record is a few kB; anything far bigger isn't it.
    let body = response.bytes().await.ok()?;
    if body.len() > 1 << 20 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&body).ok()?;
    Version::parse(value.get("version")?.as_str()?)
}

/// The newest version, from the cache when it's fresh, else from `url` (and cached, a failure too, so a machine
/// offline doesn't ask at every start).
pub async fn latest_cached(dir: &Path, url: &str, now: u64) -> Option<Version> {
    if let Some(checked) = Checked::load(dir).filter(|c| c.fresh(now)) {
        return checked.latest;
    }
    let latest = fetch_latest(url).await;
    Checked { at: now, latest }.save(dir);
    latest
}

/// How this binary was installed, from where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// The npm package: the binary is inside a `node_modules` folder.
    Npm,
    /// install.sh: the binary is `<prefix>/bin/marv`.
    Script { prefix: PathBuf },
    /// Anything else, a build from source (`target/release/marv`) among them.
    Other { exe: PathBuf },
}

impl Install {
    pub fn detect(exe: &Path) -> Self {
        if exe.components().any(|c| c.as_os_str() == "node_modules") {
            return Install::Npm;
        }
        let in_target = exe.components().any(|c| c.as_os_str() == "target");
        match exe.parent() {
            Some(bin)
                if !in_target && bin.file_name().is_some_and(|n| n == "bin") && exe.file_name().is_some_and(|n| n == "marv") =>
            {
                Install::Script { prefix: bin.parent().unwrap_or(Path::new("/")).to_path_buf() }
            }
            _ => Install::Other { exe: exe.to_path_buf() },
        }
    }

    /// This process's install.
    pub fn current() -> Self {
        let exe = std::env::current_exe().and_then(std::fs::canonicalize).unwrap_or_default();
        Self::detect(&exe)
    }
}

/// The transcript's line when `latest` is newer than `current`.
pub fn notice(current: Version, latest: Version, install: &Install) -> Option<String> {
    if latest <= current {
        return None;
    }
    let how = match install {
        Install::Npm | Install::Script { .. } => "run `marv update` to update".to_string(),
        Install::Other { .. } => "see github.com/Flologix/marv/releases".to_string(),
    };
    Some(format!("✻ Marv {latest} is out (you have {current}): {how}."))
}

/// The TUI's check: the line to show, if any. `None` when it's off, up to date, or the check failed.
pub async fn startup_notice(config_on: bool, env: &Env, home: &Path) -> Option<String> {
    if !enabled(config_on, env) {
        return None;
    }
    let latest = latest_cached(&cache_dir(env, home), LATEST_URL, now()).await?;
    notice(current(), latest, &Install::current())
}

/// `marv update [--check]`: 0 up to date or updated, 1 a failure (or, with `--check`, an update is out).
pub async fn run(check_only: bool) -> i32 {
    let env: Env = std::env::vars().collect();
    let home = marv_session::paths::home_dir();
    let current = current();
    let Some(latest) = fetch_latest(LATEST_URL).await else {
        eprintln!("marv update: couldn't reach npm's registry to find the newest version. Check your connection.");
        return 1;
    };
    Checked { at: now(), latest: Some(latest) }.save(&cache_dir(&env, &home));
    if latest <= current {
        println!("Marv {current} is the newest version.");
        return 0;
    }
    let install = Install::current();
    if check_only {
        println!("Marv {latest} is out (you have {current}). Run `marv update` to update.");
        return 1;
    }
    println!("Updating Marv {current} → {latest}…");
    match install {
        Install::Npm => {
            let spec = format!("{NPM_PACKAGE}@{latest}");
            let status = std::process::Command::new("npm").args(["install", "-g", &spec]).status();
            finish(status, &format!("npm install -g {spec}"))
        }
        Install::Script { prefix } => match download_installer().await {
            Ok(script) => {
                let status = std::process::Command::new("sh")
                    .arg(&script)
                    .arg("--prefix")
                    .arg(&prefix)
                    .arg("--version")
                    .arg(format!("v{latest}"))
                    .status();
                let _ = std::fs::remove_file(&script);
                finish(status, &format!("curl -fsSL {INSTALL_SH_URL} | sh -s -- --prefix {}", prefix.display()))
            }
            Err(e) => {
                eprintln!("marv update: couldn't download the installer: {e}");
                1
            }
        },
        Install::Other { exe } => {
            println!(
                "This marv ({}) wasn't installed with npm or install.sh, so Marv can't update it. Rebuild it from \
                 source, or install the release: npm install -g {NPM_PACKAGE}",
                marv_core::printable(&exe.display().to_string())
            );
            1
        }
    }
}

/// The updater's exit, with the command to run by hand when it couldn't start.
fn finish(status: std::io::Result<std::process::ExitStatus>, by_hand: &str) -> i32 {
    match status {
        Ok(s) if s.success() => {
            println!("Updated. Start marv again to use the new version.");
            0
        }
        Ok(_) => {
            eprintln!("marv update: the update failed (see above). To try by hand: {by_hand}");
            1
        }
        Err(e) => {
            eprintln!("marv update: couldn't run the update ({e}). Run it by hand: {by_hand}");
            1
        }
    }
}

/// The newest install.sh, saved to a private temporary file.
async fn download_installer() -> Result<PathBuf, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("marv/{CURRENT}"))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(INSTALL_SH_URL).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let body = response.bytes().await.map_err(|e| e.to_string())?;
    if !body.starts_with(b"#!/bin/sh") {
        return Err("that isn't Marv's installer".into());
    }
    let file = tempfile::Builder::new().prefix("marv-install-").suffix(".sh").tempfile().map_err(|e| e.to_string())?;
    std::fs::write(file.path(), &body).map_err(|e| e.to_string())?;
    file.into_temp_path().keep().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn only_a_plain_release_version_parses_and_they_compare_by_number() {
        assert_eq!(Version::parse("0.14.0"), Some(Version(0, 14, 0)));
        assert!(Version::parse("0.10.0") > Version::parse("0.9.9"));
        assert!(Version::parse("1.0.0") > Version::parse("0.99.99"));
        for bad in ["", "0.14", "0.14.0.1", "0.14.0-rc.1", "v0.14.0", " 0.14.0", "0.014.0", "+1.0.0", "1.0.x", "0.14.0\x1b[2J"] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert_eq!(current().to_string(), CURRENT);
    }

    #[test]
    fn the_check_is_on_unless_the_config_or_the_environment_turns_it_off() {
        assert!(enabled(true, &env(&[])));
        assert!(enabled(true, &env(&[("MARV_NO_UPDATE_CHECK", "0"), ("CI", "")])));
        assert!(!enabled(false, &env(&[])));
        for off in [("MARV_NO_UPDATE_CHECK", "1"), ("NO_UPDATE_NOTIFIER", "1"), ("CI", "true")] {
            assert!(!enabled(true, &env(&[off])), "{off:?}");
        }
    }

    #[test]
    fn the_install_is_told_by_where_the_binary_is() {
        let npm = "/home/me/.local/share/mise/installs/node/26/lib/node_modules/@flologixai/marv-linux-x64/bin/marv";
        assert_eq!(Install::detect(Path::new(npm)), Install::Npm);
        assert_eq!(Install::detect(Path::new("/home/me/.local/bin/marv")), Install::Script { prefix: "/home/me/.local".into() });
        for other in ["/home/me/src/marv/target/release/marv", "/opt/marv", "/home/me/.local/bin/marv-dev"] {
            assert!(matches!(Install::detect(Path::new(other)), Install::Other { .. }), "{other}");
        }
    }

    #[test]
    fn the_notice_shows_only_for_a_newer_version_with_the_way_to_update() {
        let v = |s| Version::parse(s).unwrap();
        assert_eq!(notice(v("0.13.0"), v("0.13.0"), &Install::Npm), None);
        assert_eq!(notice(v("0.14.0"), v("0.13.0"), &Install::Npm), None);
        assert_eq!(
            notice(v("0.13.0"), v("0.14.0"), &Install::Npm).as_deref(),
            Some("✻ Marv 0.14.0 is out (you have 0.13.0): run `marv update` to update.")
        );
        let source = notice(v("0.13.0"), v("0.14.0"), &Install::Other { exe: "/x/target/release/marv".into() });
        assert!(source.unwrap().contains("github.com/Flologix/marv/releases"));
    }

    #[test]
    fn the_cache_round_trips_and_goes_stale_after_a_day() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Checked::load(dir.path()), None);
        let checked = Checked { at: 1_000, latest: Version::parse("0.14.0") };
        checked.save(dir.path());
        assert_eq!(Checked::load(dir.path()), Some(checked));
        assert!(checked.fresh(1_000) && checked.fresh(1_000 + CHECK_EVERY.as_secs() - 1));
        assert!(!checked.fresh(1_000 + CHECK_EVERY.as_secs()) && !checked.fresh(999), "a day later, or the clock set back");
        std::fs::write(dir.path().join(CACHE_FILE), "{not json").unwrap();
        assert_eq!(Checked::load(dir.path()), None);
    }

    #[test]
    fn the_cache_folder_follows_the_platform() {
        let home = Path::new("/home/me");
        if cfg!(target_os = "macos") {
            assert_eq!(cache_dir(&env(&[]), home), Path::new("/home/me/Library/Caches/marv"));
        } else {
            assert_eq!(cache_dir(&env(&[]), home), Path::new("/home/me/.cache/marv"));
            assert_eq!(cache_dir(&env(&[("XDG_CACHE_HOME", "/xdg")]), home), Path::new("/xdg/marv"));
            assert_eq!(cache_dir(&env(&[("XDG_CACHE_HOME", "rel")]), home), Path::new("/home/me/.cache/marv"));
        }
    }

    /// A one-request HTTP server answering `status` and `body`; its URL.
    async fn serve_once(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });
        format!("http://{addr}/latest")
    }

    #[tokio::test]
    async fn the_registrys_version_is_read_and_anything_else_is_no_answer() {
        assert_eq!(
            fetch_latest(&serve_once("200 OK", r#"{"name":"@flologixai/marv","version":"0.14.0"}"#).await).await,
            Version::parse("0.14.0")
        );
        assert_eq!(fetch_latest(&serve_once("200 OK", r#"{"version":"0.14.0\u001b[31m"}"#).await).await, None);
        assert_eq!(fetch_latest(&serve_once("200 OK", "not json").await).await, None);
        assert_eq!(fetch_latest(&serve_once("404 Not Found", r#"{"version":"9.9.9"}"#).await).await, None);
        assert_eq!(fetch_latest("http://127.0.0.1:1/latest").await, None);
    }

    #[tokio::test]
    async fn a_fresh_cache_answers_without_asking_and_a_failed_check_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve_once("200 OK", r#"{"version":"0.20.0"}"#).await;
        assert_eq!(latest_cached(dir.path(), &url, 5_000).await, Version::parse("0.20.0"));
        // Fresh: the dead URL isn't asked.
        assert_eq!(latest_cached(dir.path(), "http://127.0.0.1:1/latest", 5_001).await, Version::parse("0.20.0"));
        // Stale and offline: no answer, remembered so the next start doesn't ask again.
        let later = 5_000 + CHECK_EVERY.as_secs();
        assert_eq!(latest_cached(dir.path(), "http://127.0.0.1:1/latest", later).await, None);
        assert_eq!(Checked::load(dir.path()), Some(Checked { at: later, latest: None }));
    }
}

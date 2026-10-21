//! `docs/SECURITY.md`, "Sandbox", on macOS: the Seatbelt backend (`sandbox-exec` with `marv __guard`, #166), as Marv
//! spawns it (`Sandbox::prepare`, `run_command`), each guard the macOS twin of a Linux one or a door Linux doesn't have
//! (spec §9.3). They need a working `sandbox-exec`; CI's macOS job sets `MARV_REQUIRE_SANDBOX=1`, so
//! `where_the_sandbox_is_required_it_really_runs` fails rather than let them skip.
//!
//! The fixtures: a fake home (with `.ssh`, Marv's config and skills, a toolchain, a credential and a symlinked one) and a
//! project inside it, in `/private/var/tmp`, which no rule but the home's closes: in the user's temporary folder
//! (`/var/folders/…/T`, which every profile closes whole) a guard would pass with the home's rules removed (#166
//! review). Each refusal has a control: the same command run outside the sandbox gets through.
#![cfg(target_os = "macos")]

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marv_guards::{Ran, TempDir, require_sandbox, run_sandboxed, sandbox_with_home};
use marv_sandbox::git_guard::git_guard;
use marv_sandbox::protected::{protected_snapshot, scan_created};
use marv_sandbox::{CommandMounts, Sandbox};
use marv_tools::bash::{CommandResult, RunOptions, run_command};

/// A fake home with a project in it (see the module doc). The project holds `.envrc` and `pkg/AGENTS.md`.
struct Home {
    dir: TempDir,
}

impl Home {
    fn new(tag: &str) -> Self {
        let dir = TempDir::outside_tmp(tag);
        assert!(dir.path().starts_with("/private/var/tmp"), "the fake home must be outside the user's temporary folder");
        dir.write(".ssh/known_hosts", "SECRET-SSH\n");
        dir.write(".marv/config.json", "{\"key\": \"SECRET-CONFIG\"}\n");
        dir.write(".marv/skills/x/SKILL.md", "a skill\n");
        dir.write(".cargo/bin/tool", "a toolchain file\n");
        dir.write(".cargo/credentials.toml", "SECRET-CARGO\n");
        dir.write(".dotfiles/git-credentials", "SECRET-LINKED\n");
        std::fs::create_dir_all(dir.path().join(".config/git")).unwrap();
        std::os::unix::fs::symlink(dir.path().join(".dotfiles/git-credentials"), dir.path().join(".config/git/credentials"))
            .unwrap();
        dir.write(".zshrc", "# rc\n");
        // A hidden entry no other rule names (not a credential, not in HOME_MASKS): only the home's own rules close it.
        dir.write(".notes/hidden.txt", "SECRET-HIDDEN\n");
        dir.write("proj/.envrc", "export SAFE=1\n");
        dir.write("proj/pkg/AGENTS.md", "safe\n");
        dir.write("proj/src/main.rs", "fn main() {}\n");
        Self { dir }
    }
    fn home(&self) -> &Path {
        self.dir.path()
    }
    fn proj(&self) -> PathBuf {
        self.home().join("proj")
    }
    fn sandbox(&self) -> Sandbox {
        sandbox_with_home(self.home())
    }
    /// `script` in the project's sandbox, as a command that runs without asking (`.git` confined by the git guard).
    fn run(&self, script: &str) -> Ran {
        self.run_with(&self.sandbox(), &self.proj(), false, script)
    }
    fn run_with(&self, sandbox: &Sandbox, root: &Path, network: bool, script: &str) -> Ran {
        let guard = git_guard(root);
        let mounts =
            CommandMounts { network, read_only: guard.read_only, placeholders: guard.placeholders, ..Default::default() };
        run_sandboxed(sandbox, root, &mounts, script, &[])
    }
}

/// `script` outside any sandbox, in `cwd`, for the controls: what the sandbox refuses must work here.
fn host(cwd: &Path, script: &str) -> Ran {
    let out = std::process::Command::new("bash").args(["-c", &format!("exec 2>&1\n{script}")]).current_dir(cwd).output().unwrap();
    Ran { output: String::from_utf8_lossy(&out.stdout).into_owned(), code: out.status.code() }
}

/// The tools the guards' scripts use: a missing one would make a refusal look like a pass.
fn require_tools(tools: &[&str]) {
    for tool in tools {
        let found = std::process::Command::new("bash").args(["-c", &format!("command -v {tool}")]).output().unwrap();
        assert!(found.status.success(), "{tool} is needed by this guard");
    }
}

fn denied(ran: &Ran) -> bool {
    ran.code != Some(0) && (ran.output.contains("Operation not permitted") || ran.output.contains("not permitted"))
}

async fn bash(
    sandbox: &Sandbox,
    root: &Path,
    command: &str,
    timeout: Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> CommandResult {
    let options = RunOptions { command: command.into(), root: root.to_path_buf(), sandbox: true, timeout, ..Default::default() };
    run_command(options, sandbox, cancel).await
}

/// Whether `pid` is gone within 3 s (re-parented to launchd and reaped once killed).
fn gone(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn first_pid(output: &str) -> i32 {
    output.split_whitespace().find_map(|w| w.parse().ok()).unwrap_or_else(|| panic!("no pid in {output:?}"))
}

/// **Mach and XPC doors are closed** (deny-default, the short `mach-lookup` list, no XPC service by any name): no Apple
/// Events to other apps (`osascript` telling Terminal to run something would run it outside), no LaunchServices
/// (`open`), no launchd job, no pasteboard, no keychain.
#[test]
fn the_macos_doors_are_closed() {
    require_sandbox!();
    let home = Home::new("sb-doors");
    for script in [
        "osascript -e 'tell application \"Finder\" to count windows'",
        "osascript -e 'tell application \"System Events\" to get name of first process'",
        "open -a TextEdit",
        "pbpaste && echo pasted",
    ] {
        let ran = home.run(script);
        assert_ne!(ran.code, Some(0), "{script}: {}", ran.output);
    }
    let label = format!("marv-guard-{}", std::process::id());
    let _ = home.run(&format!("launchctl submit -l {label} -- /usr/bin/true"));
    let listed = std::process::Command::new("/bin/launchctl").args(["list", &label]).output().unwrap();
    let _ = std::process::Command::new("/bin/launchctl").args(["remove", &label]).output();
    assert!(!listed.status.success(), "launchctl submit reached launchd");
}

/// **A keychain item can't be read, with or without network**: one that exists outside the sandbox (made in the
/// user's default keychain for this test, removed after), read through `security`. Reading an item needs securityd
/// (`com.apple.SecurityServer`), which no profile lets a command reach, with network or not (#166 review).
///
/// Proven: see the PR's proof comment (`com.apple.SecurityServer` added back to the network services).
#[test]
fn a_keychain_item_cant_be_read_with_or_without_network() {
    require_sandbox!();
    let home = Home::new("sb-keychain");
    let service = format!("marv-guard-{}", std::process::id());
    struct Remove(String);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::process::Command::new("security").args(["delete-generic-password", "-s", &self.0]).output();
        }
    }
    let added = std::process::Command::new("security")
        .args(["add-generic-password", "-a", "marv-guard", "-s", &service, "-w", "SECRET-KEYCHAIN-ITEM"])
        .output()
        .unwrap();
    let _remove = Remove(service.clone());
    let find = format!("security find-generic-password -s {service} -w");
    let control = host(home.home(), &find);
    if !added.status.success() || !control.output.contains("SECRET-KEYCHAIN-ITEM") {
        // The limit, said: without a keychain to put an item in, only the refusal of the keychain itself is checked.
        println!(
            "limit: no keychain item could be made and read back here ({}{}); checking only that the sandbox can't list keychains",
            String::from_utf8_lossy(&added.stderr).trim(),
            control.output.trim()
        );
        for network in [false, true] {
            let ran = home.run_with(&home.sandbox(), &home.proj(), network, "security list-keychains -d user");
            assert!(!ran.output.contains("login.keychain"), "network {network}: {}", ran.output);
        }
        return;
    }
    for network in [false, true] {
        let ran = home.run_with(&home.sandbox(), &home.proj(), network, &find);
        assert!(!ran.output.contains("SECRET-KEYCHAIN-ITEM"), "network {network}: {}", ran.output);
        // A home-folder project too.
        let ran = home.run_with(&home.sandbox(), home.home(), network, &find);
        assert!(!ran.output.contains("SECRET-KEYCHAIN-ITEM"), "home project, network {network}: {}", ran.output);
    }
}

/// **No Unix socket on the machine but the command's own**: the SSH agent's kind (`/private/tmp/…`), one in the
/// user's temporary folder, one in a home-folder project; with or without network (unlike bwrap's shared network
/// namespace, where abstract sockets are reachable).
#[test]
fn no_socket_outside_the_sandbox_is_reachable() {
    require_sandbox!();
    let home = Home::new("sb-sock");
    let shared = TempDir::short("sb-agent");
    let agent = shared.path().join("agent.sock");
    let listeners = [UnixListener::bind(&agent).unwrap(), UnixListener::bind(home.home().join("app.sock")).unwrap()];
    let connect = |path: &Path| {
        format!(
            "python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); print(\"connec\" \"ted\")' '{}'",
            path.display()
        )
    };
    require_tools(&["python3"]);
    for sock in [agent.clone(), home.home().join("app.sock")] {
        let control = host(home.home(), &connect(&sock));
        assert!(control.output.contains("connected"), "control, {}: {}", sock.display(), control.output);
    }
    for network in [false, true] {
        for sock in [agent.clone(), home.home().join("app.sock")] {
            let ran = home.run_with(&home.sandbox(), &home.proj(), network, &connect(&sock));
            assert!(!ran.output.contains("connected"), "{} (network {network}): {}", sock.display(), ran.output);
            // A home-folder project shows the home, and still no socket in it.
            let ran = home.run_with(&home.sandbox(), home.home(), network, &connect(&sock));
            assert!(!ran.output.contains("connected"), "home project, {} (network {network}): {}", sock.display(), ran.output);
        }
    }
    // The command's own sockets, in its private TMPDIR, work.
    let ran = home.run(
        "python3 -c 'import socket,os; p=os.environ[\"TMPDIR\"]+\"/s.sock\"; l=socket.socket(socket.AF_UNIX); l.bind(p); l.listen(1); c=socket.socket(socket.AF_UNIX); c.connect(p); print(\"o\" \"wn\")'",
    );
    assert!(ran.output.contains("own"), "{}", ran.output);
    drop(listeners);
}

/// **Marv's runtime folder can't be listed or connected to, but for the one plug** the command was given, which it
/// finds through `MARV_MESH_SOCKET` (Seatbelt can't mount it at `/run/marv/mesh.sock`).
#[test]
fn the_runtime_folder_is_closed_but_for_the_plug() {
    require_sandbox!();
    require_tools(&["python3"]);
    let home = Home::new("sb-run");
    let runtime = TempDir::short("sb-rt");
    let plug = runtime.path().join("plug.sock");
    let other = runtime.path().join("door.sock");
    let listeners = [UnixListener::bind(&plug).unwrap(), UnixListener::bind(&other).unwrap()];
    let mut sandbox = home.sandbox();
    sandbox.hidden = vec![runtime.path().to_path_buf()];
    sandbox.mesh_socket = Some(plug.clone());
    let connect =
        "python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); print(\"connec\" \"ted\")'";
    let ran = home.run_with(&sandbox, &home.proj(), false, &format!("{connect} \"$MARV_MESH_SOCKET\""));
    assert!(ran.output.contains("connected"), "the plug: {}", ran.output);
    let ran = home.run_with(&sandbox, &home.proj(), false, &format!("{connect} '{}'", other.display()));
    assert!(!ran.output.contains("connected"), "another socket: {}", ran.output);
    let control = host(home.home(), &format!("{connect} '{}'", other.display()));
    assert!(control.output.contains("connected"), "control: {}", control.output);
    let ran = home.run_with(&sandbox, &home.proj(), false, &format!("ls '{}'", runtime.path().display()));
    assert!(denied(&ran), "listing: {}", ran.output);
    drop(listeners);
}

/// **The home is closed** (G6, G15, G17): its files, Marv's config wherever it is, a credential and where its symlink
/// leads can't be read; Marv's skills and the toolchains can.
#[test]
fn the_home_marvs_config_and_credentials_cant_be_read() {
    require_sandbox!();
    let home = Home::new("sb-home");
    let h = home.home().display().to_string();
    for file in [
        ".ssh/known_hosts",
        ".marv/config.json",
        ".cargo/credentials.toml",
        ".config/git/credentials",
        ".dotfiles/git-credentials",
        ".notes/hidden.txt",
        ".zshrc",
    ] {
        let ran = home.run(&format!("cat '{h}/{file}'"));
        assert!(!ran.output.contains("SECRET") && ran.code != Some(0), "{file}: {}", ran.output);
        assert_eq!(host(home.home(), &format!("cat '{h}/{file}'")).code, Some(0), "control: {file}");
    }
    assert!(denied(&home.run("ls ~")), "the home can't be listed");
    // But `cd ~` works (bash may say it can't name the folder it's in: its parents can't be read).
    let ran = home.run("cd ~ && echo in");
    assert_eq!(ran.output.lines().last(), Some("in"), "{}", ran.output);
    let ran = home.run(&format!("cat '{h}/.marv/skills/x/SKILL.md' '{h}/.cargo/bin/tool'"));
    assert_eq!(ran.code, Some(0), "{}", ran.output);
    // Marv's config folder elsewhere (MARV_CONFIG_DIR) is closed too.
    let config = TempDir::outside_tmp("sb-config");
    config.write("config.json", "SECRET-ELSEWHERE");
    let mut sandbox = home.sandbox();
    sandbox.config_dir = Some(config.path().to_path_buf());
    let ran = home.run_with(&sandbox, &home.proj(), false, &format!("cat '{}/config.json'", config.path().display()));
    assert!(!ran.output.contains("SECRET"), "{}", ran.output);
    assert!(host(home.home(), &format!("cat '{}/config.json'", config.path().display())).output.contains("SECRET"), "control");
    // A home-folder project: the hidden entries and ~/Library stay closed, their names upper-cased too (APFS ignores
    // case). (An upper-cased `/PRIVATE/VAR/TMP` prefix fails outside the sandbox too, so only the names are.)
    std::fs::create_dir_all(home.home().join("Library/Keychains")).unwrap();
    home.dir.write("Library/Keychains/k", "SECRET-KEYCHAIN");
    for path in [
        format!("{h}/.ssh/known_hosts"),
        format!("{h}/.SSH/KNOWN_HOSTS"),
        format!("{h}/.notes/hidden.txt"),
        format!("{h}/.NOTES/HIDDEN.TXT"),
        format!("{h}/Library/Keychains/k"),
        format!("{h}/LIBRARY/KEYCHAINS/K"),
    ] {
        let ran = home.run_with(&home.sandbox(), home.home(), false, &format!("cat '{path}'"));
        assert!(!ran.output.contains("SECRET"), "home project, {path}: {}", ran.output);
        assert!(host(home.home(), &format!("cat '{path}'")).output.contains("SECRET"), "control: {path}");
    }
    // ...while the rest of the home reads, as the home-folder project it is.
    let ran = home.run_with(&home.sandbox(), home.home(), false, "cat proj/src/main.rs");
    assert_eq!(ran.code, Some(0), "{}", ran.output);
}

/// **Writes only in the project and the private `TMPDIR`** (G2, G9): not the system, not outside the project, not the
/// home in a home-folder project, not the shared `/tmp`. The `TMPDIR` is the command's own and gone after it.
#[test]
fn writes_only_in_the_project_and_a_private_tmpdir() {
    require_sandbox!();
    let home = Home::new("sb-write");
    let outside = TempDir::outside_tmp("sb-outside");
    let shared = format!("/tmp/marv-guard-sb-{}", std::process::id());
    for target in ["/usr/local/marv-guard".to_string(), format!("{}/x", outside.path().display()), shared.clone()] {
        let ran = home.run(&format!("echo x > '{target}' && echo wrote"));
        assert!(!ran.output.contains("wrote"), "{target}: {}", ran.output);
        assert!(!Path::new(&target).exists(), "{target}");
    }
    let ran = home.run_with(&home.sandbox(), home.home(), false, "echo pwned >> ~/.zshrc; echo x > ~/new; echo done");
    assert_eq!(std::fs::read_to_string(home.home().join(".zshrc")).unwrap(), "# rc\n", "{}", ran.output);
    assert!(!home.home().join("new").exists());
    let ran = home.run("echo here > \"$TMPDIR/x\" && cat \"$TMPDIR/x\" && echo \"$TMPDIR\" && echo y > src/new.rs");
    let tmpdir = ran.output.lines().nth(1).unwrap_or_default().to_string();
    assert!(ran.output.starts_with("here") && home.proj().join("src/new.rs").exists(), "{}", ran.output);
    assert!(tmpdir.contains("/T/marv-cmd-") && !Path::new(&tmpdir).exists(), "the TMPDIR {tmpdir} must be gone");
}

/// **Protected names can't be created at any depth** (G13: what bwrap can only find afterwards), and the after-scan,
/// still run, finds nothing. Nor can `.git` be made where there's none (G12).
#[test]
fn protected_names_cant_be_created_and_the_scan_finds_nothing() {
    require_sandbox!();
    let home = Home::new("sb-names");
    let before = protected_snapshot(&home.proj());
    let ran = home.run(
        "rm -f .envrc; echo x > .envrc; echo x > pkg/agents.MD; mkdir -p sub; echo x > sub/AGENTS.md; mkdir .git; \
         mkdir -p .marv && echo x > .marv/x; mkdir -p .vscode && echo '{}' > .vscode/tasks.json; echo x > .env.local; echo done",
    );
    assert!(ran.output.contains("done"), "{}", ran.output);
    assert_eq!(std::fs::read_to_string(home.proj().join(".envrc")).unwrap(), "export SAFE=1\n");
    for created in ["sub/AGENTS.md", ".git", ".marv/x", ".vscode/tasks.json", ".env.local"] {
        assert!(!home.proj().join(created).exists(), "{created}");
    }
    // APFS ignores case: pkg/agents.MD is pkg/AGENTS.md, which must be unchanged.
    assert_eq!(std::fs::read_to_string(home.proj().join("pkg/AGENTS.md")).unwrap(), "safe\n");
    assert!(scan_created(&home.proj(), &before).is_empty(), "the after-scan found something the rules missed");
    // node_modules may hold an AGENTS.md (an install writes one).
    let ran = home.run("mkdir -p node_modules/p && echo x > node_modules/p/AGENTS.md && echo ok");
    assert!(ran.output.contains("ok"), "{}", ran.output);
}

/// **A protected file can't be changed through a rename or a hard link**: renaming its folder is refused (its ancestors
/// can't be unlinked), and no hard link to it (or to the home's files) can be made, into the project or the
/// `TMPDIR`. `link()` is checked against both paths (#166 probe), so hard links are allowed between the project's and
/// the TMPDIR's own files.
#[test]
fn protected_files_cant_be_renamed_away_or_hard_linked() {
    require_sandbox!();
    require_tools(&["python3"]);
    let home = Home::new("sb-links");
    let h = home.home().display().to_string();
    let ran = home.run("mv pkg pkg2; mv pkg/AGENTS.md x; echo done");
    assert!(home.proj().join("pkg/AGENTS.md").exists() && !home.proj().join("pkg2").exists(), "{}", ran.output);
    for script in [
        format!("ln '{h}/.ssh/known_hosts' x && cat x"),
        format!("ln '{h}/.ssh/known_hosts' \"$TMPDIR/x\" && cat \"$TMPDIR/x\""),
        format!("python3 -c 'import os; os.link(\"{h}/.ssh/known_hosts\", \"y\")' && cat y"),
        "ln .envrc y && echo pwned >> y".to_string(),
        "ln pkg/AGENTS.md \"$TMPDIR/z\" && echo pwned >> \"$TMPDIR/z\"".to_string(),
    ] {
        let ran = home.run(&script);
        assert!(ran.code != Some(0) && !ran.output.contains("SECRET"), "{script}: {}", ran.output);
    }
    // Controls: outside the sandbox each of those links can be made.
    for (src, dst) in [(format!("{h}/.ssh/known_hosts"), "ctl-x"), (".envrc".to_string(), "ctl-y")] {
        let control = host(&home.proj(), &format!("ln '{src}' {dst} && echo made && rm {dst}"));
        assert!(control.output.contains("made"), "control, {src}: {}", control.output);
    }
    assert_eq!(std::fs::read_to_string(home.proj().join(".envrc")).unwrap(), "export SAFE=1\n");
    assert_eq!(std::fs::read_to_string(home.proj().join("pkg/AGENTS.md")).unwrap(), "safe\n");
    let ran = home.run("ln src/main.rs src/linked.rs && echo a > \"$TMPDIR/t\" && ln \"$TMPDIR/t\" t && echo linked");
    assert!(ran.output.contains("linked"), "{}", ran.output);
}

/// **No network without asking** (G18): no IP at all, the host's loopback included (Seatbelt has no loopback of its
/// own, and the user's local services are there); no bind either.
#[test]
fn without_network_no_ip_not_even_loopback() {
    require_sandbox!();
    require_tools(&["python3", "curl"]);
    let home = Home::new("sb-net");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Controls: the host reaches its own listener; with network, the sandbox reaches the internet and may bind.
    let control = host(home.home(), &format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo reached"));
    assert!(control.output.contains("reached"), "control: {}", control.output);
    for script in [
        "curl -sS -m 20 -o /dev/null https://example.com && echo reached",
        "python3 -c 'import socket; s=socket.socket(); s.bind((\"0.0.0.0\", 0)); s.listen(1); print(\"reach\" \"ed\")'",
    ] {
        let ran = home.run_with(&home.sandbox(), &home.proj(), true, script);
        assert!(ran.output.contains("reached"), "control with network, {script}: {}", ran.output);
    }
    for script in [
        "curl -sS -m 5 -o /dev/null https://example.com && echo reached".to_string(),
        format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo reached"),
        "python3 -c 'import socket; s=socket.socket(); s.bind((\"0.0.0.0\", 0)); s.listen(1); print(\"reach\" \"ed\")'"
            .to_string(),
        "ping -c 1 -t 2 127.0.0.1 && echo reached".to_string(),
    ] {
        let ran = home.run(&script);
        assert!(!ran.output.contains("reached"), "{script}: {}", ran.output);
    }
}

/// **Other processes can't be seen or signalled** (G20): not Marv, not `ps` (setuid programs don't run at all in the
/// sandbox: no `sudo` either).
#[test]
fn host_processes_cant_be_reached_and_setuid_programs_dont_run() {
    require_sandbox!();
    let home = Home::new("sb-proc");
    let ran = home.run(&format!("kill -0 {} && echo reached", std::process::id()));
    assert!(!ran.output.contains("reached"), "{}", ran.output);
    for script in ["ps -A && echo listed", "sudo -n true && echo root"] {
        let ran = home.run(script);
        assert!(!ran.output.contains("listed") && !ran.output.contains("root"), "{script}: {}", ran.output);
    }
    let ran = home.run("echo x > /dev/tty && echo wrote");
    assert!(!ran.output.contains("wrote"), "/dev/tty: {}", ran.output);
}

/// **Nothing of Marv's environment reaches the command** (G19): only the plan's variables, the caches in the private
/// `TMPDIR`. `sandbox-exec` passes its environment on, so Marv spawns it with the plan's alone.
#[tokio::test]
async fn the_command_gets_only_the_plans_environment() {
    require_sandbox!();
    let home = Home::new("sb-env");
    let cancel = tokio_util::sync::CancellationToken::new();
    let result = bash(&home.sandbox(), &home.proj(), "env", Duration::from_secs(30), &cancel).await;
    let allowed = [
        "PATH",
        "HOME",
        "LANG",
        "TERM",
        "TMPDIR",
        "BUN_INSTALL_CACHE_DIR",
        "XDG_CACHE_HOME",
        "GOCACHE",
        "PIP_CACHE_DIR",
        "npm_config_cache",
        "npm_config_store_dir",
        "PWD",
        "SHLVL",
        "_",
        "OLDPWD",
    ];
    for line in result.output.lines().filter(|l| l.contains('=')) {
        let key = line.split('=').next().unwrap();
        assert!(allowed.contains(&key), "{key} reached the command: {}", result.output);
    }
    assert!(result.output.contains("XDG_CACHE_HOME=") && result.output.contains("/T/marv-cmd-"), "{}", result.output);
}

/// **Nothing a command starts outlives it** (G21, the guard): what it leaves in the background ends with it, at its
/// timeout, and at a stop; a process that left the group (`setsid`) is still in the sandbox (it can't write outside the
/// project), which is the one documented survivor.
#[tokio::test]
async fn nothing_outlives_the_command_and_an_escapee_stays_sandboxed() {
    require_sandbox!();
    let home = Home::new("sb-life");
    let cancel = tokio_util::sync::CancellationToken::new();
    let sandbox = home.sandbox();
    // A normal exit with a background child holding the pipes: it returns at once, and the child is gone.
    let started = Instant::now();
    let result = bash(&sandbox, &home.proj(), "sleep 600 & echo $!", Duration::from_secs(60), &cancel).await;
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "held by its background child: {:?}", started.elapsed());
    assert!(gone(first_pid(&result.output)), "the background child outlived the command");
    // At the timeout.
    let result = bash(&sandbox, &home.proj(), "sleep 600 & echo $!; sleep 600", Duration::from_secs(1), &cancel).await;
    assert!(result.timed_out && result.exit_code.is_none(), "{result:?}");
    assert!(gone(first_pid(&result.output)));
    // At a stop.
    let stop = tokio_util::sync::CancellationToken::new();
    let stopper = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(800)).await;
        stopper.cancel();
    });
    let result = bash(&sandbox, &home.proj(), "sleep 600 & echo $!; sleep 600", Duration::from_secs(60), &stop).await;
    assert!(result.aborted, "{result:?}");
    assert!(gone(first_pid(&result.output)));
    // An escapee: its own session, double-forked; still sandboxed.
    let outside = TempDir::new("sb-escape");
    let target = outside.path().join("escaped");
    let script = format!(
        "( python3 -c 'import os,time; os.setsid(); time.sleep(1.5); open(\"{}\", \"w\").write(\"x\")' >/dev/null 2>&1 & ); echo started",
        target.display()
    );
    let result = bash(&sandbox, &home.proj(), &script, Duration::from_secs(30), &cancel).await;
    assert!(result.output.contains("started"), "{result:?}");
    std::thread::sleep(Duration::from_secs(3));
    assert!(!target.exists(), "an escapee wrote outside the project");
}

/// A dropped command (a turn given up) takes its whole group with it, not only the sandbox program's pid.
#[tokio::test]
async fn a_dropped_command_takes_its_group_with_it() {
    require_sandbox!();
    let home = Home::new("sb-drop");
    let pid_file = home.proj().join("bg.pid");
    let cancel = tokio_util::sync::CancellationToken::new();
    let sandbox = home.sandbox();
    let root = home.proj();
    let run = bash(&sandbox, &root, "sleep 600 & echo $! > bg.pid; sleep 600", Duration::from_secs(60), &cancel);
    let _ = tokio::time::timeout(Duration::from_secs(2), run).await;
    let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    assert!(gone(pid), "the background child outlived the dropped command");
}

/// A sandboxed command that runs `(sleep 2; touch late) & sleep 600`, stopped by `cancel` (already cancelled, or soon
/// after the spawn): the call must return at once and leave nothing running (`late` never appears).
async fn stopped_early(tag: &str, cancel: tokio_util::sync::CancellationToken) {
    let (project, home) = (TempDir::new(tag), TempDir::outside_tmp(&format!("{tag}-home")));
    let options = marv_tools::bash::RunOptions {
        command: "(sleep 2; touch late) & sleep 600".into(),
        root: project.path().to_path_buf(),
        sandbox: true,
        timeout: std::time::Duration::from_secs(60),
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let result = marv_tools::bash::run_command(options, &sandbox_with_home(home.path()), &cancel).await;
    assert!(result.aborted, "{result:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "the stop didn't hold: {:?}", started.elapsed());
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(!project.path().join("late").exists(), "{tag}: the stopped command went on running ({result:?})");
}

/// **A stop that comes before the sandbox is set up still stops the command** (#166 review): Seatbelt's process group
/// exists only once `marv __guard` has called `setsid`, so a group kill alone, sent earlier, reached nothing and the
/// command ran on. An already-cancelled token, and a stop right after the spawn.
///
/// Proven: `Seatbelt::kill` as `killpg` alone → failed on macOS (see the PR's proof comment).
///
/// macOS only: on Linux a SIGKILL that reaches bwrap while it's still setting up (before `--die-with-parent` holds)
/// leaves the command running in its namespace; that's bwrap's own race, followed up separately.
#[tokio::test]
async fn a_stop_before_the_sandbox_is_up_still_stops_the_command() {
    require_sandbox!();
    let cancelled = tokio_util::sync::CancellationToken::new();
    cancelled.cancel();
    stopped_early("stop-early", cancelled).await;
    for delay in [1, 5, 20] {
        let soon = tokio_util::sync::CancellationToken::new();
        let stopper = soon.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            stopper.cancel();
        });
        stopped_early(&format!("stop-soon-{delay}"), soon).await;
    }
}

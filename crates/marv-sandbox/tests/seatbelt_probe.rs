//! Probes of `sandbox-exec` on a real macOS (#166, `docs/specs/2026-10-17-macos-sandbox.md` §9.2): what the spec and
//! the profile builder mark **to verify**, run against the kernel rather than assumed.
//!
//! `probe_report` (ignored by default) runs every probe and prints what it saw, one `PROBE name: …` line each, for the
//! record on #166: run it with `cargo test -p marv-sandbox --test seatbelt_probe -- --ignored --nocapture`. It never
//! fails on a result; the settled answers are pinned by the guards (`tests/guards/tests/seatbelt.rs`) and by the
//! asserting tests below it.
//!
//! The fixtures live in the real home (`~/mx-probe-<pid>` as the project, `~/.mx-probe-<pid>/known_hosts` as a file the
//! closed home hides), in `/private/tmp/mx-probe-<pid>` and in the user's `T/` folder, and are removed afterwards.
#![cfg(target_os = "macos")]

use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use marv_sandbox::seatbelt::{self, Invocation, Profile, SANDBOX_EXEC};
use marv_sandbox::{CommandMounts, Plan, RealFs, Sandbox};

/// One run's exit code and output (trimmed, cut to a line's worth).
struct Out {
    code: Option<i32>,
    out: String,
    err: String,
}

impl std::fmt::Display for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cut = |s: &str| {
            let s = s.trim().replace('\n', " | ");
            if s.len() > 300 { format!("{}…", &s[..s.char_indices().nth(300).map_or(s.len(), |(i, _)| i)]) } else { s }
        };
        write!(f, "rc={:?} out=[{}] err=[{}]", self.code, cut(&self.out), cut(&self.err))
    }
}

fn output(cmd: &mut Command) -> Out {
    match cmd.stdin(Stdio::null()).output() {
        Ok(o) => Out {
            code: o.status.code(),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Out { code: None, out: String::new(), err: format!("spawn: {e}") },
    }
}

/// The user's temporary folder (`…/T`), canonical, from `getconf` (not `TMPDIR`).
fn user_t() -> PathBuf {
    let o = Command::new("/usr/bin/getconf").arg("DARWIN_USER_TEMP_DIR").output().unwrap();
    std::fs::canonicalize(String::from_utf8_lossy(&o.stdout).trim()).unwrap()
}

/// The fixtures: see the module doc.
struct Fx {
    home: PathBuf,
    root: PathBuf,
    secret_dir: PathBuf,
    secret: PathBuf,
    shared: PathBuf,
    tmp: PathBuf,
    user_temp: PathBuf,
    extra: Vec<PathBuf>,
}

impl Fx {
    fn new(tag: &str) -> Self {
        let home = std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap();
        let id = format!("{}-{tag}", std::process::id());
        let root = home.join(format!("mx-probe-{id}"));
        let secret_dir = home.join(format!(".mx-probe-{id}"));
        let shared = PathBuf::from(format!("/private/tmp/mx-probe-{id}"));
        for d in [&root, &secret_dir, &shared] {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).unwrap();
        }
        let secret = secret_dir.join("known_hosts");
        std::fs::write(&secret, "SECRET\n").unwrap();
        std::fs::write(root.join(".envrc"), "export SAFE=1\n").unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/AGENTS.md"), "safe\n").unwrap();
        let t = user_t();
        let tmp = nix::unistd::mkdtemp(&t.join("marv-probe-XXXXXX")).unwrap();
        let user_temp = t.parent().unwrap().to_path_buf();
        Self { home, root, secret_dir, secret, shared, tmp, user_temp, extra: Vec::new() }
    }

    fn plan(&self, root: &Path, network: bool) -> Plan {
        let mut sb = Sandbox::from_env(true);
        sb.home = self.home.clone();
        sb.path = std::env::var("PATH").unwrap_or_default();
        sb.plan(root, &CommandMounts { network, ..Default::default() })
    }

    fn run(&self) -> Invocation {
        Invocation { tmp: self.tmp.clone(), user_temp: Some(self.user_temp.clone()), exe: None, ports: vec![] }
    }

    fn profile(&self, network: bool) -> Profile {
        seatbelt::profile(&self.plan(&self.root, network), &self.run(), &RealFs).unwrap()
    }

    /// `script` in bash under `profile`, in `cwd`, with only the sandbox's environment.
    fn sh(&self, profile: &Profile, cwd: &Path, script: &str) -> Out {
        output(
            Command::new(SANDBOX_EXEC)
                .args(profile.args())
                .arg("--")
                .args(["/bin/bash", "-c", script])
                .current_dir(cwd)
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .env("HOME", &self.home)
                .env("TMPDIR", &self.tmp)
                .env("LANG", "C.UTF-8")
                .env("SECRET", &self.secret)
                .env("SHARED", &self.shared)
                .env("XDG_CACHE_HOME", self.tmp.join("cache"))
                .env("GOCACHE", self.tmp.join("go-cache"))
                .env("GOPATH", self.tmp.join("go"))
                .env("npm_config_cache", self.tmp.join("npm-cache"))
                .env("npm_config_store_dir", self.tmp.join("pnpm-store")),
        )
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        for d in [&self.root, &self.secret_dir, &self.shared, &self.tmp].into_iter().chain(&self.extra) {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// Whether `sandbox-exec` accepts `(version 1)(allow default)` plus `snippet` (it runs `/usr/bin/true`).
fn accepts(snippet: &str) -> Out {
    output(Command::new(SANDBOX_EXEC).args(["-p", &format!("(version 1)\n(allow default)\n{snippet}\n"), "/usr/bin/true"]))
}

/// A listener on a Unix socket at `path`, accepting in the background until the test ends.
fn unix_listener(path: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut s = s;
            let _ = s.write_all(b"hi\n");
        }
    });
    Ok(())
}

fn tcp_listener(addr: &str) -> Option<u16> {
    let l = std::net::TcpListener::bind(addr).ok()?;
    let port = l.local_addr().ok()?.port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut s = s;
            let _ = s.write_all(b"hi\n");
        }
    });
    Some(port)
}

const PY_UNIX_CONNECT: &str =
    r#"python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); print("connected")'"#;

#[test]
#[ignore = "a report for #166: run with --ignored --nocapture on macOS"]
fn probe_report() {
    let mut report: Vec<String> = Vec::new();
    let mut note = |name: &str, what: String| {
        println!("PROBE {name}: {what}");
        report.push(format!("{name}: {what}"));
    };
    note("sw_vers", output(&mut Command::new("/usr/bin/sw_vers")).to_string());

    // --- Syntax: each snippet on its own, so one bad rule names itself.
    for (name, snippet) in [
        ("syntax.regex_literal", r#"(deny file-read* (regex #"^/nonexistent/a\.b$"))"#),
        ("syntax.regex_escaped_quote", r#"(deny file-read* (regex #"^/nonexistent/q\"q$"))"#),
        ("syntax.regex_escaped_backslash", r#"(deny file-read* (regex #"^/nonexistent/q\\q$"))"#),
        ("syntax.unix_socket_subpath", r#"(deny network-outbound (remote unix-socket (subpath "/private/tmp/mx")))"#),
        ("syntax.unix_socket_path_literal", r#"(deny network-outbound (remote unix-socket (path-literal "/private/tmp/mx")))"#),
        ("syntax.unix_socket_bind_subpath", r#"(allow network-bind (local unix-socket (subpath "/private/tmp/mx")))"#),
        ("syntax.fcntl", "(deny system-fcntl (fcntl-command 80) (fcntl-command 110))"),
        (
            "syntax.system_socket_domains",
            "(allow system-socket (socket-domain AF_UNIX) (socket-domain AF_INET) (socket-domain AF_INET6))",
        ),
        ("syntax.file_link", r#"(deny file-link (subpath "/private/tmp/mx"))"#),
        ("syntax.file_link_literal", r#"(deny file-link (literal "/private/tmp/mx"))"#),
        ("syntax.file_clone", r#"(deny file-clone (subpath "/private/tmp/mx"))"#),
        ("syntax.localhost_star", r#"(allow network-outbound (remote ip "localhost:*"))"#),
        ("syntax.localhost_port", r#"(allow network-outbound (remote ip "localhost:8080"))"#),
        ("syntax.inbound_localhost", r#"(allow network-inbound (local ip "localhost:*"))"#),
        ("syntax.file_mode_exec", "(deny process-exec (file-mode #o4000))"),
        ("syntax.same_sandbox", "(allow signal (target same-sandbox))"),
        ("syntax.xpc_prefix", r#"(deny mach-lookup (xpc-service-name-prefix ""))"#),
        ("syntax.file_write_unlink", r#"(deny file-write-unlink (literal "/private/tmp/mx"))"#),
        ("syntax.require_all", r#"(deny file-write* (require-all (regex #"^/x/") (require-not (regex #"^/x/y/"))))"#),
    ] {
        note(name, accepts(snippet).to_string());
    }

    let fx = Fx::new("main");
    let full = fx.profile(false);
    note("profile.size_bytes", full.text.len().to_string());
    note("profile.accepted_network_off", fx.sh(&full, &fx.root, "echo ok").to_string());
    let net = fx.profile(true);
    note("profile.accepted_network_on", fx.sh(&net, &fx.root, "echo ok").to_string());
    let linking = seatbelt::profile_allowing_links(&fx.plan(&fx.root, false), &fx.run(), &RealFs).unwrap();
    note("profile.accepted_links_allowed", fx.sh(&linking, &fx.root, "echo ok").to_string());

    // --- Regex literals: is `\.` a regex escape (the backslash reaches the regex) or eaten by the reader?
    let rdir = fx.shared.join("rx");
    std::fs::create_dir_all(&rdir).unwrap();
    std::fs::write(rdir.join("a.b"), "x").unwrap();
    std::fs::write(rdir.join("aXb"), "x").unwrap();
    std::fs::write(rdir.join("q\"q"), "x").unwrap();
    let rx = format!("(version 1)\n(allow default)\n(deny file-read-data (regex #\"^{}/a\\.b$\"))\n", rdir.display());
    let p = Profile { text: rx, params: vec![] };
    note("regex.backslash_dot.a.b", fx.sh(&p, &fx.shared, &format!("cat {}/a.b", rdir.display())).to_string());
    note("regex.backslash_dot.aXb", fx.sh(&p, &fx.shared, &format!("cat {}/aXb", rdir.display())).to_string());
    let rq = format!("(version 1)\n(allow default)\n(deny file-read-data (regex #\"^{}/q\\\"q$\"))\n", rdir.display());
    let p = Profile { text: rq, params: vec![] };
    note("regex.escaped_quote.q\"q", fx.sh(&p, &fx.shared, &format!("cat '{}/q\"q'", rdir.display())).to_string());

    // --- BLOCKING: does file-link check the link's source? With the allow on, from a project under the home.
    for (name, prof) in [("links_allowed", &linking), ("links_off", &full)] {
        note(&format!("link.{name}.secret_into_project"), fx.sh(prof, &fx.root, r#"ln "$SECRET" x && cat x"#).to_string());
        note(
            &format!("link.{name}.secret_into_tmpdir"),
            fx.sh(prof, &fx.root, r#"ln "$SECRET" "$TMPDIR/x" && cat "$TMPDIR/x""#).to_string(),
        );
        note(&format!("link.{name}.envrc_in_project"), fx.sh(prof, &fx.root, "ln .envrc y && echo linked").to_string());
        note(&format!("link.{name}.agents_in_project"), fx.sh(prof, &fx.root, "ln pkg/AGENTS.md z && echo linked").to_string());
        note(
            &format!("link.{name}.plain_in_project"),
            fx.sh(prof, &fx.root, "echo a > plain && ln plain plain2 && echo linked").to_string(),
        );
        note(
            &format!("link.{name}.plain_in_tmpdir"),
            fx.sh(prof, &fx.root, r#"echo a > "$TMPDIR/p" && ln "$TMPDIR/p" "$TMPDIR/p2" && echo linked"#).to_string(),
        );
        note(&format!("link.{name}.symlink_to_secret"), fx.sh(prof, &fx.root, r#"ln -s "$SECRET" s && cat s"#).to_string());
        for f in ["x", "y", "z", "plain", "plain2", "s"] {
            let _ = std::fs::remove_file(fx.root.join(f));
        }
    }
    // Links made outside the sandbox: writing a protected file, reading a hidden one, through a second name.
    std::fs::hard_link(fx.root.join(".envrc"), fx.root.join("hl")).unwrap();
    note("link.premade.write_through_to_envrc", fx.sh(&full, &fx.root, "echo pwned >> hl && echo wrote").to_string());
    note("link.premade.envrc_after", std::fs::read_to_string(fx.root.join(".envrc")).unwrap_or_default().replace('\n', " | "));
    let _ = std::fs::remove_file(fx.root.join("hl"));
    std::fs::hard_link(&fx.secret, fx.tmp.join("pre")).unwrap();
    note("link.premade.read_secret_in_tmpdir", fx.sh(&full, &fx.root, r#"cat "$TMPDIR/pre""#).to_string());
    let _ = std::fs::remove_file(fx.tmp.join("pre"));

    // --- Case: rules against typed case or on-disk case?
    note("case.secret_upper", fx.sh(&full, &fx.root, r#"cat "$(printf %s "$SECRET" | tr a-z A-Z)""#).to_string());
    note("case.secret_lower_ok", fx.sh(&full, &fx.root, r#"cat "$SECRET""#).to_string());
    note(
        "case.write_project_via_upper_path",
        fx.sh(&full, &fx.root, r#"echo a > "$(printf %s "$PWD" | tr a-z A-Z)/upper-new" && echo wrote"#).to_string(),
    );
    note(
        "case.write_envrc_via_upper_path",
        fx.sh(&full, &fx.root, r#"echo pwned >> "$(printf %s "$PWD" | tr a-z A-Z)/.envrc" && echo wrote"#).to_string(),
    );
    note("case.write_ENVRC_name", fx.sh(&full, &fx.root, "echo pwned >> .ENVRC && echo wrote").to_string());
    note("case.new_agents_md_lower", fx.sh(&full, &fx.root, "echo x > pkg/agents.MD && echo wrote").to_string());
    note("case.envrc_after", std::fs::read_to_string(fx.root.join(".envrc")).unwrap_or_default().replace('\n', " | "));
    let _ = std::fs::remove_file(fx.root.join("upper-new"));
    let upper_home = fx.home.to_string_lossy().to_uppercase();
    note("case.realpath_upper_home", format!("{:?}", std::fs::canonicalize(&upper_home)));
    note("case.realpath_inside", fx.sh(&full, &fx.root, &format!("/bin/realpath '{upper_home}'")).to_string());

    // --- file-write-unlink on an ancestor and rename().
    note("unlink.mv_folder_with_agents", fx.sh(&full, &fx.root, "mv pkg pkg2 && echo moved").to_string());
    note("unlink.rm_rf_folder_with_agents", fx.sh(&full, &fx.root, "rm -rf pkg; ls pkg").to_string());
    note(
        "unlink.mv_plain_folder",
        fx.sh(&full, &fx.root, "mkdir -p other && mv other other2 && echo moved && rmdir other2").to_string(),
    );
    note("unlink.agents_after", format!("{}", fx.root.join("pkg/AGENTS.md").exists()));

    // --- The keychain.
    for (name, prof) in [("network_off", &full), ("network_on", &net)] {
        note(
            &format!("keychain.{name}.find"),
            fx.sh(prof, &fx.root, "security find-generic-password -s mx-probe-none -w").to_string(),
        );
        note(&format!("keychain.{name}.list"), fx.sh(prof, &fx.root, "security list-keychains").to_string());
    }
    let home_plan = fx.plan(&fx.home, true);
    let home_net = seatbelt::profile(&home_plan, &fx.run(), &RealFs).unwrap();
    note(
        "keychain.home_project_network_on.find",
        fx.sh(&home_net, &fx.home, "security find-generic-password -s mx-probe-none -w").to_string(),
    );

    // --- Home-folder project: ~/LIBRARY, an upper-cased hidden entry.
    note("home_project.library", fx.sh(&home_net, &fx.home, "ls ~/Library | head -3").to_string());
    note("home_project.LIBRARY_upper", fx.sh(&home_net, &fx.home, "ls ~/LIBRARY | head -3").to_string());
    note(
        "home_project.hidden_entry_upper",
        fx.sh(&home_net, &fx.home, r#"cat "$(printf %s "$SECRET" | tr a-z A-Z)""#).to_string(),
    );
    note("home_project.hidden_entry", fx.sh(&home_net, &fx.home, r#"cat "$SECRET""#).to_string());
    note("home_project.visible_listing", fx.sh(&home_net, &fx.home, "ls | head -3").to_string());

    // --- Signals and process info: same-sandbox, and a parallel sandbox with an identical profile.
    let mut a = Command::new(SANDBOX_EXEC)
        .args(full.args())
        .args(["--", "/bin/sleep", "30"])
        .current_dir(&fx.root)
        .env_clear()
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    note("signal.parallel_same_profile", fx.sh(&full, &fx.root, &format!("kill -0 {} && echo reached", a.id())).to_string());
    note("signal.host_process", fx.sh(&full, &fx.root, &format!("kill -0 {} && echo reached", std::process::id())).to_string());
    note("signal.own_child", fx.sh(&full, &fx.root, "sleep 5 & kill $! && echo reached").to_string());
    note("process.ps", fx.sh(&full, &fx.root, "ps -A | wc -l").to_string());
    let _ = a.kill();
    let _ = a.wait();

    // --- setuid.
    note("setuid.sudo_n", fx.sh(&full, &fx.root, "sudo -n true && echo root").to_string());
    note("setuid.sudo_n_network_on", fx.sh(&net, &fx.root, "sudo -n id -u").to_string());

    // --- ICMP without network.
    note("icmp.ping_network_off", fx.sh(&full, &fx.root, "ping -c 1 -t 2 127.0.0.1").to_string());
    note("icmp.ping_network_on", fx.sh(&net, &fx.root, "ping -c 1 -t 2 127.0.0.1").to_string());

    // --- Unicode: an NFC project name, reached by its NFD spelling.
    let nfc = fx.home.join(format!("mx-probe-{}-caf\u{e9}", std::process::id()));
    let nfd = fx.home.join(format!("mx-probe-{}-cafe\u{301}", std::process::id()));
    let _ = std::fs::remove_dir_all(&nfc);
    std::fs::create_dir_all(&nfc).unwrap();
    std::fs::write(nfc.join(".envrc"), "safe\n").unwrap();
    let uni = seatbelt::profile(&fx.plan(&nfc, false), &fx.run(), &RealFs);
    match uni {
        Ok(uni) => {
            note(
                "unicode.on_disk_name",
                format!(
                    "{:?}",
                    std::fs::read_dir(&fx.home)
                        .unwrap()
                        .flatten()
                        .map(|e| e.file_name())
                        .filter(|n| n.to_string_lossy().contains("caf"))
                        .collect::<Vec<_>>()
                ),
            );
            note("unicode.write_cwd", fx.sh(&uni, &nfc, "echo a > new && echo wrote").to_string());
            note(
                "unicode.write_nfd_path",
                fx.sh(&uni, &nfc, &format!("echo a > '{}/new2' && echo wrote", nfd.display())).to_string(),
            );
            note(
                "unicode.envrc_nfd_path",
                fx.sh(&uni, &nfc, &format!("echo pwned >> '{}/.envrc' && echo wrote", nfd.display())).to_string(),
            );
            note(
                "unicode.agents_nfd_path",
                fx.sh(&uni, &nfc, &format!("echo x > '{}/AGENTS.md' && echo wrote", nfd.display())).to_string(),
            );
        }
        Err(e) => note("unicode.profile", format!("refused: {e}")),
    }
    let _ = std::fs::remove_dir_all(&nfc);

    // --- file-clone and copies.
    note("clone.cp_c", fx.sh(&full, &fx.root, "echo a > f && cp -c f g && echo cloned").to_string());
    note("clone.cp", fx.sh(&full, &fx.root, "cp f h && echo copied").to_string());
    note("clone.ditto", fx.sh(&full, &fx.root, "ditto f i && echo dittoed").to_string());
    note("clone.secret_into_tmpdir", fx.sh(&full, &fx.root, r#"cp -c "$SECRET" "$TMPDIR/c" && cat "$TMPDIR/c""#).to_string());
    note("clone.envrc_clone_then_write", fx.sh(&full, &fx.root, "cp -c .envrc e2 && echo x >> e2 && echo ok").to_string());

    // --- Network interfaces (Vite, Go) without AF_ROUTE/AF_SYSTEM.
    note(
        "ifaces.node",
        fx.sh(&full, &fx.root, r#"node -e 'console.log(JSON.stringify(Object.keys(require("os").networkInterfaces())))'"#)
            .to_string(),
    );
    std::fs::write(
        fx.root.join("ifaces.go"),
        "package main\nimport (\"fmt\"; \"net\")\nfunc main() { i, err := net.Interfaces(); fmt.Println(len(i), err) }\n",
    )
    .unwrap();
    note("ifaces.go", fx.sh(&full, &fx.root, "go run ifaces.go").to_string());

    // --- Unix sockets: an agent-like socket in /private/tmp and one in the user's T/, the plug in a denied folder.
    let agent = fx.shared.join("agent.sock");
    unix_listener(&agent).unwrap();
    let t_sock = user_t().join(format!("mx-probe-{}.sock", std::process::id()));
    unix_listener(&t_sock).unwrap();
    for (name, prof) in [("network_off", &full), ("network_on", &net)] {
        note(
            &format!("unix.{name}.shared_tmp"),
            fx.sh(prof, &fx.root, &format!("{PY_UNIX_CONNECT} {}", agent.display())).to_string(),
        );
        note(
            &format!("unix.{name}.user_t"),
            fx.sh(prof, &fx.root, &format!("{PY_UNIX_CONNECT} {}", t_sock.display())).to_string(),
        );
        note(
            &format!("unix.{name}.ssh_auth_sock"),
            fx.sh(prof, &fx.root, &format!("{PY_UNIX_CONNECT} \"${{SSH_AUTH_SOCK:-/nonexistent}}\"")).to_string(),
        );
    }
    note(
        "unix.own_tmpdir",
        fx.sh(&full, &fx.root, r#"python3 -c 'import socket,os; p=os.environ["TMPDIR"]+"/s.sock"; l=socket.socket(socket.AF_UNIX); l.bind(p); l.listen(1); c=socket.socket(socket.AF_UNIX); c.connect(p); print("connected")'"#).to_string(),
    );
    let _ = std::fs::remove_file(&t_sock);
    let run_dir = PathBuf::from(format!("/private/tmp/mx-probe-{}-run", std::process::id()));
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(&run_dir).unwrap();
    let plug = run_dir.join("plug.sock");
    unix_listener(&plug).unwrap();
    let other = run_dir.join("other.sock");
    unix_listener(&other).unwrap();
    let mut plugged = fx.plan(&fx.root, false);
    plugged.hidden.push(run_dir.clone());
    plugged.mesh_socket = Some(plug.clone());
    let plugged = seatbelt::profile(&plugged, &fx.run(), &RealFs).unwrap();
    note("plug.connect", fx.sh(&plugged, &fx.root, &format!("{PY_UNIX_CONNECT} {}", plug.display())).to_string());
    note("plug.list_folder", fx.sh(&plugged, &fx.root, &format!("ls {}", run_dir.display())).to_string());
    note("plug.other_socket", fx.sh(&plugged, &fx.root, &format!("{PY_UNIX_CONNECT} {}", other.display())).to_string());
    note(
        "plug.via_tmp_symlink",
        fx.sh(&plugged, &fx.root, &format!("{PY_UNIX_CONNECT} {}", plug.display().to_string().replace("/private/tmp", "/tmp")))
            .to_string(),
    );
    let _ = std::fs::remove_dir_all(&run_dir);

    // --- localhost filters.
    let p4 = tcp_listener("127.0.0.1:0").unwrap();
    let p6 = tcp_listener(&format!("[::1]:{p4}"));
    let q = tcp_listener("127.0.0.1:0").unwrap();
    let forwarded = seatbelt::profile(&fx.plan(&fx.root, false), &Invocation { ports: vec![p4], ..fx.run() }, &RealFs).unwrap();
    let tcp = |host: &str, port: u16| format!("exec 3<>/dev/tcp/{host}/{port} && echo connected");
    note("localhost.forwarded.127", fx.sh(&forwarded, &fx.root, &tcp("127.0.0.1", p4)).to_string());
    note("localhost.forwarded.localhost_name", fx.sh(&forwarded, &fx.root, &tcp("localhost", p4)).to_string());
    note(
        &format!("localhost.forwarded.v6_listener={}", p6.is_some()),
        fx.sh(
            &forwarded,
            &fx.root,
            &format!(
                "python3 -c 'import socket; s=socket.socket(socket.AF_INET6); s.connect((\"::1\", {p4})); print(\"connected\")'"
            ),
        )
        .to_string(),
    );
    note("localhost.forwarded.other_port", fx.sh(&forwarded, &fx.root, &tcp("127.0.0.1", q)).to_string());
    note("localhost.network_off.127", fx.sh(&full, &fx.root, &tcp("127.0.0.1", p4)).to_string());
    let selftest = |host: &str| {
        format!(
            "python3 -c 'import socket; l=socket.socket(); l.bind((\"{host}\", 0)); l.listen(1); c=socket.socket(); c.connect((\"127.0.0.1\", l.getsockname()[1])); a,_=l.accept(); print(\"self-connected\")'"
        )
    };
    note("localhost.network_off.self_server", fx.sh(&full, &fx.root, &selftest("127.0.0.1")).to_string());
    note("localhost.network_on.self_server", fx.sh(&net, &fx.root, &selftest("127.0.0.1")).to_string());
    note("localhost.network_on.self_server_any", fx.sh(&net, &fx.root, &selftest("0.0.0.0")).to_string());
    note(
        "localhost.network_off.bind_any",
        fx.sh(
            &full,
            &fx.root,
            "python3 -c 'import socket; l=socket.socket(); l.bind((\"0.0.0.0\", 0)); l.listen(1); print(\"bound\")'",
        )
        .to_string(),
    );
    let inbound_only = Profile {
        text: format!("{}\n(allow network-bind (local ip))\n(allow network-inbound (local ip \"localhost:*\"))\n", full.text),
        params: full.params.clone(),
    };
    note("localhost.inbound_rule.listen_any", fx.sh(&inbound_only, &fx.root, "python3 -c 'import socket; l=socket.socket(); l.bind((\"0.0.0.0\", 0)); l.listen(1); print(\"listening\", l.getsockname()[1])'").to_string());
    note(
        "network.curl_off",
        fx.sh(&full, &fx.root, "curl -sS -m 5 -o /dev/null -w '%{http_code}' https://example.com").to_string(),
    );
    note(
        "network.curl_on",
        fx.sh(&net, &fx.root, "curl -sS -m 10 -o /dev/null -w '%{http_code}' https://example.com").to_string(),
    );

    // --- Common tools under the closed profile.
    let repo = fx.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let _ = output(Command::new("git").args(["init", "-q"]).current_dir(&repo));
    for (name, script) in [
        ("tools.git_status", "git -C repo status --short && echo ok"),
        ("tools.git_log", "git -C repo log -1 2>&1; echo done"),
        ("tools.cargo_version", "cargo --version"),
        ("tools.rustc_version", "rustc --version"),
        ("tools.node", "node -e 'console.log(process.version)'"),
        ("tools.npm_version", "npm --version"),
        ("tools.python3", "python3 -c 'import sys, os, subprocess; print(sys.version.split()[0], os.cpu_count())'"),
        ("tools.uname", "uname -a"),
        ("tools.whoami", "whoami; id -un"),
        ("tools.sysctl_ncpu", "sysctl -n hw.ncpu"),
        ("tools.date_tz", "date"),
        ("tools.ls_home", "ls ~"),
        ("tools.cd_home", "cd ~ && echo in-home"),
        ("tools.write_home", "echo x > ~/.mx-probe-write && echo wrote"),
        ("tools.write_shared_tmp", "echo x > /tmp/mx-probe-write && echo wrote"),
        ("tools.write_tmpdir", r#"echo x > "$TMPDIR/w" && cat "$TMPDIR/w""#),
        ("tools.write_usr_local", "echo x > /usr/local/mx-probe && echo wrote"),
        ("tools.dev_tty", "echo x > /dev/tty && echo wrote"),
        ("tools.dev_ttys000", ": < /dev/ttys000 && echo opened"),
        ("tools.osascript", "osascript -e 'tell application \"Finder\" to get name'"),
        ("tools.open", "open -a TextEdit"),
        (
            "tools.launchctl",
            "launchctl submit -l mx-probe -- /usr/bin/true && echo submitted; launchctl remove mx-probe 2>/dev/null; true",
        ),
        ("tools.pbpaste", "pbpaste && echo pasted"),
    ] {
        note(name, fx.sh(&full, &fx.root, script).to_string());
    }
    let _ = std::fs::remove_file("/private/tmp/mx-probe-write");
    let _ = std::fs::remove_file(fx.home.join(".mx-probe-write"));

    // --- The -f fallback: the same profile from a file in the private TMPDIR.
    let file = fx.tmp.join("profile.sb");
    std::fs::write(&file, &full.text).unwrap();
    let mut args: Vec<std::ffi::OsString> = vec!["-f".into(), file.clone().into()];
    args.extend(full.args().into_iter().skip(2));
    note(
        "profile_file.f",
        output(Command::new(SANDBOX_EXEC).args(&args).args(["--", "/bin/echo", "ok"]).current_dir(&fx.root).env_clear())
            .to_string(),
    );
    let big = Profile { text: format!("{}{}", full.text, "; padding\n".repeat(300_000 / 10)), params: full.params.clone() };
    note("profile_file.p_over_256k", fx.sh(&big, &fx.root, "echo ok").to_string());

    // --- clean-tmps and what the kernel logged as denied.
    note(
        "clean_tmps",
        output(&mut Command::new("/usr/bin/grep").args([
            "-n",
            "find\\|-type\\|daily_clean_tmps",
            "/etc/periodic/daily/110.clean-tmps",
        ]))
        .to_string(),
    );
    note(
        "defaults.clean_tmps",
        output(&mut Command::new("/usr/bin/grep").args(["-n", "clean_tmps", "/etc/defaults/periodic.conf"])).to_string(),
    );
    let denied = output(&mut Command::new("/usr/bin/log").args([
        "show",
        "--style",
        "compact",
        "--last",
        "10m",
        "--predicate",
        "sender == \"Sandbox\" AND eventMessage CONTAINS \"deny\"",
    ]));
    let mut kinds: Vec<String> = denied
        .out
        .lines()
        .filter_map(|l| l.split_once("deny(1) ").map(|(_, rest)| rest.split_whitespace().take(2).collect::<Vec<_>>().join(" ")))
        .filter(|l| {
            l.starts_with("sysctl")
                || l.starts_with("mach")
                || l.starts_with("iokit")
                || l.starts_with("file-ioctl")
                || l.starts_with("ipc")
                || l.starts_with("system")
                || l.starts_with("user-pref")
        })
        .collect();
    kinds.sort();
    kinds.dedup();
    note("log.denied_kinds", kinds.join(", "));

    println!("\n===== REPORT =====\n{}\n===== END =====", report.join("\n"));
}

/// Round 2: `link()` itself (python's `os.link`, no `stat` of the source first, as `ln` does), the sysctls node's
/// `os.networkInterfaces()` and `sysctl -n` need, Apple Events that really leave the process, and kqueue on a child
/// that has exited but isn't reaped yet.
#[test]
#[ignore = "a report for #166: run with --ignored --nocapture on macOS"]
fn probe_round2() {
    let mut report: Vec<String> = Vec::new();
    let mut note = |name: &str, what: String| {
        println!("PROBE {name}: {what}");
        report.push(format!("{name}: {what}"));
    };
    let fx = Fx::new("r2");
    let full = fx.profile(false);
    let net = fx.profile(true);
    let linking = seatbelt::profile_allowing_links(&fx.plan(&fx.root, false), &fx.run(), &RealFs).unwrap();
    // A file the user owns outside the home and every allowed folder, on the same volume: /Users/Shared.
    let outside_dir = PathBuf::from(format!("/Users/Shared/mx-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outside_dir);
    let made_outside = std::fs::create_dir_all(&outside_dir).is_ok();
    let outside = outside_dir.join("victim");
    let _ = std::fs::write(&outside, "untouched\n");
    note("setup.outside_writable", made_outside.to_string());
    let cargo_env = fx.home.join(".cargo/env");
    note("setup.cargo_env_exists", cargo_env.exists().to_string());
    std::fs::write(fx.tmp.join("t"), "t\n").unwrap();
    std::fs::write(fx.root.join("plain"), "p\n").unwrap();
    let pylink = |src: &Path, dst: &str| {
        format!("python3 -c 'import os,sys; os.link(sys.argv[1], sys.argv[2]); print(\"linked\")' '{}' \"{dst}\"", src.display())
    };
    for (name, prof) in [("links_allowed", &linking), ("links_off", &full)] {
        let cases: Vec<(&str, String)> = vec![
            ("secret_to_project", pylink(&fx.secret, "x1")),
            ("secret_to_tmpdir", pylink(&fx.secret, "$TMPDIR/x2")),
            ("outside_to_project", format!("{} && echo pwned >> x3; cat '{}'", pylink(&outside, "x3"), outside.display())),
            ("outside_to_tmpdir", pylink(&outside, "$TMPDIR/x4")),
            ("cargo_env_to_project", pylink(&cargo_env, "x5")),
            ("envrc_to_project", pylink(&fx.root.join(".envrc"), "x6")),
            ("agents_to_tmpdir", pylink(&fx.root.join("pkg/AGENTS.md"), "$TMPDIR/x7")),
            ("plain_to_tmpdir", pylink(&fx.root.join("plain"), "$TMPDIR/x8")),
            ("tmpdir_to_project", pylink(&fx.tmp.join("t"), "x9")),
            ("project_to_outside", pylink(&fx.root.join("plain"), &outside_dir.join("x10").to_string_lossy())),
        ];
        for (case, script) in cases {
            note(&format!("pylink.{name}.{case}"), fx.sh(prof, &fx.root, &script).to_string());
        }
        for f in ["x1", "x3", "x5", "x6", "x9"] {
            let _ = std::fs::remove_file(fx.root.join(f));
        }
        for f in ["x2", "x4", "x7", "x8"] {
            let _ = std::fs::remove_file(fx.tmp.join(f));
        }
    }
    note("pylink.outside_after", std::fs::read_to_string(&outside).unwrap_or_default().replace('\n', " | "));
    let _ = std::fs::remove_dir_all(&outside_dir);

    // Sysctls: what node's os.networkInterfaces() and `sysctl -n` need.
    let with = |extra: &str| Profile { text: format!("{}\n{extra}\n", full.text), params: full.params.clone() };
    let node_ifaces = r#"node -e 'console.log(JSON.stringify(Object.keys(require("os").networkInterfaces())))'"#;
    note("ifaces.node.network_on", fx.sh(&net, &fx.root, node_ifaces).to_string());
    note(
        "ifaces.node.routetable",
        fx.sh(&with("(allow sysctl-read (sysctl-name-prefix \"net.routetable.\"))"), &fx.root, node_ifaces).to_string(),
    );
    note(
        "ifaces.python_if_nameindex",
        fx.sh(&full, &fx.root, "python3 -c 'import socket; print(socket.if_nameindex())'").to_string(),
    );
    let sysctl_meta = "(allow sysctl-read (sysctl-name \"sysctl.name2oid\") (sysctl-name-prefix \"sysctl.oidfmt.\"))";
    note("sysctl_n.with_meta", fx.sh(&with(sysctl_meta), &fx.root, "sysctl -n hw.ncpu hw.memsize").to_string());
    note("sysctl_n.brand_with_meta", fx.sh(&with(sysctl_meta), &fx.root, "sysctl -n machdep.cpu.brand_string").to_string());
    note(
        "node.cpus",
        fx.sh(&full, &fx.root, "node -e 'console.log(require(\"os\").cpus().length, require(\"os\").cpus()[0].model)'")
            .to_string(),
    );
    note(
        "python.platform",
        fx.sh(
            &full,
            &fx.root,
            "python3 -c 'import platform, os; print(platform.platform(), platform.processor(), os.cpu_count())'",
        )
        .to_string(),
    );
    let go =
        output(Command::new("/bin/sh").args(["-c", "ls -d /Users/runner/hostedtoolcache/go/*/arm64/bin 2>/dev/null | tail -1"]))
            .out
            .trim()
            .to_string();
    if !go.is_empty() {
        std::fs::write(
            fx.root.join("ifaces.go"),
            "package main\nimport (\"fmt\"; \"net\")\nfunc main() { i, err := net.Interfaces(); fmt.Println(len(i), err) }\n",
        )
        .unwrap();
        note("ifaces.go", fx.sh(&full, &fx.root, &format!("PATH={go}:$PATH; go run ifaces.go")).to_string());
        note(
            "ifaces.go.routetable",
            fx.sh(
                &with("(allow sysctl-read (sysctl-name-prefix \"net.routetable.\"))"),
                &fx.root,
                &format!("PATH={go}:$PATH; go run ifaces.go"),
            )
            .to_string(),
        );
    } else {
        note("ifaces.go", "no go on the runner".into());
    }

    // Apple Events that leave the process (an application's `name` is answered locally).
    for (name, script) in [
        ("finder_windows", "osascript -e 'tell application \"Finder\" to count windows'"),
        ("system_events", "osascript -e 'tell application \"System Events\" to get name of first process'"),
        ("finder_desktop", "osascript -e 'tell application \"Finder\" to get POSIX path of (desktop as alias)'"),
    ] {
        note(&format!("appleevents.{name}.sandboxed"), fx.sh(&full, &fx.root, script).to_string());
        note(&format!("appleevents.{name}.unsandboxed"), output(Command::new("/bin/bash").args(["-c", script])).to_string());
    }
    note("launchctl.after", output(Command::new("/bin/launchctl").args(["list", "mx-probe"])).to_string());

    // kqueue NOTE_EXIT on a child that exited and isn't reaped yet (a zombie).
    {
        use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let kq = Kqueue::new().unwrap();
        let change = KEvent::new(
            child.id() as usize,
            EventFilter::EVFILT_PROC,
            EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
            FilterFlag::NOTE_EXIT,
            0,
            0,
        );
        let mut events = [KEvent::new(0, EventFilter::EVFILT_PROC, EvFlags::empty(), FilterFlag::empty(), 0, 0)];
        let timeout = nix::libc::timespec { tv_sec: 2, tv_nsec: 0 };
        let got = kq.kevent(&[change], &mut events, Some(timeout));
        note(
            "kqueue.zombie",
            format!("{got:?} flags={:?} fflags={:?} data={}", events[0].flags(), events[0].fflags(), events[0].data()),
        );
        let _ = child.wait();
        let mut running = Command::new("/bin/sleep").arg("0.5").spawn().unwrap();
        let change = KEvent::new(
            running.id() as usize,
            EventFilter::EVFILT_PROC,
            EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
            FilterFlag::NOTE_EXIT,
            0,
            0,
        );
        let started = std::time::Instant::now();
        let got = kq.kevent(&[change], &mut events, Some(timeout));
        note("kqueue.running", format!("{got:?} after {:?} fflags={:?}", started.elapsed(), events[0].fflags()));
        note("kqueue.running.unreaped", format!("{:?}", running.try_wait()));
        let _ = running.wait();
    }
    println!("\n===== REPORT =====\n{}\n===== END =====", report.join("\n"));
}

/// Round 3: which path `file-link` matches (source, new name, or both), with minimal profiles; the sysctls node's
/// `os.cpus()` and the `sysctl` tool need.
#[test]
#[ignore = "a report for #166: run with --ignored --nocapture on macOS"]
fn probe_round3() {
    let mut report: Vec<String> = Vec::new();
    let mut note = |name: &str, what: String| {
        println!("PROBE {name}: {what}");
        report.push(format!("{name}: {what}"));
    };
    let fx = Fx::new("r3");
    let base = fx.shared.join("links");
    let (a, b) = (base.join("A"), base.join("B"));
    let pylink = |src: &Path, dst: &Path| {
        format!(
            "python3 -c 'import os,sys; os.link(sys.argv[1], sys.argv[2]); print(\"linked\")' '{}' '{}'",
            src.display(),
            dst.display()
        )
    };
    let reset = || {
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("f"), "a").unwrap();
        std::fs::write(b.join("f"), "b").unwrap();
    };
    let profiles = [
        ("deny_A", format!("(version 1)\n(allow default)\n(deny file-link (subpath \"{}\"))\n", a.display())),
        (
            "deny_all_allow_A",
            format!("(version 1)\n(allow default)\n(deny file-link)\n(allow file-link (subpath \"{}\"))\n", a.display()),
        ),
        (
            "deny_all_allow_A_literal_dir",
            format!("(version 1)\n(allow default)\n(deny file-link)\n(allow file-link (literal \"{}\"))\n", a.display()),
        ),
    ];
    for (name, text) in profiles {
        let p = Profile { text, params: vec![] };
        for (case, src, dst) in [
            ("A_to_B", a.join("f"), b.join("g")),
            ("B_to_A", b.join("f"), a.join("g")),
            ("A_to_A", a.join("f"), a.join("h")),
            ("B_to_B", b.join("f"), b.join("h")),
        ] {
            reset();
            note(&format!("linkop.{name}.{case}"), fx.sh(&p, &fx.shared, &pylink(&src, &dst)).to_string());
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    // The full profile with links allowed, all four directions between the project and the private TMPDIR.
    let linking = seatbelt::profile_allowing_links(&fx.plan(&fx.root, false), &fx.run(), &RealFs).unwrap();
    std::fs::write(fx.root.join("p"), "p").unwrap();
    std::fs::write(fx.tmp.join("t"), "t").unwrap();
    for (case, src, dst) in [
        ("project_to_project", fx.root.join("p"), fx.root.join("p2")),
        ("project_to_tmpdir", fx.root.join("p"), fx.tmp.join("p3")),
        ("tmpdir_to_project", fx.tmp.join("t"), fx.root.join("t2")),
        ("tmpdir_to_tmpdir", fx.tmp.join("t"), fx.tmp.join("t3")),
    ] {
        note(&format!("fullink.{case}"), fx.sh(&linking, &fx.root, &pylink(&src, &dst)).to_string());
    }
    note("fullink.rules", linking.text.lines().filter(|l| l.contains("file-link")).collect::<Vec<_>>().join(" ;; "));

    let full = fx.profile(false);
    let with = |extra: &str| Profile { text: format!("{}\n{extra}\n", full.text), params: full.params.clone() };
    let cpus = "node -e 'const c=require(\"os\").cpus(); console.log(c.length, c[0] && c[0].model, c[0] && c[0].speed)'";
    note("cpus.none", fx.sh(&full, &fx.root, cpus).to_string());
    note(
        "cpus.brand",
        fx.sh(&with("(allow sysctl-read (sysctl-name \"machdep.cpu.brand_string\"))"), &fx.root, cpus).to_string(),
    );
    note(
        "cpus.brand_freq",
        fx.sh(
            &with("(allow sysctl-read (sysctl-name \"machdep.cpu.brand_string\") (sysctl-name \"hw.cpufrequency\") (sysctl-name \"hw.cpufrequency_max\") (sysctl-name \"kern.clockrate\"))"),
            &fx.root,
            cpus,
        )
        .to_string(),
    );
    note("cpus.all_sysctls", fx.sh(&with("(allow sysctl-read)"), &fx.root, cpus).to_string());
    note(
        "sysctl_n.prefix_sysctl",
        fx.sh(&with("(allow sysctl-read (sysctl-name-prefix \"sysctl.\"))"), &fx.root, "sysctl -n hw.ncpu").to_string(),
    );
    note(
        "sysctl_n.all_sysctls",
        fx.sh(&with("(allow sysctl-read)"), &fx.root, "sysctl -n hw.ncpu machdep.cpu.brand_string").to_string(),
    );
    note("getconf_ncpu", fx.sh(&full, &fx.root, "getconf _NPROCESSORS_ONLN").to_string());
    let denied = output(&mut Command::new("/usr/bin/log").args([
        "show",
        "--style",
        "compact",
        "--last",
        "3m",
        "--predicate",
        "sender == \"Sandbox\" AND eventMessage CONTAINS \"sysctl\"",
    ]));
    let mut names: Vec<String> =
        denied.out.lines().filter_map(|l| l.split_once("sysctl-read ").map(|(_, r)| r.trim().to_string())).collect();
    names.sort();
    names.dedup();
    note("log.denied_sysctls", names.join(", "));
    println!("\n===== REPORT =====\n{}\n===== END =====", report.join("\n"));
}

/// Round 4: with the profile as it ships now, the interface list (node, Go) with and without network, and pnpm's
/// clone import with network (the store in the TMPDIR or where pnpm puts it by default).
#[test]
#[ignore = "a report for #166: run with --ignored --nocapture on macOS"]
fn probe_round4() {
    let mut report: Vec<String> = Vec::new();
    let mut note = |name: &str, what: String| {
        println!("PROBE {name}: {what}");
        report.push(format!("{name}: {what}"));
    };
    let fx = Fx::new("r4");
    let off = fx.profile(false);
    let on = fx.profile(true);
    let node_ifaces = r#"node -e 'console.log(JSON.stringify(Object.keys(require("os").networkInterfaces())))'"#;
    std::fs::write(
        fx.root.join("ifaces.go"),
        "package main\nimport (\"fmt\"; \"net\")\nfunc main() { i, err := net.Interfaces(); fmt.Println(len(i), err) }\n",
    )
    .unwrap();
    for (name, prof) in [("network_off", &off), ("network_on", &on)] {
        note(&format!("ifaces.node.{name}"), fx.sh(prof, &fx.root, node_ifaces).to_string());
        note(&format!("ifaces.go.{name}"), fx.sh(prof, &fx.root, "go version && go run ifaces.go").to_string());
        note(
            &format!("cpus.node.{name}"),
            fx.sh(prof, &fx.root, "node -e 'console.log(require(\"os\").cpus().length)'").to_string(),
        );
        note(&format!("sysctl_n.{name}"), fx.sh(prof, &fx.root, "sysctl -n hw.ncpu").to_string());
    }
    std::fs::write(fx.root.join("package.json"), "{\"name\":\"p\",\"version\":\"1.0.0\"}\n").unwrap();
    for (name, script) in [
        ("pnpm.version", "npx -y pnpm@9 --version"),
        ("pnpm.add_default_store", "npx -y pnpm@9 add is-number@7 && ls node_modules"),
        (
            "pnpm.add_clone_tmp_store",
            "rm -rf node_modules; npx -y pnpm@9 add is-number@7 --package-import-method clone --store-dir \"$TMPDIR/pnpm-store\" && ls node_modules",
        ),
        (
            "pnpm.add_auto_tmp_store",
            "rm -rf node_modules; npx -y pnpm@9 add is-number@7 --store-dir \"$TMPDIR/pnpm-store\" && ls node_modules/.pnpm | head -3",
        ),
        ("npm.install", "rm -rf node_modules; npm install is-number@7 --no-audit --no-fund && ls node_modules"),
    ] {
        note(name, fx.sh(&on, &fx.root, script).to_string());
    }
    println!("\n===== REPORT =====\n{}\n===== END =====", report.join("\n"));
}

/// Round 5 (#166 review): with `com.apple.SecurityServer` dropped from the network services, the TLS clients a command
/// uses, under the network profile as it ships: curl, git, cargo (its own HTTP stack), npm, pip (truststore: macOS's
/// Security framework) and a Swift `URLSession` client (Security.framework / Network.framework).
#[test]
#[ignore = "a report for #166: run with --ignored --nocapture on macOS"]
fn probe_round5() {
    let mut report: Vec<String> = Vec::new();
    let mut note = |name: &str, what: String| {
        println!("PROBE {name}: {what}");
        report.push(format!("{name}: {what}"));
    };
    let fx = Fx::new("r5");
    let net = fx.profile(true);
    note("profile.security_server", net.text.contains("com.apple.SecurityServer").to_string());
    std::fs::create_dir_all(fx.root.join("crate/src")).unwrap();
    std::fs::write(
        fx.root.join("crate/Cargo.toml"),
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nitoa = \"1\"\n",
    )
    .unwrap();
    std::fs::write(fx.root.join("crate/src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        fx.root.join("fetch.swift"),
        "import Foundation\nlet s = DispatchSemaphore(value: 0)\nURLSession.shared.dataTask(with: URL(string: \"https://example.com\")!) { d, r, e in print((r as? HTTPURLResponse)?.statusCode ?? -1, e.map { \"\\($0)\" } ?? \"ok\"); s.signal() }.resume()\ns.wait()\n",
    )
    .unwrap();
    for (name, script) in [
        ("curl", "curl -sS -m 20 -o /dev/null -w '%{http_code}' https://example.com"),
        ("git_clone", "git clone -q --depth 1 https://github.com/octocat/Hello-World.git hw && ls hw"),
        ("cargo_fetch", "cd crate && CARGO_HOME=\"$TMPDIR/cargo-home\" cargo fetch -q && echo fetched"),
        ("npm_view", "npm view is-number version"),
        ("pip_download", "python3 -m pip download --no-deps -q -d \"$TMPDIR/pip\" six && ls \"$TMPDIR/pip\""),
        (
            "pip_truststore",
            "python3 -m pip --version; python3 -c 'import pip._vendor.truststore as t; print(\"truststore\", t.__name__)'",
        ),
        ("swift_urlsession", "swift -module-cache-path \"$TMPDIR/mc\" fetch.swift"),
    ] {
        note(name, fx.sh(&net, &fx.root, script).to_string());
    }
    println!("\n===== REPORT =====\n{}\n===== END =====", report.join("\n"));
}

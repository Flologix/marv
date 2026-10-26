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

/// What `sender == "Sandbox"` logs while `f` runs: `log stream`'s lines with a denial in them (#183).
fn sandbox_denials<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!("mx-log-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let file = std::fs::File::create(&path).unwrap();
    let mut stream = Command::new("/usr/bin/log")
        .args(["stream", "--style", "compact", "--predicate", "sender == \"Sandbox\""])
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let got = f();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let _ = stream.kill();
    let _ = stream.wait();
    let lines = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    (got, lines.lines().filter(|l| l.contains("deny")).map(|l| l.trim().to_string()).collect())
}

/// Round 6 (#183): which rule refuses `ln ~/.ssh/known_hosts x` once the home's read deny is gone, links are allowed
/// anywhere and the home is writable (the #178 proof), with `log stream` naming each denied operation; and Go's
/// `net.Interfaces()` under the profile, with a Go outside the home (`MARV_PROBE_GO`: its `bin` folder).
#[test]
#[ignore = "a report for #183: run with --ignored --nocapture on macOS"]
fn probe_round6() {
    let mut report: Vec<String> = Vec::new();
    let mut note = |name: &str, what: String| {
        println!("PROBE {name}: {what}");
        report.push(format!("{name}: {what}"));
    };
    let mut fx = Fx::new("r6");
    // The guards' fixture: a fake home outside the user's temporary folder, a project in it.
    let home = PathBuf::from(format!("/private/var/tmp/mx-probe-{}-r6home", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::create_dir_all(home.join("proj")).unwrap();
    std::fs::write(home.join(".ssh/known_hosts"), "SECRET-SSH\n").unwrap();
    fx.extra.push(home.clone());
    let proj = home.join("proj");
    let plan = {
        let mut sb = Sandbox::from_env(true);
        sb.home = home.clone();
        sb.path = std::env::var("PATH").unwrap_or_default();
        sb.plan(&proj, &CommandMounts::default())
    };
    let shipped = seatbelt::profile(&plan, &fx.run(), &RealFs).unwrap();
    note("rules.link", shipped.text.lines().filter(|l| l.contains("file-link")).collect::<Vec<_>>().join(" ;; "));
    const HOME_READ: &str = "(deny file-read* (subpath (param \"HOME\")))\n";
    assert!(shipped.text.contains(HOME_READ));
    let edit = |remove: &[&str], append: &[&str]| {
        let mut text = shipped.text.clone();
        for r in remove {
            text = text.replace(r, "");
        }
        for a in append {
            text.push_str(a);
            text.push('\n');
        }
        Profile { text, params: shipped.params.clone() }
    };
    const LINKS: &str = "(allow file-link)";
    const HOME_WRITE: &str = "(allow file-write* (subpath (param \"HOME\")))";
    let variants = [
        ("shipped", edit(&[], &[])),
        ("no_home_read", edit(&[HOME_READ], &[])),
        ("no_home_read.links_anywhere", edit(&[HOME_READ], &[LINKS])),
        ("no_home_read.links_anywhere.home_writable", edit(&[HOME_READ], &[LINKS, HOME_WRITE])),
        ("shipped.allow_file_all_last", edit(&[], &["(allow file*)"])),
        ("shipped.allow_file_link_and_data_last", edit(&[], &["(allow file-link file-read* file-write*)"])),
        ("minimal_allow_default", Profile { text: "(version 1)\n(allow default)\n".into(), params: vec![] }),
    ];
    let k = home.join(".ssh/known_hosts");
    for (name, prof) in &variants {
        let script = format!(
            "K='{k}'; cat \"$K\" && echo read-ok; echo w >> \"$K\" && echo write-ok; \
             ln \"$K\" ln-{name} && echo ln-ok; \
             python3 -c 'import os,sys; os.link(sys.argv[1], sys.argv[2]); print(\"pylink-ok\")' \"$K\" py-{name}; \
             echo p > plain-{name} && ln plain-{name} plain2-{name} && echo plain-ln-ok",
            k = k.display()
        );
        let (out, denials) = sandbox_denials(|| fx.sh(prof, &proj, &script));
        note(&format!("link6.{name}"), out.to_string());
        note(&format!("link6.{name}.denials"), denials.join(" || "));
    }

    // Go's net.Interfaces(), with a Go outside the home (the runner's toolcache Go lives under it).
    let gos = std::env::var("MARV_PROBE_GO").unwrap_or_default();
    std::fs::write(
        fx.root.join("ifaces.go"),
        "package main\nimport (\"fmt\"; \"net\")\nfunc main() { i, err := net.Interfaces(); fmt.Println(len(i), err) }\n",
    )
    .unwrap();
    for go in gos.split(':').filter(|g| !g.is_empty()) {
        note(&format!("ifaces6.go[{go}].host"), output(Command::new(format!("{go}/go")).args(["env", "GOROOT"])).to_string());
        for (name, prof) in [("network_off", fx.profile(false)), ("network_on", fx.profile(true))] {
            let script = format!(
                "PATH='{go}':$PATH; go version && echo GOROOT=$(go env GOROOT) && ls \"$(go env GOROOT)/src/net\" | head -1 && go run ifaces.go"
            );
            let (out, denials) = sandbox_denials(|| fx.sh(&prof, &fx.root, &script));
            note(&format!("ifaces6.go[{go}].{name}"), out.to_string());
            note(&format!("ifaces6.go[{go}].{name}.denials"), denials.join(" || "));
        }
    }
    if gos.is_empty() {
        note("ifaces6.go", "MARV_PROBE_GO not set".into());
    }
    println!("\n===== REPORT =====\n{}\n===== END =====", report.join("\n"));
}

/// **Seatbelt refuses `link()` from a source it denies for reading or writing the file's data, whatever the
/// `file-link` rules say** (#166 review probe, #183). The profile's link denies are a second layer on top of this
/// (`HARD_LINK_OP` in `seatbelt.rs`); not a probe but a test, so an OS that stops doing it fails here.
#[test]
fn link_is_refused_from_a_source_denied_for_reads_or_writes() {
    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = Dir(PathBuf::from(format!("/private/var/tmp/mx-link-{}", std::process::id())));
    let (a, b) = (dir.0.join("A"), dir.0.join("B"));
    for d in [&a, &b] {
        std::fs::create_dir_all(d).unwrap();
    }
    let src = a.join("f");
    std::fs::write(&src, "a").unwrap();
    let link = |rules: &str, dst: &Path| {
        output(
            Command::new(SANDBOX_EXEC)
                .args(["-p", &format!("(version 1)\n(allow default)\n{rules}\n"), "/bin/ln"])
                .arg(&src)
                .arg(dst),
        )
    };
    // The control: with nothing denied, the link is made.
    let dst = b.join("control");
    let out = link("", &dst);
    assert!(out.code == Some(0) && dst.exists(), "control: {out}");
    let a_rule = |op: &str| format!("(deny {op} (subpath \"{}\"))", a.display());
    let refused = |name: &str, rules: &str| {
        let dst = b.join(name);
        let out = link(rules, &dst);
        assert!(out.code != Some(0) && out.err.contains("Operation not permitted") && !dst.exists(), "{name}: {out}");
    };
    for (name, deny) in [("read", a_rule("file-read-data")), ("write", a_rule("file-write-data"))] {
        refused(name, &deny);
        refused(&format!("{name}+allow_link_A"), &format!("{deny}\n(allow file-link (subpath \"{}\"))", a.display()));
    }
    // Why the #178 proof's "links allowed anywhere" changed nothing: an allow with no filter doesn't override an
    // earlier deny with one (#183 probe, macOS 14 and 15), so `(allow file-link)` left the home's link deny in force.
    // An allow naming the path does override it. The profile's allows all name their paths.
    let deny_a = a_rule("file-link");
    refused("link_denied+allow_all", &format!("{deny_a}\n(allow file-link)"));
    let dst = b.join("link_denied+allow_A");
    let out = link(&format!("{deny_a}\n(allow file-link (subpath \"{}\"))", a.display()), &dst);
    assert!(out.code == Some(0) && dst.exists(), "link_denied+allow_A: {out}");
}

/// **Each link rule of the shipped profile refuses a link on its own** (#183): with the read or write deny that
/// refuses the link first taken out of the profile (see the test above: Seatbelt refuses `link()` from a source it
/// can't read or write), the link rule alone must still refuse it, and with that rule taken out too the same link is
/// made (the control: the edit reached the kernel, and nothing else held it). A regression in any one link rule, the
/// home's and the shared temporary folder's deny, the protected names' regex or a protected path's literal deny,
/// fails here, which no outcome guard can catch: with the whole profile in place the first layer refuses first.
#[test]
fn each_link_rule_refuses_on_its_own() {
    let pid = std::process::id();
    let home = PathBuf::from(format!("/private/var/tmp/mx-linkrules-{pid}"));
    let shared = PathBuf::from(format!("/private/tmp/mx-linkrules-{pid}"));
    let t = user_t();
    let tmp = nix::unistd::mkdtemp(&t.join("marv-probe-XXXXXX")).unwrap();
    struct Dirs(Vec<PathBuf>);
    impl Drop for Dirs {
        fn drop(&mut self) {
            for d in &self.0 {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }
    let _dirs = Dirs(vec![home.clone(), shared.clone(), tmp.clone()]);
    let proj = home.join("proj");
    for d in [home.join(".ssh"), proj.join("sub"), shared.clone()] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(home.join(".ssh/known_hosts"), "SECRET\n").unwrap();
    std::fs::write(proj.join(".envrc"), "export SAFE=1\n").unwrap();
    std::fs::write(proj.join("plain"), "plain\n").unwrap();
    std::fs::write(shared.join("f"), "shared\n").unwrap();
    let plan = {
        let mut sb = Sandbox::from_env(true);
        sb.home = home.clone();
        sb.path = std::env::var("PATH").unwrap_or_default();
        sb.plan(&proj, &CommandMounts::default())
    };
    let run = Invocation { tmp: tmp.clone(), user_temp: Some(t.parent().unwrap().to_path_buf()), exe: None, ports: vec![] };
    let shipped = seatbelt::profile(&plan, &run, &RealFs).unwrap();
    let text = &shipped.text;
    // The rules this test takes out, each checked to be there as written.
    let line_of = |start: &str| -> String {
        let found: Vec<&str> = text.lines().filter(|l| l.starts_with(start)).collect();
        assert_eq!(found.len(), 1, "one line starting {start:?} in:\n{text}");
        format!("{}\n", found[0])
    };
    let home_read = line_of("(deny file-read* (subpath (param \"HOME\")))");
    let shared_read = line_of("(deny file-read* (subpath \"/private/tmp\"))");
    let broad_link = line_of("(deny file-link (subpath (param \"HOME\")) (subpath \"/private/tmp\")");
    let names_link = line_of("(deny file-link (require-all");
    let protected_links: Vec<String> = text
        .lines()
        .filter(|l| l.starts_with("(deny file-link (literal (param \"PROTECTED_"))
        .map(|l| format!("{l}\n"))
        .collect();
    assert!(!protected_links.is_empty(), "no protected path's link deny in:\n{text}");
    let home_write = "(allow file-write* (subpath (param \"HOME\")))\n";
    let project_write = "(allow file-write* (subpath (param \"PROJECT\")))\n";
    let shared_write = "(allow file-write* (subpath \"/private/tmp\"))\n";
    let edit = |remove: &[&str], replace: &[(&str, &str)], append: &[&str]| {
        let mut t = text.clone();
        for r in remove {
            assert!(t.contains(r), "{r}");
            t = t.replace(r, "");
        }
        for (from, to) in replace {
            assert!(t.contains(from), "{from}");
            t = t.replace(from, to);
        }
        t.push_str(&append.concat());
        Profile { text: t, params: shipped.params.clone() }
    };
    let ln = |profile: &Profile, src: &str, dst: &str| {
        output(
            Command::new(SANDBOX_EXEC)
                .args(profile.args())
                .args(["--", "/bin/ln", src, dst])
                .current_dir(&proj)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("TMPDIR", &tmp),
        )
    };
    let k = home.join(".ssh/known_hosts").to_string_lossy().into_owned();
    let f = shared.join("f").to_string_lossy().into_owned();
    let no_home_in_link = ("(deny file-link (subpath (param \"HOME\")) ", "(deny file-link ");
    let no_shared_in_link = (" (subpath \"/private/tmp\")", "");
    let protected: Vec<&str> = protected_links.iter().map(String::as_str).collect();
    let mut names_and_protected: Vec<&str> = vec![names_link.as_str()];
    names_and_protected.extend(&protected);
    let cases: Vec<(&str, &str, &str, Profile, Profile)> = vec![
        (
            "the home's link deny",
            k.as_str(),
            "x1",
            edit(&[&home_read], &[], &[home_write]),
            edit(&[&home_read], &[no_home_in_link], &[home_write]),
        ),
        (
            "the shared temporary folder's link deny",
            f.as_str(),
            "x2",
            edit(&[&shared_read], &[], &[shared_write]),
            edit(&[&shared_read], &[no_shared_in_link], &[shared_write]),
        ),
        (
            "the protected names' link deny, on the new name",
            "plain",
            "sub/AGENTS.md",
            edit(&[], &[], &[project_write]),
            edit(&[&names_link], &[], &[project_write]),
        ),
        (
            "the protected names' link deny, on the source",
            ".envrc",
            "x3",
            edit(&protected, &[], &[project_write]),
            edit(&names_and_protected, &[], &[project_write]),
        ),
        (
            "a protected path's link deny",
            ".envrc",
            "x4",
            edit(&[&names_link], &[], &[project_write]),
            edit(&names_and_protected, &[], &[project_write]),
        ),
    ];
    assert!(broad_link.contains("USER_TEMP"), "{broad_link}");
    for (rule, src, dst, alone, without) in cases {
        let out = ln(&alone, src, dst);
        assert!(
            out.code != Some(0) && out.err.contains("Operation not permitted") && !proj.join(dst).exists(),
            "{rule} didn't refuse ln {src} {dst} on its own: {out}"
        );
        let out = ln(&without, src, dst);
        assert!(out.code == Some(0) && proj.join(dst).exists(), "control, {rule} taken out: ln {src} {dst}: {out}");
        std::fs::remove_file(proj.join(dst)).unwrap();
    }
}

/// Round 7 (#183): round 6 showed `ln` refused for `file-link` on the source even with `(allow file-link)` last. Does an
/// unfiltered allow lose to an earlier filtered deny (then the home's link deny is what held the #178 proof)? And does
/// a link *named* like a protected entry need the protected-name link rule, or is it a write too?
#[test]
#[ignore = "a report for #183: run with --ignored --nocapture on macOS"]
fn probe_round7() {
    let mut report: Vec<String> = Vec::new();
    let mut note = |name: &str, what: String| {
        println!("PROBE {name}: {what}");
        report.push(format!("{name}: {what}"));
    };
    let mut fx = Fx::new("r7");
    // Minimal profiles: a filtered deny, then an unfiltered or a filtered allow after it.
    let base = fx.shared.join("links");
    let (a, b) = (base.join("A"), base.join("B"));
    for d in [&a, &b] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(a.join("f"), "a").unwrap();
    let deny_a = format!("(deny file-link (subpath \"{}\"))", a.display());
    for (i, (name, rules)) in [
        ("deny_A", deny_a.clone()),
        ("deny_A.allow_all", format!("{deny_a}\n(allow file-link)")),
        ("deny_A.allow_A", format!("{deny_a}\n(allow file-link (subpath \"{}\"))", a.display())),
        (
            "deny_A.allow_A_and_B",
            format!("{deny_a}\n(allow file-link (subpath \"{}\") (subpath \"{}\"))", a.display(), b.display()),
        ),
        ("deny_read_A.allow_read_all", format!("(deny file-read-data (subpath \"{}\"))\n(allow file-read-data)", a.display())),
    ]
    .into_iter()
    .enumerate()
    {
        let p = Profile { text: format!("(version 1)\n(allow default)\n{rules}\n"), params: vec![] };
        let script = format!(
            "cat '{}' && echo read-ok; ln '{}' '{}' && echo ln-ok",
            a.join("f").display(),
            a.join("f").display(),
            b.join(format!("g{i}")).display()
        );
        note(&format!("prec.{name}"), fx.sh(&p, &fx.shared, &script).to_string());
    }

    // The guards' fixture again (a fake home outside the user's temporary folder).
    let home = PathBuf::from(format!("/private/var/tmp/mx-probe-{}-r7home", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::create_dir_all(home.join("proj/sub")).unwrap();
    std::fs::write(home.join(".ssh/known_hosts"), "SECRET-SSH\n").unwrap();
    std::fs::write(home.join("proj/plain"), "plain\n").unwrap();
    fx.extra.push(home.clone());
    let proj = home.join("proj");
    let plan = {
        let mut sb = Sandbox::from_env(true);
        sb.home = home.clone();
        sb.path = std::env::var("PATH").unwrap_or_default();
        sb.plan(&proj, &CommandMounts::default())
    };
    let shipped = seatbelt::profile(&plan, &fx.run(), &RealFs).unwrap();
    let home_read = "(deny file-read* (subpath (param \"HOME\")))\n";
    let home_link = "(deny file-link (subpath (param \"HOME\")) ";
    let names_link = shipped.text.lines().find(|l| l.starts_with("(deny file-link (require-all")).unwrap_or_default().to_string();
    assert!(shipped.text.contains(home_read) && shipped.text.contains(home_link) && !names_link.is_empty());
    let edit = |f: &dyn Fn(String) -> String| Profile { text: f(shipped.text.clone()), params: shipped.params.clone() };
    let home_write = "(allow file-write* (subpath (param \"HOME\")))\n";
    let variants = [
        ("shipped", edit(&|t| t)),
        ("no_home_read.home_writable", edit(&|t| t.replace(home_read, "") + home_write)),
        (
            "no_home_read.no_home_link.home_writable",
            edit(&|t| t.replace(home_read, "").replace(home_link, "(deny file-link ") + home_write),
        ),
        (
            "no_home_read.allow_home_link_last.home_writable",
            edit(&|t| t.replace(home_read, "") + home_write + "(allow file-link (subpath (param \"HOME\")))\n"),
        ),
        ("no_names_link", edit(&|t| t.replace(&format!("{names_link}\n"), ""))),
    ];
    let k = home.join(".ssh/known_hosts");
    for (name, prof) in &variants {
        let script = format!(
            "cat '{k}' >/dev/null && echo read-ok; ln '{k}' ln-{name} && echo ln-home-ok; \
             ln plain sub/AGENTS.md && echo ln-to-agents-ok; ln plain .envrc && echo ln-to-envrc-ok; \
             ln plain \"$TMPDIR/AGENTS.md\" && echo ln-to-tmp-agents-ok; rm -f sub/AGENTS.md .envrc",
            k = k.display()
        );
        let (out, denials) = sandbox_denials(|| fx.sh(prof, &proj, &script));
        note(&format!("link7.{name}"), out.to_string());
        let denials: Vec<&String> = denials.iter().filter(|l| l.contains("file-")).collect();
        note(&format!("link7.{name}.denials"), denials.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" || "));
    }
    println!("\n===== REPORT =====\n{}\n===== END =====", report.join("\n"));
}

// ===== #152: a background process on the host's loopback (`docs/specs/2026-10-25-macos-processes.md` §7) =====
//
// What option A of the spec would have relied on, measured on the kernel: a profile that lets a process without network
// bind and accept on `localhost:*` (with or without a deny for each port below 1024), and a bash profile with one
// outbound rule per port. The 2026-10-25 probes (PR #242) showed that `(local ip "localhost:*")` doesn't keep a bind on
// loopback (the wildcard, the rest of 127/8 and the machine's own addresses all bind), so the process door stays off on
// macOS and no command's profile has these rules: they exist only here, built on top of the closed profile.
//
// `loopback_probe_report` (ignored, run by the macOS CI job with `--nocapture`) prints every probe, one `PROBE name: …`
// line each, and never fails on a result: an OS update that tightens Seatbelt shows up in the log.
//
// Run locally, the probes that bind the machine's own interface addresses may raise the application firewall's
// "accept incoming connections?" prompt when it's on (GitHub's runners have it off). `127.0.0.2` is probed only where
// lo0 has that alias (`sudo ifconfig lo0 alias 127.0.0.2 up`; CI adds it).

/// Binds each `addr@port` given (TCP, then `listen`), one line each: `addr@port ok <port>` or `addr@port err <ERRNO>`.
/// IPv6 sockets are dual-stack (`IPV6_V6ONLY` off, Node's default for `::`).
const BIND_PY: &str = r#"import errno, socket, sys
for spec in sys.argv[1:]:
    host, _, port = spec.rpartition("@")
    fam = socket.AF_INET6 if ":" in host else socket.AF_INET
    s = socket.socket(fam, socket.SOCK_STREAM)
    try:
        if fam == socket.AF_INET6:
            s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        s.bind((host, int(port)))
        s.listen(1)
        print(spec, "ok", s.getsockname()[1], flush=True)
    except OSError as e:
        print(spec, "err", errno.errorcode.get(e.errno, e.errno), flush=True)
    finally:
        s.close()
"#;

/// Connects to each `addr@port` given, from `--from <addr>` if given first: `addr@port ok` or `addr@port err <ERRNO>`.
const CONNECT_PY: &str = r#"import errno, socket, sys
args = sys.argv[1:]
src = None
if args and args[0] == "--from":
    src, args = args[1], args[2:]
for spec in args:
    host, _, port = spec.rpartition("@")
    fam = socket.AF_INET6 if ":" in host else socket.AF_INET
    s = socket.socket(fam, socket.SOCK_STREAM)
    s.settimeout(5)
    try:
        if src:
            s.bind((src, 0))
        s.connect((host, int(port)))
        print(spec, "ok", flush=True)
    except OSError as e:
        print(spec, "err", errno.errorcode.get(e.errno, e.errno) if e.errno else type(e).__name__, flush=True)
    finally:
        s.close()
"#;

/// A server on `127.0.0.1:0`: prints `port <n>`, then whether it can connect to itself (`self ok` / `self err <ERRNO>`),
/// then answers `hi` to every connection for 30 seconds.
const SERVE_PY: &str = r#"import errno, socket, time
l = socket.socket()
l.bind(("127.0.0.1", 0))
l.listen(8)
port = l.getsockname()[1]
print("port", port, flush=True)
c = socket.socket()
c.settimeout(5)
try:
    c.connect(("127.0.0.1", port))
    print("self ok", flush=True)
    l.accept()[0].close()
except OSError as e:
    print("self err", errno.errorcode.get(e.errno, e.errno) if e.errno else type(e).__name__, flush=True)
c.close()
l.settimeout(1)
end = time.time() + 30
while time.time() < end:
    try:
        a, _ = l.accept()
        a.sendall(b"hi\n")
        a.close()
    except socket.timeout:
        pass
"#;

impl Fx {
    /// The rules option A would have given a process without network, on top of the closed profile (the last
    /// matching rule wins): bind and accept on `localhost:*`, and with `low_denies`, each port below 1024 refused.
    fn listen_profile(&self, low_denies: bool) -> Profile {
        let closed = self.profile(false);
        let mut text = closed.text.clone();
        text.push_str("(allow network-bind (local ip \"localhost:*\"))\n(allow network-inbound (local ip \"localhost:*\"))\n");
        if low_denies {
            for port in 1..1024 {
                text.push_str(&format!("(deny network-bind (local ip \"localhost:{port}\"))\n"));
            }
        }
        Profile { text, params: closed.params }
    }

    /// A bash command's closed profile with an outbound rule for each of `ports` (`Invocation::ports`).
    fn bash_profile(&self, ports: &[u16]) -> Profile {
        let run = Invocation { ports: ports.to_vec(), ..self.run() };
        seatbelt::profile(&self.plan(&self.root, false), &run, &RealFs).unwrap()
    }

    /// Writes the probe scripts into the project (readable in every sandbox).
    fn scripts(&self) {
        for (name, text) in [("bind.py", BIND_PY), ("connect.py", CONNECT_PY), ("serve.py", SERVE_PY)] {
            std::fs::write(self.root.join(name), text).unwrap();
        }
    }

    /// `script` with `args`, sandboxed by `profile` (or not, with None), one result per line: `spec -> rest`.
    fn per_line(&self, profile: Option<&Profile>, script: &str, args: &[String]) -> Vec<(String, String)> {
        let out = match profile {
            Some(p) => {
                let quoted: Vec<String> = args.iter().map(|a| format!("'{a}'")).collect();
                self.sh(p, &self.root, &format!("python3 {script} {}", quoted.join(" ")))
            }
            None => output(Command::new("python3").arg(self.root.join(script)).args(args).current_dir(&self.root)),
        };
        let mut lines: Vec<(String, String)> =
            out.out.lines().filter_map(|l| l.split_once(' ').map(|(spec, rest)| (spec.to_string(), rest.to_string()))).collect();
        if lines.is_empty() {
            lines.push(("(none)".into(), out.to_string()));
        }
        lines
    }

    /// A sandboxed `python3 <script>` running in the background, in its own process group, stdout piped.
    fn spawn(&self, profile: &Profile, script: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt as _;
        Command::new(SANDBOX_EXEC)
            .args(profile.args())
            .args(["--", "/bin/bash", "-c", &format!("exec python3 {script}")])
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("TMPDIR", &self.tmp)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
}

/// The machine's own addresses on interfaces other than lo0, from `ifconfig` (IPv6 link-local ones with their
/// `%scope`), and lo0's addresses other than 127.0.0.1 and ::1.
fn interface_addresses() -> (Vec<String>, Vec<String>) {
    let out = output(&mut Command::new("/sbin/ifconfig"));
    let (mut others, mut lo0) = (Vec::new(), Vec::new());
    let mut iface = String::new();
    for line in out.out.lines() {
        if !line.starts_with(char::is_whitespace) {
            iface = line.split(':').next().unwrap_or_default().to_string();
            continue;
        }
        let mut words = line.split_whitespace();
        let addr = match (words.next(), words.next()) {
            (Some("inet" | "inet6"), Some(addr)) => addr.to_string(),
            _ => continue,
        };
        if iface == "lo0" {
            if addr != "127.0.0.1" && addr != "::1" {
                lo0.push(addr);
            }
        } else {
            others.push(addr);
        }
    }
    (others, lo0)
}

/// Probe 1's cases: (must bind, must be refused, recorded either way), as `addr@0`.
fn bind_cases() -> (Vec<String>, Vec<String>, Vec<String>) {
    let at0 = |a: &str| format!("{a}@0");
    let (others, lo0) = interface_addresses();
    let must_work = ["127.0.0.1", "::1"].map(at0).to_vec();
    let mut refused: Vec<String> = ["0.0.0.0", "::", "::ffff:0.0.0.0", "127.0.0.2"].map(at0).to_vec();
    refused.extend(others.iter().map(|a| at0(a)));
    let mut recorded: Vec<String> = vec![at0("::ffff:127.0.0.1")];
    recorded.extend(lo0.iter().filter(|a| *a != "127.0.0.2").map(|a| at0(a)));
    (must_work, refused, recorded)
}

/// A host listener on `addr` answering a fixed HTTP response, until the test ends.
fn http_listener(addr: &str) -> Option<u16> {
    use std::io::Read as _;
    let l = std::net::TcpListener::bind(addr).ok()?;
    let port = l.local_addr().ok()?.port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut s = s;
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
        }
    });
    Some(port)
}

/// A port free on both 127.0.0.1 and ::1 with a listener on each (of `make`), or on 127.0.0.1 alone.
fn dual_listener(make: fn(&str) -> Option<u16>) -> (u16, bool) {
    for _ in 0..20 {
        let Some(p) = make("127.0.0.1:0") else { continue };
        if make(&format!("[::1]:{p}")).is_some() {
            return (p, true);
        }
    }
    (make("127.0.0.1:0").unwrap(), false)
}

/// `sandbox-exec -p <profile> -- /usr/bin/true`'s median start-to-exit time over `n` runs, in milliseconds, and how
/// many runs failed (a profile the kernel refused).
fn median_start_ms(profiles: &[&Profile], n: usize) -> Vec<(f64, usize)> {
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); profiles.len()];
    let mut failed = vec![0usize; profiles.len()];
    // Interleaved, so drift on a shared runner hits each profile alike.
    for _ in 0..n {
        for (i, p) in profiles.iter().enumerate() {
            let start = std::time::Instant::now();
            let ok = Command::new(SANDBOX_EXEC).args(p.args()).args(["--", "/usr/bin/true"]).status().is_ok_and(|s| s.success());
            times[i].push(start.elapsed().as_secs_f64() * 1000.0);
            failed[i] += usize::from(!ok);
        }
    }
    times
        .into_iter()
        .zip(failed)
        .map(|(mut t, failed)| {
            t.sort_by(|a, b| a.total_cmp(b));
            (t[t.len() / 2], failed)
        })
        .collect()
}

/// The first line of a running child's stdout that starts with `prefix`, within 15 seconds.
fn read_line(lines: &mut impl Iterator<Item = std::io::Result<String>>, prefix: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        match lines.next() {
            Some(Ok(line)) if line.starts_with(prefix) => return Some(line),
            Some(Ok(_)) => continue,
            _ => return None,
        }
    }
    None
}

/// Connects from the host to `127.0.0.1:port` and reads the server's line.
fn host_reads(port: u16) -> String {
    use std::io::Read as _;
    match std::net::TcpStream::connect(("127.0.0.1", port)) {
        Ok(mut s) => {
            let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut buf = String::new();
            match s.read_to_string(&mut buf) {
                Ok(_) => format!("ok {:?}", buf.trim()),
                Err(e) => format!("read err {e}"),
            }
        }
        Err(e) => format!("err {e}"),
    }
}

#[test]
#[ignore = "a report for #152: run with --ignored --nocapture on macOS"]
fn loopback_probe_report() {
    let mut report: Vec<String> = Vec::new();
    let mut note = |name: &str, what: String| {
        println!("PROBE {name}: {what}");
        report.push(format!("{name}: {what}"));
    };
    note("sw_vers", output(&mut Command::new("/usr/bin/sw_vers")).to_string().replace("err=[]", ""));
    note("uname_m", output(Command::new("/usr/bin/uname").arg("-m")).out.trim().to_string());
    let fx = Fx::new("lo-report");
    fx.scripts();
    let process = fx.listen_profile(true);
    let closed = fx.profile(false);
    note("profile.process.accepted", fx.sh(&process, &fx.root, "echo ok").to_string());
    note("profile.process.size_bytes", process.text.len().to_string());
    let (others, lo0) = interface_addresses();
    note("ifaces.non_lo0", format!("{others:?}"));
    note("ifaces.lo0_extra", format!("{lo0:?}"));

    // --- 1. Binds under the listen rules: control (unsandboxed), the listen profile, and the closed profile.
    let (must_work, refused, recorded) = bind_cases();
    let all: Vec<String> = must_work.iter().chain(&refused).chain(&recorded).cloned().collect();
    let control = fx.per_line(None, "bind.py", &all);
    let sandboxed = fx.per_line(Some(&process), "bind.py", &all);
    let without = fx.per_line(Some(&closed), "bind.py", &all);
    let find = |rows: &[(String, String)], spec: &str| {
        rows.iter().find(|(s, _)| s == spec).map(|(_, r)| r.clone()).unwrap_or_else(|| format!("missing {rows:?}"))
    };
    for (group, specs) in [("must_bind", &must_work), ("must_refuse", &refused), ("recorded", &recorded)] {
        for spec in specs.iter() {
            note(
                &format!("1.bind.{group}.{spec}"),
                format!(
                    "unsandboxed=[{}] process=[{}] no_listen=[{}]",
                    find(&control, spec),
                    find(&sandboxed, spec),
                    find(&without, spec)
                ),
            );
        }
    }
    // IPv6-only sockets for the wildcard too (dual-stack above).
    note(
        "1.bind.v6only_wildcard",
        fx.sh(
            &process,
            &fx.root,
            "python3 -c 'import socket; s=socket.socket(socket.AF_INET6); s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1); s.bind((\"::\", 0)); print(\"bound\")'",
        )
        .to_string(),
    );
    // Node's `listen(port)` with no host (binds `::`), and with 127.0.0.1.
    let node_listen = |host: &str| {
        format!(
            "node -e 's=require(\"net\").createServer();s.on(\"error\",e=>{{console.log(\"err\",e.code,e.message);process.exit(1)}});s.listen({{port:0{host}}},()=>{{console.log(\"listening\",JSON.stringify(s.address()));process.exit(0)}})'"
        )
    };
    note("1.node.listen_no_host", fx.sh(&process, &fx.root, &node_listen("")).to_string());
    note("1.node.listen_127", fx.sh(&process, &fx.root, &node_listen(",host:\"127.0.0.1\"")).to_string());

    // --- 2. `(remote ip "localhost:<p>")` reaches both families, and nothing else.
    let (p, both) = dual_listener(tcp_listener);
    let q = tcp_listener("127.0.0.1:0").unwrap();
    let bash = fx.bash_profile(&[p]);
    note("2.listeners", format!("port={p} v6_too={both} unlisted={q}"));
    let mut targets = vec![format!("127.0.0.1@{p}"), format!("::1@{p}"), format!("127.0.0.1@{q}")];
    let alias_listener = tcp_listener(&format!("127.0.0.2:{p}")).is_some();
    note("2.alias_listener_127.0.0.2", alias_listener.to_string());
    if alias_listener {
        targets.push(format!("127.0.0.2@{p}"));
    }
    if let Some(v4) = others.iter().find(|a| !a.contains(':'))
        && tcp_listener(&format!("{v4}:{p}")).is_some()
    {
        targets.push(format!("{v4}@{p}"));
    }
    for (spec, result) in fx.per_line(Some(&bash), "connect.py", &targets) {
        note(&format!("2.bash_connect.{spec}"), result);
    }
    for (spec, result) in fx.per_line(None, "connect.py", &targets) {
        note(&format!("2.host_connect.{spec}"), result);
    }

    // --- 3. An unsandboxed user process binds below 1024.
    for spec in ["127.0.0.1@80", "::1@80", "127.0.0.1@1023", "0.0.0.0@80"] {
        note(&format!("3.unsandboxed.{spec}"), find(&fx.per_line(None, "bind.py", &[spec.to_string()]), spec));
    }

    // --- 4. The per-port denies, and what they cost at start.
    let low = ["127.0.0.1@80", "::1@80", "127.0.0.1@1", "127.0.0.1@1023", "::1@1023", "127.0.0.1@1024", "::1@1024"]
        .map(String::from)
        .to_vec();
    for (spec, result) in fx.per_line(Some(&process), "bind.py", &low) {
        note(&format!("4.process_bind.{spec}"), result);
    }
    let no_denies = fx.listen_profile(false);
    note("4.no_denies.size_bytes", no_denies.text.len().to_string());
    for (spec, result) in fx.per_line(Some(&no_denies), "bind.py", &low[..2]) {
        note(&format!("4.no_denies_bind.{spec}"), result);
    }
    let medians = median_start_ms(&[&closed, &no_denies, &process], 50);
    note(
        "4.start_ms_median_of_50",
        format!(
            "closed={:.1} listen_without_denies={:.1} listen_with_denies={:.1} (failed runs: {}/{}/{})",
            medians[0].0, medians[1].0, medians[2].0, medians[0].1, medians[1].1, medians[2].1
        ),
    );

    // --- 5 and 6. A sandboxed server: Marv reads its descriptors, the host reaches it, it can't reach itself.
    let mut server = fx.spawn(&process, "serve.py");
    let pid = server.id();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(server.stdout.take().unwrap()));
    match read_line(&mut lines, "port ").and_then(|l| l[5..].trim().parse::<u16>().ok()) {
        Some(port) => {
            note("6.self_connect", read_line(&mut lines, "self ").unwrap_or_else(|| "no line".into()));
            note("6.host_connect_127", host_reads(port));
            note("6.host_connect_v6", find(&fx.per_line(None, "connect.py", &[format!("::1@{port}")]), &format!("::1@{port}")));
            for src in others.iter().filter(|a| !a.contains(':')).take(2) {
                let spec = format!("127.0.0.1@{port}");
                let r = fx.per_line(None, "connect.py", &["--from".into(), src.clone(), spec.clone()]);
                note(&format!("6.host_connect_from.{src}"), find(&r, &spec));
                let spec = format!("{src}@{port}");
                note(
                    &format!("6.host_connect_to.{src}"),
                    find(&fx.per_line(None, "connect.py", std::slice::from_ref(&spec)), &spec),
                );
            }
            let lsof_g = output(Command::new("/usr/sbin/lsof").args([
                "-a",
                "-g",
                &pid.to_string(),
                "-iTCP",
                "-sTCP:LISTEN",
                "-Fn",
                "-P",
                "-n",
            ]));
            note("5.marv_reads_child.lsof_g", lsof_g.to_string());
            let lsof_p = output(Command::new("/usr/sbin/lsof").args([
                "-a",
                "-p",
                &pid.to_string(),
                "-iTCP",
                "-sTCP:LISTEN",
                "-Fn",
                "-P",
                "-n",
            ]));
            note("5.marv_reads_child.lsof_p", lsof_p.to_string());
            note(
                "5.child_reads_marv.lsof_p",
                fx.sh(&process, &fx.root, &format!("/usr/sbin/lsof -p {} -Fn -P -n | head -5", std::process::id())).to_string(),
            );
            note(
                "5.child_reads_marv.lsof_g",
                fx.sh(&process, &fx.root, &format!("/usr/sbin/lsof -a -g {pid} -iTCP -Fn -P -n | head -5")).to_string(),
            );
        }
        None => note("6.server", "didn't start".into()),
    }
    let _ = server.kill();
    let _ = server.wait();

    // --- 7. No socket folders under option A: nothing to probe.
    note("7.clean_tmps", "n/a: option A makes no per-process socket folder".into());

    // --- 8. `localhost` by name, without network.
    note(
        "8.process.getaddrinfo_localhost",
        fx.sh(
            &process,
            &fx.root,
            "python3 -c 'import socket; print(sorted({a[4][0] for a in socket.getaddrinfo(\"localhost\", 0)}))'",
        )
        .to_string(),
    );
    note(
        "8.process.python_bind_localhost",
        fx.sh(&process, &fx.root, "python3 -c 'import socket; s=socket.socket(); s.bind((\"localhost\", 0)); s.listen(1); print(\"bound\", s.getsockname())'")
            .to_string(),
    );
    note("8.process.node_listen_localhost", fx.sh(&process, &fx.root, &node_listen(",host:\"localhost\"")).to_string());
    let (h, h_both) = dual_listener(http_listener);
    let bash = fx.bash_profile(&[h]);
    note("8.http_listener", format!("port={h} v6_too={h_both}"));
    note(
        "8.bash.node_fetch_localhost",
        fx.sh(
            &bash,
            &fx.root,
            &format!("node -e 'fetch(\"http://localhost:{h}\").then(r=>r.text()).then(t=>console.log(\"fetched\",t)).catch(e=>console.log(\"err\",e.cause&&e.cause.code,e.message))'"),
        )
        .to_string(),
    );
    note(
        "8.bash.python_urllib_localhost",
        fx.sh(
            &bash,
            &fx.root,
            &format!("python3 -c 'import urllib.request; print(\"fetched\", urllib.request.urlopen(\"http://localhost:{h}\", timeout=5).read())'"),
        )
        .to_string(),
    );
    note(
        "8.bash.python_urllib_127",
        fx.sh(
            &bash,
            &fx.root,
            &format!("python3 -c 'import urllib.request; print(\"fetched\", urllib.request.urlopen(\"http://127.0.0.1:{h}\", timeout=5).read())'"),
        )
        .to_string(),
    );
    note("8.bash.curl_localhost", fx.sh(&bash, &fx.root, &format!("curl -sS -m 5 http://localhost:{h}")).to_string());
    println!("\n===== REPORT =====\n{}\n===== END =====", report.join("\n"));
}

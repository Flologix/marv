//! `/permissions` (marv-rs #82, item 4): what this session may do without asking, and what of that is saved.
//!
//! New in marv-rs (Marv has no such view). A read-only snapshot: [`crate::MarvSession::permissions`] copies the
//! session's grants (the "always" answers, the pre-approved scopes from the config's `"approve"` / `--approve`, yolo,
//! the sandbox, the doors offered, taint and the web prefixes approved after it) and [`permissions_text`] renders
//! them. Like every command it is handled locally: the text goes to the person's screen only, never into the history
//! or the system prompt (the prompt cache), and nothing here grants or revokes anything.
//!
//! Every scope key is shown through `printable()`: a remembered key holds the command the model wrote.

use std::path::PathBuf;

use marv_core::{Provenance, printable};
use marv_mcp::config::ServerSource;
use marv_mcp::manager::{McpServerStatus, McpState};

use crate::approvals::APPROVE_PREFIXES;

/// The session's grants, as they are now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Permissions {
    pub yolo: bool,
    /// Yolo isn't allowed here, and why (the project is the home folder or above it).
    pub no_yolo: Option<String>,
    pub sandbox: bool,
    /// Commands on the host (sandbox off), each asked.
    pub host_commands: bool,
    /// The doors the session offers: `github`, `process`, `herdr`.
    pub doors: Vec<&'static str>,
    /// What a person answered "always" to (exact scope keys), sorted.
    pub remembered: Vec<String>,
    /// The pre-approved scope names (#98): the config's `"approve"` and the run's `--approve`.
    pub pre_approved: Vec<String>,
    /// Extra writable sandbox folders (#98), resolved.
    pub writable: Vec<PathBuf>,
    /// What tainted the session (empty: clean).
    pub tainted: Vec<Provenance>,
    /// URL prefixes a person approved after taint.
    pub approved_urls: Vec<String>,
}

/// What the person's own config file saves (it outlasts the session). The desktop has none to show.
#[derive(Debug, Clone, Copy)]
pub struct SavedGrants<'a> {
    /// As shown ("~/.marv/config.json").
    pub path: &'a str,
    pub approve: &'a [String],
    pub writable: &'a [String],
}

/// What each `--approve` name covers, in words.
fn approve_meaning(name: &str) -> &'static str {
    match name {
        "git_write" => "git commands that write (commit, branch, …)",
        "network" => "commands that need the network",
        "web" => "web_fetch from any site",
        "mcp" => "every MCP tool",
        "files" => "file changes",
        "memory" => "memory changes",
        "agent_worktree" => "subagents in their own worktrees",
        "process" => "starting background processes (with the network only with `network` too)",
        "github" => "the github door's writes and sensitive reads",
        _ if name.starts_with("mesh:") => "calls on that mesh receptacle (once you've wired it)",
        _ => "",
    }
}

/// The groups the remembered keys are shown in, by key prefix; first match wins, so longer prefixes come first.
const GROUPS: &[(&str, &str)] = &[
    ("bash:net+git:", "Commands with network and git writes"),
    ("bash:net:", "Commands with the network"),
    ("bash:git:", "Git commands that write"),
    ("bash:host:", "Commands on the host"),
    ("bash::", "Commands in the sandbox"),
    ("web:", "Web fetches (by site)"),
    ("mcp:", "MCP tools (server:tool)"),
    ("github:", "github door"),
    ("process:net:", "process door, with the network"),
    ("process:", "process door"),
    ("herdr:", "herdr door"),
    ("mesh:", "Mesh calls"),
    ("files", "File changes"),
    ("memory", "Memory changes"),
    ("agent:worktree", "Subagents in worktrees"),
];

/// A key's group and the part worth showing ("bash:git:git commit" → the git group, "git commit").
fn group_of(key: &str) -> (&'static str, &str) {
    for (prefix, title) in GROUPS {
        if let Some(rest) = key.strip_prefix(prefix) {
            return (title, rest);
        }
    }
    ("Other", key)
}

/// A key for markdown: printable, in a code span it can't close.
fn code(text: &str) -> String {
    let text = printable(text).replace('`', "'").replace('\n', " ");
    if text.is_empty() { "(all)".into() } else { format!("`{text}`") }
}

fn list(items: &[String]) -> String {
    items.iter().map(|i| code(i)).collect::<Vec<_>>().join(", ")
}

/// The `/permissions` view, markdown. `saved`: the person's config file, where the client has one; `mcp`: the
/// servers as they are now (a project's server that runs was trusted, and that trust is saved).
pub fn permissions_text(p: &Permissions, saved: Option<SavedGrants>, mcp: Option<&[McpServerStatus]>) -> String {
    let mut parts = vec![
        "**Permissions**: what Marv may do without asking. This view is yours only: it never reaches the model.".to_string(),
    ];

    let yolo = match (&p.no_yolo, p.yolo) {
        (Some(why), _) => format!("off, not allowed here ({})", printable(why)),
        (None, true) => "on: what the sandbox confines runs without asking (/yolo off)".into(),
        (None, false) => "off (/yolo)".into(),
    };
    let sandbox = match (p.sandbox, p.host_commands) {
        (true, _) => "on".to_string(),
        (false, true) => "off: every command runs on your machine and asks".into(),
        (false, false) => "off: commands are refused".into(),
    };
    let net_seeded = p.pre_approved.iter().any(|n| n == "network");
    let net_remembered = p.remembered.iter().filter(|k| k.starts_with("bash:net")).count();
    let network = match (net_seeded, net_remembered) {
        (true, _) => "pre-approved (`network`): commands that need it run without asking".to_string(),
        (false, 0) => "a command that needs it asks each time".into(),
        (false, n) => format!("asked, except {n} command{} answered \"always\" (below)", if n == 1 { "" } else { "s" }),
    };
    let taint = if p.tainted.is_empty() {
        "no".to_string()
    } else {
        let by = p.tainted.iter().map(|s| format!("{s:?}").to_lowercase()).collect::<Vec<_>>().join(", ");
        format!("yes, by {by}: network and egress calls ask every time, whatever is granted below (even yolo)")
    };
    let doors = if p.doors.is_empty() { "none".to_string() } else { p.doors.join(", ") };
    parts.push(
        [
            format!("- **Yolo**: {yolo}"),
            format!("- **Sandbox**: {sandbox}"),
            format!("- **Network**: {network}"),
            format!("- **Doors**: {doors} (each asks, unless granted below)"),
            format!("- **Tainted**: {taint}"),
        ]
        .join("\n"),
    );

    let mut pre = vec!["**Pre-approved for this session** (the config's `approve`, `--approve`):".to_string()];
    if p.pre_approved.is_empty() {
        pre.push("- none".into());
    } else {
        for name in &p.pre_approved {
            let from = match saved {
                Some(s) if s.approve.contains(name) => format!(" (from {})", printable(s.path)),
                Some(_) => " (from --approve)".into(),
                None => String::new(),
            };
            let covers = APPROVE_PREFIXES
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, prefixes)| format!(" · `{}`", prefixes.join("`, `")))
                .unwrap_or_default();
            pre.push(format!("- {}{from}: {}{covers}", code(name), approve_meaning(name)));
        }
    }
    if !p.writable.is_empty() {
        let folders: Vec<String> = p.writable.iter().map(|w| w.display().to_string()).collect();
        pre.push(format!("- writable sandbox folders: {}", list(&folders)));
    }
    parts.push(pre.join("\n"));

    let mut always = vec!["**\"Always\" this session** (your answers, and links you pasted):".to_string()];
    if p.remembered.is_empty() {
        always.push("- none yet: answer \"always\" to an approval and it shows here".into());
    } else {
        let mut groups: Vec<(&str, Vec<String>)> = Vec::new();
        for key in &p.remembered {
            let (title, shown) = group_of(key);
            match groups.iter_mut().find(|(t, _)| *t == title) {
                Some((_, keys)) => keys.push(shown.to_string()),
                None => groups.push((title, vec![shown.to_string()])),
            }
        }
        // In the table's order, so the view reads the same every time.
        groups.sort_by_key(|(t, _)| GROUPS.iter().position(|(_, g)| g == t).unwrap_or(GROUPS.len()));
        always.extend(groups.iter().map(|(title, keys)| format!("- {title}: {}", list(keys))));
    }
    always.push(
        "- never remembered, always asked: tainted egress, protected paths, commands on the host, gh's sensitive reads, wires"
            .into(),
    );
    parts.push(always.join("\n"));

    if !p.approved_urls.is_empty() {
        parts.push(format!("**Web prefixes approved after taint**: {}", list(&p.approved_urls)));
    }

    let mut kept = vec!["**Saved** (outlasts the session):".to_string()];
    match saved {
        Some(s) if s.approve.is_empty() && s.writable.is_empty() => {
            kept.push(format!("- {}: no `approve` or `writable`", printable(s.path)))
        }
        Some(s) => {
            let mut what = Vec::new();
            if !s.approve.is_empty() {
                what.push(format!("`approve` {}", list(s.approve)));
            }
            if !s.writable.is_empty() {
                what.push(format!("`writable` {}", list(s.writable)));
            }
            kept.push(format!("- {}: {}", printable(s.path), what.join("; ")));
        }
        None => {}
    }
    let trusted: Vec<String> = mcp
        .unwrap_or_default()
        .iter()
        .filter(|s| s.source == ServerSource::Project && !matches!(s.state, McpState::Untrusted | McpState::Removed))
        .map(|s| s.name.clone())
        .collect();
    if !trusted.is_empty() {
        kept.push(format!("- this project's MCP servers you trusted: {} (~/.marv/mcp-trust.json)", list(&trusted)));
    }
    kept.push("- a project's own `.marv/config.json` can't pre-approve anything; \"always\" answers end with the session".into());
    parts.push(kept.join("\n"));
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn every_kind() -> Permissions {
        Permissions {
            yolo: false,
            sandbox: true,
            doors: vec!["github", "process", "herdr"],
            remembered: keys(&[
                "agent:worktree",
                "bash::cargo test",
                "bash:git:git commit -m x",
                "bash:net+git:git push",
                "bash:net:curl x",
                "files",
                "github:pr create",
                "herdr:read",
                "mcp:linear:search",
                "memory",
                "mesh:agent.parent.inbox:send",
                "process:bun run dev",
                "process:net:npm start",
                "web:docs.rs",
            ]),
            pre_approved: keys(&["web", "network", "mesh:agent.parent.inbox"]),
            ..Permissions::default()
        }
    }

    #[test]
    fn it_lists_each_grant_kind_in_its_group() {
        let text = permissions_text(&every_kind(), None, None);
        for line in [
            "- Commands in the sandbox: `cargo test`",
            "- Git commands that write: `git commit -m x`",
            "- Commands with the network: `curl x`",
            "- Commands with network and git writes: `git push`",
            "- Web fetches (by site): `docs.rs`",
            "- MCP tools (server:tool): `linear:search`",
            "- github door: `pr create`",
            "- process door: `bun run dev`",
            "- process door, with the network: `npm start`",
            "- herdr door: `read`",
            "- Mesh calls: `agent.parent.inbox:send`",
            "- File changes: (all)",
            "- Memory changes: (all)",
            "- Subagents in worktrees: (all)",
            "- `web`: web_fetch from any site · `web:`",
            "- `network`: commands that need the network · `bash:net:`",
            "- `mesh:agent.parent.inbox`: calls on that mesh receptacle",
            "- **Yolo**: off (/yolo)",
            "- **Sandbox**: on",
            "- **Network**: pre-approved (`network`)",
            "- **Doors**: github, process, herdr",
            "- **Tainted**: no",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
        // The groups come in the table's order, whatever the keys' order.
        let sandbox = text.find("Commands in the sandbox").unwrap();
        let network = text.find("- Commands with the network").unwrap();
        assert!(network < sandbox, "{text}");
    }

    #[test]
    fn an_empty_session_says_so_and_names_whats_saved() {
        let approve = keys(&["web"]);
        let writable = keys(&["~/scratch"]);
        let saved = SavedGrants { path: "~/.marv/config.json", approve: &approve, writable: &writable };
        let p = Permissions { yolo: true, sandbox: true, pre_approved: keys(&["web", "git_write"]), ..Permissions::default() };
        let text = permissions_text(&p, Some(saved), None);
        assert!(text.contains("none yet: answer \"always\""), "{text}");
        assert!(text.contains("**Yolo**: on"), "{text}");
        assert!(text.contains("- `web` (from ~/.marv/config.json)"), "{text}");
        assert!(text.contains("- `git_write` (from --approve)"), "{text}");
        assert!(text.contains("- ~/.marv/config.json: `approve` `web`; `writable` `~/scratch`"), "{text}");
        assert!(text.contains("**Network**: a command that needs it asks each time"), "{text}");
        assert!(text.contains("**Doors**: none"), "{text}");
        let none = permissions_text(&Permissions::default(), Some(SavedGrants { approve: &[], writable: &[], ..saved }), None);
        assert!(none.contains("- none\n") && none.contains("no `approve` or `writable`"), "{none}");
    }

    #[test]
    fn taint_no_yolo_and_trusted_project_servers_show() {
        let p = Permissions {
            no_yolo: Some("the project is your home folder".into()),
            tainted: vec![Provenance::Web, Provenance::Mcp],
            approved_urls: keys(&["https://docs.rs/regex/"]),
            remembered: keys(&["bash:net:curl a", "bash:net:curl b"]),
            ..Permissions::default()
        };
        let server = |name: &str, source, state| McpServerStatus {
            name: name.into(),
            source,
            target: String::new(),
            reads: vec![],
            runs_project_files: vec![],
            state,
            tools: vec![],
            error: None,
            stderr: None,
            skipped: vec![],
            login: None,
        };
        let servers = [
            server("trusted", ServerSource::Project, McpState::Connected),
            server("waiting", ServerSource::Project, McpState::Untrusted),
            server("mine", ServerSource::Personal, McpState::Connected),
        ];
        let text = permissions_text(&p, None, Some(&servers));
        assert!(text.contains("off, not allowed here (the project is your home folder)"), "{text}");
        assert!(text.contains("**Tainted**: yes, by web, mcp"), "{text}");
        assert!(text.contains("**Web prefixes approved after taint**: `https://docs.rs/regex/`"), "{text}");
        assert!(text.contains("asked, except 2 commands answered \"always\""), "{text}");
        assert!(text.contains("MCP servers you trusted: `trusted` ("), "{text}");
        assert!(!text.contains("`waiting`") && !text.contains("`mine`"), "{text}");
        assert!(!text.contains("**Saved** (outlasts the session):\n- ~"), "no config file to show: {text}");
    }

    #[test]
    fn a_remembered_command_cant_reach_the_terminal_raw_or_break_its_code_span() {
        let p = Permissions { remembered: keys(&["bash::echo `x`\u{1b}[2J\nrm"]), ..Permissions::default() };
        let text = permissions_text(&p, None, None);
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(text.contains("`echo 'x'"), "{text}");
    }
}

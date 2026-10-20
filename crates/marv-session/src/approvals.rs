//! Pre-approved scopes: the names `marv -p --approve` (and, since #98, the config file's `"approve"` and the TUI's
//! `--approve`) take, and what each name covers.
//!
//! The list is here (not in marv-cli) because two sides need it: the headless approver (`build_approver`) and the
//! session's "always" scopes, which the config's scopes seed. Reusing one list is the point — a scope name means the
//! same thing in `-p`, in the TUI and in `~/.marv/config.json`.
//!
//! What a config-scope seeding means (#98): "always, for this session", for everything whose approval scope key
//! starts with one of the name's prefixes — through the same path a person's "always" answer takes
//! (`AlwaysScopes`), so `must_ask` (tainted egress, protected paths) and `once` (host commands, wires) still win.
//! It never grants a mesh wire: wires are a person's grant (`docs/MESH.md`), whatever the config says. And it never
//! covers `process:net:` without `network`: a process with the network shares the host's, as headless has always
//! required both scopes; likewise a command with both network and git_write (`bash:net+git:`) needs both
//! `git_write` and `network` ([`prefix_covers`]).
//!
//! Subagents get the seeds through `AlwaysScopes::for_subagent`, which strips the egress prefixes (`NOT_INHERITED`),
//! so a worker doesn't inherit the config's network, web, MCP or mesh scopes.

use marv_mesh::ReceptacleName;
use marv_mesh::approval::parse_approve_flag;

/// Known `--approve` names and the scope-key prefixes they match.
pub const APPROVE_PREFIXES: [(&str, &[&str]); 9] = [
    // A command with network and git_write (`bash:net+git:`, a `git push`) needs `network` too: `prefix_covers`.
    ("git_write", &["bash:git:"]),
    ("network", &["bash:net:"]),
    ("web", &["web:"]),
    ("mcp", &["mcp:"]),
    ("files", &["files"]),
    ("memory", &["memory"]),
    ("agent_worktree", &["agent:worktree"]),
    ("process", &["process:"]),
    // The github tool's writes and sensitive reads (#47); its reads need nothing.
    ("github", &["github:"]),
];

/// Whether `prefixes` (from `--approve`, or the config's seeded "always" scopes) answer a request with scope key
/// `key`. One shared rule, so `-p` and the seeded TUI can't drift:
/// - the listed prefixes cover their keys;
/// - `process:net:` (a background process with the host's network) needs both `process` and `network`;
/// - `bash:net+git:` (a command with network and git_write) needs both `git_write` and `network`.
///
/// Bash keys are `bash:<flags>:<command>` with the flag segment always present, so a command's text never reads
/// as a flag segment (`marv_tools::bash`).
pub fn prefix_covers(prefixes: &[String], key: &str) -> bool {
    let has = |prefix: &str| prefixes.iter().any(|p| p == prefix);
    if key.starts_with("process:net:") {
        return has("process:") && has("bash:net:");
    }
    if key.starts_with("bash:net+git:") {
        return has("bash:git:") && has("bash:net:");
    }
    prefixes.iter().any(|p| key == p || key.starts_with(p.as_str()))
}

/// The scope-key prefixes the config's (or the TUI's `--approve`) names seed as "always" for the session: the
/// non-mesh names' prefixes, plus `mesh:<r>:` for each `mesh:<receptacle>` (its calls, once a person has wired it —
/// never the wire itself). An unknown name is refused (marv -p exits 1; the TUI shows it and ignores it), so a
/// typo never silently means "approve everything".
pub fn seed_prefixes(names: &[String]) -> Result<Vec<String>, String> {
    let (mesh, prefixes) = parse_approvals(names)?;
    let mut prefixes = prefixes;
    prefixes.extend(mesh.iter().map(|r| format!("mesh:{r}:")));
    Ok(prefixes)
}

/// The names that mean something, each once, and a problem (printable) for each that doesn't: an unknown name is
/// dropped, never a reason to approve more, and never a reason to drop the good ones with it. `from` says where the
/// names came from in the problem ("--approve", "~/.marv/config.json's \"approve\"").
pub fn known_names(names: &[String], from: &str) -> (Vec<String>, Vec<String>) {
    let mut known: Vec<String> = Vec::new();
    let mut problems = Vec::new();
    for name in names {
        match parse_approvals(std::slice::from_ref(name)) {
            Ok(_) if !known.contains(name) => known.push(name.clone()),
            Ok(_) => {}
            Err(problem) => problems.push(marv_core::printable(&format!("{from}: {problem}; ignored"))),
        }
    }
    (known, problems)
}

/// The names split: `mesh:<receptacle>` grants (for headless's `HeadlessWires`), and the other names' scope-key
/// prefixes. `marv -p` (headless) and the config's seeds both build on this.
pub fn parse_approvals(names: &[String]) -> Result<(Vec<ReceptacleName>, Vec<String>), String> {
    let mut receptacles = Vec::new();
    let mut prefixes = Vec::new();
    for name in names {
        match parse_approve_flag(name) {
            Some(Ok(receptacle)) => receptacles.push(receptacle),
            Some(Err(err)) => return Err(format!("--approve {name}: {err}")),
            None => match APPROVE_PREFIXES.iter().find(|(n, _)| n == name) {
                Some((_, scope_prefixes)) => prefixes.extend(scope_prefixes.iter().map(|s| s.to_string())),
                None => {
                    let known: Vec<&str> = APPROVE_PREFIXES.iter().map(|(n, _)| *n).collect();
                    return Err(format!("Unknown --approve scope \"{name}\". Known: {}, mesh:<receptacle>", known.join(", ")));
                }
            },
        }
    }
    Ok((receptacles, prefixes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_unknown_name_is_dropped_with_a_problem_and_the_others_kept() {
        let (known, problems) = known_names(&names(&["web", "netwrk", "web", "mesh:Not A Name", "network"]), "config");
        assert_eq!(known, ["web", "network"]);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].starts_with("config: Unknown --approve scope \"netwrk\"") && problems[0].ends_with("ignored"));
        // What's left always seeds: nothing unknown can make the seeding fail (and so approve nothing, or more).
        assert!(seed_prefixes(&known).is_ok());
        let (_, problems) = known_names(&names(&["x\u{1b}[2J"]), "config");
        assert!(!problems[0].contains('\u{1b}'), "{problems:?}");
    }

    #[test]
    fn known_names_expand_to_their_prefixes() {
        let (mesh, prefixes) = parse_approvals(&names(&["git_write", "web"])).unwrap();
        assert!(mesh.is_empty());
        assert_eq!(prefixes, ["bash:git:", "web:"]);
    }

    #[test]
    fn unknown_names_are_refused() {
        let err = parse_approvals(&names(&["custom_scope"])).unwrap_err();
        assert!(err.contains("Unknown --approve scope \"custom_scope\""), "{err}");
        // mesh:<receptacle> is split off, not expanded, and still checked for its name.
        let (mesh, prefixes) = parse_approvals(&names(&["mesh:agent.parent.inbox"])).unwrap();
        assert_eq!(mesh.len(), 1);
        assert!(prefixes.is_empty());
        assert!(parse_approvals(&names(&["mesh:Not A Name"])).is_err());
    }

    #[test]
    fn seeds_cover_their_keys_like_headless_prefixes() {
        let seeds = seed_prefixes(&names(&["web", "network", "process", "mesh:agent.parent.inbox"])).unwrap();
        assert!(prefix_covers(&seeds, "web:docs.rs"));
        assert!(prefix_covers(&seeds, "bash:net:curl"));
        assert!(prefix_covers(&seeds, "process:bun run dev"));
        // A process with the network needs both process and network.
        assert!(prefix_covers(&seeds, "process:net:sleep 9"));
        // …and one of them alone doesn't answer it.
        let only_process = seed_prefixes(&names(&["process"])).unwrap();
        assert!(!prefix_covers(&only_process, "process:net:sleep 9"));
        // The mesh seeds cover its calls, nothing else.
        assert!(prefix_covers(&seeds, "mesh:agent.parent.inbox:send"));
        assert!(!prefix_covers(&seeds, "mesh:agent.other.inbox:send"));
        assert!(!prefix_covers(&seeds, "github:pr merge"));
    }

    #[test]
    fn git_write_alone_never_covers_a_network_command() {
        let git = seed_prefixes(&names(&["git_write"])).unwrap();
        assert!(prefix_covers(&git, "bash:git:git commit -m x"));
        assert!(!prefix_covers(&git, "bash:net+git:git push"));
        assert!(!prefix_covers(&git, "bash:net:git:;curl x"));
        assert!(!prefix_covers(&git, "bash:net:curl x"));
        let net = seed_prefixes(&names(&["network"])).unwrap();
        assert!(prefix_covers(&net, "bash:net:curl x"));
        assert!(!prefix_covers(&net, "bash:net+git:git push"));
        let both = seed_prefixes(&names(&["git_write", "network"])).unwrap();
        assert!(prefix_covers(&both, "bash:net+git:git push"));
        // A command whose text starts like a flag is still a command without flags.
        assert!(!prefix_covers(&both, "bash::git:x") && !prefix_covers(&both, "bash::net:x"));
    }
}

//! A GitHub repository as a source. Ports `src/mcp/install/github.ts`.
//!
//! Its README *nominates* a published package or a URL, and the resolver
//! decides. Only a package name, a URL and env variable *names* leave this
//! module: the rest of a README's config block (extra args, env values) is
//! dropped, never copied, since a README is stranger-written and its text never
//! reaches the screen unfiltered or the model at all. Several candidates → the
//! `/mcp add` commands to choose from; a package whose own repository isn't
//! this repo is flagged first. Cloning and building isn't supported (a build
//! can do anything).
//!
//! marv-rs: Marv read the README's code blocks with `marked`; here a small
//! fence reader ([`fenced_blocks`]) finds the ```` ``` ```` and `~~~` blocks,
//! which is where READMEs put their config. JSON objects keep their key order
//! (the servers are offered in the README's order).

use super::Resolved;
use super::npm::resolve_npm;
use super::proposal::{Choices, Proposal, ResolveDeps, ResolveOptions, fact, http_proposal};
use super::pypi::resolve_pypi;
use super::source::{Source, parse_source, var_name};
use super::util::{Ordered, config, name_for};
use crate::text::truncate;

/// A README's command lines and URLs are shown at most this long.
const MAX_SHOWN: usize = 80;

/// One server a README nominates, or a line Marv can't install (shown short).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidate {
    Source { source: Source, env_hints: Vec<String> },
    Unsupported(String),
}

/// The contents of a Markdown document's fenced code blocks, in order. A fence is three or more backticks or
/// tildes (backtick fences' info strings have no backtick); it closes on a line of at least as many of the same
/// character, or at the end of the document (CommonMark). Indented fences (in list items) count too.
pub fn fenced_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<(char, usize, usize, Vec<&str>)> = None;
    for line in markdown.lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let trimmed = &line[indent..];
        let fence = |c: char| trimmed.chars().take_while(|x| *x == c).count();
        match &mut open {
            None => {
                for c in ['`', '~'] {
                    let n = fence(c);
                    if n >= 3 && !(c == '`' && trimmed[n..].contains('`')) {
                        open = Some((c, n, indent, Vec::new()));
                        break;
                    }
                }
            }
            Some((c, n, opened_at, lines)) => {
                let m = fence(*c);
                if m >= *n && trimmed[m..].trim().is_empty() {
                    blocks.push(lines.join("\n"));
                    open = None;
                } else {
                    // The content loses as much indentation as the opening fence had.
                    let strip = indent.min(*opened_at);
                    lines.push(&line[strip..]);
                }
            }
        }
    }
    if let Some((_, _, _, lines)) = open {
        blocks.push(lines.join("\n"));
    }
    blocks
}

/// The server entries in one parsed block: Claude's `{"mcpServers": …}`, VS Code's `{"servers": …}` or
/// `{"mcp": {"servers": …}}`, or one entry.
fn entries_in(data: &Ordered) -> Vec<&Ordered> {
    if data.as_object().is_none() {
        return vec![];
    }
    let maps = [data.get("mcpServers"), data.get("servers"), data.get("mcp").and_then(|m| m.get("servers"))];
    if let Some(map) = maps.into_iter().flatten().find_map(Ordered::as_object) {
        return map.iter().map(|(_, v)| v).filter(|v| v.as_object().is_some()).collect();
    }
    if data.get("command").is_some() || data.get("url").is_some() { vec![data] } else { vec![] }
}

/// The package an `npx`/`uvx` line runs: its first word that isn't a flag. `uvx --from x cmd` runs a command, not
/// a package: unsupported.
fn package_arg(args: &[String]) -> Option<&str> {
    if args.iter().any(|a| a == "--from") {
        return None;
    }
    args.iter().find(|a| !a.starts_with('-')).map(String::as_str)
}

fn candidate_of(entry: &Ordered) -> Option<Candidate> {
    let env_hints: Vec<String> = entry
        .get("env")
        .and_then(Ordered::as_object)
        .unwrap_or_default()
        .iter()
        .map(|(k, _)| k.clone())
        .filter(|k| var_name(k))
        .collect();
    let command = entry.get("command").and_then(Ordered::as_str);
    let args: Vec<String> = match entry.get("args") {
        Some(Ordered::Array(a)) => a.iter().filter_map(Ordered::as_str).map(str::to_string).collect(),
        _ => vec![],
    };
    let line = |command: &str| {
        truncate(&std::iter::once(command.to_string()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" "), MAX_SHOWN)
    };
    if let Some(runner @ ("npx" | "uvx")) = command {
        let registry = if runner == "npx" { "npm" } else { "pypi" };
        let source = package_arg(&args).and_then(|pkg| parse_source(&format!("{registry}:{pkg}")).ok());
        return Some(match source {
            Some(source) => Candidate::Source { source, env_hints },
            None => Candidate::Unsupported(line(runner)),
        });
    }
    let kind = entry.get("type");
    if let Some(url) = entry.get("url").and_then(Ordered::as_str)
        && (kind.is_none() || matches!(kind.and_then(Ordered::as_str), Some("http" | "streamable-http")))
    {
        return Some(match parse_source(url) {
            Ok(source @ Source::Http { .. }) if url.starts_with("https://") => Candidate::Source { source, env_hints: vec![] },
            _ => Candidate::Unsupported(truncate(url, MAX_SHOWN)),
        });
    }
    command.map(|c| Candidate::Unsupported(line(c)))
}

/// What a README's fenced JSON blocks nominate (Claude's `mcpServers`, VS Code's
/// `servers` or `mcp.servers`, or one entry), deduplicated, in order.
pub fn readme_candidates(markdown: &str) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for block in fenced_blocks(markdown) {
        let Ok(data) = serde_json::from_str::<Ordered>(&block) else { continue };
        for entry in entries_in(&data) {
            let Some(candidate) = candidate_of(entry) else { continue };
            let key = match &candidate {
                Candidate::Source { source, .. } => format!("s{}", serde_json::to_string(source).unwrap_or_default()),
                Candidate::Unsupported(text) => format!("u{text}"),
            };
            if seen.insert(key) {
                found.push(candidate);
            }
        }
    }
    found
}

/// The text that, typed after `/mcp add`, means this source.
pub fn source_text(source: &Source) -> String {
    let at = |v: &Option<String>| v.as_ref().map(|v| format!("@{v}")).unwrap_or_default();
    match source {
        Source::Npm { name, version } => format!("{name}{}", at(version)),
        Source::Pypi { name, version } => format!("pypi:{name}{}", at(version)),
        Source::Http { url } => url.clone(),
        Source::Github { owner, repo } => format!("https://github.com/{owner}/{repo}"),
        Source::Registry { name } => name.clone(),
    }
}

pub async fn resolve_github(
    owner: &str,
    repo_name: &str,
    opts: &ResolveOptions,
    deps: &dyn ResolveDeps,
) -> crate::Result<Resolved> {
    let repo = format!("github.com/{owner}/{repo_name}");
    let Some(readme) = deps.readme(owner, repo_name, &deps.cancel()).await? else {
        return Err(config(format!(
            "{repo} has no README (or doesn't exist): add the server by its npm or PyPI package, or its URL."
        )));
    };
    let candidates = readme_candidates(&readme);
    let usable: Vec<(&Source, &Vec<String>)> = candidates
        .iter()
        .filter_map(|c| match c {
            Candidate::Source { source, env_hints } => Some((source, env_hints)),
            Candidate::Unsupported(_) => None,
        })
        .collect();
    let Some(&(nominated, env_hints)) = usable.first() else {
        let others: Vec<String> = candidates
            .iter()
            .filter_map(|c| if let Candidate::Unsupported(t) = c { Some(format!("`{t}`")) } else { None })
            .collect();
        let named = if others.is_empty() {
            String::new()
        } else {
            format!(" It shows {}: that needs building or Docker, which /mcp add doesn't do yet.", others.join(", "))
        };
        return Err(config(format!("{repo}'s README names no npm or PyPI package or https URL to add.{named}")));
    };
    if usable.len() > 1 {
        return Ok(Resolved::Choices(Choices {
            choices: usable.iter().map(|(s, _)| format!("/mcp add {}", source_text(s))).collect(),
        }));
    }

    let name = name_for(opts, nominated)?;
    let named_opts = ResolveOptions { name: Some(name.clone()), ..opts.clone() };
    let (proposal, repository) = match nominated {
        Source::Npm { name: pkg, version } => resolve_npm(pkg, version.as_deref(), &named_opts, deps).await?,
        Source::Pypi { name: pkg, version } => resolve_pypi(pkg, version.as_deref(), &named_opts, deps).await?,
        Source::Http { url } => (http_proposal(url, name, &opts.headers), None),
        _ => return Err(config(format!("{repo}'s README names something /mcp add can't follow."))),
    };

    let Proposal { name, entry, facts: own_facts, warnings: own_warnings } = proposal;
    let mut facts = vec![fact("github", &format!("the README of {repo} names {}", source_text(nominated)))];
    facts.extend(own_facts);
    let hints: Vec<&str> = env_hints.iter().map(String::as_str).filter(|h| !opts.env.iter().any(|e| e == h)).collect();
    if let Some(first) = hints.first() {
        facts.push(fact("readme", &format!("its example sets {}: not written unless you pass --env {first}", hints.join(", "))));
    }
    let mut warnings = own_warnings;
    let own = repository.as_ref().map(|r| r.to_lowercase());
    let expected = repo.to_lowercase();
    let matches = own.as_ref().is_some_and(|o| *o == expected || o.starts_with(&format!("{expected}/")));
    if !matches!(nominated, Source::Http { .. }) && !matches {
        let says = match &repository {
            Some(r) => format!("says its repository is {r}"),
            None => "names no repository".into(),
        };
        // First, where it's seen first: the README may nominate a package someone else publishes.
        facts.insert(0, fact("mismatch", &format!("the package {says}, not {repo}")));
        warnings.insert(0, format!("The package {says}, not {repo}: make sure it's the one this README means."));
    }
    Ok(Resolved::Proposal(Proposal { name, entry, facts, warnings }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::McpEntry;
    use crate::install::util::fake::Fake;

    fn npm(name: &str) -> Source {
        Source::Npm { name: name.into(), version: None }
    }

    fn readme(md: &str) -> Fake {
        Fake { readme: Some(Some(md.into())), ..Fake::default() }
    }

    fn block(servers: serde_json::Value) -> String {
        format!("```json\n{}\n```", serde_json::json!({ "mcpServers": servers }))
    }

    async fn resolve(owner: &str, repo: &str, deps: &Fake) -> crate::Result<Resolved> {
        resolve_github(owner, repo, &ResolveOptions::default(), deps).await
    }

    fn proposal(r: Resolved) -> Proposal {
        match r {
            Resolved::Proposal(p) => p,
            Resolved::Choices(c) => panic!("choices: {c:?}"),
        }
    }

    #[test]
    fn source_text_round_trips() {
        for text in ["@s/p@1.0.0", "pypi:x@2", "https://x.example/mcp", "https://github.com/o/r", "io.github.o/x"] {
            assert_eq!(source_text(&parse_source(text).unwrap()), text);
        }
    }

    #[test]
    fn fences_of_backticks_and_tildes_indented_or_unclosed() {
        let md =
            "text\n```json\n{\"a\": 1}\n```\n  ~~~~\n  x\n   ~~~ not a close\n  ~~~~~\n```js `bad`\n```\nlast\n````\n~~~\nopen";
        assert_eq!(fenced_blocks(md), ["{\"a\": 1}", "x\n ~~~ not a close", "last", "open"]);
    }

    #[test]
    fn takes_only_the_package_name_or_url_from_a_readmes_mcp_servers_block() {
        let md = "```json\n{\"mcpServers\":{\"wx\":{\"command\":\"npx\",\"args\":[\"-y\",\"weather-mcp\",\"--evil\"],\"env\":{\"WX_KEY\":\"sk-x\"}}}}\n```";
        assert_eq!(readme_candidates(md), [Candidate::Source { source: npm("weather-mcp"), env_hints: vec!["WX_KEY".into()] }]);
    }

    #[test]
    fn uvx_and_https_entries_while_docker_and_local_builds_are_reported_not_used() {
        let md = [
            "# Weather",
            "```json\n{\"mcpServers\":{\"a\":{\"command\":\"uvx\",\"args\":[\"mcp-server-fetch\"]}}}\n```",
            "```jsonc\n{\"servers\":{\"b\":{\"type\":\"http\",\"url\":\"https://mcp.example.com/mcp\"}}}\n```",
            "```json\n{\"mcpServers\":{\"c\":{\"command\":\"docker\",\"args\":[\"run\",\"-i\",\"acme/wx\"]}}}\n```",
            "```json\n{\"command\":\"node\",\"args\":[\"build/index.js\"]}\n```",
            "```json\nnot json\n```",
            "```json\n{\"mcpServers\":{\"d\":{\"type\":\"http\",\"url\":\"http://example.com/mcp\"}}}\n```",
        ]
        .join("\n\n");
        assert_eq!(
            readme_candidates(&md),
            [
                Candidate::Source { source: Source::Pypi { name: "mcp-server-fetch".into(), version: None }, env_hints: vec![] },
                Candidate::Source { source: Source::Http { url: "https://mcp.example.com/mcp".into() }, env_hints: vec![] },
                Candidate::Unsupported("docker run -i acme/wx".into()),
                Candidate::Unsupported("node build/index.js".into()),
                Candidate::Unsupported("http://example.com/mcp".into()),
            ]
        );
    }

    #[test]
    fn servers_are_offered_in_the_readmes_order() {
        let md = "```json\n{\"mcpServers\":{\"z\":{\"command\":\"npx\",\"args\":[\"zeta\"]},\"a\":{\"command\":\"npx\",\"args\":[\"alpha\"]}}}\n```";
        let names: Vec<String> = readme_candidates(md)
            .into_iter()
            .map(|c| if let Candidate::Source { source, .. } = c { source_text(&source) } else { String::new() })
            .collect();
        assert_eq!(names, ["zeta", "alpha"]);
    }

    #[tokio::test]
    async fn one_candidate_gives_its_proposal_with_a_repository_mismatch_first() {
        let md = block(
            serde_json::json!({"wx": {"command": "npx", "args": ["-y", "weather-mcp", "--evil"], "env": {"WX_KEY": "sk-x"}}}),
        );
        let p = proposal(resolve("acme", "weather-mcp", &readme(&md)).await.unwrap());
        // --evil and sk-x dropped
        assert_eq!(
            p.entry,
            McpEntry::Stdio { command: "npx".into(), args: vec!["-y".into(), "weather-mcp@0.2.0".into()], env: None }
        );
        assert!(
            p.facts[0].starts_with(
                "mismatch the package says its repository is github.com/someone-else/weather, not github.com/acme/weather-mcp"
            ),
            "{}",
            p.facts[0]
        );
        assert!(p.warnings[0].contains("someone-else"));
        assert!(p.facts.join("\n").contains("WX_KEY: not written unless you pass --env WX_KEY"));
        assert!(!format!("{p:?}").contains("sk-x"));
    }

    #[tokio::test]
    async fn a_matching_repository_isnt_flagged() {
        let md = block(serde_json::json!({"f": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem"]}}));
        let p = proposal(resolve("modelcontextprotocol", "servers", &readme(&md)).await.unwrap());
        assert!(p.facts[0].starts_with(
            "github   the README of github.com/modelcontextprotocol/servers names @modelcontextprotocol/server-filesystem"
        ));
        assert!(p.warnings.is_empty());
    }

    #[tokio::test]
    async fn several_give_the_commands_to_choose_from() {
        let md = block(
            serde_json::json!({"a": {"command": "npx", "args": ["-y", "weather-mcp"]}, "b": {"command": "uvx", "args": ["mcp-server-fetch"]}}),
        );
        assert_eq!(
            resolve("acme", "weather-mcp", &readme(&md)).await.unwrap(),
            Resolved::Choices(Choices { choices: vec!["/mcp add weather-mcp".into(), "/mcp add pypi:mcp-server-fetch".into()] })
        );
    }

    #[tokio::test]
    async fn none_is_an_error_naming_what_was_found() {
        let md = block(serde_json::json!({"c": {"command": "docker", "args": ["run", "acme/wx"]}}));
        let e = resolve("acme", "weather-mcp", &readme(&md)).await.unwrap_err().to_string();
        assert!(e.contains("`docker run acme/wx`: that needs building or Docker"), "{e}");
        let missing = Fake { readme: Some(None), ..Fake::default() };
        assert!(resolve("acme", "weather-mcp", &missing).await.unwrap_err().to_string().contains("has no README"));
    }
}

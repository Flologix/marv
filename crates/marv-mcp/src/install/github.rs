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

use super::proposal::{Choices, Proposal, ResolveDeps, ResolveOptions};
use super::source::Source;

/// One server a README nominates, or a line Marv can't install (shown short).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidate {
    Source { source: Source, env_hints: Vec<String> },
    Unsupported(String),
}

/// What a README's fenced JSON blocks nominate (Claude's `mcpServers`, VS Code's
/// `servers` or `mcp.servers`, or one entry), deduplicated, in order.
pub fn readme_candidates(_markdown: &str) -> Vec<Candidate> {
    todo!("port: readmeCandidates (fenced code blocks → JSON → entriesIn → candidateOf)")
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
    _owner: &str,
    _repo: &str,
    _opts: &ResolveOptions,
    _deps: &dyn ResolveDeps,
) -> crate::Result<Result<Proposal, Choices>> {
    todo!("port: resolveGithub")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::source::parse_source;

    #[test]
    fn source_text_round_trips() {
        for text in ["@s/p@1.0.0", "pypi:x@2", "https://x.example/mcp", "https://github.com/o/r", "io.github.o/x"] {
            assert_eq!(source_text(&parse_source(text).unwrap()), text);
        }
    }
}

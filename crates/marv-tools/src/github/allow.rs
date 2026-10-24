//! What the github tool lets gh do (#47): pure, so every rule is a table test. gh runs outside the sandbox with the
//! user's login, so its argv is read the way gh reads it (the command path first, then flags) and sorted into what
//! runs freely (reads), what asks (writes, and reads of sensitive API paths), and what never runs (anything that
//! would show the token, run a program, open a browser, run git, write files, or reach a host the project doesn't
//! name). Anything not listed is refused, with the list of what runs.
//!
//! Ports Marv's `src/tools/github/allow.ts`. Rust's `regex` has no lookahead: `origin_host`'s scp form checks the
//! "not `//`" after the colon by hand.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

/// How a gh argv is sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    Read {
        command: String,
    },
    /// Changes GitHub (other people see it): asks, "don't ask again" per command.
    Write {
        command: String,
        scope: String,
    },
    /// A read of what's more than "check the project" needs (secret names, deploy keys, webhooks): asks.
    Sensitive {
        command: String,
        scope: String,
    },
    Refused {
        reason: String,
    },
}

impl Classified {
    pub fn is_read(&self) -> bool {
        matches!(self, Self::Read { .. })
    }
}

/// The project's GitHub host (its `origin`'s, or github.com outside a repository); None: unknown, so no host may be
/// named.
#[derive(Debug, Clone, Default)]
pub struct ClassifyContext {
    pub host: Option<String>,
    /// Every host passes (for `needs_approval`, which can't see the project; `run` checks the host again).
    pub any_host: bool,
}

impl ClassifyContext {
    pub fn host(host: Option<&str>) -> Self {
        Self { host: host.map(str::to_string), any_host: false }
    }
    pub fn any() -> Self {
        Self { host: None, any_host: true }
    }
}

const READS: [&str; 23] = [
    "pr list",
    "pr view",
    "pr checks",
    "pr diff",
    "pr status",
    "issue list",
    "issue view",
    "issue status",
    "run list",
    "run view",
    "workflow list",
    "workflow view",
    "repo view",
    "release list",
    "release view",
    "label list",
    "search issues",
    "search prs",
    "search repos",
    "search code",
    "search commits",
    "status",
    "api",
];
const WRITES: [&str; 19] = [
    "pr comment",
    "pr create",
    "pr edit",
    "pr close",
    "pr reopen",
    "pr merge",
    "pr review",
    "pr ready",
    "issue comment",
    "issue create",
    "issue edit",
    "issue close",
    "issue reopen",
    "release create",
    "release edit",
    "run rerun",
    "run cancel",
    "workflow run",
    "label create",
];

fn listed(command: &str) -> bool {
    READS.contains(&command) || WRITES.contains(&command)
}

/// The first words of every listed command: anything else as the first word could be an alias or an extension.
static GROUPS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| READS.iter().chain(WRITES.iter()).map(|p| p.split(' ').next().unwrap()).collect());

static RUNS: LazyLock<String> =
    LazyLock::new(|| format!("It runs: {} (reads), and asks for {}.", READS.join(", "), WRITES.join(", ")));

fn not_run(command: &str) -> String {
    format!("gh {command} isn't run by the github tool. {}", *RUNS)
}

const KEEP_TOKEN: &str = "Marv keeps the token away from the conversation.";
const GIT_OUTSIDE: &str = "it runs git outside the sandbox, and git runs the project's hooks and config, which commands may have written. To check out a pull request, use bash with git_write: true and network: true: git fetch origin pull/<N>/head:pr-<N>.";
const SECRETS: &str = "secrets, variables and keys stay out of the conversation.";

/// Commands that are never run, and why, said so the model learns the rule.
fn never(command: &str) -> Option<String> {
    Some(match command {
        "auth" => format!("gh auth … is never run: {KEEP_TOKEN}"),
        "config" => format!("gh config … is never run: gh's settings (and the token it can print) are the user's. {KEEP_TOKEN}"),
        "extension" => "gh extension … is never run: an extension is a program, and it would run outside the sandbox.".into(),
        "alias" => "gh alias … is never run: an alias can run any command, outside the sandbox.".into(),
        "secret" | "variable" | "ssh-key" | "gpg-key" => format!("gh {command} … is never run: {SECRETS}"),
        "run download" => "gh run download is never run: it writes files outside the sandbox. Read a run with gh run view (--log, --log-failed).".into(),
        "release download" => "gh release download is never run: it writes files outside the sandbox.".into(),
        "repo clone" | "pr checkout" => format!("gh {command} is never run: {GIT_OUTSIDE}"),
        // Whole commands gh has that are out of the tool's scope (not secrets): "isn't run", with the list.
        "codespace" | "gist" | "org" | "browse" => not_run(command),
        _ => return None,
    })
}

/// gh commands that exist but aren't the github tool's (so the refusal says "isn't run", not "unknown").
const EXISTING: [&str; 8] =
    ["repo create", "repo delete", "repo fork", "repo edit", "repo sync", "repo list", "repo archive", "repo rename"];

fn unknown(command: &str) -> String {
    format!(
        "gh {command} isn't something the github tool runs (an unknown word could be an alias or an extension, which run programs). {}",
        *RUNS
    )
}

const WEB: &str = "--web is never run: it opens the user's browser. Read it here instead (view, list, diff, checks).";

fn cluster(token: &str) -> String {
    format!(
        "Marv can't tell how gh reads {token}: write its short flags separately (-w is never run, and -R must name the repository on its own)."
    )
}

fn refused(reason: impl Into<String>) -> Classified {
    Classified::Refused { reason: reason.into() }
}

static SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://").unwrap());
static SCP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:[^@/\s]+@)?([^:/\s]+):").unwrap());

/// The host an `origin` URL names, as gh reads it (https, ssh://, scp-like git@host:path); None for a local path.
pub fn origin_host(url: &str) -> Option<String> {
    // The project's own file says it: only a plain host name counts (`[a-z0-9.-]`); anything else is unknown, never
    // text that would reach gh's arguments or Marv's sentences as a host.
    origin_host_any(url)
        .filter(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-'))
}

fn origin_host_any(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if SCHEME.is_match(trimmed) {
        let parsed = url::Url::parse(trimmed).ok()?;
        let host = parsed.host_str()?.to_lowercase();
        return (!host.is_empty()).then_some(host);
    }
    let found = SCP.captures(trimmed)?;
    // Marv's `:(?!\/\/)`: not a scheme-less `host://`.
    if trimmed[found.get(0).unwrap().end()..].starts_with("//") {
        return None;
    }
    Some(found[1].to_lowercase())
}

/// Whether `name` is the project's host (gh compares hosts without case).
fn same_host(name: &str, ctx: &ClassifyContext) -> bool {
    ctx.any_host || ctx.host.as_deref().is_some_and(|h| name.to_lowercase() == h.to_lowercase())
}

fn other_host(name: &str, ctx: &ClassifyContext) -> String {
    let known = match &ctx.host {
        Some(h) => format!(" ({h})"),
        None => " (none is known here)".into(),
    };
    format!("It names {name}, and the github tool only reaches the project's own GitHub host{known}.")
}

static URL_OR_SCP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://|^[^@/\s]+@[^:]+:").unwrap());
static URL_VALUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:https?|ssh|git)://|^git@").unwrap());

/// A -R/--repo value (or repo view's argument): OWNER/REPO anywhere on the project's host, HOST/OWNER/REPO or a URL
/// only for it.
fn check_repo(value: &str, ctx: &ClassifyContext) -> Option<String> {
    if URL_OR_SCP.is_match(value) {
        let host = origin_host(value);
        return match host {
            Some(h) if same_host(&h, ctx) => None,
            Some(h) => Some(other_host(&h, ctx)),
            None => Some(other_host(value, ctx)),
        };
    }
    let parts: Vec<&str> = value.split('/').collect();
    let filled = parts.iter().all(|p| !p.is_empty());
    if parts.len() == 2 && filled {
        return None;
    }
    if parts.len() == 3 && filled {
        return if same_host(parts[0], ctx) { None } else { Some(other_host(parts[0], ctx)) };
    }
    Some(format!("{value} isn't a repository gh can read (OWNER/REPO, or HOST/OWNER/REPO on the project's host)."))
}

/// A value that is a URL (or scp-like address) names a host: only the project's.
fn check_url_value(value: &str, ctx: &ClassifyContext) -> Option<String> {
    if !URL_VALUE.is_match(value) {
        return None;
    }
    match origin_host(value) {
        Some(h) if same_host(&h, ctx) => None,
        Some(h) => Some(other_host(&h, ctx)),
        None => Some(other_host(value, ctx)),
    }
}

/// `--name=value` → (name, Some(value)); `--name` → (name, None).
fn split_long(token: &str) -> (&str, Option<&str>) {
    match token.find('=') {
        None => (&token[2..], None),
        Some(eq) => (&token[2..eq], Some(&token[eq + 1..])),
    }
}

/// A short flag's letter (`-x` → 'x') and what follows it.
fn short(token: &str) -> (char, &str) {
    let mut chars = token[1..].chars();
    let letter = chars.next().unwrap_or('-');
    (letter, chars.as_str())
}

/// The flags after the command path, for every command but api: --web never, -R/--repo and --hostname name the
/// project's host only, a URL anywhere names it too. gh's other flags differ per command and only shape a read or
/// fill in a write the user is asked about, so they pass.
fn check_flags(rest: &[String], ctx: &ClassifyContext, repo_arg: bool) -> Option<String> {
    let mut positional = false;
    let mut first_positional = true;
    let mut i = 0;
    while i < rest.len() {
        let token = rest[i].as_str();
        i += 1;
        if positional || !token.starts_with('-') || token == "-" {
            let problem = if repo_arg && first_positional { check_repo(token, ctx) } else { check_url_value(token, ctx) };
            first_positional = false;
            if problem.is_some() {
                return problem;
            }
            continue;
        }
        if token == "--" {
            positional = true;
            continue;
        }
        if token.starts_with("--") {
            let (name, inline) = split_long(token);
            if name == "web" {
                return Some(WEB.into());
            }
            if name == "repo" || name == "hostname" {
                let value = match inline {
                    Some(v) => v,
                    None => match rest.get(i) {
                        Some(v) => {
                            i += 1;
                            v.as_str()
                        }
                        None => return Some(format!("--{name} needs a value.")),
                    },
                };
                let problem = if name == "repo" {
                    check_repo(value, ctx)
                } else if same_host(value, ctx) {
                    None
                } else {
                    Some(other_host(value, ctx))
                };
                if problem.is_some() {
                    return problem;
                }
                continue;
            }
            if let Some(inline) = inline
                && let Some(problem) = check_url_value(inline, ctx)
            {
                return Some(problem);
            }
            continue;
        }
        // A short flag: -w, -R value, -Rvalue; a cluster (-fw) could hide either, so it has to be written out.
        let (letter, after) = short(token);
        if after.is_empty() {
            if letter == 'w' {
                return Some(WEB.into());
            }
            if letter == 'R' {
                let Some(value) = rest.get(i) else { return Some("-R needs a value.".into()) };
                i += 1;
                if let Some(problem) = check_repo(value, ctx) {
                    return Some(problem);
                }
            }
            continue;
        }
        if letter == 'R' {
            if let Some(problem) = check_repo(after.strip_prefix('=').unwrap_or(after), ctx) {
                return Some(problem);
            }
            continue;
        }
        if token[1..].contains(['w', 'R']) {
            return Some(cluster(token));
        }
        if let Some(problem) = check_url_value(after, ctx) {
            return Some(problem);
        }
    }
    None
}

fn api_value_flag(letter: char) -> Option<&'static str> {
    Some(match letter {
        'X' => "method",
        'f' => "raw-field",
        'F' => "field",
        'H' => "header",
        'q' => "jq",
        't' => "template",
        'p' => "preview",
        _ => return None,
    })
}
const API_LONG_VALUE: [&str; 10] =
    ["method", "raw-field", "field", "header", "input", "jq", "template", "hostname", "cache", "preview"];
const API_BOOL: [&str; 5] = ["paginate", "slurp", "include", "silent", "verbose"];
/// Headers a read may set: neither changes what the request does (no method overrides, no other credentials).
const API_HEADERS: [&str; 2] = ["accept", "x-github-api-version"];
/// Path segments of API reads that ask: names of secrets and variables, deploy keys, webhooks (their URLs),
/// installations, invitations.
const SENSITIVE_SEGMENTS: [&str; 7] = ["secrets", "variables", "keys", "hooks", "installations", "invitations", "authorizations"];
const API_FLAGS_LISTED: &str = "-X/--method, -f/--raw-field, -F/--field, -H/--header (Accept, X-GitHub-Api-Version), --jq/-q, --template/-t, --paginate, --slurp, -i/--include, --silent, --cache, --hostname";

fn not_passed(flag: &str) -> Classified {
    refused(format!("{flag} isn't one the github tool passes to gh api. It takes {API_FLAGS_LISTED}."))
}

fn classify_api(rest: &[String], ctx: &ClassifyContext) -> Classified {
    let mut method: Option<String> = None;
    let mut fields = false;
    let mut endpoints: Vec<&str> = Vec::new();
    let mut positional = false;
    let mut i = 0;
    while i < rest.len() {
        let token = rest[i].as_str();
        i += 1;
        if positional || !token.starts_with('-') || token == "-" {
            endpoints.push(token);
            continue;
        }
        if token == "--" {
            positional = true;
            continue;
        }
        let name: &str;
        let mut value: Option<&str>;
        if token.starts_with("--") {
            (name, value) = split_long(token);
            if API_BOOL.contains(&name) {
                if value.is_some() {
                    return refused(format!("--{name} takes no value here: write it on its own."));
                }
            } else if API_LONG_VALUE.contains(&name) {
                if value.is_none() {
                    value = rest.get(i).map(String::as_str);
                    i += 1;
                }
                if value.is_none() {
                    return refused(format!("--{name} needs a value."));
                }
            } else {
                return not_passed(&format!("--{name}"));
            }
        } else {
            let (letter, after) = short(token);
            if letter == 'i' && after.is_empty() {
                name = "include";
                value = None;
            } else if let Some(long) = api_value_flag(letter) {
                name = long;
                value = if after.is_empty() {
                    let v = rest.get(i).map(String::as_str);
                    i += 1;
                    v
                } else {
                    Some(after)
                };
                if value.is_none() {
                    return refused(format!("-{letter} needs a value."));
                }
            } else if !after.is_empty() {
                return refused(cluster(token));
            } else {
                return not_passed(token);
            }
        }
        let value = value.unwrap_or_default();
        match name {
            "input" => {
                return refused(
                    "gh api --input is never run: it reads a file from the user's machine and sends it. Pass the body as -f/-F fields.",
                );
            }
            "verbose" => return refused("gh api --verbose is never run: it prints the request, token included."),
            "preview" => return not_passed("--preview"),
            "method" => method = Some(value.to_uppercase()),
            "raw-field" | "field" => fields = true,
            "header" => {
                let header = value.split(':').next().unwrap_or_default().trim();
                if !API_HEADERS.contains(&header.to_lowercase().as_str()) {
                    return refused(format!(
                        "gh api's header {header} isn't passed: only Accept and X-GitHub-Api-Version, which don't change what the request does."
                    ));
                }
            }
            "hostname" if !same_host(value, ctx) => return refused(other_host(value, ctx)),
            _ => {}
        }
    }
    if endpoints.is_empty() {
        return refused("gh api needs an endpoint, like repos/{owner}/{repo}/pulls.");
    }
    if endpoints.len() > 1 {
        return refused(format!("gh api takes one endpoint; got {}.", endpoints.join(", ")));
    }
    let endpoint = endpoints[0];
    if let Some(bad) = bad_endpoint(endpoint) {
        return refused(bad);
    }
    let path = endpoint.strip_prefix('/').unwrap_or(endpoint).split('?').next().unwrap_or_default();
    let api = |kind: fn(String, String) -> Classified, scope: String| kind("api".into(), scope);
    if path == "graphql" {
        return api(|command, scope| Classified::Write { command, scope }, "github:api graphql".into());
    }
    // Any field makes gh send a body (POST, unless -X says otherwise); a GET with fields still sends them somewhere.
    let verb = method.unwrap_or_else(|| if fields { "POST".into() } else { "GET".into() });
    if verb != "GET" || fields {
        return api(|command, scope| Classified::Write { command, scope }, format!("github:api {verb}"));
    }
    if sensitive_path(path) {
        return api(|command, scope| Classified::Sensitive { command, scope }, "github:api sensitive".into());
    }
    Classified::Read { command: "api".into() }
}

static ENDPOINT_SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*:").unwrap());

/// An endpoint must be a plain API path: no other host, no encoding or dot segments that could hide what it names.
fn bad_endpoint(endpoint: &str) -> Option<String> {
    let plain = "gh api's endpoint must be a plain path, like repos/{owner}/{repo}/pulls";
    if ENDPOINT_SCHEME.is_match(endpoint) {
        return Some(format!("{plain}: a URL could name another host."));
    }
    if endpoint.starts_with("//") {
        return Some(format!("{plain}."));
    }
    let path = endpoint.split('?').next().unwrap_or_default();
    if path.chars().any(|c| c == '%' || c == '\\' || c.is_whitespace()) || path.split('/').any(|s| s == "." || s == "..") {
        return Some(format!("{plain}."));
    }
    None
}

fn sensitive_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.split('/').any(|s| SENSITIVE_SEGMENTS.contains(&s)) || lower == "user/emails" || lower.contains("actions/runners")
}

/// Sorts a gh argv (without "gh") into read, write, sensitive or refused.
pub fn classify(args: &[String], ctx: &ClassifyContext) -> Classified {
    let Some(first) = args.first() else {
        return refused(format!("The github tool needs at least one argument. {}", *RUNS));
    };
    // gh finds its command among the arguments after stripping flags, and whether a flag eats the next word depends
    // on the flag: the command path first is the only order Marv reads exactly as gh does.
    if first.starts_with('-') {
        return refused(format!("Put the command first, then its flags: [\"pr\", \"view\", \"12\", \"--comments\"]. {}", *RUNS));
    }
    if let Some(reason) = never(first) {
        return refused(reason);
    }
    if !GROUPS.contains(first.as_str()) {
        return refused(unknown(first));
    }
    if first == "api" {
        return classify_api(&args[1..], ctx);
    }
    if first == "status" {
        return match check_flags(&args[1..], ctx, false) {
            Some(problem) => refused(problem),
            None => Classified::Read { command: "status".into() },
        };
    }
    let second = match args.get(1) {
        Some(s) if !s.starts_with('-') => s,
        _ => return refused(unknown(first)),
    };
    let command = format!("{first} {second}");
    if let Some(reason) = never(&command) {
        return refused(reason);
    }
    if !listed(&command) {
        return refused(if EXISTING.contains(&command.as_str()) { not_run(&command) } else { unknown(&command) });
    }
    if let Some(problem) = check_flags(&args[2..], ctx, command == "repo view") {
        return refused(problem);
    }
    if WRITES.contains(&command.as_str()) {
        let scope = format!("github:{command}");
        return Classified::Write { command, scope };
    }
    Classified::Read { command }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }
    fn at_host(args: &[&str], host: &str) -> Classified {
        classify(&argv(args), &ClassifyContext::host(Some(host)))
    }
    fn at(args: &[&str]) -> Classified {
        at_host(args, "github.com")
    }
    fn kind(c: &Classified) -> &'static str {
        match c {
            Classified::Read { .. } => "read",
            Classified::Write { .. } => "write",
            Classified::Sensitive { .. } => "sensitive",
            Classified::Refused { .. } => "refused",
        }
    }
    fn kind_of(args: &[&str]) -> &'static str {
        kind(&at(args))
    }
    fn kind_at(args: &[&str], host: &str) -> &'static str {
        kind(&at_host(args, host))
    }
    fn reason(c: &Classified) -> &str {
        match c {
            Classified::Refused { reason } => reason,
            other => panic!("not refused: {other:?}"),
        }
    }

    #[test]
    fn reads_run_freely() {
        let reads: &[&[&str]] = &[
            &["pr", "list"],
            &["pr", "view", "12"],
            &["pr", "view", "12", "--json", "title,body", "--comments"],
            &["pr", "checks", "12"],
            &["pr", "diff", "12"],
            &["pr", "status"],
            &["issue", "list", "--state", "open", "-L", "5"],
            &["issue", "view", "3"],
            &["issue", "status"],
            &["run", "list"],
            &["run", "view", "123", "--log-failed"],
            &["run", "view", "123", "--log"],
            &["workflow", "list"],
            &["workflow", "view", "ci.yml"],
            &["repo", "view"],
            &["repo", "view", "owner/repo"],
            &["release", "list"],
            &["release", "view", "v1.0"],
            &["label", "list"],
            &["search", "issues", "auth bug"],
            &["search", "prs", "--author", "me"],
            &["search", "repos", "marv"],
            &["search", "code", "token"],
            &["search", "commits", "fix"],
            &["status"],
            &["pr", "view", "auth"], // a positional, not the auth command
        ];
        for args in reads {
            assert!(at(args).is_read(), "{args:?}: {:?}", at(args));
        }
    }

    #[test]
    fn writes_ask_scoped_by_command() {
        for path in WRITES {
            let mut args: Vec<&str> = path.split(' ').collect();
            args.push("1");
            assert_eq!(at(&args), Classified::Write { command: path.into(), scope: format!("github:{path}") }, "{path}");
        }
    }

    #[test]
    fn refused() {
        let cases: &[(&[&str], &str)] = &[
            (&["auth", "token"], "never run"),
            (&["auth", "status", "--show-token"], "never run"),
            (&["auth", "login"], "never run"),
            (&["-h", "auth", "token"], "command first"),
            (&["--hostname", "x", "auth", "token"], "command first"),
            (&["config", "get", "oauth_token"], "never run"),
            (&["extension", "exec", "x"], "never run"),
            (&["ext", "exec", "x"], "isn't something"),
            (&["alias", "set", "x", "y"], "never run"),
            (&["myalias"], "isn't something"),
            (&["secret", "list"], "never run"),
            (&["variable", "list"], "never run"),
            (&["ssh-key", "list"], "never run"),
            (&["gpg-key", "list"], "never run"),
            (&["codespace", "list"], "isn't run"),
            (&["gist", "view", "1"], "isn't run"),
            (&["org", "list"], "isn't run"),
            (&["browse"], "isn't run"),
            (&["pr", "view", "12", "--web"], "browser"),
            (&["pr", "view", "12", "-w"], "browser"),
            (&["issue", "list", "--web=true"], "browser"),
            (&["pr", "create", "-fw"], "separately"),
            (&["run", "download", "1"], "download"),
            (&["release", "download", "v1"], "download"),
            (&["repo", "clone", "o/r"], "git outside the sandbox"),
            (&["pr", "checkout", "12"], "git outside the sandbox"),
            (&["repo", "create", "x"], "isn't run"),
            (&["repo", "delete", "o/r"], "isn't run"),
            (&["repo", "fork"], "isn't run"),
            (&["repo", "edit"], "isn't run"),
            (&["repo", "sync"], "isn't run"),
            (&["pr"], "isn't something"),
            (&["pr", "frobnicate"], "isn't something"),
            (&[], "at least"),
        ];
        for (args, expected) in cases {
            let got = at(args);
            assert!(reason(&got).contains(expected), "{args:?}: {got:?}");
        }
        assert!(reason(&at(&["frob"])).contains("pr view"), "an unknown word lists what runs");
        assert!(reason(&at(&["auth", "token"])).contains("Marv keeps the token away from the conversation"));
    }

    #[test]
    fn gh_api() {
        let cases: &[(&[&str], &str, Option<&str>)] = &[
            (&["api", "repos/o/r/pulls"], "read", None),
            (&["api", "-X", "GET", "repos/o/r/pulls"], "read", None),
            (&["api", "--method=GET", "repos/o/r/pulls"], "read", None),
            (&["api", "--method", "get", "repos/o/r/pulls"], "read", None),
            (&["api", "-XGET", "repos/o/r"], "read", None),
            (&["api", "repos/{owner}/{repo}/pulls", "--paginate", "--jq", ".[].title"], "read", None),
            (
                &["api", "repos/o/r", "-q", ".name", "-i", "--slurp", "--cache", "1h", "-t", "{{.name}}", "--paginate"],
                "read",
                None,
            ),
            (&["api", "-H", "Accept: application/vnd.github.raw", "repos/o/r/contents/x"], "read", None),
            (&["api", "--header=X-GitHub-Api-Version: 2022-11-28", "repos/o/r"], "read", None),
            (&["api", "repos/o/r/issues?state=open"], "read", None),
            (&["api", "-X", "POST", "repos/o/r/issues"], "write", Some("github:api POST")),
            (&["api", "--method", "delete", "repos/o/r"], "write", Some("github:api DELETE")),
            (&["api", "repos/o/r/issues", "-f", "title=x"], "write", Some("github:api POST")),
            (&["api", "repos/o/r/issues", "--raw-field=title=x"], "write", Some("github:api POST")),
            (&["api", "repos/o/r/issues", "-F", "x=@/etc/passwd"], "write", Some("github:api POST")),
            (&["api", "-X", "GET", "search/issues", "-f", "q=x"], "write", Some("github:api GET")),
            (&["api", "graphql", "-f", "query={viewer{login}}"], "write", Some("github:api graphql")),
            (&["api", "graphql"], "write", Some("github:api graphql")),
            (&["api", "repos/o/r/actions/secrets"], "sensitive", Some("github:api sensitive")),
            (&["api", "repos/o/r/actions/variables"], "sensitive", Some("github:api sensitive")),
            (&["api", "repos/o/r/keys"], "sensitive", Some("github:api sensitive")),
            (&["api", "repos/o/r/hooks"], "sensitive", Some("github:api sensitive")),
            (&["api", "user/installations"], "sensitive", Some("github:api sensitive")),
            (&["api", "repos/o/r/invitations"], "sensitive", Some("github:api sensitive")),
            (&["api", "user/emails"], "sensitive", Some("github:api sensitive")),
            (&["api", "/user/emails"], "sensitive", Some("github:api sensitive")),
            (&["api", "authorizations"], "sensitive", Some("github:api sensitive")),
            (&["api", "repos/o/r/actions/runners/registration-token"], "sensitive", Some("github:api sensitive")),
            (&["api", "repos/o/r/Secrets"], "sensitive", Some("github:api sensitive")),
        ];
        for (args, expected, scope) in cases {
            let got = at(args);
            assert_eq!(kind(&got), *expected, "{args:?}: {got:?}");
            if let Some(scope) = scope {
                match &got {
                    Classified::Write { scope: s, .. } | Classified::Sensitive { scope: s, .. } => assert_eq!(s, scope),
                    other => panic!("{other:?}"),
                }
            }
        }
        let refusals: &[(&[&str], &str)] = &[
            (&["api", "--input", "f", "repos/o/r/issues"], "--input"),
            (&["api", "--input=f", "repos/o/r/issues"], "--input"),
            (&["api", "--verbose", "repos/o/r"], "--verbose"),
            (&["api", "-H", "X-HTTP-Method-Override: DELETE", "repos/o/r"], "header"),
            (&["api", "-H", "Authorization: token x", "repos/o/r"], "header"),
            (&["api", "--preview", "x", "repos/o/r"], "--preview"),
            (&["api"], "endpoint"),
            (&["api", "repos/o/r", "user"], "one endpoint"),
            (&["api", "https://evil.example/x"], "another host"),
            (&["api", "//evil.example/x"], "path"),
            (&["api", "repos/../user/emails"], "path"),
            (&["api", "repos/o/r/secre%74s"], "path"),
            (&["api", "-iX", "POST", "repos/o/r"], "separately"),
        ];
        for (args, expected) in refusals {
            let got = at(args);
            assert!(reason(&got).contains(expected), "{args:?}: {got:?}");
        }
    }

    #[test]
    fn hostname_only_for_the_projects_host() {
        assert_eq!(kind_at(&["api", "--hostname", "ghe.corp", "repos/o/r"], "ghe.corp"), "read");
        assert_eq!(kind_at(&["api", "--hostname=ghe.corp", "repos/o/r"], "ghe.corp"), "read");
        assert_eq!(kind_at(&["api", "--hostname", "ghe.corp", "repos/o/r"], "github.com"), "refused");
        assert_eq!(kind_at(&["api", "--hostname", "GitHub.com", "repos/o/r"], "github.com"), "read");
    }

    #[test]
    fn repo_with_a_host_part_only_for_the_projects_host() {
        assert_eq!(kind_of(&["pr", "list", "-R", "other/repo"]), "read");
        assert_eq!(kind_of(&["pr", "list", "--repo=other/repo"]), "read");
        assert_eq!(kind_of(&["pr", "list", "-Rother/repo"]), "read");
        assert_eq!(kind_of(&["pr", "list", "-R", "other.host/o/r"]), "refused");
        assert_eq!(kind_of(&["pr", "list", "-Rother.host/o/r"]), "refused");
        assert_eq!(kind_of(&["pr", "list", "--repo", "other.host/o/r"]), "refused");
        assert_eq!(kind_at(&["pr", "list", "-R", "ghe.corp/o/r"], "ghe.corp"), "read");
        assert_eq!(kind_of(&["pr", "list", "-R", "https://other.host/o/r"]), "refused");
        assert_eq!(kind_of(&["pr", "list", "-R", "https://github.com/o/r"]), "read");
        assert_eq!(kind_of(&["pr", "list", "-R", "a/b/c/d"]), "refused");
        assert_eq!(kind_of(&["pr", "merge", "1", "-R", "other.host/o/r"]), "refused");
    }

    #[test]
    fn a_url_argument_names_its_host() {
        assert_eq!(kind_of(&["pr", "view", "https://github.com/o/r/pull/1"]), "read");
        assert_eq!(kind_of(&["pr", "view", "https://other.host/o/r/pull/1"]), "refused");
        assert_eq!(kind_of(&["issue", "view", "git@other.host:o/r"]), "refused");
        assert_eq!(kind_of(&["repo", "view", "other.host/o/r"]), "refused");
        assert_eq!(kind_of(&["repo", "view", "github.com/o/r"]), "read");
    }

    #[test]
    fn without_a_known_host_anything_naming_one_is_refused() {
        let none = ClassifyContext::host(None);
        assert_eq!(kind(&classify(&argv(&["pr", "list", "-R", "github.com/o/r"]), &none)), "refused");
        assert_eq!(kind(&classify(&argv(&["pr", "list", "-R", "o/r"]), &none)), "read");
    }

    #[test]
    fn after_double_dash_everything_is_positional() {
        assert_eq!(kind_of(&["search", "issues", "--", "--web"]), "read");
    }

    #[test]
    fn origin_host_reads_the_host_as_gh_would() {
        assert_eq!(origin_host("https://github.com/o/r.git").as_deref(), Some("github.com"));
        assert_eq!(origin_host("git@ghe.corp:o/r.git").as_deref(), Some("ghe.corp"));
        assert_eq!(origin_host("ssh://git@GHE.corp:22/o/r").as_deref(), Some("ghe.corp"));
        assert_eq!(origin_host("https://user:pw@ghe.corp/o/r").as_deref(), Some("ghe.corp"));
        assert_eq!(origin_host("/local/path"), None);
        assert_eq!(origin_host("https://[::1]/o/r"), None);
        assert_eq!(origin_host("git@evil\u{202e}host:o/r"), None);
        assert_eq!(origin_host("git@a\"b`c:o/r"), None);
        assert_eq!(origin_host(""), None);
    }
}

---
name: github
description: Work with GitHub (issues, pull requests, Actions runs) through the github tool (gh with the user's login) or the github MCP server. Use when asked about GitHub issues, PRs, branches, releases, or Actions runs.
---

# Working with GitHub

Marv reaches GitHub one of two ways, both outside the sandbox with the user's credentials, which you never see:

- **The `github` tool** (when the user has `gh` installed and logged in): it runs the GitHub CLI with their
  login. Pass gh's arguments as a list, command first: `["pr", "list"]`, `["run", "view", "123", "--log-failed"]`,
  `["api", "repos/{owner}/{repo}/pulls"]`. Prefer it when it's offered.
- **The GitHub MCP server** (`~/.marv/mcp.json`): tools named `mcp__github__<tool>`.

## Rules

- **Never `gh` in bash.** The command sandbox has no network and hides the home folder, so `gh` can't log in
  there; don't suggest logging in or mounting its config either. Use the `github` tool, or the MCP tools. If
  neither is there, tell the user: install and log in to `gh` (then restart Marv), or connect the MCP server
  (check `/mcp` and `~/.marv/mcp.json`). `web_fetch` (github.com/owner/repo links) still works for public,
  read-only browsing without either.
- **Never ask for or print tokens.** The user's token stays in their environment (`MARV_GITHUB_TOKEN`);
  Marv reads it where the sandbox can't. Don't offer to set, rotate, echo, or write one anywhere.

## The workflow (this repository's)

For a change to a GitHub repository:

1. Work on a branch (not `main`): create one for each change, commit there.
2. Open a pull request. Its description says **what's in it** and **what checks it must pass**, so a
   reviewer can see the intent without reading the diff; keep it plain Markdown.
3. Merge with a **merge commit** (GitHub's default, `merge_method: "merge"` in `merge_pull_request`),
   so the PR's history stays one merge on the base branch.

## Using the github tool

- Reads (`pr list|view|checks|diff`, `issue list|view`, `run list|view`, `repo view`, `search …`, `api` GET)
  run without asking; use them freely to look things up. `pr checks` and `run view --log-failed` find what
  failed in CI.
- Changes (`pr comment|create|merge|review|…`, `issue comment|create|close|…`, `api` with fields) ask the user
  first: say what you intend to do. Merge with a merge commit: `["pr", "merge", "12", "--merge"]`.
- What it never runs (`auth`, `config`, `--web`, `pr checkout`, another host…) comes back as an error saying
  why; don't look for a way around it. To check out a PR, use bash with `git_write: true` and `network: true`:
  `git fetch origin pull/<N>/head:pr-<N>`.

## Using the MCP tools

- Read tools (`get_me`, `list_issues`, `list_pull_requests`, `pull_request_read`, `issue_read`,
  `get_file_contents`, …) run without asking; use them freely to look things up.
- Write tools (`add_issue_comment`, `create_pull_request`, `merge_pull_request`, …) ask the user first:
  that's by design, so don't try to route around it, and say what you intend to do.
- With many tools connected, Marv may offer only the tools' names with `tool_search` and `mcp_call`;
  then call `tool_search` with the names you need, and run the tool through `mcp_call`.
- The tool names GitHub's API needs (`owner`, `repo`, numbers, not URLs) are the parts of the
  repository's URL: `github.com/<owner>/<repo>/pull/<number>`.

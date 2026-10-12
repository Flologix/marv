---
name: github
description: Work with GitHub (issues, pull requests, Actions runs) through the github MCP server. Use when asked about GitHub issues, PRs, branches, releases, or Actions runs.
---

# Working with GitHub

This project is connected to GitHub through an MCP server (`~/.marv/mcp.json`). Its tools are named
`mcp__github__<tool>`; they run outside the sandbox with the user's token, which you never see.

## Rules

- **Use the GitHub MCP tools, never the `gh` CLI.** The command sandbox has no network and hides the home
  folder, so `gh` can't log in there; don't suggest logging in or mounting its config either. If a GitHub
  tool is missing, tell the user to check `/mcp` and their `~/.marv/mcp.json`, not to run `gh auth login`.
  `web_fetch` (github.com/owner/repo links) still works for public, read-only browsing without the server.
- **Never ask for or print tokens.** The user's token stays in their environment (`MARV_GITHUB_TOKEN`);
  Marv reads it where the sandbox can't. Don't offer to set, rotate, echo, or write one anywhere.

## The workflow (this repository's)

For a change to a GitHub repository:

1. Work on a branch (not `main`): create one for each change, commit there.
2. Open a pull request. Its description says **what's in it** and **what checks it must pass**, so a
   reviewer can see the intent without reading the diff; keep it plain Markdown.
3. Merge with a **merge commit** (GitHub's default, `merge_method: "merge"` in `merge_pull_request`),
   so the PR's history stays one merge on the base branch.

## Using the tools

- Read tools (`get_me`, `list_issues`, `list_pull_requests`, `pull_request_read`, `issue_read`,
  `get_file_contents`, …) run without asking; use them freely to look things up.
- Write tools (`add_issue_comment`, `create_pull_request`, `merge_pull_request`, …) ask the user first:
  that's by design, so don't try to route around it, and say what you intend to do.
- With many tools connected, Marv may offer only the tools' names with `tool_search` and `mcp_call`;
  then call `tool_search` with the names you need, and run the tool through `mcp_call`.
- The tool names GitHub's API needs (`owner`, `repo`, numbers, not URLs) are the parts of the
  repository's URL: `github.com/<owner>/<repo>/pull/<number>`.

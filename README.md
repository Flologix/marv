<p align="center"><img src="docs/images/marv.svg" alt="Marv, a pixel-art martian" width="144"></p>

# Marv

A terminal coding agent. Marv reads your code, edits files and runs commands (sandboxed, and asking for your approval wherever the sandbox can't contain what it does), inside a full-screen terminal UI with a little martian for company.

Marv is written in TypeScript on [Bun](https://bun.sh), with an [Ink](https://github.com/vadimdemedes/ink) (React for the terminal) interface. It talks to models through [OpenRouter](https://openrouter.ai) (hundreds of cloud models with one API key) or [Ollama](https://ollama.com) (models running on your own machine).

![Marv's welcome screen in the terminal](docs/images/screenshot.png)

## Features

- **An agent loop with tools.** It can read, search (`glob`, `grep`), edit, write, and run shell commands. The model decides which tools to use; Marv runs them and reports back.
- **Safe yolo mode (on by default).** Sandboxed commands and edits inside the project run without asking; anything the sandbox can't contain (network access, git commits and other repository changes, `.git` itself, memory) still shows you a colored diff or the exact command first: yes, yes for the rest of the session, or no (which stops Marv so you can redirect it). `/yolo off` asks for every change.
- **Sandboxed commands.** On Linux, commands run in [bubblewrap](https://github.com/containers/bubblewrap): only the project folder is writable, your home folder (keys, SSH, Marv's own config) is hidden, there's no network unless a command asks for it, and API keys never reach the command.
- **Dev servers you can open.** A command's processes end with it, and its `localhost` isn't yours, so Marv starts servers and watchers with its `process` tool instead: each keeps running in its own sandbox (same rules: only the project writable, no network unless asked) across turns until it's stopped or Marv exits, and the ports it listens on are forwarded to your `127.0.0.1`, so `http://localhost:5173` just works. You're asked before each start; `/processes` lists them, `/processes stop <name>` stops one, and the status bar shows how many run.
- **Any model.** OpenRouter or a local Ollama model, with a searchable model picker showing prices. Switch with `/model`.
- **MCP servers.** Connect any [Model Context Protocol](https://modelcontextprotocol.io) server, local or remote, in `.mcp.json` or `~/.marv/mcp.json` (Claude Code's format). Its tools join Marv's; calls ask first unless the server marks a tool read-only, since servers run outside the sandbox. A project's servers only start once you trust them (`/mcp trust`). `/mcp add <npm package | URL | GitHub repo>` sets one up for you: it pins the version, shows you exactly what it will write, and starts it without a restart (see [Add an MCP server](#add-an-mcp-server)). With many tools connected, the model sees only their names and loads a tool's parameters when it needs one, so they don't cost thousands of tokens on every request (`"mcpTools": "eager"` in the config sends them all).
- **Skills.** Drop a `SKILL.md` into `.marv/skills/` (or `~/.marv/skills/`) and Marv loads it when a task matches, or run it yourself with `/<skill-name>`.
- **GitHub with your `gh` login.** If you use the GitHub CLI, Marv runs it for you, outside the sandbox: reading PRs, CI runs and issues just works, anything that changes GitHub asks first, and `gh auth` and friends never run. Or connect GitHub's official MCP server. See [Connect GitHub](#connect-github).
- **Subagents.** Marv can hand a task to a fresh agent with its own context, which reports back when it's done, so the files it read don't fill up the conversation. Several can run in parallel, each in its own git worktree if they change files. Click one to watch its own transcript live. Agent types are Markdown files, compatible with Claude Code's.
- **Memory.** Marv remembers your preferences and project facts across sessions. Every change it makes to its memory needs your approval.
- **Sessions.** Every conversation is saved; `marv -c` continues the last one, `/resume` picks an earlier one.
- **Long conversations.** When the context window fills up, Marv summarizes the conversation and carries on (`/compact` does it on demand). Requests reuse the provider's prompt cache wherever possible.
- **Trajectories and feedback.** Every turn is logged step by step (requests, replies, tool calls, subagents, timings, tokens) to `~/.marv/trajectories/`, with your ratings: `/good`, `/bad`, `/label`, and what your next message implies ("thanks, perfect" or "that's wrong"). `bun run stats` sums them up per model or Marv version, so you can measure whether a change made runs better.
- **Type errors caught early.** After Marv changes files in a TypeScript project, it runs the project's own typecheck in the sandbox and tells the model about errors the change added, even in files it didn't touch (a project here means a `tsconfig.json` in the folder Marv starts in plus the project's own `typescript` or tsgo in that folder's `node_modules`; a global tsc isn't used), so it fixes broken callers before saying it's done. `/diagnostics off` turns it off.
- **Your project's rules, as checks.** Rules from your instructions ("imports use `.ts` extensions", "never call X outside Y") can live in `.marv/checks/` as code: after a step that changed files, Marv runs the checks those files concern, in the sandbox, and tells the model only the violations its change added, with the reason. An obeyed rule costs nothing; a broken one comes back when it matters. See [Checks](#checks).
- **A plan for long tasks.** For a task with several steps (or a request with several asks in one paragraph), the model writes a checklist with its `plan` tool and ticks it off as it works. Marv pins it above the prompt (`Plan 2/5`, the current item highlighted) until every item is done, saves it with the session, and hands it back to the model after the conversation is compacted, so the README it was also asked to update isn't forgotten.
- **No dead stops.** After 25 steps without finishing, Marv asks whether to keep going instead of giving up. Raise the interval with `/steps 40` (or `maxSteps` in the config), or turn the asking off with `/steps off` — like Claude Code and Codex, a long run then just keeps going (subagents still stop at their own 50).
- **Cost tracking.** OpenRouter's actual charge per request, plus context and cache use, in the status bar; `/cost` for the breakdown.
- **Herdr aware.** Run Marv in a [Herdr](https://herdr.dev) pane and Herdr's sidebar shows whether it's working, waiting for your approval, or done, with nothing to set up.
- **A comfortable TUI.** Streaming Markdown replies, mouse-wheel scrolling, drag-to-copy with auto-scroll, a `/` command menu, and Esc to interrupt.

## Requirements

- [Bun](https://bun.sh) 1.3 or newer.
- One of:
  - an [OpenRouter API key](https://openrouter.ai/settings/keys), or
  - [Ollama](https://ollama.com) running locally, with a model that supports tool calling (e.g. `ollama pull qwen3.5:9b`).
- Linux or macOS. **On Windows, use [WSL](https://learn.microsoft.com/windows/wsl/install)**: install Bun and Marv inside your WSL distribution (e.g. Ubuntu) and run `marv` there. Native Windows isn't supported yet (the `marv` launcher and the shell tool need `bash`).
- For the command sandbox: Linux (WSL included) with `bubblewrap` (`bwrap`; e.g. `sudo apt install bubblewrap`). Without it, macOS included, Marv still works, but every command asks first and warns that it runs unsandboxed.

## Install

```sh
bun add -g @flologixai/marv    # or: npm install -g @flologixai/marv (Marv still runs on Bun)
```

Or from source, to hack on it:

```sh
git clone https://github.com/FlologixAI/marv.git
cd marv
bun install    # also applies the patches in patches/
bun link       # puts `marv` on your PATH
```

Then start it in any project folder:

```sh
cd ~/some/project
marv
```

The first run opens a short setup: pick OpenRouter or Ollama, pick a model, and (for OpenRouter) paste your key. Settings are saved to `~/.marv/config.json`, readable only by you. Run `/setup` to change them later.

## Using Marv

Type what you want in plain language, for example:

```
> how does the config loading work?
> add a --verbose flag to the CLI and update the help text
> run the tests and fix whatever fails
```

### Commands

| Command | What it does |
|---|---|
| `/help` | Show available commands and shortcuts |
| `/model` | Pick a model (`/model <id>` to set one directly) |
| `/setup` | Change provider, model, or API key |
| `/config` | Show the current configuration |
| `/think` | Let thinking models reason before answering (`/think on`, `/think off`) |
| `/skills` | List available skills; run one with `/<skill-name> <request>` |
| `/agents` | List the subagent types Marv can start |
| `/memory` | Show what Marv remembers |
| `/remember` | Save a note to memory (`/remember <note>`, or `/remember project: <note>`) |
| `/forget` | Remove a memory (`/forget <text from it>`) |
| `/resume` | Pick up an earlier session in this project |
| `/compact` | Summarize the conversation to free up context (`/compact <what to keep>`) |
| `/cost` | Tokens used and cost so far |
| `/sandbox` | Show or set the command sandbox (`/sandbox on`, `/sandbox off`) |
| `/yolo` | Run what the sandbox contains without asking (`/yolo on`, `/yolo off`) |
| `/processes` | List the background processes Marv started (dev servers, watchers); `/processes stop <name>` stops one |
| `/good`, `/bad` | Rate the last turn, with an optional note (`/bad edited the wrong file`) |
| `/label` | Tag the last turn (`/label refactor, tests`) |
| `/mcp` | Show MCP servers and their tools; `/mcp add <package \| URL \| repo>` adds one, `/mcp search <words>` finds one, `/mcp remove <name>` takes yours out, `/mcp trust` starts the project's, `/mcp login <name>` logs in to a remote server (OAuth), `/mcp logout <name>` forgets the login |
| `/diagnostics` | Show or set the typecheck after edits (`/diagnostics on`, `/diagnostics off`) |
| `/checks` | List the project's checks and their last result (`/checks on`, `/checks off`) |
| `/steps` | Show or set the step limit (`/steps 40`, `/steps off` never asks) |
| `/trajectories` | Show or set run logging (`/trajectories on`, `/trajectories off`) |
| `/clear` | Start a fresh conversation (the old one stays saved) |
| `/exit` | Quit |

Type `/` to open the command menu: ↑/↓ to choose, Enter to run, Tab or → to complete.

### Keys

| Key | Action |
|---|---|
| Esc | Stop a reply or a running tool (in a subagent's view: go back) |
| ctrl+c | Stop / clear the input / press twice to quit |
| ctrl+o | Show or hide details: what subagents did, and whole diffs |
| Click a subagent | Open its own transcript, live |
| PgUp / PgDn, mouse wheel | Scroll the conversation |
| Drag with the mouse | Select text; it's copied when you let go (drag past the edge to scroll) |
| ↑ / ↓ | Previous inputs |

### Command-line options

```
marv              start an interactive session
marv -c           continue the latest session in this folder (--continue)
marv -r           pick an earlier session to resume (--resume)
marv --version    print the version
marv --help       show the help
```

## Safety model

Marv is built so that you stay in control of what changes on your machine:

1. **Reading is free; what can't be contained needs approval.** Reading and searching files never asks. With yolo mode on (the default), commands that run in a working sandbox without network, and edits inside the project (but not in `.git`), run without asking; everything else shows you exactly what will happen first: a command that wants the network or changes the git repository (it must say so with `git_write`), an edit inside `.git`, any command when the sandbox is off or unavailable, and every memory change. With `/yolo off`, every edit and command asks. "Don't ask again" covers the rest of the session only: all file edits, or one exact command.
2. **`.git` is protected.** Git runs hooks and config from `.git` outside any sandbox (your next commit would run a planted hook with full access), so commands that run without asking see it read-only. What yolo can't protect: files you later run yourself outside the sandbox, like scripts and `package.json`; review with `git diff` before running them.
3. **Paths are confined to the project.** Tools refuse anything outside the folder Marv was started in, including through symlinks.
4. **Commands run in a sandbox.** The system is read-only and the home folder is hidden (toolchains like `~/.bun` and your git config are mounted read-only). The project folder is the only writable place, and there's no network unless the command asks for it, which the approval prompt shows. The environment starts empty, so API keys can't leak into commands.
5. **Memory changes are approved too.** Memory comes back in every future session, so an instruction planted by a malicious file and saved there would be a persistent prompt injection. You see every memory before it's saved.
6. **MCP servers are trusted explicitly, and their calls ask.** A server is a program running with your permissions, outside the sandbox, so a project's `.mcp.json` servers start only after `/mcp trust` (for that exact config and the project scripts it names: if either changes, it asks again; you're shown the config as written, with the environment variables it reads, never your secrets), and every call asks unless the server marks the tool read-only. Yolo mode never skips these. Local servers get no API keys from Marv's environment, only what their config gives them. Your own servers start in your home folder, not the project (so a package planted in the project can't stand in for one); use `${MARV_PROJECT_DIR}` in their arguments if they need the project path. What a server returns is treated as data, not instructions.
7. **Background processes ask first, and stay as contained as commands.** A server or watcher the `process` tool starts asks every time a new command starts (yolo too; "don't ask again" covers that exact command for the session). It runs in its own sandbox like a command: the project is the only writable place, `.git` is read-only, the home folder is hidden, and there's no network unless the start asked for it. Only the ports it listens on (1024 and up, at most 4) are forwarded, to your `127.0.0.1` only, never another interface; nothing on your machine is reachable from it. It stops on `/processes stop`, when Marv exits, and with Marv even if Marv is killed. Subagents can't start one.
8. **Subagents ask like Marv does, or work in a worktree.** A subagent in the project folder asks before each change, and the prompt says which one is asking. One in its own git worktree is approved once when it starts; inside its sandboxed worktree its edits and commands then run without asking, except commands that want the network (and with the sandbox off, everything asks). It can't touch the repository's `.git` (it's read-only in the sandbox, so no commits, branch moves, hooks or config), and when it's done, Marv commits its changes to its branch, without running any of the repository's hooks. Its work comes back as a branch you (or Marv, with your approval) review and merge. Esc at an approval prompt declines everything waiting and stops the run.

## Skills

A skill is a folder with a `SKILL.md`:

```markdown
---
name: release-notes
description: How to write release notes for this project. Use when asked for release notes or a changelog.
---

1. Run `git log` since the last tag…
2. Group the changes into Added / Changed / Fixed…
```

Put it in `.marv/skills/<name>/` (this project) or `~/.marv/skills/<name>/` (all projects). Only each skill's name and description go into the system prompt; Marv loads the full instructions when a request matches. A skill folder can also hold scripts and reference files. Write descriptions that name the task and when to use it: that's how the model decides.

## Add an MCP server

`/mcp add` works out a server's entry, shows it to you once, writes it to `~/.marv/mcp.json` (yours, every project) and starts it in the running session:

```sh
/mcp add @modelcontextprotocol/server-filesystem -- ${MARV_PROJECT_DIR}     # an npm package (npm: prefix optional)
/mcp add pypi:mcp-server-fetch                                              # a PyPI package (run with uvx)
/mcp add https://mcp.example.com/mcp --header "Authorization: Bearer ${EXAMPLE_TOKEN}"   # a remote server
/mcp add io.github.acme/weather                                             # a name from the official MCP Registry
/mcp add https://github.com/acme/weather-mcp                                # a repo whose README names a package or URL
/mcp search weather                                                         # look in the MCP Registry
/mcp remove weather                                                         # take one of yours out again
```

- **Pinned.** A package is written as `npx -y <pkg>@<exact version>` or `uvx <pkg>@<exact version>`, resolved before you're asked (`name@1.2.3` asks for a version; it must exist). npm and PyPI never let a published version's files change.
- **One approval, of exactly what's written.** The prompt shows the command line, the version and when it was published and by whom, the repository the package names (npm and PyPI don't check that field), the registry publisher's verified namespace, every environment variable the server reads (and whether it's set), and the JSON it writes. Warnings stand out: a deprecated or yanked version, a package first published in the last week, a GitHub README naming a package whose own repository is a different one. Yes writes that JSON as shown; nothing fetched afterwards changes it. There's no "don't ask again": each server is its own decision.
- **Secrets stay in your environment.** `--env NAME` writes `"NAME": "${NAME}"`, and `--header` takes `"Name: Bearer ${VAR}"`; a value typed in (`--env KEY=sk-…`, a literal header) is refused, since the file sits on disk. A variable that isn't set means the entry is written but not started: set it in your shell and restart Marv.
- **Options:** `--name <name>` (by default it comes from the package, `@modelcontextprotocol/server-filesystem` → `filesystem`, or the URL's host), and arguments for the server after `--`. A registry entry's fixed arguments and required variables are filled in; a required argument with no value goes after `--`.
- **GitHub repos:** Marv reads the README (with `GITHUB_TOKEN` if set) and takes only a package name or an https URL from its config examples, nothing else from the block (no extra arguments, no `env` values; the variable names it uses are suggested as `--env`). Several candidates: you get the `/mcp add` commands to choose from.
- **Its tools** reach the model from your next message. `/mcp remove <name>` takes one of your servers out and stops it; its tools stay listed (failing) until `/clear` or a restart. A project's servers stay in `.mcp.json`, edited by hand.

Known limits: npx resolves a package's own dependencies by their version ranges when it first runs (there's no lockfile to pin them); a remote server is whatever its owner deploys at that URL; no Docker images and no building from source yet (a README that only shows those is reported, not followed); a PyPI package whose command has another name than the package needs a hand edit.

### Servers that need a login

A remote server that wants an OAuth login, rather than a token in `headers`, shows as `needs login` at startup and in `/mcp`. Log in with:

```sh
/mcp login linear      # opens your browser; when it comes back, Marv stores the tokens and connects
/mcp logout linear     # forgets the login, and revokes it where the server allows
```

- **Only you start a login.** Startup, a tool call and the model never open a browser or register Marv with a server; a server whose login runs out (its refresh token refused) says `needs login` again, and its tools fail with a message telling the model to ask you.
- **The browser comes back to Marv** on `127.0.0.1` (a random port, PKCE, a random `state`). Over SSH or without a display Marv doesn't open a browser: it shows the link. Open it anywhere; if the browser then shows an error page (it couldn't reach `127.0.0.1` on that machine), paste the address it ended up on into Marv's prompt. That address goes to the login, never to the model, and the transcript shows `(login callback)`. A login gives up after 5 minutes; `/mcp login` again starts over.
- **Tokens** are kept in `~/.marv/mcp-tokens.json` (0600), per server URL: your servers share one login across projects (renaming a server keeps it), and a project's server (from `.mcp.json`, trusted first) gets its own. The MCP SDK renews the access token with the refresh token when it runs out, also in `marv -p` and SDK sessions using the same config folder; they never start a login, and say which server needs one. Tokens never reach the model, the transcript, session files, trajectories or sandboxed commands.
- **Servers without dynamic registration** need a client you register yourself (an OAuth app at the provider, with the redirect URI `http://127.0.0.1:<port>/callback`), in the server's entry:

```json
{ "mcpServers": { "example": {
  "type": "http", "url": "https://mcp.example.com/mcp",
  "oauth": { "clientId": "abc123", "clientSecret": "${EXAMPLE_CLIENT_SECRET}", "callbackPort": 33418, "scope": "read write" }
} } }
```

  All of `oauth` is optional: `clientId` and `clientSecret` (a `${VAR}`, like headers; leave it out for a public client), `callbackPort` (the port of the redirect URI you registered), and `scope` (space-separated: the scope Marv asks for, instead of the one the server suggests). `/mcp` shows the client id and scope, never the secret. A server with its own `Authorization` header logs in with that, not with `/mcp login`.

Known limits of logins: no login from the SDK or `marv -p` (log in once in the TUI); a client registered by hand needs its exact `callbackPort` free when you log in.

## Connect GitHub

**If you use `gh`, there's nothing to do.** When the [GitHub CLI](https://cli.github.com/) is installed and logged in (`gh auth status`), Marv offers the model a `github` tool that runs `gh` for it, outside the sandbox, with your own login. Ask "check the current project" and it reads the open PRs, the failing CI run and the issues without a prompt. The rules:

- **Reads run at once:** `pr list|view|checks|diff|status`, `issue list|view|status`, `run list|view` (`--log-failed` too), `workflow list|view`, `repo view`, `release list|view`, `label list`, `search …`, `status`, and `gh api` GET requests.
- **Changes ask first**, since other people see them: commenting, creating, editing, closing, reopening, merging or reviewing PRs and issues, releases, re-running or cancelling runs, `workflow run`, `label create`, and `gh api` with fields, another method or `graphql`. A merge or close names the repository. "Don't ask again" works per command (`gh pr comment`). `gh api` reads of secrets, variables, keys, webhooks, installations, invitations and your emails ask too.
- **Never run:** `gh auth`, `config`, `extension`, aliases (any unknown command), `secret`, `variable`, `ssh-key`, `gpg-key`, `codespace`, `gist`, `--web`, downloads, `gh api --input` and `--verbose`, and `pr checkout`/`repo clone` (they'd run git outside the sandbox, with hooks a command may have written; the model checks out a PR with an approved `git fetch` instead). Nothing reaches a GitHub host the project doesn't name: `-R`, `--hostname` and URLs must point at the host of the project's `origin` (`github.com` outside a repository), which is all GitHub Enterprise needs.
- `gh` gets your environment minus Marv's own keys (`OPENROUTER_API_KEY`, `MARV_*`), never prompts, and is spawned without a shell. Tokens in its output (`ghp_…`, `github_pat_…`) are replaced with `[redacted token]` before the model, the transcript or a log sees them.
- Subagents can read with it, not write. In `marv -p`, reads run and changes need `--approve github`. SDK sessions get it with `github: true`.
- `MARV_GITHUB=off` hides the tool (to use the MCP server below instead). Marv checks for `gh` once at startup, so log in before you start it.

`gh` typed into a bash command still fails, on purpose: the sandbox has no network, hides `~/.config/gh` and starts with an empty environment, so a token can't be printed into output that goes to the model provider. Don't "fix" that by mounting gh's config into the sandbox; the `github` tool is the way.

**Without `gh`**, or for a fine-grained token limited to a few repositories, or for GitHub tools `gh` doesn't have, connect GitHub's official MCP server ([github/github-mcp-server](https://github.com/github/github-mcp-server)). It runs outside the sandbox with its own token, so the model never sees the token, read-only tools run without asking, and anything that changes something (a comment, a merge, a push) shows you an approval prompt first. (A `web_fetch` of a public GitHub page works without any setup; it just can't authenticate or act.)

### Set up the MCP server

1. Create a **fine-grained** personal access token ([github.com/settings/personal-access-tokens/new](https://github.com/settings/personal-access-tokens/new)): "Public repositories" or selected repositories only — the ones Marv should touch — with repository permissions `contents: read`, `issues: read`, `pull requests: read`, and `metadata: read` (selected automatically). Add the write permissions you want Marv to be able to use, e.g. `issues: write` and `pull requests: write` for comments and merging; every write still asks you first. Don't reuse the token the `gh` CLI uses (usually a classic one with full `repo` and `workflow` scope): a fine-grained token on a few repos limits the blast radius if something goes wrong.
2. Make the token available where Marv reads MCP config, without putting it in the file: export `MARV_GITHUB_TOKEN` in your shell (e.g. in `~/.profile`).
3. Write the server to `~/.marv/mcp.json` (yours, every project; for one project's `.mcp.json` instead, Marv asks you to trust it with `/mcp trust` before it starts):

```json
{ "mcpServers": { "github": {
  "command": "docker",
  "args": ["run", "-i", "--rm", "-e", "GITHUB_PERSONAL_ACCESS_TOKEN", "ghcr.io/github/github-mcp-server"],
  "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${MARV_GITHUB_TOKEN}" }
} } }
```

   `${MARV_GITHUB_TOKEN}` reads the environment variable, so the token never lands in the file. Needs [Docker](https://docs.docker.com/get-started/get-docker/); without it, [build the server from source](https://github.com/github/github-mcp-server?tab=readme-ov-file#build-from-source) and point `command`/`args` at the binary (`["<path>", "stdio"]`).

4. Restart Marv and check `/mcp`: the server should be listed with its tools. Try "list the open PRs in owner/repo" — that runs without an approval prompt (read-only), while "comment on PR 7" shows one first.

**Or with `/mcp add`.** `/mcp add` doesn't run Docker images yet, so the setup above stays a hand edit. GitHub also hosts the same server remotely (check [its README](https://github.com/github/github-mcp-server) for which accounts can use it); that one `/mcp add` can set up and start without a restart, with the same token from your environment:

```sh
/mcp add https://api.githubcopilot.com/mcp/ --name github --header "Authorization: Bearer ${MARV_GITHUB_TOKEN}"
```

**Or with an OAuth login instead of a token.** The token above is the simple path, and stays the recommended one. If you'd rather log in through the browser, GitHub's hosted server needs your own OAuth app (it offers no dynamic registration, and Marv ships no app of its own): create one under [Settings → Developer settings → OAuth Apps](https://github.com/settings/developers) with the callback URL `http://127.0.0.1:33418/callback`, export its secret as `MARV_GITHUB_CLIENT_SECRET`, and write:

```json
{ "mcpServers": { "github": {
  "type": "http", "url": "https://api.githubcopilot.com/mcp/",
  "oauth": { "clientId": "<your app's client id>", "clientSecret": "${MARV_GITHUB_CLIENT_SECRET}", "callbackPort": 33418 }
} } }
```

Then `/mcp login github` (see [Servers that need a login](#servers-that-need-a-login); `"scope"` there narrows what the token may do, e.g. `"repo read:org"`).

The official server registers dozens of tools. With many connected, Marv offers the model only their names and loads a tool's parameters when it needs one, so you don't have to trim — but limiting the toolsets still gives a small model a shorter list to choose from. The server takes `GITHUB_TOOLSETS` (or `--toolsets`), for example `default,actions` — the default is already `context`, `repos`, `issues`, `pull_requests`, `users`, which covers most issues/PR work; `GITHUB_TOOLSETS` wins when both are set:

```json
{ "mcpServers": { "github": {
  "command": "docker",
  "args": ["run", "-i", "--rm", "-e", "GITHUB_PERSONAL_ACCESS_TOKEN", "-e", "GITHUB_TOOLSETS", "ghcr.io/github/github-mcp-server"],
  "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${MARV_GITHUB_TOKEN}", "GITHUB_TOOLSETS": "default,actions" }
} } }
```

To run it strictly read-only, add `"GITHUB_READ_ONLY": "1"` to that env object (`-e GITHUB_READ_ONLY` in args): the server then offers no write tools at all, so merging or commenting is impossible rather than merely approved.

Copy the ready-made `github` skill into your personal skills to teach Marv the workflow: `mkdir -p ~/.marv/skills && cp -r examples/skills/github ~/.marv/skills/` (from a clone of this repository; the skill is [`examples/skills/github/SKILL.md`](examples/skills/github/SKILL.md)). It tells Marv to use the `github` tool (or the MCP tools without it) rather than `gh` in bash, to follow the branch → PR → merge-commit workflow, and never to ask for or print tokens.

## Agents

An agent type is a Markdown file, in the same format as Claude Code's agent files:

```markdown
---
name: code-reviewer
description: Reviews a change for bugs and missing tests. Use after implementing a task, before merging it.
tools: read_file, grep, glob, bash   # optional; Claude Code names (Read, Grep, Edit…) work too
model: inherit                       # optional; or a model id on the same provider
---

You are a careful code reviewer. Read the diff, then the code around it…
```

Put it in `.marv/agents/<name>.md` (this project) or `~/.marv/agents/<name>.md` (all projects); there's also a built-in `general-purpose` agent with every tool. A subagent starts with a fresh conversation that holds only the task it was given, and never gets the `agent` or `memory` tools. Only names and descriptions go into the system prompt, so write descriptions that say when to use the agent. A reference like `superpowers:code-reviewer` finds `code-reviewer`, so skills written for Claude Code work unchanged. `/agents` lists what loaded, and why any file didn't.

## Checks

A check is one rule of your project, in `.marv/checks/`, one file per rule. A simple rule is declarative (YAML or JSON):

```yaml
# .marv/checks/imports.yaml
glob: ["src/**/*.{ts,tsx}"]      # the files it reads; it runs after a step that changed one of them
exclude: src/generated/**        # optional
forbid: 'from\s+["''](\.{1,2}/[^"'']*?)(?<!\.tsx?)["'']'   # or require: a pattern each file must contain
flags: m                         # optional: i, m, s, u
message: relative import without its .ts extension
why: Bun runs the sources as they are, and the published package is type-checked from src/.
```

A rule that needs code is a script, `.marv/checks/<name>.ts`, run with bun from the project's root. Its first comments say which changes concern it (`// glob: src/**/*.ts`; without one it runs after any change), and it prints one JSON line per violation, `{"file": "src/a.ts", "line": 3, "message": "…", "why": "…"}`, then exits 0 (any other exit is reported as the check failing).

After a step that changed files, Marv runs the checks whose glob matches one of them and tells the model only what's new since the turn began (matched by check, file and message, not line), with each check's `why`. Checks are code from the repository, so like the typecheck they run only in the sandbox, with the project read-only and no network, never ask for approval, and don't run at all without the sandbox. `/checks` lists them, what couldn't be loaded, and the last result; `/checks off` (config `checks`, SDK option `checks`) turns them off. Marv's own repository has four in its `.marv/checks/`.

## Files and settings

| Path | Contents |
|---|---|
| `~/.marv/config.json` | Provider, model, API key (mode 0600) |
| `~/.marv/sessions/<project>/` | Saved conversations (private) |
| `~/.marv/trajectories/<project>/` | Every turn, step by step, with your ratings (private; `bun run stats` to sum up) |
| `~/.marv/memory/personal.md` | Personal memory, used in every project |
| `~/.marv/memory/projects/<project>.md` | Memory for one project (never stored in the repo) |
| `~/.marv/skills/` | Your personal skills |
| `.marv/skills/` | The project's skills |
| `~/.marv/agents/` | Your personal agent types |
| `~/.marv/mcp.json`, `.mcp.json` | MCP servers: yours, and the project's (`{"mcpServers": {"name": {"command": …} or {"type": "http", "url": …}}}`) |
| `~/.marv/mcp-trust.json` | Which project MCP servers you've trusted |
| `~/.marv/mcp-tokens.json` | MCP logins (`/mcp login`): OAuth clients and tokens, private (0600) |
| `.marv/agents/` | The project's agent types |
| `.marv/checks/` | The project's checks, run after the model's edits ([Checks](#checks)) |
| `~/.marv/worktrees/<project>/` | Subagents' worktrees while they run (removed when each finishes; its branch stays) |
| `AGENTS.md` | Project instructions, read into the system prompt at startup |

| Environment variable | Effect |
|---|---|
| `OPENROUTER_API_KEY` | Overrides the saved OpenRouter key |
| `MARV_MODEL` | Overrides the saved model |
| `MARV_MAX_STEPS` | Overrides the saved step limit (`0` or `off` never asks; `/steps`) |
| `OLLAMA_HOST` | Where Ollama runs (default `localhost:11434`) |
| `MARV_CONFIG_DIR` | Use a different folder instead of `~/.marv` |

Ollama's context window defaults to 32k tokens (`contextLength` in the config file). That is large enough for an agent to read code, and fits in 12 GB of VRAM for 9-12B models. Marv never sets it past what the model was trained for (`/api/show`'s context length), so a model trained on 8k isn't run at 32k; the config value is only ever the upper bound.

## Using Marv from scripts and other agents

`marv -p "<prompt>"` (or `marv run <prompt>`) runs one non-interactive turn in the current folder and prints the
reply to stdout. It loads the same project files and config the TUI does, so a worker started this way behaves
like an interactive session.

```sh
# Print the model's reply as text (default)
marv -p "add a test for the parser"

# Same with "run" alias
marv run "add a test for the parser"

# Get a JSON result with reply, reason, steps and usage
marv -p "what is the git log?" --output json

# Follow events as they happen (stream-json)
marv -p "update the README" --output stream-json

# Read the prompt from stdin
echo "refactor this file" | marv -p -

# Allow git writes and network commands without asking
marv -p "install the npm package" --approve git_write --approve network

# Continue the latest saved session
marv -p "now fix the other one" -c

# Use a different model
marv -p "audit the dependencies" --model "anthropic/claude-sonnet-5.5"
```

**Exit codes:**
- `0` — the model finished normally (the turn ended with reason `end` or `stop`).
- `1` — an error (the provider failed, or an internal error).
- `2` — the turn stopped before finishing: the step limit was reached (`--max-steps`), a loop was detected, or
  another reason the model didn't finish.
- `3` — approval needed but couldn't be asked (no one to approve a non-yolo-safe call), or the user interrupted it.

**Approvals.** With nobody to ask, what yolo mode allows (sandboxed commands without network, edits outside `.git`,
read-only tools) runs; everything else is refused as an error the model can read and work around. Pass
`--approve <scope>` to allow more: e.g. `--approve git_write --approve network`. A command that needs both (a `git push`) needs both named: `--approve git_write` alone never approves a command that also uses the network. The model doesn't stop, a refused
call just returns an error. `--approve process` lets the model start background processes (a server for its own tests;
`--approve network` too for one that shares the network); they all stop when the run ends.

**Options for `-p`:**
| Flag | Effect |
|---|---|
| `-p <prompt>` or `-p -` (stdin) | The prompt (required; `marv run <prompt>` is an alias) |
| `--output text` | Print the final reply as text (default) |
| `--output json` | Print a JSON object with `reply`, `reason`, `steps`, `usage` |
| `--output stream-json` | Print a JSON line per event as it happens |
| `--approve <scope>` | Auto-approve calls in this scope (repeatable) |
| `--max-steps <N>` | Steps before the turn stops (`0` never stops) |
| `--model <name>` | Use a different model |
| `-c` / `--continue` | Continue the latest saved session |
| `--version`, `--help` | Print version or help |

## Using Marv from code

Marv's engine is a library too: `@flologixai/marv/sdk` gives your Bun program the same agent the terminal runs
(the tools, subagents, MCP servers, compaction, saved sessions), without the terminal.

```sh
bun add @flologixai/marv
```

```ts
import { createSession } from "@flologixai/marv/sdk";

const session = await createSession({
  cwd: "/path/to/project",
  provider: { kind: "openrouter", apiKey: process.env.OPENROUTER_API_KEY! },
  // or { kind: "ollama", model: "qwen3.5:9b" }, or a Provider of your own
});

for await (const event of session.send("Fix the failing test")) {
  if (event.type === "text_delta") process.stdout.write(event.text);
}
await session.close();
```

- **Turns and events.** `send()` starts a turn at once and returns its events: `turn_start` first and
  `turn_end` last, exactly once however it ends, with the model's text, tool calls (`tool_start`/`tool_end`),
  subagents' events (`subagent`, tagged with the call that started them), `status` (waiting for MCP servers,
  compacting, checking, running), `check` (the typecheck after a file change: how many errors were new, a
  failure, or that it's off), `checks` (the same for the project's checks, `.marv/checks/`), compaction, and `cut_off` (a reply hit the output limit, so nothing in it ran; the
  model is told and goes on, and a second one in a row ends the turn) in between. A file change's `tool_end`
  (`edit_file`, `write_file`) carries `result.diff`: what changed, as numbered lines, for display; the model
  never sees it. Leaving the loop early interrupts the turn (and waits until
  every tool call it started has its result); so do `interrupt()` and a `signal`. A turn you never read still
  runs to the end. One turn at a time: `send()` and `clear()` throw during one, `resume()` and `compact()`
  reject; the session is free again just before `turn_end`, so you can send the next message from there.
- **Approvals.** Pass `approve: async (request) => "yes" | "always" | "no"` to decide what runs. Without it,
  only what needs no yes runs: read-only tools, what Marv's yolo mode vouches for (edits outside `.git`,
  sandboxed commands without network), subagents (shared-folder ones under these same rules; worktree ones
  when sandboxed), and MCP tools whose server says they're read-only. Anything else goes back to the model as
  refused, and it carries on. The sandbox is bubblewrap, on Linux: without it (macOS, say) every `bash`
  command needs an approver. With no one to ask, the 25-step limit is a hard stop too.
- **Not in your home folder with yolo.** Yolo's safe edits run before any approver is asked, so with `cwd`
  your home folder, a folder above it, or `/`, they'd change your dotfiles unasked. `createSession` refuses
  that unless you pass `yolo: false` (then every change goes to your approver, or is refused without one),
  and `configure({ yolo: true })` can't turn it back on there.
- **Review before you run.** What the agent writes can plant something you'll later run outside the
  sandbox. Edits that run unasked are kept out of `.git` (hooks, config), but a `package.json` script, a
  `Makefile` or an `.envrc` is an ordinary file. In a repository you don't trust, read the changes
  (`git diff`) before running anything in it.
- **Nothing from disk unless asked.** `sources: ["project"]` reads the repository's `AGENTS.md`, `.marv/` and
  `.mcp.json` (whose servers still need trusting); `"user"` reads your `~/.marv` (skills, agents, memory, MCP
  servers). Environment variables like `OPENROUTER_API_KEY` and `MARV_MODEL` are ignored: the session uses what
  you pass. `persist: true` saves the conversation where `marv -r` finds it, and `resume: id | "latest"` (with
  `persist`) continues one; `trajectories: true` logs every turn. `diagnostics` (on by default) typechecks
  TypeScript projects after the model's file changes and tells it about new errors; it needs the sandbox.
  `checks` (on by default; nothing runs without `.marv/checks/`) does the same with the project's own checks.
  `maxSteps` (default 25) sets how many steps a turn runs before it stops or asks; `0` never asks or stops the
  main agent (subagents keep their own limit). `plan` (on by default) offers the plan tool: `session.plan` is the
  model's checklist (or `null`), and each change also arrives as a `tool_end`'s `result.plan`; `plan: false`
  leaves the tool out for the whole session (it changes the tool list, so `configure()` can't toggle it).
  `processes` (on by default, where bubblewrap works) offers the process tool: each start goes to your approver
  (`door: "process"`, scope `process:<command>`), `session.processes` lists them, `session.stopProcess(name)`
  stops one, and `close()` stops them all.
- **MCP servers in code:** `mcpServers: { name: { command, args } }` (`.mcp.json`'s format). These are trusted like
  your own: no trust prompt, started in your home folder (use an absolute path, or `${MARV_PROJECT_DIR}` for
  the project). So never pass config read from a repository you don't control; `sources: ["project"]` is the
  way to use a repository's servers, and those still need trusting. `mcpTools` (`"auto"` by default) says how
  their tools are offered: `"eager"` sends every spec with every request, `"deferred"` only their names, with
  `tool_search` and `mcp_call` to load and run them; `"auto"` defers when the specs would cost more than
  min(4000 tokens, 10% of the context window).
- **Your own tools:** `tools: [{ name, description, input: z.object({...}), label, run }]`, offered to the
  main agent (not to subagents). `input` must be a zod 4 schema (it's described to the model with
  `z.toJSONSchema`). A name of a built-in tool (`tool_search` and `mcp_call` included), one used twice, or one
  starting with `mcp__` (MCP servers' tools) makes `createSession` throw. A Provider of your own is used for everything, subagents included.
- **Settings between turns.** `configure({ provider, thinking, sandbox, yolo, trajectories, diagnostics, checks, maxSteps, mcpTools })`: a turn reads its settings
  when it starts, so a change during one applies from the next. It throws only for an invalid provider (an
  unknown `kind`), a `maxSteps` that isn't a whole number of at least 0, or for turning yolo on in the home folder, and then nothing changed. `close()` stops what's running, waits for it, saves, and stops
  the MCP servers the session started; `send()` throws after it.
- **Files.** `configDir` (default `~/.marv`) moves config, memory, `mcp.json`, sessions, trajectories,
  worktrees and MCP trust; personal skills and agents are still read from `~/.marv` in your home folder, as
  the CLI does. A repository's MCP servers can only be trusted with the CLI's `/mcp trust`. `onWarning`
  hears what doesn't fit a turn's events (a trajectory that can't be written).
- **Bun only** (1.3 or newer). The SDK is TypeScript source, so your `tsconfig.json` needs
  `"moduleResolution": "bundler"` and `"allowImportingTsExtensions": true` (`bun init`'s defaults have both).

`examples/sdk.ts` is a complete script.

## Development

```sh
bun install
bun run dev          # run from source
bun test             # the test suite
bun run typecheck    # tsc --noEmit
bun run stats        # sum up your trajectory logs (--by model or --by marv)
```

The architecture and conventions are documented in [`CLAUDE.md`](CLAUDE.md) (also available as `AGENTS.md`): the provider seam, the agent loop, the tool registry, the prompt-cache rules, and how the TUI renders.

Three dependencies are patched (in [`patches/`](patches/), applied by `bun install`):

- **ink**: adds a `transformOutput` hook and `repaint()` (for drawing the mouse-selection highlight), skips drawing off-screen nodes, and skips a whole-tree search for `<Static>` on every update. Long sessions would otherwise slow every frame.
- **string-width**: caches results and skips a costly emoji regex for characters that can't be emoji, which was the biggest cost while scrolling.
- **ink-text-input**: ignores ctrl+letter (only letters: ctrl+arrows still move the cursor), so shortcuts like ctrl+o don't type the letter into the prompt.

Edit a patch with `bun patch <package>`, change the files in `node_modules/<package>`, then run `bun patch --commit node_modules/<package>`.

Patches only apply in a clone, so the published CLI is bundled with them: see Releasing.

### Releasing

```sh
bun run release:build                        # builds the package into release/
npm pack ./release                           # optional: the exact tarball, to install and try somewhere else
npm publish ./release --access public        # needs `npm login`; scoped packages are private by default
git tag v0.1.0 && git push origin v0.1.0
gh release create v0.1.0 --generate-notes
```

First move `CHANGELOG.md`'s Unreleased entries under the new version (patch for fixes, minor for features or anything
breaking while below 1.0), then bump `version` in `package.json` to match: a version number can only be published once. `release:build` bundles the CLI
(with the patched dependencies inside, since Bun applies patches only in this repository) into `release/dist/`, copies
`src/` for the SDK, and writes a `package.json` without `patchedDependencies`, the scripts or the devDependencies; the
SDK's dependencies are the packages its files import. The repository's own `package.json` is `private`, so a
`npm publish` from the root is refused.

## License

[MIT](LICENSE) © FlologixAI. "Marv" and the martian are FlologixAI's; forks are welcome under another name.

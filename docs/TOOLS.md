# Marv's built-in tools

<!-- Generated from the code (marv_tools::table). Don't edit: run
UPDATE_GOLDEN=1 cargo test -p marv-guards --test tools
to write it again. -->

Every built-in tool, in the order the model sees them, with the properties that decide how its calls are
approved and how its output is treated. A tool change shows up here as a diff.

## The tools

- **Kind**: `read` runs without asking; `write`, `execute` and `egress` ask unless the call says otherwise.
  Once the conversation holds outside text, `egress` and any call that reaches the network ask every time.
- **Source**: where the output comes from, at least (a call may be less trusted: see the calls). Its class:
  `authority` (the person, Marv) is not fenced; `workspace` (the project) is quoted; `outside` is fenced as
  data and taints the conversation.
- **Checkpoint**: the turn's checkpoint is taken before a call runs, so /undo can put it back.

| Tool | Kind | Source | Class | Checkpoint | Parallel |
|---|---|---|---|---|---|
| `read_file` | read | project | workspace | no | no |
| `glob` | read | project | workspace | no | no |
| `grep` | read | project | workspace | no | no |
| `web_fetch` | egress | web | outside | no | no |
| `github` | read | github | outside | no | no |
| `skill` | read | project | workspace | no | no |
| `edit_file` | write | project | workspace | yes | no |
| `write_file` | write | project | workspace | yes | no |
| `bash` | execute | project | workspace | yes | no |
| `process` | execute | project | workspace | yes | no |
| `memory` | write | marv | authority | no | no |
| `plan` | read | marv | authority | no | no |
| `agent` | execute | subagent | workspace | no | yes |

## Example calls

What each call does by itself, evaluated with the sandbox off and nothing tainted:

- **Asks**: whether it needs the person's approval (`no`: it runs without asking).
- **Every time**: asked every time, never answered by "don't ask again".
- **Network**: whether it can reach the network.
- **Source**: where this call's output comes from (its class in brackets).
- **Persists**: whether it writes something later conversations load as the person's own. In a conversation
  holding outside text, such a call is asked every time.
- **Door**: the door out of the sandbox it goes through, if any.

On top of this, a write to a protected path (docs/SECURITY.md: `.git`, a lifecycle script in `package.json`, ...)
is asked every time, even in yolo, and so is any call to the network once the conversation holds outside text.

| Call | Asks | Every time | Network | Source | Persists | Door |
|---|---|---|---|---|---|---|
| `read_file {"path":"src/main.rs"}` | no | no | no | project (workspace) | no | - |
| `glob {"pattern":"**/*.rs"}` | no | no | no | project (workspace) | no | - |
| `grep {"pattern":"fn main"}` | no | no | no | project (workspace) | no | - |
| `web_fetch {"url":"https://example.com/"}` | yes | no | yes | web (outside) | no | - |
| `web_fetch {"offset":2000,"url":"https://example.com/"}` | yes | no | yes | web (outside) | no | - |
| `github {"args":["issue","list"]}` | no | no | yes | github (outside) | no | github |
| `github {"args":["api","repos/o/r/actions/secrets"]}` | yes | yes | yes | github (outside) | no | github |
| `github {"args":["issue","comment","1","--body","done"]}` | yes | no | yes | github (outside) | no | github |
| `skill {"name":"release"}` | no | no | no | project (workspace) | no | - |
| `edit_file {"new_string":"b","old_string":"a","path":"src/main.rs"}` | yes | no | no | project (workspace) | no | - |
| `write_file {"content":"hi","path":"notes.txt"}` | yes | no | no | project (workspace) | no | - |
| `write_file {"content":"true","path":".git/hooks/pre-commit"}` | yes | no | no | project (workspace) | no | - |
| `bash {"command":"cargo test"}` | yes | no | no | project (workspace) | no | - |
| `bash {"command":"cargo fetch","network":true}` | yes | no | yes | network (outside) | no | - |
| `bash {"command":"git commit -m x","git_write":true}` | yes | no | no | project (workspace) | no | - |
| `process {"action":"start","command":"npm run dev"}` | yes | no | no | project (workspace) | no | process |
| `process {"action":"start","command":"npm run dev","network":true}` | yes | no | yes | network (outside) | no | process |
| `process {"action":"list"}` | no | no | no | project (workspace) | no | - |
| `memory {"action":"add","scope":"project","text":"Tests run with cargo nextest."}` | yes | no | no | marv (authority) | yes | - |
| `memory {"action":"remove","scope":"project","text":"nextest"}` | yes | no | no | marv (authority) | no | - |
| `plan {"items":[{"status":"in_progress","text":"Write the test"}]}` | no | no | no | marv (authority) | no | - |
| `agent {"description":"find the bug","prompt":"Find it."}` | no | no | no | subagent (workspace) | no | - |
| `agent {"description":"fix the bug","isolation":"worktree","prompt":"Fix it."}` | yes | no | no | subagent (workspace) | no | - |

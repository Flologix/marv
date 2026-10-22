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

- **Person** to **Subagent**: the least the call asks in each kind of turn: `never` (it runs without asking),
  `asks` (yolo or "don't ask again" may answer), `every time` (no yolo, no "don't ask again"), `refused` (not
  offered). `person`: the person typed it; `headless`: `marv -p`; `woken`: another agent's message woke the
  session; `radio`: a radio message, or a turn handling radio requests; `subagent`: a subagent of the person's
  turn (a subagent of a woken or radio turn asks as that turn does).
- **Network**: whether it can reach the network.
- **Source**: where this call's output comes from (its class in brackets).
- **Persists**: whether it writes something later conversations load as the person's own. In a conversation
  holding outside text, such a call is asked every time.
- **Door**: the door out of the sandbox it goes through, if any.

On top of this, a write to a protected path (docs/SECURITY.md: `.git`, a lifecycle script in `package.json`, ...)
is asked every time, even in yolo, and so is any call to the network once the conversation holds outside text.

| Call | Person | Headless | Woken | Radio | Subagent | Network | Source | Persists | Door |
|---|---|---|---|---|---|---|---|---|---|
| `read_file {"path":"src/main.rs"}` | never | never | never | never | never | no | project (workspace) | no | - |
| `glob {"pattern":"**/*.rs"}` | never | never | never | never | never | no | project (workspace) | no | - |
| `grep {"pattern":"fn main"}` | never | never | never | never | never | no | project (workspace) | no | - |
| `web_fetch {"url":"https://example.com/"}` | asks | asks | every time | every time | asks | yes | web (outside) | no | - |
| `web_fetch {"offset":2000,"url":"https://example.com/"}` | asks | asks | every time | every time | asks | yes | web (outside) | no | - |
| `github {"args":["issue","list"]}` | never | never | never | never | never | yes | github (outside) | no | github |
| `github {"args":["api","repos/o/r/actions/secrets"]}` | every time | every time | every time | every time | every time | yes | github (outside) | no | github |
| `github {"args":["issue","comment","1","--body","done"]}` | asks | asks | every time | every time | asks | yes | github (outside) | no | github |
| `skill {"name":"release"}` | never | never | never | never | never | no | project (workspace) | no | - |
| `edit_file {"new_string":"b","old_string":"a","path":"src/main.rs"}` | asks | asks | every time | every time | asks | no | project (workspace) | no | - |
| `write_file {"content":"hi","path":"notes.txt"}` | asks | asks | every time | every time | asks | no | project (workspace) | no | - |
| `write_file {"content":"true","path":".git/hooks/pre-commit"}` | asks | asks | every time | every time | asks | no | project (workspace) | no | - |
| `bash {"command":"cargo test"}` | asks | asks | every time | every time | asks | no | project (workspace) | no | - |
| `bash {"command":"cargo fetch","network":true}` | asks | asks | every time | every time | asks | yes | network (outside) | no | - |
| `bash {"command":"git commit -m x","git_write":true}` | asks | asks | every time | every time | asks | no | project (workspace) | no | - |
| `process {"action":"start","command":"npm run dev"}` | asks | asks | every time | every time | refused | no | project (workspace) | no | process |
| `process {"action":"start","command":"npm run dev","network":true}` | asks | asks | every time | every time | refused | yes | network (outside) | no | process |
| `process {"action":"list"}` | never | never | never | never | refused | no | project (workspace) | no | - |
| `memory {"action":"add","scope":"project","text":"Tests run with cargo nextest."}` | asks | asks | every time | every time | refused | no | marv (authority) | yes | - |
| `memory {"action":"remove","scope":"project","text":"nextest"}` | asks | asks | every time | every time | refused | no | marv (authority) | no | - |
| `plan {"items":[{"status":"in_progress","text":"Write the test"}]}` | never | never | never | never | refused | no | marv (authority) | no | - |
| `agent {"description":"find the bug","prompt":"Find it."}` | never | never | never | never | refused | no | subagent (workspace) | no | - |
| `agent {"description":"fix the bug","isolation":"worktree","prompt":"Fix it."}` | asks | asks | every time | every time | refused | no | subagent (workspace) | no | - |

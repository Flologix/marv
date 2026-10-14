# marv-guards (`tests/guards/`)

One test per rule in `docs/SECURITY.md` (#8, Marv issue #53's guard suite). It's a workspace test crate (`publish = false`), not part of any shipped crate, so the rules read as one list. CI runs it with `cargo test --workspace` and real bubblewrap (`.github/workflows/ci.yml` installs it). Tests that run bwrap skip, with a line on stderr, where it can't create namespaces.

Each guard's doc comment says how it was proven: the guard was reverted, and the test failed. The reverts were run as a script against this tree (one revert at a time, file restored after). Rules whose code isn't written yet have a guard marked `#[ignore = "needs …"]` with the body it will run.

| `SECURITY.md` rule | Test (`tests/guards/tests/…`) | Status |
|---|---|---|
| One static binary: no `.env`/`bunfig.toml` loader | `launch.rs::no_dotenv_loader_is_linked_in` | Proven |
| Config only from `~/.marv` or `$MARV_CONFIG_DIR` in the real env | `launch.rs::the_config_folder_comes_only_from_the_real_environment` | Proven |
| `printable()`: CSI/OSC/DCS, C0/C1, bidi | `launch.rs::printable_strips_escapes_controls_and_bidi` | Proven |
| … including headless text mode | `launch.rs::headless_text_output_is_printable` | Ignored: needs #11 |
| Read-only system, empty `/run` but the mesh plug | `sandbox.rs::system_is_read_only_and_run_is_empty_but_for_the_mesh_plug` | Proven (real bwrap) |
| No network unless approved, cleared env, own PID namespace | `sandbox.rs::no_network_cleared_environment_and_own_pid_namespace` | Proven (real bwrap) |
| Stand-in home, toolchains read-only, credentials masked | `sandbox.rs::the_home_is_a_stand_in_with_masked_credentials` | Proven (real bwrap) |
| Project writable, `.git` and protected paths read-only (at any level) | `sandbox.rs::the_project_is_writable_but_git_and_protected_paths_are_read_only` | Proven (real bwrap) |
| Protected files a command *creates* (at any depth outside `node_modules`/`target`/`.git`) | `sandbox.rs::protected_files_a_command_creates_are_found_and_reverted` | Proven (real bwrap) |
| … through the `bash` tool | `sandbox.rs::bash_reverts_protected_files_its_command_created` | Proven (real bwrap) |
| Process door: can't write outside the project | `process.rs::a_process_cant_write_outside_the_project` | Proven (real bwrap, #24) |
| Process door: can't change `.git` | `process.rs::a_process_cant_change_the_projects_git` | Proven (real bwrap, #24) |
| Process door: can't read the real home | `process.rs::a_process_cant_read_the_real_home` | Proven (real bwrap, #24) |
| Process door: no host `127.0.0.1` without `network: true` | `process.rs::a_process_cant_reach_the_hosts_localhost_without_network` | Proven (real bwrap, #24) |
| Process door: forwarded ports on the host's `127.0.0.1` only | `process.rs::a_forwarded_port_is_on_the_hosts_127_0_0_1_only` | Proven (real bwrap, #24) |
| Process door: dies on stop | `process.rs::a_process_dies_on_stop` | Proven (real bwrap, #24) |
| Process door: dies when Marv is SIGKILLed | `process.rs::a_process_dies_when_marv_is_sigkilled` | Proven (real bwrap, #24) |
| Process door: a start asks even in yolo; a no starts nothing | `process.rs::a_start_asks_even_in_yolo_and_a_no_starts_nothing` | Proven (#24) |
| Process door: a subagent can't start one | `process.rs::a_subagents_context_cant_start_one` | Proven (#24) |
| Protected paths ask even in yolo (every pattern, any level, through a symlink) | `sandbox.rs::protected_paths_ask_even_in_yolo` | Proven |
| `package.json` lifecycle scripts | `sandbox.rs::package_json_lifecycle_scripts_ask_even_in_yolo` | Proven (#4) |
| Secrets need approval to read | `sandbox.rs::reading_a_secret_asks` | Ignored: no secret-read gate yet |
| Secrets masked in the sandbox | `sandbox.rs::secrets_are_masked_in_the_sandbox` | Ignored: no project secret masks yet |
| Every path checked after `realpath` | `sandbox.rs::no_tool_follows_a_symlink_out_of_the_project` | Proven |
| … a dangling symlink out of the project or into `.git` | `sandbox.rs::no_write_through_a_dangling_symlink_out_of_the_project` | Proven |
| … skill files and folders | `sandbox.rs::skill_files_and_folders_stay_inside_after_realpath` | Proven |
| … instruction files: project, parent and nested `AGENTS.md`/`CLAUDE.md` | `sandbox.rs::instruction_files_stay_inside_after_realpath` | Proven (#62) |
| … and inside the project, never a secret or a `node_modules`/`target`/`.git` target | `sandbox.rs::instruction_files_never_read_secrets_or_skipped_folders` | Proven (#62, two reverts) |
| Results tagged; outside text fenced, never Marv's voice | `provenance.rs::outside_text_is_tagged_and_fenced_as_data` | Proven |
| Web/MCP/mesh/image content taints | `provenance.rs::outside_content_taints_the_session` | Proven |
| Tainted: network bash asks | `provenance.rs::tainted_network_bash_must_ask` | Proven |
| Tainted: every MCP call asks, `readOnlyHint` too | `provenance.rs::tainted_mcp_calls_ask_even_read_only_ones` | Proven (two locks: both reverted) |
| Tainted: web requests to a new prefix; "always" covers a prefix, not the site | `provenance.rs::tainted_web_fetch_to_a_new_prefix_asks` | Proven (#5; three reverts) |
| Tainted: mesh calls on unapproved wires | `provenance.rs::mesh_egress_is_default_deny_and_asks_once_tainted` | Proven |
| Tainted: writes to protected paths | `sandbox.rs::protected_paths_ask_even_in_yolo` (`must_ask`) | Proven |
| `must_ask` never answered by "always" | `provenance.rs::always_never_answers_must_ask` | Proven |
| Approvals show the full payload | `provenance.rs::approvals_show_the_full_payload` | Proven |
| A network approval lists scripts written this session | `provenance.rs::a_network_approval_lists_the_scripts_it_runs` | Ignored: not built yet |
| A mesh message that wakes an idle Marv is fenced and taints; its turn's egress asks (#78) | `mesh.rs::a_woken_turn_reads_its_message_fenced_and_tainted_and_egress_asks` | Proven |
| Subagents don't inherit network/web/mesh approvals | `provenance.rs::subagents_inherit_taint_but_no_egress_approvals` | Proven |
| A worktree subagent's `.git` is read-only; Marv commits for it | `worktree.rs::a_worktree_subagents_git_is_read_only_and_marv_commits_for_it` | Proven (real bwrap, #12) |
| … Marv's commit trusts nothing the worktree says | `worktree.rs::marvs_commit_trusts_nothing_the_worktree_says` | Proven (#12) |
| github: what gh can never do never reaches it, whatever the approver says | `github.rs::what_can_never_run_never_reaches_gh_and_isnt_asked` | Proven (fake gh) |
| github: writes and sensitive reads ask in yolo; a no runs nothing | `github.rs::writes_and_sensitive_reads_ask_in_yolo_and_a_no_runs_nothing` | Proven |
| github: a token never reaches the model, events, saved session or trajectory | `github.rs::a_token_in_ghs_output_never_reaches_the_model_the_events_the_saved_session_or_the_trajectory` | Proven |
| github: a subagent only reads | `github.rs::a_subagent_reads_but_its_write_is_refused_even_with_the_approver_saying_yes` | Proven |
| github: not offered where gh is logged out | `github.rs::not_offered_where_gh_is_logged_out_not_even_to_subagents` | Proven |
| github: `marv -p` needs `--approve github` for a write | `github.rs::headless_a_write_is_refused_without_approve_github` | Proven (the marv binary) |
| host: asks in yolo, as the host door, `once` | `host.rs::asks_in_yolo_with_the_host_door_and_once` | Proven (real bwrap) |
| host: "always" never covers a second call | `host.rs::never_uses_always_two_calls_two_asks` | Proven (real bwrap) |
| host: refused before any ask without `host_commands` | `host.rs::a_session_without_host_commands_refuses_it_before_its_approver_is_asked` | Proven (real bwrap) |
| host: a subagent can't | `host.rs::a_subagent_cant_even_where_the_session_has_the_door` | Proven (real bwrap) |
| host: `marv -p` refuses it with every `--approve` name | `host.rs::headless_refuses_it_with_every_approve_name` | Proven (two layers, both reverted) |
| Project MCP servers need trust; hash covers entry and files | `mcp.rs::project_servers_need_trust_covering_entry_and_files` | Proven |
| `npx`/`uvx` outside the project | `mcp.rs::package_runners_start_outside_the_project` | Proven |
| Server `instructions` ignored | `mcp.rs::server_instructions_never_leave_the_parser` | Proven (a source check until the manager connects, M7) |
| Hidden web text: hiding classes | `web.rs::text_hidden_by_a_class_is_dropped` | Proven (#5) |
| Hidden web text: off-screen and zero-size styles | `web.rs::text_hidden_off_screen_or_in_a_zero_size_box_is_dropped` | Proven (#5) |
| Hidden web text: zero-width and tag characters | `web.rs::invisible_characters_never_reach_the_model` | Proven (#5) |
| Hidden web text: HTML comments in raw Markdown | `web.rs::html_comments_in_raw_markdown_are_dropped` | Proven (#5) |
| Images: any image taints | `images.rs::an_image_taints` | Proven |
| Images: sources, quarantined reader, sandboxed decoding | `images.rs::*` | Ignored: needs vision |

## Notes

- **Found by the suite:** dangling symlinks. `innocent.sh -> .envrc`, where `.envrc` didn't exist yet, wasn't seen as protected, and `write_file` through `out.sh -> /elsewhere/x` wrote outside the project. Both are fixed in `paths::real_with_missing` (sandbox.md). Nested `.git` folders read-only for confined commands are proven by marv-tools' `an_approved_git_write_keeps_its_new_repository_and_confined_commands_see_nested_git_read_only`.
- **Fake homes live in `/var/tmp`** (`TempDir::outside_tmp`): the sandbox replaces `/tmp` with its own, so a fake home there would look hidden even without the home tmpfs.
- **The web guards use a local server** (`marv_guards::serve`) and a `WebFetch` that lets private addresses through: the guards never fetch the internet.
- **The process door's guards** run `marv-guards-fixture` (`src/bin/`) as the relay, in place of `marv __relay`: a library's tests can't run the `marv` binary. The same fixture serves the forwarded test page from inside the sandbox and plays the parent that gets SIGKILLed. Each guard was proven by a scripted revert (one at a time, file restored after).
- **The doors' guards** (`github.rs`, `host.rs`): gh is a fake on PATH (`marv_tools::github::fake`, a shell script that logs every argv); never the real gh. The SDK guards that need it in the environment re-run themselves in a child process with it set (`marv_guards::in_child_with_env`: Rust can't change its own environment without `unsafe`). The `marv -p` guards run the binary `cargo test --workspace` builds (`marv_oracle::binary()`), against `marv_oracle`'s fake OpenRouter on port 0.
- **The lock-file guard** reads `Cargo.lock` at run time. To prove it, the test binary was built first, because cargo prunes an unused lock entry before it runs a test.

# marv-guards (`tests/guards/`)

One test per rule in `docs/SECURITY.md` (#8, Marv issue #53's guard suite). It's a workspace test crate (`publish = false`), not part of any shipped crate, so the rules read as one list. CI runs it with `cargo test --workspace` and real bubblewrap (`.github/workflows/ci.yml` installs it). Tests that run bwrap skip, with a line on stderr, where it can't create namespaces.

Each guard's doc comment says how it was proven: the guard was reverted, and the test failed. The reverts were run as a script against this tree (one revert at a time, file restored after). Rules whose code isn't written yet have a guard marked `#[ignore = "needs …"]` with the body it will run.

| `SECURITY.md` rule | Test (`tests/guards/tests/…`) | Status |
|---|---|---|
| One static binary: no `.env`/`bunfig.toml` loader | `launch.rs::no_dotenv_loader_is_linked_in` | Proven |
| Config only from `~/.marv` or `$MARV_CONFIG_DIR` in the real env | `launch.rs::the_config_folder_comes_only_from_the_real_environment` | Proven |
| `printable()`: CSI/OSC/DCS, C0/C1, bidi | `launch.rs::printable_strips_escapes_controls_and_bidi` | Proven |
| … including headless text mode | `launch.rs::headless_text_output_is_printable` | Ignored: needs #11 |
| macOS (Seatbelt, #166): Mach/XPC doors, sockets, the runtime folder but the plug, the closed home, writes, protected names and hard links, no IP without network, host processes and setuid, the environment, process lifetime | `seatbelt.rs` (14 guards, `require_sandbox!`, macOS only) | Run on the macOS job with `MARV_REQUIRE_SANDBOX=1`. Proven on macOS CI by removing the rule (branch `proof/166-seatbelt-rules`, PR #178): the home deny, the hidden-entry regex, `~/Library`, the protected write denies, the pid kill before the group's; the link refusals hold with the link rules removed (Seatbelt refuses `link()` from a source it can't read or write) |
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
| Saved tool outputs: read_file/grep read only the session's folder; no writes, bash can't see it | `sandbox.rs::only_reading_tools_reach_the_sessions_saved_outputs_and_nothing_else_outside` | Proven (#61; two reverts; bash part real bwrap) |
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
| Subscriptions (#159): a provider never gets another provider's key | `subscriptions.rs::a_providers_key_never_goes_to_another_provider` | Proven |
| Subscriptions: a plan's usage limit is a hard stop (one request, never retried, no other key) | `subscriptions.rs::plan_limit::a_plans_limit_is_a_hard_stop` | Proven |
| Sign in with ChatGPT: the loopback listener is 127.0.0.1 only, needs the state, takes one callback | `subscriptions.rs::sign_in_listener::the_sign_in_listener_is_loopback_only_needs_the_state_and_takes_one_callback` | Proven |
| Sign in with ChatGPT: an idle local connection doesn't stall the callback | `subscriptions.rs::sign_in_listener::an_idle_connection_doesnt_stall_the_sign_in` | |
| Sign in with ChatGPT: the token goes only to api.openai.com, whatever baseUrl says | `subscriptions.rs::the_chatgpt_token_is_never_sent_to_another_host` | |
| Sign in with ChatGPT: the listener times out and stops on cancel | `subscriptions.rs::sign_in_listener::the_sign_in_listener_times_out` | |
| Sign in with ChatGPT: tokens never in `Debug` output | `subscriptions.rs::sign_in_listener::the_sign_in_tokens_never_show_in_debug_output` | |
| Radio (#124): a project's config can't turn it (or wake) on; the startup note | `radio.rs::a_projects_config_cant_turn_the_radio_on` | Proven |
| Radio: off unless the user's own config turns it on (no env variable, no flag) | `radio.rs::the_radio_is_off_unless_the_users_config_turns_it_on` (+ `marv-cli`'s `cli::tests::no_flag_turns_the_radio_on`) | Proven |
| Radio: a Marv with it off refuses joins and sends no presence | `radio.rs::a_marv_with_the_radio_off_refuses_joins_and_sends_no_presence` | Proven |
| Radio: a join from a process that isn't the named Marv is refused (pid) | `radio.rs::a_join_from_a_process_that_isnt_the_named_marv_is_refused` | Proven (python3 for the other process) |
| Radio: a frame that names its own sender is refused (the sender is the link) | `radio.rs::a_frame_that_names_its_own_sender_is_refused` | Proven (two reverts) |
| Radio: presence has exactly the listed fields | `radio.rs::presence_has_exactly_the_listed_fields` | Proven (two reverts) |
| Radio: presence never reaches a model and doesn't taint | `radio.rs::presence_never_reaches_a_model_and_doesnt_taint` | Proven (two reverts) |
| Radio: sandboxed commands can't see it | `radio.rs::sandboxed_commands_cant_see_the_radio` | Proven (real bwrap) |
| Radio: a killed peer shows lost and its socket is swept | `radio.rs::a_killed_peer_shows_lost_and_its_socket_is_swept` | Proven (two reverts; the peer is this test binary run again) |
| Radio: a silent peer greys, then drops | `radio.rs::a_silent_peer_greys_then_drops` | Proven (two reverts) |
| Radio (phase 2): radio text is fenced and taints before the request that carries it | `radio.rs::radio_text_is_fenced_and_taints_before_the_request_that_carries_it` | Proven |
| Radio: a body can't close the radio fence | `radio.rs::a_body_cant_close_the_radio_fence` | Proven |
| Radio: egress after radio text asks, even in yolo | `radio.rs::egress_after_radio_text_asks_even_in_yolo` | Proven (real bwrap) |
| Radio: nothing on the radio can answer an approval | `radio.rs::nothing_on_the_radio_can_answer_an_approval` (+ `marv-cli`'s `mesh_wake::a_radio_message_never_wakes_while_an_approval_waits`) | Proven (two reverts) |
| Radio: `by: user` is shown but changes nothing | `radio.rs::by_user_is_shown_but_changes_nothing` | Proven |
| Radio: the model can't turn the radio on | `radio.rs::the_model_cant_turn_the_radio_on` | Proven |
| Radio: a turn started by the radio asks for what yolo would have allowed | `radio.rs::a_turn_started_by_the_radio_asks_for_what_yolo_would_have_allowed` | Proven (two reverts) |
| Radio: subagents have no radio | `radio.rs::subagents_have_no_radio` | Proven |
| Radio: `marv -p` is never on the radio | `radio.rs::marv_p_is_never_on_the_radio` | Proven (the real binary) |
| Radio: `radio_tx`'s spec is constant and stays after the radio goes off | `radio.rs::radio_tx_spec_is_constant_and_stays_after_radio_off` | Proven |
| Radio: history stays append-only when radio notes arrive | `radio.rs::history_stays_append_only_when_radio_notes_arrive` | Proven |
| Radio: oversize messages are refused, not cut | `radio.rs::oversize_messages_are_refused_not_cut` | Proven (two reverts) |
| Radio: per-sender rates apply at the receiver | `radio.rs::per_sender_rates_apply_at_the_receiver` | Proven |
| Radio: broadcasts never wake | `radio.rs::broadcasts_never_wake` | Proven |
| Radio: radio wakes are limited per sender and overall | `radio.rs::radio_wakes_are_limited_per_sender_and_overall` | Proven (two reverts) |
| Radio: a slow peer never stalls the sender | `radio.rs::a_slow_peer_never_stalls_the_sender` | Proven |
| Radio: every frame is logged without its text | `radio.rs::every_frame_is_logged_without_its_text` | Proven |
| Radio (phase 3): `ack`, `done` and `failed` never get a reply or a report | `radio.rs::ack_done_and_failed_never_get_a_reply_or_a_report` | Proven |
| Radio: a request chain stops at two hops | `radio.rs::a_request_chain_stops_at_two_hops` | Proven |
| Radio: a report wakes the requester only for a request from a user's turn | `radio.rs::a_report_wakes_the_requester_only_for_a_request_from_a_users_turn` | Proven |
| Radio: a request gets one ack and one report (also when the model replied itself) | `radio.rs::a_request_gets_one_ack_and_one_report` | Proven |
| Radio: the report is built without a model call | `radio.rs::the_report_is_built_without_a_model_call` (+ `marv-cli`'s `tests/radio.rs::a_request_is_acked_handled_and_reported_back`) | Proven |
| Radio: failed reports for stopped, declined and errored turns | `radio.rs::failed_reports_for_stopped_declined_and_errored_turns` | Proven |
| Radio: a request waits while busy and is read only at a turn's start | `radio.rs::a_request_waits_while_busy_and_is_read_only_at_a_turns_start` | Proven |
| Radio: open requests survive compaction and resume | `radio.rs::open_requests_survive_compaction_and_resume` | Proven |
| Radio: a Marv leaving mid-request reports failed | `radio.rs::a_marv_leaving_mid_request_reports_failed` | Proven |
| Radio: a turn that handles a request asks for what yolo would have allowed (marv-rs's decision, past spec §8.4) | `radio.rs::a_turn_that_handles_a_request_asks_even_in_yolo` | Proven |
| Radio: `radio_tx`'s wait returns the report fenced, tainting first | `radio.rs::radio_tx_waits_for_the_report_fenced_and_tainting` | Proven |
| Radio: a pane send to a Marv on the radio is refused, with the radio hint | `radio.rs::pane_send_to_a_marv_on_the_radio_is_refused_with_the_radio_hint` | Proven (a fake `herdr`) |
| Subagents don't inherit network/web/mesh approvals | `provenance.rs::subagents_inherit_taint_but_no_egress_approvals` | Proven |
| … the config's pre-approved scopes too (#98) | `approvals.rs::subagents_dont_inherit_config_scopes` | Proven |
| Pre-approved scopes answer their own scopes (#98) | `approvals.rs::config_approve_answers_its_scopes_without_asking` | Proven (session level) |
| Pre-approved never cover tainted egress (#98) | `approvals.rs::config_approve_never_covers_tainted_egress` | Proven (the `must_ask` half reverted) |
| Pre-approved never cover protected paths (#98) | `approvals.rs::config_approve_never_covers_protected_paths` | Proven (same revert) |
| Pre-approved never cover lifecycle scripts (#98) | `approvals.rs::config_approve_never_covers_lifecycle_scripts` | Proven (same revert) |
| Pre-approved never grant a mesh wire (#98) | `approvals.rs::config_approve_never_grants_a_mesh_wire` | Proven (construction: `wire_approval` is `must_ask`+`once`) |
| In the home folder, "always" on a file change still asks the next time | `approvals.rs::in_the_home_folder_always_on_a_file_change_still_asks_the_next_time` | Proven (`home_file_write` dropped from the registry's `once` → failed) |
| … the request is `once` (no "always" offered) | `approvals.rs::in_the_home_folder_a_file_change_request_is_once` | Proven (same revert) |
| … config `"approve": ["files"]` doesn't cover it | `approvals.rs::in_the_home_folder_config_approve_files_doesnt_cover_a_file_change`, `headless_config_approve_files_in_the_home_folder_is_refused` | Proven (same revert; the second through `marv -p`) |
| … a worktree subagent's automatic yes doesn't cover a `once` request | `approvals.rs::a_subagents_automatic_yes_doesnt_cover_a_once_request` | Proven (`!request.once` dropped from `SubagentApprover` → failed) |
| A project under the home keeps "always" for file changes | `approvals.rs::a_project_under_the_home_keeps_always_for_file_changes` | Proven (`home_file_write` true for every project → failed) |
| `marv -p --approve files` still writes in the home folder | `approvals.rs::headless_approve_files_in_the_home_folder_still_writes` | Behaviour lock (the chosen rule) |
| Writable entries refused at startup (#98) | `sandbox.rs::refused_writable_entries_are_refused_at_startup` | Proven (any hidden top-level home entry: the rule dropped → failed) |
| A writable folder that is or holds a git repo refused (#98 review) | `sandbox.rs::git_repositories_are_refused_as_writable_entries` | Proven (the `.git` checks dropped → failed) |
| A writable folder writable, nothing else new (#98) | `sandbox.rs::the_writable_folder_is_writable_and_nothing_else_new_is` | Proven (real bwrap; the session merge dropped → failed) |
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
| herdr: the sandbox sees neither Herdr's variables nor its socket | `herdr.rs::a_sandboxed_command_sees_neither_herdrs_variables_nor_its_socket` | Proven (real bwrap) |
| herdr: what the door reads is fenced and taints | `herdr.rs::what_the_door_reads_back_is_fenced_and_taints_the_conversation` | Proven (fake herdr) |
| herdr: run and spawn ask every time (yolo, "always", every pre-approved scope) | `herdr.rs::run_and_spawn_ask_every_time_even_in_yolo_after_always_and_with_pre_approved_scopes` | Proven |
| herdr: refused before any ask without `host_commands` | `herdr.rs::a_session_without_host_commands_refuses_run_and_spawn_before_asking` | Proven |
| herdr: `marv -p` refuses run and spawn with every `--approve` name | `herdr.rs::headless_refuses_run_and_spawn_with_every_approve_name` | Proven (two layers, both reverted; the marv binary) |
| herdr: a subagent's context refuses run and spawn | `herdr.rs::a_subagents_context_refuses_run_and_spawn` | Proven |
| herdr: a spawned worker gets no more than its parent | `herdr.rs::a_spawned_worker_never_gets_more_than_its_parent` | Proven (two reverts) |
| herdr: the peer limit holds before any ask | `herdr.rs::the_peer_limit_holds_before_anyone_is_asked` | Proven |
| herdr: a light action in a woken turn gets no "always" | `herdr.rs::a_light_action_in_a_woken_turn_gets_no_always` | Proven |
| herdr: names and output reach the terminal through `printable()` | `herdr.rs::herdr_names_and_output_reach_the_terminal_only_through_printable` | Proven |
| A turn a mesh message wakes runs with automatic approval off: yolo asks there (#74) | `mesh.rs::an_inbox_woken_turn_asks_for_what_yolo_would_allow` | Proven |
| herdr: `/peers stop` types nothing into a busy or asking agent | `herdr.rs::stopping_a_peer_types_nothing_into_an_agent_thats_busy_or_asking` | Proven |
| doors: a `gh`/`herdr` planted in the project or a relative PATH entry is never run | `herdr.rs::a_door_program_planted_in_the_project_is_never_run` | Proven |
| herdr: nothing starting with `-` reaches the CLI | `herdr.rs::nothing_starting_with_a_dash_reaches_the_herdr_cli` | Proven (four reverts) |
| herdr: a worker spawned from a tainted conversation starts tainted | `herdr.rs::a_worker_spawned_from_a_tainted_conversation_starts_tainted` | Proven (two reverts; the marv binary) |
| herdr: a tainted prompt asks every time and says whose words | `herdr.rs::a_prompt_in_a_tainted_conversation_asks_every_time_and_says_whose_words` | Proven (two reverts) |
| `marv -p`: tainted egress refused with `--approve web` alone | `headless_taint.rs::headless_refuses_tainted_egress_with_approve_alone` | Proven (the marv binary) |
| `marv -p`: `--approve-tainted` opts in; untainted `--approve web` unchanged | `headless_taint.rs::headless_approve_tainted_allows_tainted_egress`, `headless_approve_web_still_allows_untainted_egress` | Controls |
| `marv -p`: a protected-path write needs `--approve-protected` | `headless_taint.rs::headless_refuses_protected_writes_without_approve_protected` | Proven (the marv binary) |
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
- **The Herdr door's guards** (`herdr.rs`) run a fake `herdr` (`marv_session::herdr_door::fake`, a shell script that logs every call and keeps its state in its own folder): never the user's Herdr, never `~/.config/herdr`. The `marv -p` one gives the binary a cleared environment with `HERDR_ENV` and a socket path that doesn't exist. Run the suite with Herdr's variables unset when you're inside Herdr.
- **The process door's guards** run `marv-guards-fixture` (`src/bin/`) as the relay, in place of `marv __relay`: a library's tests can't run the `marv` binary. The same fixture serves the forwarded test page from inside the sandbox and plays the parent that gets SIGKILLed. Each guard was proven by a scripted revert (one at a time, file restored after).
- **The doors' guards** (`github.rs`, `host.rs`): gh is a fake on PATH (`marv_tools::github::fake`, a shell script that logs every argv); never the real gh. The SDK guards that need it in the environment re-run themselves in a child process with it set (`marv_guards::in_child_with_env`: Rust can't change its own environment without `unsafe`). The `marv -p` guards run the binary `cargo test --workspace` builds (`marv_oracle::binary()`), against `marv_oracle`'s fake OpenRouter on port 0.
- **The lock-file guard** reads `Cargo.lock` at run time. To prove it, the test binary was built first, because cargo prunes an unused lock entry before it runs a test.

//! Settings and memory from the TUI: saving the config file, `/memory`, ratings, `/clear`, and applying a command's action.

use super::*;

impl Bridge {
    /// Saves the config, has the session use it from its next turn, and says `notice`.
    pub(super) fn save_config(&self, next: FileConfig, notice: String) {
        if let Err(e) = self.store.save(&next) {
            self.emit(system(format!("Could not save config: {e}"), true));
            return;
        }
        let config = with_model(resolve_config(Some(&next), &self.env), self.session_model().as_deref());
        *self.file.lock().unwrap_or_else(|e| e.into_inner()) = Some(next);
        let configured = self.session.configure(Configure {
            provider: Some(factory(&config, &self.make_provider, &self.load_models)),
            thinking: Some(config.thinking),
            sandbox: Some(config.sandbox),
            yolo: Some(config.yolo && self.session_yolo_allowed()),
            approve_sandboxed: Some(config.approve_sandboxed),
            trajectories: Some(config.trajectories),
            diagnostics: Some(config.diagnostics),
            checks: Some(config.checks),
            checkpoints: Some(config.checkpoints),
            max_steps: Some(config.max_steps),
            mcp_tools: Some(config.mcp_tools),
        });
        if let Err(e) = configured {
            self.emit(system(e.to_string(), true));
        }
        self.emit(UiEvent::StatusInfo(self.status_info()));
        self.emit(system(notice, false));
    }

    pub(super) fn with_file(&self, edit: impl FnOnce(&mut FileConfig)) -> FileConfig {
        let config = self.config();
        let mut next = self.file().unwrap_or_else(|| FileConfig { provider: Some(config.provider), ..FileConfig::default() });
        edit(&mut next);
        next
    }

    pub(super) async fn show_memory(&self) {
        let Some(paths) = &self.memory else {
            return self.emit(system("Memory isn't available.", false));
        };
        let memories = load_memory(paths).await.unwrap_or_default();
        let list = |entries: &[String]| {
            if entries.is_empty() {
                "*(nothing yet)*".to_string()
            } else {
                entries.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n")
            }
        };
        let text = [
            format!("**Personal** (all projects): `{}`", shorten_home(&paths.personal, &self.home)),
            list(&memories.personal),
            format!("**This project**: `{}`", shorten_home(&paths.project, &self.home)),
            list(&memories.project),
            "Edit these files directly, use `/remember` and `/forget`, or ask Marv to remember or forget something.".into(),
        ]
        .join("\n\n");
        self.emit(UiEvent::Message(view::Message { markdown: true, ..message(Role::System, text) }));
    }

    pub(super) async fn remember(&self, scope: MemoryTarget, text: &str) {
        let Some(paths) = &self.memory else { return };
        let (path, name) = match scope {
            MemoryTarget::Personal => (&paths.personal, "personal"),
            MemoryTarget::Project => (&paths.project, "project"),
        };
        match marv_tools::memory::add_memory(path, text).await {
            Ok(true) => self.emit(system(format!("Saved to {name} memory. {NEXT_TIME}"), false)),
            Ok(false) => self.emit(system(format!("That's already in {name} memory."), false)),
            Err(e) => self.emit(system(e.0, true)),
        }
    }

    pub(super) async fn forget(&self, text: &str) {
        let Some(paths) = &self.memory else { return };
        // Look in both scopes; remove only when exactly one memory matches.
        let mut matches: Vec<(&Path, String)> = Vec::new();
        for path in [&paths.personal, &paths.project] {
            for m in marv_tools::memory::find_memory(path, text).await {
                matches.push((path, m));
            }
        }
        if matches.len() != 1 {
            let text = if matches.is_empty() {
                format!("No memory matches \"{text}\".")
            } else {
                let quoted: Vec<String> = matches.iter().map(|(_, m)| format!("\"{m}\"")).collect();
                format!("{} memories match; use more of the text: {}", matches.len(), quoted.join(", "))
            };
            return self.emit(system(text, true));
        }
        let (path, m) = &matches[0];
        match marv_tools::memory::remove_memory(path, m).await {
            Ok(removed) => self.emit(system(format!("Forgot: {removed}"), false)),
            Err(e) => self.emit(system(e.0, true)),
        }
    }

    /// `/good`, `/bad`, `/label`: feedback on the latest turn of this session, in its trajectory.
    pub(super) fn rate(&self, score: i8, note: Option<String>, labels: Option<Vec<String>>) {
        let outcome = self.session.rate(Feedback { score, note: note.clone(), labels: labels.clone() });
        match outcome {
            RateOutcome::Off => {
                self.emit(system("Trajectory logging is off, so there's nothing to rate (/trajectories on).", true))
            }
            RateOutcome::Nothing => {
                self.emit(system("Nothing to rate yet: ratings apply to the last turn of this conversation.", true))
            }
            RateOutcome::Rated => {
                let what = match &labels {
                    Some(labels) => format!("Labeled the last turn: {}", labels.join(", ")),
                    None => format!("Rated the last turn: {}", if score > 0 { "good" } else { "bad" }),
                };
                let note = note.map(|n| format!(" ({n})")).unwrap_or_default();
                self.emit(system(format!("{what}{note}."), false));
            }
        }
    }

    pub(super) async fn clear(self: &Arc<Self>) {
        // A new conversation (and session file), with the memories saved during the last one.
        if let Err(e) = self.session.clear().await {
            return self.emit(system(format!("Couldn't start a new conversation: {e}"), true));
        }
        self.emit(UiEvent::Clear);
        self.emit(UiEvent::Welcome(self.welcome()));
        self.radio_title();
        self.show_hooks();
        self.report_session().await;
    }

    /// Applies one command's action (Marv's `handleSubmit` switch).
    pub(super) async fn apply(self: &Arc<Self>, line: &str, action: CommandAction) {
        let config = self.config();
        match action {
            CommandAction::Print { text, is_error, markdown } => {
                self.emit(UiEvent::Message(view::Message { is_error, markdown, ..message(Role::System, text) }))
            }
            CommandAction::Clear => self.clear().await,
            CommandAction::Setup => {
                let file = self.file();
                self.emit(UiEvent::OpenSetup(setup_context(SetupStart::Reconfigure, file.as_ref(), &config, &self.env)));
            }
            CommandAction::Model { id: None } => {
                let file = self.file();
                self.emit(UiEvent::OpenSetup(setup_context(SetupStart::Model, file.as_ref(), &config, &self.env)));
            }
            // For this session only: trying a model mustn't change the default every other Marv starts with (it
            // used to save it). /model without an id opens the picker, which saves.
            CommandAction::Model { id: Some(id) } => {
                *self.model.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
                let config = self.config();
                if let Err(e) = self.session.configure(Configure {
                    provider: Some(factory(&config, &self.make_provider, &self.load_models)),
                    ..Default::default()
                }) {
                    return self.emit(system(e.to_string(), true));
                }
                let file = self.file();
                let label =
                    provider_label(&provider_id(config.provider, file.as_ref().and_then(|f| f.custom_provider.as_deref())));
                let saved = resolve_config(file.as_ref(), &self.env).model;
                self.emit(UiEvent::StatusInfo(self.status_info()));
                self.emit(system(
                    format!("{label} · {id}, for this session. Your default stays {saved} (/model without a name picks and saves one)."),
                    false,
                ));
            }
            CommandAction::Thinking { on } => {
                let mandatory = matches!(
                    self.session.model_info().and_then(|i| i.reasoning),
                    Some(marv_provider::ReasoningSupport::Mandatory)
                );
                let notice = if on {
                    "Thinking on: models may reason before answering (slower, often better).".to_string()
                } else {
                    let except =
                        if mandatory { format!(" (except {}, which always reasons)", config.model) } else { String::new() };
                    format!("Thinking off: models answer directly{except}.")
                };
                self.save_config(self.with_file(|f| f.thinking = Some(on)), notice);
            }
            CommandAction::Sandbox { on } => {
                let notice = if on {
                    "Sandbox on: bash commands run in bubblewrap."
                } else {
                    "Sandbox off: bash commands run directly on your system (each still needs your approval)."
                };
                self.save_config(self.with_file(|f| f.sandbox = Some(on)), notice.into());
            }
            // MARV_YOLO=off (#74: a worker the Herdr door spawned from a session out of yolo) holds yolo off for this
            // process: /yolo on would claim otherwise, and write yolo into the user's config for every Marv.
            CommandAction::Yolo { on: true }
                if marv_session::config::yolo_held_off(self.env.get("MARV_YOLO").map(String::as_str)) =>
            {
                self.emit(system(YOLO_HELD_OFF, true));
            }
            CommandAction::Yolo { on } => {
                // What's in effect afterwards: `yolo: false` in the file also stops `approveSandboxed` defaulting to auto.
                let next = self.with_file(|f| f.yolo = Some(on));
                let after = resolve_config(Some(&next), &self.env);
                let notice = commands::yolo_status(&after, self.sandbox_available, self.session_yolo_allowed());
                self.save_config(next, notice);
            }
            CommandAction::Trajectories { on } => {
                let path = self.trajectories_dir.as_ref().map(|d| shorten_home(d, &self.home));
                let status = commands::trajectories_status(&Config { trajectories: on, ..config.clone() }, path.as_deref());
                self.save_config(self.with_file(|f| f.trajectories = Some(on)), format!("Trajectories {status}"));
            }
            CommandAction::Diagnostics { language, on } => {
                let next = DiagnosticsSetting::switched(self.file().and_then(|f| f.diagnostics), language, on);
                let status = commands::diagnostics_status(&Config { diagnostics: next.resolve(), ..config.clone() });
                self.save_config(self.with_file(|f| f.diagnostics = Some(next)), format!("Diagnostics {status}"));
            }
            CommandAction::ClearCheckCache => {
                // Deleting a big cache takes a while: off the UI thread, with a notice when it's done.
                self.emit(system("Deleting this project's check cache…", false));
                let me = self.clone();
                tokio::spawn(async move {
                    let session = me.session.clone();
                    let cleared = tokio::task::spawn_blocking(move || session.clear_check_cache())
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()));
                    me.emit(cleared_notice(cleared));
                });
            }
            CommandAction::Checks { on } => {
                let status = commands::checks_status(&Config { checks: on, ..config.clone() });
                self.save_config(self.with_file(|f| f.checks = Some(on)), format!("Checks {status}"));
            }
            CommandAction::Checkpoints { on } => {
                let status = commands::checkpoints_status(&Config { checkpoints: on, ..config.clone() });
                self.save_config(self.with_file(|f| f.checkpoints = Some(on)), format!("Checkpoints {status}"));
            }
            CommandAction::MeshWake { on } => {
                let status = commands::mesh_wake_status(&Config { mesh_wake: on, ..config.clone() });
                self.save_config(self.with_file(|f| f.mesh_wake = Some(on)), format!("Mesh wake {status}."));
                if let Some(radio) = self.radio() {
                    radio.set_wake(on);
                }
            }
            CommandAction::Radio { on } => self.turn_radio(on).await,
            CommandAction::Tx { to, text } => self.tx(to, &text),
            CommandAction::Ask { to, text } => self.ask(&to, &text),
            CommandAction::RadioView => match self.radio() {
                Some(radio) if radio.is_on() => self.emit(UiEvent::OpenRadio),
                Some(_) => self.emit(system(commands::RADIO_OFF, false)),
                None => self.emit(system(
                    "This Marv isn't on the mesh (no runtime folder, or its door didn't open), so it isn't on the radio.",
                    false,
                )),
            },
            CommandAction::Undo => self.undo(),
            CommandAction::Steps { every } => {
                let saved = self.with_file(|f| f.max_steps = Some(MaxStepsSetting::Every(every)));
                // MARV_MAX_STEPS still wins over the file: say what's saved, and what this session actually uses.
                let effective = resolve_config(Some(&saved), &self.env).max_steps;
                let notice = if effective == every {
                    commands::steps_status(&Config { max_steps: every, ..config.clone() })
                } else {
                    format!(
                        "Saved, but MARV_MAX_STEPS={} overrides it while it's set. Now: {}",
                        self.env.get("MARV_MAX_STEPS").map(String::as_str).unwrap_or_default(),
                        commands::steps_status(&Config { max_steps: effective, ..config.clone() })
                    )
                };
                self.save_config(saved, notice);
            }
            CommandAction::Feedback { score, note, labels } => self.rate(score, note, labels),
            CommandAction::Resume => {
                if self.activity.busy.load(Ordering::SeqCst) {
                    return self.emit(system(WORKING, true));
                }
                self.open_picker().await;
            }
            CommandAction::Memory => self.show_memory().await,
            CommandAction::Remember { scope, text } => self.remember(scope, &text).await,
            CommandAction::Forget { text } => self.forget(&text).await,
            CommandAction::Compact { focus } => self.compact(focus),
            CommandAction::Processes { stop } => {
                let me = self.clone();
                tokio::spawn(async move {
                    let stopped = me.session.stop_process(&stop).await;
                    me.emit(if stopped {
                        system(format!("Stopped {}.", printable(&stop)), false)
                    } else {
                        system(format!("No background process named \"{}\". /processes lists them.", printable(&stop)), true)
                    });
                });
            }
            CommandAction::Peers { read, stop } => {
                if !self.session.herdr_offered() {
                    return self.emit(system("No Herdr peers here: Marv isn't running in a Herdr pane.", true));
                }
                let me = self.clone();
                tokio::spawn(async move { me.peers(read, stop).await });
            }
            CommandAction::Skill { name, args } => {
                let Some(skill) = self.skills.iter().find(|s| s.skill.name == name) else { return };
                let for_model = marv_session::skills::skill_message(&skill.skill, &args);
                // As typed: the user's line, then the turn (the App starts it on its first event).
                self.emit(UiEvent::Message(message(Role::User, line)));
                self.run_turn(line.to_string(), Some(for_model));
            }
            CommandAction::McpTrust => self.trust_mcp().await,
            CommandAction::TrustProject => self.trust_project().await,
            CommandAction::McpAdd { args } => self.add_mcp(&args).await,
            CommandAction::McpSearch { words } => self.find_mcp(&words).await,
            CommandAction::McpRemove { name } => self.remove_mcp(&name).await,
            CommandAction::McpLogin { server } => self.login_mcp(&server).await,
            CommandAction::McpLogout { server } => self.logout_mcp(&server).await,
            CommandAction::Exit => self.emit(UiEvent::Exit),
        }
    }

    // ---- Mesh wake (marv-rs #78) ----
}

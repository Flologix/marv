//! MCP servers from the TUI: watching their state, trusting a project and its servers, adding, removing, logging in and out.

use super::*;

impl Bridge {
    /// Once the servers have started: one notice for those that failed, wait for trust or need a login; from then on
    /// a server whose login runs out mid-session (a refresh refused) is told once.
    pub fn watch_mcp(self: &Arc<Self>) {
        let Some(mcp) = self.mcp.clone() else { return };
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            mcp.ready().await;
            let Some(bridge) = me.upgrade() else { return };
            let status = mcp.status();
            {
                let mut told = bridge.told_needs_login.lock().unwrap_or_else(|e| e.into_inner());
                told.extend(status.iter().filter(|s| s.state == McpState::NeedsLogin).map(|s| s.name.clone()));
            }
            let watched = me.clone();
            let manager = Arc::downgrade(&mcp);
            mcp.set_on_change(move || {
                let (Some(bridge), Some(mcp)) = (watched.upgrade(), manager.upgrade()) else { return };
                for s in mcp.status() {
                    let mut told = bridge.told_needs_login.lock().unwrap_or_else(|e| e.into_inner());
                    if s.state == McpState::NeedsLogin && told.insert(s.name.clone()) {
                        drop(told);
                        bridge.emit(notice_message(Notice {
                            title: "MCP".into(),
                            tone: NoticeTone::Warning,
                            items: vec![NoticeItem {
                                text: format!("{} needs a new login: /mcp login {}", s.name, s.name),
                                detail: Some(vec!["Its tools fail until then.".into()]),
                            }],
                        }));
                    } else if s.state == McpState::Connected {
                        told.remove(&s.name);
                    }
                }
            });
            if let Some(notice) = mcp_notice(&status) {
                bridge.emit(notice_message(notice));
            }
        });
    }

    /// Busy work outside a turn: Esc stops it (`abort` cancels its token), `label` shows where "Thinking…" would.
    pub(super) async fn busy_with<T>(&self, label: String, work: impl AsyncFnOnce(CancellationToken) -> T) -> T {
        let cancel = CancellationToken::new();
        *self.busy_work.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());
        self.activity.set_busy(true);
        self.emit(UiEvent::Busy(Some(label)));
        let result = work(cancel).await;
        self.emit(UiEvent::Busy(None));
        self.activity.set_busy(false);
        *self.busy_work.lock().unwrap_or_else(|e| e.into_inner()) = None;
        result
    }

    /// /trust: the project's instruction files, skills and agent types as startup loaded them, and the subfolder
    /// instruction files this session quoted, trusted (for exactly those contents) from the next start: the system
    /// prompt and the tool list stay as they are in this session.
    pub(super) async fn trust_project(&self) {
        let quoted = self.session.nested_quoted();
        let result = marv_session::project_trust::trust_project(
            &self.config_dir,
            &self.root,
            &self.project_files,
            &self.project_trust,
            &quoted,
            "the next start of Marv here",
        )
        .await;
        match result {
            Ok(text) => self.emit(system(text, false)),
            Err(text) => self.emit(system(text, true)),
        }
    }

    /// /mcp trust: the project's servers the user hasn't trusted yet, trusted (for this exact config) and started.
    pub(super) async fn trust_mcp(&self) {
        let waiting = self.mcp.as_ref().map(|m| m.untrusted()).unwrap_or_default();
        let Some(mcp) = self.mcp.as_ref().filter(|_| !waiting.is_empty()) else {
            return self.emit(system("No MCP servers are waiting to be trusted (/mcp lists them).", false));
        };
        let names: Vec<&str> = waiting.iter().map(|s| s.name.as_str()).collect();
        self.emit(system(format!("Starting {}…", names.join(", ")), false));
        match mcp.trust_all().await {
            Ok(started) => {
                let lines: Vec<String> = started
                    .iter()
                    .map(|s| match s.state {
                        McpState::Connected => format!("{}: connected · {} tools", s.name, s.tools.len()),
                        _ => format!("{}: failed: {}", s.name, s.error.as_deref().unwrap_or_default()),
                    })
                    .collect();
                let failed = started.iter().any(|s| s.state != McpState::Connected);
                self.emit(system(lines.join("\n"), failed));
            }
            Err(e) => self.emit(system(format!("Couldn't trust the servers: {e}"), true)),
        }
    }

    /// /mcp add: look the source up (all fetching happens here, before the approval), ask once with the exact entry,
    /// write that entry to ~/.marv/mcp.json, and start it. Nothing fetched after the yes changes what runs: the file
    /// gets the JSON the approval showed, parsed back.
    pub(super) async fn add_mcp(&self, args: &str) {
        let options = match parse_add_args(args) {
            Ok(options) => options,
            Err(e) => return self.emit(system(e, true)),
        };
        let label = format!("Looking up {}…", printable(&options.source));
        let outcome = self
            .busy_with(label, async |cancel: CancellationToken| {
                let looked_up = self.look_up_and_ask(&options, &cancel).await;
                looked_up.unwrap_or_else(|e| {
                    // Esc: the lookup's error is the stop, not the source's fault.
                    AddOutcome::Failed(if cancel.is_cancelled() {
                        NOT_ADDED.into()
                    } else {
                        format!("Couldn't add the server: {e}")
                    })
                })
            })
            .await;
        let (name, entry) = match outcome {
            AddOutcome::Failed(text) => return self.emit(system(text, true)),
            AddOutcome::Choices(choices) => {
                let list: Vec<String> = choices.iter().map(|c| format!("- `{}`", printable(c))).collect();
                let text = format!("That README names several servers. Add one with:\n\n{}", list.join("\n"));
                return self.emit(UiEvent::Message(view::Message { markdown: true, ..message(Role::System, text) }));
            }
            AddOutcome::Declined(name) => return self.emit(system(format!("Not added: {name}."), false)),
            AddOutcome::Added(name, entry) => (name, entry),
        };
        // Loaded exactly as at startup, so ${VAR}s are filled in the same way.
        let mut env = self.env.clone();
        env.insert("MARV_PROJECT_DIR".into(), self.root.to_string_lossy().into_owned());
        let loaded = marv_mcp::config::parse_mcp_servers(&[(name.clone(), entry)], &env);
        let Some(server) = loaded.servers.into_iter().next() else {
            let prefix = format!("\"{name}\" in mcpServers: ");
            let why = loaded.problems.first().map_or("it couldn't be loaded", |p| p.strip_prefix(&prefix).unwrap_or(p));
            let text =
                format!("Added {name} to ~/.marv/mcp.json, but didn't start it: {why}. Set it in your shell and restart Marv.");
            return self.emit(system(text, true));
        };
        let Some(mcp) = &self.mcp else {
            return self.emit(system(format!("Added {name} to ~/.marv/mcp.json. Restart Marv to start it."), false));
        };
        self.emit(system(format!("Starting {name}…"), false));
        match mcp.add(server).await {
            Ok(status) => {
                let (text, is_error) = added_notice(&status);
                self.emit(system(text, is_error));
            }
            Err(e) => self.emit(system(
                format!("Added {name} to ~/.marv/mcp.json, but didn't start it now: {e} It starts when you restart Marv."),
                true,
            )),
        }
    }

    /// /mcp add's lookup, its checks and its question; on a yes, the entry written.
    pub(super) async fn look_up_and_ask(
        &self,
        options: &marv_mcp::install::AddOptions,
        cancel: &CancellationToken,
    ) -> Result<AddOutcome, String> {
        let deps = (self.mcp_deps)(cancel.clone());
        let proposal = match resolve_add(options, &*deps).await.map_err(|e| e.to_string())? {
            Resolved::Choices(c) => return Ok(AddOutcome::Choices(c.choices)),
            Resolved::Proposal(p) => p,
        };
        let name = proposal.name.clone();
        let status = self.mcp.as_ref().map(|m| m.status()).unwrap_or_default();
        // Removed in this conversation: its tools stay listed until /clear, so it can't come back yet (McpManager::add).
        if status.iter().any(|s| s.name == name && s.state == McpState::Removed) {
            return Err(format!(
                "\"{name}\" was removed in this conversation and its tools stay listed until the next one: /clear first, or pick another --name."
            ));
        }
        if personal_server_names(&self.config_dir).contains(&name) {
            return Err(format!(
                "You already have a server named \"{name}\": pick another with --name, or /mcp remove {name} first."
            ));
        }
        // A project server with that name is running (or waiting for trust) under it: McpManager::add can't start a
        // second one, and replacing it mid-conversation would swap the tools under the model. Another name is simpler.
        if status.iter().any(|s| s.name == name && s.source == marv_mcp::config::ServerSource::Project) {
            return Err(format!("This project has a server named \"{name}\": pick another with --name."));
        }
        if cancel.is_cancelled() {
            return Ok(AddOutcome::Failed(NOT_ADDED.into()));
        }
        let (approval, json) = approval_for(&proposal, &|v| self.env.contains_key(v))?;
        let request = marv_session::mcp_install::add_request(&approval);
        let asked = marv_tools::Approver::approve(&TuiApprover(self.activity.clone()), request).await;
        if asked != marv_tools::Decision::Yes {
            return Ok(AddOutcome::Declined(name));
        }
        let entry: Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        add_personal_server(&self.config_dir, &name, entry.clone()).await.map_err(|e| e.to_string())?;
        Ok(AddOutcome::Added(name, entry))
    }

    /// /mcp search: the MCP Registry's latest, active entries.
    pub(super) async fn find_mcp(&self, words: &str) {
        let found = self
            .busy_with("Searching the MCP Registry…".into(), async |cancel: CancellationToken| {
                let deps = (self.mcp_deps)(cancel);
                marv_mcp::install::registry::search_registry(words, &*deps).await
            })
            .await;
        match found {
            Ok(hits) => {
                let text = marv_mcp::install::registry::search_text(words, &hits);
                self.emit(UiEvent::Message(view::Message { markdown: true, ..message(Role::System, text) }));
            }
            Err(e) => self.emit(system(format!("Couldn't search the MCP Registry: {e}"), true)),
        }
    }

    /// /mcp remove: one of your servers out of ~/.marv/mcp.json, and stopped. No approval: the user typed it, and it
    /// takes capability away.
    pub(super) async fn remove_mcp(&self, name: &str) {
        let status = self.mcp.as_ref().map(|m| m.status()).unwrap_or_default();
        if !personal_server_names(&self.config_dir).iter().any(|n| n == name) {
            let project = status.iter().any(|s| s.name == name && s.source == marv_mcp::config::ServerSource::Project);
            let text = if project {
                format!("\"{name}\" is this project's server: remove it from .mcp.json.")
            } else {
                format!("You have no server named \"{name}\" in ~/.marv/mcp.json (/mcp lists them).")
            };
            return self.emit(system(text, true));
        }
        if let Err(e) = remove_personal_server(&self.config_dir, name).await {
            return self.emit(system(e.to_string(), true));
        }
        let running = status
            .iter()
            .any(|s| s.name == name && s.source == marv_mcp::config::ServerSource::Personal && s.state != McpState::Removed);
        if running && let Some(mcp) = &self.mcp {
            let _ = mcp.remove(name).await;
        }
        let stopped = if running { " and stopped it. Its tools stay listed (failing) until /clear or a restart." } else { "." };
        self.emit(system(format!("Removed {name} from ~/.marv/mcp.json{stopped}"), false));
    }

    /// /mcp login: not a turn (the prompt stays free, and a running turn keeps its tools). The browser opens where it
    /// can; the link is shown either way, with the paste fallback for a browser on another machine. Tokens are never
    /// shown.
    pub(super) async fn login_mcp(&self, name: &str) {
        let Some(mcp) = &self.mcp else { return };
        let (tx, open, server) = (self.activity.tx.clone(), self.open_url.clone(), name.to_string());
        let authorize: Authorize = Arc::new(move |url| {
            let text = format!(
                "Log in to {server} at {}: opening your browser… If it doesn't open, open this link and, if your browser then shows an error page, paste its address here:\n{url}",
                url_host(url.as_str())
            );
            let _ = tx.send(system(text, false));
            open(url);
        });
        let login = match mcp.login(name, authorize) {
            Ok(login) => login,
            Err(e) => return self.emit(system(e.to_string(), true)),
        };
        let id = self.next_login.fetch_add(1, Ordering::SeqCst);
        *self.pending_login.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(PendingLogin { id, server: name.to_string(), paste: login.paste });
        self.told_needs_login.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string());
        match login.done.await {
            Ok(Ok(status)) => {
                if status.state == McpState::Connected {
                    self.told_needs_login.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
                }
                let (text, is_error) = login_result(&status);
                self.emit(system(text, is_error));
            }
            Ok(Err(e)) => self.emit(system(format!("{name}: not logged in: {e}"), true)),
            Err(_) => self.emit(system(format!("{name}: not logged in: the login stopped"), true)),
        }
        let mut pending = self.pending_login.lock().unwrap_or_else(|e| e.into_inner());
        if pending.as_ref().is_some_and(|p| p.id == id) {
            *pending = None;
        }
    }

    /// A pasted callback address: shown as "(login callback)", handed to the waiting login.
    pub(super) fn paste_callback(&self, text: &str) {
        self.emit(UiEvent::Message(message(Role::User, "(login callback)")));
        let mut pending = self.pending_login.lock().unwrap_or_else(|e| e.into_inner());
        let Some(login) = pending.as_mut() else {
            drop(pending);
            let text = "No login is waiting for that address (/mcp login <server> starts one). It wasn't sent to the model.";
            return self.emit(system(text, true));
        };
        let check = (login.paste)(text);
        let server = login.server.clone();
        drop(pending);
        match check {
            CallbackCheck::Ok { .. } => self.emit(system(format!("Got it: finishing the login to {server}…"), false)),
            CallbackCheck::Refused { why } if why == "state" || why == "path" => self.emit(system(
                "That address isn't from this login: paste the address the browser ended up on after the latest /mcp login.",
                true,
            )),
            CallbackCheck::Refused { why } if why == "used" => self.emit(system("This login already has its callback.", true)),
            // An error= callback: the login's `done` fails with the reason, and login_mcp says so.
            CallbackCheck::Refused { .. } => {}
        }
    }

    /// /mcp logout: revoke where the server can, forget the login.
    pub(super) async fn logout_mcp(&self, name: &str) {
        let Some(mcp) = &self.mcp else { return };
        self.told_needs_login.lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string());
        match mcp.logout(name).await {
            Ok(revoked) => self.emit(system(logout_text(name, revoked), false)),
            Err(e) => self.emit(system(e.to_string(), true)),
        }
    }
}

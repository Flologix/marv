//! The mesh and the radio from the TUI: the mesh name, wake, radio messages and requests, and leaving.

use super::*;

impl Bridge {
    /// This Marv's name on the machine's mesh (its door opened), for `/mesh`.
    pub fn set_mesh_name(&self, name: &str) {
        *self.mesh_name.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_string());
    }

    /// Waits for messages landing in an inbox and, with `mesh.wake` on, lets one start a turn ([`Self::wake`]).
    /// Only the TUI does this: `marv -p` runs its one turn and never wakes.
    pub fn watch_mesh(self: &Arc<Self>) {
        let Some(mesh) = self.session.mesh() else { return };
        let (me, stop) = (Arc::downgrade(self), self.mesh_watch.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = mesh.mail() => {}
                    _ = stop.cancelled() => return,
                }
                let Some(bridge) = me.upgrade() else { return };
                bridge.wake();
            }
        });
    }

    /// A message came in. It starts a turn only when wake is on, nothing is running (a turn, `/compact`, a lookup)
    /// and no approval waits; otherwise it waits in the inbox for the next turn, as it always has. The woken turn
    /// reads it fenced and tainting (`MarvSession::wake`), with who woke it shown first. A wire over
    /// `WAKES_PER_MINUTE` is told once, and its messages wait too.
    pub(super) fn wake(self: &Arc<Self>) {
        if !self.config().mesh_wake {
            return;
        }
        let Some(mesh) = self.session.mesh() else { return };
        // Decided and claimed under the gate: a submission on its way has the gate first, and one after waits.
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if gate.closing
            || gate.turn
            || self.activity.busy.load(Ordering::SeqCst)
            || self.activity.waiting()
            || self.session.busy()
        {
            return;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let check = marv_session::mesh::wake_check(&mesh, &mut self.wake_limit.lock().unwrap_or_else(|e| e.into_inner()), now);
        for wire in &check.limited {
            self.emit(system(
                format!(
                    "{} woke Marv {} times a minute already: its messages on {} wait for your next turn.",
                    printable(wire.sender.as_str()),
                    marv_session::mesh::WAKES_PER_MINUTE,
                    printable(wire.receptacle.as_str())
                ),
                false,
            ));
        }
        if let Some(by) = check.wake {
            (gate.turn, gate.woken) = (true, true);
            self.activity.set_busy(true);
            drop(gate);
            self.stop_suggesting();
            match self.session.wake(&by) {
                Ok(Some(turn)) => {
                    self.emit(system(by.line(), false));
                    self.forward(turn);
                }
                Ok(None) | Err(_) => self.finished(Ended::Quiet),
            }
            return;
        }
        // #124: an addressed radio message (never a broadcast, never at hop 2 or more), within the radio's limits. The
        // turn it starts asks for everything that would ask (`MarvSession::wake_radio`).
        if mesh.unread_radio().is_empty() {
            return;
        }
        let check = marv_session::radio::radio_wake_check(
            &mesh,
            &mut self.radio_wake_limit.lock().unwrap_or_else(|e| e.into_inner()),
            now,
        );
        for sender in &check.limited {
            self.emit(system(
                format!(
                    "✻ Radio: {} woke Marv {} times a minute (or within {} s) already: its messages wait for your next turn.",
                    printable(sender),
                    marv_session::radio::RADIO_WAKES_PER_MINUTE,
                    marv_session::radio::RADIO_WAKE_GAP_MS / 1000
                ),
                false,
            ));
        }
        if check.paused {
            self.emit(system(
                format!(
                    "✻ Radio wakes paused: {} in {} minutes. Messages wait for your next turn.",
                    marv_session::radio::RADIO_WAKES_OVERALL,
                    marv_session::radio::RADIO_WAKES_WINDOW_MS / 60_000
                ),
                false,
            ));
        }
        let Some(by) = check.wake else { return };
        (gate.turn, gate.woken) = (true, true);
        self.activity.set_busy(true);
        drop(gate);
        self.stop_suggesting();
        match self.session.wake_radio(&by) {
            Ok(Some(turn)) => {
                self.emit(system(format!("✻ {}", by.line()), false));
                self.forward(turn);
            }
            Ok(None) | Err(_) => self.finished(Ended::Quiet),
        }
    }

    // ---- The mesh radio (marv-rs #124, phase 1: presence) ----

    pub(super) fn radio(&self) -> Option<Arc<Radio>> {
        self.radio.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// This Marv's radio, its door open: presence follows the session (state, title, wake), the strip follows the
    /// roster, and it joins now if the user's own config has it on. Only the TUI has one: `marv -p` is never on it.
    pub fn set_radio(self: &Arc<Self>, radio: Arc<Radio>) {
        let config = self.config();
        radio.set_titles(config.radio_titles);
        radio.set_wake(config.mesh_wake);
        *self.radio.lock().unwrap_or_else(|e| e.into_inner()) = Some(radio.clone());
        *self.activity.radio.lock().unwrap_or_else(|e| e.into_inner()) = Some(radio.clone());
        self.radio_title();
        self.activity.report();
        let (me, stop) = (Arc::downgrade(self), self.mesh_watch.clone());
        let mut changes = radio.subscribe();
        let watched = radio.clone();
        tokio::spawn(async move {
            // Reports already in the traffic list when the TUI started aren't new.
            let mut shown = std::collections::HashSet::new();
            for t in watched.traffic_list() {
                shown.insert(format!("{}>{}", t.id, t.reply_to.unwrap_or_default()));
            }
            loop {
                if let Some(bridge) = me.upgrade() {
                    bridge.emit(UiEvent::Radio(bridge.radio_view()));
                    bridge.show_reports(&watched, &mut shown);
                }
                tokio::select! {
                    _ = stop.cancelled() => return,
                    changed = changes.changed() => if changed.is_err() { return },
                }
            }
        });
        if config.mesh_radio {
            tokio::spawn(async move { radio.turn_on().await });
        }
    }

    /// The radio as the strip and `/radio` show it.
    pub(super) fn radio_view(&self) -> view::RadioView {
        let Some(radio) = self.radio().filter(|r| r.is_on()) else { return view::RadioView::default() };
        let peers = radio
            .roster()
            .into_iter()
            .map(|e| view::RadioPeer {
                name: e.name,
                state: match e.state {
                    RosterState::Idle => view::RadioState::Idle,
                    RosterState::Working => view::RadioState::Working,
                    RosterState::Waiting => view::RadioState::Waiting,
                    RosterState::Silent => view::RadioState::Silent,
                    RosterState::Left => view::RadioState::Left,
                    RosterState::Lost => view::RadioState::Lost,
                },
                title: e.title,
                project: e.project,
                pane: e.pane,
                since_ms: e.since_ms,
            })
            .collect();
        let traffic = radio
            .traffic_list()
            .into_iter()
            .map(|t| {
                let request = if t.kind == marv_mesh::radio::Kind::Request { Some(t.id.as_str()) } else { t.reply_to.as_deref() }
                    .map(marv_mesh::radio::short_id);
                view::RadioTraffic {
                    at_ms: t.at_ms,
                    id: t.id,
                    outgoing: t.outgoing,
                    peers: t.peers,
                    by: t.by.as_str().to_string(),
                    kind: t.kind.as_str().to_string(),
                    request,
                    text: t.text,
                    refused: t.refused,
                }
            })
            .collect();
        view::RadioView { on: true, me: radio.name.clone(), my_title: radio.presence().title, peers, traffic }
    }

    /// `/tx`: the person's message on the radio (`by: user`, hop 0). One line in Marv's voice in this transcript;
    /// nothing of it reaches this Marv's own model.
    pub(super) fn tx(&self, to: Vec<String>, text: &str) {
        let Some(radio) = self.radio().filter(|r| r.is_on()) else {
            return self.emit(system(commands::RADIO_OFF, false));
        };
        let addressed = if to.is_empty() { marv_mesh::radio::To::All } else { marv_mesh::radio::To::Names(to) };
        match radio.send_msg(&addressed, marv_mesh::radio::By::User, 0, text) {
            Err(err) => self.emit(system(format!("Radio: not sent: {}", printable(&err)), true)),
            Ok(report) => {
                if !report.sent.is_empty() {
                    let view = self.radio_view();
                    let short = |name: &str| marv_tui::ui::radio_view::short_name(&view, name);
                    let who = match &addressed {
                        marv_mesh::radio::To::All => "all".to_string(),
                        marv_mesh::radio::To::Names(_) => {
                            report.sent.iter().map(|n| format!("@{}", short(n))).collect::<Vec<_>>().join(" ")
                        }
                    };
                    self.emit(system(printable(&format!("✻ Radio → {who}: {text}")), false));
                }
                for (peer, why) in &report.refused {
                    self.emit(system(printable(&format!("Radio: not sent to {peer}: {why}.")), true));
                }
            }
        }
    }

    /// `/ask @name text`: the person's request on the radio (#124, phase 3; `by: user`, hop 0): one Marv, which
    /// acknowledges and reports back. One line in Marv's voice here; nothing of it reaches this Marv's own model (the
    /// ack and the report do, fenced, at its next turn).
    pub(super) fn ask(&self, to: &str, text: &str) {
        let Some(radio) = self.radio().filter(|r| r.is_on()) else {
            return self.emit(system(commands::RADIO_OFF, false));
        };
        match radio.send_request(to, marv_mesh::radio::By::User, 0, text) {
            Err(err) => self.emit(system(format!("Radio: not sent: {}", printable(&err)), true)),
            Ok(id) => {
                let view = self.radio_view();
                let peer = radio.resolve(to).unwrap_or_else(|_| to.to_string());
                let short = marv_tui::ui::radio_view::short_name(&view, &peer);
                let request = marv_mesh::radio::short_id(&id);
                self.emit(system(printable(&format!("✻ Radio → @{short} request {request}: {text}")), false));
            }
        }
    }

    /// A report on one of this Marv's requests came in (#124): a line in the transcript for the person, in Marv's
    /// voice with the report's text (printable). The model reads it fenced at its next turn, as any radio traffic.
    pub(super) fn show_reports(&self, radio: &Radio, shown: &mut std::collections::HashSet<String>) {
        for t in radio.traffic_list() {
            let report = matches!(t.kind, marv_mesh::radio::Kind::Done | marv_mesh::radio::Kind::Failed);
            if t.outgoing
                || !report
                || t.refused.is_some()
                || !shown.insert(format!("{}>{}", t.id, t.reply_to.clone().unwrap_or_default()))
            {
                continue;
            }
            let from = t.peers.first().cloned().unwrap_or_default();
            let short = t.reply_to.as_deref().map(|r| format!(" {}", marv_mesh::radio::short_id(r))).unwrap_or_default();
            self.emit(system(printable(&format!("✻ Radio ← {from} {}{short}: {}", t.kind.as_str(), t.text)), false));
        }
    }

    /// Presence's title: the conversation's first user message (empty for a new one), never anything else.
    pub(super) fn radio_title(&self) {
        if let Some(radio) = self.radio() {
            radio.set_title(&self.session.first_message());
        }
    }

    /// `/radio on|off`: saved in the user's own config, and joined or left at once.
    pub(super) async fn turn_radio(self: &Arc<Self>, on: bool) {
        let titles = self.file().and_then(|f| f.mesh_radio).is_none_or(|r| r.titles);
        let notice = match (on, self.radio()) {
            (true, Some(_)) => format!(
                "Radio on: your other Marvs here with the radio on show in the strip above the prompt (/radio lists \
                 them and what was said). They see this Marv's state and conversation title; /tx sends them a \
                 message, which their models read as data.{}",
                if self.session.radio_offered() {
                    ""
                } else {
                    " This Marv's model gets radio_tx from the next conversation (/clear): its tools stay as they are \
                     for this one."
                }
            ),
            (true, None) => "Radio on, saved. This Marv isn't on the mesh (no runtime folder, or its door didn't open), \
                 so it joins from the next start."
                .to_string(),
            (false, _) => "Radio off: this Marv left it.".to_string(),
        };
        self.save_config(self.with_file(|f| f.mesh_radio = Some(RadioSetting { on, titles })), notice);
        let Some(radio) = self.radio() else { return };
        if on {
            radio.set_titles(titles);
            radio.turn_on().await;
        } else {
            radio.turn_off().await;
        }
        self.emit(UiEvent::Radio(self.radio_view()));
    }

    /// Leaves the radio on the way out: a `left` to every link (bounded), so the others don't wait for a timeout.
    pub async fn leave_radio(&self) {
        if let Some(radio) = self.radio().filter(|r| r.is_on()) {
            radio.turn_off().await;
        }
    }

    /// The App is leaving (#78): no wake-up starts from now on, queued input is dropped, and the watcher stops. Before
    /// the App's flush and the shutdown's interrupt, so nothing starts a turn after the terminal has closed.
    pub fn close_mesh(&self) {
        let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        gate.closing = true;
        gate.queued.clear();
        self.mesh_watch.cancel();
    }

    // ---- MCP (Marv #6, #18, #46) ----
}

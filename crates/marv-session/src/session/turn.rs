//! A turn: sending a message (or a woken one), running the agent loop over it, and the checks and checkpoint around it.

use super::*;

impl MarvSession {
    /// Runs one turn: the user's message and everything the agent does until it stops. The turn starts at once on its
    /// own task (it must be inside a Tokio runtime); read the [`Turn`] for its events. Fails during a turn, or after
    /// `close()`.
    pub fn send(&self, text: &str, options: SendOptions) -> Result<Turn, SessionError> {
        self.start(text, options, Woken::No)
    }

    /// "Send now": the person's `text` for the running turn, delivered at its next step boundary (after the current
    /// reply and its tool calls, before the next request) as an ordinary user message, instead of after the turn.
    /// Nothing running is stopped for it. Fails when no turn runs (or it has just ended): send it as a turn then.
    /// What the turn never delivers comes back in [`SessionEvent::NotSent`]; one delivered is a
    /// [`LoopEvent::SentNow`] and joins the transcript there. `marv -p` never calls this.
    pub fn send_now(&self, text: &str) -> Result<(), SessionError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(SessionError::Invalid("Nothing to send.".into()));
        }
        let s = self.state();
        if s.closed {
            return Err(SessionError::Closed);
        }
        let Some(queue) = s.send_now.as_ref() else { return Err(SessionError::Invalid(NOT_RUNNING.into())) };
        queue.push(text.to_string()).map_err(|_| SessionError::Invalid(NOT_RUNNING.into()))?;
        // As `send`: sites the person pasted are theirs to fetch.
        for key in marv_tools::web_fetch::pasted_scopes(text) {
            self.inner.always.add(key);
        }
        Ok(())
    }

    /// marv-rs #78 (`mesh.wake`, the TUI's opt-in): a turn started by a message on a wired inbox, not by the user.
    /// `None` when the main agent's inbox has nothing unread (no turn). Refused like `send` during a turn.
    ///
    /// There's no user message: the inbox is read the way every turn reads it (fenced as data from another agent,
    /// tainting the session before the first request), followed by [`crate::mesh::WAKE_NOTE`] in Marv's voice. The
    /// transcript gets `by`'s line as Marv's, never as something the user said. Whether to wake at all (on, idle, no
    /// approval waiting, the rate limit: [`crate::mesh::wake_check`]) is the caller's; `marv -p` never calls this.
    pub fn wake(&self, by: &crate::mesh::WakeBy) -> Result<Option<Turn>, SessionError> {
        self.idle(&self.state())?;
        let Some(mesh) = &self.inner.init.mesh else { return Ok(None) };
        if mesh.unread(&crate::mesh::parent_id()).is_empty() {
            return Ok(None);
        }
        let options = SendOptions { for_model: Some(crate::mesh::WAKE_NOTE.to_string()), cancel: None };
        self.start(&by.line(), options, Woken::Inbox).map(Some)
    }

    /// marv-rs #124 (`mesh.wake` with the radio on, the TUI's): a turn started by an addressed radio message, not by
    /// the user. `None` when nothing waits on the radio (no turn). Refused like `send` during a turn.
    ///
    /// Like [`Self::wake`]: no user message, the radio's messages read fenced and tainting before the first request,
    /// then [`crate::radio::RADIO_WAKE_NOTE`], and `by`'s line in the transcript as Marv's. And **nothing is approved
    /// automatically in it** (§8.4): yolo, remembered "always" answers, approved URL prefixes and a worktree
    /// subagent's automatic yes don't apply; every call that would ask asks the person, once. The turn runs one hop
    /// past the message that woke it, and its `radio_tx` messages carry that hop. Whether to wake at all (on, idle, no
    /// approval waiting, the limits: [`crate::radio::radio_wake_check`]) is the caller's; `marv -p` never calls this.
    pub fn wake_radio(&self, by: &crate::radio::RadioWakeBy) -> Result<Option<Turn>, SessionError> {
        self.idle(&self.state())?;
        let Some(mesh) = &self.inner.init.mesh else { return Ok(None) };
        if mesh.unread_radio().is_empty() {
            return Ok(None);
        }
        let options = SendOptions { for_model: Some(by.note().to_string()), cancel: None };
        self.start(&by.line(), options, Woken::Radio(by.clone())).map(Some)
    }

    /// `send`, or a wake-up (`woken`: `text` is Marv's line for the transcript, not the user's).
    pub(super) fn start(&self, text: &str, options: SendOptions, woken: Woken) -> Result<Turn, SessionError> {
        let stop = CancellationToken::new();
        let done = CancellationToken::new();
        {
            let mut s = self.state();
            self.idle(&s)?;
            // Sites the user pasted are theirs to fetch: only what they typed counts (not a /skill's instructions,
            // a file or a page: links found there are where a planted instruction would send the model). A woken
            // turn's line isn't the user's.
            if !woken.is_woken() {
                for key in marv_tools::web_fetch::pasted_scopes(text) {
                    self.inner.always.add(key);
                }
            }
            s.running = true;
            s.current = Some(stop.clone());
            s.working = Some(done.clone());
            s.send_now = Some(marv_agent::agent::SendNow::new());
        }
        if let Some(outer) = options.cancel.clone() {
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = outer.cancelled() => stop.cancel(),
                    _ = stop.cancelled() => {}
                }
            });
        }
        let queue = Arc::new(EventQueue::new());
        let events = queue.reader().map_err(|e| SessionError::Invalid(e.into()))?;
        let session = self.clone();
        let (text, for_model, turn_stop) =
            (text.to_string(), options.for_model.unwrap_or_else(|| text.to_string()), stop.clone());
        // Started here, not when the client first reads: the turn's end is what frees the session, so it must not
        // depend on the client reading. Events wait in the queue until it does.
        let work = tokio::spawn(async move {
            let push = queue.clone();
            session.run_turn(text, for_model, woken, turn_stop, move |e| push.push(e)).await;
            queue.close();
            done.cancel();
        });
        Ok(Turn { events, stop, work: Some(work) })
    }

    /// One turn's work (Marv's `runTurn`). Never fails: whatever happens ends in `TurnEnd`.
    ///
    /// Order: `TurnStart` → wait for `memory reload` and MCP `ready()` (`WaitingForMcp`; Esc → `Interrupted`) →
    /// stopped before sending? `Interrupted`, nothing logged → read the turn's settings → decide the MCP mode (#14) →
    /// trajectory `session`/`turn_start` (+ implicit feedback on the previous turn) → compact first if nearly full →
    /// MCP announcement note at the start of the user message → `Running` → the checks' `TurnChecks` → the
    /// [`crate::runner::SessionAgentHost`] → `marv_agent::run_agent` with the [`crate::runner::SessionToolRunner`],
    /// `rewrite_call` (deferred MCP), `on_full` (mid-turn compaction), `on_limit` (the step-limit approval),
    /// `after_step` (checks), `wrap_up_at_limit` → forward `Loop` events (usage → totals; assistant → transcript;
    /// agent `ToolStart` → `subagent_start` record) → drain mesh inbox between steps → finally: reset
    /// the compaction failures, finish recorders, cancel the stop, free the session, schedule the save, `TurnEnd`.
    /// marv-rs: the parent's mesh inbox is read before the user message (its messages go in first, fenced) and
    /// after each step (`crate::mesh::with_inbox`).
    pub(super) async fn run_turn(
        &self,
        text: String,
        for_model: String,
        woken: Woken,
        stop: CancellationToken,
        push: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) {
        let turn = turn_id();
        // Nothing after TurnEnd: a subagent's callback arriving late (after the turn ended) is dropped.
        let ended = Arc::new(AtomicBool::new(false));
        let emit: Emit = {
            let ended = ended.clone();
            Arc::new(move |event| {
                if !ended.load(Ordering::SeqCst) {
                    push(event);
                }
            })
        };
        emit(SessionEvent::TurnStart { turn: turn.clone() });
        let main: Recorder = Arc::default();
        // Subagents' recorders, by call id; ids are unique within the turn.
        let subs: Recorders = Arc::default();
        let pair: Pair = Arc::default();
        let work = AssertUnwindSafe(self.turn_body(&turn, &text, &for_model, &woken, &stop, &emit, &main, &subs, &pair));
        let reason = match work.catch_unwind().await {
            Ok(reason) => reason,
            Err(panic) => {
                // A provider (or a tool's own code) that panics instead of reporting an error: the turn ends, the
                // session lives on.
                emit(SessionEvent::Loop(LoopEvent::Error(format!("Error: {}", panic_message(panic.as_ref())))));
                TurnEndReason::Done(StopReason::Error)
            }
        };
        // #124: the turn handled radio requests: each gets its report now, built without a model call (§7.3).
        self.radio_reports(reason, &emit);
        {
            // The next turn tries compaction again.
            let mut s = self.state();
            s.compact_failures = 0;
            s.compact_failed_at = None;
        }
        for (_, mut sub) in lock(&subs).drain() {
            sub.finish("interrupted");
        }
        if let Some(main) = lock(&main).as_mut() {
            main.finish("error"); // only if the loop panicked: otherwise its Done already ended the turn
        }
        // Stop anything still running (a no-op after a normal end): subagents still running in a parallel group would
        // otherwise carry on unseen, and could still ask for approvals.
        stop.cancel();
        // The after snapshot, however the turn ended, once every call is answered (a pair that changed nothing goes).
        let pair = lock(&pair).take();
        if let Some(pair) = pair {
            pair.finish().await;
        }
        // The session is free before TurnEnd goes out, so a client can send the next message as soon as it sees it,
        // and a turn nobody reads frees it too. (A newer turn can't have started yet: send() refuses while running.)
        // "Send now" closes with the turn (under the same lock `send_now()` takes, so nothing slips in after): what it
        // never delivered goes back to the client.
        let not_sent = {
            let mut s = self.state();
            s.current = None;
            s.running = false;
            s.turn_open = None;
            s.send_now.take().map(|q| q.close()).unwrap_or_default()
        };
        // At once (#212 review): a quit just after the turn ended mustn't resume it as cut off, without its reply.
        self.save_at_once();
        if !not_sent.is_empty() {
            emit(SessionEvent::NotSent { texts: not_sent });
        }
        emit(SessionEvent::TurnEnd { turn, reason });
        ended.store(true, Ordering::SeqCst);
    }

    /// The automatic reports (#124, §7.3) on the radio requests the turn that just ended handled: `done` or `failed`
    /// to each requester, from how it ended and what it said ([`crate::radio::report_of`]), with no model call. A
    /// request the model answered itself (`radio_tx kind: reply`) gets none. Each sent one is a line in the transcript.
    pub(super) fn radio_reports(&self, reason: TurnEndReason, emit: &Emit) {
        let (open, facts, changed) = {
            let mut s = self.state();
            if !std::mem::take(&mut s.radio_turn.handles) {
                return;
            }
            let changed = s.reported.get(&s.radio_turn.number).map_or(0, Vec::len);
            (std::mem::take(&mut s.radio_requests), s.radio_turn.clone(), changed)
        };
        let Some(radio) = self.inner.init.mesh.as_ref().and_then(|m| m.radio()) else { return };
        let (kind, text) = crate::radio::report_for(
            reason,
            &radio.name,
            facts.reply.as_deref(),
            facts.declined.as_deref(),
            changed,
            facts.sent_now,
        );
        let hop = self.inner.radio_hop.get();
        let now = crate::radio::now_ms();
        for open in open {
            let short = marv_mesh::radio::short_id(&open.id);
            let line = match radio.report(&open, kind, hop, &text, now.saturating_sub(open.at_ms)) {
                marv_mesh::radio::ReportOutcome::Sent => format!("Radio → {} {} {short}", open.from, kind.as_str()),
                marv_mesh::radio::ReportOutcome::AlreadyAnswered => continue,
                marv_mesh::radio::ReportOutcome::NotSent(why) => {
                    format!("Radio: the {} report on {short} to {} wasn't sent: {why}", kind.as_str(), open.from)
                }
            };
            let line = marv_core::printable(&line);
            self.note(Role::System, &line);
            emit(SessionEvent::RadioNote { text: line });
        }
    }

    /// The radio requests this session's turns read and haven't reported on yet (#124, phase 3).
    pub fn radio_requests(&self) -> Vec<marv_mesh::radio::OpenRequest> {
        self.state().radio_requests.clone()
    }

    /// `run_turn` up to the end of the loop: how the turn ended.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn turn_body(
        &self,
        turn: &str,
        text: &str,
        for_model: &str,
        woken: &Woken,
        stop: &CancellationToken,
        emit: &Emit,
        main: &Recorder,
        subs: &Recorders,
        pair: &Pair,
    ) -> TurnEndReason {
        let init = &self.inner.init;
        // MCP servers still starting: their tools must be in place before the first request (the list can't change
        // after it, or the prompt cache would start over). Interrupting the wait ends the turn. A failed start still
        // settles: the turn goes on with whatever tools there are.
        //
        // marv-rs: the first turn with MCP servers always goes through the wait (and says so), settled or not. In
        // Marv the first turn's check always came before startup could finish (connecting takes I/O, the check
        // doesn't); here startup runs on another thread, and a server that connects in a few milliseconds raced the
        // check, so `waiting_for_mcp` came and went from run to run. A wait on settled servers returns at once.
        if let Some(mcp) = &init.mcp {
            let first = !self.state().mcp_waited && !mcp.status().is_empty();
            if first || !mcp.settled() {
                emit(SessionEvent::Status(TurnStatus::WaitingForMcp));
                tokio::select! {
                    _ = mcp.ready() => {}
                    _ = stop.cancelled() => return TurnEndReason::Interrupted,
                }
            }
            self.state().mcp_waited = true;
        }
        // Session-start hooks (#108): started now if the client didn't, and waited for (each has its timeout), so the
        // first message carries their output. Interrupting the wait ends the turn; the output waits for the next one.
        self.run_start_hooks();
        let hooks_done = self.state().hooks_done.clone();
        if let Some(done) = hooks_done {
            tokio::select! {
                _ = done.cancelled() => {}
                _ = stop.cancelled() => return TurnEndReason::Interrupted,
            }
        }
        // The model list (#143): what it says changes the request (the reasoning effort, the output limit #96 and the
        // window #116), and the provider this turn takes is fixed for the whole turn. A `marv -p` run is one turn, so
        // a list that answered a moment after it started (OpenRouter's is ~0.8 MB) left the whole run without them.
        // Waited for briefly, only while it hasn't answered.
        let lookup = self.state().lookup_done.clone();
        if let Some(mut lookup) = lookup.filter(|l| !*l.borrow()) {
            tokio::select! {
                _ = tokio::time::timeout(LOOKUP_WAIT, lookup.wait_for(|done| *done)) => {}
                _ = stop.cancelled() => return TurnEndReason::Interrupted,
            }
        }
        // Stopped before the message was even sent (an already-cancelled token, or interrupt() during the wait): it
        // isn't sent, so the conversation stays as it was, and nothing is logged (a turn the model never saw would
        // otherwise be in the trajectory, and the next message's tone would rate it).
        if stop.is_cancelled() {
            return TurnEndReason::Interrupted;
        }
        // What this turn runs with, all read now: settings changed while it runs (configure(), or the model list
        // arriving and remaking the provider) apply from the next turn. That includes the factory, so a subagent
        // naming its own model gets it from the same provider as the turn, and its usage is counted (local or not) as
        // that one's, priced with what the model list said about the turn's model.
        // The servers have settled: the first turn with MCP tools decides whether this conversation defers them (#14).
        let context = self.context_length();
        let mcp_choice = self.decide_mcp(context);
        let turn_specs = init.mcp.as_ref().map(|m| m.specs()).unwrap_or_default();
        self.state().turn_mcp_specs = turn_specs.clone();
        let (specs, tools) = self.offered();
        let (
            sandbox,
            yolo,
            approve_sandboxed,
            factory,
            provider,
            info,
            diagnostics,
            checks_on,
            formatter,
            max_steps,
            history,
            system,
        ) = {
            let s = self.state();
            (
                s.sandbox,
                s.yolo,
                s.approve_sandboxed,
                s.factory.clone(),
                s.provider.clone(),
                s.info.clone(),
                s.diagnostics,
                s.checks,
                s.formatter.clone(),
                s.max_steps,
                s.conversation.clone(),
                s.system.clone(),
            )
        };
        let log = self.trajectory();
        let mut tainted = self.inner.taint.sources().len();
        if let Some(mesh) = &init.mesh {
            crate::mesh::log_to(mesh, log.as_ref());
        }
        let record: Record = {
            let log = log.clone();
            Arc::new(move |r: TrajectoryRecord| {
                if let Some(log) = &log {
                    log.write(&r);
                }
            })
        };
        self.start_turn_log(log.as_ref(), turn, text, for_model, &specs, mcp_choice, woken).await;
        *lock(main) = Some(recorder(&record, turn, "main"));

        // Nearly out of context: summarize first, so this message (and what follows) fits.
        if self.try_compact() {
            let result = self.compact_if_full(&provider, factory.as_ref(), info.as_ref(), stop, false, emit).await;
            if matches!(result, CompactResult::NotCompacted { reason: CompactFailure::Stopped, .. }) {
                // Stopping meant "stop": the message too (it would run on the nearly full context).
                if let Some(main) = lock(main).as_mut() {
                    main.finish("aborted");
                }
                return TurnEndReason::Interrupted;
            }
        }
        // After a compaction above (which forgets what was announced), so the note then lists everything again. It
        // goes at the start of the user's message, never as a message of its own (two user messages in a row), and
        // never into the transcript.
        let mcp_note = self.announce(&turn_specs);
        if let Some(note) = &mcp_note {
            record(TrajectoryRecord::McpNote { turn: turn.into(), text: note.clone() });
        }
        let message = match &mcp_note {
            Some(note) => format!("{note}\n\n{for_model}"),
            None => for_model.to_string(),
        };
        // #124: the radio's messages are read only in the person's own turn (hop 0: a person typing resets the chain)
        // and in a turn the radio started (automatic approval off; its hop rises past the deepest message it reads).
        // A turn a wired inbox woke leaves them unread: it may auto-approve (review P2-1).
        self.inner.radio_hop.set(match woken {
            Woken::Radio(by) => by.turn_hop(),
            _ => 0,
        });
        let radio_read = match woken {
            Woken::No => crate::mesh::RadioRead::Read,
            Woken::Radio(_) => crate::mesh::RadioRead::RaiseHop(self.inner.radio_hop.clone()),
            Woken::Inbox => crate::mesh::RadioRead::Skip,
        };
        // Mesh messages that came in since the last step: before the user's message, fenced as data. Radio requests
        // are read only here, at a turn's start (#124, §7.2): this turn reports on them when it ends.
        let inbox = init
            .mesh
            .as_ref()
            .and_then(|m| crate::mesh::inbox_read(m, &crate::mesh::parent_id(), &self.inner.taint, &radio_read, true));
        let handles_requests = !matches!(radio_read, crate::mesh::RadioRead::Skip) && {
            let read: Vec<_> = inbox.as_ref().map(|i| i.requests.clone()).unwrap_or_default();
            let mut s = self.state();
            for request in read {
                if !s.radio_requests.iter().any(|r| r.id == request.id) {
                    s.radio_requests.push(marv_mesh::radio::OpenRequest {
                        id: request.id,
                        from: request.from,
                        hop: request.hop,
                        at_ms: crate::radio::now_ms(),
                    });
                }
            }
            // A resumed session's: open again on the radio, so the model may answer them and they get their report.
            if let Some(radio) = init.mesh.as_ref().and_then(|m| m.radio()) {
                for open in &s.radio_requests {
                    radio.reopen(open);
                }
            }
            !s.radio_requests.is_empty()
        };
        if handles_requests {
            // Saved now, so a Marv that dies mid-turn still owes (and, resumed, sends) their reports.
            self.schedule_save();
        }
        let message = match inbox {
            Some(inbox) => format!("{}\n\n{message}", inbox.note),
            None => message,
        };
        // An undo since the last turn: the model hears of it here, in the message Marv sends anyway (the cache holds).
        let undo = self.state().pending_note.take();
        let message = match undo {
            Some(note) => format!("{note}\n\n{message}"),
            None => message,
        };
        // The session-start hooks' output (#108), once per conversation: it leads its first message, never the system
        // prompt, which stays byte-identical from session to session (and turn to turn), so the cached prefix holds.
        let hook_runs = {
            let mut s = self.state();
            if std::mem::take(&mut s.hooks_unsent) { s.hook_runs.clone() } else { None }
        };
        let message = match hook_runs.as_deref().and_then(crate::hooks::hooks_note) {
            Some(note) => format!("{note}\n\n{message}"),
            None => message,
        };
        for run in hook_runs.iter().flatten() {
            record(TrajectoryRecord::Hook {
                turn: turn.into(),
                command: run.command.clone(),
                exit_code: run.exit_code,
                timed_out: run.timed_out,
                bytes: run.bytes,
                ms: run.ms,
            });
        }
        // Spawned tainted (MARV_TAINTED=1, #74): the task this first turn carries came from a tainted conversation.
        if init.tainted_at_start && !self.inner.start_taint_marked.swap(true, Ordering::SeqCst) {
            self.inner.taint.mark(Provenance::Herdr);
        }
        // Their output taints the session as it's sent (unless every hook that printed something is `trusted`): the user
        // chose the commands, not what they print (`bd ready` lists issue titles anyone can file).
        if hook_runs.as_deref().is_some_and(crate::hooks::taints) {
            self.inner.taint.mark(Provenance::Hook);
        }
        // Either is shown and logged now, before the request, like any taint.
        let sources = self.inner.taint.sources();
        for source in sources.iter().skip(tainted) {
            record(TrajectoryRecord::Taint { turn: Some(turn.into()), source: *source });
            emit(SessionEvent::Tainted { source: *source });
        }
        tainted = sources.len();
        // After an overflow's drop note (its level 3) the history can end with a user turn: never two in a row.
        marv_agent::compact::push_user(&mut lock(&history), &message);
        self.state().person_message = Some(message.clone());
        // A woken turn (#78): who woke it is Marv's line in the transcript; the user said nothing.
        self.note(if woken.is_woken() { Role::System } else { Role::User }, text);
        // #202: saved as it goes from here, so a Marv that quits mid-turn keeps the message and the calls.
        self.state().turn_open = Some(OpenTurn::default());
        self.save_soon();
        emit(SessionEvent::Status(TurnStatus::Running));

        // The turn's checkpoint pair (#16): the before is taken by the first call that can change files.
        let (number, checkpoints_on, prune) = {
            let mut s = self.state();
            s.turn_number += 1;
            let number = s.turn_number;
            s.radio_turn = RadioTurnFacts { number, handles: handles_requests, ..RadioTurnFacts::default() };
            s.reported.insert(number, Vec::new());
            let prune = s.checkpoints && !s.pruned;
            s.pruned |= prune;
            (number, s.checkpoints, prune)
        };
        if prune {
            let store = self.inner.checkpoints.clone();
            self.state().prune_task = Some(tokio::spawn(async move { store.prune(20, 14).await }));
        }
        let checkpoint = checkpoints_on.then(|| Arc::new(self.checkpoint_turn(number, text, turn, emit, &record)));
        *lock(pair) = checkpoint.clone();

        // #124: a turn the radio started runs with automatic approval off (§8.4): its approvals are asked once each,
        // say why, and are never answered from the session's remembered "always" (nor are its subagents'); yolo and an
        // approved URL prefix don't apply (`ctx.mode` below: `CallMode::Radio`). Its hop was set before its first read, above.
        // #74: so does any turn the person didn't start, a mesh wake (#78) too: another agent's message is never a
        // reason to approve anything automatically.
        // #124, phase 3: so does a turn that handles a radio request, the person's own included (a deliberate step past
        // spec §8.4, which left the person's turn its normal approvals: the request note invites the model to act on
        // another Marv's words, so yolo mustn't carry them out unasked).
        let radio_turn = matches!(woken, Woken::Radio(_));
        let unattended = woken.is_woken() || handles_requests;
        let why = if radio_turn { crate::radio::RADIO_TURN_WARNING } else { crate::radio::RADIO_REQUEST_WARNING };
        let client: Option<Arc<dyn Approver>> = init.approve.clone().map(|inner| {
            let inner = if radio_turn || handles_requests {
                Arc::new(crate::radio::RadioTurnApprover(inner, why)) as Arc<dyn Approver>
            } else {
                inner
            };
            // #202: the turn is saved before each question a person is asked (the main agent's and subagents').
            Arc::new(SavingApprover { session: self.clone(), inner }) as Arc<dyn Approver>
        });
        let always = if unattended { AlwaysScopes::default() } else { self.inner.always.clone() };
        let approve: Option<Arc<dyn Approver>> = client
            .clone()
            .map(|inner| Arc::new(SessionApprover { inner, always: always.clone(), cancel: stop.clone() }) as Arc<dyn Approver>);
        let checks = self.turn_checks(turn, sandbox, diagnostics, checks_on, stop, emit, main, &record);
        let sink = Arc::new(TurnSink {
            session: self.clone(),
            factory: factory.clone(),
            info: info.clone(),
            emit: emit.clone(),
            subs: subs.clone(),
            turn_number: number,
        });
        // What the agent tool needs to start subagents. Only the main agent gets one; what a subagent does reaches
        // this conversation only as its tool result, and the client through Subagent events.
        let host = Arc::new(SessionAgentHost {
            agents: Arc::new(init.agents.clone()),
            cwd: self.cwd(),
            instructions: init.instructions.clone(),
            nested_instructions: init.nested_instructions,
            nested_trust: init.nested_trust.clone(),
            worktrees_dir: init.worktrees_dir.clone(),
            web: init.web,
            factory: factory.clone(),
            provider: provider.clone(),
            approver: client.clone(),
            always: always.clone(),
            sink: sink.clone(),
            mesh: init.mesh.clone().map(|mesh| MeshAccess { mesh, approver: self.wire_approver() }),
        });
        let mut ctx = ToolContext::new(&init.root);
        let mut sandbox = marv_sandbox::Sandbox::from_env(sandbox);
        // The config's extra writable folders (#98) ride in every command's sandbox (an approved command's own
        // writable mounts, e.g. a worktree's .git, still go on top).
        sandbox.writable = init.writable.clone();
        // The conversation's own /tmp, kept between its commands and gone with the conversation.
        sandbox.scratch = self.scratch_dir();
        ctx.sandbox = Arc::new(sandbox);
        ctx.home = ctx.sandbox.home.clone();
        ctx.cancel = stop.clone();
        ctx.approver = approve.clone();
        ctx.yolo = yolo && !unattended;
        // Also where yolo isn't allowed (the home folder or above it): the sandbox shows the project read-only there, so
        // a command without network can write only to /tmp, and file changes still ask (`registry::home_file_write`).
        ctx.approve_sandboxed = approve_sandboxed && !init.one_shot;
        // Who started the turn, as one value (`marv_tools::floor`): a radio turn or one handling radio requests, a
        // wake, `marv -p`, or the person.
        ctx.mode = if radio_turn || handles_requests {
            marv_tools::CallMode::Radio
        } else if woken.is_woken() {
            marv_tools::CallMode::Woken
        } else if init.one_shot {
            marv_tools::CallMode::Headless
        } else {
            marv_tools::CallMode::Person
        };
        debug_assert_eq!(ctx.mode.auto_approval(), !unattended);
        ctx.skills = self.inner.skills.clone();
        ctx.memory = init.memory.as_ref().map(|(paths, _)| paths.for_tools());
        // The session's id now (a resume changes it): its cut outputs are saved, and readable, there (#61).
        ctx.tool_output = init.tool_output_dir.as_ref().map(|dir| dir.join(self.id()));
        // Its saved pages, which `recall` reads: this conversation's own, the main agent's only (a subagent's context
        // is built without them, so it can't read its parent's).
        ctx.swap = self.swap_dir();
        self.clean_tool_output_once();
        ctx.agent_host = Some(host);
        // A running call's status line (#201: a mesh send waiting for another Marv's person) goes out as its progress.
        ctx.progress = Some(Arc::new(CallProgress(sink)));
        // Where the plan tool writes; only the main agent gets it (as it does agent_host), so subagents have no plan.
        if init.plan {
            ctx.plan = Some(Arc::new(SessionPlan(self.clone())));
        }
        ctx.before_change = checks.as_ref().map(|c| c.before_change.clone());
        // The project's formatter (#67): the edit tools run it on each file they write, inside the sandbox (it's the
        // project's own code). Off by config, and nothing runs where the sandbox is off either.
        ctx.formatter = marv_tools::formatter::ProjectFormatter::hook(&formatter, ctx.sandbox.enabled);
        ctx.checkpoint = checkpoint.map(|c| c as Arc<dyn marv_tools::Checkpoint>);
        ctx.taint = self.inner.taint.clone();
        ctx.file_versions = self.inner.file_versions.clone();
        ctx.deadline = init.deadline;
        ctx.files_anywhere = init.files_anywhere;
        // Marv's own secrets, taken out of every result (docs/SECURITY.md): the configured keys, and the MCP servers'
        // tokens as they are now (a server added mid-session counts from its next turn).
        let mcp_secrets = init.mcp.as_ref().map(|m| crate::config::mcp_secrets(&m.configs())).unwrap_or_default();
        ctx.secrets = Arc::new(marv_core::Secrets::new(init.secrets.iter().cloned().chain(mcp_secrets)));
        // The edit tools' check-and-write locks against other Marv processes in this folder: in the runtime folder,
        // else in Marv's own folder (`file_locks_dir`); with neither, within this process only.
        ctx.file_locks = marv_sandbox::runtime_dir_for(&marv_sandbox::runtime_env(), Some(&init.root))
            .map(|dir| dir.join(marv_tools::file_lock::LOCK_FOLDER))
            .or_else(|| init.file_locks_dir.clone());
        // The main agent's doors out of the sandbox, fixed for the session (the prompt names them: the cache).
        ctx.doors.host = init.host_commands;
        ctx.doors.github = init.github;
        // The Herdr door's commands and spawns (#74): where the session has the door and someone approves each command
        // at a terminal, as the host door. Reads need neither.
        ctx.doors.herdr = init.herdr.is_some() && init.host_commands;
        // The process door (Marv #51): the main agent's alone. Subagents get the forwards (their bash reaches the
        // servers), never the processes.
        ctx.doors.process = self.inner.process_on;
        if self.inner.process_on {
            ctx.processes = self.inner.processes.clone().map(|m| Arc::new(m) as Arc<dyn marv_tools::ProcessHost>);
        }
        ctx.forwards = self.inner.processes.clone().map(|m| Arc::new(m) as Arc<dyn marv_tools::ProcessForwards>);
        let mut runner = SessionToolRunner::new(ctx, Arc::new(tools));
        if init.nested_instructions {
            runner.nested = Some(self.inner.nested.clone());
        }
        let runner = Arc::new(runner);

        let mut options = AgentOptions::new(provider.clone(), history, system, specs, runner.clone(), stop.clone());
        options.max_steps = max_steps.unwrap_or(marv_agent::agent::DEFAULT_MAX_STEPS);
        // #90: with debug recording on, the providers collect the raw response payloads for the trajectory.
        options.debug_raw = crate::config::debug_raw();
        // Always one, even with no checks: the loop looks at the stop after `after_step`, so a stop during a mid-turn
        // compaction (on_full) ends the turn there instead of sending one more request (Marv stopped before it).
        options.after_step = Some(
            checks.map_or_else(|| -> marv_agent::agent::AfterStep { Arc::new(|_| Box::pin(async { None })) }, |c| c.after_step),
        );
        if let Some(mesh) = &init.mesh {
            let taint = self.inner.taint.clone();
            options.after_step = Some(crate::mesh::with_inbox(
                options.after_step.take(),
                mesh.clone(),
                crate::mesh::parent_id(),
                taint,
                radio_read,
            ));
        }
        // A direct call to a deferred MCP tool runs as the mcp_call it meant (#14), and its result says so.
        let deferred = self.state().listing.as_ref().is_some_and(|l| l.mode == McpMode::Deferred);
        let mcp_names: HashSet<String> =
            if deferred { turn_specs.iter().map(|s| s.name.clone()).collect() } else { HashSet::new() };
        if !mcp_names.is_empty() {
            let runner = runner.clone();
            options.rewrite_call = Some(Arc::new(move |call: &ToolCall| {
                let (fixed, note) = as_mcp_call(call, &mcp_names)?;
                runner.note_rewrite(&call.id, note);
                Some(fixed)
            }));
        }
        // A long turn can fill the window by itself: check between steps, and compact there too. The events say when
        // ("Compacting the conversation…") and how it went, and the loop goes on from the summary.
        options.on_full = Some({
            let (session, provider, factory, info, stop, emit) =
                (self.clone(), provider.clone(), factory.clone(), info.clone(), stop.clone(), emit.clone());
            Arc::new(move || {
                let (session, provider, factory, info, stop, emit) =
                    (session.clone(), provider.clone(), factory.clone(), info.clone(), stop.clone(), emit.clone());
                Box::pin(async move {
                    if !session.try_compact() {
                        return false;
                    }
                    let result = session.compact_if_full(&provider, factory.as_ref(), info.as_ref(), &stop, true, &emit).await;
                    match result {
                        CompactResult::Compacted { .. } => true,
                        // Esc during the summary meant "stop the turn", not "keep going on the old context".
                        CompactResult::NotCompacted { reason: CompactFailure::Stopped, .. } => {
                            stop.cancel();
                            false
                        }
                        CompactResult::NotCompacted { .. } => false,
                    }
                })
            })
        });
        // #63: a request too long for the window: make it fit (a summary of a cut-down history, or dropping the oldest
        // turns) and retry the step, instead of ending the turn with the provider's error.
        // This rewrites the history under the loop like `on_full`: the loop resets what it counts as already sent
        // (`sent_len`, for "send now") on `Recovered`, as it does after a mid-turn compaction.
        options.on_overflow = Some({
            let (session, provider, factory, info, stop, emit) =
                (self.clone(), provider.clone(), factory.clone(), info.clone(), stop.clone(), emit.clone());
            Arc::new(move |error: String, attempt: u32| {
                let (session, provider, factory, info, stop, emit) =
                    (session.clone(), provider.clone(), factory.clone(), info.clone(), stop.clone(), emit.clone());
                Box::pin(async move {
                    emit(SessionEvent::Status(TurnStatus::Compacting));
                    let (outcome, result) = session
                        .recover_overflow(&error, attempt, true, &provider, factory.as_ref(), info.as_ref(), &stop, Some(&emit))
                        .await;
                    emit(SessionEvent::Compaction(result));
                    outcome
                })
            })
        });
        // At the step limit, ask instead of stopping dead; with no one to ask, it stops there. max_steps 0 (the guard
        // off) leaves on_limit unset, so there is nothing to ask.
        if let Some(approve) = approve.clone()
            && max_steps != Some(0)
        {
            let every = max_steps.unwrap_or(marv_agent::agent::DEFAULT_MAX_STEPS);
            options.on_limit = Some(Arc::new(move |steps| {
                let approve = approve.clone();
                Box::pin(async move {
                    // Only a yes goes on: "unavailable" (marv -p --approve answers that for anything outside its list)
                    // means no one could say so, and stops like a hard limit.
                    match approve.approve(step_limit_request(steps, every)).await {
                        Decision::Yes | Decision::Always => LimitAnswer::Continue,
                        Decision::Unavailable => LimitAnswer::Unavailable,
                        Decision::No | Decision::Declined(_) => LimitAnswer::Stop,
                    }
                })
            }));
        }

        // With no one to say "keep going" (no approver: `marv -p`; or one that answers "unavailable": `marv -p
        // --approve`), the limit first gets one tools-off wrap-up reply (#59), the headless reply. After the user's "no"
        // the loop stops without it.
        options.wrap_up_at_limit = true;
        // `marv -p --time-limit`: the loop adds the time left to each result, says "finish now" near it, and stops at it.
        options.deadline = init.deadline;
        // The person's "send now" messages: the main agent's alone, at its step boundaries.
        options.send_now = self.state().send_now.clone();

        let mut reason = TurnEndReason::Done(StopReason::Error);
        let mut subagents = 0;
        let mut events = std::pin::pin!(marv_agent::run_agent(options));
        while let Some(event) = events.next().await {
            if let Some(main) = lock(main).as_mut() {
                main.event(&event);
            }
            self.state().note_changed(number, &event);
            self.turn_progress(&event);
            // After a call's result, or the mesh inbox read before the turn or between steps: anything but a delta.
            let check_taint =
                !matches!(event, LoopEvent::TextDelta(_) | LoopEvent::ThinkingDelta(_) | LoopEvent::ToolCallDelta { .. });
            match &event {
                LoopEvent::Usage(usage, _) => {
                    self.state().last = Some(*usage);
                    self.count(usage, factory.as_ref(), info.as_ref(), None);
                }
                // "Send now": in the transcript where the model read it, as the person's.
                LoopEvent::SentNow { text } => {
                    self.note(Role::User, text);
                    self.state().radio_turn.sent_now = true;
                }
                LoopEvent::Assistant { text } => {
                    self.note(Role::Assistant, text);
                    if !text.trim().is_empty() {
                        self.state().radio_turn.reply = Some(text.clone());
                    }
                }
                LoopEvent::ToolStart { call, .. } if call.name == "agent" => {
                    let args = marv_agent::agent::agent_args(&call.arguments);
                    subagents += 1;
                    let subagent = format!("{turn}.{subagents}");
                    record(TrajectoryRecord::SubagentStart {
                        who: Who { turn: turn.into(), agent: "main".into() },
                        subagent: subagent.clone(),
                        call: call.id.clone(),
                        agent_type: args.kind,
                        description: args.description,
                        prompt: args.prompt,
                        isolation: args.isolation,
                    });
                    lock(subs).insert(call.id.clone(), recorder(&record, turn, &subagent));
                }
                LoopEvent::ToolEnd { call, run } => {
                    // Where the result came from, for its saved page (#63) if a compaction pages it out.
                    let worked = !run.result.is_error && !run.declined;
                    self.state().provenance.insert(call.id.clone(), (run.provenance, worked));
                    if run.declined {
                        self.state().radio_turn.declined = Some(call.name.clone());
                    }
                    // A subagent that failed before its loop started never sent Done.
                    if let Some(mut sub) = lock(subs).remove(&call.id) {
                        sub.finish(if run.result.is_error { "error" } else { "end" });
                    }
                }
                LoopEvent::Done(r) => reason = TurnEndReason::Done(*r),
                _ => {}
            }
            emit(SessionEvent::Loop(event));
            // marv-rs: a result that tainted the session (web, MCP, mesh, image) is shown and logged once.
            if check_taint {
                let sources = self.inner.taint.sources();
                for source in sources.iter().skip(tainted) {
                    record(TrajectoryRecord::Taint { turn: Some(turn.into()), source: *source });
                    emit(SessionEvent::Tainted { source: *source });
                }
                tainted = sources.len();
            }
        }
        reason
    }

    /// The typecheck and the project's checks after file changes: a baseline before the first, a check after each
    /// step that made one. Both can run at once: `Checking` lasts until the last of them is done.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn turn_checks(
        &self,
        turn: &str,
        sandbox: bool,
        diagnostics: Diagnostics,
        checks_on: bool,
        stop: &CancellationToken,
        emit: &Emit,
        main: &Recorder,
        record: &Record,
    ) -> Option<TurnChecks> {
        if !diagnostics.any() && !checks_on {
            return None;
        }
        let checking = Arc::new(AtomicI32::new(0));
        let on_status: Arc<dyn Fn(bool) + Send + Sync> = {
            let (emit, main) = (emit.clone(), main.clone());
            Arc::new(move |on| {
                let now =
                    if on { checking.fetch_add(1, Ordering::SeqCst) + 1 } else { checking.fetch_sub(1, Ordering::SeqCst) - 1 };
                if on && now == 1 {
                    emit(SessionEvent::Status(TurnStatus::Checking));
                }
                if !on && now == 0 {
                    emit(SessionEvent::Status(TurnStatus::Running));
                    if let Some(main) = lock(&main).as_mut() {
                        main.restart_clock();
                    }
                }
            })
        };
        let root = self.inner.init.root.clone();
        let mut diagnostic_parts = Vec::new();
        for (_, checker) in self.inner.diagnostics.iter().filter(|(language, _)| diagnostics.get(*language)) {
            let (emit, record, turn, kind) = (emit.clone(), record.clone(), turn.to_string(), checker.kind.clone());
            // A kind with a cache: its folder's last measured size goes with each record (read from the record file).
            let cache = checker.cache.clone().filter(|_| kind.cache());
            let key = cache.as_ref().map(|_| crate::diagnostics::cache::repo_key(&root));
            diagnostic_parts.extend(checker.turn(TurnOptions {
                root: root.clone(),
                sandbox,
                cancel: stop.clone(),
                on_status: on_status.clone(),
                on_result: Arc::new(move |result: CheckResult| {
                    let size = cache.as_ref().zip(key.as_ref()).and_then(|(cache, key)| cache.recorded_size(key));
                    record(check_record(&turn, kind.source(), &result, |added| kind.note(added), size));
                    emit(SessionEvent::Check { label: kind.label(), result });
                }),
            }));
        }
        // The languages' notes share one step's budget (#187 §6.7); the project checks keep their own.
        let mut parts: Vec<TurnChecks> =
            combine_checks_within(diagnostic_parts, crate::diagnostics::turn::MAX_REPORTED_PER_STEP).into_iter().collect();
        if checks_on {
            let (emit, record, turn, kind) =
                (emit.clone(), record.clone(), turn.to_string(), self.inner.project_checks.kind.clone());
            parts.extend(self.inner.project_checks.turn(TurnOptions {
                root,
                sandbox,
                cancel: stop.clone(),
                on_status,
                on_result: Arc::new(move |result: ProjectCheckResult| {
                    record(check_record(&turn, kind.source(), &result, |added| kind.note(added), None));
                    emit(SessionEvent::Checks(result));
                }),
            }));
        }
        combine_checks(parts)
    }

    /// The turn's checkpoint pair: its snapshots go to the trajectory, a failure is said once (and kept for
    /// `undo_plan()` and /checkpoints), and a slow before shows as `Checkpointing`. A failure the last turn already
    /// had (the same message, counts aside: a home folder's "too large" every turn) isn't said again until a
    /// checkpoint works or another failure comes.
    pub(super) fn checkpoint_turn(&self, number: u32, label: &str, turn: &str, emit: &Emit, record: &Record) -> CheckpointTurn {
        let (session, record, turn) = (self.clone(), record.clone(), turn.to_string());
        let on_snapshot = move |kind: CheckpointKind, r: &SnapshotResult| {
            record(checkpoint_record(&turn, kind, r));
            if r.ok() {
                session.state().checkpoint_failure = None;
            }
        };
        let (session, failed_emit, said) = (self.clone(), emit.clone(), AtomicBool::new(false));
        let on_failed = move |message: String| {
            let before = session.state().checkpoint_failure.replace(message.clone());
            if before.is_some_and(|before| same_failure(&before, &message)) {
                said.store(true, Ordering::SeqCst);
            }
            if !said.swap(true, Ordering::SeqCst) {
                failed_emit(SessionEvent::CheckpointFailed { message });
            }
        };
        let slow_emit = emit.clone();
        let on_slow =
            move |on: bool| slow_emit(SessionEvent::Status(if on { TurnStatus::Checkpointing } else { TurnStatus::Running }));
        CheckpointTurn::new(self.inner.checkpoints.clone(), number, label)
            .on_snapshot(Arc::new(on_snapshot))
            .on_failed(Arc::new(on_failed))
            .on_slow(Arc::new(on_slow))
    }
}

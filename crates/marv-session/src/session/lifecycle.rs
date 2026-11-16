//! A conversation's start and end: session-start hooks, `clear()`, `resume()` and `/undo`.

use super::*;

impl MarvSession {
    /// Starts this conversation's session-start hooks (#108) in the background, unless they've started already (a
    /// no-op without hooks). Must run inside a Tokio runtime. A client calls it as it starts, to show the hooks at once
    /// ([`Self::start_hooks_done`]); otherwise the first turn starts them. `clear()` and `resume()` start a new round.
    pub fn run_start_hooks(&self) {
        self.start_round(false);
    }

    /// A new round of hooks for a new conversation: `clear()`, `resume()`. A round still running is stopped (its
    /// commands killed, their output unused).
    pub(super) fn restart_hooks(&self) {
        self.start_round(true);
    }

    /// Starts a round: always with `again`, else only if this conversation's hasn't started. Checked and claimed under
    /// one lock, so two callers at once can't both start the commands (Greptile on #115). Nothing after `close()`.
    pub(super) fn start_round(&self, again: bool) {
        let hooks = self.inner.init.hooks.clone();
        let done = CancellationToken::new();
        let stop = CancellationToken::new();
        let mut s = self.state();
        if s.closed || (!again && (hooks.is_empty() || s.hooks_started)) {
            return;
        }
        s.hook_round += 1;
        let round = s.hook_round;
        s.hooks_started = !hooks.is_empty();
        s.hooks_done = (!hooks.is_empty()).then(|| done.clone());
        s.hook_runs = None;
        s.hooks_unsent = false;
        if let Some(old) = s.hook_stop.take() {
            old.cancel();
        }
        s.hook_task = None; // stopped above; it ends on its own
        if hooks.is_empty() {
            return;
        }
        s.hook_stop = Some(stop.clone());
        let (session, root) = (self.clone(), self.inner.init.root.clone());
        // Spawned with the lock held, so the task's handle is in place before anyone can look for it.
        s.hook_task = Some(tokio::spawn(async move {
            let runs = crate::hooks::run_hooks(&hooks, &root, &stop).await;
            {
                let mut s = session.state();
                if s.hook_round == round && !stop.is_cancelled() {
                    s.hook_runs = Some(runs);
                    s.hooks_unsent = true;
                }
            }
            done.cancel();
        }));
    }

    /// The current round of session-start hooks, once it has finished: `None` without hooks, or when a newer round
    /// (a `clear()` or `resume()` meanwhile) replaced it. For a client to show them.
    pub async fn start_hooks_done(&self) -> Option<Vec<HookRun>> {
        let (round, done) = {
            let s = self.state();
            (s.hook_round, s.hooks_done.clone()?)
        };
        done.cancelled().await;
        let s = self.state();
        if s.hook_round == round { s.hook_runs.clone() } else { None }
    }

    /// `/clear`: a new conversation (and session file), with memory reloaded for the new system prompt. Totals stay.
    /// Saves the previous conversation first. Its taint ends with it (but for a networked process still running, or a
    /// Marv started tainted).
    pub async fn clear(&self) -> Result<(), SessionError> {
        {
            let transcript = self.transcript();
            let mut s = self.state();
            self.idle(&s)?;
            // The last turn's save may still be waiting: it must go to the old file, with the old conversation.
            self.save_now(&mut s, transcript);
            s.saved = new_session(&self.inner.init.root, s.factory.id(), s.factory.model(), crate::time::now_ms());
            s.new_chain(&self.inner.checkpoints);
            s.conversation = Arc::default();
            s.messages.clear();
            s.last = None;
            s.listing = None; // a new conversation decides its MCP mode again
            s.plan = None; // a new conversation, a new plan
            s.provenance.clear();
            s.person_message = None;
            // #124: the new conversation offers radio_tx if it starts with the radio on (fixed until the next clear).
            s.radio_offered = self.radio_on_now();
        }
        self.hold_tool_output(); // the new id's folder (#61)
        lock(&self.inner.loaded).clear();
        self.inner.file_versions.forget_all(); // the new conversation has read nothing yet
        self.inner.nested.reset();
        // Taint lasts as long as the text does: the cleared conversation's outside text is gone, so its taint (and
        // the URL prefixes approved under it) goes too, but for what outlives the clear (`taint_kept_by_clear`).
        self.inner.taint.restore(&taint_kept_by_clear(&self.processes(), self.inner.init.tainted_at_start));
        if let Some((paths, _)) = &self.inner.init.memory
            && let Ok(memories) = crate::memory::load_memory(paths).await
        {
            self.state().memories = Some(memories);
        }
        // Servers removed with /mcp remove kept their tools listed for the old conversation's sake; not for this one.
        if let Some(mcp) = &self.inner.init.mcp {
            mcp.forget_removed();
        }
        let system = self.build_system();
        self.state().system = system;
        // The new conversation's own hooks (#108): what they report may have changed since the last one began.
        self.restart_hooks();
        self.changed();
        Ok(())
    }

    /// Brings back a saved session (its id, or the latest here); it keeps saving to the same file. `None` when
    /// there's none. Fails during a turn.
    pub async fn resume(&self, target: ResumeTarget) -> Result<Option<Resumed>, SessionError> {
        {
            let s = self.state();
            self.idle(&s)?;
        }
        let Some(store) = self.inner.init.sessions.clone() else {
            return Ok(None);
        };
        let root = &self.inner.init.root;
        let saved = match &target {
            ResumeTarget::Latest => store.latest(root).await,
            ResumeTarget::Id(id) => store.load(root, id).await,
        };
        let Some(saved) = saved else { return Ok(None) };
        let (resumed, id) = {
            let transcript = self.transcript();
            let mut s = self.state();
            // A turn may have started while it loaded: swapping the conversation under it would mix the two.
            self.idle(&s)?;
            // The last turn's save may still be waiting: it belongs to the conversation being left.
            self.save_now(&mut s, transcript);
            lock(&self.inner.loaded).clear();
            // What the conversation being left read says nothing about what the resumed one has seen.
            self.inner.file_versions.forget_all();
            // The saved conversation may lack a note this one had: give them again (a repeat is harmless).
            self.inner.nested.reset();
            // A file 0.8.0 saved can hold two user turns in a row: repaired as it comes back (a valid one is unchanged).
            s.conversation = Arc::new(Mutex::new(marv_agent::compact::normalize(saved.conversation.clone())));
            s.messages = saved.transcript.clone();
            s.totals = saved.totals.clone();
            s.last = None;
            s.listing = saved.mcp_listing();
            s.provenance.clear();
            s.person_message = None;
            // The model needs nothing extra: its last plan call (or the summary holding the plan) is still in the
            // conversation. A session without the plan tool restores none: compaction would tell the model to keep
            // updating it.
            s.plan = if self.inner.init.plan { saved.plan_items() } else { None };
            // marv-rs: the conversation still holds the web, MCP or mesh text that tainted it, so its taint comes back
            // with it (docs/SECURITY.md): egress keeps asking after `marv -c`.
            self.inner.taint.restore(&saved.taint_sources());
            // #124: radio requests a turn read and didn't report on (Marv stopped mid-turn): the next turn that reads
            // the radio reports on them. Ones that break the rules are dropped.
            for open in saved.radio_requests.iter().flatten().filter(|r| r.valid()) {
                if !s.radio_requests.iter().any(|r| r.id == open.id) {
                    s.radio_requests.push(open.clone());
                }
            }
            // #202: its last turn never ended (Marv quit mid-turn): the transcript says so, and the model hears it at
            // the start of the next message (every call already has a result: the file was saved valid).
            // A turn still running in another Marv (its process alive) isn't cut off: left as it is (#212 review).
            let cut_off = saved
                .cut_off
                .clone()
                .filter(|d| !d.is_empty())
                .filter(|_| !crate::cut_off::saved_by_another_live_marv(saved.pid));
            let notice = cut_off.as_deref().map(cut_off_notice);
            if let Some(notice) = &notice {
                let id = s.messages.last().map_or(0, |m| m.id) + 1;
                s.messages.push(Message::new(id, Role::System, notice.clone()));
            }
            s.cut_off_notice = notice.clone();
            let resumed = Resumed {
                id: saved.id.clone(),
                updated_at: saved.updated_at,
                model: saved.model.clone(),
                transcript: s.messages.clone(),
                totals: saved.totals.clone(),
                plan: s.plan.clone(),
                cut_off: notice,
            };
            let (saved_checkpoints, pending_note, id) = (saved.saved_checkpoints(), saved.pending_note(), saved.id.clone());
            let pending_note = match (pending_note, cut_off.as_deref().map(cut_off_note)) {
                (Some(earlier), Some(note)) => Some(format!("{earlier}\n\n{note}")),
                (earlier, note) => note.or(earlier),
            };
            s.saved = SavedSession { cut_off: None, pid: None, conversation: Vec::new(), ..saved };
            s.new_chain(&self.inner.checkpoints);
            // Its chain (the saved one: a recovery below may have replaced the session's own), if it still exists:
            // numbering goes on after its last checkpointed turn, and an undo preview still knows what each turn reported.
            if let Some(saved) = saved_checkpoints {
                s.chain_id = saved.chain;
                self.inner.checkpoints.set_session(&s.chain_id);
                s.reported = saved.reported.into_iter().collect();
            }
            // An undo's note the model hadn't been sent yet: it still leads the next message (the conversation is
            // untouched).
            s.pending_note = pending_note;
            (resumed, id)
        };
        // Its saved outputs are this conversation's again: kept while it lives, 7 days from now after (#61).
        self.hold_tool_output();
        // One that can't be read is left as it is: this conversation's turns go on a chain of their own, so their
        // numbers can't meet its turns. Its id is saved with the session, so the next resume finds those turns.
        let chain = self.inner.checkpoints.chain().await;
        let mut s = self.state();
        match chain {
            Ok(chain) => s.turn_number = chain.iter().map(|c| c.turn).max().unwrap_or(0),
            Err(error) => {
                s.chain_id = format!("{id}.{}", crate::time::now_ms());
                self.inner.checkpoints.set_session(&s.chain_id);
                s.reported.clear();
                s.checkpoint_failure = Some(format!("{error}: the turns before this resume can't be undone"));
            }
        }
        drop(s);
        // The resumed conversation goes on now: its next message gets the hooks' output as of now (#108).
        self.restart_hooks();
        // #202: marked once: the file now has the notice and the note, and no cut-off turn.
        if resumed.cut_off.is_some() {
            self.save_at_once();
        }
        Ok(Some(resumed))
    }

    /// /undo's preview: what `undo()` would put back, or why there's nothing to. Fails during a turn.
    pub async fn undo_plan(&self) -> Result<Result<UndoPlan, UndoRefusal>, SessionError> {
        let (on, failure) = {
            let s = self.state();
            self.idle(&s)?;
            (s.checkpoints, s.checkpoint_failure.clone())
        };
        let refusal = |reason: String| Ok(Err(UndoRefusal { reason, failed: false }));
        if !on {
            return refusal("checkpoints are off (/checkpoints on turns them on)".into());
        }
        let store = &self.inner.checkpoints;
        if let Some(off) = store.off() {
            return refusal(format!("checkpoints are off for this project: {off}"));
        }
        let reported = |turn: u32| self.state().reported.get(&turn).cloned().unwrap_or_default();
        let plan = plan_undo(store, reported, None).await;
        if let Err(refused) = &plan {
            if let Err(error) = store.locate().await {
                return refusal(format!("no checkpoints here: {error}"));
            }
            if let Some(failure) = failure {
                return Ok(Err(UndoRefusal {
                    reason: format!("{}; the last checkpoint failed: {failure}", refused.reason),
                    failed: refused.failed,
                }));
            }
        }
        Ok(plan)
    }

    /// /undo's yes: puts the plan's files back (after a snapshot of how they are now), and tells the model at the
    /// start of the next message. Fails during a turn, and if the plan is stale.
    pub async fn undo(&self, plan: &UndoPlan) -> Result<UndoResult, SessionError> {
        {
            let mut s = self.state();
            self.idle(&s)?;
            s.running = true; // no turn may start while files are being written back
        }
        struct Free<'a>(&'a MarvSession);
        impl Drop for Free<'_> {
            fn drop(&mut self) {
                self.0.state().running = false;
                self.0.schedule_save(); // the note waits in the session file, in case Marv exits before the next message
            }
        }
        let _free = Free(self);
        let result = apply_undo(&self.inner.checkpoints, plan, None).await.map_err(SessionError::Undo)?;
        if let Some(log) = self.trajectory() {
            let turn = self.state().last_turn.as_ref().map(|(turn, _)| turn.clone()).unwrap_or_default();
            log.write(&TrajectoryRecord::Checkpoint {
                turn,
                kind: CheckpointKind::PreUndo.as_str(),
                ok: true,
                sha: Some(result.pre_undo.clone()),
                files: Some(result.pre_undo_files),
                ms: result.pre_undo_ms,
                message: None,
            });
            let paths = result.restored.iter().chain(&result.recreated).chain(&result.deleted).cloned().collect();
            let skipped = result.refused.iter().map(|r| r.path.clone()).collect();
            log.write(&TrajectoryRecord::Undo { turn: plan.turn, paths, skipped });
        }
        if let Some(note) = undo_note(plan, &result) {
            let mut s = self.state();
            s.pending_note = Some(match s.pending_note.take() {
                Some(earlier) => format!("{earlier}\n\n{note}"),
                None => note,
            });
        }
        Ok(result)
    }
}

//! Saving the conversation: the snapshot a save writes, when saves run, and `flush()`/`close()`.

use super::*;

impl MarvSession {
    /// Saves the conversation now (it's also saved shortly after every turn).
    pub async fn save(&self) -> Result<(), SessionError> {
        let Some(store) = self.inner.init.sessions.clone() else {
            return Ok(());
        };
        let transcript = self.transcript();
        let (saved, seq, written) = {
            let mut s = self.state();
            let saved = self.snapshot(&mut s, transcript);
            s.save_seq += 1;
            (saved, s.save_seq, s.written.clone())
        };
        let mut written = written.lock().await;
        if written.get(&saved.id).is_none_or(|last| seq > *last) {
            store.save(&saved).await?;
            written.insert(saved.id.clone(), seq);
        }
        Ok(())
    }

    /// #202: the open turn's progress (a reply, a call started or ended): saved soon.
    pub(super) fn turn_progress(&self, event: &LoopEvent) {
        {
            let mut s = self.state();
            let Some(open) = s.turn_open.as_mut() else { return };
            match event {
                LoopEvent::ToolStart { call, .. } => open.started(call),
                LoopEvent::ToolEnd { call, run } => open.finished(call, &run.result.output),
                LoopEvent::ToolSkipped { call, output } => open.finished(call, output),
                LoopEvent::Assistant { .. } | LoopEvent::Compacted => {}
                _ => return,
            }
        }
        self.save_soon();
    }

    /// #202: an approval is being asked in the open turn: saved now, since no one may ever answer it. `None` outside a
    /// turn (/undo's, a mesh wire's).
    pub(crate) fn asking(&self, request: &marv_tools::ApprovalRequest) -> Option<u64> {
        let id = self.state().turn_open.as_mut().map(|open| open.ask(request))?;
        self.save_at_once();
        Some(id)
    }

    /// The question `asking` returned this id for has its answer (or its wait ended).
    pub(crate) fn answered(&self, id: u64) {
        if let Some(open) = self.state().turn_open.as_mut() {
            open.answered(id);
        }
    }

    /// #202: the resumed conversation's last turn was cut off: the notice for the client to show, once. A client
    /// that resumed with `resume()` has it in `Resumed` (and the transcript) already.
    pub fn take_cut_off_notice(&self) -> Option<String> {
        self.state().cut_off_notice.take()
    }

    /// The client's transcript, read without the session's lock held (its callback may read the session).
    pub(super) fn transcript(&self) -> Option<Vec<Message>> {
        self.inner.init.transcript.as_ref().map(|f| f())
    }

    /// What `save()` writes: every tool call answered (a run winding down may still have unanswered calls).
    pub(super) fn snapshot(&self, s: &mut State, transcript: Option<Vec<Message>>) -> SavedSession {
        // #63 review: alternating roles too (0.8.0 saved two user turns in a row after an errored or stopped turn); a
        // valid history is unchanged. #202: mid-turn, a waiting call gets its own result (its output, or why there is
        // none), and the file says what a quit now cuts off.
        let (conversation, cut_off) = {
            let history = s.conversation.lock().unwrap_or_else(|e| e.into_inner());
            match &s.turn_open {
                None => (marv_agent::compact::normalize(marv_agent::agent::answer_all_calls(&history)), None),
                Some(open) => {
                    let waiting = open.waiting(&history);
                    let answered = marv_agent::agent::answer_all_calls_with(&history, |c| open.result(c, &waiting));
                    (marv_agent::compact::normalize(answered), Some(open.detail(&history)))
                }
            }
        };
        // `s.saved` keeps no conversation (nothing reads it there): cloning it doesn't copy the history again.
        s.saved.conversation = Vec::new();
        let mut saved = s.saved.clone();
        saved.cut_off = cut_off;
        // #212 review: the process that saved a cut-off turn, so a resume elsewhere can tell it's still alive.
        saved.pid = saved.cut_off.is_some().then(std::process::id);
        saved.provider = s.factory.id().into();
        saved.model = s.factory.model().into();
        saved.conversation = conversation;
        saved.transcript = transcript.unwrap_or_else(|| s.messages.clone());
        saved.totals = s.totals.clone();
        saved.mcp = s.listing.as_ref().and_then(|l| serde_json::to_value(l).ok());
        saved.plan = s.plan.as_ref().and_then(|p| serde_json::to_value(p).ok());
        let checkpoints = SavedCheckpoints {
            chain: s.chain_id.clone(),
            reported: s.reported.iter().map(|(turn, paths)| (*turn, paths.clone())).collect(),
        };
        saved.checkpoints = serde_json::to_value(checkpoints).ok();
        saved.pending_note = s.pending_note.clone().map(serde_json::Value::String);
        saved.taint = Some(self.inner.taint.sources());
        saved.radio_requests = (!s.radio_requests.is_empty()).then(|| s.radio_requests.clone());
        let conversation = std::mem::take(&mut saved.conversation);
        s.saved = saved.clone();
        saved.conversation = conversation;
        saved
    }

    /// The waiting save, now: before the conversation is swapped (clear, resume), or the timer would save the new one.
    /// The snapshot is taken here, so starting the write is enough; flush() waits for it to land.
    pub(super) fn save_now(&self, s: &mut State, transcript: Option<Vec<Message>>) {
        if let Some(timer) = s.save_timer.take() {
            timer.abort();
            self.start_save(s, transcript);
        }
    }

    /// Writes the current snapshot in the background (`flushing`).
    pub(super) fn start_save(&self, s: &mut State, transcript: Option<Vec<Message>>) {
        let (Some(store), Ok(handle)) = (self.inner.init.sessions.clone(), tokio::runtime::Handle::try_current()) else {
            return;
        };
        let saved = self.snapshot(s, transcript);
        s.save_seq += 1;
        let (seq, written) = (s.save_seq, s.written.clone());
        s.flushing.retain(|h| !h.is_finished());
        s.flushing.push(handle.spawn(async move {
            // Saves overlap now (#202: mid-turn ones): an older snapshot never lands after a newer one.
            let mut written = written.lock().await;
            if written.get(&saved.id).is_none_or(|last| seq > *last) {
                let _ = store.save(&saved).await;
                written.insert(saved.id.clone(), seq);
            }
        }));
    }

    /// #202: saves within [`SAVE_DELAY`] (a waiting save isn't pushed back, so a busy turn still gets saved).
    pub(super) fn save_soon(&self) {
        if self.state().save_timer.is_none() {
            self.schedule_save();
        }
    }

    /// #202: saves now (the write starts; `flush()` waits for it): before an approval nobody may answer.
    pub(super) fn save_at_once(&self) {
        if self.inner.init.sessions.is_none() {
            return;
        }
        let transcript = self.transcript();
        let mut s = self.state();
        if let Some(timer) = s.save_timer.take() {
            timer.abort();
        }
        self.start_save(&mut s, transcript);
    }

    /// Saves shortly (a burst of turns is one write). When the timer fires it's `save_now`, so flush() waits for it.
    pub(super) fn schedule_save(&self) {
        if self.inner.init.sessions.is_none() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let session = self.clone();
        let mut s = self.state();
        if let Some(timer) = s.save_timer.take() {
            timer.abort();
        }
        s.save_timer = Some(handle.spawn(async move {
            tokio::time::sleep(SAVE_DELAY).await;
            let transcript = session.transcript();
            let mut s = session.state();
            // This task is the timer: taken, not aborted.
            if s.save_timer.take().is_some() {
                session.start_save(&mut s, transcript);
            }
        }));
    }

    /// Before exiting: the pending save, and the trajectory's queued records, on disk.
    pub async fn flush(&self) {
        let (timer, log, flushing) = {
            let mut s = self.state();
            (s.save_timer.take(), s.log.clone(), std::mem::take(&mut s.flushing))
        };
        if let Some(t) = timer {
            t.abort();
        }
        let _ = self.save().await;
        if let Some(log) = log {
            log.flush().await;
        }
        for write in flushing {
            let _ = write.await;
        }
    }

    /// Stops what's running, waits for it to wind down, flushes, and closes the MCP servers the session started.
    /// After it, `send()`, `compact()`, `clear()` and `resume()` fail; calling it again is fine.
    pub async fn close(&self) {
        let working = {
            let mut s = self.state();
            s.closed = true;
            if let Some(stop) = &s.current {
                stop.cancel();
            }
            (s.working.take(), s.hook_stop.take(), s.hook_task.take(), s.prune_task.take())
        };
        let (working, hook_stop, hook_task, prune_task) = working;
        // The checkers' warm-ups belong to the session (#187).
        for (_, checker) in &self.inner.diagnostics {
            checker.stop();
        }
        // Session-start hooks still running are killed (with what they started) and waited for (Greptile on #115).
        if let Some(stop) = hook_stop {
            stop.cancel();
        }
        // A backstop: the commands are bounded already (killed, then a short grace for their pipes).
        if let Some(task) = hook_task {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
        // A turn's end saves its conversation (and arms a save timer): waiting for it means that save is part of
        // this flush, rather than one that lands after close() returned.
        if let Some(done) = working {
            done.cancelled().await;
        }
        // The checkpoint prune may be in a `git gc` (bounded at 120 s): one that outlived close() kept writing in the
        // config folder after the caller had removed it (CI's leftover-folder check on the herdr guards, #142).
        if let Some(task) = prune_task {
            let _ = task.await;
        }
        self.flush().await;
        if self.inner.init.owns_mcp
            && let Some(mcp) = &self.inner.init.mcp
        {
            mcp.close().await;
        }
        if let Some(processes) = &self.inner.processes {
            processes.close_all().await;
        }
    }
}

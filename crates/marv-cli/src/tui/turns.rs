//! Running turns from the TUI: the queue of messages typed during a turn, forwarding the session's events, suggestions, `/compact` and `/undo`.

use super::*;

impl Bridge {
    /// Runs one turn's events through the mapping, on its own task.
    pub(super) fn run_turn(self: &Arc<Self>, text: String, for_model: Option<String>) {
        // The person typed: radio wakes paused by the overall limit may wake again (#124).
        self.radio_wake_limit.lock().unwrap_or_else(|e| e.into_inner()).person_typed();
        {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            if gate.closing {
                return;
            }
            // A turn is running that the App didn't know of when this was typed (a wake-up whose start hadn't reached
            // it, or the end just shown while the next one is being handed over): this goes next, never lost.
            if gate.turn {
                // Already in the App's transcript: ahead of the person's queued messages (behind other shown input),
                // so it runs next, in the order the transcript shows. The person sent something: no hold.
                let at = gate.queued.iter().position(|q| !q.shown).unwrap_or(gate.queued.len());
                gate.queued.insert(at, Queued { text, for_model, shown: true });
                gate.held = false;
                if gate.woken {
                    self.emit(system("A mesh message woke Marv just now: yours goes next, when that turn ends.", false));
                }
                return;
            }
            gate.turn = true;
            // The person sent: a queue a stop kept follows this turn.
            gate.held = false;
            // Working from before the turn's task starts, so Herdr hears it before anything the turn asks.
            self.activity.set_busy(true);
        }
        self.start_claimed(text, for_model, false);
    }

    /// Enter while a turn runs (#184): the message waits for the turn, listed above the prompt. The person sent another
    /// message, so a queue a stop held goes on again. The turn already over (the App hadn't seen its end): the oldest
    /// queued message starts now, its line shown first.
    pub(super) fn queue(self: &Arc<Self>, text: String) {
        self.radio_wake_limit.lock().unwrap_or_else(|e| e.into_inner()).person_typed();
        let (listed, next) = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            if gate.closing {
                return;
            }
            gate.held = false;
            gate.queued.push_back(Queued { text, for_model: None, shown: false });
            let next = if gate.turn {
                None
            } else {
                gate.turn = true;
                self.activity.set_busy(true);
                gate.queued.pop_front()
            };
            (gate.listed(), next)
        };
        // Always the list after the change: the App listed it already (it thought a turn ran).
        self.emit(UiEvent::Queued(listed));
        if let Some(Queued { text, for_model, shown }) = next {
            if !shown {
                self.emit(UiEvent::Message(message(Role::User, text.clone())));
            }
            self.start_claimed(text, for_model, !shown);
        }
    }

    /// "Send now" text no turn read (it ended first): ahead of the person's other queued messages, behind input the
    /// App already showed. The turn's end decides whether it goes on ([`Self::finished`]).
    pub(super) fn queue_unread(&self, texts: Vec<String>) {
        if texts.is_empty() {
            return;
        }
        let listed = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            if gate.closing {
                return;
            }
            let at = gate.queued.iter().position(|q| !q.shown).unwrap_or(gate.queued.len());
            for (i, text) in texts.into_iter().enumerate() {
                gate.queued.insert(at + i, Queued { text, for_model: None, shown: false });
            }
            gate.listed()
        };
        self.emit(system("✻ The turn ended before its next step: your message is queued instead.", false));
        self.emit(UiEvent::Queued(listed));
    }

    /// Takes the person's newest (`newest`) or oldest queued message out of the queue, and lists what's left.
    pub(super) fn unqueue(&self, newest: bool) -> Option<String> {
        let (text, listed) = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            let at = if newest { gate.queued.iter().rposition(|q| !q.shown) } else { gate.queued.iter().position(|q| !q.shown) }?;
            let text = gate.queued.remove(at).map(|q| q.text)?;
            if gate.listed().is_empty() {
                gate.held = false;
            }
            (text, gate.listed())
        };
        self.emit(UiEvent::Queued(listed));
        Some(text)
    }

    /// Sends a message on a gate already taken for it.
    /// `queued`: the person's queued message (#184): refused, it goes back to the front of the queue (held, as any
    /// failure holds it), never lost.
    pub(super) fn start_claimed(self: &Arc<Self>, text: String, for_model: Option<String>, queued: bool) {
        self.stop_suggesting();
        let turn = match self.session.send(&text, SendOptions { for_model: for_model.clone(), cancel: None }) {
            Ok(turn) => {
                self.radio_title();
                turn
            }
            Err(e) => {
                // The App started its turn on submit: end it (this one, the rejected turn: no other turn's end).
                self.emit(system(e.to_string(), true));
                self.emit(UiEvent::TurnEnd { turn: String::new(), reason: view::TurnEndReason::Loop(StopReason::Error) });
                if queued {
                    let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
                    let at = gate.queued.iter().position(|q| !q.shown).unwrap_or(gate.queued.len());
                    gate.queued.insert(at, Queued { text, for_model, shown: false });
                    let listed = gate.listed();
                    drop(gate);
                    self.emit(UiEvent::Queued(listed));
                }
                self.finished(Ended::Failed);
                return;
            }
        };
        self.forward(turn);
    }

    /// The gate's turn is over: the next queued input starts on the same gate (nothing can slip in between), or the
    /// bridge is free. Input the App already showed always goes on; the person's queued messages go on only after a
    /// turn that finished (#184): after a stop or a failure they wait, and the person is told.
    pub(super) fn finished(self: &Arc<Self>, ended: Ended) {
        let (next, listed, kept) = {
            let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            gate.woken = false;
            let holds_now = matches!(ended, Ended::Stopped | Ended::Failed) && gate.queued.iter().any(|q| !q.shown);
            if holds_now {
                gate.held = true;
            }
            // Input the App already showed always goes on, wherever it waits; the person's queued messages only while
            // nothing holds them.
            let at = gate.queued.iter().position(|q| q.shown).or_else(|| (!gate.held && !gate.queued.is_empty()).then_some(0));
            let next = at.filter(|_| !gate.closing).and_then(|at| gate.queued.remove(at));
            if next.is_none() {
                gate.turn = false;
                self.activity.set_busy(false);
            }
            // Told once, by the end that held them (a later wake that finishes doesn't say it again), also when input
            // the App already showed runs now ahead of them.
            let kept = (holds_now && !gate.closing).then(|| gate.listed().len()).filter(|n| *n > 0);
            let listed = next.as_ref().is_some_and(|q| !q.shown).then(|| gate.listed());
            (next, listed, kept)
        };
        if let Some(n) = kept {
            let what = if n == 1 { "Your queued message waits".to_string() } else { format!("Your {n} queued messages wait") };
            let why = if ended == Ended::Stopped { "Stopped" } else { "The turn didn't finish" };
            self.emit(system(
                format!(
                    "✻ {why}. {what}: Enter on an empty prompt sends the next (↑ edits it, ctrl+x drops it), or they follow your next message."
                ),
                false,
            ));
        }
        if let Some(listed) = listed {
            self.emit(UiEvent::Queued(listed));
        }
        match next {
            Some(Queued { text, for_model, shown }) => {
                if !shown {
                    // As if typed now: the person's line, then the turn (the App starts it on its first event).
                    self.emit(UiEvent::Message(message(Role::User, text.clone())));
                }
                self.start_claimed(text, for_model, !shown)
            }
            // Idle again: a message that rang the mail bell while busy, after the turn's last inbox read (its final
            // reply runs no `after_step`), would otherwise wait for some later message. `wake` checks everything
            // again (on, closing, approvals, the rate limit, which also bounds a wake that keeps failing).
            None => {
                // Only after a reply that finished: not after an error, a stop or the step limit.
                if ended == Ended::Done {
                    self.suggest_next();
                }
                let me = self.clone();
                tokio::spawn(async move { me.wake() });
            }
        }
    }

    /// A running turn's events through the mapping, on its own task; not busy once it ends.
    pub(super) fn forward(self: &Arc<Self>, turn: marv_session::Turn) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut turn = turn;
            let mut map = TurnMap::default();
            let usage_of = me.session.clone();
            let totals_fn = move || totals(&usage_of.usage().totals);
            let last_of = me.session.clone();
            let last_fn = move || last_of.usage().last;
            let mut ended = Ended::Failed;
            while let Some(event) = turn.next().await {
                let end = matches!(event, SessionEvent::TurnEnd { .. });
                if let SessionEvent::TurnEnd { reason, .. } = &event {
                    ended = match reason {
                        TurnEndReason::Done(StopReason::End) => Ended::Done,
                        TurnEndReason::Done(StopReason::Aborted) | TurnEndReason::Interrupted => Ended::Stopped,
                        TurnEndReason::Done(_) => Ended::Failed,
                    };
                }
                // "Send now" text the turn ended before reading (#184): first in the queue, where the turn's end decides
                // (it goes on after a finished turn, waits after a stop), rather than back in the prompt.
                if let SessionEvent::NotSent { texts } = event {
                    me.queue_unread(texts);
                    continue;
                }
                for ui in map_event(event, &mut map, &totals_fn, &last_fn) {
                    me.emit(ui);
                }
                if end {
                    break;
                }
            }
            if let Some(result) = map.last_checks {
                *me.last_checks.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            }
            // After the end went out: input the App sends on seeing it is queued and starts here, in order.
            me.finished(ended);
        });
    }

    /// Asks for the user's next message in the background and offers it to the prompt (`Session::suggest`), unless
    /// `suggestions` is off in the config. A new turn, a command or Esc stops it ([`Self::stop_suggesting`]).
    pub(super) fn suggest_next(self: &Arc<Self>) {
        if !self.config().suggestions {
            return;
        }
        let cancel = CancellationToken::new();
        if let Some(old) = self.suggesting.lock().unwrap_or_else(|e| e.into_inner()).replace(cancel.clone()) {
            old.cancel();
        }
        let me = self.clone();
        tokio::spawn(async move {
            let suggestion = me.session.suggest(cancel.clone()).await;
            if cancel.is_cancelled() {
                return;
            }
            // Its tokens and cost count like any request's.
            let usage = me.session.usage();
            if let Some(last) = usage.last {
                me.emit(UiEvent::Usage { last, totals: totals(&usage.totals) });
            }
            if let Some(text) = suggestion {
                me.emit(UiEvent::Suggestion(text));
            }
        });
    }

    pub(super) fn stop_suggesting(&self) {
        if let Some(cancel) = self.suggesting.lock().unwrap_or_else(|e| e.into_inner()).take() {
            cancel.cancel();
        }
    }

    /// `/compact [focus]`: shown as a turn (its waiting line, Esc stops it), reported like an automatic one.
    pub(super) fn compact(self: &Arc<Self>, focus: Option<String>) {
        let me = self.clone();
        self.activity.set_busy(true);
        tokio::spawn(async move {
            me.emit(UiEvent::TurnStart { turn: String::new() });
            me.emit(UiEvent::Status(view::TurnStatus::Compacting));
            let outcome = match me.session.compact(focus.as_deref()).await {
                Ok(result) => compact_outcome(result),
                Err(e) => view::CompactOutcome::NotCompacted { reason: "failed".into(), error: e.to_string() },
            };
            let usage = me.session.usage();
            if let Some(last) = usage.last {
                me.emit(UiEvent::Usage { last, totals: totals(&usage.totals) });
            }
            me.emit(UiEvent::Compaction { automatic: false, outcome });
            me.activity.set_busy(false);
            me.emit(UiEvent::TurnEnd { turn: String::new(), reason: view::TurnEndReason::Loop(StopReason::End) });
        });
    }

    /// /undo: the last turn's changes, shown like an approval (each file's diff), and on a yes put back. Outside a
    /// turn only (a turn could be writing the same files); nothing is written before the yes.
    pub(super) fn undo(self: &Arc<Self>) {
        if self.activity.busy.load(Ordering::SeqCst) || self.session.busy() {
            return self.emit(system("Marv is working: stop the current turn (Esc) before /undo.", true));
        }
        let me = self.clone();
        self.activity.set_busy(true);
        tokio::spawn(async move {
            me.undo_now().await;
            me.activity.set_busy(false);
        });
    }

    pub(super) async fn undo_now(&self) {
        self.emit(UiEvent::Busy(Some("Reading the checkpoints…".into())));
        let plan = self.session.undo_plan().await;
        self.emit(UiEvent::Busy(None));
        let plan = match plan {
            Err(e) => return self.emit(system(format!("Couldn't read the checkpoints: {e}"), true)),
            Ok(Err(refusal)) if refusal.failed => return self.emit(system(format!("Couldn't undo: {}.", refusal.reason), true)),
            Ok(Err(refusal)) => {
                let why = refusal.reason.strip_prefix("nothing to undo: ").unwrap_or(&refusal.reason);
                return self.emit(system(format!("Nothing to undo: {why}."), false));
            }
            Ok(Ok(plan)) => plan,
        };
        if plan.files.is_empty() {
            let text = format!("Turn {} changed nothing in this project's folder, so there's nothing to undo.", plan.turn);
            return self.emit(system(text, false));
        }
        let request = marv_session::undo_request(&plan);
        let asked = marv_tools::Approver::approve(&TuiApprover(self.activity.clone()), request).await;
        if asked != marv_tools::Decision::Yes {
            return self.emit(system("Nothing was undone.", false));
        }
        self.emit(UiEvent::Busy(Some("Undoing…".into())));
        let undone = self.session.undo(&plan).await;
        self.emit(UiEvent::Busy(None));
        match undone {
            Ok(result) => self.emit(system(marv_session::undo_summary(&plan, &result), false)),
            Err(e) => self.emit(system(format!("Couldn't undo: {e}"), true)),
        }
    }
}

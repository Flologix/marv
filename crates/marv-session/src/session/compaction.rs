//! Compaction: summarizing the conversation when it's nearly full (or on `/compact`), paging out what it replaces, and recovering from a context overflow.

use super::*;

impl MarvSession {
    /// `/compact [focus]`: summarizes the conversation now and continues from the summary. Fails during a turn.
    pub async fn compact(&self, focus: Option<&str>) -> Result<CompactResult, SessionError> {
        let stop = CancellationToken::new();
        let done = CancellationToken::new();
        let (provider, factory, info) = {
            let mut s = self.state();
            self.idle(&s)?;
            s.running = true;
            s.current = Some(stop.clone());
            s.working = Some(done.clone());
            (s.provider.clone(), s.factory.clone(), s.info.clone())
        };
        // Freed however it ends, a caller dropping the future included.
        let _free = Free { session: self, done };
        Ok(self.summarize_into(focus, &stop, &provider, factory.as_ref(), info.as_ref(), CompactTrigger::Manual).await)
    }

    /// The user's likely next message (`marv_agent::suggest`), for the TUI's empty prompt; `None` when there's nothing
    /// to suggest. Asked only while idle, on the conversation's own prefix (the prompt cache), and never added to it.
    /// Never in a tainted session: text from the web, an MCP server or another agent could steer it, and a suggestion
    /// is one key away from being sent as the user's own message.
    pub async fn suggest(&self, cancel: CancellationToken) -> Option<String> {
        let (provider, factory, info, conversation, system) = {
            let s = self.state();
            if self.idle(&s).is_err() {
                return None;
            }
            (s.provider.clone(), s.factory.clone(), s.info.clone(), s.conversation.clone(), s.system.clone())
        };
        if !self.tainted().is_empty() {
            return None;
        }
        let history = lock(&conversation).clone();
        // Again with the history in hand: outside text that came in meanwhile (an inbox message) tainted it first.
        if !self.tainted().is_empty() {
            return None;
        }
        let tools = self.offered().0;
        let on_usage = |usage: Usage| self.count(&usage, factory.as_ref(), info.as_ref(), None);
        marv_agent::suggest::suggest(marv_agent::suggest::SuggestOptions {
            provider: provider.as_ref(),
            history: &history,
            system: &system,
            tools: &tools,
            cancel,
            on_usage: Some(&on_usage),
        })
        .await
    }

    /// Whether compaction should be tried at all now: nearly full, and no summary failed this turn, or the context has
    /// grown enough since one did (#144, [`may_compact_again`]).
    pub(super) fn try_compact(&self) -> bool {
        self.nearly_full() && {
            // context_length() takes the state's lock itself: read it first.
            let window = self.context_length();
            let s = self.state();
            let now = s.last.map(|l| l.prompt_tokens + l.completion_tokens);
            may_compact_again(s.compact_failures, s.compact_failed_at, now, window)
        }
    }

    /// When the context is nearly full, summarizes the conversation and continues from the summary, right where the
    /// turn is (also between steps: a long turn can fill the window by itself). Emits the status and the compaction
    /// event around it, so the client shows one notice per compaction. `mid_turn`: between two steps (not before a
    /// user message), so the summary needs a "continue" note after it. #63: an ordinary summary that overflowed the
    /// window counts its failure, then goes on to the overflow recovery's level 0.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn compact_if_full(
        &self,
        provider: &Arc<dyn Provider>,
        factory: &dyn ProviderFactory,
        info: Option<&ModelInfo>,
        stop: &CancellationToken,
        mid_turn: bool,
        emit: &Emit,
    ) -> CompactResult {
        if !self.try_compact() {
            return CompactResult::NotCompacted {
                reason: CompactFailure::Empty,
                error: "The context isn't nearly full.".into(),
                trigger: CompactTrigger::Auto,
            };
        }
        emit(SessionEvent::Status(TurnStatus::Compacting));
        let size = self.state().last.map(|l| l.prompt_tokens + l.completion_tokens);
        let result = self.summarize_into(None, stop, provider, factory, info, CompactTrigger::Auto).await;
        let result = match result {
            CompactResult::Compacted { .. } => {
                if mid_turn {
                    self.push_mid_turn_note();
                }
                result
            }
            // A summary that failed isn't tried again right away: the next step would ask for the same expensive
            // summary that just failed. Marv waited for the next turn (a new user message); marv-rs #144 also tries
            // again within the turn once the context has grown (`may_compact_again`), since `marv -p` has only one.
            CompactResult::NotCompacted { reason, ref error, .. } => {
                {
                    let mut s = self.state();
                    s.compact_failures += 1;
                    s.compact_failed_at = size;
                }
                if reason == CompactFailure::Failed && marv_provider::errors::is_context_overflow(error) {
                    let error = error.clone();
                    self.recover_overflow(&error, 0, mid_turn, provider, factory, info, stop, Some(emit)).await.1
                } else {
                    result
                }
            }
        };
        emit(SessionEvent::Compaction(result.clone()));
        result
    }

    /// The compacted history ends with the model's own "Got it" (or the tail), so mid-turn the next request needs an
    /// instruction to go on: it follows the summary, before the loop's next request. Before a user message, that
    /// message is the instruction.
    pub(super) fn push_mid_turn_note(&self) {
        let specs = self.state().turn_mcp_specs.clone();
        let text = match self.announce(&specs) {
            Some(note) => format!("{MID_TURN_NOTE}\n\n{note}"),
            None => MID_TURN_NOTE.to_string(),
        };
        let history = self.state().conversation.clone();
        // A v2 tail can end with a user turn: the note joins it.
        marv_agent::compact::push_user(&mut lock(&history), &text);
    }

    /// The compaction threshold (tokens): `nearly_full`'s `min(85% × window, compactTokens)`; `None` when neither is
    /// known.
    pub(super) fn threshold(&self) -> Option<u64> {
        let window = self.context_length().filter(|c| *c > 0).map(|c| (c as f64 * marv_agent::compact::COMPACT_AT) as u64);
        let budget = self.state().compact_tokens;
        match (window, budget) {
            (Some(w), Some(b)) => Some(w.min(b)),
            (w, b) => w.or(b),
        }
    }

    /// #63: the v2 verbatim tail's budget (`compactTailTokens`, or `min(15k, 15% of the threshold)`); 0 under v1.
    pub(super) fn tail_tokens(&self) -> u64 {
        let (version, set) = {
            let s = self.state();
            (s.compaction, s.compact_tail_tokens)
        };
        match version {
            marv_agent::compact::Version::V1 => 0,
            marv_agent::compact::Version::V2 => set.unwrap_or_else(|| marv_agent::compact::default_tail_tokens(self.threshold())),
        }
    }

    /// #63: how many turns at the start of `history` are the last summary and its acknowledgement (kept by overflow
    /// recovery). Only when the session holds a summary (never guessed from a message's text alone).
    pub(super) fn summary_head(&self, history: &[ChatTurn]) -> usize {
        use marv_agent::compact::{COMPACTED_ACK, COMPACTED_PREFIX};
        if self.state().saved.summary.is_none() {
            return 0;
        }
        match history {
            [ChatTurn::User { text }, ChatTurn::Assistant { text: ack, tool_calls, .. }, ..]
                if text.starts_with(COMPACTED_PREFIX) && ack == COMPACTED_ACK && tool_calls.is_empty() =>
            {
                2
            }
            [ChatTurn::User { text }, ..] if text.starts_with(COMPACTED_PREFIX) => 1,
            _ => 0,
        }
    }

    /// #63: what the next summary asks for: v1's, or v2's template, rolling when the session holds a summary.
    pub(super) fn summary_kind(&self) -> marv_agent::compact::SummaryKind {
        use marv_agent::compact::{SummaryKind, Version};
        let s = self.state();
        match (s.compaction, &s.saved.summary) {
            (Version::V1, _) => SummaryKind::V1,
            // Only a v2 summary is updated: a v1 one (v2 just switched on) has none of the template's sections.
            (Version::V2, Some(_)) if s.saved.summary_v2 => SummaryKind::Rolling,
            (Version::V2, _) => SummaryKind::Full,
        }
    }

    /// Summarizes the conversation and continues from the summary (`marv_agent::compact`). Only what the model sees
    /// changes. The provider, factory and info are the caller's (a turn's `info` can be `None`, and a default would
    /// swap in the current one). #63 v2: the newest turns stay verbatim after the summary, and the template is nudged
    /// for once.
    pub(super) async fn summarize_into(
        &self,
        focus: Option<&str>,
        stop: &CancellationToken,
        provider: &Arc<dyn Provider>,
        factory: &dyn ProviderFactory,
        info: Option<&ModelInfo>,
        trigger: CompactTrigger,
    ) -> CompactResult {
        let history = lock(&self.state().conversation.clone()).clone();
        let empty =
            || CompactResult::NotCompacted { reason: CompactFailure::Empty, error: "Nothing to compact yet.".into(), trigger };
        if history.is_empty() {
            return empty();
        }
        let tail = self.tail_tokens();
        // v2: a history that fits in the tail has nothing worth compacting (`tail_start` case 1).
        if tail > 0 && marv_agent::compact::tail_start(&history, tail) == 0 {
            return empty();
        }
        let v2 = self.state().compaction == marv_agent::compact::Version::V2;
        self.summarize_level(&history, &history, tail, v2, focus, trigger, stop, provider, factory, info, None).await
    }

    /// One summary: asked over `request` (the history, or overflow recovery's cut-down copy of it), then installed in
    /// place of `history` with the newest `tail` tokens of it kept verbatim (v2). Failures keep the history and are
    /// logged (`compact_failed`, #144).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn summarize_level(
        &self,
        history: &[ChatTurn],
        request: &[ChatTurn],
        tail: u64,
        nudge: bool,
        focus: Option<&str>,
        trigger: CompactTrigger,
        stop: &CancellationToken,
        provider: &Arc<dyn Provider>,
        factory: &dyn ProviderFactory,
        info: Option<&ModelInfo>,
        emit: Option<&Emit>,
    ) -> CompactResult {
        let (system, before) = {
            let s = self.state();
            (s.system.clone(), s.last)
        };
        // The tail (v2): never the old summary's own turns, never starting at a tool result. `tail_start` 0 means the
        // whole history would fit in it: that's no tail here (the caller decided there's something to compact).
        let keep = self.summary_head(history);
        let start = match tail {
            0 => history.len(),
            tail => match marv_agent::compact::tail_start(history, tail) {
                0 => history.len(),
                start => (start.max(keep)..history.len())
                    .find(|&i| !matches!(history[i], ChatTurn::Tool { .. }))
                    .unwrap_or(history.len()),
            },
        };
        // What's summarized away is saved before the summary is asked for (a page and the page table): if that
        // fails, nothing is replaced. In overflow recovery the turn would be dead otherwise: it goes on without a page,
        // and says so.
        let paged: Vec<(usize, ChatTurn)> = history[..start].iter().cloned().enumerate().collect();
        let (page, table) = match self.page_out(paged, trigger).await {
            Ok(saved) => saved.unzip(),
            Err(error) if matches!(trigger, CompactTrigger::Overflow { .. }) => {
                if let Some(emit) = emit {
                    emit(SessionEvent::Notice { text: format!("{error} The conversation is compacted without them.") });
                }
                (None, None)
            }
            Err(error) => {
                let error = format!("{error} The conversation was kept as it was.");
                return CompactResult::NotCompacted { reason: CompactFailure::Failed, error, trigger };
            }
        };
        let kind = self.summary_kind();
        let tools = self.offered().0;
        let on_usage = |usage: Usage| self.count(&usage, factory, info, None);
        let result = marv_agent::compact::summarize(SummarizeOptions {
            provider: provider.as_ref(),
            history: request,
            system: &system,
            tools: &tools,
            cancel: stop.clone(),
            focus,
            on_usage: Some(&on_usage),
            kind,
            nudge,
        })
        .await;
        let summary = match result {
            Ok(summary) => summary,
            Err(error) => {
                let reason = if stop.is_cancelled() { CompactFailure::Stopped } else { CompactFailure::Failed };
                // #144: why it failed (a tool call, a cut-off or empty summary, the provider's error) goes on record.
                if let Some(log) = self.trajectory() {
                    log.write(&TrajectoryRecord::CompactFailed {
                        turn: self.trajectory_turn(&log),
                        reason: reason.as_str(),
                        error: error.clone(),
                        tokens: before.map(|b| b.prompt_tokens + b.completion_tokens),
                        trigger: trigger.as_str(),
                        level: trigger.level(),
                    });
                }
                // The history stays: so does what it would have replaced, and the page goes.
                if let Some(page) = page {
                    self.unpage(page).await;
                }
                return CompactResult::NotCompacted { reason, error, trigger };
            }
        };
        // The list of saved pages is Marv's: a summary that writes one (restating it, or making one up) loses it.
        let summary = marv_agent::compact::Summary {
            text: marv_tools::swap::strip_page_lists(&summary.text),
            structured: summary.structured,
        };
        let plan = self.state().plan.clone().filter(|p| marv_tools::plan::plan_open(Some(p)));
        let kept: Vec<String> = plan.as_deref().map(marv_tools::plan::plan_text).into_iter().chain(table).collect();
        let kept = (!kept.is_empty()).then(|| kept.join("\n\n"));
        // A tainted conversation's summary says so in Marv's fixed words: it may repeat the outside text, unfenced.
        let tainted = self.inner.taint.is_tainted();
        let new =
            marv_agent::compact::compacted_history_with_tail(&summary.text, kept.as_deref(), history[start..].to_vec(), tainted);
        let tail_turns = history.len() - start;
        self.install(new, Some(summary.text.clone()));
        // From here the model sees the summary, not the turns before it: the trajectory needs it to say what the model
        // saw. The turn it follows only if that turn is in this file (after resume(), the last turn was in another one).
        if let Some(log) = self.trajectory() {
            log.write(&TrajectoryRecord::Compact {
                turn: self.trajectory_turn(&log),
                summary: Some(summary.text.clone()),
                trigger: trigger.as_str(),
                level: trigger.level(),
                tail_turns,
                structured: summary.structured,
                rolling: kind == marv_agent::compact::SummaryKind::Rolling,
                page,
            });
        }
        CompactResult::Compacted {
            summary: summary.text,
            tokens_before: before.map(|b| b.prompt_tokens + b.completion_tokens),
            trigger,
        }
    }

    /// The current conversation's swap folder (its saved pages), next to its saved file; `None` without a session
    /// store (nothing is saved then, and `recall` has nothing to read).
    pub(super) fn swap_dir(&self) -> Option<PathBuf> {
        let store = self.inner.init.sessions.as_ref()?;
        Some(marv_tools::swap::swap_dir(&store.file(&self.inner.init.root, &self.id())))
    }

    /// Where a turn the history holds came from, for its saved page: a result as its run said (when that taints), else
    /// as its fence or its tool says (after a resume the runs are gone), erring on the tainted side as
    /// `SavedSession::taint_sources` does.
    pub(super) fn paged_provenance(&self, turn: &ChatTurn) -> Provenance {
        let fenced = |text: &str| -> Option<Provenance> {
            let marker = format!("<{} source=\"", crate::provenance::FENCE_TAG);
            let at = text.find(&marker)? + marker.len();
            let name = text[at..].split('"').next()?;
            serde_json::from_value(serde_json::Value::String(name.into())).ok()
        };
        match turn {
            ChatTurn::Tool { call_id, name, text } => {
                if let Some(run) = self.state().provenance.get(call_id).map(|(p, _)| *p).filter(|p| p.taints()) {
                    return run;
                }
                fenced(text).unwrap_or(match name.as_str() {
                    // As `SavedSession::taint_sources` reads an old file: GitHub's text is GitHub's (a networked
                    // command's output carries its own `network` fence, read above).
                    "web_fetch" | "agent" => Provenance::Web,
                    "github" => Provenance::Github,
                    n if n.starts_with("mcp") => Provenance::Mcp,
                    n if n.starts_with("mesh") || n.starts_with("radio") => Provenance::Mesh,
                    "herdr" => Provenance::Herdr,
                    _ => Provenance::Project,
                })
            }
            ChatTurn::User { text } => fenced(text).unwrap_or(Provenance::User),
            ChatTurn::Assistant { .. } => Provenance::Marv,
        }
    }

    /// Saves `turns` (the history from index `first`) as the next page of this conversation's swap, with its page
    /// table entry, before the history they're in is replaced. `Ok(Some((page, table text)))` when saved; `Ok(None)`
    /// with nothing to save or no store; `Err` when the write failed: the caller keeps the history.
    pub(super) async fn page_out(
        &self,
        turns: Vec<(usize, ChatTurn)>,
        trigger: CompactTrigger,
    ) -> Result<Option<(u32, String)>, String> {
        let Some(dir) = self.swap_dir() else { return Ok(None) };
        if turns.is_empty() {
            return Ok(None);
        }
        let paged: Vec<marv_tools::swap::PagedTurn> = turns
            .into_iter()
            .map(|(index, turn)| marv_tools::swap::PagedTurn { index, provenance: self.paged_provenance(&turn), turn })
            .collect();
        // Held before the first page goes in, so no cleanup takes it before the session file exists.
        self.hold_swap();
        let facts = self.page_facts(&paged);
        let entry =
            marv_tools::swap::save_page(&dir, &paged, |page| marv_tools::swap::page_entry(page, &paged, trigger.as_str(), facts))
                .await
                .map_err(|e| format!("Couldn't save the messages a compaction would replace ({e})."))?;
        let table = marv_tools::swap::page_table_text(&marv_tools::swap::read_table(&dir)).unwrap_or_default();
        Ok(Some((entry.page, table)))
    }

    /// What a page's table entry lists, from the calls' runs rather than any text: the files read and changed by calls
    /// that worked (their `path`, resolved inside the project, project-relative) and the saved outputs of its results
    /// (the session's saved-outputs folder, by call id).
    pub(super) fn page_facts(&self, paged: &[marv_tools::swap::PagedTurn]) -> marv_tools::swap::PageFacts {
        let root = marv_sandbox::paths::normalize(&self.inner.init.root);
        let runs = self.state().provenance.clone();
        let saved = self.inner.init.tool_output_dir.as_ref().map(|d| d.join(self.id()));
        let mut facts = marv_tools::swap::PageFacts::default();
        for turn in paged.iter().map(|p| &p.turn) {
            match turn {
                ChatTurn::Assistant { tool_calls, .. } => {
                    for call in tool_calls {
                        let Some(read) = marv_tools::swap::path_kind(&call.name) else { continue };
                        if !runs.get(&call.id).is_some_and(|(_, worked)| *worked) {
                            continue;
                        }
                        let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or_default();
                        let Some(path) = args.get("path").and_then(serde_json::Value::as_str) else { continue };
                        let (path, _) = marv_tools::repair::repair_project_path(&root, path);
                        let abs = marv_sandbox::paths::normalize(&root.join(&path));
                        let Ok(rel) = abs.strip_prefix(&root) else { continue };
                        let rel = rel.to_string_lossy().into_owned();
                        if rel.is_empty() {
                            continue;
                        }
                        if read { facts.files_read.push(rel) } else { facts.files_changed.push(rel) }
                    }
                }
                ChatTurn::Tool { call_id, .. } => {
                    if let Some(dir) = &saved {
                        let found = marv_tools::output::saved_outputs(dir, call_id);
                        facts.tool_outputs.extend(found.iter().map(|p| p.display().to_string()));
                    }
                }
                ChatTurn::User { .. } => {}
            }
        }
        facts
    }

    /// The conversation's scratch folder (`/tmp` in its sandbox), inside its swap folder: held like the pages (so the
    /// startup cleanup leaves it alone) and removed with the saved session. `None` without a session store, or when it
    /// can't be made: `/tmp` is then empty for each command, as before.
    pub(super) fn scratch_dir(&self) -> Option<PathBuf> {
        let dir = self.swap_dir()?;
        self.hold_swap();
        marv_tools::swap::scratch_dir(&dir).ok()
    }

    /// Holds this conversation's swap folder (made if missing; never through a symlink) for as long as it's current.
    pub(super) fn hold_swap(&self) {
        let Some(dir) = self.swap_dir() else { return };
        let mut held = lock(&self.inner.swap_hold);
        if held.as_ref().is_some_and(|(d, _)| *d == dir) {
            return;
        }
        if std::fs::symlink_metadata(&dir).is_ok_and(|m| !m.is_dir()) {
            return;
        }
        // Once per session start or resume: a chat's /tmp that has grown past the cap starts empty.
        marv_tools::swap::trim_scratch(&dir, marv_tools::swap::SCRATCH_CAP_BYTES);
        *held = marv_tools::output::hold(&dir).ok().map(|h| (dir, h));
    }

    /// Takes the person's message (turn `u` of `history`, as it stood) out of the conversation: it alone is larger than
    /// the window. A turn that holds Marv's notes too keeps them.
    pub(super) fn drop_person_message(&self, history: &[ChatTurn], u: usize) {
        let message = self.state().person_message.take().unwrap_or_default();
        let mut edited = history.to_vec();
        if let ChatTurn::User { text } = &mut edited[u] {
            let rest = text.replacen(message.as_str(), "", 1);
            if message.is_empty() || rest.trim().is_empty() {
                edited.remove(u);
            } else {
                *text = rest.trim().to_string();
            }
        }
        let conversation = self.state().conversation.clone();
        *lock(&conversation) = marv_agent::compact::normalize(marv_agent::agent::answer_all_calls(&edited));
    }

    /// Takes back a page whose compaction didn't happen (its summary failed): the history still holds its turns.
    pub(super) async fn unpage(&self, page: u32) {
        if let Some(dir) = self.swap_dir() {
            marv_tools::swap::remove_page(&dir, page).await;
        }
    }

    /// Puts `new` in place of the conversation, in place: the loop holds this very history for the whole turn, so a
    /// compaction between its steps must rewrite it where it stands; replacing the Arc would leave the loop on the
    /// full history. `summary`: the new summary, or `None` to keep the last one (level 3). Called only once what it
    /// replaces is saved (`page_out`). Never touches the taint: dropping the turns that carried outside text doesn't
    /// untaint the session (#63).
    pub(super) fn install(&self, new: Vec<ChatTurn>, summary: Option<String>) {
        let kept_calls: HashSet<String> = new
            .iter()
            .filter_map(|t| if let ChatTurn::Tool { call_id, .. } = t { Some(call_id.clone()) } else { None })
            .collect();
        let conversation = self.state().conversation.clone();
        *lock(&conversation) = new;
        {
            // A summary may drop the list of MCP tools: the next message announces them all again (the cache starts
            // over here anyway), and schemas seen before it may be gone too.
            let mut s = self.state();
            if let Some(listing) = s.listing.as_mut() {
                listing.announced.clear();
            }
            s.last = None;
            if let Some(summary) = summary {
                s.saved.summary = Some(summary);
                s.saved.summary_v2 = s.compaction == marv_agent::compact::Version::V2;
            }
        }
        lock(&self.inner.loaded).clear();
        // The summary replaces the tool results that carried nested instruction files (#62): give them again, except
        // those whose notes sit in a result kept verbatim (#63).
        self.inner.nested.keep_calls(&kept_calls);
    }

    /// #63: makes the history fit after a request overflowed the window, starting at overflow level `attempt` (or
    /// below the level that last succeeded in this step). Levels 0–2 summarize a cut-down history at 70%, 50% and 35%
    /// of the window (the tail shrinking, none at 2); level 3 drops the oldest turns to fit 35% with no request, or,
    /// when 35% leaves no room, the whole window (the last attempt, in its place). A summary that overflows too goes
    /// one level down within the same call. Every failed summary counts toward #144's `MAX_COMPACT_FAILURES`; past
    /// it, straight to level 3. Returns what the loop does next and the compaction notice to show.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn recover_overflow(
        &self,
        error: &str,
        attempt: u32,
        mid_turn: bool,
        provider: &Arc<dyn Provider>,
        factory: &dyn ProviderFactory,
        info: Option<&ModelInfo>,
        stop: &CancellationToken,
        emit: Option<&Emit>,
    ) -> (marv_agent::agent::OverflowOutcome, CompactResult) {
        use marv_agent::agent::OverflowOutcome;
        use marv_agent::compact::{
            DROPPED_NOTE, MIN_HISTORY_BUDGET, V2_OVERFLOW_TARGETS, apply_drop, fit_for_summary, plan_drop, summary_instructions,
        };
        use marv_core::{estimate_history_tokens, estimate_tokens, estimate_turn_tokens};
        // One notice per recovery for pages that couldn't be saved, however many levels tried.
        let once: Option<Emit> = emit.map(|emit| {
            let (emit, noticed) = (emit.clone(), Arc::new(AtomicBool::new(false)));
            Arc::new(move |event: SessionEvent| {
                if matches!(event, SessionEvent::Notice { .. }) && noticed.swap(true, Ordering::SeqCst) {
                    return;
                }
                emit(event)
            }) as Emit
        });
        let emit = once.as_ref();
        let mut level = {
            let mut s = self.state();
            if attempt == 0 {
                s.overflow_level = None;
                s.overflow_last_attempt = false;
            }
            attempt.max(s.overflow_level.map_or(0, |l| l + 1))
        };
        let (system, before) = {
            let s = self.state();
            (s.system.clone(), s.last)
        };
        let history = lock(&self.state().conversation.clone()).clone();
        let tools = self.offered().0;
        // What every retried step sends whatever the history: the system prompt and the tools (kept in recovery: some
        // upstreams reject a history with tool calls when no tools are defined).
        let fixed_step = estimate_tokens(&system) + estimate_tokens(&serde_json::to_string(&tools).unwrap_or_default());
        // A refusal that says the prompt's size calibrates the estimates (as the adapter's `tokens_per_byte` does).
        let refusal = marv_provider::errors::overflow_numbers(error);
        let estimated = estimate_history_tokens(&history) + fixed_step;
        let known = self.context_length().filter(|w| *w > 0).or(refusal.map(|r| r.0));
        // Without numbers, the request was at least the window: the estimate can't have been more than that.
        let ratio = match (refusal, known) {
            (Some((_, input)), _) => input as f64 / estimated.max(1) as f64,
            (None, Some(window)) => (window as f64 / estimated.max(1) as f64).max(1.0),
            (None, None) => 1.0,
        }
        .clamp(0.25, 8.0);
        let window = known.unwrap_or((estimated as f64 * ratio) as u64);
        let reserve = 8192.min(window / 8);
        // Budgets in estimated tokens (the history is measured by the estimate).
        let est = |real: f64| (real / ratio).max(0.0);
        let (window_est, reserve_est, fixed_est) = (est(window as f64), est(reserve as f64), fixed_step as f64);
        let kind = self.summary_kind();
        let instruction = estimate_tokens(&summary_instructions(kind, None)) as f64;
        let keep = self.summary_head(&history);
        // The turn holding the person's latest message: the newest user turn that contains it (Marv's notes may
        // follow it, or be merged into it).
        let person = self.state().person_message.clone().and_then(|message| {
            history.iter().rposition(|t| matches!(t, ChatTurn::User { text } if text.contains(message.as_str())))
        });
        let tail = self.tail_tokens();
        let tokens_before = before.map(|b| b.prompt_tokens + b.completion_tokens);
        let failed = |level: u32, error: String| {
            (
                OverflowOutcome::Failed(error.clone()),
                CompactResult::NotCompacted {
                    reason: CompactFailure::Failed,
                    error,
                    trigger: CompactTrigger::Overflow { level },
                },
            )
        };
        let fixed_real = (fixed_step as f64 * ratio) as u64;
        let no_room = || {
            let left = window.saturating_sub(fixed_real + reserve);
            format!(
                "After the system prompt and tool definitions (about {fixed_real} tokens) and room for the reply, only about {left} tokens of this model's {window}-token window are left for the conversation, which is too little to go on. Pick a model with a larger window (/model), or turn off MCP servers or tools you don't need (/mcp)."
            )
        };
        loop {
            if level > 3 {
                // Every level was tried in this step: nothing is left.
                let last = self.state().overflow_last_attempt;
                return failed(3, if last { no_room() } else { error.to_string() });
            }
            if level == 3 {
                let at_35 = 0.35 * window_est - fixed_est - reserve_est;
                let (budget, last) = if at_35 >= MIN_HISTORY_BUDGET as f64 {
                    (at_35, false)
                } else if fixed_real >= window {
                    return failed(
                        3,
                        format!(
                            "The system prompt and tool definitions alone (about {fixed_real} tokens) are larger than this model's context window ({window} tokens). Pick a model with a larger window (/model), or turn off MCP servers or tools you don't need (/mcp)."
                        ),
                    );
                } else {
                    let room = window_est - fixed_est - reserve_est;
                    if room < MIN_HISTORY_BUDGET as f64 {
                        return failed(3, no_room());
                    }
                    (room, true)
                };
                // What's kept, then what's dropped: saved first, and listed after the note (the summary message
                // isn't rewritten at this level).
                let trigger = CompactTrigger::Overflow { level: 3 };
                // The note and the page list (capped) are budgeted with the turns kept.
                let note_tokens =
                    estimate_tokens(DROPPED_NOTE) + marv_tools::swap::MAX_TABLE_CHARS as u64 / marv_core::CHARS_PER_TOKEN;
                let plan = plan_drop(&history, budget as u64, keep, note_tokens, person);
                // The person's own message is never dropped; one larger than the whole window can't be sent at all.
                if let Some(u) = plan.user
                    && plan.user_tokens.is_some()
                    && estimate_turn_tokens(&history[u]) as f64 > window_est - fixed_est - reserve_est
                {
                    let size = (estimate_turn_tokens(&history[u]) as f64 * ratio) as u64;
                    // It's left out of the history, so the next message doesn't meet the same wall (the person still
                    // has it in their transcript).
                    self.drop_person_message(&history, u);
                    return failed(
                        3,
                        format!(
                            "Your message (about {size} tokens) is larger than this model's context window ({window} tokens), so it can't be sent. Shorten it, or pick a model with a larger window (/model)."
                        ),
                    );
                }
                let dropped: Vec<(usize, ChatTurn)> = plan.dropped().into_iter().map(|i| (i, history[i].clone())).collect();
                let (page, table) = match self.page_out(dropped, trigger).await {
                    Ok(saved) => saved.unzip(),
                    // A dead turn is worse than dropping unsaved: go on, and say so.
                    Err(error) => {
                        if let Some(emit) = emit {
                            emit(SessionEvent::Notice { text: format!("{error} The oldest messages are dropped without them.") });
                        }
                        (None, None)
                    }
                };
                let mut note = DROPPED_NOTE.to_string();
                // What's left of a tainted conversation may still hold outside text that's no longer fenced (an older
                // summary of it): Marv's fixed words say so, as a tainted summary's do.
                if self.inner.taint.is_tainted() {
                    note = format!("{}\n\n{note}", marv_agent::compact::TAINTED_DROP_NOTE);
                }
                if let Some(table) = table {
                    note = format!("{note}\n\n{table}");
                }
                let new = apply_drop(&history, &plan, &note);
                let kept_turns = new.len().saturating_sub(keep + 1);
                self.install(new, None);
                // Announced after the install, which forgets what was announced: otherwise the next message lists the
                // MCP tools a second time.
                if mid_turn {
                    let specs = self.state().turn_mcp_specs.clone();
                    if let Some(announced) = self.announce(&specs) {
                        marv_agent::compact::push_user(&mut lock(&self.state().conversation.clone()), &announced);
                    }
                }
                if let Some(log) = self.trajectory() {
                    log.write(&TrajectoryRecord::Compact {
                        turn: self.trajectory_turn(&log),
                        summary: None,
                        trigger: trigger.as_str(),
                        level: Some(3),
                        tail_turns: kept_turns,
                        structured: false,
                        rolling: false,
                        page,
                    });
                }
                {
                    let mut s = self.state();
                    s.overflow_level = Some(3);
                    s.overflow_last_attempt = last;
                }
                let summary = self.state().saved.summary.clone().unwrap_or_default();
                return (OverflowOutcome::Recovered, CompactResult::Compacted { summary, tokens_before, trigger });
            }
            if self.state().compact_failures >= MAX_COMPACT_FAILURES {
                level = 3;
                continue;
            }
            let budget = V2_OVERFLOW_TARGETS[level as usize] * window_est - fixed_est - instruction - reserve_est;
            if budget < MIN_HISTORY_BUDGET as f64 {
                level += 1;
                continue;
            }
            let request = fit_for_summary(&history, budget as u64, keep, person);
            // The tail shrinks from the first level: the full one is what just overflowed.
            let level_tail = match level {
                0 => tail / 2,
                1 => tail / 4,
                _ => 0,
            };
            let trigger = CompactTrigger::Overflow { level };
            let result = self
                .summarize_level(&history, &request, level_tail, false, None, trigger, stop, provider, factory, info, emit)
                .await;
            match result {
                CompactResult::Compacted { .. } => {
                    if mid_turn {
                        self.push_mid_turn_note();
                    }
                    self.state().overflow_level = Some(level);
                    return (OverflowOutcome::Recovered, result);
                }
                CompactResult::NotCompacted { .. } if stop.is_cancelled() => return (OverflowOutcome::Stopped, result),
                CompactResult::NotCompacted { ref error, .. } => {
                    {
                        let mut s = self.state();
                        s.compact_failures += 1;
                        s.compact_failed_at = tokens_before;
                    }
                    if !marv_provider::errors::is_context_overflow(error) {
                        return (OverflowOutcome::Failed(error.clone()), result);
                    }
                    // Our estimate was off: try smaller.
                    level += 1;
                }
            }
        }
    }

    /// The turn a compaction record follows: the last turn, only if that turn is in this file (after resume(), the last
    /// turn was in another one).
    pub(super) fn trajectory_turn(&self, log: &Trajectory) -> Option<String> {
        self.state().last_turn.clone().filter(|(_, session)| *session == log.session).map(|(turn, _)| turn)
    }
}

//! One turn's checkpoint pair (Marv #16). Ports Marv's `src/checkpoints/turn.ts`.
//!
//! The *before* snapshot is taken once, just before the turn's first call that can change files (`run_tool` awaits
//! [`CheckpointTurn::checkpoint`], which every such call shares, parallel subagents too), and the *after* snapshot
//! when the turn ends, dropped with its before when the two trees match.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::store::{CheckpointKind, CheckpointStore, NOT_A_REPOSITORY, SnapshotOptions, SnapshotResult};

/// How a turn's pair ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointOutcome {
    /// Nothing in the turn could change a file.
    None,
    /// Both snapshots are on the chain: /undo can take this turn back.
    Kept { before: String, after: String, files: usize, ms: u64 },
    /// The turn changed nothing: both were dropped.
    Dropped,
    /// A snapshot failed: this turn can't be undone.
    Failed { message: String },
}

/// Longer than this, and the client is told ("Saving a checkpoint…"). Small trees never show it.
pub const SLOW_MS: u64 = 300;

pub type OnFailed = Arc<dyn Fn(String) + Send + Sync>;
pub type OnSlow = Arc<dyn Fn(bool) + Send + Sync>;
pub type OnSnapshot = Arc<dyn Fn(CheckpointKind, &SnapshotResult) + Send + Sync>;

pub struct CheckpointTurn {
    store: Arc<CheckpointStore>,
    turn: u32,
    label: String,
    /// A snapshot failed (once per turn), with why.
    on_failed: Option<OnFailed>,
    /// The before snapshot is taking a while (true), and it's done (false).
    on_slow: Option<OnSlow>,
    /// Each snapshot, for trajectories.
    on_snapshot: Option<OnSnapshot>,
    started: AtomicBool,
    taken: tokio::sync::OnceCell<()>,
    before: Mutex<Option<SnapshotResult>>,
    /// The chain's head before this turn's *before*: where a pair that changed nothing goes back to.
    previous: Mutex<Option<String>>,
}

impl CheckpointTurn {
    pub fn new(store: Arc<CheckpointStore>, turn: u32, label: impl Into<String>) -> Self {
        Self {
            store,
            turn,
            label: label.into(),
            on_failed: None,
            on_slow: None,
            on_snapshot: None,
            started: AtomicBool::new(false),
            taken: tokio::sync::OnceCell::new(),
            before: Mutex::default(),
            previous: Mutex::default(),
        }
    }

    pub fn on_failed(mut self, f: OnFailed) -> Self {
        self.on_failed = Some(f);
        self
    }
    pub fn on_slow(mut self, f: OnSlow) -> Self {
        self.on_slow = Some(f);
        self
    }
    pub fn on_snapshot(mut self, f: OnSnapshot) -> Self {
        self.on_snapshot = Some(f);
        self
    }

    /// The *before* snapshot: started by the turn's first call that can change files, shared by all. Never fails.
    pub async fn checkpoint(&self) {
        self.started.store(true, Ordering::SeqCst);
        self.taken.get_or_init(|| self.take_before()).await;
    }

    async fn take_before(&self) {
        let slow = Arc::new(AtomicBool::new(false));
        let timer = self.on_slow.clone().map(|on_slow| {
            let slow = slow.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(SLOW_MS)).await;
                slow.store(true, Ordering::SeqCst);
                on_slow(true);
            })
        });
        let before = self.snapshot_before().await;
        if let Some(timer) = timer {
            timer.abort();
        }
        if slow.load(Ordering::SeqCst)
            && let Some(on_slow) = &self.on_slow
        {
            on_slow(false);
        }
        let Some(before) = before else { return };
        if let Some(on_snapshot) = &self.on_snapshot {
            on_snapshot(CheckpointKind::Before, &before);
        }
        if let (SnapshotResult::Failed { message, .. }, Some(on_failed)) = (&before, &self.on_failed) {
            on_failed(message.clone());
        }
        *self.before.lock().unwrap_or_else(|e| e.into_inner()) = Some(before);
    }

    async fn snapshot_before(&self) -> Option<SnapshotResult> {
        // Outside a repository with no git dir of Marv's own to use (an SDK session without one): there are simply no
        // checkpoints here, which isn't a failure to report every turn.
        if let Err(error) = self.store.locate().await
            && error == NOT_A_REPOSITORY
        {
            return None;
        }
        *self.previous.lock().unwrap_or_else(|e| e.into_inner()) = self.store.head().await;
        Some(
            self.store
                .snapshot(SnapshotOptions { turn: self.turn, kind: CheckpointKind::Before, label: &self.label, cancel: None })
                .await,
        )
    }

    /// At the turn's end, however it ended: the *after* snapshot, or both dropped when nothing changed. Never fails.
    pub async fn finish(&self) -> CheckpointOutcome {
        if !self.started.load(Ordering::SeqCst) {
            return CheckpointOutcome::None;
        }
        self.checkpoint().await; // one still being taken (a call stopped meanwhile) finishes first
        let before = self.before.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let (before_tree, before_sha, before_ms) = match before {
            None => return CheckpointOutcome::None,
            Some(SnapshotResult::Failed { message, .. }) => return CheckpointOutcome::Failed { message },
            Some(SnapshotResult::Ok { tree, sha, ms, .. }) => (tree, sha, ms),
        };
        let previous = self.previous.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let after = self
            .store
            .snapshot(SnapshotOptions { turn: self.turn, kind: CheckpointKind::After, label: &self.label, cancel: None })
            .await;
        if let Some(on_snapshot) = &self.on_snapshot {
            on_snapshot(CheckpointKind::After, &after);
        }
        match after {
            SnapshotResult::Failed { message, .. } => {
                if let Some(on_failed) = &self.on_failed {
                    on_failed(message.clone());
                }
                // A before without its after is no pair: let it go, so /undo never sees half of one.
                self.store.drop_to(previous.as_deref()).await;
                CheckpointOutcome::Failed { message }
            }
            SnapshotResult::Ok { tree, .. } if tree == before_tree => {
                self.store.drop_to(previous.as_deref()).await;
                CheckpointOutcome::Dropped
            }
            SnapshotResult::Ok { sha, files, ms, .. } => {
                CheckpointOutcome::Kept { before: before_sha, after: sha, files, ms: before_ms + ms }
            }
        }
    }
}

#[async_trait]
impl marv_tools::Checkpoint for CheckpointTurn {
    async fn checkpoint(&self) {
        CheckpointTurn::checkpoint(self).await;
    }
}

#[cfg(test)]
mod tests {
    // Ports tests/checkpoint-turn.test.ts.
    use super::*;
    use crate::checkpoints::store::{SnapshotFailure, StoreOptions};
    use crate::checkpoints::test_support::{make_repo, temp, write};

    fn store(root: &std::path::Path, own: Option<std::path::PathBuf>) -> Arc<CheckpointStore> {
        Arc::new(CheckpointStore::new(StoreOptions::new(root, own, "s")))
    }

    #[tokio::test]
    async fn concurrent_checkpoint_calls_take_one_snapshot_finish_keeps_a_pair_that_changed_something() {
        let root = make_repo(true);
        let own = temp("cpt-own");
        let s = store(&root.0, Some(own.0.clone()));
        let turn = CheckpointTurn::new(s.clone(), 1, "go");
        tokio::join!(turn.checkpoint(), turn.checkpoint(), turn.checkpoint());
        assert_eq!(s.chain().await.unwrap().len(), 1);
        write(root.0.join("a.txt"), "b\n");
        assert!(matches!(turn.finish().await, CheckpointOutcome::Kept { .. }));
        let kinds: Vec<CheckpointKind> = s.chain().await.unwrap().iter().map(|c| c.kind).collect();
        assert_eq!(kinds, [CheckpointKind::After, CheckpointKind::Before]);
    }

    #[tokio::test]
    async fn a_pair_that_changed_nothing_is_dropped_a_turn_without_a_checkpoint_does_nothing() {
        let root = make_repo(true);
        let own = temp("cpt-own");
        let s = store(&root.0, Some(own.0.clone()));
        let turn = CheckpointTurn::new(s.clone(), 1, "go");
        turn.checkpoint().await;
        assert_eq!(turn.finish().await, CheckpointOutcome::Dropped);
        assert!(s.chain().await.unwrap().is_empty());
        assert_eq!(CheckpointTurn::new(s, 2, "").finish().await, CheckpointOutcome::None);
    }

    #[tokio::test]
    async fn a_failing_snapshot_is_reported_once_and_never_fails_the_turn() {
        let root = make_repo(true);
        let own = temp("cpt-own");
        let s = store(&root.0, Some(own.0.clone()));
        s.fake.lock().unwrap().snapshot = Some(Arc::new(|_| {
            Some(SnapshotResult::Failed { reason: SnapshotFailure::Failed, message: "disk full".into(), ms: 1 })
        }));
        let failures = Arc::new(Mutex::new(Vec::new()));
        let seen = failures.clone();
        let turn = CheckpointTurn::new(s, 1, "go").on_failed(Arc::new(move |m| seen.lock().unwrap().push(m)));
        turn.checkpoint().await;
        turn.checkpoint().await;
        assert_eq!(*failures.lock().unwrap(), ["disk full"]);
        assert_eq!(turn.finish().await, CheckpointOutcome::Failed { message: "disk full".into() });
    }

    #[tokio::test]
    async fn a_slow_snapshot_says_so_after_300_ms() {
        let root = make_repo(true);
        let own = temp("cpt-own");
        let s = store(&root.0, Some(own.0.clone()));
        s.fake.lock().unwrap().delay = Some(Duration::from_millis(350));
        let slow = Arc::new(Mutex::new(Vec::new()));
        let seen = slow.clone();
        let turn = CheckpointTurn::new(s, 1, "go").on_slow(Arc::new(move |on| seen.lock().unwrap().push(on)));
        turn.checkpoint().await;
        assert_eq!(*slow.lock().unwrap(), [true, false]);
    }

    #[tokio::test]
    async fn outside_a_repository_without_a_git_dir_of_marvs_own_no_checkpoint_and_no_failure() {
        let plain = temp("cpt-plain");
        let failures = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = failures.clone();
        let turn = CheckpointTurn::new(store(&plain.0, None), 1, "go").on_failed(Arc::new(move |m| seen.lock().unwrap().push(m)));
        turn.checkpoint().await;
        assert_eq!(turn.finish().await, CheckpointOutcome::None);
        assert!(failures.lock().unwrap().is_empty());
    }
}

// One turn's checkpoint pair (issue #16): the *before* snapshot, taken once, just before the turn's first call
// that can change files (runTool awaits `checkpoint()`, which every such call shares, parallel subagents too),
// and the *after* snapshot when the turn ends, dropped with its before when the two trees match.
import { NOT_A_REPOSITORY, type CheckpointStore, type SnapshotResult } from "./store.ts";

/** How a turn's pair ended. */
export type CheckpointOutcome =
  /** Nothing in the turn could change a file. */
  | { status: "none" }
  /** Both snapshots are on the chain: /undo can take this turn back. */
  | { status: "kept"; before: string; after: string; files: number; ms: number }
  /** The turn changed nothing: both were dropped. */
  | { status: "dropped" }
  /** A snapshot failed: this turn can't be undone. */
  | { status: "failed"; message: string };

/** Longer than this, and the client is told ("Saving a checkpoint…"). Small trees never show it. */
export const SLOW_MS = 300;

export class CheckpointTurn {
  private taken: Promise<void> | undefined;
  private before: SnapshotResult | undefined;
  /** The chain's head before this turn's *before*: where a pair that changed nothing goes back to. */
  private previous: string | null = null;

  constructor(
    private readonly options: {
      store: CheckpointStore;
      turn: number;
      label: string;
      /** A snapshot failed (once per turn), with why. */
      onFailed?: (message: string) => void;
      /** The before snapshot is taking a while (true), and it's done (false). */
      onSlow?: (on: boolean) => void;
      /** Each snapshot, for trajectories. */
      onSnapshot?: (kind: "before" | "after", result: SnapshotResult) => void;
    },
  ) {}

  /** The *before* snapshot: started by the turn's first call that can change files, shared by all. Never rejects. */
  checkpoint = (): Promise<void> => {
    this.taken ??= this.takeBefore().catch(() => {});
    return this.taken;
  };

  private async takeBefore(): Promise<void> {
    const { store, turn, label, onSlow } = this.options;
    let slow = false;
    const timer = setTimeout(() => {
      slow = true;
      onSlow?.(true);
    }, SLOW_MS);
    try {
      // Outside a repository with no git dir of Marv's own to use (an SDK session without one): there are simply no
      // checkpoints here, which isn't a failure to report every turn.
      const where = await store.locate();
      if ("error" in where && where.error === NOT_A_REPOSITORY) return;
      this.previous = await store.head();
      this.before = await store.snapshot({ turn, kind: "before", label });
    } finally {
      clearTimeout(timer);
      if (slow) onSlow?.(false);
    }
    if (!this.before) return;
    this.options.onSnapshot?.("before", this.before);
    if (!this.before.ok) this.options.onFailed?.(this.before.message);
  }

  /** At the turn's end, however it ended: the *after* snapshot, or both dropped when nothing changed. Never rejects. */
  async finish(): Promise<CheckpointOutcome> {
    if (!this.taken) return { status: "none" };
    await this.taken;
    const before = this.before;
    if (!before) return { status: "none" };
    if (!before.ok) return { status: "failed", message: before.message };
    const { store, turn, label } = this.options;
    const after = await store.snapshot({ turn, kind: "after", label });
    this.options.onSnapshot?.("after", after);
    if (!after.ok) {
      this.options.onFailed?.(after.message);
      // A before without its after is no pair: let it go, so /undo never sees half of one.
      await store.dropTo(this.previous).catch(() => false);
      return { status: "failed", message: after.message };
    }
    if (after.tree === before.tree) {
      await store.dropTo(this.previous).catch(() => false);
      return { status: "dropped" };
    }
    return { status: "kept", before: before.sha, after: after.sha, files: after.files, ms: before.ms + after.ms };
  }
}

# Writes, snapshots, and recovery

`graphy-store::Store` layers mutable changes over an immutable base segment. Readers use immutable `Arc<Snapshot>` values; a new commit publishes a replacement snapshot atomically, so in-flight readers continue on the view they acquired.

## Commits

`Store::apply` accepts a transaction of insert/delete operations expressed as concise term bytes. A single writer resolves terms, appends WAL frames, updates ordered delta indexes, and publishes the new epoch. `apply_with` selects `Durability::Strict` or `Durability::Relaxed`.

- `Strict` syncs the log before publication.
- `Relaxed` skips that sync and may lose recent commits after a crash.

The WAL stores terms rather than generation-local numeric identifiers. Transactions become visible only after their commit frame.

## Recovery

On open, the store validates frames and replays complete committed transactions. A torn or corrupt tail is truncated to the last valid commit boundary. Checkpoints let replay skip changes already folded into the base.

## Scans

A snapshot scan merges a base-segment iterator with the matching delta range in the same order, suppressing tombstoned base quads and older delta events. Readers do not take the writer lock while consuming results.

## Merge and compaction

`Store::merge` builds a new generation from a frozen view, preserves commits that arrive during the build, atomically updates `CURRENT`, rotates the WAL, and retires old generations after their snapshots are released. `MergeConfig` controls sort memory, profile changes, and optional pacing. `MergeScheduler` can trigger background folds from configured thresholds.

Ephemeral stores have no filesystem generation to merge. They instead expose `compact_ephemeral` and `compact_ephemeral_if_due`, which rebuild an in-memory base image and bound long-lived delta overhead.

The concurrency design assumes a single writer per `Store` instance and many snapshot readers. Cross-process writers are not coordinated.

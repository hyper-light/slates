# A work's acknowledged edits were lost at a daemon restart

Found 2026-11-04 while planning the load test's chaos step (a daemon killed under its anchor mid-run). Design: D-18
("Local acknowledgement promises daemon-restart survival only when bytes, roots, witnesses, accounting and completion
records are recoverable from anchor-owned RAM"), §4.16, and BUG-11 (closed for a plain volume's content).

## Description

An edit to a work volume was answered `Edited`. After the daemon was stopped and another started over the same anchor
segment, the work read back the green's bytes, with no refusal, and its next submit would have been accepted without
the edit. The same held for a declaration, for the base an accepted submit moves a work to, and for a rebase.

## Root cause

A work's state (its own files, its journal, its base) lived only in the shard's memory. `rebuild_work` in
`crates/server/src/verbs.rs` rebuilt every recovered work as a fresh clone of its green's head, by design ("a work's
declared edits are scratch and do not survive a restart"), while the verbs that changed it answered as acknowledged.

## Impact

Any daemon restart (a crash, an upgrade under the anchor) silently dropped every unsubmitted change of every work and
moved every work to its green's head; a client told nothing would then submit an increment without its own work.

## Edits

- `crates/db/src/op.rs`, `crates/db/src/partition.rs`: `Op::WorkLogged` and `Op::WorkRestated`, a per-work log in the
  partition counted against the merge byte budget, carried in snapshots (`PartitionSnapshot::work_logs`, appended) and
  reclaimed with the work.
- `crates/server/src/work_log.rs`: the log's entries (an edit, a declaration, a restatement), and the one change path
  the verbs and the replay share (`apply_edit`, `apply_declare`, `replay`).
- `crates/server/src/verbs.rs`: `edit` and `declare` record their entry in the verb's transaction before the work
  changes (checked first, so a full budget refuses with nothing changed); an accepted submit and a rebase restate the
  work; `rebuild_work` replays the log.
- `crates/server/tests/merge_load.rs`: `acknowledged_work_edits_survive_a_daemon_restart` (failed on `a364b03c`:
  "work A's bytes after the restart are what it acknowledged before it"; passes after).

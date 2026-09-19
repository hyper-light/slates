# A green's takeover promoted one adopted record and left no servable merge chain (AUD-14)

Date: 2026-09-18. Contracts: §4.16 owner-loss recovery ("adopts the newest records, and serves"),
§4.8 "Promotion and takeover", D-27 (recomputation, mismatch fatal-and-loud); AUD-14 in
`docs/bugs/2026-09-14_AUDIT.md`; GAP-A9-14/-7; the merge-service record's owed item.

## Symptom

The generic takeover completion (`fleet::promote`… the phase-one adoption) promoted a single adopted
record, recorded its placement and cleared the pending takeover, then materialized content only when
the adopted value decoded as a plain volume's `HeadValue`. A green's adopted record is a
`MergeRecordValue` — nothing rebuilt the owned green: no catalog record on the successor, no origin,
no chain, no engine, no placed version. The successor's holder **replica** existed (recomputed as
records arrived) but a replica is not a routable green: `versions`, a read at a version, a new work's
submit and a retry all found nothing to serve. Source-confirmed by the September 14 audit.

## Root cause

Takeover had one materialization path, for heads. A green's durable chain (the increments, the
origin) and its catalog identity lived only on the owner; the successor held the inputs (by content
identity) and the accepted records (with each version's input manifest and head identity) but no code
turned them back into an owned green.

## Fix

- `MergeRecordValue` now carries the green's **name, evidence policy and owner** (filled by
  `enqueue_record` from the catalog record), so an adopted record alone names the catalog entry a
  successor must create — as `HeadValue` does for a plain volume.
- The takeover completion routes an adopted `MergeRecordValue` to
  `ShardState::pending_green_materializations`; each period `fleet::materialize_pending_greens`
  gathers the chain on the control shard (`merge_service::recover_green_inputs`): this node's **own
  accepted merge records** for the object (its holder acceptor's persisted positions, one per
  version it recomputed) give every version's input manifest, the held content gives the bytes —
  the origin for version 0, the increment for each later one — up to the adopted head. An input not
  held, or an adopted version beyond this node's accepted prefix, leaves the green pending and counted
  (`merge.takeover_incomplete`): the general ledger-prefix transfer is §5's contract (GAP-A9-7), and
  nothing is rebuilt from a partial chain.
- On the shard the id routes to, `verbs::materialize_taken_over_green` records the catalog entry
  (guard-then-apply; refused typed when the name is taken or the chain budget cannot hold it),
  re-records the origin and every increment **durably** (a restart of the successor rebuilds the
  same green), replays them with `rebuild_green` (the boot derivation: the rejected-cache budget set,
  retention settled), and verifies the rebuilt head identity against the adopted record's — a
  mismatch is fatal-and-loud for the green on this node (`merge.takeover_mismatch`, the engine not
  installed). On success the adopted head is placed (`merge.placed`), so `await placed`, `versions`,
  reads at any version and new submits serve; the AUD-11 wait for the next version's commit then
  runs against the surviving holder.
- `Daemon::merge_chain_identities(green)` reports the chain's increment identities in order — the
  comparison between owner and successor that proves the prefix and the original results survived.
- **A second defect, found by the regression's last phase and fixed with it:** the merge record
  plane wrote every record at the node's own host epoch (`next_merge_work` carried the note that "a
  taken-over green's promotion epoch is owed"). After a takeover the holders have raised their fence
  for the departed owner's bumped epoch, so the successor's first new version was refused
  `StaleEpoch` by every holder — a typed refusal returned to the sender, hence no holder-side counter
  — and its acceptance (AUD-11) waited for good. Plain heads already write at the promotion epoch
  (`placed_heads`); the green materialization now moves the takeover's placement to the owner shard
  with the green, and the merge plane writes a taken-over green's records at that promotion epoch.
  Diagnosis: the successor showed `merge.acceptance_deferred: 1, merge.inputs_unplaced: 1` and the
  other holder no refusals at version 3 — the record never landed.

## Failing test first, and regression

`crates/server/tests/fleet.rs::a_taken_over_green_serves_every_version_and_accepts_new_work_on_the_successor`
— three-node `f = 1`: a green advances three versions on its owner (each committed at the quorum,
AUD-11), both holders recompute them, neither holder reports a placed version (unowned); the owner
dies; the rendezvous-first survivor materializes the green at version 3; its chain's increment
identities equal the owner's; through the public client on the successor `versions` answers 3, `f`
reads `hello` at version 1 and `hello world!` at the head; a new work over the green submits
version 4 — committed with the remaining holder, which recomputes it — and a retry of that submit
answers the same reply from its completion record. Before the fix the successor served nothing for
the green (no catalog record: `NotFound`; no placed version).

What this does **not** cover: a successor whose accepted prefix is shorter than the adopted head
(it was the lagging holder) — it stays pending, counted, until the ledger-prefix transfer lands
(GAP-A9-7); and the owner-era request's retry (its completion record died with the owner) — the
successor answers the *same increment* idempotently from its rebuilt `accepted` map, but the
original request id is not answerable without replicated completion records (§4.9 keeps them on the
owner partition).

## Validation (this box, 18 cores, 2026-09-18/19)

- `cargo test -p slates-server --test fleet a_taken_over_green_serves_every_version_and_accepts_new_work_on_the_successor`:
  1 passed, 9.95 s (11.70 s on the first green run, before the clippy splits).
- `cargo test -p slates-server --test fleet` alone: **47 passed, 0 failed, 347.56 s** (2026-09-19,
  08:55:59 → 09:01:47).
- `cargo test -p slates-server --lib`: 91 passed (the merge-record value's round trip and truncation
  with the new name/evidence/owner fields among them); `--test daemon`: 9 passed, 14.79 s;
  `--test recovery`: 4 passed; `cargo test -p slates-client --test client`: 4 passed.
- `cargo clippy -p slates-server --all-targets -- -D warnings`, `cargo fmt --all -- --check`,
  `cargo xtask check`: clean.
- Before the fix the successor answered `NotFound` for the green and reported no placed version;
  before the epoch fix the successor's version-4 acceptance never resolved (`merge.acceptance_deferred:
  1`, `merge.inputs_unplaced: 1`, the other holder unchanged at version 3, no refusal counted).

## Siblings reviewed

- The holder replica (`MergeShardState::replicas`) stays the recomputation oracle on the successor
  too: the rebuilt owned engine is a separate instance replayed from the durable chain, and its
  identity is checked against the adopted record, not against the replica.
- Works over the taken-over green do not survive the owner (scratch, §4.16); a client creates a new
  work on the successor, as the regression does.
- Plain-volume takeover (`materialize_taken_over`) is unchanged.

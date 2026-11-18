# A successor with no room gave up a live volume a survivor held whole (2026-11-18, A-401, goal g229)

## Description

CI's macOS leg of the live chaos suite (diag-supervisor-live-2, run 44350386549, a 7 GB runner, two shards a daemon)
failed `a_write_answered_the_instant_before_its_owner_dies_is_served_by_its_successor`. In iterations 3 and 4, no
survivor served the probe written the instant before its owner died, within the fleet wait.

No write was lost. The other survivor held the whole log through the probe: in the NFS iteration both survivors were
at head 9; in the client-write iteration one was at head 8, the end of that log, as the passing iterations' successors
adopted. The successor rebuilt the volume from the adopted log (`fleet.live.taken_over`). Its restore was then refused
`BudgetExceeded` once a period for the failover SLO's 100 periods (`fleet.materialize.refused.budget_exceeded` 100,
then 200), and it gave the volume up (`fleet.materialize.given_up` 1, then 2). Every verb on the volume then answered
`NotRecovered { TakeoverIncomplete }`, for good.

## Root cause

§4.2 says: "A remote holder makes the same admission against its own machine before acknowledging placement." For a live
log, nothing did.

- A candidate took the log's entries into a copy on its control shard's heap and reserved nothing.
- The successor is the configuration's rendezvous pick among the retirement's survivors
  (`RegionalConfiguration::successor`), never chosen by room. It made the volume's admission only at its restore
  (`verbs::make_restored`).
- On the runner, each shard's reserve was 1 GiB. Each flow volume was a bounded claim of 782 MiB. Both of the successor's
  shards already held a claim it had taken over in an earlier iteration (`committed=819986432` of `reserve=1073741824`),
  so the restore could not admit a third.

The held log was outside every ledger as well: up to twice the volume's bound, on the heap.

## Impact

After a single failure, an acknowledged write that `f + 1` hosts held was refused for good whenever the successor's
shard had filled since the volume was placed. Any bounded live volume on a fleet whose candidates admit other volumes was
exposed. On a laptop (`f = 0`) nothing changes: there are no candidates.

## Exact edits

- `crates/db/src/live.rs`: `LiveError::NoRoom { available }`, carried by a new reply kind (`REPLY_NO_ROOM`), so a holder
  that cannot admit tells its owner so, typed.
- `crates/server/src/verbs.rs`:
  - `CandidateClaim`, the credits a successor's restore takes, held on the shard the volume's id routes to.
  - `restore_credits`, the admission `make_restored` made, now one function.
  - `admit_candidate_claim`, which is idempotent.
  - `reconcile_candidate_claims`, `ClaimFound` and `claimed_bytes`.
  - `make_restored` takes the claim over (`take_claim_or_admit`), and puts the credits back as the claim when a later
    step fails (`return_to_claim`).
  - `restore_taken_over` counts the claim's bytes as the volume's own, and takes the volume's id.
- `crates/server/src/state.rs`, `daemon.rs`: `ShardState::candidate_claims`.
- `crates/server/src/live_ship.rs`:
  - `Held::charged`, and `LiveState::{claims, admitting, claims_dirty}`.
  - `Room`: the held log is charged against its claim, and past it as content held for another owner on the control
    shard.
  - `keep` is now guard-then-apply.
  - Admission: `admit_claim`, `begin_admission`, `finish_admission`, `learn_claim`.
  - Reconciliation: `reconcile_claims`, `claims_kept`, `apply_reconciled`, `readmit`.
  - `let_go` credits the log and marks its partition; `restore_copies` charges each restored log and marks its
    partition.
- `crates/server/src/fleet.rs`:
  - The record plane admits the claim before it serves an entry, a batch or an install.
  - `adopt_and_serve` reconciles the claims each period.
  - `make_live_in_turns` passes the id to the restore.

## Tests

- **Red first:** `a_candidates_claim_keeps_the_room_its_successor_makes_the_volume_in` (`crates/server/tests/fleet.rs`).
  On main 94fffd000 (Mac Studio, kiln r263) it fails: served false, given up 0 -> 1. With the change it passes: served,
  six files read back, claim taken over 0 -> 1, given up 0.
- **Units:** `a_held_log_is_charged_only_past_the_claim_its_candidate_reserved` and
  `a_holder_with_no_room_for_its_log_refuses_the_entry_and_keeps_its_copy` (`live_ship`), and
  `a_holders_no_room_crosses_the_wire_with_what_it_could_admit` (`crates/db/tests/live.rs`).
- **Unchanged:** the live takeover tests (f = 2, in turns, Continuity, a successor restarted, a refused tombstone, a burst
  of writes) and the held image's fold oracle pass. `a_write_answered_after_a_live_owners_warm_restart_survives_its_takeover`
  fails on main as well (2 of 2): it is A-392's.

## Siblings (reported, not fixed here)

- **Live-ship's own refusal.** `live-ship` still answers before any candidate admits. A candidate that cannot admit
  refuses the first entry `NoRoom`, but if the configuration later names that candidate the successor, it admits afresh
  at its restore. The design's rule is that the placement is refused typed when a candidate cannot admit. Next, under
  A-401.
- **Placed seals.** A non-live volume's successor admits only at its restore too. Its content holders charge the chunks
  they hold (AUD-29-43), not the claim.
- **A successor's own copy.** A successor keeps its own copy of the log at the old epoch after it has made the volume.
  Asked of g230, who owns `live_ship`.

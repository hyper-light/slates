# A checkpoint passed a retained entry's base, and recovery fenced the green (2026-11-08, A-257)

## Description

After a restart, a green whose works had been accepted over bases behind the head was fenced: "green load-green fenced,
its recovered history is not what was acknowledged: NotAccepted { version: N }", every read of it refused
`ContentUnavailable`. Every acknowledged version of the green became unreadable. Found by the recovery-slicing
measurement (liveness) and by loadtest-scale's primary-kill rungs (7 of 18 runs fenced, always at the first restart).

## Root cause, measured

- W3 (A-238) checkpoints a green's durable chain at its reachable floor, the oldest base a live work names. A work's
  base moves to the version it is accepted as, so the floor can pass the base recorded in an entry that stays in the
  chain above it.
- Recovery restored the green at the checkpoint and replayed each later entry by submitting it again. A verdict reads
  the green at the increment's base: the deltas since (`intervening`), the file's length there (`content_len_at`, in
  `unfit_content`) and, for an overlapping length change, its bytes (`content_at`). A restored green holds none of that
  below the checkpoint, so the entry was refused `BaseUnavailable { base, floor }` and the green fenced.
- Smallest case: two works off version 0 write different files and are accepted as versions 1 and 2; the floor is 1,
  the chain is checkpointed there, and version 2 (base 0) stays in it. One restart fences the green
  (`crates/server/tests/green_checkpoint.rs` `a_checkpoint_leaves_every_later_entry_a_base_recovery_can_replay`, red on
  main 40dceb52). In the load harness: 10 clients, 3 rounds, one `kill -9`, fenced at version 2,
  `BaseUnavailable { base: 0, floor: 1 }`; loadtest-scale's rung `BaseUnavailable { base: 0, floor: 250 }`. Every
  restart of the recovery-slicing states (5 of 5 in each of 8 runs, unsliced and sliced) was fenced.

## Rejected

- **Checkpoint at or below every retained entry's base.** Correct, and it never folds under steady concurrency: two
  clients taking turns give bases 0, 0, 1, 2, 3, 4 for versions 1 to 6, so no version above 0 qualifies, and the chain
  reaches its cap with no straggler at all, which is the load W3 exists for.
- **The checkpoint carries the deltas back to the lowest retained base.** A verdict also reads the file's length and
  bytes at the base, which `Green::restore` keeps only at each path's last change, so the window would have to carry
  every touched path's values at every base in it, a cost that follows the window's file bytes.
- **Each entry restated at base `v - 1`.** It keeps replay a re-decision, but is set aside for the record-what-was-decided
  form below, the project's own rule for a replayed log.

## Exact edits

- `crates/merge/src/chain_entry.rs` (new): a durable chain entry is the increment as submitted, byte for byte, then
  what its verdict resolved it to (`Applied`: content by reference to the increment's post-state, a splice recorded by
  its path alone when its ops are the ones declared), then the head identity where one was computed, then a BLAKE3
  digest of the whole. `increment_bytes` returns the first part.
- `crates/merge/src/engine.rs`: `Green::submit_resolved` returns the verdict's resolved effects beside the outcome;
  `Green::apply_recorded` commits an entry's effects as the next version through the same `commit`, so the deltas,
  histories, last-changed indexes and accepted identities come out as they did live; an increment already accepted
  commits nothing and returns its version (a duplicated entry is fenced `WrongVersion`).
- `crates/merge/src/checkpoint.rs`: a checkpoint records the head and its identity when it was taken (encoding version 2).
- `crates/server/src/verbs.rs`: `submit` charges the chain the bound an entry for the increment can reach, then
  appends the entry (`record_accepted`); `replay_green` applies each entry, compares any recorded identity (the entry's,
  or the checkpoint's at its head) and fences `GreenFence::IdentityDiverged` on a miss; a takeover judges each adopted
  increment afresh and records what it resolved to.
  A restore from a checkpoint logs what it crossed: "slates-server: partition P: green G restored at checkpoint N,
  applied K chain entries above it, L of them based below it" (L is the count a re-judging replay would have refused).
- `crates/server/src/merge_service.rs`: `inputs_of` returns the increment as submitted, so a holder (§4.16 "Apply on
  holders") and a successor (`recover_green_inputs`) still re-decide from it and a wrong resolution on the owner
  cannot reach them; a checkpoint carries the head's identity; `record_goes_to_holders` says when a version's identity
  is read anyway. `GreenFence::NotAccepted` is gone (replay no longer judges).
- `crates/anchor/src/layout.rs`: layout 6. A version-5 segment, whose chain holds bare increments, is refused.

## Cost, measured

On this Mac (load 22-29), the load harness over the kernel tree at 1,000 clients and 3 rounds, release builds alike
(no LTO), main 40dceb52 against this change, three interleaved runs each, every harness oracle at 0 failures:

| | main | A-257 |
|---|---|---|
| chain entry, mean over 2,944 accepts | 611 bytes (the increment) | 789 bytes (+29%) |
| verdict and append, accepts over a base behind the head, p50 | 32, 32, 32 µs | 37, 36, 36 µs |
| the same, p99 | 369, 274, 346 µs | 312, 185, 343 µs |

Nearly every accept in this load (2,941 of 2,944) was over a base behind the head. The p50 rises 4-5 µs, the
resolution's encoding; the p99 moves within its own noise. The daemon's monotonic clock reads in microseconds here.

## Integrity, stated

The digest checks every entry. The head identity is recorded and compared at every checkpoint, at every version whose
merge record goes to holders, and at the head wherever one is recorded. **At f = 0 the entries after the last
checkpoint are checked by their digest alone**, so a divergence there is found at the next recorded identity, not at its
own version. Reading the identity at every f = 0 accept cost 287-363 µs against a 35-42 µs verdict (measured
2026-11-08, load harness, kernel tree, 100 and 1,000 clients). The per-version property is proven by the oracle, which
compares identity and content at every version.

## Tests

- `crates/server/tests/green_checkpoint.rs`: the red test above passes; `two_clients_taking_turns_keep_folding_and_recover_whole`
  (200 versions under a 256 KiB cap, every base trailing its head, a restart, every last write read back).
- `crates/merge/tests/chain_replay.rs`: the recovery-equals-live oracle over generated histories (submits over bases
  behind the head, conflicts, convergent edits, retries, checkpoints at arbitrary versions, crashes anywhere; every
  dimension), the rebuilt green compared whole at every crash: head, identity at every version, every retained
  version's content, every accepted increment's retry, and every later verdict. A fixed history records every kind of
  effect across a checkpoint above a retained base; two alternating clients fold to at most two entries; hostile bytes
  are refused typed. `an_entry_over_a_change_below_its_checkpoint_replays_through_each_base_read`, and a generated step
  of the same shape: an entry based below a checkpoint on a path that changed between its base and the checkpoint,
  through each of the verdict's three reads at the base (`content_at` for a length-changing overlap, `content_len_at` in
  `fits_base`, `history_at` in `special_kind_changed`), each accepted live and replayed whole. **Reverse red-check:** replaying by re-deciding each entry fails five of the six (the oracle with
  the entry refused at the checkpoint).

## Siblings

- Holders' replicas (`merge_service::recompute`) are built from version 0 by judging each record's inputs and never
  restore from a checkpoint; a successor's `recover_green_inputs` needs every version from 0 from holders. Neither reads
  the owner's chain format. `mirror.rs` holds no green.
- After an owner's restart, A-246's re-enqueue covers only the versions above its checkpoint (W3, unchanged here): a
  completion carried on a folded version's record is not re-enqueued (told to loadtest-scale for A-255).
- A green already fenced by a wrong checkpoint stays fenced: its chain cannot be restated without the history it lost.
  Pre-release, so no migration; a version-5 segment is refused whole.
- `crates/db/tests/snapshot_cadence.rs` `a_tail_whose_records_change_size_still_replays_within_its_recovery_budget`
  fails on main 40dceb52 (3 of 3 here, 12.2 ms against its 10 ms budget at load about 20), unchanged by this.

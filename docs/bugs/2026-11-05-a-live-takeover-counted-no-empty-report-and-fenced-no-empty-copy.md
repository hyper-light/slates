# A live takeover counted no empty report of its own, and fenced no empty copy

**Found** 2026-11-05, landing 13. `a_superseded_owner_resumed_after_its_takeover_is_fenced` (Linux, 1 of 2 runs, load
18–25) and `a_live_volumes_acknowledged_writes_survive_its_owners_death` (macOS, 1 of 9, load 13–21) left a live volume
unserved. The successor waited for its live log's quorum 370 and 371 times (`fleet.live.takeover_waits_quorum`) while the
other candidate answered every fetch holding the log. In both, the successor held no copy.

**Root cause.** Phase one of a live takeover (`crates/server/src/live_ship.rs`) treated a candidate with no copy two ways:
- `fetch` added the successor's own report only when it held a copy. A successor holding nothing could never reach
  `f + 1` reports once the owner was gone (at f = 1: two of the two other candidates). That is the hang.
- `serve_fetch` answered "none" for a remote candidate with no copy, and the answer counted, but it raised no fence. So
  a superseded owner, resumed, could still ship to that candidate after it reported, and commit a position outside the
  head the takeover adopted. **This is a safety defect.**

**Impact.** On main since 7f57f1b6 (A-200 slice three, landed in 2f343fcd), for opt-in live-shipped volumes only:
- A takeover whose successor held no copy never served the volume (no write was lost; the other candidate held them).
- A superseded owner could make a write durable that the takeover never saw, two values at one position
  (StaleNeverCommits). Not observed in a process run; shown by the model below.

**What decided the rule: the nemesis model** (`crates/db/tests/live.rs`). A candidate's copy now exists only once a
message reaches it, as in the daemon, and phase one treats a candidate with no copy by a rule:
- *Make an empty copy, raise its fence, count the report* (the fix). Every invariant holds over 200 seeds × 3,000
  steps: f=1 with 110 takeovers and 57 empty reports, f=2 with 241 and 145.
- *Count the report, no fence* (the daemon before; kept as the mutation test
  `an_empty_report_without_a_fence_lets_a_superseded_owner_commit`). StaleNeverCommits fails in 10 seeds.

Seed 27 at f = 1, as the trace prints it:
1. Host 1 owns the volume at epoch 1 and has shipped nothing.
2. Host 3 takes over at epoch 2 on two empty reports, hosts 2 and 3. Neither leaves a fence.
3. Host 3 appends position 0 = 1 and position 1 = 2.
4. Host 1, resumed, appends its own position 0 = 3. Host 2 has no copy and no fence, still takes host 1 for the owner,
   and accepts it, making its copy.
5. Host 1 is durable through position 0 (itself and host 2), with value 3, where the adopted owner's position 0 is 1.

Why an empty report may count: a durable entry is held by the owner and `f` candidates, so any `f + 1` of the `2f`
other candidates include one of them. It counts only because it also promises, by its fence, to take nothing older.

**Edits.**
- `crates/server/src/live_ship.rs`: `fenced_copy` gives a candidate its copy with the fence raised to the takeover's
  epoch, making an empty one when it holds none. `fetch` (the successor's own report) and `serve_fetch` (a remote
  candidate's) both use it, so every report counts and every reporter is fenced.
- `crates/server/src/live_ship.rs`, `crates/server/src/fleet.rs`: holders count refusals by reason
  (`fleet.live.hold_refused.{not_a_member, no_acting_owner, not_the_acting_owner, not_a_candidate, no_catalog,
  stale_epoch, behind, …}`; `fleet::content_authority` names why a placement is refused).
- `crates/db/tests/live.rs`: the model change, its mutation test, and `a_refused_entry_is_never_counted_as_held`.
- `crates/cli/tests/live_fleet.rs`: `a_successor_that_holds_no_copy_still_takes_a_live_volume_over` stops exactly the
  configuration's successor (computed with `rendezvous_first`) while the owner writes, then kills the owner. Red: the
  successor waited 397 times and never served. Green: served in 3.2 s.

**Sibling found, open.** Why a candidate holds no copy at all. Over three loaded runs one candidate refused the owner
`not_a_member` 22 times, then held the log (a membership lag at the fleet's start). Another refused 6 times
`not_a_member` and then 21 times `no_catalog`, and held nothing for the whole run: a candidate that never receives the
volume's catalog refuses every entry, so the log rests on one candidate. GAPS records it; raft owns it.

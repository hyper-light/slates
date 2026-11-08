# A takeover question that timed out left its holder's link lent for good

**Found.** CI diag-raft-chaos-final-41, 2026-11-07, KIND leg, wan-loss profile (80 ms ± 20 ms, 1 % loss): "no
survivor took the volume over within 120s". Safety held: the owner read back 300 of 300 writes, and both survivors
held head 357.
- The candidate, slates-1, adopted the takeover (`fleet.takeover.adopted` 2, fence 2). It then counted
  `fleet.live.takeover_waits_quorum` 425 times.
- Its record session to the other holder, slates-0, showed `lent=true` to the end, with srtt 318 ms and pto 1.41 s
  taken from the session's last sighting.
- slates-0 was alive, answering probes, and holding the head at fence 1.

It was 1 failure in 16 wan-loss runs across diag-38 to diag-45.

**Root cause.** `slates_cluster::live::exchange` sends each holder's request with its own deadline. A request that ends
with no round trip (its deadline, or a transport fault) returns its endpoint to the exchange. The exchange drops that
endpoint and lists the holder in `Exchanged::unanswered`, and the caller must then drop the holder's record link so its
task dials afresh (`live_ship::lose_links`). The ship lane did this. A-240's two takeover exchanges did not: phase one's
report question and the adopted holder's log fetch, both from d073f7a1. They returned only `reusable` and queued
`stragglers`.

So the holder's slot in `record_sessions` kept the takeover's borrow (`borrowed` set, `lent_to` =
`fleet.lent_to.live_takeover`) with no session behind it. The link task leaves a lent slot alone, and `take_sessions`
skips a slot whose session is out. Every later phase one therefore asked the holder nothing and counted only itself:
1 of the 2 (`f + 1`) recorded holders, for as long as the holder stayed in contact. One retransmission at pto 1.41 s
past the 1 s liveness budget was enough.

A holder that is stopped, rather than late, is condemned by the failure detector. Its link task then removes the
slot and dials it afresh when the holder returns. That is why the five-node flow's 3 s stop never showed the defect,
even in a run where phase one waited 5 times during the stop.

**Impact.** Liveness only. A successor never served a volume it should not have, and no acknowledged write was lost.
A live volume whose holder answered one takeover question late, and was not condemned, stayed unserved until that
holder's link was replaced for another reason.

**Edits.**
- `crates/server/src/live_ship.rs`:
  - `settle_sessions` takes an `Exchanged` whole. It drops each unanswered holder's link, returns each answering
    holder's session, and hands back the replies and stragglers. All three exchange callers go through it: the ship
    lane (`ship_lane`), phase one (`ask_reports`, split out of `fetch`), and the log fetch (`fetch_log`).
  - Sibling sweep: `Exchanged` is the only exchange result that lists unanswered holders, and these are its only
    callers.
- `crates/server/src/daemon.rs`: two test hooks. `Daemon::ask_live_reports` asks a peer phase one's question under a
  given budget, a given number of times, and reports what is left of the link (`LinkAfter`).
  `Daemon::inject_live_state_withhold` makes a peer hold its live-state replies while answering everything else, the
  probe plane included. Neither is reachable from the wire: `ShardState::live_state_withhold_replies` is read in the
  record serve side as `discovery_withhold_replies` is.
- Failing test first: `a_takeover_question_a_holder_never_answered_leaves_its_link_dialed_afresh` in
  `crates/server/tests/fleet.rs`. Both peers withhold. Each round asks both under two heartbeats, up to six rounds,
  each waiting up to three liveness budgets for a session, and the test then waits the same again. Its first red run
  on the unfixed tree showed both paths:
  - Round 1 ended as a straggler, which the drain dropped (`fleet.live.link_lost` 0 → 1).
  - Round 2 ended unanswered, and the link was still `Lent("fleet.lent_to.live_takeover")` after the wait.
- Red-check, macOS arm64 debug, 2026-11-08: with `lose_links` taken out of `settle_sessions`, 8 of 8 runs failed with
  both peers' links left lent to the takeover. With the fix, 8 of 8 passed with both links dropped to be dialed
  afresh (`Absent`).
- Three earlier forms of the test passed on the defect, and are kept in its doc:
  - A 1 ns budget made every question a straggler.
  - Wedging the peer's shard also silenced its probes, so its slot was removed and re-dialed after about 540 ms.
  - A single peer met no unanswered question in 1 of 5 runs.

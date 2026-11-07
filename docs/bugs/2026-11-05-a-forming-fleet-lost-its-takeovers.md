# A forming fleet lost its takeovers (2026-11-05)

Contracts: §4.8 (the council's voter set follows the committed membership, D-14; Raft §6; thesis §4.2.1), §4.10 and
A-200 (a live-shipped write is acknowledged once durable, and a takeover serves it), A-92 piece 4c (a successor opens
what the owner sealed through its own key entry).

Found from raft's soak: with the live fleet tests acting before the fleet had settled, a killed owner left both
survivors at term 1 with no election, and the volume was never served. The tests were then made to wait for a settled
fleet (d91464cf); the product stayed unfixed. Reproduced here by bootstrapping a fresh three-process fleet and acting at
once, which found three defects in turn. Each is fixed, with a failing test first.

## 1. The council grew to two voters, which can lose neither

**Description.** A council formed by its first node promoted each member as soon as that member caught up. When the
third member was slow (stopped, or loaded), the council sat at two voters, the bootstrap node and the owner. If the
owner then died, the survivor could not elect: two voters need both. Both survivors stayed at term 1 and every read was
refused `OwnerUnreachable`.

**Root cause.** `RegionalCouncil::reconcile_voters` began a change to any target that differed from the sitting voters.
On a stalled staging it began a partial change to the sitting voters plus whichever staged members had caught up
(`RaftNode::promote_caught_up`, added 2026-09-30, `docs/bugs/2026-09-30-one-lagging-member-held-back-every-council-promotion.md`).
A target of two voters arose either way. Two voters tolerate `⌊(2 − 1) / 2⌋ = 0` failures, no more than one voter, and
are exposed to two deaths instead of one.

**Impact.** Any owner that died in that window (about 1 s quiet, longer under load) was never taken over. The data
was not lost (the bootstrap node held it), but it was unserved until an operator's quorum-loss recovery.

**Exact edits.**
- `crates/cluster/src/config_group.rs`: `widening(sitting, proposed, members)` decides each step. A step that only adds
  voters, and leaves some member unseated, is taken only when it raises the tolerated failures; to an even count it
  drops its highest added id, and when it then raises nothing the sitting voters stay. A step that seats every member
  is taken as proposed (a two-member region keeps two copies of its log, which quorum-loss recovery can use), and so is
  a step that removes a voter. `reconcile_voters` applies it to the full target and to the partial promotion, which it
  now computes itself from `RaftNode::staged`. The root group keeps `promote_caught_up` unchanged: its voters are
  region representatives, and two regions are two voters by design.
- A member whose staging is judged stalled is passed over for a free seat (`RegionalCouncil::passed_over`), so the next
  alive member is staged in its place; once no other remains the passed-over members are tried again, and the set
  clears whenever a change begins.
- `a_member_that_cannot_catch_up_does_not_hold_back_one_that_has` becomes
  `a_council_widens_only_by_a_step_that_raises_what_it_tolerates` (f = 1: `[OWNER]` stays while B stalls, then
  `[OWNER, A, B]` in one change). New: `a_council_at_f_two_promotes_the_caught_up_when_that_raises_what_it_tolerates`,
  `a_free_seat_goes_past_a_member_that_cannot_catch_up`, `a_council_seats_every_member_of_a_region_smaller_than_its_floor`
  and `widening_takes_only_steps_that_raise_the_tolerated_failures` (every sitting size 0 to 6 against every larger
  proposal under every membership at least that large, and the removing steps).

**Evidence.** `an_owner_killed_while_its_fleet_still_forms_is_taken_over` (`crates/cli/tests/live_fleet.rs`): red 1 of
1 before (two voters at the kill, both survivors at term 1, no leader for 40 s); after, one voter at the kill and the
volume served by a survivor in 2.6 to 17.9 s, in every run that started (macOS, load 45 to 96). Each unit test is red
with its piece of the rule taken out and green with it.

**The first form of the rule broke the fleet suite, and was corrected.** As first written (no passing over, and every
even adding step declined), the in-process fleet suite failed 7 of 89 where the unmodified base passed the same seven
7 of 7 in 57 s at load 50 to 57. Three failed "initial membership did not commit": with five members a stalled
lowest-id member held the council at its bootstrap voter for the whole wait, and a two-member region was refused its
second voter. Corrected as above, the seven pass 7 of 7 in 59 s at load 53 to 62.

## 2. A live log sealed before a candidate's pair carried no key for it

**Description.** A live volume made while one candidate was stopped, before the owner's shards had delivered their pair
keys to it, shipped a whole entry whose sealing carried no copy of the lineage key for that candidate. The owner never
sealed another whole entry. When that candidate became the successor it fetched the log and could not open it: 145,
317, 376 and 409 refused rebuilds in four runs, and the volume was never served.

**Root cause.** `live_ship::payload` wraps the key for the anchors `seal_keys::head_sealing` knows (the pairs delivered
so far), and a write's durability counted the owner and `f` holders only. Heads already wait for this
(`fleet::sealing_covers`); the live log did not.

**Impact.** Acknowledged writes on such a volume were unserved after the owner's death whenever the uncovered candidate
was the successor.

**Exact edits** (`crates/server/src/live_ship.rs`).
- `LiveVolume::whole_anchors`: the anchors the last whole entry carries a key for (`None` for a clear body).
- `covers_candidates`: every remote candidate in the neighbourhood has its anchor there. `answerable`: the durable
  position counts for a writer only once the head is placed, the last whole entry covers every candidate, and that
  entry is itself durable. `durability`, `durable_target` and `resolve` read it.
- `period`: a log whose last whole entry does not cover its candidates owes a fresh whole entry once this shard has
  delivered the missing pairs (`coverable_now`), so it is checkpointed again.

**Evidence.** `a_live_volume_made_while_its_successor_was_stopped_is_served_by_it`: red 3 of 3 with `covers_candidates`
forced true (317, 376, 409 refused rebuilds); green 8 of 8 and 4 of 4 with it, served whole in 2.1 to 7.4 s. The
writes now wait while the candidate is stopped (`returned while stopped: false` in every green run) and return once it
is back. A candidate that stays away is retired by the council, leaves the neighbourhood, and stops holding writes
back, as for a sealed head.

## 3. A takeover dropped the refusal that told it its configuration was stale

**Description.** A successor that was a learner one configuration version behind the council's leader asked for pages
and adopted records under its old generation every period, and the leader refused each `ConfigurationStale`. The
successor never fetched the newer configuration, so its round never completed and the volume was never served.

**Root cause.** `takeover::ask_pages` decoded only promises and dropped every other reply, and `takeover::adopt`
ignored `commit_record`'s `stale_version`. A learner fetches only on evidence: a stale refusal it acts on, or its
membership view differing from its configuration. With the leader's own confirmation as the only change, the
membership matched, and the refusals were the only evidence.

**Exact edits** (`crates/server/src/takeover.rs`). Both sites set `config_refresh_wanted` when a holder names a newer
version, as `fleet::ship_head` already did, counted `fleet.takeover.stale_configuration`.

**Evidence.** Seen twice under diagnostics: a learner at version 6 asked a leader at 7 every period and got nothing
(`ask_pages`), and a learner at 7 had its adoption refused `NotPlaced` every period by a leader at 8 (`adopt`). Red
rate in the keyed flow with both edits switched off: 1 of 8 (the successor at version 7, the leader at 8, 2 takeovers
pending, never served). With them: 8 of 8 green.

## Open, found on the way (not fixed here)

- **A killed owner was never condemned.** In one forming-fleet run both survivors' detectors held the killed owner with
  one suspicion and no condemnation for 40 s (`configured=false`, `refused=CorrelationUnmeasured`, judged by the pool or
  provisionally), so the council never retired it. One run in about twenty; the detector's fleet-start state, owned by
  whoever owns `slates-cluster`'s member plane.
- **A live dispatch's replies are dropped with its whole task.** `live_ship::send_from_control` returns its replies
  only when every message is sent, and its bound is twice one session wait, while each message to a holder whose session
  is out on loan waits a whole session wait. One forming-fleet run had the owner at `durable=0` while the other
  candidate held every entry (`fleet.live.held` 237 there; `fleet.live.no_session` 447 on the owner). A rewrite that
  folds each holder's replies as they come was tried and reverted: no test could show it red (the loaned-session flow
  passed 3 of 3 on the old code), so the mechanism is not confirmed.
- **raft's other two formation findings did not reproduce.** In eight full live-fleet suites run without the settled
  wait (with these fixes), no owner found no session to its only reachable candidate for long, and no candidate refused
  `no_catalog` for good: the first refusal was followed by holding every time.

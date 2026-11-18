# A forward lane's silence was judged before its shard read what came (2026-11-18, §4.8 "Lookup", §4.14, A-315)

Goal g255 of the supervisor's stack: "The forward lane's wall-clock stall budget is loadtest-scale's, judged in shard
time and capped by the failover SLO", with goal g245's open item (A-306): "a forward still queued is given up at its
stall budget while its lane moves". loadtest-scale found the first half (`6da9670d`, on `agents/loadtest-scale-spin`,
never on main) and wrote its record, `2026-11-09-a-starved-origin-gave-up-its-forwards-to-a-live-owner.md`, on that
branch; its findings are summarised here with credit, and its fix is replaced.

## Description

A verb a node cannot serve is forwarded to its owner through the peer's forward lane (`crate::forward_lane`). Each
forwarder waits for its batch's replies and gives the batch up once none of its forwards has progressed for its stall
budget: a liveness budget plus the path's measured round-trip tail. Its caller (`verbs::resolve_and_forward`) sends it
again while the owner is not confirmed dead and the failover bound allows.

The quiet was the wall clock since the batch's last progress, judged whenever the forwarder woke. Two things counted
against a live owner:

- **An origin kept off its CPU.** The waiting forwarders sleep until woken or until their budget's end. A control shard
  kept off its CPU past a budget wakes with its timers expired and its owner's replies sitting unread in its socket. The
  lane's driver reads one datagram a turn and yields between turns, so the waiting forwarders, woken by their timers,
  run ahead of it, find no progress, and give their batches up to a live owner.
- **A forward still queued.** A forward waits queued until the session's stream credit admits it. Nothing of it has
  been sent, yet its quiet ran from its queueing, so behind a backlog longer than a budget it was given up and resent
  while its lane moved: 17,000 to 52,000 batches a run behind a 256-client burst (A-306's record), each a give-up and a
  resend on the control shard.

## Measured

loadtest-scale's record (2026-11-09, adacachy2, 32 CPUs): clients' false-death runs, an origin at nice 19 beside two
spinners a CPU, refused its read after 10 to 12 s with `fleet.forward.batch_given_up` at 20 to 25 against 2 batches
begun; `a_starved_origin_keeps_its_forward_to_a_live_owner` (b's control shard at nice 19 pinned beside two spinners, 32
clients reading through b) gave up 2 to 1,208 batches to the live a in 6 of 6 runs on main.

This record's test, `a_held_origin_gives_up_no_forward_whose_reply_came_while_it_was_held`
(`crates/server/tests/fleet.rs`): a mebibyte file on a, eight clients of b reading it at once, a's answers delayed half
a budget, and b's control shard held for one and a half budgets as soon as b has begun the forwards
(`Daemon::starve_control_shard`), so a's answers arrive inside the hold:

| Tree (GitHub diag, 2026-11-18) | ubuntu-latest | macos-latest | windows-latest |
| --- | --- | --- | --- |
| main's lane, the test alone (`diag-g255-lane-red-1`, run 44253485264) | failed 3 of 3 | failed 3 of 3 | failed 3 of 3 |
| A-315's first form, the shard's waits alone (`-green-1`, run 44253484990; `-green-2`, run 44254220660) | passed 5 of 6 | passed 5 of 6 | passed 6 of 6 |
| A-315's third form, a turn after the shard's waits that read its queue empty (`-green-5`, run 44257173978) | passed 8 of 8 | not run | passed 8 of 8 |
| A-315 as landed, a turn begun after the silence that read its own socket empty, merged tree (`-green-7`, run 44260700333) | passed 8 of 8 | not run | passed 8 of 8 |
| A-315 with the sessionless turn, A-317 over it, merged with main as of `1e8f4a49e` (`diag-g255-land2-1`, run 44273659893) | passed 3 of 3 | passed 3 of 3 | passed 3 of 3 |

On main every run gave up 7 of the 8 batches and resent them (`fleet.forward.batch_given_up` 7, `fleet.forward.retried`
7, 15 forwards begun): every forwarder but the lane's driver, whose own turn reads first. With A-315 none was given up
and none resent (8 begun), and 7 silences waited for the receiver (`fleet.forward.silence_awaited_receiver`), so the
hold did bring every waiting forwarder to its budget. b's hold took its control shard 5 to 54 ms after the reads were
sent, inside a's half-budget delay, and held it 1.500 to 1.649 s.

## Fix

`forward_lane::Stall`, a pure judge each forwarder holds (A-315):

- The batch is quiet from the latest of its start, its own forwards' progress and, while one of them is still queued,
  its lane's progress (`Lane::progress_ns`: a forward begun, a reply taken, bytes of a reply read), the lane's counted
  only up to the failover bound from the forward's first send. An open forward still counts only its own reply's
  bytes, since other forwards' bytes kept forwards an owner never answered open for good (the 298 forwards of
  2026-11-07).
- Quiet for its stall budget before the bound, the batch is given up only once a driving turn of its lane, begun after
  the silence was first seen, has read the session's own socket empty (`forward_lane::left_empty`: its drive took no
  datagram, or one more receive that does not wait took none; `Endpoint::datagrams_received`, `Lane::drained_turn`),
  looking again every poll interval. The record link's sessions read their own sockets (`Endpoint::client`;
  `Endpoint::reads_its_own_socket`), so that turn has read every datagram that reached the socket before, its settles
  putting the replies into the lane. Turns are ordered by the lane's own count (`Lane::turns_begun`), exact on the one
  shard thread. Progress meanwhile ends the confirmation; a later silence is confirmed afresh. A datagram count, not
  stream bytes: a datagram of acknowledgements alone, or of bytes past a gap, consumes no stream byte.
- A driving turn that finds the peer with no session at all counts as one that read the socket empty
  (`forward_lane::note_sessionless_turn`, counted `fleet.forward.sessionless_turn`): none was established, or the last
  was dropped (judged dead or reset where a borrower returned it, a discovery page that failed, its peer out of contact,
  retired or restarted). Nothing is left to read: a dropped session is never put back (`fleet::return_sessions` drops a
  late return), so no reply on it is settled into the lane, and a forward begun on it is answered unanswered by the next
  session's settle. Only the driver counts it, between its own turns, so no turn of the lane holds the dropped session
  meanwhile. A session lent to another borrower is not counted: that borrower reads the socket, and the lane settles
  what it read when the session comes back (the first proof's failure, below). Without it the form before was held to
  its failover bound behind a stopped owner (the next section).
- Three proofs came first, and the CI runs refused each:
  - The control shard's two waits in its driver since the silence (A-292's receiver rule) alone. That form passed 16
    of 18 runs on GitHub's three runners (`diag-g255-lane-green-1` 9 of 9, run 44253484990; `-green-2` 7 of 9, run
    44254220660), and in the other two (ubuntu-latest and macos-latest of `-green-2`) gave all 7 waiting batches up
    silent, each run's lane having found its session lent to the link once (`fleet.forward.session_out` 1,
    `fleet.lent_to.link` 1; a passing run prints no counters): the shard waits while the link's discovery page holds the
    session, the lane's replies unsettled.
  - A turn begun after those waits whose drive took nothing. A session busy with other forwards' replies is never found
    so, and the 298 forwards' regression, `a_silent_forward_gives_up_while_its_session_carries_others`, failed 2 of 3
    runs on ubuntu-latest (`diag-g255-lane-green-4`, run 44256299594: the silent forward never given up while the busy
    one's replies flowed).
  - A turn begun after those waits that read its queue empty (the drive, then one receive that does not wait). The
    held-origin test passed 16 of 16 on ubuntu-latest and windows-latest (`-green-5`, run 44257173978), but on the
    merged tree the silent-forward test failed 1 of 6 on ubuntu-latest (`-green-6`, run 44258222430): its silence had
    begun its confirmation (`fleet.forward.silence_awaited_receiver` 1) and was never given up while the lane carried
    3,035 other replies, since a control shard that busy rarely waits in its driver.
- Past the bound a quiet batch is given up at once, so a shard that never waits (always busy) and a lane that never
  begins a forward hold nothing past the operator's bound on a failover. A batch whose own forwards progress is never
  given up.

`resolve_and_forward` passes the bound (its failover SLO from the first send) to every send. Counted:
`fleet.forward.given_up_silent`, `fleet.forward.given_up_at_bound` (each of `fleet.forward.batch_given_up`),
`fleet.forward.silence_awaited_receiver` (a silence held for its receiver, the confirmation's non-vacuity count) and
`fleet.forward.held_by_lane` (a queued forward kept past its own budget by its lane).

### Why not loadtest-scale's poll clock

`6da9670d` counted the quiet on a clock each lane's driving turns credited: a turn's wall time, up to the larger of a
10 ms poll pace and the CPU its shard ran meanwhile. Merged into the B13 stack, it was reverted (`b458723c9`): on
`windows-latest` `an_unanswered_forward_is_sent_again_within_the_takeover_bound` failed 5 of 5, its forward never resent
(retried 0), and the record's own starved test 0 of 5, no read served in 11 s (diag-b13-bisect-a, run 44230887960).
By the code, the clock assumed an idle shard's timer fires on time: Windows' completion-port wait keeps the system's
15.6 ms tick (`crates/rt/src/iocp.rs` asks no finer timer), so an idle turn of a 10 ms pace lasted a tick and was
credited the pace, and the clock ran at about two thirds of the wall, too slow to give the delayed forward up before its
1.5-budget answer came. Windows also charges a thread's CPU time a tick at a time. The starved test's own failure was
its depth there: its helper (`tests/common/starve.rs` on that branch) sets the shard's thread to the lowest priority
with boosts off beside time-critical spinners, and Windows' priorities are strict, so no read was served in 11 s. That
test is not brought here; this record's test holds the shard the same way on every platform. The receiver's waits are
counted the same way on every platform, and the judge reads no clock but the wall.

### Found before landing: a lane with no session

The server suite's run of the form without the sessionless turn (`diag-g255-full-5`, ubuntu-latest) failed one of the
154 tests it ran in `fleet`: `a_takeover_adopts_the_newest_head_a_holder_left_out_of_its_pages` (three nodes; the owner
a stopped; b, whose routing forgot both volumes, and c asked for their status until each is served; c must then hold
each volume's second head). Run alone on ubuntu-latest it failed 8 of 8 on that tree, 12 to 13 s a run
(`diag-g255-takeover-green-1`, run 44269560693), and passed 8 of 8 on main, 4 to 6 s a run (`-red-1`, run
44269560657). In each failed run the serve poll lasted 10.01 s, the forward's failover bound, and c then held a third
position of one of the two volumes where the test wants the second. b's counts, 3 of 3 runs alike (`diag-g255-tkc-1`,
run 44271632037): `fleet.forward.given_up_at_bound` 1, `fleet.forward.no_session` 1,
`fleet.forward.silence_awaited_receiver` 1, `fleet.link.out_of_contact` 1 and `fleet.link.stale_return` 1. b's forward
to a was begun on its session to the stopped owner; b's link then found a out of contact and dropped that session's
entry, and a borrower's return of the session found the entry gone and dropped it. From then on the lane had no
session, no turn could read anything, and the forward's silence, first seen after, waited for its bound.

With the sessionless turn the test passed 8 of 8 on ubuntu-latest (`diag-g255-fix-1`, run 44271632046), and b's lane
counted `fleet.forward.sessionless_turn` (26 to 594 a run) and `fleet.forward.given_up_silent`, none given up at the
bound. On windows-latest it failed 3 of 3 without the sessionless turn (12 to 14 s a run, `diag-g255-tkc-1`) and
passed 4 of 4 with it, 5 to 7 s a run (`diag-g255-fix-1`). Sixteen runs a side on ubuntu-latest served the volumes
2.11 to 3.13 s into the serve poll with it, each run giving one forward up silent, and 2.15 to 3.37 s on main, 16 of 16
passing on each (`diag-g255-tkab-fix-1`, run 44272364924; `-main-2`, run 44272413371): the survivor's read is as prompt
as main's.

## Tests

- `forward_lane::stall_tests`: the rule's unit tests
  (`a_silence_is_given_up_only_once_a_turn_begun_after_it_reads_the_socket_empty`,
  `progress_while_a_silence_awaits_its_receiver_starts_the_quiet_again`,
  `a_queued_forward_counts_its_lanes_progress_until_its_failover_bound`,
  `a_silence_past_its_bound_is_given_up_without_its_receiver`,
  `a_batch_whose_own_forwards_progress_is_never_given_up_at_its_bound`), a lane-level test
  (`a_queued_forward_behind_a_moving_lane_is_kept_past_its_own_budget`), and a model test of generated histories
  against the rule read off the whole history (`a_stall_judges_every_history_as_the_rule_reads_it`).
- `crates/server/tests/fleet.rs` `a_held_origin_gives_up_no_forward_whose_reply_came_while_it_was_held`, and for the
  sessionless turn `a_takeover_adopts_the_newest_head_a_holder_left_out_of_its_pages` (red 8 of 8 without it, above).

Red-checks of the rule's unit tests, each a mutation of `Stall` or the lane run in a copy of the tree on GitHub's
ubuntu-latest (`diag-g255-mut-4`, run 44260700876; the first five held their result on the earlier forms too, `-mut-1`
and `-mut-3`):

| Mutation | Unit tests red |
| --- | --- |
| a queued forward's lane progress ignored (`look_in`) | 1: the lane-level test |
| no confirmation, given up at the first look past the budget (main's rule) | 5, the model test among them |
| a confirmation kept across a later silence | 1: `progress_while_a_silence_awaits_its_receiver_starts_the_quiet_again` |
| the lane's progress counted past the failover bound | 2, the model test among them |
| no give-up past the bound unconfirmed | 3, the model test among them |
| a turn begun before the silence counted as its proof | 3, the model test among them |

Unmutated, the ten lane tests pass. `left_empty`, the proof's half that reads the session, is held by the fleet tests,
since it needs a session: without it (the shard's waits alone) the held-origin test failed 2 of 18 runs, and with a
drive that took nothing in its place the silent-forward test failed 2 of 3 (the Fix section).

## Siblings

The other forward tests keep their results with the form that landed (`-green-7`, ubuntu-latest and windows-latest):
`an_unanswered_forward_is_sent_again_within_the_takeover_bound` (the test the poll clock failed on Windows) 6 of 6,
`a_silent_forward_gives_up_while_its_session_carries_others` (the 298 forwards' regression: an open forward is still
given up while other replies flow) 12 of 12, `a_forward_lane_leaves_a_session_that_hears_nothing_for_a_new_one` and
`a_read_through_a_dead_session_keeps_its_place_behind_a_burst` 6 of 6 each. The macOS runners' queue held those jobs;
the first form ran all four 9 of 9 on the three runners (`-green-2`), and on macos-latest the landed form, with A-317
over it, passed the held-origin test 3 of 3 and the left-out-head takeover test 6 of 6 (`diag-g255-land2-1`). With the
sessionless turn (`diag-g255-fix-1`, run 44271632046) the held-origin and silent-forward tests passed 6 of 6 each, the
other three forward tests and `a_forward_waits_for_the_owners_session_while_it_is_out` (a session lent out, which the
sessionless turn must not count) 3 of 3 each, on ubuntu-latest and windows-latest alike.

The left-out-head takeover test reads c's held position once both volumes are served and wants exactly the second
head, while its own words ask only that c hold the second, not the first. Each of the 8 runs that served late (10 s)
read a third position of one volume, so a serve delayed for any reason fails it; what committed the third position was
not traced. Reported, not changed.

Waits on remote hosts that still run on a liveness budget of wall time (R3 class B, GAPS): the location round's wait
for its silent peers (`owner_location::locate_round`), the forwarded owner location's own budget (A-317 owns it), the
leader forward of a region promotion (`verbs::promote_region_here_or_forward`, no path tail), the NFS v4 back channel's
probe and a delegation's recall (`callback::call_on_slot`), a live-shipped entry's exchange (`fleet::live_budget`,
deliberately the window, A-200), and the test-only `SESSION_WAIT_NS`. A forward's own stall budget keeps its liveness
budget too: it is the owner's own bound on answering, the window itself (classes C and D).

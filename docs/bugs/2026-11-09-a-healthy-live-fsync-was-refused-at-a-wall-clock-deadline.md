# A healthy live `fsync` was refused at a wall-clock deadline under load

- **Date:** 2026-11-09; on main 2026-11-17. **Status:** fixed (the FUSE `fsync`, an NFS stable write or `COMMIT`, a
  Shards `FSYNC`, the virtio-fs device's `FSYNC`, WinFsp's held flush), and its siblings, an owner starved past a
  window's end and an owner slow to ask (GAPS). **Design:** §4.10
  status "FUSE `fsync` at durability", "The wait is judged by what it hears" and "Silence is the holders' only when
  they were asked", A-200, A-292, A-301, AC-8.3. **Found by:** raft-7's
  run of the live fleet suite; ordered by the supervisor ahead of older items, since a spurious `EIO` on `fsync` tells an
  application its data is lost. Diagnosed and built by bug-sweep (`agents/bug-sweep-2` 50fa8c56, and the total bound
  and the shared module after it); landed by g244-healthy-fsync-live.

## Description

An `fsync` on a live-shipped volume through a FUSE mount is held until the volume's log is durable where the `fsync`'s
barrier put it: the owner and `f` candidates hold it, the volume's head is placed, and its active set recorded
(`live_ship::durability`). It was answered `EIO` once one wall-clock deadline of the liveness budget (1 s) had passed
since it was held, however its candidates were answering. Under load the candidates and the owner were slow, and an
`fsync` whose candidates were alive and holding its bytes was refused.

## Evidence

- `a_fuse_fsync_on_a_live_volume_answers_only_once_a_candidate_holds_it` (`crates/cli/tests/live_fleet.rs`) answered the
  `fsync` made with every candidate running `EIO`: 1 in 6 runs on main and 2 in 9 on raft-7 at load 36–145 (adacachy).
  In raft's failing run the stopped-candidates `fsync` gave up after 1.60 s against its 1.00 s bound: the owner was late
  too.
- On demand on adacachy2 (Linux 7.3), with the test and 32 spinners pinned to four CPUs and the test's daemons reniced
  to 15: the healthy `fsync` failed in each of the 3 of 10 runs that reached it. The record the fix's instrumentation
  logs at the give-up (`fuse::settle_held` with `live_ship::durability_report`) read, each time: target 1, `durable 1`
  (a candidate held it), `head placed false`. The volume's head, shipped on the membership loop, was still being placed.
- Without spinners, at load 122–144 with the daemons at their usual nice -4, 12 of 12 passed: the spinners at nice 0
  did not compete with them.

## Root cause

`fuse::hold_fsync` set `deadline_ns = now + durable_wait_ns()` and `settle_held` answered `EIO` at `now >= deadline_ns`;
`live_ship::wait_durable` (NFS, Shards) raced its wait with `within(DURABLE_WAIT_NS)`. The constant's own doc says what
it means, "past which a holder that has not answered is not a live one to wait for", but the code measured the whole
wait, so load that stretched the exchanges spent it.

## Fix

- `live_ship::LiveVolume::heard` counts the acknowledgements that moved a volume's durability forward: a holder's
  acknowledged position advancing (`fold_into`), or its head's or catalog's placement gaining an acknowledgement or a
  newer sequence (`fleet::record_acks_in` through `live_ship::heard`).
- `slates_bridge_core::durable_bound::DurableBound`: a held reply's wait in windows of `DURABLE_WAIT_NS`, pure (the
  caller hands it what the volume has heard, `Heard`, and the clock), begun by `live_ship::begin_durable_bound` and
  judged by `live_ship::judge_durable_bound`. A window that ends with `heard` moved starts another; one that ends with
  nothing heard gives up once the daemon's control shard has read what came (the starved-owner sibling below, fixed
  2026-11-17: two of its driver waits since the silence was seen, then one more look). `fuse::settle_held`,
  `live_ship::wait_durable`, WinFsp's `winfsp::settle_held` and the virtio-fs device (`Device::settle_durable`, through
  `BridgeAccess::durable_bound` and `durable_heard`) judge by it. Each extension needs a forward step on the volume's
  own log or registers, so a wait with no live holder ends after one window and its confirming looks, a heartbeat
  apart (`live_ship::DURABLE_LOOK_NS`).
- **A total bound** (the supervisor's landing condition, 2026-11-11; banned item 8): a wait ends at the failover SLO from
  its hold (10 s by default), whatever it hears. Without it, other writes' acknowledgements and placements could keep
  moving `heard` while this reply's own position was never acknowledged, and its wait would never end. Until the
  starved-owner fix it was counted in windows (`failover_slo_ns / DURABLE_WAIT_NS`); it is kept on the clock since then,
  since a window whose silence awaits its receiver lasts longer than a window. The judgment is pure
  (`DurableBound::judge`, over `Heard` and the clock) and unit-tested: a silent window ends a wait once its receiver has
  drained, a window that heard starts the next, and trickling acknowledgements end it `AtTheBound` at the SLO, after
  exactly ten windows (red with the bound removed: a thousand windows, no end). The give-up's record names which ended
  it.
- A give-up logs what durability still lacked: the head, the durable position, the whole entry, each holder's
  acknowledged head, the lanes in flight and their age, the unreachable holders, and each condition of `answerable`.

## Tests

- New: `a_fuse_fsync_on_a_live_volume_waits_out_candidates_that_answer_slowly` (`crates/cli/tests/live_fleet.rs`).
  Both candidates cycle stopped 700 ms and running 100 ms (seven tenths and a tenth of the liveness budget) while a fresh
  live volume's first `fsync` waits; up to six rounds, until one takes longer than the liveness budget. With the old
  single deadline (the window's judgment forced to give up): red 3 of 3, `EIO` after 1.0013–1.0037 s with the head's
  placement in flight. After: 6 of 6 succeeded in 1.50–1.59 s. A shorter running spell (50 ms) refused one `fsync` of
  five after two windows with nothing heard, which is the rule; a longer one (150 ms) let it finish in one spell, under
  the budget, in 2 runs of 5, which tests nothing; hence the shape.
- `a_fuse_fsync_on_a_live_volume_answers_only_once_a_candidate_holds_it`: still refuses with both candidates stopped,
  after one window (1.0004–1.0017 s, 3 of 3, measured before the starved-owner fix; with it, a window and two or three
  looks, 1.2 to 1.3 s by construction), and succeeds healthy and after the resume. The test asserts the `EIO`, not its
  time.
- The rule's own unit tests (`crates/bridge-core/src/durable_bound.rs`): a silent window gives up only once its
  receiver has drained, a window that heard starts the next from the moment it was judged, acknowledgements read after a
  late window's end start the next window, a receiver that never drains holds the verdict only to the total bound, and
  trickling acknowledgements end it `AtTheBound` at the failover SLO from the hold.
- New: `a_guests_fsync_of_a_live_volume_waits_while_its_volume_keeps_progressing` (`crates/bridge-virtiofs/tests/serve.rs`):
  the stand-in volume hears an acknowledgement every half window, three times, and then becomes durable; the guest's
  `FSYNC` is answered `0` after a window and a half, counted as waited.

## Impact

A live volume's `fsync` through a FUSE mount, a stable NFS write or `COMMIT` on it (answered `JUKEBOX`, retried by the
client), and a Shards guest's `FSYNC` on it were refused under load while the volume was becoming durable. The FUSE and
Shards refusals told the application its data might be lost. No acknowledged data was lost.

## Siblings (GAPS 2026-11-09)

- **The virtio-fs device's held `FSYNC`: fixed in the same change.** The rule moved into
  `slates_bridge_core::durable_bound`, pure and shared by every transport; the device holds a `DurableBound` per held
  reply, begun by its owner and judged against the volume's heard count. A guest's `FSYNC` whose volume heard progress
  every half window and became durable after a window and a half is answered `0`
  (`a_guests_fsync_of_a_live_volume_waits_while_its_volume_keeps_progressing`).
- **WinFsp's held flush: fixed 2026-11-17.** It held a `deadline_ns` of one liveness budget from its hold; it now
  holds a `DurableBound` and is judged by the same rule (`winfsp::settle_held`), its give-up logged with
  `live_ship::durability_report`. Owed: a Windows run of `crates/cli/tests/live_winfsp.rs`.
- **An owner starved past a window's end: fixed 2026-11-17.** The first look after a window's end judged it, and an
  owner kept off its CPU across that end ran that look before its control shard had read the acknowledgements waiting
  in its sockets. A silent window is now given up only once the control shard has waited in its driver twice since the
  silence was seen (each wait collects everything readable, and a shard waits only when nothing is ready), at a later
  look, the looks a heartbeat apart (`slates_bridge_core::durable_bound`, `live_ship::heard_now`); the total bound moved
  onto the clock (the failover SLO from the hold), so a receiver that never waits cannot hold the verdict past it. Red
  first: `acknowledgements_read_after_a_late_window_end_start_the_next_window` and
  `a_guests_fsync_is_not_refused_while_its_receiver_has_not_read_what_came`, each answered `EIO` by the rule before.
- **An owner slow to ask: fixed 2026-11-17 (A-301).** The receiver's two waits prove the owner has read what came,
  not that it asked. A fresh volume's head and catalog are asked only by the coordinator's period, behind its other
  rounds, so a period stretched past a window left a window with nothing asked and nothing heard, and the wait gave up on
  holders it had never asked. Each live volume now keeps its owner's oldest unanswered ask
  (`live_ship::LiveVolume::unanswered_since_ns`: a lane opened by `dispatch`, a head's or catalog's placement round
  noted by `fleet::ship_head` on the volume's shard before the round), cleared by anything heard, and a silent window
  is confirmed only once that ask is a window old (`DurableBound::judge`). Red first:
  `a_window_in_which_the_owner_asked_nothing_is_given_up_only_once_its_ask_is_a_window_old`,
  `an_owner_that_never_asks_holds_the_verdict_only_to_the_total_bound` and
  `a_guests_fsync_is_not_refused_while_its_owner_has_asked_its_holders_nothing`, each given up `Silent` (`EIO`) by the
  rule before it. Closed by construction, not yet seen in a give-up record; the record now names the oldest unanswered
  ask's age (`live_ship::durability_report`), so a failing run under load tells it from a holder's silence.

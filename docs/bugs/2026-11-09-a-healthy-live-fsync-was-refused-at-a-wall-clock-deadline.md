# A healthy live `fsync` was refused at a wall-clock deadline under load

- **Date:** 2026-11-09. **Status:** fixed (FUSE `fsync`, NFS stable writes, Shards `FSYNC`); two siblings open (GAPS).
  **Design:** §4.10 status "FUSE `fsync` at durability", A-200, AC-8.3. **Found by:** raft-7's run of the live fleet
  suite; ordered by the supervisor ahead of older items, since a spurious `EIO` on `fsync` tells an application its data
  is lost.

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
- `live_ship::DurableBound`: a held reply's wait in windows of `DURABLE_WAIT_NS`. A window that ends with `heard` moved
  starts another; one that ends with nothing heard gives up. `fuse::settle_held` and `live_ship::wait_durable` judge by
  it. Each extension needs a forward step on the volume's own log or registers, so a wait with no live holder ends after
  one window.
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
  after one window (1.0004–1.0017 s, 3 of 3), and succeeds healthy and after the resume.

## Impact

A live volume's `fsync` through a FUSE mount, a stable NFS write or `COMMIT` on it (answered `JUKEBOX`, retried by the
client), and a Shards guest's `FSYNC` on it were refused under load while the volume was becoming durable. The FUSE and
Shards refusals told the application its data might be lost. No acknowledged data was lost.

## Siblings (open, GAPS 2026-11-09)

- The virtio-fs device's held `FSYNC` and WinFsp's held flush still give up at one deadline.
- An owner starved past a window's end may judge that window before it reads the acknowledgements waiting for it.

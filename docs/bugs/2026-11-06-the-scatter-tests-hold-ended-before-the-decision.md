# The scatter tests' hold ended before the decision (2026-11-06, §4.14, A-216's sibling sweep)

## Description

`crates/server/tests/ack_release.rs` holds the other shard of a two-shard daemon, fills its control channel, and asks
for a scatter (a listing, the grants, the daemon's status). The part for the held shard cannot be sent, so the scatter
must answer `Overloaded { shard }`. The hold was a fixed second. Since `d61c3be2` the part's wait for room ends at a
deadline on the scatter's own shard's clock, and on a busy machine that shard's last try can come after the hold has
ended: the drained channel then takes the part, and the scatter answers in full. The product's decision was right for
what it saw; the test's fixed hold could not stand for "the shard is still held when the decision is made".

## Measurement

- Scheduled CI run 42206043007 on `d61c3be2` (job 127852812757, ubuntu-latest): `ack_release.rs:455`,
  `a_listing_whose_part_cannot_be_sent_is_refused_overloaded`, "list with the other shard's control channel full:
  Listed { volumes: [] }".
- Diagnostics `diag-clients-ack-1` and `-ack-2` (2026-11-07 UTC, ubuntu-latest and ubuntu-24.04-arm, two spinning shell
  loops a CPU, the default test threads as CI runs them, main's file carried beside the fix): main failed 4 of 40 rounds
  (the listing and the status scatter answered in full).

## Root cause

A fixed wall hold against a decision timed by another shard's clock.

## Edits

- `crates/server/tests/ack_release.rs`: `hold_until_answered` holds the other shard until the test has the client's
  answer (bounded by the client's reply deadline), so a part that waited however long still meets a full channel. Used by
  the three scatter tests and `a_status_on_a_starved_shard_is_refused_overloaded_on_time`. Each hold has its own flags:
  the scatter tests run in parallel, and the first version shared one flag, so the first test answered ended the others'
  holds (it failed 10 of 10 rounds on both Linux runners, `diag-clients-ack-1`).

## Tests

- `diag-clients-ack-2`: the fix 20 of 20 rounds green on ubuntu x86_64 and arm64 under two spinners a CPU; main 2 of 10
  red on arm64 in the same lane.
- `diag-clients-frozen-1` (macos-latest, 5 CPUs, two spinners a CPU): the fix and main each 10 of 10 green.
- macOS arm64 (M5 Max, 18 CPUs) at the machine's own load (67–91): the fix 10 of 10 green; the starved status refused in
  0.54–1.31 s.

# A pin held its shard for the whole tree (2026-11-06, §4.4 `pin`, §4.3 "Bounded work everywhere", R9)

Found by agents/loadtest-scale while seeding the kernel tree as the load test's green. Taken with the supervisor's
decision of 2026-11-06 (the GAPS item "pinning a tree of many files", which had no owner).

## Description

`Pin` reads the named subtrees of an overlay (the whole base when none are named) into the store and witnesses them:
work proportional to the tree. The server ran it as one synchronous call on the volume's owner shard
(`verbs::pin` → `Overlay::pin`), so:

- the shard served nothing else until the whole tree was read;
- the client's call gave up at its 1 s reply deadline (`Stalled`), and so did `slates base pin` (CLI: "no reply
  within 1000000000 ns");
- the shard's heartbeat task did not run either, so an anchored daemon pinning a large tree misses its liveness
  budget and is killed as dead.

## Measured

- `Documentation/` of Linux v7.4-rc1 (11,608 files, 59 MB), in-process daemon on the Mac (load 42 to 51): the pin
  took 9,201 ms. The daemon's late-beat line says "its longest poll is 9194 ms", against the anchor's 1,000 ms budget.
- Quiet Linux CI runner (run 42182196730, 4 CPUs): `Documentation/` pinned in 312 ms with a shard poll of 916 ms.
  `arch/` (18,944 files, 127 MB) pinned in 664 ms, with a poll of 1,819 ms.
- The failing test, `crates/server/tests/pin_fairness.rs` (`a_pin_of_a_large_base_never_holds_its_shard`), on a
  one-shard daemon with a 1,000-file base and a prober on the same shard:
  - with the old verb: no probe was answered inside the pin, and the probe waiting for it was refused `Stalled` at
    1 s (a 4,000-file run: the longest probe 1.03 s, the whole pin);
  - with the fix: the pin answered `Pinned { entries: 1000 }` in 8.49 s at load 47 to 63, with 238 probes inside
    it, the longest 318 ms.

## Memory: what a pin holds per file

The GAPS entry recorded memory growing "far faster than its files … past 12 GB" inside the pin. The pin's own memory
was measured with a counting allocator around `Overlay::pin` over the real host (throwaway probes on diagnostics
branch `diag-loadtest-scale-pin-1`). It is linear: heap 3,473 / 2,150 / 1,509 / 1,389 bytes a file for
`kernel/` / `fs/` / `Documentation/` / `arch/` on Linux, and 3,480 / 2,155 / 1,511 on the Mac, with two inode versions
a file and content at the granule-rounded file bytes. The daemon's whole process peaked at 552 MB (`Documentation/`)
and 1.09 GB (`arch/`) on the quiet runner, and the growth came after the pin, during the green's creation and the
harness's walk. The 12 GB figure was not reproduced at 00e5269c. A pin's copy is the volume's content, charged to
its quota as any write is, so it is refused `NoSpace` at the quota
(`a_pin_past_its_volumes_quota_is_refused_typed`).

## Root cause

The pin predates the sliced-work discipline the granted landing (AUD-29-25), the image build (A-160) and the green
checkout (A-201) follow, and it was never moved to it.

## Exact edits

- `crates/vfs/src/base.rs`: `PinWalk`, a pin in progress (the directories still to list, by absolute path, and the
  files listed but not yet pinned, by inode number, so no handle is held between slices). `Overlay::pin_unit` does one
  unit: one file pinned, or one directory listed. `Overlay::pin` is the same walk run to its end.
- `crates/server/src/verbs.rs`: `pin` checks the overlay and the write right at once, then defers its reply to a
  `PinJob` stepped through `image::sliced` (half the live step quantum a slice) in a task of the owner shard, yielding
  between slices. The job publishes the shard before its completion and its reply commit as one atom
  (`image::complete`), as a checkout does. A retry joins it (`image::join_in_flight`).
- `crates/client/src/client.rs`: `Pin` is a deferred reply (`defers_reply`), waited for while the daemon lives.
- Tests: `pin_fairness.rs` (above); `a_pin_stepped_a_unit_at_a_time_among_other_writes_pins_the_whole_base` and
  `a_pin_past_its_volumes_quota_is_refused_typed` in `crates/vfs/tests/coverage.rs`.

## Not changed, reported

- A unit is one whole file of the small class (read whole, under the large-class bound). In a debug build that is
  about 250 µs, already past half the step quantum, so a slice is one file. On a box three times oversubscribed, each
  slice's yield then costs the shard a scheduler quantum (macOS's zero-timeout `kevent` nap, A-218): about 25 ms a
  slice at load 63.
- The job's last step publishes the whole shard in one call, as a checkout's does. That call grows with the shard's
  state, and it is one candidate for the probe tails inside the pin (p99 269 ms at load 47 to 63).

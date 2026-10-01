# A sized landing volume reserved the whole version slab (landing_fairness on the macOS runner)

**Date:** 2026-10-01. **Area:** `crates/server/tests/landing_fairness.rs`; the admission it exposed,
`verbs::inode_allowance` (§4.2). **Found by:** the macOS lane of CI run 36890745801, red after `9ab62fa`.

## Description

Two of the three landing-fairness tests failed on the macOS runner. Each failed at its first calibration
landing: 500 files written (3,890 bytes referenced), then the landing refused `NoSpace`, with refusals
`landing.volume_refused: 1, no_space: 1`. The third test, whose volumes are Bounded at 1 MiB, passed in the same
run. This machine and the Linux container passed.

## Root cause

Shown by reproducing it, not inferred:

- The sized volumes' maximum was `MOST_FILES × 2 × 16 KiB` = 262,144,000 bytes. It assumed each tiny file is
  charged a 16 KiB window. In fact a file's few bytes are kept inline and charged only themselves
  (`Volume::write_charge` returns early when `end <= inline`).
- A volume's inode allowance is its quota over `size_of::<Inode>()` (288 bytes), capped at the version slab less
  its headroom (`verbs::inode_allowance`). That is 910,222 slots. The runner's derived slab is 828,504
  (`store.max_inodes` in the failure's printed configuration; 7.5 GB, one shard). So the first sized volume
  reserved every slot.
- The landing snapshots the volume and rebases each landed file. Keeping the snapshot's version of a file
  charges one retained version slot (`Volume::make_current_inode`, `store.versions.charge_retention(1)`). No slot
  was left, so it refused `NoSpace`.
- Reproduction: the test run here with `config.store.max_inodes = 828_504` failed exactly as on the runner
  (`landing_fairness.rs:303`, the same refusals).
- Ruled out, by experiment: the memory-pressure hold. A first landing under a hold of the whole reserve
  succeeded here.

## Exact edits

- `crates/server/tests/landing_fairness.rs`: `SIZED_VOLUME_BYTES` is `MOST_FILES × 2 × size_of::<Inode>()`: one
  inode's worth of quota per file, doubled for a landing made again. That is an allowance of at most 16,000
  slots. `LARGEST_PAGE` is removed.

## Proof

With the runner's slab forced (`max_inodes = 828_504` in all three tests, a temporary edit), the suite passed 3/3
in 45.2 s. It also passed unforced, 3/3 in 34.4 s.

## Open (recorded for Ada in GAPS, §4.2)

The product admitted a volume whose inode allowance left the shard no version slot. That volume's own landing,
and any snapshot-then-modify on it, then refuses `NoSpace`. This is a further consequence of the open question
already recorded: should a Dynamic volume reserve version slots for its whole maximum up front?

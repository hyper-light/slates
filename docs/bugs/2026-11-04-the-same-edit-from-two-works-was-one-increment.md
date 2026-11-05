# The same edit from two works was one increment

Found 2026-11-04 by the edit-and-merge latency harness's oracle (`crates/client/examples/merge_bench.rs`, its
`identical` form, on its first run). Design: §4.16 data model (`Increment.id`), §4.16 "Commit" (an id already in
`seen` returns its original result).

## Description

The design names an increment by `blake3(work_volume, base, post_state, ops_doc, filter)`. The server's
`build_increment` (`crates/server/src/verbs.rs`) hashed only the ops document and the post-state. Two different works
that made the same edit from the same base therefore built one identity, and the engine's retry cache
(`Green::submit`, `accepted`) answered the second work with the first work's version without judging it.

That holds whatever the green did between them. In the failing test, works `first` and `late` are both based on
version 1. `first` writes `XXXX` over `f[0..4)` and is accepted at 2. A third work writes `ZZZZ` there, accepted at 3.
`late` then writes `XXXX` over `f[0..4)`. The design's verdict is a conflict: the range changed since `late`'s base
and the bytes differ. The server answered `Accepted(2)`, and the head kept `ZZZZ`. The agent was told its edit had
merged when the green does not hold it.

The same collision answers an identical accept with an older version: a work based on 3 writing the head's own
bytes over that range composes the third work's document and post-state, and was answered `Accepted(3)`, the third
work's version, instead of an identical accept of its own at 4.

## Root cause

The identity left out the two inputs that make an increment one work's claim from one base. The engine's cache is
correct for a true retry (the same work's increment, resent after a lost reply or a daemon restart), and that is the
only case it may answer.

## Impact

Every platform, every green with more than one work. A conflict was reported as an accept whenever two works made a
byte-identical edit from one base with a differing change to that range merged between them, and an identical
accept was reported under another increment's version. No data in the green was wrong: the head kept the merged
bytes. What was wrong was the answer to the agent, which would go on as if its edit had merged.

## Fix

`build_increment` hashes the work's volume id and its base version (both fixed width, so the concatenation is
unambiguous) before the document and the post-state. A retry of the same work's submit from the same base keeps its
identity, so the exactly-once retry is unchanged; the work id is the catalog's durable id, so a retry across a
daemon restart keeps it too. The filter is not yet a part of a work (no exclusion filter exists), so it adds nothing.

## Failing test first

`crates/client/tests/merge_identity.rs` (`the_same_edit_from_two_works_is_two_increments`), on adas-macbook-pro-2
(macOS arm64, 18 cores, load about 10 to 20), 2026-11-04, `cargo +1.98.1 test --release -p slates-client --test
merge_identity`:

- tree `76b450a1` plus the test: FAILED, `a write over bytes the green changed since the base answered Accepted(2)`;
- with the fix: `test result: ok. 1 passed`.

## Siblings

- `rebase` builds its increment through the same `build_increment` and takes the fix with it.
- The engine's `accepted` map is keyed by the same identity and is bounded by the chain (one entry per committed
  version, `crates/merge/src/engine.rs`); the fix changes its keys, not its bound.

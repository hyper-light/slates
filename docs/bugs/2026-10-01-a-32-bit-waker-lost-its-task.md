# A 32-bit waker lost its task

**Date:** 2026-10-01. **Area:** `slates-rt` (`waker.rs`, `shard.rs`, `registry.rs`, `futures.rs`,
`readiness.rs`), `slates-mem` (`handle.rs`), CI (the i686 lane). **Found by:** the new i686 lane (AUD-29-32),
running `slates-rt`'s unit tests as 32-bit code in the `linux/386` image. **Status:** fixed.

## Description

`waker::tests::a_waker_carries_its_word_and_clones_for_free` failed on i686: the word a waker returned was
not the word it was made with.

A task's waker carries its packed handle word (`slates_mem::Encoded`) in the `RawWaker`'s data pointer. The
word is 64 bits: 16 shard, 24 slot and 24 generation bits. On a 32-bit target the pointer is 32 bits, and
`raw()` converted with `usize::try_from(word).unwrap_or(0)`, so every word above `u32::MAX` silently became
0. That is every task on a shard other than 0 (the shard sits in the top bits). Such a wake named shard 0,
slot 0, generation 0, and reached the wrong task or none. The async runtime was therefore not correct on any
32-bit target. It was never noticed because no lane ran `slates-rt` as 32-bit code.

## Root cause

The waker's design assumed a 64-bit data pointer and never stated it. The `unwrap_or(0)` turned an
impossible conversion into a silent wrong value, where a refusal was owed.

## Fix

- **Narrowing the word cannot work.** No 32-bit encoding names every task lifetime in a long-running
  process. Shrinking the fields means retiring slots after their last generation: 13-bit slots and
  generations exhaust a shard after about 67 million task lifetimes, roughly two hours at 10,000 spawns a
  second.
- **A 32-bit waker names the slot.** Its data pointer carries the shard (8 bits) above the slot (24 bits),
  and decodes to the slot-only word: that shard and slot with the reserved generation
  `Encoded::ANY_GENERATION`.
  - Task arenas now issue generations only up to `Encoded::TASK_GENERATION_LIMIT` (one below the top), so
    the reserved generation never names a task. The registry's retirement check uses the same limit.
  - The cross-shard wake handler accepts a slot-only word as a wake of the slot's current occupant. That is
    what the same-shard wake path already did on every width: it pushes the slot, with no generation check.
  - A stale 32-bit waker can therefore poll a newer occupant spuriously. The `Waker` contract allows that,
    and the runtime's futures already tolerate it, because same-shard wakes have always behaved this way.
- **A task's identity is never taken from its waker.** `Sleep` (timers and the full-wheel wait) and the
  driver's readiness futures used the waker's word as the task identity they register. On i686 that put the
  slot-only word into the timer wait queue, which refused it (`StaleTask { generation: 16777215 }`). They now
  use `waker::polling_task`: the shard's current task, when the waker is that task's own. That is the exact
  word on every width.
- **The shard bound fits the pointer.** On a 32-bit target the registry holds at most 256 shards
  (`registry::MAX_SHARDS` = `waker::MAX_SHARDS_32`), the shards the pointer's top 8 bits can name. A further
  shard is refused `TooManyShards` at registration. 64-bit targets keep 1,024.
- **No silent fallback is left.** Each width has its own encode and decode, and each conversion cannot fail
  by construction.
- **64-bit wakers are unchanged:** they carry the full word, and the cross-shard generation check still
  holds.

## Tests

- `tests/foreign_wake.rs`, `a_waker_fired_from_another_thread_wakes_the_task_it_was_made_for`: two tasks
  park on a runtime's second shard. Waking only the second from an outside thread finishes the second and not
  the first; the first's own waker then finishes it.
  - **Before the fix** (the HEAD tree with this test, `linux/386`, 2026-10-01): it failed with
    `Err(Timeout)`, so the wake was lost.
  - **After:** it passes on macOS (arm64) and on i686.
- The waker unit test states each width's carried word.
- The retired-slot test (`tests/reclaim.rs`) primes the slot at `TASK_GENERATION_LIMIT`.
- `slates-rt` on macOS: every binary passes. Miri: 25 passed, 3 ignored (`cargo +nightly miri test -p
  slates-rt --lib`).
- **`slates-rt` and `slates-mem` under i686 emulation** (an arm64 Mac running `linux/386` through Docker,
  2026-10-01): every word, wake, timer and arena test passes. Six timing or measurement checks fail only
  there:
  - zero-timeout waits counted as voluntary switches;
  - a 5 ms sleep raced against debug-build steps (`overrun`, `differential`);
  - preemption attribution (`wake_estimate`);
  - the machine profile's 20 ms wake probe (`MeasurementTimeout`, `placement` and the `mem` region test).

  None involves a word or a width. GitHub's x86_64 runner executes the `linux/386` image natively, so the CI
  lane, which now runs both crates whole, decides them.

## Siblings found and fixed

- **The committed i686 lane never ran its tests.** It used `bash -lc`, and a login shell sources the image's
  `/etc/profile`, which resets PATH without cargo (reproduced locally: `timeout: failed to run command
  'cargo'`). The lane now invokes cargo directly.
- **Two trees in one target directory.** Running the pre-fix tree and the fixed tree in one target directory
  under the same `/src` path made cargo reuse the other tree's crates by modification time (the known trap).
  The packages were cleaned between runs.

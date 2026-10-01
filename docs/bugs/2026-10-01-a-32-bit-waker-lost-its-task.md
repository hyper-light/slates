# A 32-bit waker lost its task

**Date:** 2026-10-01. **Area:** `slates-rt` (`waker.rs`), `slates-mem` (`handle.rs`). **Found by:** the new
i686 lane (AUD-29-32), running `slates-rt`'s unit tests as 32-bit code in the `linux/386` image. **Status:**
open; the fix is designed below and lands in its own change.

## Description

`waker::tests::a_waker_carries_its_word_and_clones_for_free` fails on i686: the word a waker returns is not
the word it was made with.

A task's waker carries its packed handle word (`slates_mem::Encoded`) in the `RawWaker`'s data pointer. The
word is 64 bits: 16 shard, 24 slot and 24 generation bits. On a 32-bit target the pointer is 32 bits, and
`raw()` converts with `usize::try_from(word).unwrap_or(0)`, so every word above `u32::MAX` silently becomes
0. On i686 most wakers then name shard 0, slot 0, generation 0: a wake reaches the wrong task or none. The
async runtime is therefore not correct on any 32-bit target. It was never noticed because no lane ran
`slates-rt` as 32-bit code.

## Root cause

The waker's design assumed a 64-bit data pointer and never stated it. The `unwrap_or(0)` turned an
impossible conversion into a silent wrong value, where a refusal was owed.

## Designed fix (owed)

- **Narrowing the word cannot work.** No 32-bit encoding can name every task lifetime in a long-running
  process. Shrinking the fields means retiring slots after their last generation: 13-bit slots and
  generations exhaust a shard after about 67 million task lifetimes, roughly two hours at 10,000 spawns a
  second.
- **A 32-bit waker names the slot instead:** its shard and index. A wake through a stale waker then polls the
  slot's current occupant spuriously, which the `Waker` contract allows ("a future must be prepared for
  spurious wake-ups").
- **The cost of that:** an audit of every custom `Future` in `slates-rt` (timers, channels, the driver's
  readiness futures) for tolerance of spurious polls, and a 32-bit-only wake path in the registry.
- **What stays the same:** 64-bit targets keep the full word and their generation check.
- **The silent fallback goes.** `unwrap_or(0)` is replaced by a conversion that cannot fail by construction
  on each width.

## Until then

- The i686 lane runs `slates-wire`, `slates-archive`, `slates-ipc`, `slates-db` and `slates-vfs`. `slates-rt`
  and `slates-mem` join with the fix.
- `slates-mem`'s region test and `slates-machine`'s probes also fail under i686 *emulation* on this
  machine: the wake probe times out (`MeasurementTimeout { probe: "wake" }`). That is consistent with
  qemu-user's slowness rather than a 32-bit defect, but it is unverified until a native i686 run.

# Generation wrap revived stale handles; packed wakes aliased at 24 bits (2026-09-30, AUD-29-11)

Contracts: §4.2 ("generation exhaustion [is] checked and refuse[s] before mutation"; the refusal list
names `GenerationExhausted`), §4.3 (the wake word, registry slot reuse). Reported by the 2026-09-29
audit.

## Description

1. **The slab wrapped.** `Slab::remove`/`discard` advanced a slot's generation with `wrapping_add`.
   From `u32::MAX` one remove took it back to zero, and an ancient generation-zero handle resolved to
   the new occupant.
2. **The runtime masked.** Its wake word (`Encoded`: shard 16 | slot 24 | generation 24) packed a
   masked generation, and the shard compared 24 masked bits. After 2^24 reuses of one task slot, a stale
   waker named the new task, long before the 32-bit wrap.
3. **The registry wrapped too.** Its slot word (even live / odd free) used `wrapping_add`, and a shard
   slot whose arena had spent its word's generations could still be handed to a new runtime, whose tasks
   would start past the word's width.

## Root cause

Every representation of a handle counted its generation modulo its width, and nothing retired an
identity whose space was spent. The comment that called the event "counted" did not make a masked
equality detect it.

## Impact

A stale handle, waker, cancelled task's word or recycled shard id could name a live object after enough
reuse of one slot. The slab needs 2^32 reuses; the runtime 2^24, which is about a minute for a hot slot
spawning at the measured rates. It was never observed in a run. It is the class of silent aliasing the
generational handle exists to prevent (§4.2).

## Exact edits

- **`crates/mem/src/slab.rs`**
  - A slab has a generation limit (`with_generation_limit`; `u32::MAX` unless a packed representation
    is narrower).
  - A slot freed at the limit is **retired**: `Body::Retired`, never on the free list, never reissued,
    no live generation (`generation_at` is `None`).
  - An insert that finds no slot because some retired refuses `GenerationExhausted { index }`, never
    `SlabFull`.
  - A base past the limit issues nothing. `has_room` counts retired slots. `generation_high`
    saturates.
  - `remove` and `discard` share one `vacate`.
- **`crates/mem/src/handle.rs`.** `Encoded::MAX_GENERATION`. `pack` refuses a generation the word
  cannot carry, never truncating it. `matches` compares exactly.
- **`crates/rt/src/shard.rs`.** The task arena's limit is `Encoded::MAX_GENERATION`. Wake, poller
  registration and poller retention compare exactly (`GENERATION_MASK` is gone).
- **`crates/rt/src/registry.rs`.** `register` skips a slot whose arena generations are past the word's
  limit, or whose own slot word would wrap. The slot is retired for the process.

## Evidence

- **Red.** On the tree before the change (`54a426d`), the audit's witness as a unit test failed: "an
  ancient generation-zero handle resolved to the new object".
- **Green** (no million-iteration runs; each boundary is primed directly):
  - `slab::tests::a_slot_at_the_top_generation_retires_rather_than_wrapping` (by remove and by discard);
  - `a_limited_slab_retires_at_its_representations_limit`;
  - `handle::tests::a_generation_the_word_cannot_carry_is_refused_not_truncated`;
  - `crates/rt/tests/reclaim.rs::a_shard_slot_whose_wake_generations_are_spent_retires_and_is_not_reissued`.
    A shard slot primed to the word's last generation issues it once, retires the task slot (the next
    task takes another slot), refuses the stale waker in the arena, and is never handed to the next
    runtime.
- **Suites.**
  - mem and rt pass on macOS and Linux (io_uring).
  - The server daemon suite passes (17).
  - Clippy is clean on macOS, Linux and the Windows cross-lint.

## Open

- A retired shard slot is retired for the process. At `MAX_SHARDS` slots, each spending 2^24 task
  generations, that is far past any process lifetime. It is not unbounded growth.
- Handle representations outside `slates-mem` and `slates-rt` (volume and snapshot ids, attachment ids,
  request ids) are AUD-29-11's siblings. Request ids are AUD-29-21's own finding.

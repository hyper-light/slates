# A one-slot multi-producer ring overwrote unread work, and no ring enforced its single owners (AUD-29-33)

## Description

- **The one-slot overwrite.** `MpscRing::new(1)` was admitted. Pushing 11 and then 22 without a pop
  returned `Ok` both times, and the following pop returned `None`: the second push had overwritten the
  first word, unread, and moved the sequence past the consumer's expectation.
- **Unenforced single owners.** `MpscRing::consumer(&self)` and `SpscRing::split(&self)` could be called
  any number of times, so safe code could mint a second consumer, or a second producer, of a ring whose
  protocol assumes one of each. The runtime itself called `split()` on every send and `consumer()` on
  every drain.

## Root cause

- **Why capacity one breaks the queue.** In Vyukov's bounded queue a slot's sequence is its position + 1
  when full and its position + capacity when free for the next lap. At capacity one these are the same
  value, so a producer took a full slot for a free one.
- **Why the owners were not enforced.** The halves borrowed the ring and were `Sync`. Only convention kept
  them unique.

## Fix

- **Smallest geometry.** `MIN_CAPACITY = 2` (derived in `mpsc.rs`), and a one-slot ring is refused
  `BadCapacity`.
- **Claimed halves.**
  - `MpscRing::consumer()` returns `Option` and claims the half, released on drop.
  - `SpscRing::split()` returns `Option` and succeeds once in the ring's life.
- **One thread per half.** Every half is `Send` but not `Sync` (a `PhantomData<Cell<()>>`), so `push`
  and `pop` take `&self`.
- **The runtime holds its halves.**
  - `connect_pairs` splits each pair ring once and gives the producer to the source seed and the
    consumer to the target seed.
  - `ShardContext::build` claims the entry's foreign-ring consumer and keeps it in `ShardInner`.
  - Either claim failing is the new refusal `RtError::RingClaimed`.

## Evidence

- **Tests.**
  - `a_one_slot_ring_is_refused`.
  - `every_admitted_geometry_refuses_when_full_and_keeps_fifo_across_wraps`: capacities 2–64 are filled,
    the extra push is refused with its word handed back, the oldest word is kept, and FIFO holds over
    three laps.
  - `the_consumer_is_claimed_by_one_holder_at_a_time` and `a_ring_splits_once`.
  - Compile-fail doctests prove neither half can be shared across threads (E0277).
- **Loom.** The loom models, including the smallest geometry (two slots, lapping), pass:
  `RUSTFLAGS="--cfg loom" cargo test -p slates-mem -p slates-rt -p slates-ipc --lib --release loom`.
- **Instruction counts.** Recorded in `docs/wip/BENCHMARKS.md` (Single-owner ring halves).

## Follow-up: a test consumer outlived its ring (Miri, CI job 109811181968)

- **The failure.** Miri reported a use-after-free in `registry::tests::a_full_foreign_ring_spins_and_counts_without_losing_the_word`:
  the test's claimed `Consumer` was still alive when `unregister` freed the entry and its ring, and the
  consumer's drop (which releases the claim) wrote into the freed ring.
- **The sibling.** `a_wake_from_a_foreign_thread_lands_in_the_target_ring` had the same shape.
- **The fix.** Both tests drop the consumer before `unregister`.
- **Production is not affected.** Its only holder is the shard context, which drops its consumer when the
  context is reclaimed, before the slot is unregistered. The contention test's consumers live inside a
  scoped `with_entry` closure, and the parking tests' rings are leaked by design.
- **Miri.** `cargo +nightly miri test -p slates-rt --lib registry::` gives 7/7, and
  `-p slates-mem --lib -- ring mpsc` gives 9/9, both with no undefined behaviour.

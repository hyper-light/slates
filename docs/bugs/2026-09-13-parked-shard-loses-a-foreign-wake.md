# A parked shard can lose a wake sent from a foreign thread (2026-09-13)

**Found by:** the loom model of the kick-if-parked protocol, written for AC-0.7 (`crates/rt/src/parking.rs`,
`a_word_published_while_the_shard_parks_is_never_lost`), on the protocol exactly as `registry::send_foreign`,
`registry::send_control` and `shard::park` ran it at `d0a73dc`.

**Command:** `RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom cargo test -p slates-rt --release --lib
parking::loom_tests -- --nocapture --test-threads=1`

**Output (unfenced protocol):**
```
thread 'parking::loom_tests::a_word_published_while_the_shard_parks_is_never_lost' panicked at
  loom-0.7.2/src/rt/execution.rs:216:13:
deadlock; threads = [(Id(0), Blocked(Location(None))), (Id(1), Terminated)]
```
```
loom: parking: one sender against one parking shard: failed at interleaving 1 (preemption bound 2)
```
Thread 0 is the shard, blocked in its driver wait; thread 1 is the sender, finished, its word in the ring
and its kick skipped. loom reaches it in its first execution: nothing happens-before links the shard's
`SeqCst` store to the sender's `SeqCst` load, so the total order may place the load first, and it does.

## Description

A shard about to wait in its driver announces it (`parked = true`, `SeqCst`) and then re-checks its
inboxes; a sender from another thread publishes its message and then reads the announcement (`SeqCst`),
kicking the driver only when the shard announced parking (§4.7 "Wake strategy": a message to a spinning
shard costs no syscall). Each side writes and then reads — the store-buffering shape — and the comment on
`kick_if_parked` argued that both being `SeqCst` made "a lost wake need both to miss, which the total order
forbids".

That argument covers only the flag. The sender's message is published by the multi-producer ring with a
`Release` store of the slot's sequence (`slates_mem::MpscRing::push`), and the shard's re-check reads it
with an `Acquire` load (`MpscRing::is_empty`). Neither is part of the announcement's `SeqCst` total order, so
the C++20 model allows: the sender's `SeqCst` load of `parked` reads `false` (it precedes the shard's store
in the total order), and the shard's `Acquire` load of the sequence reads the old value (nothing
happens-before it). Both miss. The shard waits for a kick that never comes; the word sits in its ring.

x86-64 realizes this ordering: a `Release` store and a `SeqCst` load both compile to plain `mov`, and the
store buffer lets the load execute before the store is globally visible (only a `SeqCst` *store* gets a
locked instruction). arm64 does not: a store-release followed by a load-acquire is kept in order (RCsc
`stlr`/`ldar`), which is why this laptop never showed it. The Linux fleet hosts and the CI runners are x86.

The same argument applies to `send_control`: the control flag is stored `SeqCst`, but the shard reads it
`Acquire` after its `SeqCst` announcement store — allowed to miss under the abstract model, though the
hardware mappings on both architectures happen to order it. The one fix covers both.

## Root cause

The two sides' reads are ordered against each other's writes only through the announcement flag; the
message's own publication takes no part in that order. A correct store-buffering protocol needs a `SeqCst`
fence between the write and the read on *both* sides (C++20 `[atomics.order]`, the fence–fence rule: with
a `SeqCst` fence after the write on one thread and before the read on the other, whichever fence is first
in the total order, the later thread's read observes the earlier thread's write), which holds whatever
ordering the publication itself used.

## Impact

A wake sent through `registry::wake` from a thread that is not a shard (`send_foreign`: bridge threads —
WinFsp dispatches on its own threads — the confirmation surface, tests, and a shard without a pair ring)
can be lost when the target shard is entering its park at that instant. The task stays asleep until
something else wakes the shard: a completion, a timer deadline, or a pair-ring kick (which is
unconditional). On a quiet shard with no deadline armed that is an indefinite stall of that task. The
shard-to-shard pair rings are not affected (`send_to` always kicks; that is the cost the protocol was
built to save, so the saving was never taken there). The client command rings are a separate protocol
(see the sibling note below).

## Fix (`crates/rt/src/parking.rs`)

The protocol is now one seam, `Parking`, that both `registry` (the senders) and `shard::park` (the
shard) drive, so the loom model runs the shipped code:

- `kick_if_parked`: `fence(SeqCst)` before the `SeqCst` load of the announcement.
- `park_unless_pending`: `fence(SeqCst)` after the `SeqCst` announcement store, before the re-check.

Cost: one full fence per foreign send (a cross-thread wake, already a syscall-class path) and one per
park (the shard is about to make a syscall). The fenced model passes: 27 interleavings explored under
the two-preemption bound, some kicked, some skipped the kick, some waited (all three counted).

One behavioural change rides the seam extraction: the announcement is withdrawn after the driver's
completions are queued rather than immediately after `wait` returns — a window of a few hundred
nanoseconds in which a sender may kick a shard that is already awake (a spurious, harmless kick).

## Sibling sweep

- `registry::send_to` (pair rings): kicks unconditionally; no protocol, no bug.
- `shard::harvest_io`: never announces parking; not part of the protocol.
- `registry::send_control` + `shard::drain_control`: the same shape; covered by the same fences.
- **`slates-ipc`, reply direction** (`DaemonEnd::reply` → `ClientEnd::wait`): the client stores its
  parked flag (`Release`), reads the wake word, re-checks the slot, then `futex_wait(word, expected)`; the
  daemon writes the reply, bumps the word with an `AcqRel` RMW, then reads the flag (`Acquire`). Safe by
  construction: the RMW is a full barrier on x86 and a release before the flag's load-acquire on arm64,
  and the kernel compares the word under its own lock, so a client can never block on a word the daemon
  already bumped. Reasoned, not model-checked (the rings live in a shared-memory region with real
  futexes, which loom cannot instrument).
- **`slates-ipc`, request direction** (`ClientEnd::send` → the daemon's `mark_parked`): the same shape
  with only a `Release` store and an `Acquire` load on each side (`endpoint.rs` `send`: push the slot,
  then `daemon_parked.load(Acquire)`, doorbell only if set; `daemon.rs` serve loop: `mark_parked(true)`
  stores the flag `Release`, then `futures::idle()`), and rt's `park` re-checks only its own rings and
  control flag — the client command rings are re-checked after the announcement only by the idle spin
  (`spin_until_work` → `wake_ready_pollers`), which runs only while a client is active and `spin_ns > 0`.
  Under the abstract model a request written between the loop's last poller check and the flag's
  visibility can find the flag unset and ring no doorbell, and the shard parks over it until its next
  deadline or kick. **Not fixed here** (no failing test yet; the fix is a `SeqCst` fence after the slot
  push in `send`, a `SeqCst` fence after `set_parked(true)`, and `park`'s pending check extended to
  `wake_ready_pollers`, which is a by-use test of "a poller that becomes ready between the loop's last
  look and the park is woken without a driver wait"). Reported to Ada for a decision.

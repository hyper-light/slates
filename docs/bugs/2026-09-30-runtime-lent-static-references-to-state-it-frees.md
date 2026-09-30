# The runtime lent `'static` references to state it frees (AUD-29-08)

## Description

Safe runtime APIs returned `&'static` references into memory that an owner later frees. Safe code could
hold such a reference past the free and read freed memory; nothing in the type system stopped it.

- **`LocalRuntime::context()` and `SimRuntime::context()`.** Both returned `&'static ShardContext`.
  Dropping the runtime frees that context.
- **`ShardContext::keep(value)`.** It returned `&'static T` into a box that is dropped with the context.
  A `Sync` value's reference could be sent to another thread, or held on this one after the runtime
  dropped.
- **`registry::reclaim_context`, `unregister`, `entry` and `lend_pair_ring`.** All were public and safe.
  - `unregister(shard)` took any shard number and freed the entry that a running shard's context holds
    unguarded.
  - `entry` and `lend_pair_ring` lent `&'static` references into that entry.
- **The current-context cell.** It was published by a step and left published after the step returned.
  So after a bare `step()` the thread's next `with_current` could lend a context whose owner had since
  dropped.
- **Siblings found in the sweep.**
  - The transport's thread-local `DEMUXES` table held `&'static Demux` references.
  - The fleet's `PeerDriver`, identity and resolver were all `&'static` into kept boxes.

## Root cause

- **The old argument.** Every `&'static` rested on one claim: the context outlives every task, and only
  tasks hold these references. That claim is true, but the types never confined the references to
  tasks.
- **A `'static` reference is not bounded by anything.** It can be stored in a thread-local, leaked,
  sent to a thread, or held by the owner's own caller past the drop.

## Fix (no `Arc`, no leak)

- **Contexts are lent for a borrow.**
  - `LocalRuntime::context(&self) -> &ShardContext` and `SimRuntime::context` are bound to the owner's
    borrow.
  - `with_current` lends inside a closure. The cell is a pointer set only by an `Entered` guard that
    borrows the context: `run`, `run_until_idle`, `step` and `park` enter it, and the guard restores the
    enclosing span's shard.
- **Kept values are handles.** `keep` and `keep_with` return a `Kept<T>` handle: the registration's
  `SlotHolder` (slot and generation, never repeated in a process) plus an index.
  - The value is reached only through `Kept::with`, which checks that the running context is the
    handle's, or through `Kept::with_in(&context)`.
  - The lend is closure-scoped, and the kept values are borrowed shared for its span.
  - A keep inside a live lend is refused as `RtError::KeptInUse`.
- **Reclamation is private to the owners.**
  - `reclaim_context`, `unregister`, `entry`, `lend_pair_ring`, `ShardContext::build` and
    `ShardSeed::register` are crate-private.
  - The public `register` returns a `Registration` token whose drop retires the slot. Its holder never
    built a context over it, because contexts come only from a runtime's own seeds.
- **Transport.** `DemuxId` is `Kept<Demux>`, and the thread-local table is gone.
  - `DemuxId::run` routes through the handle and awaits the socket's readiness, now a `'static` future
    over the descriptor (`UdpSocket::readable` with `use<>`), outside the borrow.
  - `DemuxId::accept` yields `Result<Endpoint, EndpointError>` (`Closed` once unreachable).
- **Fleet.**
  - `PeerDriver` is a `Copy` value of handles (identity, name, resolver) plus two plain values.
  - Dials resolve the identity at the moment of use.
  - A lookup copies the resolver configuration before it awaits.
  - An unreachable kept value is counted as `fleet.kept.unreachable`, a tripwire.
  - An unreachable demultiplexer in the status observation is `State(Absent)`, never a zeroed row.

## Evidence

- **Compile-time.** Four `compile_fail` doctests, each rejected with its named error code:
  - a `LocalRuntime::context` lend returned past the runtime (E0515);
  - a `Kept::with` lend returned from its closure (E0521);
  - a `with_current` lend returned from its closure (E0521);
  - `registry::unregister` called from outside the runtime (E0603).
- **By use.** `crates/rt/tests/ownership.rs` runs on the simulated driver, and CI's Miri lane now
  runs it:
  - a handle held past its runtime's drop answers `None`, on this thread and another, and the value
    dropped exactly once;
  - a handle resolved on another live runtime, or on a later runtime that took the freed slot, answers
    `None`;
  - the current shard is `None` after a bare step. This test is red when the step leaves its guard
    published (the old behaviour) and green with the fix;
  - a keep inside a lend, and a keep inside another keep's build, are `KeptInUse`, and the refused build
    is dropped;
  - a nested runtime run inside a task restores the enclosing shard.
- **Suites.** On macOS:
  - `slates-rt`, `slates-transport` and `slates-server` (full) pass, and the CLI flows pass 13/13 with
    `SLATES_TEST_CLI=1` (41.49 s);
  - `cargo clippy --workspace --all-targets -D warnings` is clean.

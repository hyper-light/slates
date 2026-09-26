# Windows rings and claims used the word wake, which is unsupported there

Date: 2026-09-26. Contracts: §4.7 (rendezvous, doorbell, wake strategy), D-10 (`WaitOnAddress` is
process-local, so the cross-process wake on Windows is a named Event), CLAUDE.md banned item 9 (no
swallowed error). Found by CI run 36202635768 (`85a29fb`). It is the first run whose Windows tests ran
as separate steps (`b2b6389`); before it, one combined step hung for six hours and discarded its log.

## Symptom

`cargo test -p slates-ipc --test rendezvous`, native Windows and Windows nightly:

```
a_client_process_refused_at_the_bound_is_told_so_typed: Unsupported { feature: "the cross-process wake word (a named Event per client on Windows)" }
a_client_process_connects_and_completes_a_round_trip: HandoffLost { client_id: 1, cause: Unsupported { … } }
```

## Root cause

`wake::wait` and `wake::wake_one` are the Linux and macOS wake. On Windows they return `Unsupported`
by design (`crates/ipc/src/wake.rs`); there the wake is a named auto-reset Event. Four sites called
them anyway on Windows:

1. **Serving a claim** (`Listener::accept_one`, READY and REFUSED). The daemon woke the claimant with
   `wake_one(word)?`, which failed. So every claim on Windows failed after its slot was answered. The
   Event signal beside it came second and ignored its own result (`let _ =`).
2. **A client's doorbell ring** (`Doorbell::ring`). This ran on every send to a parked shard and
   returned `Unsupported`, so the request failed.
3. **The claim's doorbell ring.** It swallowed the `Unsupported` (`let _ =`).
4. **The daemon's doorbell thread** (`crates/server/src/doorbell.rs`). It swallowed the failed wait and
   looped straight back, spinning a core for the daemon's life. A ring was noticed only because the
   thread re-read the word on every spin.

One shared READY Event also served every claimant of an instance. An auto-reset Event wakes one waiter
per signal, so with two concurrent claims one claimant could take the other's signal. The answered
claimant then waited out its whole claim wait (a second).

## Fix

- **The doorbell is a `Bell`** (`crates/ipc/src/rendezvous.rs`, macOS and Windows): the bootstrap
  object's word plus the wake that reaches the thread where it waits. On macOS that wake is the word
  itself. On Windows it is a named doorbell Event (`slates-rv-<instance>-bell`), which the daemon holds
  from its start.
  - A ring bumps the word, then wakes.
  - The waiter waits on the Event only while the word still equals what it last acted on.
  - The client's doorbell, the claim's ring and the daemon's waiter are all `Bell`s, so the platform
    difference lives in one type.
- **One READY Event per claim slot** (`slates-rv-<instance>-rvz<slot>`), held by the daemon from its
  start. A claim is answered by signalling its own slot's Event, and a failed signal is an error.
- **The doorbell thread** (`crates/server/src/doorbell.rs`) waits on the `DoorbellWaiter` with no
  platform branch.
  - A refused wait ends the thread by name instead of spinning.
  - A failed spawn is returned from `DoorbellThread::start`, not swallowed into a missing thread.
  - The stop rings a second waiter, and a failed ring is reported.
- **A claim whose Event or ring fails gives its slot back** (`FREE`) and returns the error.

## Evidence and limits

- macOS: the rendezvous (5), ipc lib (17), client (5), consumer, attach-form, server daemon (14) and
  server lib (105) tests pass. Clippy is clean on macOS and for `x86_64-pc-windows-msvc` (`slates-rt`,
  `slates-ipc`, `slates-mem`).
- The Windows behaviour itself is proven only by the next Windows CI run. This host cannot run it.

## Found on the way (not changed here)

- `a_udp_datagram_is_received_through_the_driver` and `a_second_receive_…` hang on Windows past 15
  minutes. Both end in an unbounded `rt.shutdown()`. The tests now bound the shutdown and report the
  shard's pulse and the sends' results, so the next run says where the time goes.
- `the_typed_verbs_drive_the_lifecycle…` fails on Windows at `CreateFileMappingW` with 1450
  (`ERROR_NO_SYSTEM_RESOURCES`). The anchor segment derives to 167–188 GB on a 128 GB host (the
  per-partition log is 16 GB; the snapshot slots scale with memory). Linux and macOS back it lazily,
  but a Windows pagefile section is charged its full size at creation. Owed: reserve the section and
  commit on first use.

## Edits

- `crates/ipc/src/rendezvous.rs`, `crates/server/src/{doorbell,daemon}.rs`, `crates/rt/tests/udp.rs`.
- `docs/wip/TBD_FIXES.md`.

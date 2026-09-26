# Windows shards were never kicked

Date: 2026-09-26. Contracts: §4.3 (a sender kicks a parked shard; the registry owns the kick and
retires it after its borrowers, D-8), §4.7 ("Wake strategy"). Found by CI run 36260818113 (`ac84f1c`,
native Windows), whose UDP tests now report the shard's pulse instead of hanging.

## Symptom

`cargo test -p slates-rt --test udp` on Windows:

```
a_udp_datagram_is_received_through_the_driver: after the datagram arrived: the runtime did not shut
  down within 10s; pulse steps 6 waits 3 spawns 2 completed 2 exited false, then the same
a_second_receive_…: the sends: []; pulse steps 1 waits 1 spawns 0 completed 0 exited false (frozen)
```

- In the first test the datagram arrived, so AFD readiness works. Then the shard parked with nothing
  left to run and never woke for the shutdown.
- In the second the shard parked before its first spawn arrived and never ran it: one step, one wait.

The run before (`85a29fb`) showed the same tests stuck until the 15-minute step limit.

## Root cause

A sender wakes a parked shard through the kick its registry slot holds (`Parking::kick_if_parked`,
used by every spawn, control message and foreign wake). On Windows the runtime registered
`Kick::None` (`runtime::register_kick`, "the completion port is the driver's"), so every kick to a
Windows shard did nothing. A parked shard ran again only when a timer or an I/O completion ended its
wait. With neither pending, it never ran again.

The driver's unit test did not catch this. It kicked through the driver's own handle
(`driver.kick_handle()`), which did post to the port, not through the registry's kick that senders
use. Before `b2b6389` every Windows test ran in one step that hung for six hours, so no log showed it.

## Fix

- The completion port is owned by the registry slot, as a Unix kick descriptor is.
  - `iocp::Port` is closed only when the slot retires the registration, after the shard's thread has
    ended and every foreign borrow has returned.
  - `RegisterKick::Port` hands the slot the port. The slot mints `Kick::Iocp(KickPort)`, a generational
    name resolved under the slot's reader pin (the counterpart of `KickFd`). A kick copied before
    retirement is a miss afterwards, never a packet posted to a handle value the process has reused.
- The driver borrows the port through its `KickPort` and no longer closes it.
- The driver test now kicks through the registry's kick. A new CI step runs it in both Windows lanes
  (`cargo test -p slates-rt --lib driver::`).

## Evidence and limits

- The CI pulse above shows a shard parked with no kick, and the registration code shows why. The
  datagram's arrival shows the AFD reactor is not at fault.
- Clippy is clean for `x86_64-pc-windows-msvc` (`slates-rt`, `slates-ipc`) and on macOS. The driver test
  passes on macOS. The unsafe budget is unchanged (58 of 59): the port's one `CloseHandle` moved from
  the driver to `Port`.
- Proof on Windows is owed to the next CI run: the driver step, the UDP tests, and the rendezvous tests
  that already pass.

## Edits

- `crates/rt/src/{iocp,driver,registry,runtime}.rs`, `.github/workflows/ci.yml`, `docs/wip/TBD_FIXES.md`.

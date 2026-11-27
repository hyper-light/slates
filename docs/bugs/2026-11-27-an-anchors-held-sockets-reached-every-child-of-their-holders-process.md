# An anchor's held sockets reached every child of their holder's process

- **Date:** found 2026-11-27 by the TSan lane's diagnosis (main 01a20182's brief, item 3); red the same day; fixed
  2026-11-27 (A-931).
- **Subsystem:** the anchor's hand-off to its daemon (§2.6, §4.7, A-114): `crates/ipc/src/rendezvous.rs` (the held
  rendezvous), `crates/anchor/src/supervise.rs` (the supervisor's held listeners, its hold table and the spawn),
  `crates/api/src/role/anchor.rs` (the anchor's listeners).
- **Amends:** A-878 and its record, docs/bugs/2026-11-23-a-hand-off-left-an-inheritable-duplicate-at-every-call.md,
  whose "not changed" let these stay inheritable.
- **Severity:** a confinement defect wherever the holder's process starts any child but its daemon. Today those are the
  tests, where a test process plays the anchor, and embeddings, where an anchor and its daemon share a process
  (`Adoption`'s doc names both). A standalone `slates anchor` starts no child but its daemon.

## Description

The anchor holds descriptors for its daemon's successors: the client rendezvous's listening socket (Linux), the NFS
listener, the version-control HTTP, SSH and svn listeners, a fleet node's serve sockets, and the hold table (its
channel's end, every held FUSE device, NFS connection and copy, and the table's own object). Each was kept inheritable
in the holder itself:

- `HeldRendezvous::hold` made its listening socket without `SOCK_CLOEXEC`;
- the anchor role cleared close-on-exec on each listener as it took it (`api/role/anchor.rs`, six sites);
- the supervisor cleared it on every hold-table descriptor at each spawn (`device_env`).

So every child any thread of the holder's process started kept a copy of each, past its exec, for its whole life.

A-878 (2026-11-23) left these on purpose, on the rule that "the anchor's only child is the daemon", which marks
everything close-on-exec at its start. That rule holds for a standalone anchor. It is false in the holder's process
wherever an anchor shares it: a test of the binary, or an embedding. There, the daemon's spawner (A-924) and every hook
it starts inherit the listeners, and any other child keeps a listener's address bound after its holder lets it go.

## Measured

- **The TSan lane's parity listing** (diagnostics run 45991805330, `tsan-1.out`): both children of the spawner's parity
  test held `socket:[22118]`, flags `04002` (non-blocking, no close-on-exec): a listening socket a concurrent test of
  the binary held.
- **The red** (diagnostics run 45993337450, `diag-measurer-tsan-rendezvous-1` at 4c53b756e, ubuntu-latest):
  `a_child_spawned_beside_a_held_rendezvous_holds_none_of_it` held a rendezvous and spawned an unrelated `sh`. The shell
  held the held socket, `socket:[16158]`, flags `04002`, which `/proc/net/unix` names at the instance's abstract address
  (`@slates-rv-rv-inherit-3013/1001`). The test failed, exit 101.
- On Linux `Held::hold` is the only site that made a socket without close-on-exec: every other `SocketFlags::empty()`
  is a macOS arm that sets the flag straight after.

## Root cause

A descriptor kept inheritable in its holder so that one child inherits it reaches every child of that process. A-878
fixed this for the segment and the content object: each stays close-on-exec in the anchor, and the spawn clears it in
the daemon's child alone (`hand_on_in_child`, between the fork and the exec). The listeners and the hold table kept the
older form.

## Fix (A-931)

- **One rule for everything the anchor hands its daemon:** close-on-exec in the anchor, as each is made or received,
  and cleared in the daemon's child alone.
  - `HeldRendezvous::hold` makes its socket close-on-exec. `HeldRendezvous::handed_descriptor` names it (Linux; none
    where the rendezvous is found by name).
  - `Supervisor::hand_on_descriptor` takes a descriptor its owner holds, such as the rendezvous, and the anchor role
    hands the rendezvous on through it.
  - The anchor role no longer clears close-on-exec on its NFS, fleet serve, VCS HTTP, SSH and svn sockets. The
    fallback for "cannot be made inheritable" goes with it.
  - `device_env` no longer clears it on the hold table's descriptors. It returns their numbers.
  - The Unix `spawn_child` hands on every one of those numbers with the segment's through `hand_on_in_child`, now on
    every Unix rather than Linux alone (macOS hands the objects by name but the listeners as descriptors).
- An anchor in this process adopts the rendezvous by its number as before, since `adopt` duplicates it close-on-exec.
- The client tests' `hold_once_released` stays: a close-on-exec descriptor is still copied into a child between its fork
  and its exec (14 to 36 µs measured, docs/bugs/2026-10-26-a-dropped-rendezvous-stayed-bound-in-another-tests-spawn.md).
  That window remains; the lifetime hold is what this removes.

## Tests

- `a_child_spawned_beside_a_held_rendezvous_holds_none_of_it` (`crates/ipc/tests/rendezvous.rs`, Linux): red in run
  45993337450, green with the change.
- `a_held_listener_reaches_its_daemon_alone` (`crates/anchor/tests/anchor.rs`, Linux): a supervisor holds a
  close-on-exec loopback listener as the anchor holds its NFS listener; its supervised child must hold it once, and an
  unrelated shell none. Red before: the supervisor handed nothing on, so the child held none.
- `a_claim_begun_before_a_restart_judges_the_daemon_that_answered_it_alive` (`crates/ipc/tests/rendezvous.rs`) hands
  the held rendezvous on in its re-invoked child, as the anchor's spawn does.
- The Linux tests run on Linux CI (main's): the TSan lane and the anchor's cross-process tests (`run-requests.md`).

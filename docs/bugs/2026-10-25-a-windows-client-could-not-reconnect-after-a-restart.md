# A Windows client could not reconnect after a daemon restart

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record (A-176).
- **Subsystem:** the client rendezvous (§4.7; A-114, AC-2.3): `crates/server/src/daemon.rs` (the accept loop's region
  name) over `crates/mem/src/shared.rs` (a Windows section refuses a name that is already live).
- **Severity:** every daemon restart on Windows cost every connected client its session: the reconnect under its own id
  was refused, and the client's calls ended `DaemonGone` where AC-2.3 says the session outlives the restart and a retry
  meets its completion record.
- **Exposure:** Windows only. Linux hands a region as a descriptor, which has no name; macOS's `shm_open` creator unlinks
  the old name first, so the old client keeps its unlinked object and the new region takes the name. Present since the
  Windows rendezvous was written (Phase 2); never seen because CI had never run the restart tests on Windows.
- **Found by:** running the client binary's restart tests on jupiter (Windows 10.0.26200) once they held the
  rendezvous as the anchor does (A-176's harness change).

## Symptom

`crates/client/tests/client.rs`, five restart tests, on jupiter; for example
`a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record`:

```
slates-server: a rendezvous accept round failed: shared memory: CreateFileMappingW (the name is already a live section) refused (code Some(183)) (first occurrence; later ones are counted)
thread 'a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record' panicked at crates\client\tests\client.rs:1270:6:
called `Result::unwrap()` on an `Err` value: DaemonGone { after_ns: 2000000000 }
```

The other four ended the same way (`DaemonGone` after 2 or 3 s).

## Root cause

The daemon named a client's region `slates-cr-<instance>-<client id>`. A client reconnecting after a restart asks for
its own id (§4.9: its completion records live on the partition its id names), and it still maps the region the dead
daemon made it, so the new daemon's region had the old region's name. A Windows section's name lives while any process
maps it, and slates refuses to open a live name rather than share it (`shared.rs`: two creators would share one object
silently), so the region could not be created, the claim was refused, and the client's reconnect budget ran out.

## Fix

Each region's name is `slates-cr-<key>-<number>` (`region_name_key` in `daemon.rs`). The 64-bit key is derived with a
keyed-hash domain of its own (`slates/client-region-name/v1`) from the issuer secret every daemon start mints fresh, so
no daemon names a region as an earlier one did. The number counts the regions this daemon made, so no two of its regions
share a name either. It wraps after 2^32 regions: a name repeats only for a region made that many regions earlier, and
only a holder still mapping that one refuses the new connection, typed, as any refused region does. The name is 35 bytes,
within the 40 a claim slot's answer holds.

## Verification

On jupiter the seven restart tests pass one at a time (with the client binary's other 13, 20 of 20). Red-checked there:
with the old name back, `a_session_outlives_…` fails again with the daemon's own log line, now
`slates-server: a client's claim was refused: shared memory: CreateFileMappingW (the name is already a live section)
refused (code Some(183))`.

## Siblings

- The same name reuse in the rendezvous test's own helper (`serve_until_connected` named a region by the client id a
  fresh listener counts from one again); the helper now numbers its regions once each.
- The bootstrap object itself, whose name a dead owner's clients keep alive:
  `docs/bugs/2026-10-25-a-windows-instance-could-not-start-beside-a-dead-ones-client.md`.
- The refused claim also failed its whole accept round:
  `docs/bugs/2026-10-25-a-refused-claim-dropped-its-rounds-accepted-clients.md`.
- GAP-A9-9 stays open: a region's name is still openable by a same-user process on Windows (and on macOS); handing the
  region by handle duplication would remove the name altogether.

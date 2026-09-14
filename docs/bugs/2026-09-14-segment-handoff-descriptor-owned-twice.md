# A Linux segment handoff's descriptor is owned by every attach, so the second attach closes the first's

Date: 2026-09-14
Area: `crates/mem/src/shared.rs` (Linux `open_object`), reached from `crates/anchor/src/segment.rs`
(`attach_from_env`, `open_content`) and `crates/cli/src/daemon.rs` (`published_profile`)
Severity: the Linux anchor+daemon path — the daemon aborted or refused its shard mapping on every start
under an anchor; `slates-anchor --test anchor` aborted on the Linux CI lane (the open "anchor fd
double-close" item of the CI record).

## Symptom

Two faces of one defect:

1. The KIND lane's image proof with `anchor --quick` (2026-09-14 ~11:00 CDT, `slates:lane` in Docker):

   ```
   slates daemon: fleet node `slates-0` of `slates-fleet`: member 6012571224174635487 with 0 peer(s) at f = 0
   slates: start: segment: shared memory: mmap refused (code Some(9))
   slates anchor: daemon exited (Some(4)); restarted (1 in the window)
   ```

   `mmap` refused with `EBADF` (9): the descriptor number the anchor handed the daemon was no longer
   open when the daemon's `Daemon::start` mapped the segment.

2. `cargo test -p slates-anchor --test anchor` in the `rust:1.98` container (2026-09-14 11:05 CDT), as on
   the Linux CI lane since it was first run:

   ```
   fatal runtime error: IO Safety violation: owned file descriptor already closed, aborting
   … (signal: 6, SIGABRT: process abort signal)
   ```

## Root cause

On Linux the handoff is a descriptor number: the creator duplicates its memfd without close-on-exec and
the child inherits the number. `open_object` turned that number into an `OwnedFd` with
`OwnedFd::from_raw_fd` — ownership of the inherited number itself — under the assumption that "nothing
else in this process owns it". But one process attaches one handoff more than once: the daemon reads the
anchor's published profile (`published_profile` attaches, reads, drops — and the drop **closed the
inherited number**), then `Daemon::start` attaches again from the same environment, and every shard maps
the segment again from the daemon's own `handoff_env`. The second `from_raw_fd` of a closed number is
either a mapping refused `EBADF` (face 1) or, when both owners still exist, two closes of one number,
which Rust's I/O-safety check turns into an abort (face 2). macOS and Windows hand off a *name* and open
it afresh each time, so only Linux had the defect — and the Linux anchor+daemon flow had no green run
to show it (the CI record's "needs a Linux repro").

## Fix

`open_object` never owns the inherited number: it **duplicates** it (`fcntl_dupfd_cloexec`, so a child
of this process does not inherit the duplicate) and owns the duplicate. Each attach then closes only what
it opened; the inherited number stays open for the process's life, as inherited state does. One extra
descriptor per attach, bounded by the attaches a process makes (the profile read, the start, one per
shard).

Test first, on Linux in the `rust:1.98` container: `cargo test -p slates-anchor --test anchor` aborts
(above) before the fix and passes after; `two_attaches_from_one_handoff_each_own_their_descriptor`
(`crates/mem/src/shared.rs`, Linux only) opens one handoff twice, drops both, and opens it a third time.
The by-use gate is the image proof: the anchor's daemon starts and answers `status`.

## Siblings

- The NFS listener handoff (`crates/cli/src/anchor.rs` `hold_nfs_listener`, `ENV_NFS_LISTENER`) hands a
  raw number the same way; the daemon adopts it once (`TcpListener::from_fd`) and only once, so it is not
  double-owned today — but it carries the same assumption. Not changed here; noted.
- The macOS and Windows name handoffs are unaffected (a fresh open per attach by construction).

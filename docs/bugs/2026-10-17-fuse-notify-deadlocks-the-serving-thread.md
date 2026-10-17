# A FUSE invalidation written from the serving thread deadlocks the mount

- **Date:** 2026-10-17
- **Area:** `crates/bridge-fuse` (the FUSE channel's notify path), `crates/server` (the shard that drives it), §4.6 Linux / Cache posture
- **Status:** fixed

## Description

A Linux FUSE mount wedges permanently. Reproduced by `crates/server/tests/fuse_mount.rs`
`a_scoped_fuse_mount_presents_one_directory_and_follows_it`, which hangs forever (observed 13 minutes
at 1 second of CPU, and again under a 240 s timeout → exit 124). The daemon keeps running; the mount
serves nothing further; the mount point is left requiring `fusermount3 -u`.

It is **not** a slow test. The hung process had accumulated 1 s of CPU and was not advancing, with two
live `fuse.slates` mounts and `waiting=1` on both FUSE connections.

## Root cause

The thread that answers FUSE requests also writes FUSE **notifications**, and a notification write can
block in the kernel on a lock that only an answered request will release.

`FuseChannel::write_invalidation` (`crates/bridge-fuse/src/channel.rs`) performs a blocking
`rustix::io::write(&self.device, …)` of `FUSE_NOTIFY_INVAL_ENTRY`. It is called from `deliver_owed`,
which runs inside `serve_step` and `dispatch_ready` — the same functions that read a request and
produce its reply. In the daemon that loop runs **on a shard** (the blocked thread was `slates-shard-1`).

On the kernel side, `fuse_reverse_inval_entry()` takes the parent directory's inode lock. A concurrent
`lookup` already holds that lock for the whole request — from the moment the kernel issues it until the
daemon replies. So:

1. kernel: `open()` → `atomic_open` → `lookup` on parent *P* → takes *P*'s `i_rwsem`, queues LOOKUP, waits.
2. daemon shard: writes `NOTIFY_INVAL_ENTRY(P, "h")` → kernel blocks acquiring *P*'s `i_rwsem`.
3. the shard is the only thread that would answer the LOOKUP that would release the lock. Deadlock.

Captured stacks (kernel 7.2.9-cachyos):

```
test thread:  path_openat → fuse_atomic_open → fuse_lookup_name → fuse_chan_send   (S)
slates-shard-1: __x64_sys_write → fuse_dev_write → fuse_notify → fuse_reverse_inval_entry   (D)
```

Captured trace (a temporary `eprintln!` around the notify write):

```
NOTIFY-TRACE begin Entry { parent: 3659174697238530, name: "h", expire: false }
begin=1  end=0
```

One notification attempted; the `write(2)` never returned.

`FUSE_EXPIRE_ONLY` does **not** avoid this. The channel already negotiates `FUSE_HAS_EXPIRE_ONLY` and
uses it when the caller asks to *expire*, but the kernel takes the parent's inode lock before it
consults the flags, so an expire blocks exactly as an invalidate does. (The failing call is
`expire: false` in any case.)

The ordering inside our own loop cannot fix it either: the kernel holds the parent lock from the moment
it issues the lookup, independent of when we read that request off the device. Delivering invalidations
*before* reading a request blocks just the same.

## Impact

Any FUSE mount that receives an entry invalidation while a lookup is in flight on the same directory can
wedge the serving shard. The exercised case is two attachments to one volume (a whole mount plus a
`--subtree` scoped mount) where a change through one invalidates a name the other has cached, but
nothing about the deadlock is specific to scoped mounts — one mount plus any concurrent lookup suffices.

Severity is high because the blocked thread is a **shard**, which serves other volumes too, and because
`Invalidated` cache posture (§4.6, design line ~1986) hands the kernel unbounded entry lifetimes on the
promise of "proven invalidation delivery" — the very delivery that deadlocks.

Pre-existing, not introduced by the DAX work in flight: reproduced identically against the baseline
`crates/vfs` at `d930b735` (before the DAX backing A/B), hanging on the same test. The test arrived with
`d04f89f5` (AUD-29-76, the subtree/scoped-mount half). CI runs `cargo test --workspace`, which includes
this suite, so the lane is exposed to it on a kernel that schedules the race.

## Fix

A round of invalidations is written on a thread of its own (`NotifyThread` in
`crates/bridge-fuse/src/channel.rs`), owned and joined by the `ServeState` that started it, while the
serving thread stays free to answer requests — which is what releases the lock the write waits on. It
mirrors the landing barrier's thread (A-132, `crates/server/src/landing.rs`), which exists for the same
reason: a blocking call must not be made on the thread everything else waits behind.

`FuseChannel::write_invalidation` — the API that wrote a notification on the caller's thread — is gone,
replaced by `encode_invalidation`, so the hazard cannot be reintroduced by a new caller. One round is out
at a time: `Bridge::invalidations` consumes what it reports, so a second round is never gathered while one
is in flight, which is also its bound. The cursor advances only for a round handed off whole, and
`Coherence::rewind` puts it back if the kernel refuses the write (delivering an invalidation twice is
harmless — the kernel drops nothing it does not hold).

Not done by this fix: the daemon's serve loop waits on device readability, so a finished round's outcome
is observed at its next turn rather than immediately. The only outcome needing action is a device refusal,
which accompanies a mount that is ending; `ServeState::wakes_with` is available for a loop that carries a
change signal.

**Verified:** `a_scoped_fuse_mount_presents_one_directory_and_follows_it` went from hanging 100% of the
time (exit 124 under a 240 s timeout) to passing 5 runs out of 5 in ~0.22 s. A deterministic unit test,
`a_round_whose_write_blocks_does_not_hold_its_owner`, holds the invariant without needing a kernel: a round
whose write cannot land leaves its owner free, and is joined once it can. Regression on the box:
bridge-fuse all suites, `nfs_mount` 36/0, `daemon` 22/0, `virtiofs` 18/0, `attach_forms` 8/0, recovery
oracle 41/0, clippy `--all-targets -D warnings` clean.

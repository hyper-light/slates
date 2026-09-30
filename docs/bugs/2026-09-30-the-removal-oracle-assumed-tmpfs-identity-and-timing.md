# The removal oracle assumed tmpfs's identity and timing (2026-09-30)

Contracts: §4.15 step 6, D-26, AUD-29-04 (the removal oracle). Found by A-50's fixture move: the OS leg
`crates/land/tests/os_removal.rs` had run only on the Linux lane's `/dev/shm`; it now runs in the build
output on the host's disk.

## Description

On ext4 (the Linux container's build volume), the oracle failed histories the engine had landed
correctly, in two ways.

1. "File at calls 9 and 11: the entry was written, yet its witnessed object is still at /doomed."
2. "Replace { exchange: true } at calls 25 and 27: an armed edit never happened." This one came from
   different histories on different runs.

## Root cause

1. **Identity by inode number.** The oracle identifies objects by inode number. ext4 hands a freed number
   to the next file at once; tmpfs and APFS do not. A probe showed the later outsider file created with
   the removed witnessed object's number (`witnessed=101079124`, second save `created=101079124`). The
   oracle read that as the witnessed object still being there.
2. **A fixed call count.** The harness bounded a second outsider save by a separate single-save run's
   seam-call count, which assumes the landing's path is the same every run. On a coarse-clock filesystem
   it is not. ext4 stamps ctime in kernel ticks (1 ms steps observed). Whether the engine's own exchange
   moves the displaced original's ctime depends on the tick, and the engine rightly re-verifies when it
   did. Logged seam traces showed one history ending at 29 calls in one run and 27 in the next. Both
   runs made identical calls and stats up to the exchange; only the post-exchange ctime differed.

The product is not implicated. The engine's `Fingerprint` carries size, mtime, ctime and mode beside
the inode, and every history it landed kept the rules.

## Exact edits (`crates/land/tests/os_removal.rs`, `common/removal.rs`, `removal.rs`)

- **Pinned identities.** The harness holds a descriptor on the witnessed object and on every outsider
  file for the whole history (`O_PATH|O_NOFOLLOW` on Linux, `O_SYMLINK` on macOS), so no tracked
  number can be reused. A held descriptor changes nothing a landing observes.
- **Trace-driven positions.** Each loop arms saves at successive calls until a landing ends before its
  armed call. The self-check is exact: `reached_edits_fired` means every edit armed at a call the run
  reached fired. The deterministic simulated leg keeps "every armed edit fired", which is exactly true
  there.
- **Non-vacuity.** Each kind must also have fired both saves in some two-save history.
- **Rejected.** Waiting after seeding until the filesystem's clock passed the seed's ctime: it forced
  one timing and so left the same-tick path untested on real disks, and it still failed 1 run in 5
  (a later same-tick ordering).

## Evidence

- Linux ext4: 20 of 20 runs pass.
- macOS APFS: passes.
- The simulated leg (`tests/removal.rs`) passes.

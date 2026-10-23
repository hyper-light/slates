# A CLI test's unmount met a sibling test's child holding its files

**Status:** explained and fixed in the tests, 2026-10-23. A harness artifact, not a product bug; the box (Linux 7.2.9),
by temporary instrumentation since removed.

**Severity:** a flaky test, not a product defect. `a_fuse_mount_survives_its_daemons_kill_with_its_open_files_usable`
failed "fusermount3: failed to unmount …: Device or resource busy" in about 5 of 16 full CLI-suite runs on the box
under load, and never alone. GAPS had recorded it since A-150 as "unexplained; the box's KDE session is a suspect".

## Evidence

At load average 81 to 114, with temporary probes in the test helper and a request log in the FUSE channel (each
request's caller pid and process name):
- **At the refused unmount:**
  - the mount's FUSE connection had `waiting=0`;
  - `fuser -vm` and a sweep of every process's fds, cwd, root and maps found no holder;
  - a retry 0.5 s later succeeded.
- **The test's dead daemon** received `FLUSH`es on the test's two files from a dozen processes named after other tests'
  threads (`a_closed_files_`, `removing_the_sh`, `the_handles_a_d`, `slates_mount_su`, `the_anchor_and_`, …).
- **Children spawned by this test's own thread** sent `FLUSH`es to five other tests' daemons.
- **`kded6`** (KDE's daemon) did look up a slates mount's root, but 20 s after this refusal: a prober, not this holder.

## Cause

The CLI tests run as threads of one process. Every child a test spawns gets a copy of every descriptor open in the
process, other tests' files on their FUSE mounts included. It holds them from the fork until its `exec` closes them
(`O_CLOEXEC`); the `FLUSH`es are those closes, since every close of any copy of a FUSE descriptor sends one.

A sibling's child forked just before a test closed its files, and slow to reach `exec` under load, holds them at the
instant the test unmounts. The kernel rightly answers `EBUSY`. By the time the probes ran, the child had exec'd.

## The fix

`crates/cli/tests/cli.rs` `unmount_through_the_cli`: `slates unmount`, tried again while the kernel answers busy, until
`WAIT_FOR`, the tests' derived wait of ten liveness budgets. Any other failure returns at once. The ten call sites that
assert an unmount's success use it. `unmount_after_container` already waits out a container runtime's hold, and two
cleanups ignore the result.

The product's `slates unmount` stays one attempt, truthful at the instant it is asked, as `fusermount3` and `umount`
are.

## Verification

Ten full CLI-suite runs on the box under load, listed in the commit.

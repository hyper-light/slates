# landing: a landed directory lost the mode bits the process umask cleared (found by the container lane's hermeticity leg)

**Date:** 2026-10-01. **Audit:** AUD-29-78 (the Linux container hermeticity leg). **Design:** §4.15 step 6.
**Found by:** the first `oci-linux × hermeticity` run in the Linux container lane, whose completeness check
compares the landed tree with the whole mounted tree.

## Description

The run's landing manifest did not match the mount:

```
the landed tree differs from the complete mounted tree: mounted=… conformance-173 … mode: 509 …
landed=… conformance-173 … mode: 493
```

The directories the harness made through the mount were 0775 (509), and the landing made them 0755 (493) on the
disk. Files kept their modes.

## Root cause

`OsLand::mkdir` (`crates/land/src/os.rs`) made a directory with `mkdirat(dir, name, mode)` and nothing else. The
kernel applies the process umask to `mkdirat`'s mode, so a daemon running under umask 022 cleared the group-write
bit the volume recorded. Files were never affected: a file's temporary is created 0600 and then given the
recorded mode with `fchmod`, which the umask does not touch. The landing oracle runs the engine over `SimHost`,
which has no umask, so it could not see the difference. The real-disk tests in `crates/land/tests/os.rs` used
0755 directories, which a 022 umask leaves unchanged.

## Impact

Every landing of a directory whose mode had a bit the daemon's umask clears (0775, 0777, group- or
other-writable shared trees) wrote a narrower mode than the volume held, on Linux and macOS. No content was lost
and nothing was widened; the landed tree simply disagreed with the volume, and a later landing saw no change to
correct, because the plan compares against the volume.

## Exact edits

- `crates/land/src/os.rs` `mkdir`: after `mkdirat`, the new directory is opened with `O_DIRECTORY|O_NOFOLLOW`
  (never a symlink swapped in under the name) and given the recorded mode exactly with `fchmod`, as a file's
  temporary is by `set_mode`. The module header names the rule.
- `crates/land/tests/common/mod.rs`: `mkdir_with_mode`, so a test can make a directory with its own mode.

## Proof

- `crates/land/tests/os.rs` `a_landed_directory_keeps_its_mode_whatever_the_umask` sets the umask to 022 and
  lands `/shared` (0775) and `/private` (0700) into an empty real directory. Before the fix it failed on macOS
  (`shared landed 755, recorded 775`); after it, it passes on macOS and on Linux (rust:1.98.0, non-root,
  2026-10-01). The whole `slates-land` suite passes, including the crash-at-every-write-instruction oracle, whose
  step count now includes the extra `fchmod`.
- The `oci-linux × hermeticity` rerun's landed tree matches the mounted tree.

## Sibling sweep

Every other mode-taking creation site in the workspace: the landing's file temporaries (`openat O_CREAT|O_EXCL`
on macOS, `O_TMPFILE` on Linux) are followed by `fchmod` already; `crates/machine/src/segment.rs` and
`crates/mem/src/shared.rs` create 0600 shared-memory objects, which a umask cannot narrow. Symlinks carry no
mode. No other site writes a recorded mode through a umasked call. `SimHost` still has no umask; the real-disk
test is the leg that covers it, as the landing's other OS-specific rules are.

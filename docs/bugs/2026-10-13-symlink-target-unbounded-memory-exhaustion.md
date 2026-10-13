# A symlink target was unbounded, uncharged inline memory — a quota-bypassing memory-exhaustion vector

- **Date:** 2026-10-13
- **Area:** `crates/vfs/src/volume.rs` (`Volume::symlink`), the symlink target length (the "`SYMLINK_MAX` ceiling"
  previously flagged open); CLAUDE.md #8 (no unbounded growth), CWE-789.
- **Severity:** moderate — a reachable memory-exhaustion DoS. An agent commits daemon memory far past its byte
  quota by creating symlinks with large targets; the target bytes are stored inline in the inode and were
  neither length-capped nor byte-charged. Found by attacking the symlink path (the `/goal` loop), after being
  reminded to exercise rather than read.

## Description

`Volume::symlink` stores the target inline as `Body::Symlink(target.into())` with `attrs.size = target.len()`,
charging only the namespace admission and one inode slot — never the target's bytes, and with no length cap.
A file's content bytes are charged against the byte quota (`write_room`), and an xattr value's are too; a
symlink's target was not.

Reproduced e2e (`crates/vfs/tests/edges.rs`): against a **64 KiB** byte quota, an agent created **10,000 symlinks
with 8 KiB targets = 81.9 MB** of target bytes before the loop cap stopped it — a ~1250× overcommit. A control
run showed the amplification is symlink-specific: empty files and 1-byte-target symlinks hit the same (test)
inode-count ceiling, but only the 8 KiB targets committed real memory, because a file's content would have been
charged and a symlink's inline target was not.

## Root cause

The target is a variable-length inline allocation in the inode with no bound and no byte charge. A client's
`SYMLINK` carries a target up to the transport's transfer cap, so each symlink could hold that much uncharged
memory, repeated to exhaustion.

## Fix

A symlink target is a path, so cap it at the host/archive path ceiling — `PATH_MAX` (4096), which the archive
already defines as `manifest::PATH_MAX_BYTES` (made `pub`) and which a host `symlink(2)` enforces as
`ENAMETOOLONG`. `Volume::symlink` now refuses a target longer than `SYMLINK_TARGET_MAX_BYTES`
(= `slates_archive::manifest::PATH_MAX_BYTES`, a `Derived` constant) with `VfsError::NameTooLong` before any
effect. This is the same ceiling discipline as `XATTR_NAME_MAX_BYTES` — a volume never holds a target its placed
archive could not carry — and bounds each target; the per-inode charge bounds the count in production (the test
store under-charges inode count, which is why the attack's count looked unbounded there). It resolves the
previously-open "`SYMLINK_MAX` ceiling" with the principled derived value rather than leaving it undecided.

## Verification

`a_symlink_target_past_the_path_ceiling_is_refused` fails before the change (the over-long target was stored) and
passes after (`NameTooLong`, nothing created; a target exactly at the ceiling still lands and reads back). No
legitimate symlink breaks: the host-differential suite (38), NFS symlink procedures (50), base (3), lifetime (14)
and special/IPC (6) are all green — real symlink targets are paths within `PATH_MAX`.

## Sibling sweep

Other inline/variable-length client-supplied fields were checked: directory and file **names** are capped
(`names::check_length` → `NAME_MAX`), **xattr names** at `XATTR_NAME_MAX_BYTES`, **xattr values** and **file
content** are byte-charged via `write_room`. The symlink target was the one inline field with neither a cap nor a
charge; it is now capped. (A further refinement — counting the target's bytes in `referenced_bytes` like file
content — is noted for consistency but not required for the bound; the cap plus the per-inode charge bounds the
memory.)

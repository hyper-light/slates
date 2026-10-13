# A symlink target with an interior NUL byte was accepted — a cross-transport consistency and conformance hole

- **Date:** 2026-10-13
- **Area:** `crates/vfs/src/volume.rs` (`Volume::symlink`), the symlink target's byte content; CWE-158
  (improper neutralization of a null byte); POSIX.1-2017 §3.271 (a pathname is a NUL-terminated string),
  `symlink(2)` `EINVAL`.
- **Severity:** moderate — a correctness and consistency hole, not a direct escape (slates never *resolves* a
  symlink target: confinement is structural, a parent-chain climb that never follows a target). An agent could
  store a target that reads back as different bytes over different transports, and that a host `symlink(2)` or
  the placed archive would refuse or truncate. Found by attacking the symlink path (the `/goal` loop) right after
  the target-length bug, applying the sibling-sweep habit to the *other* inline content of the same field.

## Description

`Volume::symlink` stored the client's target verbatim as `Body::Symlink(target.into())` with no check for an
interior NUL byte. A component **name** is checked (`names::check` refuses any byte `== 0`), but a symlink
**target** — which legitimately contains `/`, so it cannot reuse `names::check` — was not.

Reproduced e2e (`crates/vfs/tests/edges.rs`): creating a symlink with target `"a\0b"` succeeded and stored all
three bytes (`readlink` returned `"a\0b"`, len 3). The harm is that the stored bytes then diverge from what
resolves, by transport:

- **FUSE readlink** hands the kernel a NUL-terminated target, so `"a\0b"` resolves as `"a"` — the link points
  somewhere other than what was stored.
- **NFS readlink** (XDR, length-prefixed) returns the whole `"a\0b"`, which the client kernel then rejects or
  truncates in turn.
- A **host `symlink(2)`** (on a granted landing) returns `EINVAL` for an interior NUL, and the **placed archive**
  encodes a path it cannot faithfully carry — so a volume could hold a symlink that a landing cannot materialize
  consistently, the same "a volume must not hold what it cannot place" class as the target-length bug fixed in
  the same session ([2026-10-13-symlink-target-unbounded-memory-exhaustion.md](2026-10-13-symlink-target-unbounded-memory-exhaustion.md)).

## Root cause

The target is client-supplied inline bytes with no neutralization. The name path has a NUL check; the target
path, added for symlinks, never gained the equivalent because a target is a multi-component path and the
component checker (which also forbids `/`) does not apply to it. The one rule a path *does* share with a name —
no interior NUL — was therefore missing on the target.

## Fix

`Volume::symlink` now refuses, before any effect, a target containing any `byte == 0` with
`VfsError::InvalidName` (the same refusal a name's NUL makes), placed immediately after the length cap. A target
is a path, not a component, so `/` is still allowed — only the NUL is refused. This holds the invariant that a
stored target reads back identically over every transport and can always be materialized by a host `symlink(2)`.

`symlink_no` (the by-inode wrapper the FUSE/NFS bridges call) delegates to `symlink`, so the check covers the
reachable bridge path with no second site; verified by reading the delegation (`volume.rs:1283`) rather than
assuming it.

## Verification

`a_symlink_target_with_an_interior_nul_is_refused_but_a_path_is_kept` (`crates/vfs/tests/edges.rs`): the probe
form (before the fix) stored `"a\0b"`; after the fix the interior-NUL target is refused `InvalidName` with
nothing created, and a legitimate multi-component path target (`"../some/deep/relative/target"` — slashes, no
NUL) still lands and reads back byte-for-byte, proving the check rejects only the NUL and not a path's
separators (non-vacuity). The host-differential symlink suite, the NFS symlink procedures (13), and the full
`edges` suite (13) stay green; `cargo clippy -p slates-vfs --all-targets -- -D warnings` is clean.

## Sibling sweep

The target was the inline field with both inline-content gaps this session — the length cap and the NUL check.
Names (dir/file) and xattr names go through `names::check` (NUL refused there already). Xattr *values* are opaque
byte blobs, not paths, so an interior NUL is legitimate in them and no check is owed. No other client-supplied
path-typed field stores bytes without neutralization: rename/link/lookup take component names (checked), and file
content is opaque. The two symlink-target bugs close the field.

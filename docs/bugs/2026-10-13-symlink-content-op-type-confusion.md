# Content operations on a symlink were a type confusion — read returned its target, write/truncate lied `Ok`

- **Date:** 2026-10-13
- **Area:** `crates/vfs/src/volume.rs` — the content operations (`read`/`read_body`, `seek`, `write`/`write_with`,
  `truncate`/`truncate_with`, `allocatable` for `allocate`/`admit_allocation`, the SDK `edit`, and the public
  snapshot read `read_in`). §4.4; CWE-843 (type confusion).
- **Severity:** moderate — a type confusion, not a cross-tenant or host escape (a symlink's target is the agent's
  own and already reachable through `readlink`), but a real correctness and integrity defect: `read` returned a
  symlink's target bytes as if they were a file's content, `write` and `truncate` returned `Ok(n)`/`Ok(())` while
  silently doing nothing **and** charging quota (`write`), and `allocate` would materialise holes on it. Found by
  attacking the VFS (the `/goal` loop), probing each content op against a non-file inode.

## Description

Every content operation refused a directory (`IsDirectory`) and an IPC name (`is_special()` →
`SpecialFileOperation`) but **not a symlink**. A symlink's body is a target string (`Body::Symlink(Box<str>)`),
not a file-content body, so a content op on a symlink's inode number fell through to the content machinery:

- `read(symlink)` returned the target bytes as file content (size = target length).
- `write(symlink)` returned `Ok(n)` having written nothing (the body is not a content body) — a lying success —
  after `write_charge` had charged the quota for bytes it never stored.
- `truncate(symlink)` returned `Ok(())`, a no-op that falsely claimed to resize it.
- `allocate(symlink)`/`admit_allocation` was admitted (`allocatable`'s `match` had `_ => Ok(())`, which covered
  both `File` and `Symlink`).
- `seek(symlink)` returned a position within the target's length.
- the public snapshot read `read_in(symlink)` returned the target (it reached `read_in_body`).
- `edit(symlink)` was refused, but only incidentally by a later offset-dependent check, not at the kind gate.

The kernel follows a symlink (it sends `LOOKUP`/`readlink`, not a `READ` on the symlink inode), but an SDK call
or a crafted FUSE/NFS operation can name a symlink's inode number directly, so the confusion is reachable.

## Root cause

The `is_special()` and `Kind::Dir` refusals were **duplicated inline across roughly seven content operations**,
and `Kind::Symlink` was consistently omitted from every one. The duplication is the root cause: there was no one
place that said "a content operation is allowed only on a regular file", so each site drifted (some checked
special + dir, `allocatable` used a `match` with a permissive `_`, `read_in_body` checked only special).

## Fix

Each content operation now refuses `Kind::Symlink` with `VfsError::Invalid` (POSIX `EINVAL` — a symlink is
reached through `readlink`, never read or written as content), beside its directory and IPC-name refusals:
`read_body`, `seek`, `write_with`, `truncate_with`, `edit`, and the public `read_in`; `allocatable`'s `match` was
made exhaustive (`File => Ok`, `Dir`/`Symlink`/`Fifo|Socket` each refused). A symlink stays fully functional
through `symlink`/`readlink`.

**Deliberate exception:** `read_in_body` is left unchanged. The export/archive (§4.16, `export.rs`) serializes a
symlink "as a file whose bytes are its target … never at the head", reading it through `read_in_body` directly —
so refusing a symlink there would break the archive. The public `read_in` refuses the symlink before calling
`read_in_body`, and `xattr_read_in`/the export call `read_in_body` directly on their own inodes, so the one
legitimate symlink-as-content reader is untouched.

## Verification

`a_symlink_answers_only_readlink_not_file_content` (`crates/vfs/tests/edges.rs`): read, write, seek, truncate,
allocate, and edit of a symlink's number are each `Invalid`; the target is unchanged and still reachable through
`readlink`; a regular file's read and write still work (non-vacuity — the refusal is symlink-specific). The model
oracle (`tests/model.rs`) and the real-host differential (`tests/differential.rs`) stay green — the refusals
match POSIX and the host. The snapshot/export paths stay green (`tests/recover.rs` 41, `tests/base.rs` 38), so
the archive's symlink serialization is intact. `cargo clippy -p slates-vfs --all-targets -- -D warnings` is clean.

## Sibling sweep and a hardening note for Ada

Every content-op site that carried a `Kind::Dir`/`is_special()` check was enumerated and fixed; `read_in_body` is
the one deliberate exception (above). **The duplicated inline kind checks are the root cause**, and a new content
operation could miss `Kind::Symlink` again. A shared helper — `fn refuse_non_regular_content(kind) -> Result<(),
VfsError>` returning `File => Ok`, `Dir => IsDirectory`, `Symlink => Invalid`, `Fifo|Socket =>
SpecialFileOperation` — used by every content op (and, for the archive, a separate `read_in_body` that documents
why it admits a symlink) would make the rule structural and prevent a recurrence. Flagged as a hardening refactor,
not applied here to keep the fix focused.

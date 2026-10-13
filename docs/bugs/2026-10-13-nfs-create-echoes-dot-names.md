# NFSv3 UNCHECKED CREATE of "." or ".." echoed the directory instead of refusing

- **Date:** 2026-10-13
- **Area:** `crates/bridge-nfs/src/procedures.rs` (`Procedures::do_create`), §4.6
- **Severity:** low (benign — no entry planted, no journal, no mutation, no new capability), but a
  reserved-name hardening gap and a wrong status code.

## Description

Found while hardening the VFS against escape attempts (the `/goal` loop), by a real run against a live daemon
over a real loopback NFS socket (`crates/server/tests/nfs_hostile.rs`): an NFSv3 `CREATE` with
`createmode3 = UNCHECKED` and name `"."` or `".."` returned `NFS3ERR_OK` (status 0), as if it had created a
file, and handed back a file handle.

## Root cause

`do_create` resolves the name with `self.bridge.lookup(parent, &cx, &name)` before creating. A lookup of `"."`
resolves to the directory itself and `".."` to its parent (both exist). Under `UNCHECKED`, an existing name is
"succeeded on by opening it" (RFC 1813 §3.3.8), so the handler took the `Ok(node) if UNCHECKED` arm and echoed
the directory's (or parent's) own handle as the create's result. The VFS core would have refused the name
(`slates_vfs::names::check` rejects `""`, `.`, `..`, `/`, NUL), but that check runs inside `create_leaf`, which
the echo path never reaches — the lookup short-circuits it.

Confirmed by run: `lookup(".")` returns status 0 before any create, `CREATE(".")` and `CREATE("..")` return 0,
and a second `lookup(".")` is unchanged — i.e. nothing was planted; the handle returned was the directory's own
(which the client can already obtain with `LOOKUP`). So the impact is a wrong status, not a new capability.

## Impact

A client that issues `CREATE "."`/`".."` is told a file was created when none was. It gains nothing it did not
already have (the directory and parent handles are obtainable with `LOOKUP`), writes nothing, and journals
nothing. No cross-tenant reach (the mount capability is bound to its volume — proven separately by
`a_valid_capability_cannot_be_restamped_onto_another_tenants_handle`). The risk is a client confused into
treating the directory as a file, and a reserved component slipping past the create path's name discipline.

## Fix

Refuse `"."` and `".."` in `do_create` with `NFS3ERR_EXIST` (they exist, as the directory and its parent),
before the inner lookup — the same reserved-component discipline `RMDIR` (`Inval`/`Notempty`) and `RENAME`
already apply. Separator and NUL names were already refused `NFS3ERR_INVAL` by `names::check` (confirmed by the
same run: `CREATE "a/b"` and `CREATE "x\0y"` return 22), so only the dot-name echo needed closing.

## Sibling sweep

`MKDIR`/`SYMLINK`/`LINK`/`MKNOD` reach `create_leaf` directly (no UNCHECKED lookup-echo), so `names::check`
already refuses `.`/`..`/separator/NUL there. Only `CREATE`'s UNCHECKED lookup-first path had the echo.

## Regression test

`crates/server/tests/nfs_hostile.rs::a_crafted_name_cannot_split_truncate_or_create_a_control_entry` — a live
daemon over a real socket: the honest name resolves; a NUL-bearing name does not truncate to its prefix; a
separator neither matches nor is accepted; `CREATE "."`/`".."` are `EXIST`; an over-length name is refused.

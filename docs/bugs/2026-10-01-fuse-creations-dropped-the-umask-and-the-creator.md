# FUSE creations dropped the umask and the creator (AUD-29-80, AUD-29-81)

**Date:** 2026-10-01. **Audit:** AUD-29-80 (P1), AUD-29-81 (P1). **Design:** §4.6 (the FUSE bridge), §4.13.

## Description

- **Umask.** The FUSE bridge negotiates `FUSE_DONT_MASK` (`crates/bridge-fuse/src/init.rs`), under which the
  Linux kernel does *not* apply the creating process's umask: it sends the mode unmasked and the umask beside
  it (`fuse_create_in`, `fuse_mkdir_in`, `fuse_mknod_in`; `fs/fuse/dir.c`). The handlers read the mode and
  ignored the umask, and the ABI constant's comment said the kernel applied it. A file created under umask
  077 was `0666`; a directory, `0777`.
- **Creator.** The request header carries the creating process's uid and gid, but the dispatch built its
  operation context without them, so `stamp_created_owner` fell back to the enrolled account and the parent's
  group: a file a 1000:100 process created under a context enrolled as 501 was owned `501:<parent gid>`.

## Root cause

The creation handlers were written for the mode alone, and the context's ownership fields were filled only by
the NFS edge (`AUTH_SYS`). The set-group-ID parent rule was not applied by the shared stamp for any transport.

## Exact edits

- `crates/bridge-fuse/src/bridge.rs`: `masked(mode, umask)` in create, mkdir and mknod (idempotent with a
  kernel that already masked); a creating request's context carries the header's uid and gid as ownership
  metadata (`creates`); `body_u32` reads the fields bounds-checked.
- `crates/bridge-fuse/src/abi.rs`: the `DONT_MASK` comment corrected.
- `crates/bridge-core/src/volume_bridge.rs` (`stamp_created_owner`): a set-group-ID parent gives its group to
  what is made in it, and a new directory takes the bit; otherwise the creator's group where the request names
  one, else the parent's (every transport).

## Proof

`crates/bridge-fuse/tests/creation.rs` (every host): the real dispatch over a real volume, a context enrolled as
uid 501, requests from a 1000:100 process — a file (0666, umask 077), a directory (0777, umask 022), a FIFO
(0666, umask 027) — made `0600`, `0755`, `0640`, all owned `1000:100`; under a set-group-ID root, a file and a
directory take the root's group and the directory the bit. With the fix mutated out the first assertion fails
with `0666` owned `501:20`, the audit's observation.

## Carried

The audit asks for the same at multiple umasks from a real guest or container, and with `DONT_MASK` unoffered;
the dispatch is the one both paths run, and the masking is idempotent, so the property holds by construction —
a live-guest run comes with the guest transport (AUD-29-68). Default ACLs stay typed-unsupported.

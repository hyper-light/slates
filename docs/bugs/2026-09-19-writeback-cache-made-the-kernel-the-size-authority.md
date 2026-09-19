# Negotiating writeback cache made the kernel the authority on a file's size, so an invalidation of a change made behind it was ignored

**Date:** 2026-09-19. **Found by:** AUD-02's mounted regression
(`crates/bridge-fuse/tests/coherence_mount.rs`) on a real kernel (Linux 6.12.76-linuxkit in a
container on this box, `fusermount3` 3.17.2), right after the file-type-bits fix let the mount work at
all: another attachment truncated a file, the loop wrote `FUSE_NOTIFY_INVAL_INODE` for it and the
kernel accepted the notification (`write` returned 40, no `ENOENT`), yet `stat -c %s` kept answering the
old size and sent the daemon no `GETATTR`.

## Root cause

`FUSE_INIT` asked for `FUSE_WRITEBACK_CACHE` (`crates/bridge-fuse/src/init.rs`, `wanted`). In that
mode the kernel owns a regular file's size, mtime and ctime: `fs/fuse/dir.c` `fuse_get_cache_mask`
returns `STATX_SIZE | STATX_MTIME | STATX_CTIME` for a regular file when `fc->writeback_cache`, and
`fuse_update_get_attr` never re-fetches attributes for a request whose fields all lie in that mask
(`request_mask & inval_mask & ~cache_mask` is zero — `stat -c %s` asks for the size alone), while
`fuse_change_attributes` ignores the daemon's size for such a file even when a `GETATTR` does run.
Writeback cache assumes every change reaches the file through this kernel (libfuse: "not suitable
for network filesystems"), and a slates volume changes through other attachments, the SDK and
outsiders beneath a base — exactly the sources §4.6 promises to invalidate for. The mode cannot
honour the coherence the transport advertises, and §4.6's own rule says such a transport refuses that
guarantee rather than advertising it. The design text listed writeback cache among the features
`FUSE_INIT` negotiates; a notifier encoder alone could not tell that it defeats the notifier.

## Fix

`FUSE_WRITEBACK_CACHE` is no longer requested: the kernel writes through, so a `write` reaches the
daemon before it returns (the anchor segment stays the source of truth the reply's stability claim
rests on, D-18) and a size or time invalidation for a change made elsewhere takes effect. The other
negotiated capabilities are unchanged (`PARALLEL_DIROPS`, readdirplus, `EXPLICIT_INVAL_DATA`,
`BIG_WRITES`, `DONT_MASK`, `INIT_EXT`, `HAS_EXPIRE_ONLY`). The design's §4.6 negotiation list and the
flag's own note are corrected in the same change.

## Validation

- `crates/bridge-fuse/tests/coherence_mount.rs` on the real kernel: with writeback cache requested,
  the truncate through another attachment stays invisible to `stat` (three runs, deterministic:
  `left: "4", right: "9"`); without it, the scenario passes — see the AUD-02 record for the run.
- The codec's negotiation tests (`crates/bridge-fuse/src/init.rs`, `tests/handshake.rs`) state the
  new intersection.

## Siblings

- The NFS export never had the problem: the kernel's NFS client revalidates attributes on its own
  timeout (`actimeo=1`), and slates serves every write synchronously.
- `EXPLICIT_INVAL_DATA` stays: it tells the kernel the daemon invalidates data explicitly (which the
  delivery rounds do, `Invalidation::Inode { data: true }`), rather than auto-invalidating on an mtime
  change — compatible with the coherence discipline, unlike writeback cache.

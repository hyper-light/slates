# An unmounted FUSE mount's lookups stayed counted

**Found** 2026-11-08 by bug-sweep, reading the code (not measured), as a sibling of
`2026-11-08-a-forget-after-the-last-publication-leaked-its-lookup-reference.md`. Measured and fixed 2026-11-09 (slabs).

**Description.** A FUSE mount's kernel holds a lookup reference on each inode it has looked up and gives each back with
`FORGET` when it lets the inode go. At an ordinary unmount it sends neither a `FORGET` for the inodes it still holds nor
a `DESTROY` (Linux sends `DESTROY` only to a mount that asked for one). Every reference the mount held then stayed
counted in the volume until the daemon's next restart, and an unlinked file among them kept its bytes.

**Root cause.** The bridge sweeps an attachment's references at `DESTROY` (`crates/bridge-fuse/src/bridge.rs`
`serve_destroy`, the volume's `sweep_attachment`), and nothing else did: the server's end of a mount
(`crates/server/src/fuse.rs` `ended`) revoked and drained the registry attachment and left the volume's references.

**Impact.** Memory held for the life of the daemon: every file a mount looked up and something else later unlinked kept
its bytes, and every looked-up inode its reference. A workload that mounts, reads a tree and unmounts, over and over,
grew the volume's held inodes and bytes without bound until a restart.

**Measured** on main (diag-slabs-44, run 42528067171, ubuntu-latest): 64 files of 64 KiB written through one mount and
looked up (`stat`) through a read-only mount of the same volume; the read-only mount unmounted; the files unlinked
through the first mount and its kernel's `FORGET`s awaited. `referenced_bytes` stayed at 4,194,304, where it was 0
before the files.

**The edits.**
- `crates/server/src/fuse.rs`: `sweep_references`, run by `ended` before the registry attachment is revoked: the
  attachment's references are swept through the bridge (`sweep_attachment`), as a Shards stream's end does
  (`crates/server/src/shards.rs` `sweep`); a refused sweep is counted (`fuse.sweep_refused`). Idempotent: a mount that
  did get a `DESTROY` loses nothing more.
- The test: `the_lookups_an_unmounted_fuse_mount_held_are_given_back_at_its_end` (`crates/cli/tests/cli.rs`, Linux).

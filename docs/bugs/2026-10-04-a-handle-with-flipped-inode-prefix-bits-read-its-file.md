# A loopback NFS handle with its inode-prefix bits flipped read the file it was flipped from (2026-10-04)

## Description

A file handle is 57 bytes: version, volume id, inode number, generation, attachment and token
(`crates/bridge-nfs/src/handle.rs`). The new hostile-connection test flipped one bit of a valid handle at a time
and sent a READ through it. Every flip of bits 136–151 was answered with the original file's bytes. Those bits
are the top 16 bits of the inode number: the volume prefix that `InodeNo::compose(prefix, counter)` puts above
the 48-bit counter.

Evidence: `cargo test -p slates-server --test nfs_hostile every_single -- --nocapture` printed `flips: 440 refused,
answered [136, …, 151]`. Every flip of the version, volume id, generation, attachment and token was refused.

## Root cause

The volume's inode table (`crates/vfs/src/trie.rs`) is a radix trie over the number's counter bits only.
`trie::get` ignores the prefix. The four lookups in `crates/vfs/src/volume.rs`:
- `inode`;
- `read_snapshot_at`'s snapshot lookup;
- `snapshot_inode`;
- `need_current_inode`.

All four returned whatever inode held the counter and never compared its number with the one asked for. Counters
are unique within a volume's lineage (a clone continues its origin's `next_counter`), so no two live inodes
collide. But 65,536 numbers named each inode.

## Impact

This was not an authorization bypass: the handle's capability and volume id are checked before the lookup, so
an alias reaches only an inode the capability already authorizes.

It was a correctness defect: an NFS client caches by handle, and two handles for one file can hold different
cached attributes or data. It also let a client probe the server with handles the server never minted.

## Exact edits

- `crates/vfs/src/volume.rs`: a new `exact_inode(store, root, no)` looks the counter up and refuses `NotFound`
  when the stored inode's `no` differs. The four lookups use it. `inode` keeps mapping other failures to
  `StaleHandle`, as before.
- `crates/server/tests/nfs_hostile.rs`:
  - `every_single_bit_flip_of_a_handle_outside_its_counter_is_refused`: all 456 bits, under a spinner per core.
    It fails with the check disabled (bits 136–151 answered).
  - `a_mount_survives_connections_broken_and_tampered_mid_call_under_load`: the concurrent breakers.

The recovery path's `trie::get` calls in `crates/vfs/src/recover.rs` take numbers from the image the daemon wrote,
never from a client, so they are unchanged.

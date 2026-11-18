# A snapshot's attribute-only change was lost in recovery (2026-11-17)

Found while building A-294 (device nodes kept as metadata), whose device-node versions all share one body.

## Description

A volume with two snapshots, where an inode changed only in its attributes between them (a `chmod`, a `chown`, a
`touch`) and changed again in the head after the second, recovered the second snapshot with the first snapshot's
attributes. Content was never wrong: only the inode's metadata.

Reproduced by `a_version_that_differs_only_in_its_attributes_keeps_them_through_recovery`
(`crates/vfs/tests/recover.rs`): a file, a FIFO and a block device, each made at mode 0o644, snapshot `a`, mode 0o600,
snapshot `b`, mode 0o640, then `to_image` and `from_image`. Before the fix, on macOS arm64 (2026-11-17,
`cargo test -p slates-vfs --test recover a_version_that_differs_only_in_its_attributes_keeps_them_through_recovery`):

```
assertion `left == right` failed: inode InodeNo(1970324836974594): each snapshot's mode and the head's survive recovery
  left: (420, 420, 416)
 right: (420, 384, 416)
```

Snapshot `b` read 0o644 (420), snapshot `a`'s mode, where it held 0o600 (384).

## Root cause

`Volume::capture_snapshot_tree` (`crates/vfs/src/recover.rs`) deduplicates a version that diverged from the head
against the versions earlier snapshots captured in full. It bucketed by inode number and the body's crc, and decided a
match by the body alone. The rebuild of a share (`SharedSource::Snapshot`) takes the earlier snapshot's whole record
from its rebuilt trie: attributes, owner, times, change counter, attribute table. So a later version whose bytes
matched an earlier one's, but whose metadata differed, was recorded as a share and recovered as the earlier version.

Every FIFO and socket version has an empty body, and every version of one device node carries the same number, so for
those kinds any two versions matched. For a file it took an attribute-only change between two snapshots.

## Impact

After a daemon restart (the image is how a successor rebuilds a volume, §4.8), such a snapshot reported the earlier
snapshot's mode, owner, times and change attribute: through its read-only views, a clone made from it, an archive or a
merge origin taken from it, and a landing planned from it. Content was unaffected. A volume with no snapshots, or whose
snapshot versions are all the head's or one another's very records, was unaffected.

## Exact edits

- `crates/vfs/src/recover.rs`: the dedup map keeps each canonical version's whole `InodeImage` (it kept its
  `BodyImage`), and a match needs the whole image equal. The crc still buckets by body. The image format is unchanged,
  so no version moves; an image written before the fix decodes as it did, with its shares as recorded.
- `crates/vfs/tests/recover.rs`: the test above, red before the fix and green after.

## Sibling sweep

- The head share (`SharedSource::Head`) is decided by the record's handle, and the clone-origin share
  (`SharedSource::Origin`) by the record's birth epoch, so each names the very record it shares. Neither compares
  bytes.
- The content store's and the archive's chunk deduplication compare content identities, and a chunk is content alone.
- No other match by body was found in `crates/vfs/src`.

# A takeover left cached what a kernel looked up after the last publication (2026-11-19, A-573)

## Description

Found by reading `crates/server/src/fuse.rs` and the takeover round (A-158), recorded open in GAPS as two of A-158's
siblings, and claimed as B10 of the goal-83 audit. Neither was reproduced. A FUSE mount the anchor holds across a
daemon's death opens its successor's rounds with a takeover round, which invalidates what the kernel may hold that the
successor does not. Two kinds of kernel state outlived it:

- (a) A mutation is applied, then published before its reply. A mount that read the new bytes in between holds bytes
  the successor never had once the daemon dies before the publication: a dirty read surviving the crash. When the
  kernel had looked the inode up after the last publication, the round left those bytes cached.
- (b) A base file the kernel looked up after the last publication, which an outsider wrote in place while no daemon
  ran, kept its old pages in the kernel. Its names and attributes revalidate within the base's timestamp granularity;
  a FUSE mount asks no `FUSE_AUTO_INVAL_DATA`, so its pages stayed.

## Root cause

The round named the root and every inode its owner's restored references and the write-log replay name
(`Volume::visit_takeover_inodes`). That rested on a premise: a kernel holds state older than a change only for an inode
it looked up before the change, and the change's publication recorded that reference (A-61). But a lookup made after
the last publication is in no image, and such a kernel can hold state that is not older than the volume's but other than
it: bytes the death lost (a), or bytes an outsider replaced beneath a base (b). The successor cannot tell which inodes a
kernel looked up after the last publication.

## Impact

After a daemon's death, a process reading through the held mount could be served, from the kernel's cache, bytes no
client was ever told were written (a), or a base file's bytes from before an outsider's edit (b), for as long as the
kernel kept them. Not reproduced: each needs a lookup after the last publication and a death in the window.

## Edits

- `crates/vfs/src/trie.rs`: `visit_after`, an ordered walk of the inode table resuming past any number, bounded per
  slice by the nodes on that number's path and past it; tested against the whole walk and in slices.
- `crates/vfs/src/volume.rs`: `Volume::visit_head_inodes`, every inode of the head in the table's order;
  `visit_takeover_inodes` stays a pinned view's set (its version takes no head change).
- `crates/bridge-core/src/volume_bridge.rs`: `takeover_inodes` names every inode of the head for a view of the head,
  and the references and replay for a pinned view; the trait's doc in `crates/bridge-core/src/lib.rs`.
- Tests (`crates/vfs/src/base.rs`): `a_dirty_read_through_a_lookup_after_the_last_publication_is_named_by_the_takeover`
  (a) and `a_base_file_looked_up_after_the_last_publication_is_named_by_the_takeover_after_an_outsiders_edit` (b), each
  red with the old rule put back.
- Still open (GAPS): a number the dead daemon issued after its last publication (an unpublished create another path
  showed, a base directory first loaded after the publication) is in no image and no head, so no walk names it; the
  successor refuses it (`NotFound`) and never serves it, but pages a descriptor opened before the death holds stay
  until a publication records the counter such numbers were issued below.

//! `slates-image`: the disk-image form of a volume (§4.6, the Firecracker row of `docs/wip/virtiofs.md` §8 item 7;
//! A-160). Firecracker shares no folder with its guest: no virtio-fs, only a block device or a pmem device backed by
//! a file. So a volume reaches a Firecracker guest as an image file holding a file system, and the guest's writes
//! come back by reading that file system out of the image and merging what changed into a clone.
//!
//! **The format is ext2, revision 1** ([`mod@format`]): 4 KiB blocks, 256-byte inodes, the `filetype`, `sparse_super` and
//! `large_file` features, nothing else. Every Linux guest mounts it read-write, through the ext2 driver or through the
//! ext4 driver that serves ext2 on most distribution kernels. Its large inodes carry nanosecond times, and its 4 KiB
//! blocks are the page size DAX on a pmem device needs. It was chosen over ext4 because nothing the image is for needs
//! a journal, extents or checksums, and each would be more for the reader to distrust; over a format of its own
//! because the guest's kernel must mount it with no driver slates ships.
//!
//! **Writing** ([`write::Builder`]) lays a [`tree::Tree`] out and writes it, both in steps a caller bounds, asking an
//! [`write::ImageIo`] for file bytes and handing it the image's blocks. One tree and one [`write::Identity`] always
//! make the same bytes. **Reading** ([`read::Image`]) checks every structure of an image a guest has written before
//! using it, and walks its tree in bounded steps ([`read::Walk`]), returning each node's inode, from which a merge
//! reads the bytes, and its generation, by which a merge knows the nodes the writer wrote. Every tree, written or
//! read, is checked in one place, a directory at a time ([`tree::TreeGrower`]), so no step of the daemon's pays for a
//! whole tree.
//!
//! **Evidence.** Each image is held to e2fsprogs: the round-trip oracle (`tests/round_trip.rs`) writes generated
//! trees and reads them back equal, at the format's precision; the differential (`tests/e2fsck.rs`) runs
//! `e2fsck -fn` on written images and compares `debugfs`'s view of every file; the determinism gate writes one tree
//! twice and compares the bytes; and the hostile-input tests (`tests/hostile.rs`) refuse truncation, bit flips,
//! out-of-range pointers, directory loops and absurd counts by kind. The reference is an image `mkfs.ext2 -b 4096
//! -I 256 -O ^resize_inode,^dir_index,^ext_attr` made on the box (e2fsprogs 1.47.4, 2026-10-23), which `e2fsck -fn`
//! passes with or without `lost+found`; the writer makes none, so a guest sees exactly the volume.
//!
//! **What an image does not carry:** extended attributes (no `ext_attr`), birth times (the change time stands in),
//! and holes (a sparse file takes its whole length; its zero blocks are left unwritten, so a sink that reads unwritten
//! bytes as zero stores none of them). Times outside 1901–2446 are refused, as is anything else the format cannot
//! hold, each by kind ([`error::ImageRefusal`]). The crate is pure: byte buffers only, no I/O, no `unsafe`.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are
// exempt; out-of-bounds indexing and slicing are denied workspace-wide.
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod error;
pub mod format;
pub mod layout;
pub mod read;
pub mod tree;
pub mod write;

pub use error::{Failed, ImageRefusal};
pub use read::{Image, ImageSource, ImageTree, Inode, Walk, Walked};
pub use tree::{Kind, Node, NodeId, Timestamp, Tree, TreeBuilder, TreeGrower};
pub use write::{Builder, Identity, ImageIo, Progress, Written};

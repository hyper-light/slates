//! `slates-vfs` — the copy-on-write volume core as a library: namespace, inodes, content,
//! snapshots, clones, accounting and the journal, with no bridge and no server (design §4.4,
//! §4.5; D-4, D-5, D-6, D-13; Phase 1).
//!
//! The doctrine, in one paragraph. A volume is a tree of directory nodes and an inode table, both
//! copy-on-write by birth epoch [A: Hitz et al., WAFL, USENIX'94; C: OpenZFS `dsl_dataset.c`]:
//! a node born in the current epoch is mutated in place; a node born earlier is copied along its
//! path to the root before the mutation, and the replaced nodes go on the head snapshot's
//! deadlist, so a snapshot is one record and a clone is a fork that pins its origin (D-5).
//! Directories are small sorted arrays that switch to an ordered map with a hash side index at a
//! measured cut-over (D-4). Inode numbers are monotonic and never reused; a slot's generation
//! moves on reuse. File bytes live in open, page-multiple extents until they are sealed into
//! chunks; hashing waits for the seal and dedup for Phase 7 (D-6). Accounting is exact per
//! volume: `referenced_bytes` charges every chunk the head reaches in full, `unique_bytes` what
//! the head alone holds since its last snapshot (D-13), and a bounded volume refuses the byte
//! that would exceed its quota before anything is copied. Every mutation appends a declared
//! operation to the journal. The executable model in the tests is the specification; the
//! implementation must equal it on every generated history (AC-1.1, AC-1.7).
//!
//! Nothing here names a host path: the base plane (`slates-base`) reads the disk and the landing
//! engine (`slates-land`) writes it, both behind this crate's `Body::Base` hooks.
//!
//! Modules: [`ids`], [`names`], [`clock`], [`error`], [`trie`], [`dir`], [`inode`], [`content`],
//! [`snapshot`], [`quota`], [`journal`], [`volume`].

pub mod algebra;
pub mod clock;
pub mod content;
pub mod derive;
pub mod dir;
pub mod dirtree;
pub mod error;
pub mod ids;
pub mod inode;
pub mod journal;
pub mod names;
pub mod quota;
pub mod snapshot;
pub mod trie;
pub mod volume;

pub use error::VfsError;
pub use ids::{Epoch, InodeNo, SnapshotId};
pub use names::NameEquivalence;
pub use volume::{Volume, VolumeConfig};

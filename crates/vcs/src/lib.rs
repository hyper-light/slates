//! `slates-vcs`: the one object store the version-control servers share (§4.17.1 "Objects: one copy, indexed by the
//! tool's id", items 1, 3, 4 and 5; shared piece 3; A-581; D-28).
//!
//! A git, Mercurial or Subversion server keeps its tool's objects: git's blobs, trees, commits and tags, Mercurial's
//! changesets, manifests and file revisions, Subversion's node revisions and their texts. Each tool names an object by
//! a hash of its own (git's SHA-1 or SHA-256 of `<type> <length>\0<content>`, Mercurial's SHA-1 of the sorted parents
//! and the text, Subversion's MD5 and SHA-1 of a file's text). slates keeps every object's raw bytes once, as content
//! named by BLAKE3 identity, whichever tool brought it, and gives each repository an index from its tool's id to the
//! row it keeps for the object. Five parts:
//!
//! - [`content`]: the content store. Bytes by identity, one copy however many objects, tools and repositories name
//!   them, held by count and freed with their last holder, within a budget the caller derives from its shard's memory
//!   (§4.2). No content is longer than the content plane's window (D-6), the one size the owner derives and gives the
//!   store: an object longer than that is held as its windows, cut where a volume cuts the same file's bytes, under
//!   lists of their identities, so the object's windows and a checkout's are one identity each. Whole-object identity
//!   within a window is the deduplication the design chose first: whole-file deduplication captures about three
//!   quarters of block-level deduplication's gain [A: Meyer and Bolosky, "A Study of Practical Deduplication", FAST
//!   2011; D-6].
//! - [`table`]: an immutable table of rows sorted by key, at most one window, encoded as bytes that are themselves
//!   content, so a table is named, shared and checked by its identity like any other content.
//! - [`index`]: a repository's index, runs of tables under a mutable tail: the log-structured merge tree's shape [A:
//!   O'Neil, Cheng, Gawlick and O'Neil, "The Log-Structured Merge-Tree", Acta Informatica 1996]. Rows go into the tail;
//!   once they no longer fit one table the tail seals into a run; a merge reads the newest runs this index alone holds
//!   into one. A fork holds its parent's tables once more and copies none, so it costs a count per table and adds only
//!   rows for its own objects. The table's size and the tail's length are the derived window; which runs to merge,
//!   and when, is learned from the deployment's own measured times, never a constant ([`rule`]).
//! - [`arrival`]: every object checked against its tool's id as it arrives, by the tool's own naming, a parameter
//!   ([`Naming`]), and held by the push that brought it, not by the repository, until the verdict commits: git's
//!   quarantine [B: git-receive-pack, "QUARANTINE ENVIRONMENT"]. An object whose bytes do not hash to the id the
//!   client claimed, or that its tool refuses to name, is refused by kind, and nothing of its push is indexed; a
//!   refused or abandoned push leaves nothing behind.
//!
//! - [`derived`]: derived data, one mechanism (A-645, A-647): every structure that is not a change (a version's map of
//!   paths to contents, and the tool stores as they become types) is a pure function of a version's change and its
//!   parents' values of the same type, kept immutable under the type's name and the version's, computed eagerly or
//!   lazily by the type's own declaration, a unit at a time under the shard's slices. Its first type is
//!   [`derived::paths`], a version's map of paths to contents.
//!
//! **What holds what.** A content is held once by each list entry, tail row and quarantined object that names it, and
//! once by each stored table whose rows name it, however many indexes hold that table. A table is held once by each
//! index whose runs name it. So a fork, which holds its parent's tables, holds their rows' content through them, and
//! the content of a row lives exactly as long as some tail, table or quarantine names it. The oracle test holds the
//! whole store to a model of plain maps over generated histories of arrivals, commits, seals, merges, forks and
//! releases, and requires the store empty once everything is released.
//!
//! **One matcher besides.** [`wildmatch`] is git's glob, `wildmatch` with `WM_PATHNAME` (A-597), kept here because
//! this is the crate every version-control server's crate builds on: each one that matches a pattern holds the one
//! matcher, never a copy (A-671): slates-server's policy now, git's sparse filter next (A-640 part 4).
//!
//! **Every bound is the caller's.** The content budget and the window are derived by the owner from its shard's memory
//! and pages (§4.2, D-6, D-11), and the budget may be derived again as the shard's pressure moves; an index's tail and
//! its runs' fences are charged against the same budget. Nothing in this crate is a tuning constant: the formats'
//! widths are the encodings' own, and the merge rule's inputs are times the caller measured.
//!
//! **Owed (later pieces, each stated in the design's status for §4.17.1):** the work sliced under the shard's budget
//! (a merge, a release and an abandoned push are each one call here, linear in their rows); one content index for this
//! store and the volumes' sealed windows, so a checkout and a blob are one copy (piece 4, Phase 7); pruning objects no
//! branch reaches; each tool's own naming implemented (git's, `slates_git::object_name`, and Mercurial's node), which
//! its server's pieces bring; and the wire forms cached by identity (§4.17.1 "Objects" item 2).
//!
//! No I/O, no clock, no randomness, no panic.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt;
// out-of-bounds indexing and slicing are denied workspace-wide.
// The panic rule: a print macro panics on a refused write, and this crate prints nothing.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod arrival;
pub mod cold;
pub mod content;
pub mod cut;
pub mod derived;
pub mod index;
pub mod rule;
pub mod share;
pub mod table;
pub mod usage;
pub mod wildmatch;

pub use arrival::{
  AdmitRefused, Admitted, Arriving, CommitRefused, Naming, Quarantine, object_payload, object_row,
};
pub use cold::{Prices, Rebuildable, choose};
pub use content::{
  ContentCounters, ContentId, ContentRefused, ContentStore, DerivedNode, Form, Stored, Windows,
};
pub use derived::{
  Derivations, DerivedCounters, DerivedRefused, DerivedType, Fill, Graph, Mode, Progress, Read,
};
pub use index::{AdoptRefused, Batch, Index, IndexCounters, IndexRefused, Inserted, NewRow};
pub use rule::Compaction;
pub use share::{Claim, Granted, amount_of, divide, whole_units};
pub use table::{RowRef, TableRefused, TableView};
pub use usage::{Ledger, PrincipalCount, Usage, Work, WorkUsage};

#[cfg(test)]
mod tests;

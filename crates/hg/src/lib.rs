//! Mercurial's exchange formats, decoded and encoded as pure functions over bytes (A-514; §4.17.3 "Pieces that
//! follow", piece 1).
//!
//! A Mercurial client and server exchange *bundles*. A bundle2 stream (`HG20`) is a run of typed parts with
//! parameters and chunked payloads; the legacy bundle1 (`HG10`) is one changegroup. A changegroup carries the
//! revisions of the changelog, the manifests and every file, each a delta against a base the receiver holds or
//! receives earlier. This crate reads and writes the three framings ([`bundle2`], [`bundle1`], [`changegroup`]) and
//! the deltas inside them ([`delta`]), and the part payloads a push and a pull carry ([`phases`], [`bookmarks`],
//! [`heads`], [`obsmarkers`], [`pushkey`]; A-545), byte for byte as Mercurial 7.2 writes them, and nothing more: it hashes no node
//! (piece 3), runs no compression engine (a stream compressed with `GZ`, `BZ` or `ZS` is refused by kind until the
//! engines are wired, D-17), and handles no part (piece 5).
//!
//! **Sources** (read 2026-11-18 and 2026-11-19; `docs/wip/research/mercurial-server.md`). Mercurial 7.2's
//! `internals.bundle2`, `internals.bundles` and `internals.changegroups` [B], and its code [C], which is what clients
//! run and is followed where the two disagree: `bundle2.py` (every size a signed 32-bit integer; a part type of
//! `[a-zA-Z0-9_:-]`, mandatory when it holds an upper-case letter; the interrupt), `changegroup.py` (the delta headers
//! of versions `01` to `05`; a chunk length of 1 to 4 refused as a negative one is; the tree-manifest list every
//! version from `03` carries, empty or not) and `mpatch.c` (a delta's hunks).
//!
//! **Hostile input (§4.9).** Every byte is untrusted. Every length is read as Mercurial reads it, a signed big-endian
//! 32-bit integer, so `0xFFFF_FFFF` is −1: a refused length, or in a bundle2 payload the interrupt. Every length is
//! checked against the bytes left before anything is read, no structure is walked by recursion, and every refusal is
//! a typed variant naming what it found.
//!
//! **Caps, none of them a literal.** Two bound a decode: the input itself (a length past the bytes left is
//! [`Truncated`]), and the caller's [`Admission`], the room the request may use for what a decode builds beside the
//! input it borrows, drawn from the request's budget (§4.2, A-16). A decode walks its input twice: once to check it and
//! count what it holds, charging the exact total to the admission, and once to build, so every vector is allocated
//! once at its final size, and bundle2 parts nested by interrupts, however deep, cost their entries and no stack.
//!
//! **Golden vectors.** `fixtures/` holds bundles stock Mercurial 7.2 made of one small repository (a named branch, a
//! copy, a merge, a subdirectory, a draft amended under evolution) as changegroup versions `01` to `04`, with 7.2's
//! own `hg debugbundle` listing of each. `fixtures/make-fixtures.sh` made them, run once on the Mac Studio on
//! 2026-11-19 with Mercurial 7.2 from PyPI through `uvx`; its header names the exact command. The tests decode each
//! bundle, hold every revision's nodes, parents, link and delta length to 7.2's listing, rebuild every file's text
//! from the deltas, and encode each bundle back to its own bytes.

mod admission;
pub mod bookmarks;
pub mod bundle1;
pub mod bundle2;
pub mod changegroup;
pub mod delta;
pub mod heads;
pub mod obsmarkers;
mod out;
pub mod phases;
pub mod pushkey;
mod read;

pub use admission::{Admission, PastAdmission};

/// Format: the bytes of a node, the SHA-1 that names a changeset, a manifest or a file revision
/// (`internals.revlogs` "Hash Computation").
pub const NODE_LEN: usize = 20;

/// A node: the identifier of a changeset, a manifest revision or a file revision.
pub type Node = [u8; NODE_LEN];

/// The null node, all zeros: "no parent", and the base a full text is sent against.
pub const NULL_NODE: Node = [0; NODE_LEN];

/// A length the input cannot hold: what a read needed past the bytes that were left (§4.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Truncated {
  /// What was being read.
  pub what: &'static str,
  /// Where the read began, as an offset into the bytes being read.
  pub at: u64,
  /// The bytes the read needed.
  pub needed: u64,
  /// The bytes that were left.
  pub left: u64,
}

#[cfg(test)]
#[path = "tests/fixtures.rs"]
pub(crate) mod fixtures;

#[cfg(test)]
#[path = "tests/payloads.rs"]
mod payload_tests;

//! Mercurial's exchange formats, decoded and encoded as pure functions over bytes (A-514; §4.17.3 "Pieces that
//! follow", piece 1).
//!
//! A Mercurial client and server exchange *bundles*. A bundle2 stream (`HG20`) is a run of typed parts with
//! parameters and chunked payloads; the legacy bundle1 (`HG10`) is one changegroup. A changegroup carries the
//! revisions of the changelog, the manifests and every file, each a delta against a base the receiver holds or
//! receives earlier. This crate reads and writes the three framings ([`bundle2`], [`bundle1`], [`changegroup`]) and
//! the deltas inside them ([`delta`]), and the part payloads a push and a pull carry ([`phases`], [`bookmarks`],
//! [`heads`], [`obsmarkers`], [`pushkey`]; A-545), and the wire protocol's version 1 framings both transports use
//! ([`ssh`], [`http`], their arguments [`wire`], the [`capabilities`] and [`batch`]; A-569), and the objects a
//! repository holds with their nodes ([`changeset`], [`manifest`], [`filerev`], [`node`]; A-588), and the revlogs an
//! existing store keeps them in ([`revlog`]; A-611: their zlib and zstd chunks inflated through A-607's engines), byte
//! for byte as Mercurial 7.2 writes them, and nothing more: it keeps no index (piece 3's next part), handles no part
//! (piece 5), and serves no command (piece 4). A bundle compressed with `GZ`, `BZ` or `ZS` is made plain by
//! [`compression`] (A-598): its zlib, bzip2 or zstd stream inflated within the caller's admission, a compression bomb
//! refused by kind; and a plain bundle is compressed as 7.2 compresses one.
//!
//! **Sources** (read 2026-11-18 and 2026-11-19; `docs/wip/research/mercurial-server.md`). Mercurial 7.2's
//! `internals.bundle2`, `internals.bundles` and `internals.changegroups` \[B\], and its code \[C\], which is what clients
//! run and is followed where the two disagree: `bundle2.py` (every size a signed 32-bit integer; a part type of
//! `[a-zA-Z0-9_:-]`, mandatory when it holds an upper-case letter; the interrupt), `changegroup.py` (the delta headers
//! of versions `01` to `05`; a chunk length of 1 to 4 refused as a negative one is; the tree-manifest list every
//! version from `03` carries, empty or not) and `mpatch.c` (a delta's hunks); for the objects (A-588), `changelog.py`
//! (a changeset's text and its extras' escapes), `cext/manifest.c` (a manifest's lines), `utils/storageutil.py` and
//! `filelog.py` (a file revision's metadata block, its copy and its parents' order), `utils/hashutil.py` and
//! `thirdparty/sha1dc/cext.c` (the node's SHA-1).
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
//! from the deltas, and encode each bundle back to its own bytes. `fixtures/payloads/` (A-545) holds what 7.2's client
//! sent and its server answered in three pushes over SSH, captured on the wire, and two obsstores 7.2 wrote, made by
//! `fixtures/payloads/make-payload-fixtures.sh` the same way; every part payload in them is decoded to what 7.2's
//! listings and logs say and encoded back to its bytes. `fixtures/objects/` (A-588) holds every revision of one
//! repository's changelog, manifest log and file logs as 7.2's `debugdata` wrote it, each log as its `debugindex
//! --debug` lists it, and what 7.2's own readers (`changectx`, `manifest`, `filelog.renamed`) read from each, made by
//! `fixtures/objects/make-object-fixtures.sh` twice, byte for byte alike; every node is computed from its parents and
//! text and every object read as 7.2 reads it and written back to its bytes. `fixtures/compressed/` (A-598) holds the
//! bundles 7.2 wrote of the repository `fixtures/cg01.hg` and `cg02.hg` hold, compressed (bundle1 with gzip and bzip2,
//! bundle2 with gzip, bzip2 and zstd), made by `fixtures/compressed/make-compressed-fixtures.sh` and checked by its
//! `SHA256SUMS`; each is made plain to exactly the uncompressed bundle 7.2 wrote.
//!
//! **Nodes are SHA-1 with collision detection** (A-588, [`node`]), as 7.2's `hashutil.sha1` is sha1dc's wherever its C
//! extension is built (the fixtures' runs printed it): a known collision attack is refused by kind, as 7.2 raises on
//! one, and `fixtures/sha1dc/` holds SHAttered's two prefixes (git's t0013 vector and its twin, as A-600 cut them) that
//! prove it.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt;
// out-of-bounds indexing and slicing are denied workspace-wide.
// The panic rule: a print macro panics on a refused write, and this crate prints nothing.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

mod admission;
pub mod batch;
pub mod bookmarks;
pub mod bundle1;
pub mod bundle2;
pub mod capabilities;
pub mod changegroup;
pub mod changeset;
pub mod compression;
mod decimal;
pub mod delta;
pub mod filerev;
pub mod heads;
pub mod http;
pub mod index;
mod lines;
pub mod manifest;
pub mod node;
pub mod obsmarkers;
mod out;
pub mod phases;
pub mod pushkey;
mod quote;
mod read;
pub mod revlog;
pub mod ssh;
pub mod wire;

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

#[cfg(test)]
#[path = "tests/wire.rs"]
mod wire_tests;

#[cfg(test)]
#[path = "tests/objects.rs"]
mod object_tests;

#[cfg(test)]
#[path = "tests/revlogs.rs"]
mod revlog_tests;

#[cfg(test)]
#[path = "tests/index.rs"]
mod index_tests;

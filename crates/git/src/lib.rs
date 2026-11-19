//! git's wire formats, decoded and encoded as pure functions over bytes (A-582; §4.17.2 "Pieces (git)", piece 1, the
//! framing every later piece speaks).
//!
//! A git client and server exchange *pkt-lines* ([`pktline`]): four hexadecimal digits of length, which count
//! themselves, then the data; three special packets carry no data, the flush (`0000`), and protocol v2's delimiter
//! (`0001`) and response end (`0002`). A pack travels inside the `packfile` section multiplexed by side-band-64k
//! ([`sideband`]): each data packet's first byte names its band, 1 the pack, 2 progress, 3 a fatal error. Under
//! protocol v2's `sideband-all` every data packet of a response is multiplexed so. This crate reads and writes both,
//! byte for byte as stock git 2.50 writes them. It also names objects ([`object_name`], A-600): the object header, and an
//! object's name in either `object-format`, SHA-1 with git's collision detection or SHA-256. And it speaks protocols v0
//! and v1 for clients older than v2 ([`v0`], A-604): the ref advertisement with its capabilities, a fetch's request and
//! negotiation lines, and the shallow-update. Protocol v2's messages, deltas, pack indexes and packs are A-582's next
//! parts (GAPS, the §4.17 entry).
//!
//! **Sources** (read 2026-11-19). git's `Documentation/gitprotocol-common.txt` ("pkt-line Format"),
//! `gitprotocol-v2.txt` ("Packet-Line Framing", "packfile section", `sideband-all`) and `gitprotocol-capabilities.txt`
//! ("side-band, side-band-64k") \[B\]; and git's `pkt-line.c` and `sideband.c` \[C\], which is what clients run, followed
//! where the documents leave room: a length digit may be of either case (`hexval`), `0003` is refused ("bad line
//! length"), `0004` is a data packet with no data, and a band other than 1, 2 and 3 is refused ("bad band").
//!
//! **Hostile input (§4.9).** Every byte is untrusted. A length is checked against [`pktline::MAX_LEN`], git's own
//! limit, and against the bytes left, before any byte past it is read; nothing is walked by recursion; every refusal is a
//! typed variant naming where it stopped.
//!
//! **Caps, none of them a literal.** Two bound a decode: the input itself (a packet past the bytes left is not yet whole
//! to a reader, and truncated at a whole stream's end), and the caller's [`Admission`], the room the request may use for
//! what a decode builds beside the input it borrows, drawn from its budget (§4.2, A-16). A whole stream's decode walks
//! it twice: once to check it and count its packets, charging the exact total to the admission, and once to build. The
//! format's own limits (a packet's 65520 bytes; side-band-64k's 65515 after the band byte) are git's, cited where they
//! are defined.
//!
//! **Golden vectors.** `fixtures/` holds what stock git 2.50.1 (Apple Git-155) said on its own wire to requests this
//! crate's script wrote: protocol v2's capability advertisement (plain, and with `sideband-all` and `packfile-uris`
//! allowed), an `ls-refs` request and answer, and `fetch` requests and answers carrying each section (a clone,
//! acknowledgments ending `ready`, `shallow-info`, `wanted-refs`, progress on band 2, and `packfile-uris` under
//! `sideband-all`), with packs and `.idx` files for the parts that follow. `fixtures/make-fixtures.sh` made them, run
//! once on the Mac Studio on 2026-11-19; its header names the command, and why one file's bytes vary between runs.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt;
// out-of-bounds indexing and slicing are denied workspace-wide.
// The panic rule: a print macro panics on a refused write, and this crate prints nothing.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

mod admission;
pub mod delta;
pub mod entry;
pub mod http;
pub mod lfs;
pub mod negotiation;
pub mod object_name;
pub mod objects;
pub mod out;
pub mod pack;
pub mod pktline;
pub mod receive;
pub mod refname;
pub mod sideband;
pub mod store;
pub mod unpack;
pub mod v0;
pub mod v2;

pub use admission::{Admission, PastAdmission};

/// A count or a length as `u64`. Every `usize` fits on the targets slates ships (Appendix B.2), and one that did not
/// would saturate, which every caller then refuses, rather than wrap.
pub(crate) fn to_u64(value: usize) -> u64 {
  u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "tests/fixtures.rs"]
mod fixtures;

#[cfg(test)]
#[path = "tests/pack_fixtures.rs"]
mod pack_fixtures;

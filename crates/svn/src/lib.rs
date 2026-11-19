//! `slates-svn`: the Subversion server's pure pieces (§4.17.4, A-500; D-28).
//!
//! slates serves stock Subversion clients over their own protocols (`svn://`, `svn+ssh://`, `http(s)://`), a
//! repository being one green whose version N is revision N (§4.17.1). This crate holds the parts of that server that
//! are pure functions, each proven on every host before anything drives it: svndiff, the delta format both protocols
//! carry file contents in ([`svndiff`], A-500); the ra_svn item codec, the words, numbers, strings and lists every
//! `svn://` message is made of ([`ra_svn`], A-531); and the Subversion index, every node-revision with the identities
//! FSFS gives it, computed from the revisions' changed-path records ([`index`], A-553). The commit rule follows as its
//! own piece (§4.17.4 "Pieces that follow").
//!
//! Nothing here performs I/O, reads a clock or draws randomness; every refusal is a typed value that names the
//! Subversion error a client is answered with. The evidence is `docs/wip/research/vcs-servers-subversion.md`.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt;
// out-of-bounds indexing and slicing are denied workspace-wide.
// The panic rule: a print macro panics on a refused write, and this crate prints nothing.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod dump;
pub mod index;
pub mod ra_svn;
pub mod svndiff;

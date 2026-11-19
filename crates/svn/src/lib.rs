//! `slates-svn`: the Subversion server's pure pieces (§4.17.4, A-500; D-28).
//!
//! slates serves stock Subversion clients over their own protocols (`svn://`, `svn+ssh://`, `http(s)://`), a
//! repository being one green whose version N is revision N (§4.17.1). This crate holds the parts of that server that
//! are pure functions over bytes, each proven on every host before anything drives it: today svndiff, the delta format
//! both protocols carry file contents in ([`svndiff`]). The ra_svn item codec, the Subversion index and the commit rule
//! follow as their own pieces (§4.17.4 "Pieces that follow").
//!
//! Nothing here performs I/O, reads a clock or draws randomness; every refusal is a typed value that names the
//! Subversion error a client is answered with. The evidence is `docs/wip/research/vcs-servers-subversion.md`.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt;
// out-of-bounds indexing and slicing are denied workspace-wide.
// The panic rule: a print macro panics on a refused write, and this crate prints nothing.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod svndiff;

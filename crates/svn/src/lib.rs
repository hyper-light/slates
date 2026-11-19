//! `slates-svn`: the Subversion server's pure pieces (§4.17.4, A-500; D-28).
//!
//! slates serves stock Subversion clients over their own protocols (`svn://`, `svn+ssh://`, `http(s)://`), a
//! repository being one green whose version N is revision N (§4.17.1). This crate holds the parts of that server that
//! are pure functions, each proven on every host before anything drives it: svndiff, the delta format both protocols
//! carry file contents in ([`svndiff`], A-500); the ra_svn item codec, the words, numbers, strings and lists every
//! `svn://` message is made of ([`ra_svn`], A-531); the Subversion index, every node-revision with the identities FSFS
//! gives it, computed from the revisions' changed-path records ([`index`], A-553); the lock table with the check a
//! commit meets, each guard returning the register entries it decided ([`lock`], A-602); the commit rule, the drive's
//! out-of-date test and FSFS's merge at commit stated on that index, whether a work built on a base revision commits on
//! the head or which conflict refuses it, its lock check the table's ([`commit`], A-592); the HTTP side's bodies, the
//! XML stock clients send over `http://` and `mod_dav_svn` answers, read within bounds and written byte for byte
//! ([`dav`], A-609); and authorization, the authz file read as Subversion reads it and evaluated with its precedence
//! for each principal ([`authz`], A-606).
//!
//! Nothing here performs I/O, reads a clock or draws randomness; every refusal is a typed value that names the
//! Subversion error a client is answered with. The evidence is `docs/wip/research/vcs-servers-subversion.md`.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt;
// out-of-bounds indexing and slicing are denied workspace-wide.
// The panic rule: a print macro panics on a refused write, and this crate prints nothing.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod authz;
pub mod commit;
pub mod dav;
pub mod handshake;
pub mod index;
pub mod lock;
pub mod mergeinfo;
pub mod ra_svn;
pub mod reads;
pub mod svndiff;

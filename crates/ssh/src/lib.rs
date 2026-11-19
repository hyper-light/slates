//! SSH's transport, the layer git, Mercurial and Subversion's SSH routes all stand on (A-594; §4.17.2 "Transports"
//! item 2 and "Pieces (git)" piece 9, its first part). It is a sans-io core: the caller moves bytes between a socket and
//! [`transport::ServerTransport`], hands it the time, and reads what it decided as events. Nothing here touches a
//! socket, a clock, a file or a thread.
//!
//! **What it does** (RFC 4253, with the rules OpenSSH and the SSHM drafts made standard since):
//! - the identification exchange ([`ident`]): the server's banner lines and version line, the client's line read within
//!   RFC 4253 §4.2's 255 bytes;
//! - the binary packet protocol ([`packet`]): lengths, padding and alignment checked before a body is read, sealing and
//!   opening under `chacha20-poly1305@openssh.com` and `aes256-gcm@openssh.com`, sequence numbers;
//! - algorithm negotiation ([`kexinit`]): the client's first choice the server also has, per list, with the markers of
//!   strict key exchange (the Terrapin countermeasure) and of RFC 8308's extension negotiation never chosen as methods;
//! - `curve25519-sha256` (RFC 8731) with `ssh-ed25519` host keys (RFC 8709) ([`kex`], [`hostkey`]): the exchange hash,
//!   the key derivation of RFC 4253 §7.2 and `SSH_MSG_NEWKEYS`;
//! - rekeying ([`keylife`]): by each cipher's own bounds (RFC 4344 §3) and by a key time the deployment learns.
//!
//! User authentication (RFC 4252) and channels (RFC 4254) are the next pieces; they read [`transport::Event::Message`]
//! and send through [`transport::ServerTransport::send`].
//!
//! **Sources** (read 2026-11-19; research notes §5.5 pin each by hash). RFC 4250, 4251, 4253, 4344, 5647, 5656, 7748,
//! 8032, 8308, 8709 and 8731 [B]; OpenSSH's `PROTOCOL` at `fa4b2a82` [B], which since moved its three transport
//! extensions into the SSHM working group's drafts: `draft-ietf-sshm-strict-kex-03`, `draft-ietf-sshm-chacha20-poly1305-04`
//! and `draft-miller-sshm-aes-gcm-01` [B]; OpenSSH's `kex.c` and `packet.c` at the same commit [C], followed where the
//! documents leave room, each place saying so.
//!
//! **Bounds, none of them a tuning literal.** A packet is held to the caller's cap, which RFC 4253 §6.1 floors at 35000
//! bytes, and its length is checked before its body is read. One packet is buffered at a time. One event waits at a
//! time: input stops while it does, and while the output holds a packet cap of bytes the caller has not written, so a
//! peer that stops reading cannot grow the server's memory. A name is at most RFC 4251 §6's 64 bytes, an identification
//! line RFC 4253 §4.2's 255. A key is retired by the bounds its cipher's analysis gives (RFC 4344 §3) and by the key time
//! the caller passes ([`keylife::key_time_limit_ns`]).
//!
//! **Hostile input (§4.9).** Every byte from the client is untrusted: every length is checked against what is left and
//! against its cap before anything past it is read or allocated; every refusal is a typed variant of [`refusal::Refusal`]
//! that names its SSH disconnect reason, and the transport sends that disconnect before it closes.
//!
//! **Golden vectors.** The chacha20-poly1305 draft's worked packet (Appendix A), sealed and opened byte for byte; NIST
//! CAVS 14.1's SSH key-derivation vectors (AWS-LC's `sshkdf_tests.txt` at the vendored commit); RFC 7748 §6.1's X25519
//! exchange; RFC 8032 §7.1's Ed25519 tests 1 to 3. Whole handshakes run against a client written for the tests from the
//! RFCs alone (`tests/client.rs`), sharing no code with the server's exchange hash, key derivation or packet sealing.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt;
// out-of-bounds indexing and slicing are denied workspace-wide.
// The panic rule: a print macro panics on a refused write, and this crate prints nothing.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod entropy;
pub mod hostkey;
pub mod ident;
pub mod kex;
pub mod kexinit;
pub mod keylife;
pub mod packet;
pub mod refusal;
pub mod transport;
pub mod wire;

/// A count or a length as `u64`. Every `usize` fits on the targets slates ships (Appendix B.2), and one that did not
/// would saturate, which every caller then refuses, rather than wrap.
pub(crate) fn to_u64(value: usize) -> u64 {
  u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "tests/support.rs"]
mod support;

#[cfg(test)]
#[path = "tests/client.rs"]
mod client;

#[cfg(test)]
#[path = "tests/wire.rs"]
mod wire_tests;

#[cfg(test)]
#[path = "tests/ident.rs"]
mod ident_tests;

#[cfg(test)]
#[path = "tests/packet.rs"]
mod packet_tests;

#[cfg(test)]
#[path = "tests/kexinit.rs"]
mod kexinit_tests;

#[cfg(test)]
#[path = "tests/kex.rs"]
mod kex_tests;

#[cfg(test)]
#[path = "tests/hostkey.rs"]
mod hostkey_tests;

#[cfg(test)]
#[path = "tests/keylife.rs"]
mod keylife_tests;

#[cfg(test)]
#[path = "tests/transport.rs"]
mod transport_tests;

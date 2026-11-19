//! The HTTP edge's engine for slates' version-control servers (A-625; §4.17.2 "Transports" item 1 and "Pieces (git)"
//! piece 8's next part): one connection's request loop over `slates-http`, as a state machine over bytes that holds no
//! socket, no clock and no thread. The daemon's listener moves bytes between a socket and a [`connection::Connection`],
//! waits as long as each [`connection::Phase`]'s learned bound allows, and answers the questions the engine asks: what a
//! head's route is, who its credentials prove, and what that principal may do there (§4.13, §4.17.1), each a
//! [`connection::Decision`]. git's smart HTTP is the first site; Mercurial's (§4.17.3) and Subversion's (§4.17.4) are
//! served by the same engine, which knows no tool.
//!
//! **A connection** ([`connection`]) speaks HTTPS ([`tls`]: rustls's server session over aws-lc-rs, TLS 1.3 and 1.2,
//! ALPN `http/1.1`), or cleartext on a loopback listener alone. Its request loop:
//! - a head read by `slates-http`'s head reader as its bytes arrive, charged to the request's admission;
//! - the decision asked for, and `100 Continue` sent when the client waits for it before its body (RFC 9110 §10.1.1);
//! - the body's framing and gzip coding removed as it streams, each run handed to the service and taken before more is
//!   read, so a push's pack passes in bounded memory (§4.17.2 "A push" item 3), never held whole;
//! - the service's reply framed in chunks to HTTP/1.1 and by the connection's close to HTTP/1.0 (RFC 9112 §6.1);
//! - the connection kept for the client's next request unless the request, its version or a refusal ends it.
//!
//! **Refusals** are typed and carry their status: the head's (`slates-http`'s `HeadRefused`), the body's framing and
//! coding (`BodyRefused`, `GzipRefused`), the credentials' ([`credential::CredentialRefused`]), and the site's own
//! decision (401 with its challenge, 403, 404 and the rest). A refusal whose request leaves no body unread is answered and
//! the connection kept; any other ends it in stages (RFC 9112 §9.6; A-183's lesson): the reply, a half-close, then the
//! client's remaining bytes read and dropped, so the client reads the refusal rather than a reset that discards it.
//!
//! **Credentials** ([`credential`]): Basic (RFC 7617), a token as its password, over TLS alone; Bearer (RFC 6750) over
//! TLS or a loopback listener. Each ends in a principal of §4.13 by the site's authenticator; a request without them is
//! answered 401 with the challenges this connection accepts.
//!
//! **Bounds, none of them a tunable.** Each request has the admission the listener derives from its shard's budget: the
//! head's bytes and index and every inflated byte are charged to it. The engine reads only when it has handed on what it
//! holds and writes before it reads again, so a connection holds at most one read's bytes beside its head. The waits
//! between those steps are the listener's, each learned per phase.
//!
//! **Sources** (read 2026-11-19): RFC 9110, RFC 9112, RFC 7617, RFC 6750, RFC 8446, RFC 7301 \[B\]; rustls 0.23's
//! `ServerConnection` \[C\]; A-183 (the staged close the MCP edge learned on Windows, where a reset discards what the
//! client has not read).

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt;
// out-of-bounds indexing and slicing are denied workspace-wide.
// The panic rule: a print macro panics on a refused write, and this crate prints nothing.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod connection;
pub mod credential;
pub mod tls;

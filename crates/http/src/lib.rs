//! HTTP/1.1's request codec and reply framing for slates' version-control servers (A-595; §4.17.2 "Transports" 1,
//! piece 8's first part), as pure functions and state machines over bytes. git's smart HTTP (§4.17.2), Mercurial's HTTP
//! (§4.17.3) and Subversion's WebDAV (§4.17.4) read every request with it and route it themselves, so one reading of
//! HTTP/1.1 serves the three tools, and the edge that reads sockets (piece 8's next part) holds no HTTP rule of its own:
//! it feeds bytes in and writes the bytes this crate makes. It is a crate of its own, not a module of `slates-git`, so
//! that the Mercurial and Subversion servers share it without depending on git's.
//!
//! **A request** is a head and a body. [`head::HeadReader`] reads the head (RFC 9112 §2 to §5) from the bytes a
//! connection has delivered, resuming where it stopped, and yields a [`head::Head`] that borrows them: the method, the
//! target as its path and query, the fields in order, and what they decide: the framing (RFC 9112 §6.3), the content
//! coding (RFC 9110 §8.4), whether the connection is kept and whether the client waits for `100 Continue` (RFC 9110
//! §10.1.1). [`body::BodyReader`] removes the framing, `Content-Length` or chunked (RFC 9112 §7.1), as the bytes
//! arrive, handing back each run of data as a slice of its input, so a body of any size streams through in constant
//! memory, as a push's pack must (§4.17.2 "A push" item 3). [`gzip::Gunzip`] inflates a gzip body (RFC 1952) as it
//! arrives. [`reply`] writes a reply's head and chunks, and [`percent`] decodes a target's escapes (RFC 3986 §2.1).
//!
//! **Strict where two readers could differ.** A request that two HTTP readers could frame differently is refused, never
//! repaired, because a proxy in front of slates that framed it the other way would smuggle a second request through
//! (RFC 9112 §11.2): both `Content-Length` and `Transfer-Encoding`; a transfer coding in an HTTP/1.0 request; chunked
//! not last, or twice; lengths that disagree; a bare CR or LF, a NUL or another control byte in a head or in a chunked
//! body's lines; whitespace before a field's colon; a folded line. Each refusal is a typed kind that names the status it
//! is answered with, and a refusal of a head or of a body's framing ends its connection.
//!
//! **Bounds, none of them a tunable.** Every byte a request holds is charged to its [`Admission`], the room its budget
//! gives it (§4.2, A-16; the caller derives it, and nothing here sets it): a head's bytes and its field index before the
//! head is built, a trailer section's bytes, and every byte gzip inflates. So a head that never ends and a gzip bomb both
//! stop at the request's own room. One format constant bounds a line: [`MAX_LINE`], RFC 9112 §3's 8000 octets. Every line
//! this crate reads is held to it (a request line, a field line, a chunk-size line with its extensions, a trailer line),
//! as RFC 9112 §7.1.1 asks a server to limit chunk extensions "in the same way that it applies length limitations ... for
//! other parts of a message". The MCP edge holds its lines to the same bound.
//!
//! **Reused, and not.** The MCP edge's HTTP/1.1 reader (`crates/mcp/src/http.rs`) serves one endpoint on the runtime:
//! its head parser keeps MCP's own fields, takes `POST` alone, requires `Content-Length` and refuses every transfer
//! coding, and its crate depends on the runtime and the client, which a pure codec must not. This crate takes its
//! grammar and its line bound with their citations, and leaves its staged refusal (RFC 9112 §9.6: the reply, a
//! half-close, then the client's bytes drained) to the edge that writes sockets. Moving the MCP edge onto this crate is
//! owed (GAPS, the §4.17 entry).
//!
//! **Sources** (read 2026-11-19): RFC 9110 and RFC 9112 \[B\]; RFC 1952 (gzip) and RFC 3986 (URIs) \[B\]; git's
//! `http-backend.c` and `remote-curl.c` \[C\], the server and the client this crate is first proven against (`slates-git`'s
//! golden vectors, stock git's own HTTP traffic).

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds are exempt;
// out-of-bounds indexing and slicing are denied workspace-wide.
// The panic rule: a print macro panics on a refused write, and this crate prints nothing.
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

mod admission;
pub mod body;
mod grammar;
pub mod gzip;
pub mod head;
pub mod percent;
pub mod reply;

pub use admission::{Admission, PastAdmission};

/// Format: the longest line read, its CRLF not counted: a request line, a field line, a chunk-size line with its
/// extensions, or a trailer line (RFC 9112 §3: "It is RECOMMENDED that all HTTP senders and recipients support, at a
/// minimum, request-line lengths of 8000 octets"). The MCP edge holds its lines to the same bound.
pub const MAX_LINE: usize = 8000;

/// Format: the line break that ends every line of a head and of a chunked body's framing (RFC 9112 §2.1).
pub const CRLF: &[u8] = b"\r\n";

/// A count or a length as `u64`. Every `usize` fits on the targets slates ships (Appendix B.2), and one that did not
/// would saturate, which every caller then refuses, rather than wrap.
pub(crate) fn to_u64(value: usize) -> u64 {
  u64::try_from(value).unwrap_or(u64::MAX)
}

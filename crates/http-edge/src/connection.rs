//! One connection's request loop (§4.17.2 "Transports" item 1), a state machine its listener drives: it asks for what
//! it needs next ([`Connection::next`]), and the listener does that and tells it what happened.
//!
//! **The loop.** [`Next::Write`] comes first whenever the connection has bytes to send, so it never holds more than one
//! step's output. Then, by phase: [`Next::Read`] while a head or a body is incomplete; [`Next::Decide`] once a head is
//! whole, answered with [`Connection::decide`]; [`Next::Body`] for each run of the request's body, framing and coding
//! removed, answered with [`Connection::body_taken`] once the service took it; [`Next::BodyEnd`] once; then
//! [`Next::Reply`] until [`Connection::reply_end`], the service's bytes passed in with [`Connection::reply`]. A refusal
//! that leaves bytes unread ends in [`Next::CloseWrite`] and [`Next::Drain`]; every other end is [`Next::Close`].
//!
//! **The reply's head** is written when the service's first bytes are passed in, or at its end, from the decision's
//! fields: `200`, those fields, chunked to HTTP/1.1 and framed by the close to HTTP/1.0, `Connection: close` when the
//! connection ends after it. `100 Continue` is sent when a decision admits a request whose client waits for it: it sent
//! `Expect: 100-continue` and none of its body has come (RFC 9110 §10.1.1: "A server MAY omit sending a 100 (Continue)
//! response if it has already received some or all of the content").
//!
//! **Keeping the connection.** After a reply to an HTTP/1.1 request that did not ask to close, the bytes after the
//! request are the next request's, read at once. A refusal of a request whose body is fully read (none, or one already
//! taken) keeps the connection as a reply does; a 401 to a client's first, credential-less request is the common case,
//! and the client asks again on the same connection.
//!
//! **Ending in stages** (RFC 9112 §9.6: a server "SHOULD NOT" close "if it ... has not yet read" the client's data, since
//! a reset can discard the response the client has not read; A-183): a refusal with request bytes still to come is
//! answered with `Connection: close`, TLS's `close_notify` follows, the write side is closed ([`Next::CloseWrite`]), and
//! the listener reads and drops the client's bytes until it closes or the drain's learned bound passes
//! ([`Next::Drain`]).

use slates_http::head::{Head, HeadReader};
use slates_http::reply::Status;

use crate::tls::HttpsSession;

/// A reply's fields, each a name and a value, as a decision gives them.
pub type Fields = Vec<(Vec<u8>, Vec<u8>)>;

/// What a connection needs next.
#[derive(Debug, PartialEq, Eq)]
pub enum Next<'a> {
  /// Write the bytes [`Connection::take_output`] gives to the socket.
  Write,
  /// Read from the socket and pass what came to [`Connection::receive`], or [`Connection::peer_closed`] at its end.
  Read,
  /// A request's head is whole ([`Connection::head`]): decide it and answer with [`Connection::decide`].
  Decide,
  /// A run of the request's body, framing and coding removed: hand it to the service, then [`Connection::body_taken`].
  Body(&'a [u8]),
  /// The request's body ended: the service's reply follows.
  BodyEnd,
  /// The reply is under way: pass the service's bytes to [`Connection::reply`], and end with
  /// [`Connection::reply_end`] (or [`Connection::fail`]).
  Reply,
  /// Close the socket's write side: the refusal is written and the client's bytes follow.
  CloseWrite,
  /// Read and drop the client's bytes until it closes or the drain's bound passes, then close.
  Drain,
  /// Close the connection: [`Connection::ended`] says why.
  Close,
}

/// A site's answer to a request's head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
  /// Serve it: the reply is `200` with `fields`; `head_only` for a `HEAD` request, whose reply has no body.
  Serve {
    /// The reply's fields (its content type, cache fields and the like), in order.
    fields: Fields,
    /// Whether the reply is the head alone.
    head_only: bool,
  },
  /// Refuse it with `status` and `fields` (`WWW-Authenticate`, `Allow` and the like), no body.
  Refuse {
    /// The status.
    status: Status,
    /// The fields.
    fields: Fields,
  },
}

/// Where a connection is, which says how long its listener may wait for its peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
  /// The TLS handshake is running.
  Handshake,
  /// Between requests: no byte of the next has come.
  Idle,
  /// A head has started and is not yet decided.
  Head,
  /// A request's body is coming.
  Body,
  /// The service's reply is under way.
  Reply,
  /// A refusal was sent and the client's bytes are being dropped.
  Drain,
  /// The connection has ended.
  Closed,
}

/// Why a connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
  /// The client closed between requests: the ordinary end of a kept connection.
  Peer,
  /// The client closed in the middle of a request.
  Truncated,
  /// A reply ended the connection: an HTTP/1.0 request, or one that asked to close.
  Completed,
  /// A refusal ended it, with this status.
  Refused(Status),
  /// TLS failed: the alert rustls queued was sent.
  Tls,
  /// The service failed after its reply began: the reply stops short of its end, which the client reads as a failure.
  Aborted,
}

/// One connection's request loop.
pub struct Connection {
  /// The TLS session, or none on a cleartext connection.
  tls: Option<HttpsSession>,
  /// Plaintext the client sent that the loop has not consumed.
  input: Vec<u8>,
  /// Bytes for the socket.
  output: Vec<u8>,
  /// The room each request is admitted.
  room: u64,
  /// The head reader.
  reader: HeadReader,
}

impl Connection {
  /// A cleartext connection (a loopback listener's), each request admitted `room` bytes.
  pub fn clear(room: u64) -> Connection {
    Connection {
      tls: None,
      input: Vec::new(),
      output: Vec::new(),
      room,
      reader: HeadReader::new(),
    }
  }

  /// A connection over `session`, each request admitted `room` bytes.
  pub fn tls(session: HttpsSession, room: u64) -> Connection {
    Connection {
      tls: Some(session),
      ..Connection::clear(room)
    }
  }

  /// Whether the connection is TLS.
  pub fn secure(&self) -> bool {
    self.tls.is_some()
  }

  /// What the connection needs next.
  pub fn next(&mut self) -> Next<'_> {
    let _ = (&self.input, self.room, &self.reader);
    Next::Close
  }

  /// The bytes to write to the socket, taken.
  pub fn take_output(&mut self) -> Vec<u8> {
    std::mem::take(&mut self.output)
  }

  /// Bytes the socket delivered.
  pub fn receive(&mut self, bytes: &[u8]) {
    let _ = bytes;
  }

  /// The socket's read side ended.
  pub fn peer_closed(&mut self) {}

  /// The head waiting for a decision, while [`Next::Decide`] is what the connection needs.
  pub fn head(&self) -> Option<Head<'_>> {
    None
  }

  /// The decision on the head [`Next::Decide`] asked about.
  pub fn decide(&mut self, decision: Decision) {
    let _ = decision;
  }

  /// The service took the run of body [`Next::Body`] handed it.
  pub fn body_taken(&mut self) {}

  /// The service's next bytes of its reply.
  pub fn reply(&mut self, data: &[u8]) {
    let _ = data;
  }

  /// The service's reply has ended.
  pub fn reply_end(&mut self) {}

  /// The service failed: refused with `status` if its reply has not begun, aborted if it has.
  pub fn fail(&mut self, status: Status) {
    let _ = status;
  }

  /// Where the connection is.
  pub fn phase(&self) -> Phase {
    Phase::Closed
  }

  /// Why the connection ended, once it has.
  pub fn ended(&self) -> Option<Ended> {
    None
  }
}

#[cfg(test)]
#[path = "tests/connection.rs"]
mod tests;

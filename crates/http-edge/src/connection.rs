//! One connection's request loop (§4.17.2 "Transports" item 1), a state machine its listener drives: it asks for what
//! it needs next ([`Connection::next_step`]), and the listener does that and tells it what happened.
//!
//! **The loop.** [`Next::Write`] comes first whenever the connection has bytes to send, so it never holds more than one
//! step's output. Then, by phase: [`Next::Read`] while a head or a body is incomplete; [`Next::Decide`] once a head is
//! whole, answered with [`Connection::decide`]; [`Next::Body`] for each run of the request's body, framing and coding
//! removed, answered with [`Connection::body_taken`] once the service took it; [`Next::BodyEnd`] once; then
//! [`Next::Reply`] until [`Connection::reply_end`], the service's bytes passed in with [`Connection::reply`]. A refusal
//! that leaves bytes unread ends in [`Next::CloseWrite`] and [`Next::Drain`]; every other end is [`Next::Close`].
//!
//! **The reply's head** is written when the service's first bytes are passed in, or at its end, from the decision's
//! fields and then any the service names as its reply begins ([`Connection::reply_fields`]: a reply whose fields follow
//! from its outcome, as Mercurial's media type follows from its command's): its status, those fields, chunked to
//! HTTP/1.1 and framed by the close to HTTP/1.0, `Connection: close` when the connection ends after it. The status is
//! the route's to decide, at its head or after its body ([`StatusMode`]; main's ruling, 2026-11-19: per route): a
//! streamed route's is `200`, fixed by the decision, as git's services stream their replies; one decided after the body
//! is the service's to name as its reply begins ([`Connection::reply_status`]), `200` when it names no other: Git LFS's
//! `201`, `409`, `422`, `413` and `507` once it has read the request (A-629), a WebDAV `PROPFIND`'s `207` or a refusal
//! its body explains (Subversion's site, A-681). A field a reply cannot carry, a status no reply's content may follow,
//! or a status other than `200` named for a streamed route fails the request `500` before any of it is sent: the
//! decision's at the decision, the service's as it names them. `100 Continue` is sent when a decision admits a request
//! whose client waits for it: it sent
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

use slates_http::Admission;
use slates_http::body::{BodyReader, Step};
use slates_http::gzip::{Gunzip, GzipRefused};
use slates_http::head::{Coding, Framing, Head, HeadReader, HeadRefused, Progress, Version};
use slates_http::reply::{self, BadField, Body, Status};

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

/// When a served reply's status is decided (the module doc; main's ruling, 2026-11-19: per route).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusMode {
  /// At the head: `200`, the reply streamed as the service writes it (git's services, Mercurial's commands).
  Streamed,
  /// After the request's body: the service names it as its reply begins ([`Connection::reply_status`]), `200` when it
  /// names no other (Git LFS, A-629; Subversion's site, A-681).
  AfterBody,
}

/// A site's answer to a request's head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
  /// Serve it: the reply carries `fields`, its status `200` or, on a route that decides it after the body, the one its
  /// service names as its reply begins ([`Connection::reply_status`]), as `status` says; `head_only` for a `HEAD`
  /// request, whose reply has no body.
  Serve {
    /// The reply's fields (its content type, cache fields and the like), in order.
    fields: Fields,
    /// Whether the reply is the head alone.
    head_only: bool,
    /// When the reply's status is decided.
    status: StatusMode,
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

/// The facts of a request its body, its reply and the next request depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Request {
  /// Its version: an HTTP/1.1 reply is chunked, an HTTP/1.0 one framed by the close.
  version: Version,
  /// Whether the connection is kept after it.
  keep_alive: bool,
  /// How its body is framed.
  framing: Framing,
  /// Its body's content coding.
  coding: Coding,
  /// Whether its client waits for `100 Continue`.
  expect_continue: bool,
}

impl Request {
  /// The facts `head` gives.
  fn of(head: &Head<'_>) -> Request {
    Request {
      version: head.version,
      keep_alive: head.keep_alive,
      framing: head.framing,
      coding: head.coding,
      expect_continue: head.expect_continue,
    }
  }

  /// Whether a body follows the head (a `Content-Length` of zero is none).
  fn has_body(self) -> bool {
    matches!(self.framing, Framing::Chunked)
      || matches!(self.framing, Framing::Length(length) if length > 0)
  }

  /// Whether the connection is kept once this request's exchange ends: HTTP/1.1 and not asked to close, since an
  /// HTTP/1.0 reply is framed by the close.
  fn kept(self) -> bool {
    self.keep_alive && self.version == Version::Http11
  }
}

/// A served request's reply.
#[derive(Debug)]
struct Reply {
  /// The head's fields: the decision's, then any the service names as its reply begins; the head is rendered from them
  /// with the reply's first bytes, and they are dropped once it is sent.
  fields: Fields,
  /// Whether the reply is the head alone (`HEAD`).
  head_only: bool,
  /// Whether any of the reply has been sent: the head goes with its first bytes, and a failure after this aborts.
  started: bool,
  /// The head's status: `200` unless a route that decides it after the body names another before any of the reply is
  /// sent.
  status: Status,
  /// When its status is decided.
  mode: StatusMode,
}

/// A reply started: its request, whether it is the head alone, and its head's bytes still to be sent.
type ReplyStart = (Request, bool, Vec<u8>);

/// Where the loop is.
enum State {
  /// The TLS handshake is running.
  Handshake,
  /// A head is being read.
  Head,
  /// A head of `head_len` input bytes waits for its decision.
  Decide {
    /// The input bytes the head takes.
    head_len: usize,
    /// What it says.
    request: Request,
  },
  /// The request's body is coming.
  Body {
    /// The request.
    request: Request,
    /// Its framing, being removed.
    reader: BodyReader,
    /// Its gzip coding, being removed, when it has one: boxed, its inflate window being most of the state's size.
    gunzip: Option<Box<Gunzip>>,
    /// The reply to come.
    reply: Reply,
  },
  /// The body ended: [`Next::BodyEnd`] is owed.
  BodyEnded {
    /// The request.
    request: Request,
    /// The reply to come.
    reply: Reply,
  },
  /// The reply is under way.
  Reply {
    /// The request.
    request: Request,
    /// The reply.
    reply: Reply,
  },
  /// The last bytes are written: the connection closes once they are.
  Closing,
  /// A refusal is written: the write side closes once it is.
  CloseWrite,
  /// The write side is closed: the client's bytes are dropped.
  Drain,
  /// Ended.
  Closed,
}

/// A run of body handed out and not yet taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
  /// None.
  None,
  /// `input[from..to]`; the input up to `to` is consumed once it is taken.
  Input {
    /// Where the run starts in the input.
    from: usize,
    /// Where it ends.
    to: usize,
  },
  /// The inflated bytes.
  Inflated,
}

/// What [`Connection::next_step`] answers, before a body run is lent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
  /// [`Next::Write`].
  Write,
  /// [`Next::Read`].
  Read,
  /// [`Next::Decide`].
  Decide,
  /// [`Next::Body`].
  Body,
  /// [`Next::BodyEnd`].
  BodyEnd,
  /// [`Next::Reply`].
  Reply,
  /// [`Next::CloseWrite`].
  CloseWrite,
  /// [`Next::Drain`].
  Drain,
  /// [`Next::Close`].
  Close,
}

/// Whether a step changed the state, or what the connection needs.
enum Flow {
  /// The state changed: step again.
  Again,
  /// The connection needs this.
  Stop(Kind),
}

/// One connection's request loop.
pub struct Connection {
  /// The TLS session, or none on a cleartext connection.
  tls: Option<HttpsSession>,
  /// Plaintext the client sent that the loop has not consumed.
  input: Vec<u8>,
  /// Inflated body bytes not yet taken.
  inflated: Vec<u8>,
  /// Bytes for the socket.
  output: Vec<u8>,
  /// The room each request is admitted.
  room: u64,
  /// The current request's admission.
  admission: Admission,
  /// The head reader.
  reader: HeadReader,
  /// Where the loop is.
  state: State,
  /// A body run handed out and not yet taken.
  pending: Pending,
  /// Why the connection ended, once it has.
  ended: Option<Ended>,
}

impl Connection {
  /// A cleartext connection (a loopback listener's), each request admitted `room` bytes.
  pub fn clear(room: u64) -> Connection {
    Connection {
      tls: None,
      input: Vec::new(),
      inflated: Vec::new(),
      output: Vec::new(),
      room,
      admission: Admission::new(room),
      reader: HeadReader::new(),
      state: State::Head,
      pending: Pending::None,
      ended: None,
    }
  }

  /// A connection over `session`, each request admitted `room` bytes.
  pub fn tls(session: HttpsSession, room: u64) -> Connection {
    Connection {
      tls: Some(session),
      state: State::Handshake,
      ..Connection::clear(room)
    }
  }

  /// Whether the connection is TLS.
  pub fn secure(&self) -> bool {
    self.tls.is_some()
  }

  /// What the connection needs next.
  pub fn next_step(&mut self) -> Next<'_> {
    match self.kind() {
      Kind::Write => Next::Write,
      Kind::Read => Next::Read,
      Kind::Decide => Next::Decide,
      Kind::Body => self.body_run(),
      Kind::BodyEnd => Next::BodyEnd,
      Kind::Reply => Next::Reply,
      Kind::CloseWrite => Next::CloseWrite,
      Kind::Drain => Next::Drain,
      Kind::Close => Next::Close,
    }
  }

  /// Steps the state until it needs something from outside.
  fn kind(&mut self) -> Kind {
    loop {
      if !self.output.is_empty() {
        return Kind::Write;
      }
      if self.pending != Pending::None {
        return Kind::Body;
      }
      let flow = match self.state {
        State::Handshake => self.handshake_step(),
        State::Head => self.head_step(),
        State::Decide { .. } => Flow::Stop(Kind::Decide),
        State::Body { .. } => self.body_step(),
        State::BodyEnded { .. } => self.body_ended_step(),
        State::Reply { .. } => Flow::Stop(Kind::Reply),
        State::Closing => self.end(Ended::Completed),
        State::CloseWrite => {
          self.state = State::Drain;
          Flow::Stop(Kind::CloseWrite)
        }
        State::Drain => Flow::Stop(Kind::Drain),
        State::Closed => Flow::Stop(Kind::Close),
      };
      if let Flow::Stop(kind) = flow {
        return kind;
      }
    }
  }

  /// The body run [`Pending`] names, lent.
  fn body_run(&self) -> Next<'_> {
    match self.pending {
      Pending::Input { from, to } => Next::Body(self.input.get(from..to).unwrap_or_default()),
      Pending::Inflated => Next::Body(&self.inflated),
      Pending::None => Next::Read,
    }
  }

  /// The handshake: read while it runs, then the first head.
  fn handshake_step(&mut self) -> Flow {
    if self.tls.as_ref().is_some_and(HttpsSession::is_handshaking) {
      return Flow::Stop(Kind::Read);
    }
    self.state = State::Head;
    Flow::Again
  }

  /// A head: read while it is incomplete, decided once whole, refused when it breaks HTTP/1.1.
  fn head_step(&mut self) -> Flow {
    match self.reader.read(&self.input, &mut self.admission) {
      Ok(Progress::Partial) => Flow::Stop(Kind::Read),
      Ok(Progress::Complete { value, consumed }) => {
        let request = Request::of(&value);
        self.state = State::Decide {
          head_len: consumed,
          request,
        };
        Flow::Stop(Kind::Decide)
      }
      Err(refused) => {
        self.refuse_head(refused);
        Flow::Again
      }
    }
  }

  /// A body: each run handed out as it comes, the framing and coding removed; refused when either breaks.
  fn body_step(&mut self) -> Flow {
    let State::Body { reader, gunzip, .. } = &mut self.state else {
      return Flow::Again;
    };
    match reader.step(&self.input, &mut self.admission) {
      Ok(Step::Data { data, consumed }) => match gunzip {
        None => {
          self.pending = Pending::Input {
            from: consumed.saturating_sub(data.len()),
            to: consumed,
          };
          Flow::Stop(Kind::Body)
        }
        Some(gunzip) => {
          let fed = gunzip.feed(data, &mut self.admission);
          self.inflated.extend(gunzip.take());
          self.consume(consumed);
          self.inflated_flow(fed)
        }
      },
      Ok(Step::More { consumed }) => {
        self.consume(consumed);
        Flow::Stop(Kind::Read)
      }
      Ok(Step::End { consumed }) => {
        self.consume(consumed);
        self.body_end()
      }
      Err(refused) => {
        self.refuse_body(refused.status());
        Flow::Again
      }
    }
  }

  /// After a gzip feed: the inflated bytes handed out, or the feed's refusal.
  fn inflated_flow(&mut self, fed: Result<(), GzipRefused>) -> Flow {
    if let Err(refused) = fed {
      self.refuse_body(refused.status());
      return Flow::Again;
    }
    if !self.inflated.is_empty() {
      self.pending = Pending::Inflated;
    }
    Flow::Again
  }

  /// The body's framing ended: gzip's end checked and its last bytes handed out, then the body's end owed.
  fn body_end(&mut self) -> Flow {
    let State::Body {
      request,
      gunzip,
      reply,
      ..
    } = std::mem::replace(&mut self.state, State::Closed)
    else {
      return Flow::Again;
    };
    if let Some(mut gunzip) = gunzip {
      let finished = gunzip.finish(&mut self.admission);
      self.inflated.extend(gunzip.take());
      if let Err(refused) = finished {
        self.refuse_body(refused.status());
        return Flow::Again;
      }
      if !self.inflated.is_empty() {
        self.pending = Pending::Inflated;
      }
    }
    self.state = State::BodyEnded { request, reply };
    Flow::Again
  }

  /// The body's end, owed once: then the reply.
  fn body_ended_step(&mut self) -> Flow {
    let State::BodyEnded { request, reply } = std::mem::replace(&mut self.state, State::Closed)
    else {
      return Flow::Again;
    };
    self.state = State::Reply { request, reply };
    Flow::Stop(Kind::BodyEnd)
  }

  /// The connection ends now, for `ended` unless an earlier reason stands.
  fn end(&mut self, ended: Ended) -> Flow {
    self.state = State::Closed;
    self.ended.get_or_insert(ended);
    Flow::Stop(Kind::Close)
  }

  /// Drops the first `count` input bytes.
  fn consume(&mut self, count: usize) {
    self.input.drain(..count.min(self.input.len()));
  }

  /// The bytes to write to the socket, taken.
  pub fn take_output(&mut self) -> Vec<u8> {
    std::mem::take(&mut self.output)
  }

  /// Bytes the socket delivered: deciphered first on a TLS connection, ignored once the connection is draining.
  pub fn receive(&mut self, bytes: &[u8]) {
    if matches!(self.state, State::Drain | State::Closed | State::Closing) {
      return;
    }
    let Some(session) = &mut self.tls else {
      self.input.extend_from_slice(bytes);
      return;
    };
    let received = session.receive(bytes, &mut self.input);
    let transmitted = session.transmit(&mut self.output);
    let peer_closed = session.closed_by_peer();
    if received.is_err() || transmitted.is_err() {
      self.end(Ended::Tls);
    } else if peer_closed {
      self.peer_closed();
    }
  }

  /// The socket's read side ended: between requests the client's ordinary close, otherwise a request cut short.
  pub fn peer_closed(&mut self) {
    let ended = match self.state {
      State::Closed | State::Closing => return,
      State::Head if self.input.is_empty() => Ended::Peer,
      State::Drain => Ended::Peer,
      _ => Ended::Truncated,
    };
    self.end(ended);
  }

  /// The head waiting for a decision, while [`Next::Decide`] is what the connection needs.
  pub fn head(&self) -> Option<Head<'_>> {
    let State::Decide { head_len, .. } = self.state else {
      return None;
    };
    let bytes = self.input.get(..head_len)?;
    match HeadReader::new().read(bytes, &mut Admission::new(u64::MAX)) {
      Ok(Progress::Complete { value, .. }) => Some(value),
      _ => None,
    }
  }

  /// The decision on the head [`Next::Decide`] asked about.
  pub fn decide(&mut self, decision: Decision) {
    let State::Decide { head_len, request } = self.state else {
      return;
    };
    self.consume(head_len);
    match decision {
      Decision::Serve {
        fields,
        head_only,
        status,
      } => self.serve(request, fields, (head_only, status)),
      Decision::Refuse { status, fields } => self.refuse(request, status, &fields),
    }
  }

  /// Serves a request: its reply's fields judged now (a decision whose fields a reply cannot carry is refused 500),
  /// `100 Continue` sent to a client waiting for it, and its body read, if it has one.
  fn serve(&mut self, request: Request, fields: Fields, (head_only, mode): (bool, StatusMode)) {
    if rendered(request, Status::OK, &fields, &mut Vec::new()).is_err() {
      self.refuse_staged(Status::INTERNAL_SERVER_ERROR);
      return;
    }
    let reply = Reply {
      fields,
      head_only,
      started: false,
      status: Status::OK,
      mode,
    };
    if !request.has_body() {
      self.state = State::BodyEnded { request, reply };
      return;
    }
    if request.expect_continue && self.input.is_empty() {
      self.send(reply::CONTINUE);
      if self.failed() {
        return;
      }
    }
    let gunzip = (request.coding == Coding::Gzip).then(|| Box::new(Gunzip::new()));
    self.state = State::Body {
      request,
      reader: BodyReader::new(request.framing),
      gunzip,
      reply,
    };
  }

  /// Refuses a request: the connection kept when the request leaves no body unread and asked to be kept, otherwise
  /// ended in stages.
  fn refuse(&mut self, request: Request, status: Status, fields: &Fields) {
    let keep = request.kept() && !request.has_body();
    let mut head = Vec::new();
    if reply::head(status, &borrowed(fields), Body::Length(0), !keep, &mut head).is_err() {
      self.refuse_staged(Status::INTERNAL_SERVER_ERROR);
      return;
    }
    self.send(&head);
    if self.failed() {
      return;
    }
    if keep {
      self.next_request();
    } else {
      self.close_staged(Ended::Refused(status));
    }
  }

  /// Refuses a head that broke HTTP/1.1, in stages: where its request ends is unknown.
  fn refuse_head(&mut self, refused: HeadRefused) {
    let mut head = Vec::new();
    if refused.reply(&mut head).is_err() {
      head.clear();
      reply::bare(refused.status(), Body::Length(0), true, &mut head);
    }
    self.send(&head);
    self.close_staged(Ended::Refused(refused.status()));
  }

  /// Refuses a body whose framing or coding broke, in stages.
  fn refuse_body(&mut self, status: Status) {
    self.pending = Pending::None;
    self.inflated.clear();
    self.refuse_staged(status);
  }

  /// Refuses with `status` and no fields, in stages.
  fn refuse_staged(&mut self, status: Status) {
    let mut head = Vec::new();
    reply::bare(status, Body::Length(0), true, &mut head);
    self.send(&head);
    self.close_staged(Ended::Refused(status));
  }

  /// Ends the connection in stages: `close_notify` after what was sent, then the write side closed and the rest
  /// drained.
  fn close_staged(&mut self, ended: Ended) {
    self.notify_close();
    if self.failed() {
      return;
    }
    self.ended.get_or_insert(ended);
    self.state = State::CloseWrite;
  }

  /// Whether TLS failed: the connection ended there, and nothing more is sent or read.
  fn failed(&self) -> bool {
    self.ended == Some(Ended::Tls)
  }

  /// Seals TLS's `close_notify` after what was sent.
  fn notify_close(&mut self) {
    let Some(session) = &mut self.tls else {
      return;
    };
    if session
      .close()
      .and_then(|()| session.transmit(&mut self.output))
      .is_err()
    {
      self.end(Ended::Tls);
    }
  }

  /// Sends `bytes`, sealed on a TLS connection.
  fn send(&mut self, bytes: &[u8]) {
    let Some(session) = &mut self.tls else {
      self.output.extend_from_slice(bytes);
      return;
    };
    if session
      .send(bytes)
      .and_then(|()| session.transmit(&mut self.output))
      .is_err()
    {
      self.end(Ended::Tls);
    }
  }

  /// Readies the connection for the client's next request, whose bytes may already be here.
  fn next_request(&mut self) {
    self.state = State::Head;
    self.reader = HeadReader::new();
    self.admission = Admission::new(self.room);
  }

  /// The service took the run of body [`Next::Body`] handed it.
  pub fn body_taken(&mut self) {
    match std::mem::replace(&mut self.pending, Pending::None) {
      Pending::Input { to, .. } => self.consume(to),
      Pending::Inflated => self.inflated.clear(),
      Pending::None => {}
    }
  }

  /// Fields the service names for its reply's head as its reply begins, written after the decision's (the module doc).
  /// Taken only before any of the reply is sent, and only while a reply is under way; a field a reply cannot carry
  /// fails the request `500`, in stages.
  pub fn reply_fields(&mut self, fields: Fields) {
    let State::Reply { request, reply } = &mut self.state else {
      return;
    };
    if reply.started {
      return;
    }
    reply.fields.extend(fields);
    let carried = rendered(*request, reply.status, &reply.fields, &mut Vec::new()).is_ok();
    if !carried {
      self.fail(Status::INTERNAL_SERVER_ERROR);
    }
  }

  /// The status the service names for its reply's head as its reply begins (the module doc), in place of `200`. Taken
  /// only before any of the reply is sent, and only while a reply is under way: on a route that decides it after the
  /// body ([`StatusMode::AfterBody`]) any status a reply's content may follow, on a streamed route `200` alone, its
  /// own, and `204` (A-681: a WebDAV `DELETE`'s and an updating `PUT`'s answer), whose head is written with no framing
  /// field and nothing after it, the service's runs dropped. Any other (`1xx`, `205`, `304`, a code outside RFC 9110
  /// §15's classes, or a status other than `200` on a streamed route) fails the request `500`, in stages.
  pub fn reply_status(&mut self, status: Status) {
    let State::Reply { reply, .. } = &mut self.state else {
      return;
    };
    if reply.started {
      return;
    }
    let taken = match reply.mode {
      StatusMode::AfterBody => carries_content(status) || status.code == NO_CONTENT,
      StatusMode::Streamed => status == Status::OK,
    };
    if taken {
      reply.status = status;
      return;
    }
    self.fail(Status::INTERNAL_SERVER_ERROR);
  }

  /// A reply under way: its request, whether it is the head alone, and its head's bytes when they are still to be sent
  /// (rendered now, the reply then started). `None` off a reply; `Some(Err(_))` where a field cannot be carried.
  fn begin_reply(&mut self) -> Option<Result<ReplyStart, BadField>> {
    let State::Reply { request, reply } = &mut self.state else {
      return None;
    };
    let mut head = Vec::new();
    if !reply.started {
      if let Err(bad) = rendered(*request, reply.status, &reply.fields, &mut head) {
        return Some(Err(bad));
      }
      reply.started = true;
      reply.fields = Fields::new();
    }
    Some(Ok((
      *request,
      reply.head_only || reply.status.code == NO_CONTENT,
      head,
    )))
  }

  /// The service's next bytes of its reply: the head before the first, each run chunked (HTTP/1.1) or as it is
  /// (HTTP/1.0); nothing past the head of a `HEAD`'s reply.
  pub fn reply(&mut self, data: &[u8]) {
    let (request, head_only, mut bytes) = match self.begin_reply() {
      None => return,
      Some(Ok(begun)) => begun,
      Some(Err(_)) => {
        self.fail(Status::INTERNAL_SERVER_ERROR);
        return;
      }
    };
    if !head_only {
      match request.version {
        Version::Http11 => reply::chunk(data, &mut bytes),
        Version::Http10 => bytes.extend_from_slice(data),
      }
    }
    self.send(&bytes);
  }

  /// The service's reply has ended: the head if none of the reply was sent, the last chunk (HTTP/1.1), then the next
  /// request or the connection's end.
  pub fn reply_end(&mut self) {
    let (request, head_only, mut bytes) = match self.begin_reply() {
      None => return,
      Some(Ok(begun)) => begun,
      Some(Err(_)) => {
        self.fail(Status::INTERNAL_SERVER_ERROR);
        return;
      }
    };
    if !head_only && request.version == Version::Http11 {
      bytes.extend_from_slice(reply::LAST_CHUNK);
    }
    self.send(&bytes);
    if self.failed() {
      return;
    }
    if request.kept() {
      self.next_request();
    } else {
      self.notify_close();
      if !self.failed() {
        self.state = State::Closing;
      }
    }
  }

  /// The service failed: refused with `status` in stages if its reply has not begun, aborted if it has (the reply stops
  /// short of its end, which the client reads as a failure).
  pub fn fail(&mut self, status: Status) {
    match &self.state {
      State::Reply { reply, .. } if reply.started => {
        self.end(Ended::Aborted);
      }
      State::Decide { .. } | State::Body { .. } | State::BodyEnded { .. } | State::Reply { .. } => {
        self.pending = Pending::None;
        self.refuse_staged(status);
      }
      _ => {}
    }
  }

  /// Where the connection is.
  pub fn phase(&self) -> Phase {
    match self.state {
      State::Handshake => Phase::Handshake,
      State::Head if self.input.is_empty() => Phase::Idle,
      State::Head | State::Decide { .. } => Phase::Head,
      State::Body { .. } => Phase::Body,
      State::BodyEnded { .. } | State::Reply { .. } | State::Closing => Phase::Reply,
      State::CloseWrite | State::Drain => Phase::Drain,
      State::Closed => Phase::Closed,
    }
  }

  /// Why the connection ended, once it has.
  pub fn ended(&self) -> Option<Ended> {
    self.ended
  }
}

/// Renders a served reply's head for `request` into `out`: `status`, `fields`, chunked to HTTP/1.1 and framed by the
/// close to HTTP/1.0, `Connection: close` when the connection ends after it. Refused, writing nothing, where a field
/// cannot be carried.
fn rendered(
  request: Request,
  status: Status,
  fields: &Fields,
  out: &mut Vec<u8>,
) -> Result<(), BadField> {
  let body = match request.version {
    _ if status.code == NO_CONTENT => Body::Empty,
    Version::Http11 => Body::Chunked,
    Version::Http10 => Body::UntilClose,
  };
  reply::head(status, &borrowed(fields), body, !request.kept(), out)
}

/// Format: `204 No Content` (RFC 9110 §15.3.5), a reply without content.
const NO_CONTENT: u16 = 204;

/// Format: `205 Reset Content` (RFC 9110 §15.3.6), a reply without content.
const RESET_CONTENT: u16 = 205;

/// Format: `304 Not Modified` (RFC 9110 §15.4.5), a reply without content.
const NOT_MODIFIED: u16 = 304;

/// Format: the last code of RFC 9110 §15's classes, the end of `5xx`.
const LAST_CODE: u16 = 599;

/// Whether a reply of `status` may carry content, as every served reply's framing does: RFC 9110 §6.4.1, "All 1xx
/// (Informational), 204 (No Content), and 304 (Not Modified) responses do not include content"; §15.3.6, "a server
/// MUST NOT generate content in a 205 response"; and a code outside §15's classes is no status.
fn carries_content(status: Status) -> bool {
  (Status::OK.code..=LAST_CODE).contains(&status.code)
    && ![NO_CONTENT, RESET_CONTENT, NOT_MODIFIED].contains(&status.code)
}

/// `fields` as `slates-http`'s reply writer takes them.
fn borrowed(fields: &Fields) -> Vec<(&[u8], &[u8])> {
  fields
    .iter()
    .map(|(name, value)| (name.as_slice(), value.as_slice()))
    .collect()
}

#[cfg(test)]
#[path = "tests/connection.rs"]
mod tests;

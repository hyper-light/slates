//! The MCP loopback Streamable HTTP transport (§4.12). A minimal HTTP/1.1 server: the agent POSTs a
//! JSON-RPC message to the one MCP endpoint and gets the reply as `application/json`; a notification (no
//! reply) gets `202 Accepted`. This is the same-machine *edge* — one agent to its local daemon — so it is
//! HTTP/1.1 over loopback, not QUIC: QUIC's wide-area properties belong to the fleet transport (§4.10),
//! and are wasted on a loopback edge that must anyway speak what MCP clients speak. There is no
//! server-initiated stream, so `GET` (the SSE channel) is answered `405` — the stateless profile the design
//! names.
//!
//! **Bounded, owned connections on the runtime (AUD-29-23).** The edge runs on one slates shard
//! ([`serve`]): the listener's accept loop and every connection are tasks on it, so an idle or slow client
//! waits on the driver, never on the thread, and another client is accepted and served meanwhile. Every
//! bound is stated:
//! - a request line or header line past [`MAX_LINE_BYTES`] (RFC 9112 §3's recommended minimum support) is
//!   refused `431` or `414`, a head past [`MAX_FIELDS`] fields `431`, a body past [`MAX_BODY`] `413`, each
//!   before the bytes are kept;
//! - contradictory framing is refused `400` (two `Content-Length` values, any `Transfer-Encoding` — RFC 9112
//!   §6.1/§6.3: this edge does not take chunked bodies), a POST with no length `411`;
//! - a connection that has not shown the edge's bearer is held no longer than an honest client takes to show
//!   it, a bound the edge learns from its own reads ([`Arrivals`]; below), and one that has is waited on while
//!   it lives;
//! - the connections served at once are bounded by [`HttpEdge::connections`]; one past it is answered `503`
//!   and closed at once.
//!
//! A connection's failure — a reset, a truncated body, a cut-off — ends that connection only; the
//! listener keeps serving. Until 2026-10-01 this edge read with unbounded `read_line`s, served one
//! connection at a time for its whole keep-alive life, and ended the server on any connection's I/O error.
//!
//! **The bearer first, then the peer's life (A-523).** The port is loopback, so any local user can open a connection,
//! and the edge reads a request's head before it can check the bearer. So a connection is any local user's until a
//! field line carrying the edge's bearer has arrived on it: from its acceptance it gets as long as an honest client
//! takes to show the bearer and no longer. Its first request, a refusal of it and the refusal's drain all end at that
//! instant, and a slow or silent sender is cut off there unanswered, its slot freed. The bound is learned from the
//! edge's own reads ([`Arrivals`]): RFC 6298 §2's estimator (the transport's `RttEstimator`, which every learned wait in
//! slates keeps) over the time each connection that showed the bearer took from its acceptance to it. Its tail is
//! `SRTT + max(G, 4·RTTVAR)` (§2.3), `G` being this shard's granularity: its timer's tick or, when longer, the lateness
//! its scheduler has shown. Only a bearer holder's arrival is a sample, so no other local user can move the bound either
//! way, and a connection cut off is none (Karn's rule: its true arrival is unknown). Before the first arrival the bound
//! is the estimator's own prior, RFC 9002 §6.2.2's initial probe timeout ([`PRIOR_NS`], A-471's), and each arrival
//! shrinks the prior's part: after `n` of them the next outlasts every one with chance `1/(n+1)` whatever their
//! distribution (Rényi's record indicator, the share A-363's detector prices a lateness past every one seen at), so the
//! bound keeps that share of the prior's excess over the learned tail. A connection that has shown the bearer is the
//! token holder's: the rest of its request, its next request when it is kept alive and the drain after a refusal of
//! its own wait while it lives, as every wait on a trusted peer in slates does, until it closes or a read fails. Until
//! A-523 every one of these waits ran to the client's reply deadline (`Deadlines::reply_ns`, the anchor's liveness
//! budget, a fixed second): a silent local sender held a slot for that second, and a token holder's kept-alive
//! connection was closed once idle that long. Owed: an honest client cut off before its bearer arrived looks like any
//! other sender, so its cut-off teaches the bound nothing (the prior's share guards the first arrivals); and a listen
//! stream's keep-alive period still comes from the reply deadline ([`listen_keep_alive_ns`]).
//!
//! A refusal ends its connection in stages (RFC 9112 §9.6): the response, a half-close, then the client's remaining
//! bytes read and dropped until the connection's own end (above), so the client reads the refusal rather than a reset.
//! Until 2026-10-26 the edge closed at once, and on Windows, whose reset discards what the client has not read, a client
//! that sent a body past the bound never saw its `413`.
//!
//! **The caller's authority, before any dispatch (AUD-29-24).** A loopback bind does not identify the
//! caller: a web page in the user's browser can POST to `127.0.0.1`, and a DNS-rebound host can reach it
//! by name. So every request must name this edge — `Host` the loopback address and port it listens on
//! (a rebound name is refused `421`), `Origin`, when a browser sends one, the same loopback origin (the
//! MCP transport specification's "servers MUST validate the Origin header"; refused `403`), the request
//! target the one MCP endpoint ([`ENDPOINT_PATH`]; `404`), `Content-Type: application/json` (`415`: a
//! page cannot send it without a CORS preflight, which this edge never grants) — and carry the edge's
//! bearer token (`Authorization: Bearer`, [`BEARER_BYTES`] from the platform's secure random, minted when
//! the edge starts and shown only to the human who started it; `401`). The token is compared in constant
//! time. No grant verb exists on this surface (R10). Owed: the servable roots a human enrolls (§4.13).

use std::cell::{Cell, RefCell};

use serde_json::Value;
use slates_rt::futures::{detach, now_ns, spawn_child, within};
use slates_rt::shard::Kept;
use slates_rt::tcp::{TcpListener, TcpStream};
use slates_transport::rtt::RttEstimator;

use crate::McpServer;

/// The largest HTTP request body accepted: the one message bound both transports keep
/// ([`crate::MAX_MESSAGE_BYTES`]); a larger body is refused `413` rather than read.
pub const MAX_BODY: u64 = crate::MAX_MESSAGE_BYTES;

/// Format: the longest request line or header field line accepted — RFC 9112 §3: "It is RECOMMENDED that
/// all HTTP senders and recipients support, at a minimum, request-line lengths of 8000 octets", the same
/// bound applied to each field line.
pub const MAX_LINE_BYTES: usize = 8000;

/// Format: the most header fields accepted in one request — Apache httpd's `LimitRequestFields` default
/// (tier C). An MCP request carries about ten (Host, Origin, Content-Type, Content-Length, Accept,
/// Authorization, MCP-Protocol-Version, Connection, User-Agent).
pub const MAX_FIELDS: usize = 100;

/// Format: the one request target the edge serves (the MCP Streamable HTTP transport's single endpoint).
pub const ENDPOINT_PATH: &str = "/mcp";

/// Format: the bearer token's length — 128 bits, as every capability token slates mints (§4.13: "ids are
/// random 128-bit values").
pub const BEARER_BYTES: usize = 16;

/// Format: the blank line that ends an HTTP/1.1 request head (RFC 9112 §2.1).
const HEAD_END: &[u8] = b"\r\n\r\n";

/// Format: the line break that ends each line of a request head (RFC 9112 §2.1).
const CRLF: &[u8] = b"\r\n";

/// Derived: the most bytes a request head can hold — every field line at its bound, plus the request line.
const MAX_HEAD_BYTES: usize = MAX_LINE_BYTES * (MAX_FIELDS + 1);

/// Format: RFC 6298 §2.3's `K`: the tail of the learned arrivals adds four mean deviations to their smoothed mean.
const VARIATION_MULTIPLIER: u64 = 4;

/// Format: how long a connection may take to show the edge's bearer before any honest arrival was measured: the
/// estimator's own prior, RFC 9002 §6.2.2's initial probe timeout (twice its initial round trip, 666 ms), the prior the
/// S3 base's learned waits start from (A-471). Each honest arrival shrinks its part ([`Arrivals::bound_ns`]).
pub const PRIOR_NS: u64 = slates_transport::rtt::INITIAL_PTO_NS;

/// What the edge enforces, fixed when it starts.
#[derive(Clone, Debug)]
pub struct HttpEdge {
  /// The loopback port the listener is bound to — what `Host` and `Origin` must name.
  pub port: u16,
  /// The bearer token every request must carry.
  pub bearer: [u8; BEARER_BYTES],
  /// The timer tick of the runtime the edge runs on, nanoseconds (`RuntimeConfig::timer_tick_ns`, the profile's
  /// `max(wake.mean, 100 × timer overhead)`): the finest wait its timers keep, so the least granularity `G` of the
  /// learned bound ([`Arrivals::bound_ns`]).
  pub tick_ns: u64,
  /// The period a quiet `subscriptions/listen` stream sends its keep-alive comment at, nanoseconds
  /// ([`listen_keep_alive_ns`]).
  pub keep_alive_ns: u64,
  /// The connections served at once.
  pub connections: usize,
}

/// Derived: the period a quiet listen stream sends its keep-alive comment at, from the client's reply deadline
/// (`Deadlines::reply_ns`): half of it, the idle bound this edge kept on every connection until A-523, so a party that
/// closes a connection idle that long sees a comment first. Owed (R3, GAPS): a period learned from what closes this
/// edge's quiet connections, since the edge itself no longer closes a token holder's.
pub fn listen_keep_alive_ns(reply_ns: u64) -> u64 {
  (reply_ns / 2).max(1)
}

/// What the edge has learned of how long an honest client takes to show its bearer (the module doc's rule): RFC 6298
/// §2's estimator over the time each connection that showed the edge's bearer took from its acceptance to it.
#[derive(Debug, Default)]
pub struct Arrivals {
  /// The arrivals' smoothed mean and mean deviation: the transport's RFC 9002 §5.3 form of RFC 6298 §2's estimator,
  /// with no acknowledgement delay.
  estimator: RttEstimator,
  /// The honest arrivals folded in.
  samples: u64,
}

impl Arrivals {
  /// Folds in one honest arrival: a connection showed the edge's bearer `arrival_ns` after it was accepted. A connection
  /// cut off before showing it is no arrival (Karn's rule): nothing else is folded in.
  pub fn arrived(&mut self, arrival_ns: u64) {
    self.estimator.on_sample(arrival_ns, 0);
    self.samples = self.samples.saturating_add(1);
  }

  /// The honest arrivals folded in so far.
  pub fn samples(&self) -> u64 {
    self.samples
  }

  /// Derived: how long a connection accepted now may take to show the edge's bearer, `granularity_ns` being this
  /// shard's (`G`): the learned tail `SRTT + max(G, 4·RTTVAR)` (RFC 6298 §2.3), plus the share `1/(n+1)` of
  /// [`PRIOR_NS`]'s excess over it after `n` honest arrivals, since the next outlasts all `n` with that chance whatever
  /// their distribution (Rényi's record indicator). The prior itself before the first arrival, and never below `G`.
  pub fn bound_ns(&self, granularity_ns: u64) -> u64 {
    if self.samples == 0 {
      return PRIOR_NS.max(granularity_ns);
    }
    let tail_ns = self.estimator.smoothed_rtt().saturating_add(
      granularity_ns.max(self.estimator.rttvar().saturating_mul(VARIATION_MULTIPLIER)),
    );
    let prior_share_ns = PRIOR_NS
      .saturating_sub(tail_ns)
      .checked_div(self.samples.saturating_add(1))
      .unwrap_or(0);
    tail_ns.saturating_add(prior_share_ns)
  }
}

/// The granularity `G` of this shard's waits now (RFC 6298 §2.3's clock granularity): its timer's tick, or the lateness
/// its scheduler has shown (`slates_rt::futures::scheduler_overrun_ns`, the floor the fleet's detector puts under its
/// windows, §4.8) when that is longer.
fn granularity_ns(edge: &HttpEdge) -> u64 {
  edge.tick_ns.max(slates_rt::futures::scheduler_overrun_ns())
}

impl HttpEdge {
  /// The bearer token as it travels: lowercase hex.
  pub fn bearer_hex(&self) -> String {
    self
      .bearer
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect()
  }
}

/// One parsed request head: its method, target and the fields the edge reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Head {
  /// The method.
  pub method: String,
  /// The request target.
  pub target: String,
  /// The body's length, when given.
  pub content_length: Option<u64>,
  /// Whether the connection is kept alive after this request.
  pub keep_alive: bool,
  /// The `Host` field, when exactly one was given.
  pub host: Option<String>,
  /// How many `Host` fields were given.
  pub hosts: usize,
  /// The `Origin` field, when given.
  pub origin: Option<String>,
  /// The `Content-Type` field, when given.
  pub content_type: Option<String>,
  /// The `Authorization` field, when given.
  pub authorization: Option<String>,
  /// The `MCP-Protocol-Version` field, when given.
  pub protocol_version: Option<String>,
  /// The `Mcp-Method` field, when given.
  pub mcp_method: Option<String>,
  /// The `Mcp-Name` field, when given (possibly in the Base64 sentinel form).
  pub mcp_name: Option<String>,
}

/// Why a request is refused, as the status it is answered with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// `400 Bad Request`: malformed or contradictory framing, or a missing or repeated `Host`.
  BadRequest,
  /// `401 Unauthorized`: no bearer token, or the wrong one.
  Unauthorized,
  /// `403 Forbidden`: a foreign `Origin`.
  Forbidden,
  /// `404 Not Found`: a target other than the MCP endpoint.
  NotFound,
  /// `405 Method Not Allowed`: anything but `POST`.
  MethodNotAllowed,
  /// `411 Length Required`: a POST with no `Content-Length`.
  LengthRequired,
  /// `413 Content Too Large`: a body past [`MAX_BODY`].
  TooLarge,
  /// `414 URI Too Long`: a request line past [`MAX_LINE_BYTES`].
  UriTooLong,
  /// `415 Unsupported Media Type`: a body that is not `application/json`.
  UnsupportedMediaType,
  /// `421 Misdirected Request`: a `Host` that is not this edge.
  Misdirected,
  /// `431 Request Header Fields Too Large`: a field line or the head past its bound.
  HeadersTooLarge,
  /// `503 Service Unavailable`: every connection slot is in use.
  Unavailable,
}

impl Refused {
  /// The status line's code and reason.
  fn status(self) -> &'static str {
    match self {
      Refused::BadRequest => "400 Bad Request",
      Refused::Unauthorized => "401 Unauthorized",
      Refused::Forbidden => "403 Forbidden",
      Refused::NotFound => "404 Not Found",
      Refused::MethodNotAllowed => "405 Method Not Allowed",
      Refused::LengthRequired => "411 Length Required",
      Refused::TooLarge => "413 Content Too Large",
      Refused::UriTooLong => "414 URI Too Long",
      Refused::UnsupportedMediaType => "415 Unsupported Media Type",
      Refused::Misdirected => "421 Misdirected Request",
      Refused::HeadersTooLarge => "431 Request Header Fields Too Large",
      Refused::Unavailable => "503 Service Unavailable",
    }
  }
}

/// Parses a request head (the bytes before the blank line, without it): the request line and every field
/// line, each within [`MAX_LINE_BYTES`], at most [`MAX_FIELDS`] fields, the framing consistent.
pub fn parse_head(head: &[u8]) -> Result<Head, Refused> {
  let text = std::str::from_utf8(head).map_err(|_| Refused::BadRequest)?;
  let mut lines = text.split("\r\n");
  let request_line = lines.next().ok_or(Refused::BadRequest)?;
  if request_line.len() > MAX_LINE_BYTES {
    return Err(Refused::UriTooLong);
  }
  let mut parts = request_line.split(' ');
  let (Some(method), Some(target), Some(version), None) =
    (parts.next(), parts.next(), parts.next(), parts.next())
  else {
    return Err(Refused::BadRequest);
  };
  if !version.starts_with("HTTP/1.") || method.is_empty() || target.is_empty() {
    return Err(Refused::BadRequest);
  }
  let mut parsed = Head {
    method: method.to_owned(),
    target: target.to_owned(),
    keep_alive: version == "HTTP/1.1",
    ..Head::default()
  };
  let mut fields = 0usize;
  for line in lines {
    if line.len() > MAX_LINE_BYTES {
      return Err(Refused::HeadersTooLarge);
    }
    fields = fields.saturating_add(1);
    if fields > MAX_FIELDS {
      return Err(Refused::HeadersTooLarge);
    }
    read_field(&mut parsed, line)?;
  }
  Ok(parsed)
}

/// Reads one field line into `head`.
fn read_field(head: &mut Head, line: &str) -> Result<(), Refused> {
  let (name, value) = line.split_once(':').ok_or(Refused::BadRequest)?;
  if name.is_empty() || name.ends_with(' ') || name.ends_with('\t') {
    return Err(Refused::BadRequest); // RFC 9112 §5.1: no whitespace before the colon
  }
  let value = value.trim_matches([' ', '\t']).to_owned();
  match name.to_ascii_lowercase().as_str() {
    "content-length" => {
      if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Refused::BadRequest);
      }
      let length: u64 = value.parse().map_err(|_| Refused::TooLarge)?;
      if head.content_length.is_some_and(|earlier| earlier != length) {
        return Err(Refused::BadRequest);
      }
      head.content_length = Some(length);
    }
    "transfer-encoding" => return Err(Refused::BadRequest),
    "connection" => {
      for token in value.split(',').map(str::trim) {
        if token.eq_ignore_ascii_case("close") {
          head.keep_alive = false;
        } else if token.eq_ignore_ascii_case("keep-alive") {
          head.keep_alive = true;
        }
      }
    }
    "host" => {
      head.hosts = head.hosts.saturating_add(1);
      head.host = Some(value);
    }
    "origin" => head.origin = Some(value),
    "content-type" => head.content_type = Some(value),
    "authorization" => head.authorization = Some(value),
    "mcp-protocol-version" => head.protocol_version = Some(value),
    "mcp-method" => head.mcp_method = Some(value),
    "mcp-name" => head.mcp_name = Some(value),
    _ => {}
  }
  Ok(())
}

/// The loopback authorities this edge answers to on `port`, as a `Host` names them.
fn loopback_hosts(port: u16) -> [String; 3] {
  [
    format!("127.0.0.1:{port}"),
    format!("localhost:{port}"),
    format!("[::1]:{port}"),
  ]
}

/// Whether `token` equals the edge's bearer, compared in time independent of where they differ.
fn bearer_matches(edge: &HttpEdge, token: &str) -> bool {
  let expected = edge.bearer_hex();
  if token.len() != expected.len() {
    return false;
  }
  token
    .bytes()
    .zip(expected.bytes())
    .fold(0u8, |difference, (got, want)| difference | (got ^ want))
    == 0
}

/// Whether an `Authorization` field's value (trimmed as a field is read) carries the edge's bearer: `Bearer` and the
/// token, compared in constant time. One rule for [`authorize`] and for a connection's proof (A-523).
fn carries_bearer(edge: &HttpEdge, value: &str) -> bool {
  value
    .strip_prefix("Bearer ")
    .is_some_and(|token| bearer_matches(edge, token.trim()))
}

/// Whether one complete field line of a request head is an `Authorization` field carrying the edge's bearer.
fn line_carries_bearer(edge: &HttpEdge, line: &[u8]) -> bool {
  let Ok(text) = std::str::from_utf8(line) else {
    return false;
  };
  text.split_once(':').is_some_and(|(name, value)| {
    name.eq_ignore_ascii_case("authorization")
      && carries_bearer(edge, value.trim_matches([' ', '\t']))
  })
}

/// What the lines of a request head read so far show ([`scan_lines`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lines {
  /// A field line carrying the edge's bearer.
  Bearer,
  /// The blank line that ends the head, with no bearer before it.
  HeadEnded,
  /// Neither yet.
  Partial,
}

/// Reads the lines completed in `pending` since `*scanned` (the offset of a line's start, advanced past each line read):
/// whether one carries the edge's bearer or ends the head. A line still arriving is read again once it is complete.
fn scan_lines(edge: &HttpEdge, pending: &[u8], scanned: &mut usize) -> Lines {
  while let Some(rest) = pending.get(*scanned..) {
    let Some(length) = rest.windows(CRLF.len()).position(|window| window == CRLF) else {
      break;
    };
    *scanned = scanned.saturating_add(length).saturating_add(CRLF.len());
    let line = rest.get(..length).unwrap_or_default();
    if line.is_empty() {
      return Lines::HeadEnded;
    }
    if line_carries_bearer(edge, line) {
      return Lines::Bearer;
    }
  }
  Lines::Partial
}

/// Decides whether `head` may reach the MCP server at all (AUD-29-24): the endpoint, the method, `Host`,
/// `Origin`, the media type, the bearer token and the framing — before any body is read or dispatched.
pub fn authorize(edge: &HttpEdge, head: &Head) -> Result<(), Refused> {
  let hosts = loopback_hosts(edge.port);
  match (&head.host, head.hosts) {
    (Some(host), 1) => {
      if !hosts.iter().any(|ours| ours.eq_ignore_ascii_case(host)) {
        return Err(Refused::Misdirected);
      }
    }
    _ => return Err(Refused::BadRequest), // RFC 9112 §3.2: exactly one Host
  }
  if let Some(origin) = &head.origin
    && !hosts
      .iter()
      .any(|ours| origin.eq_ignore_ascii_case(&format!("http://{ours}")))
  {
    return Err(Refused::Forbidden);
  }
  if head.target != ENDPOINT_PATH {
    return Err(Refused::NotFound);
  }
  if head.method != "POST" {
    return Err(Refused::MethodNotAllowed);
  }
  if !head
    .authorization
    .as_deref()
    .is_some_and(|value| carries_bearer(edge, value))
  {
    return Err(Refused::Unauthorized);
  }
  let json = head.content_type.as_deref().is_some_and(|value| {
    value
      .split(';')
      .next()
      .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
  });
  if !json {
    return Err(Refused::UnsupportedMediaType);
  }
  match head.content_length {
    None => Err(Refused::LengthRequired),
    Some(length) if length > MAX_BODY => Err(Refused::TooLarge),
    Some(_) => Ok(()),
  }
}

/// What the edge shares among its tasks on its shard: the MCP server (one daemon client), the
/// connections being served and what its honest connections taught it.
struct Shared {
  server: RefCell<McpServer>,
  serving: Cell<usize>,
  /// The `subscriptions/listen` streams open now, at most [`crate::max_subscriptions`]: one per notification source,
  /// as on stdio, so listens cannot take every connection the edge serves.
  listening: Cell<usize>,
  /// How long the edge's honest connections took to show the bearer (A-523).
  arrivals: RefCell<Arrivals>,
}

impl Shared {
  /// The bound a connection accepted now gets to show the bearer ([`Arrivals::bound_ns`]); the prior's when the
  /// estimate is lent out, which no task on the shard holds across a wait.
  fn bound_ns(&self, granularity_ns: u64) -> u64 {
    self.arrivals.try_borrow().map_or_else(
      |_| Arrivals::default().bound_ns(granularity_ns),
      |arrivals| arrivals.bound_ns(granularity_ns),
    )
  }

  /// Folds in one honest arrival ([`Arrivals::arrived`]); lent out, the estimate loses this sample and nothing else.
  fn learn(&self, arrival_ns: u64) {
    if let Ok(mut arrivals) = self.arrivals.try_borrow_mut() {
      arrivals.arrived(arrival_ns);
    }
  }
}

/// A fresh connection before it has shown the bearer: when it was accepted and when its waits end (the module doc's
/// rule), both on the shard's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Unproven {
  /// When the connection was accepted.
  accepted_ns: u64,
  /// Its acceptance plus the bound learned then ([`Arrivals::bound_ns`]): where every wait on it ends until it shows
  /// the bearer.
  deadline_ns: u64,
}

/// How long the edge waits on a connection's peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
  /// It has not shown the bearer: every wait ends at this instant on the shard's clock.
  Until(u64),
  /// It has: its waits run while it lives, until it closes or a read fails.
  WhileItLives,
}

/// How a fresh connection's wait for the edge's bearer ended ([`await_bearer`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proof {
  /// A field line carrying the bearer arrived: the connection is the token holder's.
  Shown {
    /// From the connection's acceptance to the bearer's arrival, nanoseconds: an honest arrival.
    arrival_ns: u64,
  },
  /// Its first request's head ended without the bearer, outgrew its bounds or was cut short by the peer's close: the
  /// request is refused, within the connection's deadline.
  Absent,
  /// The peer closed or a read failed before a byte came, or the deadline passed first: the connection ends unanswered.
  Gone,
}

impl Proof {
  /// The wait the connection keeps from here (`None` once it is gone): while it lives once it has shown the bearer, to
  /// its deadline otherwise.
  fn wait(self, unproven: Unproven) -> Option<Wait> {
    match self {
      Proof::Shown { .. } => Some(Wait::WhileItLives),
      Proof::Absent => Some(Wait::Until(unproven.deadline_ns)),
      Proof::Gone => None,
    }
  }
}

/// What one read for a connection brought ([`fill_within`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filled {
  /// Bytes, kept in the connection's pending bytes.
  Bytes,
  /// The end of the stream, or a failed read.
  Ended,
  /// Nothing before the connection's deadline (or no timer could be armed for it).
  CutOff,
}

impl Filled {
  /// How a wait for the bearer ends at this read, if it does, `pending` holding what arrived before it.
  fn ends_proof(self, pending: &[u8]) -> Option<Proof> {
    match self {
      Filled::Bytes => None,
      // A head cut short by the peer's close is refused `400`, as any truncated head is.
      Filled::Ended if !pending.is_empty() => Some(Proof::Absent),
      Filled::Ended | Filled::CutOff => Some(Proof::Gone),
    }
  }
}

/// What a dispatched request is answered with: one JSON body under a status, or a `subscriptions/listen` stream that
/// opens with the acknowledgement.
enum Answer {
  /// A status and the JSON-RPC reply (none for a notification).
  Json(&'static str, Option<Value>),
  /// A listen stream: its acknowledgement, and the request's id for a refusal past the bound.
  Listen(Value, Value),
}

/// Serves MCP on `listener` under `edge` until the listener fails (§4.12). Runs on the calling thread's
/// slates shard: the caller drives it to completion on a [`slates_rt::runtime::LocalRuntime`] built from the
/// edge's derived runtime configuration. Each connection is its own task; a refusal or failure ends that
/// connection, never the listener.
pub async fn serve(
  server: McpServer,
  listener: TcpListener,
  edge: HttpEdge,
) -> Result<(), slates_rt::error::RtError> {
  let shared = slates_rt::registry::with_current(|context| {
    context.keep(Shared {
      server: RefCell::new(server),
      serving: Cell::new(0),
      listening: Cell::new(0),
      arrivals: RefCell::new(Arrivals::default()),
    })
  })
  .ok_or(slates_rt::error::RtError::NotOnShardThread)??;
  loop {
    let stream = listener.accept().await?;
    let accepted_ns = now_ns();
    let admitted = shared
      .with(|shared| {
        let serving = shared.serving.get();
        let admit = serving < edge.connections;
        if admit {
          shared.serving.set(serving.saturating_add(1));
        }
        admit
      })
      .unwrap_or(false);
    if !admitted {
      let _ = write_refusal(&stream, Refused::Unavailable).await;
      continue;
    }
    // Fixed at its acceptance: as long as an honest client has been taking to show the bearer, as learned so far.
    let granularity = granularity_ns(&edge);
    let bound_ns = shared
      .with(|shared| shared.bound_ns(granularity))
      .unwrap_or_else(|| Arrivals::default().bound_ns(granularity));
    let unproven = Unproven {
      accepted_ns,
      deadline_ns: accepted_ns.saturating_add(bound_ns),
    };
    let connection_edge = edge.clone();
    let spawned = spawn_child(async move {
      serve_connection(&stream, shared, &connection_edge, unproven).await;
      let _ = shared.with(|shared| shared.serving.set(shared.serving.get().saturating_sub(1)));
    });
    match spawned {
      Ok(task) => {
        let _ = detach(task);
      }
      Err(_) => {
        let _ = shared.with(|shared| shared.serving.set(shared.serving.get().saturating_sub(1)));
      }
    }
  }
}

/// Serves one connection until the peer closes it, a request is refused, or, before it has shown the bearer, its
/// deadline passes (the module doc's rule).
async fn serve_connection(
  stream: &TcpStream,
  shared: Kept<Shared>,
  edge: &HttpEdge,
  unproven: Unproven,
) {
  let mut pending: Vec<u8> = Vec::new();
  let proof = await_bearer(stream, &mut pending, edge, unproven).await;
  if let Proof::Shown { arrival_ns } = proof {
    // Off its shard's context the sample is lost, and the bound keeps what it has learned.
    let _ = shared.with(|shared| shared.learn(arrival_ns));
  }
  let Some(wait) = proof.wait(unproven) else {
    return;
  };
  loop {
    let (head, body) = match read_request(stream, &mut pending, edge, wait).await {
      Ok(Some(request)) => request,
      Err(refused) => {
        refuse(stream, refused, wait).await;
        return;
      }
      // The peer closed or a read failed, or the connection's deadline passed before it showed the bearer.
      Ok(None) => return,
    };
    let reply = shared
      .with(|shared| {
        let Ok(mut server) = shared.server.try_borrow_mut() else {
          return None;
        };
        Some(dispatch(&mut server, &head, &body))
      })
      .flatten();
    let (status, reply) = match reply {
      Some(Answer::Json(status, reply)) => (status, reply),
      Some(Answer::Listen(acknowledgement, id)) => {
        // A listen holds its connection until the client closes it (Streamable HTTP: closing the stream cancels the
        // request), so the connection ends with the stream.
        let admitted = shared
          .with(|shared| {
            let open = shared.listening.get();
            let admit = open < crate::max_subscriptions();
            if admit {
              shared.listening.set(open.saturating_add(1));
            }
            (admit, open)
          })
          .unwrap_or((false, 0));
        if admitted.0 {
          stream_listen(stream, &acknowledgement, edge).await;
          let _ = shared.with(|shared| {
            shared
              .listening
              .set(shared.listening.get().saturating_sub(1))
          });
          return;
        }
        let (code, message) = crate::subscriptions_full(admitted.1);
        (OK, Some(crate::error(&id, code, &message)))
      }
      None => {
        refuse(stream, Refused::Unavailable, wait).await;
        return;
      }
    };
    if stream
      .write_all(&response(status, reply, head.keep_alive))
      .await
      .is_err()
      || !head.keep_alive
    {
      return;
    }
  }
}

/// Reads a fresh connection's first request until a field line carrying the edge's bearer has arrived, its head has
/// ended without one or outgrown its bounds, its peer has closed, or its deadline has passed (the module doc's rule).
/// What arrived stays in `pending` for [`read_request`].
async fn await_bearer(
  stream: &TcpStream,
  pending: &mut Vec<u8>,
  edge: &HttpEdge,
  unproven: Unproven,
) -> Proof {
  let mut scanned = 0usize;
  loop {
    match scan_lines(edge, pending, &mut scanned) {
      Lines::Bearer => {
        return Proof::Shown {
          arrival_ns: now_ns().saturating_sub(unproven.accepted_ns),
        };
      }
      Lines::HeadEnded => return Proof::Absent,
      Lines::Partial if outgrown(pending) => return Proof::Absent,
      Lines::Partial => {}
    }
    let filled = fill_within(stream, pending, Wait::Until(unproven.deadline_ns)).await;
    if let Some(proof) = filled.ends_proof(pending) {
      return proof;
    }
  }
}

/// Reads one request off `stream` — the head, then its body — keeping any bytes past it in `pending` for
/// the next, each read under `wait`. `Ok(None)` when the peer closed before a request began or the wait ended;
/// `Err` when it is refused.
async fn read_request(
  stream: &TcpStream,
  pending: &mut Vec<u8>,
  edge: &HttpEdge,
  wait: Wait,
) -> Result<Option<(Head, Vec<u8>)>, Refused> {
  let Some(head_end) = read_head(stream, pending, wait).await? else {
    return Ok(None);
  };
  let head = parse_head(pending.get(..head_end).unwrap_or_default())?;
  authorize(edge, &head)?;
  let body_len =
    usize::try_from(head.content_length.unwrap_or(0)).map_err(|_| Refused::TooLarge)?;
  let body_start = head_end.saturating_add(HEAD_END.len());
  let body_end = body_start.saturating_add(body_len);
  if !read_body(stream, pending, body_end, wait).await? {
    return Ok(None);
  }
  let body = pending
    .get(body_start..body_end)
    .unwrap_or_default()
    .to_vec();
  pending.drain(..body_end);
  Ok(Some((head, body)))
}

/// Reads until `pending` holds a whole request head, under `wait`: where the head ends; `None` when the peer closed
/// before a request began or the wait ended; refused when the head outgrew its bounds or was cut short.
async fn read_head(
  stream: &TcpStream,
  pending: &mut Vec<u8>,
  wait: Wait,
) -> Result<Option<usize>, Refused> {
  loop {
    if let Some(at) = pending
      .windows(HEAD_END.len())
      .position(|window| window == HEAD_END)
    {
      return Ok(Some(at));
    }
    if outgrown(pending) {
      return Err(head_overflow(pending));
    }
    match fill_within(stream, pending, wait).await {
      Filled::Bytes => {}
      Filled::Ended if pending.is_empty() => return Ok(None),
      Filled::Ended => return Err(Refused::BadRequest), // a truncated head
      Filled::CutOff => return Ok(None),
    }
  }
}

/// Reads until `pending` holds `end` bytes, under `wait`: `false` when the wait ended first; refused when the peer
/// closed first (a truncated body).
async fn read_body(
  stream: &TcpStream,
  pending: &mut Vec<u8>,
  end: usize,
  wait: Wait,
) -> Result<bool, Refused> {
  while pending.len() < end {
    match fill_within(stream, pending, wait).await {
      Filled::Bytes => {}
      Filled::Ended => return Err(Refused::BadRequest),
      Filled::CutOff => return Ok(false),
    }
  }
  Ok(true)
}

/// Whether the head read so far in `pending` has outgrown its bounds: its bytes, or the line still arriving.
fn outgrown(pending: &[u8]) -> bool {
  pending.len() > MAX_HEAD_BYTES || line_too_long(pending)
}

/// Whether the line still being read in `pending` (after its last line break) is past its bound.
fn line_too_long(pending: &[u8]) -> bool {
  let start = pending
    .windows(2)
    .rposition(|window| window == b"\r\n")
    .map_or(0, |at| at.saturating_add(2));
  pending.len().saturating_sub(start) > MAX_LINE_BYTES
}

/// The refusal for a head that outgrew its bounds: the request line, or the fields.
fn head_overflow(pending: &[u8]) -> Refused {
  if pending.windows(2).any(|window| window == b"\r\n") {
    Refused::HeadersTooLarge
  } else {
    Refused::UriTooLong
  }
}

/// Reads what the peer has sent into `pending`; `false` at end of stream or on a read failure.
async fn fill(stream: &TcpStream, pending: &mut Vec<u8>) -> bool {
  let mut buffer = [0u8; MAX_LINE_BYTES];
  match stream.read(&mut buffer).await {
    Ok(0) | Err(_) => false,
    Ok(read) => {
      pending.extend_from_slice(buffer.get(..read).unwrap_or_default());
      true
    }
  }
}

/// Reads what the peer has sent into `pending` under `wait`: until its deadline before the connection showed the bearer
/// (bytes already there are taken even at the deadline, since the read is asked first), with no clock after.
async fn fill_within(stream: &TcpStream, pending: &mut Vec<u8>, wait: Wait) -> Filled {
  let read = match wait {
    Wait::WhileItLives => Some(fill(stream, pending).await),
    // The deadline passed, or no timer could be armed: either way this wait is over.
    Wait::Until(deadline_ns) => within(deadline_ns.saturating_sub(now_ns()), fill(stream, pending))
      .await
      .ok()
      .flatten(),
  };
  match read {
    Some(true) => Filled::Bytes,
    Some(false) => Filled::Ended,
    None => Filled::CutOff,
  }
}

/// Dispatches one JSON-RPC body: the reply, or `None` for a notification. A body that is not JSON draws
/// the JSON-RPC parse error.
fn dispatch(server: &mut McpServer, head: &Head, body: &[u8]) -> Answer {
  let Ok(message) = serde_json::from_slice::<Value>(body) else {
    // A body the server cannot accept is an HTTP error (Streamable HTTP: "e.g., `400 Bad Request`").
    return Answer::Json(BAD_REQUEST, Some(crate::parse_error_reply()));
  };
  let id = message.get("id").cloned().unwrap_or(Value::Null);
  // A modern request without its required `_meta`, or a method the modern era removed, is refused before the
  // header checks (SEP-2575: missing `_meta` fields are `400`, a removed method `404`).
  let params = message.get("params").cloned().unwrap_or(Value::Null);
  let method = message.get("method").and_then(Value::as_str).unwrap_or("");
  // A header and a body naming different versions disagree, whatever either names (`HeaderMismatch` first).
  let body_version = params
    .get("_meta")
    .and_then(|meta| meta.get(crate::META_PROTOCOL_VERSION_KEY))
    .and_then(Value::as_str);
  if let (Some(header), Some(body)) = (head.protocol_version.as_deref(), body_version)
    && header != body
  {
    let mismatch = format!(
      "Header mismatch: MCP-Protocol-Version header value '{header}' does not match body value '{body}'"
    );
    return Answer::Json(
      BAD_REQUEST,
      Some(crate::header_mismatch_reply(&id, &mismatch)),
    );
  }
  let modern = match crate::era(&params, method, head.protocol_version.as_deref()) {
    Ok(modern) => modern,
    Err(refusal) => {
      let status = if refusal.code == crate::METHOD_NOT_FOUND_CODE {
        NOT_FOUND
      } else {
        BAD_REQUEST
      };
      return Answer::Json(status, Some(refusal.reply(&id)));
    }
  };
  if let Err(mismatch) = check_headers(head, &message) {
    return Answer::Json(
      BAD_REQUEST,
      Some(crate::header_mismatch_reply(&id, &mismatch)),
    );
  }
  // A modern `subscriptions/listen` with an id is answered by a stream (Streamable HTTP 2026-07-28: "the server's
  // response is itself an SSE stream that stays open"); its acknowledgement is the rule stdio uses.
  if method == "subscriptions/listen" && modern && message.get("id").is_some() {
    return Answer::Listen(crate::listen_acknowledgement(&id, &params), id);
  }
  let reply = server.handle_with_header(&message, head.protocol_version.as_deref());
  let status = match reply
    .as_ref()
    .and_then(|reply| reply.get("error"))
    .and_then(|error| error.get("code"))
    .and_then(Value::as_i64)
  {
    Some(crate::UNSUPPORTED_PROTOCOL_VERSION_CODE) => BAD_REQUEST,
    Some(crate::METHOD_NOT_FOUND_CODE) => NOT_FOUND,
    _ => OK,
  };
  Answer::Json(status, reply)
}

/// Format: the head of a listen stream's response (Streamable HTTP 2026-07-28): an SSE stream, never cached, with
/// `X-Accel-Buffering: no` so a reverse proxy delivers each event at once, and closed with the connection.
const LISTEN_HEAD: &[u8] =
  b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
X-Accel-Buffering: no\r\nConnection: close\r\n\r\n";

/// Format: the SSE comment line a quiet listen stream sends as its keep-alive (Streamable HTTP 2026-07-28: "a line
/// beginning with a colon ... clients must ignore").
const KEEP_ALIVE: &[u8] = b":\r\n";

/// Serves a `subscriptions/listen` over Streamable HTTP: an SSE response whose first event is `acknowledgement`, held
/// open until the client closes it, which is the request's cancellation (the specification's "Cancellation": nothing
/// is sent after). Nothing the server publishes changes while it runs, so the only further bytes are keep-alive
/// comments, one each [`HttpEdge::keep_alive_ns`] ([`listen_keep_alive_ns`]): a party that closes a connection once it
/// has been idle longer than that sees one first. Bytes from the client end the stream: a listen takes none.
async fn stream_listen(stream: &TcpStream, acknowledgement: &Value, edge: &HttpEdge) {
  let event = format!("data: {acknowledgement}\n\n");
  if stream.write_all(LISTEN_HEAD).await.is_err()
    || stream.write_all(event.as_bytes()).await.is_err()
  {
    return;
  }
  let keep_alive_ns = edge.keep_alive_ns.max(1);
  let mut buffer = [0u8; MAX_LINE_BYTES];
  loop {
    match within(keep_alive_ns, stream.read(&mut buffer)).await {
      Ok(None) => {
        if stream.write_all(KEEP_ALIVE).await.is_err() {
          return;
        }
      }
      // The client closed (the cancellation), sent bytes, or the read or the timer failed.
      Ok(Some(_)) | Err(_) => return,
    }
  }
}

/// Format: the status lines a dispatched request is answered with.
const OK: &str = "200 OK";
/// Format: see [`OK`].
const BAD_REQUEST: &str = "400 Bad Request";
/// Format: see [`OK`].
const NOT_FOUND: &str = "404 Not Found";

/// Checks a request's MCP headers against its body (Streamable HTTP 2026-07-28, "Server Validation"): a modern
/// request (one naming its version in `_meta`) must carry `MCP-Protocol-Version` equal to that version, `Mcp-Method`
/// equal to its method, and, for `tools/call`, `resources/read` and `prompts/get`, `Mcp-Name` equal to its
/// `params.name` or `params.uri` (decoded first when in the Base64 sentinel form). A legacy request (no version in
/// `_meta`) may omit the version header, which then means 2025-03-26, a revision this server serves; one it gives
/// must be a revision this server speaks. The mismatch, said in words, when a check fails.
fn check_headers(head: &Head, message: &Value) -> Result<(), String> {
  let params = message.get("params");
  let body_version = params
    .and_then(|params| params.get("_meta"))
    .and_then(|meta| meta.get(crate::META_PROTOCOL_VERSION_KEY))
    .and_then(Value::as_str);
  let Some(body_version) = body_version else {
    return match head.protocol_version.as_deref() {
      Some(version) if !crate::speaks(version) => Err(format!(
        "MCP-Protocol-Version header value '{version}' is not a revision this server speaks"
      )),
      _ => Ok(()),
    };
  };
  let method = message.get("method").and_then(Value::as_str).unwrap_or("");
  same(
    "MCP-Protocol-Version",
    head.protocol_version.as_deref(),
    body_version,
  )?;
  same("Mcp-Method", head.mcp_method.as_deref(), method)?;
  let named = match method {
    "tools/call" | "prompts/get" => params.and_then(|params| params.get("name")),
    "resources/read" => params.and_then(|params| params.get("uri")),
    _ => None,
  };
  if let Some(name) = named {
    let name = name.as_str().unwrap_or("");
    let header = head.mcp_name.as_deref().map(decode_sentinel).transpose()?;
    same("Mcp-Name", header.as_deref(), name)?;
  }
  Ok(())
}

/// `Ok` when header `field` was given and equals `body`; the mismatch in words otherwise.
fn same(field: &str, header: Option<&str>, body: &str) -> Result<(), String> {
  match header {
    Some(value) if value == body => Ok(()),
    Some(value) => Err(format!(
      "Header mismatch: {field} header value '{value}' does not match body value '{body}'"
    )),
    None => Err(format!(
      "Header mismatch: the required {field} header is missing"
    )),
  }
}

/// Format: the Base64 sentinel's prefix and suffix (Streamable HTTP 2026-07-28, "Value Encoding").
const SENTINEL_PREFIX: &str = "=?base64?";
/// Format: see [`SENTINEL_PREFIX`].
const SENTINEL_SUFFIX: &str = "?=";

/// A header value as the body would carry it: the UTF-8 text a Base64 sentinel (`=?base64?…?=`) encodes, or the
/// value itself when it is not one.
fn decode_sentinel(value: &str) -> Result<String, String> {
  let Some(encoded) = value
    .strip_prefix(SENTINEL_PREFIX)
    .and_then(|rest| rest.strip_suffix(SENTINEL_SUFFIX))
  else {
    return Ok(value.to_owned());
  };
  let bytes = base64_decode(encoded)
    .ok_or_else(|| "Header mismatch: a malformed Base64 sentinel".to_owned())?;
  String::from_utf8(bytes)
    .map_err(|_| "Header mismatch: a Base64 sentinel that is not UTF-8".to_owned())
}

/// Format: the standard Base64 alphabet (RFC 4648 §4, Table 1): each character's position is its value.
const BASE64_ALPHABET: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Decodes standard padded Base64 (RFC 4648 §4); `None` for anything else.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
  /// Format: the bits one Base64 character carries, and the bits of a byte.
  const SEXTET: u32 = 6;
  /// Format: see [`SEXTET`].
  const OCTET: u32 = 8;
  /// Format: a byte's mask, and the bits the accumulator keeps (never more than a byte plus a sextet).
  const BYTE: u32 = 0xFF;
  /// Format: see [`BYTE`].
  const KEPT: u32 = 0x3FFF;
  let digits = text.trim_end_matches('=');
  if !text.len().is_multiple_of(4) || text.len().saturating_sub(digits.len()) > 2 {
    return None;
  }
  let mut out = Vec::with_capacity(digits.len());
  let mut buffer: u32 = 0;
  let mut held: u32 = 0;
  for byte in digits.bytes() {
    let value = BASE64_ALPHABET.iter().position(|&digit| digit == byte)?;
    buffer = (buffer.checked_shl(SEXTET)? | u32::try_from(value).ok()?) & KEPT;
    held = held.checked_add(SEXTET)?;
    if held >= OCTET {
      held = held.checked_sub(OCTET)?;
      out.push(u8::try_from(buffer.checked_shr(held)? & BYTE).ok()?);
    }
  }
  Some(out)
}

/// The response bytes for a dispatched request: `status` with the JSON reply, or `202` for a notification.
fn response(status: &str, reply: Option<Value>, keep_alive: bool) -> Vec<u8> {
  let connection = if keep_alive { "keep-alive" } else { "close" };
  match reply {
    Some(value) => {
      let body = value.to_string();
      format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: \
         {connection}\r\n\r\n{body}",
        body.len()
      )
      .into_bytes()
    }
    None => {
      format!("HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: {connection}\r\n\r\n")
        .into_bytes()
    }
  }
}

/// Answers `refused` and ends the connection in stages (RFC 9112 §9.6, "Tear-down"): the refusal (for `401` with the
/// challenge naming the scheme), a half-close of the edge's side, then what the client still sends read and dropped
/// until it closes its side, a read fails, or, for a connection that has not shown the bearer, its deadline passes.
/// Ended outright with the client's bytes unread, the connection is reset, and the reset can erase the refusal before
/// the client reads it (§9.6: "the reset packet might erase the client's unacknowledged input buffers"). On Windows it
/// did, every run: a client that sent a body past the bound read `ConnectionAborted`, never `413` (2026-10-26). The
/// drain keeps no byte; it holds a connection's slot no longer than its `wait` (the module doc's rule): a token holder
/// that sent a body past the bound is drained whole, however long it takes to send it.
async fn refuse(stream: &TcpStream, refused: Refused, wait: Wait) {
  if write_refusal(stream, refused).await.is_err() || stream.close_write().is_err() {
    return;
  }
  // However the drain ends (the client closed, a read failed, the deadline passed, or no timer could be armed), the
  // connection ends here.
  match wait {
    Wait::Until(deadline_ns) => {
      let _ = within(deadline_ns.saturating_sub(now_ns()), drain(stream)).await;
    }
    Wait::WhileItLives => drain(stream).await,
  }
}

/// Reads and drops what the peer sends until it closes its side or a read fails, in reads the size a request is read
/// in.
async fn drain(stream: &TcpStream) {
  let mut dropped = [0u8; MAX_LINE_BYTES];
  while let Ok(read) = stream.read(&mut dropped).await {
    if read == 0 {
      break;
    }
  }
}

/// Writes `refused` as a response that closes the connection. Within a served connection the close is staged by
/// [`refuse`]; a connection past the bound is answered here and closed at once, since the accept loop does not wait on a
/// connection it does not serve, so a request that is already sent can reset it and erase the `503` (GAPS
/// `8a-platforms`).
async fn write_refusal(
  stream: &TcpStream,
  refused: Refused,
) -> Result<(), slates_rt::error::RtError> {
  let challenge = if refused == Refused::Unauthorized {
    "WWW-Authenticate: Bearer\r\n"
  } else {
    ""
  };
  let text = format!(
    "HTTP/1.1 {}\r\n{challenge}Content-Length: 0\r\nConnection: close\r\n\r\n",
    refused.status()
  );
  stream.write_all(text.as_bytes()).await
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_rt::runtime::{Runtime, RuntimeConfig};
  use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
  use std::sync::mpsc::channel;
  use std::time::Duration;

  fn edge() -> HttpEdge {
    HttpEdge {
      port: 7000,
      bearer: [0xab; BEARER_BYTES],
      tick_ns: TICK_NS,
      keep_alive_ns: 1,
      connections: 1,
    }
  }

  /// A head as an MCP client sends it, with `extra` field lines appended.
  fn head_with(extra: &[&str]) -> String {
    let mut lines = vec![
      "POST /mcp HTTP/1.1".to_owned(),
      "Host: 127.0.0.1:7000".to_owned(),
      "Content-Type: application/json".to_owned(),
      "Content-Length: 2".to_owned(),
      format!("Authorization: Bearer {}", edge().bearer_hex()),
    ];
    lines.extend(extra.iter().map(|line| (*line).to_owned()));
    lines.join("\r\n")
  }

  fn decided(raw: &str) -> Result<(), Refused> {
    parse_head(raw.as_bytes()).and_then(|head| authorize(&edge(), &head))
  }

  /// AUD-29-24: do: authorize an MCP client's request, then the same with each authority defect — a
  /// foreign origin, a rebound host, a missing or repeated host, no or a wrong token, another path or
  /// method, a form body; expect the first admitted and each defect refused with its status.
  #[test]
  fn only_this_edges_own_callers_are_admitted() {
    assert_eq!(decided(&head_with(&[])), Ok(()));
    assert_eq!(
      decided(&head_with(&["Origin: http://localhost:7000"])),
      Ok(())
    );
    let refusals = [
      (
        head_with(&["Origin: https://evil.example"]),
        Refused::Forbidden,
      ),
      (
        head_with(&["Origin: http://127.0.0.1:7001"]),
        Refused::Forbidden,
      ),
      (
        head_with(&[]).replace("Host: 127.0.0.1:7000", "Host: rebound.example:7000"),
        Refused::Misdirected,
      ),
      (
        head_with(&[]).replace("Host: 127.0.0.1:7000\r\n", ""),
        Refused::BadRequest,
      ),
      (head_with(&["Host: 127.0.0.1:7000"]), Refused::BadRequest),
      (
        head_with(&[]).replace(&edge().bearer_hex(), &"cd".repeat(BEARER_BYTES)),
        Refused::Unauthorized,
      ),
      (
        head_with(&[]).replace(
          &format!("Authorization: Bearer {}", edge().bearer_hex()),
          "X-Other: v",
        ),
        Refused::Unauthorized,
      ),
      (
        head_with(&[]).replace("POST /mcp", "POST /"),
        Refused::NotFound,
      ),
      (
        head_with(&[]).replace("POST /mcp", "GET /mcp"),
        Refused::MethodNotAllowed,
      ),
      (
        head_with(&[]).replace("application/json", "application/x-www-form-urlencoded"),
        Refused::UnsupportedMediaType,
      ),
    ];
    for (raw, expected) in refusals {
      assert_eq!(decided(&raw), Err(expected), "{raw}");
    }
  }

  /// AUD-29-23 hostile input: do: parse heads with a line past the bound, too many fields, two
  /// contradictory lengths, a chunked body, a non-numeric length, a length past the body cap and none on a
  /// POST; expect each refused with its status, and a repeated identical length accepted.
  #[test]
  fn malformed_or_oversized_heads_are_refused() {
    let long = format!("X-Long: {}", "a".repeat(MAX_LINE_BYTES));
    let many: Vec<String> = (0..=MAX_FIELDS).map(|at| format!("X-{at}: v")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let cases = [
      (head_with(&[&long]), Refused::HeadersTooLarge),
      (head_with(&many), Refused::HeadersTooLarge),
      (head_with(&["Content-Length: 3"]), Refused::BadRequest),
      (
        head_with(&["Transfer-Encoding: chunked"]),
        Refused::BadRequest,
      ),
      (
        head_with(&[]).replace("Content-Length: 2", "Content-Length: 2x"),
        Refused::BadRequest,
      ),
      (
        head_with(&[]).replace(
          "Content-Length: 2",
          &format!("Content-Length: {}", MAX_BODY + 1),
        ),
        Refused::TooLarge,
      ),
      (
        head_with(&[]).replace("Content-Length: 2\r\n", ""),
        Refused::LengthRequired,
      ),
      (
        format!("POST /{} HTTP/1.1", "a".repeat(MAX_LINE_BYTES)),
        Refused::UriTooLong,
      ),
    ];
    for (raw, expected) in cases {
      assert_eq!(decided(&raw), Err(expected), "{}", raw.len());
    }
    assert_eq!(decided(&head_with(&["Content-Length: 2"])), Ok(()));
  }

  /// A refusal's status line, and the `401` challenge, as RFC 9110 writes them.
  #[test]
  fn responses_format_correctly() {
    let text =
      String::from_utf8(response(OK, Some(serde_json::json!({"ok": true})), true)).unwrap();
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(text.contains("Content-Length: 11\r\n"));
    assert!(text.ends_with("{\"ok\":true}"));
    let text = String::from_utf8(response(OK, None, false)).unwrap();
    assert!(text.starts_with("HTTP/1.1 202 Accepted\r\n"));
    assert!(text.contains("Connection: close\r\n"));
  }

  /// Streamable HTTP 2026-07-28 "Value Encoding": do decode the spec's own sentinel examples; expect each original
  /// value, a plain value passed through, and malformed Base64 refused rather than guessed.
  #[test]
  fn sentinel_values_decode_as_the_spec_encodes_them() {
    assert_eq!(
      decode_sentinel("=?base64?SGVsbG8sIOS4lueVjA==?=").unwrap(),
      "Hello, 世界"
    );
    assert_eq!(
      decode_sentinel("=?base64?IHBhZGRlZCA=?=").unwrap(),
      " padded "
    );
    assert_eq!(
      decode_sentinel("=?base64?bGluZTEKbGluZTI=?=").unwrap(),
      "line1\nline2"
    );
    assert_eq!(
      decode_sentinel("=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=").unwrap(),
      "=?base64?literal?="
    );
    assert_eq!(decode_sentinel("us-west1").unwrap(), "us-west1");
    assert!(
      decode_sentinel("=?base64?abc?=").is_err(),
      "not a multiple of four"
    );
    assert!(
      decode_sentinel("=?base64?ab*d?=").is_err(),
      "outside the alphabet"
    );
  }

  /// Shape: the timer tick of a test's runtime and edge, 100 µs (the S3 client's tests run theirs at it): the
  /// granularity `G`.
  const TICK_NS: u64 = 100_000;
  /// Shape: a quick honest arrival, 50 µs: a loopback client that writes its request as soon as it has connected.
  const QUICK_NS: u64 = 50_000;
  /// Shape: the honest arrivals the learning test folds in after its first.
  const MORE: u64 = 998;
  /// Shape: the bound the edge held every connection to before A-523, whatever it had measured: the client's reply
  /// deadline, a second.
  const OLD_DEADLINE_NS: u64 = 1_000_000_000;
  /// Shape: an honest arrival the socket test's edge has learned, 1 ms; with [`LEARNED`] of them its bound is about
  /// 68 ms, long enough that a loaded machine reads a prompt client's bytes inside it and short enough to watch a
  /// cut-off.
  const LEARNED_NS: u64 = 1_000_000;
  /// Shape: how many honest arrivals the socket test's edge has learned.
  const LEARNED: u64 = 9;
  /// Shape: how many bytes a dribbling sender sends, one at a time, across one bound.
  const DRIBBLES: u64 = 8;
  /// Shape: how long the socket test waits for its scenario, well past the old deadline.
  const SCENARIO_WAIT: Duration = Duration::from_secs(20);

  /// A-523. Do: ask the bound with nothing measured; fold in one honest arrival of 50 µs, then 998 more; ask it of a
  /// shard whose scheduler has been running it two seconds late. Expect: the estimator's prior at first; under it and
  /// past half of it after one arrival; after 999, the learned tail (`SRTT + max(G, 4·RTTVAR)`, 150 µs here) plus a
  /// thousandth of the prior, under a millisecond, where the edge gave every connection a second before A-523 whatever
  /// it had measured; and never below the granularity a loaded shard shows.
  #[test]
  fn the_bound_starts_at_the_prior_and_falls_to_the_tail_of_the_honest_arrivals() {
    let mut arrivals = Arrivals::default();
    assert_eq!(
      arrivals.bound_ns(TICK_NS),
      PRIOR_NS,
      "nothing measured: the prior"
    );
    arrivals.arrived(QUICK_NS);
    let after_one = arrivals.bound_ns(TICK_NS);
    assert!(
      after_one > PRIOR_NS / 2 && after_one < PRIOR_NS,
      "one arrival halves the prior's part: {after_one} ns"
    );
    for _ in 0..MORE {
      arrivals.arrived(QUICK_NS);
    }
    assert_eq!(arrivals.samples(), MORE + 1, "every arrival folded in");
    let learned = arrivals.bound_ns(TICK_NS);
    let tail = QUICK_NS + TICK_NS;
    assert!(
      learned >= tail && learned <= tail + PRIOR_NS / (MORE + 2),
      "the tail of {} arrivals of {QUICK_NS} ns and a thousandth of the prior: {learned} ns",
      MORE + 1
    );
    assert!(
      learned < OLD_DEADLINE_NS / 1_000,
      "under a millisecond, not the old second: {learned} ns"
    );
    let late = 2 * OLD_DEADLINE_NS;
    assert!(
      arrivals.bound_ns(late) >= late,
      "never below a loaded shard's granularity"
    );
  }

  /// A-523 hostile input. Do: scan heads, as they arrive, for the edge's bearer: one whose bearer line has arrived before
  /// the head's end; one cut inside its bearer line, then completed; the field's name in lower case; a foreign token; the
  /// token under another scheme, in the request line, and past the blank line. Expect: the bearer shown by the first, by
  /// the cut one only once its line is complete, and by the lower-case name; the head's end, with no bearer, for the
  /// rest.
  #[test]
  fn only_a_complete_field_line_carrying_the_bearer_proves_a_connection() {
    let token = edge().bearer_hex();
    let scan = |head: &str| scan_lines(&edge(), head.as_bytes(), &mut 0);
    assert_eq!(
      scan(&format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:7000\r\nAuthorization: Bearer {token}\r\n"
      )),
      Lines::Bearer
    );
    let mut cut = format!("POST /mcp HTTP/1.1\r\nAuthorization: Bearer {token}").into_bytes();
    let mut scanned = 0;
    assert_eq!(scan_lines(&edge(), &cut, &mut scanned), Lines::Partial);
    cut.extend_from_slice(CRLF);
    assert_eq!(scan_lines(&edge(), &cut, &mut scanned), Lines::Bearer);
    assert_eq!(
      scan(&format!(
        "POST /mcp HTTP/1.1\r\nauthorization: Bearer {token}\r\n"
      )),
      Lines::Bearer
    );
    let foreign = "cd".repeat(BEARER_BYTES);
    for head in [
      format!("POST /mcp HTTP/1.1\r\nAuthorization: Bearer {foreign}\r\n\r\n"),
      format!("POST /mcp HTTP/1.1\r\nAuthorization: Basic {token}\r\n\r\n"),
      format!("POST /Authorization: Bearer {token} HTTP/1.1\r\n\r\n"),
      format!("POST /mcp HTTP/1.1\r\n\r\nAuthorization: Bearer {token}\r\n"),
    ] {
      assert_eq!(scan(&head), Lines::HeadEnded, "{head}");
    }
  }

  /// A runtime of one shard for a test.
  fn config() -> RuntimeConfig {
    RuntimeConfig {
      shards: 1,
      tasks_per_shard: 64,
      timers_per_shard: 64,
      ring_entries: 64,
      step_budget_ns: 1_000_000_000,
      timer_tick_ns: TICK_NS,
      batch: 64,
      pin: false,
      cores: Vec::new(),
      page_bytes: 4096,
      spin_ns: 0,
      wake_tracking: None,
    }
  }

  /// What a test's client does once connected.
  #[derive(Clone, Copy, Debug)]
  enum Sender {
    /// Sends nothing.
    Silent,
    /// Sends the start of a head with no bearer, then one more byte of it each `pause_ns`, never ending it.
    Dribbling { pause_ns: u64 },
    /// Sends its head through its bearer line, then the rest of its request `pause_ns` later.
    Split { pause_ns: u64 },
  }

  /// What the edge's side saw of one test connection: how its wait for the bearer ended, what reading its first
  /// request then gave, and when it ended after the acceptance.
  #[derive(Debug)]
  struct EdgeSaw {
    proof: Proof,
    request: Option<FirstRequest>,
    ended_ns: u64,
  }

  /// What reading a test connection's first request gave: its head and body, nothing (the connection ended), or the
  /// refusal.
  type FirstRequest = Result<Option<(Head, Vec<u8>)>, Refused>;

  /// What a test's client saw: the bytes it dribbled after its start, and the bytes the edge answered before its end.
  #[derive(Debug)]
  struct ClientSaw {
    dribbled: usize,
    answered: usize,
  }

  /// The edge's side of one fresh connection, read as `serve_connection` reads it: accepted under `bound_ns`, its wait
  /// for the bearer, then its first request under the wait that leaves.
  async fn edge_side(listener: &TcpListener, edge: &HttpEdge, bound_ns: u64) -> EdgeSaw {
    let stream = listener.accept().await.unwrap();
    let accepted_ns = now_ns();
    let unproven = Unproven {
      accepted_ns,
      deadline_ns: accepted_ns + bound_ns,
    };
    let mut pending = Vec::new();
    let proof = await_bearer(&stream, &mut pending, edge, unproven).await;
    let request = match proof.wait(unproven) {
      Some(wait) => Some(read_request(&stream, &mut pending, edge, wait).await),
      None => None,
    };
    EdgeSaw {
      proof,
      request,
      ended_ns: now_ns() - accepted_ns,
    }
  }

  /// One test client: connects to `addr`, sends as `sender` says, then reads until the edge ends the connection.
  async fn client_side(addr: SocketAddrV4, sender: Sender, token: &str) -> ClientSaw {
    let stream = TcpStream::connect(addr).await.unwrap();
    let start = format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n", addr.port());
    let dribbled = match sender {
      Sender::Silent => 0,
      Sender::Dribbling { pause_ns } => dribble(&stream, &start, pause_ns).await,
      Sender::Split { pause_ns } => {
        let head = format!("{start}Authorization: Bearer {token}\r\n");
        stream.write_all(head.as_bytes()).await.unwrap();
        slates_rt::futures::sleep(pause_ns).await.unwrap();
        stream
          .write_all(b"Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{}")
          .await
          .unwrap();
        0
      }
    };
    let mut answered = 0;
    let mut buffer = [0u8; MAX_LINE_BYTES];
    while let Ok(read) = stream.read(&mut buffer).await {
      if read == 0 {
        break;
      }
      answered += read;
    }
    ClientSaw { dribbled, answered }
  }

  /// Sends `start` and the start of a field line, then one more byte of it each `pause_ns` until the edge ends the
  /// connection, or twice the old deadline if it never does: the bytes sent after the start.
  async fn dribble(stream: &TcpStream, start: &str, pause_ns: u64) -> usize {
    stream
      .write_all(format!("{start}X-Slow: ").as_bytes())
      .await
      .unwrap();
    let began = now_ns();
    let mut dribbled = 0;
    let mut probe = [0u8; 1];
    while now_ns() - began < 2 * OLD_DEADLINE_NS {
      slates_rt::futures::sleep(pause_ns).await.unwrap();
      // Its end, a reset or an answer: the edge is done with it.
      if !matches!(stream.read_now(&mut probe), Ok(None)) || stream.write_all(b"a").await.is_err() {
        break;
      }
      dribbled += 1;
    }
    dribbled
  }

  /// The token holder's connection, as the edge saw it: its bearer shown inside the bound, its request read whole,
  /// and the connection waited for past the bound while it lived.
  fn assert_the_token_holder_was_waited_for(edge_saw: &EdgeSaw, bound_ns: u64) {
    let Proof::Shown { arrival_ns } = edge_saw.proof else {
      panic!("the token holder showed its bearer: {:?}", edge_saw);
    };
    assert!(
      arrival_ns < bound_ns,
      "its bearer inside the bound: {arrival_ns} ns"
    );
    let Some(Ok(Some((head, body)))) = &edge_saw.request else {
      panic!("its request read whole: {:?}", edge_saw);
    };
    assert_eq!(
      (head.method.as_str(), body.as_slice()),
      ("POST", &b"{}"[..])
    );
    assert!(
      edge_saw.ended_ns > bound_ns,
      "waited for past the bound, while it lived: {:?}",
      edge_saw
    );
  }

  /// A-523 over real sockets. Do: with an edge that has learned nine honest arrivals of 1 ms, accept a connection that
  /// sends nothing; one that sends a head without the bearer a byte at a time, steadily, and never ends it; and one
  /// that sends its head through its bearer line, then the rest of its request twice the bound later. Expect: the first
  /// two cut off unanswered at the bound they were accepted under, no sooner, and long before the second the edge gave
  /// every connection before A-523, however steadily the second kept sending; the third shown the bearer inside the
  /// bound and its request read whole after it, since a token holder's connection is waited on while it lives.
  #[test]
  fn a_sender_that_never_shows_the_bearer_is_cut_off_at_the_learned_bound_and_a_token_holder_is_waited_for()
   {
    let mut learned = Arrivals::default();
    for _ in 0..LEARNED {
      learned.arrived(LEARNED_NS);
    }
    let bound_ns = learned.bound_ns(TICK_NS);
    assert!(
      bound_ns < OLD_DEADLINE_NS / 10,
      "the learned bound: {bound_ns} ns"
    );
    let senders = [
      Sender::Silent,
      Sender::Dribbling {
        pause_ns: bound_ns / DRIBBLES,
      },
      Sender::Split {
        pause_ns: 2 * bound_ns,
      },
    ];
    let count = senders.len();
    let rt = Runtime::start(&config()).unwrap();
    let id = rt.shard_ids()[0];
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), 4).unwrap();
    let addr = listener.local_addr().unwrap();
    let edge = HttpEdge {
      port: addr.port(),
      ..edge()
    };
    let token = edge.bearer_hex();
    let (edge_tx, edge_rx) = channel();
    rt.spawn_on(id, async move {
      let mut saw = Vec::new();
      for _ in 0..count {
        saw.push(edge_side(&listener, &edge, bound_ns).await);
      }
      let _ = edge_tx.send(saw);
    })
    .unwrap();
    let (client_tx, client_rx) = channel();
    rt.spawn_on(id, async move {
      let mut saw = Vec::new();
      for sender in senders {
        saw.push(client_side(addr, sender, &token).await);
      }
      let _ = client_tx.send(saw);
    })
    .unwrap();
    let edges = edge_rx.recv_timeout(SCENARIO_WAIT).unwrap();
    let clients = client_rx.recv_timeout(SCENARIO_WAIT).unwrap();
    rt.shutdown().unwrap();
    eprintln!(
      "the learned bound {bound_ns} ns; the edge saw {edges:?}; the clients saw {clients:?}"
    );
    for (edge_saw, client_saw) in edges.iter().zip(&clients).take(2) {
      assert_eq!(edge_saw.proof, Proof::Gone, "{edge_saw:?}");
      assert!(
        edge_saw.ended_ns >= bound_ns && edge_saw.ended_ns < OLD_DEADLINE_NS,
        "cut off at the {bound_ns} ns bound, not the old second: {edge_saw:?}"
      );
      assert_eq!(client_saw.answered, 0, "unanswered: {client_saw:?}");
    }
    assert!(
      clients[1].dribbled >= 2,
      "the dribbling sender kept sending: {:?}",
      clients[1]
    );
    assert_the_token_holder_was_waited_for(&edges[2], bound_ns);
  }
}

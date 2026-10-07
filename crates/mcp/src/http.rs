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
//! - each request, and each wait for the next on a kept-alive connection, is bounded by the edge's
//!   deadline ([`HttpEdge::deadline_ns`], the client's derived reply deadline: a loopback peer slower than the
//!   daemon's liveness budget is presumed gone);
//! - the connections served at once are bounded by [`HttpEdge::connections`]; one past it is answered `503`
//!   and closed at once.
//!
//! A connection's failure — a reset, a truncated body, a deadline — ends that connection only; the
//! listener keeps serving. Until 2026-10-01 this edge read with unbounded `read_line`s, served one
//! connection at a time for its whole keep-alive life, and ended the server on any connection's I/O error.
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
use slates_rt::futures::{detach, spawn_child, within};
use slates_rt::shard::Kept;
use slates_rt::tcp::{TcpListener, TcpStream};

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

/// Derived: the most bytes a request head can hold — every field line at its bound, plus the request line.
const MAX_HEAD_BYTES: usize = MAX_LINE_BYTES * (MAX_FIELDS + 1);

/// What the edge enforces, fixed when it starts.
#[derive(Clone, Debug)]
pub struct HttpEdge {
  /// The loopback port the listener is bound to — what `Host` and `Origin` must name.
  pub port: u16,
  /// The bearer token every request must carry.
  pub bearer: [u8; BEARER_BYTES],
  /// The bound on one request's arrival and on a kept-alive connection's wait for the next.
  pub deadline_ns: u64,
  /// The connections served at once.
  pub connections: usize,
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
  let token = head
    .authorization
    .as_deref()
    .and_then(|value| value.strip_prefix("Bearer "));
  if !token.is_some_and(|token| bearer_matches(edge, token.trim())) {
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

/// What the edge shares among its tasks on its shard: the MCP server (one daemon client) and the
/// connections being served.
struct Shared {
  server: RefCell<McpServer>,
  serving: Cell<usize>,
  /// The `subscriptions/listen` streams open now, at most [`crate::max_subscriptions`]: one per notification source,
  /// as on stdio, so listens cannot take every connection the edge serves.
  listening: Cell<usize>,
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
    })
  })
  .ok_or(slates_rt::error::RtError::NotOnShardThread)??;
  loop {
    let stream = listener.accept().await?;
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
    let connection_edge = edge.clone();
    let spawned = spawn_child(async move {
      serve_connection(&stream, shared, &connection_edge).await;
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

/// Serves one connection until the peer closes it, a request is refused, or a deadline passes.
async fn serve_connection(stream: &TcpStream, shared: Kept<Shared>, edge: &HttpEdge) {
  let mut pending: Vec<u8> = Vec::new();
  loop {
    let request = within(edge.deadline_ns, read_request(stream, &mut pending, edge)).await;
    let (head, body) = match request {
      Ok(Some(Ok(Some(request)))) => request,
      Ok(Some(Err(refused))) => {
        let _ = write_refusal(stream, refused).await;
        return;
      }
      // The peer closed, the deadline passed, the read failed, or the timer could not be armed.
      _ => return,
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
        let _ = write_refusal(stream, Refused::Unavailable).await;
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

/// Reads one request off `stream` — the head, then its body — keeping any bytes past it in `pending` for
/// the next. `Ok(None)` when the peer closed before a request began; `Err` when it is refused.
async fn read_request(
  stream: &TcpStream,
  pending: &mut Vec<u8>,
  edge: &HttpEdge,
) -> Result<Option<(Head, Vec<u8>)>, Refused> {
  let head_end = loop {
    if let Some(at) = pending
      .windows(HEAD_END.len())
      .position(|window| window == HEAD_END)
    {
      break at;
    }
    if pending.len() > MAX_HEAD_BYTES || line_too_long(pending) {
      return Err(head_overflow(pending));
    }
    if !fill(stream, pending).await {
      return if pending.is_empty() {
        Ok(None)
      } else {
        Err(Refused::BadRequest) // a truncated head
      };
    }
  };
  let head = parse_head(pending.get(..head_end).unwrap_or_default())?;
  authorize(edge, &head)?;
  let body_len =
    usize::try_from(head.content_length.unwrap_or(0)).map_err(|_| Refused::TooLarge)?;
  let body_start = head_end.saturating_add(HEAD_END.len());
  let body_end = body_start.saturating_add(body_len);
  while pending.len() < body_end {
    if !fill(stream, pending).await {
      return Err(Refused::BadRequest); // a truncated body
    }
  }
  let body = pending
    .get(body_start..body_end)
    .unwrap_or_default()
    .to_vec();
  pending.drain(..body_end);
  Ok(Some((head, body)))
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
/// comments, one each half of the edge's idle deadline: an intermediary that closes a connection idle for that
/// deadline, as this edge itself does, sees one first. Bytes from the client end the stream: a listen takes none.
async fn stream_listen(stream: &TcpStream, acknowledgement: &Value, edge: &HttpEdge) {
  let event = format!("data: {acknowledgement}\n\n");
  if stream.write_all(LISTEN_HEAD).await.is_err()
    || stream.write_all(event.as_bytes()).await.is_err()
  {
    return;
  }
  let keep_alive_ns = edge.deadline_ns.checked_div(2).unwrap_or(0).max(1);
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

/// Answers a refusal and closes: the status, and for `401` the challenge naming the scheme.
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

  fn edge() -> HttpEdge {
    HttpEdge {
      port: 7000,
      bearer: [0xab; BEARER_BYTES],
      deadline_ns: 1,
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
}

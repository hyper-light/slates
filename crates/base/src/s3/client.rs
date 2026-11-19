//! The S3 base's network client (A-471; §4.15 "An S3 base"): `HeadObject`, ranged `GetObject` (bound to its object
//! by `If-Match` or a version id) and `ListObjectsV2` pages, each signed by [`super::sigv4`], sent over slates' own
//! runtime (its TCP, no foreign runtime: D-9, R6) with rustls over aws-lc-rs ([`super::tls`]) or, to a store on
//! loopback alone, in the clear, framed by [`super::http`].
//!
//! - **One request at a time on a kept-alive connection**, opened on first use and again after a failure or a
//!   `Connection: close`.
//! - **Every length checked before it is held** ([`super::http`]): a request's head against its cap before a byte is
//!   sent; a response's head against its cap; its body against what the request asked (a range's length, a listing
//!   page's cap) or an error document's cap, whichever its status says the body is.
//! - **Every wait learned** ([`super::wait`]): connecting, sending and each read are bounded by the path's measured
//!   round trips plus the liveness window the caller states, doubled per request that ran out, to the ceiling the
//!   caller states; an answer S3 asks to be retried (`503 SlowDown`, `503 ServiceUnavailable`, `500 InternalError`) is
//!   retried after a jittered, doubling pause, and a lost request after a reconnect, both within the ceiling.
//! - **Every answer typed** ([`S3Error`]): `412 Precondition Failed` on an `If-Match` read is the object changed;
//!   `404`, `403`, `400 ExpiredToken`, `416`, and the retryable ones each their kind (S3's error responses); a range or
//!   a head that is not what was asked is a mismatch, never data.

use rustls::pki_types::CertificateDer;
use slates_rt::futures;
use slates_rt::tcp::{SocketAddrV4, TcpStream};
use slates_transport::pacer::NANOS_PER_SECOND;

use super::http::{self, Advance, BodyCaps, HttpError, Response, ResponseReader};
use super::list::{self, ListError, ListPage};
use super::sigv4::{self, Credentials, SESSION_TOKEN_HEADER, When};
use super::tls::{self, TlsClient, TlsError};
use super::wait::Waits;

/// Derived: the bytes one read takes from the socket: one TLS 1.2 record's ciphertext at its largest (RFC 5246
/// §6.2.3: 2^14 bytes of plaintext and at most 2,048 of expansion), which also bounds TLS 1.3's (RFC 8446 §5.2: 2^14
/// + 256), so every record a read completes yields at most this much plaintext.
pub const READ_BYTES: usize = tls::TLS_RECORD_PLAINTEXT + 2_048;
/// Derived: the most received bytes held before the reader frames them: a head's cap, which a partial head or chunk
/// line never passes, and one read's worth.
const RECEIVED_CAP: usize = http::RESPONSE_HEAD_CAP + READ_BYTES;
/// Format: the TCP port S3 serves HTTPS on, which the `Host` header leaves out.
pub const HTTPS_PORT: u16 = 443;

/// How a request names its bucket (S3's virtual-hosted and path-style requests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
  /// The bucket in the host name (`bucket.s3.region.amazonaws.com`), the path the key alone.
  VirtualHosted,
  /// The bucket the path's first segment (`/bucket/key`), as an S3-compatible store on one host serves it.
  Path,
}

/// How a connection is carried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Carriage {
  /// TLS, the server's certificate verified against these authorities (DER).
  Tls {
    /// The authorities.
    authorities: Vec<CertificateDer<'static>>,
  },
  /// In the clear: refused unless the endpoint's address is on loopback (a test's store).
  PlainLoopback,
}

/// An S3 endpoint and the bucket the base reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
  /// The address to connect to (resolved by the caller).
  pub addr: SocketAddrV4,
  /// The `Host` header's value, a port with it where the port is not the scheme's; its name, without the port, is the
  /// name the server's certificate must carry.
  pub host: String,
  /// The bucket's region, the signature's scope.
  pub region: String,
  /// The bucket.
  pub bucket: String,
  /// How requests name the bucket.
  pub style: Style,
  /// How the connection is carried.
  pub carriage: Carriage,
}

/// The bounds the caller states, which the client's waits are learned within: the lateness a live store may show (the
/// caller's liveness window, A-228), and the most one request's tries may take in all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
  /// The liveness window, added to the path's measured tail.
  pub liveness_ns: u64,
  /// The ceiling of one request's tries and of a backed-off wait.
  pub ceiling_ns: u64,
}

/// An object's head (`HeadObject`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectHead {
  /// Its size in bytes.
  pub size: u64,
  /// Its ETag, quotes and all.
  pub etag: String,
  /// Its version id, on a versioned bucket.
  pub version: Option<String>,
  /// When it was made, as S3 states it (`Last-Modified`).
  pub last_modified: Option<String>,
}

/// A range read (`GetObject` with `Range`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeRead {
  /// The bytes, from the range's first.
  pub bytes: Vec<u8>,
  /// The object's whole size.
  pub total: u64,
  /// The ETag of the object the bytes are from.
  pub etag: String,
  /// Its version id, on a versioned bucket.
  pub version: Option<String>,
}

/// A request refused, by its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum S3Error {
  /// `412 Precondition Failed` on a read bound by `If-Match`: the object is no longer the one the read was bound to.
  Changed,
  /// `404`: the key or the bucket is not there (S3's code, `NoSuchKey` or `NoSuchBucket`, where the answer carried one;
  /// a `HEAD`'s carries none).
  Missing {
    /// S3's error code.
    code: Option<String>,
  },
  /// `403`: refused (`AccessDenied`, `SignatureDoesNotMatch`, `InvalidAccessKeyId`, `RequestTimeTooSkewed`, or none on
  /// a `HEAD`).
  Denied {
    /// S3's error code.
    code: Option<String>,
  },
  /// `400 ExpiredToken`: the credential's session expired; a fresh one is needed.
  CredentialExpired,
  /// `416 InvalidRange`: the range starts past the object's end.
  RangeNotSatisfiable,
  /// `503 SlowDown`, retried until the ceiling.
  SlowDown,
  /// `500` or `503 ServiceUnavailable`, retried until the ceiling.
  Unavailable {
    /// The last status.
    status: u16,
  },
  /// No answer within the learned waits, tried until the ceiling.
  TimedOut,
  /// Another status.
  Unexpected {
    /// The status.
    status: u16,
    /// S3's error code, where the answer carried one.
    code: Option<String>,
  },
  /// The connection failed (refused, reset, or closed mid-answer), tried until the ceiling.
  Connection(String),
  /// TLS refused: an unverified certificate, a broken record.
  Tls(TlsError),
  /// The answer's framing refused.
  Http(HttpError),
  /// A listing page refused.
  Listing(ListError),
  /// An answer that is not what was asked: a range's bytes or `Content-Range` other than asked, a head without its
  /// length or its ETag.
  Mismatch(&'static str),
  /// A request this client does not send: plain HTTP to an address off loopback, an empty range, a request that could
  /// not be signed.
  Refused(&'static str),
}

/// The client's counts, for a test's non-vacuity checks and the daemon's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
  /// Requests sent, each try counted.
  pub sent: u64,
  /// Tries after the first, asked by the server or after a lost request.
  pub retries: u64,
  /// Waits that ran out.
  pub timeouts: u64,
  /// Connections opened.
  pub connections: u64,
  /// The longest continuation token a listing page named, in bytes: what part 3's live run reads against the page and
  /// request caps' assumption that no token is longer than an encoded key (S3 states no length).
  pub longest_token: u64,
}

/// One connection: its stream, its TLS session if carried so, and the bytes received and not yet framed.
struct Connection {
  stream: TcpStream,
  tls: Option<TlsClient>,
  received: Vec<u8>,
}

/// What one try ended in, before the request's retry rule judges it.
enum Tried {
  /// An answer.
  Answered(Response),
  /// The wait ran out.
  RanOut,
  /// The connection failed.
  Lost(String),
  /// A refusal no retry changes.
  Fatal(S3Error),
}

/// One request as the client asks it: its method, its path (unencoded), its query, its fields beyond the signing ones,
/// and its body's caps.
struct Ask {
  method: &'static str,
  path: String,
  query: Vec<(&'static str, String)>,
  fields: Vec<(&'static str, String)>,
  caps: BodyCaps,
}

/// An S3 client for one endpoint and credential.
pub struct Client {
  endpoint: Endpoint,
  credentials: Credentials,
  limits: Limits,
  waits: Waits,
  wall_ns: fn() -> i64,
  config: Option<tls::Config>,
  connection: Option<Connection>,
  counts: Counts,
}

/// The host's wall clock, nanoseconds since the Unix epoch: what a request's `x-amz-date` states.
pub fn host_wall_ns() -> i64 {
  use slates_vfs::clock::Clock as _;
  slates_vfs::clock::HostClock::new().wall_ns()
}

impl Client {
  /// A client of `endpoint` signing with `credentials`, its waits learned within `limits`, its requests dated by
  /// `wall_ns` ([`host_wall_ns`], or a test's fixed time) and its retry pauses jittered from `seed`. Refused when the
  /// endpoint is plain HTTP off loopback, or its authorities make no TLS configuration.
  pub fn new(
    endpoint: Endpoint,
    credentials: Credentials,
    limits: Limits,
    wall_ns: fn() -> i64,
    seed: u64,
  ) -> Result<Client, S3Error> {
    let config = match &endpoint.carriage {
      Carriage::Tls { authorities } => Some(tls::client_config(authorities).map_err(S3Error::Tls)?),
      Carriage::PlainLoopback if endpoint.addr.ip().is_loopback() => None,
      Carriage::PlainLoopback => {
        return Err(S3Error::Refused("plain HTTP to an address off loopback"));
      }
    };
    Ok(Client {
      endpoint,
      credentials,
      limits,
      waits: Waits::new(seed),
      wall_ns,
      config,
      connection: None,
      counts: Counts::default(),
    })
  }

  /// The client's counts so far.
  pub fn counts(&self) -> Counts {
    self.counts
  }

  /// The path of `key` (or of the bucket, for an empty key) as the endpoint's style names it, unencoded.
  fn path_of(&self, key: &str) -> String {
    match self.endpoint.style {
      Style::VirtualHosted => format!("/{key}"),
      Style::Path if key.is_empty() => format!("/{}", self.endpoint.bucket),
      Style::Path => format!("/{}/{key}", self.endpoint.bucket),
    }
  }

  /// `HeadObject` of `key` (at `version` when named): its size, ETag, version and time.
  pub async fn head(&mut self, key: &str, version: Option<&str>) -> Result<ObjectHead, S3Error> {
    let ask = Ask {
      method: "HEAD",
      path: self.path_of(key),
      query: version
        .map(|v| vec![("versionId", v.to_owned())])
        .unwrap_or_default(),
      fields: Vec::new(),
      caps: BodyCaps {
        success: 0,
        error: 0,
      },
    };
    let response = self.exchange(&ask).await?;
    let size = response
      .field("content-length")
      .and_then(|length| length.parse::<u64>().ok())
      .ok_or(S3Error::Mismatch("a head without its length"))?;
    let etag = response
      .field("etag")
      .ok_or(S3Error::Mismatch("a head without its ETag"))?
      .to_owned();
    Ok(ObjectHead {
      size,
      etag,
      version: response.field("x-amz-version-id").map(str::to_owned),
      last_modified: response.field("last-modified").map(str::to_owned),
    })
  }

  /// `GetObject` of `len` bytes of `key` from `first` (at `version` when named), bound by `If-Match` to `etag` when
  /// named: the bytes (fewer at the object's end), the object's size and its ETag. `Changed` when the object is no
  /// longer the one `etag` names.
  pub async fn get_range(
    &mut self,
    key: &str,
    (first, len): (u64, u64),
    etag: Option<&str>,
    version: Option<&str>,
  ) -> Result<RangeRead, S3Error> {
    let last = len
      .checked_sub(1)
      .and_then(|span| first.checked_add(span))
      .ok_or(S3Error::Refused("an empty range"))?;
    let mut fields = vec![("range", format!("bytes={first}-{last}"))];
    if let Some(etag) = etag {
      fields.push(("if-match", etag.to_owned()));
    }
    let ask = Ask {
      method: "GET",
      path: self.path_of(key),
      query: version
        .map(|v| vec![("versionId", v.to_owned())])
        .unwrap_or_default(),
      fields,
      caps: BodyCaps {
        success: len,
        error: u64::try_from(http::error_body_cap()).unwrap_or(u64::MAX),
      },
    };
    let response = self.exchange(&ask).await?;
    range_of(response, first, len)
  }

  /// One `ListObjectsV2` page of the keys under `prefix`, delimited by `/` when `delimited` (one directory's files and
  /// subdirectories), continuing from `token` when named, keys URL-encoded and decoded.
  pub async fn list_page(
    &mut self,
    prefix: &str,
    delimited: bool,
    token: Option<&str>,
  ) -> Result<ListPage, S3Error> {
    let mut query = vec![
      ("list-type", "2".to_owned()),
      ("encoding-type", "url".to_owned()),
      ("max-keys", list::MAX_KEYS.to_string()),
      ("prefix", prefix.to_owned()),
    ];
    if delimited {
      query.push(("delimiter", "/".to_owned()));
    }
    if let Some(token) = token {
      query.push(("continuation-token", token.to_owned()));
    }
    let ask = Ask {
      method: "GET",
      path: self.path_of(""),
      query,
      fields: Vec::new(),
      caps: BodyCaps {
        success: list::page_cap(list::MAX_KEYS),
        error: u64::try_from(http::error_body_cap()).unwrap_or(u64::MAX),
      },
    };
    let response = self.exchange(&ask).await?;
    let page = list::parse_page(
      &response.body,
      list::page_cap(list::MAX_KEYS),
      list::MAX_KEYS,
    )
    .map_err(S3Error::Listing)?;
    if let Some(token) = &page.next_token {
      let len = u64::try_from(token.len()).unwrap_or(u64::MAX);
      self.counts.longest_token = self.counts.longest_token.max(len);
    }
    Ok(page)
  }

  /// Asks `ask` until it is answered with something other than a retry, or its ceiling passes: a lost request is asked
  /// again on a new connection, and an answer S3 asks to be retried after a jittered pause, both within the ceiling.
  async fn exchange(&mut self, ask: &Ask) -> Result<Response, S3Error> {
    let deadline = futures::now_ns().saturating_add(self.limits.ceiling_ns);
    let mut first = true;
    loop {
      if !first {
        self.counts.retries = self.counts.retries.saturating_add(1);
      }
      first = false;
      let refusal = match self.try_once(ask).await {
        Tried::Answered(response) => match judge(response) {
          Ok(response) => {
            self.waits.settled();
            return Ok(response);
          }
          Err(Judged::Final(refusal)) => {
            self.waits.settled();
            return Err(refusal);
          }
          Err(Judged::Retry(refusal)) => refusal,
        },
        Tried::RanOut => {
          self.counts.timeouts = self.counts.timeouts.saturating_add(1);
          self.waits.ran_out();
          self.connection = None;
          S3Error::TimedOut
        }
        Tried::Lost(why) => {
          self.connection = None;
          S3Error::Connection(why)
        }
        Tried::Fatal(refusal) => {
          self.connection = None;
          return Err(refusal);
        }
      };
      let left = deadline.saturating_sub(futures::now_ns());
      let pause = self.waits.retry_pause_ns(left);
      if left == 0 || pause >= left {
        self.waits.settled();
        return Err(refusal);
      }
      if futures::sleep(pause).await.is_err() {
        return Err(refusal);
      }
    }
  }

  /// One try of `ask`: connected (again, if need be), signed, sent, and its answer read, each step within the learned
  /// bound.
  async fn try_once(&mut self, ask: &Ask) -> Tried {
    let bound = self
      .waits
      .bound_ns(self.limits.liveness_ns, self.limits.ceiling_ns);
    if let Err(tried) = self.connect(bound).await {
      return tried;
    }
    let head = match self.request_head(ask) {
      Ok(head) => head,
      Err(refusal) => return Tried::Fatal(refusal),
    };
    if let Err(tried) = self.send(&head, bound).await {
      return tried;
    }
    self.counts.sent = self.counts.sent.saturating_add(1);
    let sent_at = futures::now_ns();
    self.receive(ask, bound, sent_at).await
  }

  /// Opens a connection when none is open: TCP within `bound_ns`, then the TLS handshake, each flight within it.
  async fn connect(&mut self, bound_ns: u64) -> Result<(), Tried> {
    if self.connection.is_some() {
      return Ok(());
    }
    let stream = match futures::within(bound_ns, TcpStream::connect(self.endpoint.addr)).await {
      Ok(Some(Ok(stream))) => stream,
      Ok(Some(Err(error))) => return Err(Tried::Lost(format!("connecting: {error:?}"))),
      Ok(None) => return Err(Tried::RanOut),
      Err(error) => return Err(Tried::Lost(format!("the connect's wait: {error:?}"))),
    };
    self.counts.connections = self.counts.connections.saturating_add(1);
    let tls = match &self.config {
      Some(config) => {
        let name = self.endpoint.host.split(':').next().unwrap_or_default();
        Some(
          TlsClient::new(config.clone(), name)
            .map_err(|error| Tried::Fatal(S3Error::Tls(error)))?,
        )
      }
      None => None,
    };
    let mut connection = Connection {
      stream,
      tls,
      received: Vec::new(),
    };
    if connection.tls.is_some() {
      handshake(&mut connection, bound_ns).await?;
    }
    self.connection = Some(connection);
    Ok(())
  }

  /// `ask`'s request head, signed: the fields S3 needs (`host`, `x-amz-content-sha256`, `x-amz-date`, a temporary
  /// credential's session token) and the ask's own, all signed, and the `Authorization` header that signs them.
  fn request_head(&self, ask: &Ask) -> Result<Vec<u8>, S3Error> {
    let seconds = u64::try_from((self.wall_ns)().max(0))
      .unwrap_or(0)
      .checked_div(NANOS_PER_SECOND)
      .unwrap_or(0);
    let timestamp = sigv4::amz_timestamp(seconds);
    let mut fields: Vec<(&str, String)> = vec![
      ("host", self.endpoint.host.clone()),
      (
        "x-amz-content-sha256",
        sigv4::EMPTY_PAYLOAD_SHA256.to_owned(),
      ),
      ("x-amz-date", timestamp.clone()),
    ];
    if let Some(token) = self.credentials.session_token() {
      fields.push((SESSION_TOKEN_HEADER, token.to_owned()));
    }
    for (name, value) in &ask.fields {
      fields.push((*name, value.clone()));
    }
    let query: Vec<(&str, &str)> = ask
      .query
      .iter()
      .map(|(name, value)| (*name, value.as_str()))
      .collect();
    let signed_fields: Vec<(&str, &str)> = fields
      .iter()
      .map(|(name, value)| (*name, value.as_str()))
      .collect();
    let signed = sigv4::sign(
      &self.credentials,
      &When {
        timestamp: &timestamp,
        region: &self.endpoint.region,
      },
      &sigv4::Request {
        method: ask.method,
        path: &ask.path,
        query: &query,
        headers: &signed_fields,
        payload_hash: sigv4::EMPTY_PAYLOAD_SHA256,
      },
    )
    .map_err(|_| S3Error::Refused("a request that could not be signed"))?;
    let mut target = sigv4::uri_encode(&ask.path, false);
    let canonical = sigv4::canonical_query(&query);
    if !canonical.is_empty() {
      target.push('?');
      target.push_str(&canonical);
    }
    let mut sent = signed_fields;
    sent.push(("authorization", &signed.authorization));
    http::request_head(ask.method, &target, &sent).map_err(S3Error::Http)
  }

  /// Sends `head` on the connection, sealed when carried by TLS, within `bound_ns`.
  async fn send(&mut self, head: &[u8], bound_ns: u64) -> Result<(), Tried> {
    let Some(connection) = self.connection.as_mut() else {
      return Err(Tried::Lost("no connection".to_owned()));
    };
    let mut sealed = Vec::new();
    let bytes = match connection.tls.as_mut() {
      Some(tls) => {
        tls
          .send(head, &mut sealed)
          .map_err(|error| Tried::Fatal(S3Error::Tls(error)))?;
        sealed.as_slice()
      }
      None => head,
    };
    write_within(&connection.stream, bytes, bound_ns).await
  }

  /// Reads `ask`'s answer whole, each read within `bound_ns`, the round trip to its head measured from `sent_at`.
  async fn receive(&mut self, ask: &Ask, bound_ns: u64, sent_at: u64) -> Tried {
    let Some(connection) = self.connection.as_mut() else {
      return Tried::Lost("no connection".to_owned());
    };
    let mut reader = ResponseReader::new(ask.method == "HEAD", ask.caps);
    let mut scratch = vec![0u8; READ_BYTES];
    let mut measured = false;
    loop {
      match reader.advance(&mut connection.received) {
        Ok(Advance::Done(response)) => {
          if !measured {
            self
              .waits
              .answered(futures::now_ns().saturating_sub(sent_at));
          }
          if response.closes {
            self.connection = None;
          }
          return Tried::Answered(response);
        }
        Ok(Advance::More) => {}
        Err(error) => return Tried::Fatal(S3Error::Http(error)),
      }
      if !measured && reader.has_head() {
        measured = true;
        self
          .waits
          .answered(futures::now_ns().saturating_sub(sent_at));
      }
      let count = match futures::within(bound_ns, connection.stream.read(&mut scratch)).await {
        Ok(Some(Ok(0))) => return Tried::Lost("closed before the answer was whole".to_owned()),
        Ok(Some(Ok(count))) => count,
        Ok(Some(Err(error))) => return Tried::Lost(format!("reading: {error:?}")),
        Ok(None) => return Tried::RanOut,
        Err(error) => return Tried::Lost(format!("the read's wait: {error:?}")),
      };
      let fresh = scratch.get(..count).unwrap_or_default();
      match connection.tls.as_mut() {
        Some(tls) => {
          if let Err(error) = tls.receive(fresh, &mut connection.received, RECEIVED_CAP) {
            return Tried::Fatal(S3Error::Tls(error));
          }
          let mut out = Vec::new();
          if let Err(error) = tls.transmit(&mut out) {
            return Tried::Fatal(S3Error::Tls(error));
          }
          if !out.is_empty()
            && let Err(tried) = write_within(&connection.stream, &out, bound_ns).await
          {
            return tried;
          }
        }
        None => {
          if connection.received.len().saturating_add(count) > RECEIVED_CAP {
            return Tried::Fatal(S3Error::Http(HttpError::HeadTooLong));
          }
          connection.received.extend_from_slice(fresh);
        }
      }
    }
  }
}

/// Writes `bytes` to `stream` within `bound_ns`.
async fn write_within(stream: &TcpStream, bytes: &[u8], bound_ns: u64) -> Result<(), Tried> {
  match futures::within(bound_ns, stream.write_all(bytes)).await {
    Ok(Some(Ok(()))) => Ok(()),
    Ok(Some(Err(error))) => Err(Tried::Lost(format!("writing: {error:?}"))),
    Ok(None) => Err(Tried::RanOut),
    Err(error) => Err(Tried::Lost(format!("the write's wait: {error:?}"))),
  }
}

/// Runs a new connection's TLS handshake, each flight sent and each answer read within `bound_ns`.
async fn handshake(connection: &mut Connection, bound_ns: u64) -> Result<(), Tried> {
  let mut scratch = vec![0u8; READ_BYTES];
  loop {
    let Some(tls) = connection.tls.as_mut() else {
      return Ok(());
    };
    let mut out = Vec::new();
    tls
      .transmit(&mut out)
      .map_err(|error| Tried::Fatal(S3Error::Tls(error)))?;
    if !out.is_empty() {
      write_within(&connection.stream, &out, bound_ns).await?;
    }
    if !tls.is_handshaking() {
      return Ok(());
    }
    let count = match futures::within(bound_ns, connection.stream.read(&mut scratch)).await {
      Ok(Some(Ok(0))) => return Err(Tried::Lost("closed during the handshake".to_owned())),
      Ok(Some(Ok(count))) => count,
      Ok(Some(Err(error))) => return Err(Tried::Lost(format!("handshake read: {error:?}"))),
      Ok(None) => return Err(Tried::RanOut),
      Err(error) => return Err(Tried::Lost(format!("the handshake's wait: {error:?}"))),
    };
    tls
      .receive(
        scratch.get(..count).unwrap_or_default(),
        &mut connection.received,
        RECEIVED_CAP,
      )
      .map_err(|error| Tried::Fatal(S3Error::Tls(error)))?;
  }
}

/// How an answer is judged: final, or one S3 asks to be retried.
enum Judged {
  /// A refusal no retry changes.
  Final(S3Error),
  /// A refusal S3 asks to be retried.
  Retry(S3Error),
}

/// An answer judged by its status: a `2xx` passes; every other status is its kind (S3's error responses), the
/// retryable ones (`500`, `503`) marked so.
fn judge(response: Response) -> Result<Response, Judged> {
  if http::is_success(response.status) {
    return Ok(response);
  }
  let code = error_code(&response.body);
  match response.status {
    http::PRECONDITION_FAILED => Err(Judged::Final(S3Error::Changed)),
    http::NOT_FOUND => Err(Judged::Final(S3Error::Missing { code })),
    http::FORBIDDEN => Err(Judged::Final(S3Error::Denied { code })),
    http::RANGE_NOT_SATISFIABLE => Err(Judged::Final(S3Error::RangeNotSatisfiable)),
    http::BAD_REQUEST if code.as_deref() == Some("ExpiredToken") => {
      Err(Judged::Final(S3Error::CredentialExpired))
    }
    http::SERVICE_UNAVAILABLE if code.as_deref() == Some("SlowDown") => {
      Err(Judged::Retry(S3Error::SlowDown))
    }
    status @ (http::INTERNAL_SERVER_ERROR | http::SERVICE_UNAVAILABLE) => {
      Err(Judged::Retry(S3Error::Unavailable { status }))
    }
    status => Err(Judged::Final(S3Error::Unexpected { status, code })),
  }
}

/// The error code of an S3 error document (`<Error><Code>…</Code>…`), when it carries one of letters and digits.
fn error_code(body: &[u8]) -> Option<String> {
  let text = std::str::from_utf8(body).ok()?;
  let start = text.find("<Code>")?.saturating_add("<Code>".len());
  let rest = text.get(start..)?;
  let code = rest.get(..rest.find("</Code>")?)?;
  (!code.is_empty() && code.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| code.to_owned())
}

/// A ranged answer read as the range asked: `206` whose `Content-Range` names exactly the bytes asked (to the object's
/// end at most) and whose body is them; or a `200` with the whole object, when the range asked from its start covers
/// it. Anything else is a mismatch, never data.
fn range_of(response: Response, first: u64, len: u64) -> Result<RangeRead, S3Error> {
  let etag = response
    .field("etag")
    .ok_or(S3Error::Mismatch("a range without its ETag"))?
    .to_owned();
  let version = response.field("x-amz-version-id").map(str::to_owned);
  let body_len = u64::try_from(response.body.len()).unwrap_or(u64::MAX);
  let total = match response.status {
    http::PARTIAL_CONTENT => {
      let (from, to, total) = response
        .field("content-range")
        .and_then(content_range)
        .ok_or(S3Error::Mismatch("a range without its Content-Range"))?;
      let expected_last = first
        .saturating_add(len.saturating_sub(1))
        .min(total.saturating_sub(1));
      let span = to.checked_sub(from).and_then(|span| span.checked_add(1));
      if from != first || to != expected_last || span != Some(body_len) {
        return Err(S3Error::Mismatch("a range other than the one asked"));
      }
      total
    }
    http::OK if first == 0 && body_len <= len => body_len,
    _ => return Err(S3Error::Mismatch("a whole object where a range was asked")),
  };
  Ok(RangeRead {
    bytes: response.body,
    total,
    etag,
    version,
  })
}

/// A `Content-Range` of `bytes first-last/total`.
fn content_range(value: &str) -> Option<(u64, u64, u64)> {
  let range = value.strip_prefix("bytes ")?;
  let (span, total) = range.split_once('/')?;
  let (from, to) = span.split_once('-')?;
  let number = |text: &str| {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
      .then(|| text.parse::<u64>().ok())
      .flatten()
  };
  Some((number(from)?, number(to)?, number(total)?))
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
  use std::io::{Read as _, Write as _};
  use std::sync::mpsc::{Receiver, Sender, channel};
  use std::time::Duration;

  use rustls::ServerConnection;
  use rustls::pki_types::PrivateKeyDer;
  use slates_rt::runtime::{Runtime, RuntimeConfig};
  use slates_rt::tcp::{Ipv4Addr, TcpListener};

  use super::*;

  /// Format: the S3 signing page's example credential, so a request's signature can be checked against the page's
  /// rules by recomputing it.
  const ACCESS_KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
  /// Format: see [`ACCESS_KEY_ID`].
  const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
  /// Format: the page's example time, 2013-05-24T00:00:00Z, which every test request is dated.
  const EXAMPLE_NS: i64 = 1_369_353_600_000_000_000;
  /// Format: the name the test store's certificate carries.
  const STORE_NAME: &str = "store.test";
  /// Shape: the liveness window a test states: 30 ms, so a wait that runs out does so quickly.
  const LIVENESS_NS: u64 = 30_000_000;
  /// Shape: the ceiling a test states: 3 s, past two first waits (RFC 9002's initial probe timeout, 666 ms, and the
  /// window) and their pauses, so a lost request is tried again before it.
  const CEILING_NS: u64 = 3_000_000_000;
  /// Shape: how long a test waits for its scenario, well past the ceiling.
  const SCENARIO_WAIT: Duration = Duration::from_secs(20);

  /// The example time.
  fn example_ns() -> i64 {
    EXAMPLE_NS
  }

  /// A runtime of one shard for a test.
  fn config() -> RuntimeConfig {
    RuntimeConfig {
      shards: 1,
      tasks_per_shard: 64,
      timers_per_shard: 64,
      ring_entries: 64,
      step_budget_ns: 1_000_000_000,
      timer_tick_ns: 100_000,
      batch: 64,
      pin: false,
      cores: Vec::new(),
      page_bytes: 4096,
      spin_ns: 0,
      wake_tracking: None,
    }
  }

  /// What the test store answers one request with.
  enum Reply {
    /// These bytes.
    Bytes(Vec<u8>),
    /// Nothing: the request is read and left unanswered, the connection held.
    Silence,
  }

  /// A test store's TLS configuration as rustls takes it.
  // structural: allow — D-8 exception 3: a test harness's store; rustls takes its configuration as `Arc`.
  type StoreConfig = std::sync::Arc<rustls::ServerConfig>;

  /// A test's scenario: a future of the client's outcome, sent to the shard.
  type Scenario<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

  /// A store's certificate for [`STORE_NAME`] and its server configuration over aws-lc-rs.
  fn store_tls() -> (CertificateDer<'static>, StoreConfig) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec![STORE_NAME.to_owned()])
      .unwrap()
      .self_signed(&key)
      .unwrap();
    let der = cert.der().clone();
    // structural: allow — D-8 exception 3: a test harness's store; rustls takes its provider as `Arc`.
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
      .with_safe_default_protocol_versions()
      .unwrap()
      .with_no_client_auth()
      .with_single_cert(
        vec![der.clone()],
        PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
      )
      .unwrap();
    // structural: allow — D-8 exception 3: a test harness's store; rustls takes its configuration as `Arc`.
    let shared: StoreConfig = std::sync::Arc::new(config);
    (der, shared)
  }

  /// One connection of the test store: its stream, its TLS session when carried so, and the plaintext read and not yet
  /// answered.
  struct StoreConnection {
    stream: TcpStream,
    session: Option<ServerConnection>,
    plaintext: Vec<u8>,
  }

  impl StoreConnection {
    /// Reads until a whole request head is held and takes it out: `None` when the connection ends first.
    async fn next_head(&mut self) -> Option<String> {
      let mut scratch = vec![0u8; READ_BYTES];
      loop {
        if let Some(at) = self.plaintext.windows(4).position(|w| w == b"\r\n\r\n") {
          let head: Vec<u8> = self.plaintext.drain(..at + 4).collect();
          return Some(String::from_utf8(head).unwrap());
        }
        let count = match self.stream.read(&mut scratch).await {
          Ok(0) | Err(_) => return None,
          Ok(count) => count,
        };
        self.take_in(&scratch[..count]).await?;
      }
    }

    /// Takes `bytes` from the socket into the plaintext, through the TLS session when there is one, and sends what the
    /// session has to send (its handshake flights): `None` when the session or the socket fails.
    async fn take_in(&mut self, bytes: &[u8]) -> Option<()> {
      let Some(session) = self.session.as_mut() else {
        self.plaintext.extend_from_slice(bytes);
        return Some(());
      };
      session.read_tls(&mut &bytes[..]).unwrap();
      session.process_new_packets().ok()?;
      let mut piece = [0u8; 4096];
      while let Ok(read) = session.reader().read(&mut piece) {
        if read == 0 {
          break;
        }
        self.plaintext.extend_from_slice(&piece[..read]);
      }
      self.answer(&[]).await
    }

    /// Sends `bytes`, sealed by the TLS session when there is one, with whatever else the session has to send: `None`
    /// when the socket fails.
    async fn answer(&mut self, bytes: &[u8]) -> Option<()> {
      let out = match self.session.as_mut() {
        Some(session) => {
          session.writer().write_all(bytes).unwrap();
          let mut out = Vec::new();
          while session.wants_write() {
            session.write_tls(&mut out).unwrap();
          }
          out
        }
        None => bytes.to_vec(),
      };
      if out.is_empty() {
        return Some(());
      }
      self.stream.write_all(&out).await.ok()
    }
  }

  /// The test store: answers each request read on `listener` with the next of `replies`, TLS when `tls` names a
  /// configuration, recording each request's head on `requests`. A connection the client closes is replaced by the
  /// next it opens; one with no reply left is closed.
  async fn store(
    listener: TcpListener,
    replies: Vec<Reply>,
    requests: Sender<String>,
    tls: Option<StoreConfig>,
  ) {
    let mut replies = replies.into_iter();
    while let Ok(stream) = listener.accept().await {
      let mut connection = StoreConnection {
        stream,
        session: tls
          .as_ref()
          .map(|config| ServerConnection::new(config.clone()).unwrap()),
        plaintext: Vec::new(),
      };
      while let Some(head) = connection.next_head().await {
        let _ = requests.send(head);
        let answered = match replies.next() {
          Some(Reply::Bytes(bytes)) => connection.answer(&bytes).await,
          Some(Reply::Silence) => Some(()),
          None => None,
        };
        if answered.is_none() {
          break;
        }
      }
    }
  }

  /// Runs `scenario` against a test store answering `replies` (over TLS when `tls`), on a runtime of one shard: the
  /// scenario's outcome, and the request heads the store read.
  fn against<T, S>(replies: Vec<Reply>, tls: bool, scenario: S) -> (T, Vec<String>)
  where
    T: Send + 'static,
    S: FnOnce(Client) -> Scenario<T> + Send + 'static,
  {
    let rt = Runtime::start(&config()).unwrap();
    let id = rt.shard_ids()[0];
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), 4).unwrap();
    let addr = listener.local_addr().unwrap();
    let (store_tls, carriage) = if tls {
      let (der, config) = store_tls();
      (
        Some(config),
        Carriage::Tls {
          authorities: vec![der],
        },
      )
    } else {
      (None, Carriage::PlainLoopback)
    };
    let (requests_tx, requests_rx): (Sender<String>, Receiver<String>) = channel();
    rt.spawn_on(id, store(listener, replies, requests_tx, store_tls))
      .unwrap();
    let endpoint = Endpoint {
      addr,
      host: format!("{STORE_NAME}:{}", addr.port()),
      region: "us-east-1".to_owned(),
      bucket: "examplebucket".to_owned(),
      style: Style::Path,
      carriage,
    };
    let client = Client::new(
      endpoint,
      Credentials::new(ACCESS_KEY_ID, SECRET, None),
      Limits {
        liveness_ns: LIVENESS_NS,
        ceiling_ns: CEILING_NS,
      },
      example_ns,
      7,
    )
    .unwrap();
    let (tx, rx) = channel();
    rt.spawn_on(id, async move {
      let _ = tx.send(scenario(client).await);
    })
    .unwrap();
    let outcome = rx.recv_timeout(SCENARIO_WAIT).unwrap();
    rt.shutdown().unwrap();
    (outcome, requests_rx.try_iter().collect())
  }

  /// A request head's value of `name` (lower case).
  fn field<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n").find_map(|line| {
      let (field, value) = line.split_once(':')?;
      field.eq_ignore_ascii_case(name).then(|| value.trim())
    })
  }

  /// A-471: do read ten bytes of an object by range bound to its ETag, over TLS to a store whose certificate the
  /// endpoint names, the store answering `206` with the range. Expect the bytes, the object's size and its ETag; and a
  /// request that names the range and the ETag, dated, and signed as the S3 signing page's rules sign it (the
  /// signature recomputed from the request's own fields matches the one it carries).
  #[test]
  fn a_ranged_read_bound_to_its_etag_is_signed_sent_and_read_over_tls() {
    let answer =
      b"HTTP/1.1 206 Partial Content\r\nETag: \"e1\"\r\nContent-Range: bytes 0-9/4096\r\n\
                   Content-Length: 10\r\n\r\n0123456789"
        .to_vec();
    let (read, requests) = against(vec![Reply::Bytes(answer)], true, |mut client| {
      Box::pin(async move {
        client
          .get_range("dir/test.txt", (0, 10), Some("\"e1\""), None)
          .await
      })
    });
    let read = read.unwrap();
    assert_eq!(read.bytes, b"0123456789");
    assert_eq!((read.total, read.etag.as_str()), (4096, "\"e1\""));
    let head = &requests[0];
    assert!(
      head.starts_with("GET /examplebucket/dir/test.txt HTTP/1.1\r\n"),
      "{head}"
    );
    assert_eq!(field(head, "range"), Some("bytes=0-9"));
    assert_eq!(field(head, "if-match"), Some("\"e1\""));
    assert_eq!(field(head, "x-amz-date"), Some("20130524T000000Z"));
    let signed: Vec<(&str, &str)> = [
      "host",
      "x-amz-content-sha256",
      "x-amz-date",
      "range",
      "if-match",
    ]
    .iter()
    .map(|name| (*name, field(head, name).unwrap()))
    .collect();
    let expected = sigv4::sign(
      &Credentials::new(ACCESS_KEY_ID, SECRET, None),
      &When {
        timestamp: "20130524T000000Z",
        region: "us-east-1",
      },
      &sigv4::Request {
        method: "GET",
        path: "/examplebucket/dir/test.txt",
        query: &[],
        headers: &signed,
        payload_hash: sigv4::EMPTY_PAYLOAD_SHA256,
      },
    )
    .unwrap();
    assert_eq!(
      field(head, "authorization"),
      Some(expected.authorization.as_str())
    );
  }

  /// The kept-alive test's four answers: a delimited page with a continuation token, the listing's last page, an
  /// object's head at a version, and a chunked range of it.
  fn kept_alive_replies() -> Vec<Reply> {
    let page = "<ListBucketResult><EncodingType>url</EncodingType><IsTruncated>true</IsTruncated>\
                <Contents><Key>base/a+b.txt</Key><Size>3</Size><ETag>&quot;x&quot;</ETag>\
                <LastModified>t</LastModified></Contents><CommonPrefixes><Prefix>base/dir/</Prefix></CommonPrefixes>\
                <NextContinuationToken>tok/1=</NextContinuationToken></ListBucketResult>";
    let last = "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>";
    let framed = |body: &str| {
      Reply::Bytes(
        format!(
          "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
          body.len()
        )
        .into_bytes(),
      )
    };
    vec![
      framed(page),
      framed(last),
      Reply::Bytes(
        b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nETag: \"e2\"\r\nx-amz-version-id: v7\r\n\r\n"
          .to_vec(),
      ),
      Reply::Bytes(
        b"HTTP/1.1 206 Partial Content\r\nETag: \"e2\"\r\nContent-Range: bytes 4-9/4096\r\n\
          Transfer-Encoding: chunked\r\n\r\n2\r\nab\r\n4\r\ncdef\r\n0\r\n\r\n"
          .to_vec(),
      ),
    ]
  }

  /// What the kept-alive test's client was answered: the two pages, the head and the range.
  type KeptAliveAnswers = (
    (Result<ListPage, S3Error>, Result<ListPage, S3Error>),
    Result<ObjectHead, S3Error>,
    Result<RangeRead, S3Error>,
  );

  /// Judges the kept-alive test's answers: the first page's object (its key decoded), prefix and token, the last page
  /// not truncated, the head's size, ETag and version, and the chunked range's bytes.
  fn judge_kept_alive_answers((pages, head, read): KeptAliveAnswers) {
    let first = pages.0.unwrap();
    assert_eq!(first.objects[0].key, "base/a b.txt");
    assert_eq!(first.prefixes, vec!["base/dir/".to_owned()]);
    assert_eq!(first.next_token.as_deref(), Some("tok/1="));
    assert!(!pages.1.unwrap().truncated);
    let head = head.unwrap();
    assert_eq!(
      (head.size, head.etag.as_str(), head.version.as_deref()),
      (4096, "\"e2\"", Some("v7"))
    );
    assert_eq!(read.unwrap().bytes, b"abcdef");
  }

  /// Judges the kept-alive test's requests: four sent on one connection, the listing's queries canonical and
  /// URL-encoded as signed, the continuation token's length counted for the live run, and the head's key encoded.
  fn judge_kept_alive_requests(counts: Counts, requests: &[String]) {
    assert_eq!(counts.connections, 1, "one kept-alive connection");
    assert_eq!(counts.sent, 4);
    assert_eq!(
      counts.longest_token, 6,
      "the token's length, kept for the live run"
    );
    assert!(requests[0].starts_with(
      "GET /examplebucket?delimiter=%2F&encoding-type=url&list-type=2&max-keys=1000&prefix=base%2F HTTP/1.1"
    ));
    assert!(
      requests[1].contains("continuation-token=tok%2F1%3D&"),
      "{}",
      requests[1]
    );
    assert!(requests[2].starts_with("HEAD /examplebucket/base/a%20b.txt?versionId=v7 HTTP/1.1"));
  }

  /// A-471: do list a delimited page and a continued one, head an object at a version, and read a range whose answer is
  /// chunked, all on one kept-alive connection in the clear to a store on loopback. Expect the page's objects, prefixes
  /// and token, the head's size, ETag and version, the chunked bytes, and the requests' queries signed as sent: one
  /// connection for the four.
  #[test]
  fn listing_head_and_chunked_reads_share_one_kept_alive_connection() {
    let ((answers, counts), requests) = against(kept_alive_replies(), false, |mut client| {
      Box::pin(async move {
        let first = client.list_page("base/", true, None).await;
        let second = client.list_page("base/", true, Some("tok/1=")).await;
        let head = client.head("base/a b.txt", Some("v7")).await;
        let read = client
          .get_range("base/a b.txt", (4, 6), None, Some("v7"))
          .await;
        (((first, second), head, read), client.counts())
      })
    });
    judge_kept_alive_answers(answers);
    judge_kept_alive_requests(counts, &requests);
  }

  /// A-471 (S3's error responses): do read once against each of S3's refusals: `412` on an `If-Match` read, `404
  /// NoSuchKey`, a `HEAD`'s bodiless `404`, `403 AccessDenied`, `400 ExpiredToken`, `416`, and an answer of another
  /// range than asked. Expect each typed by its kind, none retried.
  #[test]
  fn each_refusal_is_typed_by_its_kind_and_never_retried() {
    let error = |status: &str, code: &str| {
      let body = format!("<Error><Code>{code}</Code><Message>m</Message></Error>");
      Reply::Bytes(
        format!(
          "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{body}",
          body.len()
        )
        .into_bytes(),
      )
    };
    let replies = vec![
      error("412 Precondition Failed", "PreconditionFailed"),
      error("404 Not Found", "NoSuchKey"),
      Reply::Bytes(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()),
      error("403 Forbidden", "AccessDenied"),
      error("400 Bad Request", "ExpiredToken"),
      error("416 Requested Range Not Satisfiable", "InvalidRange"),
      Reply::Bytes(
        b"HTTP/1.1 206 Partial Content\r\nETag: \"e\"\r\nContent-Range: bytes 1-2/10\r\nContent-Length: 2\r\n\r\nab"
          .to_vec(),
      ),
    ];
    let (outcomes, _) = against(replies, false, |mut client| {
      Box::pin(async move {
        let mut outcomes = Vec::new();
        outcomes.push(
          client
            .get_range("k", (0, 4), Some("\"old\""), None)
            .await
            .err(),
        );
        outcomes.push(client.get_range("k", (0, 4), None, None).await.err());
        outcomes.push(client.head("k", None).await.err());
        outcomes.push(client.get_range("k", (0, 4), None, None).await.err());
        outcomes.push(client.get_range("k", (0, 4), None, None).await.err());
        outcomes.push(client.get_range("k", (99, 4), None, None).await.err());
        outcomes.push(client.get_range("k", (0, 2), None, None).await.err());
        (outcomes, client.counts())
      })
    });
    let (outcomes, counts) = outcomes;
    assert_eq!(
      outcomes,
      vec![
        Some(S3Error::Changed),
        Some(S3Error::Missing {
          code: Some("NoSuchKey".to_owned())
        }),
        Some(S3Error::Missing { code: None }),
        Some(S3Error::Denied {
          code: Some("AccessDenied".to_owned())
        }),
        Some(S3Error::CredentialExpired),
        Some(S3Error::RangeNotSatisfiable),
        Some(S3Error::Mismatch("a range other than the one asked")),
      ]
    );
    assert_eq!(counts.retries, 0, "no refusal is retried");
  }

  /// A-471 (`SlowDown`, AWS's advice: back off and retry): do read against a store that answers `503 SlowDown` twice
  /// and then the range; then against one that answers `503 SlowDown` past the ceiling. Expect the first read to
  /// succeed after two retries, and the second refused `SlowDown` once the ceiling passed, retried at least once.
  #[test]
  fn slow_down_is_retried_with_backoff_and_typed_past_the_ceiling() {
    let slow = || {
      let body = "<Error><Code>SlowDown</Code></Error>";
      Reply::Bytes(
        format!(
          "HTTP/1.1 503 Slow Down\r\nContent-Length: {}\r\n\r\n{body}",
          body.len()
        )
        .into_bytes(),
      )
    };
    let ok = Reply::Bytes(
      b"HTTP/1.1 206 Partial Content\r\nETag: \"e\"\r\nContent-Range: bytes 0-1/2\r\nContent-Length: 2\r\n\r\nok"
        .to_vec(),
    );
    let ((read, counts), _) = against(vec![slow(), slow(), ok], false, |mut client| {
      Box::pin(async move {
        let read = client.get_range("k", (0, 2), None, None).await;
        (read, client.counts())
      })
    });
    assert_eq!(read.unwrap().bytes, b"ok");
    assert_eq!(counts.retries, 2);
    let ((refused, counts), _) = against((0..64).map(|_| slow()).collect(), false, |mut client| {
      Box::pin(async move {
        let refused = client.get_range("k", (0, 2), None, None).await;
        (refused, client.counts())
      })
    });
    assert_eq!(refused, Err(S3Error::SlowDown));
    assert!(counts.retries >= 1, "{counts:?}");
  }

  /// A-471 (Karn's rule, RFC 6298 §5.5): do read against a store that never answers the first request and answers the
  /// second on a new connection; then against one that never answers at all. Expect the first read to succeed after
  /// its wait ran out once and the connection was opened again, and the second refused `TimedOut` once the ceiling
  /// passed.
  #[test]
  fn an_unanswered_request_times_out_reconnects_and_is_typed_past_the_ceiling() {
    let ok = Reply::Bytes(
      b"HTTP/1.1 206 Partial Content\r\nETag: \"e\"\r\nContent-Range: bytes 0-1/2\r\nContent-Length: 2\r\n\r\nok"
        .to_vec(),
    );
    let ((read, counts), requests) = against(vec![Reply::Silence, ok], false, |mut client| {
      Box::pin(async move {
        let read = client.get_range("k", (0, 2), None, None).await;
        (read, client.counts())
      })
    });
    assert_eq!(read.unwrap().bytes, b"ok");
    assert_eq!((counts.timeouts, counts.connections), (1, 2), "{counts:?}");
    assert_eq!(requests.len(), 2);
    let ((refused, counts), _) = against(
      (0..64).map(|_| Reply::Silence).collect(),
      false,
      |mut client| {
        Box::pin(async move {
          let refused = client.get_range("k", (0, 2), None, None).await;
          (refused, client.counts())
        })
      },
    );
    assert_eq!(refused, Err(S3Error::TimedOut));
    assert!(counts.timeouts >= 1, "{counts:?}");
  }

  /// A-471 (TLS): do connect to a store over TLS whose certificate the endpoint does not name (its authorities another
  /// store's). Expect the handshake refused typed, no request sent.
  #[test]
  fn a_store_whose_certificate_is_not_named_is_refused() {
    let rt = Runtime::start(&config()).unwrap();
    let id = rt.shard_ids()[0];
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), 4).unwrap();
    let addr = listener.local_addr().unwrap();
    let (_, served) = store_tls();
    let (other, _) = store_tls();
    let (requests_tx, requests_rx) = channel();
    rt.spawn_on(id, store(listener, Vec::new(), requests_tx, Some(served)))
      .unwrap();
    let client = Client::new(
      Endpoint {
        addr,
        host: STORE_NAME.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "b".to_owned(),
        style: Style::VirtualHosted,
        carriage: Carriage::Tls {
          authorities: vec![other],
        },
      },
      Credentials::new(ACCESS_KEY_ID, SECRET, None),
      Limits {
        liveness_ns: LIVENESS_NS,
        ceiling_ns: CEILING_NS,
      },
      example_ns,
      7,
    )
    .unwrap();
    let (tx, rx) = channel();
    rt.spawn_on(id, async move {
      let mut client = client;
      let _ = tx.send(client.head("k", None).await);
    })
    .unwrap();
    let outcome = rx.recv_timeout(SCENARIO_WAIT).unwrap();
    rt.shutdown().unwrap();
    assert!(
      matches!(outcome, Err(S3Error::Tls(TlsError::Session(_)))),
      "{outcome:?}"
    );
    assert_eq!(requests_rx.try_iter().count(), 0, "no request sent");
  }

  /// A-471: do make a client for a plain endpoint off loopback, and ask an empty range. Expect each refused before
  /// anything is sent.
  #[test]
  fn plain_http_off_loopback_and_an_empty_range_are_refused() {
    let endpoint = Endpoint {
      addr: SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 80),
      host: "store".to_owned(),
      region: "us-east-1".to_owned(),
      bucket: "b".to_owned(),
      style: Style::Path,
      carriage: Carriage::PlainLoopback,
    };
    let limits = Limits {
      liveness_ns: LIVENESS_NS,
      ceiling_ns: CEILING_NS,
    };
    let credentials = Credentials::new(ACCESS_KEY_ID, SECRET, None);
    assert!(matches!(
      Client::new(endpoint, credentials, limits, example_ns, 7),
      Err(S3Error::Refused("plain HTTP to an address off loopback"))
    ));
    let (refused, requests) = against(Vec::new(), false, |mut client| {
      Box::pin(async move { client.get_range("k", (0, 0), None, None).await })
    });
    assert_eq!(refused, Err(S3Error::Refused("an empty range")));
    assert!(requests.is_empty());
  }
}

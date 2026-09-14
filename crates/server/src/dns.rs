//! Name resolution for a fleet peer's advertised address (§4.8 "Deployment": *"each node's advertised
//! address"*; the Kubernetes deployment of `docs/deploy.md`, where a node is addressed by its per-pod DNS
//! name and a rescheduled pod comes back under a new IP). A manifest may name a peer by a DNS name instead
//! of an IPv4 literal; the dialer resolves the name **at every fresh dial** — a peer that moved is reached at
//! its new address on the next re-dial, without a restart or an operator edit.
//!
//! This is a minimal, asynchronous A-record client (RFC 1035 §4) over the runtime's own UDP socket, and
//! nothing more: one question, the first `A`/`IN` answer, the nameservers and the timeout from the operating
//! system's resolver configuration (the command reads `/etc/resolv.conf`, R1 allows it to read host paths;
//! this crate takes the values). It exists because the lint wall reserves `std::net` (so `getaddrinfo`
//! through the standard library is not available here), and because a blocking resolver call on the control
//! shard would stall every SWIM probe and acknowledgement that shard serves for the length of a DNS outage —
//! a false death by name resolution. Here the wait is a future on the shard's driver like any other exchange
//! (R6), bounded by the resolver's timeout and attempt count, and it delays only the one dial that asked.
//!
//! What resolution can and cannot do to the fleet's security: nothing. A peer is admitted by the certificate
//! the manifest pins for it (mutual TLS, §4.8), so a wrong or forged answer can only make a dial fail
//! against the pin — a denial, never a wrong peer. The query id and the kernel-chosen source port are checked
//! on the reply as RFC 5452 §4 asks, so a stray or late datagram is not mistaken for the answer.
//!
//! The codec is pure and hostile-input tested (a parser of external bytes: every length is checked against
//! the message before it is read; a compression pointer must point strictly backwards, so a pointer loop is
//! impossible by construction rather than by a hop budget). The lookup is proven on the simulation fabric
//! against a nameserver task, at N=1 with no OS network (R8).

use slates_rt::RtError;
use slates_rt::futures::{now_ns, sleep};
use slates_rt::udp::{Ipv4Addr, SocketAddrV4, UdpSocket};

/// The resolver configuration the command read from the operating system (`/etc/resolv.conf`): where to
/// ask, how long to wait for each answer, and how many rounds over the nameservers to make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolver {
  /// The nameservers to ask, in the operating system's order; each is tried in turn within one attempt.
  pub nameservers: Vec<SocketAddrV4>,
  /// How long one query waits for its answer (nanoseconds) — `options timeout:` of `resolv.conf(5)`, or
  /// its documented default, read by the command.
  pub timeout_ns: u64,
  /// How many rounds over the nameservers a lookup makes before it fails — `options attempts:` of
  /// `resolv.conf(5)`, or its documented default, read by the command. At least one.
  pub attempts: u32,
}

/// Why a name did not resolve. Each names what an operator would check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsError {
  /// The name is not a valid DNS hostname (a label empty or over 63 bytes, the name over 253 bytes, or a
  /// character outside letters, digits and hyphens), so no query was sent.
  Name {
    /// The name given.
    host: String,
    /// What is wrong with it.
    reason: &'static str,
  },
  /// The resolver lists no nameserver, so there is nobody to ask.
  NoNameserver,
  /// Every attempt to every nameserver timed out.
  Timeout {
    /// The last nameserver asked.
    nameserver: SocketAddrV4,
    /// The attempts made over the nameservers.
    attempts: u32,
  },
  /// The nameserver answered with an error code: 3 (`NXDOMAIN`) means the name does not exist — a pod not
  /// yet created, or a name misspelt in the manifest; 2 (`SERVFAIL`) a nameserver that could not answer.
  Refused {
    /// The reply's `RCODE` (RFC 1035 §4.1.1).
    rcode: u8,
  },
  /// The nameserver's reply carries no `A` record for the name (an `IN` class `A` question answered with
  /// only other records — a name that has just an `AAAA`, say).
  NoAddress,
  /// The reply could not be decoded: `at` names the field the bytes failed at.
  Malformed {
    /// The field the bytes failed at.
    at: &'static str,
  },
  /// The runtime refused the socket or the send.
  Io(RtError),
}

impl std::fmt::Display for DnsError {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Name { host, reason } => write!(out, "`{host}` is not a DNS hostname: {reason}"),
      Self::NoNameserver => out.write_str("the resolver lists no nameserver"),
      Self::Timeout {
        nameserver,
        attempts,
      } => write!(
        out,
        "no answer within the timeout after {attempts} attempt(s); last asked {nameserver}"
      ),
      Self::Refused { rcode } => write!(out, "the nameserver answered rcode {rcode}"),
      Self::NoAddress => out.write_str("the answer carries no A record"),
      Self::Malformed { at } => write!(out, "the reply is malformed at its {at}"),
      Self::Io(e) => write!(out, "the runtime refused the query: {e}"),
    }
  }
}

impl std::error::Error for DnsError {}

impl DnsError {
  /// The refusal's kind as a status counter suffix (`fleet.resolve.<kind>` in `slates status`), so an
  /// operator reading the counts sees *why* a peer's name is not resolving, not just that it is not.
  pub fn kind(&self) -> &'static str {
    match self {
      Self::Name { .. } => "name",
      Self::NoNameserver => "no-nameserver",
      Self::Timeout { .. } => "timeout",
      Self::Refused { .. } => "refused",
      Self::NoAddress => "no-address",
      Self::Malformed { .. } => "malformed",
      Self::Io(_) => "io",
    }
  }
}

/// Format: the DNS message header is twelve bytes — id, flags, and the four section counts, each two bytes
/// (RFC 1035 §4.1.1).
const HEADER_BYTES: usize = 12;
/// Format: the offset of the flags word in the header (after the id).
const FLAGS_OFFSET: usize = 2;
/// Format: the offset of `QDCOUNT` in the header (after the id and the flags).
const QDCOUNT_OFFSET: usize = 4;
/// Format: the offset of `ANCOUNT` in the header (after `QDCOUNT`).
const ANCOUNT_OFFSET: usize = 6;
/// Format: a question's fixed tail after its name — `QTYPE` and `QCLASS`, two bytes each (RFC 1035 §4.1.2).
const QUESTION_TAIL_BYTES: usize = 4;
/// Format: a resource record's fixed part after its name — `TYPE`, `CLASS` (two bytes each), `TTL` (four)
/// and `RDLENGTH` (two) (RFC 1035 §4.1.3).
const RECORD_FIXED_BYTES: usize = 10;
/// Format: the offset of `RDLENGTH` within that fixed part (after `TYPE`, `CLASS` and `TTL`).
const RDLENGTH_OFFSET: usize = 8;
/// Format: the largest DNS message carried over UDP without EDNS (RFC 1035 §4.2.1); the receive buffer, and
/// the largest reply this client reads (it asks for one `A` record, which fits in far less).
const MAX_UDP_MESSAGE: usize = 512;
/// Format: the longest label of a name, in bytes (RFC 1035 §2.3.4).
const MAX_LABEL_BYTES: usize = 63;
/// Format: the longest textual name, in bytes (RFC 1035 §2.3.4's 255-byte wire limit less the length and
/// root octets, as resolvers enforce it).
const MAX_NAME_BYTES: usize = 253;
/// Format: the `A` record type (an IPv4 address; RFC 1035 §3.2.2).
const TYPE_A: u16 = 1;
/// Format: the Internet class (RFC 1035 §3.2.4).
const CLASS_IN: u16 = 1;
/// Format: an IPv4 address is four bytes of `RDATA` (RFC 1035 §3.4.1).
const A_RDATA_BYTES: u16 = 4;
/// Format: the header flag asking for recursion (`RD`, RFC 1035 §4.1.1).
const FLAG_RECURSION_DESIRED: u16 = 0x0100;
/// Format: the header flag marking a reply (`QR`, RFC 1035 §4.1.1).
const FLAG_REPLY: u16 = 0x8000;
/// Format: the header's reply code lives in the low four bits of the flags (RFC 1035 §4.1.1).
const RCODE_MASK: u16 = 0x000F;
/// Format: a length octet whose top two bits are set is a compression pointer; the remaining fourteen bits
/// are the offset (RFC 1035 §4.1.4).
const POINTER_TAG: u8 = 0xC0;
/// Format: the mask over the pointer's first octet leaving the offset's high bits.
const POINTER_OFFSET_MASK: u8 = 0x3F;

/// Checks that `host` is a DNS hostname this client can ask for: labels of one to 63 letters, digits or
/// hyphens, joined by dots, at most 253 bytes in all; a trailing dot (a fully qualified name) is allowed
/// and ignored. Returns the labels.
fn labels_of(host: &str) -> Result<Vec<&str>, DnsError> {
  let name = |reason| DnsError::Name {
    host: host.to_owned(),
    reason,
  };
  let trimmed = host.strip_suffix('.').unwrap_or(host);
  if trimmed.is_empty() {
    return Err(name("it is empty"));
  }
  if trimmed.len() > MAX_NAME_BYTES {
    return Err(name("it is longer than 253 bytes"));
  }
  let mut labels = Vec::new();
  for label in trimmed.split('.') {
    if label.is_empty() {
      return Err(name(
        "it has an empty label (two dots in a row, or a leading dot)",
      ));
    }
    if label.len() > MAX_LABEL_BYTES {
      return Err(name("a label is longer than 63 bytes"));
    }
    if !label
      .bytes()
      .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
      return Err(name(
        "a label has a character other than a letter, a digit or a hyphen",
      ));
    }
    labels.push(label);
  }
  Ok(labels)
}

/// Whether `host` is a valid DNS hostname (see [`labels_of`]); the manifest reader asks before it accepts a
/// named address, so a misspelt name is refused at boot by name rather than failing every dial.
pub fn check_hostname(host: &str) -> Result<(), DnsError> {
  labels_of(host).map(|_| ())
}

/// Encodes one recursive `A`/`IN` question for `host` under `id` (RFC 1035 §4.1).
pub fn encode_query(id: u16, host: &str) -> Result<Vec<u8>, DnsError> {
  let labels = labels_of(host)?;
  let mut out = Vec::with_capacity(HEADER_BYTES + host.len() + 2 + QUESTION_TAIL_BYTES);
  out.extend_from_slice(&id.to_be_bytes());
  out.extend_from_slice(&FLAG_RECURSION_DESIRED.to_be_bytes());
  out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
  out.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
  out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
  out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
  for label in labels {
    // `labels_of` bounded every label to 63 bytes, so the length fits an octet.
    out.push(u8::try_from(label.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(label.as_bytes());
  }
  out.push(0); // the root label ends the name
  out.extend_from_slice(&TYPE_A.to_be_bytes());
  out.extend_from_slice(&CLASS_IN.to_be_bytes());
  Ok(out)
}

/// A big-endian `u16` at `at`, or the field's malformed error.
fn u16_at(bytes: &[u8], at: usize, field: &'static str) -> Result<u16, DnsError> {
  match (bytes.get(at), bytes.get(at.wrapping_add(1))) {
    (Some(high), Some(low)) => Ok(u16::from_be_bytes([*high, *low])),
    _ => Err(DnsError::Malformed { at: field }),
  }
}

/// Reads the name at `at` into `name` as lowercase dotted labels (without the trailing dot) and returns
/// the offset just past it in the message — past the pointer octets when the name ends in a pointer.
/// A compression pointer must point strictly before its own position (RFC 1035 §4.1.4: "a prior
/// occurrence"), which is what makes a loop impossible: every hop strictly decreases the read offset.
fn read_name(bytes: &[u8], at: usize, name: &mut String) -> Result<usize, DnsError> {
  let malformed = || DnsError::Malformed { at: "name" };
  let mut cursor = at;
  // The offset just past the name in the message's flow: set at the first pointer taken (the name's
  // remaining labels live elsewhere, but the message continues after the pointer), else past the root.
  let mut next: Option<usize> = None;
  loop {
    let Some(&length) = bytes.get(cursor) else {
      return Err(malformed());
    };
    if length & POINTER_TAG == POINTER_TAG {
      let low = bytes.get(cursor.wrapping_add(1)).ok_or_else(malformed)?;
      let target = usize::from(u16::from_be_bytes([length & POINTER_OFFSET_MASK, *low]));
      if target >= cursor {
        // A forward or self pointer: refused, so a hostile message cannot loop this reader.
        return Err(malformed());
      }
      if next.is_none() {
        next = Some(cursor.wrapping_add(2));
      }
      cursor = target;
      continue;
    }
    if length & POINTER_TAG != 0 {
      // The reserved `01` and `10` tags (RFC 1035 §4.1.4).
      return Err(malformed());
    }
    if length == 0 {
      return Ok(next.unwrap_or(cursor.wrapping_add(1)));
    }
    let start = cursor.wrapping_add(1);
    let end = start.wrapping_add(usize::from(length));
    let label = bytes.get(start..end).ok_or_else(malformed)?;
    if !name.is_empty() {
      name.push('.');
    }
    if name.len().wrapping_add(label.len()) > MAX_NAME_BYTES {
      return Err(malformed());
    }
    name.extend(label.iter().map(|b| b.to_ascii_lowercase() as char));
    cursor = end;
  }
}

/// Decodes the reply `bytes` to the query `id` for `host` (RFC 1035 §4.1): checks the id and the reply
/// flag, the reply code, that the echoed question is the one asked (name, type and class — a nameserver
/// may change the name's letter case, RFC 4343), then returns the first `A`/`IN` record of the answer
/// section. A reply that is not for this query is `Malformed { at: "id" }`, so a caller keeps waiting for
/// the right one rather than failing the lookup on a stray datagram.
pub fn decode_answer(id: u16, host: &str, bytes: &[u8]) -> Result<Ipv4Addr, DnsError> {
  if u16_at(bytes, 0, "id")? != id {
    return Err(DnsError::Malformed { at: "id" });
  }
  let flags = u16_at(bytes, FLAGS_OFFSET, "flags")?;
  if flags & FLAG_REPLY == 0 {
    return Err(DnsError::Malformed { at: "flags" });
  }
  let rcode = flags & RCODE_MASK;
  if rcode != 0 {
    return Err(DnsError::Refused {
      rcode: u8::try_from(rcode).unwrap_or(u8::MAX),
    });
  }
  let questions = u16_at(bytes, QDCOUNT_OFFSET, "qdcount")?;
  let answers = u16_at(bytes, ANCOUNT_OFFSET, "ancount")?;
  if questions != 1 {
    return Err(DnsError::Malformed { at: "qdcount" });
  }
  // The echoed question must be the one asked.
  let mut asked = String::new();
  let mut cursor = read_name(bytes, HEADER_BYTES, &mut asked)?;
  let expected = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
  if asked != expected {
    return Err(DnsError::Malformed {
      at: "question name",
    });
  }
  if u16_at(bytes, cursor, "qtype")? != TYPE_A || u16_at(bytes, cursor + 2, "qclass")? != CLASS_IN {
    return Err(DnsError::Malformed { at: "question" });
  }
  cursor = cursor.wrapping_add(QUESTION_TAIL_BYTES);
  for _ in 0..answers {
    let mut owner = String::new();
    cursor = read_name(bytes, cursor, &mut owner)?;
    let record_type = u16_at(bytes, cursor, "type")?;
    let class = u16_at(bytes, cursor + 2, "class")?;
    let rdlength = u16_at(bytes, cursor + RDLENGTH_OFFSET, "rdlength")?;
    let rdata_start = cursor.wrapping_add(RECORD_FIXED_BYTES);
    let rdata_end = rdata_start.wrapping_add(usize::from(rdlength));
    let rdata = bytes
      .get(rdata_start..rdata_end)
      .ok_or(DnsError::Malformed { at: "rdata" })?;
    if record_type == TYPE_A
      && class == CLASS_IN
      && rdlength == A_RDATA_BYTES
      && let [a, b, c, d] = rdata
    {
      return Ok(Ipv4Addr::new(*a, *b, *c, *d));
    }
    cursor = rdata_end;
  }
  Err(DnsError::NoAddress)
}

/// A query id for `host` on `attempt`: sixteen bits of `BLAKE3(now ‖ host ‖ attempt)` — unpredictable
/// to an off-path sender (the clock is read at nanosecond resolution), distinct per attempt so a late
/// reply to an earlier attempt is not taken for this one.
fn query_id(host: &str, attempt: u32) -> u16 {
  let mut hasher = blake3::Hasher::new();
  hasher.update(&now_ns().to_le_bytes());
  hasher.update(host.as_bytes());
  hasher.update(&attempt.to_le_bytes());
  let bytes = *hasher.finalize().as_bytes();
  u16::from_le_bytes([bytes[0], bytes[1]])
}

/// Receives one datagram into `buf`, or `None` when `timeout_ns` pass first. A delivered datagram is
/// preferred when both are ready.
async fn recv_within(
  socket: &UdpSocket,
  buf: &mut [u8],
  timeout_ns: u64,
) -> Option<Result<(usize, SocketAddrV4), RtError>> {
  let mut receive = std::pin::pin!(socket.recv_from(buf));
  let mut timer = std::pin::pin!(sleep(timeout_ns));
  std::future::poll_fn(|cx| {
    if let std::task::Poll::Ready(received) = std::future::Future::poll(receive.as_mut(), cx) {
      return std::task::Poll::Ready(Some(received));
    }
    if std::future::Future::poll(timer.as_mut(), cx).is_ready() {
      return std::task::Poll::Ready(None);
    }
    std::task::Poll::Pending
  })
  .await
}

/// One query to one nameserver: sends it from a fresh socket and waits up to the resolver's timeout for
/// the reply that carries its id and comes from that nameserver (a stray datagram is skipped and the wait
/// continues within the same deadline). `Ok(None)` is a timeout.
async fn ask(
  resolver: &Resolver,
  nameserver: SocketAddrV4,
  host: &str,
  attempt: u32,
) -> Result<Option<Ipv4Addr>, DnsError> {
  let id = query_id(host, attempt);
  let query = encode_query(id, host)?;
  let socket =
    UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).map_err(DnsError::Io)?;
  socket.send_to(&query, nameserver).map_err(DnsError::Io)?;
  let deadline = now_ns().saturating_add(resolver.timeout_ns);
  let mut buf = [0u8; MAX_UDP_MESSAGE];
  loop {
    let remaining = deadline.saturating_sub(now_ns());
    if remaining == 0 {
      return Ok(None);
    }
    match recv_within(&socket, &mut buf, remaining).await {
      None => return Ok(None),
      Some(Err(e)) => return Err(DnsError::Io(e)),
      Some(Ok((len, from))) => {
        if from != nameserver {
          continue;
        }
        let reply = buf.get(..len).unwrap_or(&buf[..]);
        match decode_answer(id, host, reply) {
          // Not this query's reply (another id): keep waiting for the right one.
          Err(DnsError::Malformed { at: "id" }) => continue,
          Ok(address) => return Ok(Some(address)),
          Err(e) => return Err(e),
        }
      }
    }
  }
}

/// Resolves `host` to an IPv4 address through `resolver`: each attempt asks every nameserver in turn,
/// and the first answer wins; a definitive refusal (`NXDOMAIN`, a malformed reply) ends the lookup at
/// once, a timeout moves to the next nameserver, then the next attempt. Bounded by
/// `attempts × nameservers × timeout`.
pub async fn lookup(resolver: &Resolver, host: &str) -> Result<Ipv4Addr, DnsError> {
  if resolver.nameservers.is_empty() {
    return Err(DnsError::NoNameserver);
  }
  let attempts = resolver.attempts.max(1);
  let mut last_asked = None;
  for attempt in 0..attempts {
    for nameserver in &resolver.nameservers {
      last_asked = Some(*nameserver);
      if let Some(address) = ask(resolver, *nameserver, host, attempt).await? {
        return Ok(address);
      }
    }
  }
  Err(DnsError::Timeout {
    nameserver: last_asked.unwrap_or(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
    attempts,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A reply to `query` carrying `answers` — each `(owner, type, class, rdata)` — with `rcode`; the
  /// question is echoed byte for byte. The test's own encoder, so the decoder is checked against the
  /// wire shape rather than against itself.
  fn reply(query: &[u8], rcode: u16, answers: &[(&[u8], u16, u16, &[u8])]) -> Vec<u8> {
    let mut out = query.to_vec();
    let flags = FLAG_REPLY | FLAG_RECURSION_DESIRED | rcode;
    out[2..4].copy_from_slice(&flags.to_be_bytes());
    out[6..8].copy_from_slice(&u16::try_from(answers.len()).unwrap().to_be_bytes());
    for (owner, record_type, class, rdata) in answers {
      out.extend_from_slice(owner);
      out.extend_from_slice(&record_type.to_be_bytes());
      out.extend_from_slice(&class.to_be_bytes());
      out.extend_from_slice(&60u32.to_be_bytes()); // TTL
      out.extend_from_slice(&u16::try_from(rdata.len()).unwrap().to_be_bytes());
      out.extend_from_slice(rdata);
    }
    out
  }

  /// A compression pointer to the question name at the start of the question section (offset 12, the
  /// header's length).
  const POINTER_TO_QUESTION: &[u8] = &[POINTER_TAG, 12];

  /// What a fabric nameserver answers a query with.
  type Answer = fn(&[u8]) -> Vec<u8>;

  const HOST: &str = "slates-1.slates.default.svc.cluster.local";

  /// A query is the documented shape (RFC 1035 §4.1): header with `RD`, one question of labels, `A`, `IN`.
  #[test]
  fn a_query_is_the_rfc_1035_shape() {
    let query = encode_query(0x1234, "a.b").unwrap();
    assert_eq!(
      query,
      [
        0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, // header
        1, b'a', 1, b'b', 0, // the name
        0, 1, 0, 1 // A, IN
      ]
    );
    assert_eq!(
      encode_query(0x1234, "a.b.").unwrap(),
      query,
      "a trailing dot (a fully qualified name) is the same question"
    );
  }

  fn sim_config() -> slates_rt::RuntimeConfig {
    slates_rt::RuntimeConfig {
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
    }
  }

  /// Shape: the simulated resolver's per-query timeout — long against the fabric's instant delivery, short
  /// against the test's patience.
  const SIM_TIMEOUT_NS: u64 = 50_000_000;

  /// A nameserver on the fabric that answers every query as `answer` says (`None`: stays silent), and the
  /// lookup against it — the outcome is handed out through `outcome`.
  fn resolve_on_the_fabric(
    answer: Option<Answer>,
    attempts: u32,
    nameservers: usize,
  ) -> (Result<Ipv4Addr, DnsError>, u64) {
    let mut sim = slates_rt::SimRuntime::new(&sim_config(), 1).unwrap();
    let shard = sim.shard_ids()[0];
    let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
    let (outcome_tx, outcome_rx) = std::sync::mpsc::channel::<(Result<Ipv4Addr, DnsError>, u64)>();
    sim
      .spawn_on(shard, async move {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _ = port_tx.send(socket.local_addr().unwrap().port());
        let mut buf = [0u8; MAX_UDP_MESSAGE];
        loop {
          let (len, from) = socket.recv_from(&mut buf).await.unwrap();
          if let Some(answer) = answer {
            let reply = answer(&buf[..len]);
            let _ = socket.send_to(&reply, from);
          }
        }
      })
      .unwrap();
    sim
      .spawn_on(shard, async move {
        let port = loop {
          if let Ok(port) = port_rx.try_recv() {
            break port;
          }
          sleep(1_000).await;
        };
        let resolver = Resolver {
          nameservers: (0..nameservers)
            .map(|_| SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .collect(),
          timeout_ns: SIM_TIMEOUT_NS,
          attempts,
        };
        let started = now_ns();
        let outcome = lookup(&resolver, HOST).await;
        let _ = outcome_tx.send((outcome, now_ns().saturating_sub(started)));
      })
      .unwrap();
    sim.run_until_idle();
    outcome_rx.try_recv().expect("the lookup task reported")
  }

  /// A reply to the query bytes as the fabric nameserver sees them: the same id, one `A` record.
  fn answer_with_address(query: &[u8]) -> Vec<u8> {
    reply(
      query,
      0,
      &[(POINTER_TO_QUESTION, TYPE_A, CLASS_IN, &[10, 244, 0, 9])],
    )
  }

  fn answer_nxdomain(query: &[u8]) -> Vec<u8> {
    reply(query, 3, &[])
  }

  /// By use, on the simulation fabric (R8: no OS network): a lookup sends its query to the nameserver and
  /// takes the address from the answer.
  #[test]
  fn a_lookup_resolves_through_a_nameserver_on_the_fabric() {
    let (outcome, _) = resolve_on_the_fabric(Some(answer_with_address), 2, 1);
    assert_eq!(outcome, Ok(Ipv4Addr::new(10, 244, 0, 9)));
  }

  /// A silent nameserver: every attempt to every nameserver times out on the virtual clock, and the lookup
  /// ends `Timeout` after exactly `attempts × nameservers` timeouts — bounded, never hung.
  #[test]
  fn a_silent_nameserver_times_the_lookup_out_after_every_attempt() {
    let (outcome, elapsed_ns) = resolve_on_the_fabric(None, 2, 3);
    assert!(
      matches!(outcome, Err(DnsError::Timeout { attempts: 2, .. })),
      "{outcome:?}"
    );
    let bound = SIM_TIMEOUT_NS * 2 * 3;
    assert!(
      elapsed_ns >= bound && elapsed_ns < bound + SIM_TIMEOUT_NS,
      "the lookup waited its whole budget and no more: {elapsed_ns} ns against {bound} ns"
    );
  }

  /// `NXDOMAIN` (a pod not yet created) ends the lookup at once with the code, so the dialer retries on its
  /// own cadence instead of waiting out the timeout.
  #[test]
  fn a_definitive_refusal_ends_the_lookup_at_once() {
    let (outcome, elapsed_ns) = resolve_on_the_fabric(Some(answer_nxdomain), 2, 3);
    assert_eq!(outcome, Err(DnsError::Refused { rcode: 3 }));
    assert!(
      elapsed_ns < SIM_TIMEOUT_NS,
      "no timeout was waited out: {elapsed_ns} ns"
    );
  }

  /// The first `A`/`IN` answer is the address; a `CNAME` before it is skipped; the owner may be a pointer.
  #[test]
  fn the_first_a_record_is_the_address() {
    let id = 7;
    let query = encode_query(id, HOST).unwrap();
    let cname_target = b"\x05other\x05place\x00";
    let bytes = reply(
      &query,
      0,
      &[
        (POINTER_TO_QUESTION, 5, CLASS_IN, cname_target), // CNAME
        (cname_target, TYPE_A, CLASS_IN, &[10, 244, 1, 7]),
        (POINTER_TO_QUESTION, TYPE_A, CLASS_IN, &[10, 244, 1, 8]),
      ],
    );
    assert_eq!(
      decode_answer(id, HOST, &bytes),
      Ok(Ipv4Addr::new(10, 244, 1, 7))
    );
  }

  /// The echoed question may differ in letter case (RFC 4343) and carry a trailing dot on our side.
  #[test]
  fn the_question_is_matched_case_insensitively() {
    let id = 9;
    let query = encode_query(id, "Slates-0.SVC.cluster.local.").unwrap();
    let bytes = reply(
      &query,
      0,
      &[(POINTER_TO_QUESTION, TYPE_A, CLASS_IN, &[10, 0, 0, 1])],
    );
    assert_eq!(
      decode_answer(id, "slates-0.svc.CLUSTER.local", &bytes),
      Ok(Ipv4Addr::new(10, 0, 0, 1))
    );
  }

  /// The query id of the hostile-input tests, and a good reply to it: one `A` record for `HOST`.
  const HOSTILE_ID: u16 = 3;

  fn good_reply() -> (Vec<u8>, Vec<u8>) {
    let query = encode_query(HOSTILE_ID, HOST).unwrap();
    let good = reply(
      &query,
      0,
      &[(POINTER_TO_QUESTION, TYPE_A, CLASS_IN, &[1, 2, 3, 4])],
    );
    assert_eq!(
      decode_answer(HOSTILE_ID, HOST, &good),
      Ok(Ipv4Addr::new(1, 2, 3, 4))
    );
    (query, good)
  }

  /// Hostile input: every prefix of a good reply is refused, never panicked on.
  #[test]
  fn a_truncated_reply_is_refused() {
    let (_, good) = good_reply();
    for cut in 0..good.len() {
      assert!(
        decode_answer(HOSTILE_ID, HOST, &good[..cut]).is_err(),
        "a reply cut at {cut} bytes is refused"
      );
    }
  }

  /// Hostile input: a reply that is not for this query — another id (the one refusal the asker keeps
  /// waiting through), a query echoed back without the reply flag, another name's question, two
  /// questions — is refused by the field that gives it away.
  #[test]
  fn a_reply_for_another_query_is_refused() {
    let (query, good) = good_reply();
    assert_eq!(
      decode_answer(HOSTILE_ID + 1, HOST, &good),
      Err(DnsError::Malformed { at: "id" })
    );
    assert_eq!(
      decode_answer(HOSTILE_ID, HOST, &query),
      Err(DnsError::Malformed { at: "flags" })
    );
    let other = encode_query(HOSTILE_ID, "someone.else").unwrap();
    let other = reply(
      &other,
      0,
      &[(POINTER_TO_QUESTION, TYPE_A, CLASS_IN, &[1, 2, 3, 4])],
    );
    assert_eq!(
      decode_answer(HOSTILE_ID, HOST, &other),
      Err(DnsError::Malformed {
        at: "question name"
      })
    );
    let mut two_questions = good.clone();
    two_questions[4..6].copy_from_slice(&2u16.to_be_bytes());
    assert_eq!(
      decode_answer(HOSTILE_ID, HOST, &two_questions),
      Err(DnsError::Malformed { at: "qdcount" })
    );
  }

  /// `NXDOMAIN` is a definitive refusal carrying its code; a reply with only other record types, or an
  /// `A` record of the wrong length (never read past), is `NoAddress`.
  #[test]
  fn a_refusal_code_and_an_answer_without_an_address_are_typed() {
    let (query, _) = good_reply();
    let nxdomain = reply(&query, 3, &[]);
    assert_eq!(
      decode_answer(HOSTILE_ID, HOST, &nxdomain),
      Err(DnsError::Refused { rcode: 3 })
    );
    let only_aaaa = reply(
      &query,
      0,
      &[(POINTER_TO_QUESTION, 28, CLASS_IN, &[0u8; 16])],
    );
    assert_eq!(
      decode_answer(HOSTILE_ID, HOST, &only_aaaa),
      Err(DnsError::NoAddress)
    );
    let short_a = reply(
      &query,
      0,
      &[(POINTER_TO_QUESTION, TYPE_A, CLASS_IN, &[1, 2, 3])],
    );
    assert_eq!(
      decode_answer(HOSTILE_ID, HOST, &short_a),
      Err(DnsError::NoAddress)
    );
  }

  /// Hostile input in a name or a length: an `RDLENGTH` past the end, a pointer at itself or forward
  /// (no loop is possible), a label running past the end, the reserved label tags — each a typed
  /// refusal at the field.
  #[test]
  fn a_hostile_name_or_length_is_refused() {
    let (query, good) = good_reply();
    let mut overflow = good.clone();
    let rdlength_at = overflow.len() - 4 - 2;
    overflow[rdlength_at..rdlength_at + 2].copy_from_slice(&u16::MAX.to_be_bytes());
    assert_eq!(
      decode_answer(HOSTILE_ID, HOST, &overflow),
      Err(DnsError::Malformed { at: "rdata" })
    );
    let refused_name = |bytes: &[u8]| {
      assert_eq!(
        decode_answer(HOSTILE_ID, HOST, bytes),
        Err(DnsError::Malformed { at: "name" })
      );
    };
    let own_offset = u8::try_from(query.len()).unwrap();
    refused_name(&reply(
      &query,
      0,
      &[(&[POINTER_TAG, own_offset], TYPE_A, CLASS_IN, &[1, 2, 3, 4])],
    ));
    refused_name(&reply(
      &query,
      0,
      &[(&[POINTER_TAG, 0xFF], TYPE_A, CLASS_IN, &[1, 2, 3, 4])],
    ));
    let longest_label = u8::try_from(MAX_LABEL_BYTES).unwrap();
    refused_name(&reply(
      &query,
      0,
      &[(&[longest_label, b'x'], TYPE_A, CLASS_IN, &[])],
    ));
    refused_name(&reply(
      &query,
      0,
      &[(&[0x40, 0], TYPE_A, CLASS_IN, &[1, 2, 3, 4])],
    ));
  }

  /// A name that is not a hostname is refused by reason before any query is sent.
  #[test]
  fn a_bad_name_is_refused_by_reason() {
    let reason = |host: &str| match encode_query(1, host) {
      Err(DnsError::Name { reason, .. }) => reason,
      other => panic!("expected a name refusal for `{host}`, got {other:?}"),
    };
    assert_eq!(reason(""), "it is empty");
    assert_eq!(
      reason("a..b"),
      "it has an empty label (two dots in a row, or a leading dot)"
    );
    assert_eq!(
      reason(&format!("{}.x", "a".repeat(MAX_LABEL_BYTES + 1))),
      "a label is longer than 63 bytes"
    );
    assert_eq!(
      reason(&["abcdefghij"; 26].join(".")),
      "it is longer than 253 bytes"
    );
    assert_eq!(
      reason("under_score.local"),
      "a label has a character other than a letter, a digit or a hyphen"
    );
    assert!(check_hostname("slates-0.slates.default.svc.cluster.local.").is_ok());
    assert!(
      check_hostname("10.0.0.1").is_ok(),
      "digits-only labels are hostnames too"
    );
  }
}

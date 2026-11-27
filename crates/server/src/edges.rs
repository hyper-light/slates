//! A node's version-control edges, announced to its peers so a request at a node that is not a repository's owner can
//! be answered with where to go (A-925, main 01a20182's rulings of 2026-11-25; A-921 part 2's prerequisite).
//!
//! **Why.** A node's edges (the git HTTP edge, the SSH edge) are its own: the operator binds each where it likes, and a
//! test binds them at port 0. Its peers learn its probe and record addresses at discovery (A-277), never its edges, so a
//! node could name a repository's owner by member id but not say where a client reaches it.
//!
//! **What.** Each node sends each peer one edges record over its own authenticated record session (stream
//! [`EDGES_STREAM`]), once per session and again when its edges change, as its epoch announcement rides (member_task's
//! `announce_epochs`): the edges it serves as a client reaches them, its advertised host (A-277's reachable name, never
//! a bind address such as `0.0.0.0`), each edge's bound port (never 0) and its scheme. An HTTPS edge is announced only
//! where its certificate names that host (`rustls::client::verify_server_name`); a node with no reachable host, or none
//! its certificate names, announces no edge. A peer keeps each record per anchor ([`Edges::learned`]), bounded by the
//! members, and lets it go as its member leaves. Discovery's `Announcement` is unchanged: `slates_wire` refuses
//! trailing bytes, so a longer announcement would cost an older peer the whole enrollment exchange. An older peer
//! answers this stream empty, as any unknown kind, and learns nothing; membership is untouched.
//!
//! **Only its own.** A record names its sender's anchor, and is held only where that is the authenticated session's
//! anchor ([`serve_edges`]); otherwise it is refused typed ([`EdgesRefused::Foreign`]) and counted, and nothing is
//! learned, so one member cannot send another's clients elsewhere.
//!
//! **Bounded parsing.** A record is at most [`EDGES_RECORD_BYTES`]; each host is checked against [`HOST_BYTES`] (a DNS
//! name's 253) before it is read; every other shape is a typed refusal ([`EdgesRefused`]).
//!
//! **Where to go.** [`owner_edge`] answers a member's edge of a scheme, or the typed refusal naming the member where none
//! was learned (an older peer, a node serving no such edge): the one path, as a node that serves no edge.

use std::collections::BTreeMap;

use slates_db::register::HostId;
use slates_rt::futures;
use slates_transport::connection::Priority;
use slates_transport::endpoint::{ConnectionId, Endpoint};

use crate::state::{self, ShardState};

/// The stream an edges record rides (A-925). Format: a stream id past every other fleet stream (ledger reads 32).
pub(crate) const EDGES_STREAM: u64 = 33;

/// Format: the edges record's version byte, first.
const EDGES_VERSION: u8 = 1;
/// Format: a record's flag byte for an edge absent.
const EDGE_ABSENT: u8 = 0;
/// Format: a record's flag byte for an edge that follows.
const EDGE_PRESENT: u8 = 1;
/// Format: the most bytes a host name takes (RFC 1035 §2.3.4's 255 on the wire, 253 as text), an IP address's text far
/// within it.
pub(crate) const HOST_BYTES: usize = 253;
/// Format: one edge's bytes at most: its scheme, its host's length byte and host, its port.
const EDGE_BYTES: usize = 1 + 1 + HOST_BYTES + 2;
/// Format: a record's bytes at most: its version, its anchor, two edges each behind a flag byte.
pub(crate) const EDGES_RECORD_BYTES: usize = 1 + 8 + 2 * (1 + EDGE_BYTES);
/// Format: the one-byte answer a node that held a record gives (an older peer answers empty).
const HELD: u8 = 1;

/// Format: [`Scheme::Http`]'s byte on the wire.
const SCHEME_HTTP: u8 = 1;
/// Format: [`Scheme::Https`]'s byte on the wire.
const SCHEME_HTTPS: u8 = 2;
/// Format: [`Scheme::Ssh`]'s byte on the wire.
const SCHEME_SSH: u8 = 3;

/// An edge's scheme: how a client reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scheme {
  /// Git over plain HTTP (a loopback-only edge).
  Http,
  /// Git over HTTPS.
  Https,
  /// SSH.
  Ssh,
}

impl Scheme {
  /// Format: the scheme's byte on the wire.
  fn byte(self) -> u8 {
    match self {
      Scheme::Http => SCHEME_HTTP,
      Scheme::Https => SCHEME_HTTPS,
      Scheme::Ssh => SCHEME_SSH,
    }
  }

  /// The scheme of a byte, or the typed refusal.
  fn of(byte: u8) -> Result<Scheme, EdgesRefused> {
    match byte {
      SCHEME_HTTP => Ok(Scheme::Http),
      SCHEME_HTTPS => Ok(Scheme::Https),
      SCHEME_SSH => Ok(Scheme::Ssh),
      other => Err(EdgesRefused::Scheme { byte: other }),
    }
  }

  /// The URL scheme's text.
  pub(crate) fn text(self) -> &'static str {
    match self {
      Scheme::Http => "http",
      Scheme::Https => "https",
      Scheme::Ssh => "ssh",
    }
  }
}

/// One edge as a client reaches it: its scheme, host and bound port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Edge {
  /// How a client reaches it.
  pub scheme: Scheme,
  /// The reachable host, at most [`HOST_BYTES`] of printable ASCII.
  pub host: String,
  /// The bound port, never 0.
  pub port: u16,
}

/// A node's edges, as it announces them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EdgesRecord {
  /// The announcing node's anchor.
  pub anchor: HostId,
  /// Its git HTTP or HTTPS edge, if it serves one a client can reach.
  pub http: Option<Edge>,
  /// Its SSH edge, if it serves one a client can reach.
  pub ssh: Option<Edge>,
}

/// Why an edges record was refused: every malformed shape, and one naming another's anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgesRefused {
  /// Longer than [`EDGES_RECORD_BYTES`].
  Oversized,
  /// The bytes ended before a field.
  Truncated,
  /// Another version.
  Version {
    /// The byte found.
    version: u8,
  },
  /// A flag byte past its two values.
  Flag {
    /// The byte found.
    flag: u8,
  },
  /// An unknown scheme.
  Scheme {
    /// The byte found.
    byte: u8,
  },
  /// A host empty or longer than [`HOST_BYTES`].
  HostLength {
    /// The length claimed.
    len: usize,
  },
  /// A host that is not printable ASCII, or a bind address no client reaches.
  HostText,
  /// Port 0.
  Port,
  /// Bytes past the record.
  Trailing,
  /// A record naming an anchor other than its session's.
  Foreign,
}

impl EdgesRefused {
  /// Format: the counter each refusal is counted under.
  pub(crate) fn counter(self) -> &'static str {
    match self {
      EdgesRefused::Foreign => "fleet.edges.refused.foreign",
      _ => "fleet.edges.refused.malformed",
    }
  }
}

/// Whether `host` is a destination a client can reach: printable ASCII, no space, and no unspecified address.
fn reachable_host(host: &str) -> bool {
  !host.is_empty()
    && host.len() <= HOST_BYTES
    && host.bytes().all(|byte| byte.is_ascii_graphic())
    && host
      .parse::<core::net::IpAddr>()
      .map_or(true, |address| !address.is_unspecified())
}

impl EdgesRecord {
  /// The record's bytes. Format: version, anchor (u64 LE), then the HTTP and the SSH edge, each a flag byte and, where
  /// present, its scheme byte, its host's length byte and host, its port (u16 LE).
  pub(crate) fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(EDGES_RECORD_BYTES);
    out.push(EDGES_VERSION);
    out.extend_from_slice(&self.anchor.0.to_le_bytes());
    for edge in [&self.http, &self.ssh] {
      match edge {
        None => out.push(EDGE_ABSENT),
        Some(edge) => {
          out.push(EDGE_PRESENT);
          out.push(edge.scheme.byte());
          out.push(u8::try_from(edge.host.len()).unwrap_or(u8::MAX));
          out.extend_from_slice(edge.host.as_bytes());
          out.extend_from_slice(&edge.port.to_le_bytes());
        }
      }
    }
    out
  }

  /// A record from its bytes, each field checked before it is read: [`EdgesRefused`] for every malformed shape.
  pub(crate) fn decode(bytes: &[u8]) -> Result<EdgesRecord, EdgesRefused> {
    if bytes.len() > EDGES_RECORD_BYTES {
      return Err(EdgesRefused::Oversized);
    }
    let mut rest = bytes;
    let version = take_byte(&mut rest)?;
    if version != EDGES_VERSION {
      return Err(EdgesRefused::Version { version });
    }
    let (anchor, after) = rest
      .split_first_chunk::<{ core::mem::size_of::<u64>() }>()
      .ok_or(EdgesRefused::Truncated)?;
    rest = after;
    let anchor = HostId(u64::from_le_bytes(*anchor));
    let http = decode_edge(&mut rest)?;
    let ssh = decode_edge(&mut rest)?;
    if !rest.is_empty() {
      return Err(EdgesRefused::Trailing);
    }
    Ok(EdgesRecord { anchor, http, ssh })
  }
}

/// One byte off the front, or [`EdgesRefused::Truncated`].
fn take_byte(rest: &mut &[u8]) -> Result<u8, EdgesRefused> {
  let (byte, after) = rest.split_first().ok_or(EdgesRefused::Truncated)?;
  *rest = after;
  Ok(*byte)
}

/// One edge behind its flag byte.
fn decode_edge(rest: &mut &[u8]) -> Result<Option<Edge>, EdgesRefused> {
  match take_byte(rest)? {
    EDGE_ABSENT => Ok(None),
    EDGE_PRESENT => {
      let scheme = Scheme::of(take_byte(rest)?)?;
      let len = usize::from(take_byte(rest)?);
      if len == 0 || len > HOST_BYTES {
        return Err(EdgesRefused::HostLength { len });
      }
      if rest.len() < len {
        return Err(EdgesRefused::Truncated);
      }
      let (host, after) = rest.split_at(len);
      *rest = after;
      let host = std::str::from_utf8(host).map_err(|_| EdgesRefused::HostText)?;
      if !reachable_host(host) {
        return Err(EdgesRefused::HostText);
      }
      let (port, after) = rest
        .split_first_chunk::<2>()
        .ok_or(EdgesRefused::Truncated)?;
      *rest = after;
      let port = u16::from_le_bytes(*port);
      if port == 0 {
        return Err(EdgesRefused::Port);
      }
      Ok(Some(Edge {
        scheme,
        host: host.to_owned(),
        port,
      }))
    }
    flag => Err(EdgesRefused::Flag { flag }),
  }
}

/// Counter: edges records sent. Format: a counter name.
const SENT: &str = "fleet.edges.sent";
/// Counter: edges records a peer held (its one-byte answer). Format: a counter name.
const HELD_BY_PEER: &str = "fleet.edges.held_by_peer";
/// Counter: edges records a peer answered empty (an older peer). Format: a counter name.
const UNREAD_BY_PEER: &str = "fleet.edges.unread_by_peer";
/// Counter: edges records learned from a peer. Format: a counter name.
const LEARNED: &str = "fleet.edges.learned";

/// This node's edges and what it learned of its peers' (one field of the control shard's [`ShardState`], A-925).
#[derive(Debug, Default)]
pub(crate) struct Edges {
  /// This node's own record, once its edges are bound and its reachable host known; `None` before, and where it serves
  /// none a client reaches.
  local: Option<EdgesRecord>,
  /// How many times this node's record changed: a session it was sent on at an older one owes it again.
  generation: u64,
  /// Each peer member's session the record was sent on, and at which generation. Bounded by the record sessions.
  sent_on: BTreeMap<HostId, (ConnectionId, u64)>,
  /// Each peer's record, by anchor. Bounded by the members; one leaves with its member.
  learned: BTreeMap<HostId, EdgesRecord>,
}

impl Edges {
  /// Sets this node's own record, a changed one owed again on every session.
  pub(crate) fn set_local(&mut self, record: Option<EdgesRecord>) {
    if self.local != record {
      self.local = record;
      self.generation = self.generation.saturating_add(1);
    }
  }

  /// The record learned of `anchor`, if any.
  pub(crate) fn learned(&self, anchor: HostId) -> Option<&EdgesRecord> {
    self.learned.get(&anchor)
  }

  /// Forgets what was learned of anchors no longer among `present`, and the sessions of members gone.
  pub(crate) fn retain(&mut self, present: impl Fn(HostId) -> bool) {
    self.learned.retain(|anchor, _| present(*anchor));
  }
}

/// A peer's answer to an edges record (A-925): held where it names the authenticated session's anchor `peer_anchor`,
/// answered [`HELD`]; refused typed and counted otherwise, nothing learned, answered empty.
pub(crate) fn serve_edges(state: &mut ShardState, peer_anchor: HostId, request: &[u8]) -> Vec<u8> {
  let record = match EdgesRecord::decode(request) {
    Ok(record) if record.anchor == peer_anchor => record,
    Ok(_) => {
      state.count(EdgesRefused::Foreign.counter(), 1);
      return Vec::new();
    }
    Err(refused) => {
      state.count(refused.counter(), 1);
      return Vec::new();
    }
  };
  state.edges.learned.insert(peer_anchor, record);
  state.count(LEARNED, 1);
  vec![HELD]
}

/// Sends this node's edges record on each of its record sessions that has not carried its current one (A-925): once a
/// session, again when its edges change. An answer of any kind ends the session's debt, an older peer's empty one
/// included; a lost exchange is owed again.
pub(crate) async fn announce_edges() {
  let owed: Vec<HostId> = state::with_state(|state| {
    let Some(_) = state.edges.local.as_ref() else {
      return Vec::new();
    };
    let generation = state.edges.generation;
    let sent_on = &state.edges.sent_on;
    state
      .record_sessions
      .iter_mut()
      .filter_map(|(member, link)| {
        let connection = link.endpoint.as_mut()?.connection_id().ok()?;
        (sent_on.get(member) != Some(&(connection, generation))).then_some(*member)
      })
      .collect()
  })
  .unwrap_or_default();
  for member in owed {
    let mut sessions =
      crate::fleet::take_sessions("fleet.lent_to.announce_edges", |host| host == member);
    for (_, endpoint) in &mut sessions {
      announce_on(member, endpoint).await;
    }
    crate::fleet::return_sessions(sessions);
  }
}

/// One edges record on one borrowed session to `member`.
async fn announce_on(member: HostId, endpoint: &mut Endpoint) {
  let Ok(connection) = endpoint.connection_id() else {
    return;
  };
  let Some((bytes, generation)) = state::with_state(|state| {
    let generation = state.edges.generation;
    if state.edges.sent_on.get(&member) == Some(&(connection, generation)) {
      return None;
    }
    Some((state.edges.local.as_ref()?.encode(), generation))
  })
  .flatten() else {
    return;
  };
  let budget = crate::fleet::record_exchange_budget_ns();
  let before = endpoint.last_exchange();
  let answered = futures::within(
    budget,
    endpoint.request(EDGES_STREAM, Priority::Control, &bytes),
  )
  .await;
  let reply = match answered {
    Ok(Some(Ok(reply))) => reply,
    _ => {
      endpoint.abandon_begun_since(before);
      return;
    }
  };
  let _ = state::with_state_counted(|state| {
    state.count(SENT, 1);
    state.count(
      if reply.as_slice() == [HELD] {
        HELD_BY_PEER
      } else {
        UNREAD_BY_PEER
      },
      1,
    );
    state.edges.sent_on.insert(member, (connection, generation));
    let members: Vec<HostId> = state.record_sessions.keys().copied().collect();
    state
      .edges
      .sent_on
      .retain(|member, _| members.contains(member));
  });
}

/// A repository owner's edge a client reaches it at (A-925): `member`'s edge of `scheme`'s kind (git's HTTP or HTTPS, or
/// SSH), as its record announced it; or the typed refusal naming the member, where none was learned (an older peer, or a
/// node serving no such edge: the one path, as a node with no edge).
pub(crate) fn owner_edge(state: &ShardState, member: HostId, ssh: bool) -> Result<Edge, NoEdge> {
  crate::member_task::anchor_of(state, member)
    .and_then(|anchor| state.edges.learned(anchor))
    .and_then(|record| {
      if ssh {
        record.ssh.clone()
      } else {
        record.http.clone()
      }
    })
    .ok_or(NoEdge { member })
}

/// No edge is known for a repository's owner (A-925): the refusal names it by member id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NoEdge {
  /// The owner, by member id.
  pub member: HostId,
}

/// This node's own record (A-925): its edges at its advertised host `host`, each at its bound port; an HTTPS edge only
/// where `https_leaf` (the edge's certificate, its leaf) names `host`. `None` where `host` is no reachable destination
/// or no edge is served.
pub(crate) fn local_record(
  anchor: HostId,
  host: &str,
  (http, https_leaf): (Option<u16>, Option<&[u8]>),
  ssh: Option<u16>,
) -> Option<EdgesRecord> {
  if !reachable_host(host) {
    return None;
  }
  let http = http
    .filter(|port| *port != 0)
    .and_then(|port| match https_leaf {
      None => Some(Edge {
        scheme: Scheme::Http,
        host: host.to_owned(),
        port,
      }),
      Some(leaf) => certificate_names(leaf, host).then(|| Edge {
        scheme: Scheme::Https,
        host: host.to_owned(),
        port,
      }),
    });
  let ssh = ssh.filter(|port| *port != 0).map(|port| Edge {
    scheme: Scheme::Ssh,
    host: host.to_owned(),
    port,
  });
  (http.is_some() || ssh.is_some()).then_some(EdgesRecord { anchor, http, ssh })
}

/// Whether the certificate `leaf` (DER) names `host`, as a client's TLS check would (rustls's `verify_server_name`).
fn certificate_names(leaf: &[u8], host: &str) -> bool {
  let certificate = rustls::pki_types::CertificateDer::from(leaf.to_vec());
  let Ok(parsed) = rustls::server::ParsedCertificate::try_from(&certificate) else {
    return false;
  };
  let Ok(name) = rustls::pki_types::ServerName::try_from(host.to_owned()) else {
    return false;
  };
  rustls::client::verify_server_name(&parsed, &name).is_ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn record() -> EdgesRecord {
    EdgesRecord {
      anchor: HostId(7),
      http: Some(Edge {
        scheme: Scheme::Http,
        host: "127.0.0.1".to_owned(),
        port: 4321,
      }),
      ssh: Some(Edge {
        scheme: Scheme::Ssh,
        host: "127.0.0.1".to_owned(),
        port: 4322,
      }),
    }
  }

  /// A-925 (main 01a20182's condition 3: bounded parsing). Do: encode a record and decode it; then decode every cut of
  /// it. Expect the record back whole, and every cut refused.
  #[test]
  fn an_edges_record_reads_back_whole_and_refuses_every_cut_of_it() {
    let bytes = record().encode();
    assert_eq!(EdgesRecord::decode(&bytes), Ok(record()));
    for cut in 0..bytes.len() {
      assert!(
        EdgesRecord::decode(bytes.get(..cut).unwrap_or_default()).is_err(),
        "a record cut at {cut} is refused"
      );
    }
  }

  /// A-925 (main 01a20182's condition 3: bounded parsing). Do: decode a record with a trailing byte, an oversized one,
  /// and one of another version. Expect each refused typed: trailing, oversized, its version.
  #[test]
  fn an_edges_record_refuses_a_trailing_byte_an_oversized_record_and_another_version() {
    let bytes = record().encode();
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(EdgesRecord::decode(&trailing), Err(EdgesRefused::Trailing));
    assert_eq!(
      EdgesRecord::decode(&vec![EDGES_VERSION; EDGES_RECORD_BYTES + 1]),
      Err(EdgesRefused::Oversized)
    );
    let mut version = bytes;
    if let Some(byte) = version.first_mut() {
      *byte = 2;
    }
    assert_eq!(
      EdgesRecord::decode(&version),
      Err(EdgesRefused::Version { version: 2 })
    );
  }

  /// A-925 (main 01a20182's condition 3: bounded parsing). Do: decode a record with an unknown scheme, one whose host is
  /// a whole DNS name (253 bytes), and one whose host claims 254. Expect the scheme refused typed, the 253-byte host
  /// whole, and the 254-byte one refused by its length.
  #[test]
  fn an_edges_record_refuses_an_unknown_scheme_and_a_host_past_a_dns_name() {
    let mut scheme = record().encode();
    if let Some(byte) = scheme.get_mut(10) {
      *byte = 9;
    }
    assert_eq!(
      EdgesRecord::decode(&scheme),
      Err(EdgesRefused::Scheme { byte: 9 })
    );
    let long = EdgesRecord {
      http: Some(Edge {
        scheme: Scheme::Http,
        host: "h".repeat(HOST_BYTES),
        port: 1,
      }),
      ..record()
    };
    assert!(
      EdgesRecord::decode(&long.encode()).is_ok(),
      "a 253-byte host is whole"
    );
    let mut too_long = vec![EDGES_VERSION];
    too_long.extend_from_slice(&7u64.to_le_bytes());
    too_long.extend_from_slice(&[EDGE_PRESENT, 1, 254]);
    assert_eq!(
      EdgesRecord::decode(&too_long),
      Err(EdgesRefused::HostLength { len: 254 })
    );
  }

  /// A-925 (main 01a20182's condition 3: bounded parsing). Do: decode a record whose HTTP edge names a bind address, and
  /// one whose SSH edge is at port 0. Expect the bind address refused as host text and port 0 refused as a port.
  #[test]
  fn an_edges_record_refuses_a_bind_address_and_port_0() {
    let bind = EdgesRecord {
      http: Some(Edge {
        scheme: Scheme::Http,
        host: "0.0.0.0".to_owned(),
        port: 1,
      }),
      ..record()
    };
    assert_eq!(
      EdgesRecord::decode(&bind.encode()),
      Err(EdgesRefused::HostText)
    );
    let unbound = EdgesRecord {
      ssh: Some(Edge {
        scheme: Scheme::Ssh,
        host: "127.0.0.1".to_owned(),
        port: 0,
      }),
      ..record()
    };
    assert_eq!(
      EdgesRecord::decode(&unbound.encode()),
      Err(EdgesRefused::Port)
    );
  }

  /// A-925 (main 01a20182's condition 2): a node announces what a client reaches. Do: make the local record at a bind
  /// address, at a reachable host with HTTP bound at port 0 and SSH bound, and with neither edge. Expect none at the bind
  /// address, only the SSH edge where HTTP's port is 0, and none with no edge.
  #[test]
  fn a_node_announces_only_edges_a_client_reaches() {
    assert_eq!(
      local_record(HostId(1), "0.0.0.0", (Some(80), None), Some(22)),
      None
    );
    assert_eq!(
      local_record(HostId(1), "127.0.0.1", (Some(0), None), Some(22))
        .map(|record| (record.http, record.ssh.map(|ssh| ssh.port))),
      Some((None, Some(22)))
    );
    assert_eq!(
      local_record(HostId(1), "127.0.0.1", (None, None), None),
      None
    );
  }

  /// A-925 (main 01a20182's condition 2): an HTTPS edge is announced at a host only where its certificate names that
  /// host. Do: a certificate for `edge.example`; make the local record at `edge.example`, then at `other.example`. Expect
  /// the HTTPS edge at the first, and no edge at the second (no host the certificate names is known).
  #[test]
  fn an_https_edge_is_announced_only_at_a_name_its_certificate_carries() {
    let key = rcgen::KeyPair::generate().expect("a key pair");
    let leaf = rcgen::CertificateParams::new(vec!["edge.example".to_owned()])
      .expect("parameters")
      .self_signed(&key)
      .expect("a certificate");
    let named = local_record(
      HostId(1),
      "edge.example",
      (Some(443), Some(leaf.der().as_ref())),
      None,
    );
    assert_eq!(
      named
        .and_then(|record| record.http)
        .map(|edge| (edge.scheme, edge.host)),
      Some((Scheme::Https, "edge.example".to_owned()))
    );
    assert_eq!(
      local_record(
        HostId(1),
        "other.example",
        (Some(443), Some(leaf.der().as_ref())),
        None
      ),
      None
    );
  }

  /// A-925 (main 01a20182's condition 1: only its own). Do: serve a record naming anchor 7 on a session authenticated as
  /// anchor 8; then one naming 8. Expect the first refused typed (`foreign`, counted) and nothing learned for 7; the
  /// second held and learned for 8. And an owner with no learned edge is the typed refusal naming it.
  #[test]
  fn a_record_naming_another_anchor_is_refused_and_teaches_nothing() {
    let (forged, learned_seven, honest, learned_eight, foreign) =
      crate::daemon::audit_on_shard(|state| {
        let forged = serve_edges(state, HostId(8), &record().encode());
        let learned_seven = state.edges.learned(HostId(7)).cloned();
        let honest = serve_edges(
          state,
          HostId(8),
          &EdgesRecord {
            anchor: HostId(8),
            ..record()
          }
          .encode(),
        );
        let learned_eight = state.edges.learned(HostId(8)).cloned();
        (
          forged,
          learned_seven,
          honest,
          learned_eight,
          state.refusals.get("fleet.edges.refused.foreign").copied(),
        )
      });
    assert!(
      forged.is_empty() && learned_seven.is_none(),
      "nothing learned for 7"
    );
    assert_eq!(foreign, Some(1), "the forgery counted");
    assert_eq!(honest, vec![HELD]);
    assert_eq!(learned_eight.map(|record| record.anchor), Some(HostId(8)));
    let refused = crate::daemon::audit_on_shard(|state| owner_edge(state, HostId(99), false));
    assert_eq!(refused, Err(NoEdge { member: HostId(99) }));
  }
}

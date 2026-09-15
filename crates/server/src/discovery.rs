//! Authenticated, bounded fleet enrollment (§4.8, §4.13, R8). Seeds are dial addresses, not a
//! complete membership list. Operator-issued TLS certificates authorize a region and failure
//! domain through the signed DNS SAN `r<region>.d<domain>.<fleet name>` (RFC 5280 §4.2.1.6).
//! A peer exchanges one candidate at a time. Every candidate's issuer, expiry and signed scope
//! are checked before dialing; the pinned TLS session proves possession before membership moves.
//! Raft alone admits voters. Discovery never initializes or resets a consensus group.

use std::collections::BTreeMap;

use rustls::pki_types::{CertificateDer, ServerName};
use slates_db::HostId;
use slates_db::register::RegionId;
use slates_wire::Wire;

use crate::deploy::{NodeAddress, host_id_of_certificate, member_id};
use crate::fleet::{FLEET_FRAME_CAP, FleetPeer};
use crate::state::ShardState;

/// Format: the enrollment exchange follows record stream kinds 1–12.
pub(crate) const STREAM: u64 = 13;
/// Derived: an announcement carries one certificate and its routing metadata inside one frame.
const ANNOUNCEMENT_BYTES: usize = FLEET_FRAME_CAP;
/// Derived: request metadata plus one announcement; a response carries two plus its cursor.
const MESSAGE_BYTES: usize = ANNOUNCEMENT_BYTES * 3;

/// An operator-authorized identity and its current advertised endpoints. Addresses remain hints:
/// each connection authenticates the exact certificate before accepting protocol state.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub(crate) struct Announcement {
  anchor: HostId,
  certificate: Vec<u8>,
  probe: String,
  record: String,
  region: u64,
  domain: u64,
}

#[derive(Wire)]
struct Request {
  announce: Option<Announcement>,
  after: Option<HostId>,
  known: u64,
}

#[derive(Wire)]
struct Reply {
  local: Option<Announcement>,
  candidate: Option<Announcement>,
  generation: u64,
}

/// The closed enrollment refusal taxonomy; the fleet reports each variant separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
  Malformed,
  Untrusted,
  Scope,
  Capacity,
  Identity,
  Publication,
}

impl Refusal {
  pub(crate) fn counter(self) -> &'static str {
    match self {
      Self::Malformed => "fleet.enrollment_malformed",
      Self::Untrusted => "fleet.enrollment_untrusted",
      Self::Scope => "fleet.enrollment_scope",
      Self::Capacity => "fleet.enrollment_capacity",
      Self::Identity => "fleet.enrollment_identity",
      Self::Publication => "fleet.enrollment_publication",
    }
  }
}

/// Shard-owned enrollment authority and cursor table. Its bound is the fleet's derived task
/// capacity, so accepting a certificate cannot create an unbounded population of peer tasks.
pub(crate) struct Discovery {
  name: String,
  roots: Vec<CertificateDer<'static>>,
  pins: BTreeMap<HostId, CertificateDer<'static>>,
  scopes: BTreeMap<HostId, (u64, u64)>,
  local: Announcement,
  records: BTreeMap<HostId, Announcement>,
  capacity: usize,
  generation: u64,
}

impl Discovery {
  pub(crate) fn new(
    state: &ShardState,
    name: String,
    roots: Vec<CertificateDer<'static>>,
    certificate: CertificateDer<'static>,
    probe: NodeAddress,
    record_port: u16,
    peers: &[FleetPeer],
  ) -> Result<Self, Refusal> {
    let seed = member_id(state.origin_anchor, 0);
    let fleet = state.config.fleet.as_ref();
    let local = Announcement {
      anchor: state.origin_anchor,
      certificate: certificate.as_ref().to_vec(),
      record: probe.with_port(record_port).to_string(),
      probe: probe.to_string(),
      region: fleet
        .and_then(|fleet| fleet.regions.get(&seed))
        .map_or(0, |region| region.0),
      domain: fleet
        .and_then(|fleet| fleet.domains.get(&seed))
        .copied()
        .unwrap_or(state.origin_anchor.0),
    };
    check_shape(&local)?;
    let mut pins: BTreeMap<_, _> = peers
      .iter()
      .map(|peer| (peer.anchor, peer.certificate.clone()))
      .collect();
    pins.insert(local.anchor, certificate);
    let scopes = peers
      .iter()
      .map(|peer| {
        (
          peer.anchor,
          (
            fleet
              .and_then(|fleet| fleet.regions.get(&peer.host))
              .map_or(0, |region| region.0),
            fleet
              .and_then(|fleet| fleet.domains.get(&peer.host))
              .copied()
              .unwrap_or(peer.anchor.0),
          ),
        )
      })
      .chain(std::iter::once((
        local.anchor,
        (local.region, local.domain),
      )))
      .collect();
    Ok(Self {
      name,
      roots,
      pins,
      scopes,
      local,
      records: BTreeMap::new(),
      capacity: state.config.fleet_peer_capacity,
      generation: 1,
    })
  }

  pub(crate) fn recognizes(&self, certificate: &CertificateDer<'static>) -> Option<HostId> {
    self
      .pins
      .iter()
      .find(|(_, pinned)| *pinned == certificate)
      .map(|(anchor, _)| *anchor)
      .or_else(|| {
        self
          .records
          .values()
          .find(|record| record.certificate == certificate.as_ref())
          .map(|record| record.anchor)
      })
  }

  pub(crate) fn address(
    &self,
    certificate: &CertificateDer<'static>,
    plane: crate::deploy::Plane,
  ) -> Option<NodeAddress> {
    let record = self
      .records
      .values()
      .find(|record| record.certificate == certificate.as_ref())?;
    NodeAddress::parse(match plane {
      crate::deploy::Plane::Probe => &record.probe,
      crate::deploy::Plane::Record => &record.record,
    })
    .ok()
  }

  fn validate(&self, announce: &Announcement) -> Result<(), Refusal> {
    check_shape(announce)?;
    let certificate = CertificateDer::from(announce.certificate.clone());
    if let Some(pinned) = self.pins.get(&announce.anchor) {
      if pinned != &certificate {
        return Err(Refusal::Identity);
      }
      if self.scopes.get(&announce.anchor) != Some(&(announce.region, announce.domain)) {
        return Err(Refusal::Scope);
      }
      return Ok(());
    }
    if host_id_of_certificate(&certificate) != announce.anchor {
      return Err(Refusal::Identity);
    }
    slates_transport::handshake::verify_enrolled_certificate(&certificate, &self.roots)
      .map_err(|_| Refusal::Untrusted)?;
    let parsed =
      rustls::server::ParsedCertificate::try_from(&certificate).map_err(|_| Refusal::Malformed)?;
    let scope = scope_name(&self.name, announce.region, announce.domain);
    let name = ServerName::try_from(scope).map_err(|_| Refusal::Scope)?;
    rustls::client::verify_server_name(&parsed, &name).map_err(|_| Refusal::Scope)?;
    Ok(())
  }

  fn insert(&mut self, announce: Announcement, direct: bool) -> Result<bool, Refusal> {
    self.validate(&announce)?;
    if announce.anchor == self.local.anchor {
      return if announce == self.local {
        Ok(false)
      } else {
        Err(Refusal::Identity)
      };
    }
    if let Some(previous) = self.records.get(&announce.anchor) {
      if previous == &announce || !direct {
        return Ok(false);
      }
      if previous.certificate != announce.certificate
        || previous.region != announce.region
        || previous.domain != announce.domain
      {
        return Err(Refusal::Scope);
      }
    } else {
      let enrolled = self.pins.len().saturating_sub(1)
        + self
          .records
          .keys()
          .filter(|anchor| !self.pins.contains_key(anchor))
          .count();
      if enrolled >= self.capacity && !self.pins.contains_key(&announce.anchor) {
        return Err(Refusal::Capacity);
      }
    }
    self.generation = self.generation.checked_add(1).ok_or(Refusal::Capacity)?;
    self.records.insert(announce.anchor, announce);
    Ok(true)
  }
}

/// The exact signed certificate name an enrollment issuer adds alongside the fleet TLS name.
pub fn scope_name(fleet: &str, region: u64, domain: u64) -> String {
  format!("r{region}.d{domain}.{fleet}")
}

fn check_shape(announce: &Announcement) -> Result<(), Refusal> {
  if announce.to_bytes().len() > ANNOUNCEMENT_BYTES {
    return Err(Refusal::Capacity);
  }
  for address in [&announce.probe, &announce.record] {
    let parsed = NodeAddress::parse(address).map_err(|_| Refusal::Malformed)?;
    if parsed.port() == 0 {
      return Err(Refusal::Malformed);
    }
  }
  Ok(())
}

fn peer_of(announce: &Announcement) -> Result<FleetPeer, Refusal> {
  Ok(FleetPeer {
    anchor: announce.anchor,
    host: member_id(announce.anchor, 0),
    address: NodeAddress::parse(&announce.probe).map_err(|_| Refusal::Malformed)?,
    record_address: NodeAddress::parse(&announce.record).map_err(|_| Refusal::Malformed)?,
    certificate: CertificateDer::from(announce.certificate.clone()),
  })
}

/// Adds one checked candidate. Voting authority is unchanged; only the two bounded dial tasks
/// become eligible to run. Persist the address before acknowledging enrollment.
fn admit(
  state: &mut ShardState,
  announce: Announcement,
  direct: bool,
) -> Result<Option<FleetPeer>, Refusal> {
  let peer = peer_of(&announce)?;
  let discovery = state.discovery.as_mut().ok_or(Refusal::Untrusted)?;
  let already_dialed = discovery.pins.contains_key(&announce.anchor)
    || discovery.records.contains_key(&announce.anchor);
  if !discovery.insert(announce.clone(), direct)? {
    return Ok(None);
  }
  state.enrolled = discovery.records.values().cloned().collect();
  install_scope(state, &announce);
  crate::retention::publish_authorization(state).map_err(|_| Refusal::Publication)?;
  Ok((!already_dialed).then_some(peer))
}

fn install_scope(state: &mut ShardState, announce: &Announcement) {
  let seed = member_id(announce.anchor, 0);
  state
    .node_regions
    .entry(seed)
    .or_insert(RegionId(announce.region));
  if let Some(fleet) = state.config.fleet.as_mut() {
    // Manifest declarations remain the authority for explicitly pinned seeds.
    fleet.domains.entry(seed).or_insert(announce.domain);
    fleet
      .regions
      .entry(seed)
      .or_insert(RegionId(announce.region));
  }
}

pub(crate) fn restore(state: &mut ShardState) -> Result<Vec<FleetPeer>, Refusal> {
  // Revalidation does not publish: a crash at any point must leave the entire retained
  // roster available to the next start, including peers this start has not examined yet.
  let discovery = state.discovery.as_mut().ok_or(Refusal::Untrusted)?;
  let mut peers = Vec::new();
  for announce in &state.enrolled {
    let peer = peer_of(announce)?;
    if discovery.insert(announce.clone(), false)? && !discovery.pins.contains_key(&announce.anchor)
    {
      peers.push(peer);
    }
  }
  for announce in state.enrolled.clone() {
    install_scope(state, &announce);
  }
  Ok(peers)
}

pub(crate) fn serve(
  state: &mut ShardState,
  certificate: &CertificateDer<'static>,
  bytes: &[u8],
) -> Result<(Vec<u8>, Option<FleetPeer>), Refusal> {
  if bytes.len() > MESSAGE_BYTES {
    return Err(Refusal::Capacity);
  }
  let request = Request::from_bytes(bytes).map_err(|_| Refusal::Malformed)?;
  let first = request.announce.is_some();
  let peer = if let Some(announce) = request.announce {
    if announce.certificate != certificate.as_ref() {
      return Err(Refusal::Identity);
    }
    admit(state, announce, true)?
  } else {
    None
  };
  let discovery = state.discovery.as_ref().ok_or(Refusal::Untrusted)?;
  if discovery.recognizes(certificate).is_none() {
    return Err(Refusal::Untrusted);
  }
  let candidate = if request.known == discovery.generation {
    None
  } else {
    discovery
      .records
      .iter()
      .find(|(anchor, _)| request.after.is_none_or(|after| **anchor > after))
      .map(|(_, record)| record.clone())
  };
  Ok((
    Reply {
      local: first.then(|| discovery.local.clone()),
      candidate,
      generation: discovery.generation,
    }
    .to_bytes(),
    peer,
  ))
}

/// One bounded page per period over an already authenticated record session. A completed sweep
/// sends just a generation check; a roster change starts a fresh sweep on the next period.
#[derive(Default)]
pub(crate) struct Cursor {
  announced: bool,
  after: Option<HostId>,
  generation: u64,
}

impl Cursor {
  pub(crate) fn request(&self, state: &ShardState) -> Option<Vec<u8>> {
    let discovery = state.discovery.as_ref()?;
    Some(
      Request {
        announce: (!self.announced).then(|| discovery.local.clone()),
        after: self.after,
        known: if self.after.is_none() {
          self.generation
        } else {
          0
        },
      }
      .to_bytes(),
    )
  }

  pub(crate) fn receive(
    &mut self,
    state: &mut ShardState,
    bytes: &[u8],
    expected: HostId,
  ) -> Result<Vec<FleetPeer>, Refusal> {
    if bytes.len() > MESSAGE_BYTES {
      return Err(Refusal::Capacity);
    }
    let reply = Reply::from_bytes(bytes).map_err(|_| Refusal::Malformed)?;
    let mut peers = Vec::new();
    if let Some(local) = reply.local {
      if local.anchor != expected {
        return Err(Refusal::Identity);
      }
      if let Some(peer) = admit(state, local, true)? {
        peers.push(peer);
      }
    } else if !self.announced {
      return Err(Refusal::Identity);
    }
    if let Some(candidate) = reply.candidate {
      let restart = self.after.is_some() && self.generation != reply.generation;
      self.after = if restart {
        None
      } else {
        Some(candidate.anchor)
      };
      self.generation = if restart { 0 } else { reply.generation };
      if let Some(peer) = admit(state, candidate, false)? {
        peers.push(peer);
      }
    } else {
      self.after = None;
      self.generation = reply.generation;
    }
    self.announced = true;
    Ok(peers)
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::panic)]
  use super::*;

  fn issued(issuer: &rcgen::Certificate, key: &rcgen::KeyPair, domain: u64) -> Announcement {
    let node_key = rcgen::KeyPair::generate().unwrap();
    let cert =
      rcgen::CertificateParams::new(vec!["fleet".to_owned(), scope_name("fleet", 0, domain)])
        .unwrap()
        .signed_by(&node_key, issuer, key)
        .unwrap();
    Announcement {
      anchor: host_id_of_certificate(cert.der()),
      certificate: cert.der().as_ref().to_vec(),
      probe: "127.0.0.1:7000".to_owned(),
      record: "127.0.0.1:7001".to_owned(),
      region: 0,
      domain,
    }
  }

  fn authority() -> (rcgen::Certificate, rcgen::KeyPair) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["fleet".to_owned()]).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    (params.self_signed(&key).unwrap(), key)
  }

  fn discovery(issuer: &rcgen::Certificate, key: &rcgen::KeyPair) -> Discovery {
    Discovery {
      name: "fleet".to_owned(),
      roots: vec![issuer.der().clone()],
      pins: BTreeMap::new(),
      scopes: BTreeMap::new(),
      local: issued(issuer, key, 0),
      records: BTreeMap::new(),
      capacity: 1,
      generation: 1,
    }
  }

  /// AC-8.18 / T-8.20: a valid issuer permits enrollment; altered region, domain, anchor and
  /// an unrelated issuer are refused before a candidate can reach membership.
  #[test]
  fn enrollment_refuses_forged_scope_identity_and_issuer() {
    let (issuer, key) = authority();
    let discovery = discovery(&issuer, &key);
    let announce = issued(&issuer, &key, 1);
    assert_eq!(discovery.validate(&announce), Ok(()));
    let mut forged = announce.clone();
    forged.region += 1;
    assert_eq!(discovery.validate(&forged), Err(Refusal::Scope));
    forged = announce.clone();
    forged.domain += 1;
    assert_eq!(discovery.validate(&forged), Err(Refusal::Scope));
    forged = announce.clone();
    forged.anchor.0 ^= 1;
    assert_eq!(discovery.validate(&forged), Err(Refusal::Identity));
    let (other, other_key) = authority();
    assert_eq!(
      discovery.validate(&issued(&other, &other_key, 1)),
      Err(Refusal::Untrusted)
    );
    forged = announce;
    forged.probe = "broken".to_owned();
    assert_eq!(discovery.validate(&forged), Err(Refusal::Malformed));
  }

  /// AC-8.18 / T-8.20: one admitted peer exhausts a one-peer task budget. A direct TLS
  /// announcement may change its address; a stale relayed address cannot overwrite it.
  #[test]
  fn enrollment_bounds_candidates_and_only_direct_contact_updates_addresses() {
    let (issuer, key) = authority();
    let mut discovery = discovery(&issuer, &key);
    let announce = issued(&issuer, &key, 1);
    let certificate = CertificateDer::from(announce.certificate.clone());
    assert_eq!(discovery.insert(announce.clone(), true), Ok(true));
    assert_eq!(
      discovery.insert(issued(&issuer, &key, 2), true),
      Err(Refusal::Capacity)
    );
    let mut moved = announce.clone();
    moved.probe = "127.0.0.2:7000".to_owned();
    assert_eq!(discovery.insert(moved.clone(), true), Ok(true));
    assert_eq!(discovery.insert(announce, false), Ok(false));
    assert_eq!(
      discovery.address(&certificate, crate::deploy::Plane::Probe),
      Some(NodeAddress::parse(&moved.probe).unwrap())
    );
  }

  /// AC-8.18 / T-8.20: refuse warm revalidation at the peer bound, then restart with
  /// enough capacity. Both previously retained peers must remain discoverable.
  #[test]
  fn refused_revalidation_keeps_the_complete_roster_for_the_next_restart() {
    crate::daemon::audit_on_shard(|state| {
      let (issuer, key) = authority();
      let peers = vec![issued(&issuer, &key, 1), issued(&issuer, &key, 2)];
      state.enrolled = peers.clone();
      crate::retention::publish_authorization(state).unwrap();
      state.discovery = Some(discovery(&issuer, &key));
      assert!(matches!(restore(state), Err(Refusal::Capacity)));

      crate::retention::load(&state.segment)
        .unwrap()
        .unwrap()
        .restore(state)
        .unwrap();
      let mut next = discovery(&issuer, &key);
      next.capacity = peers.len();
      state.discovery = Some(next);
      let recovered = restore(state).unwrap();
      assert_eq!(
        recovered.len(),
        peers.len(),
        "a refused start must not erase unexamined peers"
      );
      for peer in peers {
        let certificate = CertificateDer::from(peer.certificate);
        assert_eq!(
          state
            .discovery
            .as_ref()
            .unwrap()
            .address(&certificate, crate::deploy::Plane::Record),
          Some(NodeAddress::parse(&peer.record).unwrap()),
        );
      }
    });
  }
}

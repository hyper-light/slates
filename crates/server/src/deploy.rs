//! Fleet deployment from **one shared manifest** (§2.6 boot step 6; §4.8 "Membership" and "TLS 1.3 via
//! rustls with certificates provisioned by the operator"). An operator writes a single manifest naming the
//! fleet, its fault tolerance `f`, and every node — its name, its advertised address, and its certificate —
//! and starts every node from that same file (`slates daemon --fleet PATH --node NAME`). From it, every
//! node computes the same facts, so no node has to be told anything about the others that the others were
//! not told about it:
//!
//! - **Member ids from certificates.** A node's fleet id ([`HostId`]) is the leading eight bytes of the
//!   BLAKE3 hash of its DER certificate ([`host_id_of_certificate`]). The certificate is what every peer
//!   pins for the mutual-TLS session, so it is the one fact about a node every other node holds, and the
//!   id follows from it without a registry (D-14: ids route to owners, no global catalog). Two nodes
//!   sharing a certificate would be one member; the plan refuses that (`DuplicateCertificate`).
//! - **The socket layout.** The membership loop binds one serve socket per peer per plane
//!   ([`FleetPeer`]: `Endpoint::accept` pins one peer per socket, until the connection-ID demux that would
//!   multiplex them is built). Rather than have the operator write `N·(N−1)` address pairs, each node
//!   advertises one base port and owns the block of `2N` ports from it: it serves the node at manifest
//!   index `j` on `base + 2j` (probes) and `base + 2j + 1` (records), and the node at index `j` dials it
//!   there ([`serve_port`]). Node `i`'s own pair (`2i`, `2i + 1`) is unused — a fixed layout with one
//!   formula beats one with a gap to compute. A block that would run past the port range is refused
//!   (`PortBlockOverflows`), never wrapped.
//!
//! This module is pure: it takes the manifest as values (the certificate and key bytes already read) and
//! returns the [`FleetMembership`] the placement authority is built over and the [`FleetTransport`] the
//! membership loop drives. Reading the manifest and the DER files from the operator's disk is the
//! command's job (`slates-cli`, a reader of host paths under R1), so this crate still names no host path.
//! A laptop has no manifest and no fleet; `f = 0` solo is the same code path (R8).

// Re-exported so the command that reads the operator's DER files builds the manifest's values without
// naming the TLS crate itself.
pub use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use slates_db::HostId;
use slates_db::register::Quorum;
use slates_rt::tcp::SocketAddrV4;
use slates_transport::handshake::Identity;

use crate::config::FleetMembership;
use crate::fleet::{FleetPeer, FleetTransport};

/// One node of the shared manifest: what every node knows about every node.
#[derive(Clone, Debug)]
pub struct FleetNodeEntry {
  /// The operator's name for the node — what `--node NAME` selects. Unique within the manifest.
  pub node: String,
  /// The node's advertised address: the IP its peers dial, and the **base** of its port block (see the
  /// module doc: the node owns `2N` ports from here).
  pub address: SocketAddrV4,
  /// The node's operator-provisioned certificate (DER): what its peers pin, and what its member id is
  /// derived from.
  pub certificate: CertificateDer<'static>,
}

/// The shared manifest, as values: the fleet's TLS name, its fault tolerance and its nodes in manifest
/// order (the order fixes the port layout, so every node must read the same manifest).
#[derive(Clone, Debug)]
pub struct FleetManifest {
  /// The TLS server name every node's certificate carries and every peer verifies the session against.
  pub name: String,
  /// The fault tolerance `f`: a write commits at `f + 1` acknowledgements of `2f + 1` candidates (§4.8).
  pub quorum: Quorum,
  /// Every node, in manifest order.
  pub nodes: Vec<FleetNodeEntry>,
}

/// A node's deployment plan: the membership policy its placement authority is built over and the
/// transport material its membership loop drives.
pub struct FleetPlan {
  /// The quorum, this node's member id, and its peers' ids — the config's fleet membership.
  pub membership: FleetMembership,
  /// This node's identity, the fleet's TLS name, and each peer with its dial and serve addresses.
  pub transport: FleetTransport,
}

/// Which of a node's two per-peer serve sockets: the SWIM probe plane or the register record plane (the
/// two ride separate sockets because their wire formats are not distinguished by content on a shared
/// stream — the connection-ID demux that would multiplex them is owed, `crate::fleet`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plane {
  /// SWIM probes and their acknowledgements.
  Probe,
  /// Register record commits, promotions and content exchanges.
  Record,
}

/// Why a manifest yields no plan. Each names what the operator must fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeployError {
  /// `--node NAME` names no entry of the manifest.
  UnknownNode {
    /// The name given.
    node: String,
  },
  /// Two entries carry one name, so `--node` could select either.
  DuplicateNode {
    /// The repeated name.
    node: String,
  },
  /// Two entries carry one certificate, so they would be one member (one id, one pinned peer).
  DuplicateCertificate {
    /// The first entry with it.
    first: String,
    /// The second.
    second: String,
  },
  /// A node's port block, `2N` ports from its base, runs past the port range.
  PortBlockOverflows {
    /// The node.
    node: String,
    /// Its base port.
    base: u16,
    /// The ports its block needs.
    needed: u16,
  },
  /// Fewer nodes than a commit needs acknowledgements (`f + 1`): nothing could ever place.
  QuorumUnreachable {
    /// The nodes listed.
    nodes: usize,
    /// The fault tolerance asked for.
    f: u32,
  },
  /// The manifest lists no node at all.
  NoNodes,
  /// This node's key and certificate, or a peer's pinned certificate, cannot be used by the TLS stack
  /// (a key shape the provider does not take, a key that does not match the certificate, a certificate
  /// that cannot be a trust anchor). Checked once at boot so it is a refusal by name here, not a mesh
  /// that never forms with only `fleet.accept` refusal counts in `status` to show for it.
  Identity {
    /// The node whose material was checked.
    node: String,
    /// The TLS stack's reason.
    reason: String,
  },
}

impl std::fmt::Display for DeployError {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::UnknownNode { node } => write!(out, "the manifest has no node named `{node}`"),
      Self::DuplicateNode { node } => write!(out, "the manifest names `{node}` twice"),
      Self::DuplicateCertificate { first, second } => write!(
        out,
        "nodes `{first}` and `{second}` carry the same certificate; they would be one member"
      ),
      Self::PortBlockOverflows { node, base, needed } => write!(
        out,
        "node `{node}` needs {needed} ports from its base {base}, past the end of the port range"
      ),
      Self::QuorumUnreachable { nodes, f } => write!(
        out,
        "{nodes} node(s) can never reach f + 1 = {} acknowledgements at f = {f}",
        f.saturating_add(1)
      ),
      Self::NoNodes => out.write_str("the manifest lists no nodes"),
      Self::Identity { node, reason } => write!(
        out,
        "node `{node}`'s key, certificate or pins cannot be used for TLS: {reason}"
      ),
    }
  }
}

impl std::error::Error for DeployError {}

/// Format: a node serves each peer on two ports (one per plane), so a node's block is two ports per
/// manifest entry (its own pair unused, see the module doc).
const PORTS_PER_ENTRY: u16 = 2;

/// A node's member id: the leading eight bytes of the BLAKE3 hash of its DER certificate, little-endian
/// (the same cut `daemon::host_id_of` takes of the machine identity's hash on a laptop). Every node of a
/// fleet derives every member's id from the certificate it pins, so the ids agree fleet-wide without a
/// registry.
pub fn host_id_of_certificate(certificate: &CertificateDer<'_>) -> HostId {
  let hash = blake3::hash(certificate.as_ref());
  let bytes = hash.as_bytes();
  HostId(u64::from_le_bytes([
    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
  ]))
}

/// The port a node with port block base `base` serves manifest entry `peer_index` on, for `plane`
/// (`base + 2·index`, `+ 1` for the record plane), or the overflow when the block runs past `u16`.
pub fn serve_port(base: u16, peer_index: usize, plane: Plane) -> Option<u16> {
  let index = u16::try_from(peer_index).ok()?;
  let offset = index.checked_mul(PORTS_PER_ENTRY)?;
  let plane_offset = match plane {
    Plane::Probe => 0,
    Plane::Record => 1,
  };
  base.checked_add(offset)?.checked_add(plane_offset)
}

/// The ports a manifest of `entries` nodes needs per node: its whole block.
fn block_len(entries: usize) -> Option<u16> {
  u16::try_from(entries).ok()?.checked_mul(PORTS_PER_ENTRY)
}

/// Checks what must hold for any node's plan: at least one node, a quorum some set of nodes can reach,
/// unique names, unique certificates, and every block inside the port range. Returns the index of `node`.
fn validate(manifest: &FleetManifest, node: &str) -> Result<usize, DeployError> {
  if manifest.nodes.is_empty() {
    return Err(DeployError::NoNodes);
  }
  let needed_acks = usize::try_from(manifest.quorum.f)
    .ok()
    .and_then(|f| f.checked_add(1))
    .unwrap_or(usize::MAX);
  if manifest.nodes.len() < needed_acks {
    return Err(DeployError::QuorumUnreachable {
      nodes: manifest.nodes.len(),
      f: manifest.quorum.f,
    });
  }
  let block = block_len(manifest.nodes.len());
  for (index, entry) in manifest.nodes.iter().enumerate() {
    for other in &manifest.nodes[..index] {
      if other.node == entry.node {
        return Err(DeployError::DuplicateNode {
          node: entry.node.clone(),
        });
      }
      if other.certificate == entry.certificate {
        return Err(DeployError::DuplicateCertificate {
          first: other.node.clone(),
          second: entry.node.clone(),
        });
      }
    }
    // The block's last port must exist: `base + 2N − 1`.
    let fits = block
      .and_then(|len| len.checked_sub(1))
      .and_then(|last| entry.address.port().checked_add(last))
      .is_some();
    if !fits {
      return Err(DeployError::PortBlockOverflows {
        node: entry.node.clone(),
        base: entry.address.port(),
        needed: block.unwrap_or(u16::MAX),
      });
    }
  }
  manifest
    .nodes
    .iter()
    .position(|entry| entry.node == node)
    .ok_or_else(|| DeployError::UnknownNode {
      node: node.to_owned(),
    })
}

/// The serve address of manifest entry `server` for entry `client` on `plane`: the server's IP, and the
/// port its block assigns the client. `validate` proved every block fits, so the overflow arm is unreachable
/// after it; it is kept typed rather than panicked (banned item 6).
fn serve_address(server: &FleetNodeEntry, client: usize, plane: Plane) -> Option<SocketAddrV4> {
  serve_port(server.address.port(), client, plane)
    .map(|port| SocketAddrV4::new(*server.address.ip(), port))
}

/// Checks, once, that the TLS stack can build this node's server side from its identity with every
/// peer's certificate pinned — the same construction the membership loop's `Endpoint::accept` makes per
/// peer at boot — so a key the provider rejects, a key that does not match the certificate, or a pin
/// that cannot be a trust anchor is refused here by name. A solo node has no pins; its own certificate
/// stands in (it is what a peer would pin), since the verifier needs one anchor to build at all.
fn check_identity(
  node: &str,
  identity: &Identity,
  own: &CertificateDer<'static>,
  pins: &[CertificateDer<'static>],
) -> Result<(), DeployError> {
  let anchors: &[CertificateDer<'static>] = if pins.is_empty() {
    std::slice::from_ref(own)
  } else {
    pins
  };
  slates_transport::handshake::server_config(identity, anchors)
    .map(|_| ())
    .map_err(|e| DeployError::Identity {
      node: node.to_owned(),
      reason: e.to_string(),
    })
}

/// This node's plan from the shared manifest: `node` selects its entry, `key` is its private key (the
/// one secret the manifest does not carry — each node reads only its own). Every peer's dial addresses
/// are the peer's serve ports for this node's index, and this node's serve binds are its own block's
/// ports for each peer's index, so the two sides of every session agree by construction. The identity
/// and the pins are checked against the TLS stack before the plan is returned.
pub fn plan(
  manifest: &FleetManifest,
  node: &str,
  key: PrivateKeyDer<'static>,
) -> Result<FleetPlan, DeployError> {
  let this = validate(manifest, node)?;
  let entry = &manifest.nodes[this];
  let host = host_id_of_certificate(&entry.certificate);
  let mut peers = Vec::with_capacity(manifest.nodes.len().saturating_sub(1));
  let mut peer_hosts = Vec::with_capacity(peers.capacity());
  for (index, peer) in manifest.nodes.iter().enumerate() {
    if index == this {
      continue;
    }
    let (Some(address), Some(record_address), Some(probe_bind), Some(record_bind)) = (
      serve_address(peer, this, Plane::Probe),
      serve_address(peer, this, Plane::Record),
      serve_address(entry, index, Plane::Probe),
      serve_address(entry, index, Plane::Record),
    ) else {
      return Err(DeployError::PortBlockOverflows {
        node: peer.node.clone(),
        base: peer.address.port(),
        needed: block_len(manifest.nodes.len()).unwrap_or(u16::MAX),
      });
    };
    let peer_host = host_id_of_certificate(&peer.certificate);
    peer_hosts.push(peer_host);
    peers.push(FleetPeer {
      host: peer_host,
      address,
      record_address,
      probe_bind,
      record_bind,
      certificate: peer.certificate.clone(),
    });
  }
  let identity = Identity::from_der(entry.certificate.clone(), key);
  let pins: Vec<CertificateDer<'static>> = peers.iter().map(|p| p.certificate.clone()).collect();
  check_identity(&entry.node, &identity, &entry.certificate, &pins)?;
  Ok(FleetPlan {
    membership: FleetMembership {
      quorum: manifest.quorum,
      peers: peer_hosts,
      host,
    },
    transport: FleetTransport {
      identity,
      name: manifest.name.clone(),
      peers,
    },
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_rt::tcp::Ipv4Addr;

  /// Shape: the fleet's TLS name in these tests; every minted certificate carries it.
  const NAME: &str = "slates-fleet";

  /// A self-signed identity minted with `rcgen`, as the fleet tests mint them: the certificate (what a
  /// manifest carries and peers pin) and the key (what only the node itself reads).
  fn mint() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let key = rcgen::KeyPair::generate().expect("a key pair");
    let cert = rcgen::CertificateParams::new(vec![NAME.to_owned()])
      .expect("certificate params")
      .self_signed(&key)
      .expect("a self-signed certificate");
    (
      cert.der().clone(),
      PrivateKeyDer::try_from(key.serialize_der()).expect("a PKCS#8 key"),
    )
  }

  fn entry(node: &str, port: u16, certificate: CertificateDer<'static>) -> FleetNodeEntry {
    FleetNodeEntry {
      node: node.to_owned(),
      address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
      certificate,
    }
  }

  /// A three-node manifest with minted identities; each node's key beside it, in manifest order.
  fn manifest() -> (FleetManifest, Vec<PrivateKeyDer<'static>>) {
    let minted: Vec<(CertificateDer<'static>, PrivateKeyDer<'static>)> =
      (0..3).map(|_| mint()).collect();
    let mut keys = Vec::new();
    let mut certs = Vec::new();
    for (cert, key) in minted {
      certs.push(cert);
      keys.push(key);
    }
    let manifest = FleetManifest {
      name: NAME.to_owned(),
      quorum: Quorum { f: 1 },
      nodes: vec![
        entry("a", 40_000, certs[0].clone()),
        entry("b", 41_000, certs[1].clone()),
        entry("c", 42_000, certs[2].clone()),
      ],
    };
    (manifest, keys)
  }

  /// The key of the node named `node` in a manifest built by [`manifest`].
  fn key_of(keys: &[PrivateKeyDer<'static>], node: &str) -> PrivateKeyDer<'static> {
    let index = ["a", "b", "c"]
      .iter()
      .position(|n| *n == node)
      .expect("a known node");
    keys[index].clone_key()
  }

  /// The manifest index of the node `peer` names (by its certificate-derived id).
  fn index_of(manifest: &FleetManifest, host: HostId) -> usize {
    manifest
      .nodes
      .iter()
      .position(|n| host_id_of_certificate(&n.certificate) == host)
      .expect("the host is a manifest node")
  }

  /// Both planes of one ordered pair agree: my dial address for the peer is the peer's serve bind for
  /// me, and the peer's dial address for me is my serve bind for it.
  fn assert_pair_agrees(mine: &FleetPeer, theirs: &FleetPeer) {
    assert_eq!(mine.address, theirs.probe_bind, "probe: dial == their bind");
    assert_eq!(
      mine.record_address, theirs.record_bind,
      "record: dial == their bind"
    );
    assert_eq!(
      theirs.address, mine.probe_bind,
      "probe: their dial == my bind"
    );
    assert_eq!(
      theirs.record_address, mine.record_bind,
      "record: their dial == my bind"
    );
  }

  /// The two sides of every session agree: b's dial address for a is a's serve bind for b, on both
  /// planes, for every ordered pair — and every peer's id is the certificate-derived one every other node
  /// computes.
  #[test]
  fn every_pair_of_plans_agrees_on_its_sockets_and_ids() {
    let (manifest, keys) = manifest();
    let plans: Vec<FleetPlan> = ["a", "b", "c"]
      .iter()
      .map(|node| plan(&manifest, node, key_of(&keys, node)).expect("a plan"))
      .collect();
    for (i, mine) in plans.iter().enumerate() {
      let my_host = host_id_of_certificate(&manifest.nodes[i].certificate);
      assert_eq!(mine.membership.host, my_host);
      assert_eq!(mine.membership.peers.len(), 2);
      assert!(!mine.membership.peers.contains(&my_host));
      for peer in &mine.transport.peers {
        let j = index_of(&manifest, peer.host);
        assert_ne!(j, i);
        let theirs = plans[j]
          .transport
          .peers
          .iter()
          .find(|p| p.host == my_host)
          .expect("the peer lists this node");
        assert_pair_agrees(peer, theirs);
      }
    }
  }

  /// The layout by number: node a (base 40 000) serves b (index 1) on 40 002/40 003, and b (base 41 000)
  /// serves a (index 0) on 41 000/41 001 — the formula `base + 2·index (+ 1)`.
  #[test]
  fn the_port_layout_is_base_plus_twice_the_index() {
    let (manifest, keys) = manifest();
    let a = plan(&manifest, "a", key_of(&keys, "a")).expect("a plan");
    let b_host = host_id_of_certificate(&manifest.nodes[1].certificate);
    let to_b = a
      .transport
      .peers
      .iter()
      .find(|p| p.host == b_host)
      .expect("b");
    assert_eq!(to_b.probe_bind.port(), 40_002);
    assert_eq!(to_b.record_bind.port(), 40_003);
    assert_eq!(to_b.address.port(), 41_000);
    assert_eq!(to_b.record_address.port(), 41_001);
    assert_eq!(serve_port(u16::MAX - 1, 1, Plane::Record), None);
  }

  /// Each refusal names what the operator must fix.
  #[test]
  fn a_bad_manifest_is_refused_by_name() {
    let (mut duplicate_name, keys) = manifest();
    duplicate_name.nodes[2].node = "a".to_owned();
    assert_eq!(
      plan(&duplicate_name, "b", key_of(&keys, "b")).err(),
      Some(DeployError::DuplicateNode {
        node: "a".to_owned()
      })
    );
    let (mut duplicate_cert, keys) = manifest();
    duplicate_cert.nodes[2].certificate = duplicate_cert.nodes[0].certificate.clone();
    assert_eq!(
      plan(&duplicate_cert, "b", key_of(&keys, "b")).err(),
      Some(DeployError::DuplicateCertificate {
        first: "a".to_owned(),
        second: "c".to_owned()
      })
    );
    let (unknown, keys) = manifest();
    assert_eq!(
      plan(&unknown, "d", key_of(&keys, "a")).err(),
      Some(DeployError::UnknownNode {
        node: "d".to_owned()
      })
    );
    let (mut overflow, keys) = manifest();
    overflow.nodes[1].address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, u16::MAX - 2);
    assert_eq!(
      plan(&overflow, "a", key_of(&keys, "a")).err(),
      Some(DeployError::PortBlockOverflows {
        node: "b".to_owned(),
        base: u16::MAX - 2,
        needed: 6
      })
    );
    let (mut too_few, keys) = manifest();
    too_few.quorum = Quorum { f: 3 };
    assert_eq!(
      plan(&too_few, "a", key_of(&keys, "a")).err(),
      Some(DeployError::QuorumUnreachable { nodes: 3, f: 3 })
    );
    let empty = FleetManifest {
      name: "x".to_owned(),
      quorum: Quorum { f: 0 },
      nodes: Vec::new(),
    };
    assert_eq!(
      plan(&empty, "a", key_of(&keys, "a")).err(),
      Some(DeployError::NoNodes)
    );
  }

  /// Material the TLS stack cannot use is refused at plan time, naming the node: another node's key
  /// (it does not match this node's certificate), and bytes that are not a key at all.
  #[test]
  fn an_unusable_identity_is_refused_at_plan_time() {
    let (manifest, keys) = manifest();
    let wrong_key = plan(&manifest, "a", key_of(&keys, "b"));
    assert!(
      matches!(wrong_key, Err(DeployError::Identity { ref node, .. }) if node == "a"),
      "another node's key is refused: {:?}",
      wrong_key.err()
    );
    let garbage = PrivateKeyDer::try_from(vec![0x30, 0x03, 0x02, 0x01, 0x01]).expect("a DER shape");
    let not_a_key = plan(&manifest, "a", garbage);
    assert!(
      matches!(not_a_key, Err(DeployError::Identity { ref node, .. }) if node == "a"),
      "bytes that are not a key are refused: {:?}",
      not_a_key.err()
    );
  }

  /// A one-node manifest at `f = 0` is the laptop: no peers, the node its own only member (R8).
  #[test]
  fn a_single_node_manifest_is_the_solo_degenerate() {
    let (cert, key) = mint();
    let solo = FleetManifest {
      name: NAME.to_owned(),
      quorum: Quorum { f: 0 },
      nodes: vec![entry("only", 50_000, cert)],
    };
    let plan = plan(&solo, "only", key).expect("a plan");
    assert!(plan.transport.peers.is_empty());
    assert!(plan.membership.peers.is_empty());
    assert_eq!(plan.membership.quorum, Quorum { f: 0 });
  }
}

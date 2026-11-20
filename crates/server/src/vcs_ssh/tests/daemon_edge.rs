//! Tests of the daemon's SSH edge (A-684): which host key it presents (the fleet node's own Ed25519 key, else the one its
//! anchor published, else a lone daemon's own), and whole daemons in-process serving it, read by slates-ssh's client
//! written from the RFCs, which checks the host key's signature over the exchange it computed itself.

use std::io::{Read, Write};
use std::net::TcpStream as ClientStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use slates_anchor::AnchorSegment;
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};
use slates_ssh::entropy::{Entropy, EntropyFailed};
use slates_ssh::hostkey::HostKey;

use super::super::HostSeed;
use super::super::ssh_client::{Client, ClientOptions, put_string};
use super::host_seed;
use crate::config::{EdgeIdentity, VcsSshConfig};
use crate::daemon::{Daemon, SegmentSource, VcsSshEdge};

/// Shape: one shard: the edge's listener and its connections live on the control shard.
const SHARDS: u16 = 1;
/// Shape: how long the client waits for each of the edge's answers: the daemon's liveness budget, four times.
const PATIENCE_NS: u64 = 4 * crate::daemon::LIVENESS_BUDGET_NS;
/// Shape: the seed the tests' anchor publishes, RFC 8032 §7.1 test 1's.
const ANCHORS_SEED: [u8; 32] = [
  0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
  0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];
/// Shape: what the tests' generator hands a lone daemon, RFC 8032 §7.1 test 2's seed.
const DRAWN_SEED: [u8; 32] = [
  0x4c, 0xcd, 0x08, 0x9b, 0x28, 0xff, 0x96, 0xda, 0x9d, 0xb6, 0xc3, 0x46, 0xec, 0x11, 0x4e, 0x0f,
  0x5b, 0x8a, 0x31, 0x9f, 0x35, 0xab, 0xa6, 0x24, 0xda, 0x8c, 0xf6, 0xed, 0x4f, 0xb8, 0xa6, 0xfb,
];

/// The `ssh-ed25519` blob of the public key `public` (RFC 8709 §4), written by the client's own encoder.
fn blob_of_public(public: &[u8]) -> Vec<u8> {
  let mut blob = Vec::new();
  put_string(&mut blob, b"ssh-ed25519");
  put_string(&mut blob, public);
  blob
}

/// The blob of the Ed25519 key of `seed`, its public half computed by AWS-LC.
fn blob(seed: &[u8; 32]) -> Vec<u8> {
  let key = Ed25519KeyPair::from_seed_unchecked(seed).expect("an Ed25519 key");
  blob_of_public(key.public_key().as_ref())
}

/// The blob of the key a chosen seed makes: the one the edge presents with it.
fn presented(seed: &HostSeed) -> Vec<u8> {
  HostKey::ed25519_from_seed(&seed.0)
    .expect("a host key")
    .public_blob()
    .to_vec()
}

/// A node identity whose key is made for `algorithm`, as a fleet's plan hands one in, and its key's public half.
fn identity(algorithm: &'static rcgen::SignatureAlgorithm) -> (EdgeIdentity, Vec<u8>) {
  let key = rcgen::KeyPair::generate_for(algorithm).expect("a key");
  let certificate = rcgen::CertificateParams::new(vec!["slates-fleet".to_owned()])
    .expect("parameters")
    .self_signed(&key)
    .expect("a self-signed certificate");
  (
    EdgeIdentity::new(vec![certificate.der().to_vec()], key.serialize_der()),
    key.public_key_raw().to_vec(),
  )
}

/// A generator that hands out its bytes once, then refuses.
struct Scripted(Option<[u8; 32]>);

impl Entropy for Scripted {
  fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyFailed> {
    match self.0.take() {
      Some(bytes) if out.len() == bytes.len() => {
        out.copy_from_slice(&bytes);
        Ok(())
      }
      _ => Err(EntropyFailed),
    }
  }
}

/// An edge on a loopback port the system picks, presenting `identity`'s key when it has one.
fn edge(identity: Option<EdgeIdentity>) -> VcsSshConfig {
  VcsSshConfig {
    address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
    identity,
  }
}

/// A-684 (main's ruling on the host key). Do: choose the host key for a fleet node whose identity's key is Ed25519, with
/// an anchor's seed and a generator beside it, then for one whose key is P-256. Expect: the node's own key, whatever else
/// is offered; and no key for the P-256 node, refused naming Ed25519: the edge does not start rather than present a key
/// that is not the node's.
#[test]
fn a_fleet_nodes_host_key_is_its_own_ed25519_key_and_no_other() {
  let (ed25519, public) = identity(&rcgen::PKCS_ED25519);
  let nodes = host_seed(
    &edge(Some(ed25519)),
    Some(HostSeed(ANCHORS_SEED)),
    &mut Scripted(Some(DRAWN_SEED)),
  )
  .expect("the node's own key");
  assert_eq!(presented(&nodes), blob_of_public(&public));
  let (p256, _) = identity(&rcgen::PKCS_ECDSA_P256_SHA256);
  let refused = host_seed(&edge(Some(p256)), None, &mut Scripted(Some(DRAWN_SEED)));
  assert!(
    matches!(&refused, Err(why) if why.contains("Ed25519")),
    "{:?}",
    refused.as_ref().err()
  );
}

/// A-684. Do: choose the host key for a daemon under an anchor that published a seed (a generator beside it), for a
/// daemon alone, and for a daemon alone whose generator refuses. Expect: the anchor's key; the generator's key; and a
/// refusal naming the generator.
#[test]
fn any_other_daemons_host_key_is_its_anchors_else_its_own() {
  let anchors = host_seed(
    &edge(None),
    Some(HostSeed(ANCHORS_SEED)),
    &mut Scripted(Some(DRAWN_SEED)),
  )
  .expect("the anchor's key");
  assert_eq!(presented(&anchors), blob(&ANCHORS_SEED));
  let drawn = host_seed(&edge(None), None, &mut Scripted(Some(DRAWN_SEED))).expect("its own key");
  assert_eq!(presented(&drawn), blob(&DRAWN_SEED));
  let none = host_seed(&edge(None), None, &mut Scripted(None));
  assert!(
    matches!(&none, Err(why) if why.contains("generator")),
    "{:?}",
    none.as_ref().err()
  );
}

/// A unique instance name for a test's daemon.
fn instance() -> String {
  static NEXT: AtomicU32 = AtomicU32::new(0);
  format!(
    "vcs-ssh-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, Ordering::Relaxed)
  )
}

/// A daemon of [`SHARDS`] shards started alone (its own segment), whose configuration asks for the SSH edge `edge`.
fn alone(edge: VcsSshConfig) -> Daemon {
  let profile = crate::daemon::test_profile();
  let instance = instance();
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_ssh = Some(edge);
  Daemon::start(&profile, config, SegmentSource::Create { name: instance })
    .expect("the daemon starts")
}

/// The host key blob the SSH edge at loopback `port` signed its first exchange with, as the client written from the
/// RFCs reads it, having checked the signature over the exchange hash it computed itself.
fn host_key_at(port: u16) -> Vec<u8> {
  let mut stream = ClientStream::connect(("127.0.0.1", port)).expect("a connection to the edge");
  stream
    .set_read_timeout(Some(Duration::from_nanos(PATIENCE_NS)))
    .expect("the client's patience");
  let mut client = Client::new(ClientOptions::openssh_like(
    "chacha20-poly1305@openssh.com",
    true,
  ));
  let mut chunk = [0u8; 4096];
  while client.session_id.is_none() || client.server_newkeys == 0 {
    let out = client.take_output();
    if !out.is_empty() {
      stream.write_all(&out).expect("the client's bytes sent");
    }
    let read = stream.read(&mut chunk).expect("the edge's bytes");
    assert!(read > 0, "the edge closed before its first exchange ended");
    client.feed(&chunk[..read]);
  }
  client.host_key.expect("the edge's host key")
}

/// The edge `daemon` started, its fingerprint checked against the key it presents to a client.
fn served(daemon: &Daemon) -> (VcsSshEdge, Vec<u8>) {
  let edge = daemon.vcs_ssh().cloned().expect("the daemon's SSH edge");
  let key = host_key_at(edge.port);
  assert_eq!(
    edge.fingerprint,
    slates_ssh::userauth::fingerprint(&key),
    "the fingerprint the daemon names is the key its edge presents"
  );
  (edge, key)
}

/// A-684. Do: play an anchor that publishes its host key's seed in a segment, as `slates anchor --vcs-ssh` does at its
/// start, and start a daemon over that segment with the SSH edge on loopback; complete a key exchange with its edge.
/// Expect: the edge presents the anchor's key, its signature over the exchange checked by the client, and the daemon
/// names that key's fingerprint, so every daemon under the anchor serves one key a client pins once.
#[test]
fn a_daemon_under_an_anchor_serves_the_anchors_host_key_and_names_its_fingerprint() {
  let profile = crate::daemon::test_profile();
  let instance = instance();
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_ssh = Some(edge(None));
  let mut segment =
    AnchorSegment::create(&instance, &profile.facts.identity, config.geometry).expect("a segment");
  segment
    .publish_ssh_host_seed(&ANCHORS_SEED)
    .expect("the anchor's key published");
  let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).expect("the rendezvous");
  let (handoff, len) = segment.handoff().expect("the segment's handoff");
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Handoff {
      handoff,
      len,
      content: None,
      rendezvous: rendezvous.adoption(),
    },
  )
  .expect("the daemon starts");
  let (_, key) = served(&daemon);
  assert_eq!(key, blob(&ANCHORS_SEED));
}

/// A-684. Do: start a daemon alone with the SSH edge on loopback, and complete a key exchange with its edge twice. Expect:
/// it serves a key of its own, the same on both connections, whose fingerprint it names.
#[test]
fn a_daemon_alone_serves_a_host_key_of_its_own() {
  let daemon = alone(edge(None));
  let (edge, first) = served(&daemon);
  assert_eq!(
    host_key_at(edge.port),
    first,
    "one key for the daemon's life"
  );
}

/// A-684 (main's ruling: a fleet node's edge presents the node's own identity). Do: start a daemon whose configuration
/// hands in a node identity with an Ed25519 key, and complete a key exchange with its edge; then start one whose node
/// identity's key is P-256. Expect: the first edge presents the node's own key, its fingerprint named; the second daemon
/// starts and serves with no SSH edge, since `ssh-ed25519` is the one host key algorithm the edge presents and no key
/// stands in for the node's.
#[test]
fn a_fleet_nodes_edge_presents_its_own_ed25519_key_and_starts_on_no_other() {
  let (ed25519, public) = identity(&rcgen::PKCS_ED25519);
  let daemon = alone(edge(Some(ed25519)));
  let (_, key) = served(&daemon);
  assert_eq!(key, blob_of_public(&public));
  drop(daemon);
  let (p256, _) = identity(&rcgen::PKCS_ECDSA_P256_SHA256);
  let daemon = alone(edge(Some(p256)));
  assert_eq!(daemon.vcs_ssh(), None);
  assert!(daemon.nfs_port().is_some(), "the daemon serves the rest");
}

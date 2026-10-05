//! The node's sealing root (A-92 piece 2a; hyper-raft `docs/seal.md` §3, §8): the key every tenant key on this node is
//! wrapped under, held for the anchor's life and never written to a disk (R1).
//!
//! At boot the daemon locks hyper-seal's key region (one process-wide region of locked pages, out of core dumps on
//! Linux, sized once), locks the anchor's supervision page the same way, and then either finds the root a previous
//! daemon under this anchor published there, or makes one from the secure random source and publishes it. So a daemon
//! restart keeps the root (and with it every tenant key, and so every volume's sealed content), and the root lives
//! exactly as long as the RAM it protects: the anchor's. Each shard then reads the root from its own attachment of the
//! segment into a key of its own in the locked region ([`shard_root`]), as it reads the grant-issuer secret: no key
//! crosses a thread, and no global holds one, so two daemons in one process (the fleet tests) each have their own.
//!
//! Where the OS will not lock the key region or the anchor's page, sealing is unavailable: reported in status, never
//! used unlocked, and the daemon still serves (`RootState::Unavailable`).

use hyper_seal::Secret32;
use hyper_seal::keys::{KeyId, WrappingKey};
use slates_anchor::AnchorSegment;
use slates_anchor::layout::{SEAL_ROOT_ID_BYTES, SEAL_ROOT_KEY_BYTES};

/// Format: the generation a node root is made at; a rotation (a new root wrapping the tenant keys again, seal.md
/// §3.1) is a later generation.
const ROOT_GENERATION: u32 = 1;

/// How this daemon came by the node's sealing root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootState {
  /// Sealing is unavailable: the key region or the anchor's page could not be locked, or the random source refused.
  Unavailable,
  /// Made by this daemon and published into the anchor's locked page, for every daemon after it.
  Minted,
  /// Found in the anchor's page, made by a previous daemon under this anchor.
  Adopted,
}

impl RootState {
  /// The state's name in a status report.
  pub fn name(self) -> &'static str {
    match self {
      RootState::Unavailable => "unavailable",
      RootState::Minted => "minted",
      RootState::Adopted => "adopted",
    }
  }
}

/// Locks the key region for `slots` keys and the anchor's page, then finds or makes the node's root in `segment` (the
/// module doc). The calling thread holds no key afterwards; the shards read the root themselves.
pub fn init(segment: &mut AnchorSegment, slots: usize) -> RootState {
  let region = hyper_seal::keys_held().is_some()
    || hyper_seal::lock_keys(slots).is_ok()
    || hyper_seal::keys_held().is_some();
  if !region {
    eprintln!(
      "slates-server: sealing unavailable: a key region of {slots} keys could not be locked"
    );
    return RootState::Unavailable;
  }
  if segment.protect_seal_page().is_err() {
    eprintln!("slates-server: sealing unavailable: the anchor's key page could not be locked");
    return RootState::Unavailable;
  }
  if matches!(segment.seal_root(), Ok(Some(_))) {
    return RootState::Adopted;
  }
  let (mut id, mut key) = ([0u8; SEAL_ROOT_ID_BYTES], [0u8; SEAL_ROOT_KEY_BYTES]);
  let made = slates_transport::handshake::secure_random(&mut id).is_ok()
    && slates_transport::handshake::secure_random(&mut key).is_ok();
  let published = made && segment.publish_seal_root(&id, &key).is_ok();
  key.fill(0);
  if published {
    RootState::Minted
  } else {
    eprintln!("slates-server: sealing unavailable: the root could not be made or published");
    RootState::Unavailable
  }
}

/// A shard's own copy of the node's root, read from its attachment of the anchor's segment into the locked region:
/// `None` while sealing is unavailable (`state`), or when the page holds no root or the region is full.
pub fn shard_root(segment: &AnchorSegment, state: RootState) -> Option<WrappingKey> {
  if state == RootState::Unavailable {
    return None;
  }
  let (id, mut key) = segment.seal_root().ok()??;
  let secret = Secret32::from_bytes(&key);
  key.fill(0);
  Some(WrappingKey::new(KeyId(id), ROOT_GENERATION, secret.ok()?))
}

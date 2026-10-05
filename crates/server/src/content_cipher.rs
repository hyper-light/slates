//! The cipher a shard seals its idle content with (A-99, §4.2; condition 9): `slates_vfs::content::ChunkCipher` over
//! hyper-seal's `VersionKey` (AES-256-GCM, seal.md §4, nonce `version ‖ segment | LAST`).
//!
//! **Keys.** One content master per shard, derived from the node's root (`seal_keys::shard_root`) when the shard
//! starts: content in the anchor's RAM lives exactly as long as the anchor, and so does the root. Under the master,
//! a key per *epoch*: a volume and a salt drawn at this daemon's start, by HKDF-SHA-384 with the volume's id and the
//! salt as its label. A key's identity is that label, which a recovery image records, so a later daemon derives the
//! same key from it and opens what this one sealed.
//!
//! **Why a fresh salt per daemon life.** The version a chunk is sealed at is a counter the store holds in memory
//! only. A restarted daemon draws a new salt, so it seals under new keys and its counter can start again without a
//! (key, nonce) ever repeating; two lives collide only if their 128-bit salts do (A-99's nonce argument).
//!
//! **Why not the volume's lineage key** (A-92's hierarchy): a lineage key is made with a database record the first
//! time it is asked for, so taking it when a volume is created or first written would nest a transaction inside the
//! verb's own, or put one on the write path. The root lives as long as this content; the HKDF label keeps keys apart
//! per volume and per life.
//!
//! Bounded: the keys held are those of the epochs some chunk still names, at most one per chunk record.

use hyper_seal::keys::{KeyId, WrappingKey};
use hyper_seal::stream::VersionKey;
use slates_vfs::content::{ChunkCipher, KeyIdentity, Tag};
use slates_vfs::error::VfsError;

/// Format: the HKDF purpose a content key is derived under (seal.md §3: a purpose per kind of key).
const PURPOSE: &[u8] = b"slates content at rest v1";
/// Format: the generation the content master is made at.
const MASTER_GENERATION: u32 = 1;
/// Format: a volume id's bytes, the first half of a key's identity.
const VOLUME_BYTES: usize = 16;

/// A shard's content cipher.
pub(crate) struct ShardCipher {
  master: WrappingKey,
  salt: [u8; VOLUME_BYTES],
  keys: Vec<(KeyIdentity, VersionKey)>,
}

impl ShardCipher {
  /// The cipher for a shard whose root is `root`, with this life's salt drawn now; `None` when the key region or the
  /// random source refuses (the shard then keeps its content in the clear, as with no root).
  pub(crate) fn new(root: &WrappingKey) -> Option<ShardCipher> {
    let (secret, _) = root.derive_child(PURPOSE, b"master").ok()?;
    Some(ShardCipher {
      master: WrappingKey::new(KeyId::random().ok()?, MASTER_GENERATION, secret),
      salt: KeyId::random().ok()?.0,
      keys: Vec::new(),
    })
  }

  fn key(&self, reference: u32) -> Result<&VersionKey, VfsError> {
    let at = usize::try_from(reference).map_err(|_| VfsError::Integrity)?;
    self
      .keys
      .get(at)
      .map(|(_, key)| key)
      .ok_or(VfsError::Integrity)
  }

  /// The reference of the key with `identity`, derived and registered when this cipher has not held it yet.
  fn register(&mut self, identity: KeyIdentity) -> Result<u32, VfsError> {
    if let Some(at) = self.keys.iter().position(|(held, _)| *held == identity) {
      return u32::try_from(at).map_err(|_| VfsError::Integrity);
    }
    let (secret, _) = self
      .master
      .derive_child(PURPOSE, &identity)
      .map_err(|_| VfsError::Integrity)?;
    let mut object = [0u8; VOLUME_BYTES];
    object.copy_from_slice(identity.get(..VOLUME_BYTES).ok_or(VfsError::Integrity)?);
    let key = VersionKey::new(&secret, object).map_err(|_| VfsError::Integrity)?;
    let at = u32::try_from(self.keys.len()).map_err(|_| VfsError::Integrity)?;
    self.keys.push((identity, key));
    Ok(at)
  }
}

impl ChunkCipher for ShardCipher {
  fn seal(
    &self,
    key: u32,
    version: u64,
    index: u32,
    last: bool,
    segment: &mut [u8],
  ) -> Result<Tag, VfsError> {
    self
      .key(key)?
      .seal(version, index, last, segment)
      .map_err(|_| VfsError::Integrity)
  }

  fn open(
    &self,
    key: u32,
    version: u64,
    index: u32,
    last: bool,
    segment: &mut [u8],
    tag: &Tag,
  ) -> Result<(), VfsError> {
    self
      .key(key)?
      .open(version, index, last, segment, tag)
      .map_err(|_| VfsError::Integrity)
  }

  fn identity(&self, key: u32) -> Option<KeyIdentity> {
    let at = usize::try_from(key).ok()?;
    self.keys.get(at).map(|(identity, _)| *identity)
  }

  fn reference(&mut self, identity: &KeyIdentity) -> Result<u32, VfsError> {
    self.register(*identity)
  }

  fn key_for_volume(&mut self, volume: [u8; 16]) -> Result<u32, VfsError> {
    let mut identity = KeyIdentity::default();
    let (left, right) = identity.split_at_mut(VOLUME_BYTES);
    left.copy_from_slice(&volume);
    right.copy_from_slice(&self.salt);
    self.register(identity)
  }
}

/// Keys `volume` (the volume with id `id`) to seal its new chunks under this life's key for it, when the shard seals;
/// a refusal leaves it in the clear, counted by the store's cipher refusals at the next seal.
pub(crate) fn key_volume(
  store: &mut slates_vfs::volume::Store,
  volume: &mut slates_vfs::volume::Volume,
  id: [u8; 16],
) {
  let key = store
    .content
    .cipher_mut()
    .and_then(|cipher| cipher.key_for_volume(id).ok());
  volume.set_seal_key(key);
}

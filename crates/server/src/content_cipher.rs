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
//! **Bounded by what is alive (A-208).** A key is held once for the life of the volume sealing under it
//! (`key_for_volume`, released when the volume's slot goes: [`release_seal_key`] from `release_slot_credits`) and once by each
//! sealed chunk record that names it (`slates_vfs::content::ChunkStore` holds at the record's insert and releases at its
//! free). It goes with its last hold, and its slot is reused by the next new key. So the keys held are at most the
//! shard's live volumes plus its sealed chunk records, and one more for each key a recovery image named whose chunk
//! then failed adoption (bounded by that image). Keys are found by an identity-to-slot index, so registering one costs
//! a tree descent, not a walk of every key held.
//!
//! Measured before A-208 (2026-11-05, macOS arm64, `provision_bench` under `xctrace`): nothing let a key go, so every
//! volume ever created kept one; 64 created, written and destroyed left 64 behind
//! (`crates/client/tests/cipher_keys.rs`). Each create found its key by a linear scan of all of them, which held
//! 24,052 of the 26,986 samples inside the server's create over the histogram's ~440,000 creates.

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

/// A key the cipher holds, and how many holds keep it (A-208).
struct HeldKey {
  identity: KeyIdentity,
  key: VersionKey,
  holds: u64,
}

/// A shard's content cipher.
pub(crate) struct ShardCipher {
  master: WrappingKey,
  salt: [u8; VOLUME_BYTES],
  /// The keys by reference; a slot whose key went is `None` until a new key takes it.
  keys: Vec<Option<HeldKey>>,
  /// The reference of each key held, by identity.
  index: std::collections::BTreeMap<KeyIdentity, u32>,
  /// The slots whose key went, for the next new key.
  free: Vec<u32>,
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
      index: std::collections::BTreeMap::new(),
      free: Vec::new(),
    })
  }

  fn held(&self, reference: u32) -> Result<&HeldKey, VfsError> {
    let at = usize::try_from(reference).map_err(|_| VfsError::Integrity)?;
    self
      .keys
      .get(at)
      .and_then(Option::as_ref)
      .ok_or(VfsError::Integrity)
  }

  fn held_mut(&mut self, reference: u32) -> Result<&mut HeldKey, VfsError> {
    let at = usize::try_from(reference).map_err(|_| VfsError::Integrity)?;
    self
      .keys
      .get_mut(at)
      .and_then(Option::as_mut)
      .ok_or(VfsError::Integrity)
  }

  fn key(&self, reference: u32) -> Result<&VersionKey, VfsError> {
    self.held(reference).map(|held| &held.key)
  }

  /// The reference of the key with `identity`, derived and registered when this cipher holds no key of that identity:
  /// in a slot a gone key left, else a new one. Registered with no hold; the caller holds it.
  fn register(&mut self, identity: KeyIdentity) -> Result<u32, VfsError> {
    if let Some(at) = self.index.get(&identity) {
      return Ok(*at);
    }
    let (secret, _) = self
      .master
      .derive_child(PURPOSE, &identity)
      .map_err(|_| VfsError::Integrity)?;
    let mut object = [0u8; VOLUME_BYTES];
    object.copy_from_slice(identity.get(..VOLUME_BYTES).ok_or(VfsError::Integrity)?);
    let key = VersionKey::new(&secret, object).map_err(|_| VfsError::Integrity)?;
    let held = Some(HeldKey {
      identity,
      key,
      holds: 0,
    });
    let at = match self.free.pop() {
      Some(at) => {
        let slot = self
          .keys
          .get_mut(usize::try_from(at).map_err(|_| VfsError::Integrity)?)
          .ok_or(VfsError::Integrity)?;
        *slot = held;
        at
      }
      None => {
        let at = u32::try_from(self.keys.len()).map_err(|_| VfsError::Integrity)?;
        self.keys.push(held);
        at
      }
    };
    self.index.insert(identity, at);
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
    self.held(key).ok().map(|held| held.identity)
  }

  fn reference(&mut self, identity: &KeyIdentity) -> Result<u32, VfsError> {
    self.register(*identity)
  }

  fn hold(&mut self, key: u32) -> Result<(), VfsError> {
    let held = self.held_mut(key)?;
    held.holds = held.holds.checked_add(1).ok_or(VfsError::Integrity)?;
    Ok(())
  }

  fn release(&mut self, key: u32) -> Result<(), VfsError> {
    let held = self.held_mut(key)?;
    held.holds = held.holds.checked_sub(1).ok_or(VfsError::Integrity)?;
    if held.holds > 0 {
      return Ok(());
    }
    // The last hold: the key goes, and its slot waits for the next new key.
    let identity = held.identity;
    let at = usize::try_from(key).map_err(|_| VfsError::Integrity)?;
    if let Some(slot) = self.keys.get_mut(at) {
      *slot = None;
    }
    self.index.remove(&identity);
    self.free.push(key);
    Ok(())
  }

  fn keys_held(&self) -> usize {
    self.index.len()
  }

  fn key_for_volume(&mut self, volume: [u8; 16]) -> Result<u32, VfsError> {
    let mut identity = KeyIdentity::default();
    let (left, right) = identity.split_at_mut(VOLUME_BYTES);
    left.copy_from_slice(&volume);
    right.copy_from_slice(&self.salt);
    let key = self.register(identity)?;
    self.hold(key)?;
    Ok(key)
  }
}

/// Format: the refusal counted when a gone volume's key hold could not be let go (A-208): the key then stays held,
/// which costs one key's memory and nothing else.
pub(crate) const KEY_RELEASE_REFUSED: &str = "content.key_release_refused";

/// Keys `volume` (the volume with id `id`) to seal its new chunks under this life's key for it, when the shard seals;
/// a refusal leaves it in the clear, counted by the store's cipher refusals at the next seal. Every caller keys a volume
/// it has just made, and the key is held for that volume's life (A-208): [`release_seal_key`] lets it go.
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

/// Lets go a gone volume's hold on its seal key `key` (A-208), when its slot leaves the shard; the key stays while a
/// sealed chunk still names it. Whether the release was made: `false` is counted by the caller as
/// [`KEY_RELEASE_REFUSED`].
pub(crate) fn release_seal_key(store: &mut slates_vfs::volume::Store, key: Option<u32>) -> bool {
  let Some(key) = key else {
    return true;
  };
  store
    .content
    .cipher_mut()
    .is_none_or(|cipher| cipher.release(key).is_ok())
}

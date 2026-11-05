//! A green's head identity as a set hash over its entries (§4.16 "Apply on holders": every holder recomputes the
//! identity and compares it with the merge record's; A-202). Each entry of the green's state (a file and its bytes, a
//! directory, a mode, a symlink, a hard link, an extended attribute, a FIFO/socket inode) is encoded as it always was
//! (its kind's tag, then every field length-prefixed) and expanded by BLAKE3's extendable output into [`LANES`]
//! sixteen-bit lanes; the green's sum is those vectors added lane by lane modulo 2^16, and the identity is the BLAKE3
//! of the sum. This is LtHash16 (Lewi, Kim, Maykov and Weis, "Securing Update Propagation with Homomorphic Hashing",
//! IACR ePrint 2019/227, tier A; Bellare and Micciancio's incremental hashing, EUROCRYPT 1997): addition is
//! commutative and invertible, so a commit updates the sum by subtracting each entry it replaces or removes and adding
//! each it sets, in O(what changed), and the sum equals a from-scratch sum over the state in any order of updates —
//! exactly, not probably, which the oracle in `crates/merge/tests/identity.rs` holds after every step of generated
//! histories. The whole-state BLAKE3 it replaces cost every accept a pass over every byte the green holds (113 of 120
//! profile samples of a 1,000-file green's submit, 2026-11-04).

use blake3::Hasher;

/// Shape: lanes of the set hash, LtHash16's 1,024 sixteen-bit lanes (2 KiB), the parameters Lewi et al. 2019 give a
/// collision resistance of at least 200 bits for.
pub(crate) const LANES: usize = 1024;

/// Format: the BLAKE3 key-derivation context an entry is expanded under, so these lanes are never another hash's.
const ENTRY_CONTEXT: &str = "slates 2026-11-05 green head identity entry lanes";
/// Format: the context the sum is hashed under into the 32-byte identity.
const SUM_CONTEXT: &str = "slates 2026-11-05 green head identity sum";
/// Format: the bytes of one lane.
const LANE_BYTES: usize = size_of::<u16>();

/// The running sum of a green's entry vectors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetHash {
  lanes: Box<[u16; LANES]>,
}

impl Default for SetHash {
  fn default() -> Self {
    SetHash {
      lanes: Box::new([0; LANES]),
    }
  }
}

/// One entry being encoded: fed field by field, then expanded.
pub(crate) struct Entry {
  hasher: Hasher,
}

impl Entry {
  /// An entry of the kind `tag`.
  pub(crate) fn new(tag: &[u8]) -> Entry {
    let mut hasher = Hasher::new_derive_key(ENTRY_CONTEXT);
    hasher.update(tag);
    Entry { hasher }
  }

  /// A length-prefixed field.
  pub(crate) fn field(mut self, bytes: &[u8]) -> Entry {
    self.hasher.update(&(bytes.len() as u64).to_le_bytes());
    self.hasher.update(bytes);
    self
  }

  /// A fixed-width field, unprefixed (a mode).
  pub(crate) fn fixed(mut self, bytes: &[u8]) -> Entry {
    self.hasher.update(bytes);
    self
  }

  /// The entry's lane vector.
  fn lanes(&self) -> Box<[u16; LANES]> {
    let mut bytes = [0u8; LANES * LANE_BYTES];
    self.hasher.finalize_xof().fill(&mut bytes);
    let mut lanes = Box::new([0u16; LANES]);
    for (lane, pair) in lanes.iter_mut().zip(bytes.as_chunks::<LANE_BYTES>().0) {
      *lane = u16::from_le_bytes(*pair);
    }
    lanes
  }
}

impl SetHash {
  /// Adds `entry` to the set.
  pub(crate) fn add(&mut self, entry: &Entry) {
    for (sum, lane) in self.lanes.iter_mut().zip(entry.lanes().iter()) {
      *sum = sum.wrapping_add(*lane);
    }
  }

  /// Removes `entry` from the set (it must have been added).
  pub(crate) fn remove(&mut self, entry: &Entry) {
    for (sum, lane) in self.lanes.iter_mut().zip(entry.lanes().iter()) {
      *sum = sum.wrapping_sub(*lane);
    }
  }

  /// The 32-byte identity of the set.
  pub(crate) fn identity(&self) -> [u8; 32] {
    let mut bytes = [0u8; LANES * LANE_BYTES];
    for (pair, lane) in bytes
      .as_chunks_mut::<LANE_BYTES>()
      .0
      .iter_mut()
      .zip(self.lanes.iter())
    {
      *pair = lane.to_le_bytes();
    }
    let mut hasher = Hasher::new_derive_key(SUM_CONTEXT);
    hasher.update(&bytes);
    *hasher.finalize().as_bytes()
  }
}

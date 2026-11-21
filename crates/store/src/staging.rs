//! Changes in flight (A-767 items 1 and 8): the bytes of a change the verdict has not yet landed, held in RAM as they
//! arrived and named by identity (plain BLAKE3 of the bytes, the identity rule of every tool's content), so a read of
//! them before their landing ([`crate::store::Store::piece`]) and their landing take the same bytes, never a copy.
//!
//! **Moved, not copied.** A change's bytes come in as the `Vec` the tool already holds (git's received pack, a
//! changegroup, a commit's texts) and are kept as that allocation: staging adds a map entry, not a buffer. A content
//! staged twice (the same bytes by two pushes) is held once, counted twice, and freed with its last holder.
//!
//! **Bounded.** The bytes held are at most the budget the caller gives, the change-in-flight share its shard lends it
//! (Ada's memory-sizing ruling: from the work, never the machine); past it a stage is refused [`Full`] with the bytes
//! handed back, so the caller sheds or waits, and nothing grows unbounded.

use std::collections::BTreeMap;

/// A content's identity: BLAKE3 of its bytes.
pub type Identity = [u8; 32];

/// The bytes of a refused stage, handed back.
#[derive(Debug, PartialEq, Eq)]
pub struct Full(pub Vec<u8>);

/// One staged content: its bytes and how many stages hold it.
#[derive(Debug)]
struct Held {
  bytes: Vec<u8>,
  holders: u32,
}

/// The changes in flight, by identity, within a budget of bytes.
#[derive(Debug)]
pub struct Staging {
  held: BTreeMap<Identity, Held>,
  bytes: usize,
  budget: usize,
}

impl Staging {
  /// Staging for at most `budget` bytes.
  pub fn new(budget: usize) -> Self {
    Self {
      held: BTreeMap::new(),
      bytes: 0,
      budget,
    }
  }

  /// Stages `bytes`, moved in, and gives back their identity. Bytes already staged gain a holder and are dropped here.
  /// Refused [`Full`] when they would pass the budget.
  pub fn stage(&mut self, bytes: Vec<u8>) -> Result<Identity, Full> {
    let identity: Identity = *blake3::hash(&bytes).as_bytes();
    if let Some(held) = self.held.get_mut(&identity) {
      held.holders = held.holders.saturating_add(1);
      return Ok(identity);
    }
    let Some(after) = self
      .bytes
      .checked_add(bytes.len())
      .filter(|after| *after <= self.budget)
    else {
      return Err(Full(bytes));
    };
    self.bytes = after;
    self.held.insert(identity, Held { bytes, holders: 1 });
    Ok(identity)
  }

  /// The staged bytes of `identity`.
  pub fn get(&self, identity: &Identity) -> Option<&[u8]> {
    self.held.get(identity).map(|held| held.bytes.as_slice())
  }

  /// Lets one holder of `identity` go; the last frees its bytes.
  pub fn release(&mut self, identity: &Identity) {
    let Some(held) = self.held.get_mut(identity) else {
      return;
    };
    held.holders = held.holders.saturating_sub(1);
    if held.holders == 0
      && let Some(gone) = self.held.remove(identity)
    {
      self.bytes = self.bytes.saturating_sub(gone.bytes.len());
    }
  }

  /// Bytes held now.
  pub fn bytes(&self) -> usize {
    self.bytes
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: stage two contents, the first twice, inside a budget; release. Expect: identities are BLAKE3 of the bytes, the
  /// twice-staged one is held once and freed with its second release, and the budget refuses with the bytes back.
  #[test]
  fn contents_are_held_once_by_identity_within_the_budget() {
    let mut staging = Staging::new(10);
    let a = staging.stage(b"abcd".to_vec()).unwrap();
    assert_eq!(a, *blake3::hash(b"abcd").as_bytes());
    assert_eq!(
      staging.stage(b"abcd".to_vec()),
      Ok(a),
      "the same bytes, held once"
    );
    assert_eq!(staging.bytes(), 4);
    let b = staging.stage(b"efghij".to_vec()).unwrap();
    assert_eq!(staging.bytes(), 10);
    assert_eq!(
      staging.stage(b"k".to_vec()),
      Err(Full(b"k".to_vec())),
      "past the budget, handed back"
    );
    staging.release(&a);
    assert_eq!(staging.get(&a), Some(&b"abcd"[..]), "one holder left");
    staging.release(&a);
    assert_eq!(staging.get(&a), None);
    assert_eq!(staging.bytes(), 6);
    staging.release(&b);
    assert_eq!(staging.bytes(), 0);
  }
}

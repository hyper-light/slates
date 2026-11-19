//! A revision's node (A-588; `internals.revlogs` "Hash Computation" \[B\]; `utils/storageutil.py` `hashrevisionsha1`,
//! `utils/hashutil.py` and `thirdparty/sha1dc/cext.c` \[C\]): the SHA-1 of its two parents, the lower first, and then
//! its text. A node names a revision's text and its history at once, so every changeset, manifest and file revision a
//! client sends is checked against it before the server keeps anything (§4.17.3, the index; AC-HG.2). The parents go in
//! ascending order whichever is first, so swapping them never changes a node: 7.2 swaps a file revision's parents on
//! the way in and out ([`crate::filerev::FileRevision::parents_as_stored`], changegroup `04`'s metadata flag) and the
//! node stays.
//!
//! **SHA-1 with collision detection, as 7.2 hashes it.** 7.2's `hashutil.sha1` is sha1collisiondetection's whenever its
//! C extension is built, as in the installs this was checked against (Homebrew's 7.2 ships
//! `sha1dc.cpython-314-darwin.so`; the fixtures' runs printed `<class 'sha1dc.sha1'>`), and its binding detects and
//! never mends: `SHA1DCSetSafeHash(&(self->ctx), 0)`, "We don't want "safe" sha1s ... We just want to detect
//! collisions", and a detection raises `OverflowError("sha1 collision attack detected")`, so a 7.2 server refuses the
//! revision. Here it is RustCrypto's `sha1-checked`, the pure-Rust port of the same detection that git's object names
//! use (A-600), configured as 7.2 configures it: an input in which a block shows a known attack's disturbance is
//! refused ([`CollisionAttack`]) and named by its plain SHA-1. SHA-1 is Mercurial's name for a revision here, never
//! slates' own integrity, which is BLAKE3's.
//!
//! No public collision has 7.2's framing: one would need the first forty bytes it hashes to be two parents in ascending
//! order, and SHAttered's prefix begins with a higher twenty bytes than its second twenty. So the detection is proven
//! on the hasher with SHAttered's two prefixes, and the framing on every node stock 7.2 stored (`tests/objects.rs`).

use sha1_checked::{Digest, Sha1};

use crate::out::Out;
use crate::{NODE_LEN, Node};

/// Format: hexadecimal, the base of a node's written digits (`node.hex`, `bin`).
const HEX_RADIX: u32 = 16;

/// Parents and a text in which SHA-1's collision detection found a known collision attack: refused, as 7.2 refuses
/// them ("sha1 collision attack detected"), with the plain SHA-1 they hash to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollisionAttack {
  /// The plain SHA-1 of what was hashed (7.2's error names none).
  pub sha1: Node,
}

/// Why a node was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A node that is not what its parents and text hash to (§4.17.3: `ObjectIdMismatch`).
  Mismatch {
    /// The node sent.
    claimed: Node,
    /// What the parents and text hash to.
    computed: Node,
  },
  /// Parents and a text that are part of a known collision attack.
  CollisionAttack(CollisionAttack),
}

impl From<CollisionAttack> for Refusal {
  fn from(attack: CollisionAttack) -> Self {
    Self::CollisionAttack(attack)
  }
}

/// SHA-1 as 7.2's `hashutil.sha1` computes it: collision detection on, the safe hash off. Its state, the detection's
/// two expanded messages included, comes to about 830 bytes by its fields' sizes and lives on the caller's stack, so a
/// node costs no allocation.
struct Hasher {
  /// The running hash.
  state: Sha1,
}

impl Hasher {
  /// A hasher configured as 7.2's binding configures sha1dc.
  fn new() -> Self {
    Self {
      state: Sha1::builder()
        .detect_collision(true)
        .safe_hash(false)
        .build(),
    }
  }

  /// Hashes `bytes` next.
  fn update(&mut self, bytes: &[u8]) {
    Digest::update(&mut self.state, bytes);
  }

  /// The SHA-1 of what was hashed, or the attack the detection found in it.
  fn finish(self) -> Result<Node, CollisionAttack> {
    let result = self.state.try_finalize();
    let mut sha1 = [0; NODE_LEN];
    for (slot, byte) in sha1.iter_mut().zip(result.hash().iter()) {
      *slot = *byte;
    }
    if result.has_collision() {
      Err(CollisionAttack { sha1 })
    } else {
      Ok(sha1)
    }
  }
}

/// The node 7.2 computes for `text` with parents `p1` and `p2` (`hashrevisionsha1`): the SHA-1 of the lower parent,
/// then the higher, then the text; refused where the detection finds an attack.
pub fn compute(p1: &Node, p2: &Node, text: &[u8]) -> Result<Node, CollisionAttack> {
  let (low, high) = if p1 <= p2 { (p1, p2) } else { (p2, p1) };
  let mut hasher = Hasher::new();
  hasher.update(low);
  hasher.update(high);
  hasher.update(text);
  hasher.finish()
}

/// The node forty hexadecimal digits write, in either case as 7.2's `bin` reads them; `None` for any other field.
pub(crate) fn from_hex(field: &[u8]) -> Option<Node> {
  let (pairs, odd) = field.as_chunks::<2>();
  if !odd.is_empty() || pairs.len() != NODE_LEN {
    return None;
  }
  let mut node = [0; NODE_LEN];
  for (byte, pair) in node.iter_mut().zip(pairs) {
    *byte = crate::quote::hex_pair(pair)?;
  }
  Some(node)
}

/// Writes a node's forty lower-case hexadecimal digits, as 7.2's `hex` writes them.
pub(crate) fn write_hex(node: &Node, out: &mut impl Out) {
  for byte in node {
    for digit in [u32::from(*byte) / HEX_RADIX, u32::from(*byte) % HEX_RADIX] {
      let character = char::from_digit(digit, HEX_RADIX).unwrap_or('0');
      out.put(&[u8::try_from(character).unwrap_or(b'0')]);
    }
  }
}

/// Checks that `node` is what `p1`, `p2` and `text` hash to.
pub fn check(node: &Node, p1: &Node, p2: &Node, text: &[u8]) -> Result<(), Refusal> {
  let computed = compute(p1, p2, text)?;
  if computed == *node {
    Ok(())
  } else {
    Err(Refusal::Mismatch {
      claimed: *node,
      computed,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::{CollisionAttack, Hasher, Refusal, check, compute, from_hex, write_hex};
  use crate::NULL_NODE;
  use crate::fixtures::{hex, node};
  use proptest::prelude::*;

  /// git's own t0013 vector, `shattered-1.pdf`, cut to the 320 bytes that hold both colliding blocks (A-600's cut, the
  /// same bytes: SHA-256 `cac8644d…`).
  const SHATTERED_1: &[u8] = include_bytes!("../fixtures/sha1dc/shattered-1.prefix.bin");

  /// Its SHAttered twin, cut the same (SHA-256 `842a2c7d…`).
  const SHATTERED_2: &[u8] = include_bytes!("../fixtures/sha1dc/shattered-2.prefix.bin");

  /// The plain SHA-1 both cuts share (`shasum -a 1` of each, 2026-11-19).
  const SHATTERED_SHA1: &str = "f92d74e3874587aaf443d1db961d4e26dde13e9c";

  /// A-588 (golden): do hash the text of a first revision, `a\n`, with no parents. Expect the node stock 7.2 gave it in
  /// A-514's and A-588's fixtures, `b789fdd9…`, and a node with a flipped byte refused naming both.
  #[test]
  fn a_first_revision_hashes_to_mercurials_node() {
    let computed = compute(&NULL_NODE, &NULL_NODE, b"a\n").unwrap();
    assert_eq!(hex(&computed), "b789fdd96dc2f3bd229c1dd8eedf0fc60e2b68e3");
    let mut wrong = computed;
    wrong[0] ^= 1;
    assert_eq!(
      check(&wrong, &NULL_NODE, &NULL_NODE, b"a\n"),
      Err(Refusal::Mismatch {
        claimed: wrong,
        computed
      })
    );
  }

  /// A-588 (7.2's sha1dc binding: detection on, the safe hash off, `OverflowError` on a detection). Do: hash
  /// SHAttered's two prefixes through the hasher every node goes through, and the first 192 bytes they share, before
  /// the colliding blocks. Expect: the two cuts different bytes, each refused as an attack named by the plain SHA-1
  /// they share (the safe hash would have named two others), and the shared start hashed without a refusal.
  #[test]
  fn the_shattered_pair_is_refused_as_7_2_refuses_it() {
    assert_ne!(SHATTERED_1, SHATTERED_2);
    let shared = node(SHATTERED_SHA1);
    for cut in [SHATTERED_1, SHATTERED_2] {
      let mut hasher = Hasher::new();
      hasher.update(cut);
      assert_eq!(hasher.finish(), Err(CollisionAttack { sha1: shared }));
    }
    let mut start = Hasher::new();
    start.update(&SHATTERED_1[..192]);
    assert!(start.finish().is_ok());
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-588: do hash a text with two parents given in either order. Expect the same node, as 7.2 sorts the parents
    /// first, and the node checking against itself in both orders.
    #[test]
    fn the_parents_order_does_not_change_the_node(
      p1 in any::<[u8; 20]>(),
      p2 in any::<[u8; 20]>(),
      text in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
      let computed = compute(&p1, &p2, &text).unwrap();
      prop_assert_eq!(compute(&p2, &p1, &text), Ok(computed));
      prop_assert_eq!(check(&computed, &p1, &p2, &text), Ok(()));
      prop_assert_eq!(check(&computed, &p2, &p1, &text), Ok(()));
    }

    /// A-588: do write any node in hexadecimal and read it back, in lower and upper case, and read it cut short and
    /// grown by a digit. Expect 7.2's lower-case digits, the node from either case, and the other two refused.
    #[test]
    fn nodes_read_and_write_as_hexadecimal(generated in any::<[u8; 20]>()) {
      let mut digits = Vec::new();
      write_hex(&generated, &mut digits);
      let lower = hex(&generated);
      prop_assert_eq!(digits.as_slice(), lower.as_bytes());
      prop_assert_eq!(from_hex(&digits), Some(generated));
      prop_assert_eq!(from_hex(&digits.to_ascii_uppercase()), Some(generated));
      prop_assert_eq!(from_hex(&digits[..39]), None);
      prop_assert_eq!(from_hex(&[digits.as_slice(), b"0"].concat()), None);
    }
  }
}

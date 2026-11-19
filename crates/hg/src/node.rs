//! A revision's node (A-588; `internals.revlogs` "Hash Computation" [B]; `utils/storageutil.py` `hashrevisionsha1`
//! [C]): the SHA-1 of its two parents, the lower first, and then its text. A node names a revision's text and its
//! history at once, so every changeset, manifest and file revision a client sends is checked against it before the
//! server keeps anything (§4.17.3, the index). SHA-1 here is Mercurial's name for a revision, never slates' own
//! integrity, which is BLAKE3's; it is computed by AWS-LC, the one cryptographic library slates builds on (A-66).

use aws_lc_rs::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};

use crate::{NODE_LEN, Node};

/// The node Mercurial computes for `text` with parents `p1` and `p2` (`hashrevisionsha1`): the SHA-1 of the lower
/// parent, then the higher, then the text.
pub fn compute(p1: &Node, p2: &Node, text: &[u8]) -> Node {
  let (low, high) = if p1 <= p2 { (p1, p2) } else { (p2, p1) };
  let mut context = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
  context.update(low);
  context.update(high);
  context.update(text);
  let digest = context.finish();
  let mut node = [0; NODE_LEN];
  for (slot, byte) in node.iter_mut().zip(digest.as_ref()) {
    *slot = *byte;
  }
  node
}

/// A node that is not what its parents and text hash to (§4.17.3: `ObjectIdMismatch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mismatch {
  /// The node sent.
  pub claimed: Node,
  /// What the parents and text hash to.
  pub computed: Node,
}

/// Checks that `node` is what `p1`, `p2` and `text` hash to.
pub fn check(node: &Node, p1: &Node, p2: &Node, text: &[u8]) -> Result<(), Mismatch> {
  let computed = compute(p1, p2, text);
  if computed == *node {
    Ok(())
  } else {
    Err(Mismatch {
      claimed: *node,
      computed,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::{Mismatch, check, compute};
  use crate::NULL_NODE;
  use proptest::prelude::*;

  /// A-588: do hash the text of a first revision, `a\n`, which Mercurial stores in a new repository's first file
  /// revision. Expect that revision's node in A-514's and A-588's fixtures, `b789fdd9...`, and a node with a flipped
  /// byte refused naming both.
  #[test]
  fn a_first_revision_hashes_to_mercurials_node() {
    let node = compute(&NULL_NODE, &NULL_NODE, b"a\n");
    assert_eq!(
      crate::fixtures::hex(&node),
      "b789fdd96dc2f3bd229c1dd8eedf0fc60e2b68e3"
    );
    let mut wrong = node;
    wrong[0] ^= 1;
    assert_eq!(
      check(&wrong, &NULL_NODE, &NULL_NODE, b"a\n"),
      Err(Mismatch {
        claimed: wrong,
        computed: node
      })
    );
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-588: do hash a text with two parents given in either order. Expect the same node, as Mercurial sorts the
    /// parents first, and the node checking against itself.
    #[test]
    fn the_parents_order_does_not_change_the_node(
      p1 in any::<[u8; 20]>(),
      p2 in any::<[u8; 20]>(),
      text in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
      let node = compute(&p1, &p2, &text);
      prop_assert_eq!(node, compute(&p2, &p1, &text));
      prop_assert_eq!(check(&node, &p1, &p2, &text), Ok(()));
    }
  }
}

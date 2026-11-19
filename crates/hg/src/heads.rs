//! The `check:heads` and `check:updated-heads` payloads (A-545; `internals.bundle2` [B]; `bundle2_part_handlers.py`
//! `handlecheckheads` and `handlecheckupdatedheads` [C]): the heads the pushing client saw, 20-byte nodes one after
//! another. `check:heads` names every head (the strict mode, and a push to an empty repository); `check:updated-heads`
//! names only the heads the push replaces (`concurrent-push-mode = check-related`, 7.2's default). 7.2 reads 20 bytes
//! at a time and asserts that nothing is left (`assert not h`); a payload that is not whole nodes is refused here.
//!
//! Nothing is allocated in decoding: the payload is checked whole, then its nodes are read in place, so the payload
//! itself is the only cap. An encode's exact length is known before it is made ([`encoded_len`]).

use core::slice::Iter;

use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::{NODE_LEN, Node};

/// Why a heads payload was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A payload that is not a whole number of nodes.
  PartialNode {
    /// The payload's length.
    len: u64,
  },
}

/// The heads a payload names, in order.
#[derive(Clone)]
pub struct Heads<'a> {
  /// The payload's nodes.
  nodes: Iter<'a, Node>,
}

/// The heads a `check:heads` or `check:updated-heads` payload names, the payload checked whole first.
pub fn decode(payload: &[u8]) -> Result<Heads<'_>, Refusal> {
  let (nodes, partial) = payload.as_chunks::<NODE_LEN>();
  if !partial.is_empty() {
    return Err(Refusal::PartialNode {
      len: to_u64(payload.len()),
    });
  }
  Ok(Heads {
    nodes: nodes.iter(),
  })
}

impl Iterator for Heads<'_> {
  type Item = Node;

  fn next(&mut self) -> Option<Node> {
    self.nodes.next().copied()
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.nodes.size_hint()
  }
}

impl ExactSizeIterator for Heads<'_> {}

/// The length of the payload that names `heads`, for a caller that charges it before encoding.
pub fn encoded_len(heads: &[Node]) -> u64 {
  let mut measure = Measure(0);
  write(heads, &mut measure);
  measure.0
}

/// Appends the payload that names `heads`, in order.
pub fn encode(heads: &[Node], out: &mut Vec<u8>) {
  write(heads, out);
}

/// Writes each head's node.
fn write(heads: &[Node], out: &mut impl Out) {
  for head in heads {
    out.put(head);
  }
}

#[cfg(test)]
mod tests {
  use super::{Refusal, decode, encode, encoded_len};
  use proptest::prelude::*;

  /// AC-HG.9 (hostile input), A-545: do give a heads payload one byte short of two nodes, one of two, and an empty
  /// one. Expect the first refused `PartialNode`, as 7.2's `assert not h` would fail, the second read as its two
  /// nodes, encoded back to its bytes at the length `encoded_len` gave, and the third read as no heads.
  #[test]
  fn a_payload_of_partial_nodes_is_refused() {
    let two = [[7_u8; 20], [9_u8; 20]];
    let mut payload = Vec::new();
    encode(&two, &mut payload);
    assert_eq!(encoded_len(&two), 40);
    assert_eq!(decode(&payload).unwrap().collect::<Vec<_>>(), two);
    assert!(matches!(
      decode(&payload[..39]),
      Err(Refusal::PartialNode { len: 39 })
    ));
    assert_eq!(decode(&[]).unwrap().count(), 0);
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// AC-HG.9, A-545: do read arbitrary bytes as heads. Expect whole nodes read and encoded back exactly, or a
    /// refusal; never a panic.
    #[test]
    fn arbitrary_payloads_read_whole_or_are_refused(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
      if let Ok(heads) = decode(&bytes) {
        let nodes: Vec<_> = heads.collect();
        let mut out = Vec::new();
        encode(&nodes, &mut out);
        prop_assert_eq!(out, bytes);
      }
    }
  }
}

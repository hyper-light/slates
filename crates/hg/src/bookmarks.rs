//! The `bookmarks` and `check:bookmarks` payloads (A-545; `internals.bundle2` [B]; `bookmarks.py` `binaryencode` and
//! `binarydecode` [C]): entries of a 20-byte node, the name's length as a big-endian unsigned 16-bit integer, and the
//! name. A node with every bit set (`wdirid`) stands for "no bookmark": in `bookmarks`, a deletion; in
//! `check:bookmarks`, a bookmark the pushing client saw absent. 7.2 refuses an entry cut short ("bad bookmark
//! stream"), as this module does.
//!
//! Nothing is allocated: the payload is checked whole, then its entries are read in place, so the payload itself is the
//! only cap. A bookmark's name is returned as sent; whether it is a name Mercurial allows is its handler's to judge
//! (piece 5).

use crate::read::Cursor;
use crate::{NODE_LEN, Node, Truncated};

/// Format: an entry's fixed part: the node, then the name's length as a big-endian unsigned 16-bit integer
/// (`_binaryentry = '>20sH'`).
pub const ENTRY_HEADER_LEN: usize = 22;

/// Format: the node that stands for "no bookmark": every bit set (`nodeconstants.wdirid`).
pub const MISSING: Node = [u8::MAX; NODE_LEN];

/// One entry: a bookmark's name and its node, or `None` where the node says "no bookmark".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bookmark<'a> {
  /// The name, as sent.
  pub name: &'a [u8],
  /// The node, or `None` for "no bookmark".
  pub node: Option<Node>,
}

/// Why a bookmarks payload was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// An entry cut short (7.2: "bad bookmark stream").
  Truncated(Truncated),
}

/// Why bookmarks could not be encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A name longer than an unsigned 16-bit length holds.
  NameTooLong {
    /// Its length.
    len: usize,
  },
  /// A node equal to [`MISSING`], which would read back as "no bookmark".
  AmbiguousNode,
}

/// The entries of a payload, in order.
#[derive(Clone)]
pub struct Bookmarks<'a> {
  /// The payload, read front to back.
  cursor: Cursor<'a>,
}

/// The entries of a `bookmarks` or `check:bookmarks` payload, the payload checked whole first.
pub fn decode(payload: &[u8]) -> Result<Bookmarks<'_>, Refusal> {
  let mut check = Cursor::new(payload);
  while !check.is_done() {
    read_one(&mut check).map_err(Refusal::Truncated)?;
  }
  Ok(Bookmarks {
    cursor: Cursor::new(payload),
  })
}

/// One entry at the cursor.
fn read_one<'a>(cursor: &mut Cursor<'a>) -> Result<Bookmark<'a>, Truncated> {
  let node = cursor.node("bookmark node")?;
  let len = usize::from(cursor.u16("bookmark name length")?);
  let name = cursor.take(len, "bookmark name")?;
  Ok(Bookmark {
    name,
    node: (node != MISSING).then_some(node),
  })
}

impl<'a> Iterator for Bookmarks<'a> {
  type Item = Bookmark<'a>;

  fn next(&mut self) -> Option<Bookmark<'a>> {
    if self.cursor.is_done() {
      return None;
    }
    read_one(&mut self.cursor).ok()
  }
}

/// Appends the payload of `bookmarks`, in order, after checking every entry can be written.
pub fn encode(bookmarks: &[Bookmark<'_>], out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  for bookmark in bookmarks {
    u16::try_from(bookmark.name.len()).map_err(|_| EncodeRefusal::NameTooLong {
      len: bookmark.name.len(),
    })?;
    if bookmark.node == Some(MISSING) {
      return Err(EncodeRefusal::AmbiguousNode);
    }
  }
  for bookmark in bookmarks {
    out.extend_from_slice(&bookmark.node.unwrap_or(MISSING));
    let len = u16::try_from(bookmark.name.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bookmark.name);
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{Bookmark, EncodeRefusal, MISSING, Refusal, decode, encode};
  use proptest::prelude::*;

  /// A-545: do encode a bookmark with a node and one without, and decode them. Expect both back, the second written
  /// with every bit of its node set, as 7.2 writes "no bookmark".
  #[test]
  fn a_bookmark_and_a_missing_one_round_trip() {
    let entries = [
      Bookmark {
        name: b"feature",
        node: Some([4; 20]),
      },
      Bookmark {
        name: b"gone",
        node: None,
      },
    ];
    let mut payload = Vec::new();
    encode(&entries, &mut payload).unwrap();
    assert_eq!(&payload[29..49], &MISSING[..]);
    assert_eq!(decode(&payload).unwrap().collect::<Vec<_>>(), entries);
  }

  /// AC-HG.9 (hostile input), A-545: do cut a payload inside an entry's node, inside its length and inside its name,
  /// and give a name length of `u16::MAX` with a short name. Expect each refused `Truncated` ("bad bookmark stream").
  #[test]
  fn entries_cut_short_are_refused() {
    let mut payload = Vec::new();
    encode(
      &[Bookmark {
        name: b"feature",
        node: Some([4; 20]),
      }],
      &mut payload,
    )
    .unwrap();
    for cut in [10, 21, 25] {
      assert!(
        matches!(decode(&payload[..cut]), Err(Refusal::Truncated(_))),
        "cut at {cut}"
      );
    }
    let mut long = vec![4; 20];
    long.extend_from_slice(&u16::MAX.to_be_bytes());
    long.extend_from_slice(b"short");
    assert!(matches!(decode(&long), Err(Refusal::Truncated(_))));
  }

  /// A-545: do encode a node equal to the "no bookmark" marker, and a name too long for its length field. Expect both
  /// refused, nothing written.
  #[test]
  fn ambiguous_nodes_and_long_names_are_not_encoded() {
    let mut out = Vec::new();
    assert_eq!(
      encode(
        &[Bookmark {
          name: b"x",
          node: Some(MISSING)
        }],
        &mut out
      ),
      Err(EncodeRefusal::AmbiguousNode)
    );
    let name = vec![b'n'; usize::from(u16::MAX) + 1];
    assert_eq!(
      encode(
        &[Bookmark {
          name: &name,
          node: None
        }],
        &mut out
      ),
      Err(EncodeRefusal::NameTooLong { len: name.len() })
    );
    assert!(out.is_empty());
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// AC-HG.9, A-545: do read arbitrary bytes as bookmarks. Expect entries that encode back exactly, or a refusal;
    /// never a panic.
    #[test]
    fn arbitrary_payloads_read_exactly_or_are_refused(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
      if let Ok(entries) = decode(&bytes) {
        let entries: Vec<_> = entries.collect();
        let mut out = Vec::new();
        encode(&entries, &mut out).unwrap();
        prop_assert_eq!(out, bytes);
      }
    }
  }
}

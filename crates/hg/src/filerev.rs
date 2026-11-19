//! A file revision's text (A-588; `utils/storageutil.py` `parsemeta` and `packmeta`, `filelog.py` `add`, `has_meta`
//! and `renamed`, `revlogutils/rewrite.py` `_has_bad_parents` \[C\]): the file's bytes, behind a metadata block when it
//! has one: `\x01\n`, a `key: value` line per key (sorted), and `\x01\n` again. 7.2 writes a block for a copy (`copy`,
//! the source's path, and `copyrev`, the source's file node in hexadecimal) and, empty, before bytes that themselves
//! begin with `\x01\n`, so that they are not read as one. The design keeps the block apart from the bytes (§4.17.3, the
//! index), so a file's bytes are the same content in every tree.
//!
//! A text is split in place, its block checked whole first, and [`FileRevision::encode`] writes a split text back to
//! exactly its bytes: whatever [`split`] reads, the writer writes back, and whatever the writer writes, `split` reads
//! back as the same block and bytes. [`pack`] instead writes a new text from entries, as 7.2's `filelog.add` does. 7.2
//! fails on a block that is never closed and on a block line without `: `, and both are refused by kind.
//!
//! **A copy, as 7.2 reads one.** `renamed` reads a copy only from a revision whose first parent is null (7.2's commit
//! gives a copy a null first parent, so a revision with another first parent "has no copy metadata"), and only when the
//! block names both `copy` and `copyrev`: "copy and copyrev occur in pairs. In rare cases due to old bugs, one can
//! occur without the other", and such a block reads as no copy. A `copyrev` that is not forty hexadecimal digits is
//! refused (7.2's `bin` raises on most, and a shorter node names nothing). Which parent comes first is 7.2's to keep:
//! since Mercurial 5.8 wrote some copies' parents the other way round (issue6528), 7.2 swaps them back on every
//! incoming file revision ([`FileRevision::parents_as_stored`]); the node is unchanged, as it hashes the parents in
//! order of value.

use crate::lines::lines;
use crate::node::from_hex;
use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::{NULL_NODE, Node};

/// Format: the bytes that open and close a metadata block (`\x01\n`).
pub const MARKER: &[u8] = b"\x01\n";

/// Format: the bytes between a block line's key and value.
const KEY_SEPARATOR: &[u8] = b": ";

/// Format: the byte that ends a block line.
const NEWLINE: u8 = b'\n';

/// Format: a byte `splitlines()` also ends a line at.
const RETURN: u8 = b'\r';

/// Format: the byte a marker begins with: a value ending in it would close the block at its line's end.
const MARKER_START: u8 = 0x01;

/// Format: the key naming a copy's source path.
pub const COPY: &[u8] = b"copy";

/// Format: the key naming a copy's source file node, in hexadecimal.
pub const COPY_REV: &[u8] = b"copyrev";

/// A file revision's text, split: its metadata block, when it has one, and its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileRevision<'a> {
  /// The block's lines, without its markers; `Some(&[])` for an empty block.
  pub metadata: Option<&'a [u8]>,
  /// The file's bytes.
  pub bytes: &'a [u8],
}

/// A copy: its source's path and file node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopySource<'a> {
  /// The source's path.
  pub path: &'a [u8],
  /// The source's file node.
  pub node: Node,
}

/// Why a file revision was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A block that is never closed.
  Unclosed,
  /// A block line without `: `.
  Entry {
    /// The line's index.
    line: u64,
  },
  /// A copy whose `copyrev` is not forty hexadecimal digits.
  Copy,
}

/// Why a file revision could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A key holding `: ` or a line end, a value holding a line end or ending as a marker begins, which would read back
  /// as other lines, or a key given twice, which 7.2's dictionary cannot hold.
  Entry {
    /// The entry's index.
    entry: usize,
  },
  /// A block [`split`] would not read back as itself: a line without `: `, or the closing marker inside it.
  Block,
  /// Bytes that begin as a block does with no block before them, which would read back as a block.
  Bytes,
}

/// A block line's key and value, split at its first `: `.
fn split_entry(line: &[u8]) -> Option<(&[u8], &[u8])> {
  let at = line
    .windows(KEY_SEPARATOR.len())
    .position(|window| window == KEY_SEPARATOR)?;
  Some((
    line.get(..at)?,
    line.get(at.saturating_add(KEY_SEPARATOR.len())..)?,
  ))
}

/// The index of a block's first line without `: `, as 7.2's `parsemeta` cuts it (`splitlines`), if one has none.
fn bad_line(block: &[u8]) -> Option<usize> {
  lines(block).position(|line| split_entry(line).is_none())
}

/// Where a marker first stands in `bytes`.
fn find_marker(bytes: &[u8]) -> Option<usize> {
  bytes
    .windows(MARKER.len())
    .position(|window| window == MARKER)
}

/// A file revision's text split into its block and its bytes, the block checked whole first.
pub fn split(text: &[u8]) -> Result<FileRevision<'_>, Refusal> {
  let Some(rest) = text.strip_prefix(MARKER) else {
    return Ok(FileRevision {
      metadata: None,
      bytes: text,
    });
  };
  let end = find_marker(rest).ok_or(Refusal::Unclosed)?;
  let block = rest.get(..end).unwrap_or_default();
  if let Some(index) = bad_line(block) {
    return Err(Refusal::Entry {
      line: to_u64(index),
    });
  }
  Ok(FileRevision {
    metadata: Some(block),
    bytes: rest
      .get(end.saturating_add(MARKER.len())..)
      .unwrap_or_default(),
  })
}

impl<'a> FileRevision<'a> {
  /// The block's entries, key and value, in order.
  pub fn entries(&self) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + use<'a> {
    lines(self.metadata.unwrap_or_default()).filter_map(split_entry)
  }

  /// The value of `key`, the last if the block names it twice, as 7.2's dictionary keeps it.
  pub fn value(&self, key: &[u8]) -> Option<&'a [u8]> {
    self
      .entries()
      .filter(|(known, _)| *known == key)
      .last()
      .map(|(_, value)| value)
  }

  /// The copy 7.2's `renamed` reads from this revision when `p1` is its first parent: none unless `p1` is null and the
  /// block names both `copy` and `copyrev`.
  pub fn copy(&self, p1: &Node) -> Result<Option<CopySource<'a>>, Refusal> {
    if *p1 != NULL_NODE {
      return Ok(None);
    }
    let (Some(path), Some(hex)) = (self.value(COPY), self.value(COPY_REV)) else {
      return Ok(None);
    };
    let node = from_hex(hex).ok_or(Refusal::Copy)?;
    Ok(Some(CopySource { path, node }))
  }

  /// The parents in the order 7.2 stores them (`_has_bad_parents`, applied to every incoming file revision unless
  /// `storage.revlog.issue6528.fix-incoming` is off): a revision with a block and one null parent keeps the null one
  /// first, and one without a block keeps it second. A censored revision is never reordered, as 7.2 reads none of it;
  /// its caller passes its parents as they came.
  pub fn parents_as_stored(&self, p1: Node, p2: Node) -> (Node, Node) {
    let swapped = if self.metadata.is_some() {
      p1 != NULL_NODE && p2 == NULL_NODE
    } else {
      p1 == NULL_NODE && p2 != NULL_NODE
    };
    if swapped { (p2, p1) } else { (p1, p2) }
  }

  /// Appends the text this split came from: the block between its markers when there is one, then the bytes; refused
  /// where [`split`] would read the text back as another.
  pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
    self.check()?;
    self.write(out);
    Ok(())
  }

  /// The length of the text [`FileRevision::encode`] writes, for a caller that charges it first; refused as it refuses.
  pub fn encoded_len(&self) -> Result<u64, EncodeRefusal> {
    self.check()?;
    let mut measure = Measure(0);
    self.write(&mut measure);
    Ok(measure.0)
  }

  /// Refuses a split [`split`] would read back as another.
  fn check(&self) -> Result<(), EncodeRefusal> {
    match self.metadata {
      None if self.bytes.starts_with(MARKER) => Err(EncodeRefusal::Bytes),
      Some(block) if find_marker(block).is_some() || bad_line(block).is_some() => {
        Err(EncodeRefusal::Block)
      }
      _ => Ok(()),
    }
  }

  /// Writes a split [`FileRevision::check`] passed.
  fn write(&self, out: &mut impl Out) {
    if let Some(block) = self.metadata {
      out.put(MARKER);
      out.put(block);
      out.put(MARKER);
    }
    out.put(self.bytes);
  }
}

/// The length of the text [`pack`] writes, for a caller that charges it first; refused as it refuses.
pub fn packed_len(metadata: &[(&[u8], &[u8])], bytes: &[u8]) -> Result<u64, EncodeRefusal> {
  check_entries(metadata)?;
  let mut measure = Measure(0);
  write_packed(metadata, bytes, &mut measure);
  Ok(measure.0)
}

/// Appends a file revision's text as 7.2's `filelog.add` writes it: a block of `metadata`'s entries sorted by key when
/// it has any, or an empty block when `bytes` begin as a block does, then the bytes.
pub fn pack(
  metadata: &[(&[u8], &[u8])],
  bytes: &[u8],
  out: &mut Vec<u8>,
) -> Result<(), EncodeRefusal> {
  check_entries(metadata)?;
  write_packed(metadata, bytes, out);
  Ok(())
}

/// Refuses an entry that would read back as other lines, and a key given twice.
fn check_entries(metadata: &[(&[u8], &[u8])]) -> Result<(), EncodeRefusal> {
  let breaks = |byte: &u8| *byte == NEWLINE || *byte == RETURN;
  for (index, (key, value)) in metadata.iter().enumerate() {
    let key_splits = key
      .windows(KEY_SEPARATOR.len())
      .any(|window| window == KEY_SEPARATOR);
    let closes = value.last() == Some(&MARKER_START);
    let again = metadata
      .get(..index)
      .unwrap_or_default()
      .iter()
      .any(|(earlier, _)| earlier == key);
    if key_splits || closes || again || key.iter().any(breaks) || value.iter().any(breaks) {
      return Err(EncodeRefusal::Entry { entry: index });
    }
  }
  Ok(())
}

/// Writes a text [`check_entries`] passed.
fn write_packed(metadata: &[(&[u8], &[u8])], bytes: &[u8], out: &mut impl Out) {
  if !metadata.is_empty() || bytes.starts_with(MARKER) {
    let mut sorted: Vec<&(&[u8], &[u8])> = metadata.iter().collect();
    sorted.sort_by(|left, right| left.0.cmp(right.0));
    out.put(MARKER);
    for (key, value) in sorted {
      out.put(key);
      out.put(KEY_SEPARATOR);
      out.put(value);
      out.put(&[NEWLINE]);
    }
    out.put(MARKER);
  }
  out.put(bytes);
}

#[cfg(test)]
mod tests {
  use super::{CopySource, EncodeRefusal, FileRevision, Refusal, pack, packed_len, split};
  use crate::NULL_NODE;
  use crate::fixtures::node;
  use proptest::prelude::*;

  const SOURCE_NODE: &[u8] = b"b6d7ec209a65c0afe68d5b7b14b68848981fd7fa";

  /// A-588: do split a copy's text, read its copy with a null first parent and with another, write the split back,
  /// and pack its entries again. Expect its block apart from its bytes, the copy's source and node with the null
  /// parent and no copy with the other (7.2's `renamed`), and the same text from both writers at the lengths they gave.
  #[test]
  fn a_copy_keeps_its_block_apart() {
    let text = [
      &b"\x01\ncopy: a\ncopyrev: "[..],
      SOURCE_NODE,
      b"\n\x01\na\nb\n",
    ]
    .concat();
    let read = split(&text).unwrap();
    assert_eq!(read.bytes, b"a\nb\n");
    let copy = read.copy(&NULL_NODE).unwrap().unwrap();
    assert_eq!(
      (copy.path, crate::fixtures::hex(&copy.node).as_bytes()),
      (&b"a"[..], SOURCE_NODE)
    );
    assert_eq!(read.copy(&[1; 20]), Ok(None));
    let mut again = Vec::new();
    read.encode(&mut again).unwrap();
    assert_eq!(again, text);
    assert_eq!(read.encoded_len(), Ok(u64::try_from(text.len()).unwrap()));
    let entries: Vec<(&[u8], &[u8])> = read.entries().collect();
    let mut out = Vec::new();
    pack(&entries, read.bytes, &mut out).unwrap();
    assert_eq!(out, text);
    assert_eq!(
      packed_len(&entries, read.bytes),
      Ok(u64::try_from(text.len()).unwrap())
    );
  }

  /// A-588: do pack bytes that begin as a block does, with no metadata, and plain bytes. Expect an empty block before
  /// the first, as 7.2 writes it, read back as an empty block and the same bytes; and no block before the second.
  #[test]
  fn bytes_that_begin_as_a_block_do_get_an_empty_one() {
    let bytes = b"\x01\nbegins as the block does\n";
    let mut out = Vec::new();
    pack(&[], bytes, &mut out).unwrap();
    assert_eq!(out, [&b"\x01\n\x01\n"[..], bytes].concat());
    assert_eq!(
      split(&out),
      Ok(FileRevision {
        metadata: Some(&[]),
        bytes
      })
    );
    let mut plain = Vec::new();
    pack(&[], b"plain\n", &mut plain).unwrap();
    assert_eq!(
      split(&plain),
      Ok(FileRevision {
        metadata: None,
        bytes: b"plain\n"
      })
    );
  }

  /// A-588 (7.2's `renamed`: "copy and copyrev occur in pairs. In rare cases due to old bugs, one can occur without the
  /// other"). Do: read the copy of blocks naming `copy` alone, `copyrev` alone, and both with `copyrev` short. Expect
  /// no copy for either half alone, as 7.2 reads them, and the short node refused.
  #[test]
  fn half_a_copy_is_no_copy() {
    let lone_copy = split(b"\x01\ncopy: a\n\x01\nx").unwrap();
    assert_eq!(lone_copy.copy(&NULL_NODE), Ok(None));
    let lone_rev = [&b"\x01\ncopyrev: "[..], SOURCE_NODE, b"\n\x01\nx"].concat();
    assert_eq!(split(&lone_rev).unwrap().copy(&NULL_NODE), Ok(None));
    let short = split(b"\x01\ncopy: a\ncopyrev: abc\n\x01\nx").unwrap();
    assert_eq!(short.copy(&NULL_NODE), Err(Refusal::Copy));
    assert_eq!(
      split(b"x").unwrap().copy(&NULL_NODE),
      Ok(None::<CopySource<'_>>)
    );
  }

  /// A-588 (7.2's `_has_bad_parents` and its doctest). Do: order the parents of a revision with a block and of one
  /// without, for each way one parent or none is null. Expect 7.2's order: with a block the null parent first, without
  /// one the null parent second, two non-null or two null parents as they came.
  #[test]
  fn parents_are_kept_in_7_2_s_order() {
    let with_block = split(b"\x01\n\x01\nx").unwrap();
    let without = split(b"x").unwrap();
    let (null, a, b) = (NULL_NODE, node(&"18".repeat(20)), node(&"42".repeat(20)));
    for (revision, (p1, p2), stored) in [
      (with_block, (null, null), (null, null)),
      (with_block, (null, b), (null, b)),
      (with_block, (a, null), (null, a)),
      (with_block, (a, b), (a, b)),
      (without, (null, null), (null, null)),
      (without, (null, b), (b, null)),
      (without, (a, null), (a, null)),
      (without, (a, b), (a, b)),
    ] {
      assert_eq!(revision.parents_as_stored(p1, p2), stored);
    }
  }

  /// AC-HG.9 (hostile input), A-588: do split a block never closed, a block line without `: `, and an empty line inside
  /// a block. Expect `Unclosed`, and `Entry` at the line's index.
  #[test]
  fn blocks_mercurial_would_fail_on_are_refused() {
    assert_eq!(split(b"\x01\ncopy: a\n"), Err(Refusal::Unclosed));
    assert_eq!(
      split(b"\x01\ncopy: a\nbroken\n\x01\n"),
      Err(Refusal::Entry { line: 1 })
    );
    assert_eq!(
      split(b"\x01\na: b\n\nc: d\n\x01\n"),
      Err(Refusal::Entry { line: 1 })
    );
  }

  /// A-588: do pack metadata whose key holds `: `, whose value holds a newline, whose value ends as a marker begins,
  /// and whose key is given twice; and write splits whose bytes begin as a block does with no block, whose block holds
  /// a closing marker, and whose block holds a line without `: `. Expect each refused naming it.
  #[test]
  fn entries_and_splits_that_would_not_read_back_are_not_written() {
    for (entries, entry) in [
      (vec![(&b"a: b"[..], &b"c"[..])], 0),
      (vec![(&b"a"[..], &b"c"[..]), (b"d", b"e\nf")], 1),
      (vec![(&b"a"[..], &b"c\x01"[..])], 0),
      (vec![(&b"k"[..], &b"1"[..]), (b"j", b"2"), (b"k", b"3")], 2),
    ] {
      assert_eq!(
        pack(&entries, b"", &mut Vec::new()),
        Err(EncodeRefusal::Entry { entry })
      );
      assert_eq!(
        packed_len(&entries, b""),
        Err(EncodeRefusal::Entry { entry })
      );
    }
    for (revision, refusal) in [
      (
        FileRevision {
          metadata: None,
          bytes: b"\x01\nx",
        },
        EncodeRefusal::Bytes,
      ),
      (
        FileRevision {
          metadata: Some(b"a: b\n\x01\nc: d\n"),
          bytes: b"",
        },
        EncodeRefusal::Block,
      ),
      (
        FileRevision {
          metadata: Some(b"no separator\n"),
          bytes: b"",
        },
        EncodeRefusal::Block,
      ),
    ] {
      let mut out = Vec::new();
      assert_eq!(revision.encode(&mut out), Err(refusal));
      assert_eq!(revision.encoded_len(), Err(refusal));
      assert!(out.is_empty());
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-588: do pack generated metadata and bytes, and split them again. Expect the same entries, sorted, and the
    /// same bytes.
    #[test]
    fn generated_revisions_split_back(
      generated in proptest::collection::btree_map("[a-z]{1,6}", "[ -~]{0,8}", 0..4),
      bytes in proptest::collection::vec(any::<u8>(), 0..32),
    ) {
      let entries: Vec<(&[u8], &[u8])> =
        generated.iter().map(|(key, value)| (key.as_bytes(), value.as_bytes())).collect();
      let mut text = Vec::new();
      pack(&entries, &bytes, &mut text).unwrap();
      let read = split(&text).unwrap();
      prop_assert_eq!(read.entries().collect::<Vec<_>>(), entries);
      prop_assert_eq!(read.bytes, bytes.as_slice());
    }

    /// AC-HG.9, A-588: do split arbitrary bytes, behind a marker or not, and write the split back. Expect a refusal,
    /// or exactly the same bytes at the length `encoded_len` gave; never a panic.
    #[test]
    fn arbitrary_bytes_split_and_write_back_or_are_refused(
      bytes in proptest::collection::vec(any::<u8>(), 0..64),
      behind_a_marker in any::<bool>(),
    ) {
      let text = if behind_a_marker { [super::MARKER, &bytes].concat() } else { bytes };
      if let Ok(read) = split(&text) {
        let mut out = Vec::new();
        prop_assert_eq!(read.encode(&mut out), Ok(()));
        prop_assert_eq!(read.encoded_len(), Ok(u64::try_from(text.len()).unwrap()));
        prop_assert_eq!(out, text.clone());
      }
    }
  }
}

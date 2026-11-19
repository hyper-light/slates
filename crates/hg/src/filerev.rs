//! A file revision's text (A-588; `utils/storageutil.py` `parsemeta` and `packmeta`, `filelog.py` `filelog.add` and
//! `renamed` [C]): the file's bytes, behind a metadata block when it has one: `\x01\n`, a `key: value` line per key
//! (sorted), and `\x01\n` again. 7.2 writes a block for a copy (`copy`, the source's path, and `copyrev`, the source's
//! file node in hexadecimal) and, empty, before bytes that themselves begin with `\x01\n`, so that they are not read as
//! one. The design keeps the block apart from the bytes (§4.17.3, the index), so a file's bytes are the same content
//! in every tree.
//!
//! A text is split in place; a block is checked whole first. 7.2 fails on a block that is never closed and on a line
//! without `: `, and these are refused by kind; so is a copy without its source's node, which 7.2's `renamed` fails on.

use crate::lines::lines;
use crate::out::{Measure, Out};
use crate::read::to_u64;
use crate::{NODE_LEN, Node};

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
pub struct Copy<'a> {
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
  /// A copy without its source's node, or one that is not forty hexadecimal digits.
  Copy,
}

/// Why a file revision could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A key holding `: ` or a line end, or a value holding a line end or ending as a marker begins, which would read
  /// back as other lines.
  Entry {
    /// The entry's index.
    entry: usize,
  },
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

/// A file revision's text split into its block and its bytes, the block checked whole first.
pub fn split(text: &[u8]) -> Result<FileRevision<'_>, Refusal> {
  let Some(rest) = text.strip_prefix(MARKER) else {
    return Ok(FileRevision {
      metadata: None,
      bytes: text,
    });
  };
  let end = rest
    .windows(MARKER.len())
    .position(|window| window == MARKER)
    .ok_or(Refusal::Unclosed)?;
  let block = rest.get(..end).unwrap_or_default();
  for (index, line) in lines(block).enumerate() {
    split_entry(line).ok_or(Refusal::Entry {
      line: to_u64(index),
    })?;
  }
  Ok(FileRevision {
    metadata: Some(block),
    bytes: rest.get(end.saturating_add(MARKER.len())..).unwrap_or_default(),
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

  /// The copy the block records, if it records one (`renamed`).
  pub fn copy(&self) -> Result<Option<Copy<'a>>, Refusal> {
    let Some(path) = self.value(COPY) else {
      return Ok(None);
    };
    let hex = self.value(COPY_REV).ok_or(Refusal::Copy)?;
    let (pairs, odd) = hex.as_chunks::<2>();
    if !odd.is_empty() || pairs.len() != NODE_LEN {
      return Err(Refusal::Copy);
    }
    let mut node = [0; NODE_LEN];
    for (byte, pair) in node.iter_mut().zip(pairs) {
      *byte = crate::quote::hex_pair(pair).ok_or(Refusal::Copy)?;
    }
    Ok(Some(Copy { path, node }))
  }
}

/// The length of the text [`pack`] writes, for a caller that charges it first; refused as it refuses.
pub fn packed_len(metadata: &[(&[u8], &[u8])], bytes: &[u8]) -> Result<u64, EncodeRefusal> {
  check(metadata)?;
  let mut measure = Measure(0);
  write(metadata, bytes, &mut measure);
  Ok(measure.0)
}

/// Appends a file revision's text as 7.2's `filelog.add` writes it: a block of `metadata`'s entries sorted by key when
/// it has any, or an empty block when `bytes` begin as a block does, then the bytes.
pub fn pack(metadata: &[(&[u8], &[u8])], bytes: &[u8], out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  check(metadata)?;
  write(metadata, bytes, out);
  Ok(())
}

/// Refuses an entry that would read back as other lines.
fn check(metadata: &[(&[u8], &[u8])]) -> Result<(), EncodeRefusal> {
  let breaks = |byte: &u8| *byte == NEWLINE || *byte == RETURN;
  for (index, (key, value)) in metadata.iter().enumerate() {
    let key_splits = key
      .windows(KEY_SEPARATOR.len())
      .any(|window| window == KEY_SEPARATOR);
    let closes = value.last() == Some(&MARKER_START);
    if key_splits || closes || key.iter().any(breaks) || value.iter().any(breaks) {
      return Err(EncodeRefusal::Entry { entry: index });
    }
  }
  Ok(())
}

/// Writes a text [`check`] passed.
fn write(metadata: &[(&[u8], &[u8])], bytes: &[u8], out: &mut impl Out) {
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
  use super::{Copy, EncodeRefusal, FileRevision, Refusal, pack, packed_len, split};
  use proptest::prelude::*;

  const SOURCE_NODE: &[u8] = b"b6d7ec209a65c0afe68d5b7b14b68848981fd7fa";

  /// A-588: do split a copy's text and pack it again. Expect its block apart from its bytes, the copy's source and
  /// node, and the same text at the length `packed_len` gave.
  #[test]
  fn a_copy_keeps_its_block_apart() {
    let text = [&b"\x01\ncopy: a\ncopyrev: "[..], SOURCE_NODE, b"\n\x01\na\nb\n"].concat();
    let read = split(&text).unwrap();
    assert_eq!(read.bytes, b"a\nb\n");
    let copy = read.copy().unwrap().unwrap();
    assert_eq!((copy.path, crate::fixtures::hex(&copy.node).as_bytes()), (&b"a"[..], SOURCE_NODE));
    let entries: Vec<(&[u8], &[u8])> = read.entries().collect();
    let mut out = Vec::new();
    pack(&entries, read.bytes, &mut out).unwrap();
    assert_eq!(out, text);
    assert_eq!(packed_len(&entries, read.bytes), Ok(u64::try_from(text.len()).unwrap()));
  }

  /// A-588: do pack bytes that begin as a block does, with no metadata, and plain bytes. Expect an empty block before
  /// the first, as 7.2 writes it, read back as an empty block and the same bytes; and no block before the second.
  #[test]
  fn bytes_that_begin_as_a_block_do_get_an_empty_one() {
    let bytes = b"\x01\nbegins as the block does\n";
    let mut out = Vec::new();
    pack(&[], bytes, &mut out).unwrap();
    assert_eq!(out, [&b"\x01\n\x01\n"[..], bytes].concat());
    assert_eq!(split(&out), Ok(FileRevision { metadata: Some(&[]), bytes }));
    let mut plain = Vec::new();
    pack(&[], b"plain\n", &mut plain).unwrap();
    assert_eq!(split(&plain), Ok(FileRevision { metadata: None, bytes: b"plain\n" }));
  }

  /// AC-HG.9 (hostile input), A-588: do split a block never closed and a block line without `: `, and read copies
  /// without a node and with a short one. Expect `Unclosed`, `Entry` at its line, and `Copy`.
  #[test]
  fn blocks_mercurial_would_fail_on_are_refused() {
    assert_eq!(split(b"\x01\ncopy: a\n"), Err(Refusal::Unclosed));
    assert_eq!(split(b"\x01\ncopy: a\nbroken\n\x01\n"), Err(Refusal::Entry { line: 1 }));
    let no_node = split(b"\x01\ncopy: a\n\x01\nx").unwrap();
    assert_eq!(no_node.copy(), Err(Refusal::Copy));
    let short = split(b"\x01\ncopy: a\ncopyrev: abc\n\x01\nx").unwrap();
    assert_eq!(short.copy(), Err(Refusal::Copy));
    assert_eq!(split(b"x").unwrap().copy(), Ok(None::<Copy<'_>>));
  }

  /// A-588: do pack metadata whose key holds `: `, whose value holds a newline, and whose value ends as a marker begins.
  /// Expect each refused naming it.
  #[test]
  fn entries_that_would_not_read_back_are_not_packed() {
    assert_eq!(pack(&[(b"a: b", b"c")], b"", &mut Vec::new()), Err(EncodeRefusal::Entry { entry: 0 }));
    assert_eq!(pack(&[(b"a", b"c"), (b"d", b"e\nf")], b"", &mut Vec::new()), Err(EncodeRefusal::Entry { entry: 1 }));
    assert_eq!(pack(&[(b"a", b"c\x01")], b"", &mut Vec::new()), Err(EncodeRefusal::Entry { entry: 0 }));
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
      let entries: Vec<(&[u8], &[u8])> = generated.iter().map(|(key, value)| (key.as_bytes(), value.as_bytes())).collect();
      let mut text = Vec::new();
      pack(&entries, &bytes, &mut text).unwrap();
      let read = split(&text).unwrap();
      prop_assert_eq!(read.entries().collect::<Vec<_>>(), entries);
      prop_assert_eq!(read.bytes, bytes.as_slice());
    }

    /// AC-HG.9, A-588: do split arbitrary bytes, pack what was read and split that. Expect a refusal at either step,
    /// or the same bytes and the same entries sorted by key, as 7.2's `packmeta` writes them; never a panic.
    #[test]
    fn arbitrary_bytes_split_and_pack_alike_or_are_refused(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
      if let Ok(read) = split(&bytes) {
        let mut entries: Vec<_> = read.entries().collect();
        let mut out = Vec::new();
        if pack(&entries, read.bytes, &mut out).is_ok() {
          let again = split(&out).unwrap();
          entries.sort_by(|left, right| left.0.cmp(right.0));
          prop_assert_eq!(again.entries().collect::<Vec<_>>(), entries);
          prop_assert_eq!(again.bytes, read.bytes);
        }
      }
    }
  }
}

//! A flat manifest's text (A-588; `manifest.py` `manifestdict`, `cext/manifest.c` `lazymanifest` \[C\]): one line per
//! file of the changeset's tree, sorted by path, each the path, a NUL, the file revision's node in forty lower-case
//! hexadecimal digits and its flag (none, `x` executable, `l` a symbolic link), and a newline:
//!
//! ```text
//! <path>\0<node hex>[x|l]\n
//! ```
//!
//! A manifest is checked whole and then read in place, so the text is its only cap; it writes back to exactly its
//! bytes. 7.2's C reader refuses lines out of order ("Manifest lines not in sorted order.") and a text that does not
//! end in a newline, as this module does.
//!
//! **Stricter than 7.2, on purpose.** A node in upper-case digits (7.2 reads either case, and writes lower), the tree
//! flag `t` (tree manifests are not served, §4.17.3) and any other flag are refused.

use core::cmp::Ordering;
use core::slice::Split;

use crate::Node;
use crate::node::{from_hex, write_hex};
use crate::out::{Measure, Out};
use crate::read::to_u64;

/// Format: the byte that ends each line.
const NEWLINE: u8 = b'\n';

/// Format: the byte between a path and its node.
const NUL: u8 = 0;

/// Format: the hexadecimal digits that write a node.
const NODE_HEX_LEN: usize = 40;

/// A file's flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
  /// A regular file.
  Regular,
  /// An executable file (`x`).
  Executable,
  /// A symbolic link, its text the target (`l`).
  Link,
}

impl Flag {
  /// The flag's byte, if it has one.
  fn byte(self) -> Option<u8> {
    match self {
      Self::Regular => None,
      Self::Executable => Some(b'x'),
      Self::Link => Some(b'l'),
    }
  }

  /// The flag a byte names.
  fn from_byte(byte: u8) -> Option<Self> {
    [Self::Executable, Self::Link]
      .into_iter()
      .find(|flag| flag.byte() == Some(byte))
  }
}

/// One line of a manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
  /// The file's path.
  pub path: &'a [u8],
  /// Its file revision's node.
  pub node: Node,
  /// Its flag.
  pub flag: Flag,
}

/// Why a manifest was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A text that does not end in a newline (7.2: "Manifest did not end in a newline.").
  Unterminated,
  /// A line that is not a path, a NUL, forty lower-case hexadecimal digits and a flag.
  Line {
    /// The line's index.
    line: u64,
  },
  /// A path not after the one before it (7.2: "Manifest lines not in sorted order.").
  Order {
    /// The line's index.
    line: u64,
  },
}

/// Why entries could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// An empty path, or one holding a NUL or a newline.
  Path {
    /// The entry's index.
    entry: usize,
  },
  /// A path not after the one before it.
  Order {
    /// The entry's index.
    entry: usize,
  },
}

/// The entries of a manifest, in order.
#[derive(Clone)]
pub struct Entries<'a> {
  /// The lines left, each without its newline.
  lines: Split<'a, u8, fn(&u8) -> bool>,
}

/// Whether a byte ends a line.
fn is_newline(byte: &u8) -> bool {
  *byte == NEWLINE
}

/// The lines of a newline-terminated text, its last newline taken off first.
fn lines(text: &[u8]) -> Split<'_, u8, fn(&u8) -> bool> {
  let separator: fn(&u8) -> bool = is_newline;
  text
    .strip_suffix(&[NEWLINE])
    .unwrap_or(text)
    .split(separator)
}

/// The entries of a manifest's text, the text checked whole first.
pub fn decode(text: &[u8]) -> Result<Entries<'_>, Refusal> {
  if text.is_empty() {
    return Ok(Entries { lines: lines(text) });
  }
  if !text.ends_with(&[NEWLINE]) {
    return Err(Refusal::Unterminated);
  }
  let mut previous: Option<&[u8]> = None;
  for (index, line) in lines(text).enumerate() {
    let entry = parse(line).ok_or(Refusal::Line {
      line: to_u64(index),
    })?;
    if previous.is_some_and(|before| before >= entry.path) {
      return Err(Refusal::Order {
        line: to_u64(index),
      });
    }
    previous = Some(entry.path);
  }
  Ok(Entries { lines: lines(text) })
}

/// Validates a manifest `text` produced by applying `delta` (`crate::delta::apply`) to a base
/// manifest that already passed [`decode`]: returns `Ok(())` if and only if [`decode(text)`]
/// returns `Ok`, and on any invalidity falls back to [`decode(text)`] so the returned [`Refusal`]
/// carries the exact line number.
///
/// Proof of equivalence: every line completely outside the expanded intervals `[lo..hi)` was copied
/// verbatim with its bounding newlines from the already-valid base manifest, so it parses and is
/// strictly ordered relative to its untouched neighbors. Expanding each hunk outward to include the
/// full immediately preceding line and the full immediately following line guarantees every
/// modified, split, joined, or newly adjacent consecutive line pair lies inside one checked
/// interval.
pub fn validate_delta(text: &[u8], delta: &[u8]) -> Result<(), Refusal> {
  if text.is_empty() {
    return Ok(());
  }
  if !text.ends_with(&[NEWLINE]) {
    return Err(Refusal::Unterminated);
  }
  let mut produced: usize = 0;
  let mut base_at: usize = 0;
  let mut open: Option<(usize, usize)> = None;
  for hunk in crate::delta::hunks(delta) {
    let Ok(hunk) = hunk else {
      return decode(text).map(|_| ());
    };
    let start = usize::try_from(hunk.start).unwrap_or(usize::MAX);
    let end = usize::try_from(hunk.end).unwrap_or(usize::MAX);
    let new_start = produced.saturating_add(start.saturating_sub(base_at));
    let new_end = new_start.saturating_add(hunk.data.len());
    produced = new_end;
    base_at = end;
    let (lo, hi) = delta_check_span(text, new_start.min(text.len()), new_end.min(text.len()));
    match open {
      Some((open_lo, open_hi)) if lo <= open_hi => {
        open = Some((open_lo, open_hi.max(hi)));
      }
      Some((open_lo, open_hi)) => {
        if !validate_span(text, open_lo, open_hi) {
          return decode(text).map(|_| ());
        }
        open = Some((lo, hi));
      }
      None => open = Some((lo, hi)),
    }
  }
  if let Some((open_lo, open_hi)) = open
    && !validate_span(text, open_lo, open_hi)
  {
    return decode(text).map(|_| ());
  }
  Ok(())
}

/// Start of the line containing `at` (byte after the preceding `\n`, or `0`).
fn line_start_of(text: &[u8], at: usize) -> usize {
  text
    .get(..at)
    .and_then(|before| before.iter().rposition(is_newline))
    .map_or(0, |newline| newline.saturating_add(1))
}

/// Byte immediately after the `\n` ending the line at or after `at` (or `text.len()`).
fn line_end_after(text: &[u8], at: usize) -> usize {
  text
    .get(at..)
    .and_then(|after| after.iter().position(is_newline))
    .map_or(text.len(), |newline| {
      at.saturating_add(newline).saturating_add(1)
    })
}

/// Expands a delta hunk's output range `[new_start..new_end)` in newline-terminated `text` to
/// include the full preceding line (`lo`) and the full following line (`hi`).
fn delta_check_span(text: &[u8], new_start: usize, new_end: usize) -> (usize, usize) {
  let lo = if new_start == 0 {
    0
  } else if text.get(new_start.saturating_sub(1)) == Some(&NEWLINE) {
    line_start_of(text, new_start.saturating_sub(1))
  } else {
    let modified_start = line_start_of(text, new_start);
    if modified_start == 0 {
      0
    } else {
      line_start_of(text, modified_start.saturating_sub(1))
    }
  };
  let hi = if new_end >= text.len() {
    text.len()
  } else {
    let first_end = line_end_after(text, new_end);
    if first_end < text.len() {
      line_end_after(text, first_end)
    } else {
      text.len()
    }
  };
  (lo, hi)
}

/// Verifies every line in newline-aligned `text[lo..hi]` parses and is strictly path-sorted.
fn validate_span(text: &[u8], lo: usize, hi: usize) -> bool {
  let Some(slice) = text.get(lo..hi) else {
    return false;
  };
  if slice.is_empty() {
    return true;
  }
  let mut previous: Option<&[u8]> = None;
  for line in lines(slice) {
    let Some(entry) = parse(line) else {
      return false;
    };
    if previous.is_some_and(|before| before >= entry.path) {
      return false;
    }
    previous = Some(entry.path);
  }
  true
}

/// The entry of `path` in a manifest's text that [`decode`] has checked, found by halving the text (A-680): its lines
/// are sorted by path, so the line holding the middle byte of what is left says which half holds `path`, as 7.2's pure
/// reader halves its lines (`manifest.py` `_lazymanifest.bsearch` \[C\], over an index of line starts it builds first;
/// here a line's start is found from the byte, so nothing is built). `None` where the manifest names no such path. Over
/// a text [`decode`] refuses, the answer is unspecified, and never a panic.
pub fn find<'a>(text: &'a [u8], path: &[u8]) -> Option<Entry<'a>> {
  let (mut low, mut high) = (0_usize, text.len());
  while low < high {
    let half = high.saturating_sub(low).checked_div(2).unwrap_or_default();
    let middle = low.saturating_add(half);
    let start = text
      .get(..middle)?
      .iter()
      .rposition(is_newline)
      .map_or(0, |newline| newline.saturating_add(1));
    let end = text
      .get(start..)?
      .iter()
      .position(is_newline)
      .map_or(text.len(), |newline| start.saturating_add(newline));
    let entry = parse(text.get(start..end)?)?;
    match entry.path.cmp(path) {
      Ordering::Equal => return Some(entry),
      Ordering::Less => low = end.saturating_add(1),
      Ordering::Greater => high = start,
    }
  }
  None
}

/// One line's entry, or `None` for a line that is not one.
pub(crate) fn parse(line: &[u8]) -> Option<Entry<'_>> {
  let nul = line.iter().position(|byte| *byte == NUL)?;
  let path = line.get(..nul).filter(|path| !path.is_empty())?;
  let rest = line.get(nul.saturating_add(1)..)?;
  let hex = rest.get(..NODE_HEX_LEN)?;
  let flag = match rest.get(NODE_HEX_LEN..)? {
    [] => Flag::Regular,
    [byte] => Flag::from_byte(*byte)?,
    _ => return None,
  };
  if hex.iter().any(u8::is_ascii_uppercase) {
    return None;
  }
  let node = from_hex(hex)?;
  Some(Entry { path, node, flag })
}

impl<'a> Iterator for Entries<'a> {
  type Item = Entry<'a>;

  fn next(&mut self) -> Option<Entry<'a>> {
    parse(self.lines.next()?)
  }
}

/// The length of a manifest of `entries`, for a caller that charges it first; refused as [`encode`] refuses.
pub fn encoded_len(entries: &[Entry<'_>]) -> Result<u64, EncodeRefusal> {
  check(entries)?;
  let mut measure = Measure(0);
  write(entries, &mut measure);
  Ok(measure.0)
}

/// Appends the manifest of `entries`, after checking their paths are sorted, distinct and writable.
pub fn encode(entries: &[Entry<'_>], out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
  check(entries)?;
  write(entries, out);
  Ok(())
}

/// Refuses an entry whose path would not read back, or is not after the one before it.
fn check(entries: &[Entry<'_>]) -> Result<(), EncodeRefusal> {
  let mut previous: Option<&[u8]> = None;
  for (index, entry) in entries.iter().enumerate() {
    if entry.path.is_empty()
      || entry
        .path
        .iter()
        .any(|byte| *byte == NUL || *byte == NEWLINE)
    {
      return Err(EncodeRefusal::Path { entry: index });
    }
    if previous.is_some_and(|before| before >= entry.path) {
      return Err(EncodeRefusal::Order { entry: index });
    }
    previous = Some(entry.path);
  }
  Ok(())
}

/// Writes entries [`check`] passed.
fn write(entries: &[Entry<'_>], out: &mut impl Out) {
  for entry in entries {
    out.put(entry.path);
    out.put(&[NUL]);
    write_hex(&entry.node, out);
    if let Some(flag) = entry.flag.byte() {
      out.put(&[flag]);
    }
    out.put(&[NEWLINE]);
  }
}

#[cfg(test)]
mod tests {
  use super::{EncodeRefusal, Entry, Flag, Refusal, decode, encode, encoded_len, find};
  use proptest::prelude::*;

  const NODE_HEX: &[u8] = b"b6d7ec209a65c0afe68d5b7b14b68848981fd7fa";

  /// The text of a manifest of `paths`, each with a node of its own index and every third executable.
  fn manifest_of(paths: &[&'static [u8]]) -> (Vec<u8>, Vec<Entry<'static>>) {
    let flags = [Flag::Regular, Flag::Link, Flag::Executable];
    let entries: Vec<Entry<'static>> = paths
      .iter()
      .enumerate()
      .map(|(index, path)| Entry {
        path,
        node: [u8::try_from(index).unwrap(); 20],
        flag: flags[index % flags.len()],
      })
      .collect();
    let mut text = Vec::new();
    encode(&entries, &mut text).unwrap();
    (text, entries)
  }

  /// A-680: do find every path of a manifest whose paths are each other's prefixes and neighbours (`a`, `a/b`, `ab`),
  /// then paths it does not name: before its first, between two, after its last, a prefix of one, and empty. Expect
  /// each named path's own entry, its node and flag, and nothing for the rest; an empty manifest names nothing.
  #[test]
  fn a_path_is_found_by_halving_and_an_absent_one_is_not() {
    let (text, entries) = manifest_of(&[b"a", b"a/b", b"ab", b"b", b"dir/x", b"z"]);
    for entry in &entries {
      assert_eq!(find(&text, entry.path), Some(*entry), "{:?}", entry.path);
    }
    for absent in [&b""[..], b"0", b"a/", b"a/a", b"aa", b"c", b"dir", b"zz"] {
      assert_eq!(find(&text, absent), None, "{absent:?}");
    }
    assert_eq!(find(b"", b"a"), None);
  }

  /// A-588: do read a manifest of a regular file, a link and an executable, and write it back. Expect each entry with
  /// its node and flag, and the same bytes at the length `encoded_len` gave.
  #[test]
  fn a_manifest_reads_and_writes_back() {
    let text = [
      &b"a\0"[..],
      NODE_HEX,
      b"\nlink\0",
      NODE_HEX,
      b"l\nrun\0",
      NODE_HEX,
      b"x\n",
    ]
    .concat();
    let entries: Vec<Entry<'_>> = decode(&text).unwrap().collect();
    let flags: Vec<(&[u8], Flag)> = entries
      .iter()
      .map(|entry| (entry.path, entry.flag))
      .collect();
    let expected: [(&[u8], Flag); 3] = [
      (b"a", Flag::Regular),
      (b"link", Flag::Link),
      (b"run", Flag::Executable),
    ];
    assert_eq!(flags, expected);
    assert_eq!(crate::fixtures::hex(&entries[0].node).as_bytes(), NODE_HEX);
    let mut out = Vec::new();
    encode(&entries, &mut out).unwrap();
    assert_eq!(out, text);
    assert_eq!(
      encoded_len(&entries),
      Ok(u64::try_from(text.len()).unwrap())
    );
    assert_eq!(decode(b"").unwrap().count(), 0);
  }

  /// AC-HG.9 (hostile input), A-588: do read a text without its last newline, a line without its NUL, a short node,
  /// an upper-case node, the tree flag, two flags, and paths out of order or repeated. Expect each refused by its
  /// kind and line.
  #[test]
  fn manifests_mercurial_would_refuse_are_refused() {
    let line = |path: &[u8], node: &[u8], flag: &[u8]| [path, b"\0", node, flag, b"\n"].concat();
    let good = line(b"a", NODE_HEX, b"");
    assert_eq!(
      decode(&good[..good.len() - 1]).err(),
      Some(Refusal::Unterminated)
    );
    let upper = NODE_HEX.to_ascii_uppercase();
    for bad in [
      b"a".to_vec(),
      line(b"a", &NODE_HEX[..39], b""),
      line(b"a", &upper, b""),
      line(b"a", NODE_HEX, b"t"),
      line(b"a", NODE_HEX, b"xl"),
    ] {
      let text = if bad.ends_with(b"\n") {
        bad
      } else {
        [bad, b"\n".to_vec()].concat()
      };
      assert_eq!(decode(&text).err(), Some(Refusal::Line { line: 0 }));
    }
    for second in [&b"a"[..], b"0"] {
      let text = [good.clone(), line(second, NODE_HEX, b"")].concat();
      assert_eq!(decode(&text).err(), Some(Refusal::Order { line: 1 }));
    }
  }

  /// A-588: do write entries with a path holding a newline, and out of order. Expect each refused naming the entry.
  #[test]
  fn entries_that_would_not_read_back_are_not_written() {
    let entry = |path: &'static [u8]| Entry {
      path,
      node: [1; 20],
      flag: Flag::Regular,
    };
    assert_eq!(
      encode(&[entry(b"a\nb")], &mut Vec::new()),
      Err(EncodeRefusal::Path { entry: 0 })
    );
    assert_eq!(
      encode(&[entry(b"b"), entry(b"a")], &mut Vec::new()),
      Err(EncodeRefusal::Order { entry: 1 })
    );
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-588: do write manifests of generated paths, nodes and flags, and read them back. Expect the same entries.
    #[test]
    fn generated_manifests_read_back(
      generated in proptest::collection::btree_map("[a-z/._-]{1,10}", (any::<[u8; 20]>(), 0_u8..3), 0..6),
    ) {
      let flags = [Flag::Regular, Flag::Executable, Flag::Link];
      let entries: Vec<Entry<'_>> = generated.iter().map(|(path, (node, flag))| Entry {
        path: path.as_bytes(),
        node: *node,
        flag: flags[usize::from(*flag)],
      }).collect();
      let mut text = Vec::new();
      encode(&entries, &mut text).unwrap();
      prop_assert_eq!(decode(&text).unwrap().collect::<Vec<_>>(), entries);
    }

    /// AC-HG.9, A-588: do change one byte of a manifest 7.2 would write to any value, or cut the manifest at any byte.
    /// Expect a refusal, or entries that write back to exactly the changed bytes; never a panic.
    #[test]
    fn changed_manifests_read_back_or_are_refused(
      generated in proptest::collection::btree_map("[a-z/._-]{1,10}", (any::<[u8; 20]>(), 0_u8..3), 1..6),
      at in any::<proptest::sample::Index>(),
      byte in any::<u8>(),
      cut in any::<bool>(),
    ) {
      let flags = [Flag::Regular, Flag::Executable, Flag::Link];
      let entries: Vec<Entry<'_>> = generated.iter().map(|(path, (node, flag))| Entry {
        path: path.as_bytes(),
        node: *node,
        flag: flags[usize::from(*flag)],
      }).collect();
      let mut text = Vec::new();
      encode(&entries, &mut text).unwrap();
      let at = at.index(text.len());
      if cut {
        text.truncate(at);
      } else {
        text[at] = byte;
      }
      if let Ok(read) = decode(&text) {
        let read: Vec<_> = read.collect();
        let mut out = Vec::new();
        encode(&read, &mut out).unwrap();
        prop_assert_eq!(out, text);
      }
    }

    /// A-680 (oracle): do look up generated paths, and every path, in manifests of generated paths, nodes and flags.
    /// Expect what a walk of every line finds: the entry of a path the manifest names, nothing for one it does not.
    #[test]
    fn halving_finds_what_a_walk_of_every_line_finds(
      generated in proptest::collection::btree_map(
        "[a-c/._-]{1,4}",
        (any::<[u8; 20]>(), 0_u8..3),
        0..24,
      ),
      probes in proptest::collection::vec("[a-c/._-]{0,5}", 0..8),
    ) {
      let flags = [Flag::Regular, Flag::Executable, Flag::Link];
      let entries: Vec<Entry<'_>> = generated.iter().map(|(path, (node, flag))| Entry {
        path: path.as_bytes(),
        node: *node,
        flag: flags[usize::from(*flag)],
      }).collect();
      let mut text = Vec::new();
      encode(&entries, &mut text).unwrap();
      let walked = |path: &[u8]| decode(&text).unwrap().find(|entry| entry.path == path);
      for path in generated.keys().map(String::as_str).chain(probes.iter().map(String::as_str)) {
        prop_assert_eq!(find(&text, path.as_bytes()), walked(path.as_bytes()), "{:?}", path);
      }
    }

    /// AC-HG.9, A-680: do look up a path in arbitrary bytes. Expect nothing, or an entry of exactly that path; never a
    /// panic, never a loop.
    #[test]
    fn halving_arbitrary_bytes_finds_only_the_path_asked(
      bytes in proptest::collection::vec(any::<u8>(), 0..96),
      path in proptest::collection::vec(any::<u8>(), 0..6),
    ) {
      if let Some(entry) = find(&bytes, &path) {
        prop_assert_eq!(entry.path, &path[..]);
      }
    }

    /// AC-HG.9, A-588: do read arbitrary bytes as a manifest. Expect entries that write back to the same bytes, or a
    /// refusal; never a panic.
    #[test]
    fn arbitrary_bytes_read_back_or_are_refused(bytes in proptest::collection::vec(any::<u8>(), 0..96)) {
      if let Ok(entries) = decode(&bytes) {
        let entries: Vec<_> = entries.collect();
        let mut out = Vec::new();
        encode(&entries, &mut out).unwrap();
        prop_assert_eq!(out, bytes);
      }
    }

    /// Oracle: `validate_delta` over any delta applied to a valid base manifest must agree
    /// identically (Ok vs exact Err variant and line number) with full `decode`, covering both
    /// valid manifest edits and arbitrary hostile mutations (newline splits/joins, order
    /// inversions, duplicate paths, invalid hex/flags, truncation).
    #[test]
    fn validate_delta_matches_decode_oracle_on_valid_and_hostile_deltas(
      base_map in proptest::collection::btree_map(
        "[a-f/._-]{1,6}",
        (any::<[u8; 20]>(), 0_u8..3),
        0..28,
      ),
      ops in proptest::collection::vec(
        (0_usize..1000, 0_usize..40, proptest::collection::vec(any::<u8>(), 0..64)),
        0..6,
      ),
      valid_edits in proptest::collection::btree_map(
        "[a-f/._-]{1,6}",
        proptest::option::of((any::<[u8; 20]>(), 0_u8..3)),
        0..6,
      ),
    ) {
      let flags = [Flag::Regular, Flag::Executable, Flag::Link];
      let build_manifest = |map: &std::collections::BTreeMap<String, ([u8; 20], u8)>| {
        let entries: Vec<Entry<'_>> = map
          .iter()
          .map(|(path, (node, flag))| Entry {
            path: path.as_bytes(),
            node: *node,
            flag: flags[usize::from(*flag)],
          })
          .collect();
        let mut out = Vec::new();
        encode(&entries, &mut out).unwrap();
        out
      };
      let base = build_manifest(&base_map);

      // 1. Valid manifest edit delta via `delta::encode_lines`.
      let mut edited_map = base_map.clone();
      for (path, change) in valid_edits {
        match change {
          Some(val) => {
            edited_map.insert(path, val);
          }
          None => {
            edited_map.remove(&path);
          }
        }
      }
      let target = build_manifest(&edited_map);
      let mut line_delta = Vec::new();
      crate::delta::encode_lines(&base, &target, &mut line_delta).unwrap();
      prop_assert_eq!(
        super::validate_delta(&target, &line_delta),
        decode(&target).map(|_| ())
      );

      // 2. Arbitrary hostile multi-hunk delta against `base`.
      let mut sorted_ops: Vec<(usize, usize, Vec<u8>)> = Vec::new();
      let mut cursor = 0_usize;
      for (skip, del, data) in ops {
        if cursor > base.len() {
          break;
        }
        let remaining = base.len().saturating_sub(cursor);
        let start = cursor.saturating_add(skip % remaining.saturating_add(1));
        let end = start.saturating_add(del).min(base.len());
        sorted_ops.push((start, end, data));
        cursor = end;
      }
      let mut raw_delta = Vec::new();
      for (start, end, data) in &sorted_ops {
        let hunk = crate::delta::Hunk {
          start: u32::try_from(*start).unwrap(),
          end: u32::try_from(*end).unwrap(),
          data,
        };
        crate::delta::encode_hunk(&hunk, &mut raw_delta).unwrap();
      }
      let mut room = crate::Admission::new(1 << 20);
      let patched = crate::delta::apply(&base, &raw_delta, &mut room).unwrap();
      prop_assert_eq!(
        super::validate_delta(&patched, &raw_delta),
        decode(&patched).map(|_| ())
      );
    }
  }
}

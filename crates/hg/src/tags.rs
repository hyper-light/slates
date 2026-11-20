//! A global tag's node as 7.2 finds it (`tags.py` `findglobaltags` \[C\], read from 7.2's source on 2026-11-20): the
//! `.hgtags` file of each head of the view read in turn, oldest head first, each distinct file revision once
//! (`_filterfnodes`), and each file's word on a tag folded into what the earlier ones said (`_updatetags`). A name
//! resolved to a revision tries a bookmark, then a tag, then a branch (`scmutil.revsymbol` over `namespaces`), so the
//! served view's `lookup` and an owner's `--from` (A-575) both ask here.
//!
//! **One file** (`_readtaghist`): its lines split where Python's `bytes.splitlines` splits them (`\n`, `\r\n`, `\r`);
//! an empty line skipped; a line split at its first space into a node in hexadecimal and a name, the name stripped of
//! ASCII white space; a line with no space, or whose node is no even run of hexadecimal digits, skipped. A tag's word in
//! one file is its last line's node, and its history the nodes its earlier lines named, in file order.
//!
//! **Folding** (`_updatetags`): a tag the earlier files did not name takes this file's word. Otherwise this file, being
//! nearer the tip, wins, unless the earlier word supersedes it: the two differ, this file's node is in the earlier
//! history, and either the earlier node is not in this file's history or the earlier history is longer. The histories
//! are then joined, this file's first and then each earlier node it lacked. A tag whose word is the null node is
//! deleted (`localrepo._findtags`).
//!
//! **Only the name asked.** A lookup needs one tag, and each tag folds apart from every other, so only the asked name's
//! lines are kept: the cost is one pass over each file and the asked tag's history, never a map of every tag. The
//! histories are compared through sets, so a hostile file of one tag on many lines costs its length, not its square.

use std::collections::HashSet;

use crate::NULL_NODE;

/// A tag's word and history as the files read so far say them (module docs, "Folding"): its node's bytes (an even run
/// of hexadecimal digits decoded, so of any length, as 7.2's `bin` decodes them) and the nodes it named before.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tag {
  /// The name.
  name: Vec<u8>,
  /// Its word and history, once a file named it.
  word: Option<(Vec<u8>, Vec<Vec<u8>>)>,
}

impl Tag {
  /// The tag `name`, no file read yet.
  pub fn new(name: &[u8]) -> Self {
    Tag {
      name: name.to_vec(),
      word: None,
    }
  }

  /// `text`, the next `.hgtags` file toward the tip, folded in (module docs).
  pub fn read(&mut self, text: &[u8]) {
    let mut history: Vec<Vec<u8>> = lines(text)
      .filter_map(entry)
      .filter(|(_, name)| *name == self.name.as_slice())
      .map(|(node, _)| node)
      .collect();
    let Some(mut node) = history.pop() else {
      return;
    };
    if let Some((earlier, earlier_history)) = self.word.take() {
      let own: HashSet<&[u8]> = history.iter().map(Vec::as_slice).collect();
      let superseded = earlier != node
        && earlier_history.contains(&node)
        && (!own.contains(earlier.as_slice()) || earlier_history.len() > history.len());
      let lacked: Vec<Vec<u8>> = earlier_history
        .iter()
        .filter(|earlier| !own.contains(earlier.as_slice()))
        .cloned()
        .collect();
      if superseded {
        node = earlier;
      }
      history.extend(lacked);
    }
    self.word = Some((node, history));
  }

  /// The node the tag names after the files read; none where none named it or its word is the null node.
  pub fn node(&self) -> Option<&[u8]> {
    let (node, _) = self.word.as_ref()?;
    (node.as_slice() != NULL_NODE.as_slice()).then_some(node.as_slice())
  }
}

/// `text`'s lines as Python's `bytes.splitlines` splits them: at `\n`, `\r\n` and `\r`, no line end kept, and no empty
/// last line after a final line end.
fn lines(text: &[u8]) -> impl Iterator<Item = &[u8]> {
  let mut rest = text;
  core::iter::from_fn(move || {
    if rest.is_empty() {
      return None;
    }
    let end = rest
      .iter()
      .position(|byte| *byte == b'\n' || *byte == b'\r')
      .unwrap_or(rest.len());
    let (line, after) = rest.split_at(end);
    rest = match after {
      [b'\r', b'\n', tail @ ..] | [b'\r' | b'\n', tail @ ..] => tail,
      _ => after,
    };
    Some(line)
  })
}

/// One line's node and name (module docs, "One file"); none for a line 7.2 skips.
fn entry(line: &[u8]) -> Option<(Vec<u8>, &[u8])> {
  if line.is_empty() {
    return None;
  }
  let space = line.iter().position(|byte| *byte == b' ')?;
  let (hex, name) = line.split_at(space);
  let name = strip(name.get(1..)?);
  Some((unhexlify(hex)?, name))
}

/// Format: the bytes Python's `bytes.strip` removes from either end: ASCII white space, the vertical tab among it
/// (which Rust's `trim_ascii` keeps).
const WHITE_SPACE: &[u8] = b" \t\n\r\x0b\x0c";

/// `name` without the white space Python's `bytes.strip` removes from either end.
fn strip(name: &[u8]) -> &[u8] {
  let start = name
    .iter()
    .position(|byte| !WHITE_SPACE.contains(byte))
    .unwrap_or(name.len());
  let end = name
    .iter()
    .rposition(|byte| !WHITE_SPACE.contains(byte))
    .map_or(start, |last| last.saturating_add(1));
  name.get(start..end).unwrap_or_default()
}

/// `hex` decoded as Python's `binascii.unhexlify` decodes it: an even number of hexadecimal digits in either case.
fn unhexlify(hex: &[u8]) -> Option<Vec<u8>> {
  let (pairs, []) = hex.as_chunks::<2>() else {
    return None;
  };
  pairs
    .iter()
    .map(|[high, low]| {
      let high = char::from(*high).to_digit(16)?;
      let low = char::from(*low).to_digit(16)?;
      u8::try_from(high.checked_mul(16)?.checked_add(low)?).ok()
    })
    .collect()
}

#[cfg(test)]
#[path = "tags_tests.rs"]
mod tests;

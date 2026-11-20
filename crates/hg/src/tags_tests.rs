//! Tests of a tag's fold against 7.2's own `tags._readtags` and `tags._updatetags`, run over 300 generated sequences of
//! `.hgtags` files (`tags_vectors.txt`, written on 2026-11-20 by `uvx --from mercurial==7.2 python -I tags_vectors.py`,
//! a seeded generator kept beside the slates checkouts in `~/.cache/w-hg-scripts`): lines naming three nodes and the
//! null node under `a`, `b` and `a` wrapped in white space Python strips (a vertical tab among it), lines 7.2 skips (an
//! empty one, one with no space, an odd or non-hexadecimal node), each file's lines ended by `\n`, `\r\n` or `\r`.

use super::Tag;

/// Golden: each case's word on `a` and on `b` as 7.2 folds it, then its files in hexadecimal.
const VECTORS: &str = include_str!("tags_vectors.txt");

/// `hex` decoded; the vectors' own format, so a bad digit fails the test.
fn decoded(hex: &str) -> Vec<u8> {
  hex
    .as_bytes()
    .chunks(2)
    .map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap())
    .collect()
}

/// A tag's node after `files`, as the vectors write it.
fn word(name: &[u8], files: &[Vec<u8>]) -> String {
  let mut tag = Tag::new(name);
  for file in files {
    tag.read(file);
  }
  tag.node().map_or_else(
    || "-".to_owned(),
    |node| node.iter().map(|byte| format!("{byte:02x}")).collect(),
  )
}

/// The tag's node were the last file naming it simply to win: what the vectors must tell apart from 7.2's fold.
fn last_naming_wins(name: &[u8], files: &[Vec<u8>]) -> String {
  files
    .iter()
    .rev()
    .find_map(|file| {
      let mut tag = Tag::new(name);
      tag.read(file);
      tag
        .word
        .as_ref()
        .map(|_| word(name, core::slice::from_ref(file)))
    })
    .unwrap_or_else(|| "-".to_owned())
}

/// A-575 (`--from` resolves a tag), §4.17.3 `lookup`: do fold each case's files for `a` and `b`. Expect every word
/// equal to 7.2's; and, so the vectors are no vacuous oracle, some case where an earlier file's word supersedes a
/// later one's (7.2's rank rule), some where white space around a name is stripped to `a`, and some where the null node
/// deletes the tag.
#[test]
fn a_tags_word_folds_its_files_as_7_2_folds_them() {
  let (mut superseded, mut cases) = (0, 0);
  for (number, case) in VECTORS.lines().enumerate() {
    let mut fields = case.split(' ');
    let (a, b) = (fields.next().unwrap(), fields.next().unwrap());
    let files: Vec<Vec<u8>> = fields
      .next()
      .unwrap_or_default()
      .split(',')
      .map(decoded)
      .collect();
    assert_eq!(word(b"a", &files), a, "case {number}: a");
    assert_eq!(word(b"b", &files), b, "case {number}: b");
    if word(b"a", &files) != last_naming_wins(b"a", &files) {
      superseded += 1;
    }
    cases += 1;
  }
  assert_eq!(cases, 300);
  assert!(superseded > 0, "no case exercises the rank rule");
  let mut stripped = Tag::new(b"a");
  stripped.read(b"0101010101010101010101010101010101010101 \x0ba\x0c \r");
  assert_eq!(stripped.node(), Some([1u8; 20].as_slice()));
  let mut deleted = Tag::new(b"a");
  deleted.read(b"0101010101010101010101010101010101010101 a\n");
  deleted.read(
    b"0101010101010101010101010101010101010101 a\n0000000000000000000000000000000000000000 a\n",
  );
  assert_eq!(deleted.node(), None);
}

/// A-575, hostile input: do fold a file of 200,000 lines naming one tag, alternating two nodes, then a second file that
/// names it once. Expect the fold to answer (its histories compared through sets, so the file costs its length) with
/// the tip-most file's word, which the earlier history does not supersede, since the later file's history is empty and
/// the earlier word is the same node.
#[test]
fn a_tag_named_on_many_lines_folds_in_one_pass() {
  let mut long = Vec::new();
  for line in 0..200_000u32 {
    let node = if line % 2 == 0 { "01" } else { "02" };
    long.extend_from_slice(node.repeat(20).as_bytes());
    long.extend_from_slice(b" a\n");
  }
  let mut tag = Tag::new(b"a");
  tag.read(&long);
  assert_eq!(tag.node(), Some([2u8; 20].as_slice()));
  tag.read(format!("{} a\n", "02".repeat(20)).as_bytes());
  assert_eq!(tag.node(), Some([2u8; 20].as_slice()));
}

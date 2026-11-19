//! The fold held to utf8proc 2.12.0, the library stock `svnserve` 1.15.0 links on the Mac Studio
//! (`crates/svn/fixtures/fold/make-fold.py`), and its tables to one Unicode version.

use super::*;
use std::collections::BTreeMap;

/// utf8proc's folds: every scalar value whose fold is not itself, then names of several code points.
const ORACLE: &str = include_str!("../../../fixtures/fold/fold.txt");

/// A line's code points written in hexadecimal, separated by spaces; none for an empty field.
fn code_points(written: &str) -> Vec<char> {
  written
    .split(' ')
    .filter(|code| !code.is_empty())
    .map(|code| scalar(code).unwrap())
    .collect()
}

/// The oracle's `every` lines: each value and its fold.
fn every_value_folded() -> BTreeMap<char, Vec<char>> {
  ORACLE
    .lines()
    .filter_map(|line| line.strip_prefix("every "))
    .map(|line| {
      let (from, to) = line.split_once(':').unwrap();
      (scalar(from).unwrap(), code_points(to))
    })
    .collect()
}

/// `characters` as the UTF-8 bytes `fold` answers with.
fn bytes(characters: &[char]) -> Vec<u8> {
  characters.iter().collect::<String>().into_bytes()
}

/// A-613 part 2, step 7 (`svn_utf__fuzzy_glob_match`'s fold): fold every Unicode scalar value alone, and each of the
/// oracle's names; expect each as utf8proc 2.12.0 folds it, a value the oracle does not list folding to itself, but for
/// the values Unicode 17 does not assign (the crate's tables) that utf8proc, on Unicode 18, folds to another, which are
/// counted and printed; and the oracle's 28 names folded alike.
#[test]
fn every_value_and_name_folds_as_utf8proc_folds_it() {
  let listed = every_value_folded();
  let (mut compared, mut skipped) = (0_usize, Vec::new());
  for character in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
    let expected = listed.get(&character).cloned();
    if expected.is_some() && !unicode_normalization::char::is_public_assigned(character) {
      skipped.push(character);
      continue;
    }
    let expected = expected.unwrap_or_else(|| vec![character]);
    assert_eq!(
      fold(&character.to_string()),
      bytes(&expected),
      "U+{:04X}",
      u32::from(character)
    );
    compared += 1;
  }
  let names: Vec<(Vec<char>, Vec<char>)> = ORACLE
    .lines()
    .filter_map(|line| line.strip_prefix("name "))
    .map(|line| {
      let (name, folded) = line.split_once(':').unwrap();
      (code_points(name), code_points(folded))
    })
    .collect();
  for (name, folded) in &names {
    let name: String = name.iter().collect();
    assert_eq!(fold(&name), bytes(folded), "{name}");
  }
  println!(
    "fold oracle: {compared} values compared, {} names; skipped as Unicode 18's: {:04X?}",
    names.len(),
    skipped
      .iter()
      .map(|character| u32::from(*character))
      .collect::<Vec<_>>()
  );
  assert_eq!((names.len(), listed.len() > skipped.len()), (28, true));
}

/// A-613 part 2, step 7: read CaseFolding.txt's foldings; expect one for each of its C and F lines and none for its S
/// and T lines, ordered by code point with no code point twice.
#[test]
fn case_folding_holds_every_c_and_f_line() {
  let lines = CASE_FOLDING
    .lines()
    .filter(|line| line.contains("; C; ") || line.contains("; F; "))
    .count();
  let foldings = case_foldings();
  assert_eq!(foldings.len(), lines);
  assert!(foldings.windows(2).all(|pair| match pair {
    [first, second] => first.from < second.from,
    _ => false,
  }));
}

/// A-613 part 2, step 7: the fold's two tables are one Unicode version: CaseFolding.txt's own first line names the
/// version the `unicode-normalization` crate's tables are.
#[test]
fn the_case_foldings_and_the_crates_tables_are_one_unicode_version() {
  let (major, minor, update) = unicode_normalization::UNICODE_VERSION;
  assert_eq!(
    CASE_FOLDING.lines().next(),
    Some(format!("# CaseFolding-{major}.{minor}.{update}.txt").as_str())
  );
}

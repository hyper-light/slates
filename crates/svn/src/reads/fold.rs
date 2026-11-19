//! The fold `svn_utf__fuzzy_glob_match` gives a name before `svn_repos_list` matches it against a pattern (A-613 part
//! 2, step 7; Subversion 1.15.0's `libsvn_subr/utf8proc.c` `normalize_cstring` through `svn_utf__xfrm`, case and
//! accents ignored): utf8proc's decomposition with `UTF8PROC_DECOMPOSE | STABLE | CASEFOLD | STRIPMARK`, then its
//! reencoding with `UTF8PROC_COMPOSE | STABLE`. The pattern is matched as the client sent it.
//!
//! - **Each code point**, in the order `utf8proc_decompose_char` takes its options: a Hangul syllable decomposed into
//!   its jamo; a mark (General_Category Mn, Mc or Me) dropped, before it could fold (U+0345 is dropped, never folded to
//!   iota); one with a full case folding (Unicode's CaseFolding.txt, statuses C and F, as utf8proc's tables take them)
//!   replaced by its folding, each code point of which is folded in turn; one with a canonical decomposition replaced by
//!   it, each piece folded in turn; any other kept.
//! - **Then canonical order** (`utf8proc_decompose`): a code point with a combining class above zero swapped before a
//!   neighbour of a higher class, until none is.
//! - **Then composition** (`utf8proc_normalize_utf32`): a code point combines with the last starter while its class
//!   is above every class since that starter (Hangul jamo too), composition exclusions excluded.
//!
//! Tables: Unicode 17.0.0, both the `unicode-normalization` crate's (canonical decomposition, the Mark category,
//! combining classes, composition) and Unicode's own CaseFolding.txt 17.0.0 (`crates/svn/data/unicode`, read once).
//! Held to the utf8proc stock `svnserve` 1.15.0 links on the Mac Studio (2.12.0, whose tables are Unicode 18.0.0) over
//! every scalar value Unicode 17 assigns and over names of several code points (`crates/svn/fixtures/fold`): a code
//! point Unicode 18 assigned since folds to itself here, where `svnserve` may fold it (GAPS).

use std::sync::OnceLock;
use unicode_normalization::char::{
  canonical_combining_class, compose, decompose_canonical, is_combining_mark,
};

/// Unicode's case folding data, whose C (common) and F (full) foldings utf8proc folds by.
const CASE_FOLDING: &str = include_str!("../../data/unicode/CaseFolding.txt");

/// The radix CaseFolding.txt writes its code points in.
/// Format: hexadecimal, as every Unicode Character Database file writes code points.
const HEXADECIMAL: u32 = 16;

/// The first and last Hangul syllables, which `utf8proc_decompose_char` decomposes before any other option.
/// Format: Unicode's Hangul syllable block, U+AC00 to U+D7A3 (The Unicode Standard §3.12).
const HANGUL_SYLLABLES: std::ops::RangeInclusive<char> = '\u{AC00}'..='\u{D7A3}';

/// One case folding: a code point and the code points it folds to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Folding {
  /// The code point folded.
  from: char,
  /// Its folding.
  to: Vec<char>,
}

/// `name` folded as `svn_utf__xfrm` folds it with case and accents ignored: its UTF-8 bytes.
pub(super) fn fold(name: &str) -> Vec<u8> {
  let mut decomposed = Vec::with_capacity(name.len());
  for character in name.chars() {
    push_folded(character, &mut decomposed);
  }
  canonical_order(&mut decomposed);
  composed(&decomposed).into_bytes()
}

/// `utf8proc_decompose_char` with decomposition, case folding and marks stripped: `character`'s code points appended
/// to `out`.
fn push_folded(character: char, out: &mut Vec<char>) {
  if HANGUL_SYLLABLES.contains(&character) {
    decompose_canonical(character, |jamo| out.push(jamo));
    return;
  }
  if is_combining_mark(character) {
    return;
  }
  if let Some(folding) = case_folding(character) {
    for folded in folding.iter().copied() {
      push_folded(folded, out);
    }
    return;
  }
  let mut pieces = Vec::new();
  decompose_canonical(character, |piece| pieces.push(piece));
  if pieces.as_slice() == [character] {
    out.push(character);
    return;
  }
  for piece in pieces {
    push_folded(piece, out);
  }
}

/// Canonical order as `utf8proc_decompose` makes it: a code point with a combining class above zero moves before a
/// neighbour whose class is higher, until none does.
fn canonical_order(characters: &mut [char]) {
  let mut at = 0_usize;
  while let Some(pair) = characters.get_mut(at..at.saturating_add(2)) {
    let [first, second] = pair else {
      return;
    };
    let (first_class, second_class) = (
      canonical_combining_class(*first),
      canonical_combining_class(*second),
    );
    if first_class > second_class && second_class > 0 {
      std::mem::swap(first, second);
      at = at.checked_sub(1).unwrap_or(1);
    } else {
      at = at.saturating_add(1);
    }
  }
}

/// Canonical composition as `utf8proc_normalize_utf32` composes: a code point combines with the last starter while
/// its class is above every class met since that starter, which a starter's own zero is not until one is met.
fn composed(characters: &[char]) -> String {
  let mut out: Vec<char> = Vec::with_capacity(characters.len());
  let mut starter: Option<usize> = None;
  let mut highest: Option<u8> = None;
  for character in characters.iter().copied() {
    let class = canonical_combining_class(character);
    let open = highest.is_none_or(|highest| class > highest);
    if open
      && let Some(slot) = starter.and_then(|at| out.get_mut(at))
      && let Some(combined) = compose(*slot, character)
    {
      *slot = combined;
      continue;
    }
    out.push(character);
    if class == 0 {
      starter = out.len().checked_sub(1);
      highest = None;
    } else if open {
      highest = Some(class);
    }
  }
  out.into_iter().collect()
}

/// `character`'s full case folding, where CaseFolding.txt gives it one.
fn case_folding(character: char) -> Option<&'static [char]> {
  let foldings = case_foldings();
  let at = foldings
    .binary_search_by_key(&character, |folding| folding.from)
    .ok()?;
  foldings.get(at).map(|folding| folding.to.as_slice())
}

/// CaseFolding.txt's C and F foldings in code point order, read once.
fn case_foldings() -> &'static [Folding] {
  static FOLDINGS: OnceLock<Vec<Folding>> = OnceLock::new();
  FOLDINGS.get_or_init(|| {
    let mut foldings: Vec<Folding> = CASE_FOLDING.lines().filter_map(folding_of).collect();
    foldings.sort_by_key(|folding| folding.from);
    foldings
  })
}

/// A CaseFolding.txt line's folding when its status is C or F (`CODE; STATUS; MAPPING; # NAME`); comments, blank
/// lines and the S and T foldings utf8proc does not take give none.
fn folding_of(line: &str) -> Option<Folding> {
  let mut fields = line.split(';').map(str::trim);
  let from = scalar(fields.next()?)?;
  if !matches!(fields.next()?, "C" | "F") {
    return None;
  }
  let to = fields
    .next()?
    .split(' ')
    .map(scalar)
    .collect::<Option<Vec<char>>>()?;
  Some(Folding { from, to })
}

/// A code point written in hexadecimal.
fn scalar(written: &str) -> Option<char> {
  u32::from_str_radix(written, HEXADECIMAL)
    .ok()
    .and_then(char::from_u32)
}

#[cfg(test)]
mod tests;

//! Objects by an abbreviated name (A-640 part 4; git v2.50.1's `object-name.c` `init_object_disambiguation`,
//! `find_short_object_filename`, `find_short_packed_object`, `match_hash`, `repo_find_unique_abbrev_r` and
//! `packfile.c` `repo_approximate_object_count`): the objects whose names begin with some hexadecimal digits, read from
//! the index in name order, since an object row's key is the object's name after one byte; and how many digits name an
//! object alone.
//!
//! **An abbreviated name** is 4 digits to 64, either case (`MINIMUM_ABBREV`; `GIT_MAX_HEXSZ`, SHA-256's length), each
//! digit a nibble, high first. git keeps every name in 32 bytes, those past a shorter format's hash zero (`oidread`),
//! and compares an abbreviation's digits against them all, so in a SHA-1 repository 41 digits to 64 name the object of
//! their first 40 when the rest are all `0`, and nothing otherwise; this search reads them so (stock git 2.50.1 serves
//! a `0` after a blob's name as the blob: `fixtures/fetch-sparse-abbrev-padded`).
//!
//! **The digits that name an object alone** (`repo_find_unique_abbrev_r`): git's default, then one more than the most
//! digits any other object's name shares with it. With `core.abbrev` unset git's default is half the bits of its count
//! of packed objects rounded up to whole digits, at least 7 (`FALLBACK_DEFAULT_ABBREV`); a store counts every object it
//! holds, as a repository whose objects are all packed. Each step of the search asks the index whether another object's
//! name begins with one more digit, so a step reads two rows at most.

use slates_vcs::ContentStore;

use super::Repository;
use super::rows::{self, OBJECT_KEY, ObjectRow};
use crate::object_name::{ObjectFormat, ObjectId};

/// Format: the fewest digits git reads as an abbreviated name (`MINIMUM_ABBREV`).
const MINIMUM_ABBREV: usize = 4;

/// Format: the most digits git reads as an abbreviated name: the longest name it knows (`GIT_MAX_HEXSZ`).
const MOST_DIGITS: usize = 64;

/// Format: git's fewest digits for an object's unique abbreviation with `core.abbrev` unset
/// (`FALLBACK_DEFAULT_ABBREV`).
const FALLBACK_DEFAULT_ABBREV: usize = 7;

/// Format: the bits one hexadecimal digit holds.
const DIGIT_BITS: u32 = 4;

/// Format: the base the digits are written in.
const HEX_RADIX: u32 = 16;

/// Format: the bits of a byte its low digit holds.
const LOW_DIGIT: u8 = 0x0f;

/// Format: git halves the bits of its count of objects, since two names are expected to share their first `b` bits once
/// there are about `2^(b/2)` of them ("divide by 2 and round up", `repo_find_unique_abbrev_r`).
const HALVES: u32 = 2;

/// Format: the digits one byte of a name holds.
const DIGITS_PER_BYTE: usize = 2;

/// Format: the bits of the count git sizes its default from (`msb`: an `unsigned long`).
const COUNT_BITS: u32 = u64::BITS;

/// An abbreviated object name (module docs): its digits' values, high nibble first, and how many there are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Abbrev {
  /// Each digit's value, 0 to 15; those past `digits` are zero.
  nibbles: [u8; MOST_DIGITS],
  /// How many digits.
  digits: usize,
}

impl Abbrev {
  /// `hex` read as git reads an abbreviated name (`init_object_disambiguation`): `None` unless it is 4 to 64
  /// hexadecimal digits, either case.
  pub fn parse(hex: &[u8]) -> Option<Abbrev> {
    if !(MINIMUM_ABBREV..=MOST_DIGITS).contains(&hex.len()) {
      return None;
    }
    let mut nibbles = [0; MOST_DIGITS];
    for (slot, digit) in nibbles.iter_mut().zip(hex) {
      *slot = char::from(*digit)
        .to_digit(HEX_RADIX)
        .and_then(|value| u8::try_from(value).ok())?;
    }
    Some(Abbrev {
      nibbles,
      digits: hex.len(),
    })
  }

  /// The first `digits` digits of `id`'s name, those past its format's length zero as git keeps them; at most 64.
  pub fn of(id: &ObjectId, digits: usize) -> Abbrev {
    let digits = digits.min(MOST_DIGITS);
    let mut nibbles = [0; MOST_DIGITS];
    for (at, slot) in nibbles.iter_mut().enumerate().take(digits) {
      *slot = nibble(id, at);
    }
    Abbrev { nibbles, digits }
  }

  /// How many digits it has.
  pub fn digits(&self) -> usize {
    self.digits
  }

  /// Its digits in lower case, as git writes an abbreviation it was given (`ds.hex_pfx`).
  pub fn hex(&self) -> Vec<u8> {
    self
      .nibbles
      .iter()
      .take(self.digits)
      .filter_map(|nibble| char::from_digit(u32::from(*nibble), HEX_RADIX))
      .filter_map(|digit| u8::try_from(digit).ok())
      .collect()
  }

  /// Whether `id`'s name begins with these digits, compared as git's `match_hash` compares them, past the format's
  /// length against zero.
  pub fn begins(&self, id: &ObjectId) -> bool {
    self
      .nibbles
      .iter()
      .take(self.digits)
      .enumerate()
      .all(|(at, value)| nibble(id, at) == *value)
  }

  /// The least object row's key a name with these digits can have in `format`: the key's byte, then the digits within
  /// the format's length packed two to a byte, an odd last digit high.
  fn first_key(&self, format: ObjectFormat) -> Vec<u8> {
    let digits = self.digits.min(format.hex_len());
    let mut key = Vec::with_capacity(digits.div_ceil(DIGITS_PER_BYTE).saturating_add(1));
    key.push(OBJECT_KEY);
    key.extend(
      self
        .nibbles
        .get(..digits)
        .unwrap_or_default()
        .chunks(DIGITS_PER_BYTE)
        .map(|pair| {
          let high = pair.first().copied().unwrap_or_default();
          let low = pair.get(1).copied().unwrap_or_default();
          high.checked_shl(DIGIT_BITS).unwrap_or_default() | low
        }),
    );
    key
  }
}

/// Digit `at` of `id`'s name, high nibble first; zero past its format's length, as git keeps a name.
fn nibble(id: &ObjectId, at: usize) -> u8 {
  let byte = at
    .checked_div(DIGITS_PER_BYTE)
    .and_then(|index| id.as_bytes().get(index))
    .copied()
    .unwrap_or_default();
  if at.is_multiple_of(DIGITS_PER_BYTE) {
    byte.checked_shr(DIGIT_BITS).unwrap_or_default()
  } else {
    byte & LOW_DIGIT
  }
}

impl Repository {
  /// The objects whose names begin with `abbrev`'s digits, each by name with its row, in name order: the index's
  /// object rows from the least key such a name can have, to the first whose name does not begin so.
  pub fn abbreviated<'s>(
    &'s self,
    store: &'s ContentStore,
    abbrev: &Abbrev,
  ) -> impl Iterator<Item = (ObjectId, ObjectRow)> + use<'s> {
    let format = self.format;
    let abbrev = *abbrev;
    self
      .index
      .iter_from(store, &abbrev.first_key(format))
      .map_while(move |row| {
        let id = row
          .key()
          .strip_prefix(&[OBJECT_KEY])
          .and_then(|name| ObjectId::from_bytes(format, name))?;
        let held = rows::decode(row.payload())?;
        abbrev.begins(&id).then_some((id, held))
      })
  }

  /// How many digits git abbreviates an object's name to by default with `core.abbrev` unset
  /// (`repo_find_unique_abbrev_r`): half the bits of the count of objects, rounded up to whole digits, at least 7.
  pub fn default_digits(&self) -> usize {
    let highest = COUNT_BITS
      .saturating_sub(self.next_position.leading_zeros())
      .saturating_sub(1);
    let bits = highest.saturating_add(1);
    usize::try_from(bits.div_ceil(HALVES))
      .unwrap_or(MOST_DIGITS)
      .max(FALLBACK_DEFAULT_ABBREV)
  }

  /// How many digits of `id`'s name git writes for it (`repo_find_unique_abbrev_r`): the fewest from `least` that no
  /// other object's name begins with, the whole name at most.
  pub fn unique_digits(&self, store: &ContentStore, id: &ObjectId, least: usize) -> usize {
    let whole = self.format.hex_len();
    (least..whole)
      .find(|digits| {
        self
          .abbreviated(store, &Abbrev::of(id, *digits))
          .all(|(other, _)| other == *id)
      })
      .unwrap_or(whole)
  }
}

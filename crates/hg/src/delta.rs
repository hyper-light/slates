//! A delta: the hunks that rewrite a base text into a revision's text (A-514; `internals.changegroups` "Delta
//! Groups" \[B\]; `mercurial/mpatch.c` \[C\]).
//!
//! A delta is a run of hunks with no separator. Each is a header of three big-endian signed 32-bit integers, `start`,
//! `end` and the new data's `length`, then that many bytes: the base's bytes `start..end` are replaced by the data.
//! Mercurial's C patcher holds two sets of rules, and so does this module. Decoding (`mpatch_decode`) refuses a hunk
//! whose `start` is negative or past its `end` or whose `length` is negative, and a delta whose hunks do not end
//! exactly where it ends ("patch cannot be decoded"). Applying (`mpatch_apply`) refuses a hunk that starts before the
//! previous one ended or ends past the base ("invalid patch").
//!
//! Hunks are read by an iterator, so checking a delta allocates nothing; applying one sizes the text from the hunks
//! first, charges it to the caller's [`Admission`], and fills one vector of exactly that length.

use crate::read::{Cursor, to_u64};
use crate::{Admission, PastAdmission};

/// Format: a hunk's header, three big-endian signed 32-bit integers: `start`, `end` and the data's length (`>lll`).
pub const HUNK_HEADER_LEN: usize = 12;

/// One hunk: replace the base's bytes `start..end` with `data`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk<'a> {
  /// The first base byte replaced.
  pub start: u32,
  /// One past the last base byte replaced.
  pub end: u32,
  /// The bytes put in their place.
  pub data: &'a [u8],
}

/// Why a delta was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// `mpatch_decode`'s refusal: a hunk whose header runs past the delta, whose `start` is negative or past its `end`,
  /// whose length is negative or runs past the delta. `at` is where the hunk began.
  Undecodable {
    /// The offset of the refused hunk in the delta.
    at: u64,
  },
  /// `mpatch_apply`'s refusal: a hunk that starts before the previous one ended.
  OutOfOrder {
    /// The hunk's start.
    start: u64,
    /// Where the previous hunk ended.
    previous_end: u64,
  },
  /// `mpatch_apply`'s refusal: a hunk that ends past the base.
  PastBase {
    /// The hunk's end.
    end: u64,
    /// The base's length.
    base: u64,
  },
  /// A value an encoded hunk cannot carry: past `i32::MAX`, the largest a signed 32-bit field holds.
  TooLong {
    /// The value.
    value: u64,
  },
  /// The patched text is larger than the caller's admission allows.
  PastAdmission(PastAdmission),
}

/// The hunks of a delta, in order, each checked by `mpatch_decode`'s rules; the first refusal ends the iteration.
pub struct Hunks<'a> {
  /// The delta, read front to back.
  cursor: Cursor<'a>,
  /// Whether a refusal has been returned.
  failed: bool,
}

/// The hunks of `delta`.
pub fn hunks(delta: &[u8]) -> Hunks<'_> {
  Hunks {
    cursor: Cursor::new(delta),
    failed: false,
  }
}

impl<'a> Hunks<'a> {
  /// Reads one hunk at the cursor.
  fn read_one(&mut self) -> Result<Hunk<'a>, Refusal> {
    let undecodable = Refusal::Undecodable {
      at: to_u64(self.cursor.at()),
    };
    let start = self.cursor.i32("hunk start").map_err(|_| undecodable)?;
    let end = self.cursor.i32("hunk end").map_err(|_| undecodable)?;
    let length = self.cursor.i32("hunk length").map_err(|_| undecodable)?;
    let start = u32::try_from(start).map_err(|_| undecodable)?;
    let end = u32::try_from(end).map_err(|_| undecodable)?;
    if start > end {
      return Err(undecodable);
    }
    let length = usize::try_from(length).map_err(|_| undecodable)?;
    let data = self
      .cursor
      .take(length, "hunk data")
      .map_err(|_| undecodable)?;
    Ok(Hunk { start, end, data })
  }
}

impl<'a> Iterator for Hunks<'a> {
  type Item = Result<Hunk<'a>, Refusal>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed || self.cursor.is_done() {
      return None;
    }
    let hunk = self.read_one();
    self.failed = hunk.is_err();
    Some(hunk)
  }
}

/// Checks `delta` by `mpatch_decode`'s rules and counts its hunks, allocating nothing.
pub fn check(delta: &[u8]) -> Result<usize, Refusal> {
  let none: usize = 0;
  hunks(delta).try_fold(none, |count, hunk| hunk.map(|_| count.saturating_add(1)))
}

/// The length of the text `delta` makes of a base `base` bytes long, checking `mpatch_apply`'s rules: every hunk
/// starts at or after the previous one's end and ends within the base.
pub fn patched_len(base: u64, delta: &[u8]) -> Result<u64, Refusal> {
  let mut out: u64 = 0;
  let mut last: u64 = 0;
  for hunk in hunks(delta) {
    let hunk = hunk?;
    let (start, end) = (u64::from(hunk.start), u64::from(hunk.end));
    let kept = start.checked_sub(last).ok_or(Refusal::OutOfOrder {
      start,
      previous_end: last,
    })?;
    if end > base {
      return Err(Refusal::PastBase { end, base });
    }
    out = out
      .saturating_add(kept)
      .saturating_add(to_u64(hunk.data.len()));
    last = end;
  }
  Ok(out.saturating_add(base.saturating_sub(last)))
}

/// The text `delta` makes of `base`: its length checked and charged to `admission` first, then built in one vector of
/// exactly that length.
pub fn apply(base: &[u8], delta: &[u8], admission: &mut Admission) -> Result<Vec<u8>, Refusal> {
  let len = patched_len(to_u64(base.len()), delta)?;
  admission.charge(len).map_err(Refusal::PastAdmission)?;
  let capacity = usize::try_from(len).map_err(|_| Refusal::TooLong { value: len })?;
  let mut out = Vec::with_capacity(capacity);
  let mut last: usize = 0;
  for hunk in hunks(delta) {
    let hunk = hunk?;
    let start = index(hunk.start)?;
    out.extend_from_slice(base_range(base, last, start)?);
    out.extend_from_slice(hunk.data);
    last = index(hunk.end)?;
  }
  out.extend_from_slice(base_range(base, last, base.len())?);
  Ok(out)
}

/// A hunk's position as an index.
fn index(value: u32) -> Result<usize, Refusal> {
  usize::try_from(value).map_err(|_| Refusal::TooLong {
    value: u64::from(value),
  })
}

/// The base's bytes `from..to`, which [`patched_len`] has already held within the base and in order.
fn base_range(base: &[u8], from: usize, to: usize) -> Result<&[u8], Refusal> {
  base.get(from..to).ok_or(Refusal::PastBase {
    end: to_u64(to),
    base: to_u64(base.len()),
  })
}

/// Appends one hunk to `out`: its three fields, then its data.
pub fn encode_hunk(hunk: &Hunk<'_>, out: &mut Vec<u8>) -> Result<(), Refusal> {
  let length = to_u64(hunk.data.len());
  for value in [u64::from(hunk.start), u64::from(hunk.end), length] {
    let field = i32::try_from(value).map_err(|_| Refusal::TooLong { value })?;
    out.extend_from_slice(&field.to_be_bytes());
  }
  out.extend_from_slice(hunk.data);
  Ok(())
}

/// Appends the delta Mercurial sends for a full text against the null base: one hunk inserting the whole text
/// (`mdiff.trivialdiffheader`, `changegroup._revisiondeltatochunks`).
pub fn encode_full_text(text: &[u8], out: &mut Vec<u8>) -> Result<(), Refusal> {
  encode_hunk(
    &Hunk {
      start: 0,
      end: 0,
      data: text,
    },
    out,
  )
}

#[cfg(test)]
mod tests {
  use super::{Hunk, Refusal, apply, check, encode_full_text, encode_hunk, hunks, patched_len};
  use crate::Admission;
  use proptest::prelude::*;

  /// The delta of `hunks`, encoded.
  fn delta(hunks: &[Hunk<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for hunk in hunks {
      encode_hunk(hunk, &mut out).unwrap();
    }
    out
  }

  /// One hunk's raw header and data, written field by field so a test can put any value in any field.
  fn raw(start: i32, end: i32, length: i32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&start.to_be_bytes());
    out.extend_from_slice(&end.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(data);
    out
  }

  fn room() -> Admission {
    Admission::new(u64::MAX)
  }

  /// A-514: do apply a delta of two hunks to a base. Expect the base's untouched bytes kept in order around each
  /// replacement, and the length predicted by `patched_len` before anything was built.
  #[test]
  fn a_delta_replaces_its_ranges_and_keeps_the_rest() {
    let base = b"hello, world\n";
    let d = delta(&[
      Hunk {
        start: 0,
        end: 5,
        data: b"goodbye",
      },
      Hunk {
        start: 7,
        end: 12,
        data: b"moon",
      },
    ]);
    assert_eq!(patched_len(13, &d), Ok(14));
    assert_eq!(apply(base, &d, &mut room()).unwrap(), b"goodbye, moon\n");
  }

  /// A-514: do encode a full text as Mercurial sends one against the null base and apply it to the empty text. Expect
  /// the text back, and a header of start 0, end 0 and the text's length (`trivialdiffheader`).
  #[test]
  fn a_full_text_is_one_hunk_against_the_empty_base() {
    let mut d = Vec::new();
    encode_full_text(b"a\nb\n", &mut d).unwrap();
    assert_eq!(d, raw(0, 0, 4, b"a\nb\n"));
    assert_eq!(apply(b"", &d, &mut room()).unwrap(), b"a\nb\n");
  }

  /// AC-HG.9 (hostile input), A-514: do give each of `mpatch_decode`'s refusals. Expect `Undecodable` at the hunk's
  /// offset for a negative start, a start past the end, a negative length (`len = u32::MAX` on the wire), a length
  /// past the delta and a header cut short.
  #[test]
  fn every_hunk_mpatch_cannot_decode_is_refused() {
    let at_0 = Err(Refusal::Undecodable { at: 0 });
    assert_eq!(check(&raw(-1, 0, 0, b"")), at_0);
    assert_eq!(check(&raw(5, 4, 0, b"")), at_0);
    assert_eq!(check(&raw(0, 0, -1, b"")), at_0);
    let mut max = raw(0, 0, 0, b"");
    max.truncate(8);
    max.extend_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(check(&max), at_0);
    assert_eq!(check(&raw(0, 0, 9, b"short")), at_0);
    let mut cut = raw(0, 0, 1, b"x");
    cut.extend_from_slice(&[0, 0, 0, 1, 0]);
    assert_eq!(check(&cut), Err(Refusal::Undecodable { at: 13 }));
    assert_eq!(check(&cut[..13]), Ok(1));
  }

  /// AC-HG.9, A-514: do apply hunks out of order, and a hunk past the base. Expect `mpatch_apply`'s two refusals, and
  /// nothing charged to the admission for either.
  #[test]
  fn hunks_out_of_order_or_past_the_base_are_refused_before_anything_is_charged() {
    let mut admission = Admission::new(64);
    let backwards = [raw(4, 6, 0, b""), raw(2, 3, 0, b"")].concat();
    assert_eq!(
      apply(b"0123456789", &backwards, &mut admission),
      Err(Refusal::OutOfOrder {
        start: 2,
        previous_end: 6
      })
    );
    assert_eq!(
      apply(b"0123", &raw(2, 5, 0, b""), &mut admission),
      Err(Refusal::PastBase { end: 5, base: 4 })
    );
    assert_eq!(admission.left(), 64);
  }

  /// A-514: do apply a delta whose text is larger than the admission. Expect `PastAdmission` before a byte is built.
  #[test]
  fn a_text_larger_than_its_admission_is_refused() {
    let d = raw(0, 0, 8, b"12345678");
    let mut admission = Admission::new(7);
    assert!(matches!(
      apply(b"", &d, &mut admission),
      Err(Refusal::PastAdmission(_))
    ));
    assert_eq!(admission.left(), 7);
  }

  /// A-514: do encode a hunk whose length cannot be a signed 32-bit integer. Expect `TooLong`, not a wrapped value.
  #[test]
  fn a_position_past_i32_is_not_encoded() {
    let mut out = Vec::new();
    assert_eq!(
      encode_hunk(
        &Hunk {
          start: u32::MAX,
          end: u32::MAX,
          data: b""
        },
        &mut out
      ),
      Err(Refusal::TooLong {
        value: u64::from(u32::MAX)
      })
    );
  }

  /// The model the property below holds `apply` to: each hunk spliced into the text in turn, its positions shifted
  /// by what the earlier hunks grew or shrank it.
  fn model(base: &[u8], edits: &[(usize, usize, Vec<u8>)]) -> Vec<u8> {
    let mut text = base.to_vec();
    let mut shift: isize = 0;
    for (start, end, data) in edits {
      let from = start.checked_add_signed(shift).unwrap();
      let to = end.checked_add_signed(shift).unwrap();
      text.splice(from..to, data.iter().copied());
      shift += isize::try_from(data.len()).unwrap() - isize::try_from(end - start).unwrap();
    }
    text
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-514 (oracle): do apply a delta of ordered, disjoint hunks within a base. Expect the splice model's text,
    /// and `patched_len` its length.
    #[test]
    fn apply_equals_the_splice_model(
      base in proptest::collection::vec(any::<u8>(), 0..64),
      cuts in proptest::collection::vec((any::<u8>(), any::<u8>(), proptest::collection::vec(any::<u8>(), 0..8)), 0..6),
    ) {
      let mut points: Vec<usize> = cuts.iter().flat_map(|(a, b, _)| [usize::from(*a), usize::from(*b)]).map(|p| p % (base.len() + 1)).collect();
      points.sort_unstable();
      let edits: Vec<(usize, usize, Vec<u8>)> = points.chunks(2).zip(cuts.iter()).filter(|(pair, _)| pair.len() == 2).map(|(pair, (_, _, data))| (pair[0], pair[1], data.clone())).collect();
      let list: Vec<Hunk<'_>> = edits.iter().map(|(s, e, d)| Hunk { start: u32::try_from(*s).unwrap(), end: u32::try_from(*e).unwrap(), data: d.as_slice() }).collect();
      let d = delta(&list);
      let expected = model(&base, &edits);
      prop_assert_eq!(patched_len(u64::try_from(base.len()).unwrap(), &d), Ok(u64::try_from(expected.len()).unwrap()));
      prop_assert_eq!(apply(&base, &d, &mut room()).unwrap(), expected);
      prop_assert_eq!(hunks(&d).count(), edits.len());
    }

    /// AC-HG.9, A-514 (hostile input): do check and apply arbitrary bytes as a delta. Expect a text or a typed
    /// refusal, never a panic.
    #[test]
    fn arbitrary_deltas_never_panic(
      base in proptest::collection::vec(any::<u8>(), 0..32),
      d in proptest::collection::vec(any::<u8>(), 0..96),
    ) {
      let checked = check(&d);
      let applied = apply(&base, &d, &mut Admission::new(4096));
      prop_assert!(checked.is_ok() || applied.is_err());
    }
  }
}

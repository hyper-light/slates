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
  let mut out = Vec::new();
  apply_into(base, delta, &mut out, admission)?;
  Ok(out)
}

/// The text `delta` makes of `base`, written over `out`: its length checked first, then `out` emptied and filled, grown
/// only where its capacity falls short of the text, and only that growth charged to `admission`. A chain read alternating
/// between two buffers is so charged each buffer's longest text once, not a text a link (main's order of 2026-11-20 on
/// the hg import's memory). Refused, `out` holds no text of this delta's, which its caller drops or writes over.
pub fn apply_into(
  base: &[u8],
  delta: &[u8],
  out: &mut Vec<u8>,
  admission: &mut Admission,
) -> Result<(), Refusal> {
  let len = patched_len(to_u64(base.len()), delta)?;
  let capacity = usize::try_from(len).map_err(|_| Refusal::TooLong { value: len })?;
  admission
    .charge(to_u64(capacity.saturating_sub(out.capacity())))
    .map_err(Refusal::PastAdmission)?;
  out.clear();
  out.reserve_exact(capacity);
  let mut last: usize = 0;
  for hunk in hunks(delta) {
    let hunk = hunk?;
    let start = index(hunk.start)?;
    out.extend_from_slice(base_range(base, last, start)?);
    out.extend_from_slice(hunk.data);
    last = index(hunk.end)?;
  }
  out.extend_from_slice(base_range(base, last, base.len())?);
  Ok(())
}

/// A base text read in ranges (goal 93, C1 (2): a delta applied over a base the content store holds in windows, never the
/// base whole): a slice, or a stored text its reader finds a window at a time.
pub trait BaseRead {
  /// The base's length.
  fn len(&self) -> u64;
  /// Whether the base has no bytes: a delta over it can only insert.
  fn is_empty(&self) -> bool {
    self.len() == 0
  }
  /// Appends the base's bytes `at..at + n` to `out`: none where the reader does not hold them now (a window let go),
  /// `out` then as it was.
  fn read_into(&mut self, at: u64, n: usize, out: &mut Vec<u8>) -> Option<()>;
}

impl BaseRead for &[u8] {
  fn len(&self) -> u64 {
    to_u64(<[u8]>::len(self))
  }

  fn read_into(&mut self, at: u64, n: usize, out: &mut Vec<u8>) -> Option<()> {
    let from = usize::try_from(at).ok()?;
    out.extend_from_slice(self.get(from..from.checked_add(n)?)?);
    Some(())
  }
}

/// What a [`Patching::fill`] came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filled {
  /// `most` bytes made, and more to make.
  More,
  /// The text made to its end.
  Done,
  /// The base's bytes from `at` are not held now: brought back, the fill is taken again where it stopped.
  Unread {
    /// Where the base's read stopped.
    at: u64,
  },
}

/// A delta applied over a base a slice at a time (goal 93, C1 (2): a revision rebuilt into one buffer, its base read
/// whole, held a push's step's working set at a long file's size): each [`Patching::fill`] makes the text's next bytes,
/// at most as many as asked, reading the base's ranges as the hunks reach them; `mpatch_apply`'s rules held as
/// [`patched_len`] holds them, each hunk as it is met. What it keeps between fills is fixed in size: where it is in the
/// delta, the base and the hunk it is writing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Patching {
  /// Where the next hunk begins in the delta.
  delta_at: usize,
  /// The next base byte to copy, or one past the last a hunk replaced.
  base_at: u64,
  /// The base is copied up to here before the hunk being written, if any.
  copy_until: u64,
  /// The hunk being written: its data's range in the delta and the base byte it resumes at.
  data: Option<(usize, usize, u64)>,
  /// The last hunk met: the base is copied to its end, and the text ends.
  ended: bool,
  /// The bytes made so far.
  made: u64,
}

impl Patching {
  /// The bytes made so far.
  pub fn made(&self) -> u64 {
    self.made
  }

  /// Appends the text's next bytes to `out`, at most `most` (at least one where any are left), from `delta` over
  /// `base`: [`Filled::Done`] once it has made the text's last; refused by `mpatch_apply`'s rules at the hunk that breaks
  /// them, what it appended before it left in `out`.
  pub fn fill(
    &mut self,
    (delta, base): (&[u8], &mut dyn BaseRead),
    most: usize,
    out: &mut Vec<u8>,
  ) -> Result<Filled, Refusal> {
    let most = most.max(1);
    let mut written = 0usize;
    loop {
      let room = most.saturating_sub(written);
      if room == 0 {
        return Ok(Filled::More);
      }
      if self.base_at < self.copy_until {
        let left =
          usize::try_from(self.copy_until.saturating_sub(self.base_at)).unwrap_or(usize::MAX);
        let n = room.min(left);
        if base.read_into(self.base_at, n, out).is_none() {
          return Ok(Filled::Unread { at: self.base_at });
        }
        self.base_at = self.base_at.saturating_add(to_u64(n));
        self.made = self.made.saturating_add(to_u64(n));
        written = written.saturating_add(n);
        continue;
      }
      if let Some((at, end, resume)) = self.data {
        if at < end {
          let n = room.min(end.saturating_sub(at));
          let bytes = delta
            .get(at..at.saturating_add(n))
            .ok_or(Refusal::Undecodable { at: to_u64(at) })?;
          out.extend_from_slice(bytes);
          self.data = Some((at.saturating_add(n), end, resume));
          self.made = self.made.saturating_add(to_u64(n));
          written = written.saturating_add(n);
          continue;
        }
        self.base_at = resume;
        self.data = None;
        continue;
      }
      if self.ended {
        return Ok(Filled::Done);
      }
      self.next_hunk(delta, base.len())?;
    }
  }

  /// Reads the hunk at `delta_at`, held to `mpatch_apply`'s rules against what was before it and a base `base` bytes
  /// long; past the delta's last, the base copied to its end and the text ended.
  fn next_hunk(&mut self, delta: &[u8], base: u64) -> Result<(), Refusal> {
    let rest = delta.get(self.delta_at..).unwrap_or_default();
    let Some(hunk) = hunks(rest).next() else {
      if self.base_at > base {
        return Err(Refusal::PastBase {
          end: self.base_at,
          base,
        });
      }
      self.copy_until = base;
      self.ended = true;
      return Ok(());
    };
    let hunk = hunk.map_err(|refusal| match refusal {
      Refusal::Undecodable { at } => Refusal::Undecodable {
        at: at.saturating_add(to_u64(self.delta_at)),
      },
      other => other,
    })?;
    let (start, end) = (u64::from(hunk.start), u64::from(hunk.end));
    if start < self.base_at {
      return Err(Refusal::OutOfOrder {
        start,
        previous_end: self.base_at,
      });
    }
    if end > base {
      return Err(Refusal::PastBase { end, base });
    }
    let data_at = self.delta_at.saturating_add(HUNK_HEADER_LEN);
    let data_end = data_at.saturating_add(hunk.data.len());
    self.copy_until = start;
    self.data = Some((data_at, data_end, end));
    self.delta_at = data_end;
    Ok(())
  }
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

/// Where the text a delta against the null base carries begins in it: past its one hunk, which must insert exactly the
/// rest at 0 (`mdiff.trivialdiffheader`, what every 7.2 writer makes), or at 0 for an empty delta (an empty text's, as
/// 7.2 sends one); none for any other delta. A Mercurial whole is kept as that text alone (A-767 C1: the store's
/// contract, a whole's place its raw text).
pub fn whole_text_start(delta: &[u8]) -> Option<usize> {
  if delta.is_empty() {
    return Some(0);
  }
  let (header, text) = delta.split_at_checked(HUNK_HEADER_LEN)?;
  let words: Vec<u32> = header
    .as_chunks::<{ size_of::<u32>() }>()
    .0
    .iter()
    .map(|word| u32::from_be_bytes(*word))
    .collect();
  let length = u32::try_from(text.len()).ok()?;
  (words == [0, 0, length]).then_some(HUNK_HEADER_LEN)
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

/// Appends a delta rewriting `base` into `text` by whole lines (A-729: a manifest kept as its change against the one
/// before it). The two texts' lines are walked together, a line's key its bytes up to a NUL (a manifest line's path) or
/// the whole line: a line equal in both is kept; at any other pair the line of smaller key is passed over (dropped from
/// the base or put from the text), and both where the keys are equal. Each run between kept lines becomes one hunk: the
/// base's bytes it passed over replaced by the text's. Every line of `text` is either kept or in a hunk, in order, so
/// applying the delta to `base` gives `text` for any two texts; over two manifests, sorted by path, the walk is one pass
/// and each hunk one stretch of changed paths.
pub fn encode_lines(base: &[u8], text: &[u8], out: &mut Vec<u8>) -> Result<(), Refusal> {
  // Each text's lines walked by a cursor, none listed (goal 93: listing both a manifest's lines was about one copy of it
  // a changeset on the push's path).
  let (mut old, mut new) = (lines_of(base).peekable(), lines_of(text).peekable());
  let mut open: Option<(usize, usize)> = None;
  loop {
    let (left, right) = (old.peek().copied(), new.peek().copied());
    if left.is_none() && right.is_none() {
      break;
    }
    if let (Some((kept, line)), Some((put, same))) = (left, right)
      && line == same
    {
      if let Some(run) = open.take() {
        hunk_of(run, (kept, put), text, out)?;
      }
      old.next();
      new.next();
      continue;
    }
    let from_base = left.map_or(base.len(), |(at, _)| at);
    let from_text = right.map_or(text.len(), |(at, _)| at);
    open.get_or_insert((from_base, from_text));
    match (left, right) {
      (Some((_, line)), Some((_, other))) => match key(line).cmp(key(other)) {
        core::cmp::Ordering::Less => {
          old.next();
        }
        core::cmp::Ordering::Greater => {
          new.next();
        }
        core::cmp::Ordering::Equal => {
          old.next();
          new.next();
        }
      },
      (Some(_), None) => {
        old.next();
      }
      _ => {
        new.next();
      }
    }
  }
  match open {
    Some(run) => hunk_of(run, (base.len(), text.len()), text, out),
    None => Ok(()),
  }
}

/// Each line of `text` and where it begins, its end of line included, walked in order.
fn lines_of(text: &[u8]) -> impl Iterator<Item = (usize, &[u8])> + Clone {
  text
    .split_inclusive(|byte| *byte == b'\n')
    .scan(0_usize, |at, line| {
      let begins = *at;
      *at = at.saturating_add(line.len());
      Some((begins, line))
    })
}

/// A line's key: its bytes up to a NUL, or the whole line.
fn key(line: &[u8]) -> &[u8] {
  line.split(|byte| *byte == 0).next().unwrap_or(line)
}

/// One hunk: the base's bytes from the run's start to `end` replaced by the text's from the run's start to `until`.
fn hunk_of(
  (start, from): (usize, usize),
  (end, until): (usize, usize),
  text: &[u8],
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let field = |value: usize| {
    u32::try_from(value).map_err(|_| Refusal::TooLong {
      value: crate::read::to_u64(value),
    })
  };
  let data = text.get(from..until).unwrap_or_default();
  encode_hunk(
    &Hunk {
      start: field(start)?,
      end: field(end)?,
      data,
    },
    out,
  )
}

#[cfg(test)]
mod tests {
  use super::{
    BaseRead, Filled, Hunk, Patching, Refusal, apply, check, encode_full_text, encode_hunk,
    encode_lines, hunks, patched_len,
  };

  /// The text `delta` makes of `base` by [`Patching`], `most` bytes a fill, or its refusal; and the longest fill. A slice
  /// holds every byte, so a base read that stops fails the test.
  fn patched_by_fills(base: &[u8], delta: &[u8], most: usize) -> (Result<Vec<u8>, Refusal>, usize) {
    let mut patching = Patching::default();
    let mut out = Vec::new();
    let mut longest = 0usize;
    let mut reader: &[u8] = base;
    loop {
      let before = out.len();
      let filled = match patching.fill((delta, &mut reader as &mut dyn BaseRead), most, &mut out) {
        Ok(filled) => filled,
        Err(refusal) => return (Err(refusal), longest),
      };
      longest = longest.max(out.len().saturating_sub(before));
      match filled {
        Filled::More => {}
        Filled::Done => return (Ok(out), longest),
        Filled::Unread { at } => panic!("a slice holds every byte: unread at {at}"),
      }
    }
  }

  /// Goal 93, C1 (2). Do: apply deltas of every shape (none, a whole, edits at the start, middle and end, an insert, a
  /// delete, many hunks) over a base, by [`apply`] and by [`Patching`] at fills of 1, 3, 64 and the whole. Expect the
  /// same text every way, no fill past its most, and the same refusals for a hunk out of order and one past the base.
  #[test]
  fn a_delta_patched_a_slice_at_a_time_makes_the_same_text() {
    let base: Vec<u8> = (0..300u32)
      .map(|at| u8::try_from(at % 251).unwrap())
      .collect();
    let mut deltas: Vec<Vec<u8>> = vec![Vec::new()];
    for hunks_of in [
      vec![(0u32, 300u32, b"whole".to_vec())],
      vec![(0, 5, b"start".to_vec())],
      vec![(100, 150, b"middle and longer than it was".to_vec())],
      vec![(290, 300, Vec::new())],
      vec![(40, 40, b"inserted".to_vec())],
      vec![
        (1, 2, b"a".to_vec()),
        (10, 20, Vec::new()),
        (20, 20, b"b".to_vec()),
        (299, 300, b"z".to_vec()),
      ],
    ] {
      let mut delta = Vec::new();
      for (start, end, data) in &hunks_of {
        encode_hunk(
          &Hunk {
            start: *start,
            end: *end,
            data,
          },
          &mut delta,
        )
        .unwrap();
      }
      deltas.push(delta);
    }
    for delta in &deltas {
      let expected = apply(&base, delta, &mut crate::Admission::new(u64::MAX)).unwrap();
      for most in [1usize, 3, 64, 1 << 20] {
        let (made, longest) = patched_by_fills(&base, delta, most);
        assert_eq!(made.unwrap(), expected, "a fill of {most}");
        assert!(longest <= most, "a fill of {most} made {longest}");
      }
    }
    let mut out_of_order = Vec::new();
    encode_hunk(
      &Hunk {
        start: 10,
        end: 20,
        data: b"",
      },
      &mut out_of_order,
    )
    .unwrap();
    encode_hunk(
      &Hunk {
        start: 15,
        end: 16,
        data: b"",
      },
      &mut out_of_order,
    )
    .unwrap();
    let mut past = Vec::new();
    encode_hunk(
      &Hunk {
        start: 10,
        end: 301,
        data: b"",
      },
      &mut past,
    )
    .unwrap();
    for delta in [&out_of_order, &past] {
      assert_eq!(
        patched_by_fills(&base, delta, 7).0.err(),
        apply(&base, delta, &mut crate::Admission::new(u64::MAX)).err()
      );
    }
  }
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

  /// A-729 (the manifest's delta). Do: encode a delta by lines between pairs of texts (manifests with paths added,
  /// changed and removed; unsorted lines; empty texts; a last line with no end), and apply each to its base. Expect each
  /// applied delta the text, and a manifest's one changed path a delta far smaller than the text.
  #[test]
  fn a_delta_by_lines_rewrites_its_base_into_its_text() {
    let manifest = |paths: &[(&str, char)]| -> Vec<u8> {
      paths
        .iter()
        .flat_map(|(path, node)| format!("{path}\0{}\n", node.to_string().repeat(40)).into_bytes())
        .collect()
    };
    let base = manifest(&[("a", '1'), ("b", '2'), ("c", '3'), ("e", '5')]);
    let text = manifest(&[("a", '1'), ("b", '9'), ("d", '4'), ("e", '5'), ("f", '6')]);
    let pairs: [(&[u8], &[u8]); 6] = [
      (&base, &text),
      (&text, &base),
      (b"", &text),
      (&base, b""),
      (b"z\ny\nx", b"y\nq\nx\nw"),
      (b"same\n", b"same\n"),
    ];
    for (old, new) in pairs {
      let mut delta = Vec::new();
      encode_lines(old, new, &mut delta).unwrap();
      assert_eq!(
        apply(old, &delta, &mut Admission::new(u64::MAX)).unwrap(),
        new
      );
    }
    let many: Vec<(String, char)> = (0..200).map(|at| (format!("path{at:03}"), 'a')).collect();
    let named: Vec<(&str, char)> = many
      .iter()
      .map(|(path, node)| (path.as_str(), *node))
      .collect();
    let mut changed = named.clone();
    changed[100].1 = 'b';
    let (old, new) = (manifest(&named), manifest(&changed));
    let mut delta = Vec::new();
    encode_lines(&old, &new, &mut delta).unwrap();
    assert!(
      delta.len() < new.len() / 50,
      "{} bytes for one path of {}",
      delta.len(),
      new.len()
    );
  }
}

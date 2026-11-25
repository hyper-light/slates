//! A manifest's delta against another, made as both stream, never whole (goal 93, C1 (2); main's ruling (B') of
//! 2026-11-24: a pushed manifest kept as its own delta against the manifest the fork holds last, the push holding
//! neither text whole).
//!
//! **Why it can stream.** A manifest's lines are sorted by path (`manifest::decode` refuses one that is not), so its
//! delta against another is a merge of two sorted line streams, as 7.2's manifest `fastdelta` makes it: a line in both is
//! kept; a base line whose path the new text passes is removed; a new line whose path the base passes is inserted; a path
//! in both on different lines (a new node or flag) is replaced. Nothing ahead of either stream's current line is needed.
//!
//! **What it makes.** Hunks in mpatch's form ([`delta::encode_hunk`]), each run of changes one hunk, a run whose data
//! passes a window cut into hunks of a window each (the next resting at the end of the one before, as mpatch takes
//! them). Applied over the base they make the new text exactly; their bytes are not `delta::encode_lines`' (which groups
//! hunks by its own line diff), and a reader holds only to the text.
//!
//! **Bounded.** It holds the base read ahead (a window, and the line that crosses it), the new text's line not yet ended,
//! and the run's data (a window and a line): each stream a window and a line at most, whatever the manifests' lengths.

use crate::delta::{self, BaseRead, Hunk};
use crate::read::to_u64;

/// Byte: a manifest line's end.
const NEWLINE: u8 = b'\n';
/// Byte: a manifest line's path's end.
const NUL: u8 = 0;

/// Why a diff stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
  /// The base's bytes from `at` are not held now: brought back, the diff is made again from its start.
  Unread {
    /// Where the base's read stopped.
    at: u64,
  },
  /// A hunk mpatch's form cannot carry (an offset past `i32::MAX`).
  Delta(delta::Refusal),
}

/// A manifest's delta against a base being made as the new text streams ([`Diffing::update`]) and the base is read
/// ahead of it (module docs).
#[derive(Clone, Debug, Default)]
pub struct Diffing {
  /// The base's bytes read and not yet passed, from `base_at`.
  base: Vec<u8>,
  /// Where `base` begins in the base: the base line next met.
  base_at: u64,
  /// The new text's line not yet ended.
  partial: Vec<u8>,
  /// The run of changes being made: where its hunk begins in the base, and its data so far.
  run: Option<(u64, Vec<u8>)>,
  /// The bytes the run's data and the base read ahead may each hold: the caller's window.
  window: usize,
}

impl Diffing {
  /// A diff whose run's data and base read ahead each hold `window` bytes and a line at most.
  pub fn within(window: usize) -> Diffing {
    Diffing {
      window: window.max(1),
      ..Diffing::default()
    }
  }

  /// The bytes it holds now: what a caller counts as text held.
  pub fn held(&self) -> u64 {
    to_u64(
      self
        .base
        .len()
        .saturating_add(self.partial.len())
        .saturating_add(self.run.as_ref().map_or(0, |(_, data)| data.len())),
    )
  }

  /// Takes `piece`, the new text's next bytes, its lines merged with the base's: the hunks they close appended to `out`.
  pub fn update(
    &mut self,
    piece: &[u8],
    base: &mut dyn BaseRead,
    out: &mut Vec<u8>,
  ) -> Result<(), Stopped> {
    let mut rest = piece;
    while let Some(end) = rest.iter().position(|byte| *byte == NEWLINE) {
      let (line, after) = rest.split_at(end.saturating_add(1));
      rest = after;
      if self.partial.is_empty() {
        self.line(line, base, out)?;
      } else {
        let mut whole = core::mem::take(&mut self.partial);
        whole.extend_from_slice(line);
        let taken = self.line(&whole, base, out);
        whole.clear();
        self.partial = whole;
        taken?;
      }
    }
    self.partial.extend_from_slice(rest);
    Ok(())
  }

  /// After the new text's last piece: a last line not ended taken as one, every base line it did not reach removed, and
  /// the last run's hunks appended to `out`.
  pub fn finish(mut self, base: &mut dyn BaseRead, out: &mut Vec<u8>) -> Result<(), Stopped> {
    if !self.partial.is_empty() {
      let last = core::mem::take(&mut self.partial);
      self.line(&last, base, out)?;
    }
    let end = base.len();
    if self.base_at < end {
      self.run.get_or_insert_with(|| (self.base_at, Vec::new()));
      self.base_at = end;
      self.base.clear();
    }
    self.close(out)
  }

  /// Merges `line`, the new text's next whole line, with the base's lines.
  fn line(
    &mut self,
    line: &[u8],
    base: &mut dyn BaseRead,
    out: &mut Vec<u8>,
  ) -> Result<(), Stopped> {
    loop {
      let Some(len) = self.base_line(base)? else {
        return self.insert(line, out);
      };
      let base_line = self.base.get(..len).unwrap_or_default();
      if base_line == line {
        self.close(out)?;
        self.pass(len, false);
        return Ok(());
      }
      match path(base_line).cmp(path(line)) {
        core::cmp::Ordering::Less => self.pass(len, true),
        core::cmp::Ordering::Greater => return self.insert(line, out),
        core::cmp::Ordering::Equal => {
          self.pass(len, true);
          return self.insert(line, out);
        }
      }
    }
  }

  /// The length of the base's next line, read ahead into `base` a window at a time until its end: none past the base's
  /// end. A last line with no newline is a line.
  fn base_line(&mut self, base: &mut dyn BaseRead) -> Result<Option<usize>, Stopped> {
    loop {
      if let Some(end) = self.base.iter().position(|byte| *byte == NEWLINE) {
        return Ok(Some(end.saturating_add(1)));
      }
      let read_to = self.base_at.saturating_add(to_u64(self.base.len()));
      let left = base.len().saturating_sub(read_to);
      if left == 0 {
        return Ok((!self.base.is_empty()).then_some(self.base.len()));
      }
      let n = usize::try_from(left).unwrap_or(usize::MAX).min(self.window);
      if base.read_into(read_to, n, &mut self.base).is_none() {
        return Err(Stopped::Unread { at: read_to });
      }
    }
  }

  /// Passes the base's next line, `len` bytes: removed into the run where `removed`, else kept.
  fn pass(&mut self, len: usize, removed: bool) {
    if removed {
      self.run.get_or_insert_with(|| (self.base_at, Vec::new()));
    }
    self.base.drain(..len.min(self.base.len()));
    self.base_at = self.base_at.saturating_add(to_u64(len));
  }

  /// Inserts `line` into the run at where the base stands, its data cut into a hunk where it passes the window.
  fn insert(&mut self, line: &[u8], out: &mut Vec<u8>) -> Result<(), Stopped> {
    let at = self.base_at;
    let (_, data) = self.run.get_or_insert_with(|| (at, Vec::new()));
    data.extend_from_slice(line);
    if data.len() >= self.window {
      self.close(out)?;
      self.run = Some((at, Vec::new()));
    }
    Ok(())
  }

  /// Closes the run: its hunk, resting where it began and ending where the base stands, appended to `out`; nothing for a
  /// run that changed nothing.
  fn close(&mut self, out: &mut Vec<u8>) -> Result<(), Stopped> {
    let Some((start, data)) = self.run.take() else {
      return Ok(());
    };
    if start == self.base_at && data.is_empty() {
      return Ok(());
    }
    let offset = |value: u64| {
      u32::try_from(value).map_err(|_| Stopped::Delta(delta::Refusal::TooLong { value }))
    };
    let hunk = Hunk {
      start: offset(start)?,
      end: offset(self.base_at)?,
      data: &data,
    };
    delta::encode_hunk(&hunk, out).map_err(Stopped::Delta)
  }
}

/// A manifest line's path: its bytes before its NUL, the whole line where it has none.
fn path(line: &[u8]) -> &[u8] {
  let end = line
    .iter()
    .position(|byte| *byte == NUL)
    .unwrap_or(line.len());
  line.get(..end).unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use proptest::prelude::*;

  use super::{Diffing, Stopped};
  use crate::delta::{self, BaseRead};

  /// A manifest's text of `entries` (a path, a node's seed and a flag), sorted by path, one line each.
  fn manifest(entries: &std::collections::BTreeMap<Vec<u8>, (u8, u8)>) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, (seed, flag)) in entries {
      out.extend_from_slice(path);
      out.push(0);
      out.extend(core::iter::repeat_n(
        b"0123456789abcdef"[usize::from(seed % 16)],
        40,
      ));
      match flag % 3 {
        0 => {}
        1 => out.push(b'x'),
        _ => out.push(b'l'),
      }
      out.push(b'\n');
    }
    out
  }

  /// `new`'s delta against `base` made by [`Diffing`], `new` fed `piece` bytes at a time within `window`; the most it
  /// held.
  fn diffed(
    base: &[u8],
    new: &[u8],
    (piece, window): (usize, usize),
  ) -> Result<(Vec<u8>, u64), Stopped> {
    let mut diffing = Diffing::within(window);
    let mut reader: &[u8] = base;
    let mut out = Vec::new();
    let mut most = 0;
    for chunk in new.chunks(piece.max(1)) {
      diffing.update(chunk, &mut reader as &mut dyn BaseRead, &mut out)?;
      most = most.max(diffing.held());
    }
    diffing.finish(&mut reader as &mut dyn BaseRead, &mut out)?;
    Ok((out, most))
  }

  /// Entries keyed by paths drawn from a few names and depths, long and short, so lines straddle every window size.
  fn entries() -> impl Strategy<Value = std::collections::BTreeMap<Vec<u8>, (u8, u8)>> {
    let path = (0u8..12, 0usize..40).prop_map(|(name, length)| {
      let mut path = format!("d{}/", name % 3).into_bytes();
      path.extend(core::iter::repeat_n(b'a' + name, length.max(1)));
      path
    });
    proptest::collection::btree_map(path, (any::<u8>(), any::<u8>()), 0..24)
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// Goal 93, C1 (2) (main's ruling (B'), oracle 1). Do: over generated sorted manifests and their edits (entries
    /// added, removed, given new nodes and flags; paths long and short), make the new one's delta against the base by
    /// [`Diffing`], the new text fed at piece sizes of 1 byte and more, within windows small enough that lines straddle
    /// them. Expect the delta applied over the base to make the new text, as `delta::encode_lines`' delta does; and what
    /// it held at most a window and a line of each stream and of its run.
    #[test]
    fn a_manifest_diffed_as_it_streams_makes_its_text(
      base in entries(),
      new in entries(),
      kept in proptest::collection::vec(any::<bool>(), 24),
      piece in 1usize..97,
      window in 1usize..65,
    ) {
      // The new manifest shares some of the base's entries, so lines are kept, removed, replaced and inserted.
      let mut merged = new.clone();
      for ((path, value), keep) in base.iter().zip(kept.iter().cycle()) {
        if *keep {
          merged.entry(path.clone()).or_insert(*value);
        }
      }
      let (base, new) = (manifest(&base), manifest(&merged));
      let (delta, most) = diffed(&base, &new, (piece, window)).unwrap();
      let room = &mut crate::Admission::new(u64::MAX);
      prop_assert_eq!(delta::apply(&base, &delta, room).unwrap(), new.clone());
      let mut lines = Vec::new();
      delta::encode_lines(&base, &new, &mut lines).unwrap();
      prop_assert_eq!(delta::apply(&base, &lines, room).unwrap(), new.clone());
      let longest = base
        .split(|byte| *byte == b'\n')
        .chain(new.split(|byte| *byte == b'\n'))
        .map(<[u8]>::len)
        .max()
        .unwrap_or(0)
        + 1;
      let bound = u64::try_from(3 * (window + longest + piece)).unwrap();
      prop_assert!(most <= bound, "held {} past {}", most, bound);
    }
  }

  /// Goal 93, C1 (2). Do: diff a manifest against the empty one, the empty one against a manifest, and a manifest
  /// against itself. Expect a whole insert, a whole removal, and no hunk at all.
  #[test]
  fn a_manifest_diffed_against_the_empty_one_or_itself() {
    let text = b"a\x0000000000000000000000000000000000000000\nb\x0011111111111111111111111111111111111111x\n";
    let room = &mut crate::Admission::new(u64::MAX);
    let (inserted, _) = diffed(b"", text, (7, 8)).unwrap();
    assert_eq!(delta::apply(b"", &inserted, room).unwrap(), text);
    let (removed, _) = diffed(text, b"", (7, 8)).unwrap();
    assert_eq!(delta::apply(text, &removed, room).unwrap(), b"");
    let (same, _) = diffed(text, text, (7, 8)).unwrap();
    assert!(same.is_empty(), "no hunk for a manifest against itself");
  }
}

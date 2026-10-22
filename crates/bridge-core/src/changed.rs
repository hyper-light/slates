//! What a transport's kernel holds stale (§4.6 "Cache posture"): the byte ranges of one file that changed after the
//! kernel last confirmed its cache of it, other than through the transport itself
//! (`docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`).
//!
//! **Why a transport needs it.** A FUSE kernel keeps a page a process dirtied through a shared map until it writes the
//! page back: periodically, at `msync`, or while it obeys an invalidation, since it launders a dirty page before it
//! drops it (Linux 6.12 `mm/truncate.c` `invalidate_inode_pages2_range` → `folio_launder`, `fs/fuse/file.c`
//! `fuse_launder_folio`). The page is the kernel's copy from before every change it has not yet confirmed, so its bytes
//! are stale exactly where another path changed the file since. Applying the whole page undid the later write: on
//! 2026-10-21, 4,095 of a page's 4,096 bytes reverted on Linux, 16,383 of 16,384 on macOS. A transport applies a cache
//! write-back only outside these ranges, which is what one shared page cache would hold.
//!
//! **The walk.** It reads the volume's journal after the kernel's confirmed point, oldest first, for one inode.
//! - Another path's data record adds the bytes it changed: an overwrite or an extension adds its range; a truncation
//!   adds everything from the new length; an insertion or a deletion adds everything from its offset, since the rest
//!   shifted; a drift adds the whole file.
//! - The transport's own record (its sequence in `own`) clears the bytes it wrote, since its own write refreshed its
//!   kernel's copy of them. Its own truncation clears everything past the new length, since the kernel dropped those
//!   pages itself.
//! - A record of metadata or of the namespace changes no bytes. A witness (a copy-up) copies the bytes the kernel
//!   already held.
//!
//! Pure over records, so it is tested on every host against a per-byte model (this module's tests).

use std::collections::BTreeMap;
use std::ops::Range;

use slates_vfs::ids::InodeNo;
use slates_vfs::journal::{Op, OpRecord};

/// Format: the end of a range that reaches the end of the file however long it is (a truncation's or a shift's tail).
pub const TO_THE_END: u64 = u64::MAX;

/// What changed in one file after a point, other than through one transport ([`crate::Bridge::changed_since`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Changed {
  /// These byte ranges, ascending, disjoint and not touching; an end of [`TO_THE_END`] reaches the end of the file.
  Known(Vec<Range<u64>>),
  /// The journal no longer holds every record since the point (retention evicted some), so what changed cannot be
  /// told. A transport must not guess: applying a stale page may revert an acknowledged write, and dropping it may
  /// discard data the process synced.
  Unknown,
}

/// The bytes of `ino` that `records` changed other than through the transport whose records' sequences lie in `own`
/// (ascending, disjoint spans), as the module doc says. `records` are the journal's after the transport's confirmed
/// point, oldest first.
pub fn changed_ranges<'r>(
  records: impl IntoIterator<Item = &'r OpRecord>,
  ino: InodeNo,
  own: &[Range<u64>],
) -> Vec<Range<u64>> {
  let mut changed = ByteRanges::default();
  let mut spans = own.iter().peekable();
  for record in records {
    // Spans and records both ascend, so one pass over each tells whether a record is the transport's own.
    while spans.next_if(|span| span.end <= record.seq).is_some() {}
    let mine = spans.peek().is_some_and(|span| span.contains(&record.seq));
    if record.inode == Some(ino) {
      apply(&mut changed, &record.op, mine);
    }
  }
  changed.into_ranges()
}

/// One record's effect on the changed bytes: another path's change adds, the transport's own write clears.
fn apply(changed: &mut ByteRanges, op: &Op, mine: bool) {
  match (op, mine) {
    (Op::Overwrite { at, len } | Op::Extend { at, len }, false) => {
      changed.insert(*at..at.saturating_add(*len));
    }
    (Op::Overwrite { at, len } | Op::Extend { at, len }, true) => {
      changed.remove(*at..at.saturating_add(*len));
    }
    (Op::Truncate { len }, false) => changed.insert(*len..TO_THE_END),
    (Op::Truncate { len }, true) => changed.remove(*len..TO_THE_END),
    (Op::Insert { at, .. } | Op::Delete { at, .. }, false) => changed.insert(*at..TO_THE_END),
    (Op::Drift, false) => changed.insert(0..TO_THE_END),
    _ => {}
  }
}

/// Disjoint, non-touching byte ranges, keyed by start: an insertion or a removal costs a logarithm and the ranges it
/// meets, so a walk over many records stays near linear. Bounded by the records walked, since each adds at most one.
#[derive(Debug, Default)]
struct ByteRanges {
  /// Start → end, half-open.
  ranges: BTreeMap<u64, u64>,
}

impl ByteRanges {
  /// Adds `range`, merging every range it overlaps or touches.
  fn insert(&mut self, range: Range<u64>) {
    if range.start >= range.end {
      return;
    }
    let (mut start, mut end) = (range.start, range.end);
    if let Some((&before, &before_end)) = self.ranges.range(..=start).next_back()
      && before_end >= start
    {
      start = before;
      end = end.max(before_end);
      self.ranges.remove(&before);
    }
    while let Some((&next, &next_end)) = self.ranges.range(start..=end).next() {
      end = end.max(next_end);
      self.ranges.remove(&next);
    }
    self.ranges.insert(start, end);
  }

  /// Removes `range`, keeping what lies on either side of it.
  fn remove(&mut self, range: Range<u64>) {
    if range.start >= range.end {
      return;
    }
    if let Some((&before, &before_end)) = self.ranges.range(..range.start).next_back()
      && before_end > range.start
    {
      self.ranges.insert(before, range.start);
      if before_end > range.end {
        self.ranges.insert(range.end, before_end);
      }
    }
    let inside: Vec<(u64, u64)> = self
      .ranges
      .range(range.start..range.end)
      .map(|(&start, &end)| (start, end))
      .collect();
    for (start, end) in inside {
      self.ranges.remove(&start);
      if end > range.end {
        self.ranges.insert(range.end, end);
      }
    }
  }

  fn into_ranges(self) -> Vec<Range<u64>> {
    self
      .ranges
      .into_iter()
      .map(|(start, end)| start..end)
      .collect()
  }
}

#[cfg(test)]
mod tests {
  // proptest's strategy types carry `Arc` (D-8's harness exception).
  #![allow(clippy::disallowed_types)]
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

  use super::*;
  use proptest::prelude::*;
  use slates_vfs::ids::Epoch;

  /// Shape: the bytes the model tracks — past every generated offset and length, so a tail to the end is a whole
  /// suffix of it.
  const MODEL_BYTES: u64 = 96;

  /// One generated record: whose it is, and what it did to the file (or to another one).
  #[derive(Clone, Debug)]
  struct Generated {
    mine: bool,
    other_inode: bool,
    op: Op,
  }

  fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
      (0..64u64, 1..32u64).prop_map(|(at, len)| Op::Overwrite { at, len }),
      (0..64u64, 1..32u64).prop_map(|(at, len)| Op::Extend { at, len }),
      (0..80u64).prop_map(|len| Op::Truncate { len }),
      (0..64u64, 1..16u64).prop_map(|(at, len)| Op::Insert { at, len }),
      (0..64u64, 1..16u64).prop_map(|(at, len)| Op::Delete { at, len }),
      Just(Op::Drift),
      Just(Op::Witness),
      Just(Op::Setattr),
      Just(Op::Create),
    ]
  }

  fn generated() -> impl Strategy<Value = Generated> {
    (any::<bool>(), prop::bool::weighted(0.15), op()).prop_map(|(mine, other_inode, op)| {
      Generated {
        mine,
        other_inode,
        op,
      }
    })
  }

  /// The records and the transport's own spans a generated history becomes: sequences from `first`, the transport's
  /// consecutive records coalesced into spans, as a transport records them.
  fn records_of(
    history: &[Generated],
    first: u64,
    ino: InodeNo,
  ) -> (Vec<OpRecord>, Vec<Range<u64>>) {
    let mut records = Vec::new();
    let mut own: Vec<Range<u64>> = Vec::new();
    for (index, step) in history.iter().enumerate() {
      let seq = first + u64::try_from(index).unwrap();
      let inode = if step.other_inode {
        InodeNo(ino.0 + 1)
      } else {
        ino
      };
      records.push(OpRecord {
        seq,
        op: step.op.clone(),
        path: "/f".into(),
        inode: Some(inode),
        epoch: Epoch(0),
        at: 0,
        prev_version: 0,
      });
      if step.mine {
        match own.last_mut() {
          Some(span) if span.end == seq => span.end = seq + 1,
          _ => own.push(seq..seq + 1),
        }
      }
    }
    (records, own)
  }

  /// The model, stated as the design's rule per byte: a byte is stale when the last record that touched it since the
  /// point was another path's change; the transport's own write of it makes it fresh again.
  fn model(history: &[Generated]) -> Vec<bool> {
    let mut stale = vec![false; usize::try_from(MODEL_BYTES).unwrap()];
    for step in history.iter().filter(|step| !step.other_inode) {
      let (from, to, mark) = match (&step.op, step.mine) {
        (Op::Overwrite { at, len } | Op::Extend { at, len }, mine) => (*at, at + len, !mine),
        (Op::Truncate { len }, mine) => (*len, MODEL_BYTES, !mine),
        (Op::Insert { at, .. } | Op::Delete { at, .. }, false) => (*at, MODEL_BYTES, true),
        (Op::Drift, false) => (0, MODEL_BYTES, true),
        _ => continue,
      };
      for byte in from.min(MODEL_BYTES)..to.min(MODEL_BYTES) {
        stale[usize::try_from(byte).unwrap()] = mark;
      }
    }
    stale
  }

  /// The walk's ranges as the same per-byte answer, over the model's bytes.
  fn as_bytes(ranges: &[Range<u64>]) -> Vec<bool> {
    (0..MODEL_BYTES)
      .map(|byte| ranges.iter().any(|range| range.contains(&byte)))
      .collect()
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
      cases: slates_test_seeds::cases(2000),
      .. ProptestConfig::default()
    }))]

    /// The dirty-page write-back rule (§4.6): over generated histories of another path's and the transport's own
    /// records, on the file and on another, the walk's stale bytes equal the per-byte model's, and its ranges are
    /// ascending, disjoint and not touching.
    #[test]
    fn the_walk_equals_the_per_byte_model(
      history in prop::collection::vec(generated(), 0..24),
      first in 1..1000u64,
    ) {
      let ino = InodeNo(7);
      let (records, own) = records_of(&history, first, ino);
      let ranges = changed_ranges(&records, ino, &own);
      prop_assert_eq!(as_bytes(&ranges), model(&history));
      for pair in ranges.windows(2) {
        prop_assert!(pair[0].end < pair[1].start, "{:?} touch or overlap", pair);
      }
      prop_assert!(ranges.iter().all(|range| range.start < range.end));
    }
  }

  /// The case measured on 2026-10-21: a whole replacement through another path (a truncation to nothing, then the new
  /// bytes) leaves every byte of the file stale, so a dirty page from before it is masked whole.
  #[test]
  fn a_replacement_through_another_path_makes_the_whole_file_stale() {
    let ino = InodeNo(3);
    let history = [
      Generated {
        mine: false,
        other_inode: false,
        op: Op::Truncate { len: 0 },
      },
      Generated {
        mine: false,
        other_inode: false,
        op: Op::Extend { at: 0, len: 8192 },
      },
    ];
    let (records, own) = records_of(&history, 40, ino);
    assert_eq!(changed_ranges(&records, ino, &own), vec![0..TO_THE_END]);
  }

  /// A write through another path over part of a page, then the transport's own write of some of the same bytes: only
  /// the bytes the other path wrote and the transport did not are stale.
  #[test]
  fn the_transports_own_write_clears_what_it_wrote() {
    let ino = InodeNo(3);
    let history = [
      Generated {
        mine: false,
        other_inode: false,
        op: Op::Overwrite { at: 100, len: 100 },
      },
      Generated {
        mine: true,
        other_inode: false,
        op: Op::Overwrite { at: 150, len: 100 },
      },
    ];
    let (records, own) = records_of(&history, 9, ino);
    assert_eq!(changed_ranges(&records, ino, &own), vec![100..150]);
  }
}

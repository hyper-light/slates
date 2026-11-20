//! One window made a part at a time (A-751): the window [`super::deltas`] makes of a target piece against a source
//! piece, and its encoding as [`crate::svndiff::Encoder::window`] writes it, byte for byte, its work cut into units a
//! caller takes a few at a time. One 100 KiB window cost about 150 us to deltify and up to 17 us to encode on an M-series
//! Mac (release, 2026-11-20), against a shard's step quantum of about 3.7 us, so a window made whole held a shard
//! milliseconds on CI's runners (A-749's measure: 4.7 ms on svn://, 6.5 ms on DAV). The update report's drive and the
//! dump exporter's format 3 make every window through this.
//!
//! **The units.** A unit is one position of the target walked, one byte compared or copied, one source block indexed
//! (its 64 bytes), or one instruction encoded; a step takes as many as it is given, at least one, and stops between
//! units. The making keeps its place between steps (where the walk stands, the match being extended, the instructions
//! and new data so far, the source's block table), never a borrow of the texts: each step is handed the same two
//! pieces again, so a caller that lends its texts a step at a time makes the window across its steps.
//!
//! **The same bytes.** Each phase is `xdelta.c`'s `compute_delta` as `super::compute_delta` ports it, cut at its loops:
//! the common start, the table of the source's blocks, the walk with its quick skip, a match's forward and backward
//! extension, the new data before it, the take-back when it meets the last instruction, the trailer; new data appended
//! in parts merges into one instruction exactly as appended whole (`Ops::new_data`). The sections are the encoder's:
//! version 0 as they are, version 2 each after its length and as an LZ4 block when that is shorter, the block made a part
//! at a time by `crate::svndiff::lz4`, which makes `lz4_flex`'s bytes. The tests hold every window and its bytes to
//! [`super::deltas`] and the encoder over every unit size.
//!
//! No I/O, no clock, no randomness, no panic.

#[cfg(test)]
mod tests;

use super::{
  BLOCK, FLAG_BYTES, NO_POSITION, Ops, SHORTEST_KEPT, block_sum, flag, hash, roll, to_u64,
};
use crate::svndiff::lz4::Compressing;
use crate::svndiff::{
  Compression, INSTRUCTION_SECTION_LIMIT, Instruction, NEW_DATA_SECTION_LIMIT, SvndiffError,
  Version, WINDOW_LIMIT, write_instruction, write_integer,
};

/// The source's whole blocks by their sums (`super::Blocks`), held without a borrow of the source.
#[derive(Clone, Debug, Default)]
struct Table {
  /// Each slot's sum and its block's position, [`NO_POSITION`] where empty.
  slots: Vec<(u32, u32)>,
  /// The largest slot index, the table's size less one.
  mask: u32,
  /// A bit per sum class that says it may have a block.
  flags: Vec<u8>,
}

impl Table {
  /// An empty table for a source of `len` bytes: twice the next power of two above its block count of slots.
  fn sized(len: usize) -> Self {
    let blocks = len.checked_div(BLOCK).unwrap_or(0).saturating_add(1);
    let mut slots: usize = 1;
    while slots <= blocks {
      slots = slots.saturating_mul(2);
    }
    slots = slots.saturating_mul(2);
    Self {
      slots: vec![(0, NO_POSITION); slots],
      mask: u32::try_from(slots.saturating_sub(1)).unwrap_or(u32::MAX),
      flags: vec![0; FLAG_BYTES],
    }
  }

  /// The block of `source` at `position` (`add_block`), unless an equal block holds its sum's place already.
  fn add(&mut self, source: &[u8], sum: u32, position: usize) {
    let Ok(at) = u32::try_from(position) else {
      return;
    };
    let mut slot = hash(sum) & self.mask;
    loop {
      let Some(&(held_sum, held_position)) = self.slots.get(slot as usize) else {
        return;
      };
      if held_position == NO_POSITION {
        break;
      }
      if held_sum == sum && block_of(source, held_position) == block_of(source, at) {
        return;
      }
      slot = slot.wrapping_add(1) & self.mask;
    }
    if let Some(entry) = self.slots.get_mut(slot as usize) {
      *entry = (sum, at);
    }
    let (byte, bit) = flag(sum);
    if let Some(flags) = self.flags.get_mut(byte) {
      *flags |= bit;
    }
  }

  /// Whether a sum may have a block.
  fn may_hold(&self, sum: u32) -> bool {
    let (byte, bit) = flag(sum);
    self.flags.get(byte).is_some_and(|flags| flags & bit != 0)
  }

  /// The position of a block of `source` equal to `bytes` with `sum` (`find_block`).
  fn find(&self, source: &[u8], sum: u32, bytes: &[u8]) -> Option<usize> {
    let mut slot = hash(sum) & self.mask;
    loop {
      let &(held_sum, held_position) = self.slots.get(slot as usize)?;
      if held_position == NO_POSITION {
        return None;
      }
      if held_sum == sum && block_of(source, held_position) == Some(bytes) {
        return usize::try_from(held_position).ok();
      }
      slot = slot.wrapping_add(1) & self.mask;
    }
  }
}

/// The block of `text` starting at `position`.
fn block_of(text: &[u8], position: u32) -> Option<&[u8]> {
  let start = usize::try_from(position).ok()?;
  text.get(start..start.checked_add(BLOCK)?)
}

/// A match being made: where it starts in the source and the target, and its length so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Found {
  /// Its start in the source.
  from: usize,
  /// Its start in the target.
  to: usize,
  /// Its length.
  length: usize,
}

/// Where the making stands.
#[derive(Clone, Debug)]
enum Phase {
  /// The source is empty: the target is new data, `copied` bytes of it so far (`compute_window`).
  Fresh {
    /// The target bytes appended.
    copied: usize,
  },
  /// The common start, `matched` bytes so far.
  Prefix {
    /// The bytes found equal.
    matched: usize,
  },
  /// The source's blocks put in the table up to `position`.
  Index {
    /// The next block's start.
    position: usize,
  },
  /// The walk's head: the quick skip, then a block's look-up.
  Walk,
  /// A match extended forward, `compared` bytes past its block of `most`.
  Forward {
    /// The match.
    found: Found,
    /// The most bytes it may run on.
    most: usize,
    /// The bytes past its block found equal.
    compared: usize,
  },
  /// A match extended back.
  Backward(Found),
  /// The new data between the walk's last instruction and a match, appended up to `upto`.
  Gap {
    /// The next target byte to append.
    upto: usize,
    /// The match that follows.
    found: Found,
  },
  /// How far a match meeting the last instruction reaches back over it, `matched` bytes of `most` so far.
  Reach {
    /// The match.
    found: Found,
    /// The most it may reach.
    most: usize,
    /// The bytes found equal.
    matched: usize,
  },
  /// The last instructions taken back over `most` bytes, `taken` so far (`svn_txdelta__remove_copy`).
  TakeBack {
    /// The match.
    found: Found,
    /// The most to take back.
    most: usize,
    /// The bytes taken back.
    taken: usize,
  },
  /// The trailer's common end, from target position `start`, `matched` bytes so far.
  Trailer {
    /// Where the trailer begins in the target.
    start: usize,
    /// The bytes found equal at the ends.
    matched: usize,
  },
  /// The trailer's new data appended up to `upto`, short of `stop`; its end's copy of `end` bytes next.
  TrailerData {
    /// The next target byte to append.
    upto: usize,
    /// Where the new data stops.
    stop: usize,
    /// The common end copied after it.
    end: usize,
  },
  /// The instructions encoded up to `next`.
  Encode {
    /// The next instruction to encode.
    next: usize,
  },
  /// A version-2 section's LZ4 block being made: 0 the instructions, 1 the new data.
  Compress {
    /// Which section.
    which: usize,
    /// Its block.
    making: Box<Compressing>,
  },
  /// The window is made.
  Done,
}

/// One window in the making (module documentation).
#[derive(Clone, Debug)]
pub struct WindowMaking {
  /// The svndiff version of its encoding.
  version: Version,
  /// How a version-2 window's sections are stored.
  compression: Compression,
  /// Where its source piece starts in the source.
  source_offset: u64,
  /// Its source piece's length.
  source_len: usize,
  /// Its target piece's length.
  target_len: usize,
  /// Where the making stands.
  phase: Phase,
  /// The instructions and new data so far.
  ops: Ops,
  /// The source's block table.
  table: Table,
  /// The walk's position in the target.
  at: usize,
  /// The target's first byte no instruction makes yet.
  pending: usize,
  /// The rolling sum at `at`.
  sum: u32,
  /// The walk's end: the last position a whole block starts at.
  upper: usize,
  /// The instruction section's bytes.
  instruction_bytes: Vec<u8>,
  /// Each version-2 section's length before it.
  prefixes: [Vec<u8>; 2],
  /// Each version-2 section's LZ4 block, where it is shorter.
  compressed: [Option<Vec<u8>>; 2],
  /// The window's five integers, once it is made.
  integers: Vec<u8>,
}

/// A made window's encoding, in the parts `svndiff.c`'s writes take (`crate::report::editor`'s `DeltaWrites`).
#[derive(Clone, Copy, Debug)]
pub struct Made<'a> {
  /// The window's five integers.
  pub integers: &'a [u8],
  /// The instruction section: its length before it (version 2), then its bytes.
  pub instructions: [&'a [u8]; 2],
  /// The new-data section: its length before it (version 2), then its bytes.
  pub new_data: [&'a [u8]; 2],
  /// Its instructions.
  pub ops: &'a [Instruction],
  /// Where its source view starts.
  pub source_offset: u64,
  /// Its source view's length.
  pub source_len: u64,
  /// The target bytes it makes.
  pub target_len: u64,
}

impl Made<'_> {
  /// The window's encoding whole: its integers, then its sections.
  pub fn bytes(&self) -> Vec<u8> {
    let mut out = self.integers.to_vec();
    for part in self.instructions.iter().chain(&self.new_data) {
      out.extend_from_slice(part);
    }
    out
  }
}

impl WindowMaking {
  /// The making of the window of a target piece of `target_len` bytes against a source piece of `source_len` bytes
  /// starting at `source_offset`, encoded in `version` with `compression`.
  ///
  /// # Errors
  /// [`SvndiffError::WindowTooLarge`] for a piece past [`WINDOW_LIMIT`].
  pub fn new(
    (source_offset, source_len): (u64, usize),
    target_len: usize,
    version: Version,
    compression: Compression,
  ) -> Result<Self, SvndiffError> {
    if to_u64(source_len) > WINDOW_LIMIT || to_u64(target_len) > WINDOW_LIMIT {
      return Err(SvndiffError::WindowTooLarge);
    }
    let phase = if source_len == 0 {
      Phase::Fresh { copied: 0 }
    } else {
      Phase::Prefix { matched: 0 }
    };
    Ok(Self {
      version,
      compression,
      source_offset,
      source_len,
      target_len,
      phase,
      ops: Ops::default(),
      table: Table::default(),
      at: 0,
      pending: 0,
      sum: 0,
      upper: 0,
      instruction_bytes: Vec::new(),
      prefixes: [Vec::new(), Vec::new()],
      compressed: [None, None],
      integers: Vec::new(),
    })
  }

  /// Takes up to `work` units of the making over `source` and `target`, the pieces it was made for (module
  /// documentation, "The units"); at least one. Whether the window is made.
  ///
  /// # Errors
  /// [`SvndiffError::CorruptWindowHeader`] for pieces of other lengths than it was made for, and
  /// [`SvndiffError::WindowTooLarge`] for a section past the format's limits; the making is over then.
  pub fn step(&mut self, source: &[u8], target: &[u8], work: usize) -> Result<bool, SvndiffError> {
    if source.len() != self.source_len || target.len() != self.target_len {
      self.phase = Phase::Done;
      return Err(SvndiffError::CorruptWindowHeader);
    }
    let mut left = work.max(1);
    while left > 0 {
      if let Phase::Done = self.phase {
        break;
      }
      let used = self.unit(source, target, left)?;
      left = left.saturating_sub(used.max(1));
    }
    Ok(matches!(self.phase, Phase::Done))
  }

  /// The window's encoding, once it is made.
  pub fn made(&self) -> Option<Made<'_>> {
    if !matches!(self.phase, Phase::Done) || self.integers.is_empty() {
      return None;
    }
    let [instruction_prefix, new_prefix] = &self.prefixes;
    let [instruction_block, new_block] = &self.compressed;
    Some(Made {
      integers: &self.integers,
      instructions: [
        instruction_prefix,
        instruction_block
          .as_deref()
          .unwrap_or(&self.instruction_bytes),
      ],
      new_data: [
        new_prefix,
        new_block.as_deref().unwrap_or(&self.ops.new_data),
      ],
      ops: &self.ops.instructions,
      source_offset: self.source_offset,
      source_len: to_u64(self.source_len),
      target_len: to_u64(self.target_len),
    })
  }

  /// One piece of the making within `left` units: the units it took.
  fn unit(&mut self, source: &[u8], target: &[u8], left: usize) -> Result<usize, SvndiffError> {
    let phase = std::mem::replace(&mut self.phase, Phase::Done);
    let used = match phase {
      Phase::Fresh { copied } => self.fresh(target, copied, left),
      Phase::Prefix { matched } => self.prefix(source, target, matched, left),
      Phase::Index { position } => self.index(source, target, position, left),
      Phase::Walk => self.walk(source, target, left),
      Phase::Forward {
        found,
        most,
        compared,
      } => self.forward(source, target, (found, most, compared), left),
      Phase::Backward(found) => self.backward(source, target, found, left),
      Phase::Gap { upto, found } => self.gap(target, upto, found, left),
      Phase::Reach {
        found,
        most,
        matched,
      } => self.reach(source, target, (found, most, matched), left),
      Phase::TakeBack { found, most, taken } => self.take_back(target, (found, most, taken), left),
      Phase::Trailer { start, matched } => self.trailer(source, target, start, matched, left),
      Phase::TrailerData { upto, stop, end } => {
        self.trailer_data(source, target, (upto, stop, end), left)
      }
      Phase::Encode { next } => self.encode(next, left)?,
      Phase::Compress { which, making } => self.compress(which, making, left)?,
      Phase::Done => 1,
    };
    Ok(used)
  }

  /// New data from the target, a byte a unit (`compute_window`'s window with an empty source view).
  fn fresh(&mut self, target: &[u8], copied: usize, left: usize) -> usize {
    let end = target.len().min(copied.saturating_add(left));
    self
      .ops
      .new_data(target.get(copied..end).unwrap_or_default());
    self.phase = if end < target.len() {
      Phase::Fresh { copied: end }
    } else {
      Phase::Encode { next: 0 }
    };
    end.saturating_sub(copied)
  }

  /// The common start, a byte a unit; once found, kept as a copy when worth one, then the walk's table begun, or the
  /// trailer where no whole block is left on either side.
  fn prefix(&mut self, source: &[u8], target: &[u8], matched: usize, left: usize) -> usize {
    let most = source.len().min(target.len());
    let (same, more) = count_same(
      source.get(matched..most).unwrap_or_default(),
      target.get(matched..most).unwrap_or_default(),
      left,
    );
    let matched = matched.saturating_add(same);
    if more {
      self.phase = Phase::Prefix { matched };
      return same;
    }
    let mut at = matched;
    if at > SHORTEST_KEPT || at == target.len() {
      self.ops.source_copy(0, at);
      self.pending = at;
    } else {
      at = 0;
    }
    self.at = at;
    if target.len().saturating_sub(at) < BLOCK || source.len() < BLOCK {
      self.phase = Phase::Trailer {
        start: at,
        matched: 0,
      };
    } else {
      self.upper = target.len().saturating_sub(BLOCK);
      self.table = Table::sized(source.len());
      self.phase = Phase::Index { position: 0 };
    }
    same
  }

  /// The source's whole blocks put in the table, a block of [`BLOCK`] units at a time; then the walk's first sum.
  fn index(&mut self, source: &[u8], target: &[u8], position: usize, left: usize) -> usize {
    let mut position = position;
    let mut used = 0usize;
    while used < left {
      let Some(block) = source
        .get(position..position.saturating_add(BLOCK))
        .filter(|block| block.len() == BLOCK)
      else {
        self.sum = block_at(target, self.at).map_or(0, block_sum);
        self.phase = Phase::Walk;
        return used;
      };
      self.table.add(source, block_sum(block), position);
      position = position.saturating_add(BLOCK);
      used = used.saturating_add(BLOCK);
    }
    self.phase = Phase::Index { position };
    used
  }

  /// The walk's head: past the positions whose sums the flags say have no block, a unit each; at one that may, its
  /// block looked up, a block's units: no match moves on one byte, a match is extended. At the walk's end, the trailer.
  fn walk(&mut self, source: &[u8], target: &[u8], left: usize) -> usize {
    let mut used = 0usize;
    loop {
      if self.at >= self.upper {
        self.phase = Phase::Trailer {
          start: self.pending,
          matched: 0,
        };
        return used;
      }
      while !self.table.may_hold(self.sum) && self.at < self.upper {
        if used >= left {
          self.phase = Phase::Walk;
          return used;
        }
        self.sum = rolled(target, self.sum, self.at);
        self.at = self.at.saturating_add(1);
        used = used.saturating_add(1);
      }
      // The look-up follows the skip in the same step, as `compute_delta` makes it at the position the skip stopped
      // at, the walk's end included.
      used = used.saturating_add(BLOCK);
      let found =
        block_at(target, self.at).and_then(|block| self.table.find(source, self.sum, block));
      let Some(from) = found else {
        if self.at.saturating_add(BLOCK) < target.len() {
          self.sum = rolled(target, self.sum, self.at);
        }
        self.at = self.at.saturating_add(1);
        continue;
      };
      let to = self.at;
      let most = source
        .len()
        .saturating_sub(from)
        .saturating_sub(BLOCK)
        .min(target.len().saturating_sub(to).saturating_sub(BLOCK));
      self.phase = Phase::Forward {
        found: Found {
          from,
          to,
          length: BLOCK,
        },
        most,
        compared: 0,
      };
      return used;
    }
  }

  /// A match extended forward past its block, a byte a unit (`find_match`'s common start).
  fn forward(
    &mut self,
    source: &[u8],
    target: &[u8],
    (found, most, compared): (Found, usize, usize),
    left: usize,
  ) -> usize {
    let source_at = found.from.saturating_add(BLOCK).saturating_add(compared);
    let target_at = found.to.saturating_add(BLOCK).saturating_add(compared);
    let rest = most.saturating_sub(compared);
    let (same, more) = count_same(
      source.get(source_at..).unwrap_or_default(),
      target.get(target_at..).unwrap_or_default(),
      left.min(rest),
    );
    let compared = compared.saturating_add(same);
    self.phase = if more && compared < most {
      Phase::Forward {
        found,
        most,
        compared,
      }
    } else {
      Phase::Backward(Found {
        length: BLOCK.saturating_add(compared),
        ..found
      })
    };
    same
  }

  /// A match extended back, a byte a unit, past no byte an instruction makes already (`find_match`); then the new data
  /// before it, or how far it reaches back over the last instruction.
  fn backward(&mut self, source: &[u8], target: &[u8], found: Found, left: usize) -> usize {
    let Found {
      mut from,
      mut to,
      mut length,
    } = found;
    let mut used = 0usize;
    let floor = self.pending;
    let before = |from: usize, to: usize| {
      from > 0
        && to > floor
        && matches!(
          (source.get(from.saturating_sub(1)), target.get(to.saturating_sub(1))),
          (Some(a), Some(b)) if a == b
        )
    };
    while used < left && before(from, to) {
      from = from.saturating_sub(1);
      to = to.saturating_sub(1);
      length = length.saturating_add(1);
      used = used.saturating_add(1);
    }
    let found = Found { from, to, length };
    self.phase = if before(from, to) {
      Phase::Backward(found)
    } else if to > self.pending {
      Phase::Gap {
        upto: self.pending,
        found,
      }
    } else {
      Phase::Reach {
        found,
        most: from.min(to),
        matched: 0,
      }
    };
    used
  }

  /// The new data before a match, a byte a unit (`take_match`); then the match's copy.
  fn gap(&mut self, target: &[u8], upto: usize, found: Found, left: usize) -> usize {
    let end = found.to.min(upto.saturating_add(left));
    self.ops.new_data(target.get(upto..end).unwrap_or_default());
    if end < found.to {
      self.phase = Phase::Gap { upto: end, found };
    } else {
      self.copy(target, found);
    }
    end.saturating_sub(upto)
  }

  /// How far a match meeting the last instruction reaches back over it, a byte a unit (`take_match`'s common end);
  /// then that much taken back, or the match's copy where it reaches nothing.
  fn reach(
    &mut self,
    source: &[u8],
    target: &[u8],
    (found, most, matched): (Found, usize, usize),
    left: usize,
  ) -> usize {
    let (same, more) = count_same_back(
      source.get(..found.from).unwrap_or_default(),
      target.get(..found.to).unwrap_or_default(),
      matched,
      left.min(most.saturating_sub(matched)),
    );
    let matched = matched.saturating_add(same);
    if more && matched < most {
      self.phase = Phase::Reach {
        found,
        most,
        matched,
      };
    } else if matched > 0 {
      self.phase = Phase::TakeBack {
        found,
        most: matched,
        taken: 0,
      };
    } else {
      self.copy(target, found);
    }
    same
  }

  /// The last instructions taken back, an instruction a unit (`svn_txdelta__remove_copy`, `super::Ops::take_back`);
  /// then the match grown by what was taken, and its copy.
  fn take_back(
    &mut self,
    target: &[u8],
    (found, most, taken): (Found, usize, usize),
    left: usize,
  ) -> usize {
    let mut taken = taken;
    let mut used = 0usize;
    let mut whole = false;
    while used < left {
      used = used.saturating_add(1);
      let Some(&last) = self.ops.instructions.last() else {
        whole = true;
        break;
      };
      let length = match last {
        Instruction::Target { .. } => {
          whole = true;
          break;
        }
        Instruction::Source { length, .. } | Instruction::New { length } => {
          usize::try_from(length).unwrap_or(usize::MAX)
        }
      };
      if length.saturating_add(taken) > most {
        if let Instruction::New { .. } = last {
          self.ops.cut_new_data(most.saturating_sub(taken));
          taken = most;
        }
        whole = true;
        break;
      }
      if let Instruction::New { .. } = last {
        self.ops.cut_new_data(length);
      }
      self.ops.instructions.pop();
      taken = taken.saturating_add(length);
    }
    if whole {
      self.copy(
        target,
        Found {
          from: found.from.saturating_sub(taken),
          to: found.to.saturating_sub(taken),
          length: found.length.saturating_add(taken),
        },
      );
    } else {
      self.phase = Phase::TakeBack { found, most, taken };
    }
    used
  }

  /// A match's copy, and the walk on past it, its next block's sum taken.
  fn copy(&mut self, target: &[u8], found: Found) {
    self.ops.source_copy(found.from, found.length);
    self.at = found.to.saturating_add(found.length);
    self.pending = self.at;
    if let Some(block) = block_at(target, self.at) {
      self.sum = block_sum(block);
    }
    self.phase = Phase::Walk;
  }

  /// The trailer's common end with the source, a byte a unit (`store_delta_trailer`); then its new data.
  fn trailer(
    &mut self,
    source: &[u8],
    target: &[u8],
    start: usize,
    matched: usize,
    left: usize,
  ) -> usize {
    let rest = target.len().saturating_sub(start);
    let most = source.len().min(rest);
    if most == 0 {
      self.phase = Phase::Encode { next: 0 };
      return 0;
    }
    let (same, more) = count_same_back(
      source,
      target,
      matched,
      left.min(most.saturating_sub(matched)),
    );
    let matched = matched.saturating_add(same);
    if more && matched < most {
      self.phase = Phase::Trailer { start, matched };
      return same;
    }
    let end = if matched <= SHORTEST_KEPT { 0 } else { matched };
    self.phase = Phase::TrailerData {
      upto: start,
      stop: target.len().saturating_sub(end),
      end,
    };
    same
  }

  /// The trailer's new data, a byte a unit; then its common end's copy, and the window's encoding.
  fn trailer_data(
    &mut self,
    source: &[u8],
    target: &[u8],
    (upto, stop, end): (usize, usize, usize),
    left: usize,
  ) -> usize {
    let until = stop.min(upto.saturating_add(left));
    self
      .ops
      .new_data(target.get(upto..until).unwrap_or_default());
    if until < stop {
      self.phase = Phase::TrailerData {
        upto: until,
        stop,
        end,
      };
    } else {
      if end > 0 {
        self.ops.source_copy(source.len().saturating_sub(end), end);
      }
      self.phase = Phase::Encode { next: 0 };
    }
    until.saturating_sub(upto)
  }

  /// The instructions encoded, one a unit (`encode_window`); then each version-2 section's length, its LZ4 block next
  /// where asked for, or the window's integers.
  fn encode(&mut self, next: usize, left: usize) -> Result<usize, SvndiffError> {
    let end = self.ops.instructions.len().min(next.saturating_add(left));
    for &instruction in self.ops.instructions.get(next..end).unwrap_or_default() {
      write_instruction(instruction, &mut self.instruction_bytes);
    }
    if end < self.ops.instructions.len() {
      self.phase = Phase::Encode { next: end };
      return Ok(end.saturating_sub(next));
    }
    if self.version != Version::Zero {
      let lengths = [self.instruction_bytes.len(), self.ops.new_data.len()];
      for (prefix, length) in self.prefixes.iter_mut().zip(lengths) {
        write_integer(to_u64(length), prefix);
      }
      if self.compression == Compression::Lz4 {
        self.phase = self.compressing(0);
        return Ok(end.saturating_sub(next));
      }
    }
    self.finish()?;
    Ok(end.saturating_sub(next))
  }

  /// The making of section `which`'s LZ4 block.
  fn compressing(&self, which: usize) -> Phase {
    let len = self.section(which).len();
    Phase::Compress {
      which,
      making: Box::new(Compressing::new(len, len)),
    }
  }

  /// Section `which`'s bytes as they are: 0 the instructions, 1 the new data.
  fn section(&self, which: usize) -> &[u8] {
    if which == 0 {
      &self.instruction_bytes
    } else {
      &self.ops.new_data
    }
  }

  /// A version-2 section's LZ4 block, `left` units of it; kept where shorter than the section, then the next section's,
  /// or the window's integers.
  fn compress(
    &mut self,
    which: usize,
    mut making: Box<Compressing>,
    left: usize,
  ) -> Result<usize, SvndiffError> {
    let used = making.step(self.section(which), left);
    if !making.over() {
      self.phase = Phase::Compress { which, making };
      return Ok(used);
    }
    if let Some(slot) = self.compressed.get_mut(which) {
      *slot = making.block().map(<[u8]>::to_vec);
    }
    if which == 0 {
      self.phase = self.compressing(1);
    } else {
      self.finish()?;
    }
    Ok(used)
  }

  /// The window's five integers, its sections held to the format's limits; the window made.
  fn finish(&mut self) -> Result<(), SvndiffError> {
    let sizes: Vec<u64> = (0..2)
      .map(|which| {
        let prefix = self.prefixes.get(which).map_or(0, Vec::len);
        let body = self
          .compressed
          .get(which)
          .and_then(Option::as_ref)
          .map_or(self.section(which).len(), Vec::len);
        to_u64(prefix.saturating_add(body))
      })
      .collect();
    let (instructions, new_data) = (
      sizes.first().copied().unwrap_or(0),
      sizes.get(1).copied().unwrap_or(0),
    );
    self.phase = Phase::Done;
    if instructions > INSTRUCTION_SECTION_LIMIT || new_data > NEW_DATA_SECTION_LIMIT {
      return Err(SvndiffError::WindowTooLarge);
    }
    for value in [
      self.source_offset,
      to_u64(self.source_len),
      to_u64(self.target_len),
      instructions,
      new_data,
    ] {
      write_integer(value, &mut self.integers);
    }
    Ok(())
  }
}

/// The bytes `a` and `b` share from their starts, at most `most`, and whether that many were equal so more may be.
fn count_same(a: &[u8], b: &[u8], most: usize) -> (usize, bool) {
  let span = most.min(a.len()).min(b.len());
  let same = a
    .iter()
    .zip(b)
    .take(span)
    .take_while(|(left, right)| left == right)
    .count();
  (same, same == span && span == most)
}

/// The bytes `a` and `b` share at their ends past the `skip` already found equal, at most `most` more, and whether that
/// many were equal so more may be.
fn count_same_back(a: &[u8], b: &[u8], skip: usize, most: usize) -> (usize, bool) {
  let a = a.get(..a.len().saturating_sub(skip)).unwrap_or_default();
  let b = b.get(..b.len().saturating_sub(skip)).unwrap_or_default();
  let span = most.min(a.len()).min(b.len());
  let same = a
    .iter()
    .rev()
    .zip(b.iter().rev())
    .take(span)
    .take_while(|(left, right)| left == right)
    .count();
  (same, same == span && span == most)
}

/// The block of `text` at `at`, when whole.
fn block_at(text: &[u8], at: usize) -> Option<&[u8]> {
  text.get(at..at.checked_add(BLOCK)?)
}

/// The rolling sum moved past the target's byte at `at`.
fn rolled(target: &[u8], sum: u32, at: usize) -> u32 {
  match (target.get(at), target.get(at.saturating_add(BLOCK))) {
    (Some(&out), Some(&entering)) => roll(sum, out, entering),
    _ => sum,
  }
}

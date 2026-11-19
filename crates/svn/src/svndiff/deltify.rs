//! Subversion's deltifier (A-652): the windows a stock server makes of a text against its base, the crate's one delta
//! generator. The export of a format-3 dump (A-628) writes its texts with it, and a reporter's drive sends a client its
//! changed texts with it.
//!
//! **The windows** are `text_delta.c`'s (`txdelta_next_window` 356-397, `svn_txdelta2`, and `tpush_write_handler`
//! 508-556, the push form FSFS writes its stored deltas with, which makes the same windows): the target is cut into
//! pieces of the window's room, the last one shorter; the source into pieces of the same room in step with them,
//! source piece *i* beside target piece *i*, until the source runs out, after which each window has an empty source
//! view starting where the source ended. FSFS answers `svn_fs_get_file_delta_stream` with the delta it stored when it
//! stored one against the very base asked for, and computes one otherwise (`fs_fs.c`
//! `svn_fs_fs__get_file_delta_stream`); both are these windows, so a dump's or a drive's delta is a function of the
//! two texts alone.
//!
//! **A window's instructions** are `xdelta.c`'s (`compute_delta` 371-480): the longest common start copied when it is
//! longer than four bytes; the source's 64-byte blocks indexed by a pseudo-Adler-32 sum in an open-addressing table;
//! the target walked a byte at a time with the rolling sum, a block that matches extended forward and back, the bytes
//! between matches kept as new data; a match that meets the previous instruction may take back what that instruction
//! copied when the match reaches further back; and the longest common end copied when it is longer than four bytes.
//! Instructions are merged as `svn_txdelta__insert_op` merges them (text_delta.c 209-278): new data after new data, and
//! a source copy that continues the previous one. A window with an empty source view is the target as new data
//! (`compute_window` 146-170).
//!
//! The port keeps Subversion's integers: the sums wrap in 32 bits, as C's unsigned arithmetic does, and every position
//! is bounded by the window's room. No I/O, no clock, no randomness, no panic.

use std::num::NonZeroUsize;

use super::{Instruction, SvndiffError, WINDOW_LIMIT, Window};

/// The length of the blocks of a source piece the deltifier indexes.
/// Format: `xdelta.c` `MATCH_BLOCKSIZE`, 64.
const BLOCK: usize = 64;

/// The bytes of the table of flags that say a sum may have a block.
/// Format: `xdelta.c` `FLAGS_COUNT`, 32 × 1024 bits, held as `FLAGS_COUNT / 8` bytes.
const FLAG_BYTES: usize = 4096;

/// The flag bits a byte of the flags table holds.
/// Format: `xdelta.c` `hash_flags`, a bit a sum's three low bits name.
const FLAG_BITS: u32 = 7;

/// How far right a sum is shifted when it is folded into the table's hash.
/// Format: `xdelta.c` `hash_func`, `sum ^ (sum >> 12)`.
const HASH_FOLD: u32 = 12;

/// How far right a sum is shifted to name its byte of the flags table.
/// Format: `xdelta.c` `hash_flags`, `(sum >> 16) & ((FLAGS_COUNT / 8) - 1)`.
const FLAG_SHIFT: u32 = 16;

/// The multiplier of a sum's upper half: a pseudo-Adler-32's second sum sits above its first.
/// Format: `xdelta.c` `init_adler32` (`s2 * 0x10000 + s1`) and `adler32_replace`.
const UPPER_HALF: u32 = 0x10000;

/// The longest common start or end not worth a copy: a copy of more bytes is shorter than its new data.
/// Format: `xdelta.c` `compute_delta` (`lo > 4`) and `store_delta_trailer` (`end_match <= 4`).
const SHORTEST_KEPT: usize = 4;

/// The block length as a 32-bit factor of the rolling sum.
/// Format: `xdelta.c` `adler32_replace`, `MATCH_BLOCKSIZE * 0x10000u`.
const BLOCK_FACTOR: u32 = 64;

/// The windows Subversion's deltifier makes of `target` against `source`, each window making at most `window_room`
/// bytes of the target from a source view of at most as many (the module documentation). A stock server's windows are
/// these with `window_room` [`WINDOW_LIMIT`], `SVN_DELTA_WINDOW_SIZE`.
///
/// # Errors
/// [`SvndiffError::WindowTooLarge`] for a room past [`WINDOW_LIMIT`], whose windows no stock decoder takes.
pub fn deltas<'a>(
  source: &'a [u8],
  target: &'a [u8],
  window_room: NonZeroUsize,
) -> Result<Deltas<'a>, SvndiffError> {
  let room = window_room.get();
  if to_u64(room) > WINDOW_LIMIT {
    return Err(SvndiffError::WindowTooLarge);
  }
  Ok(Deltas {
    source,
    target,
    room,
    source_read: 0,
    more_source: true,
    target_read: 0,
  })
}

/// The windows of one delta, made one at a time as they are asked for ([`deltas`]).
#[derive(Clone, Debug)]
pub struct Deltas<'a> {
  /// The text the windows read from.
  source: &'a [u8],
  /// The text they make.
  target: &'a [u8],
  /// The most bytes of each a window takes.
  room: usize,
  /// The source bytes the windows so far have viewed.
  source_read: usize,
  /// Whether the last source piece was whole, so the next window views more of the source (`more_source`).
  more_source: bool,
  /// The target bytes the windows so far have made.
  target_read: usize,
}

impl Iterator for Deltas<'_> {
  type Item = Window;

  /// The next window (`txdelta_next_window`), or none once the target is made.
  fn next(&mut self) -> Option<Window> {
    let source_piece = if self.more_source {
      let piece = piece(self.source, self.source_read, self.room);
      self.more_source = piece.len() == self.room;
      piece
    } else {
      &[]
    };
    self.source_read = self.source_read.saturating_add(source_piece.len());
    let target_piece = piece(self.target, self.target_read, self.room);
    if target_piece.is_empty() {
      return None;
    }
    self.target_read = self.target_read.saturating_add(target_piece.len());
    let source_offset = self.source_read.saturating_sub(source_piece.len());
    Some(window(source_piece, target_piece, source_offset))
  }
}

/// The `room` bytes of `text` from `start`, fewer at its end.
fn piece(text: &[u8], start: usize, room: usize) -> &[u8] {
  let rest = text.get(start..).unwrap_or_default();
  rest.get(..room).unwrap_or(rest)
}

/// One window of `target` from `source`, the source's piece starting at `source_offset` (`compute_window`).
fn window(source: &[u8], target: &[u8], source_offset: usize) -> Window {
  let mut ops = Ops::default();
  if source.is_empty() || target.len() < usize::MAX {
    ops.new_data(target);
  } else {
    compute_delta(&mut ops, source, target);
  }
  Window {
    source_offset: to_u64(source_offset),
    source_len: to_u64(source.len()),
    target_len: to_u64(target.len()),
    instructions: ops.instructions,
    new_data: ops.new_data,
  }
}

/// A count held in memory as the format's numbers count it: exact wherever `usize` is at most 64 bits, as on every
/// target slates builds for.
fn to_u64(count: usize) -> u64 {
  u64::try_from(count).unwrap_or(u64::MAX)
}

/// A window's instructions and new data as they are chosen (`svn_txdelta__ops_baton_t`).
#[derive(Default)]
struct Ops {
  /// The instructions.
  instructions: Vec<Instruction>,
  /// The bytes the new-data instructions copy.
  new_data: Vec<u8>,
}

impl Ops {
  /// New data, merged into a new-data instruction that ends the window so far (`svn_txdelta__insert_op`).
  fn new_data(&mut self, bytes: &[u8]) {
    if bytes.is_empty() {
      return;
    }
    self.new_data.extend_from_slice(bytes);
    let length = to_u64(bytes.len());
    if let Some(Instruction::New { length: last }) = self.instructions.last_mut() {
      *last = last.saturating_add(length);
      return;
    }
    self.instructions.push(Instruction::New { length });
  }

  /// A copy of `length` source bytes from `offset`, merged into a source copy that ends where it starts
  /// (`svn_txdelta__insert_op`).
  fn source_copy(&mut self, offset: usize, length: usize) {
    let (offset, length) = (to_u64(offset), to_u64(length));
    if let Some(Instruction::Source {
      offset: last_offset,
      length: last_length,
    }) = self.instructions.last_mut()
      && last_offset.saturating_add(*last_length) == offset
    {
      *last_length = last_length.saturating_add(length);
      return;
    }
    self.instructions.push(Instruction::Source { offset, length });
  }

  /// Takes back up to `most` bytes the last instructions made, back to front, so a match can reach over them
  /// (`svn_txdelta__remove_copy`): new data is cut short, a copy is taken back only whole, and nothing before a target
  /// copy is touched. Answers the bytes taken back.
  fn take_back(&mut self, most: usize) -> usize {
    let mut taken: usize = 0;
    while let Some(&last) = self.instructions.last() {
      let length = match last {
        Instruction::Target { .. } => break,
        Instruction::Source { length, .. } | Instruction::New { length } => {
          usize::try_from(length).unwrap_or(usize::MAX)
        }
      };
      if length.saturating_add(taken) > most {
        if let Instruction::New { .. } = last {
          let cut = most.saturating_sub(taken);
          self.cut_new_data(cut);
          taken = most;
        }
        break;
      }
      if let Instruction::New { .. } = last {
        self.cut_new_data(length);
      }
      self.instructions.pop();
      taken = taken.saturating_add(length);
    }
    taken
  }

  /// Takes the last `count` bytes off the last instruction, which is new data, and off the new data.
  fn cut_new_data(&mut self, count: usize) {
    self
      .new_data
      .truncate(self.new_data.len().saturating_sub(count));
    if let Some(Instruction::New { length }) = self.instructions.last_mut() {
      *length = length.saturating_sub(to_u64(count));
    }
  }
}

/// The pseudo-Adler-32 sum of a block (`init_adler32`): the byte sum below, the sum of the running sums above, each in
/// 32-bit arithmetic.
fn block_sum(block: &[u8]) -> u32 {
  let (first, second) = block.iter().fold((0u32, 0u32), |(first, second), &byte| {
    let first = first.wrapping_add(u32::from(byte));
    (first, second.wrapping_add(first))
  });
  second.wrapping_mul(UPPER_HALF).wrapping_add(first)
}

/// The rolling sum moved one byte on, `out` leaving and `entering` joining (`adler32_replace`).
fn roll(sum: u32, out: u8, entering: u8) -> u32 {
  let out = u32::from(out);
  let sum = sum
    .wrapping_sub(BLOCK_FACTOR.wrapping_mul(UPPER_HALF).wrapping_mul(out))
    .wrapping_sub(out)
    .wrapping_add(u32::from(entering));
  sum.wrapping_add(sum.wrapping_mul(UPPER_HALF))
}

/// The table slot a sum starts its probe at (`hash_func`).
fn hash(sum: u32) -> u32 {
  sum ^ sum.wrapping_shr(HASH_FOLD)
}

/// The byte of the flags table a sum names (`hash_flags`) and its bit.
fn flag(sum: u32) -> (usize, u8) {
  let byte = usize::try_from(sum.wrapping_shr(FLAG_SHIFT)).unwrap_or(0) & FLAG_BYTES.saturating_sub(1);
  (byte, 1u8.wrapping_shl(sum & FLAG_BITS))
}

/// An empty slot of the blocks table.
/// Format: `xdelta.c` `NO_POSITION`, `(apr_uint32_t)-1`.
const NO_POSITION: u32 = u32::MAX;

/// The source's whole blocks, indexed by their sums (`struct blocks`, `init_blocks_table`).
struct Blocks<'a> {
  /// The source piece.
  data: &'a [u8],
  /// Each slot's sum and the block's position, `NO_POSITION` where empty.
  slots: Vec<(u32, u32)>,
  /// The largest slot index, the table's size less one.
  mask: u32,
  /// A bit per sum class that says it may have a block.
  flags: Vec<u8>,
}

impl<'a> Blocks<'a> {
  /// The table of `data`'s whole blocks: twice the next power of two above the block count of slots, each block put in
  /// by its sum unless an equal block holds it already.
  fn new(data: &'a [u8]) -> Self {
    let blocks = data.len().checked_div(BLOCK).unwrap_or(0).saturating_add(1);
    let mut slots: usize = 1;
    while slots <= blocks {
      slots = slots.saturating_mul(2);
    }
    slots = slots.saturating_mul(2);
    let mut table = Self {
      data,
      slots: vec![(0, NO_POSITION); slots],
      mask: u32::try_from(slots.saturating_sub(1)).unwrap_or(u32::MAX),
      flags: vec![0; FLAG_BYTES],
    };
    let mut position = 0usize;
    while let Some(block) = data.get(position..position.saturating_add(BLOCK)) {
      if block.len() < BLOCK {
        break;
      }
      table.add(block_sum(block), position);
      position = position.saturating_add(BLOCK);
    }
    table
  }

  /// The block at `position` (`add_block`).
  fn add(&mut self, sum: u32, position: usize) {
    let Ok(position) = u32::try_from(position) else {
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
      if held_sum == sum && self.block_at(held_position) == self.block_at(position) {
        return;
      }
      slot = slot.wrapping_add(1) & self.mask;
    }
    if let Some(entry) = self.slots.get_mut(slot as usize) {
      *entry = (sum, position);
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

  /// The position of a block equal to `bytes` with `sum`, if the table holds one (`find_block`).
  fn find(&self, sum: u32, bytes: &[u8]) -> Option<usize> {
    let mut slot = hash(sum) & self.mask;
    loop {
      let &(held_sum, held_position) = self.slots.get(slot as usize)?;
      if held_position == NO_POSITION {
        return None;
      }
      if held_sum == sum && self.block_at(held_position) == Some(bytes) {
        return usize::try_from(held_position).ok();
      }
      slot = slot.wrapping_add(1) & self.mask;
    }
  }

  /// The block starting at `position`.
  fn block_at(&self, position: u32) -> Option<&[u8]> {
    let start = usize::try_from(position).ok()?;
    self.data.get(start..start.checked_add(BLOCK)?)
  }
}

/// How many bytes `a` and `b` share from their starts, at most `most` (`svn_cstring__match_length`).
fn common_start(a: &[u8], b: &[u8], most: usize) -> usize {
  a.iter()
    .zip(b)
    .take(most)
    .take_while(|(left, right)| left == right)
    .count()
}

/// How many bytes `a` and `b` share at their ends, at most `most` (`svn_cstring__reverse_match_length`).
fn common_end(a: &[u8], b: &[u8], most: usize) -> usize {
  a.iter()
    .rev()
    .zip(b.iter().rev())
    .take(most)
    .take_while(|(left, right)| left == right)
    .count()
}

/// The match found at a target position: where it starts in the source and the target, and its length.
struct Match {
  /// Its start in the source.
  source: usize,
  /// Its start in the target.
  target: usize,
  /// Its length.
  length: usize,
}

/// The match of the target's block at `at`, if the source holds one, extended forward as far as both run, and back as
/// far as both run past no byte before `floor` (`find_match`).
fn find_match(blocks: &Blocks<'_>, sum: u32, source: &[u8], target: &[u8], at: usize, floor: usize) -> Option<Match> {
  let block = target.get(at..at.checked_add(BLOCK)?)?;
  let mut from = blocks.find(sum, block)?;
  let mut to = at;
  let forward_most = source
    .len()
    .saturating_sub(from)
    .saturating_sub(BLOCK)
    .min(target.len().saturating_sub(to).saturating_sub(BLOCK));
  let mut length = BLOCK.saturating_add(common_start(
    source.get(from.saturating_add(BLOCK)..).unwrap_or_default(),
    target.get(to.saturating_add(BLOCK)..).unwrap_or_default(),
    forward_most,
  ));
  while from > 0 && to > floor {
    let (Some(before_source), Some(before_target)) = (
      source.get(from.saturating_sub(1)),
      target.get(to.saturating_sub(1)),
    ) else {
      break;
    };
    if before_source != before_target {
      break;
    }
    from = from.saturating_sub(1);
    to = to.saturating_sub(1);
    length = length.saturating_add(1);
  }
  Some(Match {
    source: from,
    target: to,
    length,
  })
}

/// The target's bytes from `start` as new data, but for a common end with the source longer than four bytes, which is
/// a copy of the source's end (`store_delta_trailer`).
fn trailer(ops: &mut Ops, source: &[u8], target: &[u8], start: usize) {
  let rest = target.get(start..).unwrap_or_default();
  let most = source.len().min(rest.len());
  if most == 0 {
    return;
  }
  let mut end = common_end(source, target, most);
  if end <= SHORTEST_KEPT {
    end = 0;
  }
  ops.new_data(rest.get(..rest.len().saturating_sub(end)).unwrap_or_default());
  if end > 0 {
    ops.source_copy(source.len().saturating_sub(end), end);
  }
}

/// The instructions that make `target` from `source` (`compute_delta`).
fn compute_delta(ops: &mut Ops, source: &[u8], target: &[u8]) {
  let mut at = common_start(source, target, source.len().min(target.len()));
  let mut pending: usize = 0;
  if at > SHORTEST_KEPT || at == target.len() {
    ops.source_copy(0, at);
    pending = at;
  } else {
    at = 0;
  }
  if target.len().saturating_sub(at) < BLOCK || source.len() < BLOCK {
    trailer(ops, source, target, at);
    return;
  }
  let upper = target.len().saturating_sub(BLOCK);
  let blocks = Blocks::new(source);
  let mut sum = block_at(target, at).map_or(0, block_sum);
  while at < upper {
    (sum, at) = skip_unmatched(&blocks, target, sum, at, upper);
    match find_match(&blocks, sum, source, target, at, pending) {
      None => {
        if at.saturating_add(BLOCK) < target.len() {
          sum = rolled(target, sum, at);
        }
        at = at.saturating_add(1);
      }
      Some(found) => {
        at = take_match(ops, source, target, found, pending);
        pending = at;
        if let Some(block) = block_at(target, at) {
          sum = block_sum(block);
        }
      }
    }
  }
  trailer(ops, source, target, pending);
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

/// Moves past the positions whose sums the flags say have no block, up to `upper` (`compute_delta`'s quick skip).
fn skip_unmatched(blocks: &Blocks<'_>, target: &[u8], sum: u32, at: usize, upper: usize) -> (u32, usize) {
  let (mut sum, mut at) = (sum, at);
  while !blocks.may_hold(sum) && at < upper {
    sum = rolled(target, sum, at);
    at = at.saturating_add(1);
  }
  (sum, at)
}

/// Writes a match: the new data pending before it, or, when it meets the last instruction, as much of that taken back
/// as the match reaches over; then its source copy. Answers where the target stands after it.
fn take_match(ops: &mut Ops, source: &[u8], target: &[u8], found: Match, pending: usize) -> usize {
  let Match {
    source: mut from,
    target: mut to,
    mut length,
  } = found;
  if to > pending {
    ops.new_data(target.get(pending..to).unwrap_or_default());
  } else {
    let reach = common_end(
      source.get(..from).unwrap_or_default(),
      target.get(..to).unwrap_or_default(),
      from.min(to),
    );
    if reach > 0 {
      let taken = ops.take_back(reach);
      from = from.saturating_sub(taken);
      length = length.saturating_add(taken);
      to = to.saturating_sub(taken);
    }
  }
  ops.source_copy(from, length);
  to.saturating_add(length)
}

#[cfg(test)]
mod tests;

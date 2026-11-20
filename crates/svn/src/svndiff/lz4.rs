//! An LZ4 block made a part at a time (A-751): the block `lz4_flex::block::compress_into` makes of a section, byte for
//! byte, its work cut into units a caller takes a few at a time, so a 100 KiB section never holds a shard past its step
//! quantum. svndiff version 2 stores each window section as an LZ4 block when that is shorter (`compress_lz4.c`), and
//! the window maker (`super::deltify::making`) makes its sections with this.
//!
//! **The port** is `lz4_flex` 0.11.6's `compress_internal` as the workspace builds it (`safe-encode`, no dictionary,
//! the whole input from position 0): the 4,096-entry table of 16-bit positions for an input shorter than 65,535 bytes,
//! hashed from four bytes (`HashTable4KU16`), else of 32-bit positions hashed from eight (`HashTable4K`, `hash5`); a
//! search that steps further after each 32 misses; a match taken back over equal bytes before it, extended forward to
//! the last literals, and its end hashed in; each sequence's token, lengths, literals and offset as `lz4_flex` writes
//! them; the last literals at the end. The tests hold it to `lz4_flex` over every unit size.
//!
//! No I/O, no clock, no randomness, no panic.

#[cfg(test)]
mod tests;

/// The table's entries.
/// Format: `lz4_flex` `HASHTABLE_SIZE_4K`.
const TABLE: usize = 4096;
/// How far a hash is shifted down to its entry.
/// Format: `lz4_flex` `HASHTABLE_BIT_SHIFT_4K`.
const TABLE_SHIFT: usize = 4;
/// The shortest input whose positions fit the 16-bit table: inputs shorter are hashed from four bytes.
/// Format: `lz4_flex` `compress_into_sink_with_dict` (`input.len() < u16::MAX`).
const SHORT_INPUT: usize = 65_535;
/// The farthest a match may lie behind.
/// Format: LZ4's `MAX_DISTANCE`, `(1 << 16) - 1`.
const MAX_DISTANCE: usize = 65_535;
/// The shortest match.
/// Format: LZ4's `MINMATCH`.
const MIN_MATCH: usize = 4;
/// The bytes at the end no match may start in.
/// Format: LZ4's `MFLIMIT`.
const MF_LIMIT: usize = 12;
/// The shortest input that is not all literals.
/// Format: `lz4_flex` `LZ4_MIN_LENGTH`, `MFLIMIT + 1`.
const MIN_LENGTH: usize = MF_LIMIT + 1;
/// The bytes at the end that are always literals, and one more: no match runs into them.
/// Format: `lz4_flex` `END_OFFSET`, `LAST_LITERALS + 1`.
const END_OFFSET: usize = 6;
/// The misses after which the search steps one byte further.
/// Format: `lz4_flex` `INCREASE_STEPSIZE_BITSHIFT`, 5.
const STEP_SHIFT: u32 = 5;
/// A length that does not fit a token's nibble, written on in bytes.
/// Format: LZ4's token, four bits a length.
const NIBBLE: usize = 0xF;
/// One byte of a length written on: the most it adds before the next.
/// Format: LZ4's length bytes.
const LENGTH_BYTE: usize = 0xFF;
/// How far a token's literal length is shifted.
/// Format: LZ4's token, the literal length in the high nibble.
const LITERAL_SHIFT: u32 = 4;
/// The multiplier of the four-byte hash.
/// Format: `lz4_flex` `hash`, Knuth's 2654435761.
const HASH4_PRIME: u32 = 2_654_435_761;
/// How far the four-byte hash's product is shifted down.
/// Format: `lz4_flex` `hash`.
const HASH4_SHIFT: u32 = 16;
/// The multiplier of the eight-byte hash on a little-endian machine.
/// Format: `lz4_flex` `hash5`.
const HASH5_PRIME: u64 = 889_523_592_379;
/// How far the eight-byte sequence is shifted up before its product.
/// Format: `lz4_flex` `hash5`, `sequence << 24`.
const HASH5_UP: u32 = 24;
/// How far the eight-byte hash's product is shifted down.
/// Format: `lz4_flex` `hash5`, `>> 48`.
const HASH5_SHIFT: u32 = 48;

/// Where the block's making stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
  /// Looking for the next match from `cur`, the search's own counters kept.
  Search {
    /// The misses since the search began, which set its step.
    misses: usize,
    /// Where it looks next.
    next: usize,
  },
  /// Taking a found match back over equal bytes before it.
  Back {
    /// The match's start in the input.
    cur: usize,
    /// Its earlier copy's start.
    candidate: usize,
    /// How far behind the copy lies.
    offset: u16,
  },
  /// Extending a match forward.
  Forward {
    /// The literals' end, the match's start.
    start: usize,
    /// How far behind the copy lies.
    offset: u16,
    /// The next byte compared in the input.
    cur: usize,
    /// The next byte compared in the copy.
    candidate: usize,
  },
  /// Writing a sequence's literals, its token and length already written.
  Literals {
    /// The next literal to write.
    from: usize,
    /// The literals' end.
    to: usize,
    /// What follows them: the offset, the match's length past its nibble, and where the next literals start.
    then: (u16, usize, usize),
  },
  /// Writing the last literals, their token and length already written.
  Last {
    /// The next literal to write.
    from: usize,
  },
  /// The block is whole.
  Done,
}

/// One LZ4 block in the making (module documentation).
#[derive(Clone, Debug)]
pub(crate) struct Compressing {
  /// The table of positions by hash.
  table: Vec<u32>,
  /// Whether the input's positions are hashed from four bytes (an input shorter than [`SHORT_INPUT`]).
  short: bool,
  /// Where the literals not yet written start.
  literal_start: usize,
  /// Where the making stands.
  phase: Phase,
  /// The block so far.
  out: Vec<u8>,
  /// The most bytes the block is worth making: once past, it is given up.
  most: usize,
}

impl Compressing {
  /// The block of an input of `len` bytes, given up once it reaches `most` bytes (a section stores its LZ4 block only
  /// when that is shorter than the section).
  pub(crate) fn new(len: usize, most: usize) -> Self {
    let phase = if len < MIN_LENGTH {
      Phase::Last { from: 0 }
    } else {
      Phase::Search {
        misses: 1 << STEP_SHIFT,
        next: 1,
      }
    };
    let mut made = Self {
      table: vec![0; TABLE],
      short: len < SHORT_INPUT,
      literal_start: 0,
      phase,
      out: Vec::new(),
      most,
    };
    if let Phase::Last { .. } = made.phase {
      made.last_token(len);
    }
    made
  }

  /// The block, once whole and shorter than the most it was worth; none before, or when it was given up.
  pub(crate) fn block(&self) -> Option<&[u8]> {
    (self.phase == Phase::Done && self.out.len() < self.most).then_some(self.out.as_slice())
  }

  /// Whether the making is over: the block whole, or given up.
  pub(crate) fn over(&self) -> bool {
    self.phase == Phase::Done
  }

  /// Takes up to `work` units of the making of `input`'s block (the same bytes each call); at least one. Answers the
  /// units taken.
  pub(crate) fn step(&mut self, input: &[u8], work: usize) -> usize {
    // `lz4_flex` hashes position 0 in before its search; holding 0 where every entry is 0 already, that changes nothing.
    let mut left = work.max(1);
    while left > 0 && self.phase != Phase::Done {
      if self.out.len() >= self.most {
        self.phase = Phase::Done;
        break;
      }
      left = left.saturating_sub(self.unit(input, left));
    }
    work.max(1).saturating_sub(left)
  }

  /// One piece of the making, within `left` units: the units it took, at least one.
  fn unit(&mut self, input: &[u8], left: usize) -> usize {
    match self.phase {
      Phase::Search { misses, next } => self.search(input, misses, next),
      Phase::Back {
        cur,
        candidate,
        offset,
      } => self.back(input, cur, candidate, offset, left),
      Phase::Forward {
        start,
        offset,
        cur,
        candidate,
      } => self.forward(input, (start, offset), cur, candidate, left),
      Phase::Literals { from, to, then } => self.literals(input, from, to, then, left),
      Phase::Last { from } => self.last(input, from, left),
      Phase::Done => 1,
    }
  }

  /// One probe of the search (`compress_internal`'s inner loop).
  fn search(&mut self, input: &[u8], misses: usize, next: usize) -> usize {
    let step = misses >> STEP_SHIFT;
    let cur = next;
    let next = next.saturating_add(step);
    let end_check = input.len().saturating_sub(MF_LIMIT);
    if cur > end_check {
      self.last_token(input.len());
      return 1;
    }
    let hash = self.hash_at(input, cur);
    let candidate = self.get(hash);
    self.put(hash, cur);
    let misses = misses.saturating_add(1);
    self.phase = Phase::Search { misses, next };
    if cur.saturating_sub(candidate) > MAX_DISTANCE {
      return 1;
    }
    if batch(input, candidate) == batch(input, cur) {
      let offset = u16::try_from(cur.saturating_sub(candidate)).unwrap_or(u16::MAX);
      self.phase = Phase::Back {
        cur,
        candidate,
        offset,
      };
    }
    1
  }

  /// The match taken back over equal bytes before it, past no literal already written (`backtrack_match`).
  fn back(
    &mut self,
    input: &[u8],
    cur: usize,
    candidate: usize,
    offset: u16,
    left: usize,
  ) -> usize {
    let (mut cur, mut candidate) = (cur, candidate);
    let mut taken = 0usize;
    while taken < left && candidate > 0 && cur > self.literal_start {
      let (Some(a), Some(b)) = (
        input.get(cur.saturating_sub(1)),
        input.get(candidate.saturating_sub(1)),
      ) else {
        break;
      };
      if a != b {
        break;
      }
      cur = cur.saturating_sub(1);
      candidate = candidate.saturating_sub(1);
      taken = taken.saturating_add(1);
    }
    let more = taken == left
      && candidate > 0
      && cur > self.literal_start
      && input.get(cur.saturating_sub(1)) == input.get(candidate.saturating_sub(1));
    self.phase = if more {
      Phase::Back {
        cur,
        candidate,
        offset,
      }
    } else {
      Phase::Forward {
        start: cur,
        offset,
        cur: cur.saturating_add(MIN_MATCH),
        candidate: candidate.saturating_add(MIN_MATCH),
      }
    };
    taken.max(1)
  }

  /// The match extended forward, short of the last literals (`count_same_bytes`); once it ends, its sequence's token
  /// and literal length written and its end hashed in.
  fn forward(
    &mut self,
    input: &[u8],
    (start, offset): (usize, u16),
    cur: usize,
    candidate: usize,
    left: usize,
  ) -> usize {
    let end = input.len().saturating_sub(END_OFFSET);
    let ahead = input.get(cur..end).unwrap_or_default();
    let copy = input.get(candidate..).unwrap_or_default();
    let most = ahead.len().min(copy.len());
    let span = most.min(left);
    let same = ahead
      .iter()
      .zip(copy)
      .take(span)
      .take_while(|(a, b)| a == b)
      .count();
    let (cur, candidate) = (cur.saturating_add(same), candidate.saturating_add(same));
    if same == span && span < most {
      self.phase = Phase::Forward {
        start,
        offset,
        cur,
        candidate,
      };
      return same.max(1);
    }
    let literals = start.saturating_sub(self.literal_start);
    let duplicate = cur.saturating_sub(start).saturating_sub(MIN_MATCH);
    let hash = self.hash_at(input, cur.saturating_sub(2));
    self.put(hash, cur.saturating_sub(2));
    self.out.push(token(literals, duplicate));
    if literals >= NIBBLE {
      self.length(literals.saturating_sub(NIBBLE));
    }
    self.phase = Phase::Literals {
      from: self.literal_start,
      to: start,
      then: (offset, duplicate, cur),
    };
    same.max(1)
  }

  /// A sequence's literals, a unit a byte; once written, its offset and match length, and the next search begun.
  fn literals(
    &mut self,
    input: &[u8],
    from: usize,
    to: usize,
    (offset, duplicate, next): (u16, usize, usize),
    left: usize,
  ) -> usize {
    let end = to.min(from.saturating_add(left));
    self
      .out
      .extend_from_slice(input.get(from..end).unwrap_or_default());
    let written = end.saturating_sub(from);
    if end < to {
      self.phase = Phase::Literals {
        from: end,
        to,
        then: (offset, duplicate, next),
      };
      return written.max(1);
    }
    self.out.extend_from_slice(&offset.to_le_bytes());
    if duplicate >= NIBBLE {
      self.length(duplicate.saturating_sub(NIBBLE));
    }
    self.literal_start = next;
    self.phase = Phase::Search {
      misses: 1 << STEP_SHIFT,
      next,
    };
    written.max(1)
  }

  /// The last literals' token and length (`handle_last_literals`), their bytes next.
  fn last_token(&mut self, len: usize) {
    let literals = len.saturating_sub(self.literal_start);
    self.out.push(token(literals, 0) & 0xF0);
    if literals >= NIBBLE {
      self.length(literals.saturating_sub(NIBBLE));
    }
    self.phase = Phase::Last {
      from: self.literal_start,
    };
  }

  /// The last literals, a unit a byte; the block whole once they are written.
  fn last(&mut self, input: &[u8], from: usize, left: usize) -> usize {
    let end = input.len().min(from.saturating_add(left));
    self
      .out
      .extend_from_slice(input.get(from..end).unwrap_or_default());
    self.phase = if end < input.len() {
      Phase::Last { from: end }
    } else {
      Phase::Done
    };
    end.saturating_sub(from).max(1)
  }

  /// A length past its nibble, in bytes of 255 and the rest (`write_integer`).
  fn length(&mut self, length: usize) {
    let mut rest = length;
    while rest >= LENGTH_BYTE {
      rest = rest.saturating_sub(LENGTH_BYTE);
      self.out.push(u8::MAX);
    }
    self.out.push(u8::try_from(rest).unwrap_or(u8::MAX));
  }

  /// The hash of the input at `at`, as its table hashes it.
  fn hash_at(&self, input: &[u8], at: usize) -> usize {
    if self.short {
      let product = batch(input, at).wrapping_mul(HASH4_PRIME);
      usize::try_from(product >> HASH4_SHIFT).unwrap_or(0)
    } else {
      hash5(input, at)
    }
  }

  /// The position held for `hash`, 0 for none.
  fn get(&self, hash: usize) -> usize {
    let held = self.table.get(hash >> TABLE_SHIFT).copied().unwrap_or(0);
    usize::try_from(held).unwrap_or(0)
  }

  /// Holds `position` for `hash`, as the table's width holds it.
  fn put(&mut self, hash: usize, position: usize) {
    let short = self.short;
    if let Some(slot) = self.table.get_mut(hash >> TABLE_SHIFT) {
      *slot = if short {
        u32::from(u16::try_from(position & usize::from(u16::MAX)).unwrap_or(u16::MAX))
      } else {
        u32::try_from(position).unwrap_or(u32::MAX)
      };
    }
  }
}

/// A sequence's token: the literal length's nibble above the match length's, each 15 when it is written on.
fn token(literals: usize, duplicate: usize) -> u8 {
  let high = u8::try_from(literals.min(NIBBLE)).unwrap_or(0xF) << LITERAL_SHIFT;
  high | u8::try_from(duplicate.min(NIBBLE)).unwrap_or(0xF)
}

/// The four bytes at `at`, native-endian (`get_batch`); zero past the end, where no caller reads.
fn batch(input: &[u8], at: usize) -> u32 {
  input
    .get(at..at.saturating_add(4))
    .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
    .map_or(0, u32::from_ne_bytes)
}

/// The eight-byte hash at `at` (`hash5` over `get_batch_arch`), on a 64-bit little-endian machine.
#[cfg(all(target_pointer_width = "64", target_endian = "little"))]
fn hash5(input: &[u8], at: usize) -> usize {
  let sequence = input
    .get(at..at.saturating_add(8))
    .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
    .map_or(0, u64::from_ne_bytes);
  let product = (sequence << HASH5_UP).wrapping_mul(HASH5_PRIME) >> HASH5_SHIFT;
  usize::try_from(product).unwrap_or(0)
}

/// The eight-byte table's hash on a 32-bit machine: `lz4_flex` hashes four bytes there.
#[cfg(not(all(target_pointer_width = "64", target_endian = "little")))]
fn hash5(input: &[u8], at: usize) -> usize {
  let product = batch(input, at).wrapping_mul(HASH4_PRIME);
  usize::try_from(product >> HASH4_SHIFT).unwrap_or(0)
}

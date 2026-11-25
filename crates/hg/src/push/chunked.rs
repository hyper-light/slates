//! A push's segment payload written as stock Mercurial 7.2's revlog chunks, once, as it lands (goal 93, main's ruling (b)
//! of 2026-11-25: content kept once, on arrival, in the form a stream sends it, so a stream is a copy and a fetch reads
//! a chunk where `hg serve` reads one).
//!
//! **What it writes.** For each revision a segment kept, in order, its delta (or whole) as the changegroup carried it,
//! made into the chunk 7.2's `revlog.compress` would store ([`crate::revlog_chunk`]'s rule: empty as it is; 50 bytes or
//! more one zstd frame at level 3 naming its size, where that is shorter; else as it is where it begins with a NUL; else
//! `u` and the bytes), laid one after another: the segment's payload, each revision's place its chunk's range in it.
//!
//! **Bounded work** (main's ruling (b), its condition (1)). A step compresses at most the bytes its caller gives it
//! (the shard's copy slice, `copy_slice_bytes`), however long one delta is: a frame is made through zstd's streaming
//! context over as many steps as its bytes need, the frame's content size pledged first, so each frame names its size as
//! the one-shot frame does. Counted: the most plain bytes one step compressed ([`Chunked::most_step`]).
//!
//! **Its own context.** The frame of one delta may span steps, between which a stream on the same shard compresses
//! through the thread's one context (`revlog_chunk`'s): so a segment's chunking holds a context of its own, made at its
//! first frame and let go with it.

use core::ops::Range;

use zstd::zstd_safe::{self, CCtx, CParameter, InBuffer, OutBuffer, ResetDirective};

use crate::push::intake::KeptText;
use crate::read::to_u64;
use crate::revlog_chunk::{ChunkRefused, SHORTEST_COMPRESSED, ZSTD_LEVEL};

/// Format: the header of a chunk stored as it is (`revlog.compress`'s `u`).
const UNCOMPRESSED: u8 = b'u';

/// A segment's payload being made into chunks, a step at a time (module docs).
pub struct Chunked {
  /// The revision being chunked.
  at: usize,
  /// The bytes of its delta fed to its frame so far.
  fed: usize,
  /// Its frame so far.
  frame: Vec<u8>,
  /// The context its frames are made through, made at the first.
  context: Option<CCtx<'static>>,
  /// The payload so far.
  payload: Vec<u8>,
  /// Each revision's chunk's range in it, in order.
  places: Vec<Range<u64>>,
  /// The most plain bytes one step compressed.
  most_step: u64,
  /// The plain bytes laid so far.
  plain: u64,
  /// The most bytes one step compressed past the budget it was given (none: a step's first unit is all a step must
  /// take, and a frame's piece is held to the budget).
  most_past: u64,
}

impl core::fmt::Debug for Chunked {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.debug_struct("Chunked")
      .field("at", &self.at)
      .field("fed", &self.fed)
      .field("payload", &self.payload.len())
      .finish_non_exhaustive()
  }
}

impl Chunked {
  /// The chunking of a segment of `count` revisions.
  ///
  /// # Errors
  /// [`ChunkRefused::NoState`] where its places cannot be held.
  pub fn begin(count: usize) -> Result<Self, ChunkRefused> {
    let mut places = Vec::new();
    places
      .try_reserve_exact(count)
      .map_err(|_| ChunkRefused::NoState)?;
    Ok(Chunked {
      at: 0,
      fed: 0,
      frame: Vec::new(),
      context: None,
      payload: Vec::new(),
      places,
      most_step: 0,
      plain: 0,
      most_past: 0,
    })
  }

  /// One step: at most `budget` bytes (one at least) of `texts`' deltas, read at their places in `slice` (the segment's
  /// changegroup bytes), made into chunks. Whether more follow.
  ///
  /// # Errors
  /// [`ChunkRefused`] where a place is not in `slice`, or zstd refused.
  pub fn step(
    &mut self,
    (texts, slice): (&[KeptText], &[u8]),
    budget: u64,
  ) -> Result<bool, ChunkRefused> {
    let mut left = usize::try_from(budget).unwrap_or(usize::MAX).max(1);
    let mut stepped = 0usize;
    while left > 0 {
      let Some(kept) = texts.get(self.at) else {
        break;
      };
      let plain = place_of(kept, slice)?;
      if plain.len() < SHORTEST_COMPRESSED {
        // Laid whole or left for the next step: a step never takes more than it is given, past its first unit.
        if plain.len() > left && stepped > 0 {
          break;
        }
        self.lay(plain, None);
        left = left.saturating_sub(plain.len().max(1));
        stepped = stepped.saturating_add(plain.len());
        continue;
      }
      let take = left.min(plain.len().saturating_sub(self.fed));
      self.feed(plain, take)?;
      left = left.saturating_sub(take.max(1));
      stepped = stepped.saturating_add(take);
    }
    self.most_step = self.most_step.max(to_u64(stepped));
    self.most_past = self.most_past.max(to_u64(stepped).saturating_sub(budget));
    Ok(self.at < texts.len())
  }

  /// The payload and each revision's chunk's range in it, the context let go.
  pub fn finish(self) -> (Vec<u8>, Vec<Range<u64>>) {
    (self.payload, self.places)
  }

  /// The most plain bytes one step compressed.
  pub fn most_step(&self) -> u64 {
    self.most_step
  }

  /// The most bytes one step compressed past the budget it was given: zero but where a step's first unit is a short text
  /// longer than its budget.
  pub fn most_past(&self) -> u64 {
    self.most_past
  }

  /// The plain bytes compressed so far and the bytes their chunks took.
  pub fn bytes(&self) -> (u64, u64) {
    (self.plain, to_u64(self.payload.len()))
  }

  /// `take` more bytes of `plain`'s frame made: begun at its first (its size pledged), ended at its last, and then laid
  /// as its chunk (the frame where shorter than `plain`, else `plain` as it is).
  fn feed(&mut self, plain: &[u8], take: usize) -> Result<(), ChunkRefused> {
    let engine = |code| ChunkRefused::Engine(zstd_safe::get_error_name(code));
    if self.context.is_none() {
      let mut made = CCtx::try_create().ok_or(ChunkRefused::NoState)?;
      made
        .set_parameter(CParameter::CompressionLevel(ZSTD_LEVEL))
        .map_err(engine)?;
      self.context = Some(made);
    }
    let context = self.context.as_mut().ok_or(ChunkRefused::NoState)?;
    if self.fed == 0 {
      context.reset(ResetDirective::SessionOnly).map_err(engine)?;
      context
        .set_pledged_src_size(Some(to_u64(plain.len())))
        .map_err(engine)?;
      self.frame.clear();
      self
        .frame
        .try_reserve(zstd_safe::compress_bound(plain.len()))
        .map_err(|_| ChunkRefused::NoState)?;
    }
    let end = self.fed.saturating_add(take);
    let piece = plain.get(self.fed..end).ok_or(ChunkRefused::NoState)?;
    let last = end == plain.len();
    let directive = if last {
      zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_end
    } else {
      zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_continue
    };
    let mut input = InBuffer::around(piece);
    loop {
      let at = self.frame.len();
      let mut output = OutBuffer::around_pos(&mut self.frame, at);
      let remaining = context
        .compress_stream2(&mut output, &mut input, directive)
        .map_err(engine)?;
      let consumed = input.pos() == piece.len();
      if consumed && (!last || remaining == 0) {
        break;
      }
      if self.frame.len() == self.frame.capacity() {
        self
          .frame
          .try_reserve(zstd_safe::compress_bound(plain.len()).max(1))
          .map_err(|_| ChunkRefused::NoState)?;
      }
    }
    self.fed = end;
    if last {
      let frame = core::mem::take(&mut self.frame);
      let shorter = frame.len() < plain.len();
      self.lay(plain, shorter.then_some(frame.as_slice()));
      self.frame = frame;
    }
    Ok(())
  }

  /// Lays `plain`'s chunk at the payload's end: `frame` where given, else `plain` as 7.2 stores a text it does not
  /// compress (empty as it is; as it is where it begins with a NUL; else `u` and it); the revision done.
  fn lay(&mut self, plain: &[u8], frame: Option<&[u8]>) {
    let from = to_u64(self.payload.len());
    match frame {
      Some(frame) => self.payload.extend_from_slice(frame),
      None if plain.is_empty() || plain.first() == Some(&0) => {
        self.payload.extend_from_slice(plain)
      }
      None => {
        self.payload.push(UNCOMPRESSED);
        self.payload.extend_from_slice(plain);
      }
    }
    self.places.push(from..to_u64(self.payload.len()));
    self.plain = self.plain.saturating_add(to_u64(plain.len()));
    self.at = self.at.saturating_add(1);
    self.fed = 0;
  }
}

/// `kept`'s delta in `slice`.
fn place_of<'s>(kept: &KeptText, slice: &'s [u8]) -> Result<&'s [u8], ChunkRefused> {
  let start = usize::try_from(kept.place.start).map_err(|_| ChunkRefused::NoState)?;
  let end = usize::try_from(kept.place.end).map_err(|_| ChunkRefused::NoState)?;
  slice.get(start..end).ok_or(ChunkRefused::NoState)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Kept texts whose places are `deltas` laid one after another in a slice: the texts and the slice.
  fn laid(deltas: &[Vec<u8>]) -> (Vec<KeptText>, Vec<u8>) {
    let mut slice = Vec::new();
    let mut texts = Vec::new();
    for delta in deltas {
      let start = to_u64(slice.len());
      slice.extend_from_slice(delta);
      texts.push(KeptText {
        path: b"f".to_vec(),
        node: [0; 20],
        identity: [0; 32],
        len: to_u64(delta.len()),
        base: None,
        place: start..to_u64(slice.len()),
      });
    }
    (texts, slice)
  }

  /// Goal 93, main's ruling (b), its condition (1). Do: chunk deltas of every kind (empty, short, beginning with a NUL,
  /// incompressible, compressible past several steps) a step of `budget` bytes at a time, for budgets of one byte to past
  /// the whole. Expect: each chunk the one-shot chunk of its delta byte for byte (`revlog_chunk::chunk`, 7.2's rule), and
  /// decoded back to its delta; no step compressing more than its budget; the steps taken more than one where the
  /// budget is short of the deltas (non-vacuity).
  #[test]
  fn a_segments_chunks_made_a_step_at_a_time_are_the_one_shot_chunks() {
    let compressible: Vec<u8> = (0..40_000u32)
      .map(|at| b"manifest line\n"[(at % 14) as usize])
      .collect();
    let incompressible: Vec<u8> = (0..3_000u32)
      .map(|at| u8::try_from((at.wrapping_mul(2_654_435_761) >> 13) & 0xff).unwrap())
      .collect();
    let deltas = vec![
      Vec::new(),
      b"short".to_vec(),
      [&[0u8][..], &compressible[..200]].concat(),
      incompressible,
      compressible.clone(),
      [&[0u8; 12][..], &compressible[..5_000]].concat(),
    ];
    let (texts, slice) = laid(&deltas);
    let total = to_u64(slice.len());
    for budget in [1u64, 7, 4_096, 65_536, total * 2] {
      let mut chunked = Chunked::begin(texts.len()).unwrap();
      let mut steps = 1u64;
      while chunked.step((&texts, &slice), budget).unwrap() {
        steps += 1;
      }
      assert!(
        chunked.most_step() <= budget.max(SHORTEST_COMPRESSED as u64),
        "{budget}"
      );
      assert!(budget >= total || steps > 1, "{budget}: more than one step");
      let (payload, places) = chunked.finish();
      for (delta, place) in deltas.iter().zip(&places) {
        let range = usize::try_from(place.start).unwrap()..usize::try_from(place.end).unwrap();
        let stored = &payload[range];
        let mut one_shot = Vec::new();
        crate::revlog_chunk::chunk(delta, crate::revlog_chunk::Pass::Send, &mut one_shot).unwrap();
        assert_eq!(stored, one_shot.as_slice(), "{budget}: the one-shot chunk");
        let decoded = crate::revlog_chunk::decode_within(stored, to_u64(delta.len())).unwrap();
        assert_eq!(&decoded, delta, "{budget}: decoded back");
      }
    }
  }
}

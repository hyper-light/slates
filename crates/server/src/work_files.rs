//! A work volume's files as a delta over its base (§4.16 "Role": each agent works on a clone of the green; D-5
//! clones share what they did not change). A work holds only the files it changed since its base version, each as
//! its bytes or as removed; every other file is the green's at that version, read from the green's history, which
//! keeps that version while any work is based there (`merge_service::reachable_floor`).
//!
//! Before 2026-11-04 a work held a whole copy of its green: every accept copied every file of the green back into
//! the work and recounted its charge, so a submit's cost followed the green's size (a 10,000-file green of 4 KiB
//! files: 5.46 ms at the median, two thirds of it that copy, `crates/client/examples/edit_merge_bench.rs`). Now an
//! accept or a rebase moves the base and clears the delta, and a work is charged for what it holds itself.
//!
//! A file a work changed is held as pieces over its base (A-790): ranges of the base file, which stay the green's at
//! the work's base version (pinned by its floor, `merge_service::reachable_floor`), and ranges of the bytes the work
//! wrote. Before 2026-11-20 a first edit copied the whole file into the work: eight works each overwriting one byte of a
//! 64 MiB file added 201 MB resident and 2.38 ms a work (`tests/work_edit_cow.rs` red, release, Mac Studio); held as
//! pieces they add 32 KiB and 0.15 ms. An edit, a rename and an unlink read only the base file's length, so a work off
//! its green's shard asks the owner for lengths, never bytes (`crate::work_home`'s `Need::Lengths`).
//!
//! A work whose green is gone has no base to read: a file it did not change answers `UnknownBase`, as its submit
//! does (§4.16 failure matrix); a file it changed reads only where the work wrote all of it, since its other pieces are
//! the base's.

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_merge::engine::Green;

/// A page of one base file the green's owner answered (A-247).
#[derive(Debug)]
pub struct FetchedPage {
  /// The file.
  pub path: String,
  /// The offset the page starts at.
  pub offset: u64,
  /// Its bytes and the file's whole length; `None` when the base has no such file.
  pub answer: Option<(Vec<u8>, u64)>,
}

/// What the green's owner answered for the one verb now running on a work's shard, when the green lives on another
/// shard (A-247): the base's files the verb asked for, the page of a paged read, or the base's files and lengths.
/// Taken by that verb and gone with it, so a base's bytes are never held for a work between verbs: nothing to
/// invalidate and nothing to charge. What the verb did not ask for reads as [`BaseGone`], which `crate::work_home`'s
/// gate rules out by asking for every path a verb reads.
#[derive(Debug, Default)]
pub struct FetchedBase {
  /// The green no longer exists.
  pub gone: bool,
  /// The files asked for, `None` for one the base does not have.
  pub files: BTreeMap<String, Option<Vec<u8>>>,
  /// A page of one file, when one was asked for.
  pub page: Option<FetchedPage>,
  /// Every file of the base with its length, when asked for.
  pub sizes: Option<Vec<(String, u64)>>,
  /// The base entries of the files asked for (`Green::path_base`), which the work keeps to compose its increment.
  pub entries: BTreeMap<String, slates_merge::increment::Base>,
  /// The lengths of the files asked for, `None` for one the base does not have: what an edit or a declaration of a
  /// file the work has not changed needs of it, never its bytes.
  pub lengths: BTreeMap<String, Option<u64>>,
}

/// What a work changed since its base: per path, the file as it holds it (pieces over its base), or `None` for a file
/// of the base it removed.
#[derive(Clone, Debug, Default)]
pub struct WorkFiles {
  changed: BTreeMap<String, Option<Held>>,
}

/// One run of a held file's bytes: a range of its origin's base bytes, or a range of what the work wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
  /// `len` bytes of the origin's base file from `at`.
  Base { at: u64, len: u64 },
  /// `len` bytes of the work's written buffer from `at`.
  Own { at: u64, len: u64 },
}

impl Piece {
  fn len(self) -> u64 {
    match self {
      Piece::Base { len, .. } | Piece::Own { len, .. } => len,
    }
  }

  /// The part of this piece in `[from, to)` of its own bytes.
  fn cut(self, from: u64, to: u64) -> Piece {
    let len = to.saturating_sub(from);
    match self {
      Piece::Base { at, .. } => Piece::Base {
        at: at.saturating_add(from),
        len,
      },
      Piece::Own { at, .. } => Piece::Own {
        at: at.saturating_add(from),
        len,
      },
    }
  }

  /// This piece and `next` as one, when `next` continues it in the same source.
  fn joined(self, next: Piece) -> Option<Piece> {
    match (self, next) {
      (
        Piece::Base { at, len },
        Piece::Base {
          at: next_at,
          len: more,
        },
      ) if at.saturating_add(len) == next_at => Some(Piece::Base {
        at,
        len: len.saturating_add(more),
      }),
      (
        Piece::Own { at, len },
        Piece::Own {
          at: next_at,
          len: more,
        },
      ) if at.saturating_add(len) == next_at => Some(Piece::Own {
        at,
        len: len.saturating_add(more),
      }),
      _ => None,
    }
  }
}

/// Appends `piece` to `pieces`, joined to the last one where it continues it; an empty piece adds nothing.
fn push(pieces: &mut Vec<Piece>, piece: Piece) {
  if piece.len() == 0 {
    return;
  }
  if let Some(last) = pieces.last_mut()
    && let Some(joined) = last.joined(piece)
  {
    *last = joined;
    return;
  }
  pieces.push(piece);
}

/// One run of a held file as its parts name it ([`HeldParts`]): `len` bytes from `at` of the origin's base file, or of
/// the written bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiecePart {
  /// A range of the origin's base file.
  Base {
    /// Where in the base file.
    at: u64,
    /// Its length.
    len: u64,
  },
  /// A range of the written bytes.
  Own {
    /// Where in the written bytes.
    at: u64,
    /// Its length.
    len: u64,
  },
}

/// A held file's parts (A-806): the base path its base pieces read (`None` when none do), its pieces in order, the
/// bytes it wrote, and its length. What a rebase answers for a changed file and a restatement logs, in place of the
/// file's bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeldParts {
  /// The base path its base pieces read.
  pub origin: Option<String>,
  /// The pieces, in order.
  pub pieces: Vec<PiecePart>,
  /// The written bytes the own pieces name.
  pub written: Vec<u8>,
  /// The file's length.
  pub len: u64,
}

impl HeldParts {
  /// `bytes` held whole: one own piece, no base.
  pub fn whole(bytes: Vec<u8>) -> HeldParts {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let pieces = if len == 0 {
      Vec::new()
    } else {
      vec![PiecePart::Own { at: 0, len }]
    };
    HeldParts {
      origin: None,
      pieces,
      written: bytes,
      len,
    }
  }

  /// The file's bytes, its base pieces read from `base` (the origin's base bytes), or `None` when a base piece reads
  /// past them. For tests and checks.
  pub fn materialize(&self, base: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(usize::try_from(self.len).ok()?);
    for piece in &self.pieces {
      let (from, at, len) = match *piece {
        PiecePart::Base { at, len } => (base, at, len),
        PiecePart::Own { at, len } => (self.written.as_slice(), at, len),
      };
      let start = usize::try_from(at).ok()?;
      let end = start.checked_add(usize::try_from(len).ok()?)?;
      out.extend_from_slice(from.get(start..end)?);
    }
    Some(out)
  }
}

/// Format: the run [`equal_run`] compares at once before looking at bytes, a page, so the comparison runs at the
/// machine's `memcmp` speed over the equal runs, which are most of a file a small edit changed.
const COMPARE_CHUNK: usize = 4096;

/// How many leading bytes `a` and `b` share: whole chunks compared at once, then the bytes of the first chunk that
/// differs.
fn equal_run(a: &[u8], b: &[u8]) -> usize {
  let limit = a.len().min(b.len());
  let mut at = 0usize;
  while at < limit {
    let end = at.saturating_add(COMPARE_CHUNK).min(limit);
    if a.get(at..end) == b.get(at..end) {
      at = end;
      continue;
    }
    return at.saturating_add(
      a.get(at..end)
        .unwrap_or_default()
        .iter()
        .zip(b.get(at..end).unwrap_or_default())
        .take_while(|(x, y)| x == y)
        .count(),
    );
  }
  limit
}

/// How many trailing bytes `a` and `b` share, at most `room`: [`equal_run`] from the end.
fn equal_tail(a: &[u8], b: &[u8], room: usize) -> usize {
  let mut shared = 0usize;
  while shared < room {
    let step = COMPARE_CHUNK.min(room.saturating_sub(shared));
    let a_end = a.len().saturating_sub(shared);
    let b_end = b.len().saturating_sub(shared);
    let a_chunk = a.get(a_end.saturating_sub(step)..a_end).unwrap_or_default();
    let b_chunk = b.get(b_end.saturating_sub(step)..b_end).unwrap_or_default();
    if a_chunk == b_chunk {
      shared = shared.saturating_add(step);
      continue;
    }
    return shared.saturating_add(
      a_chunk
        .iter()
        .rev()
        .zip(b_chunk.iter().rev())
        .take_while(|(x, y)| x == y)
        .count(),
    );
  }
  room
}

/// Normalizes and coalesces `keep_own` byte ranges clamped to `len` into sorted non-overlapping spans.
fn merge_own_spans(keep_own: &[(u64, u64)], len: usize) -> Vec<(usize, usize)> {
  let mut merged: Vec<(usize, usize)> = keep_own
    .iter()
    .filter_map(|&(start, end)| {
      let lo = usize::try_from(start).unwrap_or(len).min(len);
      let hi = usize::try_from(end).unwrap_or(len).min(len);
      (lo < hi).then_some((lo, hi))
    })
    .collect();
  merged.sort_unstable();
  let mut spans: Vec<(usize, usize)> = Vec::with_capacity(merged.len());
  for (lo, hi) in merged {
    match spans.last_mut() {
      Some((_, prev_hi)) if lo <= *prev_hi => *prev_hi = (*prev_hi).max(hi),
      _ => spans.push((lo, hi)),
    }
  }
  spans
}

/// Appends a [`PiecePart::Base`] run, coalescing with an adjacent base piece.
fn push_base_piece(pieces: &mut Vec<PiecePart>, at: usize, run: usize) {
  if run == 0 {
    return;
  }
  let at_u = u64::try_from(at).unwrap_or(u64::MAX);
  let run_u = u64::try_from(run).unwrap_or(u64::MAX);
  if let Some(PiecePart::Base {
    at: prev_at,
    len: prev_len,
  }) = pieces.last_mut()
    && prev_at.saturating_add(*prev_len) == at_u
  {
    *prev_len = prev_len.saturating_add(run_u);
    return;
  }
  pieces.push(PiecePart::Base {
    at: at_u,
    len: run_u,
  });
}

/// Appends `run` to `written` as a [`PiecePart::Own`] piece, coalescing with an adjacent own piece.
fn push_own_piece(pieces: &mut Vec<PiecePart>, written: &mut Vec<u8>, run: &[u8]) {
  if run.is_empty() {
    return;
  }
  let at_u = u64::try_from(written.len()).unwrap_or(u64::MAX);
  let run_u = u64::try_from(run.len()).unwrap_or(u64::MAX);
  written.extend_from_slice(run);
  if let Some(PiecePart::Own {
    at: prev_at,
    len: prev_len,
  }) = pieces.last_mut()
    && prev_at.saturating_add(*prev_len) == at_u
  {
    *prev_len = prev_len.saturating_add(run_u);
    return;
  }
  pieces.push(PiecePart::Own {
    at: at_u,
    len: run_u,
  });
}

/// Emits base/own pieces across a same-length gap `[from..to)` outside required own spans.
fn emit_same_len_gap(
  (base, bytes): (&[u8], &[u8]),
  (from, to): (usize, usize),
  pieces: &mut Vec<PiecePart>,
  written: &mut Vec<u8>,
) {
  let mut at = from;
  let mut own_from = from;
  while at < to {
    let equal = equal_run(
      base.get(at..to).unwrap_or_default(),
      bytes.get(at..to).unwrap_or_default(),
    );
    if equal > PIECE_BYTES
      || (at == from && equal > 0)
      || (at.saturating_add(equal) == to && equal > 0)
    {
      push_own_piece(pieces, written, bytes.get(own_from..at).unwrap_or_default());
      push_base_piece(pieces, at, equal);
      at = at.saturating_add(equal);
      own_from = at;
    } else {
      let differ = base
        .get(at..to)
        .unwrap_or_default()
        .iter()
        .zip(bytes.get(at..to).unwrap_or_default())
        .skip(equal)
        .take_while(|(a, b)| a != b)
        .count();
      at = at.saturating_add(equal.saturating_add(differ).max(1));
    }
  }
  push_own_piece(pieces, written, bytes.get(own_from..to).unwrap_or_default());
}

/// Like [`parts_over_base`], guaranteeing that every range in `keep_own` (post-file offsets written by the work's
/// restated journal) is emitted as [`PiecePart::Own`] rather than [`PiecePart::Base`] (Wave 8B: prevents matching
/// runs inside a work's own write from turning into base pieces that fail off-owner composition with `BaseGone`).
pub fn parts_over_base_keeping(
  path: &str,
  base: Option<&[u8]>,
  bytes: Vec<u8>,
  keep_own: &[(u64, u64)],
) -> HeldParts {
  if keep_own.is_empty() {
    return parts_over_base(path, base, bytes);
  }
  let Some(base) = base else {
    return HeldParts::whole(bytes);
  };
  let len = bytes.len();
  let spans = merge_own_spans(keep_own, len);
  if spans.is_empty() {
    return parts_over_base(path, Some(base), bytes);
  }
  let mut pieces: Vec<PiecePart> = Vec::new();
  let mut written: Vec<u8> = Vec::new();
  if len == base.len() {
    let mut cursor = 0usize;
    for (lo, hi) in spans {
      emit_same_len_gap((base, &bytes), (cursor, lo), &mut pieces, &mut written);
      push_own_piece(
        &mut pieces,
        &mut written,
        bytes.get(lo..hi).unwrap_or_default(),
      );
      cursor = hi;
    }
    emit_same_len_gap((base, &bytes), (cursor, len), &mut pieces, &mut written);
  } else {
    let first_lo = spans.first().map_or(len, |(lo, _)| *lo);
    let last_hi = spans.last().map_or(0, |(_, hi)| *hi);
    let prefix = equal_run(
      base.get(..first_lo.min(base.len())).unwrap_or_default(),
      bytes.get(..first_lo).unwrap_or_default(),
    );
    let max_suffix = len
      .saturating_sub(last_hi)
      .min(base.len().saturating_sub(prefix));
    let suffix = equal_tail(base, &bytes, max_suffix);
    push_base_piece(&mut pieces, 0, prefix);
    push_own_piece(
      &mut pieces,
      &mut written,
      bytes
        .get(prefix..len.saturating_sub(suffix))
        .unwrap_or_default(),
    );
    push_base_piece(&mut pieces, base.len().saturating_sub(suffix), suffix);
  }
  HeldParts {
    origin: pieces
      .iter()
      .any(|piece| matches!(piece, PiecePart::Base { .. }))
      .then(|| path.to_owned()),
    pieces,
    written,
    len: u64::try_from(len).unwrap_or(u64::MAX),
  }
}

/// `bytes`, a file's content at a work's new base, as parts over `base`, the same path's bytes at that base (A-806): the
/// runs they share are base pieces, and only the runs that differ are written. The common prefix and suffix are found
/// first; inside them, equal runs longer than a piece's own cost ([`PIECE_BYTES`]) are kept as base pieces when the two
/// lengths agree (overwrites, the common edit), and the middle is written whole when they do not. Exact whatever the
/// content: the parts read back as `bytes`.
pub fn parts_over_base(path: &str, base: Option<&[u8]>, bytes: Vec<u8>) -> HeldParts {
  let Some(base) = base else {
    return HeldParts::whole(bytes);
  };
  let len = bytes.len();
  let prefix = equal_run(base, &bytes);
  let room = len.min(base.len()).saturating_sub(prefix);
  let suffix = equal_tail(base, &bytes, room);
  let mut pieces: Vec<PiecePart> = Vec::new();
  let mut written: Vec<u8> = Vec::new();
  let to_u64 = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
  let base_run = |pieces: &mut Vec<PiecePart>, at: usize, run: usize| {
    if run > 0 {
      pieces.push(PiecePart::Base {
        at: to_u64(at),
        len: to_u64(run),
      });
    }
  };
  let mut own_run = |pieces: &mut Vec<PiecePart>, run: &[u8]| {
    if !run.is_empty() {
      pieces.push(PiecePart::Own {
        at: to_u64(written.len()),
        len: to_u64(run.len()),
      });
      written.extend_from_slice(run);
    }
  };
  base_run(&mut pieces, 0, prefix);
  let middle_end = len.saturating_sub(suffix);
  if len == base.len() {
    // Same length: walk the middle, keeping equal runs worth a piece as base pieces.
    let mut at = prefix;
    let mut own_from = prefix;
    while at < middle_end {
      let equal = equal_run(
        base.get(at..middle_end).unwrap_or_default(),
        bytes.get(at..middle_end).unwrap_or_default(),
      );
      if equal > PIECE_BYTES {
        own_run(&mut pieces, bytes.get(own_from..at).unwrap_or_default());
        base_run(&mut pieces, at, equal);
        at = at.saturating_add(equal);
        own_from = at;
      } else {
        let differ = base
          .get(at..middle_end)
          .unwrap_or_default()
          .iter()
          .zip(bytes.get(at..middle_end).unwrap_or_default())
          .skip(equal)
          .take_while(|(a, b)| a != b)
          .count();
        at = at.saturating_add(equal.saturating_add(differ).max(1));
      }
    }
    own_run(
      &mut pieces,
      bytes.get(own_from..middle_end).unwrap_or_default(),
    );
  } else {
    own_run(
      &mut pieces,
      bytes.get(prefix..middle_end).unwrap_or_default(),
    );
  }
  base_run(&mut pieces, base.len().saturating_sub(suffix), suffix);
  HeldParts {
    origin: pieces
      .iter()
      .any(|piece| matches!(piece, PiecePart::Base { .. }))
      .then(|| path.to_owned()),
    pieces,
    written,
    len: to_u64(len),
  }
}

/// Shape: what one piece costs a work's charge: its size in memory.
const PIECE_BYTES: usize = std::mem::size_of::<Piece>();

/// A file a work changed (D-5: clones share what they did not change). Its bytes are pieces in order: ranges of its
/// origin's base bytes, which stay the green's, pinned at the work's base version, and ranges of `written`, the bytes
/// the work wrote. So a first edit of a base file holds the bytes it wrote and a few pieces, never the file: a one-byte
/// edit of a 1 GB file holds one byte and three pieces. `written` only grows by what an edit inserts, and is compacted
/// to its live bytes once the dead ones (written, then overwritten or deleted) outnumber them, so a file rewritten again
/// and again holds at most twice what it now shows of its own.
#[derive(Clone, Debug, Default)]
struct Held {
  /// The base path its base pieces are read from: the path itself, or the path it was renamed from. `None` once no
  /// piece reads the base.
  origin: Option<String>,
  pieces: Vec<Piece>,
  written: Vec<u8>,
  len: u64,
}

/// What a held file's charge counts, besides its path: its origin's name, its written bytes and its pieces.
fn held_bytes(origin: Option<&str>, written: usize, pieces: usize) -> usize {
  origin
    .map_or(0, str::len)
    .saturating_add(written)
    .saturating_add(pieces.saturating_mul(PIECE_BYTES))
}

impl Held {
  /// A file of the base the work has not changed, as its first change finds it: every byte the base's.
  fn over(path: &str, len: u64) -> Held {
    let mut pieces = Vec::new();
    push(&mut pieces, Piece::Base { at: 0, len });
    Held {
      origin: (!pieces.is_empty()).then(|| path.to_owned()),
      pieces,
      written: Vec::new(),
      len,
    }
  }

  /// A file held whole, every byte the work's (a restatement's or a rebase's).
  /// The held file `parts` name (A-806), its pieces merged where they continue each other.
  fn from_parts(parts: HeldParts) -> Held {
    let mut pieces = Vec::with_capacity(parts.pieces.len());
    for piece in parts.pieces {
      push(
        &mut pieces,
        match piece {
          PiecePart::Base { at, len } => Piece::Base { at, len },
          PiecePart::Own { at, len } => Piece::Own { at, len },
        },
      );
    }
    let origin = parts.origin.filter(|_| {
      pieces
        .iter()
        .any(|piece| matches!(piece, Piece::Base { .. }))
    });
    Held {
      origin,
      pieces,
      written: parts.written,
      len: parts.len,
    }
  }

  /// This held file's parts (A-806).
  fn parts(&self) -> HeldParts {
    HeldParts {
      origin: self.origin.clone(),
      pieces: self
        .pieces
        .iter()
        .map(|piece| match *piece {
          Piece::Base { at, len } => PiecePart::Base { at, len },
          Piece::Own { at, len } => PiecePart::Own { at, len },
        })
        .collect(),
      written: self.written.clone(),
      len: self.len,
    }
  }

  fn whole(bytes: Vec<u8>) -> Held {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let mut pieces = Vec::new();
    push(&mut pieces, Piece::Own { at: 0, len });
    Held {
      origin: None,
      pieces,
      written: bytes,
      len,
    }
  }

  fn footprint(&self, path: &str) -> u64 {
    crate::work_charge::file_footprint(
      path,
      held_bytes(
        self.origin.as_deref(),
        self.written.len(),
        self.pieces.len(),
      ),
    )
  }

  /// The pieces splicing `[at, at + delete_len)` for `inserted` bytes appended at `own_at` of `written` leaves, and the
  /// length then.
  fn spliced(
    &self,
    (at, delete_len): (u64, u64),
    (own_at, inserted): (u64, u64),
  ) -> (Vec<Piece>, u64) {
    let start = at.min(self.len);
    let end = start.saturating_add(delete_len.min(self.len.saturating_sub(start)));
    let mut out = Vec::with_capacity(self.pieces.len().saturating_add(2));
    let mut logical = 0u64;
    for piece in &self.pieces {
      let (from, to) = (logical, logical.saturating_add(piece.len()));
      logical = to;
      if from < start {
        push(&mut out, piece.cut(0, to.min(start).saturating_sub(from)));
      }
    }
    push(
      &mut out,
      Piece::Own {
        at: own_at,
        len: inserted,
      },
    );
    logical = 0;
    for piece in &self.pieces {
      let (from, to) = (logical, logical.saturating_add(piece.len()));
      logical = to;
      if to > end {
        push(
          &mut out,
          piece.cut(end.max(from).saturating_sub(from), piece.len()),
        );
      }
    }
    let len = self
      .len
      .saturating_sub(end.saturating_sub(start))
      .saturating_add(inserted);
    (out, len)
  }

  /// The live bytes of `written` `pieces` show.
  fn live(pieces: &[Piece]) -> u64 {
    pieces
      .iter()
      .map(|piece| match piece {
        Piece::Own { len, .. } => *len,
        Piece::Base { .. } => 0,
      })
      .fold(0, u64::saturating_add)
  }

  /// Whether `written` of `total` bytes, `live` of them shown, is compacted: once the dead outnumber the live.
  fn compacts(total: u64, live: u64) -> bool {
    total.saturating_sub(live) > live
  }

  /// What this file's charge bytes would be after the splice ([`Held::splice`] without applying it).
  fn footprint_after(&self, path: &str, (at, delete_len): (u64, u64), inserted: u64) -> u64 {
    let total = u64::try_from(self.written.len())
      .unwrap_or(u64::MAX)
      .saturating_add(inserted);
    let (pieces, _) = self.spliced((at, delete_len), (total.saturating_sub(inserted), inserted));
    let live = Held::live(&pieces);
    let written = if Held::compacts(total, live) {
      live
    } else {
      total
    };
    let reads_base = pieces
      .iter()
      .any(|piece| matches!(piece, Piece::Base { .. }));
    crate::work_charge::file_footprint(
      path,
      held_bytes(
        self.origin.as_deref().filter(|_| reads_base),
        usize::try_from(written).unwrap_or(usize::MAX),
        pieces.len(),
      ),
    )
  }

  /// Splices `[at, at + delete_len)` for `bytes`: they are appended to `written`, and only the pieces change.
  fn splice(&mut self, (at, delete_len): (u64, u64), bytes: &[u8]) {
    let own_at = u64::try_from(self.written.len()).unwrap_or(u64::MAX);
    let inserted = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let (pieces, len) = self.spliced((at, delete_len), (own_at, inserted));
    self.written.extend_from_slice(bytes);
    self.pieces = pieces;
    self.len = len;
    if !self
      .pieces
      .iter()
      .any(|piece| matches!(piece, Piece::Base { .. }))
    {
      self.origin = None;
    }
    let total = u64::try_from(self.written.len()).unwrap_or(u64::MAX);
    if Held::compacts(total, Held::live(&self.pieces)) {
      self.compact();
    }
  }

  /// `written` cut to the bytes the pieces show, each own piece moved to where its bytes now are. The count of pieces
  /// is kept, so the charge [`Held::footprint_after`] foresaw holds.
  fn compact(&mut self) {
    let live = usize::try_from(Held::live(&self.pieces)).unwrap_or(usize::MAX);
    let mut written = Vec::with_capacity(live);
    for piece in &mut self.pieces {
      if let Piece::Own { at, len } = *piece {
        let (from, to) = (
          usize::try_from(at).unwrap_or(usize::MAX),
          usize::try_from(at.saturating_add(len)).unwrap_or(usize::MAX),
        );
        let moved_to = u64::try_from(written.len()).unwrap_or(u64::MAX);
        written.extend_from_slice(self.written.get(from..to).unwrap_or_default());
        *piece = Piece::Own { at: moved_to, len };
      }
    }
    self.written = written;
  }

  /// The file's bytes when the work wrote them all in one run: lent, never copied.
  fn lent(&self) -> Option<&[u8]> {
    match self.pieces.as_slice() {
      [] => Some(&[]),
      [Piece::Own { at, len }] => self
        .written
        .get(usize::try_from(*at).ok()?..usize::try_from(at.saturating_add(*len)).ok()?),
      _ => None,
    }
  }

  /// Copies the file's bytes in `[from, to)` directly into `out`, returning the bytes copied (zero heap allocation).
  fn copy_into(
    &self,
    base: Base<'_>,
    (from, to): (u64, u64),
    out: &mut [u8],
  ) -> Result<usize, BaseGone> {
    let to = to.min(self.len);
    let mut logical = 0u64;
    let mut written = 0usize;
    for piece in &self.pieces {
      let (start, end) = (logical, logical.saturating_add(piece.len()));
      logical = end;
      if end <= from || start >= to {
        continue;
      }
      let part = piece.cut(
        from.max(start).saturating_sub(start),
        to.min(end).saturating_sub(start),
      );
      match part {
        Piece::Own { at, len } => {
          let (lo, hi) = (
            usize::try_from(at).unwrap_or(usize::MAX),
            usize::try_from(at.saturating_add(len)).unwrap_or(usize::MAX),
          );
          let src = self.written.get(lo..hi).unwrap_or_default();
          let take = src.len().min(out.len().saturating_sub(written));
          if let (Some(dst), Some(slice)) = (
            out.get_mut(written..written.saturating_add(take)),
            src.get(..take),
          ) {
            dst.copy_from_slice(slice);
            written = written.saturating_add(take);
          }
        }
        Piece::Base { at, len } => {
          let origin = self.origin.as_deref().ok_or(BaseGone)?;
          let src = base.range(origin, at, len)?;
          let take = src.len().min(out.len().saturating_sub(written));
          if let (Some(dst), Some(slice)) = (
            out.get_mut(written..written.saturating_add(take)),
            src.get(..take),
          ) {
            dst.copy_from_slice(slice);
            written = written.saturating_add(take);
          }
        }
      }
    }
    Ok(written)
  }

  /// Appends the file's bytes in `[from, to)` (clamped to its length) to `out`: written bytes from `written`, base
  /// bytes from the base's range alone.
  fn read_into(
    &self,
    base: Base<'_>,
    (from, to): (u64, u64),
    out: &mut Vec<u8>,
  ) -> Result<(), BaseGone> {
    let to = to.min(self.len);
    let mut logical = 0u64;
    for piece in &self.pieces {
      let (start, end) = (logical, logical.saturating_add(piece.len()));
      logical = end;
      if end <= from || start >= to {
        continue;
      }
      let part = piece.cut(
        from.max(start).saturating_sub(start),
        to.min(end).saturating_sub(start),
      );
      match part {
        Piece::Own { at, len } => {
          let (lo, hi) = (
            usize::try_from(at).unwrap_or(usize::MAX),
            usize::try_from(at.saturating_add(len)).unwrap_or(usize::MAX),
          );
          out.extend_from_slice(self.written.get(lo..hi).unwrap_or_default());
        }
        Piece::Base { at, len } => {
          let origin = self.origin.as_deref().ok_or(BaseGone)?;
          out.extend_from_slice(&base.range(origin, at, len)?);
        }
      }
    }
    Ok(())
  }

  /// The span of base bytes the file's `[from, to)` reads, `(at, len)`, when it reads any.
  fn base_span(&self, (from, to): (u64, u64)) -> Option<(u64, u64)> {
    let mut logical = 0u64;
    let mut span: Option<(u64, u64)> = None;
    for piece in &self.pieces {
      let (start, end) = (logical, logical.saturating_add(piece.len()));
      logical = end;
      if end <= from || start >= to {
        continue;
      }
      if let Piece::Base { at, len } = piece.cut(
        from.max(start).saturating_sub(start),
        to.min(end).saturating_sub(start),
      ) {
        let (lo, hi) = span.unwrap_or((at, at));
        span = Some((lo.min(at), hi.max(at.saturating_add(len))));
      }
    }
    span.map(|(lo, hi)| (lo, hi.saturating_sub(lo)))
  }
}

/// A work's base: its green, when the green still exists, and the version the work is based on.
#[derive(Clone, Copy)]
pub struct Base<'a> {
  /// The green, when it lives on this shard; `None` once it is destroyed or when it lives on another.
  pub green: Option<&'a Green>,
  /// What the green's owner answered for this verb, when the green lives on another shard (A-247).
  pub fetched: Option<&'a FetchedBase>,
  /// The base version.
  pub version: u64,
  /// The work's kept base entries at that version (A-247; `WorkState::entries`), each naming its path's length there:
  /// where a length is read when neither the green nor this verb's fetch has it, so an edit after a rebase asks no one
  /// for a length the rebase already gave (A-808). `None` where no work's entries apply.
  pub entries: Option<&'a BTreeMap<String, slates_merge::increment::Base>>,
  /// The shard's content plane, where a base file the green holds by its object's name alone is read (A-760 (b) 4d);
  /// none for a base read without one, which refuses such a file [`BaseGone`].
  pub(crate) plane: Option<crate::vcs_store::named_objects::PlaneObjects<'a>>,
}

impl<'a> Base<'a> {
  /// The base's bytes of `path`: `Ok(None)` when the base has no such file, `Err(BaseGone)` when the green is gone
  /// and the base cannot be read, or the file is held by its content-plane object alone and the plane cannot give it
  /// now (A-760 (b) 4d: read through the plane otherwise).
  pub(crate) fn file(self, path: &str) -> Result<Option<Cow<'a, [u8]>>, BaseGone> {
    if let Some(green) = self.green {
      return green
        .content_ref_at(path, self.version)
        .map(|content| match content {
          slates_merge::engine::ContentAt::Bytes(bytes) => Ok(bytes),
          named => crate::vcs_store::named_objects::whole_of(named, self.plane.as_ref())
            .map(Cow::Owned)
            .ok_or(BaseGone),
        })
        .transpose();
    }
    match self.fetched {
      Some(fetched) if !fetched.gone => match fetched.files.get(path) {
        Some(file) => Ok(file.as_deref().map(Cow::Borrowed)),
        None => Err(BaseGone),
      },
      _ => Err(BaseGone),
    }
  }

  /// The base's length of `path`, never its bytes: `Ok(None)` when the base has no such file.
  pub(crate) fn len(self, path: &str) -> Result<Option<u64>, BaseGone> {
    let as_u64 = |len: usize| u64::try_from(len).unwrap_or(u64::MAX);
    if let Some(green) = self.green {
      return Ok(green.content_len_at(path, self.version).map(as_u64));
    }
    let fetched = self.fetched.filter(|fetched| !fetched.gone);
    if let Some(fetched) = fetched {
      if let Some(len) = fetched.lengths.get(path) {
        return Ok(*len);
      }
      if let Some(file) = fetched.files.get(path) {
        return Ok(file.as_ref().map(|bytes| as_u64(bytes.len())));
      }
      if let Some(page) = fetched.page.as_ref().filter(|page| page.path == path) {
        return Ok(page.answer.as_ref().map(|(_, total)| *total));
      }
    }
    // A fetch that says the green is gone is the answer: no base.
    if self.fetched.is_some_and(|fetched| fetched.gone) {
      return Err(BaseGone);
    }
    self.entry_len(path).ok_or(BaseGone)
  }

  /// `path`'s length at the base from the work's kept entry of it (A-808): `Some(Some(len))` for a file there,
  /// `Some(None)` for an entry that holds no file of that path, `None` when no entry of `path` is kept.
  pub(crate) fn entry_len(self, path: &str) -> Option<Option<u64>> {
    let entry = self.entries?.get(path)?;
    Some(
      entry
        .files
        .iter()
        .find(|(file, _)| file == path)
        .map(|(_, len)| *len),
    )
  }

  /// The base's bytes of `path` in `[at, at + len)`, which a held file's piece names: copied out of that range alone,
  /// or lent from what the owner sent.
  fn range(self, path: &str, at: u64, len: u64) -> Result<Cow<'a, [u8]>, BaseGone> {
    let slice = |bytes: &'a [u8], from: u64| {
      let lo = usize::try_from(at.saturating_sub(from)).unwrap_or(usize::MAX);
      let hi = usize::try_from(at.saturating_add(len).saturating_sub(from)).unwrap_or(usize::MAX);
      bytes.get(lo..hi).map(Cow::Borrowed)
    };
    if let Some(green) = self.green {
      let (page, _) = green
        .content_page_at(path, self.version, (at, len))
        .ok_or(BaseGone)?;
      return unread_gone(path, page, (at, len), self.plane.as_ref())
        .map(|page| Cow::Owned(page.into_owned()));
    }
    let fetched = self
      .fetched
      .filter(|fetched| !fetched.gone)
      .ok_or(BaseGone)?;
    if let Some(Some(bytes)) = fetched.files.get(path) {
      return slice(bytes, 0).ok_or(BaseGone);
    }
    match &fetched.page {
      Some(FetchedPage {
        path: paged,
        offset,
        answer: Some((bytes, _)),
      }) if paged == path => slice(bytes, *offset).ok_or(BaseGone),
      _ => Err(BaseGone),
    }
  }

  /// The page of `path` the owner answered, cut to `want` bytes, when this verb asked for one at `offset`.
  fn fetched_page(self, path: &str, (offset, want): (u64, u64)) -> Option<Option<(Vec<u8>, u64)>> {
    let page = self
      .fetched
      .filter(|fetched| !fetched.gone)?
      .page
      .as_ref()?;
    if page.path != path || page.offset != offset {
      return None;
    }
    Some(page.answer.as_ref().map(|(bytes, total)| {
      let keep = usize::try_from(want).unwrap_or(usize::MAX).min(bytes.len());
      (bytes.get(..keep).unwrap_or_default().to_vec(), *total)
    }))
  }
}

/// The base of a work cannot be read: its green is gone, or (A-760 (b) 4d) a file of it is held by its content-plane
/// object alone and the plane cannot give it now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaseGone;

/// A base file's page (`Green::content_page_at`'s, at `range`) as its bytes, read through `plane` for one held by its
/// content-plane object alone (A-760 (b) 4d): refused [`BaseGone`] where the plane cannot give them now.
fn unread_gone<'a>(
  path: &str,
  page: slates_merge::engine::ContentAt<'a>,
  range: (u64, u64),
  plane: Option<&crate::vcs_store::named_objects::PlaneObjects<'_>>,
) -> Result<Cow<'a, [u8]>, BaseGone> {
  crate::vcs_store::named_objects::page_of(path, page, range, plane).map_err(|_| BaseGone)
}

/// The footprint a work's delta holds at one key: the path and what it holds of the file, the path alone for a removal.
fn held(path: &str, entry: Option<&Option<Held>>) -> u64 {
  match entry {
    None => 0,
    Some(None) => crate::work_charge::file_footprint(path, 0),
    Some(Some(file)) => file.footprint(path),
  }
}

/// A path the work holds of its own, and its bytes put together from its pieces (none: removed).
pub type ChangedEntry<'a> = (&'a str, Option<Cow<'a, [u8]>>);

/// A file the work sees, by its path, and its bytes ([`WorkFiles::files`]).
pub type SeenFile<'a> = (String, Cow<'a, [u8]>);

impl WorkFiles {
  /// The file at `path` as the work sees it: its own bytes, nothing where it removed one, else the base's. A file the
  /// work changed is lent where it wrote it all, else put together from its pieces (the base's ranges and its own).
  pub fn get<'a>(&'a self, base: Base<'a>, path: &str) -> Result<Option<Cow<'a, [u8]>>, BaseGone> {
    match self.changed.get(path) {
      Some(None) => Ok(None),
      Some(Some(file)) => match file.lent() {
        Some(bytes) => Ok(Some(Cow::Borrowed(bytes))),
        None => {
          let mut bytes = Vec::with_capacity(usize::try_from(file.len).unwrap_or(0));
          file.read_into(base, (0, file.len), &mut bytes)?;
          Ok(Some(Cow::Owned(bytes)))
        }
      },
      None => base.file(path),
    }
  }

  /// The length of the file at `path` as the work sees it, never its bytes: what an edit's operations and charge need.
  pub fn len(&self, base: Base<'_>, path: &str) -> Result<Option<u64>, BaseGone> {
    match self.changed.get(path) {
      Some(entry) => Ok(entry.as_ref().map(|file| file.len)),
      None => base.len(path),
    }
  }

  /// Copies the file at `path`'s bytes from `from` into `out`, as far as the file reaches: the bytes copied, `None`
  /// when the work sees no such file. Only the range is read directly into `out` without intermediate allocation.
  pub fn copy_range(
    &self,
    base: Base<'_>,
    path: &str,
    from: u64,
    out: &mut [u8],
  ) -> Result<Option<usize>, BaseGone> {
    let want = u64::try_from(out.len()).unwrap_or(u64::MAX);
    match self.changed.get(path) {
      Some(None) => Ok(None),
      Some(Some(file)) => file
        .copy_into(base, (from, from.saturating_add(want)), out)
        .map(Some),
      None => match base.len(path)? {
        None => Ok(None),
        Some(len) => {
          let to = from.saturating_add(want).min(len);
          if to <= from {
            Ok(Some(0))
          } else {
            let bytes = base.range(path, from, to.saturating_sub(from))?;
            let copied = bytes.len().min(out.len());
            if let (Some(dst), Some(src)) = (out.get_mut(..copied), bytes.get(..copied)) {
              dst.copy_from_slice(src);
            }
            Ok(Some(copied))
          }
        }
      },
    }
  }

  /// Every file the work sees, sorted by path: the base's files it did not change and the files it holds. Without
  /// the base (its green gone) only the files the work holds whole are known. The paths are owned, as the green's walk
  /// gives them (its maps are persistent radix trees, A-534).
  /// A base file held by its content-plane object alone is read through the plane, refused [`BaseGone`] where the
  /// plane cannot give it now (A-760 (b) 4d).
  pub fn files<'a>(&'a self, base: Base<'a>) -> Result<Vec<SeenFile<'a>>, BaseGone> {
    let mut files: BTreeMap<String, Cow<'a, [u8]>> = match base.green {
      Some(green) => green
        .files_at(base.version)
        .into_iter()
        .map(|(path, content)| {
          let bytes = match content {
            slates_merge::engine::ContentAt::Bytes(bytes) => bytes,
            named => crate::vcs_store::named_objects::whole_of(named, base.plane.as_ref())
              .map(Cow::Owned)
              .ok_or(BaseGone)?,
          };
          Ok((path.clone(), bytes))
        })
        .collect::<Result<_, BaseGone>>()?,
      None => BTreeMap::new(),
    };
    for path in self.changed.keys() {
      match self.get(base, path) {
        Ok(Some(bytes)) => {
          files.insert(path.clone(), bytes);
        }
        Ok(None) | Err(BaseGone) => {
          files.remove(path.as_str());
        }
      }
    }
    Ok(files.into_iter().collect())
  }

  /// A page of the file at `path` as the work sees it: its bytes in `[offset, offset + want)` and its length, from its
  /// pieces or the base's ([`Green::content_page_at`]), never the whole file.
  pub fn page(
    &self,
    base: Base<'_>,
    path: &str,
    (offset, want): (u64, u64),
  ) -> Result<Option<(Vec<u8>, u64)>, BaseGone> {
    match self.changed.get(path) {
      Some(Some(file)) => {
        let end = offset.saturating_add(want).min(file.len);
        let mut page = Vec::with_capacity(usize::try_from(end.saturating_sub(offset)).unwrap_or(0));
        file.read_into(base, (offset, end), &mut page)?;
        Ok(Some((page, file.len)))
      }
      Some(None) => Ok(None),
      None => match base.green {
        Some(green) => green
          .content_page_at(path, base.version, (offset, want))
          .map(|(page, total)| {
            Ok((
              unread_gone(path, page, (offset, want), base.plane.as_ref())?.into_owned(),
              total,
            ))
          })
          .transpose(),
        None => base.fetched_page(path, (offset, want)).ok_or(BaseGone),
      },
    }
  }

  /// The span of the base's bytes, `(at, len)` of the file's origin, a read of `[offset, offset + want)` of a file the
  /// work changed needs (A-247: what a work off its green's shard asks the owner for), when it needs any.
  pub fn base_span(&self, path: &str, (offset, want): (u64, u64)) -> Option<(String, u64, u64)> {
    let file = self.changed.get(path)?.as_ref()?;
    let (at, len) = file.base_span((offset, offset.saturating_add(want)))?;
    Some((file.origin.clone()?, at, len))
  }

  /// Every file the work sees with its length, sorted by path: [`WorkFiles::files`] without the bytes.
  pub fn sizes(&self, base: Base<'_>) -> Vec<(String, u64)> {
    let mut sizes: BTreeMap<String, u64> = base
      .green
      .map(|green| {
        green
          .file_lens_at(base.version)
          .into_iter()
          .map(|(path, len)| (path, u64::try_from(len).unwrap_or(u64::MAX)))
          .collect()
      })
      .or_else(|| {
        let fetched = base.fetched?.sizes.as_ref()?;
        Some(
          fetched
            .iter()
            .map(|(path, len)| (path.clone(), *len))
            .collect(),
        )
      })
      .unwrap_or_default();
    for (path, entry) in &self.changed {
      match entry {
        Some(file) => {
          sizes.insert(path.clone(), file.len);
        }
        None => {
          sizes.remove(path.as_str());
        }
      }
    }
    sizes.into_iter().collect()
  }

  /// The footprint of everything the delta holds (the work's charge less its journal's).
  pub fn footprint(&self) -> u64 {
    self
      .changed
      .iter()
      .map(|(path, entry)| held(path, Some(entry)))
      .fold(0, u64::saturating_add)
  }

  /// What splicing `[at, at + delete_len)` of `path` for `inserted` bytes changes in the delta's footprint, before
  /// it is applied: `(added, removed)`. `old_len` is the file's length as the work sees it now (`None` when it is new):
  /// a file the work had not changed starts as the base's pieces, so only what the edit writes is added.
  pub fn splice_delta(
    &self,
    path: &str,
    old_len: Option<u64>,
    (at, delete_len, inserted): (u64, u64, usize),
  ) -> (u64, u64) {
    let before = held(path, self.changed.get(path));
    let inserted = u64::try_from(inserted).unwrap_or(u64::MAX);
    let after = match self.changed.get(path) {
      Some(Some(file)) => file.footprint_after(path, (at, delete_len), inserted),
      Some(None) => Held::default().footprint_after(path, (at, delete_len), inserted),
      None => {
        Held::over(path, old_len.unwrap_or(0)).footprint_after(path, (at, delete_len), inserted)
      }
    };
    (after.saturating_sub(before), before.saturating_sub(after))
  }

  /// Splices `[at, at + delete_len)` of `path` for `bytes`. A file the work has not changed starts as its base's
  /// pieces (only its length is read), a new path or a removed one empty; then the bytes are written and the pieces
  /// spliced, so nothing of the base is copied. The base's length must be readable for a file the work has not changed.
  pub fn splice(
    &mut self,
    base: Base<'_>,
    path: &str,
    (at, delete_len): (u64, u64),
    bytes: &[u8],
  ) -> Result<(), BaseGone> {
    let file = match self.changed.get_mut(path) {
      Some(Some(file)) => file,
      _ => {
        let started = match self.changed.get(path) {
          Some(None) => Held::default(),
          _ => base
            .len(path)?
            .map_or_else(Held::default, |len| Held::over(path, len)),
        };
        self.changed.insert(path.to_owned(), Some(started));
        match self.changed.get_mut(path) {
          Some(Some(file)) => file,
          _ => return Ok(()),
        }
      }
    };
    file.splice((at, delete_len), bytes);
    Ok(())
  }

  /// What removing `path` from the work's view leaves at that key: `Some(true)` a removal (the base has the file,
  /// or cannot be read, so it must stay hidden), `Some(false)` no entry (the file was the work's own), `None` when
  /// the work sees no such file and nothing changes.
  fn removal(&self, base: Base<'_>, path: &str) -> Result<Option<bool>, BaseGone> {
    if self.len(base, path)?.is_none() {
      return Ok(None);
    }
    Ok(Some(base.len(path).map_or(true, |len| len.is_some())))
  }

  /// The footprint `removal` leaves at `path`.
  fn held_after_removal(path: &str, removal: Option<bool>, before: u64) -> u64 {
    match removal {
      None => before,
      Some(true) => crate::work_charge::file_footprint(path, 0),
      Some(false) => 0,
    }
  }

  /// Applies `removal` at `path`.
  fn remove_as(&mut self, path: &str, removal: Option<bool>) {
    match removal {
      None => {}
      Some(true) => {
        self.changed.insert(path.to_owned(), None);
      }
      Some(false) => {
        self.changed.remove(path);
      }
    }
  }

  /// The file at `from` as a rename moves it: what the work holds there, or the base's pieces of a file it has not
  /// changed (only its length read). `None` when the work sees no such file.
  fn moved(&self, base: Base<'_>, from: &str) -> Result<Option<Held>, BaseGone> {
    match self.changed.get(from) {
      Some(entry) => Ok(entry.clone()),
      None => Ok(base.len(from)?.map(|len| Held::over(from, len))),
    }
  }

  /// What a namespace declaration `op` changes in the delta's footprint, before it is applied: `(added, removed)`.
  pub fn declare_delta(
    &self,
    base: Base<'_>,
    op: &slates_merge::increment::VolumeOp,
  ) -> Result<(u64, u64), BaseGone> {
    use slates_merge::increment::VolumeOp;
    let (before, after) = match op {
      VolumeOp::Unlink { path } => {
        let before = held(path, self.changed.get(path));
        let removal = self.removal(base, path)?;
        (before, Self::held_after_removal(path, removal, before))
      }
      VolumeOp::Rename { from, to } if from != to => {
        let Some(file) = self.moved(base, from)? else {
          return Ok((0, 0));
        };
        let held_from = held(from, self.changed.get(from));
        let before = held_from.saturating_add(held(to, self.changed.get(to)));
        let removal = self.removal(base, from)?;
        let after =
          Self::held_after_removal(from, removal, held_from).saturating_add(file.footprint(to));
        (before, after)
      }
      _ => (0, 0),
    };
    Ok((after.saturating_sub(before), before.saturating_sub(after)))
  }

  /// Applies a namespace declaration to the work's view: an unlink removes the file, a rename moves it (replacing
  /// what `to` named), its pieces moving with it, so no bytes are copied. Other declarations change no file.
  pub fn declare(
    &mut self,
    base: Base<'_>,
    op: &slates_merge::increment::VolumeOp,
  ) -> Result<(), BaseGone> {
    use slates_merge::increment::VolumeOp;
    match op {
      VolumeOp::Unlink { path } => {
        let removal = self.removal(base, path)?;
        self.remove_as(path, removal);
      }
      VolumeOp::Rename { from, to } if from != to => {
        let Some(file) = self.moved(base, from)? else {
          return Ok(());
        };
        let removal = self.removal(base, from)?;
        self.remove_as(from, removal);
        self.changed.insert(to.clone(), Some(file));
      }
      _ => {}
    }
    Ok(())
  }

  /// Whether the work holds its own entry at `path`, changed or removed, so no verb needs the base's file there to
  /// edit it (a read of a changed file may still need the base's ranges its pieces name, [`WorkFiles::base_span`]).
  pub fn holds(&self, path: &str) -> bool {
    self.changed.contains_key(path)
  }

  /// A work holding `changed`, each file as its parts (A-806): what a rebase answers and a restatement logs, so the work
  /// holds, and its log carries, what it wrote and the pieces naming its base, never the file.
  pub fn from_parts(changed: BTreeMap<String, Option<HeldParts>>) -> WorkFiles {
    WorkFiles {
      changed: changed
        .into_iter()
        .map(|(path, parts)| (path, parts.map(Held::from_parts)))
        .collect(),
    }
  }

  /// What the work holds of its own, each file as its parts (A-806), in path order.
  pub fn parts(&self) -> BTreeMap<String, Option<HeldParts>> {
    self
      .changed
      .iter()
      .map(|(path, held)| (path.clone(), held.as_ref().map(Held::parts)))
      .collect()
  }

  /// A work's delta as a clean rebase leaves it (`Rebased::changed`), or as a restatement in its log replays it
  /// (A-207): per path its bytes where they differ from the base, `None` where it removes the base's file.
  pub fn from_changed(changed: BTreeMap<String, Option<Vec<u8>>>) -> WorkFiles {
    WorkFiles {
      changed: changed
        .into_iter()
        .map(|(path, bytes)| (path, bytes.map(Held::whole)))
        .collect(),
    }
  }

  /// What the work holds of its own, in path order, each file's bytes put together from its pieces (A-207: what a
  /// restatement in its log carries; a restatement follows a rebase, whose files are held whole, so they are lent).
  pub fn changed_entries<'a>(&'a self, base: Base<'a>) -> Result<Vec<ChangedEntry<'a>>, BaseGone> {
    self
      .changed
      .keys()
      .map(|path| Ok((path.as_str(), self.get(base, path)?)))
      .collect()
  }

  /// Whether the work holds nothing of its own: it equals its base.
  pub fn is_empty(&self) -> bool {
    self.changed.is_empty()
  }

  /// Forgets every change: the work now equals its (new) base.
  pub fn clear(&mut self) {
    self.changed.clear();
  }

  /// Removes and drops one changed file entry, returning `true` while entries remain (used to
  /// release large work sets cooperatively across shard turns after acceptance).
  pub fn pop_one(&mut self) -> bool {
    self.changed.pop_first().is_some()
  }
}

#[cfg(test)]
mod tests {
  #![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
  )]
  use super::*;

  /// The bytes `edits` (offset share, length, kind: overwrite, insert or delete) make of `base`.
  fn edited(base: &[u8], edits: &[(u16, u8, u8)]) -> Vec<u8> {
    let mut bytes = base.to_vec();
    for (share, len, kind) in edits {
      let at = if bytes.is_empty() {
        0
      } else {
        usize::from(*share) % (bytes.len() + 1)
      };
      let len = usize::from(*len % 32);
      match kind % 3 {
        0 => {
          for (offset, byte) in bytes.iter_mut().skip(at).take(len).enumerate() {
            *byte = byte
              .wrapping_add(1)
              .wrapping_add(u8::try_from(offset % 7).unwrap());
          }
        }
        1 => {
          let insert: Vec<u8> = (0..len).map(|n| u8::try_from(n % 251).unwrap()).collect();
          bytes.splice(at..at, insert);
        }
        _ => {
          let end = (at + len).min(bytes.len());
          bytes.drain(at..end);
        }
      }
    }
    bytes
  }

  proptest::proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::prelude::ProptestConfig::default()))]
    /// A-806 (the oracle of a rebased file's parts). Do: from a generated base, make a file by generated overwrites,
    /// inserts and deletes, and take its parts over the base (`parts_over_base`). Expect: the parts read back as the
    /// file exactly, never write more than the file, name the base only through the file's own path, and a file the
    /// edits left unchanged writes nothing.
    #[test]
    fn a_rebased_files_parts_read_back_as_the_file(
      base in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..2048),
      edits in proptest::collection::vec((proptest::prelude::any::<u16>(), proptest::prelude::any::<u8>(), proptest::prelude::any::<u8>()), 0..6),
    ) {
      let bytes = edited(&base, &edits);
      let parts = parts_over_base("f", Some(&base), bytes.clone());
      proptest::prop_assert_eq!(parts.materialize(&base), Some(bytes.clone()));
      proptest::prop_assert!(parts.written.len() <= bytes.len());
      proptest::prop_assert!(parts.origin.as_deref().is_none_or(|origin| origin == "f"));
      if bytes == base {
        proptest::prop_assert!(parts.written.is_empty());
      }
    }
  }

  /// A-806. Do: overwrite 64 bytes in the middle of a 1 MiB file and take its parts over the file. Expect: 64 bytes
  /// written and three pieces (base, written, base), reading back as the file; and a file with no base written whole.
  #[test]
  fn an_overwrite_of_a_large_file_is_its_written_bytes_over_the_base() {
    let base: Vec<u8> = (0..1usize << 20)
      .map(|n| u8::try_from(n % 251).unwrap())
      .collect();
    let mut bytes = base.clone();
    bytes[1 << 19..(1 << 19) + 64].fill(b'w');
    let parts = parts_over_base("big", Some(&base), bytes.clone());
    assert_eq!((parts.written.len(), parts.pieces.len()), (64, 3));
    assert_eq!(parts.materialize(&base), Some(bytes));
    let fresh = parts_over_base("new", None, b"fresh".to_vec());
    assert_eq!((fresh.written, fresh.origin), (b"fresh".to_vec(), None));
  }
  use slates_merge::origin::Origin;

  /// Shape: a base file far larger than any edit, so a copy of it would dwarf what an edit holds.
  const BIG: usize = 1 << 20;

  fn green_with(path: &str, bytes: Vec<u8>) -> Green {
    Green::with_origin(Origin {
      files: [(path.to_owned(), bytes)].into_iter().collect(),
      ..Origin::default()
    })
  }

  fn pattern(len: usize) -> Vec<u8> {
    (0..len)
      .map(|index| u8::try_from(index % 251).unwrap())
      .collect()
  }

  /// §4.16, D-5: do overwrite one byte in the middle of a 1 MiB base file; expect the work to hold that byte and three
  /// pieces (its charge is the path, the origin's name, one byte and three pieces, nothing of the file), and to read the
  /// file as the base with that byte changed.
  #[test]
  fn a_one_byte_edit_of_a_base_file_holds_the_byte_and_three_pieces() {
    let green = green_with("big", pattern(BIG));
    let base = Base {
      green: Some(&green),
      fetched: None,
      version: 0,
      entries: None,
      plane: None,
    };
    let mut files = WorkFiles::default();
    let at = u64::try_from(BIG / 2).unwrap();
    let (added, _) = files.splice_delta("big", files.len(base, "big").unwrap(), (at, 1, 1));
    files.splice(base, "big", (at, 1), b"w").unwrap();
    let expected = crate::work_charge::file_footprint("big", "big".len() + 1 + 3 * PIECE_BYTES);
    assert_eq!(files.footprint(), expected);
    assert_eq!(added, expected, "the charge foreseen is the charge held");
    let mut want = pattern(BIG);
    want[BIG / 2] = b'w';
    assert_eq!(
      files.get(base, "big").unwrap().unwrap().as_ref(),
      want.as_slice()
    );
    let (page, total) = files.page(base, "big", (at - 2, 5)).unwrap().unwrap();
    assert_eq!(total, u64::try_from(BIG).unwrap());
    assert_eq!(page, want[BIG / 2 - 2..BIG / 2 + 3].to_vec());
  }

  /// A file rewritten whole again and again holds at most twice what it shows: do overwrite a 4 KiB file's every byte
  /// 100 times; expect its written bytes to stay within twice the file, and the file to read as the last write.
  #[test]
  fn a_file_rewritten_again_and_again_holds_at_most_twice_its_bytes() {
    let green = green_with("f", pattern(4096));
    let base = Base {
      green: Some(&green),
      fetched: None,
      version: 0,
      entries: None,
      plane: None,
    };
    let mut files = WorkFiles::default();
    let mut last = Vec::new();
    for round in 0..100u8 {
      last = vec![round; 4096];
      files.splice(base, "f", (0, 4096), &last).unwrap();
      let held = files.changed["f"].as_ref().unwrap();
      assert!(
        held.written.len() <= 2 * 4096,
        "round {round}: {} written",
        held.written.len()
      );
    }
    assert_eq!(
      files.get(base, "f").unwrap().unwrap().as_ref(),
      last.as_slice()
    );
  }

  /// A rename moves a file's pieces, never its bytes: do edit a base file, rename it; expect the new name to read the
  /// edited file from the old name's base bytes, the old name gone, and an off-shard read's base span named by the old
  /// name in the base's own offsets.
  #[test]
  fn a_rename_moves_the_pieces_and_a_read_names_its_base_span() {
    let green = green_with("old", pattern(BIG));
    let base = Base {
      green: Some(&green),
      fetched: None,
      version: 0,
      entries: None,
      plane: None,
    };
    let mut files = WorkFiles::default();
    files.splice(base, "old", (10, 0), b"INSERT").unwrap();
    let rename = slates_merge::increment::VolumeOp::Rename {
      from: "old".to_owned(),
      to: "new".to_owned(),
    };
    files.declare(base, &rename).unwrap();
    assert_eq!(files.get(base, "old").unwrap(), None);
    let mut want = pattern(BIG);
    want.splice(10..10, b"INSERT".iter().copied());
    assert_eq!(
      files.get(base, "new").unwrap().unwrap().as_ref(),
      want.as_slice()
    );
    assert_eq!(
      files.base_span("new", (12, 10)),
      Some(("old".to_owned(), 10, 6))
    );
    assert_eq!(
      files.base_span("new", (11, 4)),
      None,
      "inside the insert, nothing of the base"
    );
  }
}

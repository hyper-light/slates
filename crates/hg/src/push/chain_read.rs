//! A manifest the fork holds read in ranges, never whole (goal 93, C1 (2); main's ruling (B) of 2026-11-24: a push's
//! manifest made a window a step over its base read so).
//!
//! **How.** A manifest row is kept as a delta against the row before it, over a chain that rests on a whole text (A-729):
//! the chain's deltas are loaded (each a stored object; together at most the text's own length, the chain's bound) and
//! folded, lowest first, into one list of pieces over the text the chain makes ([`Piece`], as `mpatch`'s fold folds a
//! chain into one patch): each piece a range of the whole text the chain rests on, read through the content store's
//! windows ([`ContentStore::run_at`]), or a range of a delta's own bytes. A read finds its first piece by the pieces'
//! starts and copies from there.
//!
//! **Bounded, and flat.** The deltas' bytes, which a caller counts as text it holds, and the pieces, at most two a hunk
//! and one more: both the chain's bytes' order, kept on the heap in room the push hands from chain to chain. Nothing
//! recurses. Until 2026-11-25 the text was made level by level, a delta's patching reading the level below it, one stack
//! frame a delta: a read of a line no delta touched went through every delta to the whole text, and a 2,000-changeset
//! push overflowed a debug shard's 2 MiB stack (main's order; `a_deep_chain_is_read_on_a_shards_stack`: 10,000 one-line
//! deltas aborted a 2 MiB thread).

use slates_vcs::{ContentId, ContentStore};

use crate::Node;
use crate::delta::BaseRead;
use crate::index::{HgIndex, Log, NONE};
use crate::read::to_u64;

/// A manifest's chain, loaded: the whole text it rests on and each delta above it, lowest first. The deltas lie end to
/// end in room the push hands from chain to chain ([`Chain::into_room`]), so loading a chain allocates nothing once the
/// push has some (main's churn of hg's 57k push at e51d998bf: 2.49 GB of `object_bytes` under `Chain::of`, a new buffer
/// a delta a revision; bisected to b47217878: 17,899 B to 136,995 B an edit at a 91,200 B manifest).
#[derive(Debug)]
pub(crate) struct Chain {
  /// The whole text the chain rests on: its object and length.
  root: (ContentId, u64),
  /// Its deltas, in room a chain let go before kept.
  room: ChainRoom,
}

/// The room a [`Chain`]'s deltas are loaded into, handed from chain to chain ([`Chain::into_room`]).
#[derive(Debug, Default)]
pub(crate) struct ChainRoom {
  /// The deltas' bytes, end to end, in the order they were read (the topmost first).
  bytes: Vec<u8>,
  /// Each delta above the root, lowest first: its range in `bytes`, with the length of the text it makes.
  deltas: Vec<(core::ops::Range<usize>, u64)>,
  /// The text the chain makes, as pieces in its order ([`Chain::fold`]).
  pieces: Vec<Piece>,
  /// The pieces a fold step makes, before they take the list's place.
  folding: Vec<Piece>,
}

impl ChainRoom {
  /// Emptied for the next chain, its room kept.
  fn cleared(mut self) -> ChainRoom {
    self.bytes.clear();
    self.deltas.clear();
    self.pieces.clear();
    self.folding.clear();
    self
  }

  /// The object of `row`, a delta of the chain, read onto the end: none where it is not held whole.
  fn push(&mut self, store: &ContentStore, row: &crate::index::RevisionRow) -> Option<()> {
    let range = read_object(store, (&row.bytes, row.stored), &mut self.bytes)?;
    self.deltas.push((range, row.len));
    Some(())
  }
}

impl Chain {
  /// The chain of the manifest row `rev` of `index`: none where a row or a delta's object is not held whole, or the
  /// chain does not end at a whole text. Its deltas are read into `room` (a chain let go before's, [`Chain::into_room`]).
  pub(crate) fn of(
    index: &HgIndex,
    store: &ContentStore,
    rev: u32,
    room: ChainRoom,
  ) -> Option<Chain> {
    let mut room = room.cleared();
    let mut at = rev;
    loop {
      let row = index.revision(store, Log::Manifest, at)?;
      if row.base == NONE {
        room.deltas.reverse();
        return Chain {
          root: (row.bytes, row.len),
          room,
        }
        .folded();
      }
      room.push(store, &row)?;
      if row.base >= at {
        return None;
      }
      at = row.base;
    }
  }

  /// The chain of the manifest row `rev` of `index` read over `root`, the whole text (its object and length) of revision
  /// `from` of its chain, given by the changeset store where that one was let go once landed (w-interop's matrix at
  /// 29221d5ab: a merge's manifest read over one the same push kept, a delta on the let-go tip's): the rows above `from`
  /// its deltas, read into `room`. None where a row or a delta's object above it is not held whole, or `from` is not in
  /// its chain.
  pub(crate) fn over(
    index: &HgIndex,
    store: &ContentStore,
    (from, root): (u32, (ContentId, u64)),
    (rev, room): (u32, ChainRoom),
  ) -> Option<Chain> {
    let mut room = room.cleared();
    let mut at = rev;
    while at != from {
      let row = index.revision(store, Log::Manifest, at)?;
      if row.base == NONE || row.base >= at {
        return None;
      }
      room.push(store, &row)?;
      at = row.base;
    }
    room.deltas.reverse();
    Chain { root, room }.folded()
  }

  /// The topmost revision of manifest row `rev`'s chain (`rev` itself first) whose object `store` does not hold whole: what
  /// the changeset store is asked for where the chain is not at hand, a landed revision let go; none where every link is
  /// held whole.
  pub(crate) fn let_go_link(
    index: &HgIndex,
    store: &ContentStore,
    rev: u32,
  ) -> Option<(u32, Node)> {
    let mut at = rev;
    loop {
      let row = index.revision(store, Log::Manifest, at)?;
      if !store.holds_whole(&row.bytes) {
        return Some((at, row.node));
      }
      if row.base == NONE || row.base >= at {
        return None;
      }
      at = row.base;
    }
  }

  /// A chain over the whole text `root` (its object in `store` and its length) of `deltas`, lowest first, each its own
  /// text's length worked out from the one below: a test's chain, made with no index.
  #[cfg(test)]
  pub(crate) fn of_deltas<'d>(
    store: &ContentStore,
    root: (ContentId, u64),
    deltas: impl Iterator<Item = &'d [u8]>,
  ) -> Option<Chain> {
    let _ = store;
    let mut room = ChainRoom::default();
    let mut len = root.1;
    for delta in deltas {
      len = crate::delta::patched_len(len, delta).ok()?;
      let start = room.bytes.len();
      room.bytes.extend_from_slice(delta);
      room.deltas.push((start..room.bytes.len(), len));
    }
    Chain { root, room }.folded()
  }

  /// The chain of a text held whole under `object`, `len` bytes long: no delta.
  pub(crate) fn whole(object: ContentId, len: u64) -> Chain {
    let mut room = ChainRoom::default();
    if len > 0 {
      room.pieces.push(Piece {
        at: 0,
        len,
        from: From::Root(0),
      });
    }
    Chain {
      root: (object, len),
      room,
    }
  }

  /// The object of the whole text it rests on.
  pub(crate) fn root_object(&self) -> ContentId {
    self.root.0
  }

  /// The text's length.
  pub(crate) fn len(&self) -> u64 {
    self.room.deltas.last().map_or(self.root.1, |(_, len)| *len)
  }

  /// The delta bytes and the pieces it holds loaded: what a reader of it counts as held.
  pub(crate) fn held(&self) -> u64 {
    to_u64(self.room.bytes.len()).saturating_add(to_u64(
      self
        .room
        .pieces
        .len()
        .saturating_mul(core::mem::size_of::<Piece>()),
    ))
  }

  /// The chain with its deltas folded into its pieces, lowest first ([`Piece`]): each delta's hunks laid over the pieces
  /// the deltas below made, the bytes before a hunk and after the last kept, those it replaces dropped, its data a piece.
  /// None where a delta's hunks are out of order, run past the text below, or make another length than its row says.
  fn folded(mut self) -> Option<Chain> {
    let room = &mut self.room;
    room.pieces.clear();
    if self.root.1 > 0 {
      room.pieces.push(Piece {
        at: 0,
        len: self.root.1,
        from: From::Root(0),
      });
    }
    for (range, len) in &room.deltas {
      let delta = room.bytes.get(range.clone())?;
      room.folding.clear();
      fold_one((&room.pieces, delta, range.start), &mut room.folding)?;
      if room.folding.last().map_or(0, Piece::end) != *len {
        return None;
      }
      core::mem::swap(&mut room.pieces, &mut room.folding);
    }
    room.folding.clear();
    Some(self)
  }

  /// Its deltas' room, for the next chain loaded ([`Chain::of`]).
  pub(crate) fn into_room(self) -> ChainRoom {
    self.room
  }
}

/// The object `root` names, `len` bytes, read through its windows onto the end of `buffer`: its range there; none where
/// the store holds another length (a window let go), what was read of it dropped again.
fn read_object(
  store: &ContentStore,
  (root, len): (&ContentId, u64),
  buffer: &mut Vec<u8>,
) -> Option<core::ops::Range<usize>> {
  let start = buffer.len();
  for window in store.windows(root) {
    buffer.extend_from_slice(&window);
  }
  if to_u64(buffer.len().saturating_sub(start)) != len {
    buffer.truncate(start);
    return None;
  }
  Some(start..buffer.len())
}

/// Where a piece of a chain's text comes from: the whole text the chain rests on, from an offset in it; or the chain's
/// delta bytes, from an offset in them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum From {
  /// The whole text, from this offset.
  Root(u64),
  /// The deltas' bytes, from this offset.
  Bytes(usize),
}

/// A piece of a chain's text: where it starts in the text, its length, and where its bytes come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
  /// Its first byte's place in the text.
  at: u64,
  /// Its bytes.
  len: u64,
  /// Where they come from.
  from: From,
}

impl Piece {
  /// One past its last byte's place in the text.
  fn end(&self) -> u64 {
    self.at.saturating_add(self.len)
  }

  /// Its bytes from `skip` on, `len` of them, placed at `at`.
  fn part(&self, (skip, len): (u64, u64), at: u64) -> Option<Piece> {
    let from = match self.from {
      From::Root(offset) => From::Root(offset.checked_add(skip)?),
      From::Bytes(offset) => From::Bytes(offset.checked_add(usize::try_from(skip).ok()?)?),
    };
    Some(Piece { at, len, from })
  }
}

/// Format: an encoded hunk's header, its start, end and length, four bytes each (`mpatch`).
const HUNK_HEADER: usize = 12;

/// One delta (`delta`, at `offset` in the chain's bytes) laid over `below`, the pieces of the text it is a delta
/// against: the pieces of the text it makes, onto `out`. None where its hunks are out of order or run past the text.
fn fold_one((below, delta, offset): (&[Piece], &[u8], usize), out: &mut Vec<Piece>) -> Option<()> {
  let mut taking = Taking {
    below,
    at: 0,
    made: 0,
  };
  let mut data_at = offset;
  for hunk in crate::delta::hunks(delta) {
    let hunk = hunk.ok()?;
    let (start, end) = (u64::from(hunk.start), u64::from(hunk.end));
    taking.copy_to(start, out)?;
    taking.skip_to(end)?;
    data_at = data_at.checked_add(HUNK_HEADER)?;
    let len = to_u64(hunk.data.len());
    if len > 0 {
      out.push(Piece {
        at: taking.made,
        len,
        from: From::Bytes(data_at),
      });
      taking.made = taking.made.checked_add(len)?;
    }
    data_at = data_at.checked_add(hunk.data.len())?;
  }
  let end = below.last().map_or(0, Piece::end);
  taking.copy_to(end, out)
}

/// The pieces of a text below a delta taken in order: how far into it the fold has come, and the text made so far.
struct Taking<'p> {
  /// The text below's pieces.
  below: &'p [Piece],
  /// The place in the text below the fold has come to.
  at: u64,
  /// The bytes of the text made so far.
  made: u64,
}

impl Taking<'_> {
  /// The text below from where the fold is to `to`, onto `out` as pieces of the text made: none where `to` is behind.
  fn copy_to(&mut self, to: u64, out: &mut Vec<Piece>) -> Option<()> {
    if to < self.at {
      return None;
    }
    let first = self.below.partition_point(|piece| piece.end() <= self.at);
    for piece in self.below.get(first..)? {
      if piece.at >= to {
        break;
      }
      let from = self.at.max(piece.at);
      let until = to.min(piece.end());
      let len = until.checked_sub(from)?;
      out.push(piece.part((from.checked_sub(piece.at)?, len), self.made)?);
      self.made = self.made.checked_add(len)?;
      self.at = until;
    }
    (self.at == to).then_some(())
  }

  /// The text below from where the fold is to `to` passed over, a hunk's replaced bytes: none where `to` is behind
  /// or past the text.
  fn skip_to(&mut self, to: u64) -> Option<()> {
    let end = self.below.last().map_or(0, Piece::end);
    if to < self.at || to > end {
      return None;
    }
    self.at = to;
    Some(())
  }
}

/// A reader of a [`Chain`]'s text: the piece its last read ended in, where a reader that reads forward begins its next.
#[derive(Clone, Debug, Default)]
pub(crate) struct Cursor {
  /// That piece's number.
  piece: usize,
}

impl Cursor {
  /// The cursor begun again for another chain.
  pub(crate) fn reset(&mut self) {
    self.piece = 0;
  }
}

/// A [`Cursor`] lent its chain and store: a [`BaseRead`].
pub(crate) struct Reading<'c> {
  /// The chain.
  pub(crate) chain: &'c Chain,
  /// The cursor.
  pub(crate) cursor: &'c mut Cursor,
  /// The content store holding the chain's whole text.
  pub(crate) store: &'c ContentStore,
}

impl BaseRead for Reading<'_> {
  fn len(&self) -> u64 {
    self.chain.len()
  }

  fn read_into(&mut self, at: u64, n: usize, out: &mut Vec<u8>) -> Option<()> {
    let before = out.len();
    let read = self.read_pieces(at, n, out);
    if read.is_none() {
      out.truncate(before);
    }
    read
  }
}

impl Reading<'_> {
  /// `n` bytes of the text from `at` onto `out`, piece by piece from the one holding `at` ([`Reading::first_piece`]):
  /// none past the text, or where the store does not hold a window.
  fn read_pieces(&mut self, at: u64, n: usize, out: &mut Vec<u8>) -> Option<()> {
    let end = at.checked_add(to_u64(n))?;
    if end > self.chain.len() {
      return None;
    }
    let mut number = self.first_piece(at);
    let mut at = at;
    while at < end {
      let piece = self.chain.room.pieces.get(number)?;
      if piece.end() <= at {
        number = number.checked_add(1)?;
        continue;
      }
      at = at.checked_add(self.read_from_piece(piece, (at, end), out)?)?;
      self.cursor.piece = number;
    }
    Some(())
  }

  /// The piece a read from `at` begins its search at: the cursor's where it starts at or before `at`, else the first
  /// whose end passes `at`, found by the pieces' starts.
  fn first_piece(&self, at: u64) -> usize {
    let pieces = &self.chain.room.pieces;
    match pieces.get(self.cursor.piece) {
      Some(piece) if piece.at <= at => self.cursor.piece,
      _ => pieces.partition_point(|piece| piece.end() <= at),
    }
  }

  /// The bytes of `piece` from `at` up to its end or `end`, whichever comes first, onto `out`, from the root's windows or
  /// the chain's own bytes as the piece is from: how many, none where the store does not hold a window.
  fn read_from_piece(
    &self,
    piece: &Piece,
    (at, end): (u64, u64),
    out: &mut Vec<u8>,
  ) -> Option<u64> {
    let skip = at.checked_sub(piece.at)?;
    let len = end.min(piece.end()).checked_sub(at)?;
    match piece.from {
      From::Root(offset) => read_root(
        self.store,
        self.chain.root,
        (offset.checked_add(skip)?, usize::try_from(len).ok()?),
        out,
      )?,
      From::Bytes(offset) => {
        let from = offset.checked_add(usize::try_from(skip).ok()?)?;
        let until = from.checked_add(usize::try_from(len).ok()?)?;
        out.extend_from_slice(self.chain.room.bytes.get(from..until)?);
      }
    }
    Some(len)
  }
}

/// Reads `n` bytes from `at` of the whole text `root` (its object and length) through the store's windows.
fn read_root(
  store: &ContentStore,
  (object, len): (ContentId, u64),
  (at, n): (u64, usize),
  out: &mut Vec<u8>,
) -> Option<()> {
  let end = at.checked_add(to_u64(n))?;
  if end > len {
    return None;
  }
  let mut at = at;
  while at < end {
    let run = store.run_at(&object, len, at)?;
    let take = usize::try_from(end.saturating_sub(at)).ok()?.min(run.len());
    if take == 0 {
      return None;
    }
    out.extend_from_slice(run.get(..take)?);
    at = at.saturating_add(to_u64(take));
  }
  Some(())
}

#[cfg(test)]
#[path = "chain_read_tests.rs"]
mod tests;

//! A manifest the fork holds read front to back in ranges, never whole (goal 93, C1 (2); main's ruling (B) of
//! 2026-11-24: a push's manifest made a window a step over its base read so).
//!
//! **How.** A manifest row is kept as a delta against the row before it, over a chain that rests on a whole text (A-729):
//! the chain's deltas are loaded (each a stored object; together at most the text's own length, the chain's bound), and
//! the text is made level by level, each level's [`delta::Patching`] reading the level below it, the lowest the whole
//! text the chain rests on, read through the content store's windows ([`ContentStore::run_at`]). Each level keeps where
//! it is, so a reader that reads forward (a delta's copies, a hash of the whole) walks each level once. A read behind
//! where a level stands is not served.
//!
//! **Bounded.** The chain's deltas, which a caller counts as text it holds, and a level's state each, fixed in size.

use slates_vcs::{ContentId, ContentStore};

use crate::Node;
use crate::delta::{BaseRead, Filled, Patching};
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
}

impl ChainRoom {
  /// Emptied for the next chain, its room kept.
  fn cleared(mut self) -> ChainRoom {
    self.bytes.clear();
    self.deltas.clear();
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
        return Some(Chain {
          root: (row.bytes, row.len),
          room,
        });
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
    Some(Chain { root, room })
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

  /// The chain of a text held whole under `object`, `len` bytes long: no delta.
  pub(crate) fn whole(object: ContentId, len: u64) -> Chain {
    Chain {
      root: (object, len),
      room: ChainRoom::default(),
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

  /// The delta bytes it holds loaded: what a reader of it counts as held.
  pub(crate) fn held(&self) -> u64 {
    to_u64(self.room.bytes.len())
  }

  /// The number of its deltas.
  fn depth(&self) -> usize {
    self.room.deltas.len()
  }

  /// Its `k`th delta above the root (0 the lowest): its bytes and the length of the text it makes.
  fn delta(&self, k: usize) -> Option<(&[u8], u64)> {
    let (range, len) = self.room.deltas.get(k)?;
    Some((self.room.bytes.get(range.clone())?, *len))
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

/// Where one level of a chain's text stands: its delta applied so far and the bytes it has made.
#[derive(Clone, Copy, Debug, Default)]
struct Level {
  /// Its delta, applied so far.
  patching: Patching,
  /// The bytes of its text made so far.
  made: u64,
}

/// A reader of a [`Chain`]'s text, front to back: each level where it stands, and the buffer each level makes the bytes
/// it skips into (dropped once made), kept across reads so a skip allocates nothing once a level has one (main's churn
/// of hg's 57k push at e51d998bf: `read_level`'s skips and copies 1.5 GB, a new buffer a call at every level).
#[derive(Clone, Debug, Default)]
pub(crate) struct Cursor {
  /// Each delta's level, lowest first.
  levels: Vec<Level>,
  /// Each level's buffer for the bytes it skips, lowest first.
  skipped: Vec<Vec<u8>>,
}

impl Cursor {
  /// The cursor begun again for another chain, its buffers kept: its levels taken at its next read.
  pub(crate) fn reset(&mut self) {
    self.levels.clear();
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
    let top = self.chain.depth();
    // A cursor begun empty takes a level for each of its chain's deltas at its first read, in the room it kept.
    if self.cursor.levels.len() != top {
      self.cursor.levels.clear();
      self.cursor.levels.resize(top, Level::default());
    }
    if self.cursor.skipped.len() < top {
      self.cursor.skipped.resize_with(top, Vec::new);
    }
    let read = read_level(
      (self.chain, self.store),
      (&mut self.cursor.levels, &mut self.cursor.skipped),
      top,
      (at, n),
      out,
    );
    if read.is_none() {
      out.truncate(before);
    }
    read
  }
}

/// Reads `n` bytes from `at` of the text of level `level` of `chain` (0 the whole text it rests on, `k` the text its
/// `k`th delta makes) into `out`, `levels` the cursor's levels below and at it: a level behind `at` makes and drops the
/// bytes before it first. None where the store does not hold a window, or `at` is behind where the level stands.
fn read_level(
  (chain, store): (&Chain, &ContentStore),
  (levels, skipped): (&mut [Level], &mut [Vec<u8>]),
  level: usize,
  (at, n): (u64, usize),
  out: &mut Vec<u8>,
) -> Option<()> {
  let Some(above_root) = level.checked_sub(1) else {
    return read_root(store, chain.root, (at, n), out);
  };
  let (below, here) = levels.split_at_mut(above_root);
  let here = here.first_mut()?;
  let (skipped_below, dropped) = skipped.split_at_mut(above_root);
  let dropped = dropped.first_mut()?;
  let (delta, _) = chain.delta(above_root)?;
  if at < here.made {
    return None;
  }
  let mut lower = Lower {
    chain,
    store,
    levels: below,
    skipped: skipped_below,
    level: above_root,
  };
  // The bytes before `at`, made and dropped, at most `n` a fill, so nothing past what is asked is held.
  while here.made < at {
    let gap = usize::try_from(at.saturating_sub(here.made))
      .unwrap_or(usize::MAX)
      .min(n.max(1));
    dropped.clear();
    let filled = here.patching.fill((delta, &mut lower), gap, dropped).ok()?;
    here.made = here.made.saturating_add(to_u64(dropped.len()));
    match filled {
      Filled::More | Filled::Done if dropped.is_empty() => return None,
      Filled::Unread { .. } => return None,
      _ => {}
    }
  }
  let mut wanted = n;
  while wanted > 0 {
    let before = out.len();
    let filled = here.patching.fill((delta, &mut lower), wanted, out).ok()?;
    let got = out.len().saturating_sub(before);
    here.made = here.made.saturating_add(to_u64(got));
    wanted = wanted.saturating_sub(got);
    match filled {
      Filled::Unread { .. } => return None,
      Filled::Done if wanted > 0 => return None,
      _ => {}
    }
  }
  Some(())
}

/// The levels below one, as that level's base ([`BaseRead`]).
struct Lower<'l> {
  /// The chain.
  chain: &'l Chain,
  /// The content store.
  store: &'l ContentStore,
  /// The cursor's levels below.
  levels: &'l mut [Level],
  /// Their buffers for the bytes they skip.
  skipped: &'l mut [Vec<u8>],
  /// The level this one reads: 0 the whole text.
  level: usize,
}

impl BaseRead for Lower<'_> {
  fn len(&self) -> u64 {
    match self.level.checked_sub(1) {
      None => self.chain.root.1,
      Some(delta) => self.chain.delta(delta).map_or(0, |(_, len)| len),
    }
  }

  fn read_into(&mut self, at: u64, n: usize, out: &mut Vec<u8>) -> Option<()> {
    read_level(
      (self.chain, self.store),
      (self.levels, self.skipped),
      self.level,
      (at, n),
      out,
    )
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

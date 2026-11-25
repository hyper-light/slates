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

/// A manifest's chain, loaded: the whole text it rests on and each delta above it, lowest first.
#[derive(Debug)]
pub(crate) struct Chain {
  /// The whole text the chain rests on: its object and length.
  root: (ContentId, u64),
  /// Each delta above it, lowest first, with the length of the text it makes.
  deltas: Vec<(Vec<u8>, u64)>,
}

impl Chain {
  /// The chain of the manifest row `rev` of `index`: none where a row or a delta's object is not held whole, or the
  /// chain does not end at a whole text.
  pub(crate) fn of(index: &HgIndex, store: &ContentStore, rev: u32) -> Option<Chain> {
    let mut deltas = Vec::new();
    let mut at = rev;
    loop {
      let row = index.revision(store, Log::Manifest, at)?;
      if row.base == NONE {
        deltas.reverse();
        return Some(Chain {
          root: (row.bytes, row.len),
          deltas,
        });
      }
      deltas.push((store.object_bytes(&row.bytes, row.stored)?, row.len));
      if row.base >= at {
        return None;
      }
      at = row.base;
    }
  }

  /// The chain of the manifest row `rev` of `index` read over `root`, the whole text (its object and length) of revision
  /// `from` of its chain, given by the changeset store where that one was let go once landed (w-interop's matrix at
  /// 29221d5ab: a merge's manifest read over one the same push kept, a delta on the let-go tip's): the rows above `from`
  /// its deltas. None where a row or a delta's object above it is not held whole, or `from` is not in its chain.
  pub(crate) fn over(
    index: &HgIndex,
    store: &ContentStore,
    (from, root): (u32, (ContentId, u64)),
    rev: u32,
  ) -> Option<Chain> {
    let mut deltas = Vec::new();
    let mut at = rev;
    while at != from {
      let row = index.revision(store, Log::Manifest, at)?;
      if row.base == NONE || row.base >= at {
        return None;
      }
      deltas.push((store.object_bytes(&row.bytes, row.stored)?, row.len));
      at = row.base;
    }
    deltas.reverse();
    Some(Chain { root, deltas })
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
      deltas: Vec::new(),
    }
  }

  /// The text's length.
  pub(crate) fn len(&self) -> u64 {
    self.deltas.last().map_or(self.root.1, |(_, len)| *len)
  }

  /// The delta bytes it holds loaded: what a reader of it counts as held.
  pub(crate) fn held(&self) -> u64 {
    self.deltas.iter().fold(0, |held, (delta, _)| {
      held.saturating_add(to_u64(delta.len()))
    })
  }
}

/// Where one level of a chain's text stands: its delta applied so far and the bytes it has made.
#[derive(Clone, Copy, Debug, Default)]
struct Level {
  /// Its delta, applied so far.
  patching: Patching,
  /// The bytes of its text made so far.
  made: u64,
}

/// A reader of a [`Chain`]'s text, front to back: each level where it stands.
#[derive(Clone, Debug, Default)]
pub(crate) struct Cursor {
  /// Each delta's level, lowest first.
  levels: Vec<Level>,
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
    let top = self.chain.deltas.len();
    // A cursor begun empty takes a level for each of its chain's deltas at its first read.
    if self.cursor.levels.len() != top {
      self.cursor.levels = vec![Level::default(); top];
    }
    let read = read_level(
      (self.chain, self.store),
      &mut self.cursor.levels,
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
  levels: &mut [Level],
  level: usize,
  (at, n): (u64, usize),
  out: &mut Vec<u8>,
) -> Option<()> {
  let Some(above_root) = level.checked_sub(1) else {
    return read_root(store, chain.root, (at, n), out);
  };
  let (below, here) = levels.split_at_mut(above_root);
  let here = here.first_mut()?;
  let (delta, _) = chain.deltas.get(above_root)?;
  if at < here.made {
    return None;
  }
  let mut lower = Lower {
    chain,
    store,
    levels: below,
    level: above_root,
  };
  // The bytes before `at`, made and dropped, at most `n` a fill, so nothing past what is asked is held.
  let mut dropped = Vec::new();
  while here.made < at {
    let gap = usize::try_from(at.saturating_sub(here.made))
      .unwrap_or(usize::MAX)
      .min(n.max(1));
    dropped.clear();
    let filled = here
      .patching
      .fill((delta, &mut lower), gap, &mut dropped)
      .ok()?;
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
  /// The level this one reads: 0 the whole text.
  level: usize,
}

impl BaseRead for Lower<'_> {
  fn len(&self) -> u64 {
    match self.level.checked_sub(1) {
      None => self.chain.root.1,
      Some(delta) => self.chain.deltas.get(delta).map_or(0, |(_, len)| *len),
    }
  }

  fn read_into(&mut self, at: u64, n: usize, out: &mut Vec<u8>) -> Option<()> {
    read_level(
      (self.chain, self.store),
      self.levels,
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

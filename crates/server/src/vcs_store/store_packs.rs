//! A git pack read from the node's changeset store (A-767 C5; main's split of 2026-11-20: w-store writes this adapter,
//! w-git wires git's push landing, recovery and replay on top of it).
//!
//! **What it is.** git reads a held pack through one synchronous seam, `slates_git::store::packed::PackSource::piece`
//! (a replay walking the pack forward, an object read at its place). On the store a pack is a landed changeset's
//! `Payload` section, and its bytes come only from the store's owner thread, asked as a task
//! (`slates_store::owner::StoreHandle::piece`). R6 rules out a second, blocking way to them. So [`StorePacks`] is the
//! one way: an awaited [`StorePacks::fill`] fetches the store's pieces that cover a range into a bounded set this
//! adapter holds, and the synchronous `piece` lends from that set alone. A `piece` that finds nothing held answers
//! `None`, the trait's own "not held", and the caller awaits a fill of the range and steps again; there is no blocking
//! read behind a miss.
//!
//! **Pieces.** The set holds the store's own read unit, a piece of `slates_store::store::PIECE_BYTES` aligned to it in
//! the pack (the last one shorter), so a range is a run of whole pieces and two fills share what they overlap.
//!
//! **Bound.** At most `most` bytes are held, a bound the caller derives from its own work (a replay's step reads its
//! chunk; an object read the object's span) and states at [`StorePacks::new`]. A fill whose pieces alone exceed it is
//! refused [`PackFillRefused::PastBound`]. Otherwise the least recently used pieces are let go first; a fill touches
//! every piece of its range, so none of them is let go by the same fill.
//!
//! **Identity.** A pack is named by the 32 bytes the store keys its `P` rows on: plain BLAKE3 of the `Payload`
//! section's bytes. Its `slates_vcs::ContentId` here must be that hash (`ContentId::of(pack)`), not a list root of the
//! content store; the record names it so (w-git's side).

use std::borrow::Cow;
use std::collections::VecDeque;

use slates_store::owner::{OwnerError, StoreHandle};
use slates_store::store::PIECE_BYTES;
use slates_vcs::ContentId;

/// Why a fill did not make its range held.
#[derive(Debug)]
pub(crate) enum PackFillRefused {
  /// The range's pieces alone are more bytes than the adapter may hold.
  PastBound {
    /// The bytes the range's pieces take.
    asked: u64,
    /// The adapter's bound.
    most: usize,
  },
  /// The store holds no pack of this identity and length, or it ended short of `at`.
  Missing {
    /// The offset no piece was found at.
    at: u64,
  },
  /// The store's owner did not answer (busy, gone, or its read refused).
  Owner(OwnerError),
}

impl std::fmt::Display for PackFillRefused {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::PastBound { asked, most } => {
        write!(
          f,
          "a pack's range of {asked} bytes past the {most} the reader may hold"
        )
      }
      Self::Missing { at } => write!(
        f,
        "the store holds no pack of this identity and length at {at}"
      ),
      Self::Owner(error) => write!(f, "the store's owner: {error}"),
    }
  }
}

impl std::error::Error for PackFillRefused {}

/// One held piece of a pack.
#[derive(Debug)]
struct Held {
  pack: ContentId,
  len: u64,
  start: u64,
  bytes: Vec<u8>,
}

/// A pack's bytes read from the changeset store, a bounded set of pieces at a time (the module docs).
#[derive(Debug)]
pub(crate) struct StorePacks {
  disk: StoreHandle,
  /// Least recently used first.
  held: VecDeque<Held>,
  held_bytes: usize,
  most: usize,
  /// Pieces fetched from the store.
  fetched: u64,
  /// Pieces a fill found held already (the non-vacuity counter of the held set).
  reused: u64,
}

impl StorePacks {
  /// An adapter over the node's store that holds at most `most` bytes of pieces.
  pub(crate) fn new(disk: StoreHandle, most: usize) -> Self {
    Self {
      disk,
      held: VecDeque::new(),
      held_bytes: 0,
      most,
      fetched: 0,
      reused: 0,
    }
  }

  /// Pieces fetched from the store, and pieces a fill found already held.
  pub(crate) fn counts(&self) -> (u64, u64) {
    (self.fetched, self.reused)
  }

  /// Makes `pack`'s (of `len` bytes) range `offset..until` held: each piece of it fetched from the store unless held,
  /// every one of them marked most recently used. A range past the pack's end is cut at its end.
  pub(crate) async fn fill(
    &mut self,
    pack: &ContentId,
    len: u64,
    offset: u64,
    until: u64,
  ) -> Result<(), PackFillRefused> {
    let piece = u64::try_from(PIECE_BYTES).unwrap_or(u64::MAX);
    let until = until.min(len);
    if offset >= until {
      return Ok(());
    }
    let first = offset.checked_div(piece).unwrap_or(0);
    let last = until.saturating_sub(1).checked_div(piece).unwrap_or(0);
    let pieces = last.saturating_sub(first).saturating_add(1);
    let asked = pieces.saturating_mul(piece);
    if usize::try_from(asked).map_or(true, |asked| asked > self.most) {
      return Err(PackFillRefused::PastBound {
        asked,
        most: self.most,
      });
    }
    for index in first..=last {
      let start = index.saturating_mul(piece);
      if let Some(at) = self.find(pack, len, start) {
        if let Some(held) = self.held.remove(at) {
          self.held.push_back(held);
        }
        self.reused = self.reused.saturating_add(1);
        continue;
      }
      let bytes = self
        .disk
        .piece(pack.0, len, start)
        .await
        .map_err(PackFillRefused::Owner)?
        .filter(|bytes| !bytes.is_empty())
        .ok_or(PackFillRefused::Missing { at: start })?;
      self.fetched = self.fetched.saturating_add(1);
      self.make_room(bytes.len());
      self.held_bytes = self.held_bytes.saturating_add(bytes.len());
      self.held.push_back(Held {
        pack: *pack,
        len,
        start,
        bytes,
      });
    }
    Ok(())
  }

  /// The held piece of `pack` that begins at `start`.
  fn find(&self, pack: &ContentId, len: u64, start: u64) -> Option<usize> {
    self
      .held
      .iter()
      .position(|held| held.pack == *pack && held.len == len && held.start == start)
  }

  /// Lets the least recently used pieces go until `adding` more bytes fit the bound.
  fn make_room(&mut self, adding: usize) {
    while self.held_bytes.saturating_add(adding) > self.most {
      let Some(gone) = self.held.pop_front() else {
        return;
      };
      self.held_bytes = self.held_bytes.saturating_sub(gone.bytes.len());
    }
  }
}

/// A held pack's bytes from `offset` to the end of the held piece it lies in; none where that piece is not held (the
/// caller awaits [`StorePacks::fill`] and steps again) or `offset` is past the pack's end.
impl slates_git::store::packed::PackSource for StorePacks {
  fn piece(&self, pack: &ContentId, len: u64, offset: u64) -> Option<Cow<'_, [u8]>> {
    if offset >= len {
      return None;
    }
    self.held.iter().find_map(|held| {
      if held.pack != *pack || held.len != len || offset < held.start {
        return None;
      }
      let within = usize::try_from(offset.checked_sub(held.start)?).ok()?;
      held
        .bytes
        .get(within..)
        .filter(|rest| !rest.is_empty())
        .map(Cow::Borrowed)
    })
  }
}

#[cfg(test)]
#[path = "store_packs/tests.rs"]
mod tests;

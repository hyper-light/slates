//! A green's held texts, by content identity: the one byte store the version-control servers keep a file's text in
//! (A-716; A-64 "content once"; §4.17.1 "Durability and residency"; Ada's ruling of 2026-11-20, content once for git,
//! Mercurial and Subversion).
//!
//! **Why.** A Subversion repository held every text three times: whole in its records (`slates-svn`'s
//! `Repository.contents`), in the green's files, and in the register entry that logged the revision. On the Mac Studio
//! a 1,000-revision import of git/git's history (a 223 MiB dump) held 305.6 MiB of texts in the records alone, 1.37 times
//! the dump, beside the green's 585 MB (BENCHMARKS.md, "Where the svn server's time and memory go"). git's and
//! Mercurial's stores keep their own copies of the bytes the greens hold too. A table here lets each tool name a text by
//! its identity and read it from the green, so the bytes live once.
//!
//! **What it holds.** Each entry is a content identity (the 32 bytes the caller names the text by: BLAKE3 for
//! Subversion's records, the object's id for an intake) mapped to one block of the green's file blocks
//! (`crate::dims::Blocks`) and the count of its holders. Two ways to hold:
//! - by path ([`crate::engine::Green::hold_path`]): the block the head's file at the path holds, one more holder, so the
//!   held text shares the file's bytes and copies none. Subversion holds each text its revision brought this way, after
//!   the green's increment wrote the file;
//! - by bytes ([`crate::engine::Green::hold_bytes`]): a new block named by no path, for an intake that streams objects
//!   before any tree names them (git's and Mercurial's).
//!
//! Holding an identity already held adds a holder and keeps the block it has: the identity names the bytes, so a second
//! copy would be the same bytes. A block outlives the file that named it while the table holds it, so a text stays
//! readable whatever later versions do to the path, and a checkpoint's folding, which drops history below its floor,
//! drops no block the table holds (the blocks are not history).
//!
//! - by its pieces ([`crate::engine::Green::stage`], `append_staged`, then `seal_staged` or `discard_staged`): a text
//!   arriving a piece at a time, a commit's svndiff windows (§4.17.1 "A push's intake", rule 1), kept in its one block
//!   as each piece comes, so its holder never holds the whole text; named once its last piece is in, when its identity
//!   is known. A staged text whose identity is held already gives its block back and adds a holder.
//!
//! **Bounds.** The table holds at most its room's entries ([`HeldRefused::AtRoom`]), which its owner derives and sets
//! (`Green::set_held_room`; zero until it does, so every hold is refused until the owner says what it may hold). An
//! entry's holders are counted in a `u64` and a hold past it is refused ([`HeldRefused::Holders`]), never wrapped.
//!
//! **Owed** (GAPS' §4.17 entry): a green op that sets a path from a held identity, reusing its block (A-716 slice 1b);
//! the table carried in the green's checkpoint, so recovery and replication rebuild it (slice 1c); Subversion's records
//! reading their texts through it (slices 2 and 3).

use std::collections::BTreeMap;

use crate::dims::Block;

/// A text's identity: the 32 bytes its holder names it by.
pub type Identity = [u8; 32];

/// Why a hold or a release was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldRefused {
  /// The table holds its room's entries already; nothing was held.
  AtRoom {
    /// The room, in entries.
    room: usize,
  },
  /// The identity's holders are at the most a count can hold; nothing was held.
  Holders,
  /// No file stands at the path to hold.
  NoFile,
  /// The identity is not held.
  NotHeld,
}

/// A text being staged a piece at a time ([`crate::engine::Green::stage`]): a block of the green's named by no path and
/// held by no identity yet, which its owner seals under the text's identity or discards. One owner: neither `Clone` nor
/// `Copy`, so a staged block is sealed or discarded once.
#[derive(Debug, PartialEq, Eq)]
pub struct Staged {
  /// Its block.
  pub(crate) block: Block,
  /// The bytes appended so far.
  pub(crate) length: u64,
}

impl Staged {
  /// The bytes appended so far.
  pub fn length(&self) -> u64 {
    self.length
  }
}

/// One held text: its block and its holders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
  /// The block holding its bytes.
  pub(crate) block: Block,
  /// How many holders hold it, at least one.
  pub(crate) holders: u64,
}

/// The table (module docs).
#[derive(Debug, Default)]
pub(crate) struct Held {
  /// Each held identity's entry.
  pub(crate) entries: BTreeMap<Identity, Entry>,
  /// The most entries it may hold.
  pub(crate) room: usize,
}

impl Held {
  /// Adds a holder to `identity` if it is held: whether it was, refused past the count's most.
  pub(crate) fn hold_again(&mut self, identity: &Identity) -> Result<bool, HeldRefused> {
    match self.entries.get_mut(identity) {
      Some(entry) => {
        entry.holders = entry.holders.checked_add(1).ok_or(HeldRefused::Holders)?;
        Ok(true)
      }
      None => Ok(false),
    }
  }

  /// Refuses a new entry past the room.
  pub(crate) fn room_for_one(&self) -> Result<(), HeldRefused> {
    if self.entries.len() >= self.room {
      return Err(HeldRefused::AtRoom { room: self.room });
    }
    Ok(())
  }

  /// One holder of `identity` fewer: its block when that was the last, for the caller to release.
  pub(crate) fn release(&mut self, identity: &Identity) -> Result<Option<Block>, HeldRefused> {
    let entry = self.entries.get_mut(identity).ok_or(HeldRefused::NotHeld)?;
    entry.holders = entry.holders.saturating_sub(1);
    if entry.holders > 0 {
      return Ok(None);
    }
    Ok(self.entries.remove(identity).map(|entry| entry.block))
  }
}

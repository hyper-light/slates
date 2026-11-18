//! Where a green's content bytes are (A-273 step 3; D-7, "immutable chunk reads from any core"; A-64, content once): in
//! sealed blocks (`slates_mem::sealed`). On the green's owner the blocks are its own: it seals each new run of bytes
//! (a file's current value, a history chunk), releases what it names no more, and frees a released block once every
//! replica has passed the feed message that released it. On a replica they are its owner's, read in place: it holds
//! no byte of content of its own.
//!
//! **One code path (A-273).** A replica replays each version through the engine's own commit, which seals the bytes the
//! version makes, in the order it makes them. On the owner a seal makes a block; on a replica it adopts the next block
//! the owner sealed in the same message ([`Store::begin`] loads them, in the owner's order) and checks that the block
//! holds exactly the bytes the replay made. A replay that makes other bytes, or seals more or fewer blocks than the
//! owner did, is not the owner's green: the replica is refused and rebuilt ([`Store::finish`]), never answered from.
//!
//! **Reading.** A read of a block this store cannot read (on a replica, a block not in its live table: released and
//! passed, or never adopted) answers no bytes and is counted ([`Store::reads_refused`]); it is never a read of memory
//! the owner freed, which `slates_mem::sealed` holds by its reader's live table. A replica whose count moved has
//! answered from bytes it did not have: its holder forwards to the owner.

use std::cell::Cell;
use std::collections::VecDeque;

use slates_mem::sealed::{
  Block, BlockReader, Changes, Joined, Left, Passed, ReaderRefusal, SealedBlocks,
};

use crate::replica::ReplicaRefusal;

/// Where a green's content bytes are (module docs).
#[derive(Debug)]
pub(crate) enum Store {
  /// The owner's own blocks, and the reads of its own it could not answer.
  Own(SealedBlocks, Cell<u64>),
  /// A replica's: its owner's blocks, read in place.
  Read(Reading),
}

/// A replica's reading of its owner's blocks.
#[derive(Debug)]
pub(crate) struct Reading {
  reader: BlockReader,
  /// The blocks the message being replayed sealed, in the owner's order, not yet adopted.
  adopting: VecDeque<Block>,
  /// Whether the replay sealed other bytes than the owner did, or more blocks.
  diverged: bool,
}

impl Default for Store {
  fn default() -> Store {
    Store::Own(SealedBlocks::new(), Cell::new(0))
  }
}

impl Store {
  /// The bytes of `block`, or none (counted) where this store cannot read it.
  pub(crate) fn bytes(&self, block: Block) -> &[u8] {
    let bytes = match self {
      Store::Own(blocks, _) => blocks.bytes(block),
      Store::Read(reading) => reading.reader.read(block),
    };
    if bytes.is_none()
      && let Store::Own(_, refused) = self
    {
      refused.set(refused.get().saturating_add(1));
    }
    bytes.unwrap_or_default()
  }

  /// A block holding `bytes`: sealed on the owner; on a replica, the next block the owner sealed in the message being
  /// replayed, which must hold exactly these bytes (module docs).
  pub(crate) fn seal(&mut self, bytes: Vec<u8>) -> Block {
    match self {
      Store::Own(blocks, _) => blocks.seal(bytes),
      Store::Read(reading) => {
        let Some(block) = reading.adopting.pop_front() else {
          reading.diverged = true;
          return Block::NONE;
        };
        if reading.reader.read(block) != Some(bytes.as_slice()) {
          reading.diverged = true;
        }
        block
      }
    }
  }

  /// The green names `block` no more. The owner releases it (freed once every replica has passed this message); a
  /// replica's reader takes it out of its live table when it passes the message that released it.
  pub(crate) fn release(&mut self, block: Block) {
    if let Store::Own(blocks, _) = self {
      blocks.release(block);
    }
  }

  /// The owner's blocks sealed and released since its last feed message, at its feed position, which then moves past
  /// it; none on a replica.
  pub(crate) fn take_changes(&mut self) -> Option<Changes> {
    match self {
      Store::Own(blocks, _) => Some(blocks.take_changes()),
      Store::Read(_) => None,
    }
  }

  /// The feed position: on the owner, its next message's; on a replica, the next message it applies.
  pub(crate) fn position(&self) -> u64 {
    match self {
      Store::Own(blocks, _) => blocks.position(),
      Store::Read(reading) => reading.reader.position(),
    }
  }

  /// A replica's registration with the owner's blocks now, refused past `limit` replicas (A-273 step 4) and on a
  /// replica, which has no blocks of its own.
  pub(crate) fn join(&mut self, limit: usize) -> Result<Joined, ReplicaRefusal> {
    match self {
      Store::Own(blocks, _) => blocks.join(limit).map_err(|full| ReplicaRefusal::Full {
        readers: full.readers,
        limit: full.limit,
      }),
      Store::Read(_) => Err(ReplicaRefusal::NotAnOwner),
    }
  }

  /// The bytes the owner keeps for its replicas past which the slowest is evicted (A-273 step 4).
  pub(crate) fn set_held_limit(&mut self, bytes: usize) {
    if let Store::Own(blocks, _) = self {
      blocks.set_held_limit(bytes);
    }
  }

  /// At the start of a change on the owner: a change recorded and never sent to the replicas means they cannot be fed
  /// in order, so every replica is evicted (A-273 step 4, `slates_mem::sealed`'s "Bounds").
  pub(crate) fn evict_if_unfed(&mut self) {
    if let Store::Own(blocks, _) = self
      && blocks.has_pending()
    {
      blocks.evict_all();
    }
  }

  /// Whether `changes` says the owner evicted this replica, read without taking it; never on the owner.
  pub(crate) fn evicted_by(&self, changes: &Changes) -> bool {
    match self {
      Store::Read(reading) => reading.reader.evicted_by(changes),
      Store::Own(..) => false,
    }
  }

  /// Replicas the owner evicted: for what it kept for them, and for changes left unfed.
  pub(crate) fn evictions(&self) -> (u64, u64) {
    match self {
      Store::Own(blocks, _) => blocks.evictions(),
      Store::Read(_) => (0, 0),
    }
  }

  /// A replica's store over a registration with its owner's blocks.
  pub(crate) fn reading(joined: Joined) -> Store {
    Store::Read(Reading {
      reader: BlockReader::new(joined),
      adopting: VecDeque::new(),
      diverged: false,
    })
  }

  /// A replica passed a message: the owner frees what every replica has passed.
  pub(crate) fn acknowledge(&mut self, passed: Passed) {
    if let Store::Own(blocks, _) = self {
      blocks.acknowledge(passed);
    }
  }

  /// A replica is done: the owner keeps nothing for it.
  pub(crate) fn leave(&mut self, left: Left) {
    if let Store::Own(blocks, _) = self {
      blocks.leave(left);
    }
  }

  /// The replica's registration given back, for its owner's [`Store::leave`]; none from the owner's own store.
  pub(crate) fn into_left(self) -> Option<Left> {
    match self {
      Store::Read(reading) => Some(reading.reader.leave()),
      Store::Own(..) => None,
    }
  }

  /// Before a replica replays the message that carries `changes`: the blocks it sealed are admitted and queued for
  /// adoption in the owner's order. Refused, unchanged, where the reader refuses them (evicted, another writer's, out
  /// of order) or the store is the owner's own, which replays nothing (refused as another writer's).
  pub(crate) fn begin(&mut self, changes: &Changes) -> Result<(), ReaderRefusal> {
    let Store::Read(reading) = self else {
      return Err(ReaderRefusal::OtherWriter);
    };
    reading.reader.admit(changes)?;
    reading.adopting = changes.sealed().collect();
    reading.diverged = false;
    Ok(())
  }

  /// After the replay: the message passed (its released blocks leave the live table), and what the owner frees by; none
  /// when the replay was not the owner's (it sealed other bytes, or more or fewer blocks), which leaves the replica for
  /// rebuilding.
  pub(crate) fn finish(&mut self, changes: &Changes) -> Option<Passed> {
    let Store::Read(reading) = self else {
      return None;
    };
    if reading.diverged || !reading.adopting.is_empty() {
      return None;
    }
    reading.reader.pass(changes).ok()
  }

  /// The bytes kept on the owner only for its replicas (none on a replica).
  pub(crate) fn held_bytes(&self) -> usize {
    match self {
      Store::Own(blocks, _) => blocks.held_bytes(),
      Store::Read(_) => 0,
    }
  }

  /// [`Store::held_bytes`] recounted from the owner's kept list.
  pub(crate) fn held_bytes_recounted(&self) -> usize {
    match self {
      Store::Own(blocks, _) => blocks.held_bytes_recounted(),
      Store::Read(_) => 0,
    }
  }

  /// The frees the owner kept for a replica since it was made.
  pub(crate) fn frees_held(&self) -> u64 {
    match self {
      Store::Own(blocks, _) => blocks.frees_held(),
      Store::Read(_) => 0,
    }
  }

  /// Reads this store could not answer (module docs).
  pub(crate) fn reads_refused(&self) -> u64 {
    match self {
      Store::Own(_, refused) => refused.get(),
      Store::Read(reading) => reading.reader.refused(),
    }
  }

  /// Whether this is a replica's reading of its owner's blocks.
  pub(crate) fn is_replica(&self) -> bool {
    matches!(self, Store::Read(_))
  }
}

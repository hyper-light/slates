//! The purge in progress, published in one slot (A-249, amending A-190 and A-198; §4.6 Windows). The mount thread
//! publishes each purge, with its client's process id, before it asks the client to open the file through the drive, and
//! publishes no purge once the client has answered (`crate::host`, `MountThread::purge`). The receive worker reads the
//! slot when a Create arrives, to tell the purge's own open from a program's (`crate::dispatcher::PrivateOpens`). This
//! module is the slot's protocol, apart from the memory it lives in: on Windows an unnamed section both threads map
//! (`crate::purge_intents`), and in its tests plain atomics, or loom's under `--cfg loom`.
//!
//! **A seqlock whose word names the client.** The slot's word holds the client's process id in its low 32 bits (zero:
//! no purge), a writing flag above it, and a count of publications above that. A publication sets the flag, lands the
//! purge's bytes in the slot's racy span, then stores the new count with the client and the flag clear. So a Create
//! from any other process costs one load of the word: Ada authorized that one atomic read on every Create (2026-11-07),
//! in place of the channel drain it replaces. Only the purge's own opens read further: the purge's bytes, then the word
//! again to know the copy whole.
//!
//! **Publishing never waits.** Until 2026-11-07 the intents went through a channel of two messages that the worker
//! drained after each control call. WinFsp's control call never goes pending: it waits for a request inside the call
//! (v2.1 `src/sys/fsctl.c:95-99`, `src/sys/volume.c:1171`). So when the kernel refused a purge's open before any request
//! reached slates, nothing made an idle worker's call return, and the third publish waited for the call's 1,000 ms
//! transaction timeout: four such purges took 3.02 s on arm64 and 3.01 s on x86_64 (diag-winfsp-overlap-43). Cancelling
//! the worker's call from the publisher (`CancelIoEx`) left 2.54 s and 2.65 s in the same run, and was not landed.
//!
//! **A torn read is no purge.** A read that finds the word's writing flag set, or the word changed under the copy,
//! answers that no purge is in progress. No purge's open can be in flight then: the client is asked only after its purge
//! is published whole, and the next publication follows the client's answer, by which time its opens are all answered.

#[cfg(loom)]
use loom::sync::atomic::{Ordering, fence};
#[cfg(not(loom))]
use std::sync::atomic::{Ordering, fence};

use crate::purge::Purge;

/// Format: the slot's word, first in its memory.
pub const WORD_AT: usize = 0;
/// Format: the word's writing flag, above the client's 32-bit process id.
const WRITING: u64 = 1 << u32::BITS;
/// Format: the publication count starts above the writing flag.
const COUNT_SHIFT: u32 = u32::BITS + 1;
/// Shape: the purge's bytes start a cache line pair after the word, the workspace's padding for a hot atomic
/// (`#[repr(align(128))]`, CLAUDE.md), so the word every Create loads shares no line with them.
pub const PURGE_AT: usize = 128;
/// Format: the purge's head: the file's inode and generation, the purge's offset and length, and the path's length in
/// bytes; the path's UTF-8 follows.
const INODE_AT: usize = 0;
/// Format: see [`INODE_AT`].
const OBJECT_GENERATION_AT: usize = INODE_AT + size_of::<u64>();
/// Format: see [`INODE_AT`].
const OFFSET_AT: usize = OBJECT_GENERATION_AT + size_of::<u64>();
/// Format: see [`INODE_AT`].
const LEN_AT: usize = OFFSET_AT + size_of::<u64>();
/// Format: see [`INODE_AT`].
const PATH_LEN_AT: usize = LEN_AT + size_of::<u64>();
/// Format: see [`INODE_AT`].
pub const HEAD: usize = PATH_LEN_AT + size_of::<u64>();
/// Derived: a purge names a path Windows holds open, at most [`crate::notify::MAX_PATH_UNITS`] UTF-16 units, and one
/// unit takes at most three bytes of UTF-8 (a surrogate pair's two units take four), so this many bytes hold any of them.
pub const PATH_BYTES: usize = crate::notify::MAX_PATH_UNITS * 3;
/// Format: the slot's memory: the word, its padding, the head and the longest path.
pub const SLOT_BYTES: usize = PURGE_AT + HEAD + PATH_BYTES;

/// One purge in progress: the purge, and the process id of the client making its opens (`crate::purge_client`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
  /// The purge.
  pub purge: Purge,
  /// The purge client's process id, never zero.
  pub client: u32,
}

/// Why the slot refused.
#[derive(Debug)]
pub enum Refusal {
  /// The slot's memory could not be reached.
  Memory(slates_mem::MemError),
  /// A purge whose path does not fit, whose client is process zero, or whose whole publication is not one.
  Purge,
}

impl std::fmt::Display for Refusal {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Memory(error) => write!(out, "the purge slot's memory refused: {error}"),
      Self::Purge => write!(out, "a purge does not fit the purge slot"),
    }
  }
}

/// The memory a slot lives in: its word and its racy bytes, which a reader copies while a writer may be rewriting them.
pub trait Memory {
  /// Loads the word.
  fn load(&self, order: Ordering) -> Result<u64, Refusal>;
  /// Stores the word.
  fn store(&self, value: u64, order: Ordering) -> Result<(), Refusal>;
  /// Copies racy bytes out at `at`, a byte offset from [`PURGE_AT`].
  fn copy_out(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal>;
  /// Copies racy bytes in at `at`, a byte offset from [`PURGE_AT`].
  fn copy_in(&mut self, at: usize, from: &[u8]) -> Result<(), Refusal>;
}

/// The only writer's side of a slot.
pub struct Writer {
  /// The count of publications made: this writer is the only one, so it never reads the word back.
  published: u64,
}

impl Writer {
  /// A writer over memory holding no publication (its word zero).
  pub fn new() -> Self {
    Self { published: 0 }
  }

  /// Publishes `intent` as the purge in progress, or no purge. Never waits for a reader.
  pub fn publish<M: Memory>(
    &mut self,
    memory: &mut M,
    intent: Option<&Intent>,
  ) -> Result<(), Refusal> {
    let count = self.published.wrapping_add(1);
    let counted = count.wrapping_shl(COUNT_SHIFT);
    let client = match intent {
      Some(intent) if intent.client == 0 || intent.purge.path.len() > PATH_BYTES => {
        return Err(Refusal::Purge);
      }
      Some(intent) => u64::from(intent.client),
      None => 0,
    };
    memory.store(counted | WRITING, Ordering::Relaxed)?;
    // The flag is seen set before any byte of the purge that follows it.
    fence(Ordering::Release);
    if let Some(intent) = intent {
      let path = intent.purge.path.as_bytes();
      let mut head = [0u8; HEAD];
      let fields: [(usize, u64); 5] = [
        (INODE_AT, intent.purge.object.inode),
        (OBJECT_GENERATION_AT, intent.purge.object.generation),
        (OFFSET_AT, intent.purge.offset),
        (LEN_AT, intent.purge.len),
        (
          PATH_LEN_AT,
          u64::try_from(path.len()).map_err(|_| Refusal::Purge)?,
        ),
      ];
      for (at, value) in fields {
        head
          .get_mut(at..at.saturating_add(size_of::<u64>()))
          .ok_or(Refusal::Purge)?
          .copy_from_slice(&value.to_le_bytes());
      }
      memory.copy_in(0, &head)?;
      memory.copy_in(HEAD, path)?;
    }
    // The purge's bytes are seen before the word that names its client.
    memory.store(counted | client, Ordering::Release)?;
    self.published = count;
    Ok(())
  }
}

impl Default for Writer {
  fn default() -> Self {
    Self::new()
  }
}

/// The purge in progress whose opens `process` makes, or `None`: none published, another process's, or a publication
/// under way (see the module doc).
pub fn current_for<M: Memory>(memory: &M, process: u32) -> Result<Option<Intent>, Refusal> {
  // Ada authorized this one atomic read on every Create (2026-11-07), replacing the purge-intent channel's `try_recv`
  // that drained it before every callback. It is the only atomic a program's Create meets here.
  let word = memory.load(Ordering::Acquire)?;
  if word & WRITING != 0 || word & u64::from(u32::MAX) != u64::from(process) || process == 0 {
    return Ok(None);
  }
  let mut head = [0u8; HEAD];
  memory.copy_out(0, &mut head)?;
  let len = usize::try_from(field(&head, PATH_LEN_AT)?).map_err(|_| Refusal::Purge)?;
  if len > PATH_BYTES {
    return whole_or_none(memory, word, Err(Refusal::Purge));
  }
  let mut path = vec![0u8; len];
  memory.copy_out(HEAD, &mut path)?;
  let intent = String::from_utf8(path)
    .map_err(|_| Refusal::Purge)
    .and_then(|path| {
      Ok(Intent {
        purge: Purge {
          object: slates_bridge_core::ObjectId::new(
            field(&head, INODE_AT)?,
            field(&head, OBJECT_GENERATION_AT)?,
          ),
          path,
          offset: field(&head, OFFSET_AT)?,
          len: field(&head, LEN_AT)?,
        },
        client: process,
      })
    });
  whole_or_none(memory, word, intent)
}

/// `read` when the word is still `word` after the bytes it was made from were copied; no purge when it changed, since
/// a copy made across a publication is no purge's (see the module doc).
fn whole_or_none<M: Memory>(
  memory: &M,
  word: u64,
  read: Result<Intent, Refusal>,
) -> Result<Option<Intent>, Refusal> {
  // The bytes copied above are ordered before the word's second read.
  fence(Ordering::Acquire);
  if memory.load(Ordering::Relaxed)? != word {
    return Ok(None);
  }
  read.map(Some)
}

fn field(head: &[u8], at: usize) -> Result<u64, Refusal> {
  head
    .get(at..at.saturating_add(size_of::<u64>()))
    .and_then(|bytes| bytes.try_into().ok())
    .map(u64::from_le_bytes)
    .ok_or(Refusal::Purge)
}

// Two attributes rather than `all(test, loom)`: clippy's test-context rule (unwrap allowed in tests) recognizes only a
// bare `cfg(test)`, as in `slates-ipc`'s park model.
#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
  use super::*;

  #[cfg(loom)]
  use loom::sync::atomic::{AtomicU8, AtomicU64};
  #[cfg(not(loom))]
  use std::sync::atomic::{AtomicU8, AtomicU64};

  /// A slot in plain atomics: its word and its racy bytes, each copied through an atomic byte as the section's are. Both
  /// threads hold it by shared reference, so the writer's copy-in needs no exclusive borrow.
  struct Atomics {
    word: AtomicU64,
    bytes: Vec<AtomicU8>,
  }

  impl Atomics {
    /// A slot whose racy span holds a purge's head and `path` bytes of path.
    fn new(path: usize) -> Self {
      Self {
        word: AtomicU64::new(0),
        bytes: (0..HEAD + path).map(|_| AtomicU8::new(0)).collect(),
      }
    }
  }

  impl Memory for &Atomics {
    fn load(&self, order: Ordering) -> Result<u64, Refusal> {
      Ok(self.word.load(order))
    }
    fn store(&self, value: u64, order: Ordering) -> Result<(), Refusal> {
      self.word.store(value, order);
      Ok(())
    }
    fn copy_out(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal> {
      for (offset, byte) in into.iter_mut().enumerate() {
        *byte = self.bytes[at + offset].load(Ordering::Relaxed);
      }
      Ok(())
    }
    fn copy_in(&mut self, at: usize, from: &[u8]) -> Result<(), Refusal> {
      for (offset, byte) in from.iter().enumerate() {
        self.bytes[at + offset].store(*byte, Ordering::Relaxed);
      }
      Ok(())
    }
  }

  /// A purge whose every field and path is derived from `seed`, so a mix of two publications is visible.
  fn intent(seed: u32, client: u32) -> Intent {
    Intent {
      purge: Purge {
        object: slates_bridge_core::ObjectId::new(u64::from(seed), u64::from(seed) + 1),
        path: format!("\\{seed}"),
        offset: u64::from(seed) * 4096,
        len: 4096,
      },
      client,
    }
  }

  /// Shape: the client the racing models' purges name, and a process that is not it.
  const CLIENT: u32 = 7;
  /// Shape: see [`CLIENT`].
  const PROGRAM: u32 = 8;

  #[cfg(not(loom))]
  mod plain {
    use super::*;

    /// Shape: enough publications that a writer that waited for its reader would be caught many times over.
    const PUBLICATIONS: u32 = 10_000;

    /// GAPS, "A private purge's intent can wait up to the 1 s transaction timeout". Do: publish 10,000 purges and their
    /// ends with nothing reading. Expect: every publication returns; the slot then reads no purge, then the next purge
    /// whole for its own client and no purge for any other process. The channel this replaced blocked on the third.
    #[test]
    fn a_publisher_never_waits_for_its_reader() {
      let memory = Atomics::new(PATH_BYTES);
      let mut slot = &memory;
      let mut writer = Writer::new();
      for seed in 1..=PUBLICATIONS {
        writer
          .publish(&mut slot, Some(&intent(seed, seed)))
          .unwrap();
        writer.publish(&mut slot, None).unwrap();
      }
      assert_eq!(current_for(&slot, PUBLICATIONS).unwrap(), None);
      let next = intent(PUBLICATIONS + 1, CLIENT);
      writer.publish(&mut slot, Some(&next)).unwrap();
      assert_eq!(current_for(&slot, CLIENT).unwrap(), Some(next));
      assert_eq!(current_for(&slot, PROGRAM).unwrap(), None);
    }

    /// A-249. Do: one thread publishes 10,000 distinct purges, every one for the same client, while another reads the
    /// slot as that client's Creates would and as a program's would, until the writer is done. Expect: every read of the
    /// client's is no purge or one publication whole, never a mix of two; every read of the program's is no purge; and
    /// once the writer is done, the last purge reads whole.
    #[test]
    fn a_publish_racing_a_create_reads_no_purge_or_a_whole_one() {
      let memory = Atomics::new(PATH_BYTES);
      let reader = &memory;
      let done = std::sync::atomic::AtomicBool::new(false);
      let mut racing = 0u32;
      std::thread::scope(|scope| {
        scope.spawn(|| {
          let mut writer = Writer::new();
          let mut slot = &memory;
          for seed in 1..=PUBLICATIONS {
            writer
              .publish(&mut slot, Some(&intent(seed, CLIENT)))
              .unwrap();
          }
          done.store(true, Ordering::Release);
        });
        while !done.load(Ordering::Acquire) {
          assert_eq!(current_for(&reader, PROGRAM).unwrap(), None);
          if let Some(read) = current_for(&reader, CLIENT).unwrap() {
            let seed = u32::try_from(read.purge.object.inode).unwrap();
            assert_eq!(read, intent(seed, CLIENT), "a torn read of purge {seed}");
          }
          racing += 1;
        }
      });
      assert!(racing > 0, "no read raced the writer");
      assert_eq!(
        current_for(&reader, CLIENT).unwrap(),
        Some(intent(PUBLICATIONS, CLIENT))
      );
    }
  }

  #[cfg(loom)]
  mod model {
    use super::*;
    use slates_mem::loom_bounds;

    /// A-249 (AC-0.7). Do: a writer publishes two purges for the same client, then no purge, while a Create of that
    /// client and a program's read the slot, in every interleaving within the workspace's bounds. Expect: the client's
    /// read is no purge or one of the two whole, never a mix; the program's is always no purge.
    #[test]
    fn loom_a_publish_racing_a_create_reads_no_purge_or_a_whole_one() {
      loom_bounds::explore("purge slot: a publish racing a Create", || {
        // Test scaffolding under `cfg(loom)` only (D-8 exception 3): the model's writer and reader share it, leaked for
        // the `'static` loom's threads need, as `slates-ipc`'s park model does.
        let memory: &'static Atomics = Box::leak(Box::new(Atomics::new(size_of::<u64>())));
        let writer = loom::thread::spawn(move || {
          let mut writer = Writer::new();
          let mut slot = memory;
          writer.publish(&mut slot, Some(&intent(1, CLIENT))).unwrap();
          writer.publish(&mut slot, Some(&intent(2, CLIENT))).unwrap();
          writer.publish(&mut slot, None).unwrap();
        });
        assert_eq!(current_for(&memory, PROGRAM).unwrap(), None);
        if let Some(read) = current_for(&memory, CLIENT).unwrap() {
          assert!(
            read == intent(1, CLIENT) || read == intent(2, CLIENT),
            "a torn read: {read:?}"
          );
        }
        writer.join().unwrap();
      });
    }
  }
}

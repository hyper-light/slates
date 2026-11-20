//! An increment's content named by identity rather than carried (A-760, "1b"; main's ruling of 2026-11-20 ~09:10;
//! §4.16 data model; §4.17.1 "A push's intake"; A-727 (d)).
//!
//! **Why.** Subversion's intake writes each text once into the repository's content plane, durable before the revision's
//! entry (w-svn's A-761). An increment that then carried the text's bytes in its post-state made a second copy in the
//! log and a third in the green, and its landing read a 500 MB text in one call. Here an increment names such a run:
//! the object's identity (plain BLAKE3 of its bytes, and its length: the same on every node whatever its page size,
//! the 07:36 ruling) and which bytes of it. The log records the names, never the bytes.
//!
//! **One address space.** An op's `src` keeps meaning an offset into the post-state: the inline bytes first, then each
//! named run in the table's order. So every reader of added bytes in the engine is unchanged once the runs are read.
//!
//! **Read in bounded steps.** [`crate::engine::Increment::read_step`] reads the runs' bytes through the owner's
//! [`Objects`] (its content plane) a slice a step, appended to the post-state reserved once at its whole length (a
//! large reservation is mapped, its pages made only as they are written), so no step reads more than its slice. The
//! engine refuses, typed, to judge or commit a work whose runs are not all read: a missing byte is never a zero.

/// An object's identity: plain BLAKE3 of its bytes, and its length (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity {
  /// BLAKE3 of the bytes.
  pub hash: [u8; 32],
  /// The object's length.
  pub len: u64,
}

/// A run of an object's bytes an increment names: `len` bytes from `offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamedRun {
  /// The object's identity.
  pub identity: Identity,
  /// Where the run starts in the object.
  pub offset: u64,
  /// The run's length.
  pub len: u64,
}

/// An increment's named runs (module docs): none for a work that carries all its bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Named {
  /// The post-state's inline bytes, before the first run: meaningful while a run is named.
  pub inline_len: u64,
  /// The runs, in the post-state's order.
  pub runs: Vec<NamedRun>,
}

impl Named {
  /// The bytes every run names together.
  pub fn len(&self) -> u64 {
    self
      .runs
      .iter()
      .fold(0u64, |total, run| total.saturating_add(run.len))
  }

  /// Whether no run is named.
  pub fn is_empty(&self) -> bool {
    self.runs.is_empty()
  }
}

/// A named object the owner does not hold, or a range past its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectMissing {
  /// The object asked for.
  pub identity: Identity,
}

/// Where an increment's named runs are read from: its owner's content plane (the server's, over vcs_store's objects;
/// a test's, over a map).
pub trait Objects {
  /// Copies `into.len()` bytes of `identity`'s object from `at` into `into`: refused when the object is not held here
  /// or the range is past its length.
  fn read(&self, identity: &Identity, at: u64, into: &mut [u8]) -> Result<(), ObjectMissing>;
}

/// Format: the bytes one named run takes in an increment's encoding: the hash, the object's length, the run's offset
/// and its length.
pub(crate) const RUN_BYTES: usize = 32 + 3 * size_of::<u64>();

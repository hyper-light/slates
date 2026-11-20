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
  /// Each run's bytes as read so far ([`crate::engine::Increment::read_step`]), one buffer a run, so a whole text's
  /// buffer becomes the green's file at the commit with no copy ([`Read`]).
  pub read: Read,
}

/// The named runs' bytes read so far, one buffer a run: what the plane holds, read here a slice a step. Not part of
/// the work's value: two works naming the same runs are equal whatever each has read, and a clone names the runs and
/// reads them again (a clone that copied a 500 MB text would break the sizing rule A-760 keeps), so it carries none.
#[derive(Debug, Default)]
pub struct Read(pub(crate) Vec<Vec<u8>>);

impl Clone for Read {
  fn clone(&self) -> Read {
    Read(Vec::new())
  }
}

impl PartialEq for Read {
  fn eq(&self, _other: &Read) -> bool {
    true
  }
}

impl Eq for Read {}

/// A work's post-state as the engine reads it (A-760): its inline bytes, then each named run's bytes as read, one
/// address space an op's `src` indexes. A span inside one piece is lent, one across pieces copied.
#[derive(Clone, Copy, Debug)]
pub struct Post<'a> {
  /// The inline bytes.
  pub(crate) inline: &'a [u8],
  /// Each run's bytes as read.
  pub(crate) runs: &'a [Vec<u8>],
}

impl<'a> Post<'a> {
  /// The post-state of `inline` bytes and `runs`.
  pub(crate) fn of(inline: &'a [u8], runs: &'a [Vec<u8>]) -> Post<'a> {
    Post { inline, runs }
  }

  /// Its length: the inline bytes and every run's as read.
  pub(crate) fn len(&self) -> u64 {
    let runs = self
      .runs
      .iter()
      .fold(0usize, |total, run| total.saturating_add(run.len()));
    u64::try_from(self.inline.len().saturating_add(runs)).unwrap_or(u64::MAX)
  }

  /// The run whose bytes are exactly the `len` bytes at `src`, by its place: what a whole text's file is made of.
  pub(crate) fn whole_run(&self, src: u64, len: u64) -> Option<usize> {
    let mut start = u64::try_from(self.inline.len()).ok()?;
    for (at, run) in self.runs.iter().enumerate() {
      let run_len = u64::try_from(run.len()).ok()?;
      if src == start && len == run_len {
        return Some(at);
      }
      start = start.saturating_add(run_len);
    }
    None
  }

  /// The `len` bytes at `src`, clamped to its length (as a slice of one buffer was).
  pub(crate) fn span(&self, src: u64, len: u64) -> std::borrow::Cow<'a, [u8]> {
    let end = src.saturating_add(len).min(self.len());
    let mut pieces: Vec<&'a [u8]> = Vec::new();
    let mut start = 0u64;
    for piece in core::iter::once(self.inline).chain(self.runs.iter().map(Vec::as_slice)) {
      let piece_end = start.saturating_add(u64::try_from(piece.len()).unwrap_or(u64::MAX));
      if src < piece_end && end > start {
        let from = usize::try_from(src.saturating_sub(start)).unwrap_or(usize::MAX);
        let to = usize::try_from(end.min(piece_end).saturating_sub(start)).unwrap_or(usize::MAX);
        pieces.push(piece.get(from..to).unwrap_or_default());
      }
      if piece_end >= end {
        break;
      }
      start = piece_end;
    }
    match pieces.as_slice() {
      [] => std::borrow::Cow::Borrowed(&[]),
      [one] => std::borrow::Cow::Borrowed(one),
      many => std::borrow::Cow::Owned(many.concat()),
    }
  }
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

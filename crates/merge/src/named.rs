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
  /// The runs, in the post-state's order, with where each ends ([`RunTable`]).
  pub runs: RunTable,
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

/// A work's named runs in the post-state's order, with where each ends among them (A-760): what [`Post`] finds a byte's
/// run by, a halving search, so a version of many files reads each file's bytes at a logarithm of the runs, never a walk
/// of them all (goal 93: a revision of 6,000 files spent 442 ms of one step measuring its post-state a file at a time,
/// on a debug build). Changed only by [`RunTable::push`], so the ends never stand apart from the runs; read as the runs'
/// slice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunTable {
  /// The runs.
  runs: Vec<NamedRun>,
  /// Each run's end in the runs' address space: the bytes of every run up to and including it.
  ends: Vec<u64>,
}

/// No runs: a post-state of inline bytes alone.
static NO_RUNS: RunTable = RunTable {
  runs: Vec::new(),
  ends: Vec::new(),
};

impl RunTable {
  /// Names `run` after every run named so far.
  pub fn push(&mut self, run: NamedRun) {
    let end = self.total().saturating_add(run.len);
    self.runs.push(run);
    self.ends.push(end);
  }

  /// The bytes every run names together.
  pub fn total(&self) -> u64 {
    self.ends.last().copied().unwrap_or(0)
  }

  /// Where the `at`th run starts in the runs' address space.
  fn start(&self, at: usize) -> u64 {
    at.checked_sub(1)
      .and_then(|before| self.ends.get(before))
      .copied()
      .unwrap_or(0)
  }

  /// The first run ending past `offset` in the runs' address space: the run holding its byte, or one after a run of
  /// none there.
  fn first_ending_past(&self, offset: u64) -> usize {
    self.ends.partition_point(|end| *end <= offset)
  }
}

impl core::ops::Deref for RunTable {
  type Target = [NamedRun];

  fn deref(&self) -> &[NamedRun] {
    &self.runs
  }
}

impl<'table> IntoIterator for &'table RunTable {
  type Item = &'table NamedRun;
  type IntoIter = core::slice::Iter<'table, NamedRun>;

  fn into_iter(self) -> Self::IntoIter {
    self.runs.iter()
  }
}

impl FromIterator<NamedRun> for RunTable {
  fn from_iter<I: IntoIterator<Item = NamedRun>>(runs: I) -> RunTable {
    let mut table = RunTable::default();
    for run in runs {
      table.push(run);
    }
    table
  }
}

/// A work's post-state as the engine reads it (A-760): its inline bytes, then each named run's bytes as read, one
/// address space an op's `src` indexes. A span inside one piece is lent, one across pieces copied. Each run takes its
/// declared length in that space, read or not (A-887, main 01a20182's ruling of 2026-11-24 02:17: a replay leaves
/// unread a run its apply names whole, which no op reads, so no later run's address may move with it); a run never read
/// spans no bytes.
#[derive(Clone, Copy, Debug)]
pub struct Post<'a> {
  /// The inline bytes.
  pub(crate) inline: &'a [u8],
  /// Each run's bytes as read.
  pub(crate) runs: &'a [Vec<u8>],
  /// Each run as named, its declared length its place in the address space.
  pub(crate) declared: &'a RunTable,
}

impl<'a> Post<'a> {
  /// The post-state of `inline` bytes and `runs` as read, each the run `declared` names at its place.
  pub(crate) fn of(inline: &'a [u8], runs: &'a [Vec<u8>], declared: &'a RunTable) -> Post<'a> {
    Post {
      inline,
      runs,
      declared,
    }
  }

  /// The post-state of `inline` bytes alone, no run named.
  pub(crate) fn inline(inline: &'a [u8]) -> Post<'a> {
    Post::of(inline, &[], &NO_RUNS)
  }

  /// The inline bytes' length: where the first run starts.
  fn inline_len(&self) -> u64 {
    u64::try_from(self.inline.len()).unwrap_or(u64::MAX)
  }

  /// Its length: the inline bytes and every run's. A lookup.
  pub(crate) fn len(&self) -> u64 {
    self.inline_len().saturating_add(self.declared.total())
  }

  /// Each run by its place in the address space and its length, the first of runs alike: the run whose bytes are
  /// exactly the `len` bytes at `src` is the one under `(src, len)`, what a whole text's file is made of. Made once for
  /// a version's files (`Green::sole_runs`), so each file's lookup is a logarithm of the runs, not a walk of them all.
  pub(crate) fn run_places(&self) -> std::collections::BTreeMap<(u64, u64), usize> {
    let inline = self.inline_len();
    let mut places = std::collections::BTreeMap::new();
    for (at, run) in self.declared.iter().enumerate() {
      let start = inline.saturating_add(self.declared.start(at));
      places.entry((start, run.len)).or_insert(at);
    }
    places
  }

  /// The `len` bytes at `src`, clamped to its length (as a slice of one buffer was); a run's bytes not read span none.
  /// The first run it reaches is found by a halving search, then the runs it spans in turn.
  pub(crate) fn span(&self, src: u64, len: u64) -> std::borrow::Cow<'a, [u8]> {
    let end = src.saturating_add(len).min(self.len());
    let inline = self.inline_len();
    let mut pieces: Vec<&'a [u8]> = Vec::new();
    if src < inline && end > 0 {
      let from = usize::try_from(src).unwrap_or(usize::MAX);
      let to = usize::try_from(end.min(inline)).unwrap_or(usize::MAX);
      pieces.push(self.inline.get(from.min(to)..to).unwrap_or_default());
    }
    if end > inline {
      // The span in the runs' address space.
      let (from_run, to_run) = (src.saturating_sub(inline), end.saturating_sub(inline));
      let mut at = self.declared.first_ending_past(from_run);
      while let Some(run_end) = self.declared.ends.get(at).copied() {
        let start = self.declared.start(at);
        if start >= to_run {
          break;
        }
        let bytes: &'a [u8] = self.runs.get(at).map_or(&[], Vec::as_slice);
        let from = usize::try_from(from_run.saturating_sub(start)).unwrap_or(usize::MAX);
        let to = usize::try_from(to_run.min(run_end).saturating_sub(start)).unwrap_or(usize::MAX);
        let to = to.min(bytes.len());
        pieces.push(bytes.get(from.min(to)..to).unwrap_or_default());
        at = at.saturating_add(1);
      }
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
    self.runs.total()
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

#[cfg(test)]
mod tests {
  use super::{Identity, NamedRun, Post, RunTable};

  /// Goal 93's oracle for the indexed post-state ([`RunTable`]). Do: lay out inline bytes and runs of lengths 0, 1, 3,
  /// 0, 5 and 2 (the fourth left unread, so it spans none), and take every span `(src, len)` up to past the end. Expect
  /// each equal to the same range of the address space laid out byte by byte (an unread run's place holding no bytes),
  /// and the length the inline bytes and every declared run.
  #[test]
  fn every_span_of_an_indexed_post_state_is_its_bytes_laid_out() {
    let lens = [0u64, 1, 3, 0, 5, 2];
    let unread = 4;
    let inline: Vec<u8> = vec![200, 201];
    let mut table = RunTable::default();
    let mut read: Vec<Vec<u8>> = Vec::new();
    for (at, len) in lens.iter().enumerate() {
      table.push(NamedRun {
        identity: Identity {
          hash: [u8::try_from(at).unwrap(); 32],
          len: *len,
        },
        offset: 0,
        len: *len,
      });
      let bytes: Vec<u8> = (0..*len)
        .map(|byte| u8::try_from(at * 10).unwrap() + u8::try_from(byte).unwrap())
        .collect();
      read.push(if at == unread { Vec::new() } else { bytes });
    }
    let post = Post::of(&inline, &read, &table);
    let total = inline.len() as u64 + lens.iter().sum::<u64>();
    assert_eq!(post.len(), total);
    // The address space byte by byte: `None` where an unread run's place is.
    let mut laid: Vec<Option<u8>> = inline.iter().copied().map(Some).collect();
    for (at, len) in lens.iter().enumerate() {
      for byte in 0..*len {
        laid.push(read[at].get(usize::try_from(byte).unwrap()).copied());
      }
    }
    for src in 0..=total + 1 {
      for len in 0..=total + 1 {
        let end = (src + len).min(total);
        let expected: Vec<u8> = (src..end.max(src))
          .filter_map(|at| laid[usize::try_from(at).unwrap()])
          .collect();
        assert_eq!(
          post.span(src, len).as_ref(),
          expected.as_slice(),
          "span({src}, {len})"
        );
      }
    }
  }
}

//! A read-only file whose content is references into the shard's content store (A-645 P3 ruling 2; §4.5, A-64,
//! A-727): an ordered list of pieces, each an identity of the content store with its length, or a few bytes the
//! presenter made (an FSFS node-revision header, a directory's entry list, a pack's trailer). A hook's view of a
//! repository (svn's FSFS directory, git's bare repository) is a volume of such files, so every text the view shows is
//! the store's one copy of it, lent per read, and nothing is assembled into the volume.
//!
//! The volume never sees the content store. It reads a [`Piece::Content`] through [`ReferencedContent`], and holds
//! and releases one through [`ReferencedHolds`], seams the shard implements over its store (two, so a read borrows the
//! store shared and only a create or a release borrows it to change it) (the server's `ContentStore`, whose `resolve` turns an identity into its sealed
//! chunks); a test implements it over a map. A read asks the seam for the run of bytes at an offset of one identity
//! (`run_at`), copies that run into the caller's buffer, and asks again past it, so a read never needs an identity's
//! whole bytes in one place. A run the store could only answer by copying (`Cow::Owned`) is counted in
//! [`ReferencedCounts::bytes_copied`]; a lent one (`Cow::Borrowed`) is not. The P3 test holds `bytes_copied` at zero over
//! a view of N texts read whole, beside `bytes_read` equal to the view's length, so the zero is never vacuous.
//!
//! Holds. [`crate::Volume::create_referenced`] holds every content identity once through the seam before the file
//! exists (a refused hold fails the create with nothing held), and the volume keeps each hold in a ledger. The ledger
//! belongs to the volume, not the inode: an inode is copied on write and kept by snapshots under the epoch rule, so the
//! last record naming an identity is not a step the volume can see cheaply, while the volume's end is. The holds leave
//! at [`crate::Volume::release_references`], which the presenter calls before it destroys the volume; a destroy with
//! holds left is refused [`crate::VfsError::ReferencesHeld`], so a hold cannot leak by a forgotten release.
//!
//! Every change to a referenced file (a write, a truncate, an allocation, a punch) is refused
//! [`crate::VfsError::ReadOnly`] (`EROFS`): the bytes are the store's, shared by identity, and a view is regenerated,
//! never edited. Its length is the pieces' sum, known at creation, so a `stat` reads nothing.

use std::borrow::Cow;

use crate::error::VfsError;

/// An identity of the shard's content store: the 32-byte digest it names an object by (A-64).
pub type Identity = [u8; 32];

/// One piece of a referenced file's content, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
  /// The whole of the content-store object `id`, `len` bytes long.
  Content {
    /// The object's identity.
    id: Identity,
    /// The object's length, as the record naming it holds it: the store keeps no object's length, and finds a window
    /// of a long object down the tree it cut from that length.
    len: u64,
  },
  /// Bytes the presenter made, kept in the volume: small by construction (headers, entry lists, trailers).
  Made(Box<[u8]>),
}

impl Piece {
  /// The bytes this piece shows.
  pub fn len(&self) -> u64 {
    match self {
      Self::Content { len, .. } => *len,
      Self::Made(bytes) => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    }
  }

  /// Whether the piece shows no bytes.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

/// A referenced file's body ([`crate::inode::Body::Referenced`]): its pieces and, for each, the offset its bytes end
/// at, so the piece holding an offset is a binary search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferencedBody {
  pieces: Box<[Piece]>,
  ends: Box<[u64]>,
}

impl ReferencedBody {
  /// The body of `pieces`, or [`VfsError::FileTooLarge`] when their lengths sum past what a file addresses.
  pub fn new(pieces: Vec<Piece>) -> Result<Self, VfsError> {
    let mut ends = Vec::with_capacity(pieces.len());
    let mut end = 0u64;
    for piece in &pieces {
      end = end.checked_add(piece.len()).ok_or(VfsError::FileTooLarge)?;
      ends.push(end);
    }
    Ok(Self {
      pieces: pieces.into_boxed_slice(),
      ends: ends.into_boxed_slice(),
    })
  }

  /// The file's length: the pieces' sum.
  pub fn len(&self) -> u64 {
    self.ends.last().copied().unwrap_or(0)
  }

  /// Whether the file shows no bytes.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// The pieces, in file order.
  pub fn pieces(&self) -> &[Piece] {
    &self.pieces
  }

  /// The pieces `[off, off + want)` touches, each with its start in the file, in file order.
  pub(crate) fn overlapping(&self, off: u64, end: u64) -> impl Iterator<Item = (u64, &Piece)> {
    let first = self.ends.partition_point(|piece_end| *piece_end <= off);
    let starts = std::iter::once(0)
      .chain(self.ends.iter().copied())
      .skip(first);
    self
      .pieces
      .iter()
      .skip(first)
      .zip(starts)
      .take_while(move |(_, start)| *start < end)
      .map(|(piece, start)| (start, piece))
  }
}

/// The seam a volume reads a [`Piece::Content`] through, implemented by the shard over its content store.
pub trait ReferencedContent {
  /// The run of `id`'s bytes that begins at `offset` (below `len`, the object's length): at least one byte, as many as
  /// the store holds together there (a sealed chunk's window, from the offset to its end). Borrowed when the store lends
  /// its own bytes; owned when it could only copy them, which the volume counts. [`VfsError::ReferenceUnresolved`] when
  /// the store does not hold `id`, or holds no such byte of it.
  fn run_at(&self, id: &Identity, len: u64, offset: u64) -> Result<Cow<'_, [u8]>, VfsError>;
}

/// The seam a volume holds and releases a [`Piece::Content`]'s identity through, implemented by the shard over its
/// content store.
pub trait ReferencedHolds {
  /// Holds `id` so the store keeps its bytes until the matching [`ReferencedHolds::release`];
  /// [`VfsError::ReferenceUnresolved`] when the store does not hold it.
  fn hold(&mut self, id: &Identity) -> Result<(), VfsError>;

  /// Lets go of one hold of `id` that [`ReferencedHolds::hold`] took; [`VfsError::ReferenceUnresolved`] when the store
  /// holds no such hold.
  fn release(&mut self, id: &Identity) -> Result<(), VfsError>;
}

/// What referenced reads have done, over a volume's life: the non-vacuity counters of the lent path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReferencedCounts {
  /// Bytes referenced reads returned, made pieces and content pieces both.
  pub bytes_read: u64,
  /// Content bytes a read took from a run the store copied rather than lent.
  pub bytes_copied: u64,
}

/// Copies `[off, off + out.len())` of `body` into `out`, the made pieces from the volume and the content pieces through
/// `content`; the counts of what it read and what it took from copied runs.
pub(crate) fn read_into(
  body: &ReferencedBody,
  content: Option<&dyn ReferencedContent>,
  off: u64,
  out: &mut [u8],
) -> Result<ReferencedCounts, VfsError> {
  let end = off.saturating_add(u64::try_from(out.len()).unwrap_or(u64::MAX));
  let mut counts = ReferencedCounts::default();
  for (start, piece) in body.overlapping(off, end) {
    let piece_end = start.saturating_add(piece.len());
    let mut at = off.max(start);
    while at < piece_end.min(end) {
      let within = at.saturating_sub(start);
      let into = usize::try_from(at.saturating_sub(off)).map_err(|_| VfsError::FileTooLarge)?;
      let room = usize::try_from(piece_end.min(end).saturating_sub(at)).unwrap_or(usize::MAX);
      let written = match piece {
        Piece::Made(bytes) => {
          let from = usize::try_from(within).map_err(|_| VfsError::FileTooLarge)?;
          let run = bytes.get(from..).unwrap_or_default();
          copy_run(run, room, out, into)
        }
        Piece::Content { id, len } => {
          let content = content.ok_or(VfsError::ReferenceUnresolved)?;
          let run = content.run_at(id, *len, within)?;
          let written = copy_run(&run, room, out, into);
          if matches!(run, Cow::Owned(_)) {
            counts.bytes_copied = counts
              .bytes_copied
              .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
          }
          written
        }
      };
      if written == 0 {
        return Err(VfsError::ReferenceUnresolved);
      }
      at = at.saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
    }
  }
  counts.bytes_read = u64::try_from(out.len()).unwrap_or(u64::MAX);
  Ok(counts)
}

/// Copies at most `room` bytes of `run` into `out` from `into`: how many it copied.
fn copy_run(run: &[u8], room: usize, out: &mut [u8], into: usize) -> usize {
  let take = run.len().min(room);
  let Some(source) = run.get(..take) else {
    return 0;
  };
  let Some(target) = out.get_mut(into..into.saturating_add(take)) else {
    return 0;
  };
  target.copy_from_slice(source);
  take
}

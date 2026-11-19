//! Every object checked against its tool's id as it arrives, and held by the push that brought it until the verdict
//! commits (§4.17.1 "Objects" items 4 and 5; "Refusals (shared)", `ObjectIdMismatch { id }`; "Failure matrix").
//!
//! **Checked on arrival.** The server hashes each object as its tool does ([`Naming`]: git hashes
//! `<type> <length>\0<content>`, Mercurial the sorted parents and the text, and a Subversion text is checked by the
//! checksums its client sent) and compares the name with the id the client claimed. An object whose bytes do not hash
//! to it is refused, [`ArrivalRefused::IdMismatch`], before it is stored or indexed; the caller refuses its push and
//! abandons the push's quarantine, so nothing of the push is indexed. The same id brought twice with other bytes is a
//! collision of the tool's hash, refused by kind too.
//!
//! **Held by the push, not the repository.** An admitted object's bytes are content held by the push's
//! [`Quarantine`], as git's `receive-pack` keeps a push's objects in a quarantine directory that is "removed entirely"
//! when the push fails [B: git-receive-pack, "QUARANTINE ENVIRONMENT"]. The verdict's accept commits the quarantine
//! into the repository's index as one step ([`Quarantine::commit`], which checks every row before it applies any); a
//! refused, disconnected or abandoned push gives back every hold ([`Quarantine::abandon`]), and a content no other
//! holder names is freed with it.
//!
//! **The object row** (§4.17.1 "Objects" item 3: "an index from the tool's id to the object's kind, length and content
//! identity"): the key is the id, the row names the content, and the payload is the kind and the length
//! ([`object_payload`]), so a tool answers a size (git's `object-info`) without reading the bytes. A tool's own rows
//! (git's commit rows, Mercurial's revlog rows, Subversion's node rows) are its section's, kept by the same tables.

use std::collections::BTreeMap;
use std::mem::size_of;

use crate::content::{ContentId, ContentRefused, ContentStore};
use crate::index::{Batch, Index, IndexRefused, NewRow};
use crate::table::RowRef;

/// Format: an object row's payload: the kind, one byte, then the length, a little-endian `u64`.
pub const OBJECT_PAYLOAD_BYTES: usize = size_of::<u8>() + size_of::<u64>();

/// A tool's rule for naming an object (module docs).
pub trait Naming {
  /// The id the tool gives the object of `kind` whose raw bytes are `bytes`, with what else the tool hashes into it,
  /// `context` (Mercurial's parents and metadata block; nothing for git).
  fn name(&self, kind: u8, context: &[u8], bytes: &[u8]) -> Vec<u8>;
}

/// An object as its push brings it.
#[derive(Clone, Copy, Debug)]
pub struct Arriving<'a> {
  /// The id the client claims for it.
  pub id: &'a [u8],
  /// The tool's kind of object.
  pub kind: u8,
  /// What the tool hashes beside the bytes ([`Naming::name`]).
  pub context: &'a [u8],
  /// The raw bytes, kept as content.
  pub bytes: &'a [u8],
}

/// Why an arrival or a commit was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArrivalRefused {
  /// The object's bytes do not hash to the id the client claimed (§4.17.1's `ObjectIdMismatch`); nothing was stored.
  IdMismatch {
    /// The id claimed.
    id: Box<[u8]>,
  },
  /// The push brought the id once more with other bytes or another kind; nothing was stored.
  Collision {
    /// The id.
    id: Box<[u8]>,
  },
  /// The store refused the bytes (the budget); nothing was stored.
  Content(ContentRefused),
  /// The index refused the commit (a row it holds of one of the ids differs); nothing was committed.
  Index(IndexRefused),
}

/// What an admission did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admitted {
  /// The object is held now.
  New,
  /// The push had brought the same object already.
  Again,
}

/// The payload of an object row of `kind` and `len` bytes.
pub fn object_payload(kind: u8, len: u64) -> [u8; OBJECT_PAYLOAD_BYTES] {
  let mut payload = [0; OBJECT_PAYLOAD_BYTES];
  let (kind_byte, length) = payload.split_at_mut(size_of::<u8>());
  kind_byte.copy_from_slice(&[kind]);
  length.copy_from_slice(&len.to_le_bytes());
  payload
}

/// An object row read back: its kind, its length and its content, or `None` when the row is not one.
pub fn object_row(row: &RowRef<'_>) -> Option<(u8, u64, ContentId)> {
  let payload = <[u8; OBJECT_PAYLOAD_BYTES]>::try_from(row.payload()).ok()?;
  let (kind, length) = payload.split_first()?;
  let len = u64::from_le_bytes(<[u8; size_of::<u64>()]>::try_from(length).ok()?);
  let mut contents = row.contents();
  let content = contents.next()?;
  if contents.next().is_some() {
    return None;
  }
  Some((*kind, len, content))
}

/// An object a push brought: its kind, its content and its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Staged {
  kind: u8,
  content: ContentId,
  len: u64,
}

/// A push's objects, held until its verdict (module docs).
#[derive(Debug, Default)]
pub struct Quarantine {
  staged: BTreeMap<Box<[u8]>, Staged>,
}

impl Quarantine {
  /// An empty quarantine.
  pub fn new() -> Quarantine {
    Quarantine::default()
  }

  /// Checks an arriving object against its id by `naming`, then holds its bytes as content.
  pub fn admit(
    &mut self,
    store: &mut ContentStore,
    naming: &impl Naming,
    arriving: Arriving<'_>,
  ) -> Result<Admitted, ArrivalRefused> {
    if naming.name(arriving.kind, arriving.context, arriving.bytes) != arriving.id {
      return Err(ArrivalRefused::IdMismatch {
        id: Box::from(arriving.id),
      });
    }
    if let Some(staged) = self.staged.get(arriving.id) {
      return Quarantine::again(staged, arriving);
    }
    let len = u64::try_from(arriving.bytes.len())
      .map_err(|_| ArrivalRefused::Content(ContentRefused::CountExhausted))?;
    let content = store.put(arriving.bytes).map_err(ArrivalRefused::Content)?;
    self.staged.insert(
      Box::from(arriving.id),
      Staged {
        kind: arriving.kind,
        content,
        len,
      },
    );
    Ok(Admitted::New)
  }

  /// An id the push brought before: the same object, or a collision.
  fn again(staged: &Staged, arriving: Arriving<'_>) -> Result<Admitted, ArrivalRefused> {
    if staged.kind == arriving.kind && staged.content == ContentId::of(arriving.bytes) {
      return Ok(Admitted::Again);
    }
    Err(ArrivalRefused::Collision {
      id: Box::from(arriving.id),
    })
  }

  /// The objects held.
  pub fn len(&self) -> usize {
    self.staged.len()
  }

  /// Whether no object is held.
  pub fn is_empty(&self) -> bool {
    self.staged.is_empty()
  }

  /// The kind, content and length of the object held under `id`.
  pub fn object(&self, id: &[u8]) -> Option<(u8, ContentId, u64)> {
    self
      .staged
      .get(id)
      .map(|staged| (staged.kind, staged.content, staged.len))
  }

  /// Commits every object into `index` as one step, then gives back the quarantine's holds, leaving it empty. Refused,
  /// it commits nothing and keeps every object, for the caller to abandon.
  pub fn commit(
    &mut self,
    index: &mut Index,
    store: &mut ContentStore,
  ) -> Result<Batch, ArrivalRefused> {
    let named: Vec<([ContentId; 1], [u8; OBJECT_PAYLOAD_BYTES])> = self
      .staged
      .values()
      .map(|staged| ([staged.content], object_payload(staged.kind, staged.len)))
      .collect();
    let rows: Vec<NewRow<'_>> = self
      .staged
      .keys()
      .zip(&named)
      .map(|(key, (contents, payload))| NewRow {
        key,
        contents,
        payload,
      })
      .collect();
    let batch = index
      .insert_all(store, &rows)
      .map_err(ArrivalRefused::Index)?;
    self.abandon(store)?;
    Ok(batch)
  }

  /// Gives back every hold the quarantine took, leaving it empty; a content no one else names is freed. It goes on
  /// past a refusal and answers the first.
  pub fn abandon(&mut self, store: &mut ContentStore) -> Result<(), ArrivalRefused> {
    let staged = std::mem::take(&mut self.staged);
    let mut first = Ok(());
    for object in staged.values() {
      first = first.and(store.release(&object.content).map(drop));
    }
    first.map_err(ArrivalRefused::Content)
  }
}

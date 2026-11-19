//! Every object checked against its tool's id as it arrives, and held by the push that brought it until the verdict
//! commits (§4.17.1 "Objects" items 4 and 5; "Refusals (shared)", `ObjectIdMismatch { id }`; "Failure matrix").
//!
//! **Checked on arrival.** The server hashes each object as its tool does ([`Naming`], a parameter: git's SHA-1 with
//! collision detection or SHA-256 of `<type> <length>\0<content>`, `slates_git::object_name::name_of`; Mercurial's
//! SHA-1 of the sorted parents and the text; a Subversion text by the checksums its client sent) and compares the name
//! with the id the client claimed. An object whose bytes do not hash to it is refused, [`AdmitRefused::IdMismatch`],
//! before a byte of it is stored; one the tool will not name at all is refused with the tool's own reason,
//! [`AdmitRefused::NotNamed`] (git's SHA-1 refuses an input its collision detection finds to be part of a known attack,
//! "SHA-1 appears to be part of a collision attack", and names its plain SHA-1, which the server's answer carries). The
//! caller refuses the push and abandons its quarantine, so nothing of the push is indexed. The same id brought twice
//! with other bytes is a collision of the tool's hash, refused by kind too.
//!
//! **Held by the push, not the repository.** An admitted object's bytes are content held by the push's
//! [`Quarantine`] ([`ContentStore::put_object`]: windows, and lists naming them past one window), as git's
//! `receive-pack` keeps a push's objects in a quarantine directory that is "removed entirely" when the push fails [B:
//! git-receive-pack, "QUARANTINE ENVIRONMENT"]. The verdict's accept commits the quarantine into the repository's index
//! as one step ([`Quarantine::commit`], which checks every row before it applies any); a refused, disconnected or
//! abandoned push gives back every hold ([`Quarantine::abandon`]), and a content no other holder names is freed with
//! it.
//!
//! **The object row** (§4.17.1 "Objects" item 3: "an index from the tool's id to the object's kind, length and content
//! identity"): the key is the id, the row names the object's root content, and the payload is the kind and the length
//! ([`object_payload`]), so a tool answers a size (git's `object-info`) without reading the bytes. A tool's own rows
//! (git's commit rows, Mercurial's revlog rows, Subversion's node rows) are its section's, kept by the same tables.

use std::collections::BTreeMap;
use std::mem::size_of;

use crate::content::{ContentId, ContentRefused, ContentStore};
use crate::index::{Batch, Index, IndexRefused, NewRow};
use crate::table::RowRef;

/// Format: an object row's payload: the kind, one byte, then the length, a little-endian `u64`.
pub const OBJECT_PAYLOAD_BYTES: usize = size_of::<u8>() + size_of::<u64>();

/// A tool's rule for naming an object (module docs): the parameter every arrival is checked by, never one hash.
pub trait Naming {
  /// The tool's id, as the bytes a client claims it by.
  type Id: AsRef<[u8]>;
  /// Why the tool refuses to name bytes at all: git's `CollisionAttack`; a tool whose hash never refuses names
  /// `core::convert::Infallible`.
  type Refused;

  /// The id the tool gives the object of `kind` whose raw bytes are `bytes`, with what else the tool hashes into it,
  /// `context` (Mercurial's parents and metadata block; nothing for git).
  fn name(&self, kind: u8, context: &[u8], bytes: &[u8]) -> Result<Self::Id, Self::Refused>;
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

/// Why an arriving object was refused; nothing of it was stored. `R` is the tool's own reason ([`Naming::Refused`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmitRefused<R> {
  /// The object's bytes do not hash to the id the client claimed (§4.17.1's `ObjectIdMismatch`).
  IdMismatch {
    /// The id claimed.
    id: Box<[u8]>,
  },
  /// The tool refused to name the bytes at all, for its own `reason` (git: a known collision attack).
  NotNamed {
    /// The id claimed.
    id: Box<[u8]>,
    /// The tool's reason.
    reason: R,
  },
  /// The push brought the id once more with other bytes or another kind.
  Collision {
    /// The id.
    id: Box<[u8]>,
  },
  /// The store refused the bytes (the budget).
  Content(ContentRefused),
}

/// Why a quarantine's commit was refused; nothing was committed and the quarantine still holds every object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitRefused {
  /// The index refused (a row it holds of one of the ids differs, or the budget).
  Index(IndexRefused),
  /// Giving back the quarantine's holds after the commit met a content the store does not hold: the store and the
  /// quarantine disagree. The rows are committed.
  Content(ContentRefused),
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

/// An object row read back: its kind, its length and its root content, or `None` when the row is not one.
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

/// An object a push brought: its kind, its root content and its length.
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
  pub fn admit<N: Naming>(
    &mut self,
    store: &mut ContentStore,
    naming: &N,
    arriving: Arriving<'_>,
  ) -> Result<Admitted, AdmitRefused<N::Refused>> {
    let named = naming
      .name(arriving.kind, arriving.context, arriving.bytes)
      .map_err(|reason| AdmitRefused::NotNamed {
        id: Box::from(arriving.id),
        reason,
      })?;
    if named.as_ref() != arriving.id {
      return Err(AdmitRefused::IdMismatch {
        id: Box::from(arriving.id),
      });
    }
    self.stage(store, arriving)
  }

  /// Holds bytes its caller has already named by its tool's rule, under that name: a tool's store that names each object
  /// itself as it arrives (git's, A-614, which hashes each of a pack's objects once and checks a claimed id against
  /// that one name). It is [`Quarantine::admit`] after its check: an id the push brought before is `Again` for the same
  /// bytes and a `Collision` for others, and the budget refuses by kind.
  pub fn hold_named(
    &mut self,
    store: &mut ContentStore,
    id: &[u8],
    kind: u8,
    bytes: &[u8],
  ) -> Result<Admitted, AdmitRefused<core::convert::Infallible>> {
    self.stage(
      store,
      Arriving {
        id,
        kind,
        context: &[],
        bytes,
      },
    )
  }

  /// Holds a named object's bytes, or answers an id the push brought before.
  fn stage<R>(
    &mut self,
    store: &mut ContentStore,
    arriving: Arriving<'_>,
  ) -> Result<Admitted, AdmitRefused<R>> {
    if let Some(staged) = self.staged.get(arriving.id) {
      return Quarantine::again(store, staged, arriving);
    }
    let len = u64::try_from(arriving.bytes.len())
      .map_err(|_| AdmitRefused::Content(ContentRefused::CountExhausted))?;
    let content = store
      .put_object(arriving.bytes)
      .map_err(AdmitRefused::Content)?;
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
  fn again<R>(
    store: &ContentStore,
    staged: &Staged,
    arriving: Arriving<'_>,
  ) -> Result<Admitted, AdmitRefused<R>> {
    if staged.kind == arriving.kind && staged.content == store.object_id(arriving.bytes) {
      return Ok(Admitted::Again);
    }
    Err(AdmitRefused::Collision {
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

  /// The kind, root content and length of the object held under `id`.
  pub fn object(&self, id: &[u8]) -> Option<(u8, ContentId, u64)> {
    self
      .staged
      .get(id)
      .map(|staged| (staged.kind, staged.content, staged.len))
  }

  /// Every object held, in its ids' order: its id, kind, root content and length. A tool's own index writes its rows
  /// from these (git's object and commit rows, A-614), then abandons the quarantine to give its holds back.
  pub fn objects(&self) -> impl Iterator<Item = (&[u8], u8, ContentId, u64)> + '_ {
    self
      .staged
      .iter()
      .map(|(id, staged)| (&**id, staged.kind, staged.content, staged.len))
  }

  /// Commits every object into `index` as one step, then gives back the quarantine's holds, leaving it empty. Refused
  /// by the index, it commits nothing and keeps every object, for the caller to abandon.
  pub fn commit(
    &mut self,
    index: &mut Index,
    store: &mut ContentStore,
  ) -> Result<Batch, CommitRefused> {
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
      .map_err(CommitRefused::Index)?;
    self.abandon(store).map_err(CommitRefused::Content)?;
    Ok(batch)
  }

  /// Gives back every hold the quarantine took, leaving it empty; a content no one else names is freed. It goes on
  /// past a refusal and answers the first.
  pub fn abandon(&mut self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    let staged = std::mem::take(&mut self.staged);
    let mut first = Ok(());
    for object in staged.values() {
      first = first.and(store.release(&object.content).map(drop));
    }
    first
  }
}

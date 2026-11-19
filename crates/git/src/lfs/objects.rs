//! A repository's LFS objects in the version-control servers' one object store (A-629, part 1;
//! §4.17.2 "Large files"; `slates-vcs`, A-581).
//!
//! **Named by SHA-256, checked on arrival.** An LFS object's id is the SHA-256 of its bytes and
//! nothing else ([`LfsNaming`]; spec.md: the oid is "prefixed by its hashing method", and the
//! clean filter streams the content "while calculating its SHA-256 signature" \[B\]). An upload
//! is admitted through `slates-vcs`'s quarantine with that naming, so bytes that do not hash to
//! the oid the client named are refused ([`ObjectRefused::Mismatch`]) and nothing of them is kept;
//! bytes that do are committed as one row of the repository's LFS index, under the oid, naming
//! their content. The content is the shared store's, by BLAKE3 identity, one copy however many
//! repositories and tools hold it, cut into the store's windows as any long object is.
//!
//! **One index of its own.** A repository's LFS objects have an index apart from its git
//! objects', keyed by the oid's 32 bytes: the two id spaces never meet, and an LFS object is no
//! git object.
//!
//! **Bounds.** Every byte is the shared store's, charged to its derived budget; an upload past it
//! is refused typed ([`ObjectRefused::Store`]). An upload is admitted whole, as `slates-vcs` puts
//! an object: a streaming put, window by window as the body arrives, is owed to that crate.

use slates_vcs::{
  AdmitRefused, Admitted, Arriving, CommitRefused, ContentRefused, ContentStore, Index,
  IndexRefused, Naming, Quarantine, object_row,
};

use super::Oid;

/// Format: the kind number an LFS object's row carries. LFS has one kind of object, and its rows
/// live in an index of their own, so the number only marks the row as LFS's.
pub const LFS_KIND: u8 = 0;

/// LFS's naming: an object's id is the SHA-256 of its bytes, nothing else hashed beside them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LfsNaming;

impl LfsNaming {
  /// The oid of `bytes`.
  pub fn oid(bytes: &[u8]) -> Oid {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    let mut oid = [0u8; 32];
    for (slot, byte) in oid.iter_mut().zip(digest.as_ref()) {
      *slot = *byte;
    }
    Oid::from_bytes(oid)
  }
}

impl Naming for LfsNaming {
  type Id = Oid;
  type Refused = core::convert::Infallible;

  fn name(&self, _kind: u8, _context: &[u8], bytes: &[u8]) -> Result<Oid, Self::Refused> {
    Ok(Self::oid(bytes))
  }
}

/// Why an upload was not kept. Nothing of it is held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectRefused {
  /// The bytes do not hash to the oid the client named.
  Mismatch,
  /// The shared store refused to hold them: past its budget, or a count exhausted.
  Store(ContentRefused),
  /// The index refused the row: past the budget, or a row of the oid that differs.
  Index(IndexRefused),
}

/// A repository's LFS objects: an index from oid to the object's row.
#[derive(Debug, Default)]
pub struct LfsObjects {
  /// The rows, keyed by the oid's bytes.
  index: Index,
}

impl LfsObjects {
  /// A repository holding no LFS object.
  pub fn new() -> Self {
    Self {
      index: Index::new(),
    }
  }

  /// The size of the object `oid`, when the repository holds it.
  pub fn held(&self, store: &ContentStore, oid: &Oid) -> Option<u64> {
    let row = self.index.get(store, oid.as_bytes())?;
    object_row(&row).map(|(_, len, _)| len)
  }

  /// The bytes of the object `oid`, when the repository holds it.
  pub fn bytes(&self, store: &ContentStore, oid: &Oid) -> Option<Vec<u8>> {
    let row = self.index.get(store, oid.as_bytes())?;
    let (_, len, root) = object_row(&row)?;
    store.object_bytes(&root, len)
  }

  /// Keeps `bytes` as the object `oid`: admitted through a quarantine with [`LfsNaming`], so bytes
  /// that do not hash to `oid` are refused, then committed as the object's row. An object the
  /// repository holds already is kept as it is.
  pub fn admit(
    &mut self,
    store: &mut ContentStore,
    oid: &Oid,
    bytes: &[u8],
  ) -> Result<Admitted, ObjectRefused> {
    if self.held(store, oid).is_some() {
      return Ok(Admitted::Again);
    }
    let mut quarantine = Quarantine::new();
    let arriving = Arriving {
      id: oid.as_bytes(),
      kind: LFS_KIND,
      context: &[],
      bytes,
    };
    let admitted = match quarantine.admit(store, &LfsNaming, arriving) {
      Ok(admitted) => admitted,
      // A fresh quarantine holding one object cannot meet its id twice; were it to, the bytes
      // would not be the oid's, which is a mismatch.
      Err(AdmitRefused::IdMismatch { .. } | AdmitRefused::Collision { .. }) => {
        return Err(ObjectRefused::Mismatch);
      }
      Err(AdmitRefused::Content(refused)) => return Err(ObjectRefused::Store(refused)),
      Err(AdmitRefused::NotNamed { reason, .. }) => match reason {},
    };
    match quarantine.commit(&mut self.index, store) {
      Ok(_) => Ok(admitted),
      Err(CommitRefused::Content(refused)) => Err(ObjectRefused::Store(refused)),
      Err(CommitRefused::Index(refused)) => {
        // The rows were not written: give the staged content back, so nothing of it is held.
        quarantine.abandon(store).map_err(ObjectRefused::Store)?;
        Err(ObjectRefused::Index(refused))
      }
    }
  }

  /// The objects held.
  pub fn len(&self) -> u64 {
    self.index.rows()
  }

  /// Whether no object is held.
  pub fn is_empty(&self) -> bool {
    self.index.rows() == 0
  }
}

#[cfg(test)]
#[path = "../tests/lfs_objects.rs"]
mod tests;

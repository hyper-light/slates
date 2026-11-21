//! A Subversion repository's FSFS view as a slates volume (A-645 P3; A-738's `slates_svn::fsfs`; ruling 2's referenced
//! files, `slates_vfs::referenced`): the directory a hook is handed as `REPOS-PATH` and reads with stock `svnlook`.
//!
//! **What it makes.** Into a volume of its own, the view's directories and fixed files (`format`, `db/format`,
//! `db/uuid`, `db/current`, ...), each revision's file and each revision's properties file, as `fsfs` lays them out.
//! A revision file is a referenced file: its made bytes (headers, node-revisions, lists, the changed paths, the
//! trailer) are kept in the volume, and each text is a piece naming the text by its identity (the BLAKE3 of its bytes,
//! the content store's identity too) and its length, so no text is copied into the volume. Every text is shown whole
//! (`PLAIN`): the records' own deltas are no content a piece can name, and per-file deltas become derived forms of the
//! changeset log (Ada's ruling of 2026-11-20 ~13:35), so `DELTA` representations wait for those.
//!
//! **Where a text is read.** [`ViewTexts`] answers a referenced read from the repository's own text source
//! ([`super::svn_texts::HeadTexts`]): the green's held head text, else the content store's window (a text a commit
//! stored, A-761), else the records' delta window. [`ViewHolds`] holds each text the content store keeps for as long as
//! the view's volume holds the file, through the store's `hold` and `release`; a text the records keep lives as long as
//! the repository's records do, which outlive its view.
//!
//! **Growing.** [`Presented::advance`] lays out the revisions the records hold past those already shown, at most a
//! budget of node-revisions a call, writes their files, and then rewrites `db/current`, so a reader never sees a
//! youngest revision whose file is missing. [`Presented::revision_properties_changed`] rewrites one revision's
//! properties file after a revision property change.

use std::borrow::Cow;

use slates_svn::fsfs::{self, Advance, FilePiece, View, ViewRefusal};
use slates_svn::index::Revision;
use slates_svn::migration::{Repository as SvnRecords, TextSource};
use slates_vcs::{ContentId, ContentStore};
use slates_vfs::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::referenced::{Identity, Piece, ReferencedContent, ReferencedHolds};
use slates_vfs::volume::{Store, Volume};

use super::svn_texts::{HeadTexts, SvnView};

/// Format: the mode of every directory of the view: read and search for its user, nothing for others.
const DIRECTORY_MODE: u32 = 0o700;

/// Format: the mode of every file of the view: read-only for its user, as FSFS keeps its revision files.
const FILE_MODE: u32 = 0o400;

/// Why the view could not be presented.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PresentRefusal {
  /// The view could not be laid out or read from the records.
  View(ViewRefusal),
  /// The volume refused a step.
  Volume(VfsError),
  /// A revision file named a stored delta, which no piece can name (the source answered one; `WithTexts` never does).
  DeltaPiece,
}

impl From<ViewRefusal> for PresentRefusal {
  fn from(refusal: ViewRefusal) -> Self {
    Self::View(refusal)
  }
}

impl From<VfsError> for PresentRefusal {
  fn from(refusal: VfsError) -> Self {
    Self::Volume(refusal)
  }
}

/// A repository's view as presented so far: its layout, and how many revisions' files the volume holds.
#[derive(Debug, Default)]
pub(crate) struct Presented {
  view: View,
  written: Revision,
}

impl Presented {
  /// Nothing presented yet.
  pub(crate) fn new() -> Self {
    Self::default()
  }

  /// The revisions whose files the volume holds: revision 0 through one before this.
  pub(crate) fn written(&self) -> Revision {
    self.written
  }

  /// Lays out the records' revisions past those already shown, at most `budget` node-revisions this call, and writes
  /// them into `volume` beneath its root `root` (module docs, "Growing"). Each new revision file's texts are held
  /// through `holds`. Done when every revision of the records is shown.
  pub(crate) fn advance(
    &mut self,
    (volume, store): (&mut Volume, &mut Store),
    root: InodeNo,
    source: &SvnView<'_>,
    holds: &mut dyn ReferencedHolds,
    budget: usize,
  ) -> Result<Advance, PresentRefusal> {
    let records = source.records;
    let laid = self.view.advance(records.index(), source, budget)?;
    let shown = self.view.laid_out();
    let Some(youngest) = shown.checked_sub(1) else {
      return Ok(laid);
    };
    for directory in fsfs::directories(youngest) {
      directory_at(volume, store, root, &directory)?;
    }
    while self.written < shown {
      let revision = self.written;
      let file = self.view.revision_file(records.index(), source, revision)?;
      let pieces = file
        .pieces()
        .map(|piece| match piece {
          FilePiece::Made(bytes) => Ok(Piece::Made(bytes.into())),
          FilePiece::Text { identity, size } => Ok(Piece::Content {
            id: identity,
            len: size,
          }),
          FilePiece::Delta { .. } => Err(PresentRefusal::DeltaPiece),
        })
        .collect::<Result<Vec<_>, _>>()?;
      let path = fsfs::revision_file_path(revision);
      let (parent, name) = parent_of(volume, store, root, &path)?;
      volume.create_referenced(store, parent, name, FILE_MODE, pieces, holds)?;
      let properties = fsfs::revision_properties_file(source, revision)?;
      put_file(
        volume,
        store,
        root,
        &fsfs::revision_properties_path(revision),
        &properties,
      )?;
      self.written = revision.saturating_add(1);
    }
    for (path, bytes) in fsfs::fixed_files(records.uuid().as_bytes(), youngest) {
      put_file(volume, store, root, path, &bytes)?;
    }
    Ok(laid)
  }

  /// Rewrites revision `revision`'s properties file from the records, after a revision property change; nothing for a
  /// revision not yet shown.
  pub(crate) fn revision_properties_changed(
    &self,
    (volume, store): (&mut Volume, &mut Store),
    root: InodeNo,
    source: &SvnView<'_>,
    revision: Revision,
  ) -> Result<(), PresentRefusal> {
    if revision >= self.written {
      return Ok(());
    }
    let properties = fsfs::revision_properties_file(source, revision)?;
    put_file(
      volume,
      store,
      root,
      &fsfs::revision_properties_path(revision),
      &properties,
    )
  }
}

/// The directory `path` names beneath `root`, made where missing, component by component.
fn directory_at(
  volume: &mut Volume,
  store: &mut Store,
  root: InodeNo,
  path: &str,
) -> Result<InodeNo, PresentRefusal> {
  let mut at = root;
  for name in path.split('/').filter(|name| !name.is_empty()) {
    at = match volume.lookup_no(store, at, name) {
      Ok(found) => found.inode,
      Err(VfsError::NotFound) => volume.mkdir_no(store, at, name, DIRECTORY_MODE)?,
      Err(refusal) => return Err(refusal.into()),
    };
  }
  Ok(at)
}

/// The directory holding `path` beneath `root` (made where missing) and the file's own name.
fn parent_of<'p>(
  volume: &mut Volume,
  store: &mut Store,
  root: InodeNo,
  path: &'p str,
) -> Result<(InodeNo, &'p str), PresentRefusal> {
  let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
  Ok((directory_at(volume, store, root, directory)?, name))
}

/// Makes the file `path` beneath `root` hold exactly `bytes`: made where missing, else rewritten and cut to them.
fn put_file(
  volume: &mut Volume,
  store: &mut Store,
  root: InodeNo,
  path: &str,
  bytes: &[u8],
) -> Result<(), PresentRefusal> {
  let (parent, name) = parent_of(volume, store, root, path)?;
  let file = match volume.lookup_no(store, parent, name) {
    Ok(found) => found.inode,
    Err(VfsError::NotFound) => volume.create_file_no(store, parent, name, FILE_MODE)?,
    Err(refusal) => return Err(refusal.into()),
  };
  let len = u64::try_from(bytes.len()).map_err(|_| VfsError::FileTooLarge)?;
  volume.truncate(store, file, len)?;
  let mut at = 0usize;
  while at < bytes.len() {
    let offset = u64::try_from(at).map_err(|_| VfsError::FileTooLarge)?;
    let wrote = volume.write(store, file, offset, bytes.get(at..).unwrap_or_default())?;
    if wrote == 0 {
      return Err(VfsError::NoSpace.into());
    }
    at = at.saturating_add(wrote);
  }
  Ok(())
}

/// A view's referenced read (module docs, "Where a text is read"): a text's run from the repository's text source.
pub(crate) struct ViewTexts<'a>(pub(crate) &'a HeadTexts<'a>);

impl ReferencedContent for ViewTexts<'_> {
  fn run_at(&self, id: &Identity, len: u64, offset: u64) -> Result<Cow<'_, [u8]>, VfsError> {
    let room = usize::try_from(
      len
        .checked_sub(offset)
        .filter(|room| *room > 0)
        .ok_or(VfsError::ReferenceUnresolved)?,
    )
    .unwrap_or(usize::MAX);
    let run = self
      .0
      .text_at(id, offset)
      .ok_or(VfsError::ReferenceUnresolved)?;
    let run = match run {
      Cow::Borrowed(bytes) => Cow::Borrowed(bytes.get(..room.min(bytes.len())).unwrap_or_default()),
      Cow::Owned(mut bytes) => {
        bytes.truncate(room);
        Cow::Owned(bytes)
      }
    };
    if run.is_empty() {
      return Err(VfsError::ReferenceUnresolved);
    }
    Ok(run)
  }
}

/// A view's holds (module docs, "Where a text is read"): a text the content store keeps held there; one the records
/// keep needs none.
pub(crate) struct ViewHolds<'a> {
  /// The shard's content store, where a commit's texts are kept; none where the shard has none.
  pub(crate) store: Option<&'a mut ContentStore>,
  /// The repository's records, which say whether the store keeps a text.
  pub(crate) records: &'a SvnRecords,
}

impl ReferencedHolds for ViewHolds<'_> {
  fn hold(&mut self, id: &Identity) -> Result<(), VfsError> {
    if self.records.stored_len(id).is_none() {
      return Ok(());
    }
    self
      .store
      .as_deref_mut()
      .ok_or(VfsError::ReferenceUnresolved)?
      .hold(&ContentId(*id))
      .map_err(|_| VfsError::ReferenceUnresolved)
  }

  fn release(&mut self, id: &Identity) -> Result<(), VfsError> {
    if self.records.stored_len(id).is_none() {
      return Ok(());
    }
    self
      .store
      .as_deref_mut()
      .ok_or(VfsError::ReferenceUnresolved)?
      .release(&ContentId(*id))
      .map(|_| ())
      .map_err(|_| VfsError::ReferenceUnresolved)
  }
}

#[cfg(test)]
#[path = "svn_presenter/tests.rs"]
mod tests;

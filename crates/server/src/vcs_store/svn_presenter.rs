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
//! **Nothing in the chunk store.** Every file of the view is a referenced file, the small ones a single made piece, so
//! the volume holds no content in the shard's chunk store: a read of it is served while that store is lent to the
//! content store's windows (A-727 a2), the vfs reading only the volume's inodes and directories, and the texts read
//! through [`ViewTexts`].
//!
//! **Growing.** [`Presented::advance`] lays out the revisions the records hold past those already shown, at most a
//! budget of node-revisions a call, writes their files, and then rewrites `db/current`, so a reader never sees a
//! youngest revision whose file is missing. [`Presented::revision_properties_changed`] rewrites one revision's
//! properties file after a revision property change.
//!
//! **A hook's first view.** A hook's `REPOS-PATH` waits for the view to be laid out and its properties refreshed, work
//! that grows with the repository. So the hook's shard drives it as turns on the owner shard ([`view_turn`]), each at
//! most the owner's step quantum and each answering how far the view got, and judges the wait on that progress: a turn
//! that answers has moved the view on; a turn the owner does not answer within the liveness window of its own time, or
//! one that answers having moved nothing, refuses the hook typed. No bound is set on the whole, whose length is the
//! repository's. Until 2026-11-21 the whole first view had one liveness budget of the owner's time, so every repository
//! past what a second lays out refused its hooks (w-interop's svn matrix, git/git's newest 2,000 revisions: "its owner
//! shard did not answer in time").

use std::borrow::Cow;
use std::collections::BTreeSet;

use slates_svn::fsfs::{self, Advance, FilePiece, View, ViewRefusal};
use slates_svn::index::Revision;
use slates_svn::migration::TextSource;
use slates_vcs::{ContentId, ContentStore};
use slates_vfs::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::referenced::{Identity, Piece, ReferencedContent, ReferencedHolds};
use slates_vfs::volume::{Store, Volume};

use super::svn_texts::{HeadTexts, SvnView};

/// Format: the mode of every directory of the view: read and search for all, written by none (the view is read-only;
/// `svnadmin create`'s directories are `0755`).
const DIRECTORY_MODE: u32 = 0o755;

/// Format: the mode of every file of the view: read-only for all, as FSFS keeps its revision files (`0444`).
const FILE_MODE: u32 = 0o444;

/// Format: why a configured hook is refused where no view can be shown (A-645 (a)): a hook is shown its repository as a
/// view slates mounts in its store's granted directory, and this daemon was given none (`--vcs-store`). Answered with
/// `SVN_ERR_UNSUPPORTED_FEATURE`, as the other hooks this server cannot run are; git's push says it in its reason.
pub(crate) const UNGRANTED_WORDS: &str = "This server cannot run this repository's hooks: it holds no standing grant to show a hook its repository under (start it with --vcs-store)";

/// Where a shard's hooks are shown their repository (`ShardState::hook_views`, A-645 (a)).
#[derive(Clone, Debug, Default)]
pub(crate) enum HookViews {
  /// No store directory was granted: no view can be shown, and a configured hook is refused, typed
  /// ([`UNGRANTED_WORDS`]).
  #[default]
  Ungranted,
  /// The store's granted directory: the repository's view is presented and mounted there, read-only, and that mount is
  /// the hook's `REPOS-PATH`.
  Granted(&'static slates_land::store_root::StoreRoot),
  /// A directory the caller made (the in-process hook tests' stand-in for a mounted view,
  /// `Daemon::record_root_mount`): `REPOS-PATH` is the repository's name beneath it.
  Given(std::path::PathBuf),
}

impl HookViews {
  /// Where hooks are shown their repository for a daemon whose store's granted directory is `root`.
  pub(crate) fn of(root: Option<&'static slates_land::store_root::StoreRoot>) -> Self {
    root.map_or(Self::Ungranted, Self::Granted)
  }

  /// Whether a hook can be shown its repository at all.
  pub(crate) fn shows(&self) -> bool {
    !matches!(self, Self::Ungranted)
  }
}

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

/// What [`Presented::lay_out`] read from the records, for [`Presented::write`].
#[derive(Debug)]
pub(crate) struct Laid {
  /// Whether every revision is laid out.
  advance: Advance,
  /// The revisions laid out: revision 0 through one before this.
  shown: Revision,
  /// Each new revision file's path and pieces.
  files: Vec<(String, Vec<Piece>)>,
  /// Each small file's path and bytes, in the order they are written: the new revision properties files, then the
  /// fixed files, `db/current` among them.
  small: Vec<(String, Vec<u8>)>,
  /// The texts the new files name that the content store keeps, which their holds are taken in.
  stored: BTreeSet<Identity>,
}

impl Laid {
  /// The texts the new files name that the content store keeps.
  pub(crate) fn stored(&self) -> &BTreeSet<Identity> {
    &self.stored
  }
}

/// A repository's view as presented so far: its layout, and how many revisions' files the volume holds.
#[derive(Debug, Default)]
pub(crate) struct Presented {
  view: View,
  written: Revision,
  /// The bytes of the revision files written so far, the texts they name counted in them: the view's content, which
  /// its volume's ceiling is derived from ([`Presented::size_for`]).
  bytes: u64,
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
  /// them into `volume` beneath its root `root` (module docs, "Growing"): [`Presented::lay_out`] then
  /// [`Presented::write`]. Each new revision file's texts are held through `holds`. Done when every revision of the
  /// records is shown.
  pub(crate) fn advance(
    &mut self,
    (volume, store): (&mut Volume, &mut Store),
    root: InodeNo,
    source: &SvnView<'_>,
    holds: &mut dyn ReferencedHolds,
    budget: usize,
  ) -> Result<Advance, PresentRefusal> {
    let laid = self.lay_out(source, budget)?;
    self.write((volume, store), root, laid, holds)
  }

  /// The first half of [`Presented::advance`], which reads the records alone: the next revisions laid out, at most
  /// `budget` node-revisions, and what their files and the fixed files are to hold.
  pub(crate) fn lay_out(
    &mut self,
    source: &SvnView<'_>,
    budget: usize,
  ) -> Result<Laid, PresentRefusal> {
    let records = source.records;
    let advance = self.view.advance(records.index(), source, budget)?;
    let shown = self.view.laid_out();
    let mut laid = Laid {
      advance,
      shown,
      files: Vec::new(),
      small: Vec::new(),
      stored: BTreeSet::new(),
    };
    let Some(youngest) = shown.checked_sub(1) else {
      return Ok(laid);
    };
    for revision in self.written..shown {
      let file = self.view.revision_file(records.index(), source, revision)?;
      let mut pieces = Vec::new();
      for piece in file.pieces() {
        pieces.push(match piece {
          FilePiece::Made(bytes) => Piece::Made(bytes.into()),
          FilePiece::Text { identity, size } => {
            if records.stored_len(&identity).is_some() {
              laid.stored.insert(identity);
            }
            Piece::Content {
              id: identity,
              len: size,
            }
          }
          FilePiece::Delta { .. } => return Err(PresentRefusal::DeltaPiece),
        });
      }
      laid
        .files
        .push((fsfs::revision_file_path(revision), pieces));
      laid.small.push((
        fsfs::revision_properties_path(revision),
        fsfs::revision_properties_file(source, revision)?,
      ));
    }
    for (path, bytes) in fsfs::fixed_files(records.uuid().as_bytes(), youngest) {
      laid.small.push((path.to_owned(), bytes));
    }
    Ok(laid)
  }

  /// The second half of [`Presented::advance`], which touches the volume and the holds alone: `laid`'s directories,
  /// revision files and revision properties files made, then its fixed files, `db/current` last of all.
  pub(crate) fn write(
    &mut self,
    (volume, store): (&mut Volume, &mut Store),
    root: InodeNo,
    laid: Laid,
    holds: &mut dyn ReferencedHolds,
  ) -> Result<Advance, PresentRefusal> {
    let Some(youngest) = laid.shown.checked_sub(1) else {
      return Ok(laid.advance);
    };
    let directories = fsfs::directories(youngest);
    self.size_for(volume, &laid, &directories)?;
    for directory in directories {
      directory_at(volume, store, root, &directory)?;
    }
    for (path, pieces) in laid.files {
      let (parent, name) = parent_of(volume, store, root, &path)?;
      let made = volume.create_referenced(store, parent, name, FILE_MODE, pieces, holds)?;
      owned_as_root(volume, store, root, made)?;
    }
    for (path, bytes) in &laid.small {
      put_file(volume, store, root, path, bytes)?;
    }
    self.written = laid.shown;
    Ok(laid.advance)
  }

  /// Sizes the view's volume to its own content before `laid` is written (main's ruling of 2026-11-20: never a ceiling of
  /// its own, R3): its bytes the files it shows (every revision file's length, the texts it names counted in them, and
  /// the small files'), at least what it holds; its inodes and entries those it holds and, at most, one more for each
  /// directory and file `laid` makes. Until then the view was made with a ceiling of nothing and could hold no file
  /// (`Volume(NoSpace)`, w-interop's matrix).
  fn size_for(
    &mut self,
    volume: &mut Volume,
    laid: &Laid,
    directories: &[String],
  ) -> Result<(), PresentRefusal> {
    let files: u64 = laid
      .files
      .iter()
      .map(|(_, pieces)| pieces.iter().map(Piece::len).sum::<u64>())
      .sum();
    let small = laid
      .small
      .iter()
      .map(|(_, bytes)| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
      .fold(0u64, u64::saturating_add);
    self.bytes = self.bytes.saturating_add(files);
    let held = volume.accounting().referenced_bytes;
    volume.resize(self.bytes.saturating_add(small).max(held))?;
    let made = u64::try_from(
      directories
        .len()
        .saturating_add(laid.files.len())
        .saturating_add(laid.small.len()),
    )
    .unwrap_or(u64::MAX);
    let (inodes, _) = volume.inode_usage();
    volume.set_inode_allowance(inodes.saturating_add(made))?;
    let (entries, _) = volume.entry_usage();
    volume.set_entry_allowance(entries.saturating_add(made))?;
    Ok(())
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
      Err(VfsError::NotFound) => {
        let made = volume.mkdir_no(store, at, name, DIRECTORY_MODE)?;
        owned_as_root(volume, store, root, made)?;
        made
      }
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

/// Makes the file `path` beneath `root` hold exactly `bytes`: a referenced file of one made piece, made where missing and
/// made again where it held other bytes (a referenced file is never written in place). Every file of the view is a
/// referenced file, so the volume keeps nothing in the shard's chunk store: a read of it runs while that store is lent
/// to the content store's windows (A-727), the vfs store reading only inodes and directories (module docs).
fn put_file(
  volume: &mut Volume,
  store: &mut Store,
  root: InodeNo,
  path: &str,
  bytes: &[u8],
) -> Result<(), PresentRefusal> {
  let (parent, name) = parent_of(volume, store, root, path)?;
  match volume.lookup_no(store, parent, name) {
    Ok(found) => {
      if file_bytes(volume, store, found.inode)? == bytes {
        return Ok(());
      }
      volume.unlink_no(store, parent, name)?;
    }
    Err(VfsError::NotFound) => {}
    Err(refusal) => return Err(refusal.into()),
  }
  let pieces = if bytes.is_empty() {
    Vec::new()
  } else {
    vec![Piece::Made(bytes.into())]
  };
  let made = volume.create_referenced(store, parent, name, FILE_MODE, pieces, &mut NoHolds)?;
  owned_as_root(volume, store, root, made)
}

/// Gives `made` the owner of the view's root, `root`, the user its mount serves: the volume core makes every inode owned
/// by uid 0, so a view's entries were unreadable to the hook it is mounted for (found by use: the macOS mount's `cat`
/// of `db/current` waited, owner 0 and mode `0400`).
fn owned_as_root(
  volume: &mut Volume,
  store: &mut Store,
  root: InodeNo,
  made: InodeNo,
) -> Result<(), PresentRefusal> {
  let owner = volume.stat(store, root)?;
  volume.chown(store, made, owner.uid, owner.gid)?;
  Ok(())
}

/// A made file's bytes, read through no store (it names no content).
fn file_bytes(volume: &Volume, store: &Store, file: InodeNo) -> Result<Vec<u8>, PresentRefusal> {
  let len = usize::try_from(volume.stat(store, file)?.size).map_err(|_| VfsError::FileTooLarge)?;
  let mut bytes = vec![0u8; len];
  let read = volume.read(store, file, 0, &mut bytes)?;
  bytes.truncate(read);
  Ok(bytes)
}

/// The holds of a file that names no content: none taken, none given back.
struct NoHolds;

impl ReferencedHolds for NoHolds {
  fn hold(&mut self, _id: &Identity) -> Result<(), VfsError> {
    Err(VfsError::ReferenceUnresolved)
  }

  fn release(&mut self, _id: &Identity) -> Result<(), VfsError> {
    Err(VfsError::ReferenceUnresolved)
  }
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
  /// The texts the content store keeps, of those the files name ([`Laid::stored`]).
  pub(crate) stored: &'a BTreeSet<Identity>,
}

impl ViewHolds<'_> {
  /// The content store, for a text it keeps; none for one the records keep.
  fn store_of(&mut self, id: &Identity) -> Result<Option<&mut ContentStore>, VfsError> {
    if !self.stored.contains(id) {
      return Ok(None);
    }
    self
      .store
      .as_deref_mut()
      .map(Some)
      .ok_or(VfsError::ReferenceUnresolved)
  }
}

impl ReferencedHolds for ViewHolds<'_> {
  fn hold(&mut self, id: &Identity) -> Result<(), VfsError> {
    match self.store_of(id)? {
      Some(store) => store
        .hold(&ContentId(*id))
        .map_err(|_| VfsError::ReferenceUnresolved),
      None => Ok(()),
    }
  }

  fn release(&mut self, id: &Identity) -> Result<(), VfsError> {
    match self.store_of(id)? {
      Some(store) => store
        .release(&ContentId(*id))
        .map(|_| ())
        .map_err(|_| VfsError::ReferenceUnresolved),
      None => Ok(()),
    }
  }
}

/// The repository whose view `volume` is (`ShardState::svn_views`); none for any other volume.
pub(crate) fn view_of(
  views: &std::collections::BTreeMap<slates_db::catalog::RepositoryId, ViewSlot>,
  volume: slates_db::catalog::VolumeId,
) -> Option<slates_db::catalog::RepositoryId> {
  views
    .iter()
    .find(|(_, slot)| slot.volume == volume)
    .map(|(id, _)| *id)
}

/// Runs `step` over `state`, with the shard's chunk store lent to the content store's windows for it where `lend` says
/// (a request to a view volume, whose texts the content store may keep: module docs, "Nothing in the chunk store").
pub(crate) fn lent_if<T>(
  state: &mut crate::state::ShardState,
  lend: bool,
  step: impl FnOnce(&mut crate::state::ShardState) -> T,
) -> T {
  if lend {
    crate::vcs_store::lent_state(state, step)
  } else {
    step(state)
  }
}

/// What a request to the view of a repository reads its texts from: the repository's records, its green's engine and
/// the content store, held for the request ([`ViewSource::texts`]).
pub(crate) struct ViewSource<'a> {
  engine: Option<crate::greens::GreenRef<'a>>,
  store: Option<&'a ContentStore>,
  records: &'a slates_svn::migration::Repository,
}

impl ViewSource<'_> {
  /// The text source over what this holds ([`ViewTexts`] reads through it).
  pub(crate) fn texts(&self) -> HeadTexts<'_> {
    HeadTexts {
      green: self.engine.as_deref(),
      store: self.store,
      records: self.records,
    }
  }
}

/// What a request to the view of repository `view` reads through ([`ViewSource`]): the repository's records, its green
/// `green`'s engine and the content store; none for a volume that is no view, or whose records are gone.
pub(crate) fn view_source<'a>(
  vcs_stores: &'a super::VcsStores,
  greens: &'a crate::greens::Greens,
  view: Option<slates_db::catalog::RepositoryId>,
  green: Option<slates_db::catalog::VolumeId>,
) -> Option<ViewSource<'a>> {
  Some(ViewSource {
    records: vcs_stores.subversion_records(view?)?,
    engine: green.and_then(|green| greens.get(&green)),
    store: vcs_stores.content(),
  })
}

/// Format: the start of the words a hook is refused in when its repository's view could not be shown (A-645 (a)), the
/// reason following.
const UNSHOWN_WORDS: &str = "This server could not show this repository's hook its repository";

/// Format: the suffix of a view's mount point in the store's granted directory, after the repository's name: FSFS's
/// own word for its repository.
const MOUNT_POINT_SUFFIX: &str = ".svn";

/// Format: the start of a view volume's name, before the repository's name and the number that routes the name to the
/// repository's owner shard; a leading dot keeps it out of a listing of the host's volumes.
const VOLUME_NAME_PREFIX: &str = ".svn-view.";

/// Shape: the revisions a slice of the view's layout or of its properties' refresh takes: one, the slice `fsfs` names
/// ("a revision is the slice": laying one out costs what the index's own commit of it cost).
const REVISIONS_A_SLICE: usize = 1;

/// A repository's view on its owner shard (`ShardState::svn_views`): its volume, its layout so far, and its mount.
#[derive(Debug)]
pub(crate) struct ViewSlot {
  volume: slates_db::catalog::VolumeId,
  presented: Presented,
  mount: Mount,
}

/// Where a view is mounted.
#[derive(Debug)]
enum Mount {
  /// Nowhere yet.
  Unmounted,
  /// Its mount is being made by a run of the module's mount helper.
  Mounting,
  /// At this path, read-only.
  At(std::path::PathBuf),
}

/// The `REPOS-PATH` a hook of the repository `id`, named `name`, is handed, as `views` says (`HookViews`): the
/// repository's view presented and mounted on its owner shard where the store's grant allows it; the name beneath a
/// given directory for the in-process tests; refused in words, typed by the caller, where neither.
pub(crate) async fn repos_for_hook(
  views: &HookViews,
  id: slates_db::catalog::RepositoryId,
  name: &str,
) -> Result<String, String> {
  let root = match views {
    HookViews::Ungranted => return Err(UNGRANTED_WORDS.to_owned()),
    HookViews::Given(root) => return Ok(root.join(name).to_string_lossy().into_owned()),
    HookViews::Granted(root) => *root,
  };
  let name = name.to_owned();
  let place = crate::state::with_state(|state| {
    let partition = crate::verbs::owner_of_name(&name, state.shards.len());
    Some((
      state.shard,
      crate::verbs::shard_of_partition(state, partition)?,
    ))
  })
  .flatten();
  let Some((origin, owner)) = place else {
    return Err(format!("{UNSHOWN_WORDS}: its owner shard did not answer"));
  };
  let mut last = None;
  let mut refreshed_from = 0;
  loop {
    let turn = crate::xshard::task_in_shard_time(
      origin,
      owner,
      move || async move {
        crate::state::with_state(|state| view_turn(state, id, refreshed_from))
          .unwrap_or_else(|| Err("no shard".to_owned()))
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await
    .unwrap_or_else(|| Err("its owner shard did not answer a turn of its view in time".to_owned()))
    .map_err(|why| format!("{UNSHOWN_WORDS}: {why}"))?;
    match turn {
      ViewTurn::Shown => break,
      ViewTurn::Refreshing(next) => refreshed_from = next,
      ViewTurn::Laying(_) => {}
    }
    if last == Some(turn) {
      return Err(format!(
        "{UNSHOWN_WORDS}: a turn of its view moved nothing ({turn:?})"
      ));
    }
    last = Some(turn);
  }
  let name_for_mount = name;
  crate::xshard::task_in_shard_time(
    origin,
    owner,
    move || async move { mounted(root, id, &name_for_mount).await },
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await
  .unwrap_or_else(|| {
    Err(format!(
      "{UNSHOWN_WORDS}: its owner shard did not answer its view's mount in time"
    ))
  })
  .map(|path| path.to_string_lossy().into_owned())
}

/// How far one turn of a hook's view ([`view_turn`]) took it: what the hook's shard judges its wait by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewTurn {
  /// Laying the view out: this many revisions shown (`Presented::written`), more to show.
  Laying(Revision),
  /// Every revision shown; their properties files refreshed up to this revision, more to refresh.
  Refreshing(Revision),
  /// Shown and refreshed whole: ready to mount.
  Shown,
}

/// One turn of a hook's view, on its owner shard: the view's volume made where there is none, then its next revisions
/// laid out and written, then its properties files refreshed from `refreshed_from` on, a revision a slice, until the
/// shard's step quantum is spent (`futures::step_budget_ns`, what any sliced work here runs to) or the view is whole. A
/// turn always takes one slice, so every answer but a refusal moves the view on.
fn view_turn(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
  refreshed_from: Revision,
) -> Result<ViewTurn, String> {
  slot_for(state, id)?;
  let quantum = slates_rt::futures::step_budget_ns().unwrap_or(state.config.runtime.step_budget_ns);
  let began = slates_rt::futures::now_ns();
  let spent = || slates_rt::futures::now_ns().saturating_sub(began) >= quantum;
  while present_slice(state, id)? == Advance::More {
    if spent() {
      let written = state
        .svn_views
        .get(&id)
        .map(|slot| slot.presented.written())
        .ok_or("the view is gone")?;
      return Ok(ViewTurn::Laying(written));
    }
  }
  let mut revision = refreshed_from;
  while let Some(next) = refresh_slice(state, id, revision)? {
    revision = next;
    if spent() {
      return Ok(ViewTurn::Refreshing(revision));
    }
  }
  Ok(ViewTurn::Shown)
}

/// Makes the repository's view volume where this shard has none: a scratch volume owned by the repository's owner,
/// named so it routes to this shard, its owner (`verbs::owner_of_name`).
fn slot_for(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
) -> Result<(), String> {
  if state.svn_views.contains_key(&id) {
    return Ok(());
  }
  let record = state
    .db
    .partition()
    .repositories()
    .repository(id)
    .ok_or("the repository is not on its owner shard")?;
  let (owner, repository) = (record.owner.clone(), record.name.clone());
  let partitions = state.shards.len();
  let name = (0..=u16::MAX)
    .map(|number| format!("{VOLUME_NAME_PREFIX}{repository}.{number}"))
    .find(|name| {
      crate::verbs::owner_of_name(name, partitions) == state.partition
        && state.db.partition().volume_by_name(name).is_none()
    })
    .ok_or("no view volume name routes to the repository's shard")?;
  // Dynamic, its ceiling nothing yet: each slice of the view sizes it to the view's own content before it writes
  // (`Presented::write`), so its ceiling is never a number of its own (R3).
  let reply = crate::verbs::create(
    state,
    &owner,
    &name,
    slates_ipc::protocol::SizeClass::Dynamic { max: 0 },
    slates_ipc::protocol::NamePolicy::Exact,
    false,
    None,
  );
  let slates_ipc::protocol::ReplyBody::Created { id: volume } = reply else {
    return Err(format!("its view volume was refused: {reply:?}"));
  };
  state.svn_views.insert(
    id,
    ViewSlot {
      volume: crate::verbs::to_db_volume(volume),
      presented: Presented::new(),
      mount: Mount::Unmounted,
    },
  );
  Ok(())
}

/// Lays out and writes the view's next revision, the content store's chunks lent for the step (A-727) so a text it
/// keeps is held; whether every revision is shown.
fn present_slice(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
) -> Result<Advance, String> {
  let green = crate::repository::svn_commit::green_of(state, id).ok();
  crate::vcs_store::lent_state(state, |state| {
    let crate::state::ShardState {
      volumes,
      by_id,
      store,
      vcs_stores,
      greens,
      svn_views,
      ..
    } = state;
    let slot = svn_views.get_mut(&id).ok_or("the view is gone")?;
    let laid = {
      let records = vcs_stores
        .subversion_records(id)
        .ok_or("the repository's records are not here")?;
      let engine = green.and_then(|green| greens.get(&green));
      let texts = HeadTexts {
        green: engine.as_deref(),
        store: vcs_stores.content(),
        records,
      };
      let source = slates_svn::migration::WithTexts {
        records,
        texts: &texts,
      };
      slot
        .presented
        .lay_out(&source, REVISIONS_A_SLICE)
        .map_err(|refusal| format!("{refusal:?}"))?
    };
    let handle = *by_id.get(&slot.volume).ok_or("the view volume is gone")?;
    let held = volumes
      .get_mut(handle)
      .map_err(|_| "the view volume is gone")?;
    let root = held
      .volume
      .root_inode(store)
      .map_err(|refusal| format!("{refusal:?}"))?;
    let stored = laid.stored().clone();
    let mut holds = ViewHolds {
      store: vcs_stores.content_mut(),
      stored: &stored,
    };
    slot
      .presented
      .write((&mut held.volume, store), root, laid, &mut holds)
      .map_err(|refusal| format!("{refusal:?}"))
  })
}

/// Rewrites revision `revision`'s properties file from the records, as a revision property change may have changed
/// them since the view showed them: the next revision to refresh, none past the last.
fn refresh_slice(
  state: &mut crate::state::ShardState,
  id: slates_db::catalog::RepositoryId,
  revision: Revision,
) -> Result<Option<Revision>, String> {
  let green = crate::repository::svn_commit::green_of(state, id).ok();
  let crate::state::ShardState {
    volumes,
    by_id,
    store,
    vcs_stores,
    greens,
    svn_views,
    ..
  } = state;
  let slot = svn_views.get_mut(&id).ok_or("the view is gone")?;
  if revision >= slot.presented.written() {
    return Ok(None);
  }
  let records = vcs_stores
    .subversion_records(id)
    .ok_or("the repository's records are not here")?;
  let engine = green.and_then(|green| greens.get(&green));
  let texts = HeadTexts {
    green: engine.as_deref(),
    store: None,
    records,
  };
  let source = slates_svn::migration::WithTexts {
    records,
    texts: &texts,
  };
  let handle = *by_id.get(&slot.volume).ok_or("the view volume is gone")?;
  let held = volumes
    .get_mut(handle)
    .map_err(|_| "the view volume is gone")?;
  let root = held
    .volume
    .root_inode(store)
    .map_err(|refusal| format!("{refusal:?}"))?;
  slot
    .presented
    .revision_properties_changed((&mut held.volume, store), root, &source, revision)
    .map_err(|refusal| format!("{refusal:?}"))?;
  Ok(Some(revision.saturating_add(1)))
}

/// Where the repository's view is mounted, mounting it read-only in the store's granted directory `root` first where it
/// is not: the point made under the grant (`StoreRoot::mount_point`), then this daemon's own `slates mount`, run as a
/// helper of the shard, which mounts through each platform's bridge as any client's mount does (§4.6).
#[cfg(unix)]
async fn mounted(
  root: &'static slates_land::store_root::StoreRoot,
  id: slates_db::catalog::RepositoryId,
  name: &str,
) -> Result<std::path::PathBuf, String> {
  let begun = crate::state::with_state(|state| {
    let slot = state.svn_views.get_mut(&id)?;
    Some(match &slot.mount {
      Mount::At(path) => Ok(Some(path.clone())),
      Mount::Mounting => Err(format!(
        "{UNSHOWN_WORDS}: its view is being mounted; try again"
      )),
      Mount::Unmounted => {
        slot.mount = Mount::Mounting;
        Ok(None)
      }
    })
  })
  .flatten()
  .ok_or_else(|| format!("{UNSHOWN_WORDS}: the view is gone"))??;
  if let Some(path) = begun {
    return Ok(path);
  }
  let made = mount_once(root, id, name).await;
  crate::state::with_state(|state| {
    if let Some(slot) = state.svn_views.get_mut(&id) {
      slot.mount = match &made {
        Ok(path) => Mount::At(path.clone()),
        Err(_) => Mount::Unmounted,
      };
    }
  });
  made
}

/// One mount of the view: its point made, then the helper run and judged by its exit.
#[cfg(unix)]
async fn mount_once(
  root: &'static slates_land::store_root::StoreRoot,
  id: slates_db::catalog::RepositoryId,
  name: &str,
) -> Result<std::path::PathBuf, String> {
  let point = root
    .mount_point(&format!("{name}{MOUNT_POINT_SUFFIX}"))
    .map_err(|refusal| format!("{UNSHOWN_WORDS}: {refusal}"))?;
  let launch = crate::state::with_state(|state| {
    let volume = state.svn_views.get(&id)?.volume;
    let wire = crate::verbs::to_wire_volume(volume);
    let hex: String = wire
      .bytes
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect();
    Some((state.config.instance.clone(), hex, state.failover_ns))
  })
  .flatten();
  let Some((instance, volume, bound_ns)) = launch else {
    return Err(format!("{UNSHOWN_WORDS}: the view is gone"));
  };
  let program = std::env::current_exe().map_err(|error| format!("{UNSHOWN_WORDS}: {error}"))?;
  let point_text = point.to_string_lossy().into_owned();
  let args = [
    "--instance",
    instance.as_str(),
    "mount",
    volume.as_str(),
    point_text.as_str(),
    "--read-only",
  ];
  match crate::hook_driver::run_helper(&program, &args, bound_ns).await {
    Ok(true) => Ok(point),
    Ok(false) => Err(format!(
      "{UNSHOWN_WORDS}: its view's mount at {point_text} was refused"
    )),
    Err(why) => Err(format!("{UNSHOWN_WORDS}: its view's mount helper {why}")),
  }
}

/// No hook runs on Windows yet (the hook driver is Unix's alone), so no view is mounted there.
#[cfg(not(unix))]
async fn mounted(
  _root: &'static slates_land::store_root::StoreRoot,
  _id: slates_db::catalog::RepositoryId,
  _name: &str,
) -> Result<std::path::PathBuf, String> {
  Err(format!("{UNSHOWN_WORDS}: no hook runs on this platform"))
}

#[cfg(test)]
#[path = "svn_presenter/tests.rs"]
mod tests;

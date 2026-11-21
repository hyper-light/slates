//! A Subversion repository's head texts held by the green, by content identity (S4 slice 2, step c1; A-727 (c); A-716;
//! rule (B): the green keeps the head's files, the repository store their history).
//!
//! **What it does.** Each text a file of the repository's head holds is held in the green's held table
//! (`slates_merge::held`) by path: the head file's own block, one more holder, so no byte is copied. A revision that
//! lands releases the texts the paths it changed held before it, a directory's files with it, and holds the texts they
//! hold after it, a copied directory's files with it ([`released_before`], [`held_after`], [`follow`]). A green made
//! again by a recovery holds none (the table is not in its checkpoint, A-716 slice 1c), so the recovery holds every
//! file of the head, in byte order, once the records are replayed ([`rehold_head`]): the same table every time.
//!
//! **Reading** (c3). [`HeadTexts`] is the Subversion sources' text source (`slates_svn::migration::TextSource`). A text
//! of the head is read from the green, which holds it (borrowed, no copy-out). Any other text is read from the one place
//! the records say it is kept, never tried in turn: a text a commit stored (main's ruling (B)) from the content store by
//! its identity, its length the records'; a text the records keep themselves (an import's or a made revision's, as a
//! skip-delta svndiff, c2) from the records. A text kept in neither is no text. A read takes a piece at a time
//! (`text_at`): the green's bytes from the offset, the store's window holding it, or the records' delta window holding
//! it, its source view made window by window down the chain; nothing is made whole.
//!
//! **Room.** The table's entries are bounded by what the shard could admit, an entry its identity, its block and its
//! count: [`set_room`] derives it before each hold, so a hold past it is refused typed and counted, and the text read
//! from where the records keep it.

use std::borrow::Cow;

use slates_db::catalog::{RepositoryId, VolumeId as DbVolumeId};
use slates_merge::engine::Green;
use slates_svn::index::{Action, ChangedPath, ContentIdentity, Index, NodeKind};
use slates_svn::migration::{Repository as SvnRecords, TextReading, TextSource};

use crate::state::ShardState;

/// Counter: head texts the green holds by path (S4 c1).
pub(crate) const HELD: &str = "vcs_store.svn_texts.held";
/// Counter: holds or releases the green's table refused (its room, no file at the path): the text is read from the
/// records.
pub(crate) const REFUSED: &str = "vcs_store.svn_texts.refused";

/// Shape: what one entry of the green's held table holds, for its room: the identity, the block's index and the count,
/// and a map node's three words.
const ENTRY_BYTES: u64 =
  (size_of::<ContentIdentity>() + 2 * size_of::<u64>() + 3 * size_of::<usize>()) as u64;

/// Text pieces read through the node's changeset store for drives waiting on them (C2 stage B part 2; module docs,
/// "Reading"): each piece by its text and the offset it starts at, with how many waiting readers hold it. Bounded by
/// the readers waiting on the shard: a reader puts the piece it waited for ([`fetch_piece`]) and lets it go once its
/// next step has read it ([`let_go`]).
#[derive(Debug, Default)]
pub(crate) struct Pieces(std::collections::BTreeMap<(ContentIdentity, u64), (Vec<u8>, usize)>);

impl Pieces {
  /// How many pieces are held.
  #[cfg(test)]
  pub(crate) fn len(&self) -> usize {
    self.0.len()
  }

  /// The piece of `identity` holding `offset`, from `offset` on; none where no reader fetched it.
  fn at(&self, identity: &ContentIdentity, offset: u64) -> Option<&[u8]> {
    let ((_, start), (bytes, _)) = self
      .0
      .range(..=(*identity, offset))
      .next_back()
      .filter(|((text, _), _)| text == identity)?;
    let skip = usize::try_from(offset.checked_sub(*start)?).ok()?;
    bytes.get(skip..).filter(|rest| !rest.is_empty())
  }
}

/// Counter: a text piece a waiting drive read through the node's changeset store (C2 stage B part 2). Format: a counter
/// name in the status report.
pub(crate) const PIECE_FETCHED: &str = "vcs_store.svn_texts.piece_fetched";
/// Counter: a piece the node's store could not give (no store, its owner gone, no such text): the waiting drive's
/// connection ends. Format: a counter name.
pub(crate) const PIECE_UNFETCHED: &str = "vcs_store.svn_texts.piece_unfetched";

/// Reads the text piece `wanted` of repository `repository` through the node's changeset store and puts it where the
/// repository's views read it ([`Pieces`]), held once more for the reader that waited; whether it did. A busy owner is
/// asked again after a yield. Counted.
pub(crate) async fn fetch_piece(
  repository: slates_db::catalog::RepositoryId,
  wanted: slates_svn::reads::Wanted,
) -> bool {
  let Some(Some(disk)) = crate::state::with_state(|state| state.vcs_disk.clone()) else {
    return false;
  };
  let from = u128::from_be_bytes(repository.bytes);
  let piece = loop {
    match disk.text(from, wanted.text, wanted.offset).await {
      Ok(Some(piece)) if !piece.is_empty() => break Some(piece),
      Err(slates_store::owner::OwnerError::Busy) => slates_rt::futures::yield_now().await,
      _ => break None,
    }
  };
  crate::state::with_state(|state| {
    let Some(piece) = piece else {
      state.count(PIECE_UNFETCHED, 1);
      return false;
    };
    let held = state
      .vcs_stores
      .svn_pieces
      .0
      .entry((wanted.text, wanted.offset))
      .or_insert((piece, 0));
    held.1 = held.1.saturating_add(1);
    state.count(PIECE_FETCHED, 1);
    true
  })
  .unwrap_or(false)
}

/// Lets go the reader's hold on the piece `wanted` it fetched ([`fetch_piece`]), the piece gone with its last hold.
pub(crate) fn let_go(state: &mut ShardState, wanted: slates_svn::reads::Wanted) {
  let pieces = &mut state.vcs_stores.svn_pieces.0;
  let key = (wanted.text, wanted.offset);
  if let Some(held) = pieces.get_mut(&key) {
    held.1 = held.1.saturating_sub(1);
    if held.1 == 0 {
      pieces.remove(&key);
    }
  }
}

/// The Subversion sources' texts (module docs, "Reading"): the green's held table first, then the records.
pub(crate) struct HeadTexts<'a> {
  /// The repository's green, when it is held here.
  pub(crate) green: Option<&'a Green>,
  /// The shard's content store, its chunks lent (`crate::vcs_store::lent_state`): where a text a commit stored lives
  /// (§4.17.1 "A push's intake"; the records keep it as a reference).
  pub(crate) store: Option<&'a slates_vcs::ContentStore>,
  /// The repository's records.
  pub(crate) records: &'a SvnRecords,
  /// The pieces waiting drives fetched through the node's store, where a reader that can wait reads a stored text the
  /// content store does not hold ([`fetch_piece`]); none for a reader that cannot wait.
  pub(crate) pieces: Option<&'a Pieces>,
}

impl TextSource for HeadTexts<'_> {
  /// The head's text from the green; any other from where the records keep it (module docs, "Reading").
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    if let Some(bytes) = self.green.and_then(|green| green.held(identity)) {
      return Some(Cow::Borrowed(bytes));
    }
    match self.records.stored_len(identity) {
      Some(len) => {
        let store = self.store?;
        let id = slates_vcs::ContentId(*identity);
        store.object_bytes(&store.root_of(&id), len).map(Cow::Owned)
      }
      None => self.records.text(identity),
    }
  }

  /// The text from `offset`, a piece at a time (a fetch's read, main's order after w-bugs' runs 10/11): from the green,
  /// borrowed to the text's end; else the content store's window holding `offset`; else the records' delta window
  /// holding it. Nothing is made whole.
  fn text_at(&self, identity: &ContentIdentity, offset: u64) -> Option<Cow<'_, [u8]>> {
    if let Some(bytes) = self.green.and_then(|green| green.held(identity)) {
      return bytes
        .get(usize::try_from(offset).ok()?..)
        .map(Cow::Borrowed);
    }
    match self.records.stored_len(identity) {
      Some(len) => {
        if offset >= len {
          return (offset == len).then_some(Cow::Borrowed(&[]));
        }
        let held = self
          .store
          .and_then(|store| store.run_at(&slates_vcs::ContentId(*identity), len, offset));
        // A stored text the content store does not hold is read through the node's store (C2 stage B part 2).
        held.or_else(|| self.pieces?.at(identity, offset).map(Cow::Borrowed))
      }
      None => self.records.text_at(identity, offset).map(Cow::Owned),
    }
  }

  /// A stored text's piece neither the green, the content store nor the fetched pieces hold, for a reader that can
  /// wait: it is read through the node's changeset store (C2 stage B part 2; [`fetch_piece`]).
  fn fetchable(&self, identity: &ContentIdentity, offset: u64) -> bool {
    self.pieces.is_some()
      && self
        .records
        .stored_len(identity)
        .is_some_and(|len| offset < len)
      && self.text_at(identity, offset).is_none()
  }

  /// [`TextSource::text_at`] for a reader keeping `cache` across its steps: the green's and the content store's runs
  /// lent in place as `text_at` lends them; a text the records keep, the delta window the reader is in made once into
  /// its cache and lent from there.
  fn text_in<'a>(
    &'a self,
    identity: &ContentIdentity,
    offset: u64,
    cache: &'a mut slates_svn::reads::WindowCache,
  ) -> Option<Cow<'a, [u8]>> {
    let kept_by_records = self.records.stored_len(identity).is_none()
      && self.green.and_then(|green| green.held(identity)).is_none();
    if kept_by_records {
      return self.records.text_in(identity, offset, cache);
    }
    self.text_at(identity, offset)
  }

  /// A step of the same order: the green's held text a window's room copied, the content store's a window of the
  /// store read (`ContentStore::window_at`, from the window the bytes read so far reach), the records' a delta window
  /// applied.
  fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool> {
    if let Some(bytes) = self.green.and_then(|green| green.held(identity)) {
      return Some(reading.copy_step(bytes));
    }
    let Some(len) = self.records.stored_len(identity) else {
      return TextSource::read_step(self.records, identity, reading);
    };
    let read = reading.read();
    if read >= len {
      return Some(true);
    }
    let wanted = usize::try_from(len.saturating_sub(read)).ok()?;
    // A stored text is read from the content store; one it does not hold, through the node's store (C2 stage B part 2):
    // the piece a waiting reader fetched.
    if let Some(store) = self.store {
      let root = store.root_of(&slates_vcs::ContentId(*identity));
      let window = u64::try_from(store.window()).ok()?;
      if let Some(piece) = store.window_at(&root, len, read.checked_div(window)?) {
        let skip = usize::try_from(read.checked_rem(window)?).ok()?;
        let rest = piece.get(skip..)?;
        return Some(reading.append(rest.get(..wanted).unwrap_or(rest), len));
      }
    }
    let rest = self.pieces?.at(identity, read)?;
    Some(reading.append(rest.get(..wanted).unwrap_or(rest), len))
  }
}

/// What a Subversion repository's sessions read (§4.17.4; main's ruling (B)): its records, with their texts read through
/// [`HeadTexts`].
pub(crate) type SvnView<'a> = slates_svn::migration::WithTexts<'a, HeadTexts<'a>>;

/// A file's text identity and its path in the green, as a walk finds it.
type FileText = (ContentIdentity, String);

/// The files at and beneath `path` in `revision`, each with its text, appended to `out` in byte order; a file with no
/// text (empty) is left out. A path the index cannot read adds nothing.
fn files_at(index: &Index, path: &str, revision: u64, out: &mut Vec<FileText>) {
  let mut stack = vec![path.to_owned()];
  while let Some(at) = stack.pop() {
    let Ok(Some(node)) = index.node(&at, revision) else {
      continue;
    };
    match node.kind() {
      NodeKind::File => {
        if let Some(text) = node.text() {
          out.push((text.identity, green_path(&at).to_owned()));
        }
      }
      NodeKind::Directory => {
        let Ok(Some(mut names)) = index.children(&at, revision) else {
          continue;
        };
        names.sort_unstable_by(|left, right| right.cmp(left));
        for name in names {
          stack.push(if at == "/" {
            format!("/{name}")
          } else {
            format!("{at}/{name}")
          });
        }
      }
    }
  }
}

/// The file at `path` in `revision` with its text, appended to `out`; a directory (whose property change changes no
/// file beneath it), an empty file or a path the index cannot read adds nothing.
fn file_at(index: &Index, path: &str, revision: u64, out: &mut Vec<FileText>) {
  if let Ok(Some(node)) = index.node(path, revision)
    && node.kind() == NodeKind::File
    && let Some(text) = node.text()
  {
    out.push((text.identity, green_path(path).to_owned()));
  }
}

/// A Subversion path's place in the green: without its leading `/`.
fn green_path(path: &str) -> &str {
  path.strip_prefix('/').unwrap_or(path)
}

/// The texts the paths `changes` changes held at `youngest`, before the revision applies: a modified file's, and every
/// file's at or beneath a deleted or replaced path.
pub(crate) fn released_before(
  records: &SvnRecords,
  changes: &[ChangedPath],
  youngest: u64,
) -> Vec<FileText> {
  let index = records.index();
  let mut released = Vec::new();
  for change in changes {
    match change.action {
      Action::Delete | Action::Replace => files_at(index, &change.path, youngest, &mut released),
      Action::Modify => file_at(index, &change.path, youngest, &mut released),
      Action::Add => {}
    }
  }
  once_a_path(released)
}

/// The texts the paths `changes` changes hold at `revision`, once it applied: a modified file's, and every file's at or
/// beneath an added or replaced path (a copied directory's files with it).
pub(crate) fn held_after(
  records: &SvnRecords,
  changes: &[ChangedPath],
  revision: u64,
) -> Vec<FileText> {
  let index = records.index();
  let mut held = Vec::new();
  for change in changes {
    match change.action {
      Action::Add | Action::Replace => files_at(index, &change.path, revision, &mut held),
      Action::Modify => file_at(index, &change.path, revision, &mut held),
      Action::Delete => {}
    }
  }
  once_a_path(held)
}

/// `files` with each path once, in byte order: a path two rows reach (a file changed beneath a directory the same
/// revision copied) holds its text once.
fn once_a_path(files: Vec<FileText>) -> Vec<FileText> {
  let by_path: std::collections::BTreeMap<String, ContentIdentity> = files
    .into_iter()
    .map(|(identity, path)| (path, identity))
    .collect();
  by_path
    .into_iter()
    .map(|(path, identity)| (identity, path))
    .collect()
}

/// Sets the green's held room from what the shard could admit now (module docs, "Room").
fn set_room(state: &mut ShardState, green: DbVolumeId) {
  let room = usize::try_from(state.store.admittable() / ENTRY_BYTES).unwrap_or(usize::MAX);
  if let Some(mut engine) = state.greens.get_mut(&green) {
    let held = engine.held_count();
    engine.set_held_room(held.saturating_add(room));
  }
}

/// A landed revision followed in the green's held table: each of `released` let go, then each of `held` held by its
/// path, the head's file's own block. A refusal is counted, and that text read from the records.
pub(crate) fn follow(
  state: &mut ShardState,
  green: DbVolumeId,
  released: &[FileText],
  held: &[FileText],
) {
  set_room(state, green);
  let (mut refused, mut holds) = (0u64, 0u64);
  if let Some(mut engine) = state.greens.get_mut(&green) {
    for (identity, _) in released {
      if engine.release_held(identity).is_err() {
        refused = refused.saturating_add(1);
      }
    }
    for (identity, path) in held {
      match engine.hold_path(*identity, path) {
        Ok(()) => holds = holds.saturating_add(1),
        Err(_) => refused = refused.saturating_add(1),
      }
    }
  }
  state.count(HELD, holds);
  state.count(REFUSED, refused);
}

/// Holds every file of repository `id`'s head in its green, in byte order, where the green holds none (module docs: a
/// green a recovery made again). A green that holds texts already keeps its table.
pub(crate) fn rehold_head(state: &mut ShardState, id: RepositoryId, green: DbVolumeId) {
  let fresh = state
    .greens
    .get(&green)
    .is_some_and(|engine| engine.held_count() == 0);
  if !fresh {
    return;
  }
  let Some(records) = state.vcs_stores.subversion_records(id) else {
    return;
  };
  let mut head = Vec::new();
  files_at(records.index(), "/", records.youngest(), &mut head);
  follow(state, green, &[], &head);
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_svn::migration::Room;

  /// A-761 with main's item 2 of 2026-11-20 (a text read a bounded step at a time). Do: put a 10,000-byte text into a
  /// content store of 4,096-byte windows; land revision 1 with `/trunk/a.txt` naming that text as stored (the records
  /// keep its length, no bytes); read it through `HeadTexts` a step at a time, and whole. Expect: the text, in three
  /// steps, a store window each; the whole read the same bytes.
  #[test]
  fn a_stored_text_is_read_a_store_window_a_step() {
    let window = 4_096;
    let bytes: Vec<u8> = (0..10_000u32).map(|at| (at % 253) as u8).collect();
    let mut store = slates_vcs::ContentStore::new(1 << 20, window).unwrap();
    let put = store.put_object(&bytes).unwrap();
    let identity = *blake3::hash(&bytes).as_bytes();
    assert_eq!(put.0, identity);
    let uuid = slates_svn::dump::Uuid::parse(b"00000000-0000-4000-8000-00000000a761").unwrap();
    let room = Room {
      entries: 1 << 10,
      bytes: 1 << 20,
    };
    let mut records = SvnRecords::new(uuid, b"2026-11-20T00:00:00.000000Z", room).unwrap();
    let mut revision = crate::repository::svn_commit::tests::first();
    let file = revision
      .record
      .changes
      .iter_mut()
      .find(|row| row.path == "/trunk/a.txt")
      .unwrap();
    let alpha = file.text.unwrap().identity;
    file.text = Some(slates_svn::commit::drive::text_of(&bytes).unwrap());
    revision.contents.remove(&alpha);
    revision
      .stored
      .insert(identity, u64::try_from(bytes.len()).unwrap());
    let checked = records.check(revision).unwrap();
    records.apply(checked).unwrap();
    let texts = HeadTexts {
      green: None,
      store: Some(&store),
      records: &records,
      pieces: None,
    };
    let mut reading = TextReading::new();
    let mut steps = 0;
    while !texts.read_step(&identity, &mut reading).unwrap() {
      steps += 1;
      assert!(steps < 10, "the reading never ended");
    }
    assert_eq!((steps + 1, reading.bytes()), (3, bytes.as_slice()));
    assert_eq!(texts.text(&identity).as_deref(), Some(bytes.as_slice()));
  }
}

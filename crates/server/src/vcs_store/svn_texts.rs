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
//! **Reading.** [`HeadTexts`] is the Subversion sources' text source (`slates_svn::migration::TextSource`): a text the
//! green holds is read from it, borrowed; any other from the records. The records still hold every text's bytes until
//! step c3; the read through the records goes with them, in the same series (main's condition), and until then it is
//! named in GAPS.
//!
//! **Room.** The table's entries are bounded by what the shard could admit, an entry its identity, its block and its
//! count: [`set_room`] derives it before each hold, so a hold past it is refused typed, counted, and the text read from
//! the records.

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

/// The Subversion sources' texts (module docs, "Reading"): the green's held table first, then the records.
pub(crate) struct HeadTexts<'a> {
  /// The repository's green, when it is held here.
  pub(crate) green: Option<&'a Green>,
  /// The shard's content store, its chunks lent (`crate::vcs_store::lent_state`): where a text a commit stored lives
  /// (§4.17.1 "A push's intake"; the records keep it as a reference).
  pub(crate) store: Option<&'a slates_vcs::ContentStore>,
  /// The repository's records.
  pub(crate) records: &'a SvnRecords,
}

impl TextSource for HeadTexts<'_> {
  /// The green's held text, then the content store's (a text a commit stored, read whole through its root), then the
  /// records' (until c3 removes that arm).
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    if let Some(bytes) = self.green.and_then(|green| green.held(identity)) {
      return Some(Cow::Borrowed(bytes));
    }
    if let (Some(store), Some(len)) = (self.store, self.records.stored_len(identity)) {
      let id = slates_vcs::ContentId(*identity);
      if let Some(bytes) = store.object_bytes(&store.root_of(&id), len) {
        return Some(Cow::Owned(bytes));
      }
    }
    TextSource::text(self.records, identity)
  }

  /// A step of the same order: the green's held text a window's room copied, the content store's a window of the
  /// store read (`ContentStore::window_at`, from the window the bytes read so far reach), the records' a delta window
  /// applied.
  fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool> {
    if let Some(bytes) = self.green.and_then(|green| green.held(identity)) {
      return Some(reading.copy_step(bytes));
    }
    if let (Some(store), Some(len)) = (self.store, self.records.stored_len(identity)) {
      let root = store.root_of(&slates_vcs::ContentId(*identity));
      let window = u64::try_from(store.window()).ok()?;
      let read = reading.read();
      if read >= len {
        return Some(true);
      }
      if let Some(piece) = store.window_at(&root, len, read.checked_div(window)?) {
        let skip = usize::try_from(read.checked_rem(window)?).ok()?;
        let wanted = usize::try_from(len.saturating_sub(read)).ok()?;
        let rest = piece.get(skip..)?;
        return Some(reading.append(rest.get(..wanted).unwrap_or(rest), len));
      }
    }
    TextSource::read_step(self.records, identity, reading)
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

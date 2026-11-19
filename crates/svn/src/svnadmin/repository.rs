//! A repository's records, as svnadmin's verbs read and write them (the module documentation): the Subversion index,
//! each revision's tree, revision properties, and every text and property list held once by its content identity.

use std::collections::BTreeMap;

use super::order::SEPARATOR;
use super::properties::{encode, has_mergeinfo};
use super::{AdminError, Place};
use crate::dump::{Digests, Md5, Sha1, Uuid};
use crate::index::{
  Action, ChangedPath, ContentIdentity, CopySource, Index, NodeKind, PropertyList, Revision, RevisionRecord, Text,
};

/// A property list: names to values, in name order (FSFS keeps a node's list as a hash, which every writer sorts).
pub type Properties = BTreeMap<Vec<u8>, Vec<u8>>;

/// A node of a revision's tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
  /// File or directory.
  pub kind: NodeKind,
  /// A file's text; none for a directory.
  pub text: Option<Text>,
  /// Its property list.
  pub properties: PropertyList,
}

/// A revision's tree: every node by its path, relative to the root, which is the empty path.
pub type Tree = BTreeMap<String, Entry>;

/// The digits a transaction's sequence number is written in.
/// Format: FSFS writes a transaction's name as its base revision, `-` and its sequence number in base 36
/// (`libsvn_fs_fs/transaction.c` `create_txn_dir`, `svn__ui64tobase36`).
const NAME_BASE: u64 = 36;

/// The digits of base 36, as FSFS writes them.
/// Format: `svn__ui64tobase36` writes 0 to 9 then lowercase a to z.
const NAME_DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// A repository's records (the module documentation).
#[derive(Clone, Debug)]
pub struct Repository {
  uuid: Uuid,
  room: usize,
  index: Index,
  trees: Vec<Tree>,
  revision_properties: Vec<Properties>,
  contents: BTreeMap<ContentIdentity, Vec<u8>>,
  transactions_made: u64,
}

impl Repository {
  /// A repository as `svnadmin create` leaves one: revision 0, an empty root, `svn:date` its only revision property,
  /// `date` given. `room` bounds the index's entries (the index's own room, A-553), derived by the caller.
  pub fn create(uuid: Uuid, date: &[u8], room: usize) -> Self {
    let mut root = Tree::new();
    root.insert(String::new(), directory());
    let mut properties = Properties::new();
    properties.insert(b"svn:date".to_vec(), date.to_vec());
    Self {
      uuid,
      room,
      index: Index::new(),
      trees: vec![root],
      revision_properties: vec![properties],
      contents: BTreeMap::new(),
      transactions_made: 0,
    }
  }

  /// The repository's UUID.
  pub fn uuid(&self) -> Uuid {
    self.uuid
  }

  /// Sets the repository's UUID (a load's UUID record, `svn_fs_set_uuid`).
  pub fn set_uuid(&mut self, uuid: Uuid) {
    self.uuid = uuid;
  }

  /// The youngest revision.
  pub fn youngest(&self) -> Revision {
    self.index.youngest()
  }

  /// The Subversion index of the repository's revisions.
  pub fn index(&self) -> &Index {
    &self.index
  }

  /// The tree of `revision`, if the repository holds it.
  pub fn tree(&self, revision: Revision) -> Option<&Tree> {
    self.trees.get(usize::try_from(revision).ok()?)
  }

  /// The revision properties of `revision`, if the repository holds it.
  pub fn revision_properties(&self, revision: Revision) -> Option<&Properties> {
    self.revision_properties.get(usize::try_from(revision).ok()?)
  }

  /// Sets the revision properties of `revision`, if the repository holds it.
  pub(super) fn set_revision_properties(&mut self, revision: Revision, properties: Properties) {
    if let Some(slot) = usize::try_from(revision)
      .ok()
      .and_then(|place| self.revision_properties.get_mut(place))
    {
      *slot = properties;
    }
  }

  /// The bytes of a text or an encoded property list the repository holds by `identity`.
  pub fn content(&self, identity: &ContentIdentity) -> Option<&[u8]> {
    self.contents.get(identity).map(Vec::as_slice)
  }

  /// Holds `bytes` by their content identity and answers it.
  pub(super) fn store(&mut self, bytes: Vec<u8>) -> ContentIdentity {
    let identity = *blake3::hash(&bytes).as_bytes();
    self.contents.entry(identity).or_insert(bytes);
    identity
  }

  /// Holds `text` and answers it as the index names a text: its identity, MD5, SHA-1 and size.
  pub(super) fn store_text(&mut self, text: Vec<u8>) -> Result<Text, AdminError> {
    let (md5, sha1) = digests(&text)?;
    let size = u64::try_from(text.len()).unwrap_or(u64::MAX);
    let identity = self.store(text);
    Ok(Text {
      identity,
      md5,
      sha1,
      size,
    })
  }

  /// Holds `properties` encoded and answers the list as the index names one.
  pub(super) fn store_properties(&mut self, properties: &Properties) -> PropertyList {
    if properties.is_empty() {
      return PropertyList::Empty;
    }
    let has_mergeinfo = has_mergeinfo(properties);
    let identity = self.store(encode(properties));
    PropertyList::Held {
      identity,
      has_mergeinfo,
    }
  }

  /// A transaction on the youngest revision, named as FSFS names its next one: the base revision, `-`, and how many
  /// transactions the repository has made, in base 36.
  pub(super) fn begin(&mut self) -> Transaction {
    let base = self.youngest();
    let name = format!("{base}-{}", base36(self.transactions_made));
    self.transactions_made = self.transactions_made.saturating_add(1);
    let tree = self.tree(base).cloned().unwrap_or_default();
    Transaction {
      name,
      base,
      tree,
      changes: BTreeMap::new(),
    }
  }

  /// Commits `transaction` with `properties` as its revision properties: its changed paths appended to the index as
  /// one revision, its tree kept. Refused, typed, as the index refuses the record; nothing changes then.
  pub(super) fn commit(&mut self, transaction: Transaction, properties: Properties) -> Result<Revision, AdminError> {
    let record = transaction.record(self);
    let revision = self.index.append(record, self.room).map_err(AdminError::Index)?;
    self.trees.push(transaction.tree);
    self.revision_properties.push(properties);
    Ok(revision)
  }
}

/// An empty directory.
pub(super) fn directory() -> Entry {
  Entry {
    kind: NodeKind::Directory,
    text: None,
    properties: PropertyList::Empty,
  }
}

/// `count` in base 36, as FSFS writes a transaction's sequence number.
fn base36(count: u64) -> String {
  let mut digits = Vec::new();
  let mut rest = count;
  loop {
    let digit = usize::try_from(rest.checked_rem(NAME_BASE).unwrap_or(0)).unwrap_or(0);
    digits.push(NAME_DIGITS.get(digit).copied().unwrap_or(b'0'));
    rest = rest.checked_div(NAME_BASE).unwrap_or(0);
    if rest == 0 {
      break;
    }
  }
  digits.reverse();
  String::from_utf8_lossy(&digits).into_owned()
}

/// The MD5 and SHA-1 of `text`.
pub(super) fn digests(text: &[u8]) -> Result<(Md5, Sha1), AdminError> {
  let mut digests = Digests::new().map_err(AdminError::Stream)?;
  digests.update(text).map_err(AdminError::Stream)?;
  digests.finish().map_err(AdminError::Stream)
}

/// One path's change in a transaction, folded as FSFS folds a transaction's changes (`fold_change`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Change {
  action: Action,
  kind: NodeKind,
  copy_from: Option<CopySource>,
  text_modified: bool,
  properties_modified: bool,
  mergeinfo_modified: bool,
}

/// A transaction: the tree a revision is being made in, on its base revision, and each changed path.
#[derive(Clone, Debug)]
pub(super) struct Transaction {
  name: String,
  base: Revision,
  tree: Tree,
  changes: BTreeMap<String, Change>,
}

impl Transaction {
  /// The transaction's name.
  pub(super) fn name(&self) -> &str {
    &self.name
  }

  /// The node at `path`, if the transaction holds one.
  pub(super) fn node(&self, path: &str) -> Option<&Entry> {
    self.tree.get(path)
  }

  /// Deletes the node at `path` and everything beneath it; refused when it is absent.
  pub(super) fn delete(&mut self, path: &str) -> Result<(), AdminError> {
    let kind = self.node(path).map(|entry| entry.kind).ok_or_else(|| self.not_found(path))?;
    self.tree.retain(|existing, _| !at_or_beneath(existing, path));
    self.changes.retain(|existing, _| existing == path || !at_or_beneath(existing, path));
    self.fold(path, Action::Delete, kind, None);
    Ok(())
  }

  /// Adds `entry` at `path`, a new node; refused where one stands or its directory does not.
  pub(super) fn add(&mut self, path: &str, entry: Entry, filesystem: Uuid) -> Result<(), AdminError> {
    self.check_addable(path, filesystem)?;
    let kind = entry.kind;
    self.tree.insert(path.to_owned(), entry);
    self.fold(path, Action::Add, kind, None);
    Ok(())
  }

  /// Copies the subtree `source` (the tree of `revision`) to `path`; refused as an addition is.
  pub(super) fn copy(
    &mut self,
    path: &str,
    source: (&Tree, &str, Revision),
    filesystem: Uuid,
  ) -> Result<(), AdminError> {
    self.check_addable(path, filesystem)?;
    let (tree, source_path, revision) = source;
    let kind = tree
      .get(source_path)
      .map(|entry| entry.kind)
      .ok_or_else(|| AdminError::NotFound {
        place: Place::Revision(revision),
        path: fspath(source_path),
      })?;
    let copied: Vec<(String, Entry)> = tree
      .iter()
      .filter(|(existing, _)| at_or_beneath(existing, source_path))
      .map(|(existing, entry)| (moved(existing, source_path, path), entry.clone()))
      .collect();
    self.tree.extend(copied);
    let copy_from = CopySource {
      path: fspath(source_path),
      revision,
    };
    self.fold(path, Action::Add, kind, Some(copy_from));
    Ok(())
  }

  /// Marks the node at `path` changed in place (a change record); refused when it is absent.
  pub(super) fn change(&mut self, path: &str) -> Result<(), AdminError> {
    let kind = self.node(path).map(|entry| entry.kind).ok_or_else(|| self.not_found(path))?;
    self.fold(path, Action::Modify, kind, None);
    Ok(())
  }

  /// Sets the node at `path`'s text, marked modified.
  pub(super) fn set_text(&mut self, path: &str, text: Text) {
    if let Some(entry) = self.tree.get_mut(path) {
      entry.text = Some(text);
    }
    if let Some(change) = self.changes.get_mut(path) {
      change.text_modified = true;
    }
  }

  /// Sets the node at `path`'s property list, marked modified, and `svn:mergeinfo` modified when `mergeinfo` says one
  /// of its changes set or deleted it.
  pub(super) fn set_properties(&mut self, path: &str, properties: PropertyList, mergeinfo: bool) {
    if let Some(entry) = self.tree.get_mut(path) {
      entry.properties = properties;
    }
    if let Some(change) = self.changes.get_mut(path) {
      change.properties_modified = true;
      change.mergeinfo_modified |= mergeinfo;
    }
  }

  /// Refuses an addition at `path` where a node stands or its directory is absent or a file.
  fn check_addable(&self, path: &str, filesystem: Uuid) -> Result<(), AdminError> {
    if self.tree.contains_key(path) {
      return Err(AdminError::AlreadyExists {
        filesystem,
        transaction: self.name.clone(),
        path: fspath(path),
      });
    }
    let parent = dirname(path);
    match self.tree.get(parent) {
      Some(entry) if entry.kind == NodeKind::Directory => Ok(()),
      Some(_) => Err(AdminError::NotDirectory {
        path: fspath(parent),
        filesystem,
      }),
      None => Err(self.not_found(parent)),
    }
  }

  /// A path absent from the transaction.
  fn not_found(&self, path: &str) -> AdminError {
    AdminError::NotFound {
      place: Place::Transaction(self.name.clone()),
      path: fspath(path),
    }
  }

  /// Folds a change of `path` into what the transaction already holds for it (`fold_change`): an addition after a
  /// deletion is a replacement; an addition then a deletion leaves nothing; a change after an addition or a
  /// replacement stays one.
  fn fold(&mut self, path: &str, action: Action, kind: NodeKind, copy_from: Option<CopySource>) {
    let folded = match (self.changes.remove(path).map(|change| change.action), action) {
      (Some(Action::Delete), Action::Add) => Some(Action::Replace),
      (Some(Action::Add), Action::Delete) => None,
      (Some(Action::Replace), Action::Delete) => Some(Action::Delete),
      (Some(earlier @ (Action::Add | Action::Replace)), Action::Modify) => Some(earlier),
      (_, later) => Some(later),
    };
    if let Some(action) = folded {
      let change = Change {
        action,
        kind,
        copy_from,
        text_modified: false,
        properties_modified: false,
        mergeinfo_modified: false,
      };
      self.changes.insert(path.to_owned(), change);
    }
  }

  /// The revision record the transaction makes: its changed paths, as the index reads them.
  fn record(&self, repository: &Repository) -> RevisionRecord {
    let changes = self
      .changes
      .iter()
      .map(|(path, change)| self.row(path, change, repository))
      .collect();
    RevisionRecord {
      transaction: self.name.clone(),
      changes,
    }
  }

  /// One changed path's row.
  fn row(&self, path: &str, change: &Change, repository: &Repository) -> ChangedPath {
    let entry = self.tree.get(path);
    let text = entry
      .filter(|_| change.text_modified || new_file(change))
      .and_then(|entry| entry.text);
    let properties = entry
      .filter(|_| change.properties_modified)
      .map(|entry| entry.properties);
    let _ = repository;
    ChangedPath {
      path: fspath(path),
      action: change.action,
      kind: change.kind,
      copy_from: change.copy_from.clone(),
      text,
      properties,
      mergeinfo_modified: change.mergeinfo_modified,
    }
  }

  /// The transaction's base revision.
  pub(super) fn base(&self) -> Revision {
    self.base
  }
}

/// Whether a change makes a new file without history, whose (empty) text the index records with its row.
fn new_file(change: &Change) -> bool {
  change.kind == NodeKind::File
    && matches!(change.action, Action::Add | Action::Replace)
    && change.copy_from.is_none()
}

/// Whether `path` is `ancestor` or beneath it; every path is beneath the root.
pub(super) fn at_or_beneath(path: &str, ancestor: &str) -> bool {
  ancestor.is_empty()
    || path == ancestor
    || path
      .strip_prefix(ancestor)
      .is_some_and(|rest| rest.as_bytes().first() == Some(&SEPARATOR))
}

/// `existing`, beneath `from`, moved beneath `to`.
fn moved(existing: &str, from: &str, to: &str) -> String {
  let rest = existing.get(from.len()..).unwrap_or_default();
  if to.is_empty() {
    rest.trim_start_matches('/').to_owned()
  } else {
    format!("{to}{rest}")
  }
}

/// The directory of a path; the root's is the root.
pub(super) fn dirname(path: &str) -> &str {
  path.rfind('/').and_then(|end| path.get(..end)).unwrap_or_default()
}

/// A relative path as the filesystem writes it: absolute, the root `/`.
pub(super) fn fspath(path: &str) -> String {
  format!("/{path}")
}

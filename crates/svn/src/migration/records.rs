//! A repository's records as the migration verbs read and write them (A-628): the Subversion index (A-553), each
//! revision's properties, every text and property list held once by its content identity, and the repository's UUID,
//! within a room its caller gives. A revision is made by a [`Transaction`], which applies one action at a time as FSFS
//! applies the calls `load-fs-vtable.c` makes and records the changed paths FSFS would (`transaction.c` `fold_change`),
//! then commits them to the index as one revision.

use std::collections::{BTreeMap, BTreeSet};

use super::MigrationRefusal;
use super::paths::{ROOT, ancestors_deepest_first, beneath};
use super::properties::{decode, encode, mergeinfo};
use crate::dump::{Md5, Sha1, Uuid};
use crate::index::{
  Action, ChangedPath, ContentIdentity, CopySource, Index, NodeKind, PropertyList, Revision,
  RevisionRecord, Text,
};

/// A node's or a revision's properties: names to values, sorted by name, every byte kept.
pub type Properties = BTreeMap<Vec<u8>, Vec<u8>>;

/// The revision property every repository's revision 0 holds from its creation.
/// Format: `SVN_PROP_REVISION_DATE` (`svn_props.h`), which `svn_fs_fs__create` sets on revision 0.
const DATE: &[u8] = b"svn:date";

/// The digits of a transaction's sequence number.
/// Format: FSFS names a transaction `<base revision>-<sequence>`, the sequence in base 36 with lowercase letters
/// (`transaction.c` `create_txn_dir`, `svn__ui64tobase36`); design §4.17.4 item 3.1 names a work's transaction so.
const BASE36_DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// The base of a transaction's sequence number.
/// Format: base 36 (`svn__ui64tobase36`).
const BASE36: u64 = 36;

/// The bound a repository's records are held within, the caller's: the server derives it from its shard's room, and
/// nothing here sets it. An import that would pass it is refused, typed, before anything past it is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Room {
  /// The most entries the index may hold, node rows and deletions ([`Index::append`]'s room).
  pub entries: usize,
  /// The most bytes the repository's texts, property lists and revision properties may take.
  pub bytes: u64,
}

/// A Subversion repository's records (the module documentation).
#[derive(Clone, Debug)]
pub struct Repository {
  /// The repository's UUID.
  uuid: Uuid,
  /// Every node-revision, computed from each revision's changed-path rows.
  index: Index,
  /// Each revision's properties, by revision.
  revision_properties: Vec<Properties>,
  /// Every text and property list a node-revision names, by content identity.
  contents: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The transactions begun, which numbers the next one (FSFS's `txn-current`).
  transactions: u64,
  /// The room the records are held within.
  room: Room,
  /// The bytes held: the contents and the revision properties.
  held: u64,
}

impl Repository {
  /// A repository as `CreateRepository` leaves one (A-575): revision 0, its root an empty directory, its one revision
  /// property `svn:date` set to `date`, the time its caller read for it, as `svnadmin create` sets revision 0's.
  ///
  /// # Errors
  /// [`MigrationRefusal::Room`] when the date alone passes the room.
  pub fn new(uuid: Uuid, date: &[u8], room: Room) -> Result<Self, MigrationRefusal> {
    let properties = Properties::from([(DATE.to_vec(), date.to_vec())]);
    let held = properties_bytes(&properties)?;
    check_room(held, room)?;
    Ok(Self {
      uuid,
      index: Index::new(),
      revision_properties: vec![properties],
      contents: BTreeMap::new(),
      transactions: 0,
      room,
      held,
    })
  }

  /// The repository's UUID.
  pub fn uuid(&self) -> Uuid {
    self.uuid
  }

  /// The Subversion index: every node-revision and every revision's changed paths.
  pub fn index(&self) -> &Index {
    &self.index
  }

  /// The youngest revision.
  pub fn youngest(&self) -> Revision {
    self.index.youngest()
  }

  /// The properties of `revision`, if the repository holds it.
  pub fn revision_properties(&self, revision: Revision) -> Option<&Properties> {
    usize::try_from(revision)
      .ok()
      .and_then(|position| self.revision_properties.get(position))
  }

  /// The bytes held under `identity`: a text, or a property list in FSFS's form.
  pub fn content(&self, identity: &ContentIdentity) -> Option<&[u8]> {
    self.contents.get(identity).map(Vec::as_slice)
  }

  /// The properties a node's property list holds.
  ///
  /// # Errors
  /// [`MigrationRefusal::Corrupt`] when the list is not held, or not in FSFS's form.
  pub fn properties(&self, list: PropertyList) -> Result<Properties, MigrationRefusal> {
    match list {
      PropertyList::Empty => Ok(Properties::new()),
      PropertyList::Held { identity, .. } => self
        .content(&identity)
        .and_then(decode)
        .ok_or(MigrationRefusal::Corrupt),
    }
  }

  /// The room the records are held within.
  pub fn room(&self) -> Room {
    self.room
  }

  /// The bytes the records hold: every text and property list once, and every revision's properties.
  pub fn held(&self) -> u64 {
    self.held
  }

  /// Names the repository by `uuid`.
  pub(super) fn set_uuid(&mut self, uuid: Uuid) {
    self.uuid = uuid;
  }

  /// Sets revision 0's properties to exactly `properties`, as `svnadmin load` sets them in an empty repository
  /// (`load-fs-vtable.c` `close_revision`: every difference from the stream's applied).
  pub(super) fn set_revision_zero(&mut self, properties: Properties) -> Result<(), MigrationRefusal> {
    let old = self
      .revision_properties
      .first()
      .map(properties_bytes)
      .transpose()?
      .unwrap_or(0);
    let held = self
      .held
      .checked_sub(old)
      .and_then(|rest| rest.checked_add(properties_bytes(&properties).ok()?))
      .ok_or(MigrationRefusal::Corrupt)?;
    check_room(held, self.room)?;
    match self.revision_properties.first_mut() {
      Some(zero) => *zero = properties,
      None => self.revision_properties.push(properties),
    }
    self.held = held;
    Ok(())
  }

  /// A transaction on the youngest revision, named as FSFS names it: the base, a `-`, and the repository's count of
  /// transactions begun in base 36.
  pub(super) fn begin(&mut self) -> Result<Transaction, MigrationRefusal> {
    let sequence = self.transactions;
    self.transactions = sequence
      .checked_add(1)
      .ok_or(MigrationRefusal::Corrupt)?;
    let base = self.youngest();
    Ok(Transaction {
      name: format!("{base}-{}", base36(sequence)),
      base,
      changes: BTreeMap::new(),
      pending: BTreeMap::new(),
      pending_bytes: 0,
    })
  }

  /// Commits `transaction` as the next revision with `properties` as its revision properties: its changed paths
  /// appended to the index and the contents they name kept, or nothing changed when it is refused.
  pub(super) fn commit(
    &mut self,
    mut transaction: Transaction,
    properties: Properties,
  ) -> Result<Revision, MigrationRefusal> {
    let changes = transaction.rows(self)?;
    let fresh = transaction.take_fresh_contents(self, &changes)?;
    let added = fresh
      .iter()
      .try_fold(properties_bytes(&properties)?, |total, (_, bytes)| {
        total.checked_add(u64::try_from(bytes.len()).ok()?)
      })
      .ok_or(MigrationRefusal::Corrupt)?;
    let held = self
      .held
      .checked_add(added)
      .ok_or(MigrationRefusal::Corrupt)?;
    check_room(held, self.room)?;
    let record = RevisionRecord {
      transaction: transaction.name,
      changes,
    };
    let revision = self
      .index
      .append(record, self.room.entries)
      .map_err(MigrationRefusal::Index)?;
    self.contents.extend(fresh);
    self.revision_properties.push(properties);
    self.held = held;
    Ok(revision)
  }
}

/// Refuses `held` bytes past `room`.
fn check_room(held: u64, room: Room) -> Result<(), MigrationRefusal> {
  if held > room.bytes {
    return Err(MigrationRefusal::Room {
      needed: held,
      room: room.bytes,
    });
  }
  Ok(())
}

/// The bytes a revision's properties take: every name and every value.
fn properties_bytes(properties: &Properties) -> Result<u64, MigrationRefusal> {
  properties
    .iter()
    .try_fold(0u64, |total, (name, value)| {
      total
        .checked_add(u64::try_from(name.len()).ok()?)?
        .checked_add(u64::try_from(value.len()).ok()?)
    })
    .ok_or(MigrationRefusal::Corrupt)
}

/// `number` in base 36, as FSFS writes a transaction's sequence.
fn base36(number: u64) -> String {
  let mut digits = Vec::new();
  let mut rest = number;
  loop {
    let digit = rest
      .checked_rem(BASE36)
      .and_then(|place| usize::try_from(place).ok())
      .and_then(|place| BASE36_DIGITS.get(place))
      .copied()
      .unwrap_or(b'0');
    digits.push(char::from(digit));
    rest = rest.checked_div(BASE36).unwrap_or(0);
    if rest == 0 {
      break;
    }
  }
  digits.iter().rev().collect()
}

/// What a node is, as a transaction reads it: its kind, its text and its property list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Facts {
  /// Its kind.
  pub(super) kind: NodeKind,
  /// Its text: none for a directory and for a file no text was set on.
  pub(super) text: Option<Text>,
  /// Its property list.
  pub(super) properties: PropertyList,
}

/// What a transaction did at one path so far: the changed path FSFS records, folded as `fold_change` folds one path's
/// calls (an addition after a deletion a replacement, a deletion of an addition nothing, a modification keeping an
/// addition's action).
#[derive(Clone, Debug)]
struct Change {
  /// What the transaction did to the path.
  action: Action,
  /// The copy source of an addition or a replacement made by a copy.
  copy_from: Option<CopySource>,
  /// The node the path holds now; none after a deletion.
  now: Option<Facts>,
  /// What the node the path holds now started the transaction from: its copy source, nothing for a new node, or
  /// itself before its first change. Its `svn:mergeinfo` is what a row's mergeinfo flag compares with.
  origin: Option<Facts>,
  /// The kind of the node the path held before the transaction changed it, which a deletion's row names.
  held_before: Option<NodeKind>,
  /// Whether a text was set (FSFS's `text-mod`).
  text_set: bool,
  /// Whether a property call was made (FSFS's `prop-mod`).
  properties_set: bool,
}

/// One revision being made from a dump's records (the module documentation).
#[derive(Clone, Debug)]
pub(super) struct Transaction {
  /// Its name.
  name: String,
  /// The revision it is made on.
  base: Revision,
  /// What it did at each absolute path.
  changes: BTreeMap<String, Change>,
  /// The texts and property lists it made that the repository does not hold, by content identity.
  pending: BTreeMap<ContentIdentity, Vec<u8>>,
  /// Their bytes.
  pending_bytes: u64,
}

impl Transaction {
  /// The revision the transaction is made on.
  pub(super) fn base(&self) -> Revision {
    self.base
  }

  /// The bytes the transaction holds that the repository does not.
  pub(super) fn pending_bytes(&self) -> u64 {
    self.pending_bytes
  }

  /// The node at `path` in the transaction, or none where the path is absent.
  pub(super) fn lookup(
    &self,
    repository: &Repository,
    path: &str,
  ) -> Result<Option<Facts>, MigrationRefusal> {
    if let Some(change) = self.changes.get(path) {
      return Ok(change.now);
    }
    for (ancestor, rest) in ancestors_deepest_first(path) {
      let Some(change) = self.changes.get(ancestor) else {
        continue;
      };
      match (change.action, &change.copy_from) {
        (Action::Modify, _) => {}
        (Action::Delete, _) | (Action::Add | Action::Replace, None) => return Ok(None),
        (Action::Add | Action::Replace, Some(source)) => {
          return node(repository, &beneath(&source.path, rest), source.revision);
        }
      }
    }
    node(repository, path, self.base)
  }

  /// Records a modification of the node `facts` at `path`, which a property section or a text then changes.
  pub(super) fn modify(&mut self, path: &str, facts: Facts) {
    self.changes.entry(path.to_owned()).or_insert(Change {
      action: Action::Modify,
      copy_from: None,
      now: Some(facts),
      origin: Some(facts),
      held_before: Some(facts.kind),
      text_set: false,
      properties_set: false,
    });
  }

  /// Deletes the node `facts` at `path` and everything beneath it, as `svn_fs_delete` does; what the transaction did
  /// beneath it goes (`fold_change`'s caller drops a deleted path's children).
  pub(super) fn delete(&mut self, path: &str, facts: Facts) {
    let mut prefix = path.to_owned();
    if path != ROOT {
      prefix.push('/');
    }
    let beneath: Vec<String> = self
      .changes
      .range(prefix.clone()..)
      .take_while(|(key, _)| key.starts_with(&prefix))
      .map(|(key, _)| key.clone())
      .collect();
    for key in beneath {
      self.changes.remove(&key);
    }
    let held_before = match self.changes.remove(path) {
      Some(earlier) if earlier.action == Action::Add => return,
      Some(earlier) => earlier.held_before,
      None => Some(facts.kind),
    };
    self.changes.insert(
      path.to_owned(),
      Change {
        action: Action::Delete,
        copy_from: None,
        now: None,
        origin: None,
        held_before,
        text_set: false,
        properties_set: false,
      },
    );
  }

  /// Adds at the absent `path` a new node of `kind`, or a copy of `source`, whose node is `copied`.
  pub(super) fn add(&mut self, path: &str, kind: NodeKind, source: Option<(CopySource, Facts)>) {
    let (copy_from, origin) = match source {
      Some((source, facts)) => (Some(source), Some(facts)),
      None => (None, None),
    };
    let now = origin.unwrap_or(Facts {
      kind,
      text: None,
      properties: PropertyList::Empty,
    });
    let (action, held_before) = match self.changes.remove(path) {
      Some(earlier) => (Action::Replace, earlier.held_before),
      None => (Action::Add, None),
    };
    self.changes.insert(
      path.to_owned(),
      Change {
        action,
        copy_from,
        now: Some(now),
        origin,
        held_before,
        text_set: false,
        properties_set: false,
      },
    );
  }

  /// Sets the property list of the node at `path` to `properties`, as one or more property calls do.
  pub(super) fn set_properties(
    &mut self,
    repository: &Repository,
    path: &str,
    properties: &Properties,
  ) -> Result<(), MigrationRefusal> {
    let identity = self.store(repository, encode(properties))?;
    let list = PropertyList::Held {
      identity,
      has_mergeinfo: mergeinfo(properties).is_some(),
    };
    let change = self.changes.get_mut(path).ok_or(MigrationRefusal::Corrupt)?;
    let now = change.now.as_mut().ok_or(MigrationRefusal::Corrupt)?;
    now.properties = list;
    change.properties_set = true;
    Ok(())
  }

  /// Sets the text of the file at `path` to `bytes`, whose digests are `digests`.
  pub(super) fn set_text(
    &mut self,
    repository: &Repository,
    path: &str,
    bytes: Vec<u8>,
    digests: (Md5, Sha1),
  ) -> Result<(), MigrationRefusal> {
    let size = u64::try_from(bytes.len()).map_err(|_| MigrationRefusal::Corrupt)?;
    let identity = self.store(repository, bytes)?;
    let (md5, sha1) = digests;
    let change = self.changes.get_mut(path).ok_or(MigrationRefusal::Corrupt)?;
    let now = change.now.as_mut().ok_or(MigrationRefusal::Corrupt)?;
    now.text = Some(Text {
      identity,
      md5,
      sha1,
      size,
    });
    change.text_set = true;
    Ok(())
  }

  /// The bytes held under `identity`, by the transaction or the repository.
  pub(super) fn content<'a>(
    &'a self,
    repository: &'a Repository,
    identity: &ContentIdentity,
  ) -> Option<&'a [u8]> {
    self
      .pending
      .get(identity)
      .map(Vec::as_slice)
      .or_else(|| repository.content(identity))
  }

  /// Holds `bytes` under their content identity, once; refused when the repository's room cannot take them.
  fn store(&mut self, repository: &Repository, bytes: Vec<u8>) -> Result<ContentIdentity, MigrationRefusal> {
    let identity = *blake3::hash(&bytes).as_bytes();
    if self.content(repository, &identity).is_some() {
      return Ok(identity);
    }
    let size = u64::try_from(bytes.len()).map_err(|_| MigrationRefusal::Corrupt)?;
    let pending_bytes = self
      .pending_bytes
      .checked_add(size)
      .ok_or(MigrationRefusal::Corrupt)?;
    let needed = repository
      .held()
      .checked_add(pending_bytes)
      .ok_or(MigrationRefusal::Corrupt)?;
    check_room(needed, repository.room())?;
    self.pending.insert(identity, bytes);
    self.pending_bytes = pending_bytes;
    Ok(identity)
  }

  /// The changed paths FSFS would record for the transaction: a modification that set nothing records nothing, as
  /// a dump's change record without a section makes no call.
  fn rows(&self, repository: &Repository) -> Result<Vec<ChangedPath>, MigrationRefusal> {
    let mut rows = Vec::with_capacity(self.changes.len());
    for (path, change) in &self.changes {
      if let Some(row) = self.row(repository, path, change)? {
        rows.push(row);
      }
    }
    Ok(rows)
  }

  /// One path's changed path, if it records one.
  fn row(
    &self,
    repository: &Repository,
    path: &str,
    change: &Change,
  ) -> Result<Option<ChangedPath>, MigrationRefusal> {
    let Some(now) = change.now else {
      let kind = change.held_before.ok_or(MigrationRefusal::Corrupt)?;
      return Ok(Some(deletion(path, kind)));
    };
    if change.action == Action::Modify && !change.text_set && !change.properties_set {
      return Ok(None);
    }
    let mergeinfo_modified = change.properties_set
      && self.mergeinfo(repository, change.origin.map(|origin| origin.properties))?
        != self.mergeinfo(repository, Some(now.properties))?;
    Ok(Some(ChangedPath {
      path: path.to_owned(),
      action: change.action,
      kind: now.kind,
      copy_from: change.copy_from.clone(),
      text: now.text.filter(|_| change.text_set),
      properties: change.properties_set.then_some(now.properties),
      mergeinfo_modified,
    }))
  }

  /// The `svn:mergeinfo` a property list holds, if any.
  fn mergeinfo(
    &self,
    repository: &Repository,
    list: Option<PropertyList>,
  ) -> Result<Option<Vec<u8>>, MigrationRefusal> {
    let Some(PropertyList::Held {
      identity,
      has_mergeinfo: true,
    }) = list
    else {
      return Ok(None);
    };
    let properties = self
      .content(repository, &identity)
      .and_then(decode)
      .ok_or(MigrationRefusal::Corrupt)?;
    Ok(mergeinfo(&properties).map(<[u8]>::to_vec))
  }

  /// The contents `changes` name that the repository does not hold, taken out of the transaction.
  fn take_fresh_contents(
    &mut self,
    repository: &Repository,
    changes: &[ChangedPath],
  ) -> Result<Vec<(ContentIdentity, Vec<u8>)>, MigrationRefusal> {
    let mut named = BTreeSet::new();
    for change in changes {
      if let Some(text) = change.text {
        named.insert(text.identity);
      }
      if let Some(PropertyList::Held { identity, .. }) = change.properties {
        named.insert(identity);
      }
    }
    let mut fresh = Vec::new();
    for identity in named {
      if repository.content(&identity).is_none() {
        let bytes = self
          .pending
          .remove(&identity)
          .ok_or(MigrationRefusal::Corrupt)?;
        fresh.push((identity, bytes));
      }
    }
    Ok(fresh)
  }
}

/// A deletion's changed path.
fn deletion(path: &str, kind: NodeKind) -> ChangedPath {
  ChangedPath {
    path: path.to_owned(),
    action: Action::Delete,
    kind,
    copy_from: None,
    text: None,
    properties: None,
    mergeinfo_modified: false,
  }
}

/// The node at `path` in `revision` of the repository's index, or none where the path is absent.
pub(super) fn node(
  repository: &Repository,
  path: &str,
  revision: Revision,
) -> Result<Option<Facts>, MigrationRefusal> {
  let node = repository
    .index()
    .node(path, revision)
    .map_err(MigrationRefusal::Index)?;
  Ok(node.map(|node| Facts {
    kind: node.kind(),
    text: node.text(),
    properties: node.properties(),
  }))
}

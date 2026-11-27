//! A repository's records (A-628): the Subversion index (A-553), each revision's properties, every text and property
//! list held once by its content identity, the lock table (A-602) and the repository's UUID, within a room its caller
//! gives. A dump's revision is made by a [`Transaction`], which applies one action at a time as FSFS applies the calls
//! `load-fs-vtable.c` makes and records the changed paths FSFS would (`transaction.c` `fold_change`).
//!
//! **The commit step** (A-655), one for a client's commit and an import's revision alike: [`Repository::check`]
//! judges a [`RevisionToCommit`] against the records and changes nothing, so its owner checks a revision before its log
//! records it; [`Repository::apply`] commits what was checked, so the owner applies it after, and a recovery that
//! checks and applies the logged revision again reaches the same records ([`Repository::commit`] is the two at once).
//! Beside revisions, an import sets the repository's UUID and revision 0's properties (A-652 part 3), each a record of
//! its own the owner logs before it applies it ([`Repository::check_revision_properties`], then
//! [`Repository::set_revision_properties`]; [`Repository::set_uuid`]); [`Repository::commit_record`] commits any of
//! them as its step does.
//! Checked, in order: the revision's head is the youngest ("Transaction out of date" otherwise, as FSFS's
//! `commit_body` answers a transaction whose base moved, `transaction.c` 3784-3786; the owner's step judges a moved work
//! again by position before it comes here, as `svn_fs_fs__commit_txn` merges again); its lock entries apply; every
//! text and property list its rows name is held by the records or brought by the revision; its properties and new
//! contents fit the room; the index takes its rows ([`Index::plan`]). A refused revision changes nothing.
//!
//! **Texts** (C2 stage B part 2 (b); the KeptText move, main's 10:35 item 3 of 2026-11-21). The records keep no text.
//! A row's text is named stored, by its length ([`Repository::stored_len`]); its bytes are where the records' caller
//! holds them: a daemon's on the node's changeset store and its shard's content store, records standing alone in
//! [`super::Standalone`]. A caller takes a revision's texts out before its step ([`Repository::take_texts`]), and the
//! step refuses a text that arrives inline (`Corrupt`). Property lists are kept whole and read in place
//! ([`Repository::content`]); a text whose bytes a property list holds too is read as that list's bytes. An import's
//! revision arrives with its texts named stored already: its [`Transaction`] holds none, each put into the import's
//! sink as the dump was read (A-862). Before, records
//! kept every text as an svndiff of version 2 against its node's skip-delta predecessor (S4 slice 2 c2, A-727 (c)): a
//! second text path a daemon no longer took after C2 stage B, removed with its delta chains and window reads.
//!
//! A repository is also the drive's [`Green`] (A-617): it reads the texts and property lists it holds and names new
//! ones as it keeps them, so a client's drive and its commit run over one set of records.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use super::MigrationRefusal;
use super::paths::{ROOT, ancestors_deepest_first, beneath};
use super::properties::{MERGEINFO, decode, encode, entries, mergeinfo};
use crate::commit::drive::Green;
use crate::commit::record::Record;
use crate::commit::revision::{Revision as RevisionToCommit, StepRefusal};
use crate::commit::{CommitRefusal, verdict};
use crate::dump::Uuid;
use crate::index::{
  Action, ChangedPath, ContentIdentity, CopySource, Index, NodeKind, PlanStep, Planned, Planning,
  PropertyList, Revision, RevisionRecord, Text,
};
use crate::lock::{AprTime, CommitLocks, LockEntry, LockTable};
use crate::mergeinfo::Contents;
use crate::reads::Repository as ReadSource;
use crate::svndiff;

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
  /// Every property list a node-revision names, by content identity, whole (FSFS's form, read in place).
  contents: BTreeMap<ContentIdentity, Vec<u8>>,
  /// Every text a commit brought through its sink (§4.17.1 "A push's intake"; main's ruling of 2026-11-20 on c2 and
  /// (B)), by content identity, with its length: held once in the repository's content store, read through the lent
  /// [`TextSource`], never copied or deltified here.
  stored: BTreeMap<ContentIdentity, u64>,
  /// The locks (A-602).
  locks: LockTable,
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
      stored: BTreeMap::new(),
      locks: LockTable::new(),
      transactions: 0,
      room,
      held,
    })
  }

  /// The lock table.
  pub fn locks(&self) -> &LockTable {
    &self.locks
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

  /// The property list held under `identity`, in FSFS's form, read in place. A text is read by [`Repository::text`].
  pub fn content(&self, identity: &ContentIdentity) -> Option<&[u8]> {
    self.contents.get(identity).map(Vec::as_slice)
  }

  /// Whether the records hold a text or a property list under `identity`.
  pub fn holds(&self, identity: &ContentIdentity) -> bool {
    self.contents.contains_key(identity) || self.stored.contains_key(identity)
  }

  /// The length of a text held in the content store, none for a text the records do not keep as stored.
  pub fn stored_len(&self, identity: &ContentIdentity) -> Option<u64> {
    self.stored.get(identity).copied()
  }

  /// The texts held in the content store, each with its length ([`Repository`]'s `stored`).
  pub fn stored_texts(&self) -> impl Iterator<Item = (&ContentIdentity, u64)> + '_ {
    self.stored.iter().map(|(identity, len)| (identity, *len))
  }

  /// Whether the records hold `identity` as the kind a row names it as: a text (named stored, or bytes a property list
  /// holds too) or a property list (whole, read in place). The same bytes may be both.
  fn holds_as(&self, identity: &ContentIdentity, as_text: bool) -> bool {
    if as_text {
      self.holds(identity)
    } else {
      self.contents.contains_key(identity)
    }
  }

  /// The text held under `identity` whose bytes the records hold themselves: a property list's bytes, read as a text
  /// (module docs, "Texts"); none for a text named stored, whose bytes are where the records' caller holds them.
  pub fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    self
      .contents
      .get(identity)
      .map(|bytes| Cow::Borrowed(bytes.as_slice()))
  }

  /// [`Repository::text`] from `offset` to its end: empty at its end, none past it or for a text the records do not
  /// hold. `cache` is a reader's window cache, which a text held in one piece does not need.
  pub fn text_in<'a>(
    &'a self,
    identity: &ContentIdentity,
    offset: u64,
    _cache: &'a mut crate::reads::WindowCache,
  ) -> Option<Cow<'a, [u8]>> {
    from_offset(self.text(identity)?, offset)
  }

  /// [`Repository::text`] from `offset` to its end, copied: empty at its end, none past it or for a text the records do
  /// not hold.
  pub fn text_at(&self, identity: &ContentIdentity, offset: u64) -> Option<Vec<u8>> {
    self
      .contents
      .get(identity)?
      .get(usize::try_from(offset).ok()?..)
      .map(<[u8]>::to_vec)
  }

  /// Keeps whole the property lists `changes` name that `fresh` brings (module docs, "Texts": no text is kept).
  fn keep_lists(&mut self, changes: &[ChangedPath], fresh: BTreeMap<ContentIdentity, Vec<u8>>) {
    let lists: BTreeSet<ContentIdentity> = changes
      .iter()
      .filter_map(|change| match change.properties {
        Some(PropertyList::Held { identity, .. }) => Some(identity),
        _ => None,
      })
      .collect();
    for (identity, bytes) in fresh {
      if lists.contains(&identity) {
        self.contents.insert(identity, bytes);
      }
    }
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

  /// Sets the room the records are held within: their owner lends each step the room its shard could admit then (A-655
  /// part 2b), as it lends a content store its budget. A room below what the records hold refuses any growth and
  /// changes nothing they hold.
  pub fn set_room(&mut self, room: Room) {
    self.room = room;
  }

  /// The bytes the records hold: every text and property list once, and every revision's properties.
  pub fn held(&self) -> u64 {
    self.held
  }

  /// Names the repository by `uuid` (`svn_fs_set_uuid`), as an import names a repository that holds no revision but 0
  /// by its stream's UUID record (A-652 part 3).
  pub fn set_uuid(&mut self, uuid: Uuid) {
    self.uuid = uuid;
  }

  /// The check of [`Repository::set_revision_properties`], which changes nothing: the bytes the records would hold
  /// more once `revision`'s properties are `properties`, none where they would hold fewer.
  ///
  /// # Errors
  /// [`StepRefusal::Corrupt`] for a revision the records do not hold; [`StepRefusal::Room`] past the room.
  pub fn check_revision_properties(
    &self,
    revision: Revision,
    properties: &Properties,
  ) -> Result<u64, StepRefusal> {
    let held = self.held_with_properties(revision, properties)?;
    Ok(held.saturating_sub(self.held))
  }

  /// Sets `revision`'s properties to exactly `properties` (`svn_fs_change_rev_prop2` for each difference), as
  /// `svnadmin load` sets revision 0's in an empty repository (`load-fs-vtable.c` `close_revision`) and an import
  /// does (A-652 part 3).
  ///
  /// # Errors
  /// Each refusal of [`Repository::check_revision_properties`]'s; refused, nothing changes.
  pub fn set_revision_properties(
    &mut self,
    revision: Revision,
    properties: Properties,
  ) -> Result<(), StepRefusal> {
    let held = self.held_with_properties(revision, &properties)?;
    let slot = usize::try_from(revision)
      .ok()
      .and_then(|position| self.revision_properties.get_mut(position))
      .ok_or(StepRefusal::Corrupt)?;
    *slot = properties;
    self.held = held;
    Ok(())
  }

  /// The bytes the records would hold with `revision`'s properties replaced by `properties`, refused for a revision
  /// they do not hold and past the room.
  fn held_with_properties(
    &self,
    revision: Revision,
    properties: &Properties,
  ) -> Result<u64, StepRefusal> {
    let old = self
      .revision_properties(revision)
      .ok_or(StepRefusal::Corrupt)?;
    let corrupt = |_| StepRefusal::Corrupt;
    let held = self
      .held
      .checked_sub(properties_bytes(old).map_err(corrupt)?)
      .and_then(|rest| rest.checked_add(properties_bytes(properties).ok()?))
      .ok_or(StepRefusal::Corrupt)?;
    if held > self.room.bytes {
      return Err(StepRefusal::Room {
        needed: held,
        room: self.room.bytes,
      });
    }
    Ok(held)
  }

  /// Commits `record` as the commit step commits it (A-652 part 3): a revision checked and applied
  /// ([`Repository::commit`]), the UUID set, a revision's properties set whole, a lock verb's lock entries applied. A creation is refused: a repository is
  /// made by its creation's record once ([`Repository::new`]). What a caller that holds the records alone commits an
  /// import's records through, and what a recovery replays every record after the creation's through.
  ///
  /// # Errors
  /// [`StepRefusal::Corrupt`] for a creation; each refusal of the record's own step; refused, nothing changes.
  pub fn commit_record(&mut self, record: Record) -> Result<(), StepRefusal> {
    match record {
      Record::Created { .. } => Err(StepRefusal::Corrupt),
      Record::Uuid(uuid) => {
        self.set_uuid(uuid);
        Ok(())
      }
      Record::RevisionProperties {
        revision,
        properties,
      } => self.set_revision_properties(revision, properties),
      Record::Revision(revision) => self.commit(revision).map(drop),
      Record::Locks(entries) => self.apply_lock_entries(&entries),
    }
  }

  /// Takes out of `revision`'s contents every text its rows name that the records do not hold, and names each in its
  /// stored texts by its length instead (C2 stage B part 2 (b)): the texts its caller holds apart, a daemon on the
  /// node's store, records standing alone in [`super::Standalone`]. A text whose bytes a row also names as a property
  /// list stays among the contents, the records keeping a list whole.
  pub fn take_texts(&self, revision: &mut RevisionToCommit) -> BTreeMap<ContentIdentity, Vec<u8>> {
    let lists: BTreeSet<ContentIdentity> = revision
      .record
      .changes
      .iter()
      .filter_map(|change| match change.properties {
        Some(PropertyList::Held { identity, .. }) => Some(identity),
        _ => None,
      })
      .collect();
    let mut texts = BTreeMap::new();
    for text in revision
      .record
      .changes
      .iter()
      .filter_map(|change| change.text)
    {
      if self.holds(&text.identity) || texts.contains_key(&text.identity) {
        continue;
      }
      let bytes = if lists.contains(&text.identity) {
        revision.contents.get(&text.identity).cloned()
      } else {
        revision.contents.remove(&text.identity)
      };
      if let Some(bytes) = bytes {
        revision.stored.insert(text.identity, text.size);
        texts.insert(text.identity, bytes);
      }
    }
    texts
  }

  /// A transaction on the youngest revision, named as FSFS names it: the base, a `-`, and the repository's count of
  /// transactions begun in base 36.
  pub(super) fn begin(&mut self) -> Result<Transaction, MigrationRefusal> {
    let sequence = self.transactions;
    self.transactions = sequence.checked_add(1).ok_or(MigrationRefusal::Corrupt)?;
    let base = self.youngest();
    Ok(Transaction {
      name: format!("{base}-{}", base36(sequence)),
      base,
      changes: BTreeMap::new(),
      pending: BTreeMap::new(),
      pending_bytes: 0,
      stored: BTreeMap::new(),
    })
  }

  /// The close of `transaction` begun (A-920 7b): the revision it makes with `properties` as its revision properties,
  /// made by [`Repository::close_step`] a unit at a time while the repository stands as it is.
  pub(super) fn close_begin(
    &self,
    mut transaction: Transaction,
    properties: Properties,
  ) -> Closing {
    let changes = std::mem::take(&mut transaction.changes);
    Closing {
      rows: Vec::with_capacity(changes.len()),
      transaction,
      properties,
      changes,
      phase: ClosePhase::Rows,
      named: BTreeSet::new(),
      fresh: BTreeMap::new(),
      stored: BTreeMap::new(),
    }
  }

  /// One step of `closing` ([`Repository::close_begin`]), a unit at least, until `spent` says the step's slice is spent,
  /// stopping between its phases where it is: the changed path each path records (FSFS's rows), a path a unit; the
  /// contents the rows name, a row a unit; those the repository does not hold taken out of the transaction, a name a
  /// unit; then the texts the transaction set that the rows name, each with its length (named stored, their bytes the
  /// sink's), a row a unit. Closed, the revision made on the youngest revision, exactly the stream's, with no lock
  /// entries: what the import commits through the step. (A-920 7b: closed whole in one call, 20.4 ms debug at 9,000
  /// files.)
  ///
  /// # Errors
  /// [`MigrationRefusal::Corrupt`] for a deletion of a path the transaction never saw held, a property list whose
  /// `svn:mergeinfo` cannot be read, or a content the rows name that neither the transaction nor the repository holds.
  pub(super) fn close_step(
    &self,
    mut closing: Closing,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<CloseStep, MigrationRefusal> {
    let mut step_spent = || spent() || close_a_unit_a_step();
    loop {
      let ended = match closing.phase {
        ClosePhase::Rows => closing.rows_some(self, &mut step_spent)?,
        ClosePhase::Naming(at) => closing.name_some(at, &mut step_spent),
        ClosePhase::Fresh => closing.fresh_some(self, &mut step_spent)?,
        ClosePhase::Stored(at) => closing.stored_some(self, at, &mut step_spent),
        ClosePhase::Closed => return Ok(CloseStep::Closed(Box::new(closing.closed()))),
      };
      if !ended || step_spent() {
        return Ok(CloseStep::More(Box::new(closing)));
      }
    }
  }

  /// The commit step's check (the module documentation): `revision` judged against the records, nothing changed. The
  /// answer is what [`Repository::apply`] commits.
  ///
  /// # Errors
  /// [`StepRefusal::OutOfDate`] when the youngest revision is not the revision's head; [`StepRefusal::Lock`] for a lock
  /// entry the table does not take; [`StepRefusal::Corrupt`] for a text or property list the rows name that neither the
  /// records nor the revision hold; [`StepRefusal::Room`] past the room's bytes; the index's refusal of the rows, its
  /// room's among them.
  pub fn check(&self, revision: RevisionToCommit) -> Result<Checked, StepRefusal> {
    let mut checking = self.check_begin(revision)?;
    loop {
      match self.check_step(checking, &mut whole_check_spent)? {
        CheckStep::More(more) => checking = *more,
        CheckStep::Checked(checked) => return Ok(*checked),
      }
    }
  }

  /// [`Repository::check`] begun (A-920 pieces 2 and 2b): the revision's head and lock entries checked, every other
  /// check made by [`Repository::check_step`] a slice at a time while the repository stands as it is.
  ///
  /// # Errors
  /// [`StepRefusal::OutOfDate`] when the youngest revision is not the revision's head; [`StepRefusal::Lock`] for a lock
  /// entry the table does not take.
  pub fn check_begin(&self, revision: RevisionToCommit) -> Result<Checking, StepRefusal> {
    let youngest = self.youngest();
    if revision.head != youngest {
      return Err(StepRefusal::OutOfDate {
        head: revision.head,
        youngest,
      });
    }
    self
      .locks
      .check_entries(&revision.lock_entries)
      .map_err(StepRefusal::Lock)?;
    Ok(Checking {
      phase: CheckPhase::Naming(0),
      record: Some(revision.record),
      planning: None,
      named: BTreeSet::new(),
      brought: revision.contents,
      stored_in: revision.stored,
      properties: revision.properties,
      fresh: BTreeMap::new(),
      stored: BTreeMap::new(),
      lock_entries: revision.lock_entries,
    })
  }

  /// One step of `checking` ([`Repository::check_begin`]), a unit at least, until `spent` says the step's slice is
  /// spent, stopping between its phases where it is: the texts and property lists its rows name, a row a unit; those
  /// the records do not hold taken from what the revision brought (or named stored), a name a unit, then the room
  /// checked; the texts its commit stored that the records do not hold, a text a unit; then the index's plan of its
  /// rows, a row a unit ([`Index::plan_step`]). Checked once every row is.
  ///
  /// # Errors
  /// [`Repository::check`]'s: [`StepRefusal::Corrupt`] for a text or property list the rows name that neither the
  /// records nor the revision hold; [`StepRefusal::Room`] past the room's bytes; the index's refusals of the rows, a
  /// revision committed since the check began among them.
  pub fn check_step(
    &self,
    mut checking: Checking,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<CheckStep, StepRefusal> {
    let mut first = true;
    loop {
      let ended = match checking.phase {
        CheckPhase::Naming(at) => checking.name_some(at, spent),
        CheckPhase::Fresh => self.fresh_some(&mut checking, spent)?,
        CheckPhase::Stored => self.stored_some(&mut checking, spent)?,
        // A whole part begins a step, never ends one past its quantum (A-920 piece 2c).
        CheckPhase::PlanBegin if !first => return Ok(CheckStep::More(Box::new(checking))),
        CheckPhase::PlanBegin => self.plan_begin_now(&mut checking)?,
        CheckPhase::Planning => return self.plan_some(checking, spent),
      };
      first = false;
      if !ended || spent() {
        return Ok(CheckStep::More(Box::new(checking)));
      }
    }
  }

  /// [`Repository::check_step`]'s names, a name a unit until `spent`: each the records do not hold taken from what the
  /// revision brought, a text it names stored passed; then the room checked. Whether the phase ended.
  fn fresh_some(
    &self,
    checking: &mut Checking,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<bool, StepRefusal> {
    while let Some((identity, as_text)) = checking.named.pop_first() {
      if !self.holds_as(&identity, as_text) && !checking.fresh.contains_key(&identity) {
        checking.take_fresh(identity, as_text)?;
      }
      if !checking.named.is_empty() && spent() {
        return Ok(false);
      }
    }
    self.grown(&checking.properties, &checking.fresh)?;
    checking.phase = CheckPhase::Stored;
    Ok(true)
  }

  /// [`Repository::check_step`]'s stored texts, a text a unit until `spent`: those the records do not hold kept, the
  /// index's plan to begin next. Whether the phase ended.
  fn stored_some(
    &self,
    checking: &mut Checking,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<bool, StepRefusal> {
    while let Some((identity, len)) = checking.stored_in.pop_first() {
      if !self.holds(&identity) {
        checking.stored.insert(identity, len);
      }
      if !checking.stored_in.is_empty() && spent() {
        return Ok(false);
      }
    }
    checking.phase = CheckPhase::PlanBegin;
    Ok(true)
  }

  /// [`Repository::check_step`]'s plan begun ([`Index::plan_begin`]: the rows sorted, the new root made; 3.4 ms whole at
  /// 9,000 files, debug), the first unit of a step of its own: whether the phase ended.
  fn plan_begin_now(&self, checking: &mut Checking) -> Result<bool, StepRefusal> {
    let record = checking.record.take().ok_or(StepRefusal::Corrupt)?;
    checking.planning = Some(self.index.plan_begin(record, self.room.entries)?);
    checking.phase = CheckPhase::Planning;
    Ok(true)
  }

  /// [`Repository::check_step`]'s plan of the rows, until `spent`: checked once every row is.
  fn plan_some(
    &self,
    mut checking: Checking,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<CheckStep, StepRefusal> {
    let planning = checking.planning.take().ok_or(StepRefusal::Corrupt)?;
    Ok(match self.index.plan_step(planning, spent)? {
      PlanStep::More(planning) => {
        checking.planning = Some(*planning);
        CheckStep::More(Box::new(checking))
      }
      PlanStep::Planned(planned) => CheckStep::Checked(Box::new(Checked {
        planned,
        properties: checking.properties,
        fresh: checking.fresh,
        stored: checking.stored,
        lock_entries: checking.lock_entries,
      })),
    })
  }

  /// The commit step's apply: commits what [`Repository::check`] checked as the next revision, its rows appended to
  /// the index, its new contents and its properties kept and its lock entries applied, and answers its number.
  ///
  /// # Errors
  /// [`StepRefusal::OutOfDate`] when a revision was committed since the check, and every refusal of the check's that a
  /// change since could bring (a lock entry, the room); refused, nothing changes.
  pub fn apply(&mut self, checked: Checked) -> Result<Revision, StepRefusal> {
    let mut applying = self.apply_begin(checked)?;
    loop {
      if let Some(revision) = self.apply_step(&mut applying, &mut whole_check_spent)? {
        return Ok(revision);
      }
    }
  }

  /// [`Repository::apply`] begun (A-920 piece 5b): the plan's revision still the next, its lock entries and the room
  /// checked again, its property lists kept; then [`Repository::apply_step`] names its stored texts and commits its
  /// rows a slice at a time. Refused, nothing changes.
  ///
  /// # Errors
  /// [`Repository::apply`]'s.
  pub fn apply_begin(&mut self, checked: Checked) -> Result<Applying, StepRefusal> {
    let Checked {
      planned,
      properties,
      fresh,
      stored,
      lock_entries,
    } = checked;
    let held = self.apply_checks(planned.revision(), (&properties, &fresh), &lock_entries)?;
    let committing = self.index.commit_begin(planned)?;
    // Its property lists kept now: read only through its rows, which no read reaches until its last step.
    self.keep_lists(committing.changes(), fresh);
    Ok(Applying {
      committing,
      stored,
      properties: Some(properties),
      lock_entries,
      held,
    })
  }

  /// One step of `applying` ([`Repository::apply_begin`]), a unit at least, until `spent`: its stored texts named, a
  /// text a unit; then its rows committed ([`Index::commit_step`]); in its last step, its lock entries applied, its
  /// properties kept and what the records hold. Until then the records read as they did before it began: what a step
  /// adds is reached only through the revision, and every read is at or below the youngest. The revision once applied;
  /// none while more remains.
  ///
  /// # Errors
  /// [`Index::commit_step`]'s, a revision committed since the apply began; a lock entry the table no longer takes.
  pub fn apply_step(
    &mut self,
    applying: &mut Applying,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<Option<Revision>, StepRefusal> {
    while let Some((identity, len)) = applying.stored.pop_first() {
      self.stored.entry(identity).or_insert(len);
      if spent() {
        return Ok(None);
      }
    }
    let Some(revision) = self.index.commit_step(&mut applying.committing, spent)? else {
      return Ok(None);
    };
    self
      .locks
      .apply_all(&applying.lock_entries)
      .map_err(StepRefusal::Lock)?;
    self
      .revision_properties
      .push(applying.properties.take().unwrap_or_default());
    self.held = applying.held;
    Ok(Some(revision))
  }

  /// What applying a revision checks again of the records as they stand now: `planned` the next revision, its lock
  /// entries taken, the room holding its properties and fresh contents; what the records then hold.
  fn apply_checks(
    &self,
    planned: Revision,
    (properties, fresh): (&Properties, &BTreeMap<ContentIdentity, Vec<u8>>),
    lock_entries: &[LockEntry],
  ) -> Result<u64, StepRefusal> {
    let youngest = self.youngest();
    if planned != youngest.checked_add(1).ok_or(StepRefusal::Corrupt)? {
      return Err(StepRefusal::OutOfDate {
        head: planned.saturating_sub(1),
        youngest,
      });
    }
    self
      .locks
      .check_entries(lock_entries)
      .map_err(StepRefusal::Lock)?;
    self.grown(properties, fresh)
  }

  /// [`Repository::apply`]'s first part: the plan's revision still the next, its lock entries and the room checked
  /// again, its rows appended to the index and its lock entries applied; its number, its rows, and what the records hold
  /// with it before its texts' deltas save.
  fn apply_rows(
    &mut self,
    planned: Planned,
    properties: &Properties,
    fresh: &BTreeMap<ContentIdentity, Vec<u8>>,
    lock_entries: &[LockEntry],
  ) -> Result<(Revision, Vec<ChangedPath>, u64), StepRefusal> {
    let held = self.apply_checks(planned.revision(), (properties, fresh), lock_entries)?;
    // Checked just above against these very records, so neither refuses; a refusal would still be answered, typed.
    let revision = self.index.commit_planned(planned)?;
    self
      .locks
      .apply_all(lock_entries)
      .map_err(StepRefusal::Lock)?;
    let changes = self
      .index
      .changes(revision)
      .map(<[ChangedPath]>::to_vec)
      .unwrap_or_default();
    Ok((revision, changes, held))
  }

  /// [`Repository::apply`]'s last part, once its property lists are kept: its stored texts named, its properties
  /// kept, and what the records hold.
  fn finish_revision(
    &mut self,
    (stored, properties): (BTreeMap<ContentIdentity, u64>, Properties),
    held: u64,
  ) {
    for (identity, len) in stored {
      self.stored.entry(identity).or_insert(len);
    }
    self.revision_properties.push(properties);
    self.held = held;
  }

  /// A recorded revision's replay begun ([`RevisionReplay`]): checked as [`Repository::commit_record`] checks it.
  ///
  /// # Errors
  /// Every refusal of [`Repository::check`]'s.
  pub fn replay_revision(&self, revision: RevisionToCommit) -> Result<RevisionReplay, StepRefusal> {
    Ok(RevisionReplay {
      stage: ReplayStage::Checked(Box::new(self.check(revision)?)),
    })
  }

  /// One step of `replay` ([`RevisionReplay`]): its rows applied at its first step, its property lists, properties and
  /// stored texts kept at its second; the revision once it is whole, none while a step remains. Until then the records
  /// are mid-revision, which a recovery lends to no reader.
  ///
  /// # Errors
  /// Every refusal of [`Repository::apply`]'s; [`StepRefusal::Corrupt`] for a replay already ended.
  pub fn replay_step(
    &mut self,
    replay: &mut RevisionReplay,
  ) -> Result<Option<Revision>, StepRefusal> {
    match std::mem::replace(&mut replay.stage, ReplayStage::Ended) {
      ReplayStage::Checked(checked) => {
        let Checked {
          planned,
          properties,
          fresh,
          stored,
          lock_entries,
        } = *checked;
        let (revision, changes, held) =
          self.apply_rows(planned, &properties, &fresh, &lock_entries)?;
        replay.stage = ReplayStage::Applied(Box::new(Applied {
          revision,
          changes,
          fresh,
          stored,
          properties,
          held,
        }));
        Ok(None)
      }
      ReplayStage::Applied(applied) => {
        let Applied {
          revision,
          changes,
          fresh,
          stored,
          properties,
          held,
        } = *applied;
        self.keep_lists(&changes, fresh);
        self.finish_revision((stored, properties), held);
        Ok(Some(revision))
      }
      ReplayStage::Ended => Err(StepRefusal::Corrupt),
    }
  }

  /// The commit step's check of a client's accepted work at the moment its owner commits it (A-655 part 2b), as FSFS
  /// judges a transaction when it commits: a work whose head is no longer the youngest revision is merged again at the
  /// youngest, A-592's verdict over the rows of the revisions since its head (`tree.c` `svn_fs_fs__commit_txn`
  /// 2255-2310: FSFS merges the youngest into a transaction whose base moved, so only a conflict refuses it, and
  /// `commit_body` refuses only a base that moved while that merge ran, `transaction.c` 3784-3786); and every work's
  /// locks are verified against the lock table as it stands at the step, for the principal and the tokens `locks`
  /// names at the owner's instant `now` (`commit_body` 3797, `verify_locks`), its lock entries the step's own; then
  /// [`Repository::check`]. The rows are the work's: the merge takes from the youngest every node the work left alone,
  /// as the index's apply of the rows against the youngest keeps them. A work whose verdict ran at the youngest
  /// revision, with the table as it stands, is answered as that verdict answered it.
  ///
  /// # Errors
  /// [`StepRefusal::Verdict`] for the conflict the merge finds or the lock in the way, in the drive's words; each
  /// refusal of [`Repository::check`]'s.
  pub fn check_work(
    &self,
    revision: RevisionToCommit,
    locks: &CommitLocks,
    now: AprTime,
  ) -> Result<Checked, StepRefusal> {
    self.check(self.judge_again(revision, locks, now)?)
  }

  /// A client's accepted work as the step commits it ([`Repository::check_work`]'s judgment, before its check): merged
  /// again at the youngest revision where its head moved, its locks verified as the table stands, its lock entries the
  /// step's, its head the youngest it was judged at; its rows, properties and contents its own. What the owner records
  /// is this revision (A-655 part 2b), so a replay checks it as the live step did.
  ///
  /// # Errors
  /// [`StepRefusal::Verdict`] for the conflict the merge finds or the lock in the way, in the drive's words.
  pub fn judge_again(
    &self,
    mut revision: RevisionToCommit,
    locks: &CommitLocks,
    now: AprTime,
  ) -> Result<RevisionToCommit, StepRefusal> {
    let youngest = self.youngest();
    let refused = |refusal| StepRefusal::Verdict {
      refusal,
      user: locks.user.clone(),
    };
    let verify = |changes: &[ChangedPath]| {
      let changed = changes
        .iter()
        .map(|change| (change.path.as_str(), change.action));
      self.locks.verdict(changed, locks, now)
    };
    let lock_entries = if revision.head < youngest {
      let entries =
        verdict(&self.index, revision.head, &revision.record, verify).map_err(refused)?;
      revision.head = youngest;
      entries
    } else {
      verify(&revision.record.changes).map_err(|refusal| refused(CommitRefusal::Locked(refusal)))?
    };
    revision.lock_entries = lock_entries;
    Ok(revision)
  }

  /// The commit step whole: [`Repository::check`], then [`Repository::apply`].
  ///
  /// # Errors
  /// Each refusal of [`Repository::check`]'s; refused, nothing changes.
  pub fn commit(&mut self, revision: RevisionToCommit) -> Result<Revision, StepRefusal> {
    let checked = self.check(revision)?;
    self.apply(checked)
  }

  /// Applies the entries a lock verb's guard decided (A-602), all of them or none.
  ///
  /// # Errors
  /// The lock table's refusal of an entry it does not take; refused, nothing changes.
  pub fn apply_lock_entries(&mut self, entries: &[LockEntry]) -> Result<(), StepRefusal> {
    self.locks.apply_all(entries).map_err(StepRefusal::Lock)
  }

  /// The bytes the records would hold with `properties` and `fresh` added, refused past the room.
  fn grown(
    &self,
    properties: &Properties,
    fresh: &BTreeMap<ContentIdentity, Vec<u8>>,
  ) -> Result<u64, StepRefusal> {
    let added = fresh
      .values()
      .try_fold(
        properties_bytes(properties).map_err(|_| StepRefusal::Corrupt)?,
        |total, bytes| total.checked_add(u64::try_from(bytes.len()).ok()?),
      )
      .ok_or(StepRefusal::Corrupt)?;
    let held = self.held.checked_add(added).ok_or(StepRefusal::Corrupt)?;
    if held > self.room.bytes {
      return Err(StepRefusal::Room {
        needed: held,
        room: self.room.bytes,
      });
    }
    Ok(held)
  }
}

/// A recorded revision's replay a step at a time ([`Repository::replay_revision`], [`Repository::replay_step`]): where it
/// stands. Its rows are applied at one step and the rest kept at the next, so a recovery yields between them.
#[derive(Debug)]
pub struct RevisionReplay {
  /// Where it stands.
  stage: ReplayStage,
}

/// Where a [`RevisionReplay`] stands.
#[derive(Debug)]
enum ReplayStage {
  /// Checked; its rows not yet applied.
  Checked(Box<Checked>),
  /// Its rows applied; its property lists, properties and stored texts not yet kept.
  Applied(Box<Applied>),
  /// Ended.
  Ended,
}

/// A replayed revision whose rows are applied ([`ReplayStage::Applied`]).
#[derive(Debug)]
struct Applied {
  /// The revision.
  revision: Revision,
  /// Its rows.
  changes: Vec<ChangedPath>,
  /// The property lists it brought.
  fresh: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The texts its commit stored.
  stored: BTreeMap<ContentIdentity, u64>,
  /// Its revision properties.
  properties: Properties,
  /// What the records hold with it.
  held: u64,
}

/// A revision being checked against a repository a slice at a time ([`Repository::check_begin`],
/// [`Repository::check_step`]; A-920 piece 2): every check made but the index's plan of its rows, part made.
#[derive(Debug)]
pub struct Checking {
  /// Where it stands.
  phase: CheckPhase,
  /// Its record, until the index's plan of it begins.
  record: Option<RevisionRecord>,
  /// The index's plan of its rows, once begun.
  planning: Option<Planning>,
  /// The texts (true) and property lists (false) its rows name, not yet taken.
  named: BTreeSet<(ContentIdentity, bool)>,
  /// The texts and property lists it brought, not yet taken.
  brought: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The texts its commit stored, not yet filtered.
  stored_in: BTreeMap<ContentIdentity, u64>,
  /// Its revision properties.
  properties: Properties,
  /// The texts and property lists its rows name that the repository does not hold.
  fresh: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The texts its rows name that its commit stored in the content store and the repository does not hold yet.
  stored: BTreeMap<ContentIdentity, u64>,
  /// Its lock entries.
  lock_entries: Vec<LockEntry>,
}

/// A checked revision being applied to the records a slice at a time ([`Repository::apply_begin`],
/// [`Repository::apply_step`]; A-920 piece 5b).
#[derive(Debug)]
pub struct Applying {
  /// Its rows' commit into the index.
  committing: crate::index::Committing,
  /// The texts its commit stored, not yet named.
  stored: BTreeMap<ContentIdentity, u64>,
  /// Its revision properties, until its last step.
  properties: Option<Properties>,
  /// Its lock entries.
  lock_entries: Vec<LockEntry>,
  /// What the records hold once it is applied.
  held: u64,
}

/// Where a [`Checking`] stands.
#[derive(Clone, Copy, Debug)]
enum CheckPhase {
  /// Naming the texts and property lists of its rows, from this row.
  Naming(usize),
  /// Taking the names the records do not hold.
  Fresh,
  /// Filtering the texts its commit stored.
  Stored,
  /// The index's plan to begin, the first unit of a step.
  PlanBegin,
  /// The index planning its rows.
  Planning,
}

impl Checking {
  /// The texts and property lists the rows from `at` name, a row a unit until `spent`: whether the phase ended.
  fn name_some(&mut self, mut at: usize, spent: &mut dyn FnMut() -> bool) -> bool {
    let changes = self
      .record
      .as_ref()
      .map_or(&[][..], |record| &record.changes);
    while let Some(change) = changes.get(at) {
      if let Some(text) = change.text {
        self.named.insert((text.identity, true));
      }
      if let Some(PropertyList::Held { identity, .. }) = change.properties {
        self.named.insert((identity, false));
      }
      at = at.saturating_add(1);
      if at < changes.len() && spent() {
        self.phase = CheckPhase::Naming(at);
        return false;
      }
    }
    self.phase = CheckPhase::Fresh;
    true
  }

  /// `identity`, named as a text (`as_text`) or a property list and held by neither the records nor this check yet:
  /// a text its commit stored passed, one it carries refused (module docs, "Texts": its caller stores it first,
  /// `Repository::take_texts`); a property list taken from what the revision brought, refused where it brought none.
  fn take_fresh(&mut self, identity: ContentIdentity, as_text: bool) -> Result<(), StepRefusal> {
    if as_text {
      return if self.stored_in.contains_key(&identity) {
        Ok(())
      } else {
        Err(StepRefusal::Corrupt)
      };
    }
    let bytes = self.brought.remove(&identity).ok_or(StepRefusal::Corrupt)?;
    self.fresh.insert(identity, bytes);
    Ok(())
  }
}

/// Whether a whole check's or apply's step ([`Repository::check`], [`Repository::apply`]) is spent: never, but in the
/// tests that take every one a unit a step ([`CHECK_A_UNIT_A_STEP`]).
fn whole_check_spent() -> bool {
  #[cfg(test)]
  {
    CHECK_A_UNIT_A_STEP.with(std::cell::Cell::get)
  }
  #[cfg(not(test))]
  {
    false
  }
}

#[cfg(test)]
std::thread_local! {
  /// Test support: every [`Repository::check`] and [`Repository::apply`] taken a unit a step, as a staging step whose
  /// slice is always spent takes it (A-920 pieces 2b and 5b's oracle).
  pub(crate) static CHECK_A_UNIT_A_STEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What a step of [`Repository::check_step`] left.
#[derive(Debug)]
pub enum CheckStep {
  /// The plan goes on at the next step.
  More(Box<Checking>),
  /// Checked: what [`Repository::apply`] commits.
  Checked(Box<Checked>),
}

/// A revision the commit step checked against a repository ([`Repository::check`]): what [`Repository::apply`]
/// commits, so long as the repository did not change since.
#[derive(Debug)]
pub struct Checked {
  /// The index's plan of its rows.
  planned: Planned,
  /// Its revision properties.
  properties: Properties,
  /// The texts and property lists its rows name that the repository does not hold.
  fresh: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The texts its rows name that its commit stored in the content store and the repository does not hold yet.
  stored: BTreeMap<ContentIdentity, u64>,
  /// Its lock entries.
  lock_entries: Vec<LockEntry>,
}

impl Checked {
  /// The revision it commits as.
  pub fn revision(&self) -> Revision {
    self.planned.revision()
  }

  /// Its changed-path rows, sorted by path.
  pub fn changes(&self) -> &[ChangedPath] {
    self.planned.changes()
  }

  /// Its revision properties.
  pub fn properties(&self) -> &Properties {
    &self.properties
  }

  /// The texts and property lists its rows name that the repository does not hold, by content identity.
  pub fn fresh(&self) -> &BTreeMap<ContentIdentity, Vec<u8>> {
    &self.fresh
  }

  /// Its lock entries.
  pub fn lock_entries(&self) -> &[LockEntry] {
    &self.lock_entries
  }
}

/// Where the node at a path a revision changes stood before the revision changed it, as the revision's own copies leave
/// its tree (A-655 part 2b): what its owner's green holds there for the revision to change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Before {
  /// At the path itself, in the revision's head.
  Head,
  /// Beneath a copy the revision made: at `path`, beneath the copy's source, in the source's `revision`.
  Copied {
    /// The path beneath the copy's source.
    path: String,
    /// The copy's source revision.
    revision: Revision,
  },
  /// Nowhere: beneath a node the revision deleted, or added without history.
  Absent,
}

/// Where the node at `path` stood before the revision whose rows are `changes`, sorted by path and each path once (a
/// checked revision's, `Checked::changes`), changed it ([`Before`]), each ancestor's row found by a search of them, as
/// a dump's transaction reads its own tree (`Transaction::lookup`): beneath the deepest ancestor the rows add or replace, nowhere without a
/// copy source and beneath the source with one; nowhere beneath a deleted ancestor; else at the path itself.
pub fn before(changes: &[ChangedPath], path: &str) -> Before {
  for (ancestor, rest) in ancestors_deepest_first(path) {
    let found = changes.binary_search_by(|row| row.path.as_str().cmp(ancestor));
    let Some(row) = found.ok().and_then(|at| changes.get(at)) else {
      continue;
    };
    match (row.action, &row.copy_from) {
      (Action::Modify, _) => {}
      (Action::Delete, _) | (Action::Add | Action::Replace, None) => return Before::Absent,
      (Action::Add | Action::Replace, Some(source)) => {
        return Before::Copied {
          path: beneath(&source.path, rest),
          revision: source.revision,
        };
      }
    }
  }
  Before::Head
}

/// The records as the read commands, the report drive and the DAV site read a repository (A-681; the svn:// listener's
/// connection too, A-689): a directory's entries from the index ([`Index::children`]); a property list's `svn:mergeinfo`
/// from the list held under its identity.
impl Contents for Repository {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    self.index.children(directory, revision).ok().flatten()
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    self
      .content(properties)
      .and_then(entries)?
      .into_iter()
      .find(|(name, _)| *name == MERGEINFO)
      .map(|(_, value)| value)
  }
}

/// The records as a read source (A-681): a revision's properties as held; a property list decoded in place from the
/// bytes held under its identity, FSFS's form; a text from an offset to its end, held in one piece.
impl ReadSource for Repository {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    let properties = Repository::revision_properties(self, revision)?;
    Some(
      properties
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect(),
    )
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    self.content(identity).and_then(entries)
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<Cow<'_, [u8]>> {
    TextSource::text_at(self, identity, offset)
  }

  fn text_in<'a>(
    &'a self,
    identity: &ContentIdentity,
    offset: u64,
    cache: &'a mut crate::reads::WindowCache,
  ) -> Option<Cow<'a, [u8]>> {
    Repository::text_in(self, identity, offset, cache)
  }
}

/// Where a repository's texts are read (S4, with A-716: a text held once, by the green's content-identity index, the
/// records naming it by identity): the reads, the report drive, the commit drive, the export and the import read a
/// text through a source their caller lends them, never assuming the records hold its bytes.
pub trait TextSource {
  /// The text held under `identity`, whole: borrowed where the source holds it in one piece, owned where it must copy
  /// it; none where the source holds no such text.
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>>;

  /// The text held under `identity` from `offset`: a run of at least one byte where any is left (a read's next piece,
  /// as [`crate::reads::Repository::text`] answers), empty at its end, none past it or where the source holds no such
  /// text. A source that keeps texts in pieces answers the piece holding `offset` and makes nothing whole.
  fn text_at(&self, identity: &ContentIdentity, offset: u64) -> Option<Cow<'_, [u8]>> {
    from_offset(self.text(identity)?, offset)
  }

  /// [`TextSource::text_at`] for a reader keeping `cache` across its steps (`crate::reads::Repository::text_in`).
  fn text_in<'a>(
    &'a self,
    identity: &ContentIdentity,
    offset: u64,
    _cache: &'a mut crate::reads::WindowCache,
  ) -> Option<Cow<'a, [u8]>> {
    self.text_at(identity, offset)
  }

  /// Reads `identity`'s text into `reading` a bounded step (§4.17.1 "A fetch's answer", rule 3: the export's text
  /// rebuild sliced, main's item 2 of 2026-11-20): at most one window's room of bytes copied, or one svndiff window of
  /// a delta chain applied. True once `reading` holds the text whole ([`TextReading::bytes`]); none where the source
  /// holds no such text or its chain does not hold together.
  fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool>;

  /// Whether a read of the text `identity` from `offset` that this source answered none for is a piece it reads
  /// asynchronously from a store it does not hold in memory (C2 stage B part 2; `crate::reads::Repository::fetchable`):
  /// none of a source that holds every text it names in memory, the default.
  fn fetchable(&self, _identity: &ContentIdentity, _offset: u64) -> bool {
    false
  }
}

/// The records as a text source: the text bytes they hold themselves, a property list's read as a text (module docs,
/// "Texts"); every text named stored is the caller's source's ([`WithTexts`]).
impl TextSource for Repository {
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    Repository::text(self, identity)
  }

  fn text_at(&self, identity: &ContentIdentity, offset: u64) -> Option<Cow<'_, [u8]>> {
    Repository::text_at(self, identity, offset).map(Cow::Owned)
  }

  fn text_in<'a>(
    &'a self,
    identity: &ContentIdentity,
    offset: u64,
    cache: &'a mut crate::reads::WindowCache,
  ) -> Option<Cow<'a, [u8]>> {
    Repository::text_in(self, identity, offset, cache)
  }

  fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool> {
    Repository::read_step(self, identity, reading)
  }
}

/// A text being read a bounded step at a time ([`TextSource::read_step`]): the bytes read so far.
#[derive(Clone, Debug, Default)]
pub struct TextReading {
  /// The text's bytes read so far; the whole text once a step answered true.
  bytes: Vec<u8>,
}

impl TextReading {
  /// A reading that has read nothing.
  pub fn new() -> Self {
    Self::default()
  }

  /// The bytes read so far: the text whole once a step answered true.
  pub fn bytes(&self) -> &[u8] {
    &self.bytes
  }

  /// The text read, the reading ended.
  pub fn into_bytes(self) -> Vec<u8> {
    self.bytes
  }

  /// How many bytes are read so far: where the next piece of a text held in one piece starts.
  pub fn read(&self) -> u64 {
    u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
  }

  /// Appends `piece`, the next of a text read in pieces (a content store's window), true once `len` bytes are read.
  pub fn append(&mut self, piece: &[u8], len: u64) -> bool {
    self.bytes.extend_from_slice(piece);
    self.read() >= len
  }

  /// Copies the next piece of `whole`, a text held in one piece, at most a window's room ([`svndiff::WINDOW_LIMIT`]);
  /// true once all of it is copied.
  pub fn copy_step(&mut self, whole: &[u8]) -> bool {
    let room = usize::try_from(svndiff::WINDOW_LIMIT).unwrap_or(usize::MAX);
    let rest = whole.get(self.bytes.len()..).unwrap_or_default();
    self
      .bytes
      .extend_from_slice(rest.get(..room).unwrap_or(rest));
    self.bytes.len() >= whole.len()
  }
}

impl Repository {
  /// One step of reading the text held under `identity` into `reading` ([`TextSource::read_step`]): a property list's
  /// bytes, the only text bytes the records hold, copied a window's room a step.
  pub fn read_step(&self, identity: &ContentIdentity, reading: &mut TextReading) -> Option<bool> {
    self
      .contents
      .get(identity)
      .map(|bytes| reading.copy_step(bytes))
  }
}

/// `text` from `offset` to its end: none past it, as a read's source answers.
pub(super) fn from_offset(text: Cow<'_, [u8]>, offset: u64) -> Option<Cow<'_, [u8]>> {
  let offset = usize::try_from(offset).ok()?;
  match text {
    Cow::Borrowed(bytes) => bytes.get(offset..).map(Cow::Borrowed),
    Cow::Owned(bytes) => bytes.get(offset..).map(|rest| Cow::Owned(rest.to_vec())),
  }
}

/// The records with their texts read from `texts` (S4): what a lender hands the reads, the report drive and the commit
/// drive once the records hold no text bytes of their own. Every property list, revision property, entry and lock is
/// the records'; every text is the source's.
#[derive(Clone, Copy)]
pub struct WithTexts<'a, T: ?Sized> {
  /// The records.
  pub records: &'a Repository,
  /// Where their texts are read.
  pub texts: &'a T,
}

impl<T: TextSource + ?Sized> Contents for WithTexts<'_, T> {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    self.records.entries(directory, revision)
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    self.records.mergeinfo_value(properties)
  }
}

impl<T: TextSource + ?Sized> ReadSource for WithTexts<'_, T> {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    ReadSource::revision_properties(self.records, revision)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    ReadSource::properties(self.records, identity)
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<Cow<'_, [u8]>> {
    self.texts.text_at(identity, offset)
  }

  fn text_in<'a>(
    &'a self,
    identity: &ContentIdentity,
    offset: u64,
    cache: &'a mut crate::reads::WindowCache,
  ) -> Option<Cow<'a, [u8]>> {
    self.texts.text_in(identity, offset, cache)
  }

  fn fetchable(&self, identity: &ContentIdentity, offset: u64) -> bool {
    self.texts.fetchable(identity, offset)
  }
}

impl<T: TextSource + ?Sized> Green for WithTexts<'_, T> {
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    self.texts.text(identity)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    Green::properties(self.records, identity)
  }

  fn name_text(&self, text: &[u8]) -> ContentIdentity {
    self.records.name_text(text)
  }

  fn name_properties(&self, properties: &[(Vec<u8>, Vec<u8>)]) -> ContentIdentity {
    self.records.name_properties(properties)
  }
}

impl Green for Repository {
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    TextSource::text(self, identity)
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    self.content(identity).and_then(entries)
  }

  fn name_text(&self, text: &[u8]) -> ContentIdentity {
    *blake3::hash(text).as_bytes()
  }

  fn name_properties(&self, properties: &[(Vec<u8>, Vec<u8>)]) -> ContentIdentity {
    let list: Properties = properties.iter().cloned().collect();
    *blake3::hash(&encode(&list)).as_bytes()
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
  /// The property lists it made that the repository does not hold, by content identity.
  pending: BTreeMap<ContentIdentity, Vec<u8>>,
  /// Their bytes.
  pending_bytes: u64,
  /// The texts it set, each with its length: named stored, their bytes in the import's sink, never here (the import's
  /// module docs, "Texts").
  stored: BTreeMap<ContentIdentity, u64>,
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
    let change = self
      .changes
      .get_mut(path)
      .ok_or(MigrationRefusal::Corrupt)?;
    let now = change.now.as_mut().ok_or(MigrationRefusal::Corrupt)?;
    now.properties = list;
    change.properties_set = true;
    Ok(())
  }

  /// Sets the text of the file at `path` to `text`, whose bytes the import's sink holds: named stored by its length
  /// where the repository does not hold it already.
  pub(super) fn set_text(
    &mut self,
    repository: &Repository,
    path: &str,
    text: Text,
  ) -> Result<(), MigrationRefusal> {
    let change = self
      .changes
      .get_mut(path)
      .ok_or(MigrationRefusal::Corrupt)?;
    let now = change.now.as_mut().ok_or(MigrationRefusal::Corrupt)?;
    now.text = Some(text);
    change.text_set = true;
    if !repository.holds(&text.identity) {
      self.stored.insert(text.identity, text.size);
    }
    Ok(())
  }

  /// The bytes held under `identity`, by the transaction or the repository: a property list in place, a text the
  /// repository keeps made from its delta.
  pub(super) fn content<'a>(
    &'a self,
    repository: &'a Repository,
    identity: &ContentIdentity,
  ) -> Option<Cow<'a, [u8]>> {
    if let Some(bytes) = self.pending.get(identity) {
      return Some(Cow::Borrowed(bytes));
    }
    if let Some(bytes) = repository.content(identity) {
      return Some(Cow::Borrowed(bytes));
    }
    repository.text(identity)
  }

  /// Whether the transaction or the repository holds `identity`.
  fn holds(&self, repository: &Repository, identity: &ContentIdentity) -> bool {
    self.pending.contains_key(identity) || repository.holds(identity)
  }

  /// Holds a property list's `bytes` under their content identity, once; refused when the repository's room cannot take
  /// them. A text's bytes are never held here ([`Transaction::set_text`]).
  fn store(
    &mut self,
    repository: &Repository,
    bytes: Vec<u8>,
  ) -> Result<ContentIdentity, MigrationRefusal> {
    let identity = *blake3::hash(&bytes).as_bytes();
    if self.holds(repository, &identity) {
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
      .and_then(|bytes| decode(&bytes))
      .ok_or(MigrationRefusal::Corrupt)?;
    Ok(mergeinfo(&properties).map(<[u8]>::to_vec))
  }
}

/// A revision being closed a unit at a time ([`Repository::close_begin`], [`Repository::close_step`]; A-920 7b).
#[derive(Clone, Debug)]
pub(super) struct Closing {
  /// The transaction closed, its changes taken into [`Closing::changes`].
  transaction: Transaction,
  /// The revision's properties, the stream's.
  properties: Properties,
  /// What the transaction did at each path not yet made a row, taken a path a unit.
  changes: BTreeMap<String, Change>,
  /// Where the close is.
  phase: ClosePhase,
  /// The changed paths FSFS records for the revision, in path order: a modification that set nothing records nothing,
  /// as a dump's change record without a section makes no call.
  rows: Vec<ChangedPath>,
  /// The contents the rows name, each with whether as a text, not yet looked for.
  named: BTreeSet<(ContentIdentity, bool)>,
  /// The contents the rows name that the repository does not hold, taken out of the transaction.
  fresh: BTreeMap<ContentIdentity, Vec<u8>>,
  /// The texts the transaction set that the rows name and the repository does not hold, each with its length.
  stored: BTreeMap<ContentIdentity, u64>,
}

/// Where a [`Closing`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosePhase {
  /// Making each path's row.
  Rows,
  /// Naming the contents of the rows from this one on.
  Naming(usize),
  /// Taking out of the transaction each named content the repository does not hold.
  Fresh,
  /// Naming stored each text the transaction set that the rows from this one on name.
  Stored(usize),
  /// Closed.
  Closed,
}

/// What a step of [`Repository::close_step`] left.
#[derive(Debug)]
pub(super) enum CloseStep {
  /// The close goes on at the next step.
  More(Box<Closing>),
  /// Closed: the revision for the caller to commit.
  Closed(Box<RevisionToCommit>),
}

impl Closing {
  /// The rows, a path a unit until `spent`: whether every path is a row.
  fn rows_some(
    &mut self,
    repository: &Repository,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<bool, MigrationRefusal> {
    while let Some((path, change)) = self.changes.pop_first() {
      if let Some(row) = self.transaction.row(repository, &path, &change)? {
        self.rows.push(row);
      }
      if spent() {
        return Ok(false);
      }
    }
    self.phase = ClosePhase::Naming(0);
    Ok(true)
  }

  /// The contents the rows from `at` name, a row a unit until `spent`: whether every row's are named.
  fn name_some(&mut self, mut at: usize, spent: &mut dyn FnMut() -> bool) -> bool {
    while let Some(row) = self.rows.get(at) {
      if let Some(text) = row.text {
        self.named.insert((text.identity, true));
      }
      if let Some(PropertyList::Held { identity, .. }) = row.properties {
        self.named.insert((identity, false));
      }
      at = at.saturating_add(1);
      if spent() {
        self.phase = ClosePhase::Naming(at);
        return false;
      }
    }
    self.phase = ClosePhase::Fresh;
    true
  }

  /// Each named content the repository does not hold taken out of the transaction, a name a unit until `spent`; a text
  /// the transaction set is named stored instead ([`Closing::stored_some`]), its bytes the sink's. Whether every name
  /// is looked for.
  fn fresh_some(
    &mut self,
    repository: &Repository,
    spent: &mut dyn FnMut() -> bool,
  ) -> Result<bool, MigrationRefusal> {
    while let Some((identity, as_text)) = self.named.pop_first() {
      let stored = as_text && self.transaction.stored.contains_key(&identity);
      if !stored && !repository.holds_as(&identity, as_text) && !self.fresh.contains_key(&identity)
      {
        let bytes = match self.transaction.pending.remove(&identity) {
          Some(bytes) => bytes,
          None => repository
            .text(&identity)
            .ok_or(MigrationRefusal::Corrupt)?
            .into_owned(),
        };
        self.fresh.insert(identity, bytes);
      }
      if spent() {
        return Ok(false);
      }
    }
    self.phase = ClosePhase::Stored(0);
    Ok(true)
  }

  /// Each text the transaction set that the rows from `at` name and the repository does not hold, named stored with
  /// its length, a row a unit until `spent`: whether every row's is.
  fn stored_some(
    &mut self,
    repository: &Repository,
    mut at: usize,
    spent: &mut dyn FnMut() -> bool,
  ) -> bool {
    while let Some(row) = self.rows.get(at) {
      if let Some(text) = row.text
        && !repository.holds(&text.identity)
        && let Some(len) = self.transaction.stored.get(&text.identity)
      {
        self.stored.insert(text.identity, *len);
      }
      at = at.saturating_add(1);
      if spent() {
        self.phase = ClosePhase::Stored(at);
        return false;
      }
    }
    self.phase = ClosePhase::Closed;
    true
  }

  /// The revision closed: made on the transaction's base, with no lock entries.
  fn closed(self) -> RevisionToCommit {
    RevisionToCommit {
      head: self.transaction.base,
      record: RevisionRecord {
        transaction: self.transaction.name,
        changes: self.rows,
      },
      properties: self.properties,
      contents: self.fresh,
      // An import's texts are its sink's, named stored here by their lengths (the import's module docs, "Texts").
      stored: self.stored,
      lock_entries: Vec::new(),
    }
  }
}

/// Whether a close's step ([`Repository::close_step`]) is spent beyond its caller's say: never, but in the tests that
/// take every one a unit a step ([`CLOSE_A_UNIT_A_STEP`]).
fn close_a_unit_a_step() -> bool {
  #[cfg(test)]
  {
    CLOSE_A_UNIT_A_STEP.with(std::cell::Cell::get)
  }
  #[cfg(not(test))]
  {
    false
  }
}

#[cfg(test)]
std::thread_local! {
  /// Test support: every [`Repository::close_step`] taken a unit a step, as an import call whose slice is always spent
  /// takes it (A-920 7b's oracle).
  pub(crate) static CLOSE_A_UNIT_A_STEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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

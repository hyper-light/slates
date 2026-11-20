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
//! **Texts** (S4 slice 2 c2, A-727 (c); rule (B): the repository's history compressed, never its texts whole). Each
//! text is kept once, when the revision that brings it is applied, as an svndiff of version 2 (LZ4 sections, FSFS
//! 1.10's form) against the text of its node's skip-delta predecessor (FSFS's `choose_delta_base`: the predecessor
//! whose count is the node's with its lowest set bit cleared), a node's first version against nothing. A read makes the
//! text from its chain of bases, at most `log2` of the node's predecessors deltas applied ([`Repository::text`]); nothing
//! is deltified again later. Property lists are kept whole and read in place ([`Repository::content`]).
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
use crate::dump::{Md5, Sha1, Uuid};
use crate::index::{
  Action, ChangedPath, ContentIdentity, CopySource, Index, NodeKind, Planned, PropertyList,
  Revision, RevisionRecord, Text,
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
  /// Every text a node-revision names, by content identity, as a delta against its skip-delta predecessor's text
  /// (S4 slice 2 c2, module docs "Texts").
  texts: BTreeMap<ContentIdentity, KeptText>,
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
      texts: BTreeMap::new(),
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
    self.contents.contains_key(identity) || self.texts.contains_key(identity)
  }

  /// Whether the records hold `identity` as the kind a row names it as: a text (kept as a delta, or bytes a property
  /// list holds too) or a property list (whole, read in place). The same bytes may be both.
  fn holds_as(&self, identity: &ContentIdentity, as_text: bool) -> bool {
    if as_text {
      self.holds(identity)
    } else {
      self.contents.contains_key(identity)
    }
  }

  /// The text held under `identity`, made from its delta and its base's, the base's from its own, down its chain of
  /// skip-delta predecessors (module docs, "Texts": at most `log2` of its node's predecessors deltas applied); none
  /// when the records hold no such text, or its chain does not hold together.
  pub fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    if !self.texts.contains_key(identity) {
      // A text whose bytes are a property list's too is that list's bytes.
      return self
        .contents
        .get(identity)
        .map(|bytes| Cow::Borrowed(bytes.as_slice()));
    }
    let mut chain = Vec::new();
    let mut at = *identity;
    loop {
      let kept = self.texts.get(&at)?;
      chain.push(kept);
      match kept.base {
        Some(base) if chain.len() <= MAX_CHAIN => at = base,
        Some(_) => return None,
        None => break,
      }
    }
    let mut bytes = Vec::new();
    for kept in chain.iter().rev() {
      let (_, windows) = svndiff::decode_all(&kept.delta).ok()?;
      bytes = svndiff::apply_all(&bytes, &windows).ok()?;
    }
    Some(Cow::Owned(bytes))
  }

  /// How many deltas reading the text held under `identity` applies ([`Repository::text`]); none when not held.
  pub fn text_chain(&self, identity: &ContentIdentity) -> Option<usize> {
    let mut at = *identity;
    let mut applied = 0usize;
    loop {
      let kept = self.texts.get(&at)?;
      applied = applied.saturating_add(1);
      match kept.base {
        Some(base) if applied <= MAX_CHAIN => at = base,
        Some(_) => return None,
        None => return Some(applied),
      }
    }
  }

  /// The bytes the texts' deltas hold.
  pub fn text_bytes(&self) -> u64 {
    self
      .texts
      .values()
      .map(|kept| u64::try_from(kept.delta.len()).unwrap_or(u64::MAX))
      .fold(0, u64::saturating_add)
  }

  /// Keeps the texts `fresh` brings that the rows of `revision` name (module docs, "Texts"): each as a delta against
  /// the text of its node's skip-delta predecessor, a first version against nothing; property lists whole. What the
  /// texts' deltas take less than their bytes did.
  fn keep_fresh(
    &mut self,
    revision: Revision,
    changes: &[ChangedPath],
    fresh: BTreeMap<ContentIdentity, Vec<u8>>,
  ) -> Result<u64, StepRefusal> {
    let mut saved = 0u64;
    for change in changes {
      let Some(text) = change.text else {
        continue;
      };
      if self.texts.contains_key(&text.identity) {
        continue;
      }
      let Some(bytes) = fresh.get(&text.identity) else {
        continue;
      };
      let bytes = bytes.clone();
      let base = self.skip_base(&change.path, revision, &text.identity);
      let base_bytes = match base {
        Some(base) => self.text(&base).ok_or(StepRefusal::Corrupt)?.into_owned(),
        None => Vec::new(),
      };
      let delta = encode_delta(&base_bytes, &bytes).ok_or(StepRefusal::Corrupt)?;
      saved = saved.saturating_add(
        u64::try_from(bytes.len())
          .unwrap_or(u64::MAX)
          .saturating_sub(u64::try_from(delta.len()).unwrap_or(u64::MAX)),
      );
      self.texts.insert(text.identity, KeptText { base, delta });
    }
    // The property lists the rows name, kept whole; a text's bytes are in its delta alone.
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
    Ok(saved)
  }

  /// The text of the skip-delta predecessor of the node at `path` in `revision` (FSFS's rule, `fs_fs.c`
  /// `choose_delta_base`: the predecessor whose count is the node's with its lowest set bit cleared); none for a node with
  /// no predecessor, one whose base holds no text the records keep, or the text itself.
  fn skip_base(
    &self,
    path: &str,
    revision: Revision,
    identity: &ContentIdentity,
  ) -> Option<ContentIdentity> {
    let node = self.index.node(path, revision).ok().flatten()?;
    let count = node.predecessor_count();
    let target = count & count.saturating_sub(1);
    let mut at = node;
    for _ in 0..count.saturating_sub(target) {
      at = self.index.node_revision(at.predecessor()?).ok()?;
    }
    let base = at.text()?.identity;
    (base != *identity && self.texts.contains_key(&base)).then_some(base)
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
    })
  }

  /// The revision `transaction` makes with `properties` as its revision properties, exactly the stream's: the changed
  /// paths FSFS records for it and the contents they name that the repository does not hold, made on the youngest
  /// revision, with no lock entries; what the import commits through the step.
  pub(super) fn revision_of(
    &self,
    mut transaction: Transaction,
    properties: Properties,
  ) -> Result<RevisionToCommit, MigrationRefusal> {
    let changes = transaction.rows(self)?;
    let fresh = transaction.take_fresh_contents(self, &changes)?;
    Ok(RevisionToCommit {
      head: transaction.base,
      record: RevisionRecord {
        transaction: transaction.name,
        changes,
      },
      properties,
      contents: fresh.into_iter().collect(),
      lock_entries: Vec::new(),
    })
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
    let fresh = self.fresh(&revision.record.changes, revision.contents)?;
    self.grown(&revision.properties, &fresh)?;
    let planned = self.index.plan(revision.record, self.room.entries)?;
    Ok(Checked {
      planned,
      properties: revision.properties,
      fresh,
      lock_entries: revision.lock_entries,
    })
  }

  /// The commit step's apply: commits what [`Repository::check`] checked as the next revision, its rows appended to
  /// the index, its new contents and its properties kept and its lock entries applied, and answers its number.
  ///
  /// # Errors
  /// [`StepRefusal::OutOfDate`] when a revision was committed since the check, and every refusal of the check's that a
  /// change since could bring (a lock entry, the room); refused, nothing changes.
  pub fn apply(&mut self, checked: Checked) -> Result<Revision, StepRefusal> {
    let Checked {
      planned,
      properties,
      fresh,
      lock_entries,
    } = checked;
    let youngest = self.youngest();
    if planned.revision() != youngest.checked_add(1).ok_or(StepRefusal::Corrupt)? {
      return Err(StepRefusal::OutOfDate {
        head: planned.revision().saturating_sub(1),
        youngest,
      });
    }
    self
      .locks
      .check_entries(&lock_entries)
      .map_err(StepRefusal::Lock)?;
    let held = self.grown(&properties, &fresh)?;
    // Checked just above against these very records, so neither refuses; a refusal would still be answered, typed.
    let revision = self.index.commit_planned(planned)?;
    self
      .locks
      .apply_all(&lock_entries)
      .map_err(StepRefusal::Lock)?;
    let changes = self
      .index
      .changes(revision)
      .map(<[ChangedPath]>::to_vec)
      .unwrap_or_default();
    let saved = self.keep_fresh(revision, &changes, fresh)?;
    self.revision_properties.push(properties);
    self.held = held.saturating_sub(saved);
    Ok(revision)
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

  /// The texts and property lists `changes` name that the records do not hold, taken from `brought`; refused when one
  /// is held by neither.
  fn fresh(
    &self,
    changes: &[ChangedPath],
    mut brought: BTreeMap<ContentIdentity, Vec<u8>>,
  ) -> Result<BTreeMap<ContentIdentity, Vec<u8>>, StepRefusal> {
    let mut fresh = BTreeMap::new();
    for (identity, as_text) in named_as(changes) {
      if self.holds_as(&identity, as_text) || fresh.contains_key(&identity) {
        continue;
      }
      // A property list whose bytes the records hold as a text is those bytes: no client brings what is held.
      let bytes = match brought.remove(&identity) {
        Some(bytes) => bytes,
        None => self
          .text(&identity)
          .ok_or(StepRefusal::Corrupt)?
          .into_owned(),
      };
      fresh.insert(identity, bytes);
    }
    Ok(fresh)
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

/// A text the records keep (module docs, "Texts"): the text it was made against, none for a first version, and the
/// svndiff (version 2, LZ4 sections) that makes it from that text.
#[derive(Clone, Debug)]
struct KeptText {
  /// The skip-delta predecessor's text the delta is against.
  base: Option<ContentIdentity>,
  /// The delta's bytes.
  delta: Vec<u8>,
}

/// The most deltas one read applies: a node's predecessor count is a `u64`, and the skip-delta rule halves the distance
/// to the first version at every step (module docs, "Texts"), so a chain longer than this does not hold together.
const MAX_CHAIN: usize = 65;

/// `target` as an svndiff of version 2 against `source`, each window's sections LZ4 blocks where that is shorter, as
/// FSFS 1.10 and later write a representation; none if the encoder refuses a window it made, which it never does.
fn encode_delta(source: &[u8], target: &[u8]) -> Option<Vec<u8>> {
  let room = std::num::NonZeroUsize::new(usize::try_from(svndiff::WINDOW_LIMIT).ok()?)?;
  let mut encoder = svndiff::Encoder::new(svndiff::Version::Two);
  let mut out = encoder.header().to_vec();
  for window in svndiff::deltas(source, target, room).ok()? {
    encoder
      .window(&window, svndiff::Compression::Lz4, &mut out)
      .ok()?;
  }
  Some(out)
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

/// Where the node at `path` stood before the revision whose rows are `changes`, by path, changed it ([`Before`]), as
/// a dump's transaction reads its own tree (`Transaction::lookup`): beneath the deepest ancestor the rows add or replace, nowhere without a
/// copy source and beneath the source with one; nowhere beneath a deleted ancestor; else at the path itself.
pub fn before(changes: &BTreeMap<&str, &ChangedPath>, path: &str) -> Before {
  for (ancestor, rest) in ancestors_deepest_first(path) {
    let Some(row) = changes.get(ancestor) else {
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

/// The texts and property lists `changes` name, each with whether a row names it as a text, in identity order; the
/// same bytes named as both come twice.
fn named_as(changes: &[ChangedPath]) -> BTreeSet<(ContentIdentity, bool)> {
  let mut named = BTreeSet::new();
  for change in changes {
    if let Some(text) = change.text {
      named.insert((text.identity, true));
    }
    if let Some(PropertyList::Held { identity, .. }) = change.properties {
      named.insert((identity, false));
    }
  }
  named
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
    from_offset(TextSource::text(self, identity)?, offset)
  }
}

/// Where a repository's texts are read (S4, with A-716: a text held once, by the green's content-identity index, the
/// records naming it by identity): the reads, the report drive, the commit drive, the export and the import read a
/// text through a source their caller lends them, never assuming the records hold its bytes.
pub trait TextSource {
  /// The text held under `identity`, whole: borrowed where the source holds it in one piece, owned where it must copy
  /// it; none where the source holds no such text.
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>>;
}

/// The records hold their own texts until the green's content index serves them (S4's second slice, A-716): a text
/// is the bytes held under its identity.
impl TextSource for Repository {
  fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    Repository::text(self, identity)
  }
}

/// `text` from `offset` to its end: none past it, as a read's source answers.
fn from_offset(text: Cow<'_, [u8]>, offset: u64) -> Option<Cow<'_, [u8]>> {
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
    from_offset(self.texts.text(identity)?, offset)
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
    let change = self
      .changes
      .get_mut(path)
      .ok_or(MigrationRefusal::Corrupt)?;
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
    let change = self
      .changes
      .get_mut(path)
      .ok_or(MigrationRefusal::Corrupt)?;
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

  /// Holds `bytes` under their content identity, once; refused when the repository's room cannot take them.
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
      .and_then(|bytes| decode(&bytes))
      .ok_or(MigrationRefusal::Corrupt)?;
    Ok(mergeinfo(&properties).map(<[u8]>::to_vec))
  }

  /// The contents `changes` name that the repository does not hold, taken out of the transaction.
  fn take_fresh_contents(
    &mut self,
    repository: &Repository,
    changes: &[ChangedPath],
  ) -> Result<Vec<(ContentIdentity, Vec<u8>)>, MigrationRefusal> {
    let mut fresh = Vec::new();
    for (identity, as_text) in named_as(changes) {
      if !repository.holds_as(&identity, as_text)
        && !fresh.iter().any(|(held, _)| *held == identity)
      {
        let bytes = match self.pending.remove(&identity) {
          Some(bytes) => bytes,
          None => repository
            .text(&identity)
            .ok_or(MigrationRefusal::Corrupt)?
            .into_owned(),
        };
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

//! The version-control servers' stores on their owner shard (A-662; §4.17.1 "Durability and residency" and shared
//! pieces 2, 3 and 7; §4.17.2 "The repository in git's terms"; A-581, A-588, A-553, A-614, A-645, A-647).
//!
//! **One map.** A shard holds one content store for every repository of its partition (`slates_vcs::ContentStore`:
//! content by identity once, whichever repository and tool brought it, A-581), and one entry a repository, its tool's
//! store with what is derived from it beside it ([`RepositoryStore`]):
//! - git: A-614's store (`slates_git::store::Repository`) with its derived values (`slates_vcs::Derivations`), the first
//!   of them A-647 part 2's reach sets, kept as each version commits;
//! - Mercurial: A-588's index (`slates_hg::index::HgIndex`) with its derived values;
//! - Subversion: A-553's index (`slates_svn::index::Index`) beside the repository's one green.
//!
//! The entry is made with the repository's record by `CreateRepository` on its owner shard ([`make`]), freed with the
//! repository ([`free`]), and made again by a recovery ([`recover`]). The servers reach a store only through this map,
//! a step at a time, and keep no store of their own.
//!
//! **Budget (§4.2).** Every byte the content store counts (objects, rows, tables, the versions' records, derived values)
//! is charged to the shard's memory budget as retention, as a green's history is (`merge_service`). A step that grows the
//! store is lent the room the shard's budget could admit now, the arena first grown from its pool by what the step
//! brings, as a green's retention grows it; after the step the shard's charge is trued up to what the store holds, and
//! the store's own budget set back to what is charged ([`settle`]). A step past what the shard can lend is refused by
//! the store (`OverBudget`), typed, with nothing changed. No figure here is a tuning constant.
//!
//! **Durable as greens are.** A push's objects are its changeset; everything the store computes from them (positions,
//! rows, D(k), reached sets, derived values, pack fragments) is derived. A git push's one entry (A-635's append) holds
//! its record ([`record`]), written as the append commits: the objects of the push that the repository lacks then, by
//! the predicate the store's commit gives positions by ([`record_of`]), by content identity in admission order with
//! their bytes; the moves the store committed them with; and the deepest chain the push's pack carried, which every
//! record of the push carries. The partition's register keeps each entry's objects for as long as the repository lives
//! and a snapshot carries them (`slates_db`, A-662). The store commits between the register's guard and the entry's
//! apply ([`commit`], called by the append), so a push the guard or a verdict refuses never reaches the store. A
//! recovery makes each store again and replays every record, in log order, through the live path's own steps:
//! admission, seal, the moves, the commit and the derived values kept ([`replay_git`]), and refuses a record whose
//! objects are not the contents it names or whose admission would record other objects. That is guard then apply, as
//! `slates_db::partition` replays its log, so the store after a recovery equals the store before. A commit whose entry
//! was then not made durable leaves the store ahead of the log, so the store is made again from the log the same way
//! ([`rebuild`]).
//!
//! **Keys.** A-614's store names a green by a `u64` ([`green_key`]): the half of its volume id that holds its partition
//! and counter, which `verbs::fresh_volume_id` makes unique among the partition's volumes, so a recovery keys each green
//! as before.
//!
//! **Owed** (each in GAPS' §4.17 entry). A snapshot carrying each store's sealed tables by identity, so a recovery
//! replays only the log's suffix; the replay and a rebuild sliced a unit at a time within a record (a record is one unit
//! today, as a commit is one step); Mercurial's records and their replay (g304's push) and Subversion's (g268's commit
//! step), whose arms are placed here and whose records a recovery refuses typed until then; the Subversion index charged
//! by the bytes it holds, once slates-svn measures them; a version's pack fragments encoded again after a recovery, off
//! the answer's path as a live version's are (A-654); and the fleet half, holders replaying the same records (A-636).

use std::collections::BTreeMap;
use std::convert::Infallible;

use slates_db::catalog::{
  ObjectFormat as DbFormat, RepositoryId, RepositoryRecord, VcsTool, VolumeId as DbVolumeId,
};
use slates_git::object_name::{ObjectFormat, ObjectId};
use slates_git::store::views::PushedObjects;
use slates_git::store::{Incoming, Move, Push, PushRefused, ReachSets, Repository, StoreRefused};
use slates_hg::index::HgIndex;
use slates_ipc::protocol::Refusal;
use slates_vcs::{Admitted, ContentRefused, ContentStore, Derivations, Mode};
use slates_vfs::volume::Store;

use crate::state::ShardState;

pub(crate) mod paths;
pub(crate) mod record;
#[cfg(test)]
mod tests;

use record::{Noted, Record, RecordRefused, RecordWriter, RecordedMove, WriteRefused};

/// Counter: a repository whose store could not be made (a window too short for two identities, or a git repository
/// recorded in no git format): zero on every machine. Format: a counter name in the status report.
pub(crate) const STORE_UNMADE: &str = "vcs_store.unmade";
/// Counter: a refusal met while a store was given back; the rest was given back. Format: a counter name.
pub(crate) const RELEASE_REFUSED: &str = "vcs_store.release_refused";
/// Counter: bytes a step held that the shard's budget would not take, which the next step's settle charges: zero in every
/// correct use, since a step uses only the room it was lent. Format: a counter name.
pub(crate) const CHARGE_SHORT: &str = "vcs_store.charge_short";
/// Counter: a moved green's derived value not kept after its commit: the value is derived again when read. Format: a
/// counter name.
pub(crate) const VALUE_UNKEPT: &str = "vcs_store.value_unkept";
/// Counter: a record replayed into its store by a recovery or a rebuild: the replay's non-vacuity counter. Format: a
/// counter name.
pub(crate) const REPLAYED: &str = "vcs_store.replayed";
/// Counter: a store a recovery could not make again from its records: zero in every correct use. Format: a counter name.
pub(crate) const REPLAY_REFUSED: &str = "vcs_store.replay_refused";
/// Counter: a store made again from the log after its commit's entry was not made durable. Format: a counter name.
pub(crate) const REBUILT: &str = "vcs_store.rebuilt";
/// Counter: a store that refused a commit its entry's guard and verdicts passed, for a reason other than the budget:
/// zero in every correct use. Format: a counter name.
pub(crate) const COMMIT_REFUSED: &str = "vcs_store.commit_refused";

/// The stores of every repository on this shard's partition (module docs).
#[derive(Debug, Default)]
pub(crate) struct VcsStores {
  /// The content every store holds, made at the first repository with the content plane's window.
  content: Option<ContentStore>,
  /// Each repository's store.
  entries: BTreeMap<RepositoryId, RepositoryStore>,
  /// The repositories whose stores a recovery could not make again, and why: each is served no more until a reviewed
  /// recovery; bounded by the partition's repositories.
  unrecovered: BTreeMap<RepositoryId, ReplayRefused>,
  /// What the content store holds of the shard's budget, charged as retention.
  charged: u64,
}

/// One repository's store, by its tool (module docs).
#[derive(Debug)]
pub(crate) enum RepositoryStore {
  /// git's.
  Git(GitStore),
  /// Mercurial's.
  Mercurial(HgStore),
  /// Subversion's.
  Subversion(SvnStore),
}

/// A git repository's store: A-614's store and the values derived from it (A-647).
#[derive(Debug)]
pub(crate) struct GitStore {
  /// The store.
  pub(crate) repository: Repository,
  /// Its derived values, kept under (type, version).
  pub(crate) derivations: Derivations,
  /// The reach sets' type, eager: each version's value kept as it commits, from the admission's own walks.
  pub(crate) reach: ReachSets,
  /// The path map's type over the repository's greens (A-664 part 3, [`paths`]): each green's versions' maps of paths
  /// to git's files, kept in `derivations` as each version commits. Declared with the content store's window when the
  /// store is made ([`made`]); `None` for a store made without one.
  pub(crate) paths: Option<paths::GreenPaths>,
  /// Each green's map of its head, by the green's key (A-664 part 3, [`paths`]): one a member green that committed a
  /// version, its root held once by the store, replaced as each version commits and made again by a recovery.
  pub(crate) heads: BTreeMap<u64, paths::Head>,
  /// For each green's key, the green's version at each of the store's versions of it, in order: what a push's old
  /// value is read back to (`version_of`), as each record names it.
  versions: BTreeMap<u64, Vec<u64>>,
  /// What `versions` holds of the content store's budget (`ContentStore::charge`), charged before a commit keeps a
  /// version and given back with the store ([`versions_cost`]).
  charged: u64,
}

impl GitStore {
  /// An empty store of `format`.
  fn new(format: ObjectFormat) -> GitStore {
    GitStore {
      repository: Repository::new(format),
      derivations: Derivations::new(),
      reach: ReachSets::declare(Mode::Eager),
      paths: None,
      heads: BTreeMap::new(),
      versions: BTreeMap::new(),
      charged: 0,
    }
  }

  /// The green's version, and the tree, of the newest of the store's versions of the green keyed `key` whose head is
  /// `head`: a push's base (§4.17.1 item 1). It reads the green's versions newest first, each one row.
  pub(crate) fn version_of(
    &self,
    content: &ContentStore,
    key: u64,
    head: &ObjectId,
  ) -> Option<(u64, ObjectId)> {
    let versions = self.versions.get(&key)?;
    versions.iter().enumerate().rev().find_map(|(at, version)| {
      (self.repository.head(content, key, at).as_ref() == Some(head))
        .then(|| {
          self
            .repository
            .tree(content, key, at)
            .map(|tree| (*version, tree))
        })
        .flatten()
    })
  }

  /// The green's versions at each of the store's versions of the green keyed `key`.
  pub(crate) fn versions(&self, key: u64) -> &[u64] {
    self.versions.get(&key).map_or(&[], Vec::as_slice)
  }
}

/// Which green a push's move names: one the repository holds, or the one its append makes for a new branch, named by its
/// branch's name until the append makes it (`repository::push::push_sealed` resolves it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
  /// A green of the repository.
  Green(DbVolumeId),
  /// The green the append makes for the branch of this name.
  Created(String),
}

/// A Mercurial repository's store: A-588's index and the values derived from it.
#[derive(Debug, Default)]
pub(crate) struct HgStore {
  /// The index.
  pub(crate) index: HgIndex,
  /// Its derived values.
  pub(crate) derivations: Derivations,
}

/// A Subversion repository's store: A-553's index, beside its one green.
#[derive(Debug, Default)]
pub(crate) struct SvnStore {
  /// The index.
  pub(crate) index: slates_svn::index::Index,
}

/// Why a recovery could not make a store again from its records. Each leaves the store unserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplayRefused {
  /// A record's bytes.
  Record(RecordRefused),
  /// An object's bytes are not the content its record names.
  Identity,
  /// The admission made another record than the one the log holds: the recomputation differs (§4.16).
  Diverged,
  /// The store refused what it took before.
  Git(GitRefused),
  /// A record of a tool whose replay is owed: Mercurial's (g304's push) and Subversion's (g268's commit step).
  Owed,
}

/// Why a git push's step was refused. Each leaves the store as it was, and the push holding what it held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GitRefused {
  /// No git store of the repository is held here.
  NoStore,
  /// An object refused on arrival: git's naming, its claimed id, a commit git's parser refuses, or the budget.
  Object(PushRefused<Infallible>),
  /// The store refused: an object neither the push nor the repository holds, a green moved, or the budget.
  Store(StoreRefused),
  /// The allocator refused a record's room, or its length is past what an address can hold.
  Allocation,
}

/// A record not written, in a push's terms: an object whose bytes the content store does not hold whole is one the
/// store cannot read, as its own refusal names it.
fn unwritten(refused: WriteRefused) -> GitRefused {
  match refused {
    WriteRefused::Unreadable { id } => GitRefused::Store(StoreRefused::Unreadable { id }),
    WriteRefused::Allocation => GitRefused::Allocation,
  }
}

/// A content store's refusal, in a push's terms.
fn content_refused(refused: ContentRefused) -> GitRefused {
  GitRefused::Store(StoreRefused::Content(refused))
}

impl VcsStores {
  /// Whether repository `id`'s store is held here.
  pub(crate) fn holds(&self, id: RepositoryId) -> bool {
    self.entries.contains_key(&id)
  }

  /// Why repository `id`'s store was not made again by the recovery, if it was not.
  pub(crate) fn unrecovered(&self, id: RepositoryId) -> Option<&ReplayRefused> {
    self.unrecovered.get(&id)
  }

  /// The content store, once a repository made it.
  pub(crate) fn content(&self) -> Option<&ContentStore> {
    self.content.as_ref()
  }

  /// What the stores hold of the shard's budget.
  pub(crate) fn charged(&self) -> u64 {
    self.charged
  }

  /// Repository `id`'s store, if held.
  pub(crate) fn store(&self, id: RepositoryId) -> Option<&RepositoryStore> {
    self.entries.get(&id)
  }

  /// Repository `id`'s git store with the content store, lent for one step.
  pub(crate) fn git(&mut self, id: RepositoryId) -> Option<(&mut ContentStore, &mut GitStore)> {
    let content = self.content.as_mut()?;
    match self.entries.get_mut(&id)? {
      RepositoryStore::Git(store) => Some((content, store)),
      _ => None,
    }
  }

  /// Repository `id`'s git store with the content store, read.
  pub(crate) fn git_ref(&self, id: RepositoryId) -> Option<(&ContentStore, &GitStore)> {
    let content = self.content.as_ref()?;
    match self.entries.get(&id)? {
      RepositoryStore::Git(store) => Some((content, store)),
      _ => None,
    }
  }

  /// Repository `id`'s Mercurial store with the content store, lent for one step.
  pub(crate) fn mercurial(
    &mut self,
    id: RepositoryId,
  ) -> Option<(&mut ContentStore, &mut HgStore)> {
    let content = self.content.as_mut()?;
    match self.entries.get_mut(&id)? {
      RepositoryStore::Mercurial(store) => Some((content, store)),
      _ => None,
    }
  }

  /// Repository `id`'s Subversion store, lent for one step.
  pub(crate) fn subversion(&mut self, id: RepositoryId) -> Option<&mut SvnStore> {
    match self.entries.get_mut(&id)? {
      RepositoryStore::Subversion(store) => Some(store),
      _ => None,
    }
  }
}

/// The git format a repository's record names: SHA-1 or SHA-256; none for the revisions Subversion numbers.
fn git_format(format: DbFormat) -> Option<ObjectFormat> {
  match format {
    DbFormat::Sha1 => Some(ObjectFormat::Sha1),
    DbFormat::Sha256 => Some(ObjectFormat::Sha256),
    DbFormat::Revisions => None,
  }
}

/// The key a green has in its repository's git store (module docs): its volume id's partition and counter.
pub(crate) fn green_key(green: DbVolumeId) -> u64 {
  green
    .bytes
    .get(GREEN_KEY_START..)
    .and_then(|half| <[u8; GREEN_KEY_BYTES]>::try_from(half).ok())
    .map_or(0, u64::from_be_bytes)
}

/// Format: where a volume id's partition and counter begin (`verbs::fresh_volume_id`: the host's eight bytes first).
const GREEN_KEY_START: usize = size_of::<u64>();
/// Format: the bytes of a green's key: the partition's two and the counter's six.
const GREEN_KEY_BYTES: usize = size_of::<u64>();

/// Makes `record`'s store on its owner shard, beside its record (module docs): empty, so it charges nothing yet. A
/// store held already is kept.
pub(crate) fn make(state: &mut ShardState, record: &RepositoryRecord) {
  let window = state.store.content.chunk_bytes();
  if made(&mut state.vcs_stores, record, window).is_err() {
    state.count(STORE_UNMADE, 1);
  }
}

/// [`make`]'s step over the stores, the content store made at the first repository with the content plane's `window`.
fn made(stores: &mut VcsStores, record: &RepositoryRecord, window: usize) -> Result<(), Unmade> {
  if stores.content.is_none() {
    let content = ContentStore::new(stores.charged, window).map_err(|_| Unmade)?;
    stores.content = Some(content);
  }
  let store = match record.tool {
    VcsTool::Git => {
      let mut git = GitStore::new(git_format(record.format).ok_or(Unmade)?);
      // The map splits by the content store's own window, which its type's name carries (A-647).
      git.paths = stores
        .content
        .as_ref()
        .and_then(|content| paths::declare(content.window()));
      RepositoryStore::Git(git)
    }
    VcsTool::Mercurial => RepositoryStore::Mercurial(HgStore::default()),
    VcsTool::Subversion => RepositoryStore::Subversion(SvnStore::default()),
  };
  stores.entries.entry(record.id).or_insert(store);
  Ok(())
}

/// A store not made ([`made`]).
struct Unmade;

/// Frees repository `id`'s store with the repository (module docs): every hold of its rows, tables, versions and
/// derived values given back to the content store, then the shard's charge trued up to what remains. A refusal while
/// giving back is counted, and the rest is given back.
pub(crate) fn free(state: &mut ShardState, id: RepositoryId) {
  state.vcs_stores.unrecovered.remove(&id);
  if !release(&mut state.vcs_stores, id) {
    state.count(RELEASE_REFUSED, 1);
  }
  settle(state);
}

/// Gives back everything repository `id`'s store holds; whether every hold went back.
fn release(stores: &mut VcsStores, id: RepositoryId) -> bool {
  let (Some(entry), Some(content)) = (stores.entries.remove(&id), stores.content.as_mut()) else {
    return true;
  };
  match entry {
    RepositoryStore::Git(store) => {
      let versions = content.discharge(store.charged).is_ok();
      let values = store.derivations.release(content).is_ok();
      let maps = store.heads.values().fold(true, |whole, head| {
        content.release(&head.root).is_ok() && whole
      });
      store.repository.release(content).is_ok() && values && versions && maps
    }
    RepositoryStore::Mercurial(store) => {
      let values = store.derivations.release(content).is_ok();
      store.index.release(content).is_ok() && values
    }
    RepositoryStore::Subversion(_) => true,
  }
}

/// Lends the stores room for one step (module docs): the arena grown by `bytes` from its pool where it can be, then
/// every byte the shard's budget could admit now given to the content store as room beside what it holds.
fn lend(store: &mut Store, stores: &mut VcsStores, bytes: u64) {
  store.make_room(bytes);
  let room = store.admittable();
  let budget = stores.charged.saturating_add(room);
  if let Some(content) = stores.content.as_mut() {
    content.set_budget(budget);
  }
}

/// Trues the shard's charge up to what the content store holds after a step, and sets the content store's budget back
/// to what is charged (module docs). Whether the shard took every byte the step held ([`CHARGE_SHORT`] otherwise).
fn settle_in(store: &mut Store, stores: &mut VcsStores) -> bool {
  let used = stores.content.as_ref().map_or(0, ContentStore::used);
  let mut whole = true;
  if used > stores.charged {
    let more = used.saturating_sub(stores.charged);
    if store.budget.charge_retention(more).is_ok() {
      stores.charged = used;
    } else {
      whole = false;
    }
  } else {
    store
      .budget
      .credit_retention(stores.charged.saturating_sub(used));
    stores.charged = used;
  }
  let charged = stores.charged;
  if let Some(content) = stores.content.as_mut() {
    content.set_budget(charged);
  }
  whole
}

/// [`settle_in`] over the shard's state, counting a charge the budget would not take.
pub(crate) fn settle(state: &mut ShardState) {
  if !settle_in(&mut state.store, &mut state.vcs_stores) {
    state.count(CHARGE_SHORT, 1);
  }
}

/// Runs `step` over the stores with the room [`lend`] gives for `bytes`, then settles.
fn within_budget<T>(
  state: &mut ShardState,
  bytes: u64,
  step: impl FnOnce(&mut VcsStores) -> T,
) -> T {
  lend(&mut state.store, &mut state.vcs_stores, bytes);
  let answer = step(&mut state.vcs_stores);
  settle(state);
  answer
}

/// Runs `read` over the stores, read only, with the room [`lend`] gives: what a step that parses objects needs, since
/// A-614's store lists a tree, a tag or a commit within the content store's room (`ContentStore::room`), as A-638's view
/// does, and a settled store has none, its budget being what it holds. Read only, the stores hold after it what they
/// held before, so the room lent is given back and nothing is charged.
fn lent_to_read<T>(
  (store, stores): (&mut Store, &mut VcsStores),
  read: impl FnOnce(&VcsStores) -> T,
) -> T {
  lend(store, stores, 0);
  let answer = read(stores);
  let charged = stores.charged;
  if let Some(content) = stores.content.as_mut() {
    content.set_budget(charged);
  }
  answer
}

/// A git push in its quarantine (§4.17.2 "A push" item 3): each object admitted to the repository's store as it comes
/// and noted for the push's record, which an append writes as it commits ([`SealedPush::record`]).
#[derive(Debug)]
pub(crate) struct GitPush {
  /// The repository.
  repository: RepositoryId,
  /// The objects, in quarantine.
  push: Push,
  /// The record, as written so far.
  record: RecordWriter,
}

/// A git push sealed: what A-635's append commits beside its entry, with the moves that append admits
/// (`repository::push::push_sealed`, [`SealedPush::admit`], [`commit`]). Its caller keeps it across appends: a push
/// without `atomic` appends each ref on its own, and a refused append leaves it whole for the next. The first commit
/// gives the store every object the push brought, so later records hold their moves alone.
#[derive(Debug)]
pub(crate) struct SealedPush {
  /// The repository.
  repository: RepositoryId,
  /// The objects, in quarantine until a commit takes them.
  push: Push,
  /// The moves the current append admitted, in the order the store commits them.
  moves: Vec<Move>,
  /// The same moves as the record names them: key, the green's version, head.
  keyed: Vec<RecordedMove>,
  /// The record's objects, until a commit takes them.
  objects: RecordWriter,
}

impl SealedPush {
  /// The repository it is a push to.
  pub(crate) fn repository(&self) -> RepositoryId {
    self.repository
  }

  /// The record the entry holds (`RepositoryEntry::objects`): the moves now admitted and the objects not yet committed,
  /// written now, as the append commits ([`record_of`]).
  pub(crate) fn record(&self, stores: &VcsStores) -> Result<Vec<u8>, GitRefused> {
    let (content, store) = stores.git_ref(self.repository).ok_or(GitRefused::NoStore)?;
    record_of(
      content,
      store,
      (&self.objects, self.push.deepest()),
      &self.keyed,
    )
  }

  /// Admits `moves` (each a green's key, its version once the entry moves it, and its new head) in place of any
  /// admitted before, against the store as it is now: what one append commits. The admission walks the push's trees
  /// and tags within the room lent for it ([`lent_to_read`]). Refused, the push keeps what it holds.
  pub(crate) fn admit(
    &mut self,
    stores: (&mut Store, &mut VcsStores),
    moves: Vec<RecordedMove>,
  ) -> Result<(), GitRefused> {
    let (repository, push) = (self.repository, &self.push);
    let admitted = lent_to_read(stores, |stores| {
      let (content, store) = stores.git_ref(repository).ok_or(GitRefused::NoStore)?;
      admit_moves(content, store, push, &moves)
    })?;
    self.moves = admitted;
    self.keyed = moves;
    Ok(())
  }

  /// Gives back every hold the push took: a push no append committed. One a commit took holds nothing.
  pub(crate) fn abandon(self, state: &mut ShardState) {
    abandon_push(state, self.push);
  }
}

impl GitPush {
  /// An empty push into repository `repository`'s git store, in its format; none when no git store of it is held here.
  pub(crate) fn begin(stores: &VcsStores, repository: RepositoryId) -> Option<GitPush> {
    let (_, store) = stores.git_ref(repository)?;
    Some(GitPush {
      repository,
      push: Push::new(store.repository.format()),
      record: RecordWriter::default(),
    })
  }

  /// Admits `objects`, in the order the push brought them, with the room their bytes take lent for the step. Refused at
  /// the first object refused; the push keeps what it admitted before, for its caller to abandon.
  pub(crate) fn admit_all(
    &mut self,
    state: &mut ShardState,
    objects: &[Incoming<'_>],
  ) -> Result<(), GitRefused> {
    let bytes = objects
      .iter()
      .map(|incoming| u64::try_from(incoming.content.len()).unwrap_or(u64::MAX))
      .fold(0, u64::saturating_add);
    within_budget(state, bytes, |stores| {
      let (content, store) = stores.git(self.repository).ok_or(GitRefused::NoStore)?;
      for incoming in objects {
        admit_object(
          content,
          store,
          (&mut self.push, &mut self.record),
          *incoming,
        )?;
      }
      Ok(())
    })
  }

  /// Seals the push: every commit's generation number, its parents read from the push or the repository (A-614). Each
  /// append then admits its own moves ([`SealedPush::admit`]). Refused, the push is abandoned and its holds given back.
  pub(crate) fn seal(mut self, state: &mut ShardState) -> Result<SealedPush, GitRefused> {
    let sealed = match state.vcs_stores.git(self.repository) {
      Some((content, store)) => store
        .repository
        .seal(content, &mut self.push)
        .map_err(GitRefused::Store),
      None => Err(GitRefused::NoStore),
    };
    match sealed {
      Ok(()) => Ok(SealedPush {
        repository: self.repository,
        push: self.push,
        moves: Vec::new(),
        keyed: Vec::new(),
        objects: self.record,
      }),
      Err(refused) => {
        abandon_push(state, self.push);
        Err(refused)
      }
    }
  }

  /// Gives back every hold the push took.
  pub(crate) fn abandon(self, state: &mut ShardState) {
    abandon_push(state, self.push);
  }

  /// Runs `read` over the repository's objects and this push's, the index first (A-638's view), as a ref's work is
  /// built from them (A-618), within the room lent for it: the view lists a tree within the content store's room
  /// ([`lent_to_read`]). None when no git store of the repository is held here.
  pub(crate) fn read<R>(
    &self,
    stores: (&mut Store, &mut VcsStores),
    read: impl FnOnce(&PushedObjects<'_>) -> R,
  ) -> Option<R> {
    lent_to_read(stores, |stores| {
      let (content, git) = stores.git_ref(self.repository)?;
      Some(read(&PushedObjects::new(
        &git.repository,
        content,
        &self.push,
      )))
    })
  }
}

/// Gives back `push`'s holds, then settles.
fn abandon_push(state: &mut ShardState, push: Push) {
  let given = match state.vcs_stores.content.as_mut() {
    Some(content) => push.abandon(content).is_ok(),
    None => true,
  };
  if !given {
    state.count(RELEASE_REFUSED, 1);
  }
  settle(state);
}

/// Admits one object into `push`: named by git's rule and checked against the id its source claims (A-614). An object
/// arriving under a claimed id the repository indexes already is passed over, since the store reads an indexed object
/// from its index before any push (A-638's order). Every object new to the push is noted for the record by the root its
/// quarantine holds it under, whether or not the repository holds it now: which of them the record holds is decided
/// when an append commits ([`record_of`]), by the predicate that commit gives positions by, since another push can
/// commit an object this one holds between its admission and its append, and a store made again from the log can lose
/// one. So the record names exactly the objects its commit positions.
fn admit_object(
  content: &mut ContentStore,
  store: &GitStore,
  (push, record): (&mut Push, &mut RecordWriter),
  incoming: Incoming<'_>,
) -> Result<(), GitRefused> {
  if let Some(claimed) = incoming.claimed
    && store.repository.object(content, &claimed).is_some()
  {
    return Ok(());
  }
  let naming = store.repository.naming();
  let (id, admitted) = push
    .admit::<_, Infallible>(content, &naming, incoming)
    .map_err(GitRefused::Object)?;
  if admitted == Admitted::New {
    let (_, identity, len) = push
      .object(&id)
      .ok_or(GitRefused::Store(StoreRefused::Unreadable { id }))?;
    record.object(Noted {
      kind: incoming.kind,
      id,
      identity,
      len,
    });
  }
  Ok(())
}

/// The record a commit of `moves` writes now (`record::RecordWriter::finish`): `deepest` and `moves`, then each object
/// `writer` noted that the repository lacks now, its bytes read back from `content`. Lacking is the predicate the
/// store's commit gives positions by (`Repository::object`, the index its `fresh` reads), so the record holds exactly
/// the objects the commit that follows positions.
pub(crate) fn record_of(
  content: &ContentStore,
  store: &GitStore,
  (writer, deepest): (&RecordWriter, u64),
  moves: &[RecordedMove],
) -> Result<Vec<u8>, GitRefused> {
  writer
    .finish(content, lacks(content, store), (deepest, moves))
    .map_err(unwritten)
}

/// Whether the repository lacks an object: the predicate a commit gives positions by ([`record_of`]).
fn lacks<'s>(content: &'s ContentStore, store: &'s GitStore) -> impl Fn(&ObjectId) -> bool + 's {
  move |id| store.repository.object(content, id).is_none()
}

/// Seals `push` and admits each of `moves` against the store, in order (A-614's `seal` and `admit`).
fn seal_moves(
  content: &ContentStore,
  store: &GitStore,
  push: &mut Push,
  moves: &[RecordedMove],
) -> Result<Vec<Move>, GitRefused> {
  store
    .repository
    .seal(content, push)
    .map_err(GitRefused::Store)?;
  admit_moves(content, store, push, moves)
}

/// Admits each of `moves` against the store and the sealed `push`, in order (A-614's `admit`).
fn admit_moves(
  content: &ContentStore,
  store: &GitStore,
  push: &Push,
  moves: &[RecordedMove],
) -> Result<Vec<Move>, GitRefused> {
  moves
    .iter()
    .map(|(green, _, head)| {
      store
        .repository
        .admit(content, push, *green, *head)
        .map_err(GitRefused::Store)
    })
    .collect()
}

/// Commits `sealed` into its repository's store (module docs), once the entry that holds its record has passed the
/// register's guard and before anything of the entry is applied: positions, rows and versions as A-614's commit makes
/// them, then each moved green's derived value kept. Committed, the store holds the push's objects and later records
/// hold its moves alone. Refused, nothing of the store changed and the push still holds its objects.
pub(crate) fn commit(state: &mut ShardState, sealed: &mut SealedPush) -> Result<(), GitRefused> {
  let repository = sealed.repository;
  let unkept = within_budget(state, 0, |stores| {
    let (content, store) = stores.git(repository).ok_or(GitRefused::NoStore)?;
    commit_moves(
      content,
      store,
      &mut sealed.push,
      (&sealed.moves, &sealed.keyed),
    )
  })?;
  sealed.objects = RecordWriter::default();
  state.count(VALUE_UNKEPT, unkept);
  Ok(())
}

/// A-614's commit of `push` with `moves`, each green's version kept with its key, charged first ([`versions_cost`]), then
/// each moved green's newest value of the reach sets kept ([`keep_values`]). Refused, the charge is given back and
/// nothing of the store changed. The values not kept: each is derived again when read.
fn commit_moves(
  content: &mut ContentStore,
  store: &mut GitStore,
  push: &mut Push,
  (moves, keyed): (&[Move], &[RecordedMove]),
) -> Result<u64, GitRefused> {
  let cost = versions_cost(&store.versions, keyed);
  content.charge(cost).map_err(content_refused)?;
  if let Err(refused) = store.repository.commit(content, push, moves) {
    content.discharge(cost).map_err(content_refused)?;
    return Err(GitRefused::Store(refused));
  }
  store.charged = store.charged.saturating_add(cost);
  for (key, version, _) in keyed {
    store.versions.entry(*key).or_default().push(*version);
  }
  Ok(keep_values(content, store, moves))
}

/// Derived: the bytes a kept version takes in its green's list, one word.
const VERSION_BYTES: usize = size_of::<u64>();

/// Derived: the bytes a green's first kept version adds besides its word: its key's word and its list's header.
const KEY_BYTES: usize = size_of::<u64>() + size_of::<Vec<u64>>();

/// What keeping `keyed`'s versions adds to a store's `versions`, by the types it holds, as A-614's store charges its
/// chains by their entries: a version's word each, and a key's word and a list's header for a green it holds no version
/// of yet (`GitStore::charged`).
fn versions_cost(versions: &BTreeMap<u64, Vec<u64>>, keyed: &[RecordedMove]) -> u64 {
  let mut cost = 0usize;
  for (at, (key, ..)) in keyed.iter().enumerate() {
    cost = cost.saturating_add(VERSION_BYTES);
    let earlier = keyed
      .get(..at)
      .unwrap_or_default()
      .iter()
      .any(|(other, ..)| other == key);
    if !versions.contains_key(key) && !earlier {
      cost = cost.saturating_add(KEY_BYTES);
    }
  }
  u64::try_from(cost).unwrap_or(u64::MAX)
}

/// Each moved green's newest value of the reach sets kept as the store's admission computed it, with no second walk
/// (A-647 part 2), the caller's own hold of it given back once the row holds it. The values not kept: each is derived
/// again when read.
fn keep_values(content: &mut ContentStore, store: &mut GitStore, moves: &[Move]) -> u64 {
  let mut unkept = 0u64;
  for movement in moves {
    let green = movement.green();
    let Ok((version, root)) = store.repository.reach_value(content, green) else {
      unkept = unkept.saturating_add(1);
      continue;
    };
    let kept = store.derivations.keep(
      content,
      &store.reach,
      &store.repository,
      (green, version),
      root,
    );
    let released = content.release(&root);
    if kept.is_err() || released.is_err() {
      unkept = unkept.saturating_add(1);
    }
  }
  unkept
}

/// The wire's refusal for a store that refused a commit: the shard's budget, by the room it had (a record's room the
/// allocator refused, by what the shard could admit now, as `staging` answers one); anything else is a store not
/// available, which [`COMMIT_REFUSED`] counts, since a store kept beside its greens has no other reason to refuse a
/// commit their verdicts passed.
pub(crate) fn refusal_of(state: &mut ShardState, refused: &GitRefused) -> Refusal {
  let budget = match refused {
    GitRefused::Store(StoreRefused::Content(ContentRefused::OverBudget { room, .. }))
    | GitRefused::Object(PushRefused::Content(ContentRefused::OverBudget { room, .. })) => {
      Some(*room)
    }
    GitRefused::Allocation => Some(state.store.admittable()),
    _ => None,
  };
  match budget {
    Some(available) => Refusal::BudgetExceeded { available },
    None => {
      state.count(COMMIT_REFUSED, 1);
      Refusal::ContentUnavailable
    }
  }
}

/// Replays one record into `store` (module docs): every object checked against the content identity the record names
/// before it is admitted, then the live path's own steps in its order; the objects the admission would record must be
/// the record's. A refused replay gives back what it held. The derived values not kept, as [`commit_moves`] answers
/// them.
fn replay_git(
  content: &mut ContentStore,
  store: &mut GitStore,
  bytes: &[u8],
) -> Result<u64, ReplayRefused> {
  let format = store.repository.format();
  let record = Record::decode(bytes, format).map_err(ReplayRefused::Record)?;
  let mut push = Push::new(format);
  let replayed = replay_push(content, store, &mut push, &record);
  // A committed push holds nothing, so this gives back only what a refused replay held.
  let given = push.abandon(content);
  let unkept = replayed?;
  given.map_err(|refused| ReplayRefused::Git(GitRefused::Object(PushRefused::Content(refused))))?;
  Ok(unkept)
}

/// [`replay_git`]'s steps over `push`: the record's deepest chain carried (every record of a push carries it, the ones
/// holding no object too, `Push::carried`), the objects, the objects the admission would record compared with the
/// record's, the seal and the moves, the commit.
fn replay_push(
  content: &mut ContentStore,
  store: &mut GitStore,
  push: &mut Push,
  record: &Record<'_>,
) -> Result<u64, ReplayRefused> {
  push.carried(record.deepest);
  let mut remade = RecordWriter::default();
  replay_objects(content, store, (&mut *push, &mut remade), record)?;
  if !recorded_alike(content, store, &remade, record) {
    return Err(ReplayRefused::Diverged);
  }
  let moves = seal_moves(content, store, push, &record.moves).map_err(ReplayRefused::Git)?;
  commit_moves(content, store, push, (&moves, &record.moves)).map_err(ReplayRefused::Git)
}

/// Whether the objects the replay's admission would record now, by the predicate the live append wrote its record by
/// ([`record_of`]), are the record's, in order, by type, content identity and length. Each identity was checked
/// against the record's bytes before the object was admitted, so this compares the record written again without
/// copying its bytes: a recomputation that differs is refused (§4.16).
fn recorded_alike(
  content: &ContentStore,
  store: &GitStore,
  remade: &RecordWriter,
  record: &Record<'_>,
) -> bool {
  let admitted = remade
    .lacked(lacks(content, store))
    .map(|noted| (noted.kind, noted.identity, noted.len));
  let recorded = record.objects.iter().map(|object| {
    let len = u64::try_from(object.content.len()).unwrap_or(u64::MAX);
    (object.kind, object.identity, len)
  });
  admitted.eq(recorded)
}

/// Admits a record's objects, each checked against its identity first, at the record's deepest chain.
fn replay_objects(
  content: &mut ContentStore,
  store: &GitStore,
  (push, remade): (&mut Push, &mut RecordWriter),
  record: &Record<'_>,
) -> Result<(), ReplayRefused> {
  for object in &record.objects {
    if content.object_id(object.content) != object.identity {
      return Err(ReplayRefused::Identity);
    }
    let incoming = Incoming {
      kind: object.kind,
      claimed: None,
      content: object.content,
      depth: record.deepest,
    };
    admit_object(content, store, (&mut *push, &mut *remade), incoming)
      .map_err(ReplayRefused::Git)?;
  }
  Ok(())
}

/// Replays repository `id`'s record at `index` into its store, with the room the record's bytes take lent for the step;
/// none when it has no record there.
fn replay_record(
  state: &mut ShardState,
  id: RepositoryId,
  index: usize,
) -> Option<Result<(), ReplayRefused>> {
  let ShardState {
    db,
    store,
    vcs_stores,
    ..
  } = &mut *state;
  let bytes = db.partition().repositories().object_record(id, index)?;
  lend(
    store,
    vcs_stores,
    u64::try_from(bytes.len()).unwrap_or(u64::MAX),
  );
  let replayed = replay_into(vcs_stores, id, bytes);
  let whole = settle_in(store, vcs_stores);
  if !whole {
    state.count(CHARGE_SHORT, 1);
  }
  Some(replayed.map(|unkept| state.count(VALUE_UNKEPT, unkept)))
}

/// Replays `bytes` into repository `id`'s store, by its tool: the derived values not kept.
fn replay_into(
  stores: &mut VcsStores,
  id: RepositoryId,
  bytes: &[u8],
) -> Result<u64, ReplayRefused> {
  let unheld = ReplayRefused::Git(GitRefused::NoStore);
  let content = stores.content.as_mut().ok_or(unheld.clone())?;
  match stores.entries.get_mut(&id) {
    Some(RepositoryStore::Git(store)) => replay_git(content, store, bytes),
    Some(RepositoryStore::Mercurial(_) | RepositoryStore::Subversion(_)) => {
      Err(ReplayRefused::Owed)
    }
    None => Err(unheld),
  }
}

/// The outcome of one record's replay, counted; a refused one leaves repository `id`'s store unserved, given back, with
/// why. Whether to go on.
fn replayed(state: &mut ShardState, id: RepositoryId, outcome: Result<(), ReplayRefused>) -> bool {
  match outcome {
    Ok(()) => {
      state.count(REPLAYED, 1);
      true
    }
    Err(why) => {
      slates_machine::log_line!(
        "slates-server: partition {}: a repository's store was not made again from its records: {why:?}",
        state.partition
      );
      state.count(REPLAY_REFUSED, 1);
      if !release(&mut state.vcs_stores, id) {
        state.count(RELEASE_REFUSED, 1);
      }
      settle(state);
      state.vcs_stores.unrecovered.insert(id, why);
      false
    }
  }
}

/// Makes every repository's store of this shard's partition again, and replays each one's records in log order (module
/// docs), a record a unit of the recovery's slices.
pub(crate) async fn recover(
  state: &mut ShardState,
  slices: &mut crate::recovery_slices::RecoverySlices,
) {
  let records: Vec<RepositoryRecord> = state
    .db
    .partition()
    .repositories()
    .repositories()
    .cloned()
    .collect();
  for record in records {
    make(state, &record);
    let mut index = 0usize;
    while let Some(outcome) = replay_record(state, record.id, index) {
      if !replayed(state, record.id, outcome) {
        break;
      }
      index = index.saturating_add(1);
      slices.unit_done().await;
    }
    // Each green's map of its head, made again as the live store holds it (A-664 part 3), a green a unit.
    let greens: Vec<DbVolumeId> = state
      .db
      .partition()
      .repositories()
      .members_of(record.id)
      .map(|member| member.green)
      .collect();
    for green in greens {
      paths::remake_green(state, green);
      slices.unit_done().await;
    }
  }
}

/// Makes repository `id`'s store again from the log (module docs): what a commit whose entry was not made durable left
/// ahead of the log is given back, and every record replayed, as a recovery replays them, in one step.
pub(crate) fn rebuild(state: &mut ShardState, id: RepositoryId) {
  free(state, id);
  let Some(record) = state.db.partition().repositories().repository(id).cloned() else {
    return;
  };
  make(state, &record);
  let mut index = 0usize;
  while let Some(outcome) = replay_record(state, id, index) {
    if !replayed(state, id, outcome) {
      break;
    }
    index = index.saturating_add(1);
  }
  // Each green's map of its head, made again as the live store holds it (A-664 part 3).
  paths::remake(state, id);
  state.count(REBUILT, 1);
}

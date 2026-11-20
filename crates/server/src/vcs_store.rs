//! The version-control servers' stores on their owner shard (A-662; §4.17.1 "Durability and residency" and shared
//! pieces 2, 3 and 7; §4.17.2 "The repository in git's terms"; A-581, A-588, A-553, A-614, A-645, A-647).
//!
//! **One map.** A shard holds one content store for every repository of its partition (`slates_vcs::ContentStore`:
//! content by identity once, whichever repository and tool brought it, A-581), and one entry a repository, its tool's
//! store with what is derived from it beside it ([`RepositoryStore`]):
//! - git: A-614's store (`slates_git::store::Repository`) with its derived values (`slates_vcs::Derivations`), the first
//!   of them A-647 part 2's reach sets, kept as each version commits;
//! - Mercurial: A-588's index (`slates_hg::index::HgIndex`) with its derived values;
//! - Subversion: the repository's records (`slates_svn::migration::Repository`: A-553's index, every revision's
//!   properties, every text and property list by content identity, the lock table and the UUID) beside its one green
//!   (A-655 part 2b), made from the record its creation's entry holds.
//!
//! The entry is made with the repository's record by `CreateRepository` on its owner shard ([`make`]), freed with the
//! repository ([`free`]), and made again by a recovery ([`recover`]). The servers reach a store only through this map,
//! a step at a time, and keep no store of their own. A Mercurial store made empty takes the history of a repository
//! directory on the host through an import ([`hg_import`], A-675), which lives in the map between its turns, one a
//! repository, and is given back with the repository.
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
//! ([`rebuild`]). A Mercurial import's entry (A-687, [`hg_import`]) holds its record, every revision it kept, whole,
//! and its markers: replayed through the import's own steps into the same index ([`replay_hg`]), on this node or a
//! successor, which never reads the importing host's directory (R8).
//!
//! **Keys.** A-614's store names a green by a `u64` ([`green_key`]): the half of its volume id that holds its partition
//! and counter, which `verbs::fresh_volume_id` and `verbs::fresh_family_id_at` make unique among the partition's volumes
//! (a family id's family bit, tag and number in the counter's place, its number unique on the partition), so a recovery
//! keys each green as before.
//!
//! **Git LFS objects** (A-629 part 3). A git repository's LFS objects live in its store's LFS index (`GitStore::lfs`,
//! `slates_git::lfs::objects`): each object's bytes in the shared content store, one row under its oid, apart from the
//! git objects' index. An upload is one entry of the repository's logs holding the object's record ([`lfs_record`]: the
//! oid and the bytes); the index keeps the object between the register's guard and the entry's apply ([`admit_lfs`],
//! the bytes checked against the oid there), so an upload a guard refuses keeps nothing and one acknowledged is
//! durable as the entry is. A recovery replays the record into the index through the same check ([`replay_lfs`]), told
//! from a push's record by its first word.
//!
//! **Subversion** (A-655 part 2b). A Subversion repository's entries hold its records (`slates_svn::commit::record`):
//! the entry that joins its root holds its creation (its UUID and revision 0's date), and each entry that moves its
//! green holds the revision the commit step committed. A recovery replays them through the step's own check and apply
//! (`replay_svn`), so the records after it equal the records before; the creation's own entry is replayed the same way
//! when the repository is made ([`make_from_log`]). The records are charged to the shard's budget by the bytes they hold
//! (their texts, property lists and revision properties) as retention: a step is lent the room the shard could admit
//! ([`lend_svn`]), the growth it checked is charged before its append ([`reserve_svn`]), and the charge is trued up after
//! it ([`settle_svn`]); their index's entries are bounded by the room's bytes over the fewest an entry takes
//! (`Index::ENTRY_BYTES`). A dump's import ([`svn_import`], A-652 part 3) commits each record it makes through the same
//! steps: a revision moves the green, and the UUID and revision 0's properties it sets are each an entry of their own
//! that moves no green, replayed by the step's own apply (`commit_record`).
//!
//! **Owed** (each in GAPS' §4.17 entry). A snapshot carrying each store's sealed tables by identity, so a recovery
//! replays only the log's suffix; a git or Subversion record's replay, and a rebuild's, sliced a unit at a time within a
//! record (a Mercurial record's recovery replay is, a revision a unit since A-732, `replay_hg_sliced`); a Mercurial push's obsmarkers in its record (its revisions replay since
//! A-680 part 2c, as an import's since A-687, [`replay_hg`]); the Subversion index charged by the bytes it holds, once
//! slates-svn measures them (its entries are bounded meanwhile); a version's pack fragments encoded again after a
//! recovery, off the answer's path as a live version's are (A-654); and the fleet half, holders replaying the same
//! records (A-636).

use std::collections::BTreeMap;
use std::convert::Infallible;

use slates_db::catalog::{
  ObjectFormat as DbFormat, RepositoryId, RepositoryRecord, VcsTool, VolumeId as DbVolumeId,
};
use slates_git::lfs::Oid;
use slates_git::lfs::objects::{LfsObjects, ObjectRefused};
use slates_git::object_name::{ObjectFormat, ObjectId};
use slates_git::store::views::PushedObjects;
use slates_git::store::{Incoming, Move, Push, PushRefused, ReachSets, Repository, StoreRefused};
use slates_hg::index::HgIndex;
use slates_ipc::protocol::Refusal;
use slates_svn::commit::record::{Record as SvnRecord, RecordRefused as SvnRecordRefused};
use slates_svn::commit::revision::StepRefusal;
use slates_svn::dav::commit::DavCommit;
use slates_svn::index::Index as SvnIndex;
use slates_svn::migration::{Repository as SvnRecords, Room as SvnRoom};
use slates_vcs::{Admitted, ContentRefused, ContentStore, Derivations, Mode};
use slates_vfs::volume::Store;

use crate::state::ShardState;

pub(crate) mod commits;
pub(crate) mod git_encode;
pub(crate) mod hg_import;
pub(crate) mod hg_made;
pub(crate) mod hg_view;
pub(crate) mod homes;
pub(crate) mod lfs_record;
pub(crate) mod owed;
pub(crate) mod paths;
pub(crate) mod record;
pub(crate) mod svn_import;
pub(crate) mod svn_made;
pub(crate) mod svn_view;
// Its push helpers serve the hooks' tests too (`crate::git_hooks`, A-665 part 2).
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod upload;

use lfs_record::LfsRecordRefused;
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

/// Counter: an HTTP commit's transaction released idle past the edge's learned idle bound (A-681).
const DAV_COMMITS_RELEASED: &str = "vcs_store.dav_commits_released";

/// An HTTP commit kept between its requests: who made it, the monotonic instant of its last step, and its drive.
#[derive(Debug)]
pub(crate) struct DavHeld {
  /// The key of the principal that made it.
  principal: Vec<u8>,
  /// The monotonic instant of its last step.
  last_ns: u64,
  /// Its drive.
  commit: DavCommit,
}

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
  /// The last generation a Mercurial store's index was given ([`HgStore::generation`]); counts up.
  generations: u64,
  /// The Mercurial repositories whose stores are being made again from their records a step at a time ([`rebuild`],
  /// A-732): not lent to a read, a push or a made changeset until they are whole; bounded by the partition's
  /// repositories.
  rebuilding: std::collections::BTreeSet<RepositoryId>,
  /// The git and Subversion repositories whose stores are being made again from their records a record a turn
  /// ([`rebuild`], A-732's sibling): each with its rebuild's generation and the next record to replay. Their stores are
  /// lent to nothing ([`VcsStores::git`], [`VcsStores::subversion`] and their kin answer none) until the last record is
  /// replayed; bounded by the partition's repositories.
  rebuild_turns: BTreeMap<RepositoryId, (u64, usize)>,
  /// The last rebuild's generation: a rebuild started again ends the turns of the one before.
  rebuild_generations: u64,
  /// What each repository's entries' objects, which the partition's register keeps (A-662), hold of the shard's budget,
  /// charged as retention ([`settle_objects`]): bounded by the partition's repositories.
  objects_charged: BTreeMap<RepositoryId, u64>,
  /// Each Mercurial repository's import under way, between its turns ([`hg_import`]): one a repository, so bounded by
  /// the partition's repositories.
  imports: BTreeMap<RepositoryId, hg_import::Importing>,
  /// Each Subversion repository's import of a dump under way, between its turns ([`svn_import`], A-652 part 3): one a
  /// repository, so bounded by the partition's repositories.
  svn_imports: BTreeMap<RepositoryId, svn_import::Importing>,
  /// The request each import's end answers, by repository (A-678; `crate::repository::import`): one an import, so
  /// bounded as the imports are.
  answers: BTreeMap<RepositoryId, crate::repository::import::Answer>,
  /// Each git repository's version being encoded as its fragments, between its turns ([`git_encode`], A-718): one a
  /// repository, so bounded by the partition's repositories.
  encodings: git_encode::Encodings,
  /// Each git green's whole map built in slices after a push, between its slices ([`paths::whole_build`], A-664's live
  /// whole builds): one a green, so bounded by the partition's greens.
  whole_builds: paths::whole_build::Builds,
  /// Each HTTP commit under way (A-681 part C: a transaction a POST made, between its requests), by repository and
  /// transaction name, with the key of the principal that made it and the monotonic instant of its last step: at most
  /// one a repository and principal, a new POST replacing the older ([`VcsStores::begin_dav_commit`]), so bounded by the
  /// partition's repositories and principals; and one idle past the edge's learned idle bound is released at the
  /// table's next use ([`VcsStores::release_idle_dav_commits`]).
  dav_commits: BTreeMap<(RepositoryId, String), DavHeld>,
  /// The HTTP transactions this shard has named, which a transaction's name counts.
  dav_named: u64,
}

/// One repository's store, by its tool (module docs).
#[derive(Debug)]
pub(crate) enum RepositoryStore {
  /// git's. Each arm is boxed: their stores differ in size by hundreds of bytes.
  Git(Box<GitStore>),
  /// Mercurial's.
  Mercurial(Box<HgStore>),
  /// Subversion's.
  Subversion(Box<SvnStore>),
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
  /// The path map's type over the repository's greens (A-664 part 3, [`paths`]): what each green's map of its head in
  /// `heads` is made and read by. Declared with the content store's window when the store is made ([`made`]); `None`
  /// for a window too small for the map's branches, which no content plane's window is.
  pub(crate) paths: Option<paths::GreenPaths>,
  /// The git trees' type over the repository's greens (A-664 part 3, [`paths`]): what each head's trees in `heads` are
  /// made and read by, declared beside `paths` in the repository's format; `None` as `paths` is.
  pub(crate) trees: Option<paths::GreenTrees>,
  /// The commits' type the server makes its commits by (A-664 part 3, [`commits`]), declared beside `paths`; `None` as
  /// `paths` is.
  pub(crate) commits: Option<commits::GreenCommits>,
  /// Each green's map and git trees of its head, by the green's key (A-664 part 3, [`paths`]): one a member green that
  /// committed a version, its two roots held once by the store, replaced as each version commits, given back as the
  /// green leaves its lineage and made again by a recovery.
  pub(crate) heads: BTreeMap<u64, paths::Head>,
  /// For each green's key, the green's version at each of the store's versions of it, in order: what a push's old
  /// value is read back to (`version_of`), as each record names it.
  versions: BTreeMap<u64, Vec<u64>>,
  /// What `versions` holds of the content store's budget (`ContentStore::charge`), charged before a commit keeps a
  /// version and given back with the store ([`versions_cost`]).
  charged: u64,
  /// The repository's Git LFS objects (A-629 part 3): each upload's bytes in the content store under its oid, one row
  /// of an index apart from the git objects', put there by its entry's append ([`admit_lfs`]) and by a recovery's
  /// replay of that entry's record ([`lfs_record`]), and given back with the store.
  pub(crate) lfs: LfsObjects,
  /// The commits owed by submits whose maps are built whole after them (A-664's live whole builds, part 2, [`owed`]),
  /// by the green's key: one a green, each settled as the made commit's move of its green to its version commits.
  pub(crate) owed: BTreeMap<u64, owed::Owed>,
}

impl GitStore {
  /// An empty store of `format`, its values' trees shaped by `window`, its content store's (A-647 part 4).
  fn new(format: ObjectFormat, window: usize) -> GitStore {
    GitStore {
      repository: Repository::new(format),
      derivations: Derivations::new(),
      reach: ReachSets::declare(window, Mode::Eager),
      paths: None,
      trees: None,
      commits: None,
      heads: BTreeMap::new(),
      versions: BTreeMap::new(),
      charged: 0,
      lfs: LfsObjects::new(),
      owed: BTreeMap::new(),
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
  /// The changeset the server made of each green's newest made version (A-731, `hg_made`): by the green's key, the
  /// version and the changeset; a green's newest only, so bounded by the repository's greens, and made again by a
  /// recovery from the made records.
  pub(crate) made: BTreeMap<u64, (u64, slates_hg::Node)>,
  /// The index's generation (A-720): a number of the shard's, taken afresh whenever the store is made or its index
  /// replaced, so a push's fork made from one index is known stale once another landed ([`VcsStores::generation_of`]).
  pub(crate) generation: u64,
  /// Its derived values.
  pub(crate) derivations: Derivations,
}

/// A Subversion repository's store: its records, beside its one green (A-655 part 2b; module docs, "Subversion").
#[derive(Debug, Default)]
pub(crate) struct SvnStore {
  /// The records: none until the creation's record is replayed.
  pub(crate) records: Option<SvnRecords>,
  /// What the records hold of the shard's budget, charged as retention.
  pub(crate) charged: u64,
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
  /// A Subversion record's bytes (A-655 part 2b).
  SvnRecord(SvnRecordRefused),
  /// A Subversion repository's records not made from its creation's record: past the room (A-655 part 2b).
  SvnCreated(slates_svn::migration::MigrationRefusal),
  /// A Subversion revision the commit step refused again (A-655 part 2b).
  SvnStep(StepRefusal),
  /// A Subversion record out of its order: a creation once the records are made, or a revision before them.
  SvnOrder,
  /// A Mercurial import's record refused by its replay (A-687): its bytes, a text not its node's, or the import's own
  /// step.
  Mercurial(slates_hg::import::record::Refused),
  /// The index a replayed one took the place of was not given back whole: the content store's count of holds is off.
  Released,
  /// An LFS object's record's bytes (A-629 part 3).
  LfsRecord(LfsRecordRefused),
  /// An LFS object the repository's LFS index refused again: bytes that are not their oid's, or past the budget.
  Lfs(ObjectRefused),
  /// A version-2 git record's pack (A-723) that git's reader refuses: its bytes, or a base outside it the store lacks.
  Pack(slates_git::unpack::Refusal),
  /// An owed commit's record's bytes (A-664's live whole builds, part 2).
  Owed(owed::OwedRefused),
}

/// The repository's objects as the bases a replayed pack's reference deltas may name (A-723), read through the index view
/// a live push reads them by.
struct StoreBases<'a>(slates_git::store::views::IndexedObjects<'a>);

impl slates_git::unpack::Outside for StoreBases<'_> {
  fn object(&self, id: &ObjectId) -> Option<(slates_git::object_name::Kind, &[u8])> {
    slates_git::receive::Repository::object(&self.0, id)
  }
}

/// No pack index: a replayed pack names its own objects and the store's (A-723).
static NO_INDEX: slates_git::unpack::NoIndex = slates_git::unpack::NoIndex;

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
/// store cannot read, as its own refusal names it; a pack not made or a room refused, the record's room.
fn unwritten(refused: WriteRefused) -> GitRefused {
  match refused {
    WriteRefused::Unreadable { id } => GitRefused::Store(StoreRefused::Unreadable { id }),
    WriteRefused::Whole(_) | WriteRefused::Allocation => GitRefused::Allocation,
  }
}

/// A content store's refusal, in a push's terms.
fn content_refused(refused: ContentRefused) -> GitRefused {
  GitRefused::Store(StoreRefused::Content(refused))
}

impl VcsStores {
  /// The shard's next generation for a Mercurial store's index (A-720).
  fn next_generation(&mut self) -> u64 {
    self.generations = self.generations.saturating_add(1);
    self.generations
  }

  /// Repository `id`'s Mercurial index's generation, while its store is held here (A-720).
  pub(crate) fn generation_of(&self, id: RepositoryId) -> Option<u64> {
    match self.entries.get(&id) {
      Some(RepositoryStore::Mercurial(store)) => Some(store.generation),
      _ => None,
    }
  }

  /// Notes that repository `id`'s Mercurial index was replaced: a fork made from the one before is stale (A-720).
  pub(crate) fn index_changed(&mut self, id: RepositoryId) {
    let generation = self.next_generation();
    if let Some(RepositoryStore::Mercurial(store)) = self.entries.get_mut(&id) {
      store.generation = generation;
    }
  }

  /// Whether repository `id`'s store is held here.
  pub(crate) fn holds(&self, id: RepositoryId) -> bool {
    self.entries.contains_key(&id)
  }

  /// Whether an import of repository `id` is under way ([`hg_import`], [`svn_import`]).
  pub(crate) fn importing(&self, id: RepositoryId) -> bool {
    self.imports.contains_key(&id) || self.svn_imports.contains_key(&id)
  }

  /// Whether repository `id`'s Mercurial store is being made again from its records (A-732), and so lent to nothing.
  pub(crate) fn rebuilding(&self, id: RepositoryId) -> bool {
    self.rebuilding.contains(&id)
  }

  /// Holds `answer` as the request repository `id`'s import's end answers (A-678): the answer it replaces.
  pub(crate) fn hold_answer(
    &mut self,
    id: RepositoryId,
    answer: crate::repository::import::Answer,
  ) -> Option<crate::repository::import::Answer> {
    self.answers.insert(id, answer)
  }

  /// The request repository `id`'s import's end answers, if one is held.
  pub(crate) fn answer(&self, id: RepositoryId) -> Option<&crate::repository::import::Answer> {
    self.answers.get(&id)
  }

  /// Takes the request repository `id`'s import's end answers, if one is held.
  pub(crate) fn take_answer(
    &mut self,
    id: RepositoryId,
  ) -> Option<crate::repository::import::Answer> {
    self.answers.remove(&id)
  }

  /// Why repository `id`'s store was not made again by the recovery, if it was not.
  pub(crate) fn unrecovered(&self, id: RepositoryId) -> Option<&ReplayRefused> {
    self.unrecovered.get(&id)
  }

  /// The content store, once a repository made it.
  pub(crate) fn content(&self) -> Option<&ContentStore> {
    self.content.as_ref()
  }

  /// The content store, lent for one step that gives back what something beside a repository's
  /// store holds in it: a hook's view, whose holds outlive no repository entry
  /// (`crate::git_hooks`, A-665 part 2).
  pub(crate) fn content_mut(&mut self) -> Option<&mut ContentStore> {
    self.content.as_mut()
  }

  /// What the stores hold of the shard's budget.
  pub(crate) fn charged(&self) -> u64 {
    self.charged
  }

  /// Repository `id`'s store, if held.
  pub(crate) fn store(&self, id: RepositoryId) -> Option<&RepositoryStore> {
    if self.rebuild_turns.contains_key(&id) {
      return None;
    }
    self.entries.get(&id)
  }

  /// Repository `id`'s git store with the content store, lent for one step.
  pub(crate) fn git(&mut self, id: RepositoryId) -> Option<(&mut ContentStore, &mut GitStore)> {
    if self.rebuild_turns.contains_key(&id) {
      return None;
    }
    let content = self.content.as_mut()?;
    match self.entries.get_mut(&id)? {
      RepositoryStore::Git(store) => Some((content, store)),
      _ => None,
    }
  }

  /// Repository `id`'s git store with the content store, read.
  pub(crate) fn git_ref(&self, id: RepositoryId) -> Option<(&ContentStore, &GitStore)> {
    if self.rebuild_turns.contains_key(&id) {
      return None;
    }
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

  /// Repository `id`'s Mercurial store with the content store, read (A-680 part 2: the served view lends from it).
  pub(crate) fn mercurial_ref(&self, id: RepositoryId) -> Option<(&ContentStore, &HgStore)> {
    let content = self.content.as_ref()?;
    match self.entries.get(&id)? {
      RepositoryStore::Mercurial(store) => Some((content, store)),
      _ => None,
    }
  }

  /// Repository `id`'s Subversion store, lent for one step.
  pub(crate) fn subversion(&mut self, id: RepositoryId) -> Option<&mut SvnStore> {
    if self.rebuild_turns.contains_key(&id) {
      return None;
    }
    self.svn_unlent(id)
  }

  /// [`VcsStores::subversion`] whether or not the store is lent: a rebuild's own replay reaches it this way.
  fn svn_unlent(&mut self, id: RepositoryId) -> Option<&mut SvnStore> {
    match self.entries.get_mut(&id)? {
      RepositoryStore::Subversion(store) => Some(store),
      _ => None,
    }
  }

  /// Repository `id`'s Subversion records, read; none before its creation's record is replayed.
  /// A fresh HTTP transaction's name in repository `id` (§4.17.4 item 3.1: `svnserve`'s `REV-SEQ`): the youngest
  /// revision it is based on, then this shard's count of the transactions it has named.
  pub(crate) fn dav_transaction_name(&mut self, youngest: u64) -> String {
    self.dav_named = self.dav_named.saturating_add(1);
    format!("{youngest}-{}", self.dav_named)
  }

  /// Keeps `commit`, named `name`, for repository `id` and the principal keyed `principal`, dropping any other the
  /// same principal left open in it (the field's bound).
  pub(crate) fn begin_dav_commit(
    &mut self,
    id: RepositoryId,
    principal: &[u8],
    name: String,
    (commit, now_ns): (DavCommit, u64),
  ) {
    self.release_idle_dav_commits(now_ns, crate::vcs_http::idle_bound_ns());
    self
      .dav_commits
      .retain(|(repository, _), held| !(*repository == id && held.principal == principal));
    self.dav_commits.insert(
      (id, name),
      DavHeld {
        principal: principal.to_vec(),
        last_ns: now_ns,
        commit,
      },
    );
  }

  /// Releases every HTTP commit idle past `idle_ns` at `now_ns`, the edge's learned idle bound (§4.17.2's reaping, a
  /// client's silence between requests): its client has lost the connection it committed over, as the edge judges a
  /// silence that long, and nothing it made has landed. With no bound learned yet, none is released. Each release is
  /// counted ([`DAV_COMMITS_RELEASED`]).
  pub(crate) fn release_idle_dav_commits(&mut self, now_ns: u64, idle_ns: Option<u64>) {
    let Some(idle_ns) = idle_ns else {
      return;
    };
    let before = self.dav_commits.len();
    self
      .dav_commits
      .retain(|_, held| now_ns.saturating_sub(held.last_ns) <= idle_ns);
    for _ in self.dav_commits.len()..before {
      crate::fleet::count_refusal(DAV_COMMITS_RELEASED);
    }
  }

  /// Takes out repository `id`'s transaction `name`, made by the principal keyed `principal`; none for another's.
  pub(crate) fn take_dav_commit(
    &mut self,
    id: RepositoryId,
    principal: &[u8],
    (name, now_ns): (&str, u64),
  ) -> Option<DavCommit> {
    self.release_idle_dav_commits(now_ns, crate::vcs_http::idle_bound_ns());
    let key = (id, name.to_owned());
    if self
      .dav_commits
      .get(&key)
      .is_none_or(|held| held.principal != principal)
    {
      return None;
    }
    self.dav_commits.remove(&key).map(|held| held.commit)
  }

  /// Puts back what [`VcsStores::take_dav_commit`] took, unless it has ended.
  pub(crate) fn keep_dav_commit(
    &mut self,
    id: RepositoryId,
    principal: &[u8],
    name: &str,
    (commit, now_ns): (DavCommit, u64),
  ) {
    if !commit.over() {
      self.dav_commits.insert(
        (id, name.to_owned()),
        DavHeld {
          principal: principal.to_vec(),
          last_ns: now_ns,
          commit,
        },
      );
    }
  }

  pub(crate) fn subversion_records(&self, id: RepositoryId) -> Option<&SvnRecords> {
    if self.rebuild_turns.contains_key(&id) {
      return None;
    }
    match self.entries.get(&id)? {
      RepositoryStore::Subversion(store) => store.records.as_ref(),
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

/// Makes `record`'s store on its owner shard, beside its record (module docs): empty, so it charges nothing yet but
/// the objects its entries hold in the partition's register, which a recovery finds there ([`settle_objects`]). A store
/// held already is kept.
pub(crate) fn make(state: &mut ShardState, record: &RepositoryRecord) {
  let window = state.store.content.chunk_bytes();
  if made(&mut state.vcs_stores, record, window).is_err() {
    state.count(STORE_UNMADE, 1);
  }
  settle_objects(state, record.id);
}

/// [`make`]'s step over the stores, the content store made at the first repository with the content plane's `window`.
fn made(stores: &mut VcsStores, record: &RepositoryRecord, window: usize) -> Result<(), Unmade> {
  if stores.content.is_none() {
    let content = ContentStore::new(stores.charged, window).map_err(|_| Unmade)?;
    stores.content = Some(content);
  }
  let window = stores.content.as_ref().map_or(window, ContentStore::window);
  let store = match record.tool {
    VcsTool::Git => {
      let format = git_format(record.format).ok_or(Unmade)?;
      let mut git = GitStore::new(format, window);
      // The map, the trees and the commits split by the content store's own window, which their types' names carry
      // (A-647, A-664 part 2).
      git.paths = paths::declare(window);
      git.trees = paths::declare_trees(format, window);
      git.commits = Some(commits::declare(format, window));
      RepositoryStore::Git(Box::new(git))
    }
    VcsTool::Mercurial => RepositoryStore::Mercurial(Box::new(HgStore {
      generation: stores.next_generation(),
      ..HgStore::default()
    })),
    VcsTool::Subversion => RepositoryStore::Subversion(Box::default()),
  };
  stores.entries.entry(record.id).or_insert(store);
  Ok(())
}

/// A store not made ([`made`]).
struct Unmade;

/// Frees repository `id`'s store with the repository (module docs): an import of it under way given back first
/// ([`hg_import::abandon`], [`svn_import::abandon`]), then every hold of its rows, tables, versions and derived values
/// given back to the content store, then the shard's charge trued up to what remains. A refusal while giving back is
/// counted, and the rest is given back.
pub(crate) fn free(state: &mut ShardState, id: RepositoryId) {
  // A git or Subversion rebuild under way ends with its store: its task finds its turns gone and stops.
  if state.vcs_stores.rebuild_turns.remove(&id).is_some() {
    state.vcs_stores.rebuilding.remove(&id);
  }
  paths::whole_build::abandon(state, id);
  hg_import::abandon(state, id);
  git_encode::abandon(state, id);
  svn_import::abandon(state, id);
  if let Some(charged) = state.vcs_stores.objects_charged.remove(&id) {
    state.store.budget.credit_retention(charged);
  }
  state.vcs_stores.unrecovered.remove(&id);
  state.vcs_usage.forget(&id);
  if !release_charged(state, id) {
    state.count(RELEASE_REFUSED, 1);
  }
  settle(state);
}

/// Trues the shard's charge for repository `id`'s entries' objects up to what the partition's register holds of them
/// (A-662; `Repositories::objects_bytes`): the repository's content is charged to its owner shard's memory budget, as
/// its tool store is, so a repository's bound is what its shard can hold, never the partition's merge budget. A charge
/// the budget would not take is counted ([`CHARGE_SHORT`]) and kept as charged, as the store's own true-up keeps it.
pub(crate) fn settle_objects(state: &mut ShardState, id: RepositoryId) {
  let held =
    u64::try_from(state.db.partition().repositories().objects_bytes(id)).unwrap_or(u64::MAX);
  let charged = state
    .vcs_stores
    .objects_charged
    .get(&id)
    .copied()
    .unwrap_or(0);
  if held > charged {
    if state
      .store
      .budget
      .charge_retention(held.saturating_sub(charged))
      .is_err()
    {
      state.count(CHARGE_SHORT, 1);
    }
  } else {
    state
      .store
      .budget
      .credit_retention(charged.saturating_sub(held));
  }
  if held == 0 {
    state.vcs_stores.objects_charged.remove(&id);
  } else {
    state.vcs_stores.objects_charged.insert(id, held);
  }
}

impl VcsStores {
  /// What repository `id`'s entries' objects hold of the shard's budget ([`settle_objects`]).
  pub(crate) fn objects_charged(&self, id: RepositoryId) -> u64 {
    self.objects_charged.get(&id).copied().unwrap_or(0)
  }
}

/// Whether the shard's budget could admit `bytes` more of a repository's entries' objects now ([`settle_objects`]):
/// what an append asks before its entry is judged; the bytes it could admit otherwise.
pub(crate) fn admit_objects(state: &ShardState, bytes: usize) -> Result<(), u64> {
  let room = state.store.admittable();
  if u64::try_from(bytes).unwrap_or(u64::MAX) > room {
    return Err(room);
  }
  Ok(())
}

/// [`release`], the Subversion records' charge given back to the shard's budget first (A-655 part 2b).
fn release_charged(state: &mut ShardState, id: RepositoryId) -> bool {
  if let Some(store) = state.vcs_stores.subversion(id) {
    let charged = core::mem::take(&mut store.charged);
    state.store.budget.credit_retention(charged);
  }
  release(&mut state.vcs_stores, id)
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
      // Every head's map is released, whichever fails: no short circuit.
      let mut maps = true;
      for head in store.heads.values() {
        maps &= paths::release_head(content, head);
      }
      let lfs = store.lfs.release(content).is_ok();
      store.repository.release(content).is_ok() && values && versions && maps && lfs
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

/// The room a Subversion repository's records may grow into now: what they hold, charged, and every byte the shard's
/// budget could admit; their index's entries those it holds and as many more as those bytes could hold at the fewest
/// an entry takes (`Index::ENTRY_BYTES`, module docs).
fn svn_room(store: &Store, charged: u64, entries: usize) -> SvnRoom {
  let room = store.admittable();
  let more = usize::try_from(room)
    .unwrap_or(usize::MAX)
    .checked_div(SvnIndex::ENTRY_BYTES)
    .unwrap_or(0);
  SvnRoom {
    entries: entries.saturating_add(more),
    bytes: charged.saturating_add(room),
  }
}

/// Lends repository `id`'s Subversion records room for one step that brings `bytes` (A-655 part 2b; module docs,
/// "Subversion"): the arena grown by them from its pool where it can be, then the room [`svn_room`] derives. Whether
/// its records are held here.
pub(crate) fn lend_svn(state: &mut ShardState, id: RepositoryId, bytes: u64) -> bool {
  let ShardState {
    store, vcs_stores, ..
  } = &mut *state;
  let Some(svn) = vcs_stores.subversion(id) else {
    return false;
  };
  let Some(records) = svn.records.as_mut() else {
    return false;
  };
  store.make_room(bytes);
  records.set_room(svn_room(store, svn.charged, records.index().entries()));
  true
}

/// Charges `bytes` repository `id`'s Subversion records are about to hold to the shard's budget, before the step that
/// grows them appends (A-655 part 2b), so its append is lent only what remains; refused with the bytes the budget could
/// admit, nothing charged.
pub(crate) fn reserve_svn(state: &mut ShardState, id: RepositoryId, bytes: u64) -> Result<(), u64> {
  let ShardState {
    store, vcs_stores, ..
  } = &mut *state;
  let Some(svn) = vcs_stores.subversion(id) else {
    return Err(0);
  };
  if store.budget.charge_retention(bytes).is_err() {
    return Err(store.admittable());
  }
  svn.charged = svn.charged.saturating_add(bytes);
  Ok(())
}

/// Gives back a reservation ([`reserve_svn`]) whose step was refused before its records grew.
pub(crate) fn unreserve_svn(state: &mut ShardState, id: RepositoryId, bytes: u64) {
  let ShardState {
    store, vcs_stores, ..
  } = &mut *state;
  if let Some(svn) = vcs_stores.subversion(id) {
    let given = bytes.min(svn.charged);
    svn.charged = svn.charged.saturating_sub(given);
    store.budget.credit_retention(given);
  }
}

/// Trues the shard's charge up to what repository `id`'s Subversion records hold after a step, and sets their room back
/// to what they hold (A-655 part 2b), counting a charge the budget would not take ([`CHARGE_SHORT`]).
pub(crate) fn settle_svn(state: &mut ShardState, id: RepositoryId) {
  let ShardState {
    store, vcs_stores, ..
  } = &mut *state;
  let whole = vcs_stores
    .subversion(id)
    .is_none_or(|svn| settle_records(store, svn));
  if !whole {
    state.count(CHARGE_SHORT, 1);
  }
}

/// [`settle_svn`]'s step over one store: whether the shard took every byte the records hold.
fn settle_records(store: &mut Store, svn: &mut SvnStore) -> bool {
  let Some(records) = svn.records.as_mut() else {
    return true;
  };
  let held = records.held();
  let mut whole = true;
  if held > svn.charged {
    if store
      .budget
      .charge_retention(held.saturating_sub(svn.charged))
      .is_ok()
    {
      svn.charged = held;
    } else {
      whole = false;
    }
  } else {
    store
      .budget
      .credit_retention(svn.charged.saturating_sub(held));
    svn.charged = held;
  }
  records.set_room(SvnRoom {
    entries: records.index().entries(),
    bytes: svn.charged,
  });
  whole
}

/// Runs `step` over the stores with the room [`lend`] gives for `bytes`, then settles: a push's
/// steps here, and a hook's view opened and sealed over the store (`crate::git_hooks`, A-665
/// part 2).
pub(crate) fn within_budget<T>(
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
  set_back(stores);
  answer
}

/// [`lent_to_read`] over the whole shard's state: what a step needs that reads the stores beside the shard's greens and
/// records, as git's push judges each command over the greens, its repository's store and its quarantine at once
/// (A-654). Read only, so the room lent is given back and nothing is charged.
pub(crate) fn with_room<T>(state: &mut ShardState, read: impl FnOnce(&ShardState) -> T) -> T {
  lend(&mut state.store, &mut state.vcs_stores, 0);
  let answer = read(state);
  set_back(&mut state.vcs_stores);
  answer
}

/// Sets the content store's budget back to what is charged after a read lent room ([`lent_to_read`], [`with_room`]).
fn set_back(stores: &mut VcsStores) {
  let charged = stores.charged;
  if let Some(content) = stores.content.as_mut() {
    content.set_budget(charged);
  }
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

  /// The objects it holds in quarantine, read only: what a ref's work is read from beside the repository's index
  /// (A-638's `PushedObjects`), and how a move changes its ref (A-644's `Repository::change`), as git's push judges
  /// each command (A-654). Also what a hook's view shows of the push (`crate::git_hooks`, A-665
  /// part 2): in git's quarantine for `pre-receive`, among the repository's packs for `update`.
  pub(crate) fn quarantine(&self) -> &Push {
    &self.push
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

  /// The objects admitted so far, in quarantine, read only: what a pack being read reads a delta's base back from and a
  /// push's checks read its objects from (A-719 part 3).
  pub(crate) fn quarantine(&self) -> &Push {
    &self.push
  }

  /// Keeps the push's pack as the client sent it (A-723), which the push's first record holds in place of its objects'
  /// bytes.
  pub(crate) fn keep_pack(&mut self, pack: Vec<u8>) {
    self.record.keep_pack(pack);
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
/// `writer` noted that the repository lacks now, named by type, content identity and length, and the push's pack as the
/// client sent it (A-723). Lacking is the predicate the
/// store's commit gives positions by (`Repository::object`, the index its `fresh` reads), so the record holds exactly
/// the objects the commit that follows positions.
pub(crate) fn record_of(
  content: &ContentStore,
  store: &GitStore,
  (writer, deepest): (&RecordWriter, u64),
  moves: &[RecordedMove],
) -> Result<Vec<u8>, GitRefused> {
  writer
    .finish(
      (content, store.repository.format()),
      lacks(content, store),
      (deepest, moves),
    )
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
    // A made commit's move of its green to the version a commit was owed of settles it ([`owed`]).
    if store
      .owed
      .get(key)
      .is_some_and(|owed| owed.version <= *version)
    {
      store.owed.remove(key);
    }
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
  // An owed commit's record (A-664's live whole builds, part 2): owed again, unless a made commit settled it already.
  if owed::is_owed(bytes) {
    let owed = owed::Owed::decode(bytes).map_err(ReplayRefused::Owed)?;
    let settled = store
      .versions(owed.key)
      .last()
      .is_some_and(|made| *made >= owed.version);
    if !settled {
      store.owed.insert(owed.key, owed);
    }
    return Ok(0);
  }
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
  if record.objects.is_empty() {
    return admitted.eq(record.names.iter().copied());
  }
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
  if !record.pack.is_empty() {
    return replay_pack(content, store, (push, remade), record);
  }
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

/// Admits a version-2 record's objects (A-723): its pack read through git's own reader, a reference delta's base from the
/// pack or the store, within the room the content store has left; then each object admitted as a live push admits it,
/// at its place in its chain. The names the record holds are compared after ([`recorded_alike`]), so a pack that makes
/// other objects than the live push recorded is refused.
fn replay_pack(
  content: &mut ContentStore,
  store: &GitStore,
  (push, remade): (&mut Push, &mut RecordWriter),
  record: &Record<'_>,
) -> Result<(), ReplayRefused> {
  let unpacked = {
    let room = content.room();
    let bases = StoreBases(slates_git::store::views::IndexedObjects::new(
      &store.repository,
      content,
      room,
    ));
    let mut admission = slates_git::Admission::new(room);
    let mut reader = slates_git::unpack::Reader::new(store.repository.format(), &NO_INDEX);
    reader
      .read(record.pack, &mut admission)
      .map_err(ReplayRefused::Pack)?;
    reader
      .finish_with(&bases, &mut admission)
      .map_err(ReplayRefused::Pack)?
  };
  for object in &unpacked.objects {
    let incoming = Incoming {
      kind: object.kind,
      claimed: Some(object.id),
      content: &object.content,
      depth: object.chain.as_ref().map_or(0, |chain| chain.depth),
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
  let brought = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  lend(store, vcs_stores, brought);
  let room = vcs_stores.svn_unlent(id).map(|svn| {
    let entries = svn
      .records
      .as_ref()
      .map_or(0, |records| records.index().entries());
    svn_room(store, svn.charged, entries)
  });
  let replayed = replay_into(vcs_stores, id, bytes, room);
  let mut whole = settle_in(store, vcs_stores);
  if let Some(svn) = vcs_stores.svn_unlent(id) {
    whole &= settle_records(store, svn);
  }
  if !whole {
    state.count(CHARGE_SHORT, 1);
  }
  Some(replayed.map(|unkept| state.count(VALUE_UNKEPT, unkept)))
}

/// Replays repository `id`'s Mercurial record at `index` a unit at a time, a unit of `slices` each (A-732; the
/// record's `Replay`): its bytes read from the register afresh at each step, the room its bytes take lent once and the
/// shard's charge settled after each step; none when it has no record there.
async fn replay_hg_sliced(
  state: &mut ShardState,
  id: RepositoryId,
  index: usize,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Option<Result<(), ReplayRefused>> {
  let begun = hg_record_step(state, id, index, None)?;
  let mut replay = match begun {
    Ok(HgStep::Begun(replay)) => replay,
    Ok(HgStep::Stepped(..)) => return Some(Ok(())),
    Err(refused) => return Some(Err(refused)),
  };
  loop {
    match hg_record_step(state, id, index, Some(replay))? {
      Ok(HgStep::Stepped(Some(more))) => {
        replay = more;
        slices.unit_done().await;
      }
      Ok(HgStep::Stepped(None) | HgStep::Begun(_)) => return Some(Ok(())),
      Err(refused) => return Some(Err(refused)),
    }
  }
}

/// One step of a Mercurial record's sliced replay: begun, or advanced (the replay back while more follow, none once it
/// finished and the fork took the index's place); a refusal gives the fork back.
enum HgStep {
  /// The replay begun.
  Begun(slates_hg::import::record::Replay),
  /// A unit replayed: the replay while more follow.
  Stepped(Option<slates_hg::import::record::Replay>),
}

/// [`replay_hg_sliced`]'s one step over the shard's state (its type's docs); none when the register holds no record at
/// `index`.
fn hg_record_step(
  state: &mut ShardState,
  id: RepositoryId,
  index: usize,
  replay: Option<slates_hg::import::record::Replay>,
) -> Option<Result<HgStep, ReplayRefused>> {
  let ShardState {
    db,
    store,
    vcs_stores,
    ..
  } = &mut *state;
  let bytes = db.partition().repositories().object_record(id, index)?;
  let lent = if replay.is_none() {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
  } else {
    0
  };
  lend(store, vcs_stores, lent);
  let stepped = hg_step_in(vcs_stores, id, bytes, replay);
  let whole = settle_in(store, vcs_stores);
  if !whole {
    state.count(CHARGE_SHORT, 1);
  }
  Some(stepped)
}

/// [`hg_record_step`] over the stores.
fn hg_step_in(
  stores: &mut VcsStores,
  id: RepositoryId,
  bytes: &[u8],
  replay: Option<slates_hg::import::record::Replay>,
) -> Result<HgStep, ReplayRefused> {
  let unheld = ReplayRefused::Git(GitRefused::NoStore);
  let content = stores.content.as_mut().ok_or(unheld.clone())?;
  let Some(RepositoryStore::Mercurial(store)) = stores.entries.get_mut(&id) else {
    return Err(unheld);
  };
  let Some(mut replay) = replay else {
    return slates_hg::import::record::Replay::begin(bytes, &mut store.index, content)
      .map(HgStep::Begun)
      .map_err(ReplayRefused::Mercurial);
  };
  match replay.step(bytes, content) {
    Ok(true) => Ok(HgStep::Stepped(Some(replay))),
    Ok(false) => {
      let replayed = replay
        .finish(&mut store.index, content)
        .map_err(ReplayRefused::Mercurial)?;
      replayed_hg(store, &replayed).map(|_| HgStep::Stepped(None))
    }
    Err(refused) => {
      if replay.abandon(content).is_err() {
        return Err(ReplayRefused::Released);
      }
      Err(ReplayRefused::Mercurial(refused))
    }
  }
}

/// Replays `bytes` into repository `id`'s store, by its tool, a Subversion record within `svn_room`: the derived values
/// not kept.
fn replay_into(
  stores: &mut VcsStores,
  id: RepositoryId,
  bytes: &[u8],
  svn_room: Option<SvnRoom>,
) -> Result<u64, ReplayRefused> {
  let unheld = ReplayRefused::Git(GitRefused::NoStore);
  let content = stores.content.as_mut().ok_or(unheld.clone())?;
  match stores.entries.get_mut(&id) {
    Some(RepositoryStore::Git(store)) if lfs_record::is_lfs(bytes) => {
      replay_lfs(content, store, bytes).map(|()| 0)
    }
    Some(RepositoryStore::Git(store)) => replay_git(content, store, bytes),
    Some(RepositoryStore::Subversion(store)) => {
      replay_svn(store, bytes, svn_room.ok_or(unheld)?).map(|()| 0)
    }
    Some(RepositoryStore::Mercurial(store)) => replay_hg(content, store, bytes),
    None => Err(unheld),
  }
}

/// Replays a Mercurial record into `store` (A-687; module docs): its revisions appended to the store's index through
/// the import's own steps, in a fork that takes the index's place once the whole record is in
/// (`slates_hg::import::record::replay`, the one entry point for an import's record and a push's); an import's record
/// replays only into an empty index, as the live landing takes only an empty store. Refused, the store is as it was;
/// the index replaced not given back whole is refused too, the content store's count of holds being off. No derived
/// value is kept with it.
fn replay_hg(
  content: &mut ContentStore,
  store: &mut HgStore,
  bytes: &[u8],
) -> Result<u64, ReplayRefused> {
  let replayed = slates_hg::import::record::replay(bytes, &mut store.index, content)
    .map_err(ReplayRefused::Mercurial)?;
  replayed_hg(store, &replayed)
}

/// What a replayed Mercurial record leaves beside the index: a made changeset's node noted from the record alone
/// (A-731); refused where the index replaced was not given back whole.
fn replayed_hg(
  store: &mut HgStore,
  replayed: &slates_hg::import::record::Replayed,
) -> Result<u64, ReplayRefused> {
  if let Some((green, version, node)) = replayed.made {
    let newer = store
      .made
      .get(&green)
      .is_none_or(|(held, _)| *held < version);
    if newer {
      store.made.insert(green, (version, node));
    }
  }
  if replayed.whole {
    Ok(0)
  } else {
    Err(ReplayRefused::Released)
  }
}

/// Replays one LFS object's record (A-629 part 3; [`lfs_record`]) into the repository's LFS index through the path its
/// upload took: the bytes checked against the oid before they are kept. An object the index holds already is kept as
/// it is, so a record replayed twice holds one object.
fn replay_lfs(
  content: &mut ContentStore,
  store: &mut GitStore,
  bytes: &[u8],
) -> Result<(), ReplayRefused> {
  let record = lfs_record::decode(bytes).map_err(ReplayRefused::LfsRecord)?;
  store
    .lfs
    .admit(content, &record.oid, record.bytes)
    .map(drop)
    .map_err(ReplayRefused::Lfs)
}

/// Keeps the LFS object `oid` of repository `id` with `bytes` in its LFS index (A-629 part 3), the step its upload's
/// append takes between the register's guard and the entry's apply (`crate::repository::push::push_lfs`), lent the room
/// the bytes take: checked against the oid first, so bytes that are not the oid's are refused with nothing kept, and
/// past the budget refused typed. An object held already is kept as it is.
pub(crate) fn admit_lfs(
  state: &mut ShardState,
  id: RepositoryId,
  (oid, bytes): (&Oid, &[u8]),
) -> Result<Admitted, LfsAdmitRefused> {
  let brought = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  within_budget(state, brought, |stores| {
    let (content, store) = stores.git(id).ok_or(LfsAdmitRefused::NoStore)?;
    store
      .lfs
      .admit(content, oid, bytes)
      .map_err(LfsAdmitRefused::Object)
  })
}

/// Why [`admit_lfs`] kept nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LfsAdmitRefused {
  /// No git store of the repository is held here.
  NoStore,
  /// The LFS index refused the object: bytes that are not its oid's, or past the budget.
  Object(ObjectRefused),
}

/// The refusal an LFS upload's append answers when the LFS index kept nothing ([`admit_lfs`]): past the budget the room
/// the shard's budget had, otherwise content that cannot be had, counted as a store's refused commit is.
pub(crate) fn lfs_refusal_of(state: &mut ShardState, refused: &LfsAdmitRefused) -> Refusal {
  match refused {
    LfsAdmitRefused::Object(ObjectRefused::Store(ContentRefused::OverBudget { room, .. })) => {
      Refusal::BudgetExceeded { available: *room }
    }
    // Bytes that are not their oid's are the client's to send again; the caller answers them in LFS's words.
    LfsAdmitRefused::Object(ObjectRefused::Mismatch) => Refusal::BadRequest {
      reason: "the object's bytes are not its oid's".to_owned(),
    },
    LfsAdmitRefused::NoStore | LfsAdmitRefused::Object(_) => {
      state.count(COMMIT_REFUSED, 1);
      Refusal::ContentUnavailable
    }
  }
}

/// The LFS object `oid`'s size in repository `id`'s LFS index, when it holds it (A-629 part 3).
pub(crate) fn lfs_held(stores: &VcsStores, id: RepositoryId, oid: &Oid) -> Option<u64> {
  let (content, store) = stores.git_ref(id)?;
  store.lfs.held(content, oid)
}

/// The LFS object `oid`'s bytes in repository `id`'s LFS index, when it holds it (A-629 part 3).
pub(crate) fn lfs_bytes(stores: &VcsStores, id: RepositoryId, oid: &Oid) -> Option<Vec<u8>> {
  let (content, store) = stores.git_ref(id)?;
  store.lfs.bytes(content, oid)
}

/// Replays one Subversion record (A-655 part 2b; module docs, "Subversion") within `room`: a creation makes the
/// records, revision 0 dated as recorded; every later record is committed by the step its live one took
/// (`commit_record`): a revision checked and applied, its lock entries as recorded; the UUID and a revision's properties
/// an import set (A-652 part 3). A record out of its order is refused, and so is one the step refuses; either leaves
/// the records as they were.
fn replay_svn(store: &mut SvnStore, bytes: &[u8], room: SvnRoom) -> Result<(), ReplayRefused> {
  match SvnRecord::decode(bytes).map_err(ReplayRefused::SvnRecord)? {
    SvnRecord::Created { uuid, date } => {
      if store.records.is_some() {
        return Err(ReplayRefused::SvnOrder);
      }
      let records = SvnRecords::new(uuid, &date, room).map_err(ReplayRefused::SvnCreated)?;
      store.records = Some(records);
      Ok(())
    }
    record => {
      let records = store.records.as_mut().ok_or(ReplayRefused::SvnOrder)?;
      records.set_room(room);
      records
        .commit_record(record)
        .map_err(ReplayRefused::SvnStep)
    }
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
      if !release_charged(state, id) {
        state.count(RELEASE_REFUSED, 1);
      }
      settle(state);
      state.vcs_stores.unrecovered.insert(id, why);
      false
    }
  }
}

/// Makes `record`'s store and replays the records its register holds, in log order (module docs), in one step: what
/// `CreateRepository` does on its owner shard, where a Subversion repository's creation's entry holds its first record
/// (A-655 part 2b), and what a rebuild does.
pub(crate) fn make_from_log(state: &mut ShardState, record: &RepositoryRecord) {
  make(state, record);
  replay_all(state, record.id);
}

/// Replays every record repository `id`'s register holds into its store, in log order, stopping at a refused one.
fn replay_all(state: &mut ShardState, id: RepositoryId) {
  let mut index = 0usize;
  while let Some(outcome) = replay_record(state, id, index) {
    if !replayed(state, id, outcome) {
      break;
    }
    index = index.saturating_add(1);
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
    loop {
      // A Mercurial record is replayed a revision a unit (A-732): whole in one unit, a large repository's held its
      // shard past the watchdog's window at every restart.
      let outcome = if record.tool == VcsTool::Mercurial {
        replay_hg_sliced(state, record.id, index, slices).await
      } else {
        replay_record(state, record.id, index)
      };
      let Some(outcome) = outcome else {
        break;
      };
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
      paths::remake_green_sliced(state, green, slices).await;
      slices.unit_done().await;
    }
    paths::whole_build::resume_owed(state, record.id);
  }
}

/// Makes repository `id`'s store again from the log (module docs): what a commit whose entry was not made durable left
/// ahead of the log is given back, and every record replayed, as a recovery replays them, in one step.
pub(crate) fn rebuild(state: &mut ShardState, id: RepositoryId) {
  let mercurial = state
    .db
    .partition()
    .repositories()
    .repository(id)
    .is_some_and(|record| record.tool == VcsTool::Mercurial);
  if !mercurial {
    rebuild_sliced(state, id);
    return;
  }
  // A Mercurial repository's records replayed whole in one step held a large repository's shard past the watchdog's
  // window (A-732): its store made empty now, lent to nothing, and its records replayed a step a turn by a task.
  free(state, id);
  let Some(record) = state.db.partition().repositories().repository(id).cloned() else {
    return;
  };
  make(state, &record);
  state.vcs_stores.rebuilding.insert(id);
  let task = slates_rt::futures::spawn(replay_sliced(id)).and_then(slates_rt::futures::detach);
  if task.is_err() {
    // No task to run it: the store stays lent to nothing, and the next recovery makes it again a unit at a time.
    state.count(REPLAY_REFUSED, 1);
  }
  state.count(REBUILT, 1);
}

/// [`rebuild`] of a git or Subversion repository (A-732's sibling; main's ruling of 2026-11-20): a large repository's
/// records replayed in one step held its shard past the watchdog's window and crash-looped the daemon, as Mercurial's did.
/// Its store made empty now and lent to nothing ([`VcsStores::rebuild_turns`]), its records replayed a record a turn by
/// a task ([`rebuild_turn`]), and once the last is in, each green's map made again in slices by its build
/// (`paths::whole_build`), whose end makes any commit owed.
fn rebuild_sliced(state: &mut ShardState, id: RepositoryId) {
  free(state, id);
  let Some(record) = state.db.partition().repositories().repository(id).cloned() else {
    return;
  };
  make(state, &record);
  let generation = state.vcs_stores.rebuild_generations.wrapping_add(1);
  state.vcs_stores.rebuild_generations = generation;
  state.vcs_stores.rebuild_turns.insert(id, (generation, 0));
  state.vcs_stores.rebuilding.insert(id);
  let task =
    slates_rt::futures::spawn(rebuild_turns(id, generation)).and_then(slates_rt::futures::detach);
  if task.is_err() {
    // No task to run it: the store stays lent to nothing, and the next recovery makes it again a unit at a time.
    state.count(REPLAY_REFUSED, 1);
  }
  state.count(REBUILT, 1);
}

/// Repository `id`'s rebuild of `generation` driven a turn at a time, a yield between, until it ends or is replaced.
async fn rebuild_turns(id: RepositoryId, generation: u64) {
  while crate::state::with_state(|state| rebuild_turn(state, id, generation)) == Some(true) {
    slates_rt::futures::yield_now().await;
  }
}

/// One turn of repository `id`'s rebuild of `generation`: its next record replayed, or, past the last or at a refusal,
/// the rebuild ended ([`rebuild_ended`]). Whether more turns remain; none for a rebuild ended or replaced.
fn rebuild_turn(state: &mut ShardState, id: RepositoryId, generation: u64) -> bool {
  let Some((current, index)) = state.vcs_stores.rebuild_turns.get(&id).copied() else {
    return false;
  };
  if current != generation {
    return false;
  }
  if let Some(outcome) = replay_record(state, id, index)
    && replayed(state, id, outcome)
  {
    state
      .vcs_stores
      .rebuild_turns
      .insert(id, (generation, index.saturating_add(1)));
    return true;
  }
  rebuild_ended(state, id);
  false
}

/// A git or Subversion rebuild's end: the store lent again, then each git green's map made again in slices by its build
/// (`paths::whole_build`), whose end makes any commit a submit owed of it.
fn rebuild_ended(state: &mut ShardState, id: RepositoryId) {
  state.vcs_stores.rebuild_turns.remove(&id);
  state.vcs_stores.rebuilding.remove(&id);
  if !matches!(state.vcs_stores.store(id), Some(RepositoryStore::Git(_))) {
    return;
  }
  let greens: Vec<DbVolumeId> = state
    .db
    .partition()
    .repositories()
    .members_of(id)
    .map(|member| member.green)
    .collect();
  for green in greens {
    let head = state.greens.get(&green).map(|engine| engine.head());
    if let Some(head) = head.filter(|head| *head > 0) {
      paths::whole_build::start_remade(state, id, green, head);
    }
  }
}

/// Runs repository `id`'s git or Subversion rebuild under way to its end in this step, then every map build: what a test
/// that makes a store again and reads it does, since a test's step runs no other task between its calls.
#[cfg(test)]
pub(crate) fn settle_rebuild(state: &mut ShardState, id: RepositoryId) {
  if let Some((generation, _)) = state.vcs_stores.rebuild_turns.get(&id).copied() {
    while rebuild_turn(state, id, generation) {}
  }
  paths::whole_build::settle(state);
}

/// One turn of repository `id`'s git or Subversion rebuild under way: whether more remain.
#[cfg(test)]
pub(crate) fn rebuild_turn_once(state: &mut ShardState, id: RepositoryId) -> bool {
  state
    .vcs_stores
    .rebuild_turns
    .get(&id)
    .copied()
    .is_some_and(|(generation, _)| rebuild_turn(state, id, generation))
}

/// [`rebuild`] in one step, every record replayed at once: the tests' and Mercurial's tests'.
pub(crate) fn rebuild_whole(state: &mut ShardState, id: RepositoryId) {
  free(state, id);
  let Some(record) = state.db.partition().repositories().repository(id).cloned() else {
    return;
  };
  make_whole(state, &record);
  state.count(REBUILT, 1);
}

/// Repository `id`'s Mercurial records replayed into its store a step a turn, a yield between (A-732: [`rebuild`]'s
/// task); then the store lent again, its index's generation moved, so a push forked from the store before is re-based.
async fn replay_sliced(id: RepositoryId) {
  let mut index = 0usize;
  let mut replay = None;
  loop {
    let stepped = crate::state::with_state(|state| hg_record_step(state, id, index, replay.take()));
    match stepped.flatten() {
      Some(Ok(HgStep::Begun(begun) | HgStep::Stepped(Some(begun)))) => replay = Some(begun),
      Some(Ok(HgStep::Stepped(None))) => {
        let next = crate::state::with_state(|state| replayed(state, id, Ok(()))).unwrap_or(false);
        if !next {
          break;
        }
        index = index.saturating_add(1);
      }
      Some(Err(refused)) => {
        crate::state::with_state(|state| {
          let _ = replayed(state, id, Err(refused));
        })
        .unwrap_or_default();
        break;
      }
      None => break,
    }
    slates_rt::futures::yield_now().await;
  }
  crate::state::with_state(|state| {
    state.vcs_stores.rebuilding.remove(&id);
    state.vcs_stores.index_changed(id);
  })
  .unwrap_or_default();
}

/// Makes `record`'s store from the log ([`make_from_log`]) and each member green's map and trees from its head, as the
/// live store holds them (A-664 part 3, [`paths::remake`]), in one step: what a rebuild holds, and what a successor holds
/// of a repository it took over once it rebuilt it from its record and logs (A-636 part 2c-2), its greens' heads the
/// adopted ones (A-664's holders' maps).
pub(crate) fn make_whole(state: &mut ShardState, record: &RepositoryRecord) {
  make_from_log(state, record);
  paths::remake(state, record.id);
  // A commit a submit owed and no made commit settled is made by its build's end (A-664's live whole builds, part 2).
  paths::whole_build::resume_owed(state, record.id);
}

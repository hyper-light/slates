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
//! its record ([`record`]), written as the append commits: the names of the objects of the push that the repository
//! lacks then, by the predicate the store's commit gives positions by ([`SealedPush::names_now`]), and the changeset
//! on the node's store their pack landed in (version 5, A-767 C5; no record carries a push's objects since C5 part 2b
//! piece 3); the moves the store committed them with; and the deepest chain the push's pack carried, which every record
//! of the push carries. The partition's register keeps each entry's objects for as long as the repository lives
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
//! **Subversion** (A-655 part 2b; C2 of Ada's close list, A-767 item 1). A Subversion repository's history is the
//! node's changeset store: each record (`slates_svn::commit::record`: its creation, a revision, a lock verb's entries, a
//! revision's properties, the UUID, a deltified text) lands there as one changeset, and the register's entry names it
//! by its sequence alone (`repository::svn_commit::landing`), with the green's version where the record moves one. A
//! recovery or a rebuild reads each changeset the register names back from the store, in sequence order, before its
//! replay ([`svn_to_fetch`], [`fetch_svn`]), and replays it through the step's own check and apply (`replay_svn`, a
//! revision a row at a time), so the records after it equal the records before; a changeset landed past the last the
//! register names was never committed and is dropped. A new repository's store is made from its creation record in
//! hand ([`make_created`]), which lands on the store as `CreateRepository` is answered. The records are charged to the shard's budget by the bytes they hold
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

pub(crate) mod arena_windows;
pub(crate) mod commits;
pub(crate) mod durable;
#[allow(unused_imports)] // `ContentTicket` names the barrier's term for its callers.
pub(crate) use durable::{BarrierRefused, ContentTicket, content_durable, content_ticket};
pub(crate) mod git_derive;
pub(crate) mod git_encode;
pub(crate) mod git_replay;
pub(crate) mod hg_import;
pub(crate) mod hg_made;
pub(crate) mod hg_tags;
pub(crate) mod hg_view;
pub(crate) mod homes;
pub(crate) mod index_upkeep;
pub(crate) mod lfs_record;
pub(crate) mod named_objects;
pub(crate) mod owed;
pub(crate) mod paths;
pub(crate) mod read_through;
pub(crate) mod record;
#[cfg_attr(not(test), allow(dead_code))] // Owed caller: record v3 (w-perf's A-727 (b)).
pub(crate) mod record_names;
// Owed caller: A-645 P3's presenter, which makes a hook's view of referenced files.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod referenced;
#[cfg_attr(not(test), allow(dead_code))]
// Owed caller: w-git's C5 (the push landed on the store, its replay).
pub(crate) mod store_packs;
pub(crate) mod svn_import;
pub(crate) mod svn_made;
pub(crate) mod svn_presenter;
pub(crate) mod svn_texts;
pub(crate) mod svn_view;
// Its push helpers serve the hooks' tests too (`crate::git_hooks`, A-665 part 2).
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod upload;

use lfs_record::LfsRecordRefused;
use record::{
  Noted, Record, RecordRefused, RecordWriter, RecordedBase, RecordedMove, WriteRefused,
};

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
/// Counter (A-727 slice a3): repository windows a recovery took back from the shard image, their chunks claimed.
pub(crate) const WINDOWS_RESTORED: &str = "vcs_store.windows_restored";
/// Counter (A-727 slice a3): window pieces of the shard image a recovery could not take back (put again by replay).
pub(crate) const WINDOWS_UNRESTORED: &str = "vcs_store.windows_unrestored";
/// Counter (A-727 slice a3): restored windows no replayed record named, freed once the records were replayed.
pub(crate) const WINDOWS_UNHELD: &str = "vcs_store.windows_unheld";
/// Counter (A-727 slice a3b): a content store's lists and names a recovery took back from the shard image.
pub(crate) const NODES_RESTORED: &str = "vcs_store.nodes_restored";
/// Counter (A-727 slice a3b): lists and names of the shard image a recovery could not take back.
pub(crate) const NODES_UNRESTORED: &str = "vcs_store.nodes_unrestored";
/// Counter (A-724 piece 8c): region holds of the shard image a recovery held again.
pub(crate) const REGION_HOLDS_RESTORED: &str = "vcs_store.region_holds_restored";
/// Counter (goal 93 item 2b; main 01a20182's ruling of 2026-11-22 07:33): region holds of the shard image a recovery
/// could not take back (its window gone, its hold refused, or the set borrowed), logged: never skipped unseen, since a
/// region hold pins a landed window in RAM.
pub(crate) const REGION_HOLDS_UNRESTORED: &str = "vcs_store.region_holds_unrestored";
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
  /// The git records whose replay is under way a turn at a time (A-722 part 4, [`git_replay`]): at most one a
  /// repository, the record its rebuild or recovery replays now; bounded by the partition's repositories.
  git_replays: BTreeMap<RepositoryId, Box<git_replay::GitReplay>>,
  /// The Subversion revision records whose replay is under way a turn at a time (R4 of Ada's close list,
  /// `slates_svn::migration::RevisionReplay`): at most one a repository, the record its rebuild or recovery replays
  /// now; bounded by the partition's repositories.
  svn_replays: BTreeMap<RepositoryId, slates_svn::migration::RevisionReplay>,
  /// Each Subversion repository's landing turn (C2 of Ada's close list, `svn_commit::landing`): one changeset landing
  /// a repository at a time, from its check to its register's commit.
  pub(crate) svn_landings: crate::repository::svn_commit::landing::Gates,
  /// The Subversion records read back from the node's store for the register record a rebuild or recovery replays
  /// next (C2), by repository, with the register's index they are for, in their changeset's order (a commit's one, an
  /// import's batch's revisions, C2 stage C): at most one changeset's a repository.
  svn_fetched: BTreeMap<RepositoryId, (usize, std::collections::VecDeque<Vec<u8>>)>,
  /// The text pieces read through the node's store for drives waiting on them (C2 stage B part 2,
  /// [`svn_texts::Pieces`]).
  pub(crate) svn_pieces: svn_texts::Pieces,
  /// The generations git fetches hold, by repository (A-745 rule 4, [`upload::GitGeneration`]): one fork a stamp the
  /// repository stood at when a fetch began, shared by every fetch that began there, given back with its last holder;
  /// bounded by the fetches the shard's edges seat.
  git_forks: BTreeMap<RepositoryId, Vec<upload::GitFork>>,
  /// The price of a byte git's fetches send over this shard's links, learned from the time their writes waited for the
  /// peer (A-859 part A, [`upload::LinkPrice`]).
  pub(crate) git_link: upload::LinkPrice,
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
  /// Where the content store keeps its windows once it is made (A-727 slice a2): the shard's repository arena, given by
  /// the shard's start; none in a test's stores, whose content store keeps them in memory.
  window_storage: Option<Box<dyn slates_vcs::WindowStorage>>,
  /// A recovery's lists and names of the content store (A-727 slice a3b), taken in when the store is made.
  restored_nodes: Option<RestoredNodes>,
  /// A recovery's windows let go of the content store, each with its length (A-767 C1 step 2b), taken in before the
  /// lists when the store is made.
  restored_aways: Vec<(slates_vcs::ContentId, u64)>,
  /// What the content store took back of them: restored, refused (counted by the recovery).
  nodes_restored: (u64, u64),
  /// A recovery's region holds (A-724 piece 8c), held again when the content store is made over the restored windows.
  restored_region: Vec<slates_vcs::ContentId>,
  /// What the content store took back of them: held, and the ones it could not take (counted and logged by the
  /// recovery, [`REGION_HOLDS_UNRESTORED`]).
  region_restored: (u64, Vec<slates_vcs::ContentId>),
  /// Landed pushes' packs owed a let-go (goal 93 item 2b), by repository: committed while a fetch of it was answered or
  /// its encoding was under way, let go once it goes idle ([`let_go_owed`]); bounded by the repository's landed pushes
  /// since then.
  landed_owed: BTreeMap<RepositoryId, Vec<slates_vcs::ContentId>>,
  /// Each git repository's version being encoded as its fragments, between its turns ([`git_encode`], A-718): one a
  /// repository, so bounded by the partition's repositories.
  encodings: git_encode::Encodings,
  /// Each git repository's derivation of the sets its versions admitted with no walk reach, between its turns
  /// ([`git_derive`], A-614 amended): one a repository, so bounded by the partition's repositories.
  derivings: git_derive::Derivings,
  /// Each git green's whole map built in slices after a push, between its slices ([`paths::whole_build`], A-664's live
  /// whole builds): one a green, so bounded by the partition's greens.
  whole_builds: paths::whole_build::Builds,
  /// The git commits the server makes of versions already committed, landed and recorded a job at a time
  /// ([`commits::MadeAfter`], C5 part 2b piece 3): bounded by the shard's client seats.
  pub(crate) made_after: commits::MadeAfter,
  /// Each repository's index merge under way between its slices ([`index_upkeep`]): one a repository, so bounded by the
  /// partition's repositories.
  index_upkeep: index_upkeep::Upkeep,
  /// Each HTTP commit under way (A-681 part C: a transaction a POST made, between its requests), by repository and
  /// transaction name, with the key of the principal that made it and the monotonic instant of its last step: at most
  /// one a repository and principal, a new POST replacing the older ([`VcsStores::begin_dav_commit`]), so bounded by the
  /// partition's repositories and principals; and one idle past the edge's learned idle bound is released at the
  /// table's next use ([`VcsStores::release_idle_dav_commits`]).
  dav_commits: BTreeMap<(RepositoryId, String), DavHeld>,
  /// The HTTP transactions this shard has named, which a transaction's name counts.
  dav_named: u64,
  /// How many fetches hold each Subversion store's generation (A-749, §4.17.1 "A fetch's answer" rule 4,
  /// [`hold_svn`]): bounded by the fetches in flight.
  svn_holders: BTreeMap<u64, usize>,
  /// The records of each Subversion store freed while a fetch held its generation ([`free`]), by that generation, with
  /// their repository and what they hold of the shard's budget, until the last hold is given back
  /// ([`release_svn_hold`]): bounded by the fetches in flight.
  svn_held: BTreeMap<u64, (RepositoryId, SvnRecords, u64)>,
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
  /// The sequence of the changeset this repository landed last on the node's changeset log (A-767 C5): 0 its creation's
  /// (046628f88), then each push's, taken in turn on this shard ([`VcsStores::next_changeset`]) and read back from the
  /// register's records at recovery, so the next push after a crash lands at the last one named + 1.
  pub(crate) landed: u64,
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
      landed: 0,
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
  /// The sequence of the changeset this repository landed last on the node's changeset log (A-767 C4): 0 its creation's
  /// (046628f88), then each push's, taken in turn on this shard ([`VcsStores::next_changeset`]) and read back from the
  /// register's references at recovery.
  pub(crate) landed: u64,
}

/// A Subversion repository's store: its records, beside its one green (A-655 part 2b; module docs, "Subversion").
#[derive(Debug, Default)]
pub(crate) struct SvnStore {
  /// The generation the store was made as: a store made again (a rebuild) is another, which a fetch's hold on this one
  /// never reads (A-749 rule 4, [`hold_svn`]).
  pub(crate) generation: u64,
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
  /// A Subversion revision whose record carries a text its rows name inline, not named stored: the form a log held
  /// before C2 stage B part 2 (b) (2026-11-21, main's ruling of 09:20, retiring the records' RAM texts). A repository's
  /// texts are its changesets' on the node's store, and records a daemon keeps hold none in RAM.
  SvnTextsInline,
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
  /// A version-3 git record's names (A-724 piece 8c) not read: the store holds no content of their identity whole (not
  /// yet fetched from their region homes), or its rows are refused.
  Names(record_names::NamesRefused),
  /// A Mercurial record naming a changeset on the changeset log (record v7, A-767 C4), or a git record version 5 (C5),
  /// the node cannot read back: no log held, its owner busy or gone, the changeset missing, or its names or payload not
  /// the identities and lengths the record names.
  Unlanded,
  /// A Subversion register record that is not a changeset's reference, or names another sequence than its place
  /// (C2: the register names each changeset in sequence order).
  SvnReference,
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
  /// A push's names and pack were homed and landed, but they are no longer what its record would name (another push
  /// committed one of its objects since): the push is refused, for its client's retry, never written inline (C5 part 2b).
  Unlanded,
  /// A record's moves and their bases (A-850) are not one each: written so, or replayed so.
  Unaligned,
}

/// A record not written, in a push's terms: an object whose bytes the content store does not hold whole is one the
/// store cannot read, as its own refusal names it; a pack not made or a room refused, the record's room.
fn unwritten(refused: WriteRefused) -> GitRefused {
  match refused {
    WriteRefused::Allocation => GitRefused::Allocation,
    WriteRefused::Unaligned => GitRefused::Unaligned,
  }
}

/// A content store's refusal, in a push's terms.
fn content_refused(refused: ContentRefused) -> GitRefused {
  GitRefused::Store(StoreRefused::Content(refused))
}

impl VcsStores {
  /// Stores whose content store, once made, keeps its windows in `storage` (A-727 slice a2: the shard's repository
  /// arena).
  pub(crate) fn with_windows(storage: Box<dyn slates_vcs::WindowStorage>) -> VcsStores {
    VcsStores {
      window_storage: Some(storage),
      ..VcsStores::default()
    }
  }

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

  /// The memory status's gauges for the repositories' stores (main's ruling of 2026-11-22 03:46): the content store's
  /// census (`ContentStore::census`), and over every git repository its read scratch, the objects it read lately, its
  /// maps' entries and the heads of its greens' maps. A walk of the content store's entries, so only a memory status
  /// asks.
  pub(crate) fn memory_rows(&self) -> Vec<(&'static str, u64)> {
    let mut rows = Vec::new();
    if let Some(content) = &self.content {
      let census = content.census();
      rows.extend([
        ("mem.gauge.vcs_content.stored", census.stored),
        ("mem.gauge.vcs_content.stored_bytes", census.stored_bytes),
        ("mem.gauge.vcs_content.away", census.away),
        ("mem.gauge.vcs_content.away_bytes", census.away_bytes),
        ("mem.gauge.vcs_content.here_windows", census.here_windows),
        (
          "mem.gauge.vcs_content.here_window_bytes",
          census.here_window_bytes,
        ),
        ("mem.gauge.vcs_content.lists", census.lists),
        ("mem.gauge.vcs_content.list_bytes", census.list_bytes),
        ("mem.gauge.vcs_content.tables", census.tables),
        ("mem.gauge.vcs_content.table_bytes", census.table_bytes),
        ("mem.gauge.vcs_content.derived", census.derived),
        ("mem.gauge.vcs_content.derived_bytes", census.derived_bytes),
        ("mem.gauge.vcs_content.objects", census.objects),
        ("mem.gauge.vcs_content.used_bytes", census.used),
        ("mem.gauge.vcs_content.charged_bytes", census.charged),
      ]);
    }
    let mut git = slates_git::store::RepositoryHeld::default();
    let (mut repositories, mut heads) = (0u64, 0u64);
    for store in self.entries.values() {
      if let RepositoryStore::Git(store) = store {
        let held = store.repository.held();
        git.scratch = git.scratch.saturating_add(held.scratch);
        git.recent = git.recent.saturating_add(held.recent);
        git.map_entries = git.map_entries.saturating_add(held.map_entries);
        git.charged = git.charged.saturating_add(held.charged);
        repositories = repositories.saturating_add(1);
        heads = heads.saturating_add(u64::try_from(store.heads.len()).unwrap_or(u64::MAX));
      }
    }
    rows.extend([
      ("mem.gauge.git.repositories", repositories),
      ("mem.gauge.git.scratch_bytes", git.scratch),
      ("mem.gauge.git.recent_bytes", git.recent),
      ("mem.gauge.git.map_entries", git.map_entries),
      ("mem.gauge.git.charged_bytes", git.charged),
      ("mem.gauge.git.green_heads", heads),
    ]);
    rows
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

  /// The sequence repository `id`'s next changeset lands at on the changeset log (A-767 C4, C5), taken now so two pushes
  /// landing side by side never share one; none where the repository has no Mercurial or git store here.
  pub(crate) fn next_changeset(&mut self, id: RepositoryId) -> Option<u64> {
    let landed = match self.entries.get_mut(&id)? {
      RepositoryStore::Mercurial(store) => &mut store.landed,
      RepositoryStore::Git(store) => &mut store.landed,
      _ => return None,
    };
    *landed = landed.checked_add(1)?;
    Some(*landed)
  }

  /// The last changeset repository `id`'s store counts landed; none for a store keeping no changeset log's count.
  pub(crate) fn landed_of(&self, id: RepositoryId) -> Option<u64> {
    match self.entries.get(&id)? {
      RepositoryStore::Mercurial(store) => Some(store.landed),
      RepositoryStore::Git(store) => Some(store.landed),
      _ => None,
    }
  }

  /// Counts repository `id`'s landed changesets back to `sequence`: an import given back, its segments unnamed (main's
  /// ruling, decision 2: the next landing takes the last the register names, plus one).
  pub(crate) fn set_landed(&mut self, id: RepositoryId, sequence: u64) {
    match self.entries.get_mut(&id) {
      Some(RepositoryStore::Mercurial(store)) => store.landed = sequence,
      Some(RepositoryStore::Git(store)) => store.landed = sequence,
      _ => {}
    }
  }

  /// Notes that repository `id` landed changeset `sequence` (a recovery reading the register's records).
  pub(crate) fn landed_changeset(&mut self, id: RepositoryId, sequence: u64) {
    match self.entries.get_mut(&id) {
      Some(RepositoryStore::Mercurial(store)) => store.landed = store.landed.max(sequence),
      Some(RepositoryStore::Git(store)) => store.landed = store.landed.max(sequence),
      _ => {}
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

  /// How many text pieces read through the node's store are held for waiting drives (C2 stage B part 2).
  #[cfg(test)]
  pub(crate) fn svn_pieces_held(&self) -> usize {
    self.svn_pieces.len()
  }

  /// Repository `id`'s Subversion store, lent for one step.
  pub(crate) fn subversion(&mut self, id: RepositoryId) -> Option<&mut SvnStore> {
    if self.rebuild_turns.contains_key(&id) {
      return None;
    }
    self.svn_unlent(id)
  }

  /// Repository `id`'s Subversion records and the shard's content store, lent together for one step: an import's call,
  /// which reads the records and puts its texts into the store as it reads (`svn_import::texts`). None as
  /// [`VcsStores::subversion`] answers none, or where either is not made.
  pub(crate) fn subversion_with_content(
    &mut self,
    id: RepositoryId,
  ) -> Option<(&mut SvnRecords, &mut ContentStore)> {
    if self.rebuild_turns.contains_key(&id) {
      return None;
    }
    let content = self.content.as_mut()?;
    match self.entries.get_mut(&id)? {
      RepositoryStore::Subversion(store) => Some((store.records.as_mut()?, content)),
      _ => None,
    }
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

  /// The name of the HTTP commit the principal keyed `principal` holds in repository `id` that is DeltaV v1's
  /// `activity` (A-766 part 2); none where it holds none by that activity.
  pub(crate) fn dav_commit_of_activity(
    &self,
    id: RepositoryId,
    principal: &[u8],
    activity: &str,
  ) -> Option<String> {
    self
      .dav_commits
      .iter()
      .find(|((repository, _), held)| {
        *repository == id && held.principal == principal && held.commit.activity() == Some(activity)
      })
      .map(|((_, name), _)| name.clone())
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
  stores.content_made(window).map_err(|_| Unmade)?;
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
    VcsTool::Subversion => RepositoryStore::Subversion(Box::new(SvnStore {
      generation: stores.next_generation(),
      ..SvnStore::default()
    })),
  };
  stores.entries.entry(record.id).or_insert(store);
  Ok(())
}

/// A store not made ([`made`]).
struct Unmade;

/// Runs `step` with repository `id`'s miss-ratio curve lent to the shard's content store, so every content its step
/// reads is noted in it (A-724 piece 2d; §4.17.5 "Learned demand"): taken from its usage row, bounded to the windows
/// its floor holds ([`curve_keys`]), lent for the step and given back, the curve lent before restored, so a step inside
/// a step is noted in its own repository's. With no content store made yet, `step` runs unobserved.
pub(crate) fn observed<R>(
  state: &mut ShardState,
  id: RepositoryId,
  step: impl FnOnce(&mut ShardState) -> R,
) -> R {
  let Some(granule) = state
    .vcs_stores
    .content()
    .map(|content| u64::try_from(content.window()).unwrap_or(u64::MAX))
  else {
    return step(state);
  };
  let now = slates_rt::futures::now_ns();
  let capacity = curve_keys(state);
  let curve = state.vcs_usage.take_curve(&id, (capacity, granule), now);
  let before = state
    .vcs_stores
    .content()
    .and_then(|content| content.observe(Some(curve)));
  let answer = step(state);
  let lent = state
    .vcs_stores
    .content()
    .and_then(|content| content.observe(before));
  if let Some(curve) = lent {
    state.vcs_usage.put_curve(&id, curve);
  }
  answer
}

/// The sampled keys a repository's curve keeps (A-724 piece 2d): the content windows its floor of the shard's room
/// holds, `room / (repositories served × window)`, at least one. A curve need resolve no more than its floor could
/// cache: a working set that fits it is sampled whole, a larger one at that resolution, the curve costing about
/// `56 / window` of the floor (0.09% at a 64 KiB window). Measured accuracy (`slates_vcs::mrc`): 8,000 keys, a floor of
/// about 512 MiB, erred 0.008 to 0.049; a smaller floor's curve is coarser.
pub(crate) fn curve_keys(state: &ShardState) -> usize {
  let served = u64::try_from(state.vcs_usage.rows().count())
    .unwrap_or(u64::MAX)
    .max(1);
  let window = state
    .vcs_stores
    .content()
    .map_or(1, |content| {
      u64::try_from(content.window()).unwrap_or(u64::MAX)
    })
    .max(1);
  let keys = state
    .store
    .admittable()
    .checked_div(served)
    .and_then(|floor| floor.checked_div(window))
    .unwrap_or(0);
  usize::try_from(keys).unwrap_or(usize::MAX).max(1)
}

/// Frees repository `id`'s store with the repository (module docs): an import of it under way given back first
/// ([`hg_import::abandon`], [`svn_import::abandon`]), then every hold of its rows, tables, versions and derived values
/// given back to the content store, then the shard's charge trued up to what remains. A refusal while giving back is
/// counted, and the rest is given back.
pub(crate) fn free(state: &mut ShardState, id: RepositoryId) {
  // A git or Subversion rebuild under way ends with its store: its task finds its turns gone and stops.
  if state.vcs_stores.rebuild_turns.remove(&id).is_some() {
    state.vcs_stores.rebuilding.remove(&id);
  }
  abandon_git_replay(state, id);
  state.vcs_stores.svn_replays.remove(&id);
  state.vcs_stores.svn_fetched.remove(&id);
  paths::whole_build::abandon(state, id);
  hg_import::abandon(state, id);
  git_encode::abandon(state, id);
  git_derive::abandon(state, id);
  svn_import::abandon(state, id);
  if let Some(charged) = state.vcs_stores.objects_charged.remove(&id) {
    state.store.budget.credit_retention(charged);
  }
  state.vcs_stores.unrecovered.remove(&id);
  state.vcs_usage.forget(&id);
  keep_held_svn(&mut state.vcs_stores, id);
  if !release_charged(state, id) {
    state.count(RELEASE_REFUSED, 1);
  }
  settle(state);
}

/// The holds repository `id`'s git records keep of the names and the packs they name (version 5, A-767 C5; A-824):
/// each record holds one of each, taken when it was written (`put_names`, `hold_for_record`) or replayed. The records'
/// own, so never [`free`]'s, which a rebuild runs before it replays the same records: a repository's destroy reads them
/// here while its register still names them, and lets them go once its destroy is recorded (A-843,
/// `crate::repository::destroy_repository`). Nothing for a repository of another tool, or one whose store is not held
/// here.
pub(crate) fn record_holds(state: &ShardState, id: RepositoryId) -> Vec<slates_vcs::ContentId> {
  let Some(format) = state
    .vcs_stores
    .git_ref(id)
    .map(|(_, git)| git.repository.format())
  else {
    return Vec::new();
  };
  state
    .db
    .partition()
    .repositories()
    .objects(id)
    .filter(|bytes| !owed::is_owed(bytes) && !lfs_record::is_lfs(bytes))
    .filter_map(|bytes| Record::decode(bytes, format).ok())
    .flat_map(|record| {
      record
        .named
        .into_iter()
        .chain(record.packed)
        .map(|(identity, _)| identity)
    })
    .collect()
}

/// Lets go of every hold [`record_holds`] names for repository `id`, as its destroy does.
#[cfg(test)]
pub(crate) fn release_record_holds(state: &mut ShardState, id: RepositoryId) {
  for identity in record_holds(state, id) {
    release_names(state, &identity);
  }
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

/// Subversion repository `id`'s records kept aside with their charge as its store is freed, where a fetch holds their
/// generation (A-749, §4.17.1 "A fetch's answer" rule 4): the fetch ends on the records it began on, whatever rebuilds
/// or removes the store meanwhile, and the last hold's release gives them and their charge back ([`release_svn_hold`]).
fn keep_held_svn(stores: &mut VcsStores, id: RepositoryId) {
  let Some(RepositoryStore::Subversion(store)) = stores.entries.get_mut(&id) else {
    return;
  };
  if !stores.svn_holders.contains_key(&store.generation) {
    return;
  }
  if let Some(records) = store.records.take() {
    let charged = core::mem::take(&mut store.charged);
    stores
      .svn_held
      .insert(store.generation, (id, records, charged));
  }
}

/// A fetch's hold on Subversion repository `id`'s store as it is now (A-749 rule 4): its generation, counted held;
/// none when its records are not lent here.
pub(crate) fn hold_svn(stores: &mut VcsStores, id: RepositoryId) -> Option<u64> {
  stores.subversion_records(id)?;
  let generation = stores.subversion(id)?.generation;
  let holders = stores.svn_holders.entry(generation).or_insert(0);
  *holders = holders.saturating_add(1);
  Some(generation)
}

/// Gives back one hold on `generation` ([`hold_svn`]): at the last, the records kept aside for it, if its store was
/// freed meanwhile, are dropped and their charge given back to the shard's budget.
pub(crate) fn release_svn_hold(state: &mut ShardState, generation: u64) {
  let stores = &mut state.vcs_stores;
  match stores.svn_holders.get_mut(&generation) {
    Some(holders) if *holders > 1 => {
      *holders = holders.saturating_sub(1);
      return;
    }
    Some(_) => {
      stores.svn_holders.remove(&generation);
    }
    None => return,
  }
  if let Some((_, _, charged)) = stores.svn_held.remove(&generation) {
    state.store.budget.credit_retention(charged);
  }
}

/// The records a fetch's hold on repository `id`'s `generation` reads (A-749 rule 4): those kept aside when its store
/// was freed, else the store's own while it is that generation and lent; none otherwise.
pub(crate) fn held_svn_records(
  stores: &VcsStores,
  id: RepositoryId,
  generation: u64,
) -> Option<&SvnRecords> {
  if let Some((held, records, _)) = stores.svn_held.get(&generation) {
    return (*held == id).then_some(records);
  }
  let records = stores.subversion_records(id)?;
  match stores.entries.get(&id)? {
    RepositoryStore::Subversion(store) if store.generation == generation => Some(records),
    _ => None,
  }
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
  count_region_restored(state);
}

/// Counts what the content store's making took back of a recovery's region holds ([`VcsStores::content_made`], which may
/// run in any step after the recovery: the store is made by its first use), and logs the ones it could not: a region
/// hold is the pin that keeps a landed window in RAM (goal 93 item 2b, main 01a20182's ruling of 2026-11-22 07:33).
fn count_region_restored(state: &mut ShardState) {
  if state.vcs_stores.region_restored.0 == 0 && state.vcs_stores.region_restored.1.is_empty() {
    return;
  }
  let (held, unheld) = std::mem::take(&mut state.vcs_stores.region_restored);
  state.count(REGION_HOLDS_RESTORED, held);
  if let Some(first) = unheld.first() {
    state.count(
      REGION_HOLDS_UNRESTORED,
      u64::try_from(unheld.len()).unwrap_or(u64::MAX),
    );
    slates_machine::log_line!(
      "slates-server: partition {}: {} region holds of the shard image not taken back (the first {first:?}): their windows \
       are no longer pinned for the region",
      state.partition,
      unheld.len()
    );
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
  let answer = {
    let lent = Lent::new(&mut state.store, &mut state.vcs_stores);
    step(&mut *lent.stores)
  };
  settle(state);
  answer
}

impl VcsStores {
  /// Makes the shard's content store if it has none, over the windows' storage it was given (`with_windows`) or the
  /// heap's, with the content plane's `window`: what a first repository, or a first window homed here, makes.
  pub(crate) fn content_made(&mut self, window: usize) -> Result<(), slates_vcs::ContentRefused> {
    if self.content.is_none() {
      let content = match self.window_storage.take() {
        Some(storage) => ContentStore::with_storage(self.charged, window, storage),
        None => ContentStore::new(self.charged, window),
      }?;
      self.content = Some(content);
      // A recovery's windows let go, then its lists and names (a3b), over the windows the storage handed back: counted
      // by the owner. The windows let go first, since a list holds the nodes it names.
      let aways = std::mem::take(&mut self.restored_aways);
      let (mut offered, mut refused) = (u64::try_from(aways.len()).unwrap_or(u64::MAX), 0u64);
      if let Some(content) = self.content.as_mut() {
        refused = content.restore_away(aways);
      }
      if let (Some((lists, names)), Some(content)) =
        (self.restored_nodes.take(), self.content.as_mut())
      {
        offered = offered.saturating_add(
          u64::try_from(lists.len().saturating_add(names.len())).unwrap_or(u64::MAX),
        );
        refused = refused.saturating_add(content.restore_nodes(lists, names));
      }
      if offered > 0 {
        self.nodes_restored = (offered.saturating_sub(refused), refused);
      }
      if let Some(content) = self.content.as_mut() {
        let region = std::mem::take(&mut self.restored_region);
        let (held, refused) = homes::restore_region_holds(content, &region);
        self.region_restored = (held, refused);
      }
    }
    Ok(())
  }

  /// Lends `chunks` to the content store's windows, or to the storage a content store not made yet will keep them in,
  /// so a store made inside the step finds it lent (`Lent`): whether it took them.
  fn lend_in(&mut self, chunks: &mut slates_vfs::content::ChunkStore) -> bool {
    match (self.content.as_mut(), self.window_storage.as_mut()) {
      (Some(content), _) => content.lend_in(chunks),
      (None, Some(storage)) => storage.lend_in(chunks),
      (None, None) => false,
    }
  }

  /// Hands back what [`VcsStores::lend_in`] took, from the content store or the storage not made into one yet.
  fn hand_back(&mut self, chunks: &mut slates_vfs::content::ChunkStore) -> bool {
    match (self.content.as_mut(), self.window_storage.as_mut()) {
      (Some(content), _) => content.hand_back(chunks),
      (None, Some(storage)) => storage.hand_back(chunks),
      (None, None) => false,
    }
  }
}

/// A content store's lists, each by identity with its bytes, and its names, an object's identity with its root, as a
/// shard image carried them (A-727 slice a3b).
type RestoredNodes = (
  Vec<(slates_vcs::ContentId, Vec<u8>)>,
  Vec<(slates_vcs::ContentId, slates_vcs::ContentId)>,
);

/// The shard's chunk store lent to the repository content store for one step (A-727 slice a2, `arena_windows`): taken
/// in when made, handed back when dropped, so no path out of the step keeps it, and the shard budget extended by what
/// the step's puts grew the arena from its pool. A lend inside a lend takes nothing and hands nothing back; with no
/// content store made yet, the storage it will be made over is lent.
pub(crate) struct Lent<'a> {
  /// The shard's chunk store, or the placeholder the content store left in its place while it is lent.
  chunks: &'a mut slates_vfs::content::ChunkStore,
  /// The shard's budget, extended by the arena's growth at the end.
  budget: &'a mut slates_mem::budget::ShardBudget,
  /// The stores the step runs over.
  pub(crate) stores: &'a mut VcsStores,
  /// The arena's capacity when lent, if this lend took the chunk store in.
  took: Option<usize>,
}

impl<'a> Lent<'a> {
  /// Lends the chunk store of `store` to the content store of `stores` for as long as this lives.
  pub(crate) fn new(store: &'a mut Store, stores: &'a mut VcsStores) -> Lent<'a> {
    let Store {
      content, budget, ..
    } = store;
    let took = lend_chunks(content, stores);
    Lent {
      chunks: content,
      budget,
      stores,
      took,
    }
  }
}

impl Drop for Lent<'_> {
  fn drop(&mut self) {
    hand_chunks_back(self.chunks, self.budget, self.stores, self.took);
  }
}

/// The shard's chunk store taken in by [`take_chunks`], for [`give_chunks`] to hand back.
#[derive(Debug)]
#[must_use = "a chunk store taken in is handed back by give_chunks"]
pub(crate) struct Taken(Option<usize>);

/// Lends the shard's chunk store to its content store for a caller that holds the state across a serving call whose
/// reads are `&self` (`lfs::ShardObjects`): handed back by [`give_chunks`].
pub(crate) fn take_chunks(state: &mut ShardState) -> Taken {
  Taken(lend_chunks(&mut state.store.content, &mut state.vcs_stores))
}

/// Hands back what [`take_chunks`] took, the shard budget extended by the arena's growth.
pub(crate) fn give_chunks(state: &mut ShardState, taken: Taken) {
  let ShardState {
    store, vcs_stores, ..
  } = state;
  hand_chunks_back(&mut store.content, &mut store.budget, vcs_stores, taken.0);
}

/// Lends `chunks` to `stores`: the arena's capacity then, if taken.
fn lend_chunks(
  chunks: &mut slates_vfs::content::ChunkStore,
  stores: &mut VcsStores,
) -> Option<usize> {
  let capacity = chunks.arena().capacity();
  stores.lend_in(chunks).then_some(capacity)
}

/// Hands back what [`lend_chunks`] took, `budget` extended by what the arena grew meanwhile.
fn hand_chunks_back(
  chunks: &mut slates_vfs::content::ChunkStore,
  budget: &mut slates_mem::budget::ShardBudget,
  stores: &mut VcsStores,
  took: Option<usize>,
) {
  let Some(before) = took else {
    return;
  };
  stores.hand_back(chunks);
  let grown = chunks.arena().capacity().saturating_sub(before);
  if grown > 0 {
    budget.extend(u64::try_from(grown).unwrap_or(u64::MAX));
  }
}

/// Runs `read` over the stores, read only, with the room [`lend`] gives: what a step that parses objects needs, since
/// A-614's store lists a tree, a tag or a commit within the content store's room (`ContentStore::room`), as A-638's view
/// does, and a settled store has none, its budget being what it holds. Read only, the stores hold after it what they
/// held before, so the room lent is given back and nothing is charged.
pub(crate) fn lent_to_read<T>(
  (store, stores): (&mut Store, &mut VcsStores),
  read: impl FnOnce(&VcsStores) -> T,
) -> T {
  lend(store, stores, 0);
  let answer = {
    let lent = Lent::new(&mut *store, stores);
    read(&*lent.stores)
  };
  set_back(stores);
  answer
}

/// [`lent_to_read`] over the whole shard's state: what a step needs that reads the stores beside the shard's greens and
/// records, as git's push judges each command over the greens, its repository's store and its quarantine at once
/// (A-654). Read only, so the room lent is given back and nothing is charged.
pub(crate) fn with_room<T>(state: &mut ShardState, read: impl FnOnce(&ShardState) -> T) -> T {
  lend(&mut state.store, &mut state.vcs_stores, 0);
  let answer = lent_state(state, |state| read(state));
  set_back(&mut state.vcs_stores);
  answer
}

/// Runs `step` over the whole shard state with the shard's chunk store lent to the repository content store (`Lent`'s
/// rule, for a step that needs more of the state than the stores): taken in before and handed back after, nothing
/// between the two able to leave another way. Within it the volume store's chunk store is the placeholder, so the step
/// grows nothing and reads no volume chunk; [`lend`] runs before it.
pub(crate) fn lent_state<T>(state: &mut ShardState, step: impl FnOnce(&mut ShardState) -> T) -> T {
  let took = lend_chunks(&mut state.store.content, &mut state.vcs_stores);
  let answer = step(state);
  let ShardState {
    store, vcs_stores, ..
  } = &mut *state;
  hand_chunks_back(&mut store.content, &mut store.budget, vcs_stores, took);
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
  /// Moves admitted a turn at a time before their append (A-722 part 4c, `git_push`'s judge), each taken by its append
  /// while its green's head is still the one it was admitted from.
  prepared: Vec<Move>,
  /// The names homed in the region at `f + 1` before the record (A-724 piece 8c): their content's identity and count, so
  /// the record is written as version 4 if they are still the names it would write and its pack is homed too; none for
  /// version 2.
  named: Option<(slates_vcs::ContentId, u64)>,
  /// The push's pack homed in the region at `f + 1` before the record (A-722's record v4): the identity and length of
  /// the content its staging put it in, held for the record; none for version 2.
  packed: Option<(slates_vcs::ContentId, u64)>,
  /// The sequence of the changeset the node's store landed the names and the pack as, before the record names it
  /// (A-767 C5); 0 for none.
  landed: u64,
}

/// Puts a push's names' rows `bytes` into the shard's content store as one content, held for its record (A-724 piece
/// 8c), lent the room the shard can admit for the step and settled after: its identity; none where refused.
pub(crate) fn put_names(state: &mut ShardState, bytes: &[u8]) -> Option<slates_vcs::ContentId> {
  let need = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  lend(&mut state.store, &mut state.vcs_stores, need);
  let put = {
    let lent = Lent::new(&mut state.store, &mut state.vcs_stores);
    lent
      .stores
      .content
      .as_mut()
      .map(|content| slates_vcs::rows::put(content, bytes))
  };
  settle(state);
  put.and_then(Result::ok)
}

/// Takes a hold of the content `identity` for the record that will name it (A-722's record v4: the push's pack, held by
/// its record as a version 4 record's names are): whether the store holds it.
pub(crate) fn hold_for_record(state: &mut ShardState, identity: &slates_vcs::ContentId) -> bool {
  let held = {
    let lent = Lent::new(&mut state.store, &mut state.vcs_stores);
    lent
      .stores
      .content
      .as_mut()
      .is_some_and(|content| content.hold(identity).is_ok())
  };
  settle(state);
  held
}

/// Gives back the hold [`put_names`] or [`hold_for_record`] took of the content `identity`, where no record names it
/// (its homing fell short).
pub(crate) fn release_names(state: &mut ShardState, identity: &slates_vcs::ContentId) {
  {
    let lent = Lent::new(&mut state.store, &mut state.vcs_stores);
    if let Some(content) = lent.stores.content.as_mut() {
      let _ = content.release(identity);
    }
  }
  settle(state);
}

impl SealedPush {
  /// The rows of the names the record would write now (A-724 piece 8c): each object the repository lacks, as
  /// `record::RecordWriter::names_of` names it, encoded (`record_names::encode_names`), and their count.
  pub(crate) fn names_now(&self, stores: &VcsStores) -> Result<(Vec<u8>, u64), GitRefused> {
    let (content, store) = stores.git_ref(self.repository).ok_or(GitRefused::NoStore)?;
    let names = self.objects.names_of(lacks(content, store));
    let count = u64::try_from(names.len()).unwrap_or(u64::MAX);
    let bytes = record_names::encode_names(&names).map_err(|_| GitRefused::NoStore)?;
    Ok((bytes, count))
  }

  /// The names homed in the region at `f + 1` under `named` (their identity and count): the record is version 4 while
  /// they are still its names and its pack is homed too.
  pub(crate) fn homed_as(&mut self, named: (slates_vcs::ContentId, u64)) {
    self.named = Some(named);
  }

  /// The push's pack to home in the region for a version 4 record (A-722's record v4): its bytes, and the identity and
  /// length its staging put it in the store under; none for a push that put no pack. The bytes are copied for the
  /// homing, which runs off the shard's step.
  pub(crate) fn pack_to_home(&self) -> Option<(Vec<u8>, (slates_vcs::ContentId, u64))> {
    let stored = self.push.stored_pack()?;
    Some((self.push.pack().to_vec(), stored))
  }

  /// Keeps `packed`, the push's pack homed in the region under its identity and length: the record is version 5 while
  /// it is still the push's stored pack and the names homed are still its names.
  pub(crate) fn pack_homed_as(&mut self, packed: (slates_vcs::ContentId, u64)) {
    self.packed = Some(packed);
  }

  /// Keeps `sequence`, the changeset the node's store landed the homed names and pack as (A-767 C5): the version 5 record
  /// names it.
  pub(crate) fn landed_as(&mut self, sequence: u64) {
    self.landed = sequence;
  }

  /// Gives back the holds the homed names and pack took for a record that will never name them (a push refused after
  /// its homing: its changeset unlanded).
  pub(crate) fn release_homed(&mut self, state: &mut ShardState) {
    for (identity, _) in [self.named.take(), self.packed.take()]
      .into_iter()
      .flatten()
    {
      release_names(state, &identity);
    }
  }

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
  /// written now, as the append commits (C5 part 2b). An append that brings no object the repository lacks names none,
  /// version 5. One whose names and pack its edge homed and landed names them and its changeset, version 5, while they
  /// are still the names it would write (another push may have committed one of its objects since: refused, for the
  /// client's retry, never written inline). One bringing objects nobody homed and landed is refused `Unlanded`: no record
  /// carries a push's objects (C5 part 2b piece 3; a server-made commit lands as a client's push does,
  /// `commits::make_deferred` and `commits::MadeAfter`).
  pub(crate) fn record(&self, stores: &VcsStores) -> Result<Vec<u8>, GitRefused> {
    let (bytes, now) = self.names_now(stores)?;
    // A-850: each move's base, the version it was made at, as its admission found it, so a replay takes the same.
    let bases: Vec<RecordedBase> = self.moves.iter().map(Move::base).collect();
    let moves = (self.push.deepest(), self.keyed.as_slice(), bases.as_slice());
    if now == 0 {
      return RecordWriter::finish_packed(moves, None, None, 0).map_err(unwritten);
    }
    if let Some((identity, count)) = self.named {
      return match self.packed {
        Some(packed)
          if self.push.stored_pack() == Some(packed)
            && now == count
            && slates_vcs::ContentId::of(&bytes) == identity
            && self.landed > 0 =>
        {
          RecordWriter::finish_packed(moves, Some((identity, count)), Some(packed), self.landed)
            .map_err(unwritten)
        }
        _ => Err(GitRefused::Unlanded),
      };
    }
    // Objects nobody homed and landed: no record carries a push's objects (C5 part 2b piece 3).
    Err(GitRefused::Unlanded)
  }

  /// Admits `moves` (each a green's key, its version once the entry moves it, and its new head) in place of any
  /// admitted before, against the store as it is now: what one append commits. The admission walks the push's trees
  /// and tags within the room lent for it ([`lent_to_read`]). Refused, the push keeps what it holds.
  pub(crate) fn admit(
    &mut self,
    stores: (&mut Store, &mut VcsStores),
    moves: Vec<RecordedMove>,
  ) -> Result<(), GitRefused> {
    let (repository, push, prepared) = (self.repository, &self.push, &mut self.prepared);
    let admitted = lent_to_read(stores, |stores| {
      let (content, store) = stores.git_ref(repository).ok_or(GitRefused::NoStore)?;
      moves
        .iter()
        .map(|(green, _, head)| {
          let taken = head.and_then(|to| {
            let current = store.repository.current_head(*green);
            let at = prepared
              .iter()
              .position(|admitted| admitted.to() == Some(to) && admitted.from_head() == current)?;
            Some(prepared.swap_remove(at).of_green(*green))
          });
          match taken {
            Some(admitted) => Ok(admitted),
            None => store
              .repository
              .admit(content, push, *green, *head)
              .map_err(GitRefused::Store),
          }
        })
        .collect::<Result<Vec<Move>, GitRefused>>()
    })?;
    self.moves = admitted;
    self.keyed = moves;
    Ok(())
  }

  /// Gives back every hold the push took: a push no append committed. One a commit took holds nothing.
  pub(crate) fn abandon(self, state: &mut ShardState) {
    abandon_push(state, self.push);
  }

  /// One turn of the push's stage made again where a landing overtook it since its seal (A-722 part 4e), begun where
  /// `staging` holds none, for `ns` of the shard's clock and a unit at least: whether it is current, so its append's
  /// commit adopts it rather than staging it again whole. A refusal ends it, the commit staging it and answering the
  /// refusal there.
  pub(crate) fn restage_turn(
    &mut self,
    state: &mut ShardState,
    (staging, ns): (&mut Option<slates_git::store::Staging>, u64),
  ) -> bool {
    let repository = self.repository;
    let push = &mut self.push;
    // Nothing to stage again, and none under way: current at once, no room reckoned (each decision step asks).
    let room = match state.vcs_stores.git_ref(repository) {
      Some((_, store)) if staging.is_none() && !store.repository.needs_stage(push) => return true,
      Some((_, store)) => store.repository.stage_room(push),
      None => 0,
    };
    let turned = within_budget(state, room, |stores| {
      let (content, store) = stores.git(repository).ok_or(GitRefused::NoStore)?;
      let mut begun = match staging.take() {
        Some(begun) => begun,
        None if !store.repository.needs_stage(push) => return Ok(true),
        None => store
          .repository
          .staging(content, push)
          .map_err(GitRefused::Store)?,
      };
      let mut clock = crate::git_push::ShardClock;
      let quantum = slates_git::store::hook_view::Quantum {
        clock: &mut clock,
        ns,
      };
      let done = store
        .repository
        .stage_turn(content, push, &mut begun, quantum)
        .map_err(GitRefused::Store)?;
      if !done {
        *staging = Some(begun);
      }
      // A stage that a landing overtook while it was made is made again from the next turn.
      Ok::<bool, GitRefused>(done && !store.repository.needs_stage(push))
    });
    turned.unwrap_or(true)
  }

  /// Gives back what a stage under way ([`SealedPush::restage_turn`]) holds: a push dropped mid-decision.
  pub(crate) fn abandon_staging(state: &mut ShardState, staging: slates_git::store::Staging) {
    let given = {
      let lent = Lent::new(&mut state.store, &mut state.vcs_stores);
      lent
        .stores
        .content
        .as_mut()
        .map_or(Ok(()), |content| staging.abandon(content))
    };
    if given.is_err() {
      state.count(RELEASE_REFUSED, 1);
    }
  }

  /// The rank a push without `atomic` decides a command moving a ref to `to` by (A-850, `receive::Judge::rank`): the
  /// generation number of the commit `to` peels to, read through the push and the repository's index, so its moves are
  /// admitted oldest first and each create's search back meets the one below. 0 for an object neither holds or one that
  /// peels to no commit (refused, or decided, in its turn as before).
  pub(crate) fn rank(&self, state: &mut ShardState, to: ObjectId) -> u64 {
    let (repository, push) = (self.repository, &self.push);
    lent_to_read((&mut state.store, &mut state.vcs_stores), |stores| {
      stores
        .git_ref(repository)
        .and_then(|(content, store)| store.repository.peeled_generation(content, push, to))
        .unwrap_or(0)
    })
  }

  /// One turn of the admission of the move of the green keyed `green` to `to` (A-722 part 4c), begun where `admitting`
  /// holds none, for `ns` of the shard's clock and a unit at least: whether it is done, its move kept for its append
  /// ([`SealedPush::admit`]). A refusal ends it, the append admitting the move again and answering the refusal there.
  pub(crate) fn admission_turn(
    &mut self,
    state: &mut ShardState,
    (green, to): (u64, ObjectId),
    (admitting, ns): (&mut Option<slates_git::store::Admitting>, u64),
  ) -> bool {
    let (repository, push, prepared) = (self.repository, &self.push, &mut self.prepared);
    if prepared.iter().any(|admitted| admitted.to() == Some(to)) {
      return true;
    }
    let turned = lent_to_read((&mut state.store, &mut state.vcs_stores), |stores| {
      let (content, store) = stores.git_ref(repository).ok_or(GitRefused::NoStore)?;
      let mut begun = match admitting.take() {
        Some(begun) => begun,
        None => store
          .repository
          .admitting(content, push, green, to)
          .map_err(GitRefused::Store)?,
      };
      let mut clock = crate::git_push::ShardClock;
      let quantum = slates_git::store::hook_view::Quantum {
        clock: &mut clock,
        ns,
      };
      let done = store
        .repository
        .admit_turn(content, push, &mut begun, quantum)
        .map_err(GitRefused::Store)?;
      if done {
        if let Some(admitted) = begun.admitted() {
          prepared.push(admitted);
        }
      } else {
        *admitting = Some(begun);
      }
      Ok::<bool, GitRefused>(done)
    });
    turned.unwrap_or(true)
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

  /// Keeps the pack the client sent as far as `sent` bytes, the push's objects' one copy (`Push::extend_pack`): what it
  /// lacks of it is the end of `input`, the bytes of the call that brought them. The push's first record holds it in
  /// place of its objects' bytes (A-723).
  pub(crate) fn keep_pack_to(&mut self, sent: u64, input: &[u8]) {
    let kept = u64::try_from(self.push.pack().len()).unwrap_or(u64::MAX);
    let owed = usize::try_from(sent.saturating_sub(kept)).unwrap_or(usize::MAX);
    let from = input.len().saturating_sub(owed);
    self.push.extend_pack(input.get(from..).unwrap_or_default());
  }

  /// Admits `objects`, a pack's as its reader resolved them, each held where its entry lies in the push's pack
  /// ([`GitPush::keep_pack_to`]): its bytes named and checked, then kept only as read lately (`Push::keep_read`, taken
  /// whole). Refused at the first object refused; the push keeps what it admitted before, for its caller to abandon.
  pub(crate) fn admit_placed_all(
    &mut self,
    state: &mut ShardState,
    objects: Vec<slates_git::unpack::Object>,
  ) -> Result<(), GitRefused> {
    within_budget(state, 0, |stores| {
      let (content, store) = stores.git(self.repository).ok_or(GitRefused::NoStore)?;
      for object in objects {
        let incoming = Incoming {
          kind: object.kind,
          claimed: Some(object.id),
          content: &object.content,
          depth: object.chain.as_ref().map_or(0, |chain| chain.depth),
        };
        admit_object(
          content,
          store,
          (&mut self.push, &mut self.record),
          (incoming, Some(object.offset)),
        )?;
        self
          .push
          .keep_read(object.offset, object.kind, object.content);
      }
      // What it keeps read lately is charged on the store's room (A-861).
      self.push.true_up_recent(content);
      Ok(())
    })
  }

  /// Admits `objects`, handed whole (the server's own commits), in the order the push brought them: written first as a
  /// pack of whole objects (`slates_git::pack::of_whole_placed`, as git's `pack-objects` writes an object it finds no
  /// delta for), which the push holds as its own, each admitted where its entry lies there as a client's is (A-722's
  /// record v4: every push's objects are a pack's, so its record is that pack and a replay makes the store the live
  /// one made). Refused at the first object refused; the push keeps what it admitted before, for its caller to
  /// abandon.
  pub(crate) fn admit_all(
    &mut self,
    state: &mut ShardState,
    objects: &[Incoming<'_>],
  ) -> Result<(), GitRefused> {
    let format = self.push.format();
    let (pack, offsets) = slates_git::pack::of_whole_placed(
      format,
      objects
        .iter()
        .map(|incoming| (incoming.kind, incoming.content)),
    )
    .map_err(|_| GitRefused::Allocation)?;
    let sent = u64::try_from(self.push.pack().len().saturating_add(pack.len())).unwrap_or(u64::MAX);
    let base = u64::try_from(self.push.pack().len()).unwrap_or(u64::MAX);
    self.keep_pack_to(sent, &pack);
    within_budget(state, 0, |stores| {
      let (content, store) = stores.git(self.repository).ok_or(GitRefused::NoStore)?;
      for (incoming, offset) in objects.iter().zip(offsets) {
        admit_object(
          content,
          store,
          (&mut self.push, &mut self.record),
          (*incoming, Some(base.saturating_add(offset))),
        )?;
      }
      Ok(())
    })
  }

  /// Seals the push whole: [`GitPush::sealing`]'s turns run to their end in one step (a commit the server makes, whose
  /// push is a version's few objects). Refused, the push is abandoned and its holds given back.
  pub(crate) fn seal(self, state: &mut ShardState) -> Result<SealedPush, GitRefused> {
    let mut sealing = self.sealing(state)?;
    loop {
      match sealing.turn(state, u64::MAX)? {
        SealTurn::More(more) => sealing = *more,
        SealTurn::Sealed(sealed) => return Ok(*sealed),
      }
    }
  }

  /// The push's seal begun (A-722 part 4, `slates_git::store::Repository::sealing`): every commit's generation number,
  /// its parents read from the push or the repository (A-614), then its rows written for its landing to adopt (A-722
  /// part 3), a turn at a time ([`SealingPush::turn`]). Each append then admits its own moves ([`SealedPush::admit`]).
  /// Refused, the push is abandoned and its holds given back.
  pub(crate) fn sealing(mut self, state: &mut ShardState) -> Result<SealingPush, GitRefused> {
    let repository = self.repository;
    let push = &mut self.push;
    let begun = within_budget(state, 0, |stores| match stores.git(repository) {
      Some((content, store)) => store
        .repository
        .sealing(content, push)
        .map(|staging| (staging, store.repository.stage_room(push)))
        .map_err(GitRefused::Store),
      None => Err(GitRefused::NoStore),
    });
    match begun {
      Ok((staging, room)) => Ok(SealingPush {
        push: self,
        staging,
        room,
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

/// A git push being sealed a turn at a time (A-722 part 4; [`GitPush::sealing`]).
pub(crate) struct SealingPush {
  /// The push.
  push: GitPush,
  /// Its seal and stage under way.
  staging: slates_git::store::Staging,
  /// Derived: the most its rows could charge the store, lent each turn (`Repository::stage_room`).
  room: u64,
}

/// What a turn of a push's seal left.
pub(crate) enum SealTurn {
  /// More turns remain (boxed: the push is most of its size).
  More(Box<SealingPush>),
  /// Sealed and staged (boxed too: a sealed push is hundreds of bytes).
  Sealed(Box<SealedPush>),
}

impl SealingPush {
  /// One turn of the seal, held to `ns` of the shard's clock and a unit at least, its rows' room lent. Refused, the push
  /// and its staging are given back.
  pub(crate) fn turn(mut self, state: &mut ShardState, ns: u64) -> Result<SealTurn, GitRefused> {
    let repository = self.push.repository;
    let (push, staging) = (&mut self.push.push, &mut self.staging);
    let turned = within_budget(state, self.room, |stores| match stores.git(repository) {
      Some((content, store)) => {
        let mut clock = crate::git_push::ShardClock;
        let quantum = slates_git::store::hook_view::Quantum {
          clock: &mut clock,
          ns,
        };
        store
          .repository
          .stage_turn(content, push, staging, quantum)
          .map_err(GitRefused::Store)
      }
      None => Err(GitRefused::NoStore),
    });
    match turned {
      Ok(false) => Ok(SealTurn::More(Box::new(self))),
      Ok(true) => Ok(SealTurn::Sealed(Box::new(SealedPush {
        repository,
        push: self.push.push,
        moves: Vec::new(),
        keyed: Vec::new(),
        objects: self.push.record,
        prepared: Vec::new(),
        named: None,
        packed: None,
        landed: 0,
      }))),
      Err(refused) => {
        self.abandon(state);
        Err(refused)
      }
    }
  }

  /// Gives back every hold the push and its staging took.
  pub(crate) fn abandon(self, state: &mut ShardState) {
    let given = {
      let lent = Lent::new(&mut state.store, &mut state.vcs_stores);
      lent
        .stores
        .content
        .as_mut()
        .map_or(Ok(()), |content| self.staging.abandon(content))
    };
    if given.is_err() {
      state.count(RELEASE_REFUSED, 1);
    }
    abandon_push(state, self.push.push);
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
/// when an append commits ([`SealedPush::names_now`]), by the predicate that commit gives positions by, since another push can
/// commit an object this one holds between its admission and its append, and a store made again from the log can lose
/// one. So the record names exactly the objects its commit positions.
fn admit_object(
  content: &mut ContentStore,
  store: &GitStore,
  (push, record): (&mut Push, &mut RecordWriter),
  (incoming, offset): (Incoming<'_>, Option<u64>),
) -> Result<(), GitRefused> {
  if let Some(claimed) = incoming.claimed
    && store.repository.object(content, &claimed).is_some()
  {
    // A-864: its entry is still noted, so the one before it ends where it begins and a delta against it is named.
    if let Some(offset) = offset {
      push.entry_at(offset, claimed);
    }
    return Ok(());
  }
  let naming = store.repository.naming();
  // An object of the push's own pack is held where it lies there (`slates_git::store::Push::admit_placed`); one no pack
  // brought (the server's own commits, a replay's) in the content store.
  let (id, admitted) = match offset {
    Some(offset) => push.admit_placed::<_, Infallible>(content, &naming, (incoming, offset)),
    None => push.admit::<_, Infallible>(content, &naming, incoming),
  }
  .map_err(GitRefused::Object)?;
  if admitted == Admitted::New {
    let held = push
      .object(&id)
      .ok_or(GitRefused::Store(StoreRefused::Unreadable { id }))?;
    record.object(Noted {
      kind: incoming.kind,
      id,
      identity: held.identity,
      len: held.len,
    });
  }
  Ok(())
}

/// Whether the repository lacks an object: the predicate a commit gives positions by ([`SealedPush::names_now`]).
fn lacks<'s>(content: &'s ContentStore, store: &'s GitStore) -> impl Fn(&ObjectId) -> bool + 's {
  move |id| store.repository.object(content, id).is_none()
}

/// Seals `push` and admits each of `moves` against the store, in order (A-614's `seal` and `admit`).
fn seal_moves(
  content: &mut ContentStore,
  store: &mut GitStore,
  push: &mut Push,
  moves: &[RecordedMove],
) -> Result<Vec<Move>, GitRefused> {
  store
    .repository
    .seal(content, push)
    .and_then(|()| store.repository.stage(content, push))
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

/// A replay's admitted `moves`, each made at the base its record names in place of the one the heads the replay met
/// gave (A-850, `Repository::based`), so the store made again derives the sets and fragments the live one did. Refused
/// [`GitRefused::Unaligned`] unless there is one base for each move, and as the store refuses a base it could not have
/// been made at.
fn based_moves(
  content: &ContentStore,
  store: &GitStore,
  push: &Push,
  (moves, bases): (Vec<Move>, &[RecordedBase]),
) -> Result<Vec<Move>, GitRefused> {
  if moves.len() != bases.len() {
    return Err(GitRefused::Unaligned);
  }
  moves
    .into_iter()
    .zip(bases)
    .map(|(admitted, base)| store.repository.based(content, push, admitted, *base))
    .collect::<Result<Vec<Move>, StoreRefused>>()
    .map_err(GitRefused::Store)
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
  // A landed push's pack stays resident: its let-go (goal 93 item 2b) is safe only once every git read of the
  // repository brings a let-go window back, and some do not yet. Run r52 (2026-11-22, the big Studio, main 6f6e87d82):
  // once git/git's push ended and its pack was let go, every fetch was refused `Store(Unreadable)` (the advertisement's
  // tag peel among the reads), so stock git read "possible repository corruption" on ls-remote and every clone. So no
  // landed pack is let go or owed here until w-git's read-through covers every reader; the let-go itself
  // (`let_go_landed_pack`, `let_go_owed`) is unchanged and tested directly (read_through's tests), and nothing reaches it
  // from a push. GAPS: goal 93 item 2b.
  Ok(())
}

/// Lets go again the window `window` a read brought back ([`read_through::bring_back`]) once the read has passed it (w-git's
/// fetch look-ahead, goal 93): by the same rules as a landed pack's let-go, a landed window whatever its holders, unless a
/// home holds it here ([`landed_pin`], refused `Pinned`, which its caller leaves). The bytes freed. A window a second
/// fetch still needs is brought back again by that fetch's own read: a read again, never another's bytes.
pub(crate) fn let_go_read(
  state: &mut ShardState,
  window: &slates_vcs::ContentId,
) -> Result<u64, slates_vcs::LetGoRefused> {
  let pin = landed_pin(state.shard, &state.shards, window);
  let freed = within_budget(state, 0, |stores| {
    stores
      .content_mut()
      .ok_or(slates_vcs::LetGoRefused::NotHeld)?
      .let_go_landed(window, pin)
  });
  if let Ok(bytes) = freed {
    state.count(LANDED_LET_GO_BYTES, bytes);
  }
  freed
}

/// Whether repository `id`'s landed packs may be let go now (goal 93 item 2b): no fetch of it answered, no encoding of
/// it under way, and no intake of it under way but `own` (a push's own, as it ends): a push still deciding its refs
/// reads the objects its pack staged (w-git and main 01a20182, 2026-11-22: a non-atomic many-ref push's later refs).
fn landed_let_go_may(state: &ShardState, id: RepositoryId, own: u64) -> bool {
  crate::vcs_share::fetches_live(id) == 0
    && !git_encode::ahead_under_way(state, id)
    && crate::vcs_share::intakes_live(id) <= own
}

/// Lets go of the landed packs repository `id` owes a let-go ([`VcsStores`]'s `landed_owed`): what its going idle runs
/// (`git_encode::give_back_idle`), no fetch of it answered and no encoding under way. Kept owed while a push of it is
/// under way, whose later refs read their objects: its end lets them go ([`let_go_owed_as_push_ends`]).
pub(crate) fn let_go_owed(state: &mut ShardState, id: RepositoryId) {
  if crate::vcs_share::intakes_live(id) > 0 {
    return;
  }
  let_go_owed_now(state, id);
}

/// Lets go of the landed packs repository `id` owes a let-go as a push of it ends (its `ReceivePack` dropped, its own
/// intake still counted): where no other intake, no fetch and no encoding of it is under way; else the last of them to
/// end does, or its going idle.
pub(crate) fn let_go_owed_as_push_ends(state: &mut ShardState, id: RepositoryId) {
  if state.vcs_stores.landed_owed.contains_key(&id) && landed_let_go_may(state, id, 1) {
    let_go_owed_now(state, id);
  }
}

/// [`let_go_owed`]'s let-go, its conditions met.
fn let_go_owed_now(state: &mut ShardState, id: RepositoryId) {
  let owed = state.vcs_stores.landed_owed.remove(&id).unwrap_or_default();
  for pack in owed {
    let_go_landed_pack(state, &pack);
  }
}

/// Lets go of the windows of `pack`, a landed push's pack as its staging put it in the shard's content store, now that
/// the node's store holds it as the push's changeset (goal 93 item 2b; main 01a20182's rulings of 2026-11-22 05:45 and
/// 06:58): every window but those a home holds here ([`landed_pin`]), each brought back by its pack's identity when a
/// read meets it ([`read_through::bring_back`]). Counted with its bytes, and the windows kept pinned.
fn let_go_landed_pack(state: &mut ShardState, pack: &slates_vcs::ContentId) {
  let shards = state.shards.clone();
  let_go_landed_pack_among(state, pack, &shards);
}

/// [`let_go_landed_pack`] with the node's shards `shards`, which rank each window's home ([`landed_pin`]).
pub(crate) fn let_go_landed_pack_among(
  state: &mut ShardState,
  pack: &slates_vcs::ContentId,
  shards: &[u16],
) {
  let shard = state.shard;
  let (freed, pinned) = within_budget(state, 0, |stores| {
    stores.content_mut().map_or((0, 0), |content| {
      content.let_go_object_landed(pack, &|window| landed_pin(shard, shards, window))
    })
  });
  if freed > 0 {
    state.count(LANDED_LET_GO_BYTES, freed);
  }
  if pinned > 0 {
    state.count(LANDED_PINNED, pinned);
  }
}

/// The holder of `window` on shard `shard` (of the node's `shards`) that pins it once landed: a home, which reads it by
/// its identity alone and cannot bring it back (`homes::read_here`, the region's serve and the healer; main 01a20182's
/// ruling of 2026-11-22 07:00 (ii)), where the window's home is this shard or the shard holds it for the region. Read by
/// where the identity's home is, so it holds across a restart, whatever was held before it. None for any other window.
pub(crate) fn landed_pin(
  shard: u16,
  shards: &[u16],
  window: &slates_vcs::ContentId,
) -> Option<&'static str> {
  let homed_here = homes::home_of(shards, window) == Some(shard);
  (homed_here || homes::region_holds_here(window) > 0).then_some("homes")
}

/// Format: the status counter of the bytes of landed pushes' packs let go from the shard's content store (goal 93 item
/// 2b), each brought back by a read that meets it.
pub(crate) const LANDED_LET_GO_BYTES: &str = "vcs_store.landed.let_go_bytes";
/// Format: the status counter of landed packs' windows kept because a home holds them here ([`landed_pin`]).
pub(crate) const LANDED_PINNED: &str = "vcs_store.landed.pinned";

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
    let (version, root) = match store.repository.reach_value(content, green) {
      Ok(Some(value)) => value,
      // Admitted with no walk: its sets are its first reader's to derive (A-614 amended), nothing to keep now.
      Ok(None) => continue,
      Err(_) => {
        unkept = unkept.saturating_add(1);
        continue;
      }
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
  record: &Record,
) -> Result<u64, ReplayRefused> {
  push.carried(record.deepest);
  // A version-4 record's names and pack (A-724 piece 8c; A-722's record v4): read by their identity before anything is
  // admitted, and held for the record when nothing holds them (a3: a window restored from the shard image is held for
  // no one until a record holds it again; one fetched from its region homes is held by its fetch).
  if let Some((identity, count)) = record.named {
    record_names::names_from_content(content, &identity, count).map_err(ReplayRefused::Names)?;
  }
  for identity in [record.named, record.packed]
    .into_iter()
    .flatten()
    .map(|(identity, _)| identity)
  {
    // The region's own hold (this shard its home, restored with the image) is not the record's: the record's hold is
    // taken again unless a fetch for this replay put the content, whose hold is the record's (a hold lost at every
    // restart of a home before, found by `daemon::tests::t_a727_repository_windows_survive_a_warm_restart_in_the_shard_image`).
    if content.holders(&identity) <= homes::region_holds_here(&identity) {
      content
        .hold(&identity)
        .map_err(|refused| ReplayRefused::Git(GitRefused::Object(PushRefused::Content(refused))))?;
    }
  }
  let mut remade = RecordWriter::default();
  replay_objects(content, store, (&mut *push, &mut remade), record)?;
  if !recorded_alike(content, store, &remade, record) {
    return Err(ReplayRefused::Diverged);
  }
  let moves = seal_moves(content, store, push, &record.moves)
    .and_then(|moves| based_moves(content, store, push, (moves, &record.bases)))
    .map_err(ReplayRefused::Git)?;
  commit_moves(content, store, push, (&moves, &record.moves)).map_err(ReplayRefused::Git)
}

/// Whether the objects the replay's admission would record now, by the predicate the live append wrote its record by
/// ([`SealedPush::names_now`]), are the names the record's content holds, by type, content identity and length, as sets
/// ([`names_alike`]); none where it names none. This compares the record written again without copying its bytes: a
/// recomputation that differs is refused (§4.16).
fn recorded_alike(
  content: &ContentStore,
  store: &GitStore,
  remade: &RecordWriter,
  record: &Record,
) -> bool {
  if let Some((identity, count)) = record.named {
    return record_names::names_from_content(content, &identity, count)
      .is_ok_and(|names| names_alike(content, store, remade, &names));
  }
  // A record that names none: the replay must admit nothing the repository lacks either.
  names_alike(content, store, remade, &[])
}

/// Whether the objects the replay's admission would record now are `names`, a pack record's ([`recorded_alike`]), as
/// sets: the live push admitted its pack's objects as they resolved (those read whole or from a base already read in
/// pack order as the client's bytes arrived, a thin pack's waiting deltas once the pack was whole, in the turns that
/// resolved them), and a replay resolves them in turns of its own, so the same objects can come in another order (A-722:
/// a thin pack whose waiting delta came first was refused as diverged, and its repository's store with it). A push
/// holds an object once, so each name is once on each side. The two lists are kept to sort them: one name an object.
fn names_alike(
  content: &ContentStore,
  store: &GitStore,
  remade: &RecordWriter,
  names: &[(slates_git::object_name::Kind, slates_vcs::ContentId, u64)],
) -> bool {
  let mut made: Vec<(u8, slates_vcs::ContentId, u64)> = remade
    .lacked(lacks(content, store))
    .map(|noted| (noted.kind.pack_number(), noted.identity, noted.len))
    .collect();
  if made.len() != names.len() {
    return false;
  }
  let mut named: Vec<(u8, slates_vcs::ContentId, u64)> = names
    .iter()
    .map(|(kind, identity, len)| (kind.pack_number(), *identity, *len))
    .collect();
  made.sort_unstable();
  named.sort_unstable();
  made == named
}

/// Admits a record's objects, each checked against its identity first, at the record's deepest chain.
fn replay_objects(
  content: &mut ContentStore,
  store: &GitStore,
  (push, remade): (&mut Push, &mut RecordWriter),
  record: &Record,
) -> Result<(), ReplayRefused> {
  if let Some((identity, len)) = record.packed {
    // A version-4 record's pack is read from the store by its identity (A-722's record v4).
    let pack = content
      .object_bytes(&identity, len)
      .ok_or(ReplayRefused::Pack(
        slates_git::unpack::Refusal::Truncated { at: 0 },
      ))?;
    return replay_pack(content, store, (push, remade), &pack);
  }
  // A record that names no pack brought no object.
  Ok(())
}

/// Admits a record's objects (A-723, version 5): its pack read through git's own reader, a reference delta's base from the
/// pack or the store, within the room the content store has left; then each object admitted as a live push admits it,
/// at its place in its chain. The names the record holds are compared after ([`recorded_alike`]), so a pack that makes
/// other objects than the live push recorded is refused.
fn replay_pack(
  content: &mut ContentStore,
  store: &GitStore,
  (push, remade): (&mut Push, &mut RecordWriter),
  pack: &[u8],
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
      .read(pack, &mut admission)
      .map_err(ReplayRefused::Pack)?;
    reader
      .finish_with(&bases, &mut admission)
      .map_err(ReplayRefused::Pack)?
  };
  // Each object is held where it lies in the record's pack, as the live push held it (A-722's record v4: rows by pack
  // place), so the store made again from the log is the live one.
  push.extend_pack(pack);
  for object in &unpacked.objects {
    let incoming = Incoming {
      kind: object.kind,
      claimed: Some(object.id),
      content: &object.content,
      depth: object.chain.as_ref().map_or(0, |chain| chain.depth),
    };
    admit_object(
      content,
      store,
      (&mut *push, &mut *remade),
      (incoming, Some(object.offset)),
    )
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
  if matches!(
    state.vcs_stores.entries.get(&id),
    Some(RepositoryStore::Subversion(_))
  ) {
    loop {
      let replayed =
        svn_fetched_record(state, id, index)?.and_then(|bytes| replay_bytes(state, id, &bytes));
      if replayed.is_err() || !svn_more(state, id, index) {
        return Some(replayed);
      }
    }
  }
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
  let replayed = {
    let lent = Lent::new(&mut *store, vcs_stores);
    replay_into(lent.stores, id, bytes, room)
  };
  let mut whole = settle_in(store, vcs_stores);
  if let Some(svn) = vcs_stores.svn_unlent(id) {
    whole &= settle_records(store, svn);
  }
  if !whole {
    state.count(CHARGE_SHORT, 1);
  }
  Some(replayed.map(|unkept| state.count(VALUE_UNKEPT, unkept)))
}

/// Replays `bytes`, a record read back from the node's store, into repository `id`'s store, as [`replay_record`]
/// replays one the register holds.
fn replay_bytes(
  state: &mut ShardState,
  id: RepositoryId,
  bytes: &[u8],
) -> Result<(), ReplayRefused> {
  let ShardState {
    store, vcs_stores, ..
  } = &mut *state;
  let brought = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  lend(store, vcs_stores, brought);
  let room = vcs_stores.svn_unlent(id).map(|svn| {
    let entries = svn
      .records
      .as_ref()
      .map_or(0, |records| records.index().entries());
    svn_room(store, svn.charged, entries)
  });
  let replayed = {
    let lent = Lent::new(&mut *store, vcs_stores);
    replay_into(lent.stores, id, bytes, room)
  };
  let mut whole = settle_in(store, vcs_stores);
  if let Some(svn) = vcs_stores.svn_unlent(id) {
    whole &= settle_records(store, svn);
  }
  if !whole {
    state.count(CHARGE_SHORT, 1);
  }
  replayed.map(|unkept| state.count(VALUE_UNKEPT, unkept))
}

/// Repository `id`'s Subversion record at register `index`, as read back from the node's store before its replay
/// (C2, [`fetch_svn`]): none past the register's last; refused `SvnReference` where the register's record is not the
/// reference to changeset `index`, and `Unlanded` where it was not read back.
fn svn_fetched_record(
  state: &mut ShardState,
  id: RepositoryId,
  index: usize,
) -> Option<Result<Vec<u8>, ReplayRefused>> {
  let named = state
    .db
    .partition()
    .repositories()
    .object_record(id, index)?;
  let sequence = crate::repository::svn_commit::landing::sequence_of(named);
  if sequence != u64::try_from(index).ok() {
    return Some(Err(ReplayRefused::SvnReference));
  }
  let fetched = state.vcs_stores.svn_fetched.get_mut(&id);
  let next = match fetched {
    Some((at, queue)) if *at == index => queue.pop_front(),
    _ => None,
  };
  if !svn_more(state, id, index) {
    state.vcs_stores.svn_fetched.remove(&id);
  }
  Some(next.ok_or(ReplayRefused::Unlanded))
}

/// Whether repository `id`'s changeset for register `index` holds a record read back and not yet replayed (an import's
/// batch, C2 stage C): its replay goes on before the next register record's.
fn svn_more(state: &ShardState, id: RepositoryId, index: usize) -> bool {
  state
    .vcs_stores
    .svn_fetched
    .get(&id)
    .is_some_and(|(at, queue)| *at == index && !queue.is_empty())
}

/// What repository `id`'s rebuild or recovery must read back from the node's store before its record at register
/// `index` replays (C2): the store, where the daemon holds one, and the changeset's sequence; none for another tool's
/// repository, a revision's replay under way, a record read back already, or past the register's last.
fn svn_to_fetch(
  state: &ShardState,
  id: RepositoryId,
  index: usize,
) -> Option<(Option<slates_store::owner::StoreHandle>, u64)> {
  if !matches!(
    state.vcs_stores.entries.get(&id),
    Some(RepositoryStore::Subversion(_))
  ) || state.vcs_stores.svn_replays.contains_key(&id)
    || state
      .vcs_stores
      .svn_fetched
      .get(&id)
      .is_some_and(|(at, _)| *at == index)
  {
    return None;
  }
  let named = state
    .db
    .partition()
    .repositories()
    .object_record(id, index)?;
  let sequence = crate::repository::svn_commit::landing::sequence_of(named)?;
  Some((state.vcs_disk.clone(), sequence))
}

/// Reads repository `id`'s changeset `sequence`'s records back from the node's store `disk` for its record at register
/// `index` (C2): every `Record` section in order (an import's batch lands its revisions' together, C2 stage C), the
/// creation's changeset its last alone (the store's own creation record comes first); nothing kept where they could not
/// be read, and the replay then refuses it `Unlanded`, counted.
async fn fetch_svn(
  id: RepositoryId,
  index: usize,
  (disk, sequence): (Option<slates_store::owner::StoreHandle>, u64),
) -> Option<(usize, std::collections::VecDeque<Vec<u8>>)> {
  let disk = disk?;
  let read = if sequence == 0 {
    crate::repository::svn_commit::landing::changeset_record(&disk, id, sequence)
      .await
      .map(|record| vec![record])
  } else {
    crate::repository::svn_commit::landing::changeset_records(&disk, id, sequence).await
  };
  match read {
    Ok(records) => Some((index, records.into())),
    Err(why) => {
      let _ = crate::state::with_state(|state| {
        slates_machine::log_line!(
          "slates-server: partition {}: a Subversion changeset {sequence} was not read back from the store: {why:?}",
          state.partition
        );
      });
      None
    }
  }
}

/// A paced turn (A-783, the one primitive every paced path uses: a fetch's answer, A-745 rule 2; a git version's
/// encoding, A-718): its steps run while the
/// turn's quantum has room for the next one's estimated cost, and at the quantum's end it asks once whether anyone else
/// waits on the shard: if someone does it yields there; if no one does it takes another quantum, up to the shard's
/// ceiling (the watchdog's look). The quantum is a few microseconds (2.2 us on an M4 Max), and a stream's step is one
/// index entry or one chunk, so a turn that yielded at its first quantum took one step: hg-src's stream was about
/// 460,000 turns of 64 bytes or a chunk each, each with its view made and its bytes written (r23 on bbcf0f72f: 15.8 s
/// against stock's 0.44-0.50 s, the serving shard idle two thirds of it). A step's unit is one answer step; its cost
/// is measured across the exchange's turns (`costs`).
pub(crate) fn paced_turn(quantum_ns: u64, began: u64) -> slates_pace::Step {
  let env = slates_pace::Env {
    clock: slates_rt::futures::now_ns,
    waiting: slates_rt::futures::others_waiting,
    quantum_ns,
    ceiling_ns: slates_rt::futures::step_ceiling_ns().unwrap_or(quantum_ns),
    target_ppm: crate::git_push::target_ppm(slates_rt::futures::scheduler_overrun_ns(), quantum_ns),
  };
  slates_pace::Step::begin(env, began)
}

/// The unit kinds a paced turn measures ([`paced_turn`]): one, the turn's step.
pub(crate) type TurnCosts = slates_pace::Costs<1>;

/// The one unit kind of a paced turn: its step (an answer's step, an encoding's advance).
pub(crate) const ANSWER_STEP: usize = 0;

/// Counter: the units a Mercurial record's sliced replay ran (A-732 part 5), each under one lend of the stores and one
/// settle of the charge, as many of the record's steps as its time allowed. Format: a counter name in the status report.
pub(crate) const HG_REPLAY_TURNS: &str = "vcs_store.hg_replay_turns";

/// Replays repository `id`'s Mercurial record at `index` a unit at a time, a unit of `slices` each (A-732; the
/// record's `Replay`): its bytes read from the register afresh at each unit, the room its bytes take lent once and the
/// shard's charge settled after each unit. A unit takes the record's steps until a slice's budget is spent (part 5:
/// found by use, a restart of hg's history ran 259,321 units of one revision each, each lending the stores and settling
/// the charge); none when it has no record there.
async fn replay_hg_sliced(
  state: &mut ShardState,
  id: RepositoryId,
  index: usize,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Option<Result<(), ReplayRefused>> {
  let begun = hg_record_step(state, id, index, None, slices.budget_ns())?;
  let mut replay = match begun {
    Ok(HgStep::Begun(replay)) => replay,
    Ok(HgStep::Stepped(..)) => return Some(Ok(())),
    Err(refused) => return Some(Err(refused)),
  };
  loop {
    match hg_record_step(state, id, index, Some(replay), slices.budget_ns())? {
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

/// [`replay_hg_sliced`]'s one unit over the shard's state (its type's docs): the record's steps taken under one lend
/// until `ns` of the shard's clock pass or the record ends, the first always; none when the register holds no record
/// at `index`. Each unit counted ([`HG_REPLAY_TURNS`]).
fn hg_record_step(
  state: &mut ShardState,
  id: RepositoryId,
  index: usize,
  replay: Option<slates_hg::import::record::Replay>,
  ns: u64,
) -> Option<Result<HgStep, ReplayRefused>> {
  let deadline_ns = slates_rt::futures::now_ns().saturating_add(ns);
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
  let stepped = {
    let lent = Lent::new(&mut *store, vcs_stores);
    hg_step_in(
      lent.stores,
      id,
      HgSource::Record(bytes),
      replay,
      deadline_ns,
    )
  };
  let whole = settle_in(store, vcs_stores);
  if !whole {
    state.count(CHARGE_SHORT, 1);
  }
  state.count(HG_REPLAY_TURNS, 1);
  Some(stepped)
}

/// Counter: the times a changeset's replay wanted payload bytes its window did not hold, each answered by reading the
/// store's pieces (A-767 C1, the piece-reading intake); more than the changesets replayed shows the payload was read a
/// piece at a time, never whole. Format: a counter name in the status report.
pub(crate) const HG_REPLAY_WANTS: &str = "vcs_store.hg_replay_wants";
/// Counter: the most payload bytes a changeset's replay window held at once on this shard, raised to each new high
/// (A-767 C1; R2: a replay holds at most its longest delta and a piece, never its payload whole). Format: a counter name.
pub(crate) const HG_REPLAY_WINDOW_MOST: &str = "vcs_store.hg_replay_window_most";

/// What a changeset's replay step came to ([`hg_piece_step`]).
enum PieceStep {
  /// More follows: the replay under way.
  More(slates_hg::import::record::Replay),
  /// The step needs these bytes of the payload: the replay under way, the window to be filled
  /// ([`fill_window`]).
  Wanted(slates_hg::import::record::Replay, core::ops::Range<u64>),
  /// The step needs a file delta's base from the node's changeset store (A-767 C1): the replay under way, its wants to
  /// be answered ([`crate::vcs_store::hg_view::answer_wants_over`]).
  Store(slates_hg::import::record::Replay),
  /// The changeset is replayed into the store.
  Done,
}

/// [`hg_record_step`] for a changeset on the changeset log (A-767 C4, C1): its `record`, and its payload as far as
/// `window` holds it, framed and taken through the live push's intake under one lend until `ns` of the shard's clock
/// pass, the payload's length (`len`) lent at its first step.
fn hg_piece_step(
  state: &mut ShardState,
  id: RepositoryId,
  (record, window, len): (&[u8], &slates_hg::framing::Window, u64),
  replay: Option<slates_hg::import::record::Replay>,
  ns: u64,
) -> Result<PieceStep, ReplayRefused> {
  let deadline_ns = slates_rt::futures::now_ns().saturating_add(ns);
  let lent = if replay.is_none() { len } else { 0 };
  let ShardState {
    store, vcs_stores, ..
  } = &mut *state;
  lend(store, vcs_stores, lent);
  let stepped = {
    let lent = Lent::new(&mut *store, vcs_stores);
    hg_piece_in(lent.stores, id, (record, window), replay, deadline_ns)
  };
  if !settle_in(store, vcs_stores) {
    state.count(CHARGE_SHORT, 1);
  }
  state.count(HG_REPLAY_TURNS, 1);
  stepped
}

/// [`hg_piece_step`] over the stores.
fn hg_piece_in(
  stores: &mut VcsStores,
  id: RepositoryId,
  (record, window): (&[u8], &slates_hg::framing::Window),
  replay: Option<slates_hg::import::record::Replay>,
  deadline_ns: u64,
) -> Result<PieceStep, ReplayRefused> {
  use slates_hg::import::record::{Progress, Replay};
  let unheld = ReplayRefused::Git(GitRefused::NoStore);
  let content = stores.content.as_mut().ok_or(unheld.clone())?;
  let Some(RepositoryStore::Mercurial(store)) = stores.entries.get_mut(&id) else {
    return Err(unheld);
  };
  let mut replay = match replay {
    Some(replay) => replay,
    None => Replay::begin_sectioned(record).map_err(ReplayRefused::Mercurial)?,
  };
  loop {
    match replay.step_sectioned(window, &mut store.index, content) {
      Ok(Progress::More) if slates_rt::futures::now_ns() < deadline_ns => {}
      Ok(Progress::More) => return Ok(PieceStep::More(replay)),
      Ok(Progress::Wanted(range)) => return Ok(PieceStep::Wanted(replay, range)),
      Ok(Progress::Store) => return Ok(PieceStep::Store(replay)),
      Ok(Progress::Done) => {
        let replayed = replay
          .finish(&mut store.index, content)
          .map_err(ReplayRefused::Mercurial)?;
        return replayed_hg(store, &replayed).map(|_| PieceStep::Done);
      }
      Err(refused) => {
        if replay.abandon(content).is_err() {
          return Err(ReplayRefused::Released);
        }
        return Err(ReplayRefused::Mercurial(refused));
      }
    }
  }
}

/// Puts `wanted` of changeset `reference`'s payload into `window`, read from the node's changeset log a piece at a time
/// (`StoreHandle::piece`, by the payload's identity and length), the bytes before it let go: each fill charged to `room`
/// as it is put in, so the window holds at most its step's bytes and a piece. None where the store cannot give them.
async fn fill_window(
  disk: &slates_store::owner::StoreHandle,
  reference: &slates_hg::import::record::Reference,
  window: &mut slates_hg::framing::Window,
  (wanted, room): (core::ops::Range<u64>, u64),
) -> Option<()> {
  window.keep_from(wanted.start);
  let mut admission = slates_hg::Admission::new(room);
  while window.end() < wanted.end {
    let piece = disk
      .piece(reference.payload, reference.len, window.end())
      .await
      .ok()??;
    if piece.is_empty() {
      return None;
    }
    window.put(&piece, &mut admission).ok()?;
  }
  Some(())
}

/// A filled window counted: one want more, and the window's bytes the shard's new high where they pass it.
fn count_fill(state: &mut ShardState, window: &slates_hg::framing::Window) {
  state.count(HG_REPLAY_WANTS, 1);
  let most = state
    .refusals
    .get(HG_REPLAY_WINDOW_MOST)
    .copied()
    .unwrap_or(0);
  if window.held() > most {
    state.count(HG_REPLAY_WINDOW_MOST, window.held().saturating_sub(most));
  }
}

/// What a git record version 5 names on the node's changeset store (A-767 C5), read before its replay: the changeset,
/// its names' and pack's identities and sizes, and which of them this node's content store already holds.
#[derive(Clone, Copy, Debug)]
struct ChangesetWanted {
  /// The repository's 128-bit id, as the store keys it.
  repository: u128,
  /// The changeset's sequence.
  sequence: u64,
  /// The names' content: identity and count.
  named: (slates_vcs::ContentId, u64),
  /// The pack's content: identity and length.
  packed: (slates_vcs::ContentId, u64),
  /// Whether the content store holds the names already.
  names_held: bool,
  /// Whether the content store holds the pack already.
  pack_held: bool,
}

/// Repository `id`'s record at `index` where it is version 5 naming a changeset (A-767 C5): always for a recovery
/// (`always`, so the changeset the register names is found on the store whatever the shard image restored), else only
/// where the content store lacks its names or its pack (a rebuild).
fn changeset_wanted(
  state: &ShardState,
  id: RepositoryId,
  index: usize,
  always: bool,
) -> Option<ChangesetWanted> {
  let Some(RepositoryStore::Git(git)) = state.vcs_stores.entries.get(&id) else {
    return None;
  };
  let bytes = state
    .db
    .partition()
    .repositories()
    .object_record(id, index)?;
  let record = Record::decode(bytes, git.repository.format()).ok()?;
  if record.sequence == 0 {
    return None;
  }
  let (named, packed) = (record.named?, record.packed?);
  let content = state.vcs_stores.content.as_ref();
  let holds =
    |identity: &slates_vcs::ContentId| content.is_some_and(|content| content.contains(identity));
  let wanted = ChangesetWanted {
    repository: u128::from_be_bytes(id.bytes),
    sequence: record.sequence,
    named,
    packed,
    names_held: holds(&named.0),
    // A pack whose windows a landed push let go (goal 93 item 2b) is not held whole: read again from the node's store,
    // its put bringing them back, so its replay reads every object.
    pack_held: content.is_some_and(|content| content.holds_whole(&packed.0)),
  };
  (always || !wanted.names_held || !wanted.pack_held).then_some(wanted)
}

/// What [`read_changeset`] read: the names' rows and the pack's bytes, each only where the content store lacks it.
#[derive(Debug, Default)]
struct ChangesetRead {
  /// The names' rows.
  names: Option<Vec<u8>>,
  /// The pack.
  pack: Option<Vec<u8>>,
}

/// Reads `wanted`'s changeset from the node's store (A-767 C5): its row and sections, its `Names` section checked
/// against the record's identity, its `Payload` found under the pack's identity and length (the store keys a payload by
/// the plain BLAKE3 of its bytes, which the record's pack identity is); the names and the pack read only where the
/// content store lacks them, the pack a piece at a time. Refused `Unlanded` where any of it is missing or differs.
async fn read_changeset(
  disk: &slates_store::owner::StoreHandle,
  wanted: &ChangesetWanted,
) -> Result<ChangesetRead, ReplayRefused> {
  let unlanded = || ReplayRefused::Unlanded;
  let row = disk
    .changesets(wanted.repository, wanted.sequence, 1)
    .await
    .map_err(|_| unlanded())?
    .into_iter()
    .find(|row| row.sequence == wanted.sequence)
    .ok_or_else(unlanded)?;
  let sections = disk.sections(row).await.map_err(|_| unlanded())?;
  let of = |kind| sections.iter().find(|section| section.section.kind == kind);
  let names_at = of(slates_store::log::SectionKind::Names).ok_or_else(unlanded)?;
  let payload_at = of(slates_store::log::SectionKind::Payload).ok_or_else(unlanded)?;
  if payload_at.section.len != wanted.packed.1 {
    return Err(unlanded());
  }
  let names = match &names_at.inline {
    Some(inline) => inline.clone(),
    None => section_bytes(disk, &names_at.section)
      .await
      .ok_or_else(unlanded)?,
  };
  if slates_vcs::ContentId::of(&names) != wanted.named.0 {
    return Err(unlanded());
  }
  let pack = if wanted.pack_held {
    // Held already: the store's payload row under the record's identity is the check.
    disk
      .piece(wanted.packed.0.0, wanted.packed.1, 0)
      .await
      .map_err(|_| unlanded())?
      .ok_or_else(unlanded)?;
    None
  } else {
    Some(
      payload_bytes(disk, wanted.packed)
        .await
        .ok_or_else(unlanded)?,
    )
  };
  Ok(ChangesetRead {
    names: (!wanted.names_held).then_some(names),
    pack,
  })
}

/// A section's bytes read by its place a piece at a time ([`slates_store::owner::StoreHandle::read`]).
async fn section_bytes(
  disk: &slates_store::owner::StoreHandle,
  section: &slates_store::log::Section,
) -> Option<Vec<u8>> {
  let len = usize::try_from(section.len).ok()?;
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(len).ok()?;
  while bytes.len() < len {
    let at = u64::try_from(bytes.len()).ok()?;
    let piece = disk
      .read(
        section.offset.checked_add(at)?,
        section.len.checked_sub(at)?,
      )
      .await
      .ok()?;
    if piece.is_empty() {
      return None;
    }
    bytes.extend_from_slice(&piece);
  }
  (bytes.len() == len).then_some(bytes)
}

/// A landed payload's bytes read by its identity a piece at a time ([`slates_store::owner::StoreHandle::piece`]).
async fn payload_bytes(
  disk: &slates_store::owner::StoreHandle,
  (identity, len): (slates_vcs::ContentId, u64),
) -> Option<Vec<u8>> {
  let whole = usize::try_from(len).ok()?;
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(whole).ok()?;
  while bytes.len() < whole {
    let at = u64::try_from(bytes.len()).ok()?;
    let piece = disk.piece(identity.0, len, at).await.ok()??;
    if piece.is_empty() {
      return None;
    }
    bytes.extend_from_slice(&piece);
  }
  (bytes.len() == whole).then_some(bytes)
}

/// Puts what [`read_changeset`] read into the shard's content store, each checked against the record's identity and
/// held by its put as one fetched from the region is, and notes the changeset's sequence as landed, so the next push
/// lands at the last one named + 1 (main's ruling of 2026-11-20: a changeset landed past the register's head is
/// dropped, and the next landing takes its sequence). Counted.
fn put_changeset(
  state: &mut ShardState,
  id: RepositoryId,
  wanted: &ChangesetWanted,
  read: ChangesetRead,
) -> Result<(), ReplayRefused> {
  for (bytes, identity) in [(read.names, wanted.named.0), (read.pack, wanted.packed.0)] {
    if let Some(bytes) = bytes
      && put_names(state, &bytes) != Some(identity)
    {
      return Err(ReplayRefused::Unlanded);
    }
  }
  state.vcs_stores.landed_changeset(id, wanted.sequence);
  state.count(CHANGESET_READ, 1);
  Ok(())
}

/// Counter: git changesets a recovery or rebuild read from the node's store before replaying the record naming them
/// (A-767 C5). Format: a counter name.
pub(crate) const CHANGESET_READ: &str = "vcs_store.git_changeset_read";

/// Repository `id`'s record at `index` where it names a changeset on the changeset log (record v7, A-767 C4).
fn hg_reference(
  state: &ShardState,
  id: RepositoryId,
  index: usize,
) -> Option<slates_hg::import::record::Reference> {
  let bytes = state
    .db
    .partition()
    .repositories()
    .object_record(id, index)?;
  slates_hg::import::record::reference_of(bytes)
}

/// Replays changeset `reference` from the node's changeset log into repository `id`'s store, a unit of `slices` a step
/// (A-767 C4; A-732's slicing): its payload read back a piece at a time as the replay wants it (A-767 C1,
/// [`fill_window`]), framed and taken again through the live push's intake.
async fn replay_hg_referenced(
  state: &mut ShardState,
  id: RepositoryId,
  reference: slates_hg::import::record::Reference,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Result<(), ReplayRefused> {
  let disk = state.vcs_disk.clone().ok_or(ReplayRefused::Unlanded)?;
  state.vcs_stores.landed_changeset(id, reference.sequence);
  let record = reference.record();
  let mut window = slates_hg::framing::Window::new(reference.len);
  let mut replay = None;
  loop {
    let stepped = hg_piece_step(
      state,
      id,
      (&record, &window, reference.len),
      replay.take(),
      slices.budget_ns(),
    )?;
    match stepped {
      PieceStep::More(more) => {
        replay = Some(more);
        slices.unit_done().await;
      }
      PieceStep::Wanted(more, wanted) => {
        let room = crate::vcs_share::room(state, id).bytes;
        if fill_window(&disk, &reference, &mut window, (wanted, room))
          .await
          .is_none()
        {
          let _ = with_released(state, more);
          return Err(ReplayRefused::Unlanded);
        }
        count_fill(state, &window);
        replay = Some(more);
      }
      PieceStep::Store(mut more) => {
        let repository = u128::from_be_bytes(id.bytes);
        let answered = match more.store_wants() {
          Some(wants) => hg_view::answer_wants_over(&disk, repository, wants).await,
          None => false,
        };
        if !answered {
          let _ = with_released(state, more);
          return Err(ReplayRefused::Unlanded);
        }
        replay = Some(more);
      }
      PieceStep::Done => return Ok(()),
    }
  }
}

/// Gives a replay under way back: the store's index as it was before it.
fn with_released(
  state: &mut ShardState,
  replay: slates_hg::import::record::Replay,
) -> Result<(), ReplayRefused> {
  let content = state
    .vcs_stores
    .content
    .as_mut()
    .ok_or(ReplayRefused::Released)?;
  replay.abandon(content).map_err(|_| ReplayRefused::Released)
}

/// What a Mercurial replay reads: a register record whole (a changeset on the changeset log is replayed from its
/// payload's pieces, [`hg_piece_step`]).
#[derive(Clone, Copy)]
enum HgSource<'a> {
  /// A record the register holds whole, or an import's segment record read back from the changeset log (A-767 C1 step
  /// 3).
  Record(&'a [u8]),
}

/// [`hg_record_step`] over the stores.
fn hg_step_in(
  stores: &mut VcsStores,
  id: RepositoryId,
  source: HgSource<'_>,
  replay: Option<slates_hg::import::record::Replay>,
  deadline_ns: u64,
) -> Result<HgStep, ReplayRefused> {
  let HgSource::Record(bytes) = source;
  let unheld = ReplayRefused::Git(GitRefused::NoStore);
  let content = stores.content.as_mut().ok_or(unheld.clone())?;
  let Some(RepositoryStore::Mercurial(store)) = stores.entries.get_mut(&id) else {
    return Err(unheld);
  };
  let Some(mut replay) = replay else {
    let begun = slates_hg::import::record::Replay::begin(bytes, &mut store.index, content);
    return begun.map(HgStep::Begun).map_err(ReplayRefused::Mercurial);
  };
  let mut stepped = replay.step(bytes, content);
  while matches!(stepped, Ok(true)) && slates_rt::futures::now_ns() < deadline_ns {
    stepped = replay.step(bytes, content);
  }
  match stepped {
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
    SvnRecord::Revision(revision) if svn_texts_inline(&revision) => {
      Err(ReplayRefused::SvnTextsInline)
    }
    record => {
      let records = store.records.as_mut().ok_or(ReplayRefused::SvnOrder)?;
      records.set_room(room);
      // A revision's stored texts are its changeset's on the node's store, read through it (C2 stage B part 2 (b)): the
      // content store holds none of them for the records.
      records
        .commit_record(record)
        .map_err(ReplayRefused::SvnStep)
    }
  }
}

/// Whether `revision`'s record carries a text its rows name inline, not named stored ([`ReplayRefused::SvnTextsInline`]):
/// the form logs held before C2 stage B part 2 (b). A text whose bytes a row also names as a property list is named
/// stored beside them, so it is not one.
fn svn_texts_inline(revision: &slates_svn::commit::revision::Revision) -> bool {
  revision
    .record
    .changes
    .iter()
    .filter_map(|change| change.text)
    .any(|text| {
      revision.contents.contains_key(&text.identity)
        && !revision.stored.contains_key(&text.identity)
    })
}

/// Replay's end for repository `id`: its index, built whole, merged a slice at a time ([`index_upkeep::built`]). A
/// Subversion repository's stored texts are not held again: they are its changesets' on the node's store, read through
/// it (C2 stage B part 2 (b)).
fn replay_ended(state: &mut ShardState, id: RepositoryId) {
  index_upkeep::built(state, id);
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

/// Makes a new Subversion repository's store from its creation record `created`, in hand (C2): what
/// `CreateRepository` does on its owner shard, where the creation lands on the node's store as the verb is answered,
/// and a recovery reads it back from there.
pub(crate) fn make_created(state: &mut ShardState, record: &RepositoryRecord, created: &[u8]) {
  make(state, record);
  let outcome = replay_bytes(state, record.id, created);
  if replayed(state, record.id, outcome) {
    replay_ended(state, record.id);
  }
}

/// Replays every record repository `id`'s register holds into its store, in log order, stopping at a refused one.
fn replay_all(state: &mut ShardState, id: RepositoryId) {
  let mut index = 0usize;
  while let Some(outcome) = replay_record(state, id, index) {
    if !replayed(state, id, outcome) {
      return;
    }
    index = index.saturating_add(1);
  }
  replay_ended(state, id);
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
    recover_repository(state, &record, slices).await;
  }
  release_unheld_windows(state);
}

/// The repository windows kept as chunks of the shard's chunk store (A-727, `arena_windows`), whether the content store
/// is made over them yet or not; none where the stores keep their windows elsewhere (a test's heap).
pub(crate) fn chunk_windows(stores: &VcsStores) -> Option<&arena_windows::ChunkWindows> {
  let storage = match stores.content.as_ref() {
    Some(content) => content.storage(),
    None => &**stores.window_storage.as_ref()?,
  };
  storage.as_any()?.downcast_ref()
}

/// [`chunk_windows`], mutably.
pub(crate) fn chunk_windows_mut(
  stores: &mut VcsStores,
) -> Option<&mut arena_windows::ChunkWindows> {
  let storage = match stores.content.as_mut() {
    Some(content) => content.storage_mut(),
    None => &mut **stores.window_storage.as_mut()?,
  };
  storage.as_any_mut()?.downcast_mut()
}

/// A content store's lists (each identity with its bytes), names (each identity with its root) and region holds, as a
/// restart restores them.
pub(crate) type NodesToRestore<'a> = (
  &'a [(slates_vcs::ContentId, Vec<u8>)],
  &'a [(slates_vcs::ContentId, slates_vcs::ContentId)],
  &'a [slates_vcs::ContentId],
  &'a [(slates_vcs::ContentId, u64)],
);

/// Takes the repository windows the shard image names back (A-727 slice a3), before the volumes and the records: each
/// chunk's block claimed in the shard's chunk store and its chunk recorded, the windows handed to the content store the
/// first repository's replay makes. Only before a content store is made; counted restored and not.
pub(crate) fn restore_windows(
  state: &mut ShardState,
  pieces: &[(u64, Vec<u8>)],
  (lists, names, region, aways): NodesToRestore<'_>,
) {
  if state.vcs_stores.content.is_some() {
    return;
  }
  // The region holds are taken back with the lists and names, when the content store is made (A-724 piece 8c).
  state.vcs_stores.restored_region = region.to_vec();
  // The windows let go are taken in before the lists that name them, when the content store is made (A-767 C1 step 2b).
  state.vcs_stores.restored_aways = aways.to_vec();
  // The lists and names are taken in when the content store is made over the windows (`VcsStores::content_made`).
  if !lists.is_empty() || !names.is_empty() {
    state.vcs_stores.restored_nodes = Some((lists.to_vec(), names.to_vec()));
  }
  if pieces.is_empty() {
    return;
  }
  let (windows, refused) =
    arena_windows::ChunkWindows::restore(state.config.page, pieces, &mut state.store.content);
  let restored = u64::try_from(pieces.len())
    .unwrap_or(u64::MAX)
    .saturating_sub(refused);
  state.vcs_stores.window_storage = Some(Box::new(windows));
  state.count(WINDOWS_RESTORED, restored);
  state.count(WINDOWS_UNRESTORED, refused);
}

/// Frees the restored windows no replayed record named (A-727 slice a3), once every record is replayed, and settles the
/// shard's charge.
fn release_unheld_windows(state: &mut ShardState) {
  let (restored, refused) = std::mem::take(&mut state.vcs_stores.nodes_restored);
  state.count(NODES_RESTORED, restored);
  state.count(NODES_UNRESTORED, refused);
  count_region_restored(state);
  let freed = {
    let lent = Lent::new(&mut state.store, &mut state.vcs_stores);
    lent
      .stores
      .content
      .as_mut()
      .map_or(0, ContentStore::release_unheld)
  };
  if freed > 0 {
    state.count(WINDOWS_UNHELD, freed);
  }
  settle(state);
}

/// A Mercurial record `index` replayed a revision a unit (A-732): by the changeset its reference names, or sliced from
/// the register's record. None past the last.
async fn hg_record_replayed(
  state: &mut ShardState,
  (id, index): (RepositoryId, usize),
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Option<Result<(), ReplayRefused>> {
  if let Some(run) = hg_run(state, id, index) {
    return Some(replay_hg_run(state, id, run, slices).await);
  }
  match hg_reference(state, id, index) {
    Some(reference) => Some(replay_hg_referenced(state, id, reference, slices).await),
    None => replay_hg_sliced(state, id, index, slices).await,
  }
}

/// Counter: an import's segment record a replay read back from the changeset log and replayed by identity (A-767 C1
/// step 3), no text rebuilt and no payload read. Format: a counter name in the status report.
pub(crate) const HG_REPLAY_SEGMENTS: &str = "vcs_store.hg_replay_segments";
/// Counter: the bytes of the segment records a replay read back: the index's rows by identity, never a text. Format: a
/// counter name.
pub(crate) const HG_REPLAY_RECORD_BYTES: &str = "vcs_store.hg_replay_record_bytes";

/// Repository `id`'s record at `index` where it names an import's run of segment changesets (record v9, A-767 C1 step
/// 3).
fn hg_run(
  state: &ShardState,
  id: RepositoryId,
  index: usize,
) -> Option<slates_hg::import::record::Run> {
  let bytes = state
    .db
    .partition()
    .repositories()
    .object_record(id, index)?;
  slates_hg::import::record::run_of(bytes)
}

/// The `Record` section of repository `repository`'s changeset `sequence`, read back from the node's changeset log: its
/// bytes in place where they fit one piece, else read a piece at a time; none where the log holds no such changeset or
/// section.
async fn segment_record(
  disk: &slates_store::owner::StoreHandle,
  repository: u128,
  sequence: u64,
) -> Option<Vec<u8>> {
  let row = disk
    .changesets(repository, sequence, 1)
    .await
    .ok()?
    .into_iter()
    .find(|row| row.sequence == sequence)?;
  let section = disk
    .sections(row)
    .await
    .ok()?
    .into_iter()
    .find(|at| at.section.kind == slates_store::log::SectionKind::Record)?;
  if let Some(bytes) = section.inline {
    return Some(bytes);
  }
  let len = usize::try_from(section.section.len).ok()?;
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(len).ok()?;
  while bytes.len() < len {
    let read = u64::try_from(bytes.len()).ok()?;
    let piece = disk
      .read(
        section.section.offset.saturating_add(read),
        section.section.len.saturating_sub(read),
      )
      .await
      .ok()?;
    if piece.is_empty() {
      return None;
    }
    bytes.extend_from_slice(&piece);
  }
  Some(bytes)
}

/// [`hg_record_step`] for an import's segment record read back from the changeset log (`bytes`), its length lent at
/// its first step.
fn hg_segment_step(
  state: &mut ShardState,
  id: RepositoryId,
  bytes: &[u8],
  replay: Option<slates_hg::import::record::Replay>,
  ns: u64,
) -> Result<HgStep, ReplayRefused> {
  let deadline_ns = slates_rt::futures::now_ns().saturating_add(ns);
  let lent = if replay.is_none() {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
  } else {
    0
  };
  let ShardState {
    store, vcs_stores, ..
  } = &mut *state;
  lend(store, vcs_stores, lent);
  let stepped = {
    let lent = Lent::new(&mut *store, vcs_stores);
    hg_step_in(
      lent.stores,
      id,
      HgSource::Record(bytes),
      replay,
      deadline_ns,
    )
  };
  if !settle_in(store, vcs_stores) {
    state.count(CHARGE_SHORT, 1);
  }
  state.count(HG_REPLAY_TURNS, 1);
  stepped
}

/// Replays an import's `run` of segment changesets into repository `id`'s store (A-767 C1 step 3; main's ruling of
/// 2026-11-21 01:16): each segment's record read back from the changeset log and its rows appended by identity, a unit
/// of `slices` a step, so a restart reads the index's rows, never a text nor a payload, whatever the history's size.
async fn replay_hg_run(
  state: &mut ShardState,
  id: RepositoryId,
  run: slates_hg::import::record::Run,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Result<(), ReplayRefused> {
  let disk = state.vcs_disk.clone().ok_or(ReplayRefused::Unlanded)?;
  let repository = u128::from_be_bytes(id.bytes);
  for sequence in run.sequences() {
    let bytes = segment_record(&disk, repository, sequence)
      .await
      .ok_or(ReplayRefused::Unlanded)?;
    state.count(HG_REPLAY_SEGMENTS, 1);
    state.count(
      HG_REPLAY_RECORD_BYTES,
      u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    );
    let mut replay = None;
    while let HgStep::Begun(begun) | HgStep::Stepped(Some(begun)) =
      hg_segment_step(state, id, &bytes, replay.take(), slices.budget_ns())?
    {
      replay = Some(begun);
      slices.unit_done().await;
    }
  }
  if let Some(last) = run.last() {
    state.vcs_stores.landed_changeset(id, last);
  }
  Ok(())
}

/// [`replay_hg_run`] for a rebuild's task, which reaches the shard's state a step at a time.
async fn rebuild_hg_run(id: RepositoryId, run: slates_hg::import::record::Run) -> Unit {
  let Some(disk) = crate::state::with_state(|state| state.vcs_disk.clone()).flatten() else {
    return Unit::Refused(ReplayRefused::Unlanded);
  };
  let repository = u128::from_be_bytes(id.bytes);
  for sequence in run.sequences() {
    let Some(bytes) = segment_record(&disk, repository, sequence).await else {
      return Unit::Refused(ReplayRefused::Unlanded);
    };
    crate::state::with_state(|state| {
      state.count(HG_REPLAY_SEGMENTS, 1);
      state.count(
        HG_REPLAY_RECORD_BYTES,
        u64::try_from(bytes.len()).unwrap_or(u64::MAX),
      );
    })
    .unwrap_or_default();
    let mut replay = None;
    loop {
      let stepped = crate::state::with_state(|state| {
        let ns = state.config.step_quantum_ns();
        hg_segment_step(state, id, &bytes, replay.take(), ns)
      });
      match stepped {
        Some(Ok(HgStep::Begun(begun) | HgStep::Stepped(Some(begun)))) => replay = Some(begun),
        Some(Ok(HgStep::Stepped(None))) => break,
        Some(Err(refused)) => return Unit::Refused(refused),
        None => return Unit::Gone,
      }
      slates_rt::futures::yield_now().await;
    }
  }
  if let Some(last) = run.last() {
    crate::state::with_state(|state| state.vcs_stores.landed_changeset(id, last))
      .unwrap_or_default();
  }
  Unit::Replayed
}

/// What [`recover_repository`] reads from the node's store before record `index`'s turn: a Subversion record's bytes
/// (C2: the register names it alone), and a version-5 record's changeset, once, before its first turn (A-767 C5: the
/// register names it, so it is there, or the replay is refused). False once refused: the replay stops.
async fn fetched_before_turn(
  state: &mut ShardState,
  (id, index): (RepositoryId, usize),
  read_index: &mut Option<usize>,
) -> bool {
  if let Some(place) = svn_to_fetch(state, id, index)
    && let Some(fetched) = fetch_svn(id, index, place).await
  {
    state.vcs_stores.svn_fetched.insert(id, fetched);
  }
  if *read_index == Some(index) {
    return true;
  }
  *read_index = Some(index);
  let Some(wanted) = changeset_wanted(state, id, index, true) else {
    return true;
  };
  let read = match state.vcs_disk.clone() {
    Some(disk) => read_changeset(&disk, &wanted).await,
    None => Err(ReplayRefused::Unlanded),
  };
  if let Err(refused) = read.and_then(|read| put_changeset(state, id, &wanted, read)) {
    let _ = replayed(state, id, Err(refused));
    return false;
  }
  true
}

/// Makes `record`'s store again and replays its records in log order, a record a unit of the recovery's slices, then
/// makes each green's map of its head again, a green a unit.
async fn recover_repository(
  state: &mut ShardState,
  record: &RepositoryRecord,
  slices: &mut crate::recovery_slices::RecoverySlices,
) {
  make(state, record);
  let mut index = 0usize;
  // The git record whose changeset was read from the node's store (A-767 C5), so it is read once, before its first turn.
  let mut read_index = None;
  loop {
    // A Mercurial record is replayed a revision a unit (A-732): whole in one unit, a large repository's held its
    // shard past the watchdog's window at every restart.
    let outcome = if record.tool == VcsTool::Mercurial {
      hg_record_replayed(state, (record.id, index), slices).await
    } else {
      if !fetched_before_turn(state, (record.id, index), &mut read_index).await {
        break;
      }
      // A git record carrying a pack is replayed a turn's units a unit of the recovery (A-722 part 4).
      let ns = state.config.step_quantum_ns();
      match record_turn(state, (record.id, index), ns) {
        Some(RecordTurn::More) => {
          slices.unit_done().await;
          continue;
        }
        Some(RecordTurn::Replayed(outcome)) => Some(outcome),
        None => None,
      }
    };
    let Some(outcome) = outcome else {
      replay_ended(state, record.id);
      break;
    };
    if !replayed(state, record.id, outcome) {
      break;
    }
    index = index.saturating_add(1);
    slices.unit_done().await;
  }
  remake_greens(state, record, slices).await;
}

/// Each of `record`'s greens' map of its head made again as the live store holds it (A-664 part 3), and a Subversion
/// green's head texts held again (S4 c1), a green a unit of the recovery's slices.
async fn remake_greens(
  state: &mut ShardState,
  record: &RepositoryRecord,
  slices: &mut crate::recovery_slices::RecoverySlices,
) {
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
    // A Subversion green made again holds none of its head's texts (S4 c1): every one held again, in byte order. And it
    // keeps no version its readers do not name (A-820): a replay past its checkpoint made each one again (P7's finding
    // (2): 434 MB of git-2000's versions kept after a recovery).
    if record.tool == VcsTool::Subversion {
      svn_texts::rehold_head(state, record.id, green);
      crate::repository::svn_commit::fold_to_readers(state, green);
    }
    slices.unit_done().await;
  }
  paths::whole_build::resume_owed(state, record.id);
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
  // window (A-732): its store made empty now, lent to nothing, and its records replayed a step a turn by a task. Its
  // records name contents by identity (A-729 part 2), so the index it had is set aside, holding them, until the replay
  // ends.
  let aside = set_aside(state, id);
  free(state, id);
  let Some(record) = state.db.partition().repositories().repository(id).cloned() else {
    release_aside(state, aside);
    return;
  };
  make(state, &record);
  state.vcs_stores.rebuilding.insert(id);
  let task =
    slates_rt::futures::spawn(replay_sliced(id, aside)).and_then(slates_rt::futures::detach);
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
/// Before a version-4 record's replay begins, its names and its pack are read from the region where this node does not
/// hold them (A-724 piece 8c; A-722's record v4; [`names_to_fetch`]): each put into the content store, so the replay
/// finds them held. One that no home answers is left missing, and the replay refuses it typed.
async fn rebuild_turns(id: RepositoryId, generation: u64) {
  loop {
    // A version-5 record's names and pack read from the node's store where this node lacks them (A-767 C5).
    if let Some(Some((wanted, disk))) = crate::state::with_state(|state| {
      let under_way = state.vcs_stores.rebuild_turns.get(&id).copied()?;
      if under_way.0 != generation || state.vcs_stores.git_replays.contains_key(&id) {
        return None;
      }
      let wanted = changeset_wanted(state, id, under_way.1, false)?;
      Some((wanted, state.vcs_disk.clone()?))
    }) {
      let read = read_changeset(&disk, &wanted).await;
      let _ = crate::state::with_state(|state| {
        read.and_then(|read| put_changeset(state, id, &wanted, read))
      });
    }
    while let Some(Some((origin, shards, identity))) =
      crate::state::with_state(|state| names_to_fetch(state, id, generation))
    {
      let fetched =
        homes::read_in_region(origin, &shards, identity, crate::fleet::live_budget()).await;
      let put = crate::state::with_state(|state| match fetched {
        Some(bytes) if put_names(state, &bytes) == Some(identity) => {
          state.count(NAMES_FETCHED, 1);
          true
        }
        _ => {
          state.count(NAMES_UNFETCHED, 1);
          false
        }
      });
      if put != Some(true) {
        break;
      }
    }
    // A Subversion record is read back from the node's store before its replay (C2): the register names it alone.
    let place = crate::state::with_state(|state| {
      let (current, index) = state.vcs_stores.rebuild_turns.get(&id).copied()?;
      (current == generation)
        .then(|| svn_to_fetch(state, id, index).map(|place| (index, place)))
        .flatten()
    })
    .flatten();
    if let Some((index, place)) = place
      && let Some(fetched) = fetch_svn(id, index, place).await
    {
      crate::state::with_state(|state| state.vcs_stores.svn_fetched.insert(id, fetched));
    }
    if crate::state::with_state(|state| rebuild_turn(state, id, generation)) != Some(true) {
      return;
    }
    slates_rt::futures::yield_now().await;
  }
}

/// Counter: a version-4 record's names or pack read from their region homes before its replay (A-724 piece 8c; A-722's
/// record v4). Format: a counter name.
pub(crate) const NAMES_FETCHED: &str = "vcs_store.names_fetched";
/// Counter: a version-4 record's names or pack no region home answered; its replay is refused typed. Format: a counter
/// name.
pub(crate) const NAMES_UNFETCHED: &str = "vcs_store.names_unfetched";

/// A content repository `id`'s rebuild of `generation` must read from the region before its next record's replay begins
/// (A-724 piece 8c; A-722's record v4): the identity of that record's names, else its pack, when it is version 4 and
/// this node's content store does not hold it, with the shard asking and the node's shards; none otherwise (a replay
/// under way, a version-2 record, both held).
fn names_to_fetch(
  state: &ShardState,
  id: RepositoryId,
  generation: u64,
) -> Option<(u16, Vec<u16>, slates_vcs::ContentId)> {
  let (current, index) = state.vcs_stores.rebuild_turns.get(&id).copied()?;
  if current != generation || state.vcs_stores.git_replays.contains_key(&id) {
    return None;
  }
  let Some(RepositoryStore::Git(git)) = state.vcs_stores.entries.get(&id) else {
    return None;
  };
  let bytes = state
    .db
    .partition()
    .repositories()
    .object_record(id, index)?;
  let record = Record::decode(bytes, git.repository.format()).ok()?;
  let content = state.vcs_stores.content.as_ref();
  let identity = [record.named, record.packed]
    .into_iter()
    .flatten()
    .map(|(identity, _)| identity)
    .find(|identity| !content.is_some_and(|content| content.contains(identity)))?;
  Some((state.shard, state.shards.clone(), identity))
}

/// One turn of repository `id`'s rebuild of `generation`: its next record replayed, or, past the last or at a refusal,
/// the rebuild ended ([`rebuild_ended`]). Whether more turns remain; none for a rebuild ended or replaced.
fn rebuild_turn(state: &mut ShardState, id: RepositoryId, generation: u64) -> bool {
  let ns = state.config.step_quantum_ns();
  rebuild_turn_within(state, (id, generation), ns)
}

/// [`rebuild_turn`] held to `ns` of the shard's clock: a git record's replay takes a turn's units at least one
/// ([`git_replay`], A-722 part 4), any other record whole.
fn rebuild_turn_within(
  state: &mut ShardState,
  (id, generation): (RepositoryId, u64),
  ns: u64,
) -> bool {
  let Some((current, index)) = state.vcs_stores.rebuild_turns.get(&id).copied() else {
    return false;
  };
  if current != generation {
    return false;
  }
  let advanced = match record_turn(state, (id, index), ns) {
    Some(RecordTurn::More) => return true,
    Some(RecordTurn::Replayed(outcome)) => replayed(state, id, outcome),
    None => false,
  };
  if advanced {
    state
      .vcs_stores
      .rebuild_turns
      .insert(id, (generation, index.saturating_add(1)));
    return true;
  }
  rebuild_ended(state, id);
  false
}

/// What a turn of a record's replay left.
enum RecordTurn {
  /// A git record's replay under way: more turns remain.
  More,
  /// The record replayed, or refused.
  Replayed(Result<(), ReplayRefused>),
}

/// One turn of repository `id`'s record at `index`, held to `ns` of the shard's clock (A-722 part 4): a git record
/// carrying a pack replayed a turn's units by its [`git_replay::GitReplay`], begun at its first turn with the room its
/// bytes take lent, kept between turns; any other record replayed whole ([`replay_record`]). None when it has no record
/// there.
fn record_turn(
  state: &mut ShardState,
  (id, index): (RepositoryId, usize),
  ns: u64,
) -> Option<RecordTurn> {
  if matches!(
    state.vcs_stores.entries.get(&id),
    Some(RepositoryStore::Subversion(_))
  ) {
    return svn_record_turn(state, (id, index), ns);
  }
  let chunk = usize::try_from(state.config.archive_slice_bytes_within(ns)).unwrap_or(usize::MAX);
  let deadline_ns = slates_rt::futures::now_ns().saturating_add(ns);
  let under_way = state.vcs_stores.git_replays.remove(&id);
  let ShardState {
    db,
    store,
    vcs_stores,
    ..
  } = &mut *state;
  let bytes = db.partition().repositories().object_record(id, index)?;
  let format = match vcs_stores.entries.get(&id) {
    Some(RepositoryStore::Git(git)) => Some(git.repository.format()),
    _ => None,
  };
  let stepped =
    under_way.is_some() || format.is_some_and(|format| git_replay::stepped(bytes, format));
  if !stepped {
    return replay_record(state, id, index).map(RecordTurn::Replayed);
  }
  let lent = if under_way.is_none() {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
  } else {
    0
  };
  lend(store, vcs_stores, lent);
  let turned = {
    let lent = Lent::new(&mut *store, vcs_stores);
    git_turn_in(lent.stores, id, bytes, under_way, (deadline_ns, chunk))
  };
  if !settle_in(store, vcs_stores) {
    state.count(CHARGE_SHORT, 1);
  }
  Some(match turned {
    Ok(git_replay::Stepped::More(replay)) => {
      state.vcs_stores.git_replays.insert(id, replay);
      RecordTurn::More
    }
    Ok(git_replay::Stepped::Done(unkept)) => {
      state.count(VALUE_UNKEPT, unkept);
      RecordTurn::Replayed(Ok(()))
    }
    Err(refused) => RecordTurn::Replayed(Err(refused)),
  })
}

/// [`record_turn`]'s turn of a git record's replay over the stores: begun where none is `under_way`.
fn git_turn_in(
  stores: &mut VcsStores,
  id: RepositoryId,
  bytes: &[u8],
  under_way: Option<Box<git_replay::GitReplay>>,
  (deadline_ns, chunk): (u64, usize),
) -> Result<git_replay::Stepped, ReplayRefused> {
  let unheld = ReplayRefused::Git(GitRefused::NoStore);
  let content = stores.content.as_mut().ok_or(unheld.clone())?;
  let Some(RepositoryStore::Git(store)) = stores.entries.get_mut(&id) else {
    return Err(unheld);
  };
  let replay = match under_way {
    Some(replay) => replay,
    None => git_replay::GitReplay::begin(content, store.repository.format(), bytes)?,
  };
  let turn = git_replay::Turn { deadline_ns, chunk };
  let mut now = slates_rt::futures::now_ns;
  replay.step((content, store), (turn, &mut now))
}

/// Counter: the turns a Subversion revision record's replay took (R4 of Ada's close list), each its rows' texts kept until
/// the turn's time ran, one row at least. Format: a counter name in the status report.
pub(crate) const SVN_REPLAY_TURNS: &str = "vcs_store.svn_replay_turns";

/// One turn of repository `id`'s Subversion record at `index`, held to `ns` of the shard's clock (R4: a recovery
/// replayed a revision's record whole, every text it brought deltified in one step): a revision's replay begun at its
/// first turn (`RevisionReplay`, checked, the room its bytes take lent), kept between turns, its rows' texts kept
/// until the turn's time runs, one row at least; any other record replayed whole ([`replay_record`]). None when it has no
/// record there.
fn svn_record_turn(
  state: &mut ShardState,
  (id, index): (RepositoryId, usize),
  ns: u64,
) -> Option<RecordTurn> {
  let deadline_ns = slates_rt::futures::now_ns().saturating_add(ns);
  let (pending, brought) = match state.vcs_stores.svn_replays.remove(&id) {
    Some(replay) => (SvnPending::UnderWay(replay), 0),
    None => {
      let bytes = match svn_fetched_record(state, id, index)? {
        Ok(bytes) => bytes,
        Err(refused) => return Some(RecordTurn::Replayed(Err(refused))),
      };
      let brought = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
      match SvnRecord::decode(&bytes) {
        Ok(SvnRecord::Revision(revision)) if svn_texts_inline(&revision) => {
          return Some(RecordTurn::Replayed(Err(ReplayRefused::SvnTextsInline)));
        }
        Ok(SvnRecord::Revision(revision)) => (SvnPending::Begin(revision), brought),
        _ => {
          let replayed = replay_bytes(state, id, &bytes);
          return Some(match replayed {
            Ok(()) if svn_more(state, id, index) => RecordTurn::More,
            replayed => RecordTurn::Replayed(replayed),
          });
        }
      }
    }
  };
  let ShardState {
    store, vcs_stores, ..
  } = &mut *state;
  lend(store, vcs_stores, brought);
  let room = vcs_stores.svn_unlent(id).map(|svn| {
    let entries = svn
      .records
      .as_ref()
      .map_or(0, |records| records.index().entries());
    svn_room(store, svn.charged, entries)
  });
  let stepped = {
    let lent = Lent::new(&mut *store, vcs_stores);
    svn_step_in(lent.stores, id, (pending, room), deadline_ns)
  };
  let mut whole = settle_in(store, vcs_stores);
  if let Some(svn) = vcs_stores.svn_unlent(id) {
    whole &= settle_records(store, svn);
  }
  if !whole {
    state.count(CHARGE_SHORT, 1);
  }
  state.count(SVN_REPLAY_TURNS, 1);
  Some(match stepped {
    // The changeset's next revision replays at the next turn (an import's batch, C2 stage C).
    Ok(None) if svn_more(state, id, index) => RecordTurn::More,
    Ok(None) => RecordTurn::Replayed(Ok(())),
    Ok(Some(replay)) => {
      state.vcs_stores.svn_replays.insert(id, replay);
      RecordTurn::More
    }
    Err(refused) => RecordTurn::Replayed(Err(refused)),
  })
}

/// A Subversion revision record's replay as a turn takes it up: to begin from its decoded revision, or under way.
enum SvnPending {
  /// The revision, to check and begin once the turn's room is set.
  Begin(slates_svn::commit::revision::Revision),
  /// Its replay under way.
  UnderWay(slates_svn::migration::RevisionReplay),
}

/// [`svn_record_turn`]'s steps over the stores, within `room` (set first, as [`replay_svn`] sets it before a record's
/// check): the replay while steps remain, none once the revision is whole.
fn svn_step_in(
  stores: &mut VcsStores,
  id: RepositoryId,
  (pending, room): (SvnPending, Option<SvnRoom>),
  deadline_ns: u64,
) -> Result<Option<slates_svn::migration::RevisionReplay>, ReplayRefused> {
  let Some(RepositoryStore::Subversion(store)) = stores.entries.get_mut(&id) else {
    return Err(ReplayRefused::Git(GitRefused::NoStore));
  };
  let records = store.records.as_mut().ok_or(ReplayRefused::SvnOrder)?;
  if let Some(room) = room {
    records.set_room(room);
  }
  let mut replay = match pending {
    SvnPending::UnderWay(replay) => replay,
    SvnPending::Begin(revision) => records
      .replay_revision(revision)
      .map_err(ReplayRefused::SvnStep)?,
  };
  loop {
    match records.replay_step(&mut replay) {
      Ok(Some(_)) => return Ok(None),
      Ok(None) if slates_rt::futures::now_ns() >= deadline_ns => return Ok(Some(replay)),
      Ok(None) => {}
      Err(refusal) => return Err(ReplayRefused::SvnStep(refusal)),
    }
  }
}

/// Gives back what repository `id`'s git record's replay under way holds, where one is: its rebuild ended or replaced.
fn abandon_git_replay(state: &mut ShardState, id: RepositoryId) {
  let Some(replay) = state.vcs_stores.git_replays.remove(&id) else {
    return;
  };
  let given = {
    let lent = Lent::new(&mut state.store, &mut state.vcs_stores);
    lent
      .stores
      .content
      .as_mut()
      .map(|content| replay.abandon(content))
  };
  if !matches!(given, Some(Ok(()))) {
    state.count(RELEASE_REFUSED, 1);
  }
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

/// Waits for repository `id`'s rebuild under way to end, its task reading each Subversion record back from the node's
/// store (C2), then runs every map build: what a test's history running as a task on the shard awaits once it
/// rebuilt a store.
#[cfg(test)]
pub(crate) async fn rebuilt(id: RepositoryId) {
  while crate::state::with_state(|state| state.vcs_stores.rebuild_turns.contains_key(&id))
    == Some(true)
  {
    slates_rt::futures::yield_now().await;
  }
  let _ = crate::state::with_state(paths::whole_build::settle);
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

/// One turn of repository `id`'s git or Subversion rebuild under way, held to `ns` of the shard's clock: whether more
/// remain.
#[cfg(test)]
pub(crate) fn rebuild_turn_once_within(state: &mut ShardState, id: RepositoryId, ns: u64) -> bool {
  state
    .vcs_stores
    .rebuild_turns
    .get(&id)
    .copied()
    .is_some_and(|(generation, _)| rebuild_turn_within(state, (id, generation), ns))
}

/// Repository `id`'s Mercurial index taken out of its store, still holding every content it names (A-729 part 2): its
/// records name contents by identity, never their bytes, so a rebuild that replays them must find them held, and this
/// holds them until the replay ends ([`release_aside`]). None for another tool's store, or none at all.
pub(crate) fn set_aside(
  state: &mut ShardState,
  id: RepositoryId,
) -> Option<slates_hg::index::HgIndex> {
  match state.vcs_stores.entries.get_mut(&id) {
    Some(RepositoryStore::Mercurial(store)) => Some(core::mem::replace(
      &mut store.index,
      slates_hg::index::HgIndex::new(),
    )),
    _ => None,
  }
}

/// Gives back an index [`set_aside`] kept, its holds released; a refused release counted.
pub(crate) fn release_aside(state: &mut ShardState, aside: Option<slates_hg::index::HgIndex>) {
  let Some(index) = aside else {
    return;
  };
  let released = within_budget(state, 0, |stores| {
    stores
      .content_mut()
      .map_or(Ok(()), |content| index.release(content))
  });
  if released.is_err() {
    state.count(RELEASE_REFUSED, 1);
  }
  settle(state);
}

/// What [`open_record`] made of record `index` before its units.
enum Opened {
  /// A run of segment changesets replayed whole: the next record is due.
  Ran,
  /// The replay ends here: refused, gone, or every record replayed.
  Stop,
  /// Its units are to be replayed (a sectioned record opened, or one held whole).
  Units,
}

/// Record `index` of repository `id` opened for [`replay_sliced`]: a run of segment changesets replayed whole (A-767 C1
/// step 3); or a record naming a changeset on the changeset log, its reference, the store and the window its payload is
/// read into opened as `sectioned` (A-767 C4, C1); or neither, its units replayed from the register.
async fn open_record(id: RepositoryId, index: usize, sectioned: &mut Option<Sectioned>) -> Opened {
  let run = crate::state::with_state(|state| hg_run(state, id, index)).flatten();
  if let Some(run) = run {
    return match rebuild_hg_run(id, run).await {
      Unit::Replayed => {
        if crate::state::with_state(|state| replayed(state, id, Ok(()))).unwrap_or(false) {
          Opened::Ran
        } else {
          Opened::Stop
        }
      }
      Unit::Refused(refused) => {
        crate::state::with_state(|state| {
          let _ = replayed(state, id, Err(refused));
        })
        .unwrap_or_default();
        Opened::Stop
      }
      Unit::More | Unit::Gone => Opened::Stop,
    };
  }
  let found = crate::state::with_state(|state| {
    hg_reference(state, id, index).map(|reference| (reference, state.vcs_disk.clone()))
  })
  .flatten();
  let Some((reference, disk)) = found else {
    return Opened::Units;
  };
  let Some(disk) = disk else {
    crate::state::with_state(|state| {
      let _ = replayed(state, id, Err(ReplayRefused::Unlanded));
    })
    .unwrap_or_default();
    return Opened::Stop;
  };
  crate::state::with_state(|state| state.vcs_stores.landed_changeset(id, reference.sequence));
  *sectioned = Some(Sectioned {
    record: reference.record(),
    window: slates_hg::framing::Window::new(reference.len),
    reference,
    disk,
  });
  Opened::Units
}

/// Repository `id`'s Mercurial records replayed into its store a step a turn, a yield between (A-732: [`rebuild`]'s
/// task); then the store lent again, its index's generation moved, so a push forked from the store before is re-based.
async fn replay_sliced(id: RepositoryId, aside: Option<slates_hg::index::HgIndex>) {
  let mut index = 0usize;
  let mut replay = None;
  // A record naming a changeset on the changeset log (A-767 C4): its record, its reference, the store and the window
  // its payload is read into a piece at a time as the replay wants it (A-767 C1).
  let mut sectioned: Option<Sectioned> = None;
  loop {
    if replay.is_none() && sectioned.is_none() {
      match open_record(id, index, &mut sectioned).await {
        Opened::Ran => {
          index = index.saturating_add(1);
          slates_rt::futures::yield_now().await;
          continue;
        }
        Opened::Stop => break,
        Opened::Units => {}
      }
    }
    let outcome = match sectioned.as_mut() {
      Some(changeset) => sectioned_unit(id, changeset, &mut replay).await,
      None => record_unit(id, index, &mut replay),
    };
    match outcome {
      Unit::More => {}
      Unit::Replayed => {
        sectioned = None;
        let next = crate::state::with_state(|state| replayed(state, id, Ok(()))).unwrap_or(false);
        if !next {
          break;
        }
        index = index.saturating_add(1);
      }
      Unit::Refused(refused) => {
        crate::state::with_state(|state| {
          let _ = replayed(state, id, Err(refused));
        })
        .unwrap_or_default();
        break;
      }
      Unit::Gone => break,
    }
    slates_rt::futures::yield_now().await;
  }
  crate::state::with_state(|state| {
    state.vcs_stores.rebuilding.remove(&id);
    index_upkeep::built(state, id);
    state.vcs_stores.index_changed(id);
    release_aside(state, aside);
  })
  .unwrap_or_default();
}

/// A changeset of the changeset log a rebuild is replaying (A-767 C4, C1).
struct Sectioned {
  /// Its `Record` section.
  record: Vec<u8>,
  /// Its reference, as the register's entry names it.
  reference: slates_hg::import::record::Reference,
  /// The node's store.
  disk: slates_store::owner::StoreHandle,
  /// Its payload's bytes as far as the replay has wanted them.
  window: slates_hg::framing::Window,
}

/// What one unit of a rebuild's replay came to.
enum Unit {
  /// More follows in the record or changeset under way.
  More,
  /// The record or changeset is replayed.
  Replayed,
  /// The replay is refused.
  Refused(ReplayRefused),
  /// The shard's state is gone.
  Gone,
}

/// One unit of a changeset's replay in a rebuild ([`hg_piece_step`]), its window filled from the store where the step
/// wants bytes.
async fn sectioned_unit(
  id: RepositoryId,
  changeset: &mut Sectioned,
  replay: &mut Option<slates_hg::import::record::Replay>,
) -> Unit {
  let Sectioned {
    record,
    reference,
    disk,
    window,
  } = changeset;
  let stepped = crate::state::with_state(|state| {
    let ns = state.config.step_quantum_ns();
    hg_piece_step(
      state,
      id,
      (record, window, reference.len),
      replay.take(),
      ns,
    )
  });
  match stepped {
    Some(Ok(PieceStep::More(more))) => {
      *replay = Some(more);
      Unit::More
    }
    Some(Ok(PieceStep::Wanted(more, wanted))) => {
      let room =
        crate::state::with_state(|state| crate::vcs_share::room(state, id).bytes).unwrap_or(0);
      if fill_window(disk, reference, window, (wanted, room))
        .await
        .is_none()
      {
        crate::state::with_state(|state| {
          let _ = with_released(state, more);
        })
        .unwrap_or_default();
        return Unit::Refused(ReplayRefused::Unlanded);
      }
      crate::state::with_state(|state| count_fill(state, window)).unwrap_or_default();
      *replay = Some(more);
      Unit::More
    }
    Some(Ok(PieceStep::Store(mut more))) => {
      let repository = u128::from_be_bytes(id.bytes);
      let answered = match more.store_wants() {
        Some(wants) => hg_view::answer_wants_over(disk, repository, wants).await,
        None => false,
      };
      if !answered {
        crate::state::with_state(|state| {
          let _ = with_released(state, more);
        })
        .unwrap_or_default();
        return Unit::Refused(ReplayRefused::Unlanded);
      }
      *replay = Some(more);
      Unit::More
    }
    Some(Ok(PieceStep::Done)) => Unit::Replayed,
    Some(Err(refused)) => Unit::Refused(refused),
    None => Unit::Gone,
  }
}

/// One unit of a register-held record's replay in a rebuild ([`hg_record_step`]).
fn record_unit(
  id: RepositoryId,
  index: usize,
  replay: &mut Option<slates_hg::import::record::Replay>,
) -> Unit {
  let stepped = crate::state::with_state(|state| {
    let ns = state.config.step_quantum_ns();
    hg_record_step(state, id, index, replay.take(), ns)
  });
  match stepped.flatten() {
    Some(Ok(HgStep::Begun(begun) | HgStep::Stepped(Some(begun)))) => {
      *replay = Some(begun);
      Unit::More
    }
    Some(Ok(HgStep::Stepped(None))) => Unit::Replayed,
    Some(Err(refused)) => Unit::Refused(refused),
    None => Unit::Gone,
  }
}

/// Hands `read` the objects a green's owed push reads (P2, `crate::green_ready`): the store of the tool `repository` is
/// in, as a push's work is built from it, handed over rather than returned so a view borrowing the shard's state
/// outlives nothing. For git, the push's own view over an empty push (`PushedObjects`), which reads every object from
/// the repository's committed index once the push that brought them landed: the objects the eager create's build read
/// from the same index and the push's quarantine. Its caller lends the stores room and the shard's chunk store first
/// (`vcs_store::with_room`), as a push's build has them: the pack's windows are read from that chunk store, and its
/// parses are admitted the lent room. None for another tool's repository, or a store not held here, whose owing green
/// answers its readers `ContentUnavailable`. Whether there were any.
pub(crate) fn with_owed_push_objects(
  state: &crate::state::ShardState,
  repository: Option<RepositoryId>,
  read: &mut dyn FnMut(&dyn slates_merge::push::PushObjects),
) -> bool {
  let Some((content, git)) = repository.and_then(|repository| state.vcs_stores.git_ref(repository))
  else {
    return false;
  };
  let empty = Push::new(git.repository.format());
  let objects = slates_git::store::views::PushedObjects::new(&git.repository, content, &empty);
  read(&objects);
  true
}

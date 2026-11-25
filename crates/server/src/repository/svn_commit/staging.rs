//! An import's revisions staged in RAM and landed as one changeset, committed as one register entry (C2 stage C, the
//! records' side; main's rulings of 2026-11-21 01:02 and 08:17; the green's side is A-814's `crate::import_batch`).
//!
//! **Why.** An import landed each revision as its own changeset, one store flush and one register commit each:
//! git-2000's 2,001 revisions took 24.5 s against 8.2 s before the store (GAPS, "C2 stage C waits").
//!
//! **What.** A [`Staging`] holds the repository's landing turn for its life, so no other landing moves the records it
//! advances. Each revision ([`Staging::stage`]) arrives with its texts in the content store already, put by the import's
//! sink as the dump was read and named stored (`crate::vcs_store::svn_import::texts`; goal 93, P7 finding (2)), their
//! holds handed to the batch (`svn_commit::stage_texts`); its copies from older revisions planned (`super::restore`); it is checked and applied to
//! the records in RAM, and its increment, built against the batch's scratch member, staged there (`ImportBatch::stage`).
//! [`Staging::land`] lands one changeset: every text the batch put as its `Payload` with `Texts` rows, then one `Record`
//! section a revision in order. Then it commits every version and the changeset's reference as one register entry
//! (`ImportBatch::commit`), and gives the content store's holds back.
//!
//! **A refusal** before the entry ([`Staging::abandon`]) gives the batch's scratch member and the content store's holds
//! back and makes the records again from the changesets the register names (`crate::vcs_store::rebuild`). The batch's
//! revisions were never committed, never answered, and never visible: a Subversion repository serves no reads while it
//! imports. A crash before the entry leaves a changeset no entry names, which the next landing replaces at its sequence.
//!
//! **Bounded by the import's admission** (main's ruling of 08:17). The batch's increments' post-state bytes are charged
//! to the shard as the import's holding (`vcs_store::svn_import`), beside the texts the content store charges as it
//! takes them. A revision that the shard could not admit flushes the batch first. So under concurrency the batch
//! shrinks by itself, and a flush's cost amortizes over what memory allows. A changeset holds at most [`MAX_RECORDS`]
//! records. And a batch holds no more of its green's retention than the green may: a revision whose reservation would
//! take it past its room (`ImportBatch::room`) is handed to the next batch whole ([`Deferred`], [`Flush::Retention`]),
//! and one past the room alone is refused (A-892; main's red of 2026-11-23, the Studio's git-2000 import); and its
//! chain entries take no more of the partition's green chains than their room when it began
//! (`ImportBatch::chain_room`): a revision whose entry would take it past is handed to the next batch whole
//! ([`Flush::Chain`], A-895).

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_svn::commit::revision::Revision;
use slates_svn::index::ContentIdentity;

use slates_merge::engine::Increment;
use slates_svn::migration::Checked;

use super::{Building, Uncommitted, brought, checked, green_of, grown, identity, landing, restore};
use crate::import_batch::ImportBatch;
use crate::repository::push::EntryParts;
use crate::state::ShardState;

/// Format: the most records one changeset holds: the store's sections are counted in a `u16` (`slates_store::log`),
/// and a changeset that brings texts spends two on its `Payload` and `Texts`.
pub(crate) const MAX_RECORDS: usize = (u16::MAX as usize) - 2;

/// Counter: a batch landed because the next revision's charge would not be admitted. Format: a counter name.
pub(crate) const FLUSHED_ADMISSION: &str = "repository.svn_import.flushed_admission";
/// Counter: a batch landed at [`MAX_RECORDS`]. Format: a counter name.
pub(crate) const FLUSHED_SECTIONS: &str = "repository.svn_import.flushed_sections";
/// Counter: a batch landed because the next revision's reservation would take it past the green's retention room
/// (`ImportBatch::room`). Format: a counter name.
pub(crate) const FLUSHED_RETENTION: &str = "repository.svn_import.flushed_retention";
/// Counter: a batch landed because the next revision's chain entry would take it past its chains' room
/// (`ImportBatch::chain_room`, A-895). Format: a counter name.
pub(crate) const FLUSHED_CHAIN: &str = "repository.svn_import.flushed_chain";
/// Counter: a batch landed at the import's end, or before a record that moves no version. Format: a counter name.
pub(crate) const FLUSHED_END: &str = "repository.svn_import.flushed_end";
/// Counter: a batch landed to its end ([`Staging::land`]): what its stages' times below are summed over (main 01a20182's
/// ruling of 2026-11-25: where an import's per-batch time goes, before any pipelining). Format: a counter name.
pub(crate) const LAND_BATCHES: &str = "repository.svn_import.land_batches";
/// Counter: the nanoseconds batches' changesets took to land durable on the node's store (`landing::land_changeset`,
/// one flush each), summed. Format: a counter name.
pub(crate) const LAND_FLUSH_NS: &str = "repository.svn_import.land_flush_ns";
/// Counter: the nanoseconds batches' entries took to commit to the register (the reference admitted, the batch
/// committed), summed. Format: a counter name.
pub(crate) const LAND_COMMIT_NS: &str = "repository.svn_import.land_commit_ns";
/// Counter: the nanoseconds batches' versions took to apply to the green, a slice a turn (`apply_slices`), summed.
/// Format: a counter name.
pub(crate) const LAND_APPLY_NS: &str = "repository.svn_import.land_apply_ns";
/// Counter: the nanoseconds batches' tails took (`Staging::land_tail`: the green checkpointed and folded, its held table
/// followed, its texts given back and let go, its landing turn given back), summed. Format: a counter name.
pub(crate) const LAND_TAIL_NS: &str = "repository.svn_import.land_tail_ns";
/// Counter: a batch landed because the texts it carries and the next revision's would pass the batch's bound
/// ([`texts_bound`]; goal 93 P7, main 01a20182's ruling of 2026-11-24). Format: a counter name.
pub(crate) const FLUSHED_TEXTS: &str = "repository.svn_import.flushed_texts";

#[cfg(test)]
thread_local! {
  /// A test's import with no texts bound ([`Staging::due_before`] passes over [`Flush::Texts`]), as an import was before
  /// it: the oracle's other leg, the same repository made with a batch's texts unbounded.
  pub(crate) static TEXTS_UNBOUNDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Derived: the most bytes of texts a batch carries before it lands (goal 93 P7; main's rulings of 2026-11-24 and
/// 2026-11-25), rent or buy at the daemon's own exchange of memory against time:
///
/// `bound = clamp(F × M, U, max(U, R / 2))`
///
/// - `F`, the device's durable flush ([`durable_flush_ns`]): what landing a batch costs, the store's own `sync_data` as
///   each batch this shard lands durably took it (`slates_store::owner::take_landed_flush_ns`; the landing's await holds
///   the owner's queue and the writes before the flush, which would overstate it, main's ruling of 2026-11-25), the
///   least so far (w-svn's review). Until one has landed, unknown, and the bound is `U`.
/// - `M`, the machine's memcpy rate, bytes a second, the boot profile's figure as every derived value takes it
///   (`DaemonConfig::memcpy_bytes_per_second`; the profile prices a byte of RAM at the time it takes to move it,
///   `CodecPolicy::byte_worth_from_memcpy`). Holding `B` bytes of texts is worth `B / M` of time, so a batch grows while
///   holding its bytes costs less than the flush it saves: `B < F × M`. Its swing across samples is the profile's.
/// - `U`, the node's changeset store's own unit, one trunk node of its memtable (`vcs_store::hg_import::segment_bytes`:
///   the content plane's chunk times SplinterDB's fanout; 512 KiB at a 4 KiB page, 2 MiB at 16 KiB): the floor, P7's
///   bound before.
/// - `R`, the import's admitted room on its shard ([`import_room`]), halved: the next revision's texts are put before
///   the batch is judged, so an import holds up to twice its bound, which stays within the room (w-svn's review). Under
///   load it shrinks the bound toward `U`.
///
/// Per operation: it scales with the device's flush and copy, never with the history's length or the shard's free
/// memory alone. Measured at git-2000 on a MacBook (F 23.5 ms, M 13.27 GB/s): 312 MB, 2 batches against 180 at `U`,
/// 415 MB resident against 224 MB (debug; main's release rows in BENCHMARKS). One revision's texts past the bound stay one
/// revision, since a revision is staged whole; a batch's texts are given back only once it lands, and the next
/// revision's are put as the dump is read, so an import holds at most twice the larger of the bound and its largest
/// revision's texts.
pub(crate) fn texts_bound(state: &mut ShardState, repository: RepositoryId) -> TextsBound {
  let unit = u64::try_from(crate::vcs_store::hg_import::segment_bytes(state)).unwrap_or(u64::MAX);
  let room = import_room(state, repository);
  let flush_ns = durable_flush_ns();
  let memcpy = state.config.memcpy_bytes_per_second;
  let rent_or_buy = flush_ns.map_or(unit, |flush_ns| {
    u64::try_from(u128::from(flush_ns).saturating_mul(u128::from(memcpy)) / NANOS_A_SECOND)
      .unwrap_or(u64::MAX)
  });
  let bytes = rent_or_buy.clamp(unit, (room / 2).max(unit));
  #[cfg(test)]
  let bytes = TEXTS_BOUND.with(std::cell::Cell::get).unwrap_or(bytes);
  TextsBound {
    bytes,
    flush_ns,
    memcpy,
    unit,
    room,
  }
}

/// Format: nanoseconds in a second, for a rate in bytes a second.
const NANOS_A_SECOND: u128 = 1_000_000_000;

/// A batch's texts bound ([`texts_bound`]) with the inputs it was derived from, logged once an import ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextsBound {
  /// The bound, bytes of texts.
  pub(crate) bytes: u64,
  /// The device's durable flush, nanoseconds; none before this shard landed a batch.
  pub(crate) flush_ns: Option<u64>,
  /// The boot profile's memcpy rate, bytes a second.
  pub(crate) memcpy: u64,
  /// The changeset store's unit, the floor.
  pub(crate) unit: u64,
  /// The import's admitted room, whose half is the cap.
  pub(crate) room: u64,
}

impl std::fmt::Display for TextsBound {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self.flush_ns {
      Some(flush_ns) => write!(
        f,
        "{} B (a durable flush {flush_ns} ns x memcpy {} B/s, within the unit {} B and half the room {} B)",
        self.bytes, self.memcpy, self.unit, self.room
      ),
      None => write!(
        f,
        "{} B (the unit {} B: no durable flush measured yet; memcpy {} B/s, room {} B)",
        self.bytes, self.unit, self.memcpy, self.room
      ),
    }
  }
}

std::thread_local! {
  /// The device's durable flush as this shard measured it ([`durable_flush_ns`]): the least of its batches' changesets
  /// written and flushed on the node's store.
  static DURABLE_FLUSH_NS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// The device's durable flush, nanoseconds ([`texts_bound`]'s `F`): the least the store's own flush took for a batch
/// this shard landed; none before it has.
pub(crate) fn durable_flush_ns() -> Option<u64> {
  #[cfg(test)]
  if let Some(forced) = FLUSH_NS.with(std::cell::Cell::get) {
    return Some(forced);
  }
  DURABLE_FLUSH_NS.with(std::cell::Cell::get)
}

/// Records `flush_ns`, the store's flush of a batch's changeset, as the device's where it is the least this shard met.
fn flushed_in(flush_ns: u64) {
  DURABLE_FLUSH_NS.with(|measured| {
    let least = measured
      .get()
      .map_or(flush_ns, |least| least.min(flush_ns))
      .max(1);
    measured.set(Some(least));
  });
}

/// The room repository `repository`'s import is admitted on its shard now (`vcs_share::room`): alone there, what the
/// shard admits; beside other works, its claim's share of the shard's capacity as the room divides it.
fn import_room(state: &mut ShardState, repository: RepositoryId) -> u64 {
  #[cfg(test)]
  if let Some(room) = IMPORT_ROOM.with(std::cell::Cell::get) {
    return room;
  }
  crate::vcs_share::room(state, repository).bytes
}

#[cfg(test)]
thread_local! {
  /// A test's admitted room for an import in place of the shard's ([`import_room`]): a cut import's batches at a room
  /// it chooses, as a loaded shard's would be.
  pub(crate) static IMPORT_ROOM: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
  /// A test's durable flush in place of the device's ([`durable_flush_ns`]): the bound's `F` at a figure it chooses.
  pub(crate) static FLUSH_NS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
  /// A test's texts bound in place of the derived one ([`texts_bound`]): another host's, whose page, and so whose
  /// changeset store's unit, differs from this one's (a Linux runner's 4 KiB page makes it 512 KiB, a Mac's 16 KiB 2 MiB).
  pub(crate) static TEXTS_BOUND: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
  /// A test's import admission (main's oracle of 2026-11-21 08:17): what the shard would admit the import, less what its
  /// batch holds (its texts and its increments' post-state), where it is smaller than the shard's own. The shard's room
  /// in a test process is shared by every test's daemon, so a tight room made by withholding capacity moved under the
  /// import; this one does not. [`Staging::due_before`] reads it.
  pub(crate) static ADMISSION_CAP: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
  /// A test's kill of a batch's landing once its changeset is durable on the node's store and before its entry is
  /// appended (A-760's crash oracle, main 01a20182's ruling of 2026-11-22 ~11:35): nothing after it runs, as after a
  /// `kill -9`; it clears itself as it fires.
  pub(crate) static KILL_BEFORE_ENTRY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
thread_local! {
  /// A test's kill of a batch's landing after this many slices of its recorded apply (the sliced commit's crash test):
  /// nothing after it runs, as after a `kill -9`; it clears itself as it fires.
  pub(crate) static KILL_AT_COMMIT_SLICE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// The kill [`KILL_BEFORE_ENTRY`] sets, at its point in [`Staging::land`]: where set, it clears itself and never
/// returns.
#[cfg(test)]
async fn kill_before_entry_where_set() {
  if KILL_BEFORE_ENTRY.with(std::cell::Cell::get) {
    KILL_BEFORE_ENTRY.with(|kill| kill.set(false));
    // A kill: nothing after it runs, as after a `kill -9` between the changeset and its entry.
    std::future::pending::<()>().await;
  }
}

/// Counter: the bytes of the content arena's resident free pages an import's batches gave back past their bound
/// ([`give_back_past`]). Format: a counter name in the status report.
pub(crate) const PURGED_PAST_BOUND: &str = "repository.svn_import.purged_past_bound_bytes";

/// Runs `step` a slice a step, yielding between slices, until it answers that it is done: each slice within the shard's
/// budget, as the step bounds itself. None where the shard's state is gone.
async fn until_done(mut step: impl FnMut(&mut ShardState) -> bool) -> Option<()> {
  while !crate::state::with_state(&mut step)? {
    slates_rt::futures::yield_now().await;
  }
  Some(())
}

/// One step of a batch's give-back: the content arena's resident free pages past `keep` bytes given back, an idle
/// purge's slice of blocks (`daemon::purge_slice_blocks`), counted. Whether more is left past `keep`.
fn give_back_past(state: &mut ShardState, keep: usize) -> bool {
  let slice = crate::daemon::purge_slice_blocks(state);
  let purged = state.store.content.arena_mut().purge_past(keep, slice);
  state.count(
    PURGED_PAST_BOUND,
    u64::try_from(purged.bytes).unwrap_or(u64::MAX),
  );
  purged.more && state.store.content.arena().resident_free_bytes() > keep
}

/// A stage of a batch's landing ([`Staging::land`]) a test may kill it at ([`KILL_AT_STAGE`]).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LandStage {
  /// Before its changeset is written on the node's store.
  BeforeChangeset,
  /// Its changeset durable, before its entry is appended.
  BeforeEntry,
  /// Its entry appended, before its versions are applied to the green.
  BeforeApply,
  /// Its versions applied, before its tail (the green's fold, its texts followed, given back and let go).
  BeforeTail,
  /// Landed whole, before the import goes on.
  Landed,
}

/// Every stage of [`LandStage`], in a landing's order.
#[cfg(test)]
pub(crate) const LAND_STAGES: [LandStage; 5] = [
  LandStage::BeforeChangeset,
  LandStage::BeforeEntry,
  LandStage::BeforeApply,
  LandStage::BeforeTail,
  LandStage::Landed,
];

#[cfg(test)]
thread_local! {
  /// A test's kill of a batch's landing at a stage, once as many batches as it names have passed that stage (the cut
  /// import's crash oracle, main's ruling of 2026-11-25): nothing after it runs, as after a `kill -9`; it clears itself
  /// as it fires.
  pub(crate) static KILL_AT_STAGE: std::cell::Cell<Option<(LandStage, usize)>> = const { std::cell::Cell::new(None) };
}

/// The kill [`KILL_AT_STAGE`] sets, at `stage` in [`Staging::land`]: once its batches have passed, it clears itself and
/// never returns.
#[cfg(test)]
async fn kill_at_stage_where_set(stage: LandStage) {
  let fires = KILL_AT_STAGE.with(|kill| match kill.get() {
    Some((at, 0)) if at == stage => {
      kill.set(None);
      true
    }
    Some((at, passing)) if at == stage => {
      kill.set(Some((at, passing - 1)));
      false
    }
    _ => false,
  });
  if fires {
    // A kill: nothing after it runs, as after a `kill -9` at this stage.
    std::future::pending::<()>().await;
  }
}

/// The kill [`KILL_AT_COMMIT_SLICE`] sets, at its point in [`Staging::land`] after `slices` slices of the recorded
/// apply: at that count, it clears itself and never returns.
#[cfg(test)]
async fn kill_at_commit_slice_where_set(slices: usize) {
  if KILL_AT_COMMIT_SLICE.with(|kill| kill.get() == Some(slices)) {
    KILL_AT_COMMIT_SLICE.with(|kill| kill.set(None));
    // A kill: nothing after it runs, as after a `kill -9` between two slices.
    std::future::pending::<()>().await;
  }
}

/// Why a batch lands now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flush {
  /// The next revision's charge would not be admitted.
  Admission,
  /// The changeset holds [`MAX_RECORDS`].
  Sections,
  /// The next revision's reservation would take the batch past the green's retention room.
  Retention,
  /// The next revision's chain entry would take the batch past its chains' room.
  Chain,
  /// The import's end, or a record that moves no version next.
  End,
  /// The texts the batch carries and the next revision's would pass its bound ([`texts_bound`]).
  Texts,
}

impl Flush {
  /// The counter this cause is counted under.
  fn counter(self) -> &'static str {
    match self {
      Flush::Admission => FLUSHED_ADMISSION,
      Flush::Sections => FLUSHED_SECTIONS,
      Flush::Retention => FLUSHED_RETENTION,
      Flush::Chain => FLUSHED_CHAIN,
      Flush::End => FLUSHED_END,
      Flush::Texts => FLUSHED_TEXTS,
    }
  }
}

/// What [`Staging::stage_in`] did with a revision.
enum StagedIn {
  /// Staged: checked, its version judged into the scratch member, the records applied.
  Staged,
  /// Not staged: its reservation would take the batch past the green's retention room (`ImportBatch::room`), or its
  /// chain entry past the batch's chains' room (`ImportBatch::chain_room`): the cause the batch lands for.
  PastRoom(Flush),
}

/// A revision whose texts are put, not yet staged ([`Staging::stage_deferred`]): what a batch with no room for it hands
/// the next, its texts' holds with it.
pub(crate) struct Deferred {
  /// The revision, its texts named stored.
  revision: Revision,
  /// The texts it put into the content store, one hold each.
  put: Vec<ContentIdentity>,
  /// Those of `put` its import's sink pinned unlanded (`svn_import::texts`), unpinned before their holds go.
  pinned: Vec<ContentIdentity>,
  /// What its changeset carries of them.
  carried: Vec<landing::LandedText>,
  /// Why the batch before had no room for it: what that batch lands for.
  cause: Flush,
}

impl Deferred {
  /// Why the batch before had no room for it.
  pub(crate) fn cause(&self) -> Flush {
    self.cause
  }

  /// Gives its texts' holds back: the import refused before it was staged.
  pub(crate) fn give_back(self, state: &mut ShardState) {
    crate::vcs_store::name_holds::unpin_texts(state, &self.pinned);
    super::release_texts(state, None, &self.put);
  }
}

/// The lengths of the texts `revision` names stored.
fn stored_bytes(revision: &Revision) -> u64 {
  revision
    .stored
    .values()
    .fold(0u64, |total, len| total.saturating_add(*len))
}

/// What staging `revision` asks of the shard beyond what it holds already ([`Staging::due_before`]): its contents twice,
/// into the content store and again in its increment's post-state; its texts once more, in the post-state, since the
/// import's sink put them into the content store as the dump was read (A-862), where the shard charged them already,
/// so its admittable bytes count them now. Charged twice, a batch that fits was landed early and the admission's
/// figures lied (main 01a20182's ruling of 2026-11-22 06:22).
fn admission_charge(revision: &Revision) -> u64 {
  brought(revision)
    .saturating_mul(2)
    .saturating_add(stored_bytes(revision))
}

/// Counter: a slice of an import batch's landing tail ([`Staging::land`]): the green's held table followed and the
/// batch's texts given back, each a slice of the shard's budget a step; more than one a landing shows its tail was
/// sliced. Format: a counter name in the daemon's status report.
pub(crate) const TAIL_SLICES: &str = "repository.svn_commit.landing_tail_slices";

/// Where a slice of the shard's step budget begun now ends, on its clock.
fn slice_deadline_ns() -> u64 {
  slates_rt::futures::now_ns().saturating_add(slates_rt::futures::step_budget_ns().unwrap_or(0))
}

/// Brings green `green`'s identity to its head a key at a time (`Green::fold_identity_step`) until the shard's clock
/// passes `deadline_ns`, at least one: whether no key is left (so for a green the shard does not hold).
fn identity_some(state: &mut ShardState, green: DbVolumeId, deadline_ns: u64) -> bool {
  let Some(mut engine) = state.greens.get_mut(&green) else {
    return true;
  };
  loop {
    if engine.fold_identity_step(1) {
      return true;
    }
    if slates_rt::futures::now_ns() >= deadline_ns {
      return false;
    }
  }
}

/// Gives back the landing's hold of each of `put` from `at`, and lets its windows go, until the shard's clock passes
/// `deadline_ns`, at least one: durable on the node's store since the changeset landed, RAM holds nothing of the batch's
/// texts, the green's holds keep their entries (A-760 (b) 4d, main's (B)). `at` moved past those given back; whether
/// every one is.
fn give_back_put_some(
  state: &mut ShardState,
  put: &[ContentIdentity],
  (at, deadline_ns): (&mut usize, u64),
) -> bool {
  while let Some(text) = put.get(*at) {
    *at = at.saturating_add(1);
    let one = core::slice::from_ref(text);
    super::release_texts(state, None, one);
    crate::vcs_store::name_holds::let_go_landed_texts(state, one);
    if slates_rt::futures::now_ns() >= deadline_ns {
      break;
    }
  }
  *at >= put.len()
}

/// Counter: a step of an imported revision's staging ([`Staging::stage_in`]): its check, its build, each slice of its
/// named texts' read, and its judge with the records' apply; four or more a revision shows its staging was split.
/// Format: a counter name in the daemon's status report.
pub(crate) const STAGING_STEPS: &str = "repository.svn_commit.staging_steps";

/// One step of a revision's staging (`Staging::stage_in`) after a yield, counted: `step` within the records' room lent
/// for `lent` bytes, the room settled after. None where the shard's state is gone.
async fn within_records<T>(
  repository: RepositoryId,
  lent: u64,
  step: impl FnOnce(&mut ShardState) -> Result<T, Uncommitted>,
) -> Option<Result<T, Uncommitted>> {
  slates_rt::futures::yield_now().await;
  crate::state::with_state(|state| {
    state.count(STAGING_STEPS, 1);
    if !crate::vcs_store::lend_svn(state, repository, lent) {
      return Err(Uncommitted::NotHeld);
    }
    let stepped = step(state);
    crate::vcs_store::settle_svn(state, repository);
    stepped
  })
}

/// The increment of `checked`, its copies restored by `plans`, built against `batch`'s scratch member at its head and
/// named `work_identity`: refused `Diverged` where the records moved past the batch's head.
fn build_in(
  state: &mut ShardState,
  repository: RepositoryId,
  batch: &mut ImportBatch,
  (checked, plans): (&Checked, restore::CopyPlans),
  work_identity: [u8; 32],
) -> Result<Increment, Uncommitted> {
  let head = batch.head();
  crate::vcs_store::lent_state(state, |state| {
    let ShardState {
      greens, vcs_stores, ..
    } = state;
    let records = vcs_stores
      .subversion_records(repository)
      .ok_or(Uncommitted::NotHeld)?;
    if records.youngest() != head {
      return Err(Uncommitted::Diverged);
    }
    batch
      .with_engine_in(greens, |engine| {
        Building::new(records, engine, (checked, plans)).build(head, work_identity)
      })
      .ok_or(Uncommitted::NotHeld)?
  })
}

/// Where a revision's staging stands between its steps ([`Staging::staged_in`]).
enum StagingAt {
  /// Built, to be judged.
  Judging(Increment),
  /// Its verdict being committed into the scratch member.
  Committing,
  /// Kept in the batch, the records to apply.
  Recording,
}

/// What a revision's judge or commit step left ([`Staging::judged_in`], [`Staging::commit_in`]).
enum JudgedIn {
  /// Kept in the batch, its commit ended.
  Kept,
  /// Its verdict's commit going on.
  Committing,
  /// Past the batch's room: nothing of it staged, for the next batch.
  PastRoom(Flush),
}

/// What a refused staging of a revision comes to (`ImportBatch::stage_begin`, `stage_step`), its records' reservation
/// `growth` given back: past the green's retention room, for the next batch, or refused alone past it; past the batch's
/// chains' room, for the next batch; any other refusal logged and counted as the records' divergence.
fn stage_refused(
  state: &mut ShardState,
  (repository, growth): (RepositoryId, u64),
  refused: crate::import_batch::StageRefused,
) -> Result<Flush, Uncommitted> {
  crate::vcs_store::unreserve_svn(state, repository, growth);
  match refused {
    crate::import_batch::StageRefused::PastRoom {
      reserved: 0,
      work,
      room,
      ..
    } => Err(Uncommitted::PastRetention {
      reserved: work,
      room,
    }),
    crate::import_batch::StageRefused::PastRoom { .. } => Ok(Flush::Retention),
    crate::import_batch::StageRefused::PastChainRoom { .. } => Ok(Flush::Chain),
    refused => {
      slates_machine::log_line!(
        "slates-server: partition {}: an imported revision's version was not staged: {refused:?}",
        state.partition
      );
      state.count(super::DIVERGED, 1);
      Err(Uncommitted::Diverged)
    }
  }
}

/// The post-state bytes `built` holds beside the content store's own charge: its inline bytes and its named runs'.
fn post_bytes(built: &Increment) -> u64 {
  let named = built
    .named
    .runs
    .iter()
    .fold(0u64, |total, run| total.saturating_add(run.len));
  u64::try_from(built.post_state.len())
    .unwrap_or(u64::MAX)
    .saturating_add(named)
}

/// An import's revisions staged since its last landing (module docs).
pub(crate) struct Staging {
  /// The repository's landing turn, held for the batch's life.
  turn: Option<landing::Turn>,
  /// The green's side: the revisions' versions, staged in a scratch member.
  batch: Option<ImportBatch>,
  /// The repository.
  repository: RepositoryId,
  /// The green.
  green: DbVolumeId,
  /// Each revision's record, in order.
  records: Vec<Vec<u8>>,
  /// The texts the batch put into the content store, one hold each, given back once it lands or is abandoned.
  put: Vec<ContentIdentity>,
  /// Those of `put` the import's sink pinned unlanded (`svn_import::texts`): unpinned once the batch's changeset is
  /// durable (its texts landed), or as the batch is abandoned before.
  pinned: Vec<ContentIdentity>,
  /// What the changeset carries of them: a path and a length each.
  carried: Vec<landing::LandedText>,
  /// The staged increments' post-state bytes: the batch's holding beside the content store's own charge.
  held: u64,
  /// The revisions staged since the batch began (what an abandon makes the records again for).
  staged: usize,
  /// The texts the green's held table holds at the paths the batch changes, as it held them before the batch: let go
  /// once the batch commits (S4 c1, `vcs_store::svn_texts`).
  released: Vec<(ContentIdentity, String)>,
  /// The text each path the batch changed holds after it, by path: held once the batch commits.
  holding: std::collections::BTreeMap<String, ContentIdentity>,
  /// The most bytes of texts the batch carries before it lands, read as it began, with its inputs ([`texts_bound`]).
  bound: TextsBound,
}

impl Staging {
  /// A batch of repository `repository`'s import: its landing turn taken and the green's scratch member made. None
  /// where the shard's state is gone; refused `NotHeld` where the green cannot be staged in.
  pub(crate) async fn begin(repository: RepositoryId) -> Option<Result<Staging, Uncommitted>> {
    let turn = landing::turn(repository).await?;
    crate::state::with_state(|state| {
      let bound = texts_bound(state, repository);
      let begun = green_of(state, repository).and_then(|green| {
        ImportBatch::begin(state, green)
          .map(|batch| (green, batch))
          .map_err(|_| Uncommitted::NotHeld)
      });
      match begun {
        Ok((green, batch)) => Ok(Staging {
          turn: Some(turn),
          batch: Some(batch),
          repository,
          green,
          records: Vec::new(),
          put: Vec::new(),
          pinned: Vec::new(),
          carried: Vec::new(),
          held: 0,
          staged: 0,
          released: Vec::new(),
          holding: std::collections::BTreeMap::new(),
          bound,
        }),
        Err(refused) => {
          turn.give_back(state);
          Err(refused)
        }
      }
    })
  }

  /// The green's retention the batch may reserve (`ImportBatch::room`); none where its batch is gone.
  pub(crate) fn room(&self) -> u64 {
    self.batch.as_ref().map_or(0, ImportBatch::room)
  }

  /// The batch's holding beyond what the content store charges: its increments' post-state bytes.
  pub(crate) fn held(&self) -> u64 {
    self.held
  }

  /// Why the batch must land before `revision` is staged, if it must: the changeset is full; the texts the batch
  /// carries and the revision's would pass the batch's bound ([`texts_bound`], derived: an import holds at most twice
  /// the larger of it and its largest revision's texts); or the shard could not admit what the revision brings twice
  /// (its texts in the content store, and again in its increment's post-state). An empty batch never flushes, so a
  /// revision past the bound alone is one batch.
  /// Whether the texts the batch carries and `revision`'s would pass the batch's bound ([`texts_bound`]); never in a
  /// test that unbounds them ([`TEXTS_UNBOUNDED`]).
  fn texts_past_bound(&self, revision: &Revision) -> bool {
    #[cfg(test)]
    if TEXTS_UNBOUNDED.with(std::cell::Cell::get) {
      return false;
    }
    let carried = self
      .carried
      .iter()
      .fold(0u64, |total, text| total.saturating_add(text.len));
    carried.saturating_add(stored_bytes(revision)) > self.bound.bytes
  }

  pub(crate) fn due_before(&self, state: &ShardState, revision: &Revision) -> Option<Flush> {
    if self.records.is_empty() {
      return None;
    }
    if self.records.len() >= MAX_RECORDS {
      return Some(Flush::Sections);
    }
    if self.texts_past_bound(revision) {
      return Some(Flush::Texts);
    }
    let charge = admission_charge(revision);
    let admittable = state.store.admittable();
    // A test's cap counts what the batch holds as the shard's charge does: its texts, its post-state, and the texts
    // the open revision put into the content store already.
    #[cfg(test)]
    let admittable = ADMISSION_CAP
      .with(std::cell::Cell::get)
      .map_or(admittable, |cap| {
        let carried = self
          .carried
          .iter()
          .fold(0u64, |total, text| total.saturating_add(text.len));
        let holds = carried
          .saturating_add(self.held)
          .saturating_add(stored_bytes(revision));
        admittable.min(cap.saturating_sub(holds))
      });
    (charge > admittable).then_some(Flush::Admission)
  }

  /// Stages `revision` (module docs): the texts the import put for it, `imported`, taken with their holds, any of its
  /// contents put into the content store and named stored, its copies planned, then checked, applied to the records in
  /// RAM and its version staged in the batch. None where the shard's state is gone; refused as `land_revision` refuses,
  /// the batch then to be abandoned.
  pub(crate) async fn stage(
    &mut self,
    revision: Revision,
    imported: Vec<ContentIdentity>,
  ) -> Option<Result<Option<Deferred>, Uncommitted>> {
    let repository = self.repository;
    let mut revision = revision;
    let (storing, carried) = match super::stage_texts(repository, &mut revision, imported).await? {
      Ok(staged) => staged,
      Err(refused) => return Some(Err(refused)),
    };
    let deferred = Deferred {
      revision,
      put: storing.put,
      pinned: storing.pinned,
      carried,
      cause: Flush::End,
    };
    self.stage_deferred(deferred).await
  }

  /// Stages `deferred`, a revision whose texts are put already ([`Staging::stage`], or a batch before this one that
  /// had no room for it): the deferred revision again where its reservation would take the batch past the green's
  /// retention room ([`Flush::Retention`]), nothing of it staged, for the caller to land this batch and stage it in the
  /// next. Refused, its texts are the batch's, given back with it.
  pub(crate) async fn stage_deferred(
    &mut self,
    deferred: Deferred,
  ) -> Option<Result<Option<Deferred>, Uncommitted>> {
    let repository = self.repository;
    let Deferred {
      revision,
      put,
      pinned,
      carried,
      ..
    } = deferred;
    // A stored text a row names that the content store let go, and the texts its copies set back, are brought back
    // whole for the increment, and given back once it is built.
    let BroughtForStaging {
      texts: brought_texts,
      plans,
      restoring,
    } = match brought_for_staging(repository, &revision).await? {
      Ok(brought) => brought,
      Err(refused) => {
        self.put.extend(put);
        self.pinned.extend(pinned);
        self.carried.extend(carried);
        return Some(Err(refused));
      }
    };
    // Kept for the next batch, where this one has no room for it.
    let kept = revision.clone();
    // The staging step is a step of its own, never the tail of the texts' last put (goal 93: a revision of 9,000 files
    // put its texts' last slice and staged in one poll of 952 ms on a debug build). The brought texts are held across
    // the yield, and the landing turn keeps every other commit out.
    slates_rt::futures::yield_now().await;
    let staged = self.stage_in(repository, (revision, plans)).await;
    let released = crate::state::with_state(|state| {
      super::release_texts(state, None, &brought_texts);
      super::release_texts(state, None, &restoring);
    });
    let staged = released.and(staged)?;
    match staged {
      Ok(StagedIn::Staged) => {
        self.put.extend(put);
        self.pinned.extend(pinned);
        self.carried.extend(carried);
        Some(Ok(None))
      }
      Ok(StagedIn::PastRoom(cause)) => Some(Ok(Some(Deferred {
        revision: kept,
        put,
        pinned,
        carried,
        cause,
      }))),
      Err(refused) => {
        self.put.extend(put);
        self.pinned.extend(pinned);
        self.carried.extend(carried);
        Some(Err(refused))
      }
    }
  }

  /// [`Staging::stage`]'s steps, each a step of the shard's own and counted ([`STAGING_STEPS`]; goal 93: a revision of
  /// 9,000 files staged in one step of 476 ms on a debug build): the record checked; the increment built against the
  /// scratch member; the texts it names read a copy slice a step; then the increment judged, its verdict committed into
  /// the scratch member a slice a step, and the records applied with the held table followed (A-920). A drop between
  /// steps drops the batch, its scratch member given back unread; a refusal abandons it. Each step lends the records'
  /// room and settles it. The landing turn keeps every other commit out between steps, so the records stand unchanged
  /// from the judge to their apply, and the build and the judge each check they still stand where the batch does. None
  /// where the shard's state is gone.
  async fn stage_in(
    &mut self,
    repository: RepositoryId,
    (revision, plans): (Revision, restore::CopyPlans),
  ) -> Option<Result<StagedIn, Uncommitted>> {
    let lent = brought(&revision);
    let checked = within_records(repository, lent, |state| {
      checked(state, repository, revision, None)
    })
    .await?;
    let (checked, record) = match checked {
      Ok(checked) => checked,
      Err(refused) => return Some(Err(refused)),
    };
    let Some(batch) = self.batch.as_mut() else {
      return Some(Err(Uncommitted::NotHeld));
    };
    let built = within_records(repository, lent, |state| {
      build_in(
        state,
        repository,
        batch,
        (&checked, plans),
        identity(&record),
      )
    })
    .await?;
    let mut built = match built {
      Ok(built) => built,
      Err(refused) => return Some(Err(refused)),
    };
    // The texts it names read before the scratch member judges it (A-760), a copy slice a step, held as its post-state
    // is.
    loop {
      slates_rt::futures::yield_now().await;
      let read = crate::state::with_state(|state| {
        state.count(STAGING_STEPS, 1);
        super::read_named_slice(state, &mut built)
      })?;
      match read {
        Ok(true) => break,
        Ok(false) => {}
        Err(refused) => return Some(Err(refused)),
      }
    }
    self
      .staged_in(repository, lent, (checked, record, built))
      .await
  }

  /// [`Staging::stage_in`]'s last steps, each a slice of the shard's budget lending the records' room (A-920 piece 1:
  /// the judge's commit one call of 110 ms at 9,000 files, debug): the increment judged, its verdict committed into the
  /// scratch member a slice at a time, then the records applied and the held table followed, as many of them a step as
  /// its slice holds, so a revision that fits one stages in one step. None where the shard's state is gone.
  async fn staged_in(
    &mut self,
    repository: RepositoryId,
    lent: u64,
    (checked, record, built): (Checked, Vec<u8>, Increment),
  ) -> Option<Result<StagedIn, Uncommitted>> {
    let growth = grown(&checked);
    let post = post_bytes(&built);
    let mut at = StagingAt::Judging(built);
    let mut recording = Some((checked, record, post));
    loop {
      let stepped = within_records(repository, lent, |state| {
        self.staging_step(state, (repository, growth), (&mut at, &mut recording))
      })
      .await?;
      if let Some(ended) = stepped.transpose() {
        return Some(ended);
      }
    }
  }

  /// One of [`Staging::staged_in`]'s steps, from `at`: None while the revision is not yet staged, `at` where it goes
  /// on; else staged, or past the batch's room. The records are applied in the step the commit ends in where its slice
  /// is not spent, else in a step of their own.
  fn staging_step(
    &mut self,
    state: &mut ShardState,
    (repository, growth): (RepositoryId, u64),
    (at, recording): (&mut StagingAt, &mut Option<(Checked, Vec<u8>, u64)>),
  ) -> Result<Option<StagedIn>, Uncommitted> {
    let deadline = slice_deadline_ns();
    let (judged, recording_now) = match std::mem::replace(at, StagingAt::Committing) {
      StagingAt::Judging(built) => (
        self.judged_in(state, (repository, growth, deadline), built)?,
        false,
      ),
      StagingAt::Committing => (
        self.commit_in(state, (repository, growth, deadline))?,
        false,
      ),
      StagingAt::Recording => (JudgedIn::Kept, true),
    };
    match judged {
      JudgedIn::Committing => Ok(None),
      JudgedIn::PastRoom(cause) => Ok(Some(StagedIn::PastRoom(cause))),
      JudgedIn::Kept if !recording_now && slates_rt::futures::now_ns() >= deadline => {
        *at = StagingAt::Recording;
        Ok(None)
      }
      JudgedIn::Kept => {
        let recorded = recording.take().ok_or(Uncommitted::NotHeld)?;
        self.recorded_in(state, repository, recorded).map(Some)
      }
    }
  }

  /// [`Staging::staged_in`]'s judge, within the records' lent room: the records' room for the revision reserved and
  /// its increment judged into the batch's scratch member, kept at once where it commits nothing, else its commit begun
  /// and stepped until `deadline` ([`Staging::commit_in`]).
  fn judged_in(
    &mut self,
    state: &mut ShardState,
    (repository, growth, deadline): (RepositoryId, u64, u64),
    built: Increment,
  ) -> Result<JudgedIn, Uncommitted> {
    let batch = self.batch.as_mut().ok_or(Uncommitted::NotHeld)?;
    crate::vcs_store::reserve_svn(state, repository, growth)
      .map_err(|available| Uncommitted::Budget { available })?;
    match batch.stage_begin(state, built) {
      Ok(Some(_)) => Ok(JudgedIn::Kept),
      Ok(None) => self.commit_in(state, (repository, growth, deadline)),
      Err(refused) => stage_refused(state, (repository, growth), refused).map(JudgedIn::PastRoom),
    }
  }

  /// The scratch commit [`Staging::judged_in`] began, stepped until the shard's clock passes `deadline`, a unit at
  /// least: whether it goes on, or ended, the work kept or past the batch's room.
  fn commit_in(
    &mut self,
    state: &mut ShardState,
    (repository, growth, deadline): (RepositoryId, u64, u64),
  ) -> Result<JudgedIn, Uncommitted> {
    let batch = self.batch.as_mut().ok_or(Uncommitted::NotHeld)?;
    match batch.stage_step(state, &mut || slates_rt::futures::now_ns() >= deadline) {
      None => Ok(JudgedIn::Committing),
      Some(Ok(_)) => Ok(JudgedIn::Kept),
      Some(Err(refused)) => {
        stage_refused(state, (repository, growth), refused).map(JudgedIn::PastRoom)
      }
    }
  }

  /// [`Staging::stage_in`]'s last step, within the records' lent room: the records applied, the held table followed,
  /// the record kept, its increment kept in the batch already.
  fn recorded_in(
    &mut self,
    state: &mut ShardState,
    repository: RepositoryId,
    (checked, record, post): (Checked, Vec<u8>, u64),
  ) -> Result<StagedIn, Uncommitted> {
    let changes = checked.changes().to_vec();
    let before = state
      .vcs_stores
      .subversion_records(repository)
      .map(|records| {
        crate::vcs_store::svn_texts::released_before(records, &changes, records.youngest())
      })
      .unwrap_or_default();
    let applied = state
      .vcs_stores
      .subversion(repository)
      .and_then(|svn| svn.records.as_mut())
      .map(|records| records.apply(checked))
      .ok_or(Uncommitted::NotHeld)?;
    let revision = applied.map_err(Uncommitted::Step)?;
    self.staged = self.staged.saturating_add(1);
    let after = state
      .vcs_stores
      .subversion_records(repository)
      .map(|records| crate::vcs_store::svn_texts::held_after(records, &changes, revision))
      .unwrap_or_default();
    // The held table follows the batch's net effect: a path's text from before the batch let go the first time the
    // batch reaches it; each path's last text held.
    for (identity, path) in before {
      if self.holding.remove(&path).is_none() && !self.released.iter().any(|(_, at)| *at == path) {
        self.released.push((identity, path));
      }
    }
    for (identity, path) in after {
      self.holding.insert(path, identity);
    }
    self.records.push(record);
    self.held = self.held.saturating_add(post);
    Ok(StagedIn::Staged)
  }

  /// Lands the batch (module docs) for `cause`, counted: one changeset, then one register entry by `principal`, its
  /// owner. The revisions it committed; refused, the batch abandoned ([`Staging::abandon`]). None where the shard's
  /// state is gone.
  pub(crate) async fn land(
    mut self,
    principal: &Principal,
    cause: Flush,
  ) -> Option<Result<usize, Uncommitted>> {
    let repository = self.repository;
    let place = crate::state::with_state(|state| landing::landing_place(state, repository))?;
    let (disk, sequence) = match place {
      Ok(place) => place,
      Err(refused) => {
        let _ = crate::state::with_state(|state| self.abandon(state));
        return Some(Err(refused));
      }
    };
    let revisions = self.records.len();
    let (landed, flushing, committing) = self
      .land_changeset_timed(&disk, (repository, sequence))
      .await;
    let recorded = crate::state::with_state(|state| {
      self.record_landed(state, landed, principal, (repository, sequence))
    })?;
    let mut recorded = match recorded {
      Ok(recorded) => recorded,
      Err(refused) => {
        let _ = crate::state::with_state(|state| self.abandon(state));
        return Some(Err(refused));
      }
    };
    let (applying, tailing) = apply_recorded(&mut recorded).await?;
    self.land_tail(cause).await?;
    let ended = slates_rt::futures::now_ns();
    crate::state::with_state(|state| {
      state.count(LAND_BATCHES, 1);
      state.count(LAND_FLUSH_NS, committing.saturating_sub(flushing));
      state.count(LAND_COMMIT_NS, applying.saturating_sub(committing));
      state.count(LAND_APPLY_NS, tailing.saturating_sub(applying));
      state.count(LAND_TAIL_NS, ended.saturating_sub(tailing));
    })?;
    #[cfg(test)]
    kill_at_stage_where_set(LandStage::Landed).await;
    Some(Ok(revisions))
  }

  /// Lands the batch's changeset at `place` on `disk`: every text the batch's entry names in its Payload is durable
  /// before the entry is appended, so a named text outlives every entry naming it (A-760;
  /// `vcs_store::svn_texts::bring_back_named` relies on it). The landing's outcome, and when its flush began and ended.
  async fn land_changeset_timed(
    &mut self,
    disk: &slates_store::owner::StoreHandle,
    place: (RepositoryId, u64),
  ) -> (Result<(), Uncommitted>, u64, u64) {
    let records = std::mem::take(&mut self.records);
    #[cfg(test)]
    kill_at_stage_where_set(LandStage::BeforeChangeset).await;
    let flushing = slates_rt::futures::now_ns();
    let landed = landing::land_changeset(disk, place, records, &self.carried).await;
    let committing = slates_rt::futures::now_ns();
    // The device's durable flush, the least this shard meets: the bound's `F` (`texts_bound`), as the store's own
    // `sync_data` took it, never the landing's await, which holds the owner's queue and the writes before it.
    if landed.is_ok()
      && let Some(flush_ns) = slates_store::owner::take_landed_flush_ns()
    {
      flushed_in(flush_ns);
    }
    #[cfg(test)]
    kill_before_entry_where_set().await;
    #[cfg(test)]
    kill_at_stage_where_set(LandStage::BeforeEntry).await;
    (landed, flushing, committing)
  }

  /// The batch's register entry by `principal`, once its changeset `landed` at `sequence`: the batch's texts unpinned
  /// first, then the entry appended, refused `Diverged` where the register moved past `sequence`. The recorded batch,
  /// whose versions are applied next.
  fn record_landed(
    &mut self,
    state: &mut ShardState,
    landed: Result<(), Uncommitted>,
    principal: &Principal,
    (repository, sequence): (RepositoryId, u64),
  ) -> Result<crate::repository::push::RecordedBatch, Uncommitted> {
    // Landed: the changeset holding every text the batch put is durable on the node's store, so its texts are no
    // unlanded holders now (ruling (B)), whatever the register answers next. Unpinned here, not at the tail: a kill
    // after the entry never reaches the tail, and the texts are landed all the same (w-perf's red at a55d7cfa6,
    // `store_tests::a_subversion_restart_replays_a_named_text_with_one_released_window`).
    if landed.is_ok() {
      crate::vcs_store::name_holds::unpin_texts(state, &std::mem::take(&mut self.pinned));
    }
    landed?;
    if landing::next_sequence(state, repository) != sequence {
      state.count(super::DIVERGED, 1);
      return Err(Uncommitted::Diverged);
    }
    let reference = landing::reference(sequence);
    crate::vcs_store::admit_objects(state, reference.len())
      .map_err(|available| Uncommitted::Budget { available })?;
    let batch = self.batch.take().ok_or(Uncommitted::NotHeld)?;
    let parts = EntryParts {
      objects: reference,
      ..EntryParts::default()
    };
    batch
      .commit(state, (principal, repository), parts)
      .map_err(Uncommitted::Append)
  }

  /// A landed batch's tail, once its versions are applied: counted, its green checkpointed and folded to its readers;
  /// then the per-text work a slice of the shard's budget a step, counted (goal 93: a revision of 9,000 files ran its
  /// tail in one step of 580 ms on a debug build): the green's held table follows the batch, then the batch's texts are
  /// given back and let go; then its landing turn given back. None where the shard's state is gone.
  async fn land_tail(&mut self, cause: Flush) -> Option<()> {
    let green = self.green;
    // The green's identity brought to its head a slice of the shard's budget a step, counted, before the fold reads it
    // (A-910; goal 93: the fold brought 9,000 changed keys' identity to the head in one step of 482 ms, debug).
    until_done(|state| {
      state.count(TAIL_SLICES, 1);
      identity_some(state, green, slice_deadline_ns())
    })
    .await?;
    crate::state::with_state(|state| {
      state.count(cause.counter(), 1);
      crate::vcs_store::svn_import::batch_landed(state, self.repository, (cause, self.bound));
      crate::merge_service::checkpoint_green(state, green);
      super::fold_to_readers(state, green);
    })?;
    let holding: Vec<(ContentIdentity, String)> = std::mem::take(&mut self.holding)
      .into_iter()
      .map(|(path, identity)| (identity, path))
      .collect();
    let mut at = 0usize;
    until_done(|state| {
      state.count(TAIL_SLICES, 1);
      crate::vcs_store::svn_texts::follow_some(
        state,
        green,
        (&self.released, &holding),
        (&mut at, slice_deadline_ns()),
      )
    })
    .await?;
    let mut at = 0usize;
    until_done(|state| {
      state.count(TAIL_SLICES, 1);
      give_back_put_some(state, &self.put, (&mut at, slice_deadline_ns()))
    })
    .await?;
    // The content arena's resident free pages past what the next batch refills given back, a slice a step (goal 93,
    // main's ruling of 2026-11-25: A-742's rule for the content plane; an import never leaves its shard idle, so the idle
    // purge never ran under it, and git-2000's let-go texts stayed resident as free pages, 204 MB of a 438 MB daemon).
    // The keep is the batch's own bound: a batch takes at most that before it lands, so pages kept past it are pages no
    // batch of this import refills (`texts_bound`).
    let keep = usize::try_from(self.bound.bytes).unwrap_or(usize::MAX);
    until_done(|state| !give_back_past(state, keep)).await?;
    crate::state::with_state(|state| {
      self.put.clear();
      if let Some(turn) = self.turn.take() {
        turn.give_back(state);
      }
    })
  }

  /// Gives the batch up (module docs): its scratch member and the content store's holds given back, the records made
  /// again from what the register names, the landing turn given back.
  pub(crate) fn abandon(mut self, state: &mut ShardState) {
    let repository = self.repository;
    if let Some(batch) = self.batch.take() {
      batch.abandon(state);
    }
    crate::vcs_store::name_holds::unpin_texts(state, &std::mem::take(&mut self.pinned));
    super::release_texts(state, None, &self.put);
    self.put.clear();
    if self.staged > 0 {
      crate::vcs_store::rebuild(state, repository);
    }
    if let Some(turn) = self.turn.take() {
      turn.give_back(state);
    }
  }
}

/// What a revision's staging brings back whole before it stages ([`brought_for_staging`]).
struct BroughtForStaging {
  /// The stored texts its rows name that the content store had let go.
  texts: Vec<ContentIdentity>,
  /// Its copies' restore plans.
  plans: restore::CopyPlans,
  /// The texts those plans set back.
  restoring: Vec<ContentIdentity>,
}

/// What staging `revision` brings back whole first ([`Staging::stage_deferred`]): the stored texts its rows name that the
/// content store let go, its copies' restore plans, and the texts those plans set back, in that order. A refusal gives
/// back what was brought before it; `None` where the store went away.
async fn brought_for_staging(
  repository: RepositoryId,
  revision: &Revision,
) -> Option<Result<BroughtForStaging, Uncommitted>> {
  let texts = match super::bring_stored(repository, &revision.record.changes).await? {
    Ok(brought) => brought,
    Err(refused) => return Some(Err(refused)),
  };
  let plans = match restore::plan(repository, &revision.record.changes).await? {
    Ok(plans) => plans,
    Err(refused) => {
      let _ = crate::state::with_state(|state| super::release_texts(state, None, &texts));
      return Some(Err(refused));
    }
  };
  match super::bring_texts(repository, restore::texts_of(&plans).into_iter()).await? {
    Ok(restoring) => Some(Ok(BroughtForStaging {
      texts,
      plans,
      restoring,
    })),
    Err(refused) => {
      let _ = crate::state::with_state(|state| super::release_texts(state, None, &texts));
      Some(Err(refused))
    }
  }
}

/// A recorded batch applied: the batch is the register's now, its versions applied to the green a slice a turn
/// (`RecordedBatch`), the landing turn and the import's read gate held until the last; a kill between slices recovers
/// from the record. When the apply began and ended; None where the shard's state is gone.
async fn apply_recorded(
  recorded: &mut crate::repository::push::RecordedBatch,
) -> Option<(u64, u64)> {
  #[cfg(test)]
  kill_at_stage_where_set(LandStage::BeforeApply).await;
  let applying = slates_rt::futures::now_ns();
  apply_slices(recorded).await?;
  slates_rt::futures::yield_now().await;
  #[cfg(test)]
  kill_at_stage_where_set(LandStage::BeforeTail).await;
  Some((applying, slates_rt::futures::now_ns()))
}

/// Applies a recorded batch's versions to its green ([`crate::repository::push::RecordedBatch::apply_slice`]), each slice
/// a step of its own and the first apart from the commit (goal 93: a revision of 9,000 files applied its last slice and
/// ran the tail in one poll of 833 ms on a debug build). None where the shard's state is gone.
async fn apply_slices(recorded: &mut crate::repository::push::RecordedBatch) -> Option<()> {
  let mut slices = 0usize;
  loop {
    #[cfg(test)]
    kill_at_commit_slice_where_set(slices).await;
    slates_rt::futures::yield_now().await;
    if crate::state::with_state(|state| recorded.apply_slice(state))?.is_some() {
      return Some(());
    }
    slices = slices.saturating_add(1);
  }
}

#[cfg(test)]
mod charge_tests {
  use std::collections::BTreeMap;

  use slates_svn::commit::revision::Revision;
  use slates_svn::index::RevisionRecord;

  /// Shape: a revision's property bytes, its contents' bytes and its stored texts' lengths, each apart from the others.
  const PROPERTY: (&[u8], &[u8]) = (b"svn:log", b"one");
  /// Shape: see [`PROPERTY`].
  const CONTENT: usize = 100;
  /// Shape: see [`PROPERTY`].
  const STORED: [u64; 2] = [1_000, 10_000];

  /// Goal 93, A-862 (main 01a20182's ruling of 2026-11-22 06:22: a double charge refuses a batch that fits and makes
  /// the admission's figures lie). Do: charge a revision bringing a property, a content of [`CONTENT`] bytes and two
  /// texts named stored, as the import's sink put them into the content store before its batch judges it. Expect: its
  /// property and content twice (into the store and again in its increment's post-state), its texts once (the
  /// post-state alone: the shard charged them in the content store already). Failing first: f159578c3 charged the
  /// texts twice too.
  #[test]
  fn a_revisions_texts_already_in_the_store_are_charged_once() {
    let revision = Revision {
      head: 0,
      record: RevisionRecord {
        transaction: "0-0".to_owned(),
        changes: Vec::new(),
      },
      properties: BTreeMap::from([(PROPERTY.0.to_vec(), PROPERTY.1.to_vec())]),
      contents: BTreeMap::from([([1; 32], vec![0; CONTENT])]),
      stored: BTreeMap::from([([2; 32], STORED[0]), ([3; 32], STORED[1])]),
      lock_entries: Vec::new(),
    };
    let brought = u64::try_from(PROPERTY.0.len() + PROPERTY.1.len() + CONTENT).unwrap();
    assert_eq!(
      super::admission_charge(&revision),
      2 * brought + STORED[0] + STORED[1]
    );
  }
}

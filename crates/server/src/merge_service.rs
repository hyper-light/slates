//! The merge plane's service layer (§4.16, D-27; the A-9 integration requirement; AC-6.8,
//! AC-6.13/T-6.15): what turns the pure engine of `slates-merge` into a green-volume *service*.
//! The engine decides verdicts; this module enforces the roles at every verb, starts a green's
//! chain from scratch or from a **complete immutable base** (never an implicitly live host
//! directory), pins a green attachment to a version that moves only by `advance`, serves a read
//! at a pinned version, and destroys and reports merge volumes — and, in the fleet half (the
//! `pending`/`replica` state and the record plane's hooks), places every input to a verdict before
//! a merge record names it and recomputes the verdict on every holder before it is served.
//!
//! **Roles (D-27, §4.4).** A volume created with the `Green` role is written by nothing but its
//! merge task: an edit or declaration on it, a write attachment, a snapshot, a resize refuse
//! `ReadOnlyVolume`; the chain reads and a work's creation on a non-green refuse `NotGreen`; an
//! edit, declaration, submit or rebase on a non-work refuse `NotWork`. A `Work` volume is only
//! ever a clone of a green version (its content is seeded from the green at that version and its
//! mutations are journaled as declared operations), and a store-backed verb that a merge volume
//! cannot serve (clone, land, the base operations) refuses `Unsupported` naming the verb, never
//! `NotFound` for a volume that exists. The checks read the catalog record — one lookup per verb.
//!
//! **A complete immutable base.** `CreateGreen { base }` walks the named snapshot on its volume's
//! owner shard (the verb routes there) and refuses `ConsistentBaseUnavailable` unless
//! [`slates_vfs::coverage`] says the frozen tree depends on nothing live; the origin it captures is
//! given to the engine as version 0 and recorded durably (`GreenOriginated`) so recovery re-seeds
//! it before replaying the chain. A host edit after the create changes no green version: the
//! origin's bytes are the engine's, never the disk's (tested by use).
//!
//! **Pinned attachments (§4.16 "Attachments and versions").** A read attachment of a green pins
//! the head version at attach time; `advance` re-pins to a named version or the head and names
//! exactly the paths some version in the span changed (`Green::changed_between`); a read through
//! the attachment serves the pinned version (`Green::content_at`). A pin is owner-shard state
//! keyed by the attachment id, which routes there; it is dropped with the attachment and, like
//! every attachment, does not survive a restart (§4.8: attachments are reconciled out).
//!
//! Cost: a role check is one catalog lookup; a base seed is proportional to the snapshot (bounded
//! by the chain byte budget the origin is recorded against, a memory-derived cap; cooperative
//! slicing of the seed is owed with the extent-backed green); an advance is proportional to the
//! histories touched in the span.

use std::collections::BTreeMap;

use slates_db::catalog::{
  AttachForm, AttachmentRecord, Principal, Role, VolumeId as DbVolumeId, VolumeRecord, VolumeState,
};
use slates_db::op::Op;
use slates_ipc::protocol::{GreenBase, Intent, ReadAt, Refusal, ReplyBody, SnapshotId, VolumeId};
use slates_merge::origin::Origin;
use slates_vfs::clock::Clock;
use slates_vfs::dir::Child;
use slates_vfs::ids::InodeNo;
use slates_vfs::inode::Kind;
use slates_vfs::volume::{Store, Volume};
use slates_wire::request::RequestId;

use crate::error::{refusal_of_db, refusal_of_vfs};
use crate::state::ShardState;
use crate::verbs::{
  core_snapshot, find, forbidden, refused, rights_of, to_db_snapshot, to_db_volume, wire_id,
};

/// The merge plane's per-shard state (§4.16): one field on [`ShardState`], so the join with the
/// rest of the daemon is a single line there and a single line at init.
#[derive(Default)]
pub struct MergeShardState {
  /// On the control shard: the coordinator periods run so far (`crate::fleet`'s coordinator counts one a period). On
  /// an owner shard: the period of the record plane's latest pass over it, which a version enqueued here is stamped
  /// with (`PendingMergeRecord::first_period`).
  pub coordinator_period: u64,
  /// The version each green attachment pins, by attachment id (§4.16 "Attachments and versions").
  pub attachments: BTreeMap<u64, PinnedAttachment>,
  /// The merge records this owner shard still owes its candidate holders, by green then version
  /// (§4.16 "Commit"): each waits for its inputs to place, then ships in order. Bounded by the
  /// chain (one entry per committed version not yet held everywhere; empty at `f = 0`).
  pub pending: BTreeMap<ObjectId, PendingGreen>,
  /// The highest version of each owned green whose merge record placed at `f + 1` (`await placed`
  /// answers from it; at `f = 0` every appended version).
  pub placed: BTreeMap<ObjectId, u64>,
  /// The holders that acknowledged the version `placed` names, per owned green: whether this owner may report
  /// its neighbourhood change placed reads them (§4.8 "the newest committed record is held by f+1 of the new
  /// candidates"; [`greens_placed_on_current_cohort`]). Bounded by the owned greens, dropped with the green.
  pub placed_holders: BTreeMap<ObjectId, (u64, Vec<HostId>)>,
  /// This holder's replica of each green it backs (§4.16 "Apply on holders"): the engine the
  /// holder recomputes every version into before accepting the record that names it. Empty on a
  /// laptop (no records arrive).
  pub replicas: BTreeMap<ObjectId, Green>,
  /// The greens this holder refused for good after a recomputation mismatch (fatal-and-loud).
  pub refused: BTreeSet<ObjectId>,
  /// Faults a test injects on this holder; never set from the wire.
  pub fault: MergeFault,
  /// Submits whose acceptance waits for their version's merge record to commit at the quorum
  /// (§4.16 "Commit"; AUD-11), by green and version: where each replies and the completion key it
  /// is recorded under when it does. Bounded by the clients' credit (one entry per request in
  /// flight; a retry joins its entry). Empty at `f = 0`, where the append is the commit.
  pub awaiting: BTreeMap<(ObjectId, u64), Vec<AwaitingAcceptance>>,
  /// The waiting requests by completion key (origin, client, sequence), so a retry of one joins its
  /// entry instead of running the verb again.
  pub awaiting_by_request: BTreeMap<(u64, u32, u32), (ObjectId, u64)>,
  /// On the control shard: whether some owner shard has merge work the record plane can do now (a version
  /// enqueued, an input or a record newly acknowledged), so the coordinator runs the merge plane at once rather
  /// than at its next period (`nudge_record_plane`, `nudged`).
  pub nudged: bool,
  /// On the control shard: the coordinator's waker while it waits for its next period, woken by a nudge.
  pub nudge_waker: Option<std::task::Waker>,
  /// On an owner shard: the works created here whose catalog register is not yet committed (A-255). The record plane
  /// ships each at once, so a submit soon after the work's creation finds its route committed rather than waiting for
  /// it ([`next_merge_work`]). A work leaves once its catalog is committed or it has no record; bounded by the works.
  pub works_owing_catalog: std::collections::BTreeSet<DbVolumeId>,
  /// On an owner shard at `f > 0`: version 0's inputs of each green with an origin (A-217's held snapshot, or the bytes a
  /// taken-over green records), made once a share at a time by a task of this shard
  /// (`crate::merge_origin::OriginInputsJob`) and kept until every candidate holds version 0 (§4.16 "Commit", A-286).
  /// Made in a poll, they were a walk of the whole origin, its encoding and its archive, at the green's creation and
  /// again on every pass of the record plane until version 0 placed: a kernel-sized origin held the shard for seconds
  /// each time. Nothing of a green ships while its inputs are not made. Bounded by the greens this shard owns; dropped
  /// with the green and once every candidate holds version 0.
  pub(crate) origin_inputs: BTreeMap<ObjectId, crate::merge_origin::OriginInputs>,
  /// On a holder: the greens whose replica a task of this shard is building a share at a time from what it holds
  /// (`crate::merge_origin::ReplicaSeed`): version 0 seeded from its inputs (A-286), and on a holder that restarted,
  /// every version it accepted recomputed from its held increments (A-313). Every record of such a green is answered with
  /// no acknowledgement until the replica is built; the owner ships it again. Bounded by the greens this holder backs.
  /// A takeover successor's replica advanced to the version it adopted, or rebuilt for it, is built the same way (A-333).
  pub(crate) seeding: BTreeSet<ObjectId>,
  /// On a takeover successor's control shard: the submit completions each green it adopts carried in its ledger's records
  /// (A-255), gathered a slice of records at a time by a task of this shard for the adoption (`crate::green_adopt`,
  /// A-333). Bounded by the greens awaiting materialization here; each by its clients and the per-client completion bound.
  pub(crate) adoption_completions: BTreeMap<ObjectId, crate::green_adopt::Completions>,
  /// On an owner shard after a restart: each green its takeover adopted from this node's holder ledger (A-333), which no
  /// replay of its partition can rebuild, waiting for the boot to rebuild its engine from that ledger and hand it here
  /// (`crate::green_adopt::readopt_at_boot`). Every verb on it is answered `Overloaded` meanwhile. Bounded by the greens
  /// this shard owns; empty once the boot has re-adopted or fenced each.
  pub(crate) awaiting_adoption: BTreeMap<DbVolumeId, crate::green_adopt::AwaitingAdoption>,
}

/// A green attachment's pin: the green and the version its view is fixed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedAttachment {
  /// The green.
  pub green: DbVolumeId,
  /// The pinned version.
  pub version: u64,
}

/// Derived: the bytes a green's **rejected-result cache** may hold (§4.16; AUD-16) — the partition's
/// green-chain byte cap (`PartitionCaps::green_chain_bytes`), the one merge byte bound §4.2's
/// admission gives a shard: the cache of refused verdicts may hold at most what the durable chain of
/// accepted ones may, so a conflict flood is bounded by the same derived number that bounds the chain
/// and never by a second constant. Set on the engine when a green is created and when it is rebuilt.
pub(crate) fn rejected_cache_budget(state: &ShardState) -> usize {
  state.db.partition().caps().green_chain_bytes
}

/// The oldest version a live reader of `green` can still name — a work's base (it composes and
/// rebases against that version) or a pinned attachment (it reads at it) — or the head when there is
/// none: the floor the engine's histories are folded to (§4.16 "delta retention before folding").
/// Never past the head.
pub(crate) fn reachable_floor(state: &ShardState, green: DbVolumeId) -> u64 {
  let head = state.greens.get(&green).map_or(0, Green::head);
  let pins = state
    .merge
    .attachments
    .values()
    .filter(|pin| pin.green == green)
    .map(|pin| pin.version);
  // Every work's base is held in the partition wherever the work lives (A-247).
  let oldest_work = state.db.partition().oldest_work_floor(green);
  pins.chain(oldest_work).min().unwrap_or(head).min(head)
}

/// The version a green's durable chain may be checkpointed at (W3; §4.16): the reachable floor, kept below the
/// oldest version whose merge record still waits for its placement, since that record's inputs are read from the
/// chain entry (`inputs_of`) until it is placed. At `f = 0` nothing waits and it is the reachable floor.
pub(crate) fn checkpoint_floor(state: &ShardState, green: DbVolumeId) -> u64 {
  let floor = reachable_floor(state, green);
  state
    .merge
    .pending
    .get(&ObjectId(green.bytes))
    .and_then(PendingGreen::oldest)
    .map_or(floor, |oldest| floor.min(oldest.saturating_sub(1)))
}

/// Checkpoints `green`'s durable chain at its checkpoint floor when the increments that would fold are at least the
/// checkpoint it holds (W3; §4.16 "Deltas older than the derived retention fold into checkpoint deltas"): the
/// partition then holds the green's state at the floor in place of every increment up to it, and recovery restores
/// the green from it and replays only the increments after it (`verbs::replay_green`).
///
/// The rule is the doubling one: a checkpoint is taken once the bytes it would fold reach the size of the one it
/// replaces, so the work of taking checkpoints is at most the bytes appended to the chain, and the chain's own bytes
/// past the checkpoint stay under the checkpoint's size plus what live readers still hold below. Until 2026-11-06
/// nothing was ever folded: a green whose readers all kept up was refused `green_chains at capacity` once its
/// history passed the partition's cap (`crates/server/tests/green_checkpoint.rs`, version 31 under 256 KiB), and
/// every snapshot re-encoded the whole chain.
///
/// Never refuses the verb that called it: a checkpoint the engine cannot take at the floor (its histories already
/// folded past it) or the partition refuses is counted and skipped, and the chain stays whole and correct.
pub(crate) fn checkpoint_green(state: &mut ShardState, green: DbVolumeId) {
  checkpoint_green_when(state, green, Doubling::Wait);
}

/// Whether [`checkpoint_green_when`] waits for the increments to fold to reach the held checkpoint's size.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Doubling {
  /// Wait for them: the amortized rule, after an accept.
  Wait,
  /// Take it at once: the chain is at its cap, and no accept can come to take it otherwise.
  Now,
}

/// Whether the merge record of a version of `green` goes to holders rather than being placed by the owner's append alone
/// (the laptop): where [`enqueue_record`] reads the head's identity anyway, so the chain entry can record it (A-257).
pub(crate) fn record_goes_to_holders(state: &ShardState, green: DbVolumeId) -> bool {
  let config = state.fleet.configuration();
  let local_hold = Placement {
    acked: vec![state.fleet.host()],
    ..config.place(ObjectId(green.bytes))
  };
  !local_hold.placed(config.quorum)
}

/// [`checkpoint_green`], with the doubling rule waived when `doubling` is [`Doubling::Now`]: what a submit refused at
/// the chain's cap does first (`verbs::submit`, A-238), since the floor may have risen since the last accept (a work
/// destroyed, rebased or reset), and a chain at its cap refuses every submit whose accept would take it.
pub(crate) fn checkpoint_green_when(state: &mut ShardState, green: DbVolumeId, doubling: Doubling) {
  let floor = checkpoint_floor(state, green);
  let partition = state.db.partition();
  // The chain begins after the checkpoint held, or after the green's adoption by its takeover (A-333).
  let held = partition.green_floor(green);
  let held_bytes = partition
    .green_checkpoint(green)
    .map_or(0, |(_, bytes)| bytes.len());
  let Some(folds) = floor
    .checked_sub(held)
    .filter(|folds| *folds > 0)
    .and_then(|folds| usize::try_from(folds).ok())
  else {
    return;
  };
  let folded: usize = partition
    .green_chain(green)
    .iter()
    .take(folds)
    .map(Vec::len)
    .sum();
  if doubling == Doubling::Wait && folded < held_bytes {
    return;
  }
  // The head's identity beside it (A-257): the amortized rule above bounds how often this reads the identity.
  let Some(checkpoint) = state.greens.get_mut(&green).and_then(|engine| {
    let mut checkpoint = engine.checkpoint(floor)?;
    checkpoint.head_check = Some((engine.head(), engine.head_identity()));
    Some(checkpoint)
  }) else {
    state.count(CHECKPOINT_UNAVAILABLE, 1);
    return;
  };
  let op = Op::GreenCheckpointed {
    green,
    version: floor,
    checkpoint: checkpoint.encode(),
  };
  let now = state.clock.monotonic_ns();
  let recorded = state
    .db
    .partition()
    .check(&op, now)
    .and_then(|()| state.db.mutate(&mut state.segment, &op, now).map(|_| ()));
  match recorded {
    Ok(()) => state.count(CHECKPOINT_TAKEN, 1),
    Err(_) => state.count(CHECKPOINT_REFUSED, 1),
  }
}

/// Counter: a green's chain checkpointed (W3), the non-vacuity count of [`checkpoint_green`].
pub(crate) const CHECKPOINT_TAKEN: &str = "merge.checkpoint.taken";
/// Counter: a checkpoint the engine could not take at the floor (its histories folded past it); the chain stays
/// whole.
pub(crate) const CHECKPOINT_UNAVAILABLE: &str = "merge.checkpoint.unavailable";
/// Counter: a checkpoint the partition refused (its record did not fit the log or the budget); the chain stays
/// whole.
pub(crate) const CHECKPOINT_REFUSED: &str = "merge.checkpoint.refused";

/// A work's base (`crate::work_files`): its green on this shard, when it still exists, at the work's base version.
pub(crate) fn work_base<'a>(
  state: &'a ShardState,
  work: &crate::state::WorkState,
) -> crate::work_files::Base<'a> {
  crate::work_files::Base {
    green: state.greens.get(&work.green),
    fetched: state.work_home.fetched.as_ref(),
    version: work.base_version,
  }
}

/// The refusal for a file of a work whose green is gone: its base cannot be read (§4.16 failure matrix,
/// `UnknownBase`, as the work's submit answers).
pub(crate) fn unreadable_base(work: &crate::state::WorkState) -> Refusal {
  Refusal::UnknownBase {
    green: crate::verbs::to_wire_volume(work.green),
    version: work.base_version,
  }
}

/// Folds `green`'s histories to the reachable floor and **settles** its retention charge to exactly
/// what the engine then holds beyond its current files — the content history and the rejected-result
/// cache (`Green::retained_bytes`) — against the shard's budget (§4.2 all-cost admission: retention
/// charged by the retaining operation; AUD-16). The difference from what the green last charged is
/// charged (`ShardBudget::charge_retention`) or credited (`credit_retention`); the green's charge
/// (`ShardState::green_retention`) is the credit authority, so the accounting balances by
/// construction. `Err(available)` when the budget cannot cover a charge — nothing is then changed
/// but the fold, which only releases.
pub(crate) fn settle_green_retention(state: &mut ShardState, green: DbVolumeId) -> Result<(), u64> {
  settle_green_retention_leaving(state, green, 0)
}

/// [`settle_green_retention`], folding until the green's retention leaves `room` bytes under its cap: what a submit
/// is about to secure (`secure_green_retention`). Folding only to the cap left none, so a green whose readers all
/// kept up was refused `BudgetExceeded { available: 0 }` once its history filled the cap, though everything below
/// the head was foldable (version 34 of `crates/server/tests/green_checkpoint.rs` under a 256 KiB cap, 2026-11-06).
pub(crate) fn settle_green_retention_leaving(
  state: &mut ShardState,
  green: DbVolumeId,
  room: usize,
) -> Result<(), u64> {
  let floor = reachable_floor(state, green);
  let budget = rejected_cache_budget(state);
  let Some(engine) = state.greens.get_mut(&green) else {
    return Ok(());
  };
  // Oldest-first, only as far as the retention budget needs (the same derived cap the rejected
  // cache is bounded by: one merge byte bound per green) with `room` to spare, never past a live reader's version.
  engine.fold_history_to_budget(floor, budget.saturating_sub(room));
  let wanted = u64::try_from(engine.retention_charge()).unwrap_or(u64::MAX);
  let charged = state.green_retention.get(&green).copied().unwrap_or(0);
  if wanted > charged {
    // Capacity is claimed as it is needed (A-98), and a green's works, which claim it by their charges, live on other
    // shards (A-247): the green's own retention claims what it charges.
    state.store.make_room(wanted.saturating_sub(charged));
    state
      .store
      .budget
      .charge_retention(wanted.saturating_sub(charged))
      .map_err(|e| match e {
        slates_mem::MemError::BudgetExceeded { available, .. } => available,
        _ => 0,
      })?;
  } else if wanted < charged {
    state
      .store
      .budget
      .credit_retention(charged.saturating_sub(wanted));
  }
  state.green_retention.insert(green, wanted);
  Ok(())
}

/// Secures `bytes` of retention for `green` ahead of a verdict (A-16: the charge is taken **before**
/// the mutation, so a refused charge changes nothing): charged to the shard's budget and recorded on
/// the green's charge, which the settle after the verdict trues up — crediting what the commit did
/// not retain. `Err(available)` when the budget cannot cover it.
pub(crate) fn secure_green_retention(
  state: &mut ShardState,
  green: DbVolumeId,
  bytes: u64,
) -> Result<(), u64> {
  if bytes == 0 {
    return Ok(());
  }
  // The green's own retention cap (the derived merge byte bound) is the first gate: what the settle
  // could not fold under it — history live readers still name — plus this claim must fit, else the
  // claim is refused with the room left. The shard's budget is the second.
  let cap = u64::try_from(rejected_cache_budget(state)).unwrap_or(u64::MAX);
  let charged = state.green_retention.get(&green).copied().unwrap_or(0);
  if charged.saturating_add(bytes) > cap {
    return Err(cap.saturating_sub(charged));
  }
  state.store.make_room(bytes);
  state
    .store
    .budget
    .charge_retention(bytes)
    .map_err(|e| match e {
      slates_mem::MemError::BudgetExceeded { available, .. } => available,
      _ => 0,
    })?;
  let charged = state.green_retention.entry(green).or_insert(0);
  *charged = charged.saturating_add(bytes);
  Ok(())
}

/// Credits everything `green` charged as retention (its destroy): the budget gets the bytes back and
/// the green's charge is forgotten.
pub(crate) fn release_green_retention(state: &mut ShardState, green: DbVolumeId) {
  if let Some(charged) = state.green_retention.remove(&green) {
    state.store.budget.credit_retention(charged);
  }
}

/// What a green holds in memory beyond its durable chain and what it has charged for it
/// (`Daemon::merge_retention`; AUD-16): the engine's retained bytes, the charge on the shard's budget,
/// the fold floor, and the rejected-result cache's entries and evictions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeRetention {
  /// The current files' bytes.
  pub content: u64,
  /// The content history retained beyond the current files, as the engine's running total.
  pub history: u64,
  /// The same, recounted from the histories (the balance check).
  pub history_recounted: u64,
  /// The rejected-result cache's bytes.
  pub rejected: u64,
  /// The bytes the green has charged to the shard's budget as retention.
  pub charged: u64,
  /// The version below which the histories are folded.
  pub folded_below: u64,
  /// The rejected-result cache's entries.
  pub rejected_entries: u64,
  /// Rejected results evicted or never retained for want of budget.
  pub rejected_evicted: u64,
}

/// [`MergeRetention`] of `green` on this shard; the default (all zero) for a green this shard does not
/// own.
pub(crate) fn merge_retention(state: &ShardState, green: DbVolumeId) -> MergeRetention {
  let Some(engine) = state.greens.get(&green) else {
    return MergeRetention::default();
  };
  let retained = engine.retained_bytes();
  let (entries, _, evicted) = engine.rejected_cache();
  let as_u64 = |bytes: usize| u64::try_from(bytes).unwrap_or(u64::MAX);
  MergeRetention {
    content: as_u64(retained.content),
    history: as_u64(retained.history),
    history_recounted: as_u64(engine.history_bytes_recounted()),
    rejected: as_u64(retained.rejected),
    charged: state.green_retention.get(&green).copied().unwrap_or(0),
    folded_below: engine.folded_below(),
    rejected_entries: as_u64(entries),
    rejected_evicted: evicted,
  }
}

/// A volume's merge role as the catalog records it (§4.4 `Role`); `None` for a plain volume or
/// one with no record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergeRole {
  /// A green: the merge target, written only by its merge task.
  Green {
    /// Whether every increment must carry evidence.
    require_evidence: bool,
  },
  /// A work over a green.
  Work {
    /// The green.
    green: DbVolumeId,
  },
}

/// A path as the merge plane keys it: the volume's absolute path without its leading slashes (the
/// form the origin walk produces and the ops document names), so a declaration made with a leading
/// slash and a read without one meet the same entry.
pub(crate) fn canonical_path(path: &str) -> &str {
  path.trim_start_matches('/')
}

/// The merge role of `volume`, or `None` for a plain volume or one with no record.
pub(crate) fn merge_role(state: &ShardState, volume: DbVolumeId) -> Option<MergeRole> {
  let record = state.db.partition().volume(volume)?;
  match record.policy.role {
    Role::Plain => None,
    Role::Green {
      require_evidence, ..
    } => Some(MergeRole::Green { require_evidence }),
    Role::Work { green, .. } => Some(MergeRole::Work { green }),
  }
}

/// The catalog record of any live volume — plain, green or work — or the typed refusal: `NotFound`
/// with no record, `Destroying` for one on its way out, `NotRecovered` for one recovery could not rebuild. (Unlike [`find`], no store slot is needed:
/// a merge volume has none.)
pub(crate) fn find_record(state: &ShardState, volume: VolumeId) -> Result<VolumeRecord, Refusal> {
  // A volume recovery could not rebuild answers why to every verb that reads its record alone, a share among them, as
  // `verbs::find` does for the rest (GAPS, 2026-11-08); only its destroy is served (`verbs::destroy_unrecovered`).
  if let Some(cause) = state.unrecovered.get(&to_db_volume(volume)) {
    return Err(Refusal::NotRecovered {
      cause: cause.clone(),
    });
  }
  let record = state
    .db
    .partition()
    .volume(to_db_volume(volume))
    .cloned()
    .ok_or(Refusal::NotFound)?;
  if matches!(
    record.state,
    VolumeState::Destroying | VolumeState::Destroyed
  ) {
    return Err(Refusal::Destroying);
  }
  Ok(record)
}

/// The record of a green the verb may read, or the typed refusal (`NotFound`, `Destroying`,
/// `NotGreen`, `Forbidden`).
pub(crate) fn require_green(
  state: &ShardState,
  principal: &Principal,
  green: VolumeId,
  verb: &str,
) -> Result<VolumeRecord, Refusal> {
  let record = find_record(state, green)?;
  if !matches!(record.policy.role, Role::Green { .. }) {
    return Err(Refusal::NotGreen);
  }
  if !rights_of(&record, principal).read {
    return Err(Refusal::Forbidden {
      verb: verb.to_owned(),
    });
  }
  Ok(record)
}

/// The record of a work the verb may act on and its green, or the typed refusal: a green named
/// where a work is expected is `ReadOnlyVolume` when the verb writes (an edit or declaration would
/// write the green) and `NotWork` otherwise (a submit or rebase *of* a green is not a write to it);
/// a plain volume is `NotWork`. The caller needs the write right on the work (its own clone).
pub(crate) fn require_work(
  state: &ShardState,
  principal: &Principal,
  work: VolumeId,
  writes: bool,
  verb: &str,
) -> Result<(VolumeRecord, DbVolumeId), Refusal> {
  let record = find_record(state, work)?;
  let green = match record.policy.role {
    Role::Work { green, .. } => green,
    Role::Green { .. } if writes => return Err(Refusal::ReadOnlyVolume),
    Role::Green { .. } | Role::Plain => return Err(Refusal::NotWork),
  };
  if !rights_of(&record, principal).write {
    return Err(Refusal::Forbidden {
      verb: verb.to_owned(),
    });
  }
  Ok((record, green))
}

/// A store-backed verb a merge volume cannot serve (§4.4 verbs over the volume core), for the
/// typed refusal it gets instead of `NotFound`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreVerb {
  /// `snapshot`: a green's versions are its snapshots and only the merge task makes them.
  Snapshot,
  /// `resize`: a merge volume holds no byte reservation.
  Resize,
  /// `clone`: a work is the clone of a green version (`create_work`).
  Clone,
  /// `land`: landing a merge volume onto disk (owed with the extent-backed green).
  Land,
  /// `destroy_snapshot`, `read_base`, `rewitness`, `pin`: the volume-core and base-plane verbs.
  Base,
  /// `image_build`, `image_merge`: a disk image of a snapshot, and its merge into a clone (A-160).
  Image,
}

/// The typed refusal a store-backed `verb` gets on a merge volume, or `None` for a plain volume
/// (the verb proceeds). A green refuses `ReadOnlyVolume` for the verbs that would write it; every
/// other case is `Unsupported` naming the verb and the way to do it.
pub(crate) fn refuse_store_verb(
  state: &ShardState,
  volume: VolumeId,
  verb: StoreVerb,
) -> Option<ReplyBody> {
  let role = merge_role(state, to_db_volume(volume))?;
  let refusal = match (role, verb) {
    (MergeRole::Green { .. }, StoreVerb::Snapshot | StoreVerb::Resize) => Refusal::ReadOnlyVolume,
    (MergeRole::Work { .. }, StoreVerb::Snapshot) => Refusal::Unsupported {
      feature: "snapshot of a work volume: submit it to its green".to_owned(),
    },
    (MergeRole::Work { .. }, StoreVerb::Resize) => Refusal::Unsupported {
      feature: "resize of a work volume: it holds no byte reservation".to_owned(),
    },
    (_, StoreVerb::Clone) => Refusal::Unsupported {
      feature: "clone of a merge volume: create a work over the green".to_owned(),
    },
    (_, StoreVerb::Land) => Refusal::Unsupported {
      feature: "landing a merge volume".to_owned(),
    },
    (_, StoreVerb::Base) => Refusal::Unsupported {
      feature: "the volume-core and base-plane verbs on a merge volume".to_owned(),
    },
    (_, StoreVerb::Image) => Refusal::Unsupported {
      feature: "a disk image of a merge volume".to_owned(),
    },
  };
  Some(refused(refusal))
}

/// [`find`] for a store-backed verb: a merge volume refuses typed for `verb` ([`refuse_store_verb`])
/// before the slot lookup, a plain volume is found (or refused `NotFound`/`Destroying`) as usual.
/// One call, so the store-backed verbs keep their shape.
pub(crate) fn find_store_backed(
  state: &ShardState,
  volume: VolumeId,
  verb: StoreVerb,
) -> Result<(slates_mem::Handle<crate::state::VolumeSlot>, VolumeRecord), Box<ReplyBody>> {
  if let Some(reply) = refuse_store_verb(state, volume, verb) {
    return Err(Box::new(reply));
  }
  find(state, volume)
}

/// The record `attach` admits a volume by: a merge volume's catalog record (it has no store slot),
/// a plain volume's through [`find`] (its slot must be held here).
pub(crate) fn attachable_record(
  state: &ShardState,
  volume: VolumeId,
) -> Result<VolumeRecord, Box<ReplyBody>> {
  match merge_role(state, to_db_volume(volume)) {
    Some(_) => find_record(state, volume).map_err(|refusal| Box::new(refused(refusal))),
    None => find(state, volume).map(|(_, record)| record),
  }
}

/// Captures the origin a green starts from: the complete immutable snapshot `base` names, walked
/// on this shard (the base volume's owner). Refused as [`check_origin_base`] refuses, and the volume core's own
/// refusal for a torn read.
pub(crate) fn seed_origin(
  state: &mut ShardState,
  principal: &Principal,
  base: GreenBase,
) -> Result<Origin, Refusal> {
  let handle = check_origin_base(state, principal, base)?;
  let slot = state.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
  walk_origin(&slot.volume, &state.store, core_snapshot(base.snapshot))
    .map_err(|e| refusal_of_vfs(&e))
}

/// Whether `base` can be a green's origin for `principal`, and the slot of its volume: refused `NotFound` for a volume
/// or snapshot that does not exist here, `Forbidden` without the read right, `ConsistentBaseUnavailable` when the
/// snapshot still depends on the host directory (an overlay not pinned whole before the freeze).
pub(crate) fn check_origin_base(
  state: &mut ShardState,
  principal: &Principal,
  base: GreenBase,
) -> Result<slates_mem::Handle<crate::state::VolumeSlot>, Refusal> {
  let (handle, record) = find(state, base.volume).map_err(|reply| match *reply {
    ReplyBody::Refused { refusal } => refusal,
    _ => Refusal::NotFound,
  })?;
  if !rights_of(&record, principal).read {
    return Err(Refusal::Forbidden {
      verb: "create_green".to_owned(),
    });
  }
  if state
    .db
    .partition()
    .snapshot(record.id, to_db_snapshot(base.snapshot))
    .is_none()
  {
    return Err(Refusal::NotFound);
  }
  let slot = state
    .volumes
    .get_mut(handle)
    .map_err(|_| Refusal::NotFound)?;
  // An overlay's snapshot is judged with its host, so a directory whose listing this daemon has not read since a
  // restart is read now rather than taken for incomplete (A-153's "read again on first use").
  let complete = match slot.host.as_mut() {
    Some(host) => slot
      .volume
      .with_host(host)
      .snapshot_is_complete(&state.store, core_snapshot(base.snapshot)),
    None => slot
      .volume
      .snapshot_is_complete(&state.store, core_snapshot(base.snapshot)),
  };
  match complete {
    Ok(true) => Ok(handle),
    Ok(false) => Err(Refusal::ConsistentBaseUnavailable),
    Err(e) => Err(refusal_of_vfs(&e)),
  }
}

/// The identity a held origin is recorded under and checked against at recovery (A-217): the BLAKE3 of the origin's
/// canonical encoding (`Origin::encode`, sorted and length-prefixed), so the same snapshot walked again reproduces it.
/// Hashed as it is written ([`Origin::identity`]), never gathered into a second copy of the tree first.
pub(crate) fn origin_identity(origin: &Origin) -> [u8; 32] {
  origin.identity()
}

/// The origin a green holds as a reference (A-217), walked again from its base snapshot on this shard a unit at a time
/// ([`OriginWalk::unit`]: a directory's listing or a file's read), so a recovery can end its slice between two (§2.6
/// boot step 2, `crate::recovery_slices`) and an owner making version 0's inputs can end its own (`crate::merge_origin`,
/// A-286). Holds no borrow of the state between steps: each step finds the base volume again.
pub(crate) struct HeldWalk {
  /// The base volume walked.
  volume: DbVolumeId,
  /// The identity the green recorded for its origin, which the finished walk must reproduce.
  identity: [u8; 32],
  /// The walk so far.
  walk: OriginWalk,
  /// The walked origin, once the last unit is done.
  walked: Option<Origin>,
}

impl HeldWalk {
  /// The walk of `held`'s base snapshot, not yet begun: `NotFound` when the base volume is not here.
  pub(crate) fn begin(
    state: &ShardState,
    held: &slates_db::partition::OriginRef,
  ) -> Result<HeldWalk, Refusal> {
    held_base(state, held.volume)?;
    let snapshot = core_snapshot(SnapshotId {
      value: held.snapshot.value,
    });
    Ok(HeldWalk {
      volume: held.volume,
      identity: held.identity,
      walk: OriginWalk::new(snapshot),
      walked: None,
    })
  }

  /// One unit of the walk; true once the whole snapshot is walked.
  pub(crate) fn step(&mut self, state: &ShardState) -> Result<bool, Refusal> {
    if self.walked.is_some() {
      return Ok(true);
    }
    let slot = held_base(state, self.volume)?;
    self.walked = self
      .walk
      .unit(&slot.volume, &state.store)
      .map_err(|e| refusal_of_vfs(&e))?;
    Ok(self.walked.is_some())
  }

  /// The walked origin: `ContentUnavailable` when it is not the one the green was created from, `NotFound` when the
  /// walk was not stepped to its end.
  pub(crate) fn finish(self) -> Result<Origin, Refusal> {
    let (origin, identity) = self.into_walked()?;
    if origin_identity(&origin) != identity {
      return Err(Refusal::ContentUnavailable);
    }
    Ok(origin)
  }

  /// The walked origin with the identity it must reproduce, unchecked, for a caller that hashes it a share at a time
  /// as it writes its canonical bytes (`crate::merge_origin`, A-286) rather than in one call as [`HeldWalk::finish`]
  /// does: `NotFound` when the walk was not stepped to its end.
  pub(crate) fn into_walked(self) -> Result<(Origin, [u8; 32]), Refusal> {
    Ok((self.walked.ok_or(Refusal::NotFound)?, self.identity))
  }
}

/// The base volume a held origin is walked from, on this shard: `NotFound` when it is not here.
fn held_base(state: &ShardState, volume: DbVolumeId) -> Result<&crate::state::VolumeSlot, Refusal> {
  let handle = state.by_id.get(&volume).copied().ok_or(Refusal::NotFound)?;
  state.volumes.get(handle).map_err(|_| Refusal::NotFound)
}

/// Walks a complete snapshot into an [`Origin`]: every file with its bytes, every directory, every
/// mode, every symlink, and every further name of a multiply-linked inode as a hard link to the
/// first name met. Paths are the snapshot's absolute paths without the leading slash, as the ops
/// document names them; the root itself is not an entry. [`OriginWalk`] run to its end, in one call: for a recovery
/// and a checkout; `CreateGreen` steps the walk a slice at a time.
fn walk_origin(
  volume: &Volume,
  store: &Store,
  snapshot: slates_vfs::ids::SnapshotId,
) -> Result<Origin, slates_vfs::error::VfsError> {
  let mut walk = OriginWalk::new(snapshot);
  loop {
    if let Some(origin) = walk.unit(volume, store)? {
      return Ok(origin);
    }
  }
}

/// A walk of a complete snapshot into an [`Origin`] in progress (§4.16 "Green creation", A-217; §4.3 "Bounded work
/// everywhere"): the directories still to list, by path, and the files listed and not yet read, so a walk stepped a
/// slice at a time holds no handle between its slices, as a pin's walk does (`slates_vfs::base::PinWalk`). One unit is
/// one directory's listing or one file's read. It visits what the one-call walk visited, in its order — the
/// directories on one stack, each listing's rows in order — and gives a multiply-linked inode's first name the
/// same way, at the listing that meets it, so the origin, and so a held origin's recorded identity, is the one the
/// one-call walk gave.
#[derive(Debug)]
pub(crate) struct OriginWalk {
  snapshot: slates_vfs::ids::SnapshotId,
  dirs: Vec<String>,
  files: Vec<(String, InodeNo, u64)>,
  first_name: BTreeMap<InodeNo, String>,
  origin: Origin,
}

impl OriginWalk {
  /// A walk of `snapshot` from its root.
  pub(crate) fn new(snapshot: slates_vfs::ids::SnapshotId) -> OriginWalk {
    OriginWalk {
      snapshot,
      dirs: vec![String::new()],
      files: Vec::new(),
      first_name: BTreeMap::new(),
      origin: Origin::default(),
    }
  }

  /// One unit of the walk over `volume`'s snapshot: a file read, else a directory listed; the canonical origin once
  /// the last is done. A snapshot gone meanwhile is the volume core's refusal (`StaleHandle`, `NotFound`).
  pub(crate) fn unit(
    &mut self,
    volume: &Volume,
    store: &Store,
  ) -> Result<Option<Origin>, slates_vfs::error::VfsError> {
    if let Some((path, inode, size)) = self.files.pop() {
      let mut bytes = vec![0u8; usize::try_from(size).unwrap_or(usize::MAX)];
      let read = volume.read_in(store, self.snapshot, inode, 0, &mut bytes)?;
      bytes.truncate(read);
      self.origin.files.push((path, bytes));
      return Ok(None);
    }
    let Some(prefix) = self.dirs.pop() else {
      let mut origin = std::mem::take(&mut self.origin);
      origin.canonicalize();
      return Ok(Some(origin));
    };
    let Child::Dir(dir) = volume.resolve_in(store, self.snapshot, &prefix)?.child else {
      return Err(slates_vfs::error::VfsError::NotDirectory);
    };
    // The listing's files are read one a unit after it, last row first; the rows are met here in order, so the
    // directories stack, the modes and the first names come out as the one-call walk made them.
    let mut listed = Vec::new();
    for row in volume.readdir_in(store, dir)? {
      let path = if prefix.is_empty() {
        row.name.to_owned()
      } else {
        format!("{prefix}/{}", row.name)
      };
      let attrs = volume.stat_in(store, self.snapshot, row.inode)?;
      match row.kind {
        Kind::Dir => {
          self.origin.dirs.push(path.clone());
          self.origin.modes.push((path.clone(), attrs.mode));
          if let Child::Dir(_) = volume.lookup_in(store, dir, row.name)?.child {
            self.dirs.push(path);
          }
        }
        Kind::Fifo | Kind::Socket | Kind::BlockDevice | Kind::CharDevice => {
          if let Some(first) = self.first_name.get(&row.inode) {
            self.origin.hardlinks.push((path, first.clone()));
            continue;
          }
          use slates_merge::special::{SpecialKind, SpecialNode};
          // A device node's number is captured with it (A-294): an inode holds it from its `mknod` on.
          let number = volume
            .device_in(store, self.snapshot, row.inode)?
            .unwrap_or_default();
          let kind = match row.kind {
            Kind::Fifo => SpecialKind::Fifo,
            Kind::Socket => SpecialKind::Socket,
            Kind::BlockDevice => SpecialKind::BlockDevice {
              major: number.major,
              minor: number.minor,
            },
            _ => SpecialKind::CharDevice {
              major: number.major,
              minor: number.minor,
            },
          };
          self.origin.modes.push((path.clone(), attrs.mode));
          self.first_name.insert(row.inode, path.clone());
          self.origin.specials.push((
            path,
            SpecialNode {
              kind,
              uid: attrs.uid,
              gid: attrs.gid,
              atime: attrs.atime,
              mtime: attrs.mtime,
              ctime: attrs.ctime,
              btime: attrs.btime,
            },
          ));
        }
        Kind::Symlink => {
          let target = volume.readlink_in(store, self.snapshot, row.inode)?;
          self.origin.symlinks.push((path, target.into_string()));
        }
        Kind::File => {
          if let Some(first) = self.first_name.get(&row.inode) {
            self.origin.hardlinks.push((path, first.clone()));
            continue;
          }
          self.origin.modes.push((path.clone(), attrs.mode));
          if attrs.nlink > 1 {
            self.first_name.insert(row.inode, path.clone());
          }
          listed.push((path, row.inode, attrs.size));
        }
      }
    }
    listed.reverse();
    self.files.extend(listed);
    Ok(None)
  }
}

/// A read attachment of a green pins its head version (§4.16 "Attachments and versions"); a write
/// attachment refuses `ReadOnlyVolume` — nothing but the merge task writes a green. The attachment
/// is recorded like any other (so `detach` and a restart's reconciliation treat it the same) and
/// its pin kept on this shard.
pub(crate) fn attach_green(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  record: &VolumeRecord,
  intent: Intent,
  form: &slates_ipc::protocol::AttachRequest,
) -> ReplyBody {
  if intent == Intent::Write {
    return refused(Refusal::ReadOnlyVolume);
  }
  if !rights_of(record, principal).read {
    return forbidden("attach");
  }
  let consumer =
    match crate::verbs::consumer_of(form, client_id, crate::verbs::termed_expiry(state)) {
      Ok(consumer) => consumer,
      Err(refusal) => return refused(refusal),
    };
  let Some(engine) = state.greens.get(&record.id) else {
    return refused(Refusal::NotFound);
  };
  let version = engine.head();
  // A green pins a version and is read-only (§4.16, D-27): its attachment carries read only, and the
  // mount capability token that lets the consumer mount that pinned view over NFS read-only (§4.13;
  // AUD-01). Refused, never a weak token, if the platform's secure random is unavailable.
  let Some(token) = crate::verbs::mint_mount_token() else {
    return refused(Refusal::BadRequest {
      reason: "secure random unavailable for the mount capability token".to_owned(),
    });
  };
  let attachment = crate::verbs::next_attachment_id(state);
  let now = state.clock.monotonic_ns();
  let op = Op::AttachmentAdded {
    record: AttachmentRecord {
      id: attachment,
      volume: record.id,
      // A host mount of the green is the bridge's attachment, as a plain volume's is (AUD-01).
      consumer,
      snapshot: None,
      form: AttachForm::Root,
      principal: principal.clone(),
      rights: slates_db::catalog::Rights {
        read: true,
        write: false,
        admin: false,
      },
      token,
    },
  };
  if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
    return refused(refusal_of_db(&e));
  }
  state.merge.attachments.insert(
    attachment,
    PinnedAttachment {
      green: record.id,
      version,
    },
  );
  // A green attachment is the record form (§4.4): no host mount presents a green, so what is
  // established is the record and the capability is the record transport's (§4.6 A-9).
  let situation = crate::transports::situation(state, &rights_of(record, principal));
  ReplyBody::Attached {
    attachment,
    lease_epoch: None,
    path: None,
    version: Some(version),
    established: slates_ipc::protocol::Established::Record,
    capability: crate::transports::root(&situation),
    token: Some(token),
  }
}

/// Drops the pin an attachment held (its detach, or a destroy of its green).
pub(crate) fn forget_attachment(state: &mut ShardState, attachment: u64) {
  if let Some(pin) = state.merge.attachments.remove(&attachment) {
    // The pin may have been the oldest reachable version: fold the green's histories to the new
    // floor and credit what that releases (a settle after a pin goes only credits).
    let _ = settle_green_retention(state, pin.green);
  }
}

/// `advance(attachment, version?)` (§4.16): re-pins a green attachment to `version`, or to the
/// head, and names the paths the move invalidates — exactly those some version in the span
/// changed. A version past the head, or of a green that is gone, is `UnknownBase`; an attachment
/// that pins no green is `NotGreen`; another principal's is `Forbidden`. Moving to the pinned
/// version itself invalidates nothing. A move backwards is allowed (a reader may re-pin an older
/// version) and invalidates the same span.
pub(crate) fn advance(
  state: &mut ShardState,
  principal: &Principal,
  attachment: u64,
  version: Option<u64>,
) -> ReplyBody {
  // A CLI attachment is renewed by the verb, or refused once its term lapsed (A-230).
  if let Err(refusal) = crate::verbs::use_attachment(state, principal, attachment) {
    return refused(refusal);
  }
  let Some(record) = state.db.partition().attachment(attachment).cloned() else {
    return refused(Refusal::NotFound);
  };
  if &record.principal != principal {
    return forbidden("advance");
  }
  // A snapshot mount moves between snapshots of its volume (AUD-29-76); a green attachment between versions.
  if state.snapshot_views.contains_key(&attachment) {
    return crate::snapshot_view::advance(state, &record, version);
  }
  let Some(pin) = state.merge.attachments.get(&attachment).copied() else {
    return refused(Refusal::NotGreen);
  };
  let Some(engine) = state.greens.get(&pin.green) else {
    return refused(Refusal::UnknownBase {
      green: wire_id(pin.green),
      version: version.unwrap_or(pin.version),
    });
  };
  let target = version.unwrap_or_else(|| engine.head());
  // Past the head, or below the fold floor — a version whose history no live reader could name, so
  // it was folded away (§4.16 "delta retention before folding"; AUD-16) — is not reconstructible:
  // refused as an unknown base rather than served from the value in effect at the floor.
  if target > engine.head() || target < engine.folded_below() {
    return refused(Refusal::UnknownBase {
      green: wire_id(pin.green),
      version: target,
    });
  }
  let (from, to) = if target >= pin.version {
    (pin.version, target)
  } else {
    (target, pin.version)
  };
  let invalidated = engine.changed_between(from, to);
  state.merge.attachments.insert(
    attachment,
    PinnedAttachment {
      green: pin.green,
      version: target,
    },
  );
  // A move up may raise the reachable floor: fold and credit what that releases.
  let _ = settle_green_retention(state, pin.green);
  ReplyBody::Advanced {
    version: target,
    invalidated,
  }
}

/// `read(volume, path, at)` (§4.12 `slates.fs.read`): a file's bytes at a view. A green serves
/// its head, a named version (`UnknownBase` past the head) or the version an attachment pins (the
/// attachment must be of this green); a work serves its live content (`BadRequest` for a version
/// or attachment view, which a work has no chain for); a plain volume serves its live tree through
/// the volume core (an overlay through its host). A path that names nothing is `NotFound`; a
/// directory is `BadRequest`.
pub(crate) fn read(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  path: &str,
  at: ReadAt,
) -> ReplyBody {
  let record = match find_record(state, volume) {
    Ok(record) => record,
    Err(refusal) => return refused(refusal),
  };
  if !rights_of(&record, principal).read {
    return forbidden("read");
  }
  let key = canonical_path(path);
  match record.policy.role {
    Role::Green { .. } => read_green(state, record.id, key, at),
    Role::Work { .. } => match at {
      ReadAt::Head => match state.works.get(&record.id) {
        Some(w) => match w.files.get(work_base(state, w), key) {
          Ok(Some(bytes)) => ReplyBody::ReadBytes {
            bytes: bytes.to_vec(),
          },
          Ok(None) => refused(Refusal::NotFound),
          Err(_) => refused(unreadable_base(w)),
        },
        None => refused(Refusal::NotFound),
      },
      ReadAt::Version { .. } | ReadAt::Attachment { .. } => refused(Refusal::BadRequest {
        reason: "a work volume has no versions to read at; read its head".to_owned(),
      }),
    },
    Role::Plain => match at {
      ReadAt::Head => read_plain(state, volume, path),
      ReadAt::Version { .. } | ReadAt::Attachment { .. } => refused(Refusal::BadRequest {
        reason: "a plain volume has no green versions; read its head".to_owned(),
      }),
    },
  }
}

/// Format: the encoded bytes of a `ReadPage` reply beside its page (variant, lengths, total, stamp), with room to
/// spare: what a page leaves of its reply chunk for the header.
const READ_PAGE_HEADER_BYTES: usize = 64;

/// `ReadRange` (§4.12, paged): up to `max` bytes of a file from `offset` at a view, never more than one reply chunk
/// holds, with the file's length and a stamp that changes whenever its bytes at that view may have: a green's
/// version (its bytes there never change), a work's revision, a plain volume's inode change counter.
pub(crate) fn read_range(
  state: &mut ShardState,
  principal: &Principal,
  (volume, path, at): (VolumeId, &str, ReadAt),
  offset: u64,
  max: u64,
) -> ReplyBody {
  read_window(state, principal, (volume, path, at), offset, max, 0)
}

/// The room one page of a read leaves in a client's reply chunk: the most a client-facing `ReadPage` carries.
pub(crate) fn page_room() -> u64 {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  u64::try_from(chunk.saturating_sub(READ_PAGE_HEADER_BYTES)).unwrap_or(0)
}

/// [`read_range`] with a larger bound: up to `max` bytes, capped at `cap` where it exceeds one page's room — a
/// read-ahead window another node asked for (`RequestBody::ReadWindow`). A `cap` of zero is one page.
pub(crate) fn read_window(
  state: &mut ShardState,
  principal: &Principal,
  (volume, path, at): (VolumeId, &str, ReadAt),
  offset: u64,
  max: u64,
  cap: u64,
) -> ReplyBody {
  let record = match find_record(state, volume) {
    Ok(record) => record,
    Err(refusal) => return refused(refusal),
  };
  if !rights_of(&record, principal).read {
    return forbidden("read");
  }
  let want = max.min(page_room().max(cap));
  let key = canonical_path(path);
  match record.policy.role {
    Role::Green { .. } => read_green_range(state, record.id, key, at, (offset, want)),
    Role::Work { .. } => match at {
      ReadAt::Head => match state.works.get(&record.id) {
        Some(w) => match w.files.page(work_base(state, w), key, (offset, want)) {
          Ok(Some((bytes, total))) => ReplyBody::ReadPage {
            bytes,
            total,
            stamp: w.revision,
          },
          Ok(None) => refused(Refusal::NotFound),
          Err(_) => refused(unreadable_base(w)),
        },
        None => refused(Refusal::NotFound),
      },
      ReadAt::Version { .. } | ReadAt::Attachment { .. } => refused(Refusal::BadRequest {
        reason: "a work volume has no versions to read at; read its head".to_owned(),
      }),
    },
    Role::Plain => match at {
      ReadAt::Head => read_plain_range(state, volume, path, (offset, want)),
      ReadAt::Version { .. } | ReadAt::Attachment { .. } => refused(Refusal::BadRequest {
        reason: "a plain volume has no green versions; read its head".to_owned(),
      }),
    },
  }
}

/// A green's file page at a view; the resolved version is its stamp.
fn read_green_range(
  state: &ShardState,
  green: DbVolumeId,
  path: &str,
  at: ReadAt,
  range: (u64, u64),
) -> ReplyBody {
  let Some(engine) = state.greens.get(&green) else {
    return refused(Refusal::NotFound);
  };
  let version = match green_version(state, engine, green, at) {
    Ok(version) => version,
    Err(refusal) => return refused(refusal),
  };
  match engine.content_page_at(path, version, range) {
    Some((bytes, total)) => ReplyBody::ReadPage {
      bytes,
      total,
      stamp: version,
    },
    None => refused(Refusal::NotFound),
  }
}

/// The version a read of a green at `at` serves: the head, a named version no later than it and no earlier than the
/// engine's fold, or an attachment's pinned version; the refusal otherwise.
pub(crate) fn green_version(
  state: &ShardState,
  engine: &slates_merge::engine::Green,
  green: DbVolumeId,
  at: ReadAt,
) -> Result<u64, Refusal> {
  let version = match at {
    ReadAt::Head => engine.head(),
    ReadAt::Version { version } => version,
    ReadAt::Attachment { attachment } => match state.merge.attachments.get(&attachment) {
      Some(pin) if pin.green == green => pin.version,
      Some(_) => {
        return Err(Refusal::BadRequest {
          reason: "the attachment pins another green".to_owned(),
        });
      }
      None => return Err(Refusal::NotFound),
    },
  };
  // A version past the head does not exist yet, and one below the fold no longer can be read: its histories were
  // folded to the value in effect at the floor (and a recovered green starts at its checkpoint, W3), so it would be
  // answered as the floor's state or a file missing (`NotFound` for a file that was there). Unknown either way.
  if version > engine.head() || version < engine.folded_below() {
    return Err(Refusal::UnknownBase {
      green: wire_id(green),
      version,
    });
  }
  Ok(version)
}

/// A plain volume's file page at its head, through the volume core (an overlay reads untouched entries from the
/// host); the inode's change counter is its stamp.
fn read_plain_range(
  state: &mut ShardState,
  volume: VolumeId,
  path: &str,
  (offset, want): (u64, u64),
) -> ReplyBody {
  let (handle, _) = match find(state, volume) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  let ShardState { store, volumes, .. } = state;
  let Ok(slot) = volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let mut buffer = vec![0u8; usize::try_from(want).unwrap_or(0)];
  let read = match slot.host.as_mut() {
    Some(host) => {
      let mut overlay = slot.volume.with_host(host);
      overlay.resolve(store, path).and_then(|located| {
        if matches!(located.child, Child::Dir(_)) {
          return Err(slates_vfs::error::VfsError::IsDirectory);
        }
        let observed = overlay.observe(store, located.inode)?;
        let read = overlay.read(store, located.inode, offset, &mut buffer)?;
        Ok((read, observed.attrs.size, observed.change))
      })
    }
    None => slot.volume.resolve(store, path).and_then(|located| {
      if matches!(located.child, Child::Dir(_)) {
        return Err(slates_vfs::error::VfsError::IsDirectory);
      }
      let observed = slot.volume.observe(store, located.inode)?;
      let read = slot
        .volume
        .read(store, located.inode, offset, &mut buffer)?;
      Ok((read, observed.attrs.size, observed.change))
    }),
  };
  match read {
    Ok((read, total, stamp)) => {
      buffer.truncate(read);
      ReplyBody::ReadPage {
        bytes: buffer,
        total,
        stamp,
      }
    }
    Err(e) => refused(refusal_of_vfs(&e)),
  }
}

/// A green's file at the head, a named version, or an attachment's pinned version.
fn read_green(state: &ShardState, green: DbVolumeId, path: &str, at: ReadAt) -> ReplyBody {
  let Some(engine) = state.greens.get(&green) else {
    return refused(Refusal::NotFound);
  };
  let version = match at {
    ReadAt::Head => engine.head(),
    ReadAt::Version { version } => version,
    ReadAt::Attachment { attachment } => match state.merge.attachments.get(&attachment) {
      Some(pin) if pin.green == green => pin.version,
      Some(_) => {
        return refused(Refusal::BadRequest {
          reason: "the attachment pins another green".to_owned(),
        });
      }
      None => return refused(Refusal::NotFound),
    },
  };
  if version > engine.head() {
    return refused(Refusal::UnknownBase {
      green: wire_id(green),
      version,
    });
  }
  match engine.content_at(path, version) {
    Some(bytes) => ReplyBody::ReadBytes { bytes },
    None => refused(Refusal::NotFound),
  }
}

/// A plain volume's file at its head, through the volume core: an overlay through its host (an
/// untouched entry reads the disk), a scratch volume from the store.
fn read_plain(state: &mut ShardState, volume: VolumeId, path: &str) -> ReplyBody {
  let (handle, _) = match find(state, volume) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  let ShardState { store, volumes, .. } = state;
  let Ok(slot) = volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let read = match slot.host.as_mut() {
    Some(host) => {
      let mut overlay = slot.volume.with_host(host);
      overlay.resolve(store, path).and_then(|located| {
        if matches!(located.child, Child::Dir(_)) {
          return Err(slates_vfs::error::VfsError::IsDirectory);
        }
        let size = overlay.stat(store, located.inode)?.size;
        let mut bytes = vec![0u8; usize::try_from(size).unwrap_or(usize::MAX)];
        let read = overlay.read(store, located.inode, 0, &mut bytes)?;
        bytes.truncate(read);
        Ok(bytes)
      })
    }
    None => slot.volume.resolve(store, path).and_then(|located| {
      if matches!(located.child, Child::Dir(_)) {
        return Err(slates_vfs::error::VfsError::IsDirectory);
      }
      let size = slot.volume.stat(store, located.inode)?.size;
      let mut bytes = vec![0u8; usize::try_from(size).unwrap_or(usize::MAX)];
      let read = slot.volume.read(store, located.inode, 0, &mut bytes)?;
      bytes.truncate(read);
      Ok(bytes)
    }),
  };
  match read {
    Ok(bytes) => ReplyBody::ReadBytes { bytes },
    Err(e) => refused(refusal_of_vfs(&e)),
  }
}

/// `destroy` of a merge volume, or `None` for a plain one (the store-backed destroy runs). A work
/// is removed with its declared operations; a green with its engine, its pins and its works'
/// ability to merge (their next submit refuses `UnknownBase`, §4.16 failure matrix). Every
/// attachment of the volume is removed. The catalog records the destroy in one step — a merge
/// volume holds no store content to walk in slices — and the caller needs the admin right.
pub(crate) fn destroy_merge_volume(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
) -> Option<ReplyBody> {
  let id = to_db_volume(volume);
  merge_role(state, id)?;
  let record = match find_record(state, volume) {
    Ok(record) => record,
    Err(refusal) => return Some(refused(refusal)),
  };
  if !rights_of(&record, principal).admin {
    return Some(forbidden("destroy"));
  }
  let now = state.clock.monotonic_ns();
  // The snapshot a green holds as its origin (A-217), given back once its destroy is recorded.
  let held = state.db.partition().green_origin_ref(id).copied();
  let attachments: Vec<u64> = state
    .db
    .partition()
    .attachments_of(id)
    .iter()
    .map(|attachment| attachment.id)
    .collect();
  for attachment in &attachments {
    if let Err(e) = state.db.mutate(
      &mut state.segment,
      &Op::AttachmentRemoved { id: *attachment },
      now,
    ) {
      return Some(refused(refusal_of_db(&e)));
    }
    state.merge.attachments.remove(attachment);
  }
  let ops = [
    Op::VolumeStateChanged {
      id,
      state: VolumeState::Destroying,
    },
    Op::LeaseReleased { volume: id },
    Op::VolumeDestroyed { id },
  ];
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      return Some(refused(refusal_of_db(&e)));
    }
  }
  // A volume whose name hashes elsewhere gives its alias back there (A-236).
  crate::verbs::release_name_alias(state, id, &record.name);
  crate::verbs::retire_local_tombstones(state);
  // A retired work may have held the oldest reachable version of its green: fold and credit. A
  // destroyed green credits everything it charged as retention.
  if let Some(work) = state.works.remove(&id) {
    crate::work_charge::release(state, work.charged);
    // The floor goes with it: on this shard in this transaction, elsewhere by a message once it has committed (A-247).
    crate::work_home::tell_floor(state, work.green, id, None);
    let _ = settle_green_retention(state, work.green);
  }
  crate::verbs::release_origin_snapshot(state, held.as_ref());
  if state.greens.remove(&id).is_some() {
    state.merge.attachments.retain(|_, pin| pin.green != id);
    release_green_retention(state, id);
    // Nothing of a destroyed green is left to place: its owner-side placement state goes with it, so the
    // maps stay bounded by the greens this shard owns (banned item 8).
    let object = ObjectId(id.bytes);
    state.merge.pending.remove(&object);
    state.merge.placed.remove(&object);
    state.merge.placed_holders.remove(&object);
    // Its version 0's inputs, made or being made (a job being made sees its entry gone and ends, A-286).
    state.merge.origin_inputs.remove(&object);
  }
  Some(ReplyBody::Destroyed)
}

/// `status` of a merge volume, or `None` for a plain one: the bytes the green's head (or the
/// work's content) holds, its attachments, the head version as the head snapshot number, and the
/// placement the fleet computes for it. A merge volume has no base, so it drifts nowhere.
pub(crate) fn status_merge_volume(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
) -> Option<ReplyBody> {
  let id = to_db_volume(volume);
  let role = merge_role(state, id)?;
  let record = match find_record(state, volume) {
    Ok(record) => record,
    Err(refusal) => return Some(refused(refusal)),
  };
  if !rights_of(&record, principal).read {
    return Some(forbidden("status"));
  }
  let (bytes, head) = match role {
    MergeRole::Green { .. } => state.greens.get(&id).map_or((0, 0), |engine| {
      (
        engine.files().map(|(_, bytes)| bytes.len() as u64).sum(),
        engine.head(),
      )
    }),
    MergeRole::Work { .. } => state.works.get(&id).map_or((0, 0), |work| {
      (
        work
          .files
          .sizes(work_base(state, work))
          .iter()
          .map(|(_, len)| *len)
          .sum(),
        work.base_version,
      )
    }),
  };
  let attachments = state.db.partition().attachments_of(id).len();
  let placed = crate::verbs::placed_state(state, &record);
  // The transport report is the host's, the same for a green as for any volume (§4.6 A-9).
  let transports = Box::new(crate::transports::report(&crate::transports::situation(
    state,
    &rights_of(&record, principal),
  )));
  Some(ReplyBody::Status {
    report: Box::new(slates_ipc::protocol::StatusReport {
      transports,
      id: volume,
      name: record.name.clone(),
      referenced_bytes: bytes,
      unique_bytes: bytes,
      lease_epoch: record.lease.as_ref().map(|lease| lease.epoch),
      attachments: u32::try_from(attachments).unwrap_or(u32::MAX),
      head: SnapshotId { value: head },
      drifted: Vec::new(),
      watcher: "merge".to_owned(),
      snapshots: u32::try_from(head).unwrap_or(u32::MAX),
      placed,
      nfs_port: u16::try_from(crate::daemon::NFS_PORT.load(std::sync::atomic::Ordering::Acquire))
        .ok()
        .filter(|port| *port != 0),
      // A green has no host mount of its own to bind (§4.16: a green is served through pinned
      // attachments, the record form).
      mounts: Vec::new(),
      mounts_elided: 0,
      // A green or a work volume keeps no op log (its history is the merge engine's), so it has no journal to report.
      journal: slates_ipc::protocol::JournalReport::default(),
      // A merge volume serves no FUSE mount of its own, so nothing is kept for one (U10).
      takeover_kept: 0,
      // A green or a work volume has no base folder (A-299).
      base: None,
      // A green's state is its versions, each committed by its merge record before it is answered (§4.16 "Commit"),
      // not owner-local edits a seal would place; the volume loss window (AC-8.3) does not describe it, so none is
      // reported rather than a zero it was not measured to be.
      loss: slates_ipc::protocol::LossReport::default(),
    }),
  })
}

// ---------------------------------------------------------------------------------------------
// The fleet half (§4.16 "Commit", "Apply on holders", "Submission"; §4.10 "placed before
// committed"; D-27; AC-8.19/T-8.17): a merge record is the next entry of the green's ledger
// register, sent to its `2f + 1` candidate holders under the owner's host epoch, committed at
// `f + 1` — and issued only once every identity it references is placed. Every holder recomputes
// the version from the placed inputs before accepting the record, and refuses loudly on a mismatch.
//
// Owner side. Each accepted version (and the green's creation, version 0) is a pending record on
// the green's owner shard: the version's head identity, the increment's identity, and the
// placement of its inputs and of the record. The record plane (the control shard's coordinator,
// `crate::fleet::run_record_period`) works each holder's first missing version per green per period, in two steps that never reorder: while the version's inputs — the
// increment's ops document and post-state (or the origin) as one archive — are not placed at `f + 1`
// candidates they are put through the same content exchange the seals use (§4.10, verified on
// arrival, hedged at once to every candidate whose next record it is: the inputs are small); only then does the record ship,
// and only to a candidate that already acknowledged the version before it, so every holder sees the
// chain in order. A record that waits on its inputs is counted (`merge.inputs_unplaced`), the
// non-vacuity counter for placed-before-reference. The pending state is bounded by the chain: the
// bytes are the chain's own durable entries (never a second copy), and an entry leaves once every
// candidate holds it. At `f = 0` the record is placed the moment it is appended (R8: the same
// code, the local hold is the quorum), so a laptop keeps nothing pending.
//
// Holder side. A merge record arrives on its own stream (`MERGE_RECORD_STREAM`), dispatched by
// kind. The holder finds the version's inputs in its content hold (else refuses
// `merge.inputs_unheld` — the record waits), gathers them extent by extent with what it holds of
// them charged to its shard's budget (else refuses `merge.inputs_over_budget` — the record waits;
// A-383), replays them into its replica of the green (an
// engine of its own, at the version before), and compares the replica's head identity with the
// record's: a mismatch is refused, counted (`merge.recompute_mismatch`), printed loudly, and the
// green is never served from this holder again (fatal for the green here; the owner and the other
// holders carry on, Degraded — §4.16 failure matrix); a match accepts the record into the holder's
// acceptor exactly as a head record is. An out-of-order record (a version past the replica's next)
// is refused and counted; the owner re-ships it once its predecessor is acknowledged.

use std::collections::BTreeSet;

use slates_archive::Archive;
use slates_cluster::content::{chunk_requests, offer_request};
use slates_cluster::{ClusterError, CommitBudget, commit_record_sent_as};
use slates_db::register::{
  Acceptor, Configuration, HostEpoch, HostId, ObjectId, Placement, Quorum, Record,
};
use slates_merge::engine::{Green, Increment, Outcome};
use slates_wire::Wire;

use crate::fleet::{Dispatch, LateReplies, return_sessions, take_sessions};
use crate::xshard::call_within;

/// The stream a merge record rides between an owner and a candidate holder (§4.16 "Commit"):
/// its own id, so the holder recomputes before it accepts — never a guess from the bytes.
/// Format: a stream id past every other fleet stream (records 1, promotions 2, content 4–6,
/// configuration 7–8, root 9–10, forwards 11).
pub(crate) const MERGE_RECORD_STREAM: u64 = 12;

/// Counted on the owner each period a merge record waits for its inputs to place before it may
/// be issued (§4.16 "issued only when every identity the version references is placed"): the
/// non-vacuity counter of placed-before-reference.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
pub(crate) const INPUTS_UNPLACED: &str = "merge.inputs_unplaced";
/// Counted on a holder for each merge record that arrived with the inputs it names (§4.16 "Commit" in one exchange a
/// version): the non-vacuity counter for inputs placed inside the record that references them.
pub(crate) const INPUTS_CARRIED: &str = "merge.inputs_carried";
/// Counted on the owner for each shipment that carried only part of what its targets lacked of a version's inputs,
/// the shipment's bound of them (A-286): the non-vacuity counter of inputs larger than one shipment.
/// Format: a refusal name in the daemon's status report.
pub(crate) const INPUTS_PARTIAL: &str = "merge.inputs.partial";
/// Counted on the owner each time a shipment's answers showed a target holding more of a version's inputs than before
/// (it holds them whole, or lacks fewer chunks), which nudges the record plane for the next shipment (A-286).
/// Format: a refusal name in the daemon's status report.
pub(crate) const INPUTS_PROGRESSED: &str = "merge.inputs.progressed";
/// Counted on the owner for each version placed at the quorum in the same coordinator period it was enqueued in
/// (§4.16 "Commit", A-235): its submit's answer did not wait for the coordinator's period.
pub(crate) const PLACED_WITHIN_PERIOD: &str = "merge.placed_within_period";
/// Counted on the owner for each version placed at the quorum only after one or more coordinator periods began since
/// it was enqueued: the answer waited for the period (every version before A-235, which left it to the next period).
pub(crate) const PLACED_AFTER_PERIODS: &str = "merge.placed_after_periods";
/// Counted on a holder that refused a content put under an injected fault (a test's placement
/// refusal), so a record's wait is shown to be the refusal's doing.
/// Format: a refusal name in the daemon's status report.
pub(crate) const CONTENT_PUT_REFUSED: &str = "merge.content_put_refused";
/// Counted on a holder asked to accept a merge record whose inputs it does not hold; the record
/// waits (`ContentUnavailable`, retryable).
/// Format: a refusal name in the daemon's status report.
pub(crate) const INPUTS_UNHELD: &str = "merge.inputs_unheld";
/// Counted on a holder whose recomputation of a version did not reproduce the record's identity —
/// fatal for the green on that holder (§4.16 "mismatch fatal-and-loud").
/// Format: a refusal name in the daemon's status report.
pub(crate) const RECOMPUTE_MISMATCH: &str = "merge.recompute_mismatch";
/// Counted on a holder that received a version past the next one its replica expects (or a
/// version 0 for a green it already replicates differently); the owner re-ships in order.
/// Format: a refusal name in the daemon's status report.
pub(crate) const OUT_OF_ORDER: &str = "merge.out_of_order";
/// Counted on a holder whose held inputs did not decode as an increment or origin (a corrupt hold).
/// Format: a refusal name in the daemon's status report.
pub(crate) const INPUTS_UNDECODABLE: &str = "merge.inputs_undecodable";
/// Counted on a holder whose shard's budget could not take what recomputing a version from its held inputs holds
/// ([`RECOMPUTE_COPIES`] of their declared size, A-383): the record waits, as for inputs not yet held, and its owner
/// ships it again, so the holder recomputes it once its shard has room.
/// Format: a refusal name in the daemon's status report.
pub(crate) const INPUTS_OVER_BUDGET: &str = "merge.inputs_over_budget";
/// Derived: the copies of a version's inputs a holder's recomputation holds at once (A-383): the bytes gathered from its
/// hold, and the increment decoded from them, whose post-state is a copy of at most as many (`Increment::decode`).
const RECOMPUTE_COPIES: u64 = 2;

/// Format: the one entry of an inputs archive's manifest — a file holding the version's inputs.
pub(crate) const INPUTS_ENTRY_NAME: &str = "inputs";
/// Format: the mask a test's corruption fault XORs into one inputs byte — every bit flipped, so
/// the corrupted byte can never equal the original whatever its value.
const CORRUPTION_MASK: u8 = 0xff;

/// Counted each pass a version's record waits for its submitting work's catalog record to commit (A-255).
/// Format: a refusal name in the daemon's status report.
pub(crate) const RECORD_AWAITS_WORK_CATALOG: &str = "merge.record.awaits_work_catalog";
/// Format: a merge record's magic, "GMRC" little-endian, after the register's class byte: an unversioned record an
/// earlier layout wrote begins with its version number, whose low word is the magic only at version 1,129,467,207.
const RECORD_MAGIC: u32 = u32::from_le_bytes(*b"GMRC");
/// Format: a merge record's format version (A-255: 1 is the first versioned format, with the submit's completion key; 2
/// carries the submitting work beside the key, A-288); a decoder refuses any other.
const RECORD_VERSION: u32 = 2;
/// Format: a merge record's magic and its version are each a little-endian `u32`.
const RECORD_WORD_BYTES: usize = size_of::<u32>();
/// Format: the magic and version before a merge record's value.
const RECORD_FRAME_BYTES: usize = 2 * RECORD_WORD_BYTES;
/// Counted where a merge record of another format version was refused (a peer of another build).
/// Format: a refusal name in the daemon's status report.
pub(crate) const RECORD_VERSION_REFUSED: &str = "merge.record.version_refused";
/// Counted where a merge record's bytes were not its magic, version and one value.
/// Format: a refusal name in the daemon's status report.
pub(crate) const RECORD_MALFORMED: &str = "merge.record.malformed";

/// A merge record's value (§4.16 `MergeRecord`): what the holders recompute against. The
/// increment's bytes are never here (AC-6.5: an increment is a constant-size descriptor); they are
/// the placed inputs the `inputs` identity names. Encoded by the daemon's canonical codec, so two
/// hosts encode one record identically (the acknowledgement binds the record's identity over it).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct MergeRecordValue {
  /// The version this record commits.
  pub version: u64,
  /// The increment's identity (all zeros for version 0, which commits the origin).
  pub increment: [u8; 32],
  /// The version the increment was based on (0 for version 0).
  pub base: u64,
  /// The manifest identity of the placed inputs (the increment, or the origin), or none for a
  /// scratch green's version 0 (nothing to place).
  pub inputs: Option<[u8; 32]>,
  /// The green's head identity after this version (`Green::head_identity`).
  pub identity: [u8; 32],
  /// The evidence references the submitter attached (opaque).
  pub evidence: Vec<[u8; 32]>,
  /// The green's mount name, so a successor materializes it under the same name (AUD-14).
  pub name: String,
  /// Whether the green requires evidence on every submit (`Role::Green { require_evidence }`).
  pub require_evidence: bool,
  /// The owning principal, so a successor enforces the same rights.
  pub owner: Principal,
  /// The completion key of the submit that made this version, where a request made it (A-255): the record is the
  /// green's, and RIFL ties a request's completion record to the object it changed ("If the object migrates, then the
  /// completion record must move with it", Lee et al., SOSP 2015, §3). A green's successor records it, so the submit's
  /// resend after a takeover is answered its version. Appended.
  pub completion: Option<SubmitCompletion>,
  /// How the signature of the submit that made this version was judged (A-321), as the version's chain entry records
  /// it: a successor records it with the version, so the version's provenance outlives its owner. `None` for an
  /// unsigned submit and for version 0, which no submit made. Appended.
  pub signature: Option<slates_db::catalog::SignatureVerdict>,
}

/// A submit's completion key as a merge record carries it ([`MergeRecordValue::completion`]): the client's host, the
/// client and the request's sequence, the key a partition's completion window holds it under; and, for a client on
/// another node than the work's owner, the work the submit was made from, which a green's successor records the
/// completion under (A-288), so the work's route is kept only while a completion that route can reach is held.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmitCompletion {
  /// The client's host (`HostId.0`): this node for a local client, the authenticated origin for a forwarded one.
  pub origin: u64,
  /// The client.
  pub client: u32,
  /// The request's sequence within the client.
  pub sequence: u32,
  /// The work the submit was made from, for a submit forwarded from another node: not part of the key, the route a
  /// resend under it takes after a takeover (A-255, A-288). `None` for a client of the work's owner's own node: its
  /// resend is served by that node, from the records it made, and never takes the route; if the node is gone, so is it.
  pub work: Option<DbVolumeId>,
}

impl SubmitCompletion {
  /// The key of the request now running on `state`, a submit of `work`, if a request is running
  /// (`ShardState::current_request`); the work is named only for a request another node forwarded here.
  pub(crate) fn of_current(state: &ShardState, work: DbVolumeId) -> Option<SubmitCompletion> {
    let local = state.origin_anchor.0;
    state.current_request.map(|(origin, id)| SubmitCompletion {
      origin,
      client: id.client,
      sequence: id.sequence,
      work: (origin != local).then_some(work),
    })
  }

  /// As the green's chain entry records it.
  pub(crate) fn to_entry(self) -> slates_merge::chain_entry::Completion {
    slates_merge::chain_entry::Completion {
      origin: self.origin,
      client: self.client,
      sequence: self.sequence,
      work: self.work.map(|work| work.bytes),
    }
  }

  /// As a chain entry recorded it.
  pub(crate) fn of_entry(key: slates_merge::chain_entry::Completion) -> SubmitCompletion {
    SubmitCompletion {
      origin: key.origin,
      client: key.client,
      sequence: key.sequence,
      work: key.work.map(|bytes| DbVolumeId { bytes }),
    }
  }
}

impl MergeRecordValue {
  /// The value's canonical bytes, behind the merge register's class byte (`crate::catalog::MERGE_CLASS`): the format's
  /// magic and version, then the value.
  pub fn to_record_bytes(&self) -> Vec<u8> {
    let body = self.to_bytes();
    let mut framed = Vec::with_capacity(RECORD_FRAME_BYTES.saturating_add(body.len()));
    framed.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
    framed.extend_from_slice(&RECORD_VERSION.to_le_bytes());
    framed.extend(body);
    crate::catalog::tagged(crate::catalog::MERGE_CLASS, framed)
  }

  /// Parses a record's value, refusing typed: another register class, another format version (a record a build of
  /// another layout wrote, refused whole and never decoded by a second path, CLAUDE.md §2.7), or bytes that are not
  /// exactly one value.
  pub fn decode_record(bytes: &[u8]) -> Result<MergeRecordValue, RecordRefused> {
    let body = crate::catalog::untagged(crate::catalog::MERGE_CLASS, bytes)
      .ok_or(RecordRefused::OtherClass)?;
    let (magic, rest) = body
      .split_first_chunk::<RECORD_WORD_BYTES>()
      .ok_or(RecordRefused::Malformed)?;
    if u32::from_le_bytes(*magic) != RECORD_MAGIC {
      return Err(RecordRefused::Malformed);
    }
    let (version, mut input) = rest
      .split_first_chunk::<RECORD_WORD_BYTES>()
      .ok_or(RecordRefused::Malformed)?;
    let found = u32::from_le_bytes(*version);
    if found != RECORD_VERSION {
      return Err(RecordRefused::Version {
        found,
        expected: RECORD_VERSION,
      });
    }
    let value =
      <MergeRecordValue as Wire>::decode(&mut input).map_err(|_| RecordRefused::Malformed)?;
    if input.is_empty() {
      Ok(value)
    } else {
      Err(RecordRefused::Malformed)
    }
  }

  /// [`Self::decode_record`] for a caller that only asks whether bytes are a merge record of this format: `None` for any
  /// refusal, a record of another version included.
  pub fn from_record_bytes(bytes: &[u8]) -> Option<MergeRecordValue> {
    Self::decode_record(bytes).ok()
  }
}

/// Why bytes are not a merge record of this build's format ([`MergeRecordValue::decode_record`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordRefused {
  /// A register value of another class (a head, a catalog, a tombstone).
  OtherClass,
  /// A merge record of another format version.
  Version {
    /// The version the bytes carry.
    found: u32,
    /// This build's version.
    expected: u32,
  },
  /// A merge record's class whose bytes are not its magic, its version and exactly one value.
  Malformed,
}

impl RecordRefused {
  /// The counter a holder or successor records a refused record under; `None` for another class, which is no refusal.
  pub(crate) fn counter(self) -> Option<&'static str> {
    match self {
      Self::OtherClass => None,
      Self::Version { .. } => Some(RECORD_VERSION_REFUSED),
      Self::Malformed => Some(RECORD_MALFORMED),
    }
  }
}

/// Why a green's recovered history was fenced rather than served (AUD-29-18; §4.16, D-27): every durable
/// chain entry was an acknowledged version, so a replay that cannot reproduce each as an acceptance at its
/// own version would present a shorter or different history under the same id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GreenFence {
  /// The origin (version 0) does not decode.
  OriginCorrupt,
  /// The chain entry for `version` does not decode.
  EntryCorrupt {
    /// The version the entry was acknowledged as.
    version: u64,
  },
  /// Replaying the chain entry for `version` did not reach the head identity the entry recorded (A-257).
  IdentityDiverged {
    /// The version the entry was acknowledged as.
    version: u64,
  },
  /// The chain entry for `version` was accepted as another version.
  WrongVersion {
    /// The version the entry was acknowledged as.
    version: u64,
    /// The version the replay accepted it as.
    accepted: u64,
  },
  /// The origin is held as a reference to a base snapshot (A-217) that recovery did not bring back: the base volume
  /// or its snapshot is not on this shard.
  OriginUnavailable,
  /// The green's checkpoint (W3) does not decode, or names another version than the partition holds it at.
  CheckpointCorrupt,
  /// The retention the rebuilt histories hold could not be charged: `short` bytes past the budget.
  Retention {
    /// The bytes the budget could not cover.
    short: u64,
  },
  /// The green was adopted by this node's takeover from its own holder ledger (A-333), and the boot could not rebuild
  /// its engine from that ledger: a record or an input it names is not held here, or the rebuild did not reach the
  /// adopted identity.
  LedgerUnavailable,
}

/// A green's outstanding replication (§4.16, AUD-13). The per-holder ordered positions index
/// has one entry per missing acknowledgement, bounded by the pending records times their candidate
/// count. Selecting the next shipment reads one position per holder, independent of chain length.
#[derive(Default)]
pub struct PendingGreen {
  records: BTreeMap<u64, PendingMergeRecord>,
  owed: BTreeMap<HostId, BTreeSet<u64>>,
}

impl PendingGreen {
  /// The oldest version whose record is not yet placed, if any: its inputs are read from the chain until it is.
  fn oldest(&self) -> Option<u64> {
    self.records.keys().next().copied()
  }

  /// Registers one new position and the holders still owed it. Re-enqueuing a position preserves
  /// its acknowledgements; a pending position's identity never changes (§4.8 Continuity).
  fn insert(&mut self, version: u64, record: PendingMergeRecord) {
    if self.records.contains_key(&version) {
      return;
    }
    for host in &record.record.candidates {
      if !record.record.acked.contains(host) {
        self.owed.entry(*host).or_default().insert(version);
      }
    }
    self.records.insert(version, record);
  }

  /// Re-derives every pending position's placement from `placement`, the owner's current placement of the green: a
  /// position keeps the acknowledgements it gathered (each a durable hold) and is owed to the candidates that still
  /// lack it. A position is enqueued under the configuration the owner holds then, which can be the one-candidate
  /// placement a bootstrap installs before its council grows (§4.8); at `f = 1` that placement can never commit, so
  /// a position kept to it never placed and no submit to the green was answered (the bug record of 2026-11-06). Head
  /// records are placed by the current configuration each period (`fleet::owed_record`); merge records now are too.
  fn retarget(&mut self, placement: &Placement) {
    let mut changed = false;
    for pending in self.records.values_mut() {
      if pending.record.candidates != placement.candidates
        || pending.record.joint != placement.joint
      {
        pending.record = Placement {
          acked: std::mem::take(&mut pending.record.acked),
          mirror_acked: pending.record.mirror_acked.take(),
          ..placement.clone()
        };
        changed = true;
      }
      if pending.inputs.candidates != placement.candidates
        || pending.inputs.joint != placement.joint
      {
        pending.inputs = Placement {
          acked: std::mem::take(&mut pending.inputs.acked),
          mirror_acked: pending.inputs.mirror_acked.take(),
          ..placement.clone()
        };
        changed = true;
      }
    }
    if !changed {
      return;
    }
    for pending in self.records.values_mut() {
      pending
        .lacking
        .retain(|host, _| placement.candidates.contains(host));
    }
    self.owed.clear();
    for (version, pending) in &self.records {
      for host in &pending.record.candidates {
        if !pending.record.acked.contains(host) {
          self.owed.entry(*host).or_default().insert(*version);
        }
      }
    }
  }

  /// One first missing position per remote holder, grouped into the record dispatches that may
  /// share a quorum. A holder cannot skip its own prefix to follow a faster holder.
  fn next(&self, local: HostId) -> BTreeMap<u64, Vec<HostId>> {
    let mut selected = BTreeMap::<u64, Vec<HostId>>::new();
    for (host, versions) in &self.owed {
      if *host != local
        && let Some(version) = versions.first()
      {
        selected.entry(*version).or_default().push(*host);
      }
    }
    selected
  }
}

/// A version's merge record awaiting placement on the green's owner shard.
#[derive(Clone, Debug)]
pub struct PendingMergeRecord {
  /// The record's value.
  pub value: MergeRecordValue,
  /// The inputs' placement so far (the owner holds its own from the start).
  pub inputs: Placement,
  /// The record's placement so far (the owner's own hold is counted by the commit).
  pub record: Placement,
  /// The coordinator period the version was enqueued in (the period of the record plane's latest pass over its owner
  /// shard), or of its first pass when nothing had passed yet: what [`PLACED_WITHIN_PERIOD`] and
  /// [`PLACED_AFTER_PERIODS`] are judged from when the version places. Stamped at the enqueue, so a version the
  /// record plane leaves for its next period counts as having waited for it.
  pub first_period: Option<u64>,
  /// The work whose submit made the version, while its owner holds it in memory (A-255): the record is not shipped
  /// before that work's catalog record is committed at its quorum, so a holder never holds a submit's completion
  /// without the route a resend reaches it by. `None` for a version no submit made (a green's version 0) and for a
  /// record rebuilt at a restart, whose work's catalog was owed before the crash and is still owed by this owner.
  pub work: Option<DbVolumeId>,
  /// What each target that does not hold the inputs whole said it lacks of them: the chunks its stage still owes, as
  /// the content plane's answer to the record's offer named them (A-286). A shipment carries at most its bound of
  /// these, so inputs larger than one shipment cross in several; a target not here is sent the inputs from their
  /// first chunk. Bounded by the candidates times the inputs' chunks; a target leaves once it holds the inputs.
  pub lacking: BTreeMap<HostId, Vec<[u8; 32]>>,
}

/// Faults a test injects on a holder (never reachable from the wire): refuse every content put
/// (a placement refusal, so a record must wait), or corrupt the next inputs a merge record is
/// recomputed from (so the holder's identity mismatches).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeFault {
  /// Refuse every content put while set.
  pub refuse_content_puts: bool,
  /// Corrupt the next inputs recomputed from, once.
  pub corrupt_next_inputs: bool,
  /// Withhold every merge record's acknowledgement while set (the record is neither recomputed nor
  /// accepted; the owner counts no acknowledgement), so a version whose inputs placed still cannot
  /// commit at the quorum (AUD-11).
  pub refuse_records: bool,
  /// Withhold the acknowledgement of every work's catalog record while set (A-255), so a submitting work's catalog
  /// stays uncommitted and the version's merge record must wait for it.
  pub refuse_work_catalogs: bool,
}

/// What a holder holds of a green's replica, for a test to observe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HolderMergeState {
  /// The replica's head version, or none when the holder replicates nothing for the green.
  pub version: Option<u64>,
  /// Whether the holder refused the green for good after a recomputation mismatch.
  pub refused: bool,
}

impl MergeShardState {
  /// The version-0 or version-N inputs of `green` as one archive: a manifest with one file named
  /// [`INPUTS_ENTRY_NAME`] over the bytes cut into raw chunks of the inputs chunk size
  /// (`crate::merge_origin::inputs_chunk_bytes`, the archive's largest chunk; A-286), in the canonical form every reader
  /// admits (AUD-29-13: a chunk past the declared maximum is refused; an empty file has no extent). Deterministic: the
  /// manifest identity depends on the bytes, the entry's name and size, the chunk size and the default root metadata
  /// alone — `page` and `created_unix` are informational header fields outside it — and it is
  /// [`Archive::manifest_identity`], the one identity the put, the hold and the record all name (the tree's own identity
  /// is not it since the root's metadata joined the manifest, format minor 2). The same archive a stream of the bytes
  /// pushed in any cut makes (`crate::merge_origin::InputsBuilder`), which is how version 0's origin is archived a share
  /// at a time. Until 2026-09-30 the bytes were one chunk whatever their length, past the maximum the archive declared;
  /// until 2026-11-17 they were cut at the host page, 100,000 to 400,000 extents for a kernel-sized origin.
  pub(crate) fn inputs_archive(bytes: &[u8], created_unix: u64, page: u32) -> Archive {
    crate::merge_origin::InputsBuilder::whole(bytes).finish(created_unix, page)
  }

  /// The identity a merge record names a version's inputs by: the inputs archive's manifest identity —
  /// the root's metadata and the tree (format minor 2) — which is what the owner's put binds and the
  /// holder's hold keys by (`ContentHold::hold`, `archive_of`), so a holder that acknowledged the put
  /// finds the inputs when the record arrives. The tree's own identity is not it: a record naming that
  /// named an archive no holder ever held, and waited `INPUTS_UNHELD` for good
  /// (`docs/bugs/2026-09-16-merge-record-names-inputs-by-the-tree-only-identity.md`).
  /// Format: the archive's 32-byte manifest identity (`Archive::manifest_identity`).
  fn inputs_identity(bytes: &[u8], page: u32) -> [u8; 32] {
    Self::inputs_archive(bytes, 0, page).manifest_identity()
  }
}

/// The inputs a version references, as the chain holds them: the origin for version 0 (none for a
/// scratch green), the increment's chain entry for version N.
fn inputs_of(state: &ShardState, green: DbVolumeId, version: u64) -> Option<Vec<u8>> {
  if version == 0 {
    // Version 0's inputs, a green's origin, are made a share at a time and kept until placed (`crate::merge_origin`,
    // A-286); a scratch green's version 0 has none.
    return None;
  }
  // The chain holds the versions after its checkpoint (W3), or after its adoption by its takeover (A-333); a version
  // folded into a checkpoint has no inputs of its own any more, and `checkpoint_floor` keeps every version whose record
  // waits for them above it. An adopted version's inputs are this node's ledger's, and its records were placed by the
  // departed owner and re-committed by the takeover, so none waits here.
  let held = state.db.partition().green_floor(green);
  let index = usize::try_from(version.checked_sub(held)?.checked_sub(1)?).ok()?;
  let entry = state.db.partition().green_chain(green).get(index)?;
  // The record's inputs are the increment a holder judges afresh (§4.16 "Apply on holders"), not what this node resolved.
  slates_merge::chain_entry::increment_bytes(entry)
    .ok()
    .map(<[u8]>::to_vec)
}

/// The page the inputs archive is chunked at (informational in the header; the identity is the
/// bytes').
pub(crate) fn archive_page(state: &ShardState) -> u32 {
  u32::try_from(state.store.content.page()).unwrap_or(u32::MAX)
}

/// Enqueues the merge record of `green`'s `version` for placement (§4.16 "Commit"), on the green's
/// owner shard, right after the version was appended durably: at `f = 0` the local append is the
/// placement and the record is placed at once; at `f > 0` it waits for the record plane. The
/// inputs' identity is computed from the chain's own bytes, so the record names exactly what a
/// holder will recompute from.
pub(crate) fn enqueue_record(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  increment: [u8; 32],
  base: u64,
  evidence: Vec<[u8; 32]>,
  (completion, work): (Option<SubmitCompletion>, Option<DbVolumeId>),
) {
  enqueue_record_with(
    state,
    green,
    version,
    (increment, base, evidence, completion),
    work,
    |state| {
      state
        .greens
        .get_mut(&green)
        .map(Green::head_identity)
        .unwrap_or_default()
    },
  );
}

/// One version's merge record as a recovery replay rebuilt it (§4.16): what its owner enqueued when the version
/// was appended, with the head identity the green had at that version.
pub(crate) struct ReplayedVersion {
  /// The version.
  pub(crate) version: u64,
  /// The increment's identity (zero for version 0, the green's creation).
  pub(crate) increment: [u8; 32],
  /// The version the increment was based on.
  pub(crate) base: u64,
  /// The evidence the increment carried.
  pub(crate) evidence: Vec<[u8; 32]>,
  /// The green's head identity at this version.
  pub(crate) identity: [u8; 32],
  /// The completion key of the submit that made it, as its chain entry recorded it (A-255).
  pub(crate) completion: Option<SubmitCompletion>,
}

/// Enqueues a recovered version's merge record on a restarted owner (§4.16 "Commit", AUD-11): the pending records
/// lived in memory, so without this a version appended before the restart and not yet committed at `f + 1` was never
/// shipped again and a submit waiting on it was never answered (the bug record of 2026-11-07, a version appended
/// before its owner restarted never committed). The record is placed from the configuration the owner holds when it
/// ships, as every pending record is (`PendingGreen::retarget`); a holder that already holds the version acknowledges
/// it again at no cost.
pub(crate) fn enqueue_recovered_record(
  state: &mut ShardState,
  green: DbVolumeId,
  replayed: ReplayedVersion,
) {
  let ReplayedVersion {
    version,
    increment,
    base,
    evidence,
    identity,
    completion,
  } = replayed;
  enqueue_record_with(
    state,
    green,
    version,
    (increment, base, evidence, completion),
    None,
    move |_| identity,
  );
}

/// [`enqueue_record`] with the version's head identity from `identity_of`, asked only for a record that goes to
/// holders.
fn enqueue_record_with(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
  (increment, base, evidence, completion): ([u8; 32], u64, Vec<[u8; 32]>, Option<SubmitCompletion>),
  work: Option<DbVolumeId>,
  identity_of: impl FnOnce(&mut ShardState) -> [u8; 32],
) {
  let object = ObjectId(green.bytes);
  let config = state.fleet.configuration();
  let quorum = config.quorum;
  // The record's placement: joined with the settled cohort while this owner's neighbourhood change is in
  // flight (§4.8 "joint writes"), so every version it commits stays at `f + 1` of the cohort a takeover
  // recovers through.
  let local_hold = Placement {
    acked: vec![state.fleet.host()],
    mirror_acked: None,
    ..config.place(object)
  };
  if local_hold.placed(quorum) {
    // The laptop degenerate (R8): the owner is the quorum, the append was the commit.
    let placed = state.merge.placed.entry(object).or_insert(version);
    *placed = (*placed).max(version);
    if *placed == version {
      state
        .merge
        .placed_holders
        .insert(object, (version, local_hold.acked));
    }
    return;
  }
  // What a successor needs to materialize the green under the same name, policy and rights (AUD-14).
  let (name, require_evidence, owner) = catalog_facts(state, green);
  // The record's identities are what a holder recomputes and compares (§4.16 "Apply on holders"), so
  // they are made only for a record that goes to holders. The laptop's owner is the only holder and
  // answers above: there the head identity, a hash of every byte the green holds, had been made on
  // every accept and dropped (113 of 120 profile samples of a 1,000-file green's submit, 2026-11-04).
  let identity = identity_of(state);
  // The version's provenance (A-321), recorded in the green's engine when its entry became durable.
  let signature = state
    .greens
    .get(&green)
    .and_then(|engine| engine.signature(version))
    .map(crate::signed_work::shipped);
  let mut value = MergeRecordValue {
    version,
    increment,
    base,
    inputs: None,
    identity,
    evidence,
    name,
    require_evidence,
    owner,
    completion,
    signature,
  };
  // Version 0 of a green over a base names its origin as its inputs: made a share at a time, and the record enqueued
  // once they are (A-286), never walked, encoded and archived in this poll.
  if version == 0 && crate::merge_origin::has_origin(state, green) {
    crate::merge_origin::make_origin_inputs(state, green, value);
    return;
  }
  value.inputs = inputs_of(state, green, version)
    .map(|bytes| MergeShardState::inputs_identity(&bytes, archive_page(state)));
  insert_pending(state, object, value, work);
}

/// The green's mount name, whether it requires evidence, and its owner, as its catalog record holds them: what a
/// successor materializes it under (AUD-14). With no record, an owner no real principal is (host 0 is no node's
/// anchor, every anchor being a hash).
fn catalog_facts(state: &ShardState, green: DbVolumeId) -> (String, bool, Principal) {
  state
    .db
    .partition()
    .volume(green)
    .map(|record| {
      (
        record.name.clone(),
        matches!(
          record.policy.role,
          slates_db::catalog::Role::Green {
            require_evidence: true,
            ..
          }
        ),
        record.owner.clone(),
      )
    })
    .unwrap_or_else(|| {
      (
        String::new(),
        false,
        Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
      )
    })
}

/// Puts a version's merge record among those this owner shard owes its candidate holders (§4.16 "Commit"), placed by
/// the configuration it holds now (the record plane re-derives it every pass, `PendingGreen::retarget`), the owner's
/// own hold of the inputs counted, and nudges the record plane. Stamped with the coordinator period it is enqueued in,
/// the record plane's latest pass over this shard (A-235).
pub(crate) fn insert_pending(
  state: &mut ShardState,
  object: ObjectId,
  value: MergeRecordValue,
  work: Option<DbVolumeId>,
) {
  let local = state.fleet.host();
  let shape = state.fleet.configuration().place(object);
  let stamped = (state.merge.coordinator_period > 0).then_some(state.merge.coordinator_period);
  let version = value.version;
  state.merge.pending.entry(object).or_default().insert(
    version,
    PendingMergeRecord {
      value,
      inputs: Placement {
        acked: vec![local],
        mirror_acked: None,
        ..shape.clone()
      },
      record: Placement {
        acked: Vec::new(),
        mirror_acked: None,
        ..shape
      },
      first_period: stamped,
      work,
      lacking: BTreeMap::new(),
    },
  );
  nudge_record_plane(state);
}

/// Tells the record plane that this owner shard has merge work it can do now (§4.16 "Commit"): a version to
/// place, or one whose inputs or predecessor were just acknowledged. The coordinator runs the merge plane at once
/// rather than waiting out its period, so a submit's answer waits for the exchange, not for the coordinator
/// (before, every answer at `f > 0` waited out at least one period and usually two, 208 ms at the least: the bug
/// record of 2026-11-06, a submit at `f = 1` waited for the coordinator's period). Only an event that makes new
/// work nudges, so a holder that never answers costs no pass beyond the period's own.
pub(crate) fn nudge_record_plane(state: &mut ShardState) {
  let control = state.shards.first().copied().unwrap_or(state.shard);
  if control == state.shard {
    nudge(state);
  } else {
    crate::xshard::run_on_counted(state.shard, control, nudge);
  }
}

/// Marks the control shard's record plane nudged and wakes the coordinator if it is waiting.
fn nudge(state: &mut ShardState) {
  state.merge.nudged = true;
  if let Some(waker) = state.merge.nudge_waker.take() {
    waker.wake();
  }
}

/// Ready once the record plane has been nudged since it last ran (on the control shard, by the coordinator); the
/// nudge is consumed.
pub(crate) async fn nudged() {
  std::future::poll_fn(|cx| {
    crate::state::with_state(|s| {
      if std::mem::take(&mut s.merge.nudged) {
        std::task::Poll::Ready(())
      } else {
        s.merge.nudge_waker = Some(cx.waker().clone());
        std::task::Poll::Pending
      }
    })
    .unwrap_or(std::task::Poll::Pending)
  })
  .await;
}

/// `await placed(green, region)` (§4.4, §4.16): whether every committed version's merge record
/// is placed at `f + 1` candidates. The mirror scope is refused as it is for every volume at `f =
/// 0` (no mirror exists). `None` for a volume that is not a green.
pub(crate) fn await_placed_green(
  state: &ShardState,
  principal: &Principal,
  volume: VolumeId,
  scope: slates_ipc::protocol::Scope,
) -> Option<ReplyBody> {
  let id = to_db_volume(volume);
  match merge_role(state, id)? {
    MergeRole::Green { .. } => {}
    MergeRole::Work { .. } => {
      return Some(refused(Refusal::Unsupported {
        feature: "await placed on a work volume: submit it to its green".to_owned(),
      }));
    }
  }
  let record = match find_record(state, volume) {
    Ok(record) => record,
    Err(refusal) => return Some(refused(refusal)),
  };
  if !rights_of(&record, principal).read {
    return Some(forbidden("await_placed"));
  }
  if scope == slates_ipc::protocol::Scope::Mirror {
    return Some(refused(Refusal::Unsupported {
      feature: "mirror placement of a green".to_owned(),
    }));
  }
  let head = state.greens.get(&id).map_or(0, Green::head);
  let placed = state
    .merge
    .placed
    .get(&ObjectId(id.bytes))
    .is_some_and(|version| *version >= head);
  Some(ReplyBody::Placed {
    placed,
    mirror_age_ns: None,
  })
}

/// Whether this version's record must carry its inputs to `targets` (§4.16 "Commit", one exchange a version): some
/// target does not hold them yet. A holder accepts a merge record only once it holds the inputs the record names
/// (`accept_merge_record`), so a record that commits at `f + 1` has its inputs held at `f + 1` by construction —
/// placed before it is referenced, with no separate put ahead of it. A version without inputs (an empty green's
/// creation) never carries any.
fn carries_inputs(pending: &PendingMergeRecord, targets: &[HostId]) -> bool {
  pending.value.inputs.is_some()
    && targets
      .iter()
      .any(|host| !pending.inputs.acked.contains(host))
}

/// Format: a merge shipment's frame count, and each frame's length before it, are little-endian `u32`s.
const SHIPMENT_LENGTH_BYTES: usize = size_of::<u32>();

/// A merge shipment (the bytes a holder is sent on [`MERGE_RECORD_STREAM`]): the record, then the content requests
/// that place its inputs (an offer and its chunks), each frame behind its length.
pub(crate) fn encode_shipment(frames: &[Vec<u8>]) -> Vec<u8> {
  let total = frames.iter().fold(SHIPMENT_LENGTH_BYTES, |sum, frame| {
    sum
      .saturating_add(SHIPMENT_LENGTH_BYTES)
      .saturating_add(frame.len())
  });
  let mut bytes = Vec::with_capacity(total);
  bytes.extend_from_slice(
    &u32::try_from(frames.len())
      .unwrap_or(u32::MAX)
      .to_le_bytes(),
  );
  for frame in frames {
    bytes.extend_from_slice(&u32::try_from(frame.len()).unwrap_or(u32::MAX).to_le_bytes());
    bytes.extend_from_slice(frame);
  }
  bytes
}

/// The frames of a merge shipment, or `None` for bytes that are not one (a count or a length past the bytes, or
/// bytes left over). Hostile input never allocates beyond what the bytes themselves can hold: the count is
/// checked against the bytes before any frame is read.
pub(crate) fn decode_shipment(bytes: &[u8]) -> Option<Vec<&[u8]>> {
  let count = read_length(bytes, 0)?;
  if count > bytes.len() / SHIPMENT_LENGTH_BYTES {
    return None;
  }
  let mut frames = Vec::with_capacity(count);
  let mut at = SHIPMENT_LENGTH_BYTES;
  for _ in 0..count {
    let length = read_length(bytes, at)?;
    let begin = at.checked_add(SHIPMENT_LENGTH_BYTES)?;
    let end = begin.checked_add(length)?;
    frames.push(bytes.get(begin..end)?);
    at = end;
  }
  (at == bytes.len()).then_some(frames)
}

/// The little-endian `u32` length at `at` in `bytes`, as a `usize`.
fn read_length(bytes: &[u8], at: usize) -> Option<usize> {
  let end = at.checked_add(SHIPMENT_LENGTH_BYTES)?;
  let word: [u8; SHIPMENT_LENGTH_BYTES] = bytes.get(at..end)?.try_into().ok()?;
  usize::try_from(u32::from_le_bytes(word)).ok()
}

/// One position owed by at least one candidate this period: the record, with its inputs while a target lacks them.
pub(crate) enum MergeWork {
  /// Ship the record to the candidates that hold the version before it and not this one.
  Ship {
    /// The owner shard the pending record lives on.
    shard: u16,
    /// The green.
    object: ObjectId,
    /// The version.
    version: u64,
    /// The record.
    record: Record,
    /// The candidates to reach this period (in order behind the version before).
    targets: Vec<HostId>,
    /// The version's inputs, while the targets do not hold them: they travel with the record, at most a shipment's
    /// bound of them at a time (A-286).
    inputs: Option<ShipInputs>,
    /// The identity the record names its inputs by, so the targets' answers about them are read against it.
    manifest: Option<[u8; 32]>,
    /// The placement the record commits against: every candidate, and the cohorts joined while the owner's
    /// neighbourhood change is in flight, with nothing acknowledged.
    shape: Placement,
    /// The quorum.
    quorum: Quorum,
  },
  /// Ship a submitting work's catalog register, which a version's record waits for (A-255), now rather than at the
  /// coordinator's next period.
  ShipWorkCatalog(crate::fleet::OwedCatalog),
}

/// Ships the catalog register of each work created on this shard whose catalog is not yet committed (A-255), noting
/// each in `catalogs_owed` so a version waiting for the same catalog does not ship it twice in a pass; forgets each work
/// whose catalog is committed or which has no record any more.
fn ship_new_works_catalogs(
  state: &mut ShardState,
  local: HostId,
  catalogs_owed: &mut std::collections::BTreeSet<DbVolumeId>,
  work: &mut Vec<MergeWork>,
) {
  let owing = std::mem::take(&mut state.merge.works_owing_catalog);
  for submitting in owing {
    if crate::fleet::catalog_committed(state, submitting, local) {
      continue;
    }
    let Some(catalog) = crate::fleet::owed_work_catalog(state, submitting, local) else {
      continue;
    };
    state.merge.works_owing_catalog.insert(submitting);
    if catalogs_owed.insert(submitting) {
      work.push(MergeWork::ShipWorkCatalog(catalog));
    }
  }
}

/// The merge work this shard's greens owe this period (§4.16): each candidate's first missing
/// version, grouped by version so one dispatch can still reach a quorum. Each candidate appears in
/// at most one dispatch per green; input placement precedes that version's record. A lagging holder
/// cannot hold another holder at its own old position (AUD-13).
/// Runs in coordinator period `period`: each version this pass works is stamped with the period it was first worked in,
/// which its placement is judged against ([`PLACED_WITHIN_PERIOD`]).
pub(crate) fn next_merge_work(
  state: &mut ShardState,
  local: HostId,
  period: u64,
) -> Vec<MergeWork> {
  // The period a version enqueued on this shard from now on is stamped with.
  state.merge.coordinator_period = period;
  // A green's version 0 inputs whose task could not be spawned are started again (A-286).
  crate::merge_origin::start_every_owed(state);
  let config = state.fleet.configuration().clone();
  let mut work = Vec::new();
  let mut catalogs_owed = std::collections::BTreeSet::new();
  ship_new_works_catalogs(state, local, &mut catalogs_owed, &mut work);
  // A submit off its green's shard waits here, on its work's shard, for the work's route (A-298): a catalog committed
  // since the last pass runs it again.
  crate::work_home::routes_placed(state);
  let objects: Vec<ObjectId> = state.merge.pending.keys().copied().collect();
  for object in objects {
    // The green's catalog register first (AUD-29-17): a successor that adopted a merge record and no catalog could
    // not say who may use the green, so no record ships before the catalog's current version is committed.
    if !crate::fleet::catalog_committed(state, DbVolumeId { bytes: object.0 }, local) {
      continue;
    }
    // Nothing of a green ships while its version 0's inputs are not made (A-286): a holder takes its records in
    // version order, so a later one shipped first would only be refused.
    if crate::merge_origin::inputs_pending(state, object) {
      continue;
    }
    let placement = config.place(object);
    let Some(pending_green) = state.merge.pending.get_mut(&object) else {
      continue;
    };
    pending_green.retarget(&placement);
    let selected = pending_green.next(local);
    for version in selected.keys() {
      if let Some(pending) = pending_green.records.get_mut(version) {
        pending.first_period.get_or_insert(period);
      }
    }
    for (version, targets) in selected {
      ship_version(
        state,
        (&config, local),
        (object, version),
        &targets,
        &mut catalogs_owed,
        &mut work,
      );
    }
  }
  work
}

/// The shipments of `object`'s `version` to `targets` this pass (§4.16 "Commit"): none while the submitting work's
/// catalog is uncommitted (A-255; the catalog ships instead), else one for each group of targets sent the same bytes:
/// the record alone to targets that hold its inputs, and to the rest the record, the offer of its inputs and at most
/// the shipment bound of the chunks each lacks ([`ShipInputs::cut`], A-286).
fn ship_version(
  state: &mut ShardState,
  (config, local): (&Configuration, HostId),
  (object, version): (ObjectId, u64),
  targets: &[HostId],
  catalogs_owed: &mut std::collections::BTreeSet<DbVolumeId>,
  work: &mut Vec<MergeWork>,
) {
  let Some((value, record_shape, submitting, manifest, groups)) = state
    .merge
    .pending
    .get(&object)
    .and_then(|pending_green| pending_green.records.get(&version))
    .map(|pending| {
      (
        pending.value.to_record_bytes(),
        pending.record.clone(),
        pending.work,
        pending.value.inputs,
        ship_groups(pending, targets),
      )
    })
  else {
    return;
  };
  // The submitting work's catalog record before the record that carries its completion (A-255): a successor
  // answers a resend only through the work's catalog, which names the green, so no holder takes the completion
  // before the route is committed. A destroyed work's catalog owes nothing. A work on another shard has no record
  // here, so this passes: its shard asked for the verdict only once its catalog was committed (A-298).
  if let Some(submitting) =
    submitting.filter(|submitting| !crate::fleet::catalog_committed(state, *submitting, local))
  {
    state.count(RECORD_AWAITS_WORK_CATALOG, 1);
    // Shipped in this pass, once however many of its versions wait: its commit nudges the next pass.
    if catalogs_owed.insert(submitting)
      && let Some(catalog) = crate::fleet::owed_work_catalog(state, submitting, local)
    {
      work.push(MergeWork::ShipWorkCatalog(catalog));
    }
    return;
  }
  let record = Record {
    owner: local,
    object,
    sequence: version,
    epoch: record_epoch(state, config, object),
    generation: config.version,
    value,
  };
  let shipments = cut_shipments(state, (object, version), groups);
  for (targets, inputs) in shipments {
    if let Some(inputs) = &inputs {
      state.count(INPUTS_UNPLACED, 1);
      if !inputs.whole {
        state.count(INPUTS_PARTIAL, 1);
      }
    }
    work.push(MergeWork::Ship {
      shard: state.shard,
      object,
      version,
      record: record.clone(),
      targets,
      inputs,
      manifest,
      shape: Placement {
        acked: Vec::new(),
        ..record_shape.clone()
      },
      quorum: config.quorum,
    });
  }
}

/// The epoch `object`'s records are written at: the host's — or, for a green this node **took over**, the takeover's
/// promotion epoch (`placed_heads`, moved here with the green; AUD-14): the holders raised their fence for the
/// departed owner's bumped epoch, and a record below it is refused `StaleEpoch`, which is what left a taken-over green's
/// next version unable to commit. A restart forgets `placed_heads`; the green's record keeps the promotion epoch its
/// adoption was made at (A-333, `fleet::promotion_epoch`), so a restarted successor writes there still.
fn record_epoch(state: &ShardState, config: &Configuration, object: ObjectId) -> HostEpoch {
  let placed = state
    .placed_heads
    .get(&object)
    .map_or(config.host_epoch, |placed| placed.epoch);
  let promoted = crate::fleet::promotion_epoch(state, object).unwrap_or(config.host_epoch);
  HostEpoch(config.host_epoch.0.max(placed.0).max(promoted.0))
}

/// What one merge shipment carries to a group of targets (A-286).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Carry {
  /// The record alone: the targets hold its inputs whole, or it names none.
  Record,
  /// The record and its inputs from their first chunk: what the targets lack of them is not known yet.
  Inputs,
  /// The record and these of its inputs' chunks, which the targets said they lack.
  Lacking(Vec<[u8; 32]>),
}

/// `targets` grouped by what a shipment of `pending` carries to each: a target that holds the inputs gets the record
/// alone ([`carries_inputs`]); the rest, the chunks each said it lacks, or every chunk while it has said nothing. The
/// usual case, every target fresh or every target holding the inputs, is one group, so a version still commits in one
/// exchange (A-244).
fn ship_groups(pending: &PendingMergeRecord, targets: &[HostId]) -> BTreeMap<Carry, Vec<HostId>> {
  let mut groups: BTreeMap<Carry, Vec<HostId>> = BTreeMap::new();
  for host in targets {
    let carry = if !carries_inputs(pending, std::slice::from_ref(host)) {
      Carry::Record
    } else {
      pending
        .lacking
        .get(host)
        .map_or(Carry::Inputs, |lacking| Carry::Lacking(lacking.clone()))
    };
    groups.entry(carry).or_default().push(*host);
  }
  groups
}

/// Each group's targets with the inputs its shipment carries, cut from the version's inputs archive — version 0's as
/// its job made it (A-286), a later version's from its chain entry — made once for every group. A group whose inputs
/// cannot be had (the green destroyed, its entry gone) gets no shipment: nothing is left to place.
fn cut_shipments(
  state: &ShardState,
  (object, version): (ObjectId, u64),
  groups: BTreeMap<Carry, Vec<HostId>>,
) -> Vec<(Vec<HostId>, Option<ShipInputs>)> {
  let bound = shipment_bound(state);
  let owned;
  let archive = if !groups.keys().any(|carry| *carry != Carry::Record) {
    None
  } else if version == 0 {
    crate::merge_origin::made_archive(state, object)
  } else {
    owned = inputs_of(state, DbVolumeId { bytes: object.0 }, version)
      .map(|bytes| MergeShardState::inputs_archive(&bytes, 0, archive_page(state)));
    owned.as_ref()
  };
  groups
    .into_iter()
    .filter_map(|(carry, targets)| {
      let lacking = match carry {
        Carry::Record => return Some((targets, None)),
        Carry::Inputs => None,
        Carry::Lacking(lacking) => Some(lacking),
      };
      archive.map(|archive| {
        (
          targets,
          Some(ShipInputs::cut(
            archive,
            (object, version),
            lacking.as_deref(),
            bound,
          )),
        )
      })
    })
    .collect()
}

/// Derived: the most inputs bytes one merge shipment carries (A-286) — what a holder verifies against their identities
/// in one slice of its step quantum, as this owner measures a slice for itself (`DaemonConfig::archive_slice_bytes`:
/// half the quantum at the measured BLAKE3 rate). At least one chunk always goes, so inputs past the bound cross in
/// several shipments and a holder serves each in about a slice; an ordinary submit's increment is far below it and still
/// crosses whole in one exchange (A-244).
fn shipment_bound(state: &ShardState) -> u64 {
  state.config.archive_slice_bytes().max(1)
}

/// The inputs one merge shipment carries (§4.16 "Commit", A-286): the content plane's offer of the version's inputs
/// archive, and the chunk requests of at most a shipment's bound of the chunks its targets lack.
pub(crate) struct ShipInputs {
  /// The offer: the inputs archive's manifest, with no chunks.
  offer: Vec<u8>,
  /// The chunk requests, in the archive's reference order.
  chunks: Vec<Vec<u8>>,
  /// Whether these are every chunk the targets lack; else the shipment carries a part, and later ones the rest.
  whole: bool,
}

impl ShipInputs {
  /// The shipment of `archive`, `version`'s inputs of `object`, to targets that lack `lacking` of its chunks (every one
  /// when `None`): the offer, and in reference order the chunks they lack up to `bound` bytes — at least one, so a
  /// shipment always carries something they lack.
  fn cut(
    archive: &Archive,
    (object, version): (ObjectId, u64),
    lacking: Option<&[[u8; 32]]>,
    bound: u64,
  ) -> ShipInputs {
    let wanted: Option<BTreeSet<[u8; 32]>> =
      lacking.map(|lacking| lacking.iter().copied().collect());
    let sizes: BTreeMap<[u8; 32], u64> = archive
      .chunks
      .iter()
      .map(|chunk| (chunk.identity, chunk.raw_len))
      .collect();
    let mut batch = Vec::new();
    let mut carried = 0u64;
    let mut owed = 0usize;
    for chunk in archive.referenced_chunks() {
      if wanted
        .as_ref()
        .is_some_and(|wanted| !wanted.contains(&chunk))
      {
        continue;
      }
      owed = owed.saturating_add(1);
      let size = sizes.get(&chunk).copied().unwrap_or(0);
      // In order and without gaps: once one chunk is past the bound, none after it goes this time.
      let next_in_order = batch.len().saturating_add(1) == owed;
      if next_in_order && (batch.is_empty() || carried.saturating_add(size) <= bound) {
        carried = carried.saturating_add(size);
        batch.push(chunk);
      }
    }
    ShipInputs {
      offer: offer_request(archive, object, version),
      chunks: chunk_requests(archive, object, version, &batch),
      whole: batch.len() == owed,
    }
  }
}

/// One period of the merge record plane for the greens `shard` owns, run on the control shard by
/// `crate::fleet::run_record_period` after the seals and heads: each holder's first missing version this
/// period is dispatched over the holders' borrowed sessions, and the acknowledgements are recorded
/// back on the owner shard.
pub(crate) async fn run_merge_period(
  origin: u16,
  shard: u16,
  local: HostId,
  budget: CommitBudget,
  owner_acceptor: &mut Acceptor,
  in_flight: &mut Vec<Dispatch>,
) {
  // The coordinator period this pass runs in, read on the control shard, where the coordinator counts them.
  let period = crate::state::with_state(|s| s.merge.coordinator_period).unwrap_or(0);
  let work = call_within(
    origin,
    shard,
    move |s| next_merge_work(s, local, period),
    crate::daemon::HEARTBEAT_NS,
  )
  .await
  .unwrap_or_default();
  for item in work {
    let dispatch = match item {
      MergeWork::Ship {
        shard,
        object,
        version,
        record,
        targets,
        inputs,
        manifest,
        shape,
        quorum,
      } => {
        ship_record(
          origin,
          shard,
          local,
          owner_acceptor,
          (object, version, record, (inputs, manifest)),
          targets,
          shape,
          quorum,
          (budget, period),
        )
        .await
      }
      MergeWork::ShipWorkCatalog(catalog) => {
        let shipped =
          crate::fleet::ship_owed_catalog(origin, owner_acceptor, local, &catalog, budget).await;
        // A placement that came back is recorded on the catalog's owner shard ahead of the next pass's question there,
        // which this wakes, so the version that waited for it ships at once. Only that nudges, as only an event that
        // makes new work does ([`nudge_record_plane`]): a ship that found no session, or that the runtime refused, sent
        // nothing, and a pass run at once would only ship it again, with nothing awaited between, for good. Before, every
        // ship nudged: on windows-11-arm a work made while its catalog's candidates had no session held the control
        // shard inside its record plane's poll until the watchdog ended the process (g251, diag-g251-spin-3), and the
        // sessions, whose tasks run on that shard, could never come. Such a ship waits for the coordinator's next
        // period. A refused borrow loses this wake and is counted; the coordinator's next period still ships the version.
        if shipped.recorded {
          let _ = crate::state::with_state_counted(nudge_record_plane);
        }
        shipped.dispatch
      }
    };
    if let Some(dispatch) = dispatch {
      in_flight.push(dispatch);
    }
  }
}

/// The inputs a version's record travels with, while its targets lack them (A-286): the shipment, and the identity the
/// record names its inputs by, so the targets' answers about them are read against it.
type VersionInputs = (Option<ShipInputs>, Option<[u8; 32]>);

/// Ships a version's record to its targets over [`MERGE_RECORD_STREAM`] through the owner's own
/// acceptor at `f + 1` — with its inputs (the content exchange's offer and at most a shipment's bound of the chunks the
/// targets lack, A-286) in the same message while the targets lack them, so a version commits in one exchange — and
/// records every acknowledgement on the owner shard: the version is placed once the distinct acknowledgements reach the
/// quorum, each acknowledging holder holds its inputs, and its entry leaves the pending map once every remote candidate
/// holds it. What each target that did not acknowledge answered about the inputs is recorded too
/// ([`record_inputs_answers`]), so the next shipment carries what it still lacks.
#[allow(clippy::too_many_arguments)]
async fn ship_record(
  origin: u16,
  shard: u16,
  local: HostId,
  owner_acceptor: &mut Acceptor,
  (object, version, record, (inputs, manifest)): (ObjectId, u64, Record, VersionInputs),
  targets: Vec<HostId>,
  shape: Placement,
  quorum: Quorum,
  (budget, period): (CommitBudget, u64),
) -> Option<Dispatch> {
  let holders = take_sessions("fleet.lent_to.ship_record", |host| targets.contains(&host));
  if holders.is_empty() {
    return None;
  }
  let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
  let carried = inputs.is_some();
  let mut frames = vec![record.encode()];
  if let Some(inputs) = inputs {
    frames.push(inputs.offer);
    frames.extend(inputs.chunks);
  }
  let committed = commit_record_sent_as(
    MERGE_RECORD_STREAM,
    local,
    owner_acceptor,
    &shape,
    &record,
    encode_shipment(&frames),
    quorum,
    holders,
    budget,
  )
  .await;
  let dispatch = Dispatch::new(
    taken,
    &committed.reusable,
    committed.stragglers,
    LateReplies::Discard,
  );
  return_sessions(committed.reusable);
  let placement = match committed.outcome {
    Ok(placement) => Some(placement),
    Err(ClusterError::Uncertain { placement } | ClusterError::NotPlaced { placement }) => {
      Some(placement)
    }
    Err(_) => None,
  };
  let answers = committed.answers;
  crate::xshard::run_on_counted(origin, shard, move |s| {
    if let Some(placement) = placement {
      record_merge_acks_in(s, local, object, version, placement, quorum, period);
    }
    record_inputs_answers(s, (object, version, manifest), answers, carried);
  });
  Some(dispatch)
}

/// Records what each target that did not acknowledge `version`'s record answered about its inputs, `manifest` (§4.16
/// "Commit", A-286; the holder's side is `inputs_held` and the offer asked again): a content acknowledgement bound to
/// the inputs means it holds them whole, so its next shipment is the record alone; the content plane's missing set for
/// the offer names the chunks it still lacks, which its next shipment carries up to the bound; and a target sent the
/// record alone that answered neither no longer holds them whole, so the next shipment offers them again. A target
/// newly holding the inputs, or lacking fewer of them, nudges the record plane, so a large origin's shipments follow
/// each other at once; a target whose answer did not move waits for the coordinator's period, so one that keeps
/// refusing chunks costs no pass beyond the period's own. A record with no inputs, or no longer pending, records
/// nothing.
fn record_inputs_answers(
  state: &mut ShardState,
  (object, version, manifest): (ObjectId, u64, Option<[u8; 32]>),
  answers: Vec<(HostId, Vec<u8>)>,
  carried: bool,
) {
  let Some(manifest) = manifest else {
    return;
  };
  let Some(pending) = state
    .merge
    .pending
    .get_mut(&object)
    .and_then(|pending_green| pending_green.records.get_mut(&version))
  else {
    return;
  };
  let mut progress = false;
  for (host, answer) in answers {
    match slates_cluster::content::ContentMessage::decode(&answer) {
      Ok(slates_cluster::content::ContentMessage::Ack(ack))
        if ack.holder == host && ack.binds(object, version, &manifest) =>
      {
        pending.lacking.remove(&host);
        if pending.inputs.candidates.contains(&host) && !pending.inputs.acked.contains(&host) {
          pending.inputs.acked.push(host);
          progress = true;
        }
      }
      Ok(slates_cluster::content::ContentMessage::Missing {
        object: offered,
        sequence,
        manifest: named,
        missing,
      }) if offered == object && sequence == version && named == manifest => {
        progress |= pending
          .lacking
          .get(&host)
          .is_none_or(|before| missing.len() < before.len());
        pending.inputs.acked.retain(|held| *held != host);
        pending.lacking.insert(host, missing);
      }
      _ if !carried => {
        pending.inputs.acked.retain(|held| *held != host);
        pending.lacking.remove(&host);
      }
      _ => {}
    }
  }
  if progress {
    state.count(INPUTS_PROGRESSED, 1);
    nudge_record_plane(state);
  }
}

/// Whether every green this shard owns has its newest version's merge record held by `f + 1` of its cohort in
/// the current neighbourhood (§4.8 "the newest committed record is held by f+1 of the new candidates";
/// [`slates_db::register::Configuration::placed_on_current`]): by the
/// holders that acknowledged the version [`MergeShardState::placed`] names, or — for a green this node took
/// over — by the acknowledgements of its adoption the record plane recorded. A green whose newest version has
/// not placed yet is not. A holder can accept a merge record only once it holds every version before it
/// (§4.16 "Apply on holders"), and a new candidate holds none, so a green whose current cohort has fewer than
/// `f + 1` holders of its history keeps its owner's change unsettled — joint writes go on, the settled cohort
/// stays the one a takeover recovers through — until the owed ledger-prefix transfer (GAP-A9-7) ships the
/// history. True with no green.
pub(crate) fn greens_placed_on_current_cohort(state: &ShardState) -> bool {
  let config = state.fleet.configuration();
  state.greens.iter().all(|(id, green)| {
    let object = ObjectId(id.bytes);
    let head = green.head();
    let holders = state
      .merge
      .placed_holders
      .get(&object)
      .filter(|(version, _)| *version == head)
      .map(|(_, holders)| holders.as_slice())
      .or_else(|| {
        state
          .placed_heads
          .get(&object)
          .filter(|placed| placed.sequence == head)
          .map(|placed| placed.placement.acked.as_slice())
      });
    holders.is_some_and(|holders| config.placed_on_current(object, holders))
  })
}

/// Merges a round's acknowledgements into the version's pending record (never overwriting them: each
/// holder's acceptance is durable there), marks the version placed once the quorum holds, and drops
/// the entry once every remote candidate acknowledged it.
/// [`record_merge_acks`] for acknowledgements gathered in coordinator period `period`: a version they newly place is
/// counted [`PLACED_WITHIN_PERIOD`] when it was enqueued in that same period, else
/// [`PLACED_AFTER_PERIODS`] (§4.16 "Commit", A-235: an answer that waited for the coordinator's period).
fn record_merge_acks_in(
  state: &mut ShardState,
  local: HostId,
  object: ObjectId,
  version: u64,
  placement: Placement,
  quorum: Quorum,
  period: u64,
) {
  let before = state
    .merge
    .pending
    .get(&object)
    .and_then(|pending_green| pending_green.records.get(&version))
    .map(|pending| (pending.record.placed(quorum), pending.first_period));
  record_merge_acks(state, local, object, version, placement, quorum);
  if let Some((false, first)) = before
    && placed_version(state, object).is_some_and(|placed| placed >= version)
  {
    let waited = period.saturating_sub(first.unwrap_or(period));
    let counter = if waited == 0 {
      PLACED_WITHIN_PERIOD
    } else {
      PLACED_AFTER_PERIODS
    };
    state.count(counter, 1);
  }
}

fn record_merge_acks(
  state: &mut ShardState,
  local: HostId,
  object: ObjectId,
  version: u64,
  placement: Placement,
  quorum: Quorum,
) {
  let Some(pending_green) = state.merge.pending.get_mut(&object) else {
    return;
  };
  let acknowledged = fold_acknowledgements(pending_green, version, placement.acked);
  let Some(pending) = pending_green.records.get_mut(&version) else {
    return;
  };
  let everyone = pending
    .record
    .candidates
    .iter()
    .all(|host| *host == local || pending.record.acked.contains(host));
  let mut newly_placed = None;
  if pending.record.placed(quorum) {
    let placed = state.merge.placed.entry(object).or_insert(version);
    *placed = (*placed).max(version);
    newly_placed = Some(*placed);
    // The holders of the newest placed version: every acknowledgement of it so far, each a durable hold.
    if *placed == version {
      state
        .merge
        .placed_holders
        .insert(object, (version, pending.record.acked.clone()));
    }
  }
  if everyone {
    pending_green.records.remove(&version);
    if pending_green.records.is_empty() {
      state.merge.pending.remove(&object);
    }
    // Version 0's inputs are kept until every candidate holds them, and no later version names them (A-286).
    if version == 0 {
      state.merge.origin_inputs.remove(&object);
    }
  }
  // A holder that acknowledged this version may take the next one now.
  if acknowledged && state.merge.pending.contains_key(&object) {
    nudge_record_plane(state);
  }
  // The version is committed at the quorum: every submit whose acceptance waited on it is answered
  // now — its completion recorded, its reply delivered (§4.16 "Commit"; AUD-11).
  if let Some(placed) = newly_placed {
    resolve_accepted(state, object, placed);
  }
}

/// Folds a round's acknowledgements into `version`'s pending record, never overwriting earlier ones, and clears
/// what each acknowledging holder was owed of it. A holder accepts a merge record only once it holds the inputs the
/// record names, so its acknowledgement places them too. Whether any holder newly acknowledged the record.
fn fold_acknowledgements(
  pending_green: &mut PendingGreen,
  version: u64,
  acked: Vec<HostId>,
) -> bool {
  let PendingGreen { records, owed } = pending_green;
  let Some(pending) = records.get_mut(&version) else {
    return false;
  };
  let mut acknowledged = false;
  for host in acked {
    if pending.inputs.candidates.contains(&host) && !pending.inputs.acked.contains(&host) {
      pending.inputs.acked.push(host);
    }
    // A holder that accepted the record holds its inputs: nothing of them is owed to it any more.
    pending.lacking.remove(&host);
    if pending.record.candidates.contains(&host) && !pending.record.acked.contains(&host) {
      pending.record.acked.push(host);
      acknowledged = true;
      if let Some(versions) = owed.get_mut(&host) {
        versions.remove(&version);
        if versions.is_empty() {
          owed.remove(&host);
        }
      }
    }
  }
  acknowledged
}

/// Where a deferred reply is written (AUD-11): the shard whose client ring holds the request, the
/// client's slot index there, and the request word — what `crate::state::deliver` needs, kept so the
/// merge plane can answer a submit once its record commits. A verb forwarded from another node has
/// none (its reply travels back on the fleet exchange that carried it, which polls the completion).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyRoute {
  /// The shard the client is served on.
  pub shard: u16,
  /// The client's slot index on that shard.
  pub client_index: u32,
  /// The request word (the request id).
  pub request: u64,
  /// The reply goes to the work's shard, which settles it before the client is answered (A-247), not to a client's
  /// ring: `shard` is the work's, and `client_index` is unused.
  pub through_work: bool,
}

/// One submit whose acceptance waits for its version's merge record to commit at the quorum
/// (§4.16 "Commit"; AUD-11): where to reply, and the completion key to record the reply under on
/// this owner partition when it does. Bounded by the clients' credit: one entry per request in
/// flight, and a retry of a waiting request joins its entry rather than adding a version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwaitingAcceptance {
  /// Where to write the reply; `None` for a request forwarded from another node.
  pub route: Option<ReplyRoute>,
  /// The completion key's origin (the requesting node's stable anchor).
  pub origin: u64,
  /// The request id.
  pub id: RequestId,
}

/// Registers the acceptance of `version` of `green` as **waiting** for its record to commit at the
/// quorum (AUD-11): the reply is not written and the completion not recorded until
/// [`resolve_accepted`] runs for that version. Marks the running verb deferred so `run_recorded`
/// commits its effects without a completion or a reply.
pub(crate) fn defer_acceptance(state: &mut ShardState, green: DbVolumeId, version: u64) {
  let Some((origin, id)) = state.current_request else {
    // No recorded request is running (a rebuild's replay): nothing waits, nothing is deferred.
    return;
  };
  let object = ObjectId(green.bytes);
  let route = state.reply_route.take();
  state
    .merge
    .awaiting
    .entry((object, version))
    .or_default()
    .push(AwaitingAcceptance { route, origin, id });
  state
    .merge
    .awaiting_by_request
    .insert((origin, id.client, id.sequence), (object, version));
  state.acceptance_deferred = true;
  state.count(ACCEPTANCE_DEFERRED, 1);
}

/// Stages, in the running submit's transaction and ahead of its version's append, that its request waits for `version`
/// of `green` to commit (A-252): the append and the wait are one durable step, so a restart keeps the wait wherever it
/// falls. Its failure fails the accept, since a version durable without its wait is one a resend would run again. At
/// `f = 0` the completion recorded in the same transaction replaces it. No recorded request is running in a rebuild's
/// replay, and nothing is staged.
pub(crate) fn stage_awaiting(
  state: &mut ShardState,
  green: DbVolumeId,
  version: u64,
) -> Result<(), slates_db::DbError> {
  let Some((origin, id)) = state.current_request else {
    return Ok(());
  };
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(
      &mut state.segment,
      &slates_db::op::Op::CompletionAwaiting {
        origin,
        client: id.client,
        sequence: id.sequence,
        green,
        version,
      },
      now,
    )
    .map(|_| ())
}

/// Serves a request the partition holds as waiting for `version` of `object` (A-252): typically its resend after the
/// owner restarted and lost the in-memory wait. It never runs again. Answered now, the completion recorded, when the
/// version has committed; else `None`, the request having joined the wait so [`resolve_accepted`] answers it. A
/// completion that cannot be made durable is refused typed, for the client to retry.
pub(crate) fn serve_awaiting(
  state: &mut ShardState,
  origin: u64,
  id: RequestId,
  (object, version): ([u8; 16], u64),
) -> Option<ReplyBody> {
  let green = DbVolumeId { bytes: object };
  let object = ObjectId(object);
  if placed_version(state, object).is_some_and(|placed| placed >= version) {
    let reply = accepted_reply(state, green, version);
    state.db.begin();
    let recorded = crate::verbs::record_completion(state, origin, id, reply);
    return match state.db.commit(&mut state.segment) {
      Ok(_) => {
        state.count(ACCEPTANCE_RESOLVED, 1);
        Some(recorded)
      }
      Err(e) => {
        state.count(ACCEPTANCE_UNRECORDED, 1);
        Some(ReplyBody::Refused {
          refusal: crate::error::refusal_of_db(&e),
        })
      }
    };
  }
  if join_awaiting(state, origin, id) {
    return None;
  }
  let route = state.reply_route.take();
  state
    .merge
    .awaiting
    .entry((object, version))
    .or_default()
    .push(AwaitingAcceptance { route, origin, id });
  state
    .merge
    .awaiting_by_request
    .insert((origin, id.client, id.sequence), (object, version));
  state.count(ACCEPTANCE_REJOINED, 1);
  None
}

/// A retry of a request whose acceptance is still waiting joins the waiting entry — it will be
/// answered with the same reply when the version commits — rather than running the verb again.
/// Returns whether the request was waiting.
pub(crate) fn join_awaiting(state: &mut ShardState, origin: u64, id: RequestId) -> bool {
  let Some(key) = state
    .merge
    .awaiting_by_request
    .get(&(origin, id.client, id.sequence))
    .copied()
  else {
    return false;
  };
  let route = state.reply_route.take();
  if let Some(waiting) = state.merge.awaiting.get_mut(&key)
    && route.is_some()
  {
    waiting.push(AwaitingAcceptance { route, origin, id });
  }
  true
}

/// How the submit that made `green`'s `version` was signed (A-321), as the green's accepted record keeps it on its
/// owner: from the version's chain entry, and once a checkpoint folds that, from the checkpoint. `Unsigned` for a
/// version whose submit carried no signature and for version 0, the origin; `None` for a version past the head, for a
/// green this shard does not hold, and for a verdict this build cannot read.
pub(crate) fn recorded_verification(
  state: &ShardState,
  green: DbVolumeId,
  version: u64,
) -> Option<slates_ipc::protocol::Verification> {
  let engine = state.greens.get(&green)?;
  if version > engine.head() {
    return None;
  }
  crate::signed_work::of_entry(engine.signature(version))
}

/// The answer to a submit `green` accepted at `version` (§4.16; A-321): the version's own record answers, so a submit's
/// first answer, its deferred one (AUD-11) and its resend's after a restart agree. `SubmittedSigned` with the
/// signature's verdict when the version records one, else `Submitted`.
pub(crate) fn accepted_reply(state: &ShardState, green: DbVolumeId, version: u64) -> ReplyBody {
  match recorded_verification(state, green, version) {
    Some(verification @ slates_ipc::protocol::Verification::Verified { .. })
    | Some(verification @ slates_ipc::protocol::Verification::Invalid { .. }) => {
      ReplyBody::SubmittedSigned {
        version: Some(version),
        conflicts: Vec::new(),
        verification,
      }
    }
    Some(slates_ipc::protocol::Verification::Unsigned) | None => ReplyBody::Submitted {
      version: Some(version),
      conflicts: Vec::new(),
    },
  }
}

/// Serves `MergeRecord` (A-321): a committed version's record as a consumer reads it back — how its merge's signature
/// was judged when it merged, its provenance — on the green's owner, to a principal with the read right. Version 0 is
/// the origin, which no merge made, so it reads unsigned; a version not yet committed is `NotFound`.
pub(crate) fn merge_record_verb(
  state: &ShardState,
  principal: &Principal,
  green: VolumeId,
  version: u64,
) -> ReplyBody {
  let record = match require_green(state, principal, green, "merge_record") {
    Ok(record) => record,
    Err(refusal) => return crate::verbs::refused(refusal),
  };
  match recorded_verification(state, record.id, version) {
    Some(verification) => ReplyBody::MergeRecord {
      version,
      verification,
    },
    None => crate::verbs::refused(Refusal::NotFound),
  }
}

/// Answers every submit waiting on a version of `object` at or below `placed` (AUD-11): the
/// acceptance is recorded as the request's completion on this owner partition — one durable step,
/// so a retry after this meets the record — and the reply delivered to where the request came from
/// (a task on that shard, so the delivery never holds this shard's state); a delivery the target's
/// admission refuses is counted, and the completion record answers the client's retry. Forwarded
/// requests have no route: their fleet exchange polls the completion record.
fn resolve_accepted(state: &mut ShardState, object: ObjectId, placed: u64) {
  let versions: Vec<u64> = state
    .merge
    .awaiting
    .range((object, 0)..=(object, placed))
    .map(|((_, version), _)| *version)
    .collect();
  for version in versions {
    let Some(waiting) = state.merge.awaiting.remove(&(object, version)) else {
      continue;
    };
    let reply = accepted_reply(state, DbVolumeId { bytes: object.0 }, version);
    for entry in waiting {
      state
        .merge
        .awaiting_by_request
        .remove(&(entry.origin, entry.id.client, entry.id.sequence));
      state.db.begin();
      let recorded = crate::verbs::record_completion(state, entry.origin, entry.id, reply.clone());
      if state.db.commit(&mut state.segment).is_err() {
        // The completion could not be made durable (rolled back, AUD-06): the client's retry will run
        // the verb again and meet the engine's idempotent accept. Nothing is delivered from memory.
        state.count(ACCEPTANCE_UNRECORDED, 1);
        continue;
      }
      state.count(ACCEPTANCE_RESOLVED, 1);
      let Some(route) = entry.route else {
        continue;
      };
      let (origin, id) = (entry.origin, entry.id);
      if crate::xshard::hand_reply_home(route.shard, move || {
        if route.through_work {
          let _ = crate::state::with_state_counted(|s| {
            crate::work_home::answered_late(s, origin, id, recorded);
          });
          return;
        }
        crate::state::deliver(route.client_index, route.request, recorded, true);
      })
      .is_err()
      {
        state.count(ACCEPTANCE_UNDELIVERED, 1);
      }
    }
  }
}

/// How many submits of `green` are waiting for a version to commit (`Daemon::merge_awaiting`).
pub(crate) fn awaiting_count(state: &ShardState, green: DbVolumeId) -> usize {
  let object = ObjectId(green.bytes);
  state
    .merge
    .awaiting
    .range((object, 0)..=(object, u64::MAX))
    .map(|(_, waiting)| waiting.len())
    .sum()
}

/// Format: refusal and event names the merge plane counts for the deferred acceptance (§4.14).
/// A submit's acceptance was deferred to its version's commit at the quorum.
const ACCEPTANCE_DEFERRED: &str = "merge.acceptance_deferred";
/// A deferred acceptance resolved: the version committed, the completion recorded, the reply sent.
const ACCEPTANCE_RESOLVED: &str = "merge.acceptance_resolved";
/// A request the partition held as waiting rejoined its version's wait rather than run again: its resend under its id
/// after the owner restarted (A-252).
const ACCEPTANCE_REJOINED: &str = "merge.acceptance_rejoined";
/// A resolved acceptance whose completion record could not be made durable (rolled back); the
/// client's retry re-executes and meets the idempotent accept.
const ACCEPTANCE_UNRECORDED: &str = "merge.acceptance_unrecorded";
/// A resolved acceptance whose reply could not be carried to its client's shard (`xshard::hand_reply_home`): that
/// shard gone, as its daemon stops, or this shard's carrier at its bound. The completion record answers the client's
/// resend. (Until goal 238, 2026-11-18, a full channel there lost it too.)
const ACCEPTANCE_UNDELIVERED: &str = "merge.acceptance_undelivered";
/// A holder withheld a merge record's acknowledgement (the injected fault).
const RECORD_WITHHELD: &str = "merge.record_withheld";

/// Where a merge record's version stands against this holder's replica of its green (§4.16 "Apply on holders").
enum RecordStanding {
  /// A version the replica already applied: the acceptor re-acknowledges it (idempotent).
  Applied,
  /// The replica's next version, or version 0 of a green this holder has no replica of: it is recomputed here.
  Next,
}

/// The standing of `version` of `object`'s merge records on this holder, or `None` when nothing may be acknowledged now:
/// the replica is still being rebuilt, or the version is out of order (counted). A holder with the green's records and
/// no replica (a warm restart) rebuilds the replica from them first (A-313).
fn record_standing(
  state: &mut ShardState,
  object: ObjectId,
  version: u64,
) -> Option<RecordStanding> {
  let next = match state.merge.replicas.get(&object) {
    Some(replica) => Some(replica.head().saturating_add(1)),
    // No replica of a green whose records this holder accepted: a warm restart keeps the records and the inputs it held
    // (AUD-29-59), not the engine, which is rebuilt from them a slice at a time (A-313). Before, every later record was
    // refused out of order here, for good.
    None => match crate::merge_origin::rebuild_replica(state, object) {
      crate::merge_origin::Rebuild::Installed(head) => Some(head.saturating_add(1)),
      crate::merge_origin::Rebuild::Later => return None,
      crate::merge_origin::Rebuild::Nothing => None,
    },
  };
  match (next, version) {
    (Some(next), version) if version < next => Some(RecordStanding::Applied),
    (None, 0) => Some(RecordStanding::Next),
    (Some(next), version) if version == next => Some(RecordStanding::Next),
    _ => {
      state.count(OUT_OF_ORDER, 1);
      None
    }
  }
}

/// Whether `record` offers a different value at a position this holder's ledger holds one (A-373): a higher epoch's,
/// once `fleet::check_held_record` admitted it.
fn replaces_held_value(state: &ShardState, record: &Record) -> bool {
  state
    .holder_records
    .get(&record.object)
    .and_then(|acceptor| acceptor.value_at(record.object, record.sequence))
    .is_some_and(|held| held != record.value.as_slice())
}

/// Holds `record`, a green's merge record, in this holder's register before its inputs and its recomputation when it
/// replaces a different value held at its position (A-373): admitted by `fleet::check_held_record`, it comes under a
/// higher epoch than the held value, which therefore never committed. Until it is held the door refuses its inputs,
/// since the held value names others at that position (`content_retention::admits`), and the replica built over the
/// replaced value is rewound as it is held (`fleet::hold_checked_record`), so [`accept_merge_record`] then serves the
/// record as the replica's next version. The acknowledgement this acceptance makes is not sent: the record is
/// acknowledged once recomputed. Anything else (no value held there, the same one, a refused record, a green refused
/// here or a test's withheld acknowledgement) is left to [`accept_merge_record`].
pub(crate) fn hold_replacing_record(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  record: &Record,
) {
  if !replaces_held_value(state, record)
    || state.merge.refused.contains(&record.object)
    || state.merge.fault.refuse_records
    || MergeRecordValue::from_record_bytes(&record.value)
      .is_none_or(|value| value.version != record.sequence)
    || crate::fleet::check_held_record(state, local, peer_host, record).is_err()
  {
    return;
  }
  // The reply goes unsent: the version is answered by its recomputation, or not at all.
  let _ = crate::fleet::accept_held_record(state, local, peer_host, record);
}

/// Serves one merge record on a holder (§4.16 "Apply on holders"): the inputs must be held, the
/// replica must be at the version before, the recomputation must reproduce the record's identity;
/// then the record is accepted into the object's acceptor like a head record and the bound
/// acknowledgement returned. A holder with the green's records and no replica (a warm restart) rebuilds the replica
/// from them first (A-313); a higher epoch's different value at a version this holder holds is held first, which
/// rewinds the replica below it ([`hold_replacing_record`], A-373). Every other case is an empty reply the owner counts
/// as no acknowledgement, with the reason counted here; a mismatch is fatal for the green on this holder.
pub(crate) fn accept_merge_record(
  state: &mut ShardState,
  local: HostId,
  peer_host: HostId,
  record: &Record,
) -> Vec<u8> {
  if let Err(error) = crate::fleet::check_held_record(state, local, peer_host, record) {
    return slates_db::register::encode_refusal(&error);
  }
  let object = record.object;
  if state.merge.refused.contains(&object) {
    return Vec::new();
  }
  if state.merge.fault.refuse_records {
    // Test support: the acknowledgement is withheld, so the owner's version cannot commit here.
    state.count(RECORD_WITHHELD, 1);
    return Vec::new();
  }
  let value = match MergeRecordValue::decode_record(&record.value) {
    Ok(value) => value,
    Err(refused) => {
      state.count(refused.counter().unwrap_or(INPUTS_UNDECODABLE), 1);
      return Vec::new();
    }
  };
  if record.sequence != value.version {
    state.count(OUT_OF_ORDER, 1);
    return Vec::new();
  }
  // A different value at a position this holder holds one is held first, which rewinds a replica built over the value
  // it replaces; the record is then served as the replica's next version (A-373).
  hold_replacing_record(state, local, peer_host, record);
  match record_standing(state, object, value.version) {
    None => return Vec::new(),
    // A version this replica already applied: re-acknowledge from the acceptor (idempotent).
    Some(RecordStanding::Applied) => {
      return crate::fleet::accept_held_record(state, local, peer_host, record);
    }
    Some(RecordStanding::Next) => {}
  }
  // Version 0 of a green over a base: its replica is seeded from the origin a share at a time (A-286).
  if value.version == 0
    && let Some(manifest) = value.inputs
  {
    return crate::merge_origin::seed_version_zero(
      state, local, peer_host, record, &value, manifest,
    );
  }
  let inputs = match value.inputs {
    None => None,
    Some(manifest) => match held_inputs(state, object, manifest) {
      Ok(held) => Some(held),
      Err(reason) => {
        state.count(reason, 1);
        return Vec::new();
      }
    },
  };
  let recomputed = recompute(
    state,
    object,
    &value,
    inputs.as_ref().map(|held| held.bytes.as_slice()),
  );
  if let Some(held) = inputs {
    state.store.budget.release(held.charge);
  }
  match recomputed {
    Ok(()) => crate::fleet::accept_held_record(state, local, peer_host, record),
    Err(reason) => {
      state.count(reason, 1);
      if reason == RECOMPUTE_MISMATCH {
        state.merge.refused.insert(object);
        state.merge.replicas.remove(&object);
        slates_machine::log_line!(
          "slates-server: merge record for green {} version {} from {:?}: the recomputed identity does not match the record; the green is refused on this holder",
          hex(&object.0),
          value.version,
          peer_host
        );
      }
      Vec::new()
    }
  }
}

/// A version's inputs gathered from this holder's hold, and the charge they hold against the shard's budget until the
/// recomputation that reads them is done (A-383).
struct HeldInputs {
  /// The inputs' bytes.
  bytes: Vec<u8>,
  /// What recomputing from them holds, charged to the shard's budget: the caller releases it once it recomputed.
  charge: slates_mem::budget::Reservation,
}

/// The inputs bytes a manifest names, from this holder's content hold (A-383): the one file gathered extent by extent
/// (`crate::merge_origin::PlannedInputs`), as the replica's rebuild and seed gather it, with a test's corruption fault
/// applied once if set (the last byte of the post-state, so the increment still decodes but recomputes to different
/// bytes). Before a byte is reserved, what the recomputation holds ([`RECOMPUTE_COPIES`] of the declared size) is charged
/// to the shard's budget, so a manifest naming one chunk at many extents makes a holder hold no more than its shard
/// admits: past that the record waits, refused [`INPUTS_OVER_BUDGET`]. Until A-383 the file was restored from the
/// archive within its distinct chunks' bytes and one chunk more, on the rule that an inputs archive names each chunk
/// once; but inputs are cut at fixed offsets, so inputs repeating a chunk's bytes at the cut name a chunk more than once,
/// and every holder refused them at every shipment, for good.
fn held_inputs(
  state: &mut ShardState,
  object: ObjectId,
  manifest: [u8; 32],
) -> Result<HeldInputs, &'static str> {
  let planned = crate::merge_origin::PlannedInputs::of(state, object, manifest)?;
  let charge = state
    .store
    .grow(planned.size().saturating_mul(RECOMPUTE_COPIES))
    .map_err(|_| INPUTS_OVER_BUDGET)?;
  match planned.gather(state, object) {
    Ok(bytes) => Ok(HeldInputs { bytes, charge }),
    Err(reason) => {
      state.store.budget.release(charge);
      Err(reason)
    }
  }
}

/// A test's corruption fault, applied once to the next inputs a holder recomputes from, if it is set (never from the
/// wire): the byte before the inputs' last eight is flipped — an increment's post-state's last byte, since its encoding
/// ends with the evidence count (a `u64`), so the increment still decodes but recomputes to different bytes.
pub(crate) fn corrupt_if_faulted(state: &mut ShardState, bytes: &mut [u8]) {
  if !state.merge.fault.corrupt_next_inputs {
    return;
  }
  state.merge.fault.corrupt_next_inputs = false;
  let evidence_count_bytes = size_of::<u64>();
  let at = bytes
    .len()
    .checked_sub(evidence_count_bytes.saturating_add(1));
  if let Some(byte) = at.and_then(|at| bytes.get_mut(at)) {
    *byte ^= CORRUPTION_MASK;
  }
}

/// What a holder answers about a merge record's inputs when it did not accept the record (§4.16 "Commit", A-286): a
/// content acknowledgement bound to the inputs, from this holder, when it holds them whole (the record waits on
/// something else: its replica seeding, the version before it); `None` when the record names no inputs or this holder
/// does not hold them whole, when the content plane's answer to the shipment's offer, asked again after its chunks,
/// says what it lacks instead (`crate::fleet::serve_merge_shipment`). The owner reads it to ship the record alone, or
/// the chunks still lacked.
pub(crate) fn inputs_held(state: &ShardState, local: HostId, record: &Record) -> Option<Vec<u8>> {
  let manifest = MergeRecordValue::from_record_bytes(&record.value)?.inputs?;
  state
    .held_content
    .holds_manifest(record.object, &manifest)
    .then(|| {
      slates_cluster::content::ContentMessage::Ack(slates_cluster::content::ContentAck {
        holder: local,
        object: record.object,
        sequence: record.sequence,
        manifest,
      })
      .encode()
    })
}

/// Recomputes `value.version` into this holder's replica of `object` from `inputs` and compares the
/// head identity with the record's. Version 0 here is a scratch green's, an empty replica (a green over a base seeds
/// its version 0 from the origin a share at a time, `crate::merge_origin::seed_version_zero`, A-286); a later
/// version submits the increment, which must accept as exactly that version. `Err` names the
/// counter for the reason.
fn recompute(
  state: &mut ShardState,
  object: ObjectId,
  value: &MergeRecordValue,
  inputs: Option<&[u8]>,
) -> Result<(), &'static str> {
  let mut replica = if value.version == 0 {
    Green::new()
  } else {
    let Some(bytes) = inputs else {
      return Err(INPUTS_UNHELD);
    };
    let increment = Increment::decode(bytes).map_err(|_| INPUTS_UNDECODABLE)?;
    let mut replica = state.merge.replicas.remove(&object).ok_or(OUT_OF_ORDER)?;
    match replica.submit(&increment) {
      Outcome::Accepted { version } if version == value.version => {}
      // The owner accepted it; a holder that computes anything else has diverged.
      _ => return Err(RECOMPUTE_MISMATCH),
    }
    replica
  };
  if replica.head_identity() != value.identity {
    return Err(RECOMPUTE_MISMATCH);
  }
  // Version 0 of a green this holder already replicates: accepted only if it is the same origin.
  if value.version == 0
    && let Some(existing) = state.merge.replicas.get_mut(&object)
    && existing.head_identity() != value.identity
  {
    return Err(OUT_OF_ORDER);
  }
  if value.version == 0 && state.merge.replicas.contains_key(&object) {
    replica = state.merge.replicas.remove(&object).ok_or(OUT_OF_ORDER)?;
  }
  // The version's provenance (A-321), as the record carries it: the replica a takeover adopts (A-333) answers it as the
  // owner judged it.
  if let Some(signature) = value.signature {
    replica.record_signature(value.version, crate::signed_work::adopted(signature));
  }
  state.merge.replicas.insert(object, replica);
  Ok(())
}

/// A holder's view of `object`'s replica, for a test.
pub(crate) fn holder_state(state: &ShardState, object: ObjectId) -> HolderMergeState {
  HolderMergeState {
    version: state.merge.replicas.get(&object).map(Green::head),
    refused: state.merge.refused.contains(&object),
  }
}

/// The highest version of `object` whose record this owner has placed at quorum, if any.
pub(crate) fn placed_version(state: &ShardState, object: ObjectId) -> Option<u64> {
  state.merge.placed.get(&object).copied()
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// AC-6.8 / A-26, A-294: seed a green from a frozen namespace of FIFO and socket names and device nodes without
  /// reading a stream or opening a device; expect every kind, owner and device number in the green, each alias a link.
  #[test]
  fn ipc_snapshot_seeds_a_replayable_green_origin() {
    let encoded = {
      let page = 4096;
      let mut arena = slates_mem::arena::ChunkArena::new(page);
      arena
        .add_region(slates_mem::region::Region::map(page * 256, page, false).expect("RAM region"))
        .expect("arena");
      let mut store = Store::new(
        &slates_vfs::volume::StoreConfig {
          page,
          cache_line: 64,
          max_dirs: 64,
          max_inodes: 64,
          max_chunks: 64,
          max_dir_blocks: 64,
          dir_cutover: 4,
        },
        arena,
        0,
      );
      let mut volume = Volume::create(
        &mut store,
        slates_vfs::volume::VolumeConfig {
          prefix: 7,
          names: slates_vfs::names::NameEquivalence::Exact,
          quota: slates_vfs::quota::Quota::Bounded { limit: 1 << 20 },
          journal_bytes: 1 << 16,
          clock: Box::new(slates_vfs::clock::StepClock::new(0, 1)),
        },
      )
      .expect("create source");
      let root = volume.root_inode(&store).expect("root");
      use slates_vfs::inode::{DeviceNumber, SpecialNode};
      for (name, node) in [
        ("pipe", SpecialNode::Fifo),
        ("socket", SpecialNode::Socket),
        ("disk", SpecialNode::BlockDevice(DeviceNumber::new(8, 1))),
        ("whiteout", SpecialNode::CharDevice(DeviceNumber::new(0, 0))),
        (
          "widest",
          SpecialNode::CharDevice(DeviceNumber::new(u32::MAX, u32::MAX)),
        ),
      ] {
        let inode = volume
          .mknod_no(&mut store, root, name, 0o640, node)
          .expect("create a name with no content");
        volume.chown(&mut store, inode, 123, 456).expect("owner");
        volume
          .link_no(&mut store, root, &format!("{name}-link"), inode)
          .expect("alias");
      }
      let snapshot = volume.snapshot(&mut store).expect("freeze");
      walk_origin(&volume, &store, snapshot)
        .expect("IPC metadata is a complete immutable origin")
        .encode()
    };
    let origin = Origin::decode(&encoded).expect("decode origin");
    assert!(
      origin.files.is_empty(),
      "no stream is represented as file bytes"
    );
    assert_eq!(origin.specials.len(), 5);
    let green = Green::with_origin(origin.clone());
    use slates_merge::special::SpecialKind;
    for (path, kind) in [
      ("pipe", SpecialKind::Fifo),
      ("socket", SpecialKind::Socket),
      ("disk", SpecialKind::BlockDevice { major: 8, minor: 1 }),
      ("whiteout", SpecialKind::CharDevice { major: 0, minor: 0 }),
      (
        "widest",
        SpecialKind::CharDevice {
          major: u32::MAX,
          minor: u32::MAX,
        },
      ),
    ] {
      let node = green.special(path).expect("metadata survived capture");
      assert_eq!((node.kind, node.uid, node.gid), (kind, 123, 456));
      assert_eq!(green.special(&format!("{path}-link")), Some(node));
    }
    assert_eq!(origin.hardlinks.len(), 5);
    assert_eq!(origin.encode(), encoded);
    assert_eq!(
      Green::with_origin(origin.clone()).head_identity(),
      Green::with_origin(Origin::decode(&encoded).expect("holder origin")).head_identity()
    );
  }

  /// AC-8.19 / T-8.17, AUD-13, §4.16 "Commit" in one exchange a version: B holds a record's inputs and C does not.
  /// Expect the record to carry its inputs to a set of targets that includes C, and to go bare once C holds them.
  #[test]
  fn a_record_carries_its_inputs_while_a_target_lacks_them() {
    let (local, second, third) = (HostId(1), HostId(2), HostId(3));
    let candidates = vec![local, second, third];
    let mut pending = PendingMergeRecord {
      value: MergeRecordValue {
        version: 1,
        increment: [1; 32],
        base: 0,
        inputs: Some([2; 32]),
        identity: [3; 32],
        evidence: Vec::new(),
        name: String::new(),
        require_evidence: false,
        owner: Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        completion: None,
        signature: None,
      },
      inputs: Placement {
        candidates: candidates.clone(),
        acked: vec![local, second],
        mirror_acked: None,
        joint: Vec::new(),
      },
      record: Placement {
        candidates,
        acked: Vec::new(),
        mirror_acked: None,
        joint: Vec::new(),
      },
      first_period: None,
      work: None,
      lacking: BTreeMap::new(),
    };
    assert!(carries_inputs(&pending, &[second, third]));
    assert!(!carries_inputs(&pending, &[second]));
    pending.inputs.acked.push(third);
    assert!(!carries_inputs(&pending, &[second, third]));
    pending.value.inputs = None;
    pending.inputs.acked.clear();
    assert!(
      !carries_inputs(&pending, &[second, third]),
      "a version without inputs carries none"
    );
  }

  /// §4.16 "Commit" in one exchange a version: a shipment's frames come back as they were framed, an empty set too.
  #[test]
  fn a_merge_shipment_decodes_to_the_frames_it_was_made_from() {
    let frames = vec![b"record".to_vec(), Vec::new(), vec![7; 300]];
    let bytes = encode_shipment(&frames);
    let decoded = decode_shipment(&bytes).expect("a shipment decodes");
    assert_eq!(
      decoded,
      frames.iter().map(Vec::as_slice).collect::<Vec<_>>()
    );
    assert_eq!(decode_shipment(&encode_shipment(&[])), Some(Vec::new()));
  }

  /// CLAUDE.md §4 hostile input: a shipment whose count or a length runs past its bytes, a truncated header, bytes
  /// left over, or a count of `u32::MAX`, is refused, never read past its end or allocated for.
  #[test]
  fn a_hostile_merge_shipment_is_refused() {
    let good = encode_shipment(&[b"record".to_vec(), b"offer".to_vec()]);
    for cut in 0..good.len() {
      assert_eq!(decode_shipment(&good[..cut]), None, "truncated at {cut}");
    }
    let mut extra = good.clone();
    extra.push(0);
    assert_eq!(decode_shipment(&extra), None, "bytes left over");
    assert_eq!(
      decode_shipment(&u32::MAX.to_le_bytes()),
      None,
      "a count of u32::MAX"
    );
    let mut long = encode_shipment(&[b"record".to_vec()]);
    long[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      decode_shipment(&long),
      None,
      "a frame length past the bytes"
    );
  }

  /// AC-6.3, §4.16 ordered holder replication; AUD-13: A and B place version 0 while C is
  /// unavailable. Expect version 1 to be shipped to B while C's version 0 remains owed. Once C
  /// acknowledges 0, its next shipment must be 1, preserving its contiguous chain.
  #[test]
  fn a_silent_candidate_does_not_block_the_next_versions_quorum() {
    let (before, after, placed) = crate::daemon::audit_on_shard(|state| {
      let local = state.fleet.host();
      let second = HostId(local.0 ^ 1);
      let third = HostId(local.0 ^ 2);
      let quorum = Quorum { f: 1 };
      state.fleet = slates_cluster::fleet::FleetNode::new(local, quorum, &[second, third]);
      let object = ObjectId([21; 16]);
      let candidates = vec![local, second, third];
      for version in 0..=1 {
        state.merge.pending.entry(object).or_default().insert(
          version,
          PendingMergeRecord {
            value: MergeRecordValue {
              version,
              increment: [1; 32],
              base: 0,
              inputs: Some([2; 32]),
              identity: [3; 32],
              evidence: Vec::new(),
              name: String::new(),
              require_evidence: false,
              owner: Principal::Uid {
                host: slates_db::HostId(0),
                uid: 0,
              },
              completion: None,
              signature: None,
            },
            inputs: Placement {
              candidates: candidates.clone(),
              acked: candidates.clone(),
              mirror_acked: None,
              joint: Vec::new(),
            },
            record: Placement {
              candidates: candidates.clone(),
              acked: Vec::new(),
              mirror_acked: None,
              joint: Vec::new(),
            },
            first_period: None,
            work: None,
            lacking: BTreeMap::new(),
          },
        );
      }
      let placement = |acked| Placement {
        candidates: candidates.clone(),
        acked,
        mirror_acked: None,
        joint: Vec::new(),
      };
      record_merge_acks(
        state,
        local,
        object,
        0,
        placement(vec![local, second]),
        quorum,
      );
      let shipments = |state: &mut ShardState| {
        next_merge_work(state, local, 0)
          .into_iter()
          .filter_map(|item| match item {
            MergeWork::Ship {
              version, targets, ..
            } => Some((version, targets)),
            MergeWork::ShipWorkCatalog(_) => None,
          })
          .collect::<Vec<_>>()
      };
      let before = shipments(state);
      record_merge_acks(
        state,
        local,
        object,
        1,
        placement(vec![local, second]),
        quorum,
      );
      let placed = placed_version(state, object);
      record_merge_acks(state, local, object, 0, placement(vec![third]), quorum);
      let after = shipments(state);
      (before, after, placed)
    });
    assert!(
      before.iter().any(|(version, _)| *version == 1),
      "the available holder never received version 1: {before:?}"
    );
    assert!(
      before.iter().any(|(version, _)| *version == 0),
      "the missing holder still needs version 0"
    );
    assert_eq!(placed, Some(1));
    assert_eq!(
      after
        .iter()
        .map(|(version, _)| *version)
        .collect::<Vec<_>>(),
      vec![1]
    );
  }

  fn value() -> MergeRecordValue {
    MergeRecordValue {
      version: 3,
      increment: [5u8; 32],
      base: 2,
      inputs: Some([9u8; 32]),
      identity: [7u8; 32],
      evidence: vec![[1u8; 32]],
      name: "green".to_owned(),
      require_evidence: true,
      owner: Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1000,
      },
      completion: Some(SubmitCompletion {
        origin: 11,
        client: 12,
        sequence: 13,
        work: Some(DbVolumeId { bytes: [14u8; 16] }),
      }),
      signature: Some(slates_db::catalog::SignatureVerdict {
        kind: 1,
        detail: 0,
        fingerprint: [15u8; 32],
      }),
    }
  }

  /// A merge record's value encodes deterministically and round-trips exactly.
  #[test]
  fn a_merge_record_value_round_trips_and_is_deterministic() {
    let bytes = value().to_record_bytes();
    assert_eq!(bytes, value().to_record_bytes());
    assert_eq!(MergeRecordValue::from_record_bytes(&bytes), Some(value()));
    let origin_only = MergeRecordValue {
      version: 0,
      increment: [0u8; 32],
      base: 0,
      inputs: None,
      identity: [7u8; 32],
      evidence: Vec::new(),
      name: String::new(),
      require_evidence: false,
      owner: Principal::Uid {
        host: slates_db::HostId(0),
        uid: 0,
      },
      completion: None,
      signature: None,
    };
    assert_eq!(
      MergeRecordValue::from_record_bytes(&origin_only.to_record_bytes()),
      Some(origin_only)
    );
  }

  /// Hostile input: every truncation and any trailing byte is refused, never a panic.
  #[test]
  fn a_truncated_or_padded_merge_record_value_is_refused() {
    let bytes = value().to_record_bytes();
    for cut in 0..bytes.len() {
      assert!(
        MergeRecordValue::from_record_bytes(&bytes[..cut]).is_none(),
        "cut to {cut} bytes"
      );
    }
    let mut padded = bytes;
    padded.push(0);
    assert!(MergeRecordValue::from_record_bytes(&padded).is_none());
  }

  /// A-255, hostile input: a merge record of another format version is refused typed with the version it carries, and
  /// an unversioned record an earlier layout wrote (the value right after the class byte) and a foreign magic are refused
  /// malformed. None is decoded by a second path.
  #[test]
  fn a_merge_record_of_another_format_is_refused_typed() {
    let bytes = value().to_record_bytes();
    let mut other_version = bytes.clone();
    if let Some(version) = other_version.get_mut(5..9) {
      version.copy_from_slice(&RECORD_VERSION.saturating_add(1).to_le_bytes());
    }
    assert_eq!(
      MergeRecordValue::decode_record(&other_version),
      Err(RecordRefused::Version {
        found: RECORD_VERSION.saturating_add(1),
        expected: RECORD_VERSION,
      })
    );
    // A-288: a record of format 1, whose completion carried no submitting work, is refused typed, never read without it.
    let mut before_the_work = value().to_record_bytes();
    if let Some(version) = before_the_work.get_mut(5..9) {
      version.copy_from_slice(&1u32.to_le_bytes());
    }
    assert_eq!(
      MergeRecordValue::decode_record(&before_the_work),
      Err(RecordRefused::Version {
        found: 1,
        expected: RECORD_VERSION,
      })
    );
    let unversioned = crate::catalog::tagged(crate::catalog::MERGE_CLASS, value().to_bytes());
    assert_eq!(
      MergeRecordValue::decode_record(&unversioned),
      Err(RecordRefused::Malformed)
    );
    let mut foreign_magic = bytes;
    if let Some(magic) = foreign_magic.get_mut(1) {
      *magic ^= 0xff;
    }
    assert_eq!(
      MergeRecordValue::decode_record(&foreign_magic),
      Err(RecordRefused::Malformed)
    );
    let head_class = crate::catalog::tagged(crate::catalog::MERGE_CLASS ^ 0xff, value().to_bytes());
    assert_eq!(
      MergeRecordValue::decode_record(&head_class),
      Err(RecordRefused::OtherClass)
    );
  }

  /// The inputs archive's identity is a function of the bytes alone: the same bytes name one
  /// manifest whatever the header's informational fields, and different bytes another.
  #[test]
  fn the_inputs_archive_identity_follows_the_bytes() {
    let one = MergeShardState::inputs_archive(b"inputs", 0, 4096);
    let same = MergeShardState::inputs_archive(b"inputs", 99, 8192);
    let other = MergeShardState::inputs_archive(b"other!", 0, 4096);
    assert_eq!(one.manifest_identity(), same.manifest_identity());
    assert_ne!(one.manifest_identity(), other.manifest_identity());
    assert_eq!(
      MergeShardState::inputs_identity(b"inputs", 8192),
      one.manifest_identity()
    );
    assert_eq!(Archive::content(&one.chunks[0]).unwrap(), b"inputs");
  }

  /// §4.16 "Apply on holders" ("the holder finds the version's inputs in its content hold"): the
  /// identity a merge record names its inputs by is the identity the content hold keys the shipped
  /// archive by — put, hold and record agree — so a holder that acknowledged the put finds the inputs
  /// when the record arrives, and recomputes from exactly those bytes. Non-vacuous: the tree's own
  /// identity, which the record named until 2026-09-16, is not a key the hold has, and a record naming
  /// it waited `INPUTS_UNHELD` every period for good
  /// (`docs/bugs/2026-09-16-merge-record-names-inputs-by-the-tree-only-identity.md`).
  #[test]
  fn the_record_names_the_inputs_by_the_identity_the_hold_keys_by() {
    let bytes = b"the increment's chain entry";
    let named = MergeShardState::inputs_identity(bytes, 4096);
    let object = ObjectId::new(slates_db::HostId(1), 1);
    let tree_only = MergeShardState::inputs_archive(bytes, 0, 4096)
      .manifest
      .identity();
    let (held, found, by_tree) = crate::daemon::audit_on_shard(move |state| {
      let mut hold = slates_cluster::content::ContentHold::new();
      let held = hold
        .hold(
          &mut crate::content_holder::hold_space(&mut state.store),
          object,
          slates_cluster::content::Placed::default(),
          MergeShardState::inputs_archive(bytes, 0, 4096),
        )
        .expect("the inputs archive is whole and verifies");
      let arena = state.store.content.arena();
      let found = hold
        .archive_of(arena, object, &named)
        .expect("the holder finds the inputs the record names");
      let by_tree = hold.archive_of(arena, object, &tree_only).is_some();
      (held, found, by_tree)
    });
    assert_eq!(named, held, "the record names what the hold keys by");
    assert_eq!(Archive::content(&found.chunks[0]).unwrap(), bytes);
    assert_ne!(
      tree_only, held,
      "the tree's own identity is not the hold's key (format minor 2)"
    );
    assert!(
      !by_tree,
      "a record naming the tree's identity finds nothing"
    );
  }

  /// Holds `archive` for `object` in the shard's own content hold, as a holder that acknowledged its put holds it: the
  /// manifest identity held.
  fn hold_for(state: &mut ShardState, object: ObjectId, archive: Archive) -> [u8; 32] {
    state
      .held_content
      .hold(
        &mut crate::content_holder::hold_space(&mut state.store),
        object,
        slates_cluster::content::Placed::default(),
        archive,
      )
      .expect("the inputs archive is whole and verifies")
  }

  /// A-383, §4.16 "Apply on holders": inputs are cut into chunks at fixed offsets, so inputs that repeat a chunk's bytes
  /// at the cut name one chunk at more than one extent, and a holder that restored them within the archive's distinct
  /// chunks and one chunk more refused them at every shipment, for good. Do: hold the inputs archive of bytes holding a
  /// run of one byte three chunks long, and read the inputs as a holder recomputing a version does. Expect the very
  /// bytes; the archive naming a chunk at more than one extent (the case meant); twice their size charged to the shard's
  /// budget while they are held, and all of it given back once the charge is released.
  #[test]
  fn a_holder_reads_inputs_that_repeat_a_chunk_and_charges_them_while_it_holds_them() {
    let chunk = crate::merge_origin::inputs_chunk_bytes();
    let mut bytes = b"before the run".to_vec();
    bytes.extend(std::iter::repeat_n(b'r', chunk * 3));
    bytes.extend_from_slice(b"after the run");
    let archive = MergeShardState::inputs_archive(&bytes, 0, 4096);
    let named_twice = archive.chunks.len() > archive.referenced_chunks().len();
    let object = ObjectId::new(slates_db::HostId(1), 1);
    let (read, charged, left) = crate::daemon::audit_on_shard(move |state| {
      let manifest = hold_for(state, object, archive);
      let before = state.store.budget.committed();
      let held = held_inputs(state, object, manifest);
      let charged = state.store.budget.committed() - before;
      let read = held.map(|held| {
        state.store.budget.release(held.charge);
        held.bytes
      });
      (read, charged, state.store.budget.committed() - before)
    });
    assert!(named_twice, "the inputs archive names one chunk twice");
    assert_eq!(read, Ok(bytes.clone()), "the holder reads the very inputs");
    assert_eq!(
      charged,
      u64::try_from(bytes.len()).unwrap() * RECOMPUTE_COPIES,
      "what the recomputation holds is charged while it holds it"
    );
    assert_eq!(left, 0, "the charge is given back whole");
  }

  /// Shape: the reduced reserve the over-budget test's shard runs with, so a manifest naming one chunk past what the
  /// shard admits names a few dozen extents, not the machine's measured production reserve.
  const OVER_BUDGET_RESERVE: u64 = 32 << 20;

  /// A-383, CLAUDE.md §4 hostile input (AUD-29-13): a manifest naming one chunk at more extents than the shard can hold
  /// the inputs of is refused before a byte of them is reserved, as the cap of distinct chunks refused it before. Do: on
  /// a shard with a reduced reserve, hold an inputs archive of one chunk named at enough extents that twice their bytes
  /// pass what the shard could admit, and read the inputs as a holder recomputing a version does. Expect the read
  /// refused `merge.inputs_over_budget`, with the shard's budget as it was.
  #[test]
  fn inputs_naming_a_chunk_past_what_the_shard_admits_are_refused_before_any_is_gathered() {
    let object = ObjectId::new(slates_db::HostId(1), 1);
    let (refused, before, after) = crate::daemon::audit_on_shard_configured(
      move |state| {
        let chunk = crate::merge_origin::inputs_chunk_bytes();
        let mut archive = MergeShardState::inputs_archive(&vec![b'r'; chunk], 0, 4096);
        let len = u64::try_from(chunk).unwrap();
        let named = state.store.admittable() / (len * RECOMPUTE_COPIES) + 1;
        let slates_archive::Node::Directory(entries) = &mut archive.manifest else {
          panic!("an inputs archive is one directory");
        };
        let entry = entries
          .first_mut()
          .expect("an inputs archive has its one file");
        let slates_archive::Node::File(extents) = &mut entry.node else {
          panic!("the inputs entry is a file");
        };
        let one = *extents.first().expect("one chunk, one extent");
        *extents = (0..named)
          .map(|at| slates_archive::Extent {
            offset: at * len,
            ..one
          })
          .collect();
        entry.meta.size = named * len;
        let manifest = hold_for(state, object, archive);
        let before = state.store.budget.committed();
        let refused = held_inputs(state, object, manifest).err();
        (refused, before, state.store.budget.committed())
      },
      |config| config.reserve_per_shard = OVER_BUDGET_RESERVE,
    );
    assert_eq!(
      refused,
      Some(INPUTS_OVER_BUDGET),
      "the inputs are refused at the shard's budget"
    );
    assert_eq!(before, after, "nothing is left charged");
  }

  /// A version's inputs archive over two whole inputs chunks and half of a third (A-286).
  fn three_chunk_archive() -> Archive {
    let chunk = crate::merge_origin::inputs_chunk_bytes();
    let bytes: Vec<u8> = (0..chunk * 2 + chunk / 2)
      .map(|at| u8::try_from(at % 253).unwrap())
      .collect();
    MergeShardState::inputs_archive(&bytes, 0, 4096)
  }

  /// The chunks a shipment's chunk requests carry, by identity, in order.
  fn shipped_chunks(inputs: &ShipInputs) -> Vec<[u8; 32]> {
    inputs
      .chunks
      .iter()
      .map(
        |frame| match slates_cluster::content::ContentMessage::decode(frame) {
          Ok(slates_cluster::content::ContentMessage::Chunk { chunk, .. }) => chunk.identity,
          other => panic!("a chunk request: {other:?}"),
        },
      )
      .collect()
  }

  /// §4.16 "Commit" (A-286): a shipment carries its inputs' offer and at most its bound of the chunks its targets lack,
  /// in reference order, never none. Do: cut shipments of a three-chunk inputs archive for targets lacking every chunk
  /// under a bound of one chunk, of two, of zero bytes, and past the whole; and for targets lacking only the last chunk.
  /// Expect: the first chunk, the first two, the first alone (at least one goes), all three and whole; the last alone
  /// and whole; each shipment with the offer of the version's inputs.
  #[test]
  fn a_shipment_carries_at_most_its_bound_of_what_its_targets_lack() {
    let archive = three_chunk_archive();
    let object = ObjectId([41; 16]);
    let ids = archive.referenced_chunks();
    let chunk = u64::try_from(crate::merge_origin::inputs_chunk_bytes()).unwrap();
    let carried = |lacking: Option<&[[u8; 32]]>, bound: u64| {
      let cut = ShipInputs::cut(&archive, (object, 4), lacking, bound);
      assert!(
        matches!(
          slates_cluster::content::ContentMessage::decode(&cut.offer),
          Ok(slates_cluster::content::ContentMessage::Offer { sequence: 4, .. })
        ),
        "the shipment offers the version's inputs"
      );
      (shipped_chunks(&cut), cut.whole)
    };
    assert_eq!(ids.len(), 3);
    assert_eq!(carried(None, chunk), (ids[..1].to_vec(), false));
    assert_eq!(carried(None, 2 * chunk), (ids[..2].to_vec(), false));
    assert_eq!(
      carried(None, 0),
      (ids[..1].to_vec(), false),
      "at least one chunk goes"
    );
    assert_eq!(carried(None, u64::MAX), (ids.clone(), true));
    assert_eq!(carried(Some(&ids[2..]), chunk), (ids[2..].to_vec(), true));
  }

  /// §4.16 "Commit" (A-286): the owner reads what each target answered about a version's inputs, and ships each what it
  /// lacks next. Do: pend a version whose inputs two targets lack; record B's missing set naming one chunk, C's content
  /// acknowledgement, and answers bound to another version, to another manifest and to another holder; group the next
  /// shipment's targets; then record C answering nothing to a shipment of the record alone, and group again. Expect: B's
  /// missing set kept and C counted as holding the inputs, the progress counted; the foreign answers change nothing; the
  /// next shipment carries B the one chunk and C the record alone; once C answers nothing, C is sent the inputs from
  /// their first chunk again.
  #[test]
  fn a_targets_answer_names_what_its_next_shipment_carries() {
    let (first, second, progressed) = crate::daemon::audit_on_shard(|state| {
      use slates_cluster::content::{ContentAck, ContentMessage};
      let local = state.fleet.host();
      let (b, c) = (HostId(local.0 ^ 1), HostId(local.0 ^ 2));
      state.fleet = slates_cluster::fleet::FleetNode::new(local, Quorum { f: 1 }, &[b, c]);
      let object = ObjectId([42; 16]);
      let manifest = [8; 32];
      let lacked = [5; 32];
      let candidates = vec![local, b, c];
      state.merge.pending.entry(object).or_default().insert(
        1,
        PendingMergeRecord {
          value: MergeRecordValue {
            inputs: Some(manifest),
            version: 1,
            ..value()
          },
          inputs: Placement {
            candidates: candidates.clone(),
            acked: vec![local],
            mirror_acked: None,
            joint: Vec::new(),
          },
          record: Placement {
            candidates,
            acked: Vec::new(),
            mirror_acked: None,
            joint: Vec::new(),
          },
          first_period: None,
          work: None,
          lacking: BTreeMap::new(),
        },
      );
      let ack = |holder, sequence, manifest| {
        ContentMessage::Ack(ContentAck {
          holder,
          object,
          sequence,
          manifest,
        })
        .encode()
      };
      let missing = ContentMessage::Missing {
        object,
        sequence: 1,
        manifest,
        missing: vec![lacked],
      }
      .encode();
      record_inputs_answers(
        state,
        (object, 1, Some(manifest)),
        vec![
          (b, missing),
          (c, ack(c, 1, manifest)),
          (b, ack(b, 2, manifest)),
          (b, ack(b, 1, [9; 32])),
          (b, ack(c, 1, manifest)),
        ],
        true,
      );
      let groups = |state: &ShardState| {
        state
          .merge
          .pending
          .get(&object)
          .and_then(|pending_green| pending_green.records.get(&1))
          .map(|pending| ship_groups(pending, &[b, c]))
          .unwrap()
      };
      let first = groups(state);
      record_inputs_answers(
        state,
        (object, 1, Some(manifest)),
        vec![(c, Vec::new())],
        false,
      );
      let second = groups(state);
      let progressed = state.refusals.get(INPUTS_PROGRESSED).copied();
      (
        first.into_iter().collect::<Vec<_>>(),
        second.into_iter().collect::<Vec<_>>(),
        progressed,
      )
    });
    let lacked = [5; 32];
    assert_eq!(
      first.iter().map(|(_, hosts)| hosts.len()).sum::<usize>(),
      2,
      "both targets are grouped once: {first:?}"
    );
    assert!(
      first
        .iter()
        .any(|(carry, _)| *carry == Carry::Lacking(vec![lacked])),
      "B is sent the one chunk it lacks: {first:?}"
    );
    assert!(
      first.iter().any(|(carry, _)| *carry == Carry::Record),
      "C, holding the inputs, is sent the record alone: {first:?}"
    );
    assert!(
      second.iter().any(|(carry, _)| *carry == Carry::Inputs),
      "C, answering nothing to the record alone, is offered the inputs again: {second:?}"
    );
    assert_eq!(progressed, Some(1), "one shipment's answers made progress");
  }
}

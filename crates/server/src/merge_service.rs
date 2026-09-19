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
  AttachForm, AttachmentRecord, Consumer, Principal, Role, VolumeId as DbVolumeId, VolumeRecord,
  VolumeState,
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
  attachment_id, core_snapshot, find, forbidden, refused, rights_of, to_db_snapshot, to_db_volume,
  wire_id,
};

/// The merge plane's per-shard state (§4.16): one field on [`ShardState`], so the join with the
/// rest of the daemon is a single line there and a single line at init.
#[derive(Default)]
pub struct MergeShardState {
  /// The version each green attachment pins, by attachment id (§4.16 "Attachments and versions").
  pub attachments: BTreeMap<u64, PinnedAttachment>,
  /// The merge records this owner shard still owes its candidate holders, by green then version
  /// (§4.16 "Commit"): each waits for its inputs to place, then ships in order. Bounded by the
  /// chain (one entry per committed version not yet held everywhere; empty at `f = 0`).
  pub pending: BTreeMap<ObjectId, PendingGreen>,
  /// The highest version of each owned green whose merge record placed at `f + 1` (`await placed`
  /// answers from it; at `f = 0` every appended version).
  pub placed: BTreeMap<ObjectId, u64>,
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
  let work_bases = state
    .works
    .values()
    .filter(|work| work.green == green)
    .map(|work| work.base_version);
  let pins = state
    .merge
    .attachments
    .values()
    .filter(|pin| pin.green == green)
    .map(|pin| pin.version);
  work_bases.chain(pins).min().unwrap_or(head).min(head)
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
  let floor = reachable_floor(state, green);
  let budget = rejected_cache_budget(state);
  let Some(engine) = state.greens.get_mut(&green) else {
    return Ok(());
  };
  // Oldest-first, only as far as the retention budget needs (the same derived cap the rejected
  // cache is bounded by: one merge byte bound per green), never past a live reader's version.
  engine.fold_history_to_budget(floor, budget);
  let retained = engine.retained_bytes();
  let wanted =
    u64::try_from(retained.history.saturating_add(retained.rejected)).unwrap_or(u64::MAX);
  let charged = state.green_retention.get(&green).copied().unwrap_or(0);
  if wanted > charged {
    state
      .store
      .budget
      .charge_retention(wanted - charged)
      .map_err(|e| match e {
        slates_mem::MemError::BudgetExceeded { available, .. } => available,
        _ => 0,
      })?;
  } else if wanted < charged {
    state.store.budget.credit_retention(charged - wanted);
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
/// with no record, `Destroying` for one on its way out. (Unlike [`find`], no store slot is needed:
/// a merge volume has none.)
pub(crate) fn find_record(state: &ShardState, volume: VolumeId) -> Result<VolumeRecord, Refusal> {
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
/// on this shard (the base volume's owner). Refused `NotFound` for a volume or snapshot that does
/// not exist here, `Forbidden` without the read right, `ConsistentBaseUnavailable` when the
/// snapshot still depends on the host directory (an overlay not pinned whole before the freeze),
/// and the volume core's own refusal for a torn read.
pub(crate) fn seed_origin(
  state: &ShardState,
  principal: &Principal,
  base: GreenBase,
) -> Result<Origin, Refusal> {
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
  let slot = state.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
  let snapshot = core_snapshot(base.snapshot);
  match slot.volume.snapshot_is_complete(&state.store, snapshot) {
    Ok(true) => {}
    Ok(false) => return Err(Refusal::ConsistentBaseUnavailable),
    Err(e) => return Err(refusal_of_vfs(&e)),
  }
  walk_origin(&slot.volume, &state.store, snapshot).map_err(|e| refusal_of_vfs(&e))
}

/// Walks a complete snapshot into an [`Origin`]: every file with its bytes, every directory, every
/// mode, every symlink, and every further name of a multiply-linked inode as a hard link to the
/// first name met. Paths are the snapshot's absolute paths without the leading slash, as the ops
/// document names them; the root itself is not an entry.
fn walk_origin(
  volume: &Volume,
  store: &Store,
  snapshot: slates_vfs::ids::SnapshotId,
) -> Result<Origin, slates_vfs::error::VfsError> {
  let (_, root) = volume.snapshot_info(snapshot)?;
  let mut origin = Origin::default();
  let mut first_name: BTreeMap<InodeNo, String> = BTreeMap::new();
  let mut stack: Vec<(String, slates_mem::Handle<slates_vfs::dir::DirNode>)> =
    vec![(String::new(), root)];
  while let Some((prefix, dir)) = stack.pop() {
    for row in volume.readdir_in(store, dir)? {
      let path = if prefix.is_empty() {
        row.name.to_owned()
      } else {
        format!("{prefix}/{}", row.name)
      };
      let attrs = volume.stat_in(store, snapshot, row.inode)?;
      match row.kind {
        Kind::Dir => {
          origin.dirs.push(path.clone());
          origin.modes.push((path.clone(), attrs.mode));
          if let Child::Dir(child) = volume.lookup_in(store, dir, row.name)?.child {
            stack.push((path, child));
          }
        }
        Kind::Symlink => {
          let target = volume.readlink_in(store, snapshot, row.inode)?;
          origin.symlinks.push((path, target.into_string()));
        }
        Kind::File => {
          if let Some(first) = first_name.get(&row.inode) {
            origin.hardlinks.push((path, first.clone()));
            continue;
          }
          let mut bytes = vec![0u8; usize::try_from(attrs.size).unwrap_or(usize::MAX)];
          let read = volume.read_in(store, snapshot, row.inode, 0, &mut bytes)?;
          bytes.truncate(read);
          origin.modes.push((path.clone(), attrs.mode));
          if attrs.nlink > 1 {
            first_name.insert(row.inode, path.clone());
          }
          origin.files.push((path, bytes));
        }
      }
    }
  }
  origin.canonicalize();
  Ok(origin)
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
) -> ReplyBody {
  if intent == Intent::Write {
    return refused(Refusal::ReadOnlyVolume);
  }
  if !rights_of(record, principal).read {
    return forbidden("attach");
  }
  let Some(engine) = state.greens.get(&record.id) else {
    return refused(Refusal::NotFound);
  };
  let version = engine.head();
  let attachment = attachment_id(state.partition, state.next_attachment);
  state.next_attachment += 1;
  let now = state.clock.monotonic_ns();
  let op = Op::AttachmentAdded {
    record: AttachmentRecord {
      id: attachment,
      volume: record.id,
      consumer: Consumer::Sdk { client: client_id },
      snapshot: None,
      form: AttachForm::Root,
      principal: principal.clone(),
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
  let Some(record) = state.db.partition().attachment(attachment).cloned() else {
    return refused(Refusal::NotFound);
  };
  if &record.principal != principal {
    return forbidden("advance");
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
      ReadAt::Head => match state.works.get(&record.id).and_then(|w| w.content.get(key)) {
        Some(bytes) => ReplyBody::ReadBytes {
          bytes: bytes.clone(),
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
    Err(slates_vfs::error::VfsError::IsDirectory) => refused(Refusal::BadRequest {
      reason: "the path is a directory".to_owned(),
    }),
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
  // A retired work may have held the oldest reachable version of its green: fold and credit. A
  // destroyed green credits everything it charged as retention.
  if let Some(work) = state.works.remove(&id) {
    let _ = settle_green_retention(state, work.green);
  }
  if state.greens.remove(&id).is_some() {
    state.merge.attachments.retain(|_, pin| pin.green != id);
    release_green_retention(state, id);
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
        work.content.values().map(|bytes| bytes.len() as u64).sum(),
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
    report: slates_ipc::protocol::StatusReport {
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
    },
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
// arrival, hedged to every candidate at once: the inputs are small); only then does the record ship,
// and only to a candidate that already acknowledged the version before it, so every holder sees the
// chain in order. A record that waits on its inputs is counted (`merge.inputs_unplaced`), the
// non-vacuity counter for placed-before-reference. The pending state is bounded by the chain: the
// bytes are the chain's own durable entries (never a second copy), and an entry leaves once every
// candidate holds it. At `f = 0` the record is placed the moment it is appended (R8: the same
// code, the local hold is the quorum), so a laptop keeps nothing pending.
//
// Holder side. A merge record arrives on its own stream (`MERGE_RECORD_STREAM`), dispatched by
// kind. The holder finds the version's inputs in its content hold (else refuses
// `merge.inputs_unheld` — the record waits), replays them into its replica of the green (an
// engine of its own, at the version before), and compares the replica's head identity with the
// record's: a mismatch is refused, counted (`merge.recompute_mismatch`), printed loudly, and the
// green is never served from this holder again (fatal for the green here; the owner and the other
// holders carry on, Degraded — §4.16 failure matrix); a match accepts the record into the holder's
// acceptor exactly as a head record is. An out-of-order record (a version past the replica's next)
// is refused and counted; the owner re-ships it once its predecessor is acknowledged.

use std::collections::BTreeSet;

use slates_archive::Archive;
use slates_archive::manifest::{Entry, Extent, Node, NodeMeta};
use slates_cluster::content::put_content;
use slates_cluster::{ClusterError, CommitBudget, commit_record_on};
use slates_db::register::{Acceptor, HostId, ObjectId, Placement, Quorum, Record};
use slates_merge::engine::{Green, Increment, Outcome};
use slates_wire::Wire;

use crate::fleet::{Dispatch, LateReplies, return_sessions, take_sessions};
use crate::xshard::{call_within, run_on};

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

/// Format: the one entry of an inputs archive's manifest — a file holding the version's inputs.
const INPUTS_ENTRY_NAME: &str = "inputs";
/// Format: the mask a test's corruption fault XORs into one inputs byte — every bit flipped, so
/// the corrupted byte can never equal the original whatever its value.
const CORRUPTION_MASK: u8 = 0xff;

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
}

impl MergeRecordValue {
  /// The value's canonical bytes.
  pub fn to_record_bytes(&self) -> Vec<u8> {
    self.to_bytes()
  }

  /// Parses a record's value; `None` for bytes that are not exactly one value.
  pub fn from_record_bytes(bytes: &[u8]) -> Option<MergeRecordValue> {
    let mut input = bytes;
    let value = <MergeRecordValue as Wire>::decode(&mut input).ok()?;
    if input.is_empty() { Some(value) } else { None }
  }
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
  /// [`INPUTS_ENTRY_NAME`] over one raw chunk of the bytes (the chain's own entry). Deterministic:
  /// the manifest identity depends on the bytes, the entry's name and size, and the default root
  /// metadata alone — and it is [`Archive::manifest_identity`], the one identity the put, the hold and
  /// the record all name (the tree's own identity is not it since the root's metadata joined the
  /// manifest, format minor 2).
  fn inputs_archive(bytes: &[u8], created_unix: u64, page: u32) -> Archive {
    let chunk = Archive::raw_chunk(bytes.to_vec());
    let len = chunk.raw_len;
    let identity = chunk.identity;
    Archive {
      base_page_size: page,
      chunk_min: page,
      chunk_max: page,
      created_unix,
      volume_id: 0,
      snapshot_id: 0,
      name_policy_id: 0,
      unicode_version: 0,
      root_meta: NodeMeta::default(),
      manifest: Node::Directory(vec![Entry {
        name: INPUTS_ENTRY_NAME.to_owned(),
        meta: NodeMeta {
          size: len,
          ..NodeMeta::default()
        },
        node: Node::File(vec![Extent {
          offset: 0,
          len,
          chunk: identity,
          chunk_offset: 0,
        }]),
      }]),
      chunks: vec![chunk],
    }
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
    return state.db.partition().green_origin(green).map(<[u8]>::to_vec);
  }
  let index = usize::try_from(version.checked_sub(1)?).ok()?;
  state.db.partition().green_chain(green).get(index).cloned()
}

/// The page the inputs archive is chunked at (informational in the header; the identity is the
/// bytes').
fn archive_page(state: &ShardState) -> u32 {
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
) {
  let object = ObjectId(green.bytes);
  let config = state.fleet.configuration();
  let local = state.fleet.host();
  let quorum = config.quorum;
  let candidates = config.place(object).candidates;
  let identity = state
    .greens
    .get(&green)
    .map(Green::head_identity)
    .unwrap_or_default();
  let inputs_identity = inputs_of(state, green, version)
    .map(|bytes| MergeShardState::inputs_identity(&bytes, archive_page(state)));
  let local_hold = Placement {
    candidates: candidates.clone(),
    acked: vec![local],
    mirror_acked: None,
  };
  if local_hold.placed(quorum) {
    // The laptop degenerate (R8): the owner is the quorum, the append was the commit.
    let placed = state.merge.placed.entry(object).or_insert(version);
    *placed = (*placed).max(version);
    return;
  }
  state.merge.pending.entry(object).or_default().insert(
    version,
    PendingMergeRecord {
      value: MergeRecordValue {
        version,
        increment,
        base,
        inputs: inputs_identity,
        identity,
        evidence,
      },
      inputs: local_hold,
      record: Placement {
        candidates,
        acked: Vec::new(),
        mirror_acked: None,
      },
    },
  );
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

/// The holders ready for this record: the inputs are placed at quorum first, and each target
/// itself holds them before recomputing (§4.16). Missing input holders remain catch-up work even
/// after other holders placed the inputs; quorum placement never erases that debt.
fn record_targets(pending: &PendingMergeRecord, targets: &[HostId], quorum: Quorum) -> Vec<HostId> {
  if pending.value.inputs.is_none() {
    return targets.to_vec();
  }
  if !pending.inputs.placed(quorum) {
    return Vec::new();
  }
  targets
    .iter()
    .copied()
    .filter(|host| pending.inputs.acked.contains(host))
    .collect()
}

/// One position owed by at least one candidate this period: the inputs put while they are
/// unplaced, the record ship once they are.
pub(crate) enum MergeWork {
  /// Put the version's inputs to the candidates that do not hold them yet.
  PutInputs {
    /// The owner shard the pending record lives on.
    shard: u16,
    /// The green.
    object: ObjectId,
    /// The version.
    version: u64,
    /// The inputs archive.
    archive: Archive,
    /// Every candidate.
    candidates: Vec<HostId>,
    /// The candidates that already hold the inputs.
    acked: Vec<HostId>,
    /// The quorum.
    quorum: Quorum,
  },
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
    /// Every candidate.
    candidates: Vec<HostId>,
    /// The quorum.
    quorum: Quorum,
  },
}

/// The merge work this shard's greens owe this period (§4.16): each candidate's first missing
/// version, grouped by version so one dispatch can still reach a quorum. Each candidate appears in
/// at most one dispatch per green; input placement precedes that version's record. A lagging holder
/// cannot hold another holder at its own old position (AUD-13).
pub(crate) fn next_merge_work(state: &mut ShardState, local: HostId) -> Vec<MergeWork> {
  let config = state.fleet.configuration().clone();
  let page = archive_page(state);
  let mut work = Vec::new();
  let objects: Vec<ObjectId> = state.merge.pending.keys().copied().collect();
  for object in objects {
    let Some(pending_green) = state.merge.pending.get(&object) else {
      continue;
    };
    let selected = pending_green.next(local);
    for (version, targets) in selected {
      let Some(pending) = state
        .merge
        .pending
        .get(&object)
        .and_then(|pending_green| pending_green.records.get(&version))
        .cloned()
      else {
        continue;
      };
      let green = DbVolumeId { bytes: object.0 };
      let ready = record_targets(&pending, &targets, config.quorum);
      if ready.len() < targets.len() {
        let Some(bytes) = inputs_of(state, green, version) else {
          continue; // The chain no longer holds the entry (the green was destroyed): nothing to place.
        };
        *state.refusals.entry(INPUTS_UNPLACED).or_insert(0) += 1;
        work.push(MergeWork::PutInputs {
          shard: state.shard,
          object,
          version,
          archive: MergeShardState::inputs_archive(&bytes, 0, page),
          candidates: pending.inputs.candidates.clone(),
          acked: pending.inputs.acked.clone(),
          quorum: config.quorum,
        });
      }
      if ready.is_empty() {
        continue;
      }
      // Each target is waiting for this version's first missing position. Another holder can be
      // at a later position; a missing candidate therefore cannot stall an available commit quorum.
      // The head is written at the host's epoch (a taken-over green's promotion epoch is owed with
      // green takeover).
      work.push(MergeWork::Ship {
        shard: state.shard,
        object,
        version,
        record: Record {
          owner: local,
          object,
          sequence: version,
          epoch: config.host_epoch,
          generation: config.version,
          value: pending.value.to_record_bytes(),
        },
        targets: ready,
        candidates: pending.record.candidates.clone(),
        quorum: config.quorum,
      });
    }
  }
  work
}

/// One period of the merge record plane for the greens `shard` owns, run on the control shard by
/// [`crate::fleet::run_record_period`] after the seals and heads: each holder's first missing version this
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
  let work = call_within(
    origin,
    shard,
    move |s| next_merge_work(s, local),
    crate::daemon::HEARTBEAT_NS,
  )
  .await
  .unwrap_or_default();
  for item in work {
    let dispatch = match item {
      MergeWork::PutInputs {
        shard,
        object,
        version,
        archive,
        candidates,
        acked,
        quorum,
      } => {
        put_inputs(
          origin, shard, local, object, version, archive, candidates, acked, quorum, budget,
        )
        .await
      }
      MergeWork::Ship {
        shard,
        object,
        version,
        record,
        targets,
        candidates,
        quorum,
      } => {
        ship_record(
          origin,
          shard,
          local,
          owner_acceptor,
          object,
          version,
          record,
          targets,
          candidates,
          quorum,
          budget,
        )
        .await
      }
    };
    if let Some(dispatch) = dispatch {
      in_flight.push(dispatch);
    }
  }
}

/// Puts a version's inputs to every candidate that does not hold them (the inputs are small, so
/// every remaining candidate at once), folding the acknowledgements into the pending record on the
/// owner shard. A late acknowledgement is dropped; the next period's offer round finds the holder
/// holding the content and re-acknowledges it at no cost.
#[allow(clippy::too_many_arguments)]
async fn put_inputs(
  origin: u16,
  shard: u16,
  local: HostId,
  object: ObjectId,
  version: u64,
  archive: Archive,
  candidates: Vec<HostId>,
  acked: Vec<HostId>,
  quorum: Quorum,
  budget: CommitBudget,
) -> Option<Dispatch> {
  let holders =
    take_sessions(|host| host != local && candidates.contains(&host) && !acked.contains(&host));
  if holders.is_empty() {
    return None;
  }
  let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
  let placed = put_content(
    local,
    &archive,
    object,
    version,
    &candidates,
    quorum,
    holders,
    budget,
  )
  .await;
  let dispatch = Dispatch::new(
    taken,
    &placed.reusable,
    placed.stragglers,
    LateReplies::Discard,
  );
  return_sessions(placed.reusable);
  let placement = match placed.outcome {
    Ok(placement) => placement,
    Err(ClusterError::Uncertain { placement } | ClusterError::NotPlaced { placement }) => placement,
    Err(_) => return Some(dispatch),
  };
  let _ = run_on(origin, shard, move |s| {
    if let Some(pending) = s
      .merge
      .pending
      .get_mut(&object)
      .and_then(|pending_green| pending_green.records.get_mut(&version))
    {
      for host in placement.acked {
        if !pending.inputs.acked.contains(&host) {
          pending.inputs.acked.push(host);
        }
      }
    }
  });
  Some(dispatch)
}

/// Ships a version's record to its targets over [`MERGE_RECORD_STREAM`] through the owner's own
/// acceptor at `f + 1`, and records every acknowledgement on the owner shard: the version is
/// placed once the distinct acknowledgements reach the quorum, and its entry leaves the pending
/// map once every remote candidate holds it.
#[allow(clippy::too_many_arguments)]
async fn ship_record(
  origin: u16,
  shard: u16,
  local: HostId,
  owner_acceptor: &mut Acceptor,
  object: ObjectId,
  version: u64,
  record: Record,
  targets: Vec<HostId>,
  candidates: Vec<HostId>,
  quorum: Quorum,
  budget: CommitBudget,
) -> Option<Dispatch> {
  let holders = take_sessions(|host| targets.contains(&host));
  if holders.is_empty() {
    return None;
  }
  let taken: Vec<HostId> = holders.iter().map(|(host, _)| *host).collect();
  let committed = commit_record_on(
    MERGE_RECORD_STREAM,
    local,
    owner_acceptor,
    &candidates,
    &record,
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
    Ok(placement) => placement,
    Err(ClusterError::Uncertain { placement } | ClusterError::NotPlaced { placement }) => placement,
    Err(_) => return Some(dispatch),
  };
  let _ = run_on(origin, shard, move |s| {
    record_merge_acks(s, local, object, version, placement, quorum);
  });
  Some(dispatch)
}

/// Merges a round's acknowledgements into the version's pending record (never overwriting them: each
/// holder's acceptance is durable there), marks the version placed once the quorum holds, and drops
/// the entry once every remote candidate acknowledged it.
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
  let Some(pending) = pending_green.records.get_mut(&version) else {
    return;
  };
  for host in placement.acked {
    if pending.record.candidates.contains(&host) && !pending.record.acked.contains(&host) {
      pending.record.acked.push(host);
      if let Some(versions) = pending_green.owed.get_mut(&host) {
        versions.remove(&version);
        if versions.is_empty() {
          pending_green.owed.remove(&host);
        }
      }
    }
  }
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
  }
  if everyone {
    pending_green.records.remove(&version);
    if pending_green.records.is_empty() {
      state.merge.pending.remove(&object);
    }
  }
  // The version is committed at the quorum: every submit whose acceptance waited on it is answered
  // now — its completion recorded, its reply delivered (§4.16 "Commit"; AUD-11).
  if let Some(placed) = newly_placed {
    resolve_accepted(state, object, placed);
  }
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
  *state.refusals.entry(ACCEPTANCE_DEFERRED).or_insert(0) += 1;
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
    let reply = ReplyBody::Submitted {
      version: Some(version),
      conflicts: Vec::new(),
    };
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
        *state.refusals.entry(ACCEPTANCE_UNRECORDED).or_insert(0) += 1;
        continue;
      }
      *state.refusals.entry(ACCEPTANCE_RESOLVED).or_insert(0) += 1;
      let Some(route) = entry.route else {
        continue;
      };
      let delivery = slates_rt::task::SpawnRequest::new(
        Box::pin(async move {
          crate::state::deliver(route.client_index, route.request, recorded, true);
        }),
        None,
      );
      if slates_rt::registry::send_control(
        route.shard,
        slates_rt::control::Control::Spawn(Box::new(delivery)),
      )
      .is_err()
      {
        *state.refusals.entry(ACCEPTANCE_UNDELIVERED).or_insert(0) += 1;
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
/// A resolved acceptance whose completion record could not be made durable (rolled back); the
/// client's retry re-executes and meets the idempotent accept.
const ACCEPTANCE_UNRECORDED: &str = "merge.acceptance_unrecorded";
/// A resolved acceptance whose delivery task the client's shard refused at its admission bound; the
/// completion record answers the client's retry.
const ACCEPTANCE_UNDELIVERED: &str = "merge.acceptance_undelivered";
/// A holder withheld a merge record's acknowledgement (the injected fault).
const RECORD_WITHHELD: &str = "merge.record_withheld";

/// Serves one merge record on a holder (§4.16 "Apply on holders"): the inputs must be held, the
/// replica must be at the version before, the recomputation must reproduce the record's identity;
/// then the record is accepted into the object's acceptor like a head record and the bound
/// acknowledgement returned. Every other case is an empty reply the owner counts as no
/// acknowledgement, with the reason counted here; a mismatch is fatal for the green on this holder.
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
    *state.refusals.entry(RECORD_WITHHELD).or_insert(0) += 1;
    return Vec::new();
  }
  let Some(value) = MergeRecordValue::from_record_bytes(&record.value) else {
    *state.refusals.entry(INPUTS_UNDECODABLE).or_insert(0) += 1;
    return Vec::new();
  };
  if record.sequence != value.version {
    *state.refusals.entry(OUT_OF_ORDER).or_insert(0) += 1;
    return Vec::new();
  }
  let next = state
    .merge
    .replicas
    .get(&object)
    .map(|replica| replica.head() + 1);
  match (next, value.version) {
    // A version this replica already applied: re-acknowledge from the acceptor (idempotent).
    (Some(next), version) if version < next => {
      return crate::fleet::accept_held_record(state, local, peer_host, record);
    }
    (None, 0) => {}
    (Some(next), version) if version == next => {}
    _ => {
      *state.refusals.entry(OUT_OF_ORDER).or_insert(0) += 1;
      return Vec::new();
    }
  }
  let inputs = match value.inputs {
    None => None,
    Some(manifest) => match held_inputs(state, &manifest) {
      Some(bytes) => Some(bytes),
      None => {
        *state.refusals.entry(INPUTS_UNHELD).or_insert(0) += 1;
        return Vec::new();
      }
    },
  };
  match recompute(state, object, &value, inputs.as_deref()) {
    Ok(()) => crate::fleet::accept_held_record(state, local, peer_host, record),
    Err(reason) => {
      *state.refusals.entry(reason).or_insert(0) += 1;
      if reason == RECOMPUTE_MISMATCH {
        state.merge.refused.insert(object);
        state.merge.replicas.remove(&object);
        eprintln!(
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

/// The inputs bytes a manifest names, from this holder's content hold — the one file's one chunk —
/// with a test's corruption fault applied once if set (the last byte of the post-state, so the
/// increment still decodes but recomputes to different bytes).
fn held_inputs(state: &mut ShardState, manifest: &[u8; 32]) -> Option<Vec<u8>> {
  let archive = state.held_content.archive_of(manifest)?;
  let chunk = archive.chunks.first()?;
  let mut bytes = Archive::content(chunk).ok()?;
  if state.merge.fault.corrupt_next_inputs {
    state.merge.fault.corrupt_next_inputs = false;
    // The increment encoding ends with the evidence count (a `u64`); the byte before it is the
    // post-state's last byte when the post-state is not empty.
    let evidence_count_bytes = size_of::<u64>();
    if bytes.len() > evidence_count_bytes {
      let at = bytes.len() - evidence_count_bytes - 1;
      bytes[at] ^= CORRUPTION_MASK;
    }
  }
  Some(bytes)
}

/// Recomputes `value.version` into this holder's replica of `object` from `inputs` and compares the
/// head identity with the record's. Version 0 seeds the replica from the origin (or empty); a later
/// version submits the increment, which must accept as exactly that version. `Err` names the
/// counter for the reason.
fn recompute(
  state: &mut ShardState,
  object: ObjectId,
  value: &MergeRecordValue,
  inputs: Option<&[u8]>,
) -> Result<(), &'static str> {
  let mut replica = if value.version == 0 {
    match inputs {
      None => Green::new(),
      Some(bytes) => Green::with_origin(
        &slates_merge::origin::Origin::decode(bytes).map_err(|_| INPUTS_UNDECODABLE)?,
      ),
    }
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
    && let Some(existing) = state.merge.replicas.get(&object)
    && existing.head_identity() != value.identity
  {
    return Err(OUT_OF_ORDER);
  }
  if value.version == 0 && state.merge.replicas.contains_key(&object) {
    replica = state.merge.replicas.remove(&object).ok_or(OUT_OF_ORDER)?;
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

  /// AC-8.19 / T-8.17, AUD-13: a record's inputs reached quorum without C. Expect to ship
  /// to B now and leave C waiting for inputs; after C holds them its record becomes dispatchable.
  #[test]
  fn quorum_input_placement_preserves_a_lagging_holders_input_debt() {
    let (local, second, third) = (HostId(1), HostId(2), HostId(3));
    let quorum = Quorum { f: 1 };
    let candidates = vec![local, second, third];
    let mut pending = PendingMergeRecord {
      value: MergeRecordValue {
        version: 1,
        increment: [1; 32],
        base: 0,
        inputs: Some([2; 32]),
        identity: [3; 32],
        evidence: Vec::new(),
      },
      inputs: Placement {
        candidates: candidates.clone(),
        acked: vec![local, second],
        mirror_acked: None,
      },
      record: Placement {
        candidates,
        acked: Vec::new(),
        mirror_acked: None,
      },
    };
    assert_eq!(
      record_targets(&pending, &[second, third], quorum),
      vec![second]
    );
    pending.inputs.acked.push(third);
    assert_eq!(record_targets(&pending, &[third], quorum), vec![third]);
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
            },
            inputs: Placement {
              candidates: candidates.clone(),
              acked: candidates.clone(),
              mirror_acked: None,
            },
            record: Placement {
              candidates: candidates.clone(),
              acked: Vec::new(),
              mirror_acked: None,
            },
          },
        );
      }
      let placement = |acked| Placement {
        candidates: candidates.clone(),
        acked,
        mirror_acked: None,
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
        next_merge_work(state, local)
          .into_iter()
          .filter_map(|work| match work {
            MergeWork::Ship {
              version, targets, ..
            } => Some((version, targets)),
            _ => None,
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
    let mut hold = slates_cluster::content::ContentHold::new();
    let held = hold
      .hold(MergeShardState::inputs_archive(bytes, 0, 4096))
      .expect("the inputs archive is whole and verifies");
    assert_eq!(named, held, "the record names what the hold keys by");
    let found = hold
      .archive_of(&named)
      .expect("the holder finds the inputs the record names");
    assert_eq!(Archive::content(&found.chunks[0]).unwrap(), bytes);
    let tree_only = MergeShardState::inputs_archive(bytes, 0, 4096)
      .manifest
      .identity();
    assert_ne!(
      tree_only, held,
      "the tree's own identity is not the hold's key (format minor 2)"
    );
    assert!(
      hold.archive_of(&tree_only).is_none(),
      "a record naming the tree's identity finds nothing"
    );
  }
}

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
}

/// A green attachment's pin: the green and the version its view is fixed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedAttachment {
  /// The green.
  pub green: DbVolumeId,
  /// The pinned version.
  pub version: u64,
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
  ReplyBody::Attached {
    attachment,
    lease_epoch: None,
    path: None,
    version: Some(version),
  }
}

/// Drops the pin an attachment held (its detach, or a destroy of its green).
pub(crate) fn forget_attachment(state: &mut ShardState, attachment: u64) {
  state.merge.attachments.remove(&attachment);
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
  if target > engine.head() {
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
  let key = path.trim_start_matches('/');
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
  state.works.remove(&id);
  if state.greens.remove(&id).is_some() {
    state.merge.attachments.retain(|_, pin| pin.green != id);
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
  Some(ReplyBody::Status {
    report: slates_ipc::protocol::StatusReport {
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

//! A captured clone's capture, carried with every head it places (§4.15 `capture_base`; A-503's carry, made every
//! seal's by A-544).
//!
//! A clone of a captured base lands against its capture (A-424), which lives only on the node that made the clone. A
//! move (A-503) carries the part of it the clone changed: the captured state of every path where the capture and the
//! head the move hands over differ, in the volume's catalog, with that head's identity, and the new owner keeps the
//! head it materializes as its mark. A takeover after the owner's loss materializes the newest head placed, so the
//! record it needs is the one for that head. So every head of a captured clone carries its record: when a captured
//! clone's head is a new snapshot, its owner walks the capture against it in a task of the volume's shard, a slice a
//! turn ([`step`], `slates_vfs::captured::CarryWalk`), and records the captured state of every path where they differ
//! with the head's archive identity once the seal names it (`Op::CaptureCarried`, which raises the catalog's sequence).
//! The head register at that sequence ships only once the record is in ([`holds_head_back`]), and a head ships only once
//! its catalog's current version is placed, so every head a successor can adopt comes with its record. The successor
//! marks the snapshot it materializes the head as (`verbs::finish_making`), and its landing is judged against the
//! capture as the first owner's was. A move needs nothing more: the head it hands over is placed with its record.
//!
//! A clone of a captured base is born with its record, naming no head ([`creation_record`], `verbs::publish_clone_as`):
//! a successor that adopts a head the record is not for (its creation head, or a head whose carry was refused) holds no
//! mark, and refuses the landing by kind rather than land the clone as a volume with no capture. A carry that cannot be
//! walked (its capture or its head unreadable) lets its head ship without it, counted [`CARRY_REFUSED`]: the head's
//! content is never held back for a landing's sake, and a landing of that head on a successor is refused.

use std::collections::BTreeMap;

use slates_db::catalog::{CarriedRecord, SnapshotId as DbSnapshotId, VolumeId as DbVolumeId};
use slates_db::op::Op;
use slates_db::register::ObjectId;
use slates_vfs::captured::{CarriedCapture, CarryWalk};
use slates_vfs::names::NameEquivalence;
use slates_wire::Wire;

use crate::head::HeadValue;
use crate::state::{self, ShardState};

/// Counter: captured clones' heads whose capture was carried and recorded in the volume's catalog.
const CARRY_RECORDED: &str = "fleet.carry.recorded";
/// Counter: captured clones' heads whose capture could not be carried (its walk refused, or its task not started): the
/// head ships without its record, and its landing on a successor is refused.
const CARRY_REFUSED: &str = "fleet.carry.refused";
/// Counter: carries the database refused to record, asked again the next period.
const CARRY_UNRECORDED: &str = "fleet.carry.unrecorded";

/// Where a captured clone's carry for one of its heads stands.
#[derive(Debug)]
pub(crate) enum Stage {
  /// Its task walks the capture against the head.
  Running,
  /// Walked: what the catalog will carry, recorded once the head's seal names its identity.
  Carried(CarriedCapture),
  /// Refused: the head ships without its record.
  Refused,
}

/// A captured clone's carry for its head `snapshot`.
#[derive(Debug)]
pub(crate) struct Carry {
  /// The head snapshot carried for.
  pub(crate) snapshot: DbSnapshotId,
  /// Where it stands.
  pub(crate) stage: Stage,
}

/// The carries of a shard's captured clones, by volume id. Bounded by its volumes; an entry leaves once recorded, or
/// once its volume is no captured clone here.
pub(crate) type Carries = BTreeMap<DbVolumeId, Carry>;

/// The record a clone of a captured base is born with (A-544): no head's (a head identity no archive has), and no path.
/// A successor that adopts such a record holds no mark, so its landings are refused by kind.
pub(crate) fn creation_record() -> CarriedRecord {
  CarriedRecord {
    head: CarriedRecord::NO_HEAD,
    entries: CarriedCapture::default().to_bytes(),
  }
}

/// Whether volume `id` is a captured clone here: its capture on this node (stage 1), or a record carried for it.
fn captured(state: &ShardState, id: DbVolumeId) -> bool {
  let partition = state.db.partition();
  partition.carried(id).is_some()
    || partition
      .lineage(id)
      .is_some_and(|edge| partition.is_captured(edge.origin_volume, edge.origin_snapshot))
}

/// One period's step of volume `id`'s carry, on its owner shard, from the seal walk: nothing for a volume that is no
/// captured clone or has no sealed head; else, unless its record is for its head already, the carry for its head begun
/// (an older head's given up), or recorded once walked and its head's seal names its identity.
pub(crate) fn step(state: &mut ShardState, id: DbVolumeId) {
  if !captured(state, id) {
    state.carries.remove(&id);
    return;
  }
  let partition = state.db.partition();
  let Some(head) = partition
    .volume(id)
    .filter(|record| record.epoch > 0)
    .map(|record| record.head)
  else {
    return;
  };
  if partition
    .carried(id)
    .is_some_and(|(_, mark)| mark == Some(head))
  {
    state.carries.remove(&id);
    return;
  }
  match state.carries.get(&id) {
    Some(carry) if carry.snapshot == head => {
      if matches!(carry.stage, Stage::Carried(_)) {
        record(state, id, head);
      }
    }
    _ => begin(state, id, head),
  }
}

/// Whether volume `id`'s head `value` (for its head snapshot `head`) waits for its record (A-544): it names content, the
/// volume is a captured clone, its record is for another head, and no carry for this head was refused.
pub(crate) fn holds_head_back(
  state: &ShardState,
  id: DbVolumeId,
  head: DbSnapshotId,
  value: &HeadValue,
) -> bool {
  let Some(manifest) = value.manifest else {
    return false;
  };
  if !captured(state, id) {
    return false;
  }
  if state
    .db
    .partition()
    .carried(id)
    .is_some_and(|(carried, _)| carried.head == manifest)
  {
    return false;
  }
  !matches!(
    state.carries.get(&id),
    Some(Carry {
      snapshot,
      stage: Stage::Refused,
    }) if *snapshot == head
  )
}

/// Begins carrying volume `id`'s capture for its head `head`: a task of this shard walks the capture (`crate::landing::
/// capture_of`: its capture here, or its mark and what was carried) against the head's tree, a slice a turn
/// ([`slices`]). A capture or head that cannot be read, or a task that cannot start, refuses the carry, counted.
fn begin(state: &mut ShardState, id: DbVolumeId, head: DbSnapshotId) {
  let base = match crate::landing::capture_of(state, id) {
    Ok(Some(base)) => base,
    Ok(None) | Err(_) => {
      refuse(state, id, head);
      return;
    }
  };
  let walk = state
    .by_id
    .get(&id)
    .and_then(|handle| state.volumes.get(*handle).ok())
    .and_then(|slot| {
      let tree = slot
        .volume
        .frozen_tree(crate::verbs::core_snapshot(
          slates_ipc::protocol::SnapshotId { value: head.value },
        ))
        .ok()?;
      Some((CarryWalk::new(base, tree), slot.volume.policy()))
    });
  let Some((walk, policy)) = walk else {
    refuse(state, id, head);
    return;
  };
  state.carries.insert(
    id,
    Carry {
      snapshot: head,
      stage: Stage::Running,
    },
  );
  let spawned =
    slates_rt::futures::spawn(slices(id, head, walk, policy)).and_then(slates_rt::futures::detach);
  if spawned.is_err() {
    refuse(state, id, head);
  }
}

/// Records that volume `id`'s carry for its head `head` was refused: the head ships without its record.
fn refuse(state: &mut ShardState, id: DbVolumeId, head: DbSnapshotId) {
  state.carries.insert(
    id,
    Carry {
      snapshot: head,
      stage: Stage::Refused,
    },
  );
  state.count(CARRY_REFUSED, 1);
  slates_machine::log_line!(
    "slates daemon: the capture of a captured clone {} could not be carried for its head; the head ships without it",
    crate::verbs::hex_of(&id.bytes)
  );
}

/// The task carrying volume `id`'s capture for its head `head` ([`begin`]): `walk` stepped a slice a turn of this shard
/// (`crate::image::sliced`), yielding between slices, until it ends or its entry no longer runs for `head` (a newer head
/// began its own carry, or the volume is gone). Owned by that entry: it ends with it.
async fn slices(id: DbVolumeId, head: DbSnapshotId, mut walk: CarryWalk, policy: NameEquivalence) {
  loop {
    let more = state::with_state(|s| {
      let running = matches!(
        s.carries.get(&id),
        Some(Carry {
          snapshot,
          stage: Stage::Running,
        }) if *snapshot == head
      );
      if !running {
        return false;
      }
      match crate::image::sliced(s, |s| {
        walk
          .unit(&s.store, policy)
          .map_err(|error| crate::error::refusal_of_vfs(&error))
      }) {
        Ok(None) => true,
        Ok(Some(carried)) => {
          s.carries.insert(
            id,
            Carry {
              snapshot: head,
              stage: Stage::Carried(carried),
            },
          );
          false
        }
        Err(_) => {
          refuse(s, id, head);
          false
        }
      }
    });
    if more != Some(true) {
      return;
    }
    slates_rt::futures::yield_now().await;
  }
}

/// Records volume `id`'s walked carry for its head `head` once the head's seal names its identity (the snapshot's
/// recorded identity, or the seal in progress's manifest): `Op::CaptureCarried`, its mark the head. A refusal of the
/// database keeps the carry for the next period, counted.
fn record(state: &mut ShardState, id: DbVolumeId, head: DbSnapshotId) {
  let object = ObjectId(id.bytes);
  let identity = state
    .db
    .partition()
    .snapshot(id, head)
    .and_then(|snapshot| snapshot.identity)
    .or_else(|| {
      state
        .seals
        .get(&object)
        .filter(|job| job.snapshot == head)
        .and_then(|job| job.manifest)
    });
  let Some(identity) = identity else {
    return;
  };
  let Some(Carry {
    stage: Stage::Carried(carried),
    ..
  }) = state.carries.remove(&id)
  else {
    return;
  };
  let op = Op::CaptureCarried {
    volume: id,
    carried: CarriedRecord {
      head: identity,
      entries: carried.to_bytes(),
    },
    mark: Some(head),
  };
  let now = state.clock.monotonic_ns();
  if state.db.mutate(&mut state.segment, &op, now).is_err() {
    state.count(CARRY_UNRECORDED, 1);
    state.carries.insert(
      id,
      Carry {
        snapshot: head,
        stage: Stage::Carried(carried),
      },
    );
    return;
  }
  state.count(CARRY_RECORDED, 1);
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_ipc::protocol::{NamePolicy, ReplyBody, RequestBody, SizeClass};
  use slates_vfs::captured::CapturedState;

  /// Format: the identity the test names its clone's sealed head by, as the seal's archive would.
  const IDENTITY: [u8; 32] = [7; 32];
  /// Shape: how many looks at the shard the carry is given to be recorded, each after the shard's own turns.
  const LOOKS: usize = 500;
  /// Shape: the pause between two looks.
  const LOOK_EVERY: std::time::Duration = std::time::Duration::from_millis(10);

  /// A captured base cloned, the clone's `f` rewritten and `g` made, and the clone's head snapshotted and named as its
  /// seal would name it: the clone and its head snapshot.
  fn captured_clone_with_a_sealed_head(state: &mut ShardState) -> (DbVolumeId, DbSnapshotId) {
    let owner = slates_db::catalog::Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1234,
    };
    let ReplyBody::Created { id: origin } = crate::verbs::dispatch(
      state,
      1,
      &owner,
      RequestBody::Create {
        name: "carried-origin".to_owned(),
        size: SizeClass::Dynamic { max: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    ) else {
      panic!("the origin was created")
    };
    let write = |state: &mut ShardState, volume: DbVolumeId, name: &str, bytes: &[u8]| {
      let handle = *state.by_id.get(&volume).unwrap();
      let ShardState { store, volumes, .. } = &mut *state;
      let slot = volumes.get_mut(handle).unwrap();
      let root = slot.volume.root_inode(store).unwrap();
      let file = match slot.volume.lookup_no(store, root, name) {
        Ok(found) => found.inode,
        Err(_) => slot
          .volume
          .create_file_no(store, root, name, 0o644)
          .unwrap(),
      };
      slot.volume.truncate(store, file, 0).unwrap();
      slot.volume.write(store, file, 0, bytes).unwrap();
    };
    let origin_id = crate::verbs::to_db_volume(origin);
    write(state, origin_id, "f", b"captured");
    let ReplyBody::Snapshotted { id: snapshot, .. } =
      crate::verbs::dispatch(state, 1, &owner, RequestBody::Snapshot { volume: origin })
    else {
      panic!("the origin was snapshotted")
    };
    let captured = Op::BaseCaptured {
      volume: origin_id,
      snapshot: crate::verbs::to_db_snapshot(snapshot),
    };
    state.db.mutate(&mut state.segment, &captured, 0).unwrap();
    let reply = crate::verbs::dispatch(
      state,
      1,
      &owner,
      RequestBody::Clone {
        volume: origin,
        snapshot,
        name: "carried-clone".to_owned(),
      },
    );
    let ReplyBody::Cloned { id: clone } = reply else {
      panic!("the clone was made: {reply:?}")
    };
    let clone_id = crate::verbs::to_db_volume(clone);
    write(state, clone_id, "f", b"the agent's");
    write(state, clone_id, "g", b"made");
    let ReplyBody::Snapshotted { .. } =
      crate::verbs::dispatch(state, 1, &owner, RequestBody::Snapshot { volume: clone })
    else {
      panic!("the clone was snapshotted")
    };
    let head = state.db.partition().volume(clone_id).unwrap().head;
    let identified = Op::SnapshotIdentified {
      volume: clone_id,
      id: head,
      identity: IDENTITY,
    };
    state.db.mutate(&mut state.segment, &identified, 0).unwrap();
    (clone_id, head)
  }

  /// The head value the record plane would ship for the clone's sealed head.
  fn sealed() -> HeadValue {
    HeadValue {
      manifest: Some(IDENTITY),
      content_holders: Vec::new(),
      sealing: None,
    }
  }

  /// A-544. Do: on a shard, clone a captured base, rewrite the clone's `f` and make `g`, snapshot the clone and name the
  /// snapshot as its seal would, then step its carry as the seal walk does, a look at a time, until the record is in.
  /// Expect: the head held back before the record and shipped after; the record for that head, its mark the head,
  /// naming `f` as the capture held it and `g` as absent from the capture. Before A-544 a captured clone's heads
  /// carried nothing, and a successor that adopted one landed it as a volume with no capture.
  #[test]
  fn a544_a_captured_clones_head_waits_for_its_capture_record_and_carries_it() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let (clone, head, held_before) = crate::daemon::observe_first(&daemon, |state| {
      let (clone, head) = captured_clone_with_a_sealed_head(state);
      let held = holds_head_back(state, clone, head, &sealed());
      step(state, clone);
      (clone, head, held)
    });
    assert!(held_before, "the head waits for its record");
    let mut recorded = None;
    for _ in 0..LOOKS {
      let looked = crate::daemon::observe_first(&daemon, move |state| {
        step(state, clone);
        let record = state
          .db
          .partition()
          .carried(clone)
          .map(|(carried, mark)| (carried.clone(), mark));
        (record, holds_head_back(state, clone, head, &sealed()))
      });
      if let (Some((record, Some(mark))), held) = looked
        && record.head == IDENTITY
      {
        assert_eq!(mark, head, "the record's mark is the head it is for");
        assert!(!held, "the head ships with its record");
        recorded = Some(record);
        break;
      }
      std::thread::sleep(LOOK_EVERY);
    }
    let record = recorded.expect("the carry was recorded for the head");
    let carried = CarriedCapture::parse(&record.entries).unwrap();
    assert!(
      matches!(
        carried.get("/f"),
        Some(CapturedState::File { identity, size: 8, .. }) if *identity == *blake3::hash(b"captured").as_bytes()
      ),
      "{carried:?}"
    );
    assert_eq!(
      carried.get("/g"),
      Some(&CapturedState::Absent),
      "{carried:?}"
    );
  }
}

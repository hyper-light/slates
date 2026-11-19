//! A captured clone's capture, carried with every head it places (§4.15 `capture_base`; A-503's carry, made every
//! seal's by A-544).
//!
//! A clone of a captured base lands against its capture (A-424), which lives only on the node that made the clone. A
//! move (A-503) carries the part of it the clone changed: the captured state of every path where the capture and the
//! head the move hands over differ, in the volume's catalog, with that head's identity, and the new owner keeps the
//! head it materializes as its mark. A takeover after the owner's loss materializes the newest head placed, so the
//! record it needs is the one for that head. So every head of a captured clone carries its record: when a captured
//! clone's head is a new snapshot of its own, its owner walks the capture against it in a task of the volume's shard, a
//! slice a turn ([`step`], `slates_vfs::captured::CarryWalk`), and records the captured state of every path where they
//! differ with the head's archive identity once the seal names it (`Op::CaptureCarried`, which raises the catalog's
//! sequence). The head register ships only once the record for its head is in ([`holds_head_back`]), and a head ships
//! only once its catalog's current version is placed, so a successor adopts the record with the head and marks the
//! snapshot it materializes the head as (`verbs::finish_making`); its landing is judged against the capture as the
//! first owner's was. A move hands over a head placed this way, and waits until the head it hands over carries its
//! record ([`carries_its_head`]), as A-503's move waited for its own walk.
//!
//! A clone of a captured base is born with its record, naming no head ([`creation_record`], `verbs::publish_clone_as`):
//! a successor that adopts a head the record is not for (one whose carry was refused, or the creation head of a clone
//! whose owner died before its first head was placed) holds no mark, and refuses the landing by kind rather than land
//! the clone as a volume with no capture. Until its first snapshot a clone's head is its origin's (its record starts as
//! a copy of its origin's), which its own tree cannot walk, so it carries nothing until then ([`own_head`]). A carry
//! that cannot be walked (its capture or its head unreadable, or no mark here to carry from) lets its head ship without
//! it, counted [`CARRY_REFUSED`]: the head's content is never held back for a landing's sake, a landing of that head on
//! a successor is refused, and a move of it waits until it is refused `Unplaced`.
//!
//! Not covered: the catalog is placed before the head it waits for, so a takeover in the round between the two adopts
//! the earlier head with the newer head's record, holds no mark and refuses its landing by kind (its tree is whole); and
//! a live-shipped clone rebuilt from its log past its seal holds no mark (A-503's owed item).

use std::collections::BTreeMap;

use slates_db::catalog::{
  CarriedRecord, SnapshotId as DbSnapshotId, VolumeId as DbVolumeId, VolumeState,
};
use slates_db::op::Op;
use slates_db::register::ObjectId;
use slates_vfs::captured::{CarriedCapture, CarryWalk};
use slates_vfs::clock::Clock;
use slates_vfs::names::NameEquivalence;
use slates_wire::Wire;

use crate::head::HeadValue;
use crate::state::{self, ShardState};

/// Counter: captured clones' heads whose capture was carried and recorded in the volume's catalog.
const CARRY_RECORDED: &str = "fleet.carry.recorded";
/// Counter: captured clones' heads whose capture could not be carried (its walk refused, no mark here to carry from, or
/// its task not started): the head ships without its record, its landing on a successor is refused, and a move of it
/// waits until it is refused `Unplaced`.
pub(crate) const CARRY_REFUSED: &str = "fleet.carry.refused";
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

/// The carries of a shard's captured clones, by volume id. Bounded by the volumes it serves: an entry leaves once
/// recorded, once its volume is no captured clone here or has no head of its own, and at the end of the home seal walk
/// once the shard no longer serves its volume ([`forget_gone`]).
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

/// Volume `id`'s head when it is a snapshot of its own: sealed (`epoch > 0`), one of the volume's snapshots here, and
/// the volume not being destroyed. A clone's record starts as a copy of its origin's (`verbs::publish_clone_as`), so
/// until the clone's first snapshot its head names its origin's snapshot, which the clone's own tree cannot walk: a
/// carry begun for it was refused, counted, for every captured clone made.
fn own_head(state: &ShardState, id: DbVolumeId) -> Option<DbSnapshotId> {
  let partition = state.db.partition();
  let record = partition.volume(id)?;
  if record.epoch == 0 || record.state == VolumeState::Destroying {
    return None;
  }
  partition.snapshot(id, record.head).map(|_| record.head)
}

/// One period's step of volume `id`'s carry, on its owner shard, from the seal walk: nothing for a volume that is no
/// captured clone or has no head of its own ([`own_head`]); else, unless its record is for its head already, the carry
/// for its head begun (an older head's given up), or recorded once walked and its head's seal names its identity.
pub(crate) fn step(state: &mut ShardState, id: DbVolumeId) {
  if !captured(state, id) {
    state.carries.remove(&id);
    return;
  }
  let Some(head) = own_head(state, id) else {
    state.carries.remove(&id);
    return;
  };
  if state
    .db
    .partition()
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
/// volume is a captured clone, `head` is a snapshot of its own (its origin's head never gets a record), its record is for
/// another head, and no carry for this head was refused.
pub(crate) fn holds_head_back(
  state: &ShardState,
  id: DbVolumeId,
  head: DbSnapshotId,
  value: &HeadValue,
) -> bool {
  let Some(manifest) = value.manifest else {
    return false;
  };
  if !captured(state, id) || own_head(state, id) != Some(head) {
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

/// Whether volume `id`'s head carries its capture's record here, so a move may hand it over (A-544; A-503's move waited
/// for its own walk the same way): a volume that is no captured clone always does; a captured clone once its record is
/// for its head (its mark is its head). A carry still walking or refused, or a node holding no mark to carry from, holds
/// the move back until it is refused `Unplaced`, so a move never turns a clone that lands against its capture where it is
/// into one whose landing its new owner refuses.
pub(crate) fn carries_its_head(state: &ShardState, id: DbVolumeId) -> bool {
  if !captured(state, id) {
    return true;
  }
  let partition = state.db.partition();
  partition.volume(id).is_some_and(|record| {
    partition
      .carried(id)
      .is_some_and(|(_, mark)| mark == Some(record.head))
  })
}

/// Forgets the carries of volumes this shard no longer serves (released to a move's new owner, relinquished to a
/// successor, or destroyed), at the end of each home seal walk: the walk visits only the volumes it serves, so an entry
/// refused or waiting for its seal would otherwise outlive its volume. A task whose entry is gone ends ([`slices`]).
pub(crate) fn forget_gone(state: &mut ShardState) {
  let served = &state.by_id;
  state.carries.retain(|id, _| served.contains_key(id));
}

/// Begins carrying volume `id`'s capture for its head `head`: a task of this shard walks the capture (`crate::landing::
/// capture_of`: its capture here, or its mark and what was carried) against the head's tree, a slice a turn
/// ([`slices`]). A capture or head that cannot be read, no mark here to carry from, or a task that cannot start refuses
/// the carry, counted.
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

  /// Format: the identity the tests name a clone's sealed head by, as the seal's archive would.
  const IDENTITY: [u8; 32] = [7; 32];
  /// Shape: how many looks at the shard a carry is given to be recorded, each after the shard's own turns.
  const LOOKS: usize = 500;
  /// Shape: the pause between two looks.
  const LOOK_EVERY: std::time::Duration = std::time::Duration::from_millis(10);

  /// The principal the tests' volumes are made for.
  fn owner() -> slates_db::catalog::Principal {
    slates_db::catalog::Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1234,
    }
  }

  /// Writes `bytes` as the whole of `volume`'s root file `name`, made if absent.
  fn write(state: &mut ShardState, volume: DbVolumeId, name: &str, bytes: &[u8]) {
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
  }

  /// A base named `<name>-origin` holding `f`, its snapshot captured, and that snapshot cloned as `<name>-clone`: the
  /// clone, which has taken no snapshot of its own.
  fn captured_clone(state: &mut ShardState, name: &str) -> DbVolumeId {
    let ReplyBody::Created { id: origin } = crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::Create {
        name: format!("{name}-origin"),
        size: SizeClass::Dynamic { max: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    ) else {
      panic!("the origin was created")
    };
    write(state, crate::verbs::to_db_volume(origin), "f", b"captured");
    let ReplyBody::Snapshotted { id: snapshot, .. } =
      crate::verbs::dispatch(state, 1, &owner(), RequestBody::Snapshot { volume: origin })
    else {
      panic!("the origin was snapshotted")
    };
    let captured = Op::BaseCaptured {
      volume: crate::verbs::to_db_volume(origin),
      snapshot: crate::verbs::to_db_snapshot(snapshot),
    };
    state.db.mutate(&mut state.segment, &captured, 0).unwrap();
    let reply = crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::Clone {
        volume: origin,
        snapshot,
        name: format!("{name}-clone"),
      },
    );
    let ReplyBody::Cloned { id: clone } = reply else {
      panic!("the clone was made: {reply:?}")
    };
    crate::verbs::to_db_volume(clone)
  }

  /// `clone`'s `f` rewritten and `g` made, and its head snapshotted and named as its seal would name it: that head.
  fn sealed_head(state: &mut ShardState, clone: DbVolumeId) -> DbSnapshotId {
    write(state, clone, "f", b"the agent's");
    write(state, clone, "g", b"made");
    let volume = crate::verbs::to_wire_volume(clone);
    let ReplyBody::Snapshotted { .. } =
      crate::verbs::dispatch(state, 1, &owner(), RequestBody::Snapshot { volume })
    else {
      panic!("the clone was snapshotted")
    };
    let head = state.db.partition().volume(clone).unwrap().head;
    let identified = Op::SnapshotIdentified {
      volume: clone,
      id: head,
      identity: IDENTITY,
    };
    state.db.mutate(&mut state.segment, &identified, 0).unwrap();
    head
  }

  /// The head value the record plane would ship for a clone's sealed head.
  fn sealed() -> HeadValue {
    HeadValue {
      manifest: Some(IDENTITY),
      content_holders: Vec::new(),
      sealing: None,
    }
  }

  /// What one look at `clone` found once its record names its sealed head: the record, its mark, whether the head
  /// `head` still waited for a record, and whether a move would hand the head over.
  type Recorded = (CarriedRecord, Option<DbSnapshotId>, bool, bool);

  /// Steps `clone`'s carry as the seal walk does, a look at a time, until its record names its sealed head: what that
  /// look found, or `None` if it never did within the looks.
  fn looked_until_recorded(
    daemon: &crate::daemon::Daemon,
    clone: DbVolumeId,
    head: DbSnapshotId,
  ) -> Option<Recorded> {
    for _ in 0..LOOKS {
      let looked = crate::daemon::observe_first(daemon, move |state| {
        step(state, clone);
        let record = state
          .db
          .partition()
          .carried(clone)
          .map(|(carried, mark)| (carried.clone(), mark));
        record.map(|(record, mark)| {
          (
            record,
            mark,
            holds_head_back(state, clone, head, &sealed()),
            carries_its_head(state, clone),
          )
        })
      });
      if let Some(found) = looked
        && found.0.head == IDENTITY
      {
        return Some(found);
      }
      std::thread::sleep(LOOK_EVERY);
    }
    None
  }

  /// A-544. Do: on a shard, clone a captured base, rewrite the clone's `f` and make `g`, snapshot the clone and name the
  /// snapshot as its seal would, then step its carry as the seal walk does, a look at a time, until the record is in.
  /// Expect: before the record the head held back and a move of the clone waiting; after it the head shipped, a move
  /// free to hand it over, and the record for that head, its mark the head, naming `f` as the capture held it and `g`
  /// as absent from the capture. Before A-544 a captured clone's heads carried nothing, and a successor that adopted
  /// one landed it as a volume with no capture.
  #[test]
  fn a544_a_captured_clones_head_waits_for_its_capture_record_and_carries_it() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let (clone, head, held_before, moves_before) = crate::daemon::observe_first(&daemon, |state| {
      let clone = captured_clone(state, "carried");
      let head = sealed_head(state, clone);
      let held = holds_head_back(state, clone, head, &sealed());
      let moves = carries_its_head(state, clone);
      step(state, clone);
      (clone, head, held, moves)
    });
    assert!(held_before, "the head waits for its record");
    assert!(
      !moves_before,
      "a move waits for the record of the head it hands over"
    );
    let (record, mark, held, moves) =
      looked_until_recorded(&daemon, clone, head).expect("the carry was recorded for the head");
    assert_eq!(mark, Some(head), "the record's mark is the head it is for");
    assert!(!held, "the head ships with its record");
    assert!(moves, "a move hands the head over with its record");
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

  /// A-544. Do: on a shard, clone a captured base, ask whether the clone's head waits for a record and step its carry as
  /// the seal walk does, before the clone takes a snapshot of its own; then seal a head of the clone and step it until
  /// its record is in. Expect: before its first snapshot nothing held back and no carry refused, since the clone's head
  /// is still its origin's, which its own tree cannot walk; after it, the record for its own head, and still no carry
  /// refused. Before the fix the head waited for a record no carry would make, and a carry begun for it was refused,
  /// counted `fleet.carry.refused` with a log line, for every captured clone made (on a kiln fleet runner, every
  /// workspace).
  #[test]
  fn a544_a_fresh_captured_clone_carries_nothing_until_its_first_snapshot() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let (clone, held, refused) = crate::daemon::observe_first(&daemon, |state| {
      let clone = captured_clone(state, "fresh");
      let inherited = state.db.partition().volume(clone).unwrap().head;
      let held = holds_head_back(state, clone, inherited, &sealed());
      step(state, clone);
      (clone, held, state.refusals.get(CARRY_REFUSED).copied())
    });
    assert!(!held, "a head the clone does not own waits for no record");
    assert_eq!(
      refused, None,
      "no carry is begun, so none is refused, for a head the clone does not own"
    );
    let head = crate::daemon::observe_first(&daemon, move |state| sealed_head(state, clone));
    let (record, mark, ..) = looked_until_recorded(&daemon, clone, head)
      .expect("the carry was recorded for its first head of its own");
    assert_eq!(
      mark,
      Some(head),
      "the record's mark is the clone's own head"
    );
    assert_eq!(record.head, IDENTITY, "the record is for that head");
    let refused =
      crate::daemon::observe_first(&daemon, |state| state.refusals.get(CARRY_REFUSED).copied());
    assert_eq!(refused, None, "no carry was refused on the way");
  }

  /// A-544. Do: on a shard, begin the carries of two captured clones' sealed heads, then let the shard stop serving one
  /// (its registry entry taken out, as a move's release or a relinquish takes it) and end the home seal walk. Expect: no
  /// carry kept for the clone it let go, and the other's kept. The walk visits only the volumes the shard serves, so a
  /// carry refused or waiting for its seal outlived its volume: one more entry for every such clone the shard let go.
  #[test]
  fn a544_the_carry_of_a_clone_the_shard_no_longer_serves_is_forgotten_at_the_walks_end() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let (gone, kept) = crate::daemon::observe_first(&daemon, |state| {
      let gone = captured_clone(state, "let-go");
      sealed_head(state, gone);
      let kept = captured_clone(state, "kept");
      sealed_head(state, kept);
      step(state, gone);
      step(state, kept);
      state.by_id.remove(&gone);
      forget_gone(state);
      (
        state.carries.contains_key(&gone),
        state.carries.contains_key(&kept),
      )
    });
    assert!(
      !gone,
      "no carry is kept for a clone the shard no longer serves"
    );
    assert!(kept, "the carry of a clone the shard serves is kept");
  }
}

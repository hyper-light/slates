//! A-636 part 2b's own tests: a holder's rule for a state log's part, and the parts an owner keeps, by their bytes and
//! their candidates' acknowledgements. The record plane's use of both (an owner shipping a repository's parts, a holder
//! holding them) is tested where it runs, `crate::fleet`'s tests.

use slates_db::catalog::{
  LogSpan, LoggedChange, NameChange, RepositoryId, StateChange, StateLog, StateLogRecord, VolumeId,
};
use slates_db::register::{HostId, ObjectId, Record};

use super::{PartRefusal, StateLogs, judge, record_of, value_of};

/// A repository on a partition, its id's last byte `serial`.
fn repository(serial: u8) -> RepositoryId {
  let mut bytes = [0u8; 16];
  bytes[15] = serial;
  RepositoryId { bytes }
}

/// `repository`'s record of `log` from `from` to `to`, one bind a position, alone in its operation.
fn part(repository: RepositoryId, log: StateLog, (from, to): (u64, u64)) -> StateLogRecord {
  let changes = (from..to)
    .map(|position| match log {
      StateLog::Names => LoggedChange::Named {
        change: NameChange::Bind {
          name: format!("b{position}"),
          green: VolumeId::default(),
        },
      },
      _ => LoggedChange::Changed {
        change: StateChange::MarkerAdded {
          marker: position.to_be_bytes().to_vec(),
        },
      },
    })
    .collect();
  let record = StateLogRecord {
    repository,
    log,
    from,
    to,
    identity: slates_db::catalog::OperationIdentity {
      digest: [u8::try_from(to).unwrap_or(u8::MAX); 32],
    },
    spans: Vec::new(),
    changes,
  };
  StateLogRecord {
    spans: vec![LogSpan {
      log: record.log_name(),
      from,
      to,
    }],
    ..record
  }
}

/// The register record `part` ships as, at `sequence`, from host 1.
fn shipped(part: &StateLogRecord, sequence: u64) -> Record {
  Record {
    owner: HostId(1),
    object: part.log_name().register(),
    sequence,
    epoch: slates_db::register::FIRST_EPOCH,
    generation: 1,
    value: value_of(part),
  }
}

/// What a holder judges `record` of `log`, holding its predecessor or not.
fn judged(record: &Record, log: StateLog, held: bool) -> Result<(), PartRefusal> {
  judge(
    (record.object, record.sequence),
    record_of(&record.value).as_ref(),
    log,
    held,
  )
}

/// A-636 part 2b. Do: judge, as a holder does, a names log's first part, a later one with and without its predecessor
/// held, one sent to the phases log's register, one at another position, one naming no span of its own, one whose
/// changes are fewer than its positions, and bytes of no state log's record. Expect: the first held at the log's start,
/// the later one only with its predecessor held (`Discontinuous` otherwise), and each other refused by its kind
/// (`Misnamed`, `Misplaced`, `Malformed`), so a holder's copy of a log is always a prefix of it.
#[test]
fn a_holder_holds_a_part_only_where_it_continues_its_copy_of_its_log() {
  let repo = repository(7);
  let first = part(repo, StateLog::Names, (0, 2));
  let later = part(repo, StateLog::Names, (2, 3));
  assert_eq!(judged(&shipped(&first, 2), StateLog::Names, false), Ok(()));
  assert_eq!(judged(&shipped(&later, 3), StateLog::Names, true), Ok(()));
  assert_eq!(
    judged(&shipped(&later, 3), StateLog::Names, false),
    Err(PartRefusal::Discontinuous)
  );
  let elsewhere = Record {
    object: later
      .log_name()
      .register()
      .placement_key()
      .in_class(slates_db::register::RegisterClass::State(StateLog::Phases)),
    ..shipped(&later, 3)
  };
  assert_eq!(
    judged(&elsewhere, StateLog::Phases, true),
    Err(PartRefusal::Misnamed)
  );
  assert_eq!(
    judged(&shipped(&later, 4), StateLog::Names, true),
    Err(PartRefusal::Misplaced)
  );
  let unnamed = StateLogRecord {
    spans: Vec::new(),
    ..later.clone()
  };
  assert_eq!(
    judged(&shipped(&unnamed, 3), StateLog::Names, true),
    Err(PartRefusal::Malformed)
  );
  let short = StateLogRecord {
    changes: Vec::new(),
    ..later.clone()
  };
  assert_eq!(
    judged(&shipped(&short, 3), StateLog::Names, true),
    Err(PartRefusal::Malformed)
  );
  let garbage = Record {
    value: crate::catalog::tagged(crate::catalog::STATE_LOG_CLASS, vec![0xff; 3]),
    ..shipped(&later, 3)
  };
  assert_eq!(
    judged(&garbage, StateLog::Names, true),
    Err(PartRefusal::Malformed)
  );
}

/// A-636 part 2b, the bound. Do: hold a names log's two parts and a phases log's one, with room under a bound of the
/// first two parts' bytes; acknowledge the first names part by one of two remote candidates, then by both; then the
/// phases part by every candidate, and let go of what a laptop's log (no remote candidate) holds. Expect: room before
/// the second part and none after it; nothing let go while one candidate lacks the part; the first part let go, its
/// bytes with it, once both hold it, the second still owed to both; the phases log forgotten once held everywhere; and
/// nothing kept for no remote candidate.
#[test]
fn an_owner_keeps_each_part_until_every_candidate_holds_it_and_counts_its_bytes() {
  let repo = repository(9);
  let (one, two, phases) = (
    part(repo, StateLog::Names, (0, 2)),
    part(repo, StateLog::Names, (2, 3)),
    part(repo, StateLog::Phases, (0, 1)),
  );
  let names_register = one.log_name().register();
  let phases_register = phases.log_name().register();
  let bound = value_of(&one).len() + value_of(&two).len();
  let mut logs = StateLogs::default();
  logs.hold(&one);
  assert!(logs.within(bound), "room for the second part");
  logs.hold(&two);
  assert!(!logs.within(bound), "the bound reached");
  logs.hold(&phases);
  let (b, c) = (HostId(2), HostId(3));
  logs.acknowledged(names_register, 2, &[b]);
  logs.let_go_held(names_register, &[b, c]);
  assert_eq!(owed_positions(&logs, names_register), vec![2, 3]);
  logs.acknowledged(names_register, 2, &[c]);
  logs.let_go_held(names_register, &[b, c]);
  assert_eq!(owed_positions(&logs, names_register), vec![3]);
  assert_eq!(logs.bytes(), value_of(&two).len() + value_of(&phases).len());
  logs.acknowledged(phases_register, 1, &[b, c]);
  logs.let_go_held(phases_register, &[b, c]);
  assert!(owed_positions(&logs, phases_register).is_empty());
  logs.let_go_held(names_register, &[]);
  assert!(owed_positions(&logs, names_register).is_empty());
  assert_eq!(logs.bytes(), 0, "a laptop keeps nothing");
}

/// The positions `register`'s parts are owed at, in order.
fn owed_positions(logs: &StateLogs, register: ObjectId) -> Vec<u64> {
  logs
    .owed()
    .filter(|(owed, _, _)| *owed == register)
    .flat_map(|(_, parts, _)| parts.keys().copied().collect::<Vec<_>>())
    .collect()
}

/// A part of `repository`'s names log from `from` to `to` whose operation also moved `green` from `green_from` to
/// `green_to`, as a push's record names the greens it moved beside its names.
fn push_part(
  repository: RepositoryId,
  (from, to): (u64, u64),
  (green, green_from, green_to): (VolumeId, u64, u64),
) -> StateLogRecord {
  let mut record = part(repository, StateLog::Names, (from, to));
  record.spans.push(LogSpan {
    log: slates_db::catalog::LogName::Green { green },
    from: green_from,
    to: green_to,
  });
  record
}

/// A green, its id's last byte `serial`.
fn green(serial: u8) -> VolumeId {
  let mut bytes = [0u8; 16];
  bytes[15] = serial;
  VolumeId { bytes }
}

/// The push barrier's green rule (main 01a20182's ruling of 2026-11-23, w-chaos's `v3_takeover` frame: the names log held
/// at `f + 1` while the pushes' green versions were not, and a successor rebuilt the repository without two acknowledged
/// pushes). Do: note three pushes' records, each moving the names log and green `main` one version further (to 1, 2, 3),
/// one also moving `topic` to 1; judge them while `main` is placed through 1, then 2, then 3, `topic` through 0, then
/// gone. Expect: not placed while any green is placed short of the newest version a push took it to, placed once every
/// one is, or is gone; and a repository with nothing noted placed. Red before: the barrier asked the names log alone.
#[test]
fn a_push_is_held_until_every_green_it_moved_is_placed_through_its_version() {
  let pushed = repository(1);
  let (main, topic) = (green(1), green(2));
  let mut logs = StateLogs::default();
  logs.note_greens(&push_part(pushed, (0, 1), (main, 0, 1)));
  logs.note_greens(&push_part(pushed, (1, 2), (main, 1, 2)));
  let mut third = push_part(pushed, (2, 3), (main, 2, 3));
  third.spans.push(LogSpan {
    log: slates_db::catalog::LogName::Green { green: topic },
    from: 0,
    to: 1,
  });
  logs.note_greens(&third);
  let placed = |main_at: u64, topic_at: u64| {
    move |green: VolumeId| if green == main { main_at } else { topic_at }
  };
  assert!(
    !logs.greens_placed(pushed, placed(1, 0)),
    "main placed through 1 of 3"
  );
  assert!(
    !logs.greens_placed(pushed, placed(2, 0)),
    "main placed through 2 of 3"
  );
  assert!(
    !logs.greens_placed(pushed, placed(3, 0)),
    "topic placed through 0 of 1"
  );
  assert!(
    logs.greens_placed(pushed, placed(3, u64::MAX)),
    "main placed, topic gone"
  );
  assert!(
    logs.greens_placed(repository(2), placed(0, 0)),
    "nothing noted"
  );
}

/// A-921's second mode (main 01a20182's ruling of 2026-11-26): an owner's parts ride its shard's recovery image and are
/// owed again after a warm restart. Do: hold two names-log parts and a phases part, one names part acknowledged by one
/// candidate; take each part as a checkpoint carries it; restore them into an owner that holds none. Expect: every
/// part owed again at its position with its bytes, no acknowledgement known (so each ships to every candidate again),
/// the bytes counted, and none owed to the next publication; the changes a delta carries are those held and let go.
#[test]
fn an_owners_parts_restored_from_its_image_are_owed_again_to_every_candidate() {
  let repo = repository(7);
  let (one, two, phases) = (
    part(repo, StateLog::Names, (0, 2)),
    part(repo, StateLog::Names, (2, 3)),
    part(repo, StateLog::Phases, (0, 1)),
  );
  let mut logs = StateLogs::default();
  for held in [&one, &two, &phases] {
    logs.hold(held);
  }
  let names = one.log_name().register();
  logs.acknowledged(names, 2, &[HostId(2)]);
  assert!(
    logs.holds_unpublished(),
    "held parts owed to the next publication"
  );
  let carried: Vec<(ObjectId, u64, Vec<u8>)> = logs
    .parts()
    .map(|(register, position, value)| (register, position, value.to_vec()))
    .collect();
  let mut restored = StateLogs::default();
  for (register, position, value) in carried.clone() {
    restored.restore(register, position, value);
  }
  let again: Vec<(ObjectId, u64, Vec<u8>)> = restored
    .parts()
    .map(|(register, position, value)| (register, position, value.to_vec()))
    .collect();
  assert_eq!(again, carried, "every part owed again, at its position");
  assert!(
    restored.owed().all(|(_, _, acked)| acked.is_empty()),
    "no acknowledgement known"
  );
  assert_eq!(restored.bytes(), logs.bytes());
  assert!(!restored.holds_unpublished(), "the image carries them");
}

/// A-921's second mode: an image's owed parts are held to the bound `admit` keeps them under, which they pass by at most
/// the last admitted operation's own parts. Do: judge parts of two operations against bounds at and just past their
/// bytes less the larger operation's share, and no parts at a bound of zero. Expect: within at exactly the bytes less
/// that share plus one; refused, naming those bytes and the bound, at exactly them; no parts always within.
#[test]
fn an_images_owed_parts_past_one_operations_share_of_the_bound_are_refused() {
  let repo = repository(6);
  let (older, newer) = (
    part(repo, StateLog::Names, (0, 1)),
    part(repo, StateLog::Names, (1, 4)),
  );
  let parts: Vec<(ObjectId, u64, Vec<u8>)> = [&older, &newer]
    .iter()
    .map(|record| (record.log_name().register(), record.to, value_of(record)))
    .collect();
  let (small, large) = (value_of(&older).len(), value_of(&newer).len());
  let past = small.min(large);
  assert_eq!(super::restorable(&parts, past + 1), Ok(()));
  assert_eq!(super::restorable(&parts, past), Err((past, past)));
  assert_eq!(super::restorable(&[], 0), Ok(()));
}

/// A-921's second mode (main 01a20182's ruling of 2026-11-26: a held-back part never stalls in silence). Do: hold a
/// shard's parts back pass after pass, each refused; then carry them. Expect: no refusal below the bound, the refusal at
/// it, and none again once a publication carries them.
#[test]
fn parts_held_back_past_the_bound_surface_the_publications_refusal() {
  let mut logs = StateLogs::default();
  let refusal = slates_ipc::protocol::Refusal::NoSpace;
  let bound = 3;
  for _ in 1..bound {
    logs.held_back(refusal.clone());
  }
  assert_eq!(logs.held_back_past(bound), None, "below the bound");
  logs.held_back(refusal.clone());
  assert_eq!(logs.held_back_past(bound), Some(&refusal), "at the bound");
  logs.carried();
  assert_eq!(logs.held_back_past(bound), None, "carried again");
}

/// Main 01a20182's ruling of 2026-11-27 (A-921 part 3's footprint at f = 0, found looking at the 57k hg push's hang):
/// a part held and let go before any publication carried it leaves nothing for a delta. Do: at f = 0 (no remote
/// candidate) hold and let go parts of a names log, as each take does; then hold a part, commit a publication, and let it
/// go; then restore a part from an image and let it go. Expect: nothing changed after the first (every one held and let go
/// between two publications); the published part's and the restored part's let-go each changed, so the next delta
/// removes their pieces. Before: every hold and let-go kept a key until a publication committed, so each delta carried a
/// removal per part made since the last.
#[test]
fn a_part_let_go_before_any_publication_carried_it_leaves_no_change() {
  let repo = repository(5);
  let mut logs = StateLogs::default();
  for position in 0..4u64 {
    let held = part(repo, StateLog::Names, (position, position + 1));
    logs.hold(&held);
    logs.let_go_held(held.log_name().register(), &[]);
  }
  assert_eq!(
    logs.changed().count(),
    0,
    "held and let go between publications: nothing changed"
  );
  let published = part(repo, StateLog::Names, (4, 5));
  let register = published.log_name().register();
  logs.hold(&published);
  logs.clear_changed();
  logs.let_go_held(register, &[]);
  assert_eq!(
    logs.changed().collect::<Vec<_>>(),
    vec![(register, 5)],
    "the published part's piece removed"
  );
  logs.clear_changed();
  let restored = part(repo, StateLog::Names, (5, 6));
  logs.restore(register, 6, value_of(&restored));
  logs.let_go_held(register, &[]);
  assert_eq!(
    logs.changed().collect::<Vec<_>>(),
    vec![(register, 6)],
    "the restored part's piece removed"
  );
}

/// A-919 (C): an owner's committed floor rises over the parts it lets go, in order, each continuing the chain from the
/// log's start, the digest one link a part over its register value, and is marked changed for the next delta. Do: hold
/// names parts 0..2 and 2..3; both candidates acknowledge through 2, then through 3. Expect the floor at 2 with
/// `H(0^32 || part one)`, then at 3 with the next link, each time marked changed; a publication clears the mark.
#[test]
fn an_owners_committed_floor_rises_over_the_parts_it_lets_go_with_the_chains_digest() {
  use slates_db::register::{CHAIN_BASE, CommittedFloor, chain_digest};
  let repo = repository(11);
  let (one, two) = (
    part(repo, StateLog::Names, (0, 2)),
    part(repo, StateLog::Names, (2, 3)),
  );
  let register = one.log_name().register();
  let (b, c) = (HostId(2), HostId(3));
  let mut logs = StateLogs::default();
  logs.hold(&one);
  logs.hold(&two);
  assert_eq!(logs.floor(register), None, "nothing let go yet");
  logs.acknowledged(register, 2, &[b, c]);
  logs.let_go_held(register, &[b, c]);
  let first = chain_digest(&CHAIN_BASE, &value_of(&one));
  assert_eq!(
    logs.floor(register),
    Some(CommittedFloor {
      position: 2,
      digest: first
    })
  );
  assert_eq!(
    logs.floors_changed().count(),
    1,
    "the floor rides the next delta"
  );
  logs.clear_changed();
  logs.acknowledged(register, 3, &[b, c]);
  logs.let_go_held(register, &[b, c]);
  assert_eq!(
    logs.floor(register),
    Some(CommittedFloor {
      position: 3,
      digest: chain_digest(&first, &value_of(&two))
    })
  );
}

/// A-919 (C), the base: an owner that never let go a chain from the log's start (a takeover's successor, whose first
/// owed part continues from a later position) keeps no floor, and a part that does not continue the floor stops it.
/// Do: let go a lone part from 5 to 6, then, on another log, a part from 0 to 2 followed by a stray from 4 to 5.
/// Expect no floor on the first log, and the second's at 2.
#[test]
fn a_floor_rises_only_over_parts_that_continue_its_chain_from_the_logs_start() {
  let repo = repository(12);
  let late = part(repo, StateLog::Names, (5, 6));
  let (start, stray) = (
    part(repo, StateLog::Phases, (0, 2)),
    part(repo, StateLog::Phases, (4, 5)),
  );
  let mut logs = StateLogs::default();
  logs.hold(&late);
  logs.hold(&start);
  logs.hold(&stray);
  logs.let_go_held(late.log_name().register(), &[]);
  logs.let_go_held(start.log_name().register(), &[]);
  assert_eq!(logs.floor(late.log_name().register()), None);
  assert_eq!(
    logs
      .floor(start.log_name().register())
      .map(|floor| floor.position),
    Some(2)
  );
}

/// A-919 (C), the guard (main 01a20182's ruling of 2026-11-27 12:52): a part at or below its log's committed floor is
/// refused, as a position let go made again would be; one above it is held. Do: hold and let go a part from 0 to 2 (the
/// floor at 2), then offer a part to 2 again and one from 2 to 3. Expect the first refused, the second held.
#[test]
fn a_part_at_or_below_its_logs_floor_is_refused() {
  let repo = repository(13);
  let first = part(repo, StateLog::Names, (0, 2));
  let register = first.log_name().register();
  let mut logs = StateLogs::default();
  assert!(logs.hold(&first));
  logs.let_go_held(register, &[]);
  assert_eq!(logs.floor(register).map(|floor| floor.position), Some(2));
  assert!(
    !logs.hold(&part(repo, StateLog::Names, (1, 2))),
    "a position let go"
  );
  assert!(
    logs.hold(&part(repo, StateLog::Names, (2, 3))),
    "the next position"
  );
}

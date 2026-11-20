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
    identity: [u8::try_from(to).unwrap_or(u8::MAX); 32],
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

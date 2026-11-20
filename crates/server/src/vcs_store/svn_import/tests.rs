//! A-652 part 3's tests (§4.17.4 piece 6 and item 3.5): a stock `svnadmin dump` imported into a repository
//! `CreateRepository` made, each record landed by the owner's step and the repository's records then exporting the dump
//! byte for byte; a dump in parts loading part after part; a stream out of its sequence or cut short refused by kind,
//! keeping the revisions before it; the records made again from the log; the verb answered when its import ends; and
//! every way an import fails mapped to its refusal on the wire.

use std::time::{Duration, Instant};

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::{ImportFault, ObjectFormat, Refusal, ReplyBody, RequestBody, VcsTool};
use slates_svn::commit::record::Record;
use slates_svn::commit::revision::StepRefusal;
use slates_svn::dump::{DumpError, Format};
use slates_svn::migration::{Exporter, Exporting, MigrationRefusal, Repository};
use slates_vfs::host::sim::SimHost;
use slates_vfs::host::{FetchId, HostDir, HostError, HostFs};
use slates_wire::request::RequestId;

use super::{LoadFailed, Loaded, Turned, begin, turn};
use crate::merge_service::ReplyRoute;
use crate::repository::import::{ANSWERED, import_with, svn_refusal_of, svn_reply_of};
use crate::repository::svn_commit::Uncommitted;
use crate::state::ShardState;
use crate::verbs::{dispatch, to_db_volume};

/// Format 2, revisions 0 to 3 (`svnadmin dump -r 0:3`), A-593's golden vector.
pub(crate) const PART1: &[u8] = include_bytes!("../../../../svn/fixtures/dump/part1.dump");
/// Format 3, revisions 4 on, as changes (`svnadmin dump --deltas --incremental -r 4:13`).
pub(crate) const PART2: &[u8] = include_bytes!("../../../../svn/fixtures/dump/part2.dump");
/// Format 2, every revision whole (`svnadmin dump`): what the two parts make together.
pub(crate) const FULL: &[u8] = include_bytes!("../../../../svn/fixtures/dump/full.dump");

/// Shape: the dump's file name on the simulated host.
const DUMP: &str = "repository.dump";

/// Shape: the path the verb's tests name, whose directory only the simulated host opens.
const PATH: &str = "/dumps/repository.dump";

/// Shape: the last revision of `part1.dump` and of `full.dump` (`make-fixtures.sh` dumps `-r 0:3`, then 13 revisions).
const PART1_LAST: u64 = 3;

/// Shape: the youngest revision of `full.dump`.
pub(crate) const FULL_LAST: u64 = 13;

/// Shape: how many turns a test lets an import take before it gives up: far past a debug build's turns over the
/// vectors (one revision or a slice a unit), so only an import that never ends reaches it.
const TURN_LIMIT: u64 = 1 << 20;

/// Shape: the room the mapping test's reader may hold, any number.
const ROOM: u64 = 1 << 20;

/// Shape: the origin the verb's test request comes from, any number.
const ORIGIN: u64 = 7;

/// Shape: a client slot no client holds, so a reply sent to it is counted undeliverable rather than written.
const NO_CLIENT: u32 = 4096;

/// Shape: how long the verb's test waits for the shard's task to answer: far past a debug build's import of the vector.
const TASK_WAIT: Duration = Duration::from_secs(60);

/// The repository's owner.
pub(crate) fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Creates Subversion repository `name` on the shard: its id and its one green.
pub(crate) fn subversion(state: &mut ShardState, name: &str) -> (RepositoryId, DbVolumeId) {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool: VcsTool::Subversion,
      format: ObjectFormat::Revisions,
      default_branch: "main".to_owned(),
      green: None,
    },
  );
  let ReplyBody::GreenCreated { id } = reply else {
    panic!("no repository: {reply:?}")
  };
  let repositories = state.db.partition().repositories();
  let record = repositories.repository_named(name).expect("the record");
  (record.id, to_db_volume(id))
}

/// A simulated host holding `dump` as [`DUMP`] in its root: the host and its root.
fn host_holding(dump: &[u8]) -> (SimHost, HostDir) {
  let mut host = SimHost::new();
  host.replace_file(&format!("/{DUMP}"), dump);
  let root = host.root();
  (host, root)
}

/// Imports `dump` into repository `id` on the shard, a turn at a time, to its end: what the turns ended with, and how
/// many there were.
pub(crate) fn imported(
  state: &mut ShardState,
  id: RepositoryId,
  dump: &[u8],
) -> (Result<Loaded, LoadFailed>, u64) {
  let (host, root) = host_holding(dump);
  if let Err(failed) = begin(state, (&owner(), id), (Box::new(host), root, DUMP)) {
    return (Err(failed), 0);
  }
  for turns in 1..=TURN_LIMIT {
    match turn(state, id) {
      Some(Ok(Turned::More)) => {}
      Some(Ok(Turned::Done(loaded))) => return (Ok(loaded), turns),
      Some(Err(failed)) => return (Err(failed), turns),
      None => return (Err(LoadFailed::NoStore), turns),
    }
  }
  (Err(LoadFailed::Busy), TURN_LIMIT)
}

/// Revisions `first` to `last` of `records`, exported in format 2 (A-628's export).
fn exported(records: &Repository, first: u64, last: u64) -> Vec<u8> {
  let mut exporter = Exporter::new(records, first, last, Format::Two).expect("a range held");
  let mut out = Vec::new();
  while exporter
    .export(records, records, &mut out)
    .expect("exported")
    != Exporting::Done
  {}
  out
}

/// What the shard holds of repository `id`'s history: its records' youngest revision, and the whole of it exported.
fn history(state: &ShardState, id: RepositoryId) -> Option<(u64, Vec<u8>)> {
  let records = state.vcs_stores.subversion_records(id)?;
  Some((records.youngest(), exported(records, 0, records.youngest())))
}

/// The kinds of the records repository `id`'s register holds, in its order.
fn logged_kinds(state: &ShardState, id: RepositoryId) -> Vec<&'static str> {
  let kind = |bytes: &[u8]| match Record::decode(bytes) {
    Ok(Record::Created { .. }) => "created",
    Ok(Record::Uuid(_)) => "uuid",
    Ok(Record::RevisionProperties { .. }) => "properties",
    Ok(Record::Revision(_)) => "revision",
    Ok(Record::Locks(_)) => "locks",
    Err(_) => "refused",
  };
  let repositories = state.db.partition().repositories();
  repositories.objects(id).map(kind).collect()
}

/// Counter `name` on the shard.
fn count(state: &ShardState, name: &str) -> u64 {
  state.refusals.get(name).copied().unwrap_or(0)
}

/// A-652 part 3 (export(import(D)) = D, §4.17.4 piece 6's gate, through the owner's step). Do: import `part1.dump`
/// (revisions 0 to 3, its UUID and revision 0's date) into a repository `CreateRepository` made. Expect: three revisions
/// committed, the youngest 3; the repository's records exporting `part1.dump` byte for byte; the green at version 3; the
/// register holding the creation, the UUID, revision 0's properties and the three revisions, in that order; counted
/// three revisions and one import done.
#[test]
fn t_a652_a_dump_imports_through_the_owners_step_and_exports_again_byte_for_byte() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, green) = subversion(state, "svn");
    let (loaded, _) = imported(state, svn, PART1);
    let revisions = loaded.map(|loaded| (loaded.revisions, loaded.youngest));
    assert_eq!(revisions, Ok((PART1_LAST, PART1_LAST)));
    assert!(
      history(state, svn) == Some((PART1_LAST, PART1.to_vec())),
      "part1.dump again"
    );
    let head = state.greens.get(&green).map(|engine| engine.head());
    assert_eq!(head, Some(PART1_LAST));
    let wanted = [
      "created",
      "uuid",
      "properties",
      "revision",
      "revision",
      "revision",
    ];
    assert_eq!(logged_kinds(state, svn), wanted);
    let counted = [count(state, super::REVISIONS), count(state, super::DONE)];
    assert_eq!(counted, [PART1_LAST, 1]);
  });
}

/// A-652 part 3 (a dump made in parts loads part after part; a stream out of its sequence refused). Do: import
/// `part1.dump`, then `part2.dump` (format 3, revisions 4 to 13 as changes), then `part1.dump` again. Expect: the two
/// parts making the history `full.dump` holds, exported as `full.dump` byte for byte; the third import refused at
/// revision 0 as `svnadmin load` refuses a revision out of its sequence (`E160006`, on the wire), the history as it was.
#[test]
fn t_a652_a_dump_in_parts_loads_part_after_part_and_one_out_of_sequence_is_refused() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, _) = subversion(state, "svn");
    let parts =
      [PART1, PART2].map(|part| imported(state, svn, part).0.map(|loaded| loaded.youngest));
    assert_eq!(parts, [Ok(PART1_LAST), Ok(FULL_LAST)]);
    assert!(
      history(state, svn) == Some((FULL_LAST, FULL.to_vec())),
      "full.dump"
    );
    let (again, _) = imported(state, svn, PART1);
    let out_of_sequence = LoadFailed::Refused {
      refusal: MigrationRefusal::OutOfSequence {
        expected: FULL_LAST + 1,
        found: 0,
      },
      revision: Some(0),
    };
    assert_eq!(again, Err(out_of_sequence.clone()));
    let wire = svn_refusal_of(PATH, ROOM, out_of_sequence);
    let wanted = ImportFault::Subversion {
      code: 160_006,
      revision: Some(0),
      path: String::new(),
    };
    assert_eq!(wire, Refusal::ImportRefused { fault: wanted });
    assert!(
      history(state, svn) == Some((FULL_LAST, FULL.to_vec())),
      "as it was"
    );
  });
}

/// A-652 part 3 (a refusal keeps the revisions before it, as `svnadmin load` keeps them). Do: import `part1.dump` cut
/// three bytes short of its end. Expect: refused as the dump codec refuses a stream that ends inside a record
/// (`Truncated`, `E200003`) at revision 3; revisions 1 and 2 committed and kept, revision 3 not; the import's holds given
/// back, its counter of refusals one.
#[test]
fn t_a652_a_dump_cut_short_is_refused_and_keeps_the_revisions_before_it() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, _) = subversion(state, "svn");
    let cut = PART1.get(..PART1.len() - 3).expect("a shorter dump");
    let (loaded, _) = imported(state, svn, cut);
    let truncated = LoadFailed::Refused {
      refusal: MigrationRefusal::Stream(DumpError::Truncated),
      revision: Some(PART1_LAST),
    };
    assert_eq!(loaded, Err(truncated));
    let youngest = state
      .vcs_stores
      .subversion_records(svn)
      .map(Repository::youngest);
    assert_eq!(youngest, Some(PART1_LAST - 1));
    assert!(!state.vcs_stores.importing(svn));
    assert_eq!(count(state, super::REFUSED), 1);
  });
}

/// A-652 part 3 (the imported records are durable). Do: import `part1.dump`, then make the store again from the
/// partition's log. Expect: the records made again hold the same history, exported as `part1.dump` byte for byte, and
/// none refused; every record of the register replayed.
#[test]
fn t_a652_the_records_an_import_committed_are_made_again_from_the_log() {
  crate::daemon::audit_on_shard(|state| {
    let (svn, _) = subversion(state, "svn");
    assert!(imported(state, svn, PART1).0.is_ok());
    let replayed = count(state, crate::vcs_store::REPLAYED);
    crate::vcs_store::rebuild(state, svn);
    crate::vcs_store::settle_rebuild(state, svn);
    assert_eq!(state.vcs_stores.unrecovered(svn), None);
    assert!(
      history(state, svn) == Some((PART1_LAST, PART1.to_vec())),
      "made again"
    );
    let logged = u64::try_from(logged_kinds(state, svn).len()).unwrap_or(u64::MAX);
    assert_eq!(count(state, crate::vcs_store::REPLAYED), replayed + logged);
  });
}

/// The request `request` as the verb runner leaves it before a verb, its reply routed to a slot no client holds.
fn routed(state: &mut ShardState, request: u64) {
  state.current_request = Some((ORIGIN, RequestId::from_word(request)));
  state.reply_route = Some(ReplyRoute {
    shard: state.shard,
    client_index: NO_CLIENT,
    request,
    through_work: false,
  });
}

/// A-652 part 3 (the verb, answered when its import ends). Do: ask a running shard to import `part1.dump` from a host
/// file into a Subversion repository, then wait for the shard's task. Expect: the request held (acknowledged and
/// deferred); then within the wait the import ended and its request answered once, its reply sent (counted
/// undeliverable, no client being there), and the repository's history `part1.dump`'s.
#[test]
fn t_a652_the_verb_imports_a_dump_and_answers_when_it_ends() {
  let daemon = crate::daemon::audit_daemon();
  let id = crate::daemon::observe_first(&daemon, |state| {
    let (id, _) = subversion(state, "svn");
    routed(state, 1);
    let opened = |_: &str| -> Result<(Box<dyn HostFs>, HostDir), HostError> {
      let (host, root) = host_holding(PART1);
      Ok((Box::new(host), root))
    };
    let reply = import_with(state, &owner(), ("svn", PATH), opened);
    assert_eq!(
      (reply, state.acceptance_deferred),
      (ReplyBody::Acknowledged, true)
    );
    id
  });
  let began = Instant::now();
  let seen = loop {
    let seen = crate::daemon::observe_first(&daemon, move |state| {
      let held = state.vcs_stores.answer(id).is_some() || state.vcs_stores.importing(id);
      let counted = [
        count(state, ANSWERED),
        count(state, crate::state::REPLY_UNDELIVERABLE),
      ];
      (held, counted, history(state, id))
    });
    if !seen.0 || began.elapsed() > TASK_WAIT {
      break seen;
    }
  };
  daemon.stop();
  let (held, counted, made) = seen;
  assert!(!held, "still held after {TASK_WAIT:?}");
  assert_eq!(counted, [1, 1]);
  assert!(
    made == Some((PART1_LAST, PART1.to_vec())),
    "part1.dump's history"
  );
}

/// A-652 part 3 (each failure's refusal on the wire). Do: map every way a dump's import fails, and a finished one.
/// Expect: `ContentUnavailable` for records not held or a green that does not follow them, `UnderWay`, `Pending` for a
/// fetch still pending, the host's refusal naming the dump's path, the budget's room, `PastRoom` past the reader's
/// admission, Subversion's own number with the revision and the path a refusal names, a step's refusal as the import's,
/// `Format` for a revision this build cannot declare, an append's own refusal, and what a finished import did.
#[test]
fn t_a652_each_dump_import_failure_maps_to_its_refusal() {
  let fault = |fault| Refusal::ImportRefused { fault };
  let numbered = |code, revision, path: &str| {
    fault(ImportFault::Subversion {
      code,
      revision,
      path: path.to_owned(),
    })
  };
  let cases: [(LoadFailed, Refusal); 10] = [
    (LoadFailed::NoStore, Refusal::ContentUnavailable),
    (LoadFailed::Busy, fault(ImportFault::UnderWay)),
    (
      LoadFailed::Host(HostError::Pending(FetchId(3))),
      fault(ImportFault::Pending),
    ),
    (
      LoadFailed::Budget { available: 7 },
      Refusal::BudgetExceeded { available: 7 },
    ),
    (
      LoadFailed::Refused {
        refusal: MigrationRefusal::Stream(DumpError::PastAdmission {
          needed: 9,
          admitted: 1,
        }),
        revision: None,
      },
      fault(ImportFault::PastRoom { room: ROOM }),
    ),
    (
      LoadFailed::Refused {
        refusal: MigrationRefusal::NotFound {
          revision: Some(2),
          path: "/trunk/a".to_owned(),
        },
        revision: Some(4),
      },
      numbered(160_013, Some(4), "/trunk/a"),
    ),
    (
      LoadFailed::Uncommitted {
        refusal: Uncommitted::Step(StepRefusal::Corrupt),
        revision: 5,
      },
      numbered(160_004, Some(5), ""),
    ),
    (
      LoadFailed::Uncommitted {
        refusal: Uncommitted::Unsupported {
          path: "/trunk".to_owned(),
        },
        revision: 6,
      },
      fault(ImportFault::Format),
    ),
    (
      LoadFailed::Uncommitted {
        refusal: Uncommitted::Append(crate::repository::push::AppendRefused::Entry(
          Refusal::NotFound,
        )),
        revision: 7,
      },
      Refusal::NotFound,
    ),
    (
      LoadFailed::Uncommitted {
        refusal: Uncommitted::Diverged,
        revision: 8,
      },
      Refusal::ContentUnavailable,
    ),
  ];
  for (failed, expected) in cases {
    assert_eq!(
      svn_refusal_of(PATH, ROOM, failed.clone()),
      expected,
      "{failed:?}"
    );
  }
  check_host_refusal_and_answers();
}

/// The host's refusal names the dump's path; a finished import answers what it did; one given back, `Abandoned`.
fn check_host_refusal_and_answers() {
  let unfound = svn_refusal_of(PATH, ROOM, LoadFailed::Host(HostError::NotFound));
  let named = matches!(
    &unfound,
    Refusal::BaseUnavailable { path, .. } | Refusal::BaseUnavailableWin32 { path, .. } if path == PATH
  );
  assert!(named, "{unfound:?}");
  let finished = Loaded {
    revisions: 3,
    youngest: 3,
    turns: 2,
  };
  let answers = [Some(Ok(finished)), None].map(|outcome| svn_reply_of(PATH, ROOM, outcome));
  let abandoned = ReplyBody::Refused {
    refusal: Refusal::ImportRefused {
      fault: ImportFault::Abandoned,
    },
  };
  let imported = ReplyBody::DumpImported {
    revisions: 3,
    youngest: 3,
  };
  assert_eq!(answers, [imported, abandoned]);
}

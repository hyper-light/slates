//! A-652 part 3's tests (§4.17.4 piece 6 and item 3.5): a stock `svnadmin dump` imported into a repository
//! `CreateRepository` made, each record landed by the owner's step and the repository's records then exporting the dump
//! byte for byte; a dump in parts loading part after part; a stream out of its sequence or cut short refused by kind,
//! keeping the revisions before it; the records made again from the log; the verb answered when its import ends; and
//! every way an import fails mapped to its refusal on the wire.

use std::time::{Duration, Instant};

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::{ImportFault, ObjectFormat, Refusal, ReplyBody, VcsTool};
use slates_svn::commit::record::Record;
use slates_svn::commit::revision::StepRefusal;
use slates_svn::dump::{DumpError, Format};
use slates_svn::migration::{Exporter, Exporting, MigrationRefusal, Repository};
use slates_vfs::host::sim::SimHost;
use slates_vfs::host::{HostDir, HostError, HostFs};
use slates_wire::request::RequestId;

use super::{LoadFailed, Loaded, Turned, begin, turn};
use crate::merge_service::ReplyRoute;
use crate::repository::import::{ANSWERED, import_with, svn_refusal_of, svn_reply_of};
use crate::repository::svn_commit::Uncommitted;
use crate::repository::svn_commit::tests::{on, rebuild, stored};
use crate::state::ShardState;

/// Format 2, revisions 0 to 3 (`svnadmin dump -r 0:3`), A-593's golden vector.
pub(crate) const PART1: &[u8] = include_bytes!("../../../../svn/fixtures/dump/part1.dump");
/// Format 3, revisions 4 on, as changes (`svnadmin dump --deltas --incremental -r 4:13`).
pub(crate) const PART2: &[u8] = include_bytes!("../../../../svn/fixtures/dump/part2.dump");
/// Format 2, every revision whole (`svnadmin dump`): what the two parts make together.
pub(crate) const FULL: &[u8] = include_bytes!("../../../../svn/fixtures/dump/full.dump");

/// Shape: the dump's file name on the simulated host.
pub(crate) const DUMP: &str = "repository.dump";

/// Shape: the path the verb's tests name, whose directory only the simulated host opens.
const PATH: &str = "/dumps/repository.dump";

/// Shape: the last revision of `part1.dump` and of `full.dump` (`make-fixtures.sh` dumps `-r 0:3`, then 13 revisions).
const PART1_LAST: u64 = 3;
/// Shape: the first revision of `part2.dump` (`make-fixtures.sh` dumps `--incremental -r 4:13` into it).
const PART2_FIRST: u64 = 4;

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

/// Creates Subversion repository `name` from a history running as a task on the shard, through a client's request,
/// once its creation landed on the node's store and was recorded (A-767 item 7): its id and its one green.
pub(crate) async fn subversion(name: &str) -> (RepositoryId, DbVolumeId) {
  crate::test_store::created(
    &owner(),
    (name, VcsTool::Subversion, ObjectFormat::Revisions, "main"),
  )
  .await
}

/// A simulated host holding `dump` as [`DUMP`] in its root: the host and its root.
pub(crate) fn host_holding(dump: &[u8]) -> (SimHost, HostDir) {
  let mut host = SimHost::new();
  host.replace_file(&format!("/{DUMP}"), dump);
  let root = host.root();
  (host, root)
}

/// Imports `dump` into repository `id` from a history running as a task on the shard, a turn at a time, each record
/// handed over landed on the node's store before the next turn (C2, `super::land_owed`), to its end: what the turns
/// ended with, and how many there were.
pub(crate) async fn imported(
  id: RepositoryId,
  dump: impl AsRef<[u8]>,
) -> (Result<Loaded, LoadFailed>, u64) {
  use crate::repository::svn_commit::tests::on;
  let (host, root) = host_holding(dump.as_ref());
  // The simulated host holds the dump now: the caller's copy goes, so a measurement counts one.
  drop(dump);
  if let Err(failed) = on(|state| begin(state, (&owner(), id), (Box::new(host), root, DUMP))) {
    return (Err(failed), 0);
  }
  for turns in 1..=TURN_LIMIT {
    // A yield between turns, as the server's import runs (`super::run`): the shard serves its inbox meanwhile.
    slates_rt::futures::yield_now().await;
    if let Some(Err(failed)) = super::land_owed(id).await {
      return (Err(failed), turns);
    }
    match on(|state| turn(state, id)) {
      Some(Ok(Turned::More)) => {}
      Some(Ok(Turned::Done(loaded))) => return (Ok(loaded), turns),
      Some(Err(failed)) => return (Err(failed), turns),
      None => return (Err(LoadFailed::NoStore), turns),
    }
  }
  (Err(LoadFailed::Busy), TURN_LIMIT)
}

/// What the shard holds of repository `id`'s history: its records' youngest revision, and the whole of it exported in
/// format 2 (A-628's export) through the repository's view, each stored text's piece it waits for fetched through the
/// node's store and let go after the next call, as the export's task fetches them (C2 stage B part 2 (b)).
async fn history(id: RepositoryId) -> Option<(u64, Vec<u8>)> {
  let youngest = on(|state| {
    state
      .vcs_stores
      .subversion_records(id)
      .map(Repository::youngest)
  })?;
  let mut exporter = on(|state| {
    let records = state.vcs_stores.subversion_records(id)?;
    Exporter::new(records, 0, youngest, Format::Two).ok()
  })?;
  let mut out = Vec::new();
  let mut fetched = None;
  loop {
    let step = on(|state| {
      crate::vcs_store::svn_view::with_view(state, (id, None), |view| {
        exporter.export(view.records, view.texts, &mut out)
      })
    });
    if let Some(read) = fetched.take() {
      on(|state| crate::vcs_store::svn_texts::let_go(state, read));
    }
    match step?.expect("exported") {
      Exporting::Done => return Some((youngest, out)),
      Exporting::Wrote(_) => {}
      Exporting::Waiting(wanted) => {
        assert!(crate::vcs_store::svn_texts::fetch_piece(id, wanted).await);
        fetched = Some(wanted);
      }
    }
  }
}

/// The kinds of the records repository `id`'s register names, in its order, each read back from the node's store (C2:
/// the register holds a reference a changeset).
async fn logged_kinds(id: RepositoryId) -> Vec<&'static str> {
  use crate::repository::svn_commit::tests::{on, stored_record};
  let named = on(|state| state.db.partition().repositories().object_count(id));
  let mut kinds = Vec::new();
  for sequence in 0..named {
    kinds.push(
      match stored_record(id, u64::try_from(sequence).unwrap()).await {
        Record::Created { .. } => "created",
        Record::Uuid(_) => "uuid",
        Record::RevisionProperties { .. } => "properties",
        Record::Revision(_) => "revision",
        Record::Locks(_) => "locks",
      },
    );
  }
  kinds
}

/// Counter `name` on the shard.
fn count(state: &ShardState, name: &str) -> u64 {
  state.refusals.get(name).copied().unwrap_or(0)
}

/// A-652 part 3 (export(import(D)) = D, §4.17.4 piece 6's gate, through the owner's step). Do: import `part1.dump`
/// (revisions 0 to 3, its UUID and revision 0's date) into a repository `CreateRepository` made. Expect: three revisions
/// committed, the youngest 3; every text the import brought named stored by the records, none of its bytes kept in RAM
/// (C2 stage B part 2 (b): each revision's changeset carries its texts); the repository exporting `part1.dump` byte for
/// byte through the store; the green at version 3; the register holding the creation, the UUID, revision 0's
/// properties and one changeset of the three revisions (C2 stage C: an import's batch lands as one changeset, a
/// `Record` section a revision, landed at the import's end), in that order; counted three revisions, one batch landed
/// at the end and one import done.
#[test]
fn t_a652_a_dump_imports_through_the_owners_step_and_exports_again_byte_for_byte() {
  stored(|| async move {
    let (svn, green) = subversion("svn").await;
    let (loaded, _) = imported(svn, PART1).await;
    let revisions = loaded.map(|loaded| (loaded.revisions, loaded.youngest));
    assert_eq!(revisions, Ok((PART1_LAST, PART1_LAST)));
    let (stored, kept_in_ram) = on(|state| {
      let records = state
        .vcs_stores
        .subversion_records(svn)
        .expect("the records");
      let stored: Vec<_> = records
        .stored_texts()
        .map(|(identity, _)| *identity)
        .collect();
      let kept = stored
        .iter()
        .filter(|identity| records.text(identity).is_some())
        .count();
      (stored.len(), kept)
    });
    assert!(
      stored > 0 && kept_in_ram == 0,
      "{stored} stored, {kept_in_ram} kept in RAM"
    );
    assert!(
      history(svn).await == Some((PART1_LAST, PART1.to_vec())),
      "part1.dump again"
    );
    let head = on(|state| state.greens.get(&green).map(|engine| engine.head()));
    assert_eq!(head, Some(PART1_LAST));
    let wanted = ["created", "uuid", "properties", "revision"];
    assert_eq!(logged_kinds(svn).await, wanted);
    let disk = on(|state| state.vcs_disk.clone()).expect("the node's store");
    let batch = crate::repository::svn_commit::landing::changeset_records(&disk, svn, 3)
      .await
      .map(|records| records.len());
    assert_eq!(
      batch,
      Ok(3),
      "the batch's changeset holds a record a revision"
    );
    let counted = on(|state| {
      [
        count(state, super::REVISIONS),
        count(state, super::DONE),
        count(state, crate::repository::svn_commit::staging::FLUSHED_END),
      ]
    });
    assert_eq!(counted, [PART1_LAST, 1, 1]);
  });
}

/// A-652 part 3 with C2 stage C's resume (a dump made in parts loads part after part; a part the repository holds
/// already reads through; a stream out of its sequence refused). Do: import `part1.dump`, then `part2.dump` (format 3,
/// revisions 4 to 13 as changes), then `part1.dump` again; and `part2.dump` into a repository holding nothing. Expect:
/// the two parts making the history `full.dump` holds, exported as `full.dump` byte for byte; the third import ended,
/// committing nothing (every revision held already, its properties the repository's), the history as it was; the
/// fourth refused at revision 4 as `svnadmin load` refuses a revision out of its sequence (`E160006` on the wire,
/// naming revision 0 the last landed).
#[test]
fn t_a652_a_dump_in_parts_loads_part_after_part_and_one_out_of_sequence_is_refused() {
  stored(|| async move {
    let (svn, _) = subversion("svn").await;
    let mut parts = Vec::new();
    for part in [PART1, PART2] {
      parts.push(imported(svn, part).await.0.map(|loaded| loaded.youngest));
    }
    assert_eq!(parts, [Ok(PART1_LAST), Ok(FULL_LAST)]);
    assert!(
      history(svn).await == Some((FULL_LAST, FULL.to_vec())),
      "full.dump"
    );
    let (again, _) = imported(svn, PART1).await;
    let again = again.map(|loaded| (loaded.revisions, loaded.youngest));
    assert_eq!(
      again,
      Ok((0, FULL_LAST)),
      "a part held already reads through"
    );
    let (other, _) = subversion("other").await;
    let (ahead, _) = imported(other, PART2).await;
    let out_of_sequence = LoadFailed::Refused {
      refusal: MigrationRefusal::OutOfSequence {
        expected: 1,
        found: PART2_FIRST,
      },
      revision: Some(PART2_FIRST),
      landed: 0,
    };
    assert_eq!(ahead, Err(out_of_sequence.clone()));
    let wire = svn_refusal_of(PATH, ROOM, out_of_sequence);
    let wanted = ImportFault::SubversionLanded {
      code: 160_006,
      revision: Some(PART2_FIRST),
      path: String::new(),
      landed: 0,
    };
    assert_eq!(wire, Refusal::ImportRefused { fault: wanted });
    assert!(
      history(svn).await == Some((FULL_LAST, FULL.to_vec())),
      "as it was"
    );
  });
}

/// A-652 part 3 under C2 stage C (a refusal gives up the revisions its batch staged; those earlier batches landed stay).
/// Do: import `part1.dump` cut three bytes short of its end. Expect: refused as the dump codec refuses a stream that
/// ends inside a record (`Truncated`, `E200003`) at revision 3; revisions 1 and 2, staged in the batch the refusal ends,
/// given up with it, the records made again at revision 0 (`svnadmin load` keeps each revision as it goes: GAPS); the
/// import's holds given back, its counter of refusals one.
#[test]
fn t_a652_a_dump_cut_short_is_refused_and_gives_up_its_batch() {
  stored(|| async move {
    let (svn, _) = subversion("svn").await;
    let cut = PART1.get(..PART1.len() - 3).expect("a shorter dump");
    let (loaded, _) = imported(svn, cut).await;
    let truncated = LoadFailed::Refused {
      refusal: MigrationRefusal::Stream(DumpError::Truncated),
      revision: Some(PART1_LAST),
      landed: 0,
    };
    assert_eq!(loaded, Err(truncated));
    // The batch given up makes the records again from the register, in turns.
    crate::vcs_store::rebuilt(svn).await;
    let seen = on(|state| {
      (
        state
          .vcs_stores
          .subversion_records(svn)
          .map(Repository::youngest),
        state.vcs_stores.importing(svn),
        count(state, super::REFUSED),
      )
    });
    assert_eq!(seen, (Some(0), false, 1));
  });
}

/// A-652 part 3 (the imported records are durable). Do: import `part1.dump`, then make the store again from its
/// changesets on the node's store (C2). Expect: the records made again hold the same history, exported as `part1.dump` byte for byte, and
/// none refused; every record of the register replayed.
#[test]
fn t_a652_the_records_an_import_committed_are_made_again_from_the_log() {
  stored(|| async move {
    let (svn, _) = subversion("svn").await;
    assert!(imported(svn, PART1).await.0.is_ok());
    let replayed = on(|state| count(state, crate::vcs_store::REPLAYED));
    rebuild(svn).await;
    assert_eq!(on(|state| state.vcs_stores.unrecovered(svn).cloned()), None);
    assert!(
      history(svn).await == Some((PART1_LAST, PART1.to_vec())),
      "made again"
    );
    let logged = u64::try_from(logged_kinds(svn).await.len()).unwrap_or(u64::MAX);
    assert_eq!(
      on(|state| count(state, crate::vcs_store::REPLAYED)),
      replayed + logged
    );
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
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let asked = ("svn", VcsTool::Subversion, ObjectFormat::Revisions, "main");
  let (id, _) = crate::test_store::create_on(&daemon, 0, &owner(), asked);
  // The creation's own answer went to the route's empty slot too.
  let undeliverable = crate::daemon::observe_first(&daemon, |state| {
    count(state, crate::state::REPLY_UNDELIVERABLE)
  });
  crate::daemon::observe_first(&daemon, |state| {
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
  });
  let began = Instant::now();
  let seen = loop {
    let seen = crate::daemon::observe_first(&daemon, move |state| {
      let held = state.vcs_stores.answer(id).is_some() || state.vcs_stores.importing(id);
      let counted = [
        count(state, ANSWERED),
        count(state, crate::state::REPLY_UNDELIVERABLE),
      ];
      (held, counted)
    });
    if !seen.0 || began.elapsed() > TASK_WAIT {
      break seen;
    }
  };
  let made = crate::test_store::run_on_first(&daemon, move || history(id), TASK_WAIT);
  daemon.stop();
  let (held, counted) = seen;
  assert!(!held, "still held after {TASK_WAIT:?}");
  assert_eq!(counted, [1, undeliverable + 1]);
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
    fault(ImportFault::SubversionLanded {
      code,
      revision,
      path: path.to_owned(),
      landed: 3,
    })
  };
  let cases: [(LoadFailed, Refusal); 9] = [
    (LoadFailed::NoStore, Refusal::ContentUnavailable),
    (LoadFailed::Busy, fault(ImportFault::UnderWay)),
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
        landed: 3,
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
        landed: 3,
      },
      numbered(160_013, Some(4), "/trunk/a"),
    ),
    (
      LoadFailed::Uncommitted {
        refusal: Uncommitted::Step(StepRefusal::Corrupt),
        revision: 5,
        landed: 3,
      },
      numbered(160_004, Some(5), ""),
    ),
    (
      LoadFailed::Uncommitted {
        refusal: Uncommitted::Unsupported {
          path: "/trunk".to_owned(),
        },
        revision: 6,
        landed: 3,
      },
      fault(ImportFault::Format),
    ),
    (
      LoadFailed::Uncommitted {
        refusal: Uncommitted::Append(crate::repository::push::AppendRefused::Entry(
          Refusal::NotFound,
        )),
        revision: 7,
        landed: 3,
      },
      Refusal::NotFound,
    ),
    (
      LoadFailed::Uncommitted {
        refusal: Uncommitted::Diverged,
        revision: 8,
        landed: 3,
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

/// C2 stage C, main's conditions 1 and 2 of 2026-11-21 (a refused import names what landed; the same dump resumes).
/// Do: import `part1.dump` whole (revisions 1 to 3 land), then `part2.dump` cut short (refused inside its batch), then
/// `part2.dump` whole. Expect: the cut one refused `Truncated` naming revision 3 the last landed, the repository at 3
/// (the cut batch's revisions given up); the whole one resuming after 3, committing revisions 4 to 13, and the history
/// then `full.dump` byte for byte, as an import never cut short leaves it.
#[test]
fn t_c2c_a_refused_import_names_what_landed_and_the_same_dump_resumes_after_it() {
  stored(|| async move {
    let (svn, _) = subversion("resumed").await;
    let first = imported(svn, PART1).await.0.map(|loaded| loaded.youngest);
    assert_eq!(first, Ok(PART1_LAST));
    let cut = PART2.get(..PART2.len() / 2).expect("a shorter dump");
    let (refused, _) = imported(svn, cut).await;
    let landed = match refused {
      Err(LoadFailed::Refused {
        refusal: MigrationRefusal::Stream(DumpError::Truncated),
        landed,
        ..
      }) => landed,
      other => panic!("not refused as cut short: {other:?}"),
    };
    assert_eq!(landed, PART1_LAST, "the refusal names what landed");
    crate::vcs_store::rebuilt(svn).await;
    let standing = on(|state| {
      state
        .vcs_stores
        .subversion_records(svn)
        .map(Repository::youngest)
    });
    assert_eq!(standing, Some(PART1_LAST));
    let resumed = imported(svn, PART2)
      .await
      .0
      .map(|loaded| (loaded.revisions, loaded.youngest));
    assert_eq!(resumed, Ok((FULL_LAST - PART1_LAST, FULL_LAST)));
    assert!(
      history(svn).await == Some((FULL_LAST, FULL.to_vec())),
      "the resumed history is full.dump's"
    );
  });
}

/// Shape: the room a tight admission leaves the import of `full.dump`: past twice its largest revision's texts
/// (revision 7's 106,750 bytes, whole in the content store and again in its increment: 214 KB) with what the import's
/// records, green and content windows charge beside them, short of twice two such revisions (428 KB), so the batch lands
/// before revision 8. 320 KiB left too little beside the texts when every test ran at once (`Budget { available: 61904 }`).
const TIGHT_ROOM: u64 = 400 << 10;

/// C2 stage C, main's oracle of 2026-11-21 08:17 (the batch bounded by the import's admission) with conditions 1 and 2
/// (a refusal names what landed; the same dump resumes, reading through what it holds). Do: import `full.dump` into one
/// repository with the shard's room as it is; into another with only [`TIGHT_ROOM`] admittable (the rest withheld as
/// the pressure hold withholds it, §4.2), first cut three bytes short, then whole. Expect: the roomy import one batch, no flush for admission; the
/// tight one's cut import refused naming revision 7 the last landed (its batch landed before revision 8 for
/// admission, the batch after given up); its whole import resuming (revisions 0 to 7 read through, 8 to 13 committed);
/// both histories `full.dump` byte for byte, at the same youngest revision.
#[test]
fn t_c2c_imports_under_a_small_and_a_large_admission_end_alike_and_resume_after_what_landed() {
  stored(|| async move {
    let flushed = || {
      on(|state| {
        count(
          state,
          crate::repository::svn_commit::staging::FLUSHED_ADMISSION,
        )
      })
    };
    let (roomy, _) = subversion("roomy").await;
    let whole = imported(roomy, FULL).await.0.map(|loaded| loaded.youngest);
    assert_eq!(whole, Ok(FULL_LAST));
    assert_eq!(flushed(), 0, "a roomy import landed for admission");
    let (tight, _) = subversion("tight").await;
    let held = on(|state| {
      let held = state.store.budget.hold();
      let withheld = state.store.admittable().saturating_sub(TIGHT_ROOM);
      state.store.budget.set_hold(held.saturating_add(withheld));
      held
    });
    let cut = FULL.get(..FULL.len() - 3).expect("a shorter dump");
    let landed = match imported(tight, cut).await.0 {
      Err(LoadFailed::Refused {
        refusal: MigrationRefusal::Stream(DumpError::Truncated),
        landed,
        ..
      }) => landed,
      other => panic!("not refused as cut short: {other:?}"),
    };
    assert_eq!(landed, 7, "the refusal names what landed");
    assert!(flushed() >= 1, "no batch landed for admission");
    crate::vcs_store::rebuilt(tight).await;
    let resumed = imported(tight, FULL)
      .await
      .0
      .map(|loaded| (loaded.revisions, loaded.youngest));
    on(|state| state.store.budget.set_hold(held));
    assert_eq!(resumed, Ok((FULL_LAST - 7, FULL_LAST)));
    for repository in [roomy, tight] {
      assert!(
        history(repository).await == Some((FULL_LAST, FULL.to_vec())),
        "full.dump"
      );
    }
  });
}

/// C2 stage C (an import's batch advances the records ahead of the register): no session reads a repository while it
/// imports. Do: make a repository; lend it as a session does; begin an import of `part1.dump`; lend it again; give the
/// import back; lend it once more. Expect: lent, then not lent while the import is under way, then lent again.
#[test]
fn t_c2c_a_repository_serves_no_session_while_it_imports() {
  stored(|| async move {
    let (svn, _) = subversion("gated").await;
    let lent =
      move || on(|state| crate::vcs_store::svn_view::lend_in(state, svn, |_| ()).is_some());
    let before = lent();
    let (host, root) = host_holding(PART1);
    let begun = on(|state| begin(state, (&owner(), svn), (Box::new(host), root, DUMP)));
    assert!(begun.is_ok(), "{begun:?}");
    let during = lent();
    on(|state| super::abandon(state, svn));
    assert_eq!((before, during, lent()), (true, false, true));
  });
}

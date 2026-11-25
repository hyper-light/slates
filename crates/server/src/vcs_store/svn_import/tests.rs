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
  imported_sampling(id, dump, &mut || {}).await
}

/// [`imported`], `sampled` run after each turn's landings and owed bases, before the next turn: what a test reads of
/// the shard between turns.
pub(crate) async fn imported_sampling(
  id: RepositoryId,
  dump: impl AsRef<[u8]>,
  sampled: &mut dyn FnMut(),
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
    // A delta base the import owes brought back through the node's store, as `super::run` brings it: since A-760 (b)
    // 4d a landed batch's texts go from RAM once durable (main's ruling (B)), so a later revision's base is owed.
    if let Some(Err(failed)) = super::supply_owed_base(id).await {
      return (Err(failed), turns);
    }
    sampled();
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
pub(crate) async fn history(id: RepositoryId) -> Option<(u64, Vec<u8>)> {
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
    batches: super::Batches::default(),
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

/// Shape: the admission a tight import of `full.dump` is held to: past twice its largest revision's texts (revision 7's
/// 106,750 bytes, whole in the content store and again in its increment: 214 KB), short of twice two such revisions
/// (428 KB), so the batch lands before revision 8.
const TIGHT_ROOM: u64 = 320 << 10;

/// C2 stage C, main's oracle of 2026-11-21 08:17 (the batch bounded by the import's admission) with conditions 1 and 2
/// (a refusal names what landed; the same dump resumes, reading through what it holds). Do: import `full.dump` into one
/// repository with the shard's room as it is; into another with the import's admission held to [`TIGHT_ROOM`]
/// (`svn_commit::staging::ADMISSION_CAP`, against what its batch holds), first cut three bytes short, then whole. Expect: the roomy import one batch, no flush for admission; the
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
    let cap = &crate::repository::svn_commit::staging::ADMISSION_CAP;
    cap.with(|cap| cap.set(Some(TIGHT_ROOM)));
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
    cap.with(|cap| cap.set(None));
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

/// A format-2 dump of `revisions` revisions after revision 0, each adding one file `f<n>` of a line of text: a batch
/// large in versions and small in bytes.
fn many_revisions(revisions: usize) -> Vec<u8> {
  let props = |entries: &[(&str, &str)]| {
    let mut section = String::new();
    for (name, value) in entries {
      section.push_str(&format!(
        "K {}\n{name}\nV {}\n{value}\n",
        name.len(),
        value.len()
      ));
    }
    section.push_str("PROPS-END\n");
    section
  };
  let mut dump =
    String::from("SVN-fs-dump-format-version: 2\n\nUUID: 00000000-0000-4000-8000-000000000628\n\n");
  let zero = props(&[("svn:date", "2026-11-19T00:00:00.000000Z")]);
  dump.push_str(&format!(
    "Revision-number: 0\nProp-content-length: {0}\nContent-length: {0}\n\n{zero}\n",
    zero.len()
  ));
  for number in 1..=revisions {
    let date = format!("2026-11-19T00:00:00.{number:06}Z");
    let log = format!("r{number}");
    let section = props(&[
      ("svn:author", "alice"),
      ("svn:date", &date),
      ("svn:log", &log),
    ]);
    let text = format!("line {number}\n");
    let empty = "PROPS-END\n";
    dump.push_str(&format!(
      "Revision-number: {number}\nProp-content-length: {0}\nContent-length: {0}\n\n{section}\n",
      section.len()
    ));
    dump.push_str(&format!(
      "Node-path: f{number}\nNode-kind: file\nNode-action: add\nProp-content-length: {}\nText-content-length: {}\n\
       Content-length: {}\n\n{empty}{text}\n\n",
      empty.len(),
      text.len(),
      empty.len() + text.len()
    ));
  }
  dump.into_bytes()
}

/// A format-2 dump of `revisions` revisions after revision 0: revision 1 adds file `f` of `size` bytes, and each later
/// one rewrites it whole with `size` other bytes, so every version supersedes the whole file before it: a batch whose
/// versions' retention grows by the file's size a revision, while its bytes in flight stay one file's.
pub(crate) fn rewrites(revisions: usize, size: usize) -> Vec<u8> {
  let props = |entries: &[(&str, &str)]| {
    let mut section = String::new();
    for (name, value) in entries {
      section.push_str(&format!(
        "K {}\n{name}\nV {}\n{value}\n",
        name.len(),
        value.len()
      ));
    }
    section.push_str("PROPS-END\n");
    section
  };
  let mut dump =
    String::from("SVN-fs-dump-format-version: 2\n\nUUID: 00000000-0000-4000-8000-000000000893\n\n");
  let zero = props(&[("svn:date", "2026-11-24T00:00:00.000000Z")]);
  dump.push_str(&format!(
    "Revision-number: 0\nProp-content-length: {0}\nContent-length: {0}\n\n{zero}\n",
    zero.len()
  ));
  for number in 1..=revisions {
    let date = format!("2026-11-24T00:00:00.{number:06}Z");
    let log = format!("r{number}");
    let section = props(&[
      ("svn:author", "alice"),
      ("svn:date", &date),
      ("svn:log", &log),
    ]);
    let line = format!("{number:07}\n");
    let text: String = line.repeat(size / line.len());
    dump.push_str(&format!(
      "Revision-number: {number}\nProp-content-length: {0}\nContent-length: {0}\n\n{section}\n",
      section.len()
    ));
    let (action, empty) = if number == 1 {
      ("add", "PROPS-END\n")
    } else {
      ("change", "")
    };
    let props_header = if number == 1 {
      format!("Prop-content-length: {}\n", empty.len())
    } else {
      String::new()
    };
    dump.push_str(&format!(
      "Node-path: f\nNode-kind: file\nNode-action: {action}\n{props_header}Text-content-length: {}\n\
       Content-length: {}\n\n{empty}{text}\n\n",
      text.len(),
      empty.len() + text.len()
    ));
  }
  dump.into_bytes()
}

/// Shape: the green's retention cap the cap test runs under (`DaemonConfig::caps.green_chain_bytes`, the derived merge
/// byte bound a green's history and chain are held to): a few rewrites' worth, far below the whole import's.
const SMALL_GREEN_CAP: usize = 256 << 10;

/// Shape: the rewritten file's bytes in the cap test, and how many rewrites: together past [`SMALL_GREEN_CAP`] many
/// times over, each alone far within it.
const REWRITE_BYTES: usize = 8 << 10;
/// Shape: see [`REWRITE_BYTES`].
const REWRITES: usize = 200;

/// Goal 93, main's red of 2026-11-23 (interop round 2 on the Studio's 23 shards: the import of git-2000.dump refused
/// `BudgetExceeded { available: 373475417 }`, the daemon's log "retention refused at the green's cap ... secured=
/// 422803669 ... cap=373475417"): one batch's versions reserved more of the green's retention than its cap, and the
/// batch was bounded by the shard's memory alone. Do: under a green cap of [`SMALL_GREEN_CAP`], import [`rewrites`]'s
/// [`REWRITES`] rewrites of one [`REWRITE_BYTES`]-byte file. Expect: the import whole, revision [`REWRITES`] the
/// records' youngest, landed in more than one batch, each within the cap (`repository.svn_import.flushed_retention`
/// moved). Before, refused `Budget` at the first batch's landing.
#[test]
fn t_c2c_an_import_past_the_greens_retention_cap_lands_a_batch_within_it_at_a_time() {
  let (loaded, flushed) = crate::repository::svn_commit::tests::stored_configured(
    || async move {
      let (svn, _green) = subversion("capped").await;
      let (loaded, _turns) = imported(svn, rewrites(REWRITES, REWRITE_BYTES)).await;
      let flushed = on(|state| {
        count(
          state,
          crate::repository::svn_commit::staging::FLUSHED_RETENTION,
        )
      });
      (loaded.map(|loaded| loaded.youngest), flushed)
    },
    |config| config.caps.green_chain_bytes = SMALL_GREEN_CAP,
  );
  assert_eq!(loaded, Ok(u64::try_from(REWRITES).unwrap_or(u64::MAX)));
  assert!(flushed > 0, "landed in batches within the green's cap");
}

/// A format-2 dump whose revision 1 adds `files` files `f0`.. of about `size` bytes and whose revision 2 rewrites every
/// one of them whole: one revision of `files` paths.
fn one_wide_rewrite(files: usize, size: usize) -> Vec<u8> {
  wide_revisions(files, size, (2, false))
}

/// A format-2 dump of `revisions` revisions of `files` files of about `size` bytes each: revision 1 adds `f0`.. and
/// each later one rewrites them whole, or, where `apart`, every revision adds files of its own (`r<n>-f<i>`).
fn wide_revisions(files: usize, size: usize, (revisions, apart): (usize, bool)) -> Vec<u8> {
  let props = |entries: &[(&str, &str)]| {
    let mut section = String::new();
    for (name, value) in entries {
      section.push_str(&format!(
        "K {}\n{name}\nV {}\n{value}\n",
        name.len(),
        value.len()
      ));
    }
    section.push_str("PROPS-END\n");
    section
  };
  let mut dump =
    String::from("SVN-fs-dump-format-version: 2\n\nUUID: 00000000-0000-4000-8000-000000000894\n\n");
  let zero = props(&[("svn:date", "2026-11-24T00:00:00.000000Z")]);
  dump.push_str(&format!(
    "Revision-number: 0\nProp-content-length: {0}\nContent-length: {0}\n\n{zero}\n",
    zero.len()
  ));
  for number in 1..=revisions {
    let date = format!("2026-11-24T00:00:00.{number:06}Z");
    let log = format!("r{number}");
    let section = props(&[
      ("svn:author", "alice"),
      ("svn:date", &date),
      ("svn:log", &log),
    ]);
    dump.push_str(&format!(
      "Revision-number: {number}\nProp-content-length: {0}\nContent-length: {0}\n\n{section}\n",
      section.len()
    ));
    for file in 0..files {
      let line = format!("{number}:{file:05}\n");
      let text: String = line.repeat(size / line.len());
      let adds = number == 1 || apart;
      let (action, empty) = if adds {
        ("add", "PROPS-END\n")
      } else {
        ("change", "")
      };
      let props_header = if adds {
        format!("Prop-content-length: {}\n", empty.len())
      } else {
        String::new()
      };
      dump.push_str(&format!(
        "Node-path: {}\nNode-kind: file\nNode-action: {action}\n{props_header}Text-content-length: {}\n\
         Content-length: {}\n\n{empty}{text}\n\n",
        if apart {
          format!("r{number}-f{file}")
        } else {
          format!("f{file}")
        },
        text.len(),
        empty.len() + text.len()
      ));
    }
  }
  dump.into_bytes()
}

/// Shape: the green's retention cap the lone-revision test runs under: below what one revision adding [`WIDE_FILES`]
/// paths reserves of the change index alone.
const TINY_GREEN_CAP: usize = 16 << 10;

/// Shape: the files the lone revision adds, each a few bytes: their paths' change-index entries alone reserve past
/// [`TINY_GREEN_CAP`] (each path's entry is charged twice its bytes, `Green::changes_reservation`; 400 paths reserved
/// just under it).
const WIDE_FILES: usize = 1000;

/// Goal 93 (main's ruling of 2026-11-24 00:49: a lone revision past the cap is refused typed, naming the revision and
/// both numbers). Do: under a green cap of [`TINY_GREEN_CAP`], import [`one_wide_rewrite`] of [`WIDE_FILES`] small
/// files: revision 1 adds them all. Expect: refused at revision 1, what it would reserve past the room it names, no
/// revision landed; on the wire `ImportRefused` with `RetentionPastRoom { revision: 1, reserved, room, landed: 0 }`.
#[test]
fn t_c2c_a_revision_past_the_greens_retention_room_alone_is_refused_naming_it_and_both_numbers() {
  let failed = crate::repository::svn_commit::tests::stored_configured(
    || async move {
      let (svn, _green) = subversion("wide").await;
      let (loaded, _turns) = imported(svn, one_wide_rewrite(WIDE_FILES, 16)).await;
      loaded.err()
    },
    |config| config.caps.green_chain_bytes = TINY_GREEN_CAP,
  );
  let Some(LoadFailed::Uncommitted {
    refusal: Uncommitted::PastRetention { reserved, room },
    revision,
    landed,
  }) = failed.clone()
  else {
    panic!("refused past the green's retention room: {failed:?}");
  };
  assert_eq!((revision, landed), (1, 0));
  assert!(reserved > room, "{reserved} past {room}");
  let wire = svn_refusal_of(DUMP, ROOM, failed.expect("refused"));
  assert_eq!(
    wire,
    Refusal::ImportRefused {
      fault: ImportFault::RetentionPastRoom {
        revision: 1,
        reserved,
        room,
        landed: 0,
      }
    }
  );
}

/// Shape: the new files each of the chain test's revisions adds: a revision's chain entry (about 47 bytes a path)
/// within [`TINY_GREEN_CAP`], two revisions' past it, while their reservations (about 40 bytes a path) stay within it,
/// so the chains' room is the bound met first. (400 paths make an entry of 18,778 bytes, past the room alone.)
const CHAIN_FILES: usize = 190;

/// Shape: the chain test's revisions, each adding [`CHAIN_FILES`] files of its own.
const CHAIN_REVISIONS: usize = 2;

/// Goal 93 (main's order of 2026-11-24 02:25, item 2): a batch's chain entries meet the green chains' cap
/// (`caps.green_chain_bytes`, the partition's merge budget) at its landing. Do: under a cap of [`TINY_GREEN_CAP`],
/// import [`wide_revisions`] of [`CHAIN_REVISIONS`] revisions each adding [`CHAIN_FILES`] small files of its own.
/// Expect: the import whole, its batches each within the chains' room, one landed for it (the import's report,
/// `Loaded::batches`). Without the bound, refused `TableAtCapacity { table: "green_chains" }` at the batch's landing.
#[test]
fn t_c2c_an_import_past_the_chains_room_lands_a_batch_within_it_at_a_time() {
  let loaded = crate::repository::svn_commit::tests::stored_configured(
    || async move {
      let (svn, _green) = subversion("chained").await;
      let (loaded, _turns) = imported(
        svn,
        wide_revisions(CHAIN_FILES, 16, (CHAIN_REVISIONS, true)),
      )
      .await;
      loaded.map(|loaded| (loaded.youngest, loaded.batches.chain))
    },
    |config| config.caps.green_chain_bytes = TINY_GREEN_CAP,
  );
  let Ok((youngest, chain)) = loaded else {
    panic!("not imported whole: {loaded:?}");
  };
  assert_eq!(youngest, u64::try_from(CHAIN_REVISIONS).unwrap_or(u64::MAX));
  assert!(
    chain > 0,
    "a batch landed for its chains' room, in the import's report"
  );
}

/// Shape: the revisions of the batch whose apply the sliced commit test holds to the shard's quantum: past what one
/// quantum applies on any build (a version's apply is tens of microseconds in debug, the quantum a few).
const MANY_REVISIONS: usize = 400;

/// C2 stage C's sliced commit, main's ruling of 2026-11-21 12:43 (the red: an import batch's apply in one step held the
/// shard past its watchdog, git-2000's 2,000 versions). Do: import [`many_revisions`]'s [`MANY_REVISIONS`] revisions,
/// one batch, with the shard's own quantum. Expect: the import whole (every file at the head), its batch landed once,
/// and its apply taken in more than one slice (`repository.import_batch.apply_slices`): no step applies the batch whole.
/// Before, the apply was one loop in the commit's one step.
#[test]
fn t_c2c_a_large_batch_is_applied_a_slice_at_a_time() {
  stored(|| async move {
    let (svn, green) = subversion("sliced").await;
    let before = on(|state| count(state, crate::repository::push::BATCH_APPLY_SLICES));
    let loaded = imported(svn, many_revisions(MANY_REVISIONS))
      .await
      .0
      .map(|loaded| loaded.youngest);
    let last = u64::try_from(MANY_REVISIONS).unwrap();
    assert_eq!(loaded, Ok(last));
    let (slices, flushes, head) = on(|state| {
      (
        count(state, crate::repository::push::BATCH_APPLY_SLICES) - before,
        count(state, crate::repository::svn_commit::staging::FLUSHED_END),
        state.greens.get(&green).map(|engine| engine.head()),
      )
    });
    assert_eq!((flushes, head), (1, Some(last)));
    assert!(slices >= 2, "the batch's apply took {slices} slice(s)");
  });
}

/// Shape: the files the one-wide-revision import adds, each [`WIDE_TEXT_BYTES`]: git-2000's first revision's width and
/// more. A replay's read plan was cubic in them (A-887 part 3's `Increment::replay_reads`): 3,000 held the shard past
/// its window in the batch's first apply. The staging step's judge and commit were quadratic in them (each file's run
/// found by a walk of every run, each file's run counted against every other's, each place searched among every
/// whole's): 6,000 held the shard 1,511 ms in that one step. Its apply measured the post-state a file at a time and
/// each section ran on into the next in one poll: 9,000 held it 1,208 ms.
const WIDE_REVISION_FILES: usize = 9000;

/// Shape: each wide file's bytes, a source file's (about 1 KiB): a text the content store names (A-760).
const WIDE_TEXT_BYTES: usize = 1024;

/// Goal 93 (main's order of 2026-11-24 05:29: git-2000's import over HTTP spun shard 0 1,189 ms in one poll). Do: import
/// [`wide_revisions`]'s one revision adding [`WIDE_REVISION_FILES`] files of [`WIDE_TEXT_BYTES`]. Expect: the import
/// whole, and the shard's longest step (its registry's counted `longest_step_ns`) under its watchdog window. Red at
/// 77397a5f4: the batch's first apply planned its replay's reads over every run, change and whole named (A-887 part 3's
/// cubic `Increment::replay_reads`) in one poll, and the watchdog ended the process (3,480 ms against its 3,480 ms
/// window); green since 5606962eb made the plan n log n. Red again at 6,000 files at a0c46b705: the staging step's
/// judge and commit (`Green::sole_runs`, `alone`, the wholes' and names' searches) took 1,511 ms against the 1,000 ms
/// window (debug); 592 ms once each is a lookup. Red again at 9,000 files at 42589bb44: 1,208 ms, the apply's
/// `Post::len` a walk of every run a file and the staging step and the tail each run on with the section before them in
/// one poll; 473 ms once the post-state's runs are indexed (`RunTable`) and each section yields first.
#[test]
fn t_c2c_a_revision_of_thousands_of_files_imports_with_every_step_under_the_shards_window() {
  // Made before the shard's history begins: the fixture's own work is no step of the import's.
  let dump = wide_revisions(WIDE_REVISION_FILES, WIDE_TEXT_BYTES, (1, false));
  let (loaded, longest_ns, window_ns, (tail_slices, staging_steps, apply_steps)) =
    stored(move || async move {
      let (svn, _green) = subversion("wide-revision").await;
      let (loaded, _turns) = imported(svn, dump).await;
      let tail_slices =
        on(|state| count(state, crate::repository::svn_commit::staging::TAIL_SLICES));
      let staging_steps =
        on(|state| count(state, crate::repository::svn_commit::staging::STAGING_STEPS));
      let apply_steps = on(|state| count(state, crate::repository::push::BATCH_APPLY_STEPS));
      let shard = slates_rt::registry::current_shard().expect("on a shard");
      let longest_ns =
        slates_rt::registry::with_entry(shard, |entry| entry.pulse.longest_step_ns())
          .expect("the shard's entry");
      (
        loaded.map(|loaded| loaded.youngest),
        longest_ns,
        crate::watchdog::own_window_ns(),
        (tail_slices, staging_steps, apply_steps),
      )
    });
  assert_eq!(loaded, Ok(1));
  // Non-vacuity: the landing's tail ran in slices, so no loop over its texts ran whole in one step.
  assert!(tail_slices > 2, "the tail took {tail_slices} slice(s)");
  // Non-vacuity: the revision's version was applied a few units a call (goal 93 piece 3), never in one.
  assert!(
    apply_steps > 2,
    "its version's apply took {apply_steps} call(s)"
  );
  // Non-vacuity: the revision's staging was split, its check, build, reads and judge each a step of its own.
  assert!(
    staging_steps >= 4,
    "the staging took {staging_steps} step(s)"
  );
  assert!(
    longest_ns < window_ns,
    "a step of {longest_ns} ns against the shard's window of {window_ns} ns"
  );
}

/// Shape: the texts-bound red's rewritten file's bytes, and how many rewrites: each rewrite under the batch's texts
/// bound (`staging::texts_bound`, 2 MiB on a 16 KiB page), together many times it.
pub(crate) const BOUNDED_REWRITE_BYTES: usize = 256 << 10;
/// Shape: see [`BOUNDED_REWRITE_BYTES`].
pub(crate) const BOUNDED_REWRITES: usize = 64;

/// The bytes of windows the shard's content store holds (`mem.gauge.vcs_content.stored_bytes`).
fn stored_windows(state: &ShardState) -> u64 {
  state
    .vcs_stores
    .memory_rows()
    .into_iter()
    .find(|(kind, _)| *kind == "mem.gauge.vcs_content.stored_bytes")
    .map_or(0, |(_, bytes)| bytes)
}

/// What an import of [`rewrites`]'s [`BOUNDED_REWRITES`] rewrites of one [`BOUNDED_REWRITE_BYTES`]-byte file into a new
/// repository `name` held and made: its youngest revision, the most windows the content store held after a turn, and
/// the repository's history exported (`history`). With `unbounded`, the batch's texts bound is passed over, as an
/// import was before it (`staging::TEXTS_UNBOUNDED`).
async fn rewrites_imported(
  name: &str,
  unbounded: bool,
) -> (Result<u64, LoadFailed>, u64, Option<(u64, Vec<u8>)>) {
  let (svn, _) = subversion(name).await;
  let unbound = &crate::repository::svn_commit::staging::TEXTS_UNBOUNDED;
  unbound.with(|cell| cell.set(unbounded));
  let mut most = 0_u64;
  let (loaded, _) = imported_sampling(
    svn,
    rewrites(BOUNDED_REWRITES, BOUNDED_REWRITE_BYTES),
    &mut || most = most.max(on(|state| stored_windows(state))),
  )
  .await;
  unbound.with(|cell| cell.set(false));
  (
    loaded.map(|loaded| loaded.youngest),
    most,
    history(svn).await,
  )
}

/// Goal 93 P7 (main 01a20182's ruling of 2026-11-24; w-perf's probe of git-2000: the content store's windows grew with
/// the history, 41 MiB at revision 1 to 426 MiB at revision 2,000, none let go until the import's end, its 2,000
/// revisions one batch, whose texts go only when it lands). Derived bound (`staging::texts_bound`): a batch lands once
/// the texts it carries and the next revision's would pass the changeset store's unit, and the next revision's texts are
/// put as the dump is read, so the import holds at most twice the larger of that unit and its largest revision's texts.
/// Do: import [`rewrites`]'s [`BOUNDED_REWRITES`] rewrites of one [`BOUNDED_REWRITE_BYTES`]-byte file (each rewrite's
/// lines numbered, so no two share a window; about 16 MiB of texts in all) through the shard's own turns, reading the
/// windows the content store holds after each turn's landings; then import it again into another repository with the
/// bound passed over. Expect: both imported to the same youngest revision; the most held at most what is held at the
/// end plus twice the bound; batches landed for their texts (`repository.svn_import.flushed_texts`); and the two
/// repositories' histories, exported whole, the same byte for byte (the oracle: the extra cuts change no revision).
/// Failing first: every rewrite's text held at once, the 64 revisions one batch.
#[test]
fn an_imports_texts_are_held_a_batch_at_a_time_within_their_bound() {
  let (bounded, unbounded, flushed, (bound, at_end)) = stored(|| async move {
    // At one unit (`staging::texts_bound`'s floor): the room forced to it, whatever the device's flush.
    let unit = on(|state| crate::vcs_store::hg_import::segment_bytes(state) as u64);
    let room = &crate::repository::svn_commit::staging::IMPORT_ROOM;
    room.with(|room| room.set(Some(unit)));
    let bounded = rewrites_imported("bounded", false).await;
    let flushed = on(|state| {
      use crate::repository::svn_commit::staging::{FLUSHED_TEXTS, LAND_BATCHES, LAND_FLUSH_NS};
      (
        count(state, FLUSHED_TEXTS),
        count(state, LAND_BATCHES),
        count(state, LAND_FLUSH_NS),
      )
    });
    let unbounded = rewrites_imported("unbounded", true).await;
    room.with(|room| room.set(None));
    let ends = on(|state| (unit, stored_windows(state)));
    (bounded, unbounded, flushed, ends)
  });
  let youngest = u64::try_from(BOUNDED_REWRITES).unwrap_or(u64::MAX);
  assert_eq!((&bounded.0, &unbounded.0), (&Ok(youngest), &Ok(youngest)));
  assert!(
    bounded.1 <= at_end.saturating_add(bound.saturating_mul(2)),
    "held at most {} bytes of windows after a turn, {at_end} at the end, the bound {bound}",
    bounded.1
  );
  let (flushed, landed, flush_ns) = flushed;
  assert!(flushed > 0, "batches landed for their texts");
  assert!(
    landed > flushed && flush_ns > 0,
    "each batch's stages timed (`repository.svn_import.land_batches` {landed}, `land_flush_ns` {flush_ns})"
  );
  assert!(
    bounded.2 == unbounded.2,
    "the same history with and without the texts bound"
  );
}

/// The batches an import of [`rewrites`]'s rewrites into a new repository `name` landed, its texts bound derived from a
/// durable flush forced to what copies `flush_units` of the changeset store's unit at the profile's memcpy rate, and its
/// admitted room forced to `room_units` units (`staging::FLUSH_NS`, `staging::IMPORT_ROOM`; none, no room but the
/// largest): with the youngest revision it reached.
async fn batches_at(
  name: &str,
  (flush_units, room_units): (u64, Option<u64>),
) -> (u64, Result<u64, LoadFailed>) {
  use crate::repository::svn_commit::staging::{FLUSH_NS, IMPORT_ROOM, LAND_BATCHES};
  let (unit, memcpy) = on(|state| {
    (
      crate::vcs_store::hg_import::segment_bytes(state) as u64,
      state.config.memcpy_bytes_per_second.max(1),
    )
  });
  let flush_ns =
    u64::try_from(u128::from(flush_units * unit) * 1_000_000_000 / u128::from(memcpy)).unwrap();
  FLUSH_NS.with(|forced| forced.set(Some(flush_ns.max(1))));
  IMPORT_ROOM.with(|room| room.set(Some(room_units.map_or(u64::MAX, |units| units * unit))));
  let before = on(|state| count(state, LAND_BATCHES));
  let (youngest, _, _) = rewrites_imported(name, false).await;
  let landed = on(|state| count(state, LAND_BATCHES)) - before;
  FLUSH_NS.with(|forced| forced.set(None));
  IMPORT_ROOM.with(|room| room.set(None));
  (landed, youngest)
}

/// Shape: the bound's flush, in units the profile copies in it, the rent-or-buy reds compare: one, a few, many.
const ONE_UNIT: u64 = 1;
/// Shape: see [`ONE_UNIT`].
const FOUR_UNITS: u64 = 4;
/// Shape: see [`ONE_UNIT`].
const MANY_UNITS: u64 = 64;

/// Goal 93 P7 (main's ruling of 2026-11-25: a batch's texts bound rent or buy, `clamp(F x M, U, R)`, so the 40% P7's
/// one-unit bound cost in durable flushes is bought back with the profile's own price of a byte). Do: import
/// [`rewrites`]'s rewrites three times, the device's durable flush forced to what the profile's memcpy rate copies of one
/// unit, then of [`FOUR_UNITS`], the room past either. Expect every import whole, and fewer batches as the flush rises:
/// at four units fewer than at one. Red before: the bound one unit whatever the flush, as many batches either way.
#[test]
fn an_imports_batches_fall_as_its_durable_flush_buys_more_bytes() {
  let ((one, one_youngest), (four, four_youngest)) = stored(|| async move {
    (
      batches_at("one-unit", (ONE_UNIT, None)).await,
      batches_at("four-units", (FOUR_UNITS, None)).await,
    )
  });
  let youngest = u64::try_from(BOUNDED_REWRITES).unwrap_or(u64::MAX);
  assert_eq!((one_youngest, four_youngest), (Ok(youngest), Ok(youngest)));
  println!("svn_import: batches at a flush of one unit {one}, of four units {four}");
  assert!(
    four < one,
    "fewer batches as the flush buys more bytes: {four} at four units, {one} at one"
  );
}

/// Goal 93 P7 (main's ruling of 2026-11-25: the rent-or-buy bound capped by the import's admitted room, so a loaded
/// shard's import keeps its batches small). Do: import [`rewrites`]'s rewrites twice with the flush forced to what copies
/// [`MANY_UNITS`] units (more than all its texts): once in a room of [`FOUR_UNITS`] units (its half the cap), once with no
/// room but the largest. Expect both whole, and more batches in the room: it bounds the batch where it is the smaller. Red before: the
/// room never consulted, as many batches either way.
#[test]
fn an_imports_room_bounds_its_batch_where_it_is_smaller() {
  let ((capped, capped_youngest), (free, free_youngest)) = stored(|| async move {
    (
      batches_at("capped", (MANY_UNITS, Some(FOUR_UNITS))).await,
      batches_at("free", (MANY_UNITS, None)).await,
    )
  });
  let youngest = u64::try_from(BOUNDED_REWRITES).unwrap_or(u64::MAX);
  assert_eq!(
    (capped_youngest, free_youngest),
    (Ok(youngest), Ok(youngest))
  );
  println!(
    "svn_import: batches at a flush of {MANY_UNITS} units in a room of four {capped}, in no room {free}"
  );
  assert!(
    capped > free,
    "the room of four units cuts more batches: {capped} in it, {free} without"
  );
}

/// Shape: the appends oracle's revisions, its file's first text, and each append: every text the one before with more
/// bytes after it, so consecutive revisions hold the same opening windows (windows are named by their bytes).
const APPEND_REVISIONS: usize = 12;
/// Shape: see [`APPEND_REVISIONS`].
const APPEND_FIRST_BYTES: usize = 300 << 10;
/// Shape: see [`APPEND_REVISIONS`].
const APPEND_BYTES: usize = 7_001;

/// A format-2 dump of `revisions` revisions after revision 0: revision 1 adds file `f` holding `first` bytes of noise,
/// and each later one rewrites it whole as the same noise `step` bytes longer: an append, whose text shares every whole
/// window of the one before.
fn appends(revisions: usize, first: usize, step: usize) -> Vec<u8> {
  let props = |entries: &[(&str, &str)]| {
    let mut section = String::new();
    for (name, value) in entries {
      section.push_str(&format!(
        "K {}\n{name}\nV {}\n{value}\n",
        name.len(),
        value.len()
      ));
    }
    section.push_str("PROPS-END\n");
    section
  };
  let mut dump =
    b"SVN-fs-dump-format-version: 2\n\nUUID: 00000000-0000-4000-8000-000000000910\n\n".to_vec();
  let zero = props(&[("svn:date", "2026-11-25T00:00:00.000000Z")]);
  dump.extend_from_slice(
    format!(
      "Revision-number: 0\nProp-content-length: {0}\nContent-length: {0}\n\n{zero}\n",
      zero.len()
    )
    .as_bytes(),
  );
  for number in 1..=revisions {
    let date = format!("2026-11-25T00:00:00.{number:06}Z");
    let log = format!("r{number}");
    let section = props(&[
      ("svn:author", "alice"),
      ("svn:date", &date),
      ("svn:log", &log),
    ]);
    let text = crate::test_store::noise(first + (number - 1) * step);
    dump.extend_from_slice(
      format!(
        "Revision-number: {number}\nProp-content-length: {0}\nContent-length: {0}\n\n{section}\n",
        section.len()
      )
      .as_bytes(),
    );
    let (action, empty) = if number == 1 {
      ("add", "PROPS-END\n")
    } else {
      ("change", "")
    };
    let props_header = if number == 1 {
      format!("Prop-content-length: {}\n", empty.len())
    } else {
      String::new()
    };
    dump.extend_from_slice(
      format!(
        "Node-path: f\nNode-kind: file\nNode-action: {action}\n{props_header}Text-content-length: {}\n\
         Content-length: {}\n\n{empty}",
        text.len(),
        empty.len() + text.len()
      )
      .as_bytes(),
    );
    dump.extend_from_slice(&text);
    dump.extend_from_slice(b"\n\n");
  }
  dump
}

/// What an import of [`appends`] into a new repository `name` made, its texts bound `bound` where set (`staging::
/// TEXTS_BOUND`), or passed over where none (`staging::TEXTS_UNBOUNDED`): its youngest revision, the batches it landed
/// for their texts, and its history exported whole.
async fn appends_imported(
  name: &str,
  bound: Option<u64>,
) -> (Result<u64, LoadFailed>, u64, Option<(u64, Vec<u8>)>) {
  let staging = &crate::repository::svn_commit::staging::TEXTS_BOUND;
  let unbound = &crate::repository::svn_commit::staging::TEXTS_UNBOUNDED;
  staging.with(|cell| cell.set(bound));
  unbound.with(|cell| cell.set(bound.is_none()));
  let before = on(|state| count(state, crate::repository::svn_commit::staging::FLUSHED_TEXTS));
  let (svn, _) = subversion(name).await;
  let (loaded, _) = imported(
    svn,
    appends(APPEND_REVISIONS, APPEND_FIRST_BYTES, APPEND_BYTES),
  )
  .await;
  staging.with(|cell| cell.set(None));
  unbound.with(|cell| cell.set(false));
  let flushed =
    on(|state| count(state, crate::repository::svn_commit::staging::FLUSHED_TEXTS)) - before;
  (
    loaded.map(|loaded| loaded.youngest),
    flushed,
    history(svn).await,
  )
}

/// Main's oracle of 2026-11-24 (TSan diag 45557094379: on a Linux runner's texts bound, a revision changing a text an
/// earlier batch landed was refused `Unstored`, the landed text's let-go taking the windows the later text shared). Do:
/// import [`appends`]'s [`APPEND_REVISIONS`] appends to one file with a texts bound of one byte, so a batch lands before
/// every revision while the next revision's text is put; then import it again with the bound passed over, one batch.
/// Expect: both at the last revision, the first cut before every revision but the first, and the two histories,
/// exported whole, the same byte for byte. Red before: the second revision refused `Unstored`.
#[test]
fn an_import_cut_before_every_revision_is_the_import_uncut() {
  let (cut, uncut) = stored(|| async move {
    let cut = appends_imported("appends-cut", Some(1)).await;
    let uncut = appends_imported("appends-uncut", None).await;
    (cut, uncut)
  });
  let youngest = u64::try_from(APPEND_REVISIONS).unwrap_or(u64::MAX);
  assert_eq!((&cut.0, &uncut.0), (&Ok(youngest), &Ok(youngest)));
  assert_eq!(
    cut.1,
    youngest - 1,
    "a batch landed before every revision but the first"
  );
  assert_eq!(uncut.1, 0, "the uncut import was one batch");
  assert!(
    cut.2.is_some() && cut.2 == uncut.2,
    "the same history cut and uncut"
  );
}

/// Main's ruling (B) of 2026-11-24, the case no scoped pin covered: a landed text's let-go by another party between the
/// import's steps (a client's commit landing between two batches, a reader's give-back), while the import holds texts it
/// put and has not landed. Do: import [`appends`] cut before every revision, and before each of the import's turns let go
/// every text it has put so far, as a landed text's let-go would (`name_holds::let_go_landed_texts`); then import it
/// again uncut with no let-go. Expect: both at the last revision, the same history byte for byte, windows kept for their
/// unlanded holders (`kept_unlanded` moved) and none found let go as they were pinned (`pinned_away` 0). Red before: the
/// let-go took the open batch's unlanded texts and the windows the next revision shared, and the import was refused.
#[test]
fn a_let_go_between_an_imports_batches_keeps_its_unlanded_texts() {
  let texts: Vec<[u8; 32]> = (1..=APPEND_REVISIONS)
    .map(|number| {
      crate::repository::svn_commit::tests::identity(&crate::test_store::noise(
        APPEND_FIRST_BYTES + (number - 1) * APPEND_BYTES,
      ))
    })
    .collect();
  let (cut, uncut, (kept, away)) = stored(move || async move {
    let staging = &crate::repository::svn_commit::staging::TEXTS_BOUND;
    staging.with(|cell| cell.set(Some(1)));
    let (svn, _) = subversion("let-go-between").await;
    let (loaded, _) = imported_sampling(
      svn,
      appends(APPEND_REVISIONS, APPEND_FIRST_BYTES, APPEND_BYTES),
      &mut || {
        on(|state| crate::vcs_store::name_holds::let_go_landed_texts(state, &texts));
      },
    )
    .await;
    staging.with(|cell| cell.set(None));
    let cut = (loaded.map(|loaded| loaded.youngest), history(svn).await);
    let counters = on(|state| {
      state
        .vcs_stores
        .content()
        .map(|content| {
          (
            content.counters().kept_unlanded,
            content.counters().pinned_away,
          )
        })
        .unwrap_or_default()
    });
    let uncut = appends_imported("let-go-uncut", None).await;
    (cut, (uncut.0, uncut.2), counters)
  });
  let youngest = u64::try_from(APPEND_REVISIONS).unwrap_or(u64::MAX);
  assert_eq!((&cut.0, &uncut.0), (&Ok(youngest), &Ok(youngest)));
  assert!(
    cut.1.is_some() && cut.1 == uncut.1,
    "the same history with let-goes between batches"
  );
  assert!(kept > 0, "windows kept for their unlanded holders");
  assert_eq!(away, 0, "no window found let go as it was pinned");
}

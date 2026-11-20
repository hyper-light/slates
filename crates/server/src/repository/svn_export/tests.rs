//! A-652 part 3's export tests (§4.17.4 piece 6; §4.15): a range of a repository's revisions written into a new volume
//! as `svnadmin dump` wrote it, in format 2 and format 3, through the verb on a running shard and through the job's two
//! walks; a second walk that differs from the first refused, nothing kept; and each refusal by kind before anything is
//! made.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use slates_db::catalog::{Principal, RepositoryId, VolumeId as DbVolumeId};
use slates_ipc::protocol::{DUMP_FILE, ObjectFormat, Refusal, ReplyBody, RequestBody, VcsTool};
use slates_svn::commit::record::Record;
use slates_svn::dump::Format;
use slates_wire::request::RequestId;

use super::{ExportJob, WALKS_DIFFERED, export_dump};
use crate::merge_service::ReplyRoute;
use crate::state::ShardState;
use crate::vcs_store::svn_import::tests::{
  FULL, FULL_LAST, PART1, PART2, imported, owner, subversion,
};
use crate::verbs::dispatch;

/// Shape: the first revision of `part2.dump` (`make-fixtures.sh` dumps `--incremental -r 4:13` into it).
const PART2_FIRST: u64 = 4;

/// Shape: the origin the tests' requests come from, any number.
const ORIGIN: u64 = 7;

/// Shape: a client slot no client holds, so a reply sent to it is counted undeliverable rather than written.
const NO_CLIENT: u32 = 4096;

/// Shape: how long a test waits for the shard's task to publish an export: far past a debug build's export of the
/// vectors (milliseconds), so only a task that never ends reaches it.
const TASK_WAIT: Duration = Duration::from_secs(60);

/// Shape: how many slices a test lets a walk take before it gives up: far past the vectors' revisions.
const SLICE_LIMIT: usize = 1 << 16;

/// Another principal than the repository's owner.
fn stranger() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 4321,
  }
}

/// A Subversion repository `name` holding `full.dump`'s history, imported in its two parts: its id and its green.
fn holding_the_full_history(state: &mut ShardState, name: &str) -> (RepositoryId, DbVolumeId) {
  let (id, green) = subversion(state, name);
  for part in [PART1, PART2] {
    let (loaded, _) = imported(state, id, part);
    assert!(loaded.is_ok(), "{loaded:?}");
  }
  (id, green)
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

/// Asks for revisions `range` of repository `name` as `principal`, in format 3 when `deltas`, into a volume `volume`:
/// the reply, and whether it was deferred to the export's task (taken, as the verb runner takes it).
fn ask(
  state: &mut ShardState,
  (principal, name): (&Principal, &str),
  (range, deltas): ((u64, u64), bool),
  volume: &str,
) -> (ReplyBody, bool) {
  let reply = export_dump(state, principal, (name, range, deltas), volume.to_owned());
  (reply, std::mem::take(&mut state.acceptance_deferred))
}

/// The dump file the volume named `name` holds, once that volume is published.
fn dump_of(state: &ShardState, name: &str) -> Option<Vec<u8>> {
  let id = state.db.partition().volume_by_name(name)?.id;
  let handle = state.by_id.get(&id).copied()?;
  let slot = state.volumes.get(handle).ok()?;
  let root = slot.volume.root_inode(&state.store).ok()?;
  let file = slot
    .volume
    .lookup_no(&state.store, root, DUMP_FILE)
    .ok()?
    .inode;
  let mut bytes = Vec::new();
  let mut buffer = vec![0u8; 1 << 16];
  loop {
    let offset = u64::try_from(bytes.len()).ok()?;
    let read = slot
      .volume
      .read(&state.store, file, offset, &mut buffer)
      .ok()?;
    if read == 0 {
      return Some(bytes);
    }
    bytes.extend_from_slice(buffer.get(..read)?);
  }
}

/// A-652 part 3 (export(import(D)) = D through the verb, §4.17.4 piece 6's gate). Do: on a running shard, import
/// `part1.dump` and `part2.dump`, then ask for revisions 0 to 13 in format 2 into one volume and revisions 4 to 13 in
/// format 3 into another, under two requests. Expect: both held (acknowledged and deferred); then within the wait both
/// volumes published, the first's dump file `full.dump` byte for byte (`svnadmin dump`) and the second's `part2.dump`
/// byte for byte (`svnadmin dump --deltas --incremental -r 4:13`).
#[test]
fn t_a652_a_range_exports_into_a_new_volume_as_svnadmin_dump_wrote_it() {
  let daemon = crate::daemon::audit_daemon();
  let asked = crate::daemon::observe_first(&daemon, |state| {
    holding_the_full_history(state, "svn");
    routed(state, 1);
    let whole = ask(state, (&owner(), "svn"), ((0, FULL_LAST), false), "whole");
    routed(state, 2);
    let part = ask(
      state,
      (&owner(), "svn"),
      ((PART2_FIRST, FULL_LAST), true),
      "part",
    );
    [whole, part]
  });
  let held = (ReplyBody::Acknowledged, true);
  assert_eq!(asked, [held.clone(), held]);
  let began = Instant::now();
  let made = loop {
    let made = crate::daemon::observe_first(&daemon, |state| {
      [dump_of(state, "whole"), dump_of(state, "part")]
    });
    if made.iter().all(Option::is_some) || began.elapsed() > TASK_WAIT {
      break made;
    }
  };
  daemon.stop();
  let [whole, part] = made;
  assert!(whole.as_deref() == Some(FULL), "full.dump");
  assert!(part.as_deref() == Some(PART2), "part2.dump");
}

/// Runs `job`'s first walk to its end on the shard.
fn measured(state: &mut ShardState, job: &mut ExportJob) -> Result<(), Refusal> {
  for _ in 0..SLICE_LIMIT {
    if job.measure_step(state)? {
      return Ok(());
    }
  }
  Err(Refusal::ContentUnavailable)
}

/// Runs `job`'s second walk to its end on the shard: the admitted volume's dump file then, or the walk's refusal.
fn written(state: &mut ShardState, job: &mut ExportJob) -> Result<Vec<u8>, Refusal> {
  job.prepare(state)?;
  for _ in 0..SLICE_LIMIT {
    if job.write_step(state)? {
      return file_of(state, job).ok_or(Refusal::ContentUnavailable);
    }
  }
  Err(Refusal::ContentUnavailable)
}

/// The dump file `job`'s admitted volume holds.
fn file_of(state: &ShardState, job: &ExportJob) -> Option<Vec<u8>> {
  let volume = &job.admitted.as_ref()?.volume;
  let mut bytes = vec![0u8; usize::try_from(job.written).ok()?];
  let read = volume.read(&state.store, job.file, 0, &mut bytes).ok()?;
  bytes.truncate(read);
  Some(bytes)
}

/// A job exporting revisions `range` of `repository` in `format` into a volume named `name`, for the owner.
fn job_of(
  repository: RepositoryId,
  (range, format): ((u64, u64), Format),
  name: &str,
) -> ExportJob {
  let request = (ORIGIN, RequestId::from_word(1));
  ExportJob::new(
    (request, owner()),
    (repository, range, format),
    (name.to_owned(), None),
  )
}

/// A-652 part 3 (the two walks, sliced). Do: export revisions 0 to 3 of a repository holding `full.dump`'s history
/// through the job's walks, a slice at a time, in format 2, then the whole range in format 3. Expect: the volume admitted
/// for the first walk's length, its dump file `part1.dump` byte for byte (`svnadmin dump -r 0:3`), written in more than
/// one piece when its length passes the copy slice, and the second `deltas.dump` byte for byte (`svnadmin dump
/// --deltas`); each volume given back after.
#[test]
fn t_a652_the_jobs_two_walks_write_the_range_as_svnadmin_dump_wrote_it() {
  const DELTAS: &[u8] = include_bytes!("../../../../svn/fixtures/dump/deltas.dump");
  crate::daemon::audit_on_shard(|state| {
    let (id, _) = holding_the_full_history(state, "svn");
    let mut part = job_of(id, ((0, 3), Format::Two), "part");
    let made = measured(state, &mut part).and_then(|()| written(state, &mut part));
    part.release(state);
    assert!(made.as_deref() == Ok(PART1), "part1.dump");
    let mut deltas = job_of(id, ((0, FULL_LAST), Format::Three), "deltas");
    let made = measured(state, &mut deltas).and_then(|()| written(state, &mut deltas));
    deltas.release(state);
    assert!(made.as_deref() == Ok(DELTAS), "deltas.dump");
  });
}

/// A-652 part 3 (a second walk that differs from the first). Do: measure revisions 0 to 3 of a repository holding
/// `full.dump`'s history, then set revision 0's properties through the owner's step, then write the range. Expect: the
/// write refused `Unpublished`, counted once, and nothing kept: the job's volume given back.
#[test]
fn t_a652_a_second_walk_that_differs_from_the_first_is_refused() {
  crate::daemon::audit_on_shard(|state| {
    let (id, _) = holding_the_full_history(state, "svn");
    let mut job = job_of(id, ((0, 3), Format::Two), "changed");
    assert_eq!(measured(state, &mut job), Ok(()));
    let properties = BTreeMap::from([(b"svn:log".to_vec(), b"changed meanwhile".to_vec())]);
    let changed = Record::RevisionProperties {
      revision: 0,
      properties,
    };
    let set = crate::repository::svn_commit::record(state, (&owner(), id), changed);
    assert_eq!(set, Ok(()));
    let refused = written(state, &mut job);
    job.release(state);
    assert!(
      matches!(refused, Err(Refusal::Unpublished { .. })),
      "{refused:?}"
    );
    let counted = state.refusals.get(WALKS_DIFFERED).copied();
    assert_eq!((counted, job.admitted.is_none()), (Some(1), true));
  });
}

/// Creates a git repository `name` on the shard.
fn git(state: &mut ShardState, name: &str) {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool: VcsTool::Git,
      format: ObjectFormat::Sha1,
      default_branch: "main".to_owned(),
      green: None,
    },
  );
  assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
}

/// A-652 part 3 (refused by kind before anything is made). Do: ask for an export of a repository nobody made, of the
/// owner's repository as another principal, of a git repository, of a range whose first revision is past its last and
/// of one past the youngest revision (in `svn_repos_dump_fs4`'s words), into the name the repository's green holds,
/// and outside a recorded request. Expect `NotFound`, `Forbidden`, `Unsupported`, the two `BadRequest`s,
/// `AlreadyExists` naming the green, and `Unsupported`; none deferred.
#[test]
fn t_a652_an_export_is_refused_by_kind_before_anything_is_made() {
  crate::daemon::audit_on_shard(|state| {
    let (_, green) = holding_the_full_history(state, "svn");
    git(state, "git");
    let own = owner();
    let bad = |reason: &str| Refusal::BadRequest {
      reason: reason.to_owned(),
    };
    let cases = [
      (
        ask(state, (&own, "none"), ((0, 1), false), "a"),
        Refusal::NotFound,
      ),
      (
        ask(state, (&stranger(), "svn"), ((0, 1), false), "a"),
        crate::verbs::reply_refusal(crate::verbs::forbidden("export_dump")),
      ),
      (
        ask(state, (&own, "git"), ((0, 1), false), "a"),
        Refusal::Unsupported {
          feature: "a dump of a repository of another tool than Subversion".to_owned(),
        },
      ),
      (
        ask(state, (&own, "svn"), ((3, 2), false), "a"),
        bad("Start revision 3 is greater than end revision 2"),
      ),
      (
        ask(state, (&own, "svn"), ((0, 14), false), "a"),
        bad("End revision 14 is invalid (youngest revision is 13)"),
      ),
    ];
    for ((reply, deferred), wanted) in cases {
      assert_eq!((reply, deferred), (crate::verbs::refused(wanted), false));
    }
    let taken = state
      .db
      .partition()
      .volume(green)
      .map(|volume| volume.name.clone())
      .expect("the green's record");
    let existing = crate::verbs::to_wire_volume(green);
    let held = ask(state, (&own, "svn"), ((0, 1), false), &taken);
    let wanted = crate::verbs::refused(Refusal::AlreadyExists { existing });
    assert_eq!(held, (wanted, false));
    let unrecorded = ask(state, (&own, "svn"), ((0, 1), false), "fresh");
    let outside = match &unrecorded.0 {
      ReplyBody::Refused { refusal } => matches!(refusal, Refusal::Unsupported { .. }),
      _ => false,
    };
    assert!(outside && !unrecorded.1, "{unrecorded:?}");
  });
}

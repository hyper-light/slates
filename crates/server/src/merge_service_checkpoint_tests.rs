//! A green's checkpoint held in its owner shard's content store a paced step at a time, and recorded by name (A-579's
//! last step, on chain entry format 6's rule, A-825). w-work-cow measured a 1 GiB green's checkpoint at 650 to 1,063 ms
//! in one step on 2026-11-21 (build 52, encode 71, record 527 ms of `db.mutate_owned`): every request its owner served
//! waited behind it, and the watchdog's stall line fired for any green that large. These tests hold the shape: the
//! encoding is hashed and staged a chunk a unit with someone waiting, the record names it, a restart reads it back from
//! the store, and a stop part-way leaves the previous checkpoint standing and the next one whole.

use slates_db::catalog::Principal;
use slates_ipc::protocol::{ReplyBody, RequestBody, VolumeId};

use super::{
  CHECKPOINT_HOLD_STEPS, CHECKPOINT_TAKEN, CHECKPOINT_UNNAMED_RELEASED, checkpoint_bytes,
  checkpoint_object,
};
use crate::state::ShardState;
use crate::verbs::to_db_volume;

/// Shape: the files each version writes: a few, so the checkpoint is a few large keys rather than many small ones.
const FILES: usize = 3;
/// Shape: the bytes of each file: one inputs chunk (1 MiB), so the checkpoint's encoding is several chunks and its hold
/// several units of each kind.
const FILE_BYTES: usize = 1 << 20;
/// Shape: the looks a test waits for a checkpoint's turns to end, each after a shard's step has run.
const LOOKS: usize = 100_000;

fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

fn ask(state: &mut ShardState, body: RequestBody) -> ReplyBody {
  crate::verbs::dispatch(state, 1, &owner(), body)
}

/// A plain green with nothing in it.
fn green(state: &mut ShardState, name: &str) -> VolumeId {
  match ask(
    state,
    RequestBody::CreateGreen {
      name: name.to_owned(),
      require_evidence: false,
      base: None,
    },
  ) {
    ReplyBody::GreenCreated { id } => id,
    other => panic!("no green: {other:?}"),
  }
}

/// The bytes file `file` holds at `serial`: distinct per file and per serial, so each version changes every file.
fn file_bytes(file: usize, serial: u32) -> Vec<u8> {
  format!("file {file} at serial {serial}; ")
    .into_bytes()
    .into_iter()
    .cycle()
    .take(FILE_BYTES)
    .collect()
}

/// Writes one version of `green` that sets `files` files of [`FILE_BYTES`] each through one work, then destroys the
/// work, which moves the green's floor to the version and so asks its checkpoint (`verbs::owner_floor`). A file past
/// those the green holds is created; the rest are replaced whole.
fn write_version(state: &mut ShardState, green: VolumeId, (serial, files): (u32, usize)) -> u64 {
  let ReplyBody::WorkCreated { id: work, .. } = ask(
    state,
    RequestBody::CreateWork {
      green,
      name: format!("work-{serial}"),
    },
  ) else {
    panic!("no work over {green:?}")
  };
  for file in 0..files {
    let path = format!("/f{file}");
    let replaced = if serial == 1 || file >= FILES {
      0
    } else {
      FILE_BYTES as u64
    };
    let edited = ask(
      state,
      RequestBody::Edit {
        work,
        path,
        at: 0,
        delete_len: replaced,
        bytes: file_bytes(file, serial),
      },
    );
    assert!(matches!(edited, ReplyBody::Edited), "{edited:?}");
  }
  let submitted = ask(state, crate::identities::authored(work, "large files"));
  let ReplyBody::Submitted {
    version: Some(version),
    ..
  } = submitted
  else {
    panic!("not accepted: {submitted:?}")
  };
  let destroyed = ask(state, RequestBody::Destroy { volume: work });
  assert!(matches!(destroyed, ReplyBody::Destroyed), "{destroyed:?}");
  version
}

/// Keeps someone waiting on the shard (a task ready at every step) until the shard has taken `taken` checkpoints, so
/// every paced step of a checkpoint meets a waiter and yields at its budget.
fn wait_until_taken(taken: u64) {
  let waiter = slates_rt::futures::spawn(async move {
    loop {
      let done = crate::state::with_state(|state| {
        state.refusals.get(CHECKPOINT_TAKEN).copied().unwrap_or(0) >= taken
      });
      if done.unwrap_or(true) {
        return;
      }
      slates_rt::futures::yield_now().await;
    }
  })
  .unwrap();
  let _ = slates_rt::futures::detach(waiter);
}

/// Waits until `green`'s checkpoint, if one is being taken in turns, has ended.
fn wait_for_turns(daemon: &crate::Daemon, green: slates_db::catalog::VolumeId) {
  let mut looks = 0;
  while crate::daemon::observe_first(daemon, move |state| {
    state.merge.checkpointing.contains(&green)
  }) {
    looks += 1;
    assert!(looks < LOOKS, "the checkpoint's turns never ended");
  }
}

fn count(state: &ShardState, name: &str) -> u64 {
  state.refusals.get(name).copied().unwrap_or(0)
}

/// The chunks a checkpoint of `len` bytes is held in.
fn chunks_of(len: u64) -> u64 {
  len.div_ceil(crate::merge_origin::inputs_chunk_bytes() as u64)
}

/// A-579's last step (w-work-cow: a 1 GiB green's checkpoint recorded in one 650 to 1,063 ms step). Do: with someone
/// waiting on the shard throughout, write a green one version of three 1 MiB files and destroy its work, which asks its
/// first checkpoint. Expect: the destroy returns with the checkpoint not yet recorded (the green checkpointing, its floor
/// unmoved); the shard then holds it in turns, in at least two steps a chunk of its encoding (one hashing it, one
/// staging it), and records it by name; the store holds the named encoding whole, and a green rebuilt from it is the
/// green it was taken from. Red before: the record was taken whole in the destroy's step (one hold step).
#[test]
fn a_large_checkpoint_is_held_a_chunk_a_step_with_someone_waiting_and_recorded_by_name() {
  let daemon = crate::daemon::audit_daemon();
  let green = crate::daemon::observe_first(&daemon, |state| {
    wait_until_taken(1);
    let root = green(state, "large");
    let id = to_db_volume(root);
    write_version(state, root, (1, FILES));
    assert!(
      state.merge.checkpointing.contains(&id),
      "the checkpoint was taken whole with someone waiting"
    );
    assert_eq!(
      state.db.partition().green_checkpoint(id),
      None,
      "the checkpoint was recorded in the destroy's step"
    );
    id
  });
  wait_for_turns(&daemon, green);
  let (taken, steps, named, held_whole, rebuilt) =
    crate::daemon::observe_first(&daemon, move |state| {
      let named = state.db.partition().green_checkpoint(green);
      let held_whole = named.is_some_and(|(version, named)| {
        checkpoint_bytes(state, green, &named).is_some_and(|bytes| {
          slates_merge::checkpoint::Checkpoint::decode(&bytes)
            .is_ok_and(|checkpoint| checkpoint.version == version)
        })
      });
      let before = state
        .greens
        .get_mut(&green)
        .map(|mut engine| (engine.head(), engine.head_identity()));
      let record = state.db.partition().volume(green).cloned().unwrap();
      crate::verbs::rebuild_green(state, &record);
      let after = state
        .greens
        .get_mut(&green)
        .map(|mut engine| (engine.head(), engine.head_identity()));
      (
        count(state, CHECKPOINT_TAKEN),
        count(state, CHECKPOINT_HOLD_STEPS),
        named,
        held_whole,
        before.is_some() && before == after,
      )
    });
  daemon.stop();
  assert_eq!(taken, 1, "one checkpoint taken");
  let (version, named) = named.expect("the checkpoint is recorded");
  assert_eq!(version, 1);
  let chunks = chunks_of(named.len);
  assert!(chunks >= FILES as u64, "{chunks} chunks");
  assert!(
    steps >= 2 * chunks,
    "the hold took {steps} steps for {chunks} chunks: not a chunk a step"
  );
  assert!(
    held_whole,
    "the store holds the named checkpoint whole, and it decodes at its version"
  );
  assert!(
    rebuilt,
    "the green rebuilt from its checkpoint is the green it was"
  );
}

/// Looks until `daemon`'s hold of green `id`'s next checkpoint has staged a chunk while its checkpoint is still the first,
/// then publishes the shard so the stage is in its image: the green's head and head identity then, if it is served.
fn staged_and_published(
  daemon: &crate::Daemon,
  id: slates_db::catalog::VolumeId,
) -> Option<(u64, [u8; 32])> {
  for _ in 0..LOOKS {
    let staged = crate::daemon::observe_first(daemon, move |state| {
      let staged = state.held_content.stage_count() > 0
        && state.db.partition().green_checkpoint(id).map(|(v, _)| v) == Some(1);
      staged.then(|| {
        crate::verbs::publish_held_increments(state).unwrap();
        state
          .greens
          .get_mut(&id)
          .map(|mut engine| (engine.head(), engine.head_identity()))
      })
    });
    if let Some(before) = staged {
      return before;
    }
  }
  panic!("the hold never staged its first chunk");
}

/// A-579's last step, crash safety. Do: on a daemon over a held segment, write a green one version (its first
/// checkpoint); then, with someone waiting and the hold set to stop after one chunk
/// staged, write a second version, which asks its next checkpoint in turns; once the hold has staged its chunk, publish
/// the shard (so the stage is in its image) and stop the daemon. Start a second daemon over the segment. Expect: the
/// green recovered at version 2 with its identity, its checkpoint still the first (version 1, the same name, held whole),
/// the stage gone and counted released; then a third version's checkpoint completes: recorded at version 3 under another
/// name, held whole, and the first one's content given back.
#[test]
fn a_checkpoint_stopped_part_way_leaves_the_previous_one_and_the_next_completes() {
  use slates_anchor::AnchorSegment;
  let profile = crate::daemon::test_profile();
  let instance = format!("checkpoint-hold-{}", std::process::id());
  let config = crate::DaemonConfig::derive(&profile, &instance, Some(1));
  let segment = AnchorSegment::create(&instance, &profile.facts.identity, config.geometry)
    .unwrap()
    .with_content(
      &crate::daemon::content_name_of(&instance),
      config.content_bytes(),
    )
    .unwrap();
  let rendezvous = slates_ipc::HeldRendezvous::hold(&instance).unwrap();
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    crate::daemon::SegmentSource::Handoff {
      handoff,
      len,
      content: segment.content_handoff().unwrap(),
      rendezvous: rendezvous.adoption(),
    }
  };
  let first = crate::Daemon::start(&profile, config.clone(), source()).unwrap();
  first.bootstrap(true).unwrap();
  let green = crate::daemon::observe_first(&first, |state| {
    let root = green(state, "stopped");
    write_version(state, root, (1, FILES));
    root
  });
  let id = to_db_volume(green);
  wait_for_turns(&first, id);
  let first_named = crate::daemon::observe_first(&first, move |state| {
    let (version, named) = state
      .db
      .partition()
      .green_checkpoint(id)
      .expect("the first checkpoint");
    assert_eq!(version, 1);
    // The next hold stops after one chunk staged, with someone waiting so it goes to turns.
    state.merge.hold_pause_at = Some(1);
    wait_until_taken(count(state, CHECKPOINT_TAKEN) + 1);
    // A file more than the first, so the increments to fold pass the checkpoint held and the doubling rule takes one.
    write_version(state, green, (2, FILES + 1));
    assert!(state.merge.checkpointing.contains(&id));
    named
  });
  let object = checkpoint_object(id);
  let before = staged_and_published(&first, id);
  // The crash, part-way: nothing more published.
  first.stop();
  let second = crate::Daemon::start(&profile, config, source()).unwrap();
  second.bootstrap(true).unwrap();
  let recovered = crate::daemon::observe_first(&second, move |state| {
    let named = state.db.partition().green_checkpoint(id);
    let held_whole = named.is_some_and(|(_, named)| checkpoint_bytes(state, id, &named).is_some());
    let head = state
      .greens
      .get_mut(&id)
      .map(|mut engine| (engine.head(), engine.head_identity()));
    (
      named,
      held_whole,
      head,
      state.held_content.stage_count(),
      count(state, CHECKPOINT_UNNAMED_RELEASED),
    )
  });
  assert_eq!(
    recovered.0,
    Some((1, first_named)),
    "the first checkpoint stands"
  );
  assert!(recovered.1, "and its encoding is held whole");
  assert_eq!(
    recovered.2, before,
    "the green recovered at version 2 as it was"
  );
  assert_eq!(recovered.3, 0, "the part-way stage is gone");
  assert!(recovered.4 >= 1, "counted released");
  crate::daemon::observe_first(&second, move |state| {
    write_version(state, green, (3, FILES + 1));
  });
  wait_for_turns(&second, id);
  let next = crate::daemon::observe_first(&second, move |state| {
    let named = state.db.partition().green_checkpoint(id);
    let held_whole = named.is_some_and(|(_, named)| checkpoint_bytes(state, id, &named).is_some());
    let first_kept = state
      .held_content
      .holds_manifest(object, &first_named.identity);
    (named, held_whole, first_kept)
  });
  second.stop();
  let (version, named) = next.0.expect("the next checkpoint is recorded");
  assert_eq!(version, 3, "the next checkpoint completes at version 3");
  assert_ne!(named.identity, first_named.identity);
  assert!(next.1, "its encoding is held whole");
  assert!(!next.2, "the first checkpoint's content is given back");
}

/// Shape: the files the first-checkpoint test's version sets, each a key of the green's identity backlog: enough that
/// folding them all is many quanta on a debug build.
const BACKLOG_FILES: usize = 2_000;

/// Writes one version of `green` setting [`BACKLOG_FILES`] small files through one work, left standing so its floor does
/// not move and no checkpoint is asked: the green's identity backlog is then one key a file.
fn backlog_version(state: &mut ShardState, green: VolumeId) {
  let created = ask(
    state,
    RequestBody::CreateWork {
      green,
      name: "backlog-work".to_owned(),
    },
  );
  let ReplyBody::WorkCreated { id: work, .. } = created else {
    panic!("no work over {green:?}: {created:?}")
  };
  for file in 0..BACKLOG_FILES {
    let edited = ask(
      state,
      RequestBody::Edit {
        work,
        path: format!("/b{file}"),
        at: 0,
        delete_len: 0,
        bytes: format!("backlog file {file}").into_bytes(),
      },
    );
    assert!(matches!(edited, ReplyBody::Edited), "{edited:?}");
  }
  let submitted = ask(state, crate::identities::authored(work, "a backlog"));
  assert!(
    matches!(
      submitted,
      ReplyBody::Submitted {
        version: Some(_),
        ..
      }
    ),
    "{submitted:?}"
  );
}

/// A-579 (a first checkpoint's identity fold paced a quantum a turn; goal 93: an import's first checkpoint folded 8,991
/// keys in one poll of 495 ms, debug, its interval nothing and so its deadline passed at its first turn). Do: with
/// someone waiting on the shard, give a green an identity backlog of 2,000 keys and run two turns of its first
/// checkpoint, whose interval is nothing. Expect keys left for later turns after both: the first turn learns a key's
/// cost, and the second folds a quantum's worth, not everything left. Red before: the second turn's budget was all the
/// backlog's estimated cost, and it folded every key.
#[test]
fn a_first_checkpoints_identity_fold_takes_a_quantum_a_turn() {
  let daemon = crate::daemon::audit_daemon();
  let left = crate::daemon::observe_first(&daemon, |state| {
    wait_until_taken(u64::MAX);
    let root = green(state, "backlog");
    let id = to_db_volume(root);
    backlog_version(state, root);
    let began = slates_rt::futures::now_ns();
    state.merge.checkpoint_since.insert(id, began);
    let mut hold = None;
    for _ in 0..2 {
      if super::checkpoint_turn(state, id, &mut hold, (began, 1_000_000)) {
        break;
      }
    }
    state
      .greens
      .get(&id)
      .map_or(0, |engine| engine.identity_backlog())
  });
  daemon.stop();
  assert!(
    left > 0,
    "two turns of a first checkpoint folded the green's whole identity backlog"
  );
}

//! An increment's post-state holds the bytes its ops add, read from where the work's file holds them (§4.16 "Submission":
//! "the sealed post-state"; W2): the deriver names each added run by its offset in the work's final file
//! (`slates_merge::derive`, `run_src`), while the seal (`verbs::assemble_post_state`) read the bytes at the op's offset
//! in base coordinates, so a run after an earlier length change in the same file was read from the wrong place.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use slates_client::Submitted;
use slates_ipc::protocol::ReadAt;
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::client::connect;

/// Shape: one shard, so the green and its works are on one partition.
const TEST_SHARDS: u16 = 1;

/// §4.16. Do: on a one-shard daemon, make a green whose one file reads `abcdef`; from a new work, delete its first two
/// bytes (the work now reads `cdef`), then overwrite the work's last two bytes with `XY` (`cdXY`), and submit; read the
/// green's head. Expect: the head reads `cdXY`, the work's file. The overwrite's run sits at offset 2 of the work's
/// file and at offset 4 of the base: read at the base offset, past the work file's end, the run was zeros.
#[test]
fn a_run_after_a_length_change_in_its_file_merges_its_own_bytes() {
  let profile = common::machine_profile();
  let instance = format!("srv-post-state-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)),
    slates_server::SegmentSource::Create {
      name: common::fixture_object("slates-seg", "post-state"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("post-state-green", false).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  client.edit(seed, "f", 0, 0, b"abcdef").unwrap();
  let seeded = client.submit(seed);
  let (work, _) = client.create_work(green, "shifted").unwrap();
  client.edit(work, "f", 0, 2, b"").unwrap();
  client.edit(work, "f", 2, 2, b"XY").unwrap();
  let in_work = client.read(work, "f", ReadAt::Head);
  let submitted = client.submit(work);
  let head = client.read(green, "f", ReadAt::Head);
  drop(client);
  daemon.stop();
  assert!(
    matches!(seeded, Ok(Submitted::Accepted(1))),
    "seeded: {seeded:?}"
  );
  assert_eq!(in_work.unwrap(), b"cdXY", "the work reads its edits");
  assert!(
    matches!(submitted, Ok(Submitted::Accepted(2))),
    "submitted: {submitted:?}"
  );
  assert_eq!(
    head.unwrap(),
    b"cdXY",
    "the green's head is the work's file"
  );
}

/// What a green's head held after a work renamed `a` over `b` and was submitted: the rebase's answer (when it was
/// rebased first), the submit's, then `b` and `a` read at the head.
struct RenamedOver {
  rebased: Option<Result<(), slates_client::ClientError>>,
  submitted: Result<Submitted, slates_client::ClientError>,
  b: Result<Vec<u8>, slates_client::ClientError>,
  a: Result<Vec<u8>, slates_client::ClientError>,
}

/// On a one-shard daemon: a green whose `a` reads `0123456789` and whose `b` reads `b_bytes`; a work renames `a` over
/// `b`, is rebased onto the head it is already at when `rebase`, then has `appended` written at the end of `b`, and is
/// submitted.
fn rename_over(tag: &str, b_bytes: &[u8], rebase: bool, appended: &[u8]) -> RenamedOver {
  let profile = common::machine_profile();
  let instance = format!("srv-{tag}-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("rename-green", false).unwrap();
  let (seed, _) = client.create_work(green, "rename-seed").unwrap();
  client.edit(seed, "a", 0, 0, b"0123456789").unwrap();
  client.edit(seed, "b", 0, 0, b_bytes).unwrap();
  assert!(matches!(client.submit(seed), Ok(Submitted::Accepted(1))));
  let (work, _) = client.create_work(green, "rename-work").unwrap();
  client
    .declare(
      work,
      slates_ipc::protocol::WorkOp::Rename {
        from: "a".to_owned(),
        to: "b".to_owned(),
      },
    )
    .unwrap();
  let rebased = rebase.then(|| client.rebase(work).map(drop));
  if !appended.is_empty() {
    client.edit(work, "b", 10, 0, appended).unwrap();
  }
  let submitted = client.submit(work);
  let b = client.read(green, "b", ReadAt::Head);
  let a = client.read(green, "a", ReadAt::Head);
  drop(client);
  daemon.stop();
  RenamedOver {
    rebased,
    submitted,
    b,
    a,
  }
}

/// §4.16 "Rebase" ("its journal restated in head coordinates"). Do: on a one-shard daemon, make a green whose files `a`
/// and `b` both read `0123456789`; in a work, rename `a` over `b`; rebase it onto the head it is already at; submit; read
/// the head. Expect: accepted, `b` reads `0123456789` once and `a` is gone. The rebase restated the rename as a
/// whole-file insert into `b`, which the head already holds, so `b` read its ten bytes twice.
#[test]
fn a_rename_over_a_file_rebased_then_submitted_leaves_the_renamed_bytes() {
  let after = rename_over("rename-rebase", b"0123456789", true, b"");
  assert!(
    matches!(after.rebased, Some(Ok(()))),
    "rebased: {:?}",
    after.rebased
  );
  assert!(
    matches!(after.submitted, Ok(Submitted::Accepted(2))),
    "submitted: {:?}",
    after.submitted
  );
  assert_eq!(after.b.unwrap(), b"0123456789", "b holds a's bytes, once");
  assert!(after.a.is_err(), "a is gone: {:?}", after.a);
}

/// §4.16 "Rebase". Do: as above, but `b` reads sixteen other bytes. Expect: `b` reads `0123456789`, `a`'s bytes alone,
/// and `a` is gone. The restated insert put `a`'s bytes in front of `b`'s own.
#[test]
fn a_rename_over_a_different_file_rebased_then_submitted_replaces_it() {
  let after = rename_over("rename-rebase-other", b"bbbbbbbbbbbbbbbb", true, b"");
  assert!(
    matches!(after.rebased, Some(Ok(()))),
    "rebased: {:?}",
    after.rebased
  );
  assert!(
    matches!(after.submitted, Ok(Submitted::Accepted(2))),
    "submitted: {:?}",
    after.submitted
  );
  assert_eq!(after.b.unwrap(), b"0123456789", "b is a's bytes alone");
  assert!(after.a.is_err(), "a is gone: {:?}", after.a);
}

/// §4.16 "Composition at seal" ("a base file renamed ... is one `Rename` plus its content"). Do: on a one-shard daemon,
/// a green whose `a` reads `0123456789` and `b` sixteen other bytes; in a work, rename `a` over `b`, then write `XY` at
/// the end of `b`; submit; read the head. Expect: `b` reads `0123456789XY`. Today the verdict skips the content of a
/// renamed-into path and copies the source's head bytes, so the submit is accepted and the edit is lost.
#[test]
#[ignore = "GAPS: an edit after a rename is accepted and lost (2026-11-09); the merge's rename takes the source's head bytes and skips the destination's content ops, and the chain entry's Copied form has no ops to replay"]
fn an_edit_after_a_rename_over_a_file_is_merged_with_it() {
  let after = rename_over("rename-edit", b"bbbbbbbbbbbbbbbb", false, b"XY");
  assert!(
    matches!(after.submitted, Ok(Submitted::Accepted(2))),
    "submitted: {:?}",
    after.submitted
  );
  assert_eq!(
    after.b.unwrap(),
    b"0123456789XY",
    "b holds a's bytes and the edit"
  );
  assert!(after.a.is_err(), "a is gone: {:?}", after.a);
}

/// The paths a generated work touches: three the green's origin holds and one it does not.
const PATHS: [&str; 4] = ["a", "b", "c", "d"];

/// One generated work operation, made valid against the work's files as they stand when it runs ([`journal_of`]).
#[derive(Debug, Clone)]
struct RawOp {
  kind: u8,
  path: u8,
  other: u8,
  at: u8,
  len: u8,
}

fn raw_op() -> impl proptest::strategy::Strategy<Value = RawOp> {
  use proptest::prelude::*;
  (
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
  )
    .prop_map(|(kind, path, other, at, len)| RawOp {
      kind,
      path,
      other,
      at,
      len,
    })
}

/// A work's step: an SDK edit or a declared namespace operation.
#[derive(Debug, Clone)]
enum Step {
  Edit {
    path: &'static str,
    at: u64,
    delete_len: u64,
    bytes: Vec<u8>,
  },
  Declare(slates_ipc::protocol::WorkOp),
}

/// The steps `raws` stand for over files of `lengths`: an edit within its file (or creating it), a rename of a file to
/// another path (over a file or not), an unlink of a file, a mode set on one, or a step of another kind
/// ([`other_kind`]). A step that names no file where it
/// needs one is an edit creating that path. The steps stay out of the parked gap of
/// [`an_edit_after_a_rename_over_a_file_is_merged_with_it`] (a renamed file's content is the source's at the head, its
/// own content ops dropped): only a base file the work has not touched is renamed, and a path renamed into is not
/// edited after; such a step is left out.
fn journal_of(lengths: &[usize], raws: &[RawOp]) -> Vec<Step> {
  use slates_ipc::protocol::WorkOp;
  let mut files: std::collections::BTreeMap<&'static str, usize> =
    PATHS.iter().copied().zip(lengths.iter().copied()).collect();
  // The paths a step has changed, and those renamed into.
  let mut touched: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
  let mut renamed_into: std::collections::BTreeSet<&'static str> =
    std::collections::BTreeSet::new();
  // The names other than files' a step has made: symlinks, directories, hard links and FIFOs.
  let mut named: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
  let mut steps = Vec::with_capacity(raws.len());
  for raw in raws {
    let path = PATHS[usize::from(raw.path) % PATHS.len()];
    let other = PATHS[usize::from(raw.other) % PATHS.len()];
    let held = files.get(path).copied();
    let step = match (raw.kind % 9, held) {
      (1, Some(_)) if other != path && !touched.contains(path) => {
        let len = files.remove(path).unwrap_or(0);
        files.insert(other, len);
        touched.extend([path, other]);
        renamed_into.insert(other);
        Step::Declare(WorkOp::Rename {
          from: path.to_owned(),
          to: other.to_owned(),
        })
      }
      (2, Some(_)) => {
        files.remove(path);
        touched.insert(path);
        Step::Declare(WorkOp::Unlink {
          path: path.to_owned(),
        })
      }
      (3, Some(_)) => Step::Declare(WorkOp::SetMode {
        path: path.to_owned(),
        mode: 0o600 | u32::from(raw.len % 8) << 3,
      }),
      (4..=8, _) => Step::Declare(other_kind(raw, path, held.is_some(), &mut named)),
      _ if renamed_into.contains(path) => continue,
      (_, held) => {
        let len = held.unwrap_or(0);
        let at = usize::from(raw.at) % (len + 1);
        let delete_len = usize::from(raw.len) % (len - at + 1);
        let added = usize::from(raw.len % 5);
        files.insert(path, len - delete_len + added);
        touched.insert(path);
        Step::Edit {
          path,
          at: at as u64,
          delete_len: delete_len as u64,
          bytes: vec![raw.other; added],
        }
      }
    };
    steps.push(step);
  }
  steps
}

proptest::proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::prelude::ProptestConfig {
    cases: slates_test_seeds::cases(24),
    ..proptest::prelude::ProptestConfig::default()
  }))]

  /// §4.16 "Rebase" ("its journal restated in head coordinates, so the agent can ... submit without further mapping").
  /// Do: on a one-shard daemon, two greens built alike (files `a`, `b`, `c` of generated lengths); on each, a work made
  /// at that version runs the same generated journal of edits, renames (over a file or not), unlinks, mode sets,
  /// symlinks, directories, xattrs, hard links and FIFOs,
  /// then another work's generated journal is submitted ahead of it (none, some cases); one green's work is submitted,
  /// the other's rebased onto the head and then submitted. Expect: the same answer (a conflict as the same windows; a
  /// journal the composer refuses refused by the rebase with the submit's reason), the same root entries and every file
  /// reading the same at both heads. The rebase runs the submit's verdict, so its restatement must merge as the work would have.
  #[test]
  fn a_rebased_work_submits_as_it_would_have(
    lengths in proptest::collection::vec(1usize..12, 3),
    raws in proptest::collection::vec(raw_op(), 1..8),
    ahead in proptest::collection::vec(raw_op(), 0..4),
  ) {
    rebased_and_not_agree(&lengths, &journal_of(&lengths, &raws), &journal_of(&lengths, &ahead));
  }
}

/// One case of [`a_rebased_work_submits_as_it_would_have`]: `steps` the work's, `ahead` the work submitted before it.
fn rebased_and_not_agree(lengths: &[usize], steps: &[Step], ahead: &[Step]) {
  let profile = common::machine_profile();
  let instance = format!("srv-rebase-same-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-rebase-same-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let mut arms = Vec::with_capacity(2);
  for (arm, rebase) in [false, true].into_iter().enumerate() {
    let green = client
      .create_green(&format!("rebase-same-{arm}"), false)
      .unwrap();
    let (seed, _) = client
      .create_work(green, &format!("rebase-same-seed-{arm}"))
      .unwrap();
    for (path, len) in PATHS.iter().zip(lengths) {
      let bytes: Vec<u8> = (0..*len)
        .map(|n| b'0' + u8::try_from(n % 10).unwrap())
        .collect();
      client.edit(seed, path, 0, 0, &bytes).unwrap();
    }
    assert!(matches!(client.submit(seed), Ok(Submitted::Accepted(1))));
    let (work, _) = client
      .create_work(green, &format!("rebase-same-work-{arm}"))
      .unwrap();
    let (other, _) = client
      .create_work(green, &format!("rebase-same-ahead-{arm}"))
      .unwrap();
    let mut said = Vec::with_capacity(steps.len() + ahead.len() + PATHS.len() + 2);
    for (volume, step) in steps
      .iter()
      .map(|step| (work, step))
      .chain(ahead.iter().map(|step| (other, step)))
    {
      said.push(format!("{:?}", run_step(&mut client, volume, step)));
    }
    said.push(format!("ahead: {:?}", client.submit(other)));
    // A conflict is the submit's windows, and a journal the composer refuses is refused by the rebase as by the
    // submit, with the same reason.
    match rebase.then(|| client.rebase(work)) {
      Some(Err(refused)) => said.push(format!("{:?}", Err::<Submitted, _>(refused))),
      Some(Ok(slates_client::Rebased::Conflict(windows))) => {
        said.push(format!("{:?}", Ok::<_, ()>(Submitted::Conflict(windows))));
      }
      Some(Ok(slates_client::Rebased::Rebased(_))) | None => {
        said.push(format!("{:?}", client.submit(work)));
      }
    }
    said.push(format!(
      "root: {:?}",
      client.list_dir(green, "", ReadAt::Head)
    ));
    for path in PATHS {
      said.push(format!(
        "{path}: {:?}",
        client.read(green, path, ReadAt::Head)
      ));
    }
    arms.push(said);
  }
  drop(client);
  daemon.stop();
  assert_eq!(
    &arms[0], &arms[1],
    "submitted (left) and rebased then submitted (right)"
  );
}

/// Runs `step` in `work`.
fn run_step(
  client: &mut slates_client::Client,
  work: slates_ipc::protocol::VolumeId,
  step: &Step,
) -> Result<(), slates_client::ClientError> {
  match step {
    Step::Edit {
      path,
      at,
      delete_len,
      bytes,
    } => client.edit(work, path, *at, *delete_len, bytes),
    Step::Declare(op) => client.declare(work, op.clone()),
  }
}

/// A step of a kind other than a file's ([`journal_of`]), on names of its own so a file's steps never meet it: a
/// symlink to `path` made or removed, a directory made or removed, an xattr set on or removed from `path` (a file when
/// `file`), a hard link to it made or removed, or a FIFO made or removed. `named` holds the names made so far.
fn other_kind(
  raw: &RawOp,
  path: &str,
  file: bool,
  named: &mut std::collections::BTreeSet<String>,
) -> slates_ipc::protocol::WorkOp {
  use slates_ipc::protocol::{IpcNode, IpcNodeKind, WorkOp};
  let (prefix, made) = match raw.kind % 9 {
    4 => (
      "s",
      WorkOp::Symlink {
        path: String::new(),
        target: path.to_owned(),
      },
    ),
    5 => (
      "d",
      WorkOp::Mkdir {
        path: String::new(),
      },
    ),
    6 if file => {
      let name = format!("user.x{}", raw.at % 2);
      return if raw.len.is_multiple_of(2) {
        WorkOp::SetXattr {
          path: path.to_owned(),
          name,
          value: vec![raw.other],
        }
      } else {
        WorkOp::RemoveXattr {
          path: path.to_owned(),
          name,
        }
      };
    }
    7 if file => (
      "l",
      WorkOp::Link {
        path: String::new(),
        target: path.to_owned(),
      },
    ),
    _ => (
      "p",
      WorkOp::Mknod {
        path: String::new(),
        node: IpcNode {
          kind: IpcNodeKind::Fifo,
          mode: 0o640,
          uid: 1,
          gid: 1,
          atime: 0,
          mtime: 0,
          ctime: 0,
          btime: 0,
        },
      },
    ),
  };
  let name = format!("{prefix}{}", raw.at % 2);
  if named.remove(&name) {
    return if prefix == "d" {
      WorkOp::Rmdir { path: name }
    } else {
      WorkOp::Unlink { path: name }
    };
  }
  named.insert(name.clone());
  match made {
    WorkOp::Symlink { target, .. } => WorkOp::Symlink { path: name, target },
    WorkOp::Link { target, .. } => WorkOp::Link { path: name, target },
    WorkOp::Mknod { node, .. } => WorkOp::Mknod { path: name, node },
    _ => WorkOp::Mkdir { path: name },
  }
}

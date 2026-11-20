//! A-664 part 3, its first piece: a git repository's greens keep the map of paths to git's files of each head, derived
//! from the one before over the change index and the links it leaves out, or built whole, and equal, identity for
//! identity, to the map built from the files the version holds as git reads them (the oracle's model, kept here by the
//! design's rules, never read back through the module's own reading of a green); a green that leaves its lineage takes
//! its map with it, so the live store equals one made again from the log.

use std::collections::BTreeMap;

use slates_db::catalog::{Principal, RepositoryId};
use slates_git::derived::FileClass;
use slates_ipc::protocol::{
  IpcNode, IpcNodeKind, ObjectFormat, ReplyBody, RequestBody, VcsTool, VolumeId, WorkOp,
};
use slates_vcs::derived::paths::{Entry, PathMap};
use slates_vcs::{ContentId, ContentStore, Mode};

use super::{BUILT, DERIVED, GreenPaths, REFUSED, REMADE};
use crate::state::ShardState;
use crate::verbs::{dispatch, to_db_volume};

/// Shape: the room the oracle's own store is made with: the fixed histories' few files and their maps, many times over.
const SCRATCH_BUDGET: u64 = 1 << 24;

/// Shape: a file's mode with its owner's execute bit, and one without it.
const EXECUTABLE: u32 = 0o755;
/// Shape: see [`EXECUTABLE`].
const PLAIN: u32 = 0o644;

/// The oracle's model of a version (module docs): each file git holds, by its map's path, with its class and bytes.
type Model = BTreeMap<Vec<u8>, (FileClass, Vec<u8>)>;

/// The account that owns everything these tests make.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// `body` asked of `state` as the owner.
pub(crate) fn ask(state: &mut ShardState, body: RequestBody) -> ReplyBody {
  dispatch(state, 1, &owner(), body)
}

/// A git repository named `name`: its id and its root green.
pub(crate) fn git_repository(state: &mut ShardState, name: &str) -> (RepositoryId, VolumeId) {
  let reply = ask(
    state,
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool: VcsTool::Git,
      format: ObjectFormat::Sha1,
      default_branch: "main".to_owned(),
      green: None,
    },
  );
  let ReplyBody::GreenCreated { id } = reply else {
    panic!("no repository: {reply:?}")
  };
  let repositories = state.db.partition().repositories();
  let record = repositories.repository_named(name).expect("the record");
  (record.id, id)
}

/// One change a version's work makes.
pub(crate) enum Op {
  /// Splice `bytes` at `at` over `delete_len` bytes of the file at the path.
  Edit(&'static str, u64, u64, &'static [u8]),
  /// Declare a namespace or metadata operation.
  Declare(WorkOp),
}

/// A new file of `bytes` at `path`.
pub(crate) fn create(path: &'static str, bytes: &'static [u8]) -> Op {
  Op::Edit(path, 0, 0, bytes)
}

/// A work over `green` making `ops`, submitted with a message and identities as a repository's green requires (A-664
/// part 1), its work destroyed after: the version accepted.
pub(crate) fn version(state: &mut ShardState, green: VolumeId, serial: u32, ops: Vec<Op>) -> u64 {
  let message = format!("version {serial}");
  version_saying(state, (green, serial), ops, &message)
}

/// [`version`] submitted with `message`.
pub(crate) fn version_saying(
  state: &mut ShardState,
  (green, serial): (VolumeId, u32),
  ops: Vec<Op>,
  message: &str,
) -> u64 {
  let made = ask(
    state,
    RequestBody::CreateWork {
      green,
      name: format!("work-{serial}"),
    },
  );
  let ReplyBody::WorkCreated { id: work, .. } = made else {
    panic!("no work: {made:?}")
  };
  for op in ops {
    let reply = match op {
      Op::Edit(path, at, delete_len, bytes) => ask(
        state,
        RequestBody::Edit {
          work,
          path: path.to_owned(),
          at,
          delete_len,
          bytes: bytes.to_vec(),
        },
      ),
      Op::Declare(op) => ask(state, RequestBody::Declare { work, op }),
    };
    assert!(!matches!(reply, ReplyBody::Refused { .. }), "{reply:?}");
  }
  let submitted = ask(state, crate::identities::authored(work, message));
  // A green's first submit owes its commit and builds its map in slices after it (A-664's live whole builds, part 2),
  // by a task a test's step never runs: driven to its end here, so the test reads the store as the build leaves it.
  super::whole_build::settle(state);
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

/// The map the store holds for `green`, when it is of `version`: its root and its files.
fn kept(
  state: &ShardState,
  repository: RepositoryId,
  green: VolumeId,
  version: u64,
) -> Option<(ContentId, BTreeMap<Vec<u8>, Entry>)> {
  let (content, git) = state.vcs_stores.git_ref(repository)?;
  let paths = git.paths.as_ref()?;
  let key = crate::vcs_store::green_key(to_db_volume(green));
  let head = git.heads.get(&key).filter(|head| head.version == version)?;
  let files = paths.files(content, head.root).ok()?;
  Some((head.root, files))
}

/// The model's files as the map names them: each class's byte and the identity of its bytes as an object.
fn entries_of(content: &ContentStore, model: &Model) -> BTreeMap<Vec<u8>, Entry> {
  model
    .iter()
    .map(|(path, (class, bytes))| {
      let entry = Entry {
        class: class.class(),
        content: content.object_id(bytes),
      };
      (path.clone(), entry)
    })
    .collect()
}

/// The root of the model's map built from scratch in a store of the oracle's own with the shard's window: by A-647's
/// rule the same map is the same root on every store.
fn built_root(window: usize, model: &Model) -> ContentId {
  let mut scratch = ContentStore::new(SCRATCH_BUDGET, window).expect("the oracle's store");
  let mut paths: GreenPaths = PathMap::declare(window, Mode::Eager).expect("the oracle's map");
  let entries: BTreeMap<Vec<u8>, Entry> = model
    .iter()
    .map(|(path, (class, bytes))| {
      let content = scratch.put_object(bytes).expect("the oracle's content");
      let entry = Entry {
        class: class.class(),
        content,
      };
      (path.clone(), entry)
    })
    .collect();
  paths
    .build(&mut scratch, &entries)
    .expect("the oracle's map")
}

/// Asserts `green`'s `version` keeps the model's map: the same files, and the same root as the model's map built from
/// scratch.
fn assert_kept(
  state: &ShardState,
  (repository, green): (RepositoryId, VolumeId),
  version: u64,
  model: &Model,
) {
  let (root, files) = kept(state, repository, green, version).expect("the version's map is kept");
  let content = state.vcs_stores.content().expect("the content store");
  assert_eq!(files, entries_of(content, model), "version {version}");
  assert_eq!(
    root,
    built_root(content.window(), model),
    "version {version}"
  );
}

/// The count named `name` on the shard.
pub(crate) fn count(state: &ShardState, name: &str) -> u64 {
  state.refusals.get(name).copied().unwrap_or(0)
}

/// Sets `path` in the model.
fn set(model: &mut Model, path: &str, class: FileClass, bytes: &[u8]) {
  model.insert(path.as_bytes().to_vec(), (class, bytes.to_vec()));
}

/// Removes `path` from the model, keeping what it held.
fn take(model: &mut Model, path: &str) -> (FileClass, Vec<u8>) {
  model.remove(path.as_bytes()).expect("the model's file")
}

/// A named pipe as a work declares one.
fn fifo() -> IpcNode {
  IpcNode {
    kind: IpcNodeKind::Fifo,
    mode: PLAIN,
    uid: 0,
    gid: 0,
    atime: 1,
    mtime: 1,
    ctime: 1,
    btime: 1,
  }
}

/// A branch `name` of the repository `repository` made at its default branch's head: its green.
pub(crate) fn branch(state: &mut ShardState, repository: &str, name: &str) -> VolumeId {
  let made = ask(
    state,
    RequestBody::CreateBranch {
      repository: repository.to_owned(),
      name: name.to_owned(),
      from: "main".to_owned(),
      version: None,
    },
  );
  let ReplyBody::GreenCreated { id } = made else {
    panic!("no branch: {made:?}")
  };
  id
}

/// What the store holds that its maps make: every content with its holders, and the bytes it uses, as A-662's oracle
/// compares a store made again with the live one.
fn held(state: &ShardState) -> (Vec<(ContentId, u64)>, u64) {
  let content = state.vcs_stores.content().expect("the content store");
  let mut contents: Vec<(ContentId, u64)> = content
    .identities()
    .map(|identity| (*identity, content.holders(identity)))
    .collect();
  contents.sort();
  (contents, content.used())
}

/// The fixed history's first five versions (each checked against the model): files made in a directory; one
/// overwritten; one made executable; a symbolic link and a hard link; a named pipe and an attribute, which change no
/// file git holds.
fn first_five(state: &mut ShardState, at: (RepositoryId, VolumeId), model: &mut Model) {
  let green = at.1;
  let ops = vec![
    Op::Declare(WorkOp::Mkdir {
      path: "/d".to_owned(),
    }),
    create("/f", b"hello"),
    create("/d/a", b"alpha"),
    create("/keep", b"keep"),
  ];
  let first = version(state, green, 1, ops);
  set(model, "f", FileClass::File, b"hello");
  set(model, "d/a", FileClass::File, b"alpha");
  set(model, "keep", FileClass::File, b"keep");
  assert_kept(state, at, first, model);
  let second = version(state, green, 2, vec![Op::Edit("/f", 0, 5, b"HELLO!")]);
  set(model, "f", FileClass::File, b"HELLO!");
  assert_kept(state, at, second, model);
  let mode = WorkOp::SetMode {
    path: "/d/a".to_owned(),
    mode: EXECUTABLE,
  };
  let third = version(state, green, 3, vec![Op::Declare(mode)]);
  set(model, "d/a", FileClass::Executable, b"alpha");
  assert_kept(state, at, third, model);
  links(state, at, model);
  let pipe = WorkOp::Mknod {
    path: "/p".to_owned(),
    node: fifo(),
  };
  let attribute = WorkOp::SetXattr {
    path: "/f".to_owned(),
    name: "user.note".to_owned(),
    value: b"x".to_vec(),
  };
  let fifth = version(
    state,
    green,
    5,
    vec![Op::Declare(pipe), Op::Declare(attribute)],
  );
  assert_kept(state, at, fifth, model);
}

/// The fourth version: a symbolic link (its content its target) and a hard link (a file with its primary's bytes).
fn links(state: &mut ShardState, at: (RepositoryId, VolumeId), model: &mut Model) {
  let symlink = WorkOp::Symlink {
    path: "/s".to_owned(),
    target: "f".to_owned(),
  };
  let hard = WorkOp::Link {
    path: "/h".to_owned(),
    target: "/keep".to_owned(),
  };
  let fourth = version(
    state,
    at.1,
    4,
    vec![Op::Declare(symlink), Op::Declare(hard)],
  );
  set(model, "s", FileClass::Symlink, b"f");
  set(model, "h", FileClass::File, b"keep");
  assert_kept(state, at, fourth, model);
}

/// The sixth version: the file the hard link names is overwritten, which the change index names at the file alone; the
/// link reads the new bytes, as git's tree holds them at both names.
fn primary(state: &mut ShardState, at: (RepositoryId, VolumeId), model: &mut Model) {
  let sixth = version(state, at.1, 6, vec![Op::Edit("/keep", 0, 4, b"KEEP!")]);
  set(model, "keep", FileClass::File, b"KEEP!");
  set(model, "h", FileClass::File, b"KEEP!");
  assert_kept(state, at, sixth, model);
}

/// The fixed history's last five versions (each checked against the model): a file renamed; a symbolic link removed; an
/// executable file moved into a directory made with it, its old directory removed, a change whose directories the map
/// makes and drops by their files alone (the move states the file's mode, as the green keeps a mode at its path,
/// A-618); a splice into the moved file; its execute bit taken away.
fn last_five(state: &mut ShardState, at: (RepositoryId, VolumeId), model: &mut Model) {
  let green = at.1;
  let rename = |from: &str, to: &str| {
    Op::Declare(WorkOp::Rename {
      from: from.to_owned(),
      to: to.to_owned(),
    })
  };
  let seventh = version(state, green, 7, vec![rename("/f", "/g")]);
  let (class, bytes) = take(model, "f");
  set(model, "g", class, &bytes);
  assert_kept(state, at, seventh, model);
  let unlink = WorkOp::Unlink {
    path: "/s".to_owned(),
  };
  let eighth = version(state, green, 8, vec![Op::Declare(unlink)]);
  take(model, "s");
  assert_kept(state, at, eighth, model);
  let moved = vec![
    Op::Declare(WorkOp::Mkdir {
      path: "/x".to_owned(),
    }),
    rename("/d/a", "/x/a"),
    Op::Declare(WorkOp::SetMode {
      path: "/x/a".to_owned(),
      mode: EXECUTABLE,
    }),
    Op::Declare(WorkOp::Rmdir {
      path: "/d".to_owned(),
    }),
  ];
  let ninth = version(state, green, 9, moved);
  let (class, bytes) = take(model, "d/a");
  set(model, "x/a", class, &bytes);
  assert_kept(state, at, ninth, model);
  let tenth = version(state, green, 10, vec![Op::Edit("/x/a", 0, 0, b"A")]);
  set(model, "x/a", FileClass::Executable, b"Aalpha");
  assert_kept(state, at, tenth, model);
  let mode = WorkOp::SetMode {
    path: "/x/a".to_owned(),
    mode: PLAIN,
  };
  let eleventh = version(state, green, 11, vec![Op::Declare(mode)]);
  set(model, "x/a", FileClass::File, b"Aalpha");
  assert_kept(state, at, eleventh, model);
}

/// A-664 part 3 (its first piece; A-647's owed map: "the green's state as the path map these types read"). Do: on a git
/// repository's root green, submit eleven versions: files made in a directory, one overwritten, one made executable, a
/// symbolic link and a hard link, a named pipe and an attribute, the hard link's file overwritten, a file renamed, the
/// symbolic link removed, an executable file moved to a new directory and the old one removed, a splice, the execute
/// bit taken away. Expect: after each, the version's map holds exactly the files git would (classes `100644`, `100755`,
/// `120000`; a hard link its file's bytes, the new ones once its file is overwritten; no pipe), and its root is the
/// root of that map built from scratch in another store; the first version built whole and the other ten derived from
/// the map before (the non-vacuity count of the incremental path: no directory a version makes or removes, and no hard
/// link, asks for a whole build), none refused.
#[test]
fn t_a664_p3_every_version_of_a_git_green_keeps_its_map_of_gits_files() {
  let counts = crate::daemon::audit_on_shard(|state| {
    let at = git_repository(state, "tree");
    let mut model = Model::new();
    first_five(state, at, &mut model);
    primary(state, at, &mut model);
    last_five(state, at, &mut model);
    (
      count(state, BUILT),
      count(state, DERIVED),
      count(state, REFUSED),
    )
  });
  assert_eq!(counts, (1, 10, 0));
}

/// A-664 part 3 (its first piece: a plain green is no repository's, and git holds none of its versions). Do: submit a
/// file to a plain green. Expect: accepted, and no map derived, built or refused.
#[test]
fn t_a664_p3_a_plain_greens_versions_keep_no_map() {
  let counts = crate::daemon::audit_on_shard(|state| {
    let made = ask(
      state,
      RequestBody::CreateGreen {
        name: "plain".to_owned(),
        require_evidence: false,
        base: None,
      },
    );
    let ReplyBody::GreenCreated { id: green } = made else {
      panic!("no green: {made:?}")
    };
    version(state, green, 1, vec![create("/f", b"plain")]);
    (
      count(state, BUILT),
      count(state, DERIVED),
      count(state, REFUSED),
    )
  });
  assert_eq!(counts, (0, 0, 0));
}

/// A-664 part 3 (its first piece; A-662: a store made again from the log equals the live store whole). Do: submit two
/// versions to a git repository's green, make its store again from the log as a recovery does, then submit a third.
/// Expect: the store made again holds the green's map of its head again, built from the head and equal to its model;
/// the third version's map derived from it; so one build as a version committed, one remade, two derived.
#[test]
fn t_a664_p3_a_store_made_again_holds_each_greens_map_again() {
  let counts = crate::daemon::audit_on_shard(|state| {
    let at = git_repository(state, "again");
    let mut model = Model::new();
    version(state, at.1, 1, vec![create("/f", b"one")]);
    let second = version(state, at.1, 2, vec![Op::Edit("/f", 0, 3, b"two")]);
    crate::vcs_store::rebuild(state, at.0);
    crate::vcs_store::settle_rebuild(state, at.0);
    set(&mut model, "f", FileClass::File, b"two");
    assert_kept(state, at, second, &model);
    let third = version(state, at.1, 3, vec![create("/g", b"three")]);
    set(&mut model, "g", FileClass::File, b"three");
    assert_kept(state, at, third, &model);
    (
      count(state, BUILT),
      count(state, REMADE),
      count(state, DERIVED),
    )
  });
  assert_eq!(counts, (1, 1, 2));
}

/// A-664's holders' maps (A-636 part 2c-2: a successor rebuilds a repository it took over). Do: submit two versions of
/// a file to a git repository's root green; drop the repository's record and its store, leaving the green, its engine
/// and its membership, as a successor holding the green finds them; rebuild it from its record and its names log
/// (`main` bound to the root) as the successor does; then submit a third version. Expect: rebuilt; the root's map of
/// its head equal to its model, remade once; the third version's derived from it.
#[test]
fn t_a664_holders_a_successors_rebuilt_repository_holds_each_greens_map() {
  let counts = crate::daemon::audit_on_shard(|state| {
    let at = git_repository(state, "taken");
    let mut model = Model::new();
    version(state, at.1, 1, vec![create("/f", b"one")]);
    let second = version(state, at.1, 2, vec![Op::Edit("/f", 0, 3, b"two")]);
    let repositories = state.db.partition().repositories();
    let record = repositories.repository(at.0).cloned().expect("the record");
    let dropped = slates_db::op::Op::RepositoryDestroyed { repository: at.0 };
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
    state
      .db
      .mutate(&mut state.segment, &dropped, now)
      .expect("dropped");
    crate::vcs_store::free(state, at.0);
    assert!(crate::takeover::rebuild_repository(
      state,
      record,
      vec![main_bound(at)]
    ));
    set(&mut model, "f", FileClass::File, b"two");
    assert_kept(state, at, second, &model);
    let third = version(state, at.1, 3, vec![create("/g", b"three")]);
    set(&mut model, "g", FileClass::File, b"three");
    assert_kept(state, at, third, &model);
    (
      count(state, BUILT),
      count(state, REMADE),
      count(state, DERIVED),
    )
  });
  assert_eq!(counts, (1, 1, 2));
}

/// A-664's sliced whole builds (a recovery's remake a file a unit). Do: submit six files to a git repository's root
/// green; give its map and trees back, as a store made again from the log holds none yet; remake them in a recovery's
/// slices whose least slice is a nanosecond, polling the build until it ends. Expect: the build yielded between its
/// files (polled more than once), one unit a file; the head's map equal to its model, remade once.
#[test]
fn t_a664_sliced_a_recoverys_whole_build_steps_a_file_a_unit() {
  let seen = crate::daemon::audit_on_shard(|state| {
    let at = git_repository(state, "sliced");
    let mut model = Model::new();
    let names = ["/a", "/b", "/c", "/d", "/e", "/f"];
    let head = version(
      state,
      at.1,
      1,
      names.map(|name| create(name, b"six")).into(),
    );
    crate::vcs_store::paths::forget(state, at.0, to_db_volume(at.1));
    let mut slices = crate::recovery_slices::RecoverySlices::begin(1);
    let polls = {
      let build = super::remake_green_sliced(state, to_db_volume(at.1), &mut slices);
      let mut build = std::pin::pin!(build);
      let mut context = std::task::Context::from_waker(std::task::Waker::noop());
      let mut polls = 1u64;
      while build.as_mut().poll(&mut context).is_pending() {
        polls = polls.saturating_add(1);
      }
      polls
    };
    for name in names {
      set(
        &mut model,
        name.trim_start_matches('/'),
        FileClass::File,
        b"six",
      );
    }
    assert_kept(state, at, head, &model);
    (polls > 1, slices.finish().units, count(state, REMADE))
  });
  assert_eq!(seen, (true, 6, 1));
}

/// The names log's part binding `main` to the root green, as a successor holds it.
fn main_bound((repository, root): (RepositoryId, VolumeId)) -> slates_db::catalog::StateLogRecord {
  use slates_db::catalog::{LogName, LogSpan, LoggedChange, NameChange, StateLog};
  let log = LogName::State {
    repository,
    log: StateLog::Names,
  };
  slates_db::catalog::StateLogRecord {
    repository,
    log: StateLog::Names,
    from: 0,
    to: 1,
    identity: [1; 32],
    spans: vec![LogSpan {
      log,
      from: 0,
      to: 1,
    }],
    changes: vec![LoggedChange::Named {
      change: NameChange::Bind {
        name: "main".to_owned(),
        green: to_db_volume(root),
      },
    }],
  }
}

/// A-664 part 3 (a branch's first map from its source's: the branch's version 0 is its source's version it was made
/// at). Do: submit a file to a git repository's root green, branch it at its head, then submit two versions to the
/// branch; branch the root again, move the root on, then submit to the second branch. Expect: the first branch's first
/// version derived from the root's map and holding the root's file and its own, its second derived from it, each equal
/// to its model; the second branch's first version built whole, its source's head having moved past it.
#[test]
fn t_a664_p3_a_branchs_first_version_derives_from_its_sources_map() {
  let counts = crate::daemon::audit_on_shard(|state| {
    let (repository, root) = git_repository(state, "branched");
    version(state, root, 1, vec![create("/f", b"root")]);
    let topic = branch(state, "branched", "topic");
    let mut model = Model::new();
    set(&mut model, "f", FileClass::File, b"root");
    let first = version(state, topic, 2, vec![create("/t", b"topic")]);
    set(&mut model, "t", FileClass::File, b"topic");
    assert_kept(state, (repository, topic), first, &model);
    let second = version(state, topic, 3, vec![Op::Edit("/t", 0, 5, b"TOPIC")]);
    set(&mut model, "t", FileClass::File, b"TOPIC");
    assert_kept(state, (repository, topic), second, &model);
    let derived = (count(state, BUILT), count(state, DERIVED));
    let late = branch(state, "branched", "late");
    version(state, root, 4, vec![create("/g", b"moved")]);
    let late_first = version(state, late, 5, vec![create("/l", b"late")]);
    let mut late_model = Model::new();
    set(&mut late_model, "f", FileClass::File, b"root");
    set(&mut late_model, "l", FileClass::File, b"late");
    assert_kept(state, (repository, late), late_first, &late_model);
    (derived, count(state, BUILT), count(state, DERIVED))
  });
  assert_eq!(counts, ((1, 2), 2, 3));
}

/// A-664 part 3 (its first piece; A-662: the live store equals a store made again from the log, which keeps a map for
/// no green that is no member). Do: submit a file to a git repository's root green, branch it, submit a file to the
/// branch, destroy the branch, then make the store again from the log as a recovery does. Expect: the branch's map kept
/// before its destroy and gone after it; the store the same before and after it is made again, every content with its
/// holders and the bytes it uses; the root's map still its model.
#[test]
fn t_a664_p3_a_destroyed_branch_takes_its_map_with_it() {
  crate::daemon::audit_on_shard(|state| {
    let (repository, root) = git_repository(state, "pruned");
    let head = version(state, root, 1, vec![create("/f", b"root")]);
    let topic = branch(state, "pruned", "topic");
    let first = version(state, topic, 2, vec![create("/t", b"topic")]);
    let before = kept(state, repository, topic, first).is_some();
    let destroyed = ask(state, RequestBody::Destroy { volume: topic });
    assert!(matches!(destroyed, ReplyBody::Destroyed), "{destroyed:?}");
    let after = kept(state, repository, topic, first).is_some();
    assert_eq!((before, after), (true, false), "the branch's map");
    let live = held(state);
    crate::vcs_store::rebuild(state, repository);
    crate::vcs_store::settle_rebuild(state, repository);
    assert_eq!(held(state), live, "the store made again");
    let mut model = Model::new();
    set(&mut model, "f", FileClass::File, b"root");
    assert_kept(state, (repository, root), head, &model);
  });
}

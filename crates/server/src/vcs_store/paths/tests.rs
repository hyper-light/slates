//! A-664 part 3, its first piece: a git repository's greens keep each version's map of paths to git's files, derived
//! from the parent's over the change index or built whole, and equal, identity for identity, to the map built from the
//! files the version holds as git reads them (the oracle's model, kept here by the design's rules, never read back
//! through the module's own reading of a green).

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
fn ask(state: &mut ShardState, body: RequestBody) -> ReplyBody {
  dispatch(state, 1, &owner(), body)
}

/// A git repository named `name`: its id and its root green.
fn git_repository(state: &mut ShardState, name: &str) -> (RepositoryId, VolumeId) {
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
enum Op {
  /// Splice `bytes` at `at` over `delete_len` bytes of the file at the path.
  Edit(&'static str, u64, u64, &'static [u8]),
  /// Declare a namespace or metadata operation.
  Declare(WorkOp),
}

/// A new file of `bytes` at `path`.
fn create(path: &'static str, bytes: &'static [u8]) -> Op {
  Op::Edit(path, 0, 0, bytes)
}

/// A work over `green` making `ops`, submitted with a message and identities as a repository's green requires (A-664
/// part 1), its work destroyed after: the version accepted.
fn version(state: &mut ShardState, green: VolumeId, serial: u32, ops: Vec<Op>) -> u64 {
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
  let message = format!("version {serial}");
  let submitted = ask(state, crate::identities::authored(work, &message));
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
fn count(state: &ShardState, name: &str) -> u64 {
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

/// The fixed history's last five versions (each checked against the model): a file renamed; a symbolic link removed; a
/// file moved into a directory made with it and its old directory removed, a change naming directories; a splice into
/// the moved file; its execute bit taken away.
fn last_five(state: &mut ShardState, at: (RepositoryId, VolumeId), model: &mut Model) {
  let green = at.1;
  let rename = |from: &str, to: &str| {
    Op::Declare(WorkOp::Rename {
      from: from.to_owned(),
      to: to.to_owned(),
    })
  };
  let sixth = version(state, green, 6, vec![rename("/f", "/g")]);
  let (class, bytes) = take(model, "f");
  set(model, "g", class, &bytes);
  assert_kept(state, at, sixth, model);
  let unlink = WorkOp::Unlink {
    path: "/s".to_owned(),
  };
  let seventh = version(state, green, 7, vec![Op::Declare(unlink)]);
  take(model, "s");
  assert_kept(state, at, seventh, model);
  let moved = vec![
    Op::Declare(WorkOp::Mkdir {
      path: "/x".to_owned(),
    }),
    rename("/d/a", "/x/a"),
    Op::Declare(WorkOp::Rmdir {
      path: "/d".to_owned(),
    }),
  ];
  let eighth = version(state, green, 8, moved);
  let (class, bytes) = take(model, "d/a");
  set(model, "x/a", class, &bytes);
  assert_kept(state, at, eighth, model);
  let ninth = version(state, green, 9, vec![Op::Edit("/x/a", 0, 0, b"A")]);
  set(model, "x/a", FileClass::Executable, b"Aalpha");
  assert_kept(state, at, ninth, model);
  let mode = WorkOp::SetMode {
    path: "/x/a".to_owned(),
    mode: PLAIN,
  };
  let tenth = version(state, green, 10, vec![Op::Declare(mode)]);
  set(model, "x/a", FileClass::File, b"Aalpha");
  assert_kept(state, at, tenth, model);
}

/// A-664 part 3 (its first piece; A-647's owed map: "the green's state as the path map these types read"). Do: on a git
/// repository's root green, submit ten versions: files made in a directory, one overwritten, one made executable, a
/// symbolic link and a hard link, a named pipe and an attribute, a file renamed, the link removed, a file moved to a
/// new directory and the old one removed, a splice, the execute bit taken away. Expect: after each, the version's map holds exactly the files git
/// would (classes `100644`, `100755`, `120000`; a hard link its primary's bytes; no pipe), and its root is the root of
/// that map built from scratch in another store; the first version and the move between directories built whole, the
/// other eight derived from the map before (the non-vacuity count of the incremental path), none refused.
#[test]
fn t_a664_p3_every_version_of_a_git_green_keeps_its_map_of_gits_files() {
  let counts = crate::daemon::audit_on_shard(|state| {
    let at = git_repository(state, "tree");
    let mut model = Model::new();
    first_five(state, at, &mut model);
    last_five(state, at, &mut model);
    (
      count(state, BUILT),
      count(state, DERIVED),
      count(state, REFUSED),
    )
  });
  assert_eq!(counts, (2, 8, 0));
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

/// A-664 part 3 (its first piece: a branch's first version has no parent's map of its own green). Do: submit a file to
/// a git repository's root green, branch it at its head, then submit two versions to the branch. Expect: the branch's
/// first version built whole and holding the root's file and its own; its second derived from it; each equal to its
/// model.
#[test]
fn t_a664_p3_a_branchs_first_version_is_built_whole_then_derived() {
  let counts = crate::daemon::audit_on_shard(|state| {
    let (repository, root) = git_repository(state, "branched");
    version(state, root, 1, vec![create("/f", b"root")]);
    let made = ask(
      state,
      RequestBody::CreateBranch {
        repository: "branched".to_owned(),
        name: "topic".to_owned(),
        from: "main".to_owned(),
        version: None,
      },
    );
    let ReplyBody::GreenCreated { id: topic } = made else {
      panic!("no branch: {made:?}")
    };
    let mut model = Model::new();
    set(&mut model, "f", FileClass::File, b"root");
    let first = version(state, topic, 2, vec![create("/t", b"topic")]);
    set(&mut model, "t", FileClass::File, b"topic");
    assert_kept(state, (repository, topic), first, &model);
    let second = version(state, topic, 3, vec![Op::Edit("/t", 0, 5, b"TOPIC")]);
    set(&mut model, "t", FileClass::File, b"TOPIC");
    assert_kept(state, (repository, topic), second, &model);
    (count(state, BUILT), count(state, DERIVED))
  });
  assert_eq!(counts, (2, 1));
}

//! A-635's server tests (§4.17.1 "A push is a work"; §4.17.2 "A push" steps 6 and 7): pushes of works A-618's
//! `slates_merge::push::build` made from trees in a store, judged all or none on a daemon's owning shard and recorded as
//! one entry of the repository's register.

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_db::catalog::{
  Principal, RepositoryCheck, RepositoryId, StateChange, VolumeId as DbVolumeId, WriteMode,
  WritePolicy,
};
use slates_db::op::Op;
use slates_ipc::protocol::{ObjectFormat, Refusal, ReplyBody, RequestBody, VcsTool};
use slates_merge::engine::Increment;
use slates_merge::push::{
  EntryKind, HEAD_ATTRIBUTE, ObjectKey, Push, PushObjects, ROOT, TreeEntry, Unavailable,
};

use super::{
  BranchChange, BranchPushed, ChangeRefusal, ChangeRefused, EntryParts, PushRefused, Pushed, push,
};
use crate::state::ShardState;
use crate::verbs::{dispatch, to_db_volume};

/// Shape: the room a test's work is built within, far past any of these trees.
const ROOM: u64 = 1 << 20;
/// Format: the bytes of a tool's object id these tests name objects by (git's SHA-1 length).
const ID_BYTES: usize = 20;

fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

fn stranger() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 4321,
  }
}

/// A store of blobs and one-level trees, each named by the BLAKE3 of its kind and bytes cut to a tool id's length.
#[derive(Clone, Default)]
struct Store {
  objects: BTreeMap<ObjectKey, Object>,
}

#[derive(Clone)]
enum Object {
  Blob(Vec<u8>),
  Tree(Vec<TreeEntry>),
}

fn key(kind: &[u8], bytes: &[u8]) -> ObjectKey {
  let mut hasher = blake3::Hasher::new();
  hasher.update(kind);
  hasher.update(bytes);
  ObjectKey::new(&hasher.finalize().as_bytes()[..ID_BYTES]).expect("an id of a tool's length")
}

impl Store {
  /// A tree holding `files` at its top, each a plain file; its id.
  fn put_tree(&mut self, files: &[(&str, &[u8])]) -> ObjectKey {
    let mut entries = Vec::new();
    let mut encoded = Vec::new();
    for (name, bytes) in files {
      let id = key(b"blob", bytes);
      self.objects.insert(id, Object::Blob(bytes.to_vec()));
      encoded.extend_from_slice(name.as_bytes());
      encoded.extend_from_slice(id.as_bytes());
      entries.push(TreeEntry {
        name: name.as_bytes().to_vec(),
        kind: EntryKind::File,
        id,
      });
    }
    let id = key(b"tree", &encoded);
    self.objects.insert(id, Object::Tree(entries));
    id
  }
}

impl PushObjects for Store {
  fn tree(&self, id: ObjectKey) -> Result<Vec<TreeEntry>, Unavailable> {
    match self.objects.get(&id) {
      Some(Object::Tree(entries)) => Ok(entries.clone()),
      Some(Object::Blob(_)) => Err(Unavailable::NotATree),
      None => Err(Unavailable::Missing),
    }
  }

  fn blob_len(&self, id: ObjectKey) -> Result<u64, Unavailable> {
    match self.objects.get(&id) {
      Some(Object::Blob(bytes)) => Ok(bytes.len() as u64),
      Some(Object::Tree(_)) => Err(Unavailable::NotABlob),
      None => Err(Unavailable::Missing),
    }
  }

  fn blob(&self, id: ObjectKey) -> Result<Cow<'_, [u8]>, Unavailable> {
    match self.objects.get(&id) {
      Some(Object::Blob(bytes)) => Ok(Cow::Borrowed(bytes)),
      Some(Object::Tree(_)) => Err(Unavailable::NotABlob),
      None => Err(Unavailable::Missing),
    }
  }
}

/// A-618's work: a push to `green` from `base`'s tree at its version to `tree`, its head `head`, named by `name`.
fn work(
  state: &ShardState,
  green: DbVolumeId,
  (version, base): (u64, Option<ObjectKey>),
  (tree, head): (ObjectKey, &[u8]),
  (store, name): (&Store, u8),
) -> Increment {
  let engine = state.greens.get(&green).expect("the green's engine");
  let push = Push {
    name: [name; 32],
    base_version: version,
    base_tree: base,
    tree,
    head: Some(head),
  };
  slates_merge::push::build(&push, &engine, store, ROOM)
    .expect("the work is built")
    .increment
}

/// Creates repository `name` (git, its default `main` a new empty green): its id and the root.
fn repository(state: &mut ShardState, name: &str) -> (RepositoryId, DbVolumeId) {
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
  let ReplyBody::GreenCreated { id } = reply else {
    panic!("no repository: {reply:?}")
  };
  let repositories = state.db.partition().repositories();
  let repo = repositories.repository_named(name).expect("the repository").id;
  (repo, to_db_volume(id))
}

/// Makes branch `name` of `repository` at `from`'s head through the verb: its green.
fn branch(state: &mut ShardState, repository: &str, (name, from): (&str, &str)) -> DbVolumeId {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateBranch {
      repository: repository.to_owned(),
      name: name.to_owned(),
      from: from.to_owned(),
      version: None,
    },
  );
  let ReplyBody::GreenCreated { id } = reply else {
    panic!("no branch: {reply:?}")
  };
  to_db_volume(id)
}

/// A green's file at its head.
fn file(state: &ShardState, green: DbVolumeId, path: &str) -> Option<Vec<u8>> {
  state
    .greens
    .get(&green)
    .and_then(|engine| engine.content(path).map(<[u8]>::to_vec))
}

/// A green's head as its root's attribute holds it (§4.17.1 item 3).
fn head(state: &ShardState, green: DbVolumeId) -> Option<Vec<u8>> {
  state
    .greens
    .get(&green)
    .and_then(|engine| engine.xattr(ROOT, HEAD_ATTRIBUTE).map(<[u8]>::to_vec))
}

/// The register's position.
fn position(state: &ShardState, repository: RepositoryId) -> Option<u64> {
  state.db.partition().repositories().position(repository)
}

/// What a refused push must leave as it was: the register's position and namespace, each green's durable and engine
/// heads, identity and chain, and every engine the shard holds. (A conflict's windows stay in its green's rejected
/// cache, as a submit's do, so its retention charge is not among them.)
type Kept = (Option<u64>, Vec<(String, DbVolumeId)>, Vec<GreenKept>, usize);

/// One green's part of [`Kept`].
type GreenKept = (u64, Option<u64>, Option<[u8; 32]>, usize);

fn kept(state: &mut ShardState, repository: RepositoryId, greens: &[DbVolumeId]) -> Kept {
  let names = state
    .db
    .partition()
    .repositories()
    .branches(repository, "")
    .map(|(name, green)| (name.to_owned(), green))
    .collect();
  let greens = greens
    .iter()
    .map(|green| {
      let mut engine = state.greens.get_mut(green);
      let (engine_head, identity) = engine
        .as_mut()
        .map_or((None, None), |engine| (Some(engine.head()), Some(engine.head_identity())));
      drop(engine);
      (
        state.db.partition().green_head(*green),
        engine_head,
        identity,
        state.db.partition().green_chain(*green).len(),
      )
    })
    .collect();
  (position(state, repository), names, greens, state.greens.keys().count())
}

/// A repository `tree` whose `main` is at version 1 (file `f` = `one`, head `c1`) and whose `topic`, made from `main` at
/// version 0, is empty: the repository, the two greens, the store and `main`'s tree at version 1.
fn two_branches(state: &mut ShardState) -> (RepositoryId, [DbVolumeId; 2], Store, ObjectKey) {
  let (repo, main) = repository(state, "tree");
  let topic = branch(state, "tree", ("topic", "main"));
  let mut store = Store::default();
  let one = store.put_tree(&[("f", b"one")]);
  let first = work(state, main, (0, None), (one, b"c1"), (&store, 1));
  let pushed = push(
    state,
    &owner(),
    repo,
    vec![BranchChange::Update {
      green: main,
      work: first,
    }],
    EntryParts::default(),
  );
  assert_eq!(
    pushed.map(|pushed| pushed.changes),
    Ok(vec![BranchPushed::Moved {
      green: main,
      version: 1,
      before: false
    }])
  );
  (repo, [main, topic], store, one)
}

/// A-635 (§4.17.2 step 7: with `atomic`, one register entry for all the refs). Do: push works to two branches in one
/// push; push the same works again; then push a good work beside a stale one (based at a version another push moved
/// past). Expect: both branches move in one entry, one position, each holding the pushed files and head; the same push
/// again is answered its versions, recorded nothing; the stale push is refused naming its stale change's index with the
/// verdict's windows, and nothing of it is applied, the good work included: positions, names, heads, identities,
/// chains, retention and engines as they were.
#[test]
fn a_push_moves_every_branch_or_none_of_them() {
  crate::daemon::audit_on_shard(|state| {
    let (repo, [main, topic], mut store, one) = two_branches(state);
    let at = position(state, repo).expect("the register");
    let two = store.put_tree(&[("f", b"two")]);
    let topical = store.put_tree(&[("g", b"topic")]);
    let together = vec![
      BranchChange::Update {
        green: main,
        work: work(state, main, (1, Some(one)), (two, b"c2"), (&store, 2)),
      },
      BranchChange::Update {
        green: topic,
        work: work(state, topic, (0, None), (topical, b"t1"), (&store, 3)),
      },
    ];
    assert_both_moved_in_one_entry(state, (repo, at), [main, topic], together.clone());
    assert_sent_again_records_nothing(state, (repo, at), [main, topic], together);
    let three = store.put_tree(&[("f", b"three")]);
    let stale = store.put_tree(&[("g", b"stale")]);
    let charged = state.green_retention.get(&main).copied();
    let changes = vec![
      BranchChange::Update {
        green: main,
        work: work(state, main, (2, Some(two)), (three, b"c3"), (&store, 4)),
      },
      BranchChange::Update {
        green: topic,
        work: work(state, topic, (0, None), (stale, b"t-stale"), (&store, 5)),
      },
    ];
    assert_refused_whole(state, (repo, [main, topic]), changes, &[1]);
    assert_eq!(
      state.green_retention.get(&main).copied(),
      charged,
      "the retention secured for the discarded work was not given back"
    );
  });
}

fn assert_both_moved_in_one_entry(
  state: &mut ShardState,
  (repo, at): (RepositoryId, u64),
  [main, topic]: [DbVolumeId; 2],
  together: Vec<BranchChange>,
) {
  let pushed = push(state, &owner(), repo, together, EntryParts::default());
  assert_eq!(
    pushed,
    Ok(Pushed {
      changes: vec![
        BranchPushed::Moved {
          green: main,
          version: 2,
          before: false
        },
        BranchPushed::Moved {
          green: topic,
          version: 1,
          before: false
        },
      ],
      position: Some(at),
    })
  );
  assert_eq!(position(state, repo), Some(at + 1), "one entry for both");
  assert_eq!(file(state, main, "f").as_deref(), Some(&b"two"[..]));
  assert_eq!(file(state, topic, "g").as_deref(), Some(&b"topic"[..]));
  assert_eq!(head(state, topic).as_deref(), Some(&b"t1"[..]));
}

fn assert_sent_again_records_nothing(
  state: &mut ShardState,
  (repo, at): (RepositoryId, u64),
  [main, topic]: [DbVolumeId; 2],
  together: Vec<BranchChange>,
) {
  let again = push(state, &owner(), repo, together, EntryParts::default());
  let moved = |green, version| BranchPushed::Moved {
    green,
    version,
    before: true,
  };
  assert_eq!(
    again,
    Ok(Pushed {
      changes: vec![moved(main, 2), moved(topic, 1)],
      position: None,
    })
  );
  assert_eq!(position(state, repo), Some(at + 1));
}

/// Pushes `changes`, expected refused with the changes at `indexes`, and nothing of it applied.
fn assert_refused_whole(
  state: &mut ShardState,
  (repo, greens): (RepositoryId, [DbVolumeId; 2]),
  changes: Vec<BranchChange>,
  indexes: &[usize],
) -> Vec<ChangeRefused> {
  let before = kept(state, repo, &greens);
  let refused = match push(state, &owner(), repo, changes, EntryParts::default()) {
    Err(PushRefused::Changes(refused)) => refused,
    other => panic!("not refused by change: {other:?}"),
  };
  let named: Vec<usize> = refused.iter().map(|refused| refused.index).collect();
  assert_eq!(named, indexes);
  assert_eq!(kept(state, repo, &greens), before, "a refused push applied something");
  refused
}

/// A-635 (§4.17.2 step 6: a create makes a new green from a version, a delete retires its green). Do: in one push,
/// create `feature` at `main`'s version 1 moved by a work, delete `topic` at the version the client saw, and move
/// `main`. Expect: one entry; `feature` binds a new member holding `main`'s version 1 and the work's file, its head the
/// pushed one, at its version 1; `topic` leaves the namespace and its green stays; `main` moves.
#[test]
fn a_push_creates_and_deletes_branches_in_the_same_step() {
  crate::daemon::audit_on_shard(|state| {
    let (repo, [main, topic], mut store, one) = two_branches(state);
    let at = position(state, repo).expect("the register");
    let featured = store.put_tree(&[("f", b"one"), ("h", b"new")]);
    let two = store.put_tree(&[("f", b"two")]);
    let changes = vec![
      BranchChange::Create {
        name: "feature".to_owned(),
        from: main,
        version: 1,
        work: Some(work(state, main, (1, Some(one)), (featured, b"fe1"), (&store, 6))),
      },
      BranchChange::Delete {
        name: "topic".to_owned(),
        version: 0,
      },
      BranchChange::Update {
        green: main,
        work: work(state, main, (1, Some(one)), (two, b"c2"), (&store, 7)),
      },
    ];
    let pushed = push(state, &owner(), repo, changes, EntryParts::default()).expect("pushed");
    assert_eq!(pushed.position, Some(at));
    assert_eq!(position(state, repo), Some(at + 1), "one entry");
    assert_created_deleted_and_moved(state, repo, &pushed, [main, topic]);
  });
}

fn assert_created_deleted_and_moved(
  state: &mut ShardState,
  repo: RepositoryId,
  pushed: &Pushed,
  [main, topic]: [DbVolumeId; 2],
) {
  let Some(BranchPushed::Created { green: feature, version: 1 }) = pushed.changes.first().cloned()
  else {
    panic!("no branch made: {pushed:?}")
  };
  let repositories = state.db.partition().repositories();
  assert_eq!(repositories.branch(repo, "feature"), Some(feature));
  assert_eq!(repositories.branch(repo, "topic"), None);
  assert!(repositories.member(topic).is_some(), "a deleted branch's green is kept");
  assert_eq!(file(state, feature, "f").as_deref(), Some(&b"one"[..]));
  assert_eq!(file(state, feature, "h").as_deref(), Some(&b"new"[..]));
  assert_eq!(head(state, feature).as_deref(), Some(&b"fe1"[..]));
  assert_eq!(state.db.partition().green_head(feature), 1);
  assert_eq!(
    pushed.changes.get(1..),
    Some(
      &[
        BranchPushed::Deleted { green: topic },
        BranchPushed::Moved {
          green: main,
          version: 2,
          before: false
        },
      ][..]
    )
  );
}

/// A-635 (typed refusals naming which change and why). Do: push, at once, a create of a taken name, a create from a
/// green outside the lineage, a delete of a name no branch has, a delete at a version its branch moved past, two moves
/// of one green, and a create beside them that would pass; then a good create beside a work whose verdict conflicts.
/// Expect: every bad change refused by its index and kind (`AlreadyExists`, `NotFound`, `NotFound`, `Moved`, and
/// `Twice` for both moves, since the delete names that green too), the passing one not named; the second refused by its conflict, and the branch it would have made, made in memory for
/// its verdict, gone again; nothing of either applied.
#[test]
fn every_refused_change_is_named_and_nothing_is_applied() {
  crate::daemon::audit_on_shard(|state| {
    let (repo, [main, topic], mut store, one) = two_branches(state);
    let lone = lone_green(state);
    let two = store.put_tree(&[("f", b"two")]);
    let moving = work(state, main, (1, Some(one)), (two, b"c2"), (&store, 8));
    let changes = vec![
      create("main", main, 1),
      create("loner", lone, 0),
      BranchChange::Delete {
        name: "nope".to_owned(),
        version: 0,
      },
      BranchChange::Delete {
        name: "main".to_owned(),
        version: 0,
      },
      BranchChange::Update {
        green: main,
        work: moving.clone(),
      },
      BranchChange::Update {
        green: main,
        work: moving,
      },
      create("fine", main, 1),
    ];
    let refused = assert_refused_whole(state, (repo, [main, topic]), changes, &[0, 1, 2, 3, 4, 5]);
    assert_eq!(
      refused.iter().map(|refused| refused.why.clone()).collect::<Vec<_>>(),
      vec![
        ChangeRefusal::Refused(Refusal::AlreadyExists {
          existing: crate::verbs::to_wire_volume(main)
        }),
        ChangeRefusal::Refused(Refusal::NotFound),
        ChangeRefusal::Refused(Refusal::NotFound),
        ChangeRefusal::Moved { head: 1 },
        ChangeRefusal::Twice,
        ChangeRefusal::Twice,
      ]
    );
    assert_a_conflict_retires_the_branch_made_for_it(state, (repo, [main, topic]), &mut store);
  });
}

fn create(name: &str, from: DbVolumeId, version: u64) -> BranchChange {
  BranchChange::Create {
    name: name.to_owned(),
    from,
    version,
    work: None,
  }
}

/// A green no repository holds: its id.
fn lone_green(state: &mut ShardState) -> DbVolumeId {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateGreen {
      name: "lone".to_owned(),
      require_evidence: false,
      base: None,
    },
  );
  let ReplyBody::GreenCreated { id } = reply else {
    panic!("no green: {reply:?}")
  };
  to_db_volume(id)
}

fn assert_a_conflict_retires_the_branch_made_for_it(
  state: &mut ShardState,
  (repo, greens): (RepositoryId, [DbVolumeId; 2]),
  store: &mut Store,
) {
  let [main, topic] = greens;
  let first = store.put_tree(&[("g", b"first")]);
  let landed = work(state, topic, (0, None), (first, b"t1"), (store, 9));
  let pushed = push(
    state,
    &owner(),
    repo,
    vec![BranchChange::Update {
      green: topic,
      work: landed,
    }],
    EntryParts::default(),
  );
  assert!(pushed.is_ok(), "{pushed:?}");
  let late = store.put_tree(&[("g", b"late")]);
  let made = store.put_tree(&[("f", b"one"), ("x", b"x")]);
  let one = store.put_tree(&[("f", b"one")]);
  let changes = vec![
    BranchChange::Create {
      name: "made".to_owned(),
      from: main,
      version: 1,
      work: Some(work(state, main, (1, Some(one)), (made, b"m1"), (store, 10))),
    },
    BranchChange::Update {
      green: topic,
      work: work(state, topic, (0, None), (late, b"t-late"), (store, 11)),
    },
  ];
  let refused = assert_refused_whole(state, (repo, greens), changes, &[1]);
  assert!(matches!(
    refused.first().map(|refused| &refused.why),
    Some(ChangeRefusal::Conflict { windows }) if !windows.is_empty()
  ));
  assert_eq!(state.db.partition().repositories().branch(repo, "made"), None);
}

/// A-635 (the gates every merge passes, and the entry's own refusals). Do: push a move with a check of another
/// branch's head that no longer holds; with a phase root and a marker beside it; to a green whose write policy forbids
/// merges, and then asks for an approval of each; as a principal who does not own the repository; and to a repository
/// that is not there. Expect: the failed check refuses the entry, naming the check, nothing applied; the state moves
/// with the push in its one entry; the forbidding policy refuses `PolicyForbids`, the asking one names the work an
/// approver would approve; the stranger is refused `Forbidden` and the unknown repository `NotFound`.
#[test]
fn the_gates_and_the_entry_refuse_the_whole_push() {
  crate::daemon::audit_on_shard(|state| {
    let (repo, [main, topic], mut store, one) = two_branches(state);
    let two = store.put_tree(&[("f", b"two")]);
    let moving = work(state, main, (1, Some(one)), (two, b"c2"), (&store, 12));
    assert_a_failed_check_refuses_the_entry(state, (repo, [main, topic]), moving.clone());
    assert_the_state_moves_with_the_push(state, repo, main, moving);
    assert_the_write_policy_gates_the_push(state, repo, main, &mut store);
    let elsewhere = RepositoryId {
      bytes: [0xee; 16],
    };
    assert_eq!(
      push(state, &stranger(), repo, Vec::new(), EntryParts::default()),
      Err(PushRefused::Repository(Refusal::Forbidden {
        verb: "push".to_owned()
      }))
    );
    assert_eq!(
      push(state, &owner(), elsewhere, Vec::new(), EntryParts::default()),
      Err(PushRefused::Repository(Refusal::NotFound))
    );
  });
}

fn assert_a_failed_check_refuses_the_entry(
  state: &mut ShardState,
  (repo, greens): (RepositoryId, [DbVolumeId; 2]),
  moving: Increment,
) {
  let [main, topic] = greens;
  let before = kept(state, repo, &greens);
  let parts = EntryParts {
    checks: vec![RepositoryCheck::Head {
      green: topic,
      version: 5,
    }],
    ..EntryParts::default()
  };
  let changes = vec![BranchChange::Update {
    green: main,
    work: moving,
  }];
  assert_eq!(
    push(state, &owner(), repo, changes, parts),
    Err(PushRefused::Entry(Refusal::Database {
      refusal: slates_ipc::protocol::DatabaseRefusal::CheckFailed { index: 0 }
    }))
  );
  assert_eq!(kept(state, repo, &greens), before);
}

fn assert_the_state_moves_with_the_push(
  state: &mut ShardState,
  repo: RepositoryId,
  main: DbVolumeId,
  moving: Increment,
) {
  let at = position(state, repo).expect("the register");
  let parts = EntryParts {
    state: vec![
      StateChange::PhaseRootAdded {
        phase: 1,
        node: vec![7; ID_BYTES],
      },
      StateChange::MarkerAdded {
        marker: b"marker".to_vec(),
      },
    ],
    ..EntryParts::default()
  };
  let changes = vec![BranchChange::Update {
    green: main,
    work: moving,
  }];
  let pushed = push(state, &owner(), repo, changes, parts);
  assert_eq!(pushed.map(|pushed| pushed.position), Ok(Some(at)));
  let repositories = state.db.partition().repositories();
  assert_eq!(repositories.position(repo), Some(at + 1));
  assert_eq!(repositories.phase_roots(repo).count(), 1);
  assert_eq!(repositories.markers(repo).count(), 1);
}

fn assert_the_write_policy_gates_the_push(
  state: &mut ShardState,
  repo: RepositoryId,
  main: DbVolumeId,
  store: &mut Store,
) {
  let three = store.put_tree(&[("f", b"three")]);
  let two = store.put_tree(&[("f", b"two")]);
  let moving = work(state, main, (2, Some(two)), (three, b"c3"), (store, 13));
  for (mode, refusal) in [
    (
      WriteMode::Forbid,
      ChangeRefusal::Refused(Refusal::PolicyForbids {
        act: slates_ipc::protocol::WriteAct::Merge,
        by: slates_ipc::protocol::PolicySubject::Volume {
          volume: crate::verbs::to_wire_volume(main),
        },
      }),
    ),
    (WriteMode::Ask, ChangeRefusal::ApprovalRequired { identity: moving.id }),
  ] {
    let policy = Op::VolumeWritePolicySet {
      id: main,
      policy: WritePolicy {
        merge: mode,
        land: WriteMode::Bypass,
        require_signed_merges: false,
      },
    };
    state.db.mutate(&mut state.segment, &policy, 0).expect("the policy is set");
    let changes = vec![BranchChange::Update {
      green: main,
      work: moving.clone(),
    }];
    assert_eq!(
      push(state, &owner(), repo, changes, EntryParts::default()),
      Err(PushRefused::Changes(vec![ChangeRefused {
        index: 0,
        why: refusal
      }]))
    );
  }
}

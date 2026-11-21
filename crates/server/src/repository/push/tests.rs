//! A-635's server tests (§4.17.1 "A push is a work"; §4.17.2 "A push" steps 6 and 7): pushes of works A-618's
//! `slates_merge::push::build` made from trees in a store, judged all or none on a daemon's owning shard and recorded as
//! one entry of the repository's register.

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_db::catalog::{
  AtomicRecord, LogName, Principal, RepositoryCheck, RepositoryId, StateChange, StateLog,
  VolumeId as DbVolumeId, WriteMode, WritePolicy,
};
use slates_db::op::Op;
use slates_ipc::protocol::{ObjectFormat, Refusal, ReplyBody, RequestBody, VcsTool};
use slates_merge::engine::Increment;
use slates_merge::push::{
  EntryKind, HEAD_ATTRIBUTE, ObjectKey, Push, PushObjects, ROOT, TreeEntry, Unavailable,
};

use super::{
  AppendRefused, Appended, BranchAppended, BranchChange, ChangeRefusal, ChangeRefused, EntryParts,
  push,
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
  let reply = crate::test_store::create(
    state,
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
  let repo = repositories
    .repository_named(name)
    .expect("the repository")
    .id;
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

/// The names log's position (A-642).
fn names_position(state: &ShardState, repository: RepositoryId) -> Option<u64> {
  state
    .db
    .partition()
    .repositories()
    .log_position(repository, StateLog::Names)
}

/// The atomic record version `version` of `green` belongs to (A-642).
fn record_at(state: &ShardState, green: DbVolumeId, version: u64) -> Option<AtomicRecord> {
  state
    .db
    .partition()
    .repositories()
    .atomic_at(green, version)
    .cloned()
}

/// The logs an atomic record moved, in log order.
fn logs_of(record: &AtomicRecord) -> Vec<LogName> {
  record.spans.iter().map(|span| span.log).collect()
}

/// What a refused push must leave as it was: the names log's position and the namespace, each green's durable and
/// engine heads, identity and chain, the atomic records, and every engine the shard holds. (A conflict's windows stay
/// in its green's rejected cache, as a submit's do, so its retention charge is not among them.)
type Kept = (
  Option<u64>,
  Vec<(String, DbVolumeId)>,
  Vec<GreenKept>,
  (usize, usize),
);

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
      let (engine_head, identity) = engine.as_mut().map_or((None, None), |engine| {
        (Some(engine.head()), Some(engine.head_identity()))
      });
      drop(engine);
      (
        state.db.partition().green_head(*green),
        engine_head,
        identity,
        state.db.partition().green_chain(*green).len(),
      )
    })
    .collect();
  let records = state.db.partition().repositories().atomic_records().count();
  (
    names_position(state, repository),
    names,
    greens,
    (records, state.greens.keys().count()),
  )
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
    Ok(vec![BranchAppended::Moved {
      green: main,
      version: 1,
      before: false
    }])
  );
  (repo, [main, topic], store, one)
}

/// A-635 (§4.17.2 step 7: with `atomic`, one record for all the refs; A-642). Do: push works to two branches in one
/// push; push the same works again; then push a good work beside a stale one (based at a version another push moved
/// past). Expect: both branches move in one atomic record, which each green's log points to at its new version, each
/// holding the pushed files and head; the same push again is answered its versions and records nothing; the stale push
/// is refused naming its stale change's index with the verdict's windows, and nothing of it is applied, the good work
/// included: names, heads, identities, chains, records, retention and engines as they were.
#[test]
fn a_push_moves_every_branch_or_none_of_them() {
  crate::test_store::audit_on_stored_shard("repository-push", |state| {
    let (repo, [main, topic], mut store, one) = two_branches(state);
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
    assert_both_moved_in_one_record(state, repo, [main, topic], together.clone());
    assert_sent_again_records_nothing(state, repo, [main, topic], together);
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

fn assert_both_moved_in_one_record(
  state: &mut ShardState,
  repo: RepositoryId,
  [main, topic]: [DbVolumeId; 2],
  together: Vec<BranchChange>,
) {
  let pushed = push(state, &owner(), repo, together, EntryParts::default());
  assert_eq!(
    pushed,
    Ok(Appended {
      changes: vec![
        BranchAppended::Moved {
          green: main,
          version: 2,
          before: false
        },
        BranchAppended::Moved {
          green: topic,
          version: 1,
          before: false
        },
      ],
      recorded: true,
    })
  );
  let record = record_at(state, main, 2).expect("one atomic record for both");
  assert_eq!(record_at(state, topic, 1), Some(record.clone()));
  assert_eq!(
    logs_of(&record),
    vec![
      LogName::Green { green: main },
      LogName::Green { green: topic }
    ]
  );
  assert_eq!(file(state, main, "f").as_deref(), Some(&b"two"[..]));
  assert_eq!(file(state, topic, "g").as_deref(), Some(&b"topic"[..]));
  assert_eq!(head(state, topic).as_deref(), Some(&b"t1"[..]));
}

fn assert_sent_again_records_nothing(
  state: &mut ShardState,
  repo: RepositoryId,
  [main, topic]: [DbVolumeId; 2],
  together: Vec<BranchChange>,
) {
  let before = kept(state, repo, &[main, topic]);
  let again = push(state, &owner(), repo, together, EntryParts::default());
  let moved = |green, version| BranchAppended::Moved {
    green,
    version,
    before: true,
  };
  assert_eq!(
    again,
    Ok(Appended {
      changes: vec![moved(main, 2), moved(topic, 1)],
      recorded: false,
    })
  );
  assert_eq!(kept(state, repo, &[main, topic]), before);
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
    Err(AppendRefused::Changes(refused)) => refused,
    other => panic!("not refused by change: {other:?}"),
  };
  let named: Vec<usize> = refused.iter().map(|refused| refused.index).collect();
  assert_eq!(named, indexes);
  assert_eq!(
    kept(state, repo, &greens),
    before,
    "a refused push applied something"
  );
  refused
}

/// A-635 (§4.17.2 step 6: a create makes a new green from a version, a delete retires its green). Do: in one push,
/// create `feature` at `main`'s version 1 moved by a work, delete `topic` at the version the client saw, and move
/// `main`. Expect: one atomic record over the new green's log, `main`'s and the names log; `feature` binds a new member
/// holding `main`'s version 1 and the work's file, its head the pushed one, at its version 1; `topic` leaves the
/// namespace and its green stays; `main` moves.
#[test]
fn a_push_creates_and_deletes_branches_in_the_same_step() {
  crate::test_store::audit_on_stored_shard("repository-push", |state| {
    let (repo, [main, topic], mut store, one) = two_branches(state);
    let featured = store.put_tree(&[("f", b"one"), ("h", b"new")]);
    let two = store.put_tree(&[("f", b"two")]);
    let changes = vec![
      BranchChange::Create {
        name: "feature".to_owned(),
        from: main,
        version: 1,
        work: Some(work(
          state,
          main,
          (1, Some(one)),
          (featured, b"fe1"),
          (&store, 6),
        )),
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
    assert!(pushed.recorded);
    assert_created_deleted_and_moved(state, repo, &pushed, [main, topic]);
  });
}

fn assert_created_deleted_and_moved(
  state: &mut ShardState,
  repo: RepositoryId,
  pushed: &Appended,
  [main, topic]: [DbVolumeId; 2],
) {
  let feature = assert_feature_made(state, repo, pushed, topic);
  assert_recorded_as_one(state, repo, pushed, [main, feature, topic]);
}

/// Branch `feature` made at version 1 with its files and head, `topic`'s name retired and its green kept: `feature`.
fn assert_feature_made(
  state: &mut ShardState,
  repo: RepositoryId,
  pushed: &Appended,
  topic: DbVolumeId,
) -> DbVolumeId {
  let Some(BranchAppended::Created {
    green: feature,
    version: 1,
  }) = pushed.changes.first().cloned()
  else {
    panic!("no branch made: {pushed:?}")
  };
  let repositories = state.db.partition().repositories();
  assert_eq!(repositories.branch(repo, "feature"), Some(feature));
  assert_eq!(repositories.branch(repo, "topic"), None);
  assert!(
    repositories.member(topic).is_some(),
    "a deleted branch's green is kept"
  );
  assert_eq!(file(state, feature, "f").as_deref(), Some(&b"one"[..]));
  assert_eq!(file(state, feature, "h").as_deref(), Some(&b"new"[..]));
  assert_eq!(head(state, feature).as_deref(), Some(&b"fe1"[..]));
  assert_eq!(state.db.partition().green_head(feature), 1);
  feature
}

/// One atomic record on `main` and `feature`, naming the names log, and the delete and the move answered in order.
fn assert_recorded_as_one(
  state: &mut ShardState,
  repo: RepositoryId,
  pushed: &Appended,
  [main, feature, topic]: [DbVolumeId; 3],
) {
  let record = record_at(state, main, 2).expect("one atomic record");
  assert_eq!(record_at(state, feature, 1), Some(record.clone()));
  assert!(logs_of(&record).contains(&LogName::State {
    repository: repo,
    log: StateLog::Names
  }));
  assert_eq!(
    pushed.changes.get(1..),
    Some(
      &[
        BranchAppended::Deleted { green: topic },
        BranchAppended::Moved {
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
  crate::test_store::audit_on_stored_shard("repository-push", |state| {
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
      refused
        .iter()
        .map(|refused| refused.why.clone())
        .collect::<Vec<_>>(),
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
      work: Some(work(
        state,
        main,
        (1, Some(one)),
        (made, b"m1"),
        (store, 10),
      )),
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
  assert_eq!(
    state.db.partition().repositories().branch(repo, "made"),
    None
  );
}

/// A-635 (the gates every merge passes, and the entry's own refusals). Do: push a move with a check of another
/// branch's head that no longer holds; with a phase root and a marker beside it; to a green whose write policy forbids
/// merges, and then asks for an approval of each; as a principal who does not own the repository; and to a repository
/// that is not there. Expect: the failed check refuses the entry `Moved`, naming the green, the position read and the
/// one it is at (A-642), nothing applied; the state moves
/// with the push in its one entry; the forbidding policy refuses `PolicyForbids`, the asking one names the work an
/// approver would approve; the stranger is refused `Forbidden` and the unknown repository `NotFound`.
#[test]
fn the_gates_and_the_entry_refuse_the_whole_push() {
  crate::test_store::audit_on_stored_shard("repository-push", |state| {
    let (repo, [main, topic], mut store, one) = two_branches(state);
    let two = store.put_tree(&[("f", b"two")]);
    let moving = work(state, main, (1, Some(one)), (two, b"c2"), (&store, 12));
    assert_a_failed_check_refuses_the_entry(state, (repo, [main, topic]), moving.clone());
    assert_the_state_moves_with_the_push(state, repo, main, moving);
    assert_the_write_policy_gates_the_push(state, repo, main, &mut store);
    let elsewhere = RepositoryId { bytes: [0xee; 16] };
    assert_eq!(
      push(state, &stranger(), repo, Vec::new(), EntryParts::default()),
      Err(AppendRefused::Repository(Refusal::Forbidden {
        verb: "push".to_owned()
      }))
    );
    assert_eq!(
      push(
        state,
        &owner(),
        elsewhere,
        Vec::new(),
        EntryParts::default()
      ),
      Err(AppendRefused::Repository(Refusal::NotFound))
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
    Err(AppendRefused::Entry(Refusal::Database {
      refusal: slates_ipc::protocol::DatabaseRefusal::Moved {
        green: crate::verbs::to_wire_volume(topic),
        read: 5,
        at: 0,
      }
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
  assert_eq!(pushed.map(|pushed| pushed.recorded), Ok(true));
  let record = record_at(state, main, 2).expect("one atomic record");
  let state_log = |log| LogName::State {
    repository: repo,
    log,
  };
  assert_eq!(
    logs_of(&record),
    vec![
      LogName::Green { green: main },
      state_log(StateLog::Phases),
      state_log(StateLog::Markers),
    ]
  );
  let repositories = state.db.partition().repositories();
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
    (
      WriteMode::Ask,
      ChangeRefusal::ApprovalRequired {
        identity: moving.id,
      },
    ),
  ] {
    let policy = Op::VolumeWritePolicySet {
      id: main,
      policy: WritePolicy {
        merge: mode,
        land: WriteMode::Bypass,
        require_signed_merges: false,
      },
    };
    state
      .db
      .mutate(&mut state.segment, &policy, 0)
      .expect("the policy is set");
    let changes = vec![BranchChange::Update {
      green: main,
      work: moving.clone(),
    }];
    assert_eq!(
      push(state, &owner(), repo, changes, EntryParts::default()),
      Err(AppendRefused::Changes(vec![ChangeRefused {
        index: 0,
        why: refusal
      }]))
    );
  }
}

/// The green each change of an append made, in its changes' order.
fn made_greens(pushed: &Appended) -> Vec<DbVolumeId> {
  pushed
    .changes
    .iter()
    .filter_map(|change| match change {
      BranchAppended::Created { green, .. } => Some(*green),
      _ => None,
    })
    .collect()
}

/// A-694 (bug: every new branch of one append was minted the same id). Do: in a repository whose `main` is at version
/// 1, append three new branches made from `main` at version 1 in one entry; then make a fourth through the branch verb,
/// recorded after the entry. Expect the entry accepted, each branch its own green at version 0 holding `main`'s file and
/// bound to its name, the four greens' ids all different, and each new id's counter past the one before it. Before,
/// the second branch's id was the first's, and the entry was refused `ContentUnavailable` naming it.
#[test]
fn t_a694_an_append_makes_each_new_branch_its_own_green() {
  crate::test_store::audit_on_stored_shard("repository-push", |state| {
    let (repo, [main, _], _, _) = two_branches(state);
    let changes = vec![
      create("left", main, 1),
      create("middle", main, 1),
      create("right", main, 1),
    ];
    let pushed = push(state, &owner(), repo, changes, EntryParts::default());
    let pushed = pushed.expect("three new branches in one entry");
    let made = made_greens(&pushed);
    assert_eq!(made.len(), 3, "{pushed:?}");
    let repositories = state.db.partition().repositories();
    for (name, green) in ["left", "middle", "right"].into_iter().zip(&made) {
      assert_eq!(repositories.branch(repo, name), Some(*green), "{name}");
    }
    for green in &made {
      assert_eq!(file(state, *green, "f").as_deref(), Some(&b"one"[..]));
    }
    let fourth = branch(state, "tree", ("fourth", "main"));
    let counters: Vec<u64> = made
      .iter()
      .chain([&fourth])
      .map(|green| green.counter())
      .collect();
    assert!(
      counters.windows(2).all(|pair| pair[0] < pair[1]),
      "{counters:?}"
    );
  });
}

thread_local! {
  /// P2's test: the store an owed push reads its objects from, on the shard's thread.
  static OWED_STORE: std::cell::RefCell<Store> = std::cell::RefCell::new(Store::default());
}

/// P2's test objects: [`OWED_STORE`], as `crate::green_ready` reads an owed push's objects.
fn owed_objects() -> Box<dyn PushObjects> {
  Box::new(OWED_STORE.with(|store| store.borrow().clone()))
}

/// A request's completion as its reply, if recorded.
fn completed_reply(
  state: &ShardState,
  (origin, id): (u64, slates_wire::request::RequestId),
) -> Option<ReplyBody> {
  match state
    .db
    .partition()
    .completion(origin, id.client, id.sequence)
  {
    slates_wire::request::Seen::Completed(bytes) => {
      <ReplyBody as slates_wire::Wire>::from_bytes(&bytes).ok()
    }
    _ => None,
  }
}

/// The owed green's version 1 durable, its chain entry the push's record (not a work) and shorter than the eager
/// green's.
fn owed_is_its_record(state: &ShardState, (owed, eager): (DbVolumeId, DbVolumeId)) {
  let partition = state.db.partition();
  assert_eq!(partition.green_head(owed), 1, "the owed version is durable");
  let owed_entry = partition
    .green_chain(owed)
    .first()
    .cloned()
    .expect("the owed entry");
  let eager_entry = partition
    .green_chain(eager)
    .first()
    .cloned()
    .expect("the eager entry");
  assert!(
    slates_merge::push::DeferredPush::is_record(&owed_entry),
    "the push's record, not a work"
  );
  assert!(
    owed_entry.len() < eager_entry.len(),
    "{} against {}",
    owed_entry.len(),
    eager_entry.len()
  );
}

/// P2 (main's ruling of 2026-11-21 05:12; w-git's request): a created ref's green owes its push and is made when first
/// reached. Do: push one change creating branch `eager` with its built work and one creating `owed` deferring the same
/// tree (`BranchChange::CreateDeferred`); read `owed` around the door; fold and checkpoint it; then create a work over it
/// as a client's recorded request, and wait for the answer; then replay it from its durable chain. Expect:
/// - `owed` owes its push: `Greens::get` lends nothing (counted), its durable head is 1, its version-1 entry is the
///   push's record and far smaller than `eager`'s, and its fold and checkpoint do nothing;
/// - the request parks (no answer, no completion), the green is materialized, then the request is answered, a work
///   created over version 1;
/// - `owed` then reads as `eager`: the same head, files and head identity;
/// - its replay from the durable chain stands at its base and owes version 1 again, as a restart leaves it.
#[test]
fn a_deferred_create_is_made_when_first_reached_and_reads_as_the_eager_create() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let id = slates_wire::request::RequestId {
    client: 7,
    sequence: 1,
  };
  let (eager, owed) = crate::daemon::observe_first(&daemon, move |state| {
    let (repo, main) = repository(state, "p2");
    let mut store = Store::default();
    let tree = store.put_tree(&[("f", b"one"), ("g", b"two")]);
    let built = work(state, main, (0, None), (tree, b"h1"), (&store, 1));
    let owed_push = slates_merge::push::Push {
      name: [2; 32],
      base_version: 0,
      base_tree: None,
      tree,
      head: Some(b"h1"),
    };
    let changes = vec![
      BranchChange::Create {
        name: "eager".to_owned(),
        from: main,
        version: 0,
        work: Some(built),
      },
      BranchChange::CreateDeferred {
        name: "owed".to_owned(),
        from: main,
        version: 0,
        push: slates_merge::push::DeferredPush::of(&owed_push, ROOM),
      },
    ];
    let pushed = push(state, &owner(), repo, changes, EntryParts::default()).expect("pushed");
    assert!(pushed.recorded);
    let branch_of = |name: &str| {
      state
        .db
        .partition()
        .repositories()
        .branch(repo, name)
        .expect("the branch")
    };
    let (eager, owed) = (branch_of("eager"), branch_of("owed"));
    assert!(
      state.greens.is_owing(&owed),
      "the deferred green owes its push"
    );
    let reached = state.greens.owing_reached();
    assert!(state.greens.get(&owed).is_none(), "no lend around the door");
    assert_eq!(
      state.greens.owing_reached(),
      reached + 1,
      "the reach counted"
    );
    owed_is_its_record(state, (owed, eager));
    crate::merge_service::checkpoint_green(state, owed);
    assert!(
      state.db.partition().green_checkpoint(owed).is_none(),
      "an owing green's checkpoint is nothing"
    );
    OWED_STORE.with(|held| *held.borrow_mut() = store);
    crate::green_ready::TEST_OBJECTS.with(|objects| objects.set(Some(owed_objects)));
    let parked = crate::verbs::run_recorded_for_test(
      state,
      (7, id),
      &owner(),
      RequestBody::CreateWork {
        green: crate::verbs::to_wire_volume(owed),
        name: "over-owed".to_owned(),
      },
    );
    assert!(parked.is_none(), "parked, not answered: {parked:?}");
    assert!(
      completed_reply(state, (7, id)).is_none(),
      "nothing recorded while parked"
    );
    (eager, owed)
  });
  let began = std::time::Instant::now();
  let answered = loop {
    let answered =
      crate::daemon::observe_first(&daemon, move |state| completed_reply(state, (7, id)));
    if answered.is_some() || began.elapsed() > std::time::Duration::from_secs(60) {
      break answered;
    }
  };
  let (reads, replayed, made) = crate::daemon::observe_first(&daemon, move |state| {
    let mut read = |green: DbVolumeId| {
      let mut engine = state.greens.get_mut(&green).expect("materialized");
      let files: Vec<(String, Vec<u8>)> = engine
        .files()
        .map(|(path, bytes)| (path, bytes.to_vec()))
        .collect();
      (engine.head(), files, engine.head_identity())
    };
    let reads = (read(owed), read(eager));
    let made = state
      .refusals
      .get(crate::green_ready::MATERIALIZED)
      .copied();
    (
      reads,
      crate::verbs::replay_green_for_test(state, owed),
      made,
    )
  });
  daemon.stop();
  assert!(
    matches!(answered, Some(ReplyBody::WorkCreated { .. })),
    "the parked request answered once the green was made: {answered:?}"
  );
  assert_eq!(made, Some(1), "materialized once");
  assert_eq!(reads.0, reads.1, "the owed green reads as the eager one");
  assert_eq!(
    replayed,
    Some((0, Some(1))),
    "replayed, it stands at its base and owes version 1 again"
  );
}

/// Waits on `daemon`'s first shard until request `id` of origin 7 is answered, within a minute: its reply.
fn answered_on_shard(
  daemon: &crate::daemon::Daemon,
  id: slates_wire::request::RequestId,
) -> Option<ReplyBody> {
  let began = std::time::Instant::now();
  loop {
    let answered =
      crate::daemon::observe_first(daemon, move |state| completed_reply(state, (7, id)));
    if answered.is_some() || began.elapsed() > std::time::Duration::from_secs(60) {
      return answered;
    }
  }
}

/// P2's memory-sizing rule (main's order of 05:16: derived content is never kept unbounded). Do: create two branches
/// that each owe a push, keep at most one untouched green materialized, then read the first, the second, and the first
/// again, each as a client's recorded request that writes nothing. Expect: each read answered; reading the second
/// drops the first back to owing (counted), and reading the first again makes it again; three materializations and the
/// drops counted, the second still materialized at the end.
#[test]
fn an_untouched_materialized_green_past_the_bound_is_dropped_back_and_made_again_when_read() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let ask = |sequence: u32| slates_wire::request::RequestId {
    client: 7,
    sequence,
  };
  let (first, second) = crate::daemon::observe_first(&daemon, move |state| {
    let (repo, main) = repository(state, "p2-bound");
    let mut store = Store::default();
    let tree = store.put_tree(&[("f", b"one"), ("g", b"two")]);
    let owed = |name: &str, tag: u8| BranchChange::CreateDeferred {
      name: name.to_owned(),
      from: main,
      version: 0,
      push: slates_merge::push::DeferredPush::of(
        &slates_merge::push::Push {
          name: [tag; 32],
          base_version: 0,
          base_tree: None,
          tree,
          head: Some(b"h1"),
        },
        ROOM,
      ),
    };
    push(
      state,
      &owner(),
      repo,
      vec![owed("first", 1), owed("second", 2)],
      EntryParts::default(),
    )
    .expect("pushed");
    let branch_of = |name: &str| {
      state
        .db
        .partition()
        .repositories()
        .branch(repo, name)
        .expect("the branch")
    };
    OWED_STORE.with(|held| *held.borrow_mut() = store);
    crate::green_ready::TEST_OBJECTS.with(|objects| objects.set(Some(owed_objects)));
    crate::green_ready::TEST_KEEP_BOUND.with(|bound| bound.set(Some(1)));
    (branch_of("first"), branch_of("second"))
  });
  let read = |sequence: u32, green: DbVolumeId| {
    crate::daemon::observe_first(&daemon, move |state| {
      crate::verbs::run_recorded_for_test(
        state,
        (7, ask(sequence)),
        &owner(),
        RequestBody::Versions {
          green: crate::verbs::to_wire_volume(green),
        },
      )
    })
  };
  assert!(read(1, first).is_none(), "the first read parks");
  let one = answered_on_shard(&daemon, ask(1));
  assert!(read(2, second).is_none(), "the second read parks");
  let two = answered_on_shard(&daemon, ask(2));
  let after_two = crate::daemon::observe_first(&daemon, move |state| {
    (
      state.greens.is_owing(&first),
      state.greens.is_owing(&second),
    )
  });
  assert!(read(3, first).is_none(), "the first, dropped, parks again");
  let three = answered_on_shard(&daemon, ask(3));
  let counted = crate::daemon::observe_first(&daemon, move |state| {
    let count = |name| state.refusals.get(name).copied().unwrap_or(0);
    (
      count(crate::green_ready::MATERIALIZED),
      count(crate::green_ready::DEMATERIALIZED),
      state.greens.is_owing(&first),
    )
  });
  daemon.stop();
  for (n, answer) in [one, two, three].into_iter().enumerate() {
    assert!(
      matches!(answer, Some(ReplyBody::Versions { .. })),
      "read {} answered: {answer:?}",
      n + 1
    );
  }
  assert_eq!(
    after_two,
    (true, false),
    "the first dropped back to owing, the second kept"
  );
  assert_eq!(
    counted,
    (3, 2, false),
    "three made, two dropped (first, then second), the first made at the end"
  );
}

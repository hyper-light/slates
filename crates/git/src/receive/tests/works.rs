//! receive-pack's part 2 (A-620 with A-618, A-638 and A-642): a push's commands made works over a
//! real store's views, judged through a session, and appended through a model of per-green logs.

use std::collections::BTreeMap;
use std::convert::Infallible;

use slates_merge::push::{self as work, HEAD_ATTRIBUTE, ObjectKey, ROOT};
use slates_merge::{Green, Increment, Outcome as Verdict};
use slates_vcs::ContentStore;

use super::{ADVERTISED, commit_text, pack_of, request, unbounded, zero};
use crate::object_name::{Kind, ObjectFormat, ObjectId, name_of};
use crate::receive::works::{
  Appends, Entry, GreenRead, Greens, KIND_CHANGED, Unappended, Version, Works,
};
use crate::receive::{
  ATOMIC_TRANSACTION_FAILED, Declined, FAILED_TO_UPDATE, NoneHidden, Outcome, Received, Session,
  message,
};
use crate::store::views::{IndexedObjects, PushedObjects};
use crate::store::{Incoming, Push, Repository};

/// Shape: the store's window, as A-638's tests size it.
const WINDOW: usize = 256;
/// Shape: a store budget no test reaches.
const BUDGET: u64 = 1 << 30;
/// Shape: the room a view and a work may keep, where the room is not what a test is about.
const ROOM: u64 = 1 << 30;
/// Shape: the served branch's green.
const GREEN: u64 = 1;
/// The served branch.
const MAIN: &[u8] = b"refs/heads/main";
/// The push's name, which every work's identity begins with.
const NAME: [u8; 32] = *b"A-620 part 2, a push to a store.";

/// `id` as A-618 keys objects.
fn key(id: ObjectId) -> ObjectKey {
  ObjectKey::new(id.as_bytes()).unwrap()
}

/// A tree of `entries`, each its mode, its name and its object, in the order given.
fn tree_bytes(entries: &[(&[u8], &[u8], ObjectId)]) -> Vec<u8> {
  let mut out = Vec::new();
  for (mode, name, id) in entries {
    out.extend_from_slice(mode);
    out.push(b' ');
    out.extend_from_slice(name);
    out.push(0);
    out.extend_from_slice(id.as_bytes());
  }
  out
}

/// The name of `content` as an object of `kind`.
fn id_of(kind: Kind, content: &[u8]) -> ObjectId {
  name_of(ObjectFormat::Sha1, kind, content).unwrap()
}

/// Admits `content` as a `kind` to the quarantine `push`: its id.
fn admitted(
  store: &mut ContentStore,
  repository: &Repository,
  push: &mut Push,
  kind: Kind,
  content: &[u8],
) -> ObjectId {
  let incoming = Incoming {
    kind,
    claimed: None,
    content,
    depth: 0,
  };
  let naming = repository.naming();
  push
    .admit::<_, Infallible>(store, &naming, incoming)
    .unwrap()
    .0
}

/// A repository of one branch, `main`, at its first version, `file` holding `one\n`: the store,
/// its index, the engine's green, and that version's head and tree.
struct Served {
  store: ContentStore,
  repository: Repository,
  green: Green,
  head: ObjectId,
  tree: ObjectId,
}

impl Served {
  /// The repository, its first version committed to the store and to the green.
  fn new() -> Self {
    let mut store = ContentStore::new(BUDGET, WINDOW).unwrap();
    let mut repository = Repository::new(ObjectFormat::Sha1);
    let mut push = Push::new(ObjectFormat::Sha1);
    let mut put = |kind, bytes: &[u8]| admitted(&mut store, &repository, &mut push, kind, bytes);
    let blob = put(Kind::Blob, b"one\n");
    let tree = put(Kind::Tree, &tree_bytes(&[(b"100644", b"file", blob)]));
    let head = put(Kind::Commit, &commit_text(tree, &[]));
    repository.seal(&store, &mut push).unwrap();
    let mut green = Green::new();
    let first = work::Push {
      name: NAME,
      base_version: 0,
      base_tree: None,
      tree: key(tree),
      head: Some(head.as_bytes()),
    };
    let objects = PushedObjects::new(&repository, &store, &push);
    let built = work::build(&first, &green, &objects, ROOM).unwrap();
    drop(objects);
    let verdict = green.submit(&built.increment);
    assert!(matches!(verdict, Verdict::Accepted { version: 1 }), "{verdict:?}");
    let moved = repository.admit(&store, &push, GREEN, Some(head)).unwrap();
    repository.commit(&mut store, &mut push, &[moved]).unwrap();
    Self {
      store,
      repository,
      green,
      head,
      tree,
    }
  }
}

/// The served branch as one push reads it: its green at its head, and its one version.
struct Read<'s> {
  served: &'s Served,
}

impl Greens for Read<'_> {
  fn read(&self, name: &[u8]) -> Option<GreenRead<'_>> {
    (name == MAIN).then(|| GreenRead {
      id: GREEN,
      position: self.served.green.head(),
      green: &self.served.green,
    })
  }

  fn version_of(&self, green: u64, head: &ObjectId) -> Option<Version> {
    (green == GREEN && *head == self.served.head).then_some(Version {
      number: 1,
      tree: self.served.tree,
    })
  }
}

/// A-642's appends over a model of per-green logs: each green's position, the names greens were
/// made under, the greens another push moves before this push's append on them, and every entry
/// appended, in order.
#[derive(Default)]
struct Logs {
  positions: BTreeMap<u64, u64>,
  names: Vec<Vec<u8>>,
  racing: Vec<u64>,
  appended: Vec<Entry>,
}

impl Logs {
  /// The served branch's log at its first version.
  fn served() -> Self {
    Self {
      positions: BTreeMap::from([(GREEN, 1)]),
      names: vec![MAIN.to_vec()],
      ..Self::default()
    }
  }

  /// Whether `entry` may be appended now, after any race on its green.
  fn fits(&mut self, entry: &Entry) -> bool {
    match entry {
      Entry::Work {
        green, position, ..
      }
      | Entry::Retire { green, position } => {
        let now = self.positions.entry(*green).or_default();
        if self.racing.contains(green) {
          *now += 1;
        }
        *now == *position
      }
      Entry::Create { name, .. } => !self.names.contains(name),
    }
  }

  /// Moves the log `entry` is appended to.
  fn advance(&mut self, entry: &Entry) {
    match entry {
      Entry::Work { green, .. } | Entry::Retire { green, .. } => {
        *self.positions.entry(*green).or_default() += 1;
      }
      Entry::Create { name, .. } => self.names.push(name.clone()),
    }
  }
}

impl Appends for Logs {
  fn append(&mut self, entry: Entry) -> Result<(), Unappended> {
    if !self.fits(&entry) {
      return Err(Unappended::Moved { at: 0 });
    }
    self.advance(&entry);
    self.appended.push(entry);
    Ok(())
  }

  fn append_atomic(&mut self, entries: Vec<Entry>) -> Result<(), Unappended> {
    let fit: Vec<bool> = entries.iter().map(|entry| self.fits(entry)).collect();
    if let Some(at) = fit.iter().position(|fits| !fits) {
      return Err(Unappended::Moved { at });
    }
    for entry in &entries {
      self.advance(entry);
    }
    self.appended.extend(entries);
    Ok(())
  }
}

/// What a push of `bytes` to `served` gives through a session: checked against the index, its
/// objects admitted to a quarantine and sealed, then decided by part 2's judge appending to `logs`.
fn pushed(served: &mut Served, bytes: &[u8], logs: &mut Logs) -> Received {
  let mut session = Session::new(&ADVERTISED, &NoneHidden);
  let indexed = IndexedObjects::new(&served.repository, &served.store, ROOM);
  assert_eq!(session.input(bytes, &indexed, &mut unbounded()), Ok(true));
  drop(indexed);
  let mut quarantine = Push::new(ObjectFormat::Sha1);
  for object in session.checked().unwrap().objects() {
    let (store, repository) = (&mut served.store, &served.repository);
    admitted(store, repository, &mut quarantine, object.kind, &object.content);
  }
  served.repository.seal(&served.store, &mut quarantine).unwrap();
  let indexed = IndexedObjects::new(&served.repository, &served.store, ROOM);
  let objects = PushedObjects::new(&served.repository, &served.store, &quarantine);
  let read = Read { served };
  let mut works = Works::new(&read, &indexed, &objects, logs, NAME, ROOM);
  let mut answer = Vec::new();
  let received = session.answer(&mut works, &mut answer, &mut unbounded());
  received.unwrap().unwrap()
}

/// A client's next commit on `parent`: `file` holding `bytes`. Its objects as a pack's, and its id.
fn next_commit(parent: &[ObjectId], bytes: &[u8]) -> (Vec<(Kind, Vec<u8>)>, ObjectId) {
  let blob = id_of(Kind::Blob, bytes);
  let tree = tree_bytes(&[(b"100644", b"file", blob)]);
  let commit = commit_text(id_of(Kind::Tree, &tree), parent);
  let id = id_of(Kind::Commit, &commit);
  (
    vec![
      (Kind::Blob, bytes.to_vec()),
      (Kind::Tree, tree),
      (Kind::Commit, commit),
    ],
    id,
  )
}

/// A push of `commands` with `objects` as its pack, `capabilities` asked.
fn push_of(
  commands: &[(ObjectId, ObjectId, &[u8])],
  objects: &[(Kind, Vec<u8>)],
  capabilities: &[u8],
) -> Vec<u8> {
  let objects: Vec<(Kind, &[u8])> = objects
    .iter()
    .map(|(kind, bytes)| (*kind, bytes.as_slice()))
    .collect();
  request(&[], commands, capabilities, &[], &pack_of(&objects))
}

/// The one work `logs` appended, or none.
fn only_increment(logs: &Logs) -> Option<&Increment> {
  match logs.appended.as_slice() {
    [Entry::Work { increment, .. } | Entry::Create { increment, .. }] => Some(increment),
    _ => None,
  }
}

/// Asserts that `green` holds `file` as `bytes` and names `head` as its head.
fn assert_holds(green: &Green, bytes: &[u8], head: ObjectId) {
  assert_eq!(green.content("file"), Some(bytes));
  assert_eq!(green.xattr(ROOT, HEAD_ATTRIBUTE), Some(head.as_bytes()));
}

/// A refusal for `reason` naming `name` and `detail` on band 2.
fn refused(reason: &[u8], name: &[u8], detail: &[u8]) -> Outcome {
  Outcome::Ng(Declined {
    reason: reason.to_vec(),
    message: Some(message(&[name, b": ", detail].concat())),
  })
}

/// A-620 part 2 (§4.17.1 "A push is a work" items 1 to 4; A-642). Do: push the served branch's next
/// commit, its old value the branch's head. Expect: `ok`; one work appended on the branch's own log
/// at the position read; that work, submitted to the green, its next version holding the client's
/// bytes and naming the client's commit as its head.
#[test]
fn an_update_is_its_branchs_next_work_appended_at_the_position_read() {
  let mut served = Served::new();
  let (objects, next) = next_commit(&[served.head], b"two\n");
  let bytes = push_of(&[(served.head, next, MAIN)], &objects, b" report-status");
  let mut logs = Logs::served();
  let received = pushed(&mut served, &bytes, &mut logs);
  assert_eq!(received.outcomes, [Outcome::Ok(Vec::new())]);
  let at_read = matches!(
    logs.appended.as_slice(),
    [Entry::Work {
      green: GREEN,
      position: 1,
      ..
    }]
  );
  assert!(at_read, "{:?}", logs.appended);
  let increment = only_increment(&logs).unwrap();
  let verdict = served.green.submit(increment);
  assert!(matches!(verdict, Verdict::Accepted { version: 2 }), "{verdict:?}");
  assert_holds(&served.green, b"two\n", next);
}

/// A-620 part 2 (§4.17.1 item 1: "A value that no version of the green ever held ... is answered as
/// the tool answers a ref that moved, before any work is built"). Do: push the next commit with an
/// old value no version held, then create the branch that exists. Expect: each `failed to update
/// ref` with its band-2 line; nothing appended.
#[test]
fn an_old_value_no_version_held_is_a_ref_that_moved() {
  let mut served = Served::new();
  let (objects, next) = next_commit(&[served.head], b"two\n");
  let never = id_of(Kind::Commit, b"a commit no version held");
  for old in [never, zero()] {
    let bytes = push_of(&[(old, next, MAIN)], &objects, b" report-status");
    let mut logs = Logs::served();
    let received = pushed(&mut served, &bytes, &mut logs);
    let detail = b"cannot lock ref: it is not at the value the push read";
    assert_eq!(received.outcomes, [refused(FAILED_TO_UPDATE, MAIN, detail)]);
    assert!(logs.appended.is_empty());
  }
}

/// A-620 part 2 (§4.17.1's namespace item 4: a create makes a new green). Do: create `topic` at a
/// new commit. Expect: `ok`; one create appended under its name; its work, submitted to a new
/// green, holding the client's bytes and head.
#[test]
fn a_create_is_a_new_green_holding_the_clients_tree() {
  let mut served = Served::new();
  let (objects, topic) = next_commit(&[served.head], b"topic\n");
  let bytes = push_of(&[(zero(), topic, b"refs/heads/topic")], &objects, b" report-status");
  let mut logs = Logs::served();
  let received = pushed(&mut served, &bytes, &mut logs);
  assert_eq!(received.outcomes, [Outcome::Ok(Vec::new())]);
  let named = matches!(logs.appended.as_slice(), [Entry::Create { name, .. }] if name == b"refs/heads/topic");
  assert!(named, "{:?}", logs.appended);
  let mut green = Green::new();
  let verdict = green.submit(only_increment(&logs).unwrap());
  assert!(matches!(verdict, Verdict::Accepted { version: 1 }), "{verdict:?}");
  assert_holds(&green, b"topic\n", topic);
}

/// A-620 part 2 (§4.17.1 item 6: a tag is a green). Do: create `refs/tags/v1` at an annotated tag
/// of the served head. Expect: one create appended; its work, submitted to a new green, holding the
/// tagged commit's tree and naming the tag as its head.
#[test]
fn an_annotated_tag_is_a_green_of_its_commits_tree() {
  let mut served = Served::new();
  let tag = format!(
    "object {}\ntype commit\ntag v1\ntagger A <a@example.com> 1 +0000\n\nm\n",
    served.head
  )
  .into_bytes();
  let tag_id = id_of(Kind::Tag, &tag);
  let objects = vec![(Kind::Tag, tag)];
  let bytes = push_of(&[(zero(), tag_id, b"refs/tags/v1")], &objects, b" report-status");
  let mut logs = Logs::served();
  let received = pushed(&mut served, &bytes, &mut logs);
  assert_eq!(received.outcomes, [Outcome::Ok(Vec::new())]);
  let mut green = Green::new();
  let verdict = green.submit(only_increment(&logs).unwrap());
  assert!(matches!(verdict, Verdict::Accepted { version: 1 }), "{verdict:?}");
  assert_holds(&green, b"one\n", tag_id);
}

/// A-620 part 2 (§4.17.1's namespace item 5: deleting a branch retires its green). Do: delete the
/// branch at its head, then at a value no version held. Expect: the first `ok` with one retirement
/// appended at the position read; the second `failed to update ref`, nothing appended.
#[test]
fn a_delete_retires_its_green_only_at_its_head() {
  let mut served = Served::new();
  let bytes = push_of(&[(served.head, zero(), MAIN)], &[], b" report-status");
  let mut logs = Logs::served();
  let received = pushed(&mut served, &bytes, &mut logs);
  assert_eq!(received.outcomes, [Outcome::Ok(Vec::new())]);
  let retired = Entry::Retire {
    green: GREEN,
    position: 1,
  };
  assert_eq!(logs.appended, [retired]);
  let never = id_of(Kind::Commit, b"a commit no version held");
  let stale = push_of(&[(never, zero(), MAIN)], &[], b" report-status");
  let mut logs = Logs::served();
  let received = pushed(&mut served, &stale, &mut logs);
  let detail = b"cannot lock ref: it is not at the value the push read";
  assert_eq!(received.outcomes, [refused(FAILED_TO_UPDATE, MAIN, detail)]);
  assert!(logs.appended.is_empty());
}

/// A-620 part 2 (A-642: appended only while the green is at the position read). Do: push the next
/// commit while another push moves the branch first, without `atomic` and then with it beside a
/// create. Expect: `failed to update ref` naming the branch, then every command `atomic transaction
/// failed`, the band-2 line once; nothing appended either time.
#[test]
fn a_branch_moved_before_its_append_is_refused_and_nothing_is_appended() {
  let mut served = Served::new();
  let (objects, next) = next_commit(&[served.head], b"two\n");
  let detail = b"cannot lock ref: it moved since the push read it";
  let bytes = push_of(&[(served.head, next, MAIN)], &objects, b" report-status");
  let mut logs = Logs {
    racing: vec![GREEN],
    ..Logs::served()
  };
  let received = pushed(&mut served, &bytes, &mut logs);
  assert_eq!(received.outcomes, [refused(FAILED_TO_UPDATE, MAIN, detail)]);
  assert!(logs.appended.is_empty());
  let commands: [(ObjectId, ObjectId, &[u8]); 2] = [
    (zero(), next, b"refs/heads/topic"),
    (served.head, next, MAIN),
  ];
  let atomic = push_of(&commands, &objects, b" report-status atomic");
  let mut logs = Logs {
    racing: vec![GREEN],
    ..Logs::served()
  };
  let received = pushed(&mut served, &atomic, &mut logs);
  let first = refused(ATOMIC_TRANSACTION_FAILED, MAIN, detail);
  let other = Outcome::Ng(Declined {
    reason: ATOMIC_TRANSACTION_FAILED.to_vec(),
    message: None,
  });
  assert_eq!(received.outcomes, [first, other]);
  assert!(logs.appended.is_empty());
}

/// A-620 part 2 (A-618's typed refusals, in slates' words where git has none). Do: push a commit
/// whose tree makes the file `file` a directory. Expect: `path changes kind`, its band-2 line naming
/// the path; nothing appended.
#[test]
fn a_work_a_store_cannot_build_is_refused_in_slates_words_naming_its_path() {
  let mut served = Served::new();
  let inner = id_of(Kind::Blob, b"inner\n");
  let directory = tree_bytes(&[(b"100644", b"inner", inner)]);
  let tree = tree_bytes(&[(b"40000", b"file", id_of(Kind::Tree, &directory))]);
  let commit = commit_text(id_of(Kind::Tree, &tree), &[served.head]);
  let next = id_of(Kind::Commit, &commit);
  let objects = vec![
    (Kind::Blob, b"inner\n".to_vec()),
    (Kind::Tree, directory),
    (Kind::Tree, tree),
    (Kind::Commit, commit),
  ];
  let bytes = push_of(&[(served.head, next, MAIN)], &objects, b" report-status");
  let mut logs = Logs::served();
  let received = pushed(&mut served, &bytes, &mut logs);
  assert_eq!(received.outcomes, [refused(KIND_CHANGED, MAIN, b"file")]);
  assert!(logs.appended.is_empty());
}

//! Tests of the repository's index on the shared tables (A-588 part 2): the history stock Mercurial 7.2 stored in
//! A-588's fixtures appended as a push's groups would bring it and numbered as 7.2 numbered it, Mercurial's naming
//! through the shared store's arrival check, and a model of 7.2's `addgroup` over generated histories.

use std::collections::BTreeMap;

use slates_vcs::{AdmitRefused, Admitted, Arriving, ContentId, ContentStore, Quarantine};

use crate::fixtures::objects::{file, names};
use crate::index::{
  CHANGESET, FILE, HgIndex, HgNaming, Log, MANIFEST, NONE, NewChangeset, NewRevision, Refused,
  naming_context,
};
use crate::{Admission, NODE_LEN, NULL_NODE, Node, changeset, filerev};
use proptest::prelude::*;

/// The window the tests' stores are made with: room for a few rows a table.
const WINDOW: usize = 1 << 12;

/// A store with room for the fixtures' history many times over.
fn store() -> ContentStore {
  ContentStore::new(1 << 24, WINDOW).unwrap()
}

/// One revision as 7.2's `debugindex --debug` listed it.
struct Listed {
  rev: u32,
  link: u32,
  node: Node,
  p1: Node,
  p2: Node,
  p1_rev: u32,
  p2_rev: u32,
}

/// A listing's revisions, its columns found by the header's names.
fn listed(name: &str) -> Vec<Listed> {
  let text = core::str::from_utf8(file(name)).unwrap();
  let mut lines = text.lines();
  let header: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
  let at = |column: &str| header.iter().position(|known| *known == column).unwrap();
  let number = |value: &str| {
    if value == "-1" {
      NONE
    } else {
      value.parse().unwrap()
    }
  };
  lines
    .map(|line| {
      let fields: Vec<&str> = line.split_whitespace().collect();
      Listed {
        rev: fields[at("rev")].parse().unwrap(),
        link: fields[at("linkrev")].parse().unwrap(),
        node: crate::fixtures::node(fields[at("nodeid")]),
        p1: crate::fixtures::node(fields[at("p1-nodeid")]),
        p2: crate::fixtures::node(fields[at("p2-nodeid")]),
        p1_rev: number(fields[at("p1-rev")]),
        p2_rev: number(fields[at("p2-rev")]),
      }
    })
    .collect()
}

/// The file paths the fixtures hold a log for.
fn paths() -> Vec<String> {
  names()
    .filter_map(|name| name.strip_prefix("index-file-")?.strip_suffix(".txt"))
    .map(str::to_string)
    .collect()
}

/// A changeset's branch and whether it closes it, as 7.2 reads its extras.
fn branch_of(text: &[u8]) -> (Vec<u8>, bool) {
  let read = changeset::decode(text).unwrap();
  let mut buffer = Vec::new();
  let extras = read
    .extras(&mut Admission::new(1 << 20), &mut buffer)
    .unwrap();
  let branch = extras
    .iter()
    .rev()
    .find(|(key, _)| *key == b"branch")
    .map_or(b"default".to_vec(), |(_, value)| value.to_vec());
  (branch, extras.iter().any(|(key, _)| *key == b"close"))
}

/// The fixtures' history appended to an empty index, each log one group, as a push's changegroup brings them:
/// the changelog, the manifest log, then each file's log. Answers the index, its store, and each log's numbers.
/// An index with its store, and the numbers each log's group was given, by log.
type Appended = (HgIndex, ContentStore, BTreeMap<String, Vec<u32>>);

fn appended() -> Appended {
  let mut store = store();
  let mut index = HgIndex::new();
  let mut numbers = BTreeMap::new();
  let changelog = listed("index-changelog.txt");
  let texts: Vec<&[u8]> = changelog
    .iter()
    .map(|row| file(&format!("changeset-{}.txt", row.rev)))
    .collect();
  let branches: Vec<(Vec<u8>, bool)> = texts.iter().map(|text| branch_of(text)).collect();
  let group: Vec<NewChangeset<'_>> = changelog
    .iter()
    .zip(&texts)
    .zip(&branches)
    .map(|((row, text), (branch, closed))| NewChangeset {
      node: row.node,
      p1: row.p1,
      p2: row.p2,
      flags: 0,
      len: u64::try_from(text.len()).unwrap(),
      text: store.put(text).unwrap(),
      manifest: changeset::decode(text).unwrap().manifest_node().unwrap(),
      closed: *closed,
      branch,
    })
    .collect();
  numbers.insert(
    "changelog".to_string(),
    index.append_changesets(&mut store, &group).unwrap(),
  );
  let changeset_node = |rev: u32| changelog[usize::try_from(rev).unwrap()].node;
  let manifests = listed("index-manifest.txt");
  let group: Vec<NewRevision> = manifests
    .iter()
    .map(|row| {
      let text = file(&format!("manifest-{}.txt", row.rev));
      NewRevision {
        node: row.node,
        p1: row.p1,
        p2: row.p2,
        link: changeset_node(row.link),
        flags: 0,
        len: u64::try_from(text.len()).unwrap(),
        bytes: store.put(text).unwrap(),
        block: None,
      }
    })
    .collect();
  numbers.insert(
    "manifest".to_string(),
    index
      .append_revisions(&mut store, Log::Manifest, &group)
      .unwrap(),
  );
  for path in paths() {
    let group: Vec<NewRevision> = listed(&format!("index-file-{path}.txt"))
      .iter()
      .map(|row| {
        let text = file(&format!("file-{path}-{}.txt", row.rev));
        let split = filerev::split(text).unwrap();
        NewRevision {
          node: row.node,
          p1: row.p1,
          p2: row.p2,
          link: changeset_node(row.link),
          flags: 0,
          len: u64::try_from(text.len()).unwrap(),
          bytes: store.put(split.bytes).unwrap(),
          block: split.metadata.map(|block| store.put(block).unwrap()),
        }
      })
      .collect();
    let revs = index
      .append_revisions(&mut store, Log::File(path.as_bytes()), &group)
      .unwrap();
    numbers.insert(path, revs);
  }
  (index, store, numbers)
}

/// Holds the changelog's rows to 7.2's listing: numbers, parents, nodes, branches and closed heads.
fn check_changelog(index: &HgIndex, store: &ContentStore, numbers: &[u32]) {
  let changelog = listed("index-changelog.txt");
  assert_eq!(
    numbers,
    changelog.iter().map(|row| row.rev).collect::<Vec<_>>()
  );
  assert_eq!(index.len(store, Log::Changelog), 11);
  for row in &changelog {
    let read = index.changeset(store, row.rev).unwrap();
    let (branch, closed) = branch_of(file(&format!("changeset-{}.txt", row.rev)));
    assert_eq!(
      (read.node, read.p1, read.p2, read.branch, read.closed),
      (row.node, row.p1_rev, row.p2_rev, branch.as_slice(), closed),
      "changeset {}",
      row.rev
    );
    assert_eq!(index.rev(store, Log::Changelog, &row.node), Some(row.rev));
  }
}

/// Holds one manifest or file log's rows to 7.2's listing: numbers, parents, links and nodes both ways.
fn check_log(index: &HgIndex, store: &ContentStore, log: Log<'_>, numbers: &[u32], listing: &str) {
  let rows = listed(listing);
  assert_eq!(
    numbers,
    rows.iter().map(|row| row.rev).collect::<Vec<_>>(),
    "{listing}"
  );
  assert_eq!(
    usize::try_from(index.len(store, log)).unwrap(),
    rows.len(),
    "{listing}"
  );
  for row in rows {
    let read = index.revision(store, log, row.rev).unwrap();
    assert_eq!(
      (read.node, read.p1, read.p2, read.link),
      (row.node, row.p1_rev, row.p2_rev, row.link),
      "{listing} {}",
      row.rev
    );
    assert_eq!(index.rev(store, log, &row.node), Some(row.rev));
  }
}

/// A-588 part 2 (golden): do append the history 7.2 stored, each log as one group in 7.2's order. Expect every
/// revision numbered as 7.2's `debugindex` numbered it, its parents' and its link's numbers 7.2's, its node found
/// under its number and its number under its node, each log as long as 7.2's, the changesets' branches (`stable`
/// among them) and the closed head kept, and a file revision's block apart from its bytes.
#[test]
fn the_history_7_2_stored_is_numbered_as_7_2_numbered_it() {
  let (index, store, numbers) = appended();
  check_changelog(&index, &store, &numbers["changelog"]);
  check_log(
    &index,
    &store,
    Log::Manifest,
    &numbers["manifest"],
    "index-manifest.txt",
  );
  for path in paths() {
    let listing = format!("index-file-{path}.txt");
    check_log(
      &index,
      &store,
      Log::File(path.as_bytes()),
      &numbers[&path],
      &listing,
    );
  }
  assert!((0..11).any(|rev| index.changeset(&store, rev).unwrap().closed));
  let copy = index.revision(&store, Log::File(b"c"), 0).unwrap();
  assert!(copy.block.is_some());
  let meta = index.revision(&store, Log::File(b"meta"), 0).unwrap();
  assert_eq!(
    meta.block.map(|block| store.get(&block).map(<[u8]>::len)),
    Some(Some(0))
  );
}

/// A-588 part 2 (golden): do name every revision 7.2 stored by Mercurial's naming, a file revision's block given as
/// context and its bytes alone as the object, and admit each through the shared store's quarantine, once with its node
/// and once with a node one bit off. Expect each named as 7.2 named it, admitted under its node, and the wrong one
/// refused as an id mismatch.
#[test]
fn mercurial_names_what_the_shared_store_admits() {
  let mut store = store();
  let mut quarantine = Quarantine::new();
  let mut admitted = 0;
  let mut logs: Vec<(u8, String, String)> = vec![
    (
      CHANGESET,
      "index-changelog.txt".to_string(),
      "changeset".to_string(),
    ),
    (
      MANIFEST,
      "index-manifest.txt".to_string(),
      "manifest".to_string(),
    ),
  ];
  for path in paths() {
    logs.push((
      FILE,
      format!("index-file-{path}.txt"),
      format!("file-{path}"),
    ));
  }
  for (kind, listing, prefix) in logs {
    for row in listed(&listing) {
      let text = file(&format!("{prefix}-{}.txt", row.rev));
      let (block, bytes) = if kind == FILE {
        let split = filerev::split(text).unwrap();
        (split.metadata, split.bytes)
      } else {
        (None, text)
      };
      let mut context = Vec::new();
      naming_context(&row.p1, &row.p2, block, &mut context);
      let arriving = Arriving {
        id: &row.node,
        kind,
        context: &context,
        bytes,
      };
      assert_eq!(
        quarantine.admit(&mut store, &HgNaming, arriving),
        Ok(Admitted::New)
      );
      let mut wrong = row.node;
      wrong[NODE_LEN - 1] ^= 1;
      let lie = Arriving {
        id: &wrong,
        ..arriving
      };
      assert!(matches!(
        quarantine.admit(&mut store, &HgNaming, lie),
        Err(AdmitRefused::IdMismatch { .. })
      ));
      admitted += 1;
    }
  }
  assert_eq!(admitted, 30);
}

/// A file revision whose node is `node`, with parents, linked to `link`, its bytes `bytes`.
fn revision(node: u8, p1: Option<u8>, p2: Option<u8>, link: Node, bytes: ContentId) -> NewRevision {
  let named = |byte: Option<u8>| byte.map_or(NULL_NODE, |byte| [byte; NODE_LEN]);
  NewRevision {
    node: [node; NODE_LEN],
    p1: named(p1),
    p2: named(p2),
    link,
    flags: 0,
    len: 1,
    bytes,
    block: None,
  }
}

/// An index holding one changeset, its node `[9; 20]`, and the content `x` held.
fn with_one_changeset() -> (HgIndex, ContentStore, ContentId) {
  let mut store = store();
  let mut index = HgIndex::new();
  let text = store.put(b"x").unwrap();
  let changeset = NewChangeset {
    node: [9; NODE_LEN],
    p1: NULL_NODE,
    p2: NULL_NODE,
    flags: 0,
    len: 1,
    text,
    manifest: NULL_NODE,
    closed: false,
    branch: b"default",
  };
  assert_eq!(
    index.append_changesets(&mut store, &[changeset]),
    Ok(vec![0])
  );
  (index, store, text)
}

/// The 20 bytes that keyed a log row of `path` at revision `rev` before the rows carried kind tags of their own beside
/// object rows: `L`, `f`, the path, a NUL and the number, big-endian. A 13-byte path's are a node's length.
fn untagged_log_key(path: &[u8], rev: u32) -> Vec<u8> {
  [&b"Lf"[..], path, &[0], &rev.to_be_bytes()].concat()
}

/// A-649 (the sibling A-630 found): do append a revision of a 13-byte path, whose log row is twenty bytes long, and
/// commit an object whose twenty-byte id is exactly those bytes. Expect both rows kept and each read back as its kind:
/// the log's revision under its number, and the object's kind, length and content under its node; the quarantine's
/// holds given back.
#[test]
fn a_log_row_never_meets_an_object_key_whatever_a_path_s_length() {
  let (mut index, mut store, content) = with_one_changeset();
  let path = b"thirteen-byte";
  let log = Log::File(path);
  let group = [revision(1, None, None, [9; NODE_LEN], content)];
  assert_eq!(index.append_revisions(&mut store, log, &group), Ok(vec![0]));
  let id = untagged_log_key(path, 0);
  assert_eq!(id.len(), NODE_LEN);
  let mut quarantine = Quarantine::new();
  let held = quarantine.hold_named(&mut store, &id, FILE, b"the object's bytes");
  assert_eq!(held, Ok(Admitted::New));
  assert!(index.commit_objects(&mut store, &mut quarantine).is_ok());
  let node = Node::try_from(id.as_slice()).unwrap();
  let (kind, len, root) = index.object(&store, &node).unwrap();
  let object = (kind, len, store.get(&root));
  assert_eq!(object, (FILE, 18, Some(&b"the object's bytes"[..])));
  let revision = index.revision(&store, log, 0).map(|row| row.node);
  assert_eq!(
    (revision, quarantine.is_empty()),
    (Some([1; NODE_LEN]), true)
  );
}

/// AC-HG.9, A-649: do commit an object whose id is not a node's twenty bytes. Expect it refused by kind with nothing
/// written and the quarantine still holding it.
#[test]
fn an_object_whose_id_is_not_a_node_is_refused() {
  let (mut index, mut store, _) = with_one_changeset();
  let mut quarantine = Quarantine::new();
  let held = quarantine.hold_named(&mut store, b"short", FILE, b"bytes");
  assert_eq!(held, Ok(Admitted::New));
  let refused = index.commit_objects(&mut store, &mut quarantine);
  assert_eq!(refused, Err(Refused::NotANode { len: 5 }));
  assert_eq!(quarantine.len(), 1);
}

/// AC-HG.9 (hostile input), A-588 part 2: do append a group whose second revision names a parent no log holds, one
/// whose link names no changeset, and one naming a changeset as a file's link with a parent from another file's log.
/// Expect each refused by kind with nothing of it written: the log as long as before, the group's first revision not
/// held.
#[test]
fn a_group_with_a_missing_object_writes_nothing() {
  let (mut index, mut store, content) = with_one_changeset();
  let link = [9; NODE_LEN];
  let log = Log::File(b"a");
  let unknown_parent = [
    revision(1, None, None, link, content),
    revision(2, Some(7), None, link, content),
  ];
  assert_eq!(
    index.append_revisions(&mut store, log, &unknown_parent),
    Err(Refused::MissingParent {
      node: [7; NODE_LEN]
    })
  );
  assert_eq!(
    (
      index.len(&store, log),
      index.rev(&store, log, &[1; NODE_LEN])
    ),
    (0, None)
  );
  let unknown_link = [revision(1, None, None, [8; NODE_LEN], content)];
  assert_eq!(
    index.append_revisions(&mut store, log, &unknown_link),
    Err(Refused::MissingLink {
      node: [8; NODE_LEN]
    })
  );
  assert_eq!(
    index.append_revisions(
      &mut store,
      Log::File(b"b"),
      &[revision(1, None, None, link, content)]
    ),
    Ok(vec![0])
  );
  assert_eq!(
    index.append_revisions(
      &mut store,
      log,
      &[revision(2, Some(1), None, link, content)]
    ),
    Err(Refused::MissingParent {
      node: [1; NODE_LEN]
    })
  );
  assert_eq!(index.len(&store, log), 0);
}

/// A-588 part 2: do append a group, then the same group again, then a group that repeats one of its revisions beside a
/// new one; and append one node to two files' logs. Expect the second group passed over whole (its numbers the first
/// ones, no row added), the third numbering only its new revision, and the node numbered 0 in each file's log.
#[test]
fn a_node_the_log_holds_is_passed_over() {
  let (mut index, mut store, content) = with_one_changeset();
  let link = [9; NODE_LEN];
  let log = Log::File(b"a");
  let group = [
    revision(1, None, None, link, content),
    revision(2, Some(1), None, link, content),
  ];
  assert_eq!(
    index.append_revisions(&mut store, log, &group),
    Ok(vec![0, 1])
  );
  let rows = index.rows().rows();
  assert_eq!(
    index.append_revisions(&mut store, log, &group),
    Ok(vec![0, 1])
  );
  assert_eq!(index.rows().rows(), rows);
  let mixed = [
    revision(2, Some(1), None, link, content),
    revision(3, Some(2), Some(1), link, content),
  ];
  assert_eq!(
    index.append_revisions(&mut store, log, &mixed),
    Ok(vec![1, 2])
  );
  let read = index.revision(&store, log, 2).unwrap();
  assert_eq!((read.p1, read.p2, read.link), (1, 0, 0));
  assert_eq!(
    index.append_revisions(
      &mut store,
      Log::File(b"b"),
      &[revision(1, None, None, link, content)]
    ),
    Ok(vec![0])
  );
  assert_eq!(index.rev(&store, Log::File(b"b"), &[1; NODE_LEN]), Some(0));
  assert_eq!(index.rev(&store, log, &[1; NODE_LEN]), Some(0));
}

/// A-588 part 2: do fork an index holding a file's two revisions, append a third to the fork and a different third to
/// the parent. Expect each to see the first two and only its own third, numbered 2 in each.
#[test]
fn a_fork_shares_the_rows_and_keeps_its_own() {
  let (mut index, mut store, content) = with_one_changeset();
  let link = [9; NODE_LEN];
  let log = Log::File(b"a");
  let group = [
    revision(1, None, None, link, content),
    revision(2, Some(1), None, link, content),
  ];
  index.append_revisions(&mut store, log, &group).unwrap();
  let mut fork = index.fork(&mut store).unwrap();
  assert_eq!(
    fork.append_revisions(
      &mut store,
      log,
      &[revision(3, Some(2), None, link, content)]
    ),
    Ok(vec![2])
  );
  assert_eq!(
    index.append_revisions(
      &mut store,
      log,
      &[revision(4, Some(2), None, link, content)]
    ),
    Ok(vec![2])
  );
  assert_eq!(
    (
      fork.rev(&store, log, &[3; NODE_LEN]),
      fork.rev(&store, log, &[4; NODE_LEN])
    ),
    (Some(2), None)
  );
  assert_eq!(
    (
      index.rev(&store, log, &[4; NODE_LEN]),
      index.rev(&store, log, &[3; NODE_LEN])
    ),
    (Some(2), None)
  );
  assert_eq!((fork.len(&store, log), index.len(&store, log)), (3, 3));
  fork.release(&mut store).unwrap();
  index.release(&mut store).unwrap();
}

/// A generated history's revision at `at`: its node.
fn model_node(at: usize) -> Node {
  [u8::try_from(at + 10).unwrap_or(u8::MAX); NODE_LEN]
}

/// A history's parents: each revision's parents are any other revision of the history, by its choices.
type Parents = Vec<(Node, Node)>;

/// What 7.2's `addgroup` does with `group` (positions of the history, in order) brought onto a log holding `held`, in a
/// model: each revision's number and the log after the group, or the first missing parent's refusal.
fn model_group(
  held: &[usize],
  group: &[usize],
  parents: &Parents,
) -> Result<(Vec<u32>, Vec<usize>), Refused> {
  let mut expected = held.to_vec();
  let mut numbers = Vec::new();
  for at in group {
    if let Some(rev) = expected.iter().position(|kept| kept == at) {
      numbers.push(u32::try_from(rev).unwrap_or(NONE));
      continue;
    }
    let (p1, p2) = parents.get(*at).copied().unwrap_or((NULL_NODE, NULL_NODE));
    let known = |parent: &Node| {
      *parent == NULL_NODE || expected.iter().any(|kept| model_node(*kept) == *parent)
    };
    if let Some(missing) = [p1, p2].into_iter().find(|parent| !known(parent)) {
      return Err(Refused::MissingParent { node: missing });
    }
    expected.push(*at);
    numbers.push(u32::try_from(expected.len() - 1).unwrap_or(NONE));
  }
  Ok((numbers, expected))
}

/// Holds every revision the model's log holds to the index: its node and its parents' numbers.
fn check_model(
  index: &HgIndex,
  store: &ContentStore,
  log: Log<'_>,
  model: &[usize],
  parents: &Parents,
) {
  let number = |parent: Node| {
    if parent == NULL_NODE {
      return NONE;
    }
    let held = model
      .iter()
      .position(|kept| model_node(*kept) == parent)
      .unwrap();
    u32::try_from(held).unwrap()
  };
  for (rev, at) in model.iter().enumerate() {
    let read = index
      .revision(store, log, u32::try_from(rev).unwrap())
      .unwrap();
    let (p1, p2) = parents[*at];
    assert_eq!(
      (read.node, read.p1, read.p2),
      (model_node(*at), number(p1), number(p2))
    );
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-588 part 2 (model): do append generated histories of a file's log (each revision's parents any other revision of
  /// the history, so some name one that comes later or never; some revisions brought twice) in generated groups.
  /// Expect, after every group, what a model of 7.2's `addgroup` gives: the same numbers or the same refusal, a refused
  /// group leaving the log as it was, and every held node's parents numbered as the model numbers them.
  #[test]
  fn appending_equals_7_2_s_addgroup(
    choices in proptest::collection::vec(
      (any::<proptest::sample::Index>(), any::<proptest::sample::Index>(), 0_u8..3),
      1..24,
    ),
    cuts in proptest::collection::vec(any::<proptest::sample::Index>(), 0..6),
    repeats in proptest::collection::vec(any::<proptest::sample::Index>(), 0..4),
  ) {
    let (mut index, mut store, content) = with_one_changeset();
    let log = Log::File(b"f");
    let pick = |at: usize, choice: &proptest::sample::Index| {
      let other = choice.index(choices.len());
      if other == at { NULL_NODE } else { model_node(other) }
    };
    let parents: Parents = choices
      .iter()
      .enumerate()
      .map(|(at, (first, second, count))| {
        let p1 = if *count >= 1 { pick(at, first) } else { NULL_NODE };
        let p2 = if *count >= 2 { pick(at, second) } else { NULL_NODE };
        (p1, p2)
      })
      .collect();
    let mut order: Vec<usize> = (0..choices.len()).collect();
    for repeat in &repeats {
      let at = repeat.index(order.len());
      order.insert(at, order[at]);
    }
    let mut bounds: Vec<usize> = cuts.iter().map(|cut| cut.index(order.len() + 1)).collect();
    bounds.extend([0, order.len()]);
    bounds.sort_unstable();
    bounds.dedup();
    let mut model: Vec<usize> = Vec::new();
    for window in bounds.windows(2) {
      let group = &order[window[0]..window[1]];
      let revisions: Vec<NewRevision> = group
        .iter()
        .map(|at| NewRevision {
          node: model_node(*at),
          p1: parents[*at].0,
          p2: parents[*at].1,
          ..revision(0, None, None, [9; NODE_LEN], content)
        })
        .collect();
      let answer = index.append_revisions(&mut store, log, &revisions);
      match model_group(&model, group, &parents) {
        Ok((numbers, after)) => {
          prop_assert_eq!(answer, Ok(numbers));
          model = after;
        }
        Err(refusal) => prop_assert_eq!(answer, Err(refusal)),
      }
      prop_assert_eq!(index.len(&store, log), u32::try_from(model.len()).unwrap_or(NONE));
    }
    check_model(&index, &store, log, &model, &parents);
  }
}

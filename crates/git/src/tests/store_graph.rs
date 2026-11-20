use std::collections::BTreeSet;

use super::{GraphFault, StoreGraph};
use crate::Admission;
use crate::fixtures::{FETCH_CLONE_RESPONSE, FETCH_HAVES_RESPONSE};
use crate::negotiation::{
  Acknowledgments, Answer, Filter, Graph, Patterns, Request, Run, negotiate,
};
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::tree::Tree;
use crate::served::{self, FIRST, MAIN, REFS, SECOND, SIDE, TAG, THIRD, V1, id};
use crate::v2::fetch::{Event, decode_response};

/// Everything `start` reaches through the graph.
fn reach(graph: &StoreGraph<'_>, start: u64) -> BTreeSet<u64> {
  let mut seen = BTreeSet::new();
  let mut stack = vec![start];
  while let Some(node) = stack.pop() {
    if !seen.insert(node) {
      continue;
    }
    match graph.kind(node) {
      Kind::Commit => {
        stack.extend(graph.parents(node));
        stack.push(graph.commit(node).unwrap().tree);
      }
      Kind::Tree => stack.extend(graph.entries(node)),
      Kind::Tag => stack.extend(graph.target(node)),
      Kind::Blob => {}
    }
  }
  seen
}

/// The names of the objects in the pack a stock git response carries, its thin deltas' bases taken from the served
/// repository.
fn git_pack(response: &[u8], served: &served::Served) -> BTreeSet<ObjectId> {
  let events = decode_response(
    response,
    ObjectFormat::Sha1,
    false,
    &mut Admission::new(u64::MAX),
  )
  .unwrap();
  let pack: Vec<u8> = events
    .iter()
    .filter_map(|event| match event {
      Event::Pack(bytes) => Some(*bytes),
      _ => None,
    })
    .flatten()
    .copied()
    .collect();
  served::pack_names(&pack, &served.objects)
}

/// A-640 part 1: do read every object of the repository stock git served the golden vectors from through the graph.
/// Expect: each name and its node round trip; each kind and blob size as the pack holds it; each commit's generation,
/// root tree and parents; each tree's entries as git's tree parser reads them; the tag's object; and no fault.
#[test]
fn the_store_reads_back_as_the_negotiations_graph() {
  let served = served::served();
  let graph = StoreGraph::new(&served.repository, &served.store, &REFS);
  for object in &served.objects {
    let node = graph.node(&object.id).unwrap();
    assert_eq!(graph.id(node), object.id);
    assert_eq!(graph.kind(node), object.kind);
    match object.kind {
      Kind::Blob => assert_eq!(
        graph.size(node),
        u64::try_from(object.content.len()).unwrap()
      ),
      Kind::Tree => {
        let tree = Tree::parse(
          &object.content,
          ObjectFormat::Sha1,
          &mut Admission::new(u64::MAX),
        )
        .unwrap();
        let expected: BTreeSet<u64> = tree
          .entries()
          .iter()
          .map(|entry| {
            graph
              .node(&entry.object_id(ObjectFormat::Sha1).unwrap())
              .unwrap()
          })
          .collect();
        assert_eq!(graph.entries(node).collect::<BTreeSet<_>>(), expected);
      }
      Kind::Tag => assert_eq!(graph.target(node), graph.node(&id(SECOND))),
      Kind::Commit => {
        let tree = ObjectId::from_hex(ObjectFormat::Sha1, &object.content[5..45]).unwrap();
        assert_eq!(graph.commit(node).unwrap().tree, graph.node(&tree).unwrap());
      }
    }
  }
  let [first, second, third] = [FIRST, SECOND, THIRD].map(|hex| graph.node(&id(hex)).unwrap());
  let generations = [first, second, third].map(|node| graph.commit(node).unwrap().generation);
  assert_eq!(generations, [1, 2, 3]);
  assert_eq!(graph.parents(third).collect::<Vec<_>>(), [second]);
  assert_eq!(graph.parents(second).collect::<Vec<_>>(), [first]);
  assert_eq!(graph.parents(first).count(), 0);
  assert_eq!(graph.fault(), None);
}

/// A-640 part 1: do ask the graph of the refs and versions. Expect: every ref's head a tip and the annotated tag the
/// one tag; the greens each commit heads now, with their current versions (`first` heads `side`, and no longer
/// `main`); the versions each commit headed; each of `main`'s versions a fast-forward, a rewound one not; and D(k)
/// what the version's head reaches and the head before did not.
#[test]
fn refs_versions_and_fast_forwards_read_as_the_store_kept_them() {
  let mut served = served::served();
  served::commit(
    &mut served.repository,
    &mut served.store,
    &[],
    &[(SIDE, THIRD)],
  );
  served::commit(
    &mut served.repository,
    &mut served.store,
    &[],
    &[(SIDE, FIRST)],
  );
  let graph = StoreGraph::new(&served.repository, &served.store, &REFS);
  let [first, second, third, tag] =
    [FIRST, SECOND, THIRD, TAG].map(|hex| graph.node(&id(hex)).unwrap());
  assert_eq!(
    graph.tips().collect::<BTreeSet<_>>(),
    BTreeSet::from([third, first, tag])
  );
  assert_eq!(graph.tags().collect::<Vec<_>>(), [tag]);
  assert_eq!(graph.greens_at(third).collect::<Vec<_>>(), [(MAIN, 2)]);
  assert_eq!(graph.greens_at(first).collect::<Vec<_>>(), [(SIDE, 2)]);
  assert_eq!(graph.greens_at(second).count(), 0);
  assert_eq!(graph.versions_at(MAIN, first).collect::<Vec<_>>(), [0]);
  assert_eq!(graph.versions_at(SIDE, first).collect::<Vec<_>>(), [0, 2]);
  assert!(graph.fast_forward(MAIN, 1) && graph.fast_forward(MAIN, 2));
  assert!(graph.fast_forward(SIDE, 1), "first to third fast-forwards");
  assert!(!graph.fast_forward(SIDE, 2), "third back to first rewinds");
  assert!(!graph.fast_forward(MAIN, 0) && !graph.fast_forward(MAIN, 3));
  let expected: BTreeSet<u64> = reach(&graph, third)
    .difference(&reach(&graph, second))
    .copied()
    .collect();
  assert_eq!(graph.brought(MAIN, 2).collect::<BTreeSet<_>>(), expected);
  assert_eq!(graph.fault(), None);
}

/// A-640 part 1 (the one-round path's guard): do move greens every way a version can follow the one before: onward,
/// onto the same commit, a tag onto itself, back, from a tag to a commit, and on from there, one move a version.
/// Expect: the graph's fast-forward for each version made is the verdict the store gave the move when it admitted it,
/// the tag moved onto itself among the moves it judged no fast-forward.
#[test]
fn the_graphs_fast_forward_is_the_stores_own_verdict_at_admission() {
  let mut served = served::served();
  let moves = [
    (SIDE, THIRD),
    (SIDE, THIRD),
    (V1, TAG),
    (SIDE, FIRST),
    (V1, SECOND),
    (V1, THIRD),
  ];
  let mut judged = Vec::new();
  for (green, head) in moves {
    let made = served::commit(
      &mut served.repository,
      &mut served.store,
      &[],
      &[(green, head)],
    );
    judged.extend(
      made
        .into_iter()
        .map(|(version, verdict)| (green, version, verdict)),
    );
  }
  let verdicts: Vec<bool> = judged.iter().map(|(_, _, verdict)| *verdict).collect();
  assert_eq!(verdicts, [true, true, false, false, false, true]);
  let graph = StoreGraph::new(&served.repository, &served.store, &REFS);
  for (green, version, verdict) in judged {
    assert_eq!(
      graph.fast_forward(green, version),
      verdict,
      "green {green}, version {version}"
    );
  }
  assert_eq!(graph.fault(), None);
}

/// A-640 part 1 (the graph's faults): do read a position the store never gave, twice, then another. Expect: the null
/// name read, and the first fault alone kept.
#[test]
fn a_read_the_store_cannot_make_is_kept_as_the_first_fault() {
  let served = served::served();
  let graph = StoreGraph::new(&served.repository, &served.store, &REFS);
  assert_eq!(
    graph.id(9_999),
    id("0000000000000000000000000000000000000000")
  );
  assert_eq!(graph.kind(10_000), Kind::Blob);
  assert_eq!(graph.fault(), Some(GraphFault::NoRow { position: 9_999 }));
}

/// A-640 part 1 (§4.17.2 "A fetch"; git's `fetch-haves` and `fetch-clone` exchanges): do negotiate over the real store
/// the request git's client sent with `first` as its have, then the clone with no have. Expect: `first` acknowledged
/// and ready in one round, the run of `main`'s versions 1 and 2, nothing walked, the run carrying exactly the objects
/// in the pack stock git sent; and the clone walked, sending exactly the objects stock git's clone pack holds.
#[test]
fn the_store_answers_gits_own_fetches_with_gits_own_objects() {
  let served = served::served();
  let graph = StoreGraph::new(&served.repository, &served.store, &REFS);
  let wants = [id(THIRD)];
  let haves = [id(FIRST)];
  let request = Request {
    wants: &wants,
    haves: &haves,
    done: false,
    wait_for_done: false,
    shallow: &[],
    deepen: None,
    filter: None,
    sparse: None,
    include_tag: false,
    thin: true,
  };
  let outcome = negotiate(&graph, &request, &mut Admission::new(u64::MAX)).unwrap();
  let Answer::Pack(pack) = &outcome.answer else {
    panic!("{:?}", outcome.answer)
  };
  assert_eq!(
    pack.acknowledgments,
    Some(Acknowledgments {
      common: vec![id(FIRST)],
      ready: true
    })
  );
  assert_eq!(
    pack.runs,
    [Run {
      green: MAIN,
      after: 0,
      through: 2
    }]
  );
  assert!(pack.objects.is_empty());
  assert_eq!(
    (outcome.counters.one_round, outcome.counters.walked),
    (1, 0)
  );
  let carried: BTreeSet<ObjectId> = (1..=2)
    .flat_map(|version| graph.brought(MAIN, version))
    .map(|node| graph.id(node))
    .collect();
  assert_eq!(carried, git_pack(FETCH_HAVES_RESPONSE, &served));

  let clone = Request {
    haves: &[],
    done: true,
    ..request
  };
  let outcome = negotiate(&graph, &clone, &mut Admission::new(u64::MAX)).unwrap();
  let Answer::Pack(pack) = &outcome.answer else {
    panic!("{:?}", outcome.answer)
  };
  assert!(pack.runs.is_empty() && outcome.counters.walked > 0);
  let sent: BTreeSet<ObjectId> = pack.objects.iter().map(|node| graph.id(*node)).collect();
  assert_eq!(sent, git_pack(FETCH_CLONE_RESPONSE, &served));
  assert_eq!(graph.fault(), None);
}

/// A-640 part 4 (git's `filter_sparse` over the real store's named entries): do clone `third` as stock git's clone asks
/// it, under a `sparse:oid` filter whose patterns match every path (`*`, which matches every name, the root's empty one
/// too), then under one holding no pattern. Expect: the first sends exactly what stock git's clone pack holds; the
/// second the same less every blob, every tree still sent, as git sends every tree under a sparse filter.
#[test]
fn a_sparse_clone_sends_every_tree_and_the_blobs_its_patterns_match() {
  let served = served::served();
  let graph = StoreGraph::new(&served.repository, &served.store, &REFS);
  let wants = [id(THIRD)];
  let filter = Filter::SparseOid(b"main:.sparse".to_vec());
  let clone = |patterns: &[u8]| -> BTreeSet<ObjectId> {
    let patterns = Patterns::read(patterns, &mut Admission::new(u64::MAX)).unwrap();
    let request = Request {
      wants: &wants,
      haves: &[],
      done: true,
      wait_for_done: false,
      shallow: &[],
      deepen: None,
      filter: Some(&filter),
      sparse: Some(&patterns),
      include_tag: false,
      thin: true,
    };
    let outcome = negotiate(&graph, &request, &mut Admission::new(u64::MAX)).unwrap();
    let Answer::Pack(pack) = &outcome.answer else {
      panic!("{:?}", outcome.answer)
    };
    pack.objects.iter().map(|node| graph.id(*node)).collect()
  };
  let git = git_pack(FETCH_CLONE_RESPONSE, &served);
  assert_eq!(clone(b"*\n"), git);
  let blobs: BTreeSet<ObjectId> = git
    .iter()
    .filter(|object| {
      graph
        .node(object)
        .is_some_and(|node| graph.kind(node) == Kind::Blob)
    })
    .copied()
    .collect();
  assert!(!blobs.is_empty());
  assert_eq!(clone(b""), git.difference(&blobs).copied().collect());
  assert_eq!(graph.fault(), None);
}

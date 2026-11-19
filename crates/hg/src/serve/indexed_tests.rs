//! Tests of the served view over an index (A-680 part 1). The golden: the two repositories stock 7.2 kept one history's
//! state in (`fixtures/state`), each imported from its store and viewed at its imported phase roots and bookmarks, held
//! to what stock 7.2's `served` filter serves of them (`fixtures/state/make-served-fixtures.sh`): the changesets, why
//! each other one is left out, and the answers `heads`, `branchmap` and `listkeys` give. A clone over the view, every
//! text lent once and hashing to its node; texts longer than a window copied within the room. The model: 7.2's filter
//! stated literally (whole ancestor sets for the phases, `_revealancestors` with its stack) on generated histories with
//! phase roots of every phase, markers, prunes and bookmarks. And the refusals.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use slates_vcs::{ContentId, ContentStore};

use super::super::getbundle::{Store, changegroup};
use super::super::tests::{node_for, room};
use super::super::{Refusal, Repository, Rev, discovery, pull};
use super::{Census, Indexed, LeftOut, Lends, State};
use crate::changegroup::{Changegroup, Version};
use crate::filerev::MARKER;
use crate::fixtures::state;
use crate::import::{FileRefused, Imported, RepositoryFiles, import};
use crate::index::{HgIndex, Log, NewChangeset, NewRevision};
use crate::lines::lines;
use crate::obsmarkers::{self, Date, Format, Marker, Metadata};
use crate::phases::{Phase, PhaseHead};
use crate::{Admission, NULL_NODE, Node, PastAdmission, delta, node};
use proptest::prelude::*;

/// The window the tests' stores are made with: room for a few rows a table, and longer than every fixture's text.
const WINDOW: usize = 1 << 12;

/// A store with room for every history the tests make, many times over.
fn store() -> ContentStore {
  ContentStore::new(1 << 24, WINDOW).unwrap()
}

/// A node's lower-case hexadecimal.
fn hex(node: &[u8]) -> String {
  node.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A repository's `.hg` in memory: each file's names under `.hg` joined by `/`, and its bytes.
struct Dot(BTreeMap<Vec<u8>, Vec<u8>>);

/// The host refusal an in-memory `.hg` never makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Never {}

impl RepositoryFiles for Dot {
  type Refused = Never;

  fn len(&mut self, path: &[&[u8]]) -> Result<Option<u64>, FileRefused<Never>> {
    let len = |bytes: &Vec<u8>| u64::try_from(bytes.len()).unwrap();
    Ok(self.0.get(&path.join(&b'/')).map(len))
  }

  fn read(
    &mut self,
    path: &[&[u8]],
    offset: u64,
    len: u64,
    room: &mut Admission,
  ) -> Result<Option<Vec<u8>>, FileRefused<Never>> {
    let Some(bytes) = self.0.get(&path.join(&b'/')) else {
      return Ok(None);
    };
    let at = |value: u64| usize::try_from(value).unwrap().min(bytes.len());
    let range = &bytes[at(offset)..at(offset.saturating_add(len))];
    room
      .charge(u64::try_from(range.len()).unwrap())
      .map_err(FileRefused::PastAdmission)?;
    Ok(Some(range.to_vec()))
  }
}

/// One of `fixtures/state`'s repositories as 7.2 kept it: its `.hg/requires`, its store, and its `.hg/bookmarks` where
/// it keeps them there.
fn dot(name: &str) -> Dot {
  let mut files = BTreeMap::new();
  let requires = state::file(&format!("{name}-hg-requires.txt"));
  files.insert(b"requires".to_vec(), requires.to_vec());
  if let Some(bookmarks) = state::find(&format!("{name}-bookmarks")) {
    files.insert(b"bookmarks".to_vec(), bookmarks.to_vec());
  }
  for line in lines(state::file(&format!("{name}-files.txt"))) {
    let file = core::str::from_utf8(line).unwrap();
    let bytes = state::file(&format!("{name}/{file}"));
    files.insert(format!("store/{file}").into_bytes(), bytes.to_vec());
  }
  Dot(files)
}

/// One of `fixtures/state`'s repositories imported into a new index, and the store holding its contents.
fn imported(name: &str) -> (Imported, ContentStore) {
  let mut store = store();
  let imported = import(&mut dot(name), &mut store, Admission::new(1 << 24)).unwrap();
  (imported, store)
}

/// The register's state an import answers: its phase roots and bookmarks, publishing off as 7.2 kept it.
fn state_of(imported: &Imported) -> State<'_> {
  State {
    roots: &imported.phase_roots,
    bookmarks: &imported.bookmarks,
    publishing: false,
  }
}

/// The lines of a fixture of `fixtures/state`.
fn listed(name: &str) -> Vec<String> {
  lines(state::file(name))
    .map(|line| String::from_utf8(line.to_vec()).unwrap())
    .collect()
}

/// An answer written by `answer` with all the room it needs.
fn answered(answer: impl FnOnce(&mut Admission, &mut Vec<u8>) -> Result<(), Refusal>) -> String {
  let mut out = Vec::new();
  answer(&mut room(), &mut out).unwrap();
  String::from_utf8(out).unwrap()
}

/// `listkeys`' lines for a fixture's `{key} {value}` lines: each key, a tab and its value.
fn listkeys_of(name: &str) -> String {
  let entries: Vec<String> = listed(name)
    .iter()
    .map(|line| line.replacen(' ', "\t", 1))
    .collect();
  entries.join("\n")
}

/// Holds the view of `name` to 7.2's `served` changesets: each in order with its phase, and its parents the view's
/// numbers of the changelog's parents.
fn check_served(name: &str, view: &Indexed<'_>, imported: &Imported, store: &ContentStore) {
  let served: Vec<String> = (0..view.count())
    .map(|rev| {
      let node = view.node(rev).unwrap();
      format!("{} {}", hex(&node), view.phase(rev).name())
    })
    .collect();
  assert_eq!(served, listed(&format!("{name}-served.txt")), "{name}");
  for rev in 0..view.count() {
    let row = imported
      .index
      .changeset(
        store,
        imported
          .index
          .rev(store, Log::Changelog, &view.node(rev).unwrap())
          .unwrap(),
      )
      .unwrap();
    let parent_node =
      |parent: Option<Rev>| parent.map_or(NULL_NODE, |parent| view.node(parent).unwrap());
    let changelog_node = |parent: u32| {
      imported
        .index
        .changeset(store, parent)
        .map_or(NULL_NODE, |row| row.node)
    };
    let [p1, p2] = view.parents(rev);
    assert_eq!(
      [parent_node(p1), parent_node(p2)],
      [changelog_node(row.p1), changelog_node(row.p2)],
      "{name} {rev}"
    );
    assert!(p1.is_none_or(|p1| p1 < rev) && p2.is_none_or(|p2| p2 < rev));
  }
}

/// Holds what the view of `name` leaves out to 7.2's listing: every changeset the changelog holds and the view does
/// not serve, in order, with why.
fn check_left_out(name: &str, view: &Indexed<'_>, imported: &Imported, store: &ContentStore) {
  let changelog = imported.index.len(store, Log::Changelog);
  let mut left = Vec::new();
  for rev in 0..changelog {
    let node = imported.index.changeset(store, rev).unwrap().node;
    match (view.rev(&node), view.left_out(&node)) {
      (Some(_), None) => {}
      (None, Some(LeftOut::Hidden)) => left.push(format!("{} hidden", hex(&node))),
      (None, Some(LeftOut::Secret)) => left.push(format!("{} secret", hex(&node))),
      unexpected => panic!("{name} {rev}: {unexpected:?}"),
    }
  }
  assert_eq!(left, listed(&format!("{name}-unserved.txt")), "{name}");
}

/// Holds the answers over the view of `name` to 7.2's, byte for byte: `heads`, `branchmap`, and `listkeys` of
/// `bookmarks` and `phases`.
fn check_answers(name: &str, view: &Indexed<'_>) {
  let heads = answered(|room, out| discovery::heads(view, room, out));
  let expected = format!(
    "{}\n",
    listed(&format!("{name}-served-heads.txt")).join(" ")
  );
  assert_eq!(heads, expected, "{name} heads");
  let branchmap = answered(|room, out| discovery::branchmap(view, room, out));
  let expected = listed(&format!("{name}-served-branchmap.txt")).join("\n");
  assert_eq!(branchmap, expected, "{name} branchmap");
  for namespace in ["bookmarks", "phases"] {
    let keys = answered(|room, out| pull::listkeys(view, namespace.as_bytes(), room, out));
    let expected = listkeys_of(&format!("{name}-served-{namespace}.txt"));
    assert_eq!(keys, expected, "{name} {namespace}");
  }
}

/// A-680 part 1 (golden): do serve the two repositories stock 7.2 kept one history's state in (`fixtures/state`: a
/// public root, a draft line, a draft amended so a marker hides it, a secret changeset, a bookmark on each of three
/// changesets, the secret one's among them, and a prune of a node never held), each imported from its store and viewed
/// at its imported phase roots and bookmarks. Expect what stock 7.2's `served` filter serves of each
/// (`make-served-fixtures.sh`): its three changesets in order with their phases and parents, the amended one left out
/// as hidden and the secret one as secret, and `heads`, `branchmap` and `listkeys` of `bookmarks` (the secret
/// changeset's bookmark not among them) and `phases`, each byte for byte; one hidden and one secret counted, nothing
/// pinned, revealed or dropped.
#[test]
fn the_states_repositories_are_served_as_7_2_serves_them() {
  for name in ["states", "states-store"] {
    let (imported, store) = imported(name);
    let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room()).unwrap();
    check_served(name, &view, &imported, &store);
    check_left_out(name, &view, &imported, &store);
    check_answers(name, &view);
    let census = Census {
      hidden: 1,
      secret: 1,
      ..Census::default()
    };
    assert_eq!(view.census(), census, "{name}");
  }
}

/// The full texts of a group of a decoded changegroup, each held to its node.
fn texts_of(
  changegroup: &Changegroup<'_>,
  revisions: &core::ops::Range<usize>,
) -> Vec<(Node, Vec<u8>)> {
  changegroup
    .group(revisions)
    .iter()
    .map(|revision| {
      let text = delta::apply(&[], revision.data, &mut room()).unwrap();
      assert_eq!(
        node::compute(&revision.p1, &revision.p2, &text),
        Ok(revision.node)
      );
      (revision.node, text)
    })
    .collect()
}

/// A-680 part 1: do pack the changesets a clone of `states` asks for over its served view, its store's window longer
/// than every text. Expect exactly the three served changesets, their three manifests and the file revisions they name
/// (`a`, `b`, and `c` as amended: neither the hidden changeset's `c` nor the secret one's `d`), every text hashing to
/// its node; the two markers lent when the view was made and each text since lent once, all in place.
#[test]
fn a_clone_over_the_view_carries_its_changesets_each_text_lent_once_in_place() {
  let (imported, store) = imported("states");
  let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room()).unwrap();
  assert_eq!(
    view.lends(),
    Lends {
      in_place: 2,
      copied: 0
    }
  );
  let outgoing = pull::outgoing(&view, b"", b"", &mut room()).unwrap();
  assert_eq!(outgoing.missing, [0, 1, 2]);
  let packed = changegroup(&view, &outgoing, Version::V03, &mut room()).unwrap();
  let packed = Changegroup::decode(&packed, Version::V03, &mut room()).unwrap();
  let changesets: Vec<Node> = texts_of(&packed, &packed.changelog)
    .into_iter()
    .map(|(node, _)| node)
    .collect();
  let served: Vec<Node> = (0..view.count())
    .map(|rev| view.node(rev).unwrap())
    .collect();
  assert_eq!(changesets, served);
  assert_eq!(texts_of(&packed, &packed.manifests).len(), 3);
  let files: Vec<(&[u8], Vec<Vec<u8>>)> = packed
    .files
    .iter()
    .map(|group| {
      let texts = texts_of(&packed, &group.revisions);
      (
        group.name,
        texts.into_iter().map(|(_, text)| text).collect(),
      )
    })
    .collect();
  let expected: [(&[u8], Vec<Vec<u8>>); 3] = [
    (b"a", vec![b"a\n".to_vec()]),
    (b"b", vec![b"b\n".to_vec()]),
    (b"c", vec![b"c amended\n".to_vec()]),
  ];
  assert_eq!(files, expected);
  assert_eq!(
    view.lends(),
    Lends {
      in_place: 11,
      copied: 0
    }
  );
}

/// The bytes of a text `len` long, every byte its offset's low bits, so no two windows of it are alike.
fn long_text(len: usize, salt: u8) -> Vec<u8> {
  (0..len)
    .map(|at| u8::try_from(at % 251).unwrap() ^ salt)
    .collect()
}

/// A-680 part 1: do lend, from a store whose window is 4,096 bytes, a changeset text of 10,000 bytes, a manifest text
/// of 8,200, a file revision with a metadata block (its bytes 5,000 long) and a short one without, first the
/// changeset's with a room a byte short. Expect the short room refused before anything is copied; then the long texts
/// and the framed one copied exactly (the block between its markers, then the bytes) and the short one lent in place,
/// each with its position, parents and link, and nothing for a node no log holds.
#[test]
fn texts_longer_than_a_window_are_copied_within_the_room() {
  let mut store = store();
  let mut index = HgIndex::new();
  let changeset = long_text(10_000, 1);
  let manifest = long_text(8_200, 2);
  let (block, bytes) = (b"copy: a\ncopyrev: 0\n".to_vec(), long_text(5_000, 3));
  let framed = [MARKER, &block, MARKER, &bytes].concat();
  let len = |text: &[u8]| u64::try_from(text.len()).unwrap();
  let (node, manifest_node, f_node, g_node) = ([1; 20], [2; 20], [3; 20], [4; 20]);
  let text = store.put_object(&changeset).unwrap();
  let new = NewChangeset {
    node,
    p1: NULL_NODE,
    p2: NULL_NODE,
    flags: 0,
    len: len(&changeset),
    text,
    manifest: manifest_node,
    closed: false,
    branch: b"default",
  };
  index.append_changesets(&mut store, &[new]).unwrap();
  let revision = |store: &mut ContentStore, node: Node, bytes: &[u8], block: Option<&[u8]>| {
    let total = block.map_or(len(bytes), |block| {
      len(block) + len(bytes) + 2 * len(MARKER)
    });
    NewRevision {
      node,
      p1: NULL_NODE,
      p2: NULL_NODE,
      link: [1; 20],
      flags: 0,
      len: total,
      bytes: store.put_object(bytes).unwrap(),
      block: block.map(|block| store.put(block).unwrap()),
    }
  };
  let logs = [
    (
      Log::Manifest,
      revision(&mut store, manifest_node, &manifest[..], None),
    ),
    (
      Log::File(b"f"),
      revision(&mut store, f_node, &bytes[..], Some(&block[..])),
    ),
    (
      Log::File(b"g"),
      revision(&mut store, g_node, &b"g\n"[..], None),
    ),
  ];
  for (log, revision) in logs {
    index
      .append_revisions(&mut store, log, &[revision])
      .unwrap();
  }
  let no_state = State {
    roots: &[],
    bookmarks: &[],
    publishing: true,
  };
  let view = Indexed::new(&index, &store, no_state, &mut room()).unwrap();
  let short = view.changeset_text(0, &mut Admission::new(9_999));
  assert_eq!(
    short,
    Err(PastAdmission {
      asked: 10_000,
      left: 9_999
    })
  );
  assert_eq!(view.lends(), Lends::default());
  let lent = view.changeset_text(0, &mut room()).unwrap().unwrap();
  assert!(matches!(&lent, Cow::Owned(text) if *text == changeset));
  let logged = view.manifest(&manifest_node, &mut room()).unwrap().unwrap();
  assert!(matches!(&logged.text, Cow::Owned(text) if *text == manifest));
  assert_eq!(
    (logged.position, logged.parents, logged.link),
    (0, [NULL_NODE; 2], node)
  );
  let logged = view.file(b"f", &f_node, &mut room()).unwrap().unwrap();
  assert!(matches!(&logged.text, Cow::Owned(text) if *text == framed));
  let logged = view.file(b"g", &g_node, &mut room()).unwrap().unwrap();
  assert!(matches!(logged.text, Cow::Borrowed(b"g\n")));
  assert_eq!(
    view.lends(),
    Lends {
      in_place: 1,
      copied: 3
    }
  );
  assert_eq!(view.file(b"g", &f_node, &mut room()), Ok(None));
  assert_eq!(view.manifest(&[9; 20], &mut room()), Ok(None));
}

/// What a generated changeset is: its parents (earlier revisions), the phase root at it, the marker naming it as a
/// predecessor (a prune, or one with a successor), and whether a bookmark names it.
#[derive(Clone, Copy, Debug)]
struct Made {
  parents: [Option<usize>; 2],
  root: Option<Phase>,
  marker: Option<Obsoleted>,
  bookmark: bool,
}

/// How a generated marker makes its changeset obsolete.
#[derive(Clone, Copy, Debug)]
enum Obsoleted {
  /// A prune: no successor.
  Pruned,
  /// Rewritten as the changeset at this revision.
  By(usize),
}

/// A generated repository: its index and the store holding its contents, its nodes by revision, and the register's
/// state: phase roots and bookmarks.
struct Built {
  index: HgIndex,
  store: ContentStore,
  nodes: Vec<Node>,
  roots: Vec<PhaseHead>,
  bookmarks: Vec<(Vec<u8>, Node)>,
}

/// The payload of one format 1 marker of `predecessor`, with `successors` (whole nodes, or none for a prune).
fn marker_of(predecessor: &Node, successors: &[u8]) -> Vec<u8> {
  let marker = Marker {
    flags: 0,
    predecessor,
    successors,
    parents: None,
    date: Some(Date {
      seconds_bits: 0,
      offset_minutes: 0,
    }),
    metadata: Metadata::Sized {
      sizes: &[],
      bytes: &[],
    },
  };
  let mut payload = Vec::new();
  obsmarkers::encode(Format::V1, &[marker], &mut payload).unwrap();
  payload
}

/// A node no changeset of a generated history has.
const STRANGER: Node = [0x11; 20];

/// The repository of `made`, its markers in revision order; with `strangers`, also a phase root, a prune and a
/// bookmark of a node it never held.
fn build(made: &[Made], strangers: bool) -> Built {
  let mut store = store();
  let mut index = HgIndex::new();
  let nodes: Vec<Node> = (0..made.len())
    .map(|rev| node_for(u64::try_from(rev).unwrap() + 7))
    .collect();
  let texts: Vec<Vec<u8>> = (0..made.len())
    .map(|rev| format!("changeset {rev}\n").into_bytes())
    .collect();
  let parent = |parent: Option<usize>| parent.map_or(NULL_NODE, |parent| nodes[parent]);
  let group: Vec<NewChangeset<'_>> = made
    .iter()
    .zip(&texts)
    .enumerate()
    .map(|(rev, (made, text))| NewChangeset {
      node: nodes[rev],
      p1: parent(made.parents[0]),
      p2: parent(made.parents[1]),
      flags: 0,
      len: u64::try_from(text.len()).unwrap(),
      text: store.put(text).unwrap(),
      manifest: NULL_NODE,
      closed: false,
      branch: b"default",
    })
    .collect();
  index.append_changesets(&mut store, &group).unwrap();
  let mut markers: Vec<Vec<u8>> = made
    .iter()
    .enumerate()
    .filter_map(|(rev, made)| match made.marker? {
      Obsoleted::Pruned => Some(marker_of(&nodes[rev], &[])),
      Obsoleted::By(successor) => Some(marker_of(&nodes[rev], &nodes[successor])),
    })
    .collect();
  let mut roots: Vec<PhaseHead> = made
    .iter()
    .enumerate()
    .filter_map(|(rev, made)| {
      Some(PhaseHead {
        phase: made.root?,
        node: nodes[rev],
      })
    })
    .collect();
  let mut bookmarks: Vec<(Vec<u8>, Node)> = made
    .iter()
    .enumerate()
    .filter(|(_, made)| made.bookmark)
    .map(|(rev, _)| (format!("mark{rev}").into_bytes(), nodes[rev]))
    .collect();
  if strangers {
    markers.push(marker_of(&STRANGER, &[]));
    roots.push(PhaseHead {
      phase: Phase::Secret,
      node: STRANGER,
    });
    bookmarks.push((b"stranger".to_vec(), STRANGER));
  }
  let ids: Vec<ContentId> = markers
    .iter()
    .map(|marker| store.put(marker).unwrap())
    .collect();
  index.append_markers(&mut store, &ids).unwrap();
  Built {
    index,
    store,
    nodes,
    roots,
    bookmarks,
  }
}

/// What 7.2's filter makes of a changeset: served in a phase, or left out and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fate {
  Served(Phase),
  Left(LeftOut),
}

/// Every revision `rev` descends from, itself included, walked whole.
fn ancestors(made: &[Made], rev: usize) -> BTreeSet<usize> {
  let mut seen = BTreeSet::new();
  let mut stack = vec![rev];
  while let Some(next) = stack.pop() {
    if seen.insert(next) {
      stack.extend(made[next].parents.iter().flatten());
    }
  }
  seen
}

/// 7.2's `served` filter over a generated history, stated literally (`repoview.computeunserved`): each changeset's
/// phase the highest of the roots it descends from (`phases.py`); obsolete, a draft or secret one some marker names
/// (`_computeobsoleteset`); hideable, obsolete or archived or internal (`hideablerevs`); hidden, hideable and not a
/// bookmark's, then every hidden ancestor of a visible changeset that is not public revealed through hidden ones
/// with `_revealancestors`' stack; secret, of phase secret or above. Each changeset's fate, and the census.
fn model(made: &[Made]) -> (Vec<Fate>, Census) {
  let all = 0..made.len();
  let phase: Vec<Phase> = all
    .clone()
    .map(|rev| {
      let mine = ancestors(made, rev);
      made
        .iter()
        .enumerate()
        .filter(|(root, made)| made.root.is_some() && mine.contains(root))
        .filter_map(|(_, made)| made.root)
        .max()
        .unwrap_or(Phase::Public)
    })
    .collect();
  let obsolete =
    |rev: usize| made[rev].marker.is_some() && matches!(phase[rev], Phase::Draft | Phase::Secret);
  let hideable: BTreeSet<usize> = all
    .clone()
    .filter(|rev| obsolete(*rev) || matches!(phase[*rev], Phase::Archived | Phase::Internal))
    .collect();
  let pinned: BTreeSet<usize> = all.clone().filter(|rev| made[*rev].bookmark).collect();
  let mut hidden: BTreeSet<usize> = hideable.difference(&pinned).copied().collect();
  let before = hidden.clone();
  let mut stack: Vec<usize> = all
    .clone()
    .filter(|rev| phase[*rev] != Phase::Public && !hidden.contains(rev))
    .collect();
  while let Some(rev) = stack.pop() {
    for parent in made[rev].parents.iter().flatten() {
      if hidden.remove(parent) {
        stack.push(*parent);
      }
    }
  }
  let secret = |rev: usize| phase[rev] > Phase::Draft;
  let fates = all
    .clone()
    .map(|rev| match (hidden.contains(&rev), secret(rev)) {
      (true, _) => Fate::Left(LeftOut::Hidden),
      (false, true) => Fate::Left(LeftOut::Secret),
      (false, false) => Fate::Served(phase[rev]),
    })
    .collect();
  let count = |set: usize| u32::try_from(set).unwrap();
  let census = Census {
    hidden: count(hidden.len()),
    secret: count(
      all
        .clone()
        .filter(|rev| !hidden.contains(rev) && secret(*rev))
        .count(),
    ),
    pinned: count(hideable.intersection(&pinned).count()),
    revealed: count(before.difference(&hidden).count()),
    dropped_roots: 0,
  };
  (fates, census)
}

/// The view's fate of each changeset of `built`, by changelog revision, each served one checked to be numbered
/// densely: the count of those served before it.
fn fates(view: &Indexed<'_>, built: &Built) -> Vec<Fate> {
  let mut served: Rev = 0;
  built
    .nodes
    .iter()
    .map(|node| match view.rev(node) {
      Some(rev) => {
        assert_eq!(rev, served);
        served += 1;
        Fate::Served(view.phase(rev))
      }
      None => Fate::Left(view.left_out(node).unwrap()),
    })
    .collect()
}

/// The view of `built` at its state, publishing as asked.
fn view_of(built: &Built, publishing: bool) -> Indexed<'_> {
  let state = State {
    roots: &built.roots,
    bookmarks: &built.bookmarks,
    publishing,
  };
  Indexed::new(&built.index, &built.store, state, &mut room()).unwrap()
}

/// A history from per-revision choices: a first parent among the three before it (or none), a second now and then, a
/// phase root of any phase now and then, a marker (a prune, or a rewrite as a later or earlier changeset), a bookmark.
fn made_from(choices: &[Choice]) -> Vec<Made> {
  let count = choices.len();
  choices
    .iter()
    .enumerate()
    .map(|(rev, (first, second, root, marker, bookmark))| {
      let p1 = (rev > 0 && first % 6 != 0).then(|| rev - 1 - usize::from(*first) % rev.min(3));
      let p2 = (p1.is_some() && rev > 1 && second % 3 == 0)
        .then(|| usize::from(*second) % rev)
        .filter(|p2| Some(*p2) != p1);
      let root = match root % 10 {
        0 | 1 => Some(Phase::Draft),
        2 => Some(Phase::Secret),
        3 => Some(Phase::Archived),
        4 => Some(Phase::Internal),
        _ => None,
      };
      let marker = match marker % 8 {
        0 => Some(Obsoleted::Pruned),
        1 => Some(Obsoleted::By(usize::from(marker / 8) % count)),
        _ => None,
      };
      Made {
        parents: [p1, p2],
        root,
        marker,
        bookmark: bookmark % 5 == 0,
      }
    })
    .collect()
}

/// A generated changeset's choices: its first and second parent, its phase root, its marker and its bookmark
/// ([`made_from`]).
type Choice = (u8, u8, u8, u8, u8);

/// The choices of generated histories of one to nineteen changesets.
fn choices() -> impl Strategy<Value = Vec<Choice>> {
  proptest::collection::vec(any::<Choice>(), 1..20)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-680 part 1 (model): do view generated histories (merges, phase roots of every phase, prunes and rewrites,
  /// bookmarks, and now and then a root, a prune and a bookmark of a node the history never held). Expect each
  /// changeset's fate as 7.2's filter stated literally gives it, served ones numbered densely in order with their
  /// parents renumbered, the census equal (the stranger's root dropped), and the publishing flag passed through.
  #[test]
  fn the_view_serves_what_7_2s_served_filter_serves(
    choices in choices(),
    strangers in any::<bool>(),
    publishing in any::<bool>(),
  ) {
    let made = made_from(&choices);
    let built = build(&made, strangers);
    let view = view_of(&built, publishing);
    let (expected, mut census) = model(&made);
    census.dropped_roots = u32::from(strangers);
    prop_assert_eq!(fates(&view, &built), expected);
    prop_assert_eq!(view.census(), census);
    prop_assert_eq!(view.publishing(), publishing);
    let dense: BTreeMap<Node, Rev> =
      (0..view.count()).filter_map(|rev| Some((view.node(rev)?, rev))).collect();
    for (made, node) in made.iter().zip(&built.nodes) {
      let Some(served) = view.rev(node) else { continue };
      let renumbered = made.parents.map(|parent| dense.get(&built.nodes[parent?]).copied());
      prop_assert_eq!(view.parents(served), renumbered);
    }
  }
}

/// A history of three: a public root, a draft on it, and a changeset on that draft, each as asked.
fn three(second: Made, third: Made) -> Vec<Made> {
  let root = Made {
    parents: [None, None],
    root: None,
    marker: None,
    bookmark: false,
  };
  vec![root, second, third]
}

/// A changeset whose first parent is `parent`, with the root, marker and bookmark asked.
fn on(parent: usize, root: Option<Phase>, marker: Option<Obsoleted>, bookmark: bool) -> Made {
  Made {
    parents: [Some(parent), None],
    root,
    marker,
    bookmark,
  }
}

/// A-680 part 1 (7.2's `pinnedrevs` and `_revealancestors`): do view an obsolete draft under a secret changeset that
/// is not obsolete; then under one that is obsolete too; then the second history with a bookmark on its secret one;
/// then an archived changeset over a pruned draft, alone and bookmarked. Expect the first draft served and the secret
/// one left out as secret (revealed: one); both hidden in the second; in the third the bookmark pinning the secret one
/// (still never served) and so revealing the draft; in the fourth both hidden, and the bookmarked archived one pinned
/// and revealing the draft below it.
#[test]
fn a_visible_descendant_or_a_bookmark_keeps_an_obsolete_changeset_and_a_secret_one_never_serves() {
  let draft = |marker| on(0, Some(Phase::Draft), marker, false);
  let cases = [
    (
      three(
        draft(Some(Obsoleted::Pruned)),
        on(1, Some(Phase::Secret), None, false),
      ),
      (Fate::Served(Phase::Draft), LeftOut::Secret),
      (0, 1, 0, 1),
    ),
    (
      three(
        draft(Some(Obsoleted::Pruned)),
        on(1, Some(Phase::Secret), Some(Obsoleted::Pruned), false),
      ),
      (Fate::Left(LeftOut::Hidden), LeftOut::Hidden),
      (2, 0, 0, 0),
    ),
    (
      three(
        draft(Some(Obsoleted::Pruned)),
        on(1, Some(Phase::Secret), Some(Obsoleted::Pruned), true),
      ),
      (Fate::Served(Phase::Draft), LeftOut::Secret),
      (0, 1, 1, 1),
    ),
    (
      three(
        draft(Some(Obsoleted::Pruned)),
        on(1, Some(Phase::Archived), None, false),
      ),
      (Fate::Left(LeftOut::Hidden), LeftOut::Hidden),
      (2, 0, 0, 0),
    ),
    (
      three(
        draft(Some(Obsoleted::Pruned)),
        on(1, Some(Phase::Archived), None, true),
      ),
      (Fate::Served(Phase::Draft), LeftOut::Secret),
      (0, 1, 1, 1),
    ),
  ];
  for (case, (made, (draft_fate, top), (hidden, secret, pinned, revealed))) in
    cases.into_iter().enumerate()
  {
    let built = build(&made, false);
    let view = view_of(&built, false);
    let expected = vec![Fate::Served(Phase::Public), draft_fate, Fate::Left(top)];
    assert_eq!(fates(&view, &built), expected, "case {case}");
    let census = Census {
      hidden,
      secret,
      pinned,
      revealed,
      dropped_roots: 0,
    };
    assert_eq!(view.census(), census, "case {case}");
    assert_eq!(model(&made), (expected, census), "case {case}");
  }
}

/// A-680 part 1 (7.2's `_readroots`): do view `states` with one more phase root, secret, of a node its changelog never
/// held. Expect it dropped and counted, and the same changesets served.
#[test]
fn a_root_the_changelog_lacks_is_dropped_and_counted() {
  let (imported, store) = imported("states");
  let mut roots = imported.phase_roots.clone();
  roots.push(PhaseHead {
    phase: Phase::Secret,
    node: STRANGER,
  });
  let state = State {
    roots: &roots,
    ..state_of(&imported)
  };
  let view = Indexed::new(&imported.index, &store, state, &mut room()).unwrap();
  check_served("states", &view, &imported, &store);
  assert_eq!(view.census().dropped_roots, 1);
}

/// A-680 part 1: do view a repository with every changeset public (no root), then one with none at all. Expect every
/// changeset served as the changelog numbers it, nothing walked or left out; and an empty view whose `heads` is the
/// null node, as 7.2 answers for an empty repository.
#[test]
fn a_public_history_is_served_whole_and_an_empty_one_answers_the_null_node() {
  let made: Vec<Made> = (0..5)
    .map(|rev| Made {
      parents: [rev.checked_sub(1), None],
      root: None,
      marker: (rev == 2).then_some(Obsoleted::Pruned),
      bookmark: false,
    })
    .collect();
  let built = build(&made, false);
  let view = view_of(&built, true);
  assert_eq!(fates(&view, &built), vec![Fate::Served(Phase::Public); 5]);
  assert_eq!(view.census(), Census::default());
  let built = build(&[], false);
  let view = view_of(&built, true);
  assert_eq!(view.count(), 0);
  let heads = answered(|room, out| discovery::heads(&view, room, out));
  assert_eq!(heads, format!("{}\n", "0".repeat(40)));
}

/// AC-HG.9, A-680 part 1: do view a repository whose index holds a marker the store's bytes cannot read (an unknown
/// format's version byte). Expect the view refused naming the marker, as 7.2 aborts on an obsstore it cannot read.
#[test]
fn a_marker_the_store_cannot_read_refuses_the_view() {
  let made = three(
    on(0, Some(Phase::Draft), None, false),
    on(1, None, None, false),
  );
  let mut built = build(&made, false);
  let garbage = built.store.put(b"\x07garbage").unwrap();
  built
    .index
    .append_markers(&mut built.store, &[garbage])
    .unwrap();
  let state = State {
    roots: &built.roots,
    bookmarks: &built.bookmarks,
    publishing: false,
  };
  let refused = Indexed::new(&built.index, &built.store, state, &mut room()).err();
  assert_eq!(
    refused,
    Some(Refusal::Store {
      what: "an obsolescence marker"
    })
  );
}

/// AC-HG.9, A-680 part 1: do make the view of `states` with every room from nothing up to the least that holds it.
/// Expect each smaller room refused past its admission and the least one answering, never a panic.
#[test]
fn a_view_past_its_room_is_refused() {
  let (imported, store) = imported("states");
  let make = |room: u64| {
    Indexed::new(
      &imported.index,
      &store,
      state_of(&imported),
      &mut Admission::new(room),
    )
    .map(|_| ())
  };
  let least = (0..1 << 16).find(|room| make(*room).is_ok()).unwrap();
  assert!(least > 0);
  for room in 0..least {
    assert!(
      matches!(make(room), Err(Refusal::PastAdmission(_))),
      "{room}"
    );
  }
}

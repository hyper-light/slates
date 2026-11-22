//! Tests of the served view over an index (A-680 part 1). The golden: the two repositories stock 7.2 kept one history's
//! state in (`fixtures/state`), each imported from its store and viewed at its imported phase roots and bookmarks, held
//! to what stock 7.2's `served` filter serves of them (`fixtures/state/make-served-fixtures.sh`): the changesets, why
//! each other one is left out, and the answers `heads`, `branchmap` and `listkeys` give; and so `divergent`, a
//! divergent bookmark as stock 7.2 makes one (A-690), and `amended-root`, a draft root hidden by its amend (A-691). A
//! clone over the view, every text lent once and hashing to its node; texts longer than a window copied within the
//! room. The model: 7.2's filter stated literally (whole ancestor sets for the phases, `_revealancestors` with its
//! stack) on generated histories with phase roots of every phase, markers, prunes and bookmarks. And the refusals.

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
      .charge(crate::read::to_u64(range.len()))
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

/// Bookmarks as `{name} {node}` lines, sorted, as the fixtures list them.
fn bookmark_lines<'a>(marks: impl Iterator<Item = (&'a [u8], Node)>) -> Vec<String> {
  let mut lines: Vec<String> = marks
    .map(|(name, node)| format!("{} {}", String::from_utf8_lossy(name), hex(&node)))
    .collect();
  lines.sort();
  lines
}

/// A-690 (golden): do serve `divergent`, which stock 7.2 made as a divergent bookmark is made (`feature` moved in a
/// clone and here, the clone pulled back, so 7.2 keeps the clone's as `feature@1`; and `@` on the root), imported from
/// its store and viewed at its imported phase roots and bookmarks. Expect the view to hold all three bookmarks, and to
/// serve what stock 7.2's `served` filter serves of it (`make-served-fixtures.sh`): its three changesets, none left out,
/// and `heads`, `branchmap` and `listkeys` of `bookmarks` and `phases` byte for byte, `bookmarks` holding `@` and
/// `feature` alone (`listbinbookmarks` leaves out a name `isdivergent` holds, an `@` not as its last byte, and keeps `@`,
/// whose `@` is its last byte); and the bookmarks a `getbundle`'s `bookmarks` part carries the same two.
#[test]
fn a_divergent_bookmark_is_served_as_7_2_serves_it() {
  let name = "divergent";
  let (imported, store) = imported(name);
  let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room()).unwrap();
  assert_eq!(
    bookmark_lines(view.bookmarks()),
    listed("divergent-bookmarks.txt")
  );
  check_served(name, &view, &imported, &store);
  check_left_out(name, &view, &imported, &store);
  check_answers(name, &view);
  let carried = pull::bookmark_entries(&view, &mut room()).unwrap();
  assert_eq!(
    bookmark_lines(
      carried
        .iter()
        .filter_map(|mark| Some((mark.name, mark.node?)))
    ),
    listed("divergent-served-bookmarks.txt")
  );
  assert_eq!(view.census(), Census::default());
}

/// A-691 (golden): do serve `amended-root`, which stock 7.2 made with a draft root amended under evolution (the old
/// changeset hidden by the marker, and both kept among the draft roots 7.2 stores) and another draft made secret
/// afterwards, imported from its store and viewed at its imported phase roots. Expect what stock 7.2's `served` filter
/// serves of it (`make-served-fixtures.sh`): the public root and the amended draft, the old draft left out as hidden
/// and the secret one as secret, and `heads`, `branchmap` and `listkeys` of `bookmarks` and `phases` byte for byte,
/// `phases` listing the hidden draft root beside the amended one, since 7.2's `listphases` reads the stored draft
/// roots from the unfiltered changelog (`phases.py` 1049-1058).
#[test]
fn a_hidden_draft_root_is_listed_as_7_2_lists_it() {
  let name = "amended-root";
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
  assert_eq!(view.census(), census);
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
/// its node; the two markers lent when the view was made, each changeset's text since lent once and each manifest's
/// twice (a step each: planned, then sent): in place, but for the one manifest the index keeps as its delta against the
/// one before (A-729), rebuilt from its chain and so copied, each time.
#[test]
fn a_clone_over_the_view_carries_its_changesets_each_text_lent_in_place() {
  let (imported, store) = imported("states");
  let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room())
    .unwrap()
    .answering_from_its_texts();
  assert_eq!(
    view.lends(),
    Lends {
      in_place: 2,
      copied: 0
    }
  );
  let outgoing = pull::outgoing(&view, b"", b"", &mut room()).unwrap();
  assert_eq!(outgoing.missing.iter().collect::<Vec<_>>(), [0, 1, 2]);
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
  // Each changeset's text lent once and each manifest's twice: once as the changeset naming it is sent, its files
  // planned, and once as it is sent in its log's order, a step each (main 01a20182's order of 2026-11-22 10:40: a
  // lent text is the step's, never kept past it).
  assert_eq!(
    view.lends(),
    Lends {
      in_place: 12,
      copied: 2
    }
  );
}

/// The bytes of a text `len` long, every byte its offset's low bits, so no two windows of it are alike.
fn long_text(len: usize, salt: u8) -> Vec<u8> {
  (0..len)
    .map(|at| u8::try_from(at % 251).unwrap() ^ salt)
    .collect()
}

/// The index and store [`texts_longer_than_a_window_are_copied_within_the_room`] lends from: a changeset text of
/// 10,000 bytes (node 1), a manifest text of 8,200 (node 2), a file revision of `f` with a metadata block (node 3) and a
/// short one of `g` without (node 4); with the changeset's, the manifest's and the framed file's texts.
fn long_texts_indexed() -> (HgIndex, ContentStore, [Vec<u8>; 3]) {
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
  (index, store, [changeset, manifest, framed])
}

/// A-680 part 1: do lend, from a store whose window is 4,096 bytes, a changeset text of 10,000 bytes, a manifest text
/// of 8,200, a file revision with a metadata block (its bytes 5,000 long) and a short one without, first the
/// changeset's with a room a byte short. Expect the short room refused before anything is copied; then the long texts
/// copied exactly, each with its position, parents and link; the file revisions' rows read with no text (A-767 C1: the
/// served view lends no file's whole text, which a fetch reads from the changeset store); and nothing for a node no
/// log holds.
#[test]
fn texts_longer_than_a_window_are_copied_within_the_room() {
  let (node, manifest_node, f_node, g_node) = ([1; 20], [2; 20], [3; 20], [4; 20]);
  let (index, store, [changeset, manifest, _]) = long_texts_indexed();
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
  long_texts_are_copied(&view, [node, manifest_node], [changeset, manifest]);
  file_rows_are_read(&view, [f_node, g_node]);
  assert_eq!(
    view.lends(),
    Lends {
      in_place: 0,
      copied: 2
    }
  );
  assert_eq!(view.file_entry(b"g", &f_node), None);
  assert_eq!(view.manifest(&[9; 20], &mut room()), Ok(None));
}

/// The changeset's and the manifest's long texts of [`long_texts_indexed`] (`nodes` their nodes, `texts` their texts)
/// lent from `view` copied exactly, the manifest's with its position, parents and link.
fn long_texts_are_copied(view: &Indexed<'_>, nodes: [Node; 2], texts: [Vec<u8>; 2]) {
  let [node, manifest_node] = nodes;
  let [changeset, manifest] = texts;
  let lent = view.changeset_text(0, &mut room()).unwrap().unwrap();
  assert!(matches!(&lent, Cow::Owned(text) if *text == changeset));
  let logged = view.manifest(&manifest_node, &mut room()).unwrap().unwrap();
  assert!(matches!(&logged.text, Cow::Owned(text) if *text == manifest));
  assert_eq!(
    (logged.position, logged.parents, logged.link),
    (0, [NULL_NODE; 2], node)
  );
}

/// The file revisions of [`long_texts_indexed`] (`nodes` `f`'s and `g`'s) as `view`'s rows hold them, no text lent.
fn file_rows_are_read(view: &Indexed<'_>, [f_node, g_node]: [Node; 2]) {
  for (path, node) in [(&b"f"[..], f_node), (b"g", g_node)] {
    let entry = view.file_entry(path, &node).unwrap();
    assert_eq!(entry.parents, [NULL_NODE; 2], "{path:?}");
  }
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
  let phase = phases(made);
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

/// Each changeset's phase as 7.2 computes it (`phases.py`): the highest of the roots it descends from, itself included.
fn phases(made: &[Made]) -> Vec<Phase> {
  (0..made.len())
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
    .collect()
}

/// 7.2's `listphases` over a generated history, stated literally (`phases.py` 1049-1058): the node of each changeset
/// holding a draft root whose phase is at most draft, whether the filter serves it or not, sorted (A-691).
fn listed_draft_roots(made: &[Made], nodes: &[Node]) -> Vec<Node> {
  let phase = phases(made);
  let mut roots: Vec<Node> = made
    .iter()
    .zip(nodes)
    .enumerate()
    .filter(|(rev, (made, _))| made.root == Some(Phase::Draft) && phase[*rev] <= Phase::Draft)
    .map(|(_, (_, node))| *node)
    .collect();
  roots.sort_unstable();
  roots
}

/// The draft roots the view lists, sorted.
fn draft_roots_of(view: &Indexed<'_>) -> Vec<Node> {
  let mut roots: Vec<Node> = view.draft_roots().collect();
  roots.sort_unstable();
  roots
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
  /// parents renumbered, the census equal (the stranger's root dropped), the publishing flag passed through, and the
  /// draft roots listed as 7.2's `listphases` stated literally lists them, hidden ones among them (A-691).
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
    prop_assert_eq!(draft_roots_of(&view), listed_draft_roots(&made, &built.nodes));
    let dense: BTreeMap<Node, Rev> =
      (0..view.count()).filter_map(|rev| Some((view.node(rev)?, rev))).collect();
    for (made, node) in made.iter().zip(&built.nodes) {
      let Some(served) = view.rev(node) else { continue };
      let renumbered = made.parents.map(|parent| dense.get(&built.nodes[parent?]).copied());
      prop_assert_eq!(view.parents(served), renumbered);
    }
  }
}

/// A-691 (7.2's `listphases`): do view a public root with two draft roots on it, the first rewritten as the second (a
/// marker hides it); then a public root, a secret root on it and a draft root on that. Expect the hidden draft root
/// listed beside the served one, though the view leaves it out; and the draft root under the secret one not listed,
/// its phase being secret.
#[test]
fn a_hidden_draft_root_is_listed_and_one_under_a_secret_root_is_not() {
  let rewritten = three(
    on(0, Some(Phase::Draft), Some(Obsoleted::By(2)), false),
    on(0, Some(Phase::Draft), None, false),
  );
  let built = build(&rewritten, false);
  let view = view_of(&built, false);
  assert_eq!(view.left_out(&built.nodes[1]), Some(LeftOut::Hidden));
  let mut both = vec![built.nodes[1], built.nodes[2]];
  both.sort_unstable();
  assert_eq!(draft_roots_of(&view), both);
  let under = three(
    on(0, Some(Phase::Secret), None, false),
    on(1, Some(Phase::Draft), None, false),
  );
  let built = build(&under, false);
  assert_eq!(draft_roots_of(&view_of(&built, false)), Vec::<Node>::new());
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
  let made: Vec<Made> = (0_usize..5)
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

/// A `stream_out` answer read back: its first line, then each file's name and bytes.
type Streamed = (Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>);

/// The files a `stream_out` answer sends, read back: its first line, then each name and bytes.
fn streamed(answer: &[u8]) -> Streamed {
  let mut rest = answer;
  let line = |rest: &mut &[u8]| {
    let end = rest.iter().position(|byte| *byte == b'\n').unwrap();
    let head = rest[..end].to_vec();
    *rest = &rest[end + 1..];
    head
  };
  let status = line(&mut rest);
  let counts = String::from_utf8(line(&mut rest)).unwrap();
  let (files, total) = counts.split_once(' ').unwrap();
  let (files, total): (usize, usize) = (files.parse().unwrap(), total.parse().unwrap());
  let mut sent = Vec::new();
  for _ in 0..files {
    let head = line(&mut rest);
    let nul = head.iter().position(|byte| *byte == 0).unwrap();
    let size: usize = std::str::from_utf8(&head[nul + 1..])
      .unwrap()
      .parse()
      .unwrap();
    let (bytes, tail) = rest.split_at(size);
    sent.push((head[..nul].to_vec(), bytes.to_vec()));
    rest = tail;
  }
  assert!(rest.is_empty(), "bytes after the last file");
  assert_eq!(
    sent.iter().map(|(_, bytes)| bytes.len()).sum::<usize>(),
    total
  );
  (status, sent)
}

/// A changeset store as a stream asks it, over an imported index: every revision kept whole, its place its raw text
/// under its text's own identity (the store's contract for a whole, A-767 C1), so the stream's bytes are the whole
/// rendering's (`render::revlogs`).
struct WholeTexts<'s> {
  index: &'s crate::index::HgIndex,
  store: &'s slates_vcs::ContentStore,
  texts: BTreeMap<[u8; 32], Vec<u8>>,
}

impl crate::render::StoredSource for WholeTexts<'_> {
  fn rows(
    &mut self,
    logs: &[(Vec<u8>, Vec<Node>)],
  ) -> Option<Vec<Vec<Option<crate::render::StoredRevision>>>> {
    logs
      .iter()
      .map(|(log, nodes)| self.log_rows(log, nodes))
      .collect()
  }

  fn chunks(&mut self, places: &[crate::render::StoredPlace]) -> Option<Vec<Vec<u8>>> {
    places
      .iter()
      .map(|place| self.texts.get(&place.payload).cloned())
      .collect()
  }
}

impl WholeTexts<'_> {
  /// One log's rows ([`crate::render::StoredSource::rows`]).
  fn log_rows(
    &mut self,
    log: &[u8],
    nodes: &[Node],
  ) -> Option<Vec<Option<crate::render::StoredRevision>>> {
    let log = match log {
      b"\0changelog" => crate::index::Log::Changelog,
      b"\0manifest" => crate::index::Log::Manifest,
      path => crate::index::Log::File(path),
    };
    let mut rows = Vec::with_capacity(nodes.len());
    for node in nodes {
      let rev = self.index.rev(self.store, log, node)?;
      let kept = crate::import::record::kept(self.index, self.store, log, rev)?;
      let mut text = Vec::new();
      crate::import::record::write_text(&mut text, self.store, &kept);
      let identity = *blake3::hash(&text).as_bytes();
      let len = u64::try_from(text.len()).ok()?;
      self.texts.insert(identity, text);
      rows.push(Some(crate::render::StoredRevision {
        text: identity,
        base: None,
        place: crate::render::StoredPlace {
          payload: identity,
          offset: 0,
          len,
        },
      }));
    }
    Some(rows)
  }
}

/// The whole-text store of `index`.
fn whole_texts<'s>(
  index: &'s crate::index::HgIndex,
  store: &'s slates_vcs::ContentStore,
) -> WholeTexts<'s> {
  WholeTexts {
    index,
    store,
    texts: BTreeMap::new(),
  }
}

/// Stream clones part 2 (A-712; 7.2's `generatev1wireproto`): do ask `stream_out` of the `states` repository, which
/// holds a secret changeset, and of the same repository with its secret root dropped. Expect the first answered `1\n`
/// and listing no `streamreqs`; the second `0\n`, then every non-empty revlog of `render::revlogs` in its order, each
/// named by its store path and holding its bytes, and the capabilities listing `streamreqs` with the rendered store's
/// requirements.
#[test]
fn stream_out_sends_the_rendered_revlogs_unless_a_changeset_is_secret() {
  use super::super::stream::stream_out;
  let (imported, store) = imported("states");
  let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room()).unwrap();
  let mut answer = Vec::new();
  stream_out(
    &view,
    &mut whole_texts(&imported.index, &store),
    &mut room(),
    &mut answer,
  )
  .unwrap();
  assert_eq!(answer, b"1\n");
  assert!(!view.streams());
  let public: Vec<PhaseHead> = imported
    .phase_roots
    .iter()
    .filter(|root| root.phase != Phase::Secret)
    .copied()
    .collect();
  let state = State {
    roots: &public,
    ..state_of(&imported)
  };
  let view = Indexed::new(&imported.index, &store, state, &mut room()).unwrap();
  let mut answer = Vec::new();
  stream_out(
    &view,
    &mut whole_texts(&imported.index, &store),
    &mut room(),
    &mut answer,
  )
  .unwrap();
  let (status, sent) = streamed(&answer);
  assert_eq!(status, b"0");
  let expected: Vec<(Vec<u8>, Vec<u8>)> =
    crate::render::revlogs(&imported.index, &store, &mut room())
      .unwrap()
      .into_iter()
      .filter(|(_, bytes)| !bytes.is_empty())
      .collect();
  assert_eq!(sent, expected);
  assert!(sent.iter().any(|(name, _)| name == b"00changelog.i"));
}

/// Each manifest and file revision a changegroup carries, with its link, as a set: what a fetch brings, whatever the
/// order its packer took.
fn carried(packed: &[u8]) -> BTreeSet<(Vec<u8>, Node, Node)> {
  let packed = Changegroup::decode(packed, Version::V03, &mut room()).unwrap();
  let mut carried: BTreeSet<(Vec<u8>, Node, Node)> = packed
    .group(&packed.manifests)
    .iter()
    .map(|revision| (Vec::new(), revision.node, revision.link))
    .collect();
  for group in &packed.files {
    carried.extend(
      packed
        .group(&group.revisions)
        .iter()
        .map(|revision| (group.name.to_vec(), revision.node, revision.link)),
    );
  }
  carried
}

/// A-745 (the link walk over a real index). Do: import each of `fixtures/state`'s repositories and clone each view
/// that serves every changeset its changelog holds, by its links (`changegroup`, walking the manifest log and the file
/// logs from row to row) and by the changesets that name each revision (`getbundle::Named`, 7.2's slow path). Expect
/// the two to carry the same revisions with the same links, and at least one view walked by its links.
#[test]
fn a_full_clone_walked_by_its_links_carries_what_the_namers_name() {
  let mut walked = 0;
  for name in ["amended-root", "divergent", "states"] {
    let (imported, store) = imported(name);
    let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room())
      .unwrap()
      .answering_from_its_texts();
    if !view.serves_every_changeset() {
      continue;
    }
    walked += 1;
    let outgoing = pull::outgoing(&view, b"", b"", &mut room()).unwrap();
    let by_links = changegroup(&view, &outgoing, Version::V03, &mut room()).unwrap();
    let named =
      super::super::getbundle::named_changegroup_whole(&view, &outgoing, Version::V03, &mut room())
        .unwrap();
    assert_eq!(carried(&by_links), carried(&named), "{name}");
    assert!(!carried(&by_links).is_empty(), "{name}");
  }
  assert!(walked > 0, "no view was walked by its links");
}

/// A-745 (stream clones in slices). Do: with each of `fixtures/state`'s repositories imported and its secret roots
/// dropped, ask `stream_out` whole within one room and a step at a time (`render::Streaming`), each step given a room of
/// its own; find the least room each way. Expect each step to need less room than the whole answer, and the steps'
/// bytes the whole answer's, byte for byte.
#[test]
fn a_stream_clone_is_sent_a_row_at_a_time_each_within_its_own_room() {
  use super::super::stream::stream_out;
  for name in ["amended-root", "divergent", "states"] {
    let (imported, store) = imported(name);
    let public: Vec<PhaseHead> = imported
      .phase_roots
      .iter()
      .filter(|root| root.phase != Phase::Secret)
      .copied()
      .collect();
    let state = State {
      roots: &public,
      ..state_of(&imported)
    };
    let view = Indexed::new(&imported.index, &store, state, &mut room()).unwrap();
    let mut whole = Vec::new();
    stream_out(
      &view,
      &mut whole_texts(&imported.index, &store),
      &mut room(),
      &mut whole,
    )
    .unwrap();
    let stepped = |each: u64| -> Option<Vec<u8>> {
      let mut streaming = crate::render::Streaming::default();
      let mut source = whole_texts(&imported.index, &store);
      let mut out = Vec::new();
      loop {
        crate::render::answer_want(&mut streaming, &mut source).ok()?;
        if !streaming
          .step(&imported.index, &store, &mut Admission::new(each), &mut out)
          .ok()?
        {
          break;
        }
      }
      Some(out)
    };
    let least = |takes: &dyn Fn(u64) -> bool| {
      let (mut low, mut high) = (0_u64, 1_u64 << 24);
      while high - low > 1 {
        let middle = low + (high - low) / 2;
        if takes(middle) {
          high = middle;
        } else {
          low = middle;
        }
      }
      high
    };
    let whole_room = least(&|each| {
      stream_out(
        &view,
        &mut whole_texts(&imported.index, &store),
        &mut Admission::new(each),
        &mut Vec::new(),
      )
      .is_ok()
    });
    let step_room = least(&|each| stepped(each).is_some());
    assert!(
      step_room < whole_room,
      "{name}: a step took {step_room}, the whole {whole_room}"
    );
    assert_eq!(stepped(step_room).unwrap(), whole, "{name}");
  }
}

/// Ada's no-allocation rule and goal 93's clone (found in the code, 2026-11-20: a clone sent every manifest as its whole
/// text against the null base, about 57,000 texts of up to 179 KB for hg's own history, each rebuilt from its chain on
/// the server and diffed again by the client). Do: import each of `fixtures/state`'s repositories, its manifests kept
/// as deltas (A-729), and clone each view that serves every changeset by its links; rebuild every manifest the answer
/// carries from its base as a client does, each checked against its node. Expect every text to rebuild to its node,
/// and some manifest sent as its stored delta against the one sent before it rather than whole.
#[test]
fn a_clone_sends_a_manifest_kept_as_a_delta_as_that_delta() {
  let mut deltas = 0usize;
  for name in ["amended-root", "divergent", "states"] {
    let (imported, store) = imported(name);
    let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room())
      .unwrap()
      .answering_from_its_texts();
    if !view.serves_every_changeset() {
      continue;
    }
    let outgoing = pull::outgoing(&view, b"", b"", &mut room()).unwrap();
    let packed = changegroup(&view, &outgoing, Version::V03, &mut room()).unwrap();
    let packed = Changegroup::decode(&packed, Version::V03, &mut room()).unwrap();
    let mut texts: BTreeMap<Node, Vec<u8>> = BTreeMap::new();
    for revision in packed.group(&packed.manifests) {
      let base: &[u8] = if revision.base == NULL_NODE {
        &[]
      } else {
        deltas += 1;
        texts
          .get(&revision.base)
          .map(Vec::as_slice)
          .expect("a base sent before")
      };
      let text = delta::apply(base, revision.data, &mut room()).unwrap();
      node::check(&revision.node, &revision.p1, &revision.p2, &text).unwrap();
      texts.insert(revision.node, text);
    }
  }
  assert!(deltas > 0, "every manifest was sent whole");
}

/// A changeset store keeping each revision after a log's first as a delta against the one before it: one hunk replacing
/// the bytes the two texts do not share at their ends (an `mpatch` edit, the store's one form), the first whole.
struct Deltas<'s> {
  whole: WholeTexts<'s>,
  edits: BTreeMap<[u8; 32], Vec<u8>>,
  delta_bytes: u64,
}

impl crate::render::StoredSource for Deltas<'_> {
  fn rows(
    &mut self,
    logs: &[(Vec<u8>, Vec<Node>)],
  ) -> Option<Vec<Vec<Option<crate::render::StoredRevision>>>> {
    logs
      .iter()
      .map(|(log, nodes)| self.log_rows(log, nodes))
      .collect()
  }

  fn chunks(&mut self, places: &[crate::render::StoredPlace]) -> Option<Vec<Vec<u8>>> {
    places
      .iter()
      .map(|place| {
        self
          .edits
          .get(&place.payload)
          .cloned()
          .or_else(|| self.whole.texts.get(&place.payload).cloned())
      })
      .collect()
  }
}

impl Deltas<'_> {
  /// One log's rows ([`crate::render::StoredSource::rows`]): after its first, each revision kept as an edit against the
  /// one before it.
  fn log_rows(
    &mut self,
    log: &[u8],
    nodes: &[Node],
  ) -> Option<Vec<Option<crate::render::StoredRevision>>> {
    let wholes = self.whole.log_rows(log, nodes)?;
    let mut rows = Vec::with_capacity(wholes.len());
    let mut before: Option<([u8; 32], Vec<u8>)> = None;
    for row in wholes {
      let row = row?;
      let text = self.whole.texts.get(&row.text)?.clone();
      let stored = match &before {
        Some((base, base_text)) if *base != row.text => {
          let start = base_text
            .iter()
            .zip(&text)
            .take_while(|(a, b)| a == b)
            .count();
          let tail = base_text[start..]
            .iter()
            .rev()
            .zip(text[start..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
          let (end, data) = (base_text.len() - tail, &text[start..text.len() - tail]);
          let mut edit = Vec::new();
          for field in [start, end, data.len()] {
            edit.extend_from_slice(&u32::try_from(field).ok()?.to_be_bytes());
          }
          edit.extend_from_slice(data);
          let place = *blake3::hash(&edit).as_bytes();
          let len = u64::try_from(edit.len()).ok()?;
          self.edits.insert(place, edit);
          crate::render::StoredRevision {
            text: row.text,
            base: Some(*base),
            place: crate::render::StoredPlace {
              payload: place,
              offset: 0,
              len,
            },
          }
        }
        _ => row,
      };
      self.delta_bytes = self.delta_bytes.saturating_add(stored.place.len);
      before = Some((row.text, text));
      rows.push(Some(stored));
    }
    Some(rows)
  }
}

/// Goal 93 perf (main's ruling of 2026-11-21 00:30: a stream sends each revision as its stored delta against its stored
/// base, read in place, never a text made whole). Do: stream the `states` repository (its secret root dropped) from a
/// store keeping each revision after a log's first as a delta, and from one keeping every revision whole; read every
/// streamed revlog back with the revlog reader, each text held to its node. Expect: the delta stream's chunk bytes the
/// store's delta bytes plus one marker a chunk (so its bytes within the deltas, the entries and the file lines), fewer
/// bytes than the whole stream, and every revision read back.
#[test]
fn a_stream_sends_each_revision_as_the_store_keeps_it_within_its_delta_bytes() {
  use super::super::stream::stream_out;
  let (imported, store) = imported("states");
  let public: Vec<PhaseHead> = imported
    .phase_roots
    .iter()
    .filter(|root| root.phase != Phase::Secret)
    .copied()
    .collect();
  let state = State {
    roots: &public,
    ..state_of(&imported)
  };
  let view = Indexed::new(&imported.index, &store, state, &mut room()).unwrap();
  let mut whole = Vec::new();
  stream_out(
    &view,
    &mut whole_texts(&imported.index, &store),
    &mut room(),
    &mut whole,
  )
  .unwrap();
  let mut deltas = Deltas {
    whole: whole_texts(&imported.index, &store),
    edits: BTreeMap::new(),
    delta_bytes: 0,
  };
  let mut answer = Vec::new();
  stream_out(&view, &mut deltas, &mut room(), &mut answer).unwrap();
  let (status, sent) = streamed(&answer);
  assert_eq!(status, b"0");
  let entries: usize = sent
    .iter()
    .filter(|(name, _)| name.ends_with(b".i"))
    .map(|(_, bytes)| bytes.len())
    .sum();
  let chunks: usize = sent
    .iter()
    .filter(|(name, _)| name.ends_with(b".d"))
    .map(|(_, bytes)| bytes.len())
    .sum();
  let revisions = entries / 64;
  // Counted twice by the walk (the count, then the writing), the store's bytes are half its tally.
  let delta_bytes = usize::try_from(deltas.delta_bytes / 2).unwrap();
  assert!(
    chunks <= delta_bytes + revisions,
    "chunks {chunks}, deltas {delta_bytes}, revisions {revisions}"
  );
  assert!(
    answer.len() < whole.len(),
    "{} against the whole {}",
    answer.len(),
    whole.len()
  );
  // Every revlog read back: each text rebuilt through its chain and held to its node by the reader.
  for (name, index_bytes) in sent.iter().filter(|(name, _)| name.ends_with(b".i")) {
    let data_name = [&name[..name.len() - 2], b".d"].concat();
    let data = sent
      .iter()
      .find(|(sent_name, _)| *sent_name == data_name)
      .map_or(&[][..], |(_, bytes)| bytes.as_slice());
    let log = crate::revlog::Revlog::open(index_bytes, data, &mut room()).unwrap();
    for rev in 0..log.len() {
      log.text(rev, &mut room()).unwrap_or_else(|refused| {
        panic!("{:?} rev {rev}: {refused:?}", String::from_utf8_lossy(name))
      });
    }
  }
}

/// A changeset store that counts what a stream asks of it, answering through `inner`.
struct Counted<S> {
  inner: S,
  rows: usize,
  chunks: usize,
}

impl<S: crate::render::StoredSource> crate::render::StoredSource for Counted<S> {
  fn rows(
    &mut self,
    logs: &[(Vec<u8>, Vec<Node>)],
  ) -> Option<Vec<Vec<Option<crate::render::StoredRevision>>>> {
    self.rows += 1;
    self.inner.rows(logs)
  }

  fn chunks(&mut self, places: &[crate::render::StoredPlace]) -> Option<Vec<Vec<u8>>> {
    self.chunks += 1;
    self.inner.chunks(places)
  }
}

/// Shape: the file logs of [`a_stream_asks_the_store_by_the_room_not_by_the_log`]'s repository, many beside its three
/// other logs, so an ask a log stands out.
const MANY_LOGS: u64 = 64;

/// Goal 93 perf (main's order of 2026-11-21 05:06: the stream clone ~37x slower than stock; r20 on 94c1696bb, hg-src:
/// the serving shard idle half the stream, waiting on one store ask a log, both passes, about 12,000 of them, each a turn
/// with its view made afresh). Do: build a repository of one changeset and [`MANY_LOGS`] file logs of one revision
/// each; stream it whole, in a room that holds it, from a store that counts its asks. Expect the store asked for rows
/// once a pass (the count, then the writing) and for chunks once, whatever the logs; and the same bytes as the stream
/// made a step at a time in a room that holds one log's rows or one chunk (which asks more).
#[test]
fn a_stream_asks_the_store_by_the_room_not_by_the_log() {
  use super::super::stream::stream_out;
  let mut built = build(
    &[Made {
      parents: [None, None],
      root: None,
      marker: None,
      bookmark: false,
    }],
    false,
  );
  for at in 0..MANY_LOGS {
    let path = format!("file-{at:03}").into_bytes();
    let text = format!("text of file {at}\n").into_bytes();
    let revision = NewRevision {
      node: node::compute(&NULL_NODE, &NULL_NODE, &text).unwrap(),
      p1: NULL_NODE,
      p2: NULL_NODE,
      link: built.nodes[0],
      flags: 0,
      len: u64::try_from(text.len()).unwrap(),
      bytes: built.store.put(&text).unwrap(),
      block: None,
    };
    built
      .index
      .append_revisions(&mut built.store, Log::File(&path), &[revision])
      .unwrap();
  }
  let view = view_of(&built, true);
  let mut counted = Counted {
    inner: whole_texts(&built.index, &built.store),
    rows: 0,
    chunks: 0,
  };
  let mut answer = Vec::new();
  stream_out(&view, &mut counted, &mut room(), &mut answer).unwrap();
  let (status, sent) = streamed(&answer);
  assert_eq!(status, b"0");
  // The same stream a step at a time in a small room, its batches a log or a chunk each: the same bytes.
  let mut streaming = crate::render::Streaming::default();
  let mut small = Counted {
    inner: whole_texts(&built.index, &built.store),
    rows: 0,
    chunks: 0,
  };
  let mut stepped = Vec::new();
  loop {
    crate::render::answer_want(&mut streaming, &mut small).unwrap();
    if !streaming
      .step(
        &built.index,
        &built.store,
        &mut Admission::new(SMALL_ROOM),
        &mut stepped,
      )
      .unwrap()
    {
      break;
    }
  }
  assert_eq!(stepped, answer, "the stream asked a log at a time");
  assert!(
    small.rows > counted.rows,
    "a small room asked {} times",
    small.rows
  );
  assert_eq!(
    (
      sent
        .iter()
        .filter(|(name, _)| name.ends_with(b".i"))
        .count(),
      counted.rows,
      counted.chunks
    ),
    // The file logs and the changelog: `build`'s changesets name no manifest, so no manifest log is written.
    (usize::try_from(MANY_LOGS).unwrap() + 1, 2, 1),
    "(index files, row asks, chunk asks)"
  );
}

/// Shape: a step's room small enough that a batch holds one log's rows or one chunk, large enough for an entry.
const SMALL_ROOM: u64 = 1 << 8;

/// A-767 C1 (main's ruling (A) of 2026-11-21: lookup's `.hgtags` through the changeset store). Do: over `hidden-tag`
/// (`make-state-fixtures.sh`: a visible head whose `.hgtags` tags the root `seen`, and a pruned, hidden head whose
/// `.hgtags` alone tags `hidden-only`), read the heads' `.hgtags` from the store and look up both names. Expect one want
/// of rows naming only the visible head's `.hgtags` revision, then one of texts; `seen` the root, as stock 7.2's `hg log
/// -r seen` resolved it; `hidden-only` unknown, its hidden head's `.hgtags` never read.
#[test]
fn a_tag_only_a_hidden_head_carries_is_unknown_and_the_tags_are_read_in_one_round() {
  use super::super::lookup::{TagsRead, lookup};
  use crate::render::Want;
  let (imported, store) = imported("hidden-tag");
  let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room())
    .unwrap()
    .answering_from_its_texts();
  let mut tags = TagsRead::begin(&view, &mut room()).unwrap();
  let mut asks = Vec::new();
  while let Some(want) = tags.wants().cloned() {
    tags.give(view.stored_now(&want).unwrap());
    asks.push(want);
  }
  assert_eq!(asks.len(), 2, "one want of rows, one of texts: {asks:?}");
  match &asks[0] {
    Want::Rows { logs } => {
      assert_eq!(logs.len(), 1);
      assert_eq!(logs[0].0, b".hgtags");
      assert_eq!(logs[0].1.len(), 1, "the visible head's .hgtags alone");
    }
    other => panic!("rows first, not {other:?}"),
  }
  let answer = |key: &str| {
    let mut out = Vec::new();
    lookup(&view, Some(&tags), key.as_bytes(), &mut room(), &mut out).unwrap();
    String::from_utf8(out).unwrap()
  };
  assert_eq!(
    answer("seen"),
    "1 c3daaa7ea5f5d20902f12c53611fefce7e15cefa\n"
  );
  assert_eq!(answer("hidden-only"), "0 unknown revision 'hidden-only'\n");
}

/// A-767 C1 (main's ruling (A)). Do: over `states`, whose heads hold no `.hgtags`, begin reading the tags over a view
/// that answers the store only between steps, and look up a name nothing has, in a whole answer too. Expect nothing
/// asked of the store and the name unknown in 7.2's words.
#[test]
fn a_repository_without_hgtags_asks_the_store_nothing_for_a_lookup() {
  use super::super::lookup::{TagsRead, lookup};
  let (imported, store) = imported("states");
  let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room()).unwrap();
  let tags = TagsRead::begin(&view, &mut room()).unwrap();
  assert_eq!(tags.wants(), None);
  let mut out = Vec::new();
  lookup(&view, Some(&tags), b"nothing", &mut room(), &mut out).unwrap();
  assert_eq!(out, b"0 unknown revision 'nothing'\n");
  let whole = TagsRead::now(&view, &mut room()).unwrap();
  assert_eq!(whole.wants(), None);
}

/// A-767 C1 (main's ruling (A): one round of wants for all the names a batch looks up). Do: over `hidden-tag`, answer
/// a `batch` of two lookups that reach the tags, its view answering the store only between steps, its wants answered by
/// the caller. Expect the batch to wait on the store once, two asks in all (rows, texts) for both lookups, and its
/// answer `seen`'s node then `nope` unknown.
#[test]
fn a_batch_of_lookups_reads_the_tags_once() {
  use super::super::session::{Batch, BatchStep, Transport};
  use crate::render::StoreWants;
  let (imported, store) = imported("hidden-tag");
  let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room()).unwrap();
  let answering = Indexed::new(&imported.index, &store, state_of(&imported), &mut room())
    .unwrap()
    .answering_from_its_texts();
  let mut batch = Batch::begin(b"lookup key=seen;lookup key=nope", &mut room()).unwrap();
  let mut waits = 0;
  let mut asks = 0;
  let answer = loop {
    match batch.step(&view, Transport::Ssh, &mut room()).unwrap() {
      BatchStep::Store => {
        waits += 1;
        while let Some(want) = batch.wants().cloned() {
          asks += 1;
          match answering.stored_now(&want).unwrap() {
            super::super::getbundle::Given::Rows(rows) => batch.give_rows(rows),
            super::super::getbundle::Given::Texts(texts) => batch.give_texts(texts),
            other => panic!("a .hgtags read is given rows or texts, not {other:?}"),
          }
        }
      }
      BatchStep::Answered(answer) => break answer,
      BatchStep::Push(_) => panic!("no pushkey call"),
    }
  };
  assert_eq!((waits, asks), (1, 2));
  assert_eq!(
    String::from_utf8(answer).unwrap(),
    "1 c3daaa7ea5f5d20902f12c53611fefce7e15cefa\n;0 unknown revision 'nope'\n"
  );
}

/// Main 01a20182's order of 2026-11-22 10:40 (CI 45037561866: a stream clone of Mercurial's 57,054-changeset history
/// spun its shard past a second, the changelog's nodes gathered for one ask in one step). Do: build a line of five
/// times [`crate::render::rows_a_step`] changesets; stream it whole, and again a step at a time. Expect the same bytes;
/// and at least four steps that only gather an ask's nodes (no bytes written, nothing asked yet), so no step reads more
/// than a content window's worth of rows. Red before: none, the whole changelog gathered in one step.
#[test]
fn a_streams_ask_for_a_long_logs_rows_is_gathered_a_window_a_step() {
  use super::super::stream::stream_out;
  let probe = build(&[], false);
  let long = usize::try_from(crate::render::rows_a_step(&probe.store) * 5).unwrap();
  let line: Vec<Made> = (0..long)
    .map(|rev| Made {
      parents: [rev.checked_sub(1), None],
      root: None,
      marker: None,
      bookmark: false,
    })
    .collect();
  let built = build(&line, false);
  let view = view_of(&built, true);
  let mut whole = Vec::new();
  stream_out(
    &view,
    &mut whole_texts(&built.index, &built.store),
    &mut room(),
    &mut whole,
  )
  .unwrap();
  let mut streaming = crate::render::Streaming::default();
  let mut source = whole_texts(&built.index, &built.store);
  let (mut stepped, mut gathering) = (Vec::new(), 0);
  loop {
    crate::render::answer_want(&mut streaming, &mut source).unwrap();
    let before = stepped.len();
    let more = streaming
      .step(&built.index, &built.store, &mut room(), &mut stepped)
      .unwrap();
    if !more {
      break;
    }
    if stepped.len() == before && streaming.wants().is_none() {
      gathering += 1;
    }
  }
  assert_eq!(stepped, whole);
  assert!(
    gathering >= 4,
    "{gathering} steps gathered a {long}-changeset log's ask"
  );
}

/// Main 01a20182's order of 2026-11-22 10:40 (CI 45037561866: a clone of some heads of Mercurial's 57,054-changeset
/// history made its changesets' and manifests' groups whole in its walk's begin, 4.1 s in a debug build, and its shard
/// was killed for spinning). Do: over `states`' served view, begin the walk of a pull whose client holds the first
/// changeset (by the changesets that name each revision, not the links), then step it to its end. Expect the begin to
/// lend no text; no step to lend more than two (a changeset's and its manifest's); and the changegroup the one the
/// oracle makes whole. Red before: the begin lent every missing changeset's and manifest's text.
#[test]
fn a_pull_walked_by_its_changesets_lends_no_text_in_its_begin_and_two_at_most_a_step() {
  let (imported, store) = imported("states");
  let view = Indexed::new(&imported.index, &store, state_of(&imported), &mut room())
    .unwrap()
    .answering_from_its_texts();
  let lent = |view: &Indexed<'_>| {
    let lends = view.lends();
    lends.in_place + lends.copied
  };
  let outgoing = pull::Outgoing {
    heads: vec![view.count() - 1],
    common: vec![0],
    missing: (1..view.count()).map(|rev| rev..rev + 1).collect(),
  };
  let before = lent(&view);
  let mut walk =
    super::super::getbundle::Walk::begin(&view, &outgoing, Version::V03, &mut room()).unwrap();
  assert_eq!(lent(&view), before, "the begin lent no text");
  let (mut out, mut most) = (Vec::new(), 0);
  loop {
    let at = lent(&view);
    let more = walk.step(&view, &mut room(), &mut out).unwrap();
    most = most.max(lent(&view) - at);
    if !more {
      break;
    }
  }
  assert!(most <= 2, "a step lent {most} texts");
  let whole =
    super::super::getbundle::named_changegroup_whole(&view, &outgoing, Version::V03, &mut room())
      .unwrap();
  assert_eq!(out, whole);
}

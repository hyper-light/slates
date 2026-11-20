//! The tests of a tree's copy by reference (A-542, part 1): its cost against the tree's size, its refusals, a copy into
//! its own subtree, and the oracle: generated histories of puts, removals, copies and a fork, the file, mode and
//! attribute maps each held after every step to a reference that copies key by key, every node and block freed at the
//! end.

use std::cell::RefCell;
use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::*;

/// The destination of the cost test's copies.
const DESTINATION: &str = "dst";

/// The source of the cost test's copies.
const SOURCE: &str = "src";

/// Whether `key` is in `path`'s tree: `path` itself or beneath it; every key is in the root's.
fn in_tree(key: &str, path: &str) -> bool {
  path.is_empty()
    || key == path
    || key
      .strip_prefix(path)
      .is_some_and(|rest| rest.starts_with('/'))
}

/// `key`, in `from`'s tree, at the same place in `to`'s.
fn moved(key: &str, from: &str, to: &str) -> String {
  match (from.is_empty(), key.is_empty()) {
    (true, true) => to.to_owned(),
    (true, false) => format!("{to}/{key}"),
    (false, _) => format!("{to}{}", key.strip_prefix(from).unwrap()),
  }
}

/// The reference: `from`'s tree copied to `to` key by key, as a map that shares nothing must.
fn copied_by_hand<V: Clone>(
  model: &mut BTreeMap<String, V>,
  from: &str,
  to: &str,
) -> Result<Copied, CopyRefused> {
  if to.is_empty() {
    return Err(CopyRefused::Root);
  }
  if model.keys().any(|key| in_tree(key, to)) {
    return Err(CopyRefused::Occupied);
  }
  let tree: Vec<(String, V)> = model
    .iter()
    .filter(|(key, _)| in_tree(key, from))
    .map(|(key, value)| (moved(key, from, to), value.clone()))
    .collect();
  let copied = Copied {
    own: model.contains_key(from),
    beneath: tree.iter().any(|(key, _)| key != to),
  };
  model.extend(tree);
  Ok(copied)
}

/// The reference for attributes: every `(path, name)` of `from`'s tree copied to `to`'s, pair by pair.
fn attributes_copied_by_hand(
  model: &mut BTreeMap<(String, String), Vec<u8>>,
  from: &str,
  to: &str,
) -> Result<Copied, CopyRefused> {
  if to.is_empty() {
    return Err(CopyRefused::Root);
  }
  if model.keys().any(|(path, _)| in_tree(path, to)) {
    return Err(CopyRefused::Occupied);
  }
  let tree: Vec<((String, String), Vec<u8>)> = model
    .iter()
    .filter(|((path, _), _)| in_tree(path, from))
    .map(|((path, name), value)| ((moved(path, from, to), name.clone()), value.clone()))
    .collect();
  let copied = Copied {
    own: tree.iter().any(|((path, _), _)| path == to),
    beneath: tree.iter().any(|((path, _), _)| path != to),
  };
  model.extend(tree);
  Ok(copied)
}

/// A-542 (part 1). Do: copy a tree of one file and a tree of 4,096 files, each beneath `src`, to `dst`. Expect, for
/// both: no walk ran (the arena's visited count does not move: the non-vacuity counter that no copy reads its tree),
/// no block was made (each copied file is one more holder of its bytes), and the nodes the arena gained are at most
/// the destination path's nodes twice over and the grafted top node with its one child, whatever the tree holds: the
/// cost of the paths, never of the tree. Every copied file reads its source's bytes.
#[test]
fn a_copy_costs_its_paths_never_its_tree() {
  for count in [1usize, 4096] {
    let mut files = Files::default();
    for index in 0..count {
      let byte = u8::try_from(index % usize::from(u8::MAX)).unwrap();
      files.insert(format!("{SOURCE}/{index}"), vec![byte]);
    }
    let (visited, live, blocks) = (
      files.map.nodes.visited(),
      files.map.nodes.live(),
      files.blocks.live(),
    );
    let copied = files.copy_tree(SOURCE, DESTINATION);
    assert_eq!(
      copied,
      Ok(Copied {
        own: false,
        beneath: true
      }),
      "{count} files"
    );
    assert_eq!(files.map.nodes.visited(), visited, "no walk, {count} files");
    assert_eq!(files.blocks.live(), blocks, "no block made, {count} files");
    let bound = 2 * (DESTINATION.len() + 1) + 2;
    let gained = files.map.nodes.live() - live;
    assert!(
      gained <= bound,
      "{gained} nodes gained for {count} files, at most {bound}"
    );
    for index in 0..count {
      let source = files.get(format!("{SOURCE}/{index}")).map(<[u8]>::to_vec);
      assert_eq!(
        files
          .get(format!("{DESTINATION}/{index}"))
          .map(<[u8]>::to_vec),
        source
      );
    }
  }
}

/// A-542 (part 1), A-586. Do: copy onto a destination that is present, onto one with a key beneath it, and onto the
/// root, in each kind of map. Expect: each refused with its reason, and every map exactly as it was.
#[test]
fn a_copy_onto_what_is_there_or_the_root_is_refused_and_changes_nothing() {
  let mut modes: Dim<u32> = Dim::default();
  let mut attributes = Xattrs::default();
  for (path, mode) in [("a", 1), ("a/x", 2), ("b", 3), ("c/y", 4)] {
    modes.insert(path, mode);
    attributes.insert(path, "n", vec![u8::try_from(mode).unwrap()]);
  }
  let before: Vec<(String, u32)> = modes.iter().map(|(key, value)| (key, *value)).collect();
  let attributes_before: Vec<((String, String), Vec<u8>)> = attributes
    .iter()
    .map(|(key, value)| (key, value.clone()))
    .collect();
  for (from, to, refusal) in [
    ("a", "b", CopyRefused::Occupied),
    ("a", "c", CopyRefused::Occupied),
    ("", "a/x", CopyRefused::Occupied),
    ("a", "", CopyRefused::Root),
    ("", "", CopyRefused::Root),
  ] {
    assert_eq!(modes.copy_tree(from, to), Err(refusal), "{from} to {to}");
    assert_eq!(
      attributes.copy_tree(from, to),
      Err(refusal),
      "{from} to {to}"
    );
  }
  let after: Vec<(String, u32)> = modes.iter().map(|(key, value)| (key, *value)).collect();
  let attributes_after: Vec<((String, String), Vec<u8>)> = attributes
    .iter()
    .map(|(key, value)| (key, value.clone()))
    .collect();
  assert_eq!((after, attributes_after), (before, attributes_before));
}

/// A-542 (part 1; `tree.c` `copy_helper` refuses no copy into its own subtree, §4.17.4). Do: copy `a` to `a/b/c`
/// beneath itself, in the mode and attribute maps. Expect: the source's tree as it was before the copy appears beneath
/// the destination, once, nothing copied twice, as the reference that copies key by key gives.
#[test]
fn a_copy_into_its_own_subtree_copies_the_source_as_it_was() {
  let mut modes: Dim<u32> = Dim::default();
  let mut model = BTreeMap::new();
  let mut attributes = Xattrs::default();
  let mut attribute_model = BTreeMap::new();
  for (path, mode) in [("a", 1), ("a/b", 2), ("a/b/d", 3), ("a/e", 4)] {
    modes.insert(path, mode);
    model.insert(path.to_owned(), mode);
    attributes.insert(path, "n", vec![u8::try_from(mode).unwrap()]);
    attribute_model.insert(
      (path.to_owned(), "n".to_owned()),
      vec![u8::try_from(mode).unwrap()],
    );
  }
  assert_eq!(
    modes.copy_tree("a", "a/b/c"),
    copied_by_hand(&mut model, "a", "a/b/c")
  );
  assert_eq!(
    attributes.copy_tree("a", "a/b/c"),
    attributes_copied_by_hand(&mut attribute_model, "a", "a/b/c")
  );
  let walked: BTreeMap<String, u32> = modes.iter().map(|(key, value)| (key, *value)).collect();
  assert_eq!(walked, model);
  assert!(walked.contains_key("a/b/c/b/d") && !walked.contains_key("a/b/c/b/c"));
  let walked: BTreeMap<(String, String), Vec<u8>> = attributes
    .iter()
    .map(|(key, value)| (key, value.clone()))
    .collect();
  assert_eq!(walked, attribute_model);
}

/// A-586 (the root's copy: `svn copy ^/ ^/snap`, which a stock client sends, `libsvn_client/copy.c` refusing a copy
/// into its own child only between working copies). Do: give the root, `a`, `a/b` and `c/d` modes, files and attributes,
/// then copy the root to `snap` in each map. Expect: every map equal to the reference that copies key by key: every key
/// beneath `snap`, the root's own mode and attributes at `snap`'s own key and never at a key ending in the separator;
/// no walk ran and no file's bytes were copied.
#[test]
fn a_copy_of_the_root_brings_every_key_and_gives_the_roots_own_to_the_destination() {
  let mut modes: Dim<u32> = Dim::default();
  let mut mode_model = BTreeMap::new();
  let mut files = Files::default();
  let mut file_model = BTreeMap::new();
  let mut attributes = Xattrs::default();
  let mut attribute_model = BTreeMap::new();
  for (path, value) in [("", 1u8), ("a", 2), ("a/b", 3), ("c/d", 4)] {
    modes.insert(path, u32::from(value));
    mode_model.insert(path.to_owned(), u32::from(value));
    files.insert(path, vec![value]);
    file_model.insert(path.to_owned(), vec![value]);
    attributes.insert(path, "p", vec![value]);
    attribute_model.insert((path.to_owned(), "p".to_owned()), vec![value]);
  }
  let visited = (modes.visited(), files.visited(), attributes.visited());
  let blocks = files.blocks.live();
  assert_eq!(
    modes.copy_tree("", "snap"),
    copied_by_hand(&mut mode_model, "", "snap")
  );
  assert_eq!(
    files.copy_tree("", "snap"),
    copied_by_hand(&mut file_model, "", "snap")
  );
  assert_eq!(
    attributes.copy_tree("", "snap"),
    attributes_copied_by_hand(&mut attribute_model, "", "snap")
  );
  assert_eq!(
    (modes.visited(), files.visited(), attributes.visited()),
    visited,
    "the root's copy walked a map"
  );
  assert_eq!(files.blocks.live(), blocks, "a file's bytes were copied");
  root_copy_reads_as_the_reference(
    (&modes, &files, &attributes),
    &mode_model,
    &file_model,
    &attribute_model,
  );
}

/// The root copy's maps each equal to its reference, the root's own mode and attributes at `snap`'s own key and never
/// at a key ending in the separator.
fn root_copy_reads_as_the_reference(
  (modes, files, attributes): (&Dim<u32>, &Files, &Xattrs),
  mode_model: &BTreeMap<String, u32>,
  file_model: &BTreeMap<String, Vec<u8>>,
  attribute_model: &BTreeMap<(String, String), Vec<u8>>,
) {
  let walked: BTreeMap<String, u32> = modes.iter().map(|(key, value)| (key, *value)).collect();
  assert_eq!(&walked, mode_model);
  assert_eq!(walked.get("snap"), Some(&1));
  assert!(!walked.contains_key("snap/"));
  let walked: BTreeMap<String, Vec<u8>> = files
    .iter()
    .map(|(key, bytes)| (key, bytes.to_vec()))
    .collect();
  assert_eq!(&walked, file_model);
  let walked: BTreeMap<(String, String), Vec<u8>> = attributes
    .iter()
    .map(|(key, value)| (key, value.clone()))
    .collect();
  assert_eq!(&walked, attribute_model);
  assert_eq!(attributes.get("snap", "p"), Some(&vec![1]));
  assert_eq!(attributes.get("snap/a/b", "p"), Some(&vec![3]));
  assert!(attributes.of("snap/").next().is_none());
}

/// A-586 (a green made at a version before a copy takes the copy's tree away). Do: put modes, files and attributes at
/// `a`, `a/b`, `a/c/d`, `ab` and `b`, then cut `a`'s tree from each map. Expect: `a` and everything beneath it gone,
/// `ab` and `b` left as they were, no walk ran, and the cut files' blocks let go; then cut the root's tree, and every
/// map is empty.
#[test]
fn a_cut_takes_a_paths_tree_away_and_leaves_its_neighbours() {
  let mut modes: Dim<u32> = Dim::default();
  let mut files = Files::default();
  let mut attributes = Xattrs::default();
  for (path, value) in [("a", 1u8), ("a/b", 2), ("a/c/d", 3), ("ab", 4), ("b", 5)] {
    modes.insert(path, u32::from(value));
    files.insert(path, vec![value]);
    attributes.insert(path, "p", vec![value]);
  }
  let visited = (modes.visited(), files.visited(), attributes.visited());
  modes.cut_tree("a");
  files.cut_tree("a");
  attributes.cut_tree("a");
  assert_eq!(
    (modes.visited(), files.visited(), attributes.visited()),
    visited,
    "a cut walked a map"
  );
  left_after_the_cut(&modes, &files, &attributes);
  modes.cut_tree("");
  files.cut_tree("");
  attributes.cut_tree("");
  assert_eq!(
    (modes.len(), files.map.len(), attributes.map.len()),
    (0, 0, 0)
  );
  assert_eq!(files.blocks.live(), 0);
}

/// What the cut of `a` leaves in each map: `ab` and `b` as they were, and only the blocks of their files.
fn left_after_the_cut(modes: &Dim<u32>, files: &Files, attributes: &Xattrs) {
  let left: Vec<String> = modes.keys().collect();
  assert_eq!(left, ["ab", "b"]);
  let left: Vec<String> = files.keys().collect();
  assert_eq!(left, ["ab", "b"]);
  assert_eq!(files.blocks.live(), 2, "the cut files' blocks were kept");
  let left: Vec<(String, String)> = attributes.iter().map(|(key, _)| key).collect();
  assert_eq!(
    left,
    [
      ("ab".to_owned(), "p".to_owned()),
      ("b".to_owned(), "p".to_owned())
    ]
  );
  assert!(!modes.holds_tree("a") && !files.holds_tree("a") && !attributes.holds_tree("a"));
  assert!(modes.holds_tree("ab") && files.holds_tree("") && attributes.holds_tree("b"));
}

/// Every path the generated histories name: up to three names deep over three names, one holding a NUL, so a copy
/// meets prefixes of one another, escaped bytes and paths beneath themselves; the root first, for copies only.
fn paths() -> Vec<String> {
  let names = ["a", "b", "a\0"];
  let mut paths: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
  for _ in 0..2 {
    let deeper: Vec<String> = paths
      .iter()
      .flat_map(|path| names.iter().map(move |name| format!("{path}/{name}")))
      .collect();
    paths.extend(deeper);
  }
  paths.sort();
  paths.dedup();
  paths.insert(0, String::new());
  paths
}

/// One generated step.
#[derive(Clone, Debug)]
enum Step {
  /// Put a file, a mode and an attribute at a path.
  Put { path: usize, value: u8 },
  /// Take a path's file, mode and attributes away.
  Take { path: usize },
  /// Copy a tree, in every map.
  Copy { from: usize, to: usize },
  /// Fork every map; the steps after it change the fork alone.
  Fork,
}

/// Any step, copies and puts the most often. The union is built as `prop_oneof!` builds one of more than ten arms,
/// from boxed strategies: its shorter forms hold their arms in `Arc`s (R2).
fn step() -> impl Strategy<Value = Step> {
  Union::new_weighted(vec![
    (
      3,
      (any::<usize>(), any::<u8>())
        .prop_map(|(path, value)| Step::Put { path, value })
        .boxed(),
    ),
    (
      1,
      any::<usize>().prop_map(|path| Step::Take { path }).boxed(),
    ),
    (
      3,
      (any::<usize>(), any::<usize>())
        .prop_map(|(from, to)| Step::Copy { from, to })
        .boxed(),
    ),
    (1, Just(Step::Fork).boxed()),
  ])
}

/// What the generated histories met, so the oracle can show it compared real copies.
#[derive(Clone, Copy, Debug, Default)]
struct Census {
  /// Copies that grafted keys beneath their destination.
  grafted: usize,
  /// Copies into their own source's tree that grafted keys.
  into_themselves: usize,
  /// Copies of the root's tree that grafted keys (A-586).
  from_root: usize,
  /// Copies refused as occupied, and as the root.
  occupied: usize,
  root: usize,
  /// Histories that forked.
  forked: usize,
}

/// The three maps of one history and their references, and the originals once forked.
#[derive(Default)]
struct Maps {
  files: Files,
  modes: Dim<u32>,
  attributes: Xattrs,
  file_model: BTreeMap<String, Vec<u8>>,
  mode_model: BTreeMap<String, u32>,
  attribute_model: BTreeMap<(String, String), Vec<u8>>,
}

/// An attribute name a value picks: two names, so a path holds one or both.
fn name(value: u8) -> &'static str {
  if value.is_multiple_of(2) { "x" } else { "y" }
}

impl Maps {
  /// Applies one step to every map and its reference; a copy's answers must agree with the references'.
  fn apply(
    &mut self,
    step: &Step,
    paths: &[String],
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let at = |index: usize| paths[index % paths.len()].clone();
    match *step {
      Step::Put { path, value } => {
        // Puts name any path, the root among them, so a copy of the root carries the root's own key (A-586).
        let path = at(path);
        self.files.insert(&path, vec![value]);
        self.file_model.insert(path.clone(), vec![value]);
        self.modes.insert(&path, u32::from(value));
        self.mode_model.insert(path.clone(), u32::from(value));
        self.attributes.insert(&path, name(value), vec![value]);
        self
          .attribute_model
          .insert((path, name(value).to_owned()), vec![value]);
      }
      Step::Take { path } => self.take(&at(path)),
      Step::Copy { from, to } => self.copy(&at(from), &at(to), census)?,
      Step::Fork => {}
    }
    Ok(())
  }

  /// Takes a path's file, mode and attributes away, in the maps and the references.
  fn take(&mut self, path: &str) {
    self.files.remove(path);
    self.file_model.remove(path);
    self.modes.remove(path);
    self.mode_model.remove(path);
    for name in ["x", "y"] {
      self.attributes.remove(path, name);
      self
        .attribute_model
        .remove(&(path.to_owned(), name.to_owned()));
    }
  }

  /// Copies a tree in every map and its reference, and counts what it met.
  fn copy(&mut self, from: &str, to: &str, census: &mut Census) -> Result<(), TestCaseError> {
    let files = self.files.copy_tree(from, to);
    prop_assert_eq!(files, copied_by_hand(&mut self.file_model, from, to));
    prop_assert_eq!(
      self.modes.copy_tree(from, to),
      copied_by_hand(&mut self.mode_model, from, to)
    );
    prop_assert_eq!(
      self.attributes.copy_tree(from, to),
      attributes_copied_by_hand(&mut self.attribute_model, from, to)
    );
    match files {
      Ok(copied) if copied.beneath => {
        census.grafted += 1;
        census.into_themselves += usize::from(in_tree(to, from));
        census.from_root += usize::from(from.is_empty());
      }
      Err(CopyRefused::Occupied) => census.occupied += 1,
      Err(CopyRefused::Root) => census.root += 1,
      Ok(_) => {}
    }
    Ok(())
  }

  /// Holds every map equal to its reference.
  fn check(&self) -> Result<(), TestCaseError> {
    let files: BTreeMap<String, Vec<u8>> = self
      .files
      .iter()
      .map(|(key, bytes)| (key, bytes.to_vec()))
      .collect();
    prop_assert_eq!(&files, &self.file_model);
    let modes: BTreeMap<String, u32> = self
      .modes
      .iter()
      .map(|(key, value)| (key, *value))
      .collect();
    prop_assert_eq!(&modes, &self.mode_model);
    let attributes: BTreeMap<(String, String), Vec<u8>> = self
      .attributes
      .iter()
      .map(|(key, value)| (key, value.clone()))
      .collect();
    prop_assert_eq!(&attributes, &self.attribute_model);
    Ok(())
  }

  /// A fork of every map, holding the arenas (each map's fork leaves its arena here, so it is lent across), and of
  /// the references.
  fn fork(&mut self) -> Maps {
    let mut fork = Maps {
      files: self.files.fork(),
      modes: self.modes.fork(),
      attributes: self.attributes.fork(),
      file_model: self.file_model.clone(),
      mode_model: self.mode_model.clone(),
      attribute_model: self.attribute_model.clone(),
    };
    self.lend(&mut fork);
    fork
  }

  /// Gives every map's arena to `other`'s maps.
  fn lend(&mut self, other: &mut Maps) {
    self.files.lend(&mut other.files);
    self.modes.lend(&mut other.modes);
    self.attributes.lend(&mut other.attributes);
  }

  /// Releases every map's hold on its nodes and blocks, a node a step in each map's own arena (A-575: the stepped
  /// release is the only one).
  fn release_all(&mut self) {
    let mut releasing = self.files.releasing();
    while self
      .files
      .map
      .nodes
      .release_step(&mut releasing, &mut self.files.blocks)
    {}
    let mut releasing = self.modes.releasing();
    while self.modes.nodes.release_step(&mut releasing, &mut Plain) {}
    let mut releasing = self.attributes.releasing();
    while self
      .attributes
      .map
      .nodes
      .release_step(&mut releasing, &mut Plain)
    {}
  }

  /// Whether the arenas this value holds have nothing left alive and named no missing node.
  fn emptied(&self) -> bool {
    self.files.map.nodes.live() == 0
      && self.files.blocks.live() == 0
      && self.modes.nodes.live() == 0
      && self.attributes.map.nodes.live() == 0
      && self.files.map.nodes.missing() == 0
      && self.modes.nodes.missing() == 0
      && self.attributes.map.nodes.missing() == 0
  }
}

/// Runs one history: the steps before the first fork on the maps, the rest on their fork, each step checked; then the
/// fork checked, released and its arenas given back, the originals checked against the references as they were at the
/// fork, and released; every arena then empty.
fn run(steps: &[Step], census: &mut Census) -> Result<(), TestCaseError> {
  let paths = paths();
  let mut maps = Maps::default();
  let mut fork: Option<Maps> = None;
  for step in steps {
    if matches!(step, Step::Fork) && fork.is_none() {
      census.forked += 1;
      fork = Some(maps.fork());
      continue;
    }
    let current = fork.as_mut().unwrap_or(&mut maps);
    current.apply(step, &paths, census)?;
    current.check()?;
  }
  if let Some(mut fork) = fork {
    fork.release_all();
    fork.lend(&mut maps);
  }
  maps.check()?;
  maps.release_all();
  prop_assert!(maps.emptied(), "a node, block or link outlived every map");
  Ok(())
}

/// A-542 (part 1, the oracle). Do: generated histories of puts, removals and copies (from and onto any path, the root
/// among them, into the source's own tree among them) on a file map, a mode map and an attribute map, forked at a
/// generated step with the rest of the steps changing the fork alone. Expect: after every step each map equals a
/// reference that copies key by key, every copy's answer (what it brought, or why it was refused) equals the
/// reference's, the originals after the fork's steps equal the references as they were at the fork (a graft changes no
/// node another map holds), and once every map is released no node, block or link is left. The census shows copies
/// that grafted, copies into their own source, both refusals and forks were met.
#[test]
fn every_map_copies_a_tree_as_a_reference_that_copies_each_key() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let result = runner.run(&prop::collection::vec(step(), 1..48), |steps| {
    run(&steps, &mut census.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  let met = [
    census.grafted,
    census.into_themselves,
    census.from_root,
    census.occupied,
    census.root,
    census.forked,
  ];
  assert!(
    met.iter().all(|&count| count > 0),
    "a case never met: {census:?}"
  );
}

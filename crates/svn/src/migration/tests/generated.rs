//! Generated histories: revisions of additions, changes of texts and properties, deletions, copies from any earlier
//! revision and replacements, written as a dump record by record the way `svnadmin dump` writes them (an addition's
//! whole property list and text, a copy's sections only where they differ from its source, every change a real
//! one), imported, and held to the model tree they were made from in every revision; then exported, imported again
//! and exported again.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::support::{
  Dump, Node, VECTOR_UUID, created, export, export_all, history, import, properties,
  revision_properties,
};
use crate::dump::Format;
use crate::index::NodeKind;
use crate::migration::paths::{absolute, join};
use crate::migration::{Properties, Repository};

/// The names a generated node takes: plain, with a dot, a space and a colon.
const NAMES: [&str; 5] = ["a", "b.txt", "c d", "dir", "e:f"];

/// The property names a generated list takes, `svn:mergeinfo` among them.
const PROPERTY_NAMES: [&str; 3] = ["p", "svn:mergeinfo", "x:y"];

/// One node of the model.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelNode {
  /// Its kind.
  kind: NodeKind,
  /// Its text, for a file.
  text: Vec<u8>,
  /// Its properties.
  properties: Properties,
}

/// A revision's tree: every node by relative path, the root's empty.
type Tree = BTreeMap<String, ModelNode>;

/// One generated step before the model shapes it.
#[derive(Clone, Debug)]
struct Step {
  /// What it does.
  selector: u8,
  /// Which node or directory it picks first.
  first: u16,
  /// Which revision or node it picks second.
  second: u16,
  /// Which name it gives.
  name: u8,
  /// The bytes of a text it sets.
  bytes: Vec<u8>,
  /// The properties it sets, by name.
  properties: Vec<(u8, Vec<u8>)>,
}

/// A step.
fn step() -> impl Strategy<Value = Step> {
  (
    any::<u8>(),
    any::<u16>(),
    any::<u16>(),
    any::<u8>(),
    prop::collection::vec(any::<u8>(), 0..40),
    prop::collection::vec(
      (any::<u8>(), prop::collection::vec(any::<u8>(), 0..6)),
      0..3,
    ),
  )
    .prop_map(|(selector, first, second, name, bytes, properties)| Step {
      selector,
      first,
      second,
      name,
      bytes,
      properties,
    })
}

/// The model of a history being generated, and the dump that makes it.
struct Model {
  /// Every committed revision's tree.
  trees: Vec<Tree>,
  /// The tree of the revision being made.
  working: Tree,
  /// The paths the revision being made has a record of.
  touched: BTreeSet<String>,
  /// The dump.
  dump: Dump,
}

impl Model {
  /// A model at revision 0: the root alone.
  fn new() -> Self {
    let root = ModelNode {
      kind: NodeKind::Directory,
      text: Vec::new(),
      properties: Properties::new(),
    };
    let tree = Tree::from([(String::new(), root)]);
    let mut dump = Dump::new(Format::Two, Some(VECTOR_UUID));
    dump.revision(
      0,
      &properties(&[("svn:date", "2005-04-07T22:13:00.000000Z")]),
    );
    Self {
      trees: vec![tree.clone()],
      working: tree,
      touched: BTreeSet::new(),
      dump,
    }
  }

  /// Makes the next revision of `steps`.
  fn revision(&mut self, steps: &[Step]) {
    let number = self.trees.len() as u64;
    self.dump.revision(number, &revision_properties(number));
    self.touched.clear();
    for step in steps {
      match step.selector % 7 {
        0 => self.add(step, NodeKind::File),
        1 => self.add(step, NodeKind::Directory),
        2 => self.change_text(step),
        3 => self.change_properties(step),
        4 => self.delete(step),
        5 => self.copy(step),
        _ => self.replace(step),
      }
    }
    self.trees.push(self.working.clone());
  }

  /// The working tree's paths of `kind`, or of any kind, the root first.
  fn paths(&self, kind: Option<NodeKind>) -> Vec<String> {
    self
      .working
      .iter()
      .filter(|(_, node)| kind.is_none_or(|kind| node.kind == kind))
      .map(|(path, _)| path.clone())
      .collect()
  }

  /// A path no record of this revision names yet, of `kind` or any kind, but the root when `root` is false.
  fn untouched(&self, kind: Option<NodeKind>, root: bool, pick: u16) -> Option<String> {
    let paths: Vec<String> = self
      .paths(kind)
      .into_iter()
      .filter(|path| (root || !path.is_empty()) && !self.touched.contains(path))
      .collect();
    pick_from(&paths, pick).cloned()
  }

  /// A free path beneath a directory: the directory `pick` picks, the name `name` names.
  fn free_path(&self, pick: u16, name: u8) -> Option<String> {
    let directories = self.paths(Some(NodeKind::Directory));
    let directory = pick_from(&directories, pick)?;
    let path = join(directory, NAMES[usize::from(name) % NAMES.len()]);
    (!self.working.contains_key(&path)).then_some(path)
  }

  /// Adds a new file or directory with its whole property list and text, as `svnadmin` writes an addition.
  fn add(&mut self, step: &Step, kind: NodeKind) {
    let Some(path) = self.free_path(step.first, step.name) else {
      return;
    };
    let node = new_node(kind, step);
    self
      .dump
      .node(&written_whole(Node::add(&path, kind), &node));
    self.working.insert(path.clone(), node);
    self.touched.insert(path);
  }

  /// Changes a file's text to other bytes.
  fn change_text(&mut self, step: &Step) {
    let Some(path) = self.untouched(Some(NodeKind::File), false, step.first) else {
      return;
    };
    let node = self.working.get_mut(&path).expect("a working node");
    let mut text = step.bytes.clone();
    if text == node.text {
      text.push(b'!');
    }
    self.dump.node(&Node::change(&path).text(&text));
    node.text = text;
    self.touched.insert(path);
  }

  /// Changes a node's properties to another list.
  fn change_properties(&mut self, step: &Step) {
    let Some(path) = self.untouched(None, true, step.first) else {
      return;
    };
    let node = self.working.get_mut(&path).expect("a working node");
    let mut list = generated_properties(step);
    if list == node.properties {
      toggle(&mut list);
    }
    self
      .dump
      .node(&Node::change(&path).properties(list.clone()));
    node.properties = list;
    self.touched.insert(path);
  }

  /// Deletes a node and everything beneath it.
  fn delete(&mut self, step: &Step) {
    let Some(path) = self.untouched(None, false, step.first) else {
      return;
    };
    self.dump.node(&Node::delete(&path));
    self.remove(&path);
    self.touched.insert(path);
  }

  /// Copies a node of an earlier revision to a free path, its text and properties changed where the step differs; a
  /// path this revision deleted is left to [`Model::replace`], which keeps a copy off the node it would replace.
  fn copy(&mut self, step: &Step) {
    let Some(path) = self
      .free_path(step.first, step.name)
      .filter(|path| !self.touched.contains(path))
    else {
      return;
    };
    let Some((revision, source)) = self.copy_source(step) else {
      return;
    };
    let record = Node::add(&path, source.1.kind).copied(revision as u64, &source.0);
    let (record, node) = changed_copy(record, &source.1, step);
    self.dump.node(&record);
    self.place_copy(&path, revision, &source.0, node);
    self.touched.insert(path);
  }

  /// Replaces a node by a new one or by a copy, unless the copy is of the very node it would replace.
  fn replace(&mut self, step: &Step) {
    let Some(path) = self.untouched(None, false, step.first) else {
      return;
    };
    if step.name % 2 == 0 {
      let kind = if step.bytes.len() % 2 == 0 {
        NodeKind::File
      } else {
        NodeKind::Directory
      };
      let node = new_node(kind, step);
      self
        .dump
        .node(&written_whole(Node::replace(&path, kind), &node));
      self.remove(&path);
      self.working.insert(path.clone(), node);
    } else {
      let Some((revision, source)) = self.copy_source(step) else {
        return;
      };
      if self.working.get(&path) == Some(&source.1) {
        return;
      }
      let record = Node::replace(&path, source.1.kind).copied(revision as u64, &source.0);
      let (record, node) = changed_copy(record, &source.1, step);
      self.dump.node(&record);
      self.remove(&path);
      self.place_copy(&path, revision, &source.0, node);
    }
    self.touched.insert(path);
  }

  /// A node of an earlier revision other than its root, with its path and revision.
  fn copy_source(&self, step: &Step) -> Option<(usize, (String, ModelNode))> {
    let revision = usize::from(step.second) % self.trees.len();
    let tree = &self.trees[revision];
    let candidates: Vec<(&String, &ModelNode)> =
      tree.iter().filter(|(path, _)| !path.is_empty()).collect();
    let (path, node) = pick_from(&candidates, step.first.rotate_left(8))?;
    Some((revision, ((*path).clone(), (*node).clone())))
  }

  /// Places a copy of `source` at `revision` at `path`, its own node `node`, everything beneath the source beneath it.
  fn place_copy(&mut self, path: &str, revision: usize, source: &str, node: ModelNode) {
    let prefix = format!("{source}/");
    let beneath: Vec<(String, ModelNode)> = self.trees[revision]
      .range(prefix.clone()..)
      .take_while(|(child, _)| child.starts_with(&prefix))
      .filter_map(|(child, child_node)| {
        let rest = child.strip_prefix(&prefix)?;
        Some((format!("{path}/{rest}"), child_node.clone()))
      })
      .collect();
    self.working.insert(path.to_owned(), node);
    self.working.extend(beneath);
  }

  /// Removes `path` and everything beneath it from the working tree.
  fn remove(&mut self, path: &str) {
    let prefix = format!("{path}/");
    self
      .working
      .retain(|child, _| child != path && !child.starts_with(&prefix));
  }
}

/// The item of `items` that `pick` picks, if there is one.
fn pick_from<T>(items: &[T], pick: u16) -> Option<&T> {
  if items.is_empty() {
    return None;
  }
  items.get(usize::from(pick) % items.len())
}

/// The properties a step sets.
fn generated_properties(step: &Step) -> Properties {
  step
    .properties
    .iter()
    .map(|(name, value)| {
      let name = PROPERTY_NAMES[usize::from(*name) % PROPERTY_NAMES.len()];
      (name.as_bytes().to_vec(), value.clone())
    })
    .collect()
}

/// `list` made different from what it was: `z` added, or taken away when it is there.
fn toggle(list: &mut Properties) {
  if list.remove(b"z".as_slice()).is_none() {
    list.insert(b"z".to_vec(), b"1".to_vec());
  }
}

/// A new node of `kind` made by `step`.
fn new_node(kind: NodeKind, step: &Step) -> ModelNode {
  ModelNode {
    kind,
    text: if kind == NodeKind::File {
      step.bytes.clone()
    } else {
      Vec::new()
    },
    properties: generated_properties(step),
  }
}

/// `record` with `node`'s whole property list and, for a file, its whole text.
fn written_whole(record: Node, node: &ModelNode) -> Node {
  let record = record.properties(node.properties.clone());
  if node.kind == NodeKind::File {
    record.text(&node.text)
  } else {
    record
  }
}

/// A copy's record with the sections where `step` makes it differ from `source`, and the node it leaves.
fn changed_copy(mut record: Node, source: &ModelNode, step: &Step) -> (Node, ModelNode) {
  let mut node = source.clone();
  if source.kind == NodeKind::File && !step.bytes.is_empty() && step.bytes != source.text {
    record = record.text(&step.bytes);
    node.text = step.bytes.clone();
  }
  let list = generated_properties(step);
  if !step.properties.is_empty() && list != source.properties {
    record = record.properties(list.clone());
    node.properties = list;
  }
  (record, node)
}

/// A failed case naming `refusal`.
fn failed(refusal: impl std::fmt::Debug) -> TestCaseError {
  TestCaseError::fail(format!("{refusal:?}"))
}

/// Holds `repository` to the model's trees: every path any revision held present exactly where its tree holds it,
/// of its kind, with its text and its properties.
fn holds_the_model(repository: &Repository, trees: &[Tree]) -> Result<(), TestCaseError> {
  let every_path: BTreeSet<&String> = trees.iter().flat_map(Tree::keys).collect();
  for (revision, tree) in trees.iter().enumerate() {
    for path in &every_path {
      holds_the_node(repository, revision as u64, path, tree.get(*path))?;
    }
  }
  Ok(())
}

/// Holds the node at `path` in `revision` to the model's `expected`.
fn holds_the_node(
  repository: &Repository,
  revision: u64,
  path: &str,
  expected: Option<&ModelNode>,
) -> Result<(), TestCaseError> {
  let found = repository
    .index()
    .node(&absolute(path), revision)
    .map_err(failed)?;
  prop_assert_eq!(
    found.is_some(),
    expected.is_some(),
    "r{} {}",
    revision,
    path
  );
  let (Some(found), Some(expected)) = (found, expected) else {
    return Ok(());
  };
  let text = found
    .text()
    .and_then(|text| repository.content(&text.identity))
    .unwrap_or_default();
  let held = repository.properties(found.properties()).map_err(failed)?;
  prop_assert_eq!(
    (found.kind(), text, &held),
    (
      expected.kind,
      expected.text.as_slice(),
      &expected.properties
    ),
    "r{} {}",
    revision,
    path
  );
  Ok(())
}

/// Holds `repository`'s export to import into a new repository as the same history and to export again to the same
/// bytes.
fn round_trips(repository: &Repository, piece: usize) -> Result<(), TestCaseError> {
  let exported = export_all(repository).map_err(failed)?;
  let mut again = created();
  import(&mut again, &exported, piece).map_err(failed)?;
  prop_assert_eq!(history(&again), history(repository));
  prop_assert_eq!(export_all(&again).map_err(failed)?, exported);
  Ok(())
}

/// Holds `repository`'s history exported in two parts, split after revision `split` picks, and imported part after
/// part to be the same history.
fn round_trips_in_parts(
  repository: &Repository,
  split: u16,
  piece: usize,
) -> Result<(), TestCaseError> {
  let youngest = repository.youngest();
  let middle = u64::from(split) % youngest;
  let first = export(repository, 0, middle).map_err(failed)?;
  let second = export(repository, middle + 1, youngest).map_err(failed)?;
  let mut parts = created();
  import(&mut parts, &first, piece).map_err(failed)?;
  import(&mut parts, &second, piece).map_err(failed)?;
  prop_assert_eq!(history(&parts), history(repository));
  Ok(())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-628 (generated): do generate a history of up to seven revisions of up to four steps each (additions, text and
  /// property changes, deletions, copies from any earlier revision with and without changes, replacements by new nodes
  /// and by copies), write it as a dump and import it, a byte at a time or in pieces. Expect every revision to hold
  /// the model's tree, path by path; the repository's export to import into a new repository as the same history,
  /// transaction names, contents and mergeinfo flags included; the export of that to be the same bytes; and the
  /// history exported in two parts and imported part after part to be the same history again.
  #[test]
  fn a_generated_history_imports_to_its_model_and_round_trips_through_its_export(
    revisions in prop::collection::vec(prop::collection::vec(step(), 1..5), 1..8),
    piece in 1usize..64,
    split in any::<u16>(),
  ) {
    let mut model = Model::new();
    for steps in &revisions {
      model.revision(steps);
    }
    let mut repository = created();
    import(&mut repository, &model.dump.bytes(), piece).map_err(failed)?;
    holds_the_model(&repository, &model.trees)?;
    round_trips(&repository, piece)?;
    round_trips_in_parts(&repository, split, piece)?;
  }
}

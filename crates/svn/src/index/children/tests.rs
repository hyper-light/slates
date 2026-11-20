//! The child map's tests (A-759): an ordered map of names is the oracle, over generated runs of sets and removals,
//! every map made along the way read again whole afterwards (a map is persistent: a later change never touches it).

use super::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

/// A store of its own: the nodes pushed, the first a revision being built made, and paths as `/d/<name>`.
#[derive(Default)]
struct Store {
  /// The nodes.
  nodes: Vec<MapNode>,
  /// The nodes committed: those before this one; the rest are the revision's own.
  committed: usize,
  /// The paths, by number.
  paths: Vec<String>,
}

impl MapRead for Store {
  fn map_node(&self, handle: u32) -> Option<&MapNode> {
    self.nodes.get(handle as usize)
  }
  fn key_path(&self, key: usize) -> Option<&str> {
    self.paths.get(key).map(String::as_str)
  }
}

impl MapWrite for Store {
  fn push_map_node(&mut self, node: MapNode) -> Option<u32> {
    self.nodes.push(node);
    u32::try_from(self.nodes.len() - 1).ok()
  }
  fn staged(&self, handle: u32) -> bool {
    handle as usize >= self.committed && (handle as usize) < self.nodes.len()
  }
  fn map_node_mut(&mut self, handle: u32) -> Option<&mut MapNode> {
    self.nodes.get_mut(handle as usize)
  }
}

impl Store {
  /// The path number of `/<directory>/<name>`, interned.
  fn key(&mut self, directory: &str, name: &str) -> usize {
    let path = format!("/{directory}/{name}");
    match self.paths.iter().position(|held| *held == path) {
      Some(at) => at,
      None => {
        self.paths.push(path);
        self.paths.len() - 1
      }
    }
  }

  /// The map of `root` as names and ids, read `most` at a time through the cursor ([`entries_after`]).
  fn read_by_cursor(&self, root: u32, most: usize) -> Vec<(String, usize)> {
    let mut read: Vec<(String, usize)> = Vec::new();
    loop {
      let after = read.last().map(|(name, _)| name.clone());
      let (part, more) = entries_after(self, root, after.as_deref(), most).unwrap();
      assert!(part.len() <= most);
      read.extend(
        part
          .into_iter()
          .map(|(key, id)| (self.key_name(key as u32).unwrap().to_owned(), id.0)),
      );
      if !more {
        return read;
      }
    }
  }

  /// The map of `root` as names and ids.
  fn read(&self, root: u32) -> Vec<(String, usize)> {
    entries(self, root)
      .unwrap()
      .into_iter()
      .map(|(key, id)| (self.key_name(key as u32).unwrap().to_owned(), id.0))
      .collect()
  }
}

/// One change to a map.
#[derive(Clone, Debug)]
enum Change {
  /// A name set to an id, under one of two directories (a copy's source path or its own).
  Set(u16, bool, usize),
  /// A name removed.
  Remove(u16),
}

/// A name of the run: a few thousand, so maps grow several levels deep.
fn name(number: u16) -> String {
  format!("n{number:05}")
}

/// Applies `changes` to an empty map and to the model, committing (a revision's end) after every `batch` of them;
/// the map is the model's after every change, and every committed map reads again at the end as it did then.
fn run(changes: &[Change], batch: usize) {
  let mut store = Store::default();
  let mut root = EMPTY;
  let mut model: BTreeMap<String, usize> = BTreeMap::new();
  let mut committed = Vec::new();
  for (at, change) in changes.iter().enumerate() {
    match *change {
      Change::Set(number, copy, id) => {
        let key = store.key(if copy { "source" } else { "own" }, &name(number));
        root = set(&mut store, root, key, NodeRevisionId(id)).unwrap();
        model.insert(name(number), id);
      }
      Change::Remove(number) => {
        root = remove(&mut store, root, &name(number)).unwrap();
        model.remove(&name(number));
      }
    }
    let check = at % CHECK_EVERY == 0 || at + 1 == changes.len();
    if check {
      assert_eq!(
        store.read(root),
        model.clone().into_iter().collect::<Vec<_>>()
      );
      assert_eq!(
        store.read_by_cursor(root, 1 + at % 13),
        model.clone().into_iter().collect::<Vec<_>>()
      );
    }
    if (at + 1) % batch == 0 {
      store.committed = store.nodes.len();
      if check || committed.len() < CHECK_EVERY {
        committed.push((root, model.clone()));
      }
    }
  }
  for (root, model) in committed {
    assert_eq!(
      store.read(root),
      model.into_iter().collect::<Vec<_>>(),
      "a committed map changed"
    );
  }
}

/// How often a run's map is read against the model: every 97th change, and the last (each read is the whole map).
const CHECK_EVERY: usize = 97;

/// A-759. Do: set 5,000 names in shuffled order (a map four levels deep at most), replace every third under the other
/// directory's path, then remove all but a few. Expect: after every change the map is the model's, in name order, and
/// every map made before reads as it did.
#[test]
fn a_large_map_stays_the_models_through_sets_replacements_and_removals() {
  let mut changes = Vec::new();
  let mut order: Vec<u16> = (0..5_000).collect();
  order.sort_by_key(|number| (u64::from(*number) * 2_654_435_761) % 5_003);
  for (at, number) in order.iter().enumerate() {
    changes.push(Change::Set(*number, false, at));
  }
  for number in (0..5_000).step_by(3) {
    changes.push(Change::Set(number, true, 100_000 + usize::from(number)));
  }
  for number in order.iter().skip(7) {
    changes.push(Change::Remove(*number));
  }
  changes.push(Change::Remove(9_999));
  for batch in [1, 7, 50] {
    run(&changes, batch);
  }
}

/// A-759. Do: a set of 33 names (one split), removals down to none, a removal from the empty map. Expect: the model's
/// map each time, empty at the end.
#[test]
fn a_map_splits_and_empties() {
  let mut changes: Vec<Change> = (0..33)
    .map(|number| Change::Set(number, false, usize::from(number)))
    .collect();
  changes.extend((0..33).map(Change::Remove));
  changes.push(Change::Remove(0));
  for batch in [1, 5] {
    run(&changes, batch);
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-759. Do: a generated run of sets (under either directory) and removals over 200 names. Expect: the model's map
  /// after every change, and every map made before unchanged.
  #[test]
  fn a_generated_run_is_the_models(
    changes in proptest::collection::vec(
      prop_oneof![
        (0u16..200, any::<bool>(), 0usize..1_000).prop_map(|(n, c, id)| Change::Set(n, c, id)),
        (0u16..200).prop_map(Change::Remove),
      ],
      0..400,
    ),
    batch in 1usize..20,
  ) {
    run(&changes, batch);
  }
}

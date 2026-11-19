//! Golden and hostile tests of the import of an existing store (A-630 parts 2 and 3): one history stock Mercurial 7.2
//! stored five ways (`fixtures/store`'s dotencode and hybrid stores, the dotencode store under a pre-share-safe
//! `.hg/requires`, and `fixtures/import`'s revlog version 2 store and version 1 file index store), each imported through
//! the read seam with every text 7.2's `debugdata` and every node 7.2's, and the five indexes equal row for row; then
//! damaged and hostile stores refused by kind with nothing left held.

use std::collections::{BTreeMap, BTreeSet};

use slates_vcs::{ContentId, ContentStore};

use crate::fixtures::{import as import_fixtures, store as store_fixtures};
use crate::import::{FileRefused, ImportRefused, Imported, RepositoryFiles, import};
use crate::index::{Log, NONE, RevisionRow};
use crate::lines::lines;
use crate::revlog;
use crate::store::RequiresRefused;
use crate::{Admission, NULL_NODE, changeset, filerev, node};
use proptest::prelude::*;

/// The five repositories: `fixtures/store`'s two stores, the first under a pre-share-safe `.hg/requires`, and
/// `fixtures/import`'s two.
const REPOSITORIES: [&str; 5] = ["dot", "hybrid", "legacy", "v2", "fileindex"];

/// The window the tests' stores are made with: room for a few rows a table.
const WINDOW: usize = 1 << 12;

/// A store with room for the history many times over.
fn store() -> ContentStore {
  ContentStore::new(1 << 24, WINDOW).unwrap()
}

/// A room for the whole import many times over.
fn room() -> Admission {
  Admission::new(1 << 24)
}

/// The host's refusal the tests make: a fetch still pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending;

/// A repository's `.hg`: each file's path and bytes, every path the import asked for, and a path whose fetch is pending.
#[derive(Clone, Debug, Default)]
struct Repository {
  files: BTreeMap<Vec<u8>, Vec<u8>>,
  asked: Vec<Vec<u8>>,
  pending: Option<Vec<u8>>,
}

impl RepositoryFiles for Repository {
  type Refused = Pending;

  fn read(
    &mut self,
    path: &[&[u8]],
    room: &mut Admission,
  ) -> Result<Option<Vec<u8>>, FileRefused<Pending>> {
    let joined = path.join(&b'/');
    self.asked.push(joined.clone());
    if self.pending.as_ref() == Some(&joined) {
      return Err(FileRefused::Host(Pending));
    }
    let Some(bytes) = self.files.get(&joined) else {
      return Ok(None);
    };
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    room.charge(len).map_err(FileRefused::PastAdmission)?;
    Ok(Some(bytes.clone()))
  }
}

/// A fixture file of `fixtures/store` or of `fixtures/import`.
fn fixture(from_store: bool, name: &str) -> &'static [u8] {
  if from_store {
    store_fixtures::file(name)
  } else {
    import_fixtures::file(name)
  }
}

/// The `.hg` of one of the five repositories: its `.hg/requires` from `fixtures/import`, its store from the fixtures
/// that hold it (the pre-share-safe one's is the dotencode store without its own requirements).
fn repository(name: &str) -> Repository {
  let mut files = BTreeMap::new();
  let requires = import_fixtures::file(&format!("{name}-hg-requires.txt"));
  files.insert(b"requires".to_vec(), requires.to_vec());
  let (from_store, stored) = match name {
    "legacy" => (true, "dot"),
    "dot" | "hybrid" => (true, name),
    _ => (false, name),
  };
  for line in lines(fixture(from_store, &format!("{stored}-files.txt"))) {
    let file = core::str::from_utf8(line).unwrap();
    if name != "legacy" || file != "requires" {
      let bytes = fixture(from_store, &format!("{stored}/{file}"));
      files.insert(format!("store/{file}").into_bytes(), bytes.to_vec());
    }
  }
  Repository {
    files,
    ..Repository::default()
  }
}

/// The texts 7.2's `debugdata` wrote of one log of the history, `<prefix>-<rev>.txt`, in order.
fn texts(prefix: &str) -> Vec<&'static [u8]> {
  (0..)
    .map_while(|rev| store_fixtures::find(&format!("{prefix}-{rev}.txt")))
    .collect()
}

/// Every path the history tracked, in `dot-paths.txt`'s order, which is the `f<n>` texts' order.
fn paths() -> Vec<Vec<u8>> {
  lines(store_fixtures::file("dot-paths.txt"))
    .map(|hex| {
      hex
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| crate::quote::hex_pair(pair).unwrap())
        .collect()
    })
    .collect()
}

/// A content's bytes, read whole through its windows.
fn whole(store: &ContentStore, root: &ContentId) -> Vec<u8> {
  store.windows(root).flatten().copied().collect()
}

/// A manifest's or a file revision's text read back: the block between its markers, when it has one, then the bytes.
fn text_of(store: &ContentStore, row: &RevisionRow) -> Vec<u8> {
  let bytes = whole(store, &row.bytes);
  match row.block {
    None => bytes,
    Some(block) => [
      filerev::MARKER,
      &whole(store, &block),
      filerev::MARKER,
      &bytes,
    ]
    .concat(),
  }
}

/// Holds the changelog's rows to 7.2's texts: each text 7.2's, its node computed from its parents and its text, its
/// manifest the one its text names.
fn check_changelog(imported: &Imported, store: &ContentStore) {
  let texts = texts("00changelog");
  assert_eq!(usize::try_from(imported.changesets).unwrap(), texts.len());
  let node_of = |rev: u32| match rev {
    NONE => NULL_NODE,
    rev => imported.index.changeset(store, rev).unwrap().node,
  };
  for (rev, text) in (0_u32..).zip(&texts) {
    let row = imported.index.changeset(store, rev).unwrap();
    assert_eq!(
      store.object_bytes(&row.text, row.len).unwrap(),
      *text,
      "changeset {rev}"
    );
    let computed = node::compute(&node_of(row.p1), &node_of(row.p2), text).unwrap();
    assert_eq!(computed, row.node, "changeset {rev}");
    assert_eq!(
      changeset::decode(text).unwrap().manifest_node().unwrap(),
      row.manifest
    );
  }
}

/// Holds a manifest or file log's rows to 7.2's texts: each text 7.2's, its node computed from its parents and text.
fn check_log(imported: &Imported, store: &ContentStore, log: Log<'_>, texts: &[&[u8]]) {
  let len = imported.index.len(store, log);
  assert_eq!(usize::try_from(len).unwrap(), texts.len(), "{log:?}");
  let node_of = |rev: u32| match rev {
    NONE => NULL_NODE,
    rev => imported.index.revision(store, log, rev).unwrap().node,
  };
  for (rev, text) in (0_u32..).zip(texts) {
    let row = imported.index.revision(store, log, rev).unwrap();
    assert_eq!(text_of(store, &row), *text, "{log:?} {rev}");
    let computed = node::compute(&node_of(row.p1), &node_of(row.p2), text).unwrap();
    assert_eq!(computed, row.node, "{log:?} {rev}");
  }
}

/// Holds a whole import to the history 7.2 wrote: every log's texts and nodes, and its counts.
fn check_history(imported: &Imported, store: &ContentStore) {
  check_changelog(imported, store);
  check_log(imported, store, Log::Manifest, &texts("00manifest"));
  let paths = paths();
  let mut revisions = 0;
  for (n, path) in paths.iter().enumerate() {
    let texts = texts(&format!("f{n}"));
    check_log(imported, store, Log::File(path), &texts);
    revisions += texts.len();
  }
  let counts = (imported.file_logs, imported.file_revisions);
  let expected = (u64::try_from(paths.len()), u64::try_from(revisions));
  assert_eq!((Ok(counts.0), Ok(counts.1)), expected);
}

/// Holds what the import asked the host for: files the repository holds, each once.
fn check_asked(repository: &Repository) {
  let asked: BTreeSet<&Vec<u8>> = repository.asked.iter().collect();
  assert_eq!(asked.len(), repository.asked.len(), "a file asked twice");
  assert!(
    asked
      .iter()
      .all(|path| repository.files.contains_key(*path)),
    "{:?}",
    repository.asked
  );
}

/// An index's rows, each key with its body.
type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// The rows of an import's index.
fn rows(imported: &mut Imported, store: &ContentStore) -> Rows {
  imported
    .index
    .rows()
    .iter(store)
    .map(|row| (row.key().to_vec(), row.body().to_vec()))
    .collect()
}

/// A-630 parts 2 and 3 (golden): do import each of the five stores 7.2 wrote of one history through the read seam.
/// Expect every changeset's, manifest's and file revision's text 7.2's `debugdata` and its node 7.2's, a file revision's
/// block apart from its bytes, the five indexes equal row for row, and nothing asked of the host but files the
/// repository holds, each once (so the pre-share-safe repository's requirements come from `.hg/requires` alone).
#[test]
fn every_store_7_2_wrote_imports_to_the_same_index() {
  let mut first: Option<Rows> = None;
  for name in REPOSITORIES {
    let mut repository = repository(name);
    let mut store = store();
    let mut imported = import(&mut repository, &mut store, room()).unwrap();
    check_history(&imported, &store);
    check_asked(&repository);
    let rows = rows(&mut imported, &store);
    assert_eq!(first.get_or_insert_with(|| rows.clone()), &rows, "{name}");
  }
}

/// A-630 part 2: do import each kind of repository as `hg init` leaves it: its requirements and no other file. Expect
/// an empty index: no changeset, no manifest, no file log.
#[test]
fn a_new_repository_imports_empty() {
  for name in ["dot", "v2"] {
    let mut repository = repository(name);
    repository
      .files
      .retain(|path, _| path.as_slice() == b"requires" || path.as_slice() == b"store/requires");
    let imported = import(&mut repository, &mut store(), room()).unwrap();
    let counts = (imported.changesets, imported.manifests, imported.file_logs);
    assert_eq!(counts, (0, 0, 0), "{name}");
  }
}

/// Imports `repository` into a new store and holds the store to having been left as it was: nothing held, nothing
/// charged. The refusal.
fn refused(repository: &mut Repository) -> ImportRefused<Pending> {
  let mut store = store();
  let refused = import(repository, &mut store, room()).unwrap_err();
  assert_eq!((store.len(), store.used()), (0, 0), "{refused:?}");
  refused
}

/// A path as a refusal names it.
fn named(path: &str) -> Box<[u8]> {
  Box::from(path.as_bytes())
}

/// AC-HG.9, A-630 parts 2 and 3: do import stores whose fncache, file index and version 1 file index each name a file
/// log whose index file is gone. Expect each refused as missing, naming the file as stored, with nothing left held.
#[test]
fn a_listed_file_log_that_is_gone_is_refused() {
  for (name, gone) in [
    ("dot", "store/data/plain.txt.i"),
    ("v2", "store/data/plain.txt.i"),
    ("fileindex", "store/data/_f_o_o.i"),
  ] {
    let mut repository = repository(name);
    repository.files.remove(gone.as_bytes());
    let expected = ImportRefused::Missing { path: named(gone) };
    assert_eq!(refused(&mut repository), expected, "{name}");
  }
}

/// AC-HG.9, A-630 part 2 (7.2's `verify`): do import a store without its manifest log, one whose fncache lacks a file
/// log the changesets name, and one whose named file log is empty. Expect the first changeset, whose manifest is gone,
/// the path the listing lacks, and the empty log each refused by kind, with nothing left held.
#[test]
fn a_history_whose_logs_disagree_is_refused() {
  let mut no_manifests = repository("dot");
  no_manifests.files.remove(b"store/00manifest.i".as_slice());
  let refusal = refused(&mut no_manifests);
  assert!(
    matches!(refusal, ImportRefused::UnknownManifest { rev: 0, .. }),
    "{refusal:?}"
  );
  let mut unlisted = repository("dot");
  let kept: Vec<u8> = lines(&unlisted.files[b"store/fncache".as_slice()])
    .filter(|line| *line != b"data/plain.txt.i")
    .flat_map(|line| [line, b"\n"].concat())
    .collect();
  unlisted.files.insert(b"store/fncache".to_vec(), kept);
  let path = named("plain.txt");
  assert_eq!(refused(&mut unlisted), ImportRefused::Unlisted { path });
  let mut empty = repository("dot");
  empty
    .files
    .insert(b"store/data/plain.txt.i".to_vec(), Vec::new());
  let path = named("plain.txt");
  assert_eq!(refused(&mut empty), ImportRefused::EmptyLog { path });
}

/// AC-HG.9, A-630 parts 2 and 3: do cut a byte off a split changelog's index, an inline file log and a version 2 index
/// file. Expect each refused naming its revlog and the reader's refusal, with nothing left held.
#[test]
fn a_cut_index_is_refused_by_kind() {
  for (name, file, radix, what) in [
    ("dot", "store/00changelog.i", "00changelog", "an entry"),
    ("dot", "store/data/plain.txt.i", "data/plain.txt", "a chunk"),
    (
      "v2",
      "store/00manifest-6b8ab34b.idx",
      "00manifest",
      "an index file",
    ),
  ] {
    let mut repository = repository(name);
    repository.files.get_mut(file.as_bytes()).unwrap().pop();
    let expected = ImportRefused::Revlog {
      revlog: named(radix),
      refused: revlog::Refused::Truncated { what },
    };
    assert_eq!(refused(&mut repository), expected, "{file}");
  }
}

/// AC-HG.9, A-630 part 2: do mark an inline file log's first revision censored, ellipsis, externally stored, and with
/// a flag 7.2 does not know. Expect each refused by its kind before its text is read.
#[test]
fn revisions_whose_texts_are_not_their_nodes_are_refused_by_kind() {
  let revlog = || named("data/plain.txt");
  for (flag, expected) in [
    (
      1_u16 << 15,
      ImportRefused::Censored {
        revlog: revlog(),
        rev: 0,
      },
    ),
    (
      1 << 14,
      ImportRefused::Ellipsis {
        revlog: revlog(),
        rev: 0,
      },
    ),
    (
      1 << 13,
      ImportRefused::ExternallyStored {
        revlog: revlog(),
        rev: 0,
      },
    ),
    (
      1,
      ImportRefused::UnknownFlags {
        revlog: revlog(),
        rev: 0,
        flags: 1,
      },
    ),
  ] {
    let mut repository = repository("dot");
    let index = repository
      .files
      .get_mut(b"store/data/plain.txt.i".as_slice())
      .unwrap();
    index[6..8].copy_from_slice(&flag.to_be_bytes());
    assert_eq!(refused(&mut repository), expected);
  }
}

/// AC-HG.9, A-630 part 2: do import a repository whose store requires tree manifests, one whose store's requirements
/// are gone under share-safe, and one requiring what 7.2 does not know. Expect each refused by kind, the host asked for
/// nothing but the requirements.
#[test]
fn requirements_are_refused_before_the_store_is_read() {
  let mut tree = repository("dot");
  let requires = b"store\nfncache\ntreemanifest\n".to_vec();
  tree.files.insert(b"store/requires".to_vec(), requires);
  let requirement = named("treemanifest");
  let expected = ImportRefused::Requirements(RequiresRefused::Unsupported { requirement });
  assert_eq!(refused(&mut tree), expected);
  let mut gone = repository("dot");
  gone.files.remove(b"store/requires".as_slice());
  let path = named("store/requires");
  assert_eq!(refused(&mut gone), ImportRefused::Missing { path });
  let mut unknown = repository("legacy");
  let requires = b"store\nfncache\nfrobnicate\n".to_vec();
  unknown.files.insert(b"requires".to_vec(), requires);
  let requirement = named("frobnicate");
  let expected = ImportRefused::Requirements(RequiresRefused::Unknown { requirement });
  assert_eq!(refused(&mut unknown), expected);
  let only_requirements =
    |path: &Vec<u8>| path.as_slice() == b"requires" || path.as_slice() == b"store/requires";
  for repository in [tree, gone, unknown] {
    assert!(
      repository.asked.iter().all(only_requirements),
      "{:?}",
      repository.asked
    );
  }
}

/// The least value up to 2^24 for which `imports` holds; it holds for every greater one.
fn least(imports: impl Fn(u64) -> bool) -> u64 {
  let (mut low, mut high) = (0_u64, 1_u64 << 24);
  assert!(imports(high) && !imports(low));
  while high - low > 1 {
    let middle = low + (high - low) / 2;
    if imports(middle) {
      high = middle;
    } else {
      low = middle;
    }
  }
  high
}

/// A-630 parts 2 and 3 (non-vacuity of the bounds): do import the dotencode store and the version 2 store with the least
/// room that imports and one byte less, and with the least content budget that imports and one byte less. Expect the
/// least to import and one byte less refused by kind, the room past its admission and the budget past the store's,
/// with nothing left held.
#[test]
fn the_room_and_the_budget_bound_the_import_exactly() {
  for name in ["dot", "v2"] {
    let room =
      least(|room| import(&mut repository(name), &mut store(), Admission::new(room)).is_ok());
    let mut store = store();
    let refusal = import(&mut repository(name), &mut store, Admission::new(room - 1)).unwrap_err();
    assert!(
      matches!(refusal, ImportRefused::PastAdmission(_)),
      "{name}: {refusal:?}"
    );
    assert_eq!((store.len(), store.used()), (0, 0));
    let fits = |budget| ContentStore::new(budget, WINDOW).unwrap();
    let budget =
      least(|budget| import(&mut repository(name), &mut fits(budget), self::room()).is_ok());
    let mut small = fits(budget - 1);
    let refusal = import(&mut repository(name), &mut small, self::room()).unwrap_err();
    let by_budget = matches!(refusal, ImportRefused::Content(_) | ImportRefused::Index(_));
    assert!(by_budget, "{name}: {refusal:?}");
    assert_eq!((small.len(), small.used()), (0, 0));
  }
}

/// A-630 part 2 (§4.15's pending fetch): do import a store whose host answers one file log's index with its own
/// refusal. Expect that refusal handed back untouched, with nothing left held.
#[test]
fn a_host_refusal_is_handed_back() {
  let mut repository = repository("dot");
  repository.pending = Some(b"store/data/plain.txt.i".to_vec());
  assert_eq!(refused(&mut repository), ImportRefused::Host(Pending));
}

/// AC-HG.9, A-630 part 3: do import a version 2 store whose file index docket names its list file by an id that climbs
/// out of the store. Expect it refused before the host is asked for any such name.
#[test]
fn a_docket_id_climbing_out_of_the_store_is_refused_unasked() {
  let mut repository = repository("v2");
  let docket = repository
    .files
    .get_mut(b"store/fileindex".as_slice())
    .unwrap();
  docket[24..32].copy_from_slice(b"../../ab");
  let path = named("store/fileindex-list.../../ab");
  assert_eq!(refused(&mut repository), ImportRefused::UnsafePath { path });
  let climbing = |path: &Vec<u8>| path.split(|byte| *byte == b'/').any(|name| name == b"..");
  assert!(
    !repository.asked.iter().any(climbing),
    "{:?}",
    repository.asked
  );
}

/// AC-HG.9, A-630 part 2: do import a store whose one-revision file log holds its revision twice, the second chunk
/// after the first. Expect the second refused as a duplicate: the index would pass it over, and number any revision
/// after it otherwise than 7.2.
#[test]
fn a_node_a_log_holds_twice_is_refused() {
  let mut repository = repository("dot");
  let original = repository.files[b"store/data/au~78.txt.i".as_slice()].clone();
  let chunk = original[64..].to_vec();
  let mut second = original[..64].to_vec();
  let offset = u64::try_from(chunk.len()).unwrap() << 16;
  second[..8].copy_from_slice(&offset.to_be_bytes());
  second[16..20].copy_from_slice(&1_u32.to_be_bytes());
  let doubled = [original, second, chunk].concat();
  repository
    .files
    .insert(b"store/data/au~78.txt.i".to_vec(), doubled);
  let expected = ImportRefused::Duplicate {
    revlog: named("data/aux.txt"),
    rev: 1,
  };
  assert_eq!(refused(&mut repository), expected);
}

/// AC-HG.9, A-630 part 2: do import stores whose fncache names a tree manifest's revlog in a store without tree
/// manifests, a file log with an empty path, and a file that is no revlog's. Expect the first two refused by kind and
/// the third passed over, as 7.2's walk passes over it.
#[test]
fn fncache_entries_are_read_as_7_2_groups_them() {
  for (entry, expected) in [
    (
      "meta/a/00manifest.i",
      Some(ImportRefused::Entry {
        entry: named("meta/a/00manifest.i"),
      }),
    ),
    (
      "data/.i",
      Some(ImportRefused::UnusablePath { path: named("") }),
    ),
    ("data/leftover.txt", None),
  ] {
    let mut repository = repository("dot");
    let fncache = repository
      .files
      .get_mut(b"store/fncache".as_slice())
      .unwrap();
    fncache.extend_from_slice(format!("{entry}\n").as_bytes());
    match expected {
      Some(expected) => assert_eq!(refused(&mut repository), expected, "{entry}"),
      None => assert!(
        import(&mut repository, &mut store(), room()).is_ok(),
        "{entry}"
      ),
    }
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// AC-HG.9, A-630 parts 2 and 3: do change any byte of any file of the dotencode store or of the version 2 store to
  /// any value, or cut it at any byte, and import. Expect an import or a refusal by kind with nothing left held; never a
  /// panic.
  #[test]
  fn damaged_stores_never_panic_and_leave_nothing_held(
    version_2 in any::<bool>(),
    file in any::<proptest::sample::Index>(),
    at in any::<proptest::sample::Index>(),
    byte in any::<u8>(),
    cut in any::<bool>(),
  ) {
    let mut repository = repository(if version_2 { "v2" } else { "dot" });
    let names: Vec<Vec<u8>> = repository.files.keys().cloned().collect();
    let chosen = names[file.index(names.len())].clone();
    if let Some(target) = repository.files.get_mut(&chosen).filter(|target| !target.is_empty()) {
      let place = at.index(target.len());
      if cut { target.truncate(place) } else { target[place] = byte }
    }
    let mut store = store();
    if import(&mut repository, &mut store, room()).is_err() {
      prop_assert_eq!((store.len(), store.used()), (0, 0));
    }
  }
}

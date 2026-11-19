//! Golden and hostile tests of the import of an existing store (A-630 parts 2 and 3): one history stock Mercurial 7.2
//! stored five ways (`fixtures/store`'s dotencode and hybrid stores, the dotencode store under a pre-share-safe
//! `.hg/requires`, and `fixtures/import`'s revlog version 2 store and version 1 file index store), each imported through
//! the read seam with every text 7.2's `debugdata` and every node 7.2's, and the five indexes equal row for row; then
//! damaged and hostile stores refused by kind with nothing left held. And the state beside a history (A-649 part 2):
//! `fixtures/state`'s phases, bookmarks and markers, held to 7.2's own listings of them.

use std::collections::{BTreeMap, BTreeSet};

use slates_vcs::{ContentId, ContentStore};

use crate::fixtures::{
  import as import_fixtures, state as state_fixtures, store as store_fixtures,
};
use crate::import::{FileRefused, ImportRefused, Imported, RepositoryFiles, import};
use crate::index::{Log, NONE, RevisionRow};
use crate::lines::lines;
use crate::obsmarkers;
use crate::phases::Phase;
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
  reads: Vec<Range>,
  pending: Option<Vec<u8>>,
}

/// A range read: the file's path, the offset and the length asked.
type Range = (Vec<u8>, u64, u64);

impl RepositoryFiles for Repository {
  type Refused = Pending;

  fn len(&mut self, path: &[&[u8]]) -> Result<Option<u64>, FileRefused<Pending>> {
    let joined = path.join(&b'/');
    self.asked.push(joined.clone());
    if self.pending.as_ref() == Some(&joined) {
      return Err(FileRefused::Host(Pending));
    }
    let len = |bytes: &Vec<u8>| u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Ok(self.files.get(&joined).map(len))
  }

  fn read(
    &mut self,
    path: &[&[u8]],
    offset: u64,
    len: u64,
    room: &mut Admission,
  ) -> Result<Option<Vec<u8>>, FileRefused<Pending>> {
    let joined = path.join(&b'/');
    self.reads.push((joined.clone(), offset, len));
    let Some(bytes) = self.files.get(&joined) else {
      return Ok(None);
    };
    let at = |value: u64| {
      usize::try_from(value)
        .unwrap_or(usize::MAX)
        .min(bytes.len())
    };
    let range = bytes
      .get(at(offset)..at(offset.saturating_add(len)))
      .unwrap_or_default();
    let charged = u64::try_from(range.len()).unwrap_or(u64::MAX);
    room.charge(charged).map_err(FileRefused::PastAdmission)?;
    Ok(Some(range.to_vec()))
  }
}

/// Which fixtures hold a repository's files.
#[derive(Clone, Copy)]
enum Source {
  /// `fixtures/store`.
  Store,
  /// `fixtures/import`.
  Import,
  /// `fixtures/state`.
  State,
}

/// A fixture file, if its source holds one by that name.
fn fixture(source: Source, name: &str) -> Option<&'static [u8]> {
  match source {
    Source::Store => store_fixtures::find(name),
    Source::Import => import_fixtures::find(name),
    Source::State => state_fixtures::find(name),
  }
}

/// The `.hg` of one of the repositories: its `.hg/requires`, its store from the fixtures that hold it (the
/// pre-share-safe one's is the dotencode store without its own requirements), and its `.hg/bookmarks` where it keeps one.
fn repository(name: &str) -> Repository {
  let (source, stored) = match name {
    "legacy" => (Source::Store, "dot"),
    "dot" | "hybrid" => (Source::Store, name),
    "states" | "states-store" => (Source::State, name),
    _ => (Source::Import, name),
  };
  let requires_source = if matches!(source, Source::State) {
    Source::State
  } else {
    Source::Import
  };
  let mut files = BTreeMap::new();
  let requires = fixture(requires_source, &format!("{name}-hg-requires.txt")).unwrap();
  files.insert(b"requires".to_vec(), requires.to_vec());
  if let Some(bookmarks) = fixture(source, &format!("{name}-bookmarks")) {
    files.insert(b"bookmarks".to_vec(), bookmarks.to_vec());
  }
  for line in lines(fixture(source, &format!("{stored}-files.txt")).unwrap()) {
    let file = core::str::from_utf8(line).unwrap();
    if name != "legacy" || file != "requires" {
      let bytes = fixture(source, &format!("{stored}/{file}")).unwrap();
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

/// The files a repository may lack, which the import asks for and reads as empty: its state beside its history.
const OPTIONAL: [&[u8]; 4] = [
  b"store/phaseroots",
  b"store/obsstore",
  b"bookmarks",
  b"store/bookmarks",
];

/// Holds what the import asked the host for: files the repository holds, or its optional state's, each once.
fn check_asked(repository: &Repository) {
  let asked: BTreeSet<&Vec<u8>> = repository.asked.iter().collect();
  assert_eq!(asked.len(), repository.asked.len(), "a file asked twice");
  let known =
    |path: &&Vec<u8>| repository.files.contains_key(*path) || OPTIONAL.contains(&path.as_slice());
  assert!(asked.iter().all(known), "{:?}", repository.asked);
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

/// The reads of the file at `path`, each its length.
fn read_lengths(repository: &Repository, path: &[u8]) -> Vec<u64> {
  let of_path = |(read, _, _): &&Range| read.as_slice() == path;
  repository
    .reads
    .iter()
    .filter(of_path)
    .map(|(_, _, len)| *len)
    .collect()
}

/// A-649 part 3 (non-vacuity): do import the dotencode store, whose changelog keeps a data file, and the version 2 store,
/// whose revlogs are behind dockets. Expect each such file read in more than one range and no range as long as the file,
/// an inline revlog read whole, and texts rebuilt from the text before them.
#[test]
fn revlogs_are_read_by_ranges_never_whole() {
  let mut dot = repository("dot");
  assert!(imported(&mut dot).rebuilt_from_cache > 0);
  let mut v2 = repository("v2");
  imported(&mut v2);
  let ranged = [
    (&dot, b"store/00changelog.i".as_slice()),
    (&dot, b"store/00changelog.d"),
    (&v2, b"store/00changelog-150e1cfc.idx"),
    (&v2, b"store/00changelog-b870a51b.dat"),
  ];
  for (repository, path) in ranged {
    let whole = u64::try_from(repository.files[path].len()).unwrap();
    let lengths = read_lengths(repository, path);
    let pieces = lengths.len() > 1 && lengths.iter().all(|len| *len < whole);
    assert!(
      pieces,
      "{}: {lengths:?} of {whole}",
      String::from_utf8_lossy(path)
    );
  }
  let inline = read_lengths(&dot, b"store/data/plain.txt.i");
  let whole = u64::try_from(dot.files[b"store/data/plain.txt.i".as_slice()].len()).unwrap();
  assert!(inline.contains(&whole), "{inline:?}");
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

/// The lines of a fixture of `fixtures/state`, each cut at its first space.
fn listing(name: &str) -> Vec<(String, String)> {
  lines(state_fixtures::file(name))
    .map(|line| {
      let line = core::str::from_utf8(line).unwrap();
      let (first, rest) = line.split_once(' ').unwrap();
      (first.to_string(), rest.to_string())
    })
    .collect()
}

/// A node's lower-case hexadecimal.
fn hex(node: &[u8]) -> String {
  node.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Each changeset's phase computed from the imported roots, as 7.2 computes it: the greatest of its parents' and of a
/// root's at itself, by node in hexadecimal.
fn phases_of(imported: &Imported, store: &ContentStore) -> BTreeMap<String, Phase> {
  let mut phases: Vec<Phase> = Vec::new();
  let mut named = BTreeMap::new();
  for rev in 0..imported.changesets {
    let row = imported.index.changeset(store, rev).unwrap();
    let rooted = imported
      .phase_roots
      .iter()
      .filter(|root| root.node == row.node);
    let parents = [row.p1, row.p2]
      .into_iter()
      .filter(|parent| *parent != NONE);
    let inherited = parents.map(|parent| phases[usize::try_from(parent).unwrap()]);
    let phase = rooted
      .map(|root| root.phase)
      .chain(inherited)
      .max()
      .unwrap_or(Phase::Public);
    phases.push(phase);
    named.insert(hex(&row.node), phase);
  }
  named
}

/// Holds an import's state to 7.2's listings of `name`: every changeset's phase, every bookmark, every marker (its
/// predecessor, each in the obsstore's order, and all of them written back to the obsstore's bytes).
fn check_state(name: &str, imported: &Imported, store: &ContentStore) {
  let phases = phases_of(imported, store);
  let listed: BTreeMap<String, String> =
    listing(&format!("{name}-phases.txt")).into_iter().collect();
  let computed: BTreeMap<String, String> = phases
    .iter()
    .map(|(node, phase)| (node.clone(), phase.name().to_string()))
    .collect();
  assert_eq!(computed, listed, "{name} phases");
  let bookmarks: Vec<(String, String)> = imported
    .bookmarks
    .iter()
    .map(|(bookmark, node)| (String::from_utf8(bookmark.clone()).unwrap(), hex(node)))
    .collect();
  assert_eq!(
    bookmarks,
    listing(&format!("{name}-bookmarks.txt")),
    "{name} bookmarks"
  );
  check_markers(name, imported, store);
}

/// Holds an import's markers to 7.2's: each one's predecessor in the obsstore's order, and all of them, each read back
/// alone, written into one payload equal to the obsstore's bytes.
fn check_markers(name: &str, imported: &Imported, store: &ContentStore) {
  let listed = listing(&format!("{name}-markers.txt"));
  assert_eq!(
    usize::try_from(imported.markers).unwrap(),
    listed.len(),
    "{name} markers"
  );
  let mut written = Vec::new();
  for (number, (predecessor, _)) in (0_u64..).zip(&listed) {
    let alone = whole(store, &imported.index.marker(store, number).unwrap());
    let read = obsmarkers::decode(&alone).unwrap();
    let marker = read.iter().next().unwrap();
    assert_eq!(
      &hex(marker.predecessor),
      predecessor,
      "{name} marker {number}"
    );
    written.extend_from_slice(if written.is_empty() {
      &alone
    } else {
      &alone[1..]
    });
  }
  let obsstore = state_fixtures::file(&format!("{name}/obsstore"));
  assert_eq!(written, obsstore, "{name} obsstore");
}

/// A-649 part 2 (golden): do import the two repositories 7.2 kept one history's state in, bookmarks in `.hg` and in the
/// store. Expect every changeset's phase computed from the imported roots as 7.2 lists it (public, draft, the amended
/// one's predecessor draft, secret), every bookmark as 7.2 lists it, every marker 7.2 lists in its order and written
/// back to the obsstore's bytes, nothing dropped or malformed, the two indexes equal row for row, and each repository's
/// bookmarks read from where it keeps them alone.
#[test]
fn the_state_7_2_kept_beside_a_history_is_imported() {
  let mut first: Option<Rows> = None;
  for (name, kept, unasked) in [
    ("states", "bookmarks", "store/bookmarks"),
    ("states-store", "store/bookmarks", "bookmarks"),
  ] {
    let mut repository = repository(name);
    let mut store = store();
    let mut imported = import(&mut repository, &mut store, room()).unwrap();
    check_state(name, &imported, &store);
    let counts = (
      imported.dropped_phase_roots,
      imported.dropped_bookmarks,
      imported.malformed_bookmarks,
    );
    assert_eq!(counts, (0, 0, 0), "{name}");
    let asked = |path: &str| {
      repository
        .asked
        .iter()
        .any(|asked| asked.as_slice() == path.as_bytes())
    };
    assert_eq!((asked(kept), asked(unasked)), (true, false), "{name}");
    let rows = rows(&mut imported, &store);
    assert_eq!(first.get_or_insert_with(|| rows.clone()), &rows, "{name}");
  }
}

/// `repository` imported into a new store.
fn imported(repository: &mut Repository) -> Imported {
  import(repository, &mut store(), room()).unwrap()
}

/// The `states` repository with `file` holding `bytes`.
fn states_with(file: &str, bytes: &[u8]) -> Repository {
  let mut repository = repository("states");
  repository
    .files
    .insert(file.as_bytes().to_vec(), bytes.to_vec());
  repository
}

/// AC-HG.9, A-649 part 2 (7.2's `phases._readroots`): do import `phaseroots` lines of one field, of three, with a
/// phase 7.2 does not define, a phase that is not a number, and a node of 39 digits. Expect each refused naming its line,
/// with nothing left held.
#[test]
fn phase_roots_7_2_cannot_read_are_refused() {
  let secret = "4ed94230db9650095697695adb1b40ef155eb141";
  for (text, line) in [
    (format!("2 {secret}\n1\n"), 2),
    (format!("2 {secret} extra\n"), 1),
    (format!("7 {secret}\n"), 1),
    (format!("x {secret}\n"), 1),
    (format!("\n2 {secret}\n"), 1),
    (format!("2 {}\n", secret.get(1..).unwrap()), 1),
  ] {
    let refusal = refused(&mut states_with("store/phaseroots", text.as_bytes()));
    assert_eq!(refusal, ImportRefused::PhaseRoots { line }, "{text:?}");
  }
}

/// A-649 part 2 (7.2's `phases._readroots`): do import a `phaseroots` with a root whose node the changelog does not
/// hold, a public root, tabs and a carriage return between and after the fields, and the secret root twice. Expect the
/// unknown root dropped and counted, the public one passed over, the secret root kept once.
#[test]
fn phase_roots_whose_nodes_are_not_held_are_dropped_as_7_2_drops_them() {
  let secret = "4ed94230db9650095697695adb1b40ef155eb141";
  let text = format!(
    "2\t{secret}\r\n0 {secret}\n1 {}\n2 {secret}\n",
    "a".repeat(40)
  );
  let imported = imported(&mut states_with("store/phaseroots", text.as_bytes()));
  let roots: Vec<(Phase, String)> = imported
    .phase_roots
    .iter()
    .map(|root| (root.phase, hex(&root.node)))
    .collect();
  assert_eq!(
    (roots, imported.dropped_phase_roots),
    (vec![(Phase::Secret, secret.to_string())], 1)
  );
}

/// A-649 part 2 (7.2's `bookmarks.bmstore`): do import a bookmarks file with an empty line, a line without a space,
/// digits that are not hexadecimal, an odd count of digits, a node the changelog does not hold, a short node, a name
/// given twice, a name with a space in it and a carriage return before a line's end. Expect the malformed lines passed
/// over and counted, the unheld nodes dropped and counted, the later node kept for the name given twice, and the rest
/// kept with their names as written, sorted by name.
#[test]
fn bookmark_lines_are_read_as_7_2_reads_them() {
  let (one, two) = (
    "baa257697fd21c2c951df2f7ba0370ab6db8fa10",
    "b252f2a7c3d2e773bea91bf42aaa0acb39463176",
  );
  let text = format!(
    "\n{one}\nzz{two} bad\n{} odd\n{} unheld\n{} short\n{one} again\n{two} again\n{one} a name\r\n",
    two.get(1..).unwrap(),
    "a".repeat(40),
    two.get(..38).unwrap(),
  );
  let imported = imported(&mut states_with("bookmarks", text.as_bytes()));
  let kept: Vec<(Vec<u8>, String)> = imported
    .bookmarks
    .iter()
    .map(|(name, node)| (name.clone(), hex(node)))
    .collect();
  let expected = vec![
    (b"a name".to_vec(), one.to_string()),
    (b"again".to_vec(), two.to_string()),
  ];
  assert_eq!(
    (
      kept,
      imported.malformed_bookmarks,
      imported.dropped_bookmarks
    ),
    (expected, 3, 2)
  );
}

/// AC-HG.9, A-649 part 2: do import an obsstore cut by a byte, and an empty one. Expect the first refused by the
/// markers' reader with nothing left held, and the second read as no marker, as 7.2 reads it.
#[test]
fn an_obsstore_is_read_whole_or_refused() {
  let obsstore = state_fixtures::file("states/obsstore");
  let refusal = refused(&mut states_with(
    "store/obsstore",
    &obsstore[..obsstore.len() - 1],
  ));
  assert!(matches!(refusal, ImportRefused::Markers(_)), "{refusal:?}");
  assert_eq!(imported(&mut states_with("store/obsstore", &[])).markers, 0);
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

//! The branch's tests (A-534; §4.17.1 item 4 and shared piece 1's second half). A green made at another green's version
//! must answer every read as a green seeded from a copy of the other's state at that version does (the copying
//! reference, [`copy_at`]); its making must walk no map and set back exactly the keys that changed after that version;
//! a change to one green of a lineage must never show in another; and once every green is retired the lineage holds no
//! node and no block.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::super::{Green, Increment, Outcome};
use super::{Lineage, LineageGreen, LineageRefused};
use crate::checkpoint::Checkpoint;
use crate::increment::Base;
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::origin::Origin;
use crate::special::{SpecialKind, SpecialNode};

/// Shape: the paths works name: prefixes of one another and nested, so radix edges split and renames meet.
const PATHS: [&str; 6] = ["a", "a.b", "d/e", "d/f", "g/h", "k"];
/// Shape: the directories works make and remove.
const DIRS: [&str; 3] = ["d", "g", "m"];
/// Shape: the attribute names.
const NAMES: [&str; 2] = ["user.x", "user.y"];
/// Shape: the most greens one history keeps, so branches of branches happen.
const GREENS: usize = 4;

/// The copying reference: `green`'s whole state at `version` copied into an origin (a walk of every map and history).
fn copy_at(green: &Green, version: u64) -> Origin {
  let base = green.base_at(version);
  Origin {
    files: green
      .files_at(version)
      .into_iter()
      .map(|(path, bytes)| (path, bytes.into_owned()))
      .collect(),
    dirs: base.dirs,
    modes: base.modes,
    symlinks: base.symlinks,
    hardlinks: base.hardlinks,
    specials: base.specials,
    xattrs: base.xattrs,
  }
}

/// The oldest version `green` reconstructs.
fn floor_of(green: &Green) -> u64 {
  green.folded_below().max(green.mappable_from())
}

/// A FIFO owned by `uid`.
fn fifo(uid: u8) -> SpecialNode {
  SpecialNode {
    kind: SpecialKind::Fifo,
    uid: u32::from(uid),
    gid: 0,
    atime: 0,
    mtime: 0,
    ctime: 0,
    btime: 0,
  }
}

/// One edit of a generated work, named by its indexes into [`PATHS`], [`DIRS`] and [`NAMES`]; offsets are taken modulo
/// the file's length at the work's base.
#[derive(Clone, Debug)]
enum Edit {
  Create(usize, Vec<u8>),
  Overwrite(usize, u8, Vec<u8>),
  Insert(usize, u8, Vec<u8>),
  Delete(usize, u8, u8),
  Truncate(usize, u8),
  Unlink(usize),
  Rename(usize, usize),
  Mkdir(usize),
  Rmdir(usize),
  Mode(usize, u16),
  Symlink(usize, usize),
  Link(usize, usize),
  SetXattr(usize, usize, Vec<u8>),
  RemoveXattr(usize, usize),
  Mknod(usize, u8),
}

/// An increment under construction, laid out as the deriver lays one out.
#[derive(Default)]
struct Work {
  doc: OpsDoc,
  post: Vec<u8>,
}

impl Work {
  fn op(&mut self, kind: OpKind, path: &str, at: u64, len: u64, src: u64) {
    let path = self.doc.paths.intern(path);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path,
      at,
      len,
      src,
    });
  }

  fn stash(&mut self, bytes: &[u8]) -> u64 {
    let at = self.post.len() as u64;
    self.post.extend_from_slice(bytes);
    at
  }

  fn name(&mut self, name: &str) -> u64 {
    u64::from(self.doc.paths.intern(name))
  }
}

impl Edit {
  /// The paths the edit names, the one its offsets are taken against first.
  fn paths(&self) -> Vec<&'static str> {
    match self {
      Edit::Create(path, _)
      | Edit::Overwrite(path, ..)
      | Edit::Insert(path, ..)
      | Edit::Delete(path, ..)
      | Edit::Truncate(path, _)
      | Edit::Unlink(path)
      | Edit::Mode(path, _)
      | Edit::Symlink(path, _)
      | Edit::Link(path, _)
      | Edit::SetXattr(path, ..)
      | Edit::RemoveXattr(path, _)
      | Edit::Mknod(path, _) => vec![PATHS[*path]],
      Edit::Rename(from, to) => vec![PATHS[*from], PATHS[*to]],
      Edit::Mkdir(dir) | Edit::Rmdir(dir) => vec![DIRS[*dir]],
    }
  }

  /// Lays the edit's ops into `work`, its offsets taken modulo `len + 1` (`len`: its first path's length at the base),
  /// a content edit that would change nothing left out.
  fn emit(&self, work: &mut Work, len: u64) {
    let path = self.paths()[0];
    match self {
      Edit::Create(_, bytes) => {
        work.op(OpKind::Create, path, 0, 0, u64::MAX);
        let src = work.stash(bytes);
        work.op(OpKind::Insert, path, 0, bytes.len() as u64, src);
      }
      Edit::Overwrite(_, at, bytes) => {
        let at = u64::from(*at) % (len + 1);
        let n = (bytes.len() as u64).min(len - at);
        if n > 0 {
          let src = work.stash(&bytes[..usize::try_from(n).unwrap()]);
          work.op(OpKind::Overwrite, path, at, n, src);
        }
      }
      Edit::Insert(_, at, bytes) => {
        let src = work.stash(bytes);
        let at = u64::from(*at) % (len + 1);
        work.op(OpKind::Insert, path, at, bytes.len() as u64, src);
      }
      Edit::Delete(_, at, n) => {
        let at = u64::from(*at) % (len + 1);
        let n = u64::from(*n).min(len - at);
        if n > 0 {
          work.op(OpKind::Delete, path, at, n, 0);
        }
      }
      Edit::Truncate(_, at) => {
        let at = u64::from(*at) % (len + 1);
        if at < len {
          work.op(OpKind::Truncate, path, at, len - at, 0);
        }
      }
      Edit::Unlink(_) => work.op(OpKind::Unlink, path, 0, 0, u64::MAX),
      Edit::Rename(_, to) => {
        let src = work.name(path);
        work.op(OpKind::Rename, PATHS[*to], 0, 0, src);
      }
      Edit::Mkdir(_) => work.op(OpKind::Mkdir, path, 0, 0, u64::MAX),
      Edit::Rmdir(_) => work.op(OpKind::Rmdir, path, 0, 0, u64::MAX),
      Edit::Mode(_, mode) => work.op(OpKind::SetMode, path, 0, u64::from(*mode), 0),
      Edit::Symlink(_, target) | Edit::Link(_, target) => {
        let kind = match self {
          Edit::Symlink(..) => OpKind::Symlink,
          _ => OpKind::Link,
        };
        let src = work.name(PATHS[*target]);
        work.op(kind, path, 0, 0, src);
      }
      Edit::SetXattr(_, name, value) => {
        let name = work.name(NAMES[*name]);
        let src = work.stash(value);
        work.op(OpKind::SetXattr, path, name, value.len() as u64, src);
      }
      Edit::RemoveXattr(_, name) => {
        let name = work.name(NAMES[*name]);
        work.op(OpKind::RemoveXattr, path, name, 0, 0);
      }
      Edit::Mknod(_, uid) => {
        let node = fifo(*uid).encode();
        let src = work.stash(&node);
        work.op(OpKind::Mknod, path, 0, node.len() as u64, src);
      }
    }
  }
}

/// The increment `id` of `edits` against `green`'s `base`: an edit naming a path an earlier one named is left out, so
/// each path has one edit, and each content edit lies inside the file's length at the base.
fn work(green: &Green, base: u64, edits: &[Edit], id: u64) -> Increment {
  let mut work = Work::default();
  let mut used: BTreeSet<&str> = BTreeSet::new();
  for edit in edits {
    let paths = edit.paths();
    if paths.iter().any(|path| used.contains(path)) {
      continue;
    }
    used.extend(paths.iter().copied());
    let len = paths
      .first()
      .and_then(|path| green.content_len_at(path, base))
      .map_or(0, |len| len as u64);
    edit.emit(&mut work, len);
  }
  let mut identity = [0u8; 32];
  identity[..8].copy_from_slice(&id.to_le_bytes());
  Increment {
    id: identity,
    base,
    doc: work.doc,
    post_state: work.post,
    evidence: Vec::new(),
  }
}

/// One version's every read: what a green answers for it.
#[derive(Debug, PartialEq)]
struct VersionView {
  version: u64,
  base: Base,
  files: Vec<(String, Vec<u8>)>,
  lens: Vec<(String, usize)>,
  changed: Vec<String>,
  paths: Vec<Base>,
  pages: Vec<Option<(Vec<u8>, u64)>>,
}

/// One path's metadata at the head, as its readers answer it.
#[derive(Debug, PartialEq)]
struct Metadata {
  special: Option<SpecialNode>,
  mode: Option<u32>,
  symlink: Option<String>,
  hardlink: Option<String>,
  xattrs: Vec<Option<Vec<u8>>>,
}

/// Everything a reader can observe of a green, at every version it still reconstructs.
#[derive(Debug, PartialEq)]
struct View {
  head: u64,
  floor: u64,
  identity: [u8; 32],
  files: Vec<(String, Vec<u8>)>,
  current: Base,
  metadata: Vec<Metadata>,
  versions: Vec<VersionView>,
  checkpoint: Option<Checkpoint>,
  retained: (usize, usize, usize),
}

/// Reads `green` whole ([`View`]). Its running identity must be its recount, and its change index its histories' model.
fn view(green: &mut Green) -> View {
  let head = green.head();
  let floor = floor_of(green);
  let identity = green.head_identity();
  assert_eq!(
    identity,
    green.head_identity_from_scratch(),
    "the running identity is not the recount"
  );
  let versions = (floor..=head)
    .map(|version| {
      let changed = green.changed_between(version, head);
      assert_eq!(
        changed,
        green.changed_between_by_histories(version, head),
        "the change index is not its model from {version}"
      );
      VersionView {
        version,
        base: green.base_at(version),
        files: green
          .files_at(version)
          .into_iter()
          .map(|(path, bytes)| (path, bytes.into_owned()))
          .collect(),
        lens: green.file_lens_at(version),
        changed,
        paths: PATHS
          .iter()
          .chain(DIRS.iter())
          .map(|path| green.path_base(version, path))
          .collect(),
        pages: PATHS
          .iter()
          .map(|path| green.content_page_at(path, version, (1, 2)))
          .collect(),
      }
    })
    .collect();
  let retained = green.retained_bytes();
  View {
    head,
    floor,
    identity,
    files: green
      .files()
      .map(|(path, bytes)| (path, bytes.to_vec()))
      .collect(),
    current: green.current_base(),
    metadata: PATHS
      .iter()
      .map(|path| Metadata {
        special: green.special(path),
        mode: green.mode(path),
        symlink: green.symlink(path).map(str::to_owned),
        hardlink: green.hardlink(path).map(str::to_owned),
        xattrs: NAMES
          .iter()
          .map(|name| green.xattr(path, name).map(<[u8]>::to_vec))
          .collect(),
      })
      .collect(),
    versions,
    checkpoint: green.checkpoint(head),
    retained: (retained.content, retained.history, retained.changes),
  }
}

/// What a green held when a version was its head: what every later read of that version must answer.
#[derive(Debug, PartialEq)]
struct Snapshot {
  base: Base,
  files: Vec<(String, Vec<u8>)>,
  identity: [u8; 32],
}

/// `green`'s head as a [`Snapshot`].
fn snapshot(green: &mut Green) -> Snapshot {
  Snapshot {
    base: green.current_base(),
    files: green
      .files()
      .map(|(path, bytes)| (path, bytes.to_vec()))
      .collect(),
    identity: green.head_identity(),
  }
}

/// The keys of `before` and `after` whose values differ, each once.
fn differ<K: Ord + Clone, V: PartialEq + Clone>(before: &[(K, V)], after: &[(K, V)]) -> u64 {
  let before: BTreeMap<K, V> = before.iter().cloned().collect();
  let after: BTreeMap<K, V> = after.iter().cloned().collect();
  let keys: BTreeSet<&K> = before.keys().chain(after.keys()).collect();
  keys
    .into_iter()
    .filter(|key| before.get(*key) != after.get(*key))
    .count() as u64
}

/// The keys, in every dimension, whose value at `version` differs from the head's: what a branch at `version` must set
/// back, counted from two whole copies (the reference's walk).
fn differing_keys(green: &Green, version: u64) -> u64 {
  let then = copy_at(green, version);
  let now = copy_at(green, green.head());
  let present = |dirs: &[String]| -> Vec<(String, ())> { dirs.iter().map(|dir| (dir.clone(), ())).collect() };
  let paired = |xattrs: &[(String, String, Vec<u8>)]| -> Vec<((String, String), Vec<u8>)> {
    xattrs
      .iter()
      .map(|(path, name, value)| ((path.clone(), name.clone()), value.clone()))
      .collect()
  };
  differ(&then.files, &now.files)
    + differ(&present(&then.dirs), &present(&now.dirs))
    + differ(&then.modes, &now.modes)
    + differ(&then.symlinks, &now.symlinks)
    + differ(&then.hardlinks, &now.hardlinks)
    + differ(&then.specials, &now.specials)
    + differ(&paired(&then.xattrs), &paired(&now.xattrs))
}

/// A green of the history under test: its handle in the lineage, its copying reference (a green apart, fed the same
/// works), and what it held at each version it made.
struct Twin {
  handle: LineageGreen,
  copy: Green,
  recorded: BTreeMap<u64, Snapshot>,
}

/// What one path holds in a generated origin.
#[derive(Clone, Debug)]
enum Seed {
  Nothing,
  File(Vec<u8>),
  Symlink(usize),
  Special(u8),
  Hardlink(usize),
}

fn bytes() -> impl Strategy<Value = Vec<u8>> {
  proptest::collection::vec(prop_oneof![Just(b'x'), Just(b'y'), Just(b'z')], 1..6)
}

fn origin() -> impl Strategy<Value = Origin> {
  let seed = prop_oneof![
    3 => Just(Seed::Nothing),
    4 => bytes().prop_map(Seed::File),
    1 => (0..PATHS.len()).prop_map(Seed::Symlink),
    1 => any::<u8>().prop_map(Seed::Special),
    1 => (0..PATHS.len()).prop_map(Seed::Hardlink),
  ];
  (
    proptest::collection::vec(seed, PATHS.len()),
    proptest::collection::btree_set(0..DIRS.len(), 0..=DIRS.len()),
    proptest::collection::btree_map(0..PATHS.len(), 0u32..0o1000, 0..3),
    proptest::collection::btree_map((0..PATHS.len(), 0..NAMES.len()), bytes(), 0..3),
  )
    .prop_map(|(seeds, dirs, modes, xattrs)| {
      let mut origin = Origin::default();
      for (path, seed) in PATHS.iter().zip(seeds) {
        let path = (*path).to_owned();
        match seed {
          Seed::Nothing => {}
          Seed::File(bytes) => origin.files.push((path, bytes)),
          Seed::Symlink(target) => origin.symlinks.push((path, PATHS[target].to_owned())),
          Seed::Special(uid) => origin.specials.push((path, fifo(uid))),
          Seed::Hardlink(target) => origin.hardlinks.push((path, PATHS[target].to_owned())),
        }
      }
      origin.dirs = dirs.into_iter().map(|dir| DIRS[dir].to_owned()).collect();
      origin.modes = modes
        .into_iter()
        .map(|(path, mode)| (PATHS[path].to_owned(), mode))
        .collect();
      origin.xattrs = xattrs
        .into_iter()
        .map(|((path, name), value)| (PATHS[path].to_owned(), NAMES[name].to_owned(), value))
        .collect();
      origin
    })
}

fn edit() -> impl Strategy<Value = Edit> {
  let path = || 0..PATHS.len();
  prop_oneof![
    (path(), bytes()).prop_map(|(path, bytes)| Edit::Create(path, bytes)),
    (path(), any::<u8>(), bytes()).prop_map(|(path, at, bytes)| Edit::Overwrite(path, at, bytes)),
    (path(), any::<u8>(), bytes()).prop_map(|(path, at, bytes)| Edit::Insert(path, at, bytes)),
    (path(), any::<u8>(), any::<u8>()).prop_map(|(path, at, len)| Edit::Delete(path, at, len)),
    (path(), any::<u8>()).prop_map(|(path, at)| Edit::Truncate(path, at)),
    path().prop_map(Edit::Unlink),
    (path(), path()).prop_map(|(from, to)| Edit::Rename(from, to)),
    (0..DIRS.len()).prop_map(Edit::Mkdir),
    (0..DIRS.len()).prop_map(Edit::Rmdir),
    (path(), 0u16..0o1000).prop_map(|(path, mode)| Edit::Mode(path, mode)),
    (path(), path()).prop_map(|(path, target)| Edit::Symlink(path, target)),
    (path(), path()).prop_map(|(path, target)| Edit::Link(path, target)),
    (path(), 0..NAMES.len(), bytes()).prop_map(|(path, name, value)| Edit::SetXattr(path, name, value)),
    (path(), 0..NAMES.len()).prop_map(|(path, name)| Edit::RemoveXattr(path, name)),
    (path(), any::<u8>()).prop_map(|(path, uid)| Edit::Mknod(path, uid)),
  ]
}

/// One step of a generated history over a lineage.
#[derive(Clone, Debug)]
enum Step {
  /// A work of `edits` submitted to a green, based `back` versions before its head (within its floor).
  Submit {
    green: usize,
    back: u8,
    edits: Vec<Edit>,
  },
  /// A branch of a green at `back` versions before its head (within its floor).
  Branch { from: usize, back: u8 },
  /// A green's histories folded below `back` versions before its head.
  Fold { green: usize, back: u8 },
  /// A green retired (never the last).
  Retire { green: usize },
}

fn step() -> impl Strategy<Value = Step> {
  prop_oneof![
    6 => (0..GREENS, 0u8..3, proptest::collection::vec(edit(), 1..4))
      .prop_map(|(green, back, edits)| Step::Submit { green, back, edits }),
    2 => (0..GREENS, any::<u8>()).prop_map(|(from, back)| Step::Branch { from, back }),
    1 => (0..GREENS, any::<u8>()).prop_map(|(green, back)| Step::Fold { green, back }),
    1 => (0..GREENS).prop_map(|green| Step::Retire { green }),
  ]
}

/// `head` less `back`, kept at or above `floor`.
fn behind(head: u64, floor: u64, back: u8) -> u64 {
  head - u64::from(back) % (head - floor + 1)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(96),
    ..ProptestConfig::default()
  }))]

  /// A-534 (§4.17.1 item 4). Do: from a generated origin (files, directories, modes, symbolic links, hard links, IPC
  /// nodes and attributes), run generated histories over a lineage: works of every operation kind submitted to any
  /// green at its head or behind it, branches of any green at any version it reconstructs, folds and retires; feed each
  /// green's copying reference (a green seeded from a copy of its source's state at the branch's version) the same
  /// works. Expect: every outcome equal; after every step every green reads as its reference at every version (bases,
  /// files, lengths, pages, path bases, metadata, the change index, the checkpoint, the identity, the retained figures)
  /// and as it read when each version was its head; each branch walked no map and set back exactly the keys whose value
  /// at its version differs from the head's; no link ever named a missing node; and once every green is retired the
  /// lineage holds no node and no block.
  #[test]
  fn every_branch_reads_as_its_copy_over_every_history(
    origin in origin(),
    steps in proptest::collection::vec(step(), 0..40),
  ) {
    let (mut lineage, root) = Lineage::new(Green::with_origin(origin.clone()));
    let mut copy = Green::with_origin(origin);
    let mut recorded = BTreeMap::new();
    recorded.insert(0, snapshot(&mut copy));
    let mut greens = vec![Twin { handle: root, copy, recorded }];
    let mut ids = 0u64;
    for step in &steps {
      match step {
        Step::Submit { green, back, edits } => {
          let count = greens.len();
          let twin = &mut greens[green % count];
          let base = behind(twin.copy.head(), floor_of(&twin.copy), *back);
          ids += 1;
          let inc = work(&twin.copy, base, edits, ids);
          let expected = twin.copy.submit(&inc);
          let outcome = lineage.with(twin.handle, |green| green.submit(&inc)).unwrap();
          prop_assert_eq!(&outcome, &expected);
          if let Outcome::Accepted { version } = expected
            && !twin.recorded.contains_key(&version)
          {
            twin.recorded.insert(version, snapshot(&mut twin.copy));
          }
        }
        Step::Branch { from, back } => {
          if greens.len() >= GREENS {
            continue;
          }
          let count = greens.len();
          let source = &greens[from % count];
          let version = behind(source.copy.head(), floor_of(&source.copy), *back);
          let walked = lineage.walked();
          let reverted = lineage.reverted();
          let handle = lineage.branch(source.handle, version).unwrap();
          prop_assert_eq!(lineage.walked(), walked, "the branch walked a map");
          prop_assert_eq!(
            lineage.reverted() - reverted,
            differing_keys(&source.copy, version),
            "the branch did not set back exactly what changed after its version"
          );
          let mut copy = Green::with_origin(copy_at(&source.copy, version));
          let at_version = snapshot(&mut copy);
          prop_assert_eq!(&at_version, &source.recorded[&version], "the copy is not the source at its version");
          let mut recorded = BTreeMap::new();
          recorded.insert(0, at_version);
          greens.push(Twin { handle, copy, recorded });
        }
        Step::Fold { green, back } => {
          let count = greens.len();
          let twin = &mut greens[green % count];
          let floor = behind(twin.copy.head(), 0, *back);
          let released = twin.copy.fold_history_before(floor);
          let folded = lineage.with(twin.handle, |green| green.fold_history_before(floor)).unwrap();
          prop_assert_eq!(folded, released);
        }
        Step::Retire { green } => {
          if greens.len() < 2 {
            continue;
          }
          let count = greens.len();
          let twin = greens.remove(green % count);
          lineage.retire(twin.handle).unwrap();
          prop_assert_eq!(lineage.with(twin.handle, |_| ()), Err(LineageRefused::Stale));
        }
      }
      for twin in &mut greens {
        let expected = view(&mut twin.copy);
        let seen = lineage.with(twin.handle, view).unwrap();
        prop_assert_eq!(&seen, &expected);
        for at in &expected.versions {
          let then = &twin.recorded[&at.version];
          prop_assert_eq!(&at.base, &then.base, "version {} no longer reads as it was", at.version);
          prop_assert_eq!(&at.files, &then.files, "version {}'s files changed", at.version);
        }
      }
      prop_assert_eq!(lineage.missing(), 0);
    }
    for twin in greens {
      lineage.retire(twin.handle).unwrap();
    }
    prop_assert_eq!(lineage.live_nodes(), 0, "a lineage with every green retired holds nodes");
    prop_assert_eq!(lineage.arenas.blocks.live(), 0, "a lineage with every green retired holds blocks");
  }
}

/// The ten thousand files, their directories, a mode and an attribute of the cost test.
fn tree() -> Origin {
  let mut origin = Origin::default();
  for n in 0..10_000u32 {
    origin.files.push((
      format!("src/dir{}/file{n}.rs", n % 97),
      format!("the contents of file {n}").into_bytes(),
    ));
  }
  origin.dirs = (0..97).map(|n| format!("src/dir{n}")).collect();
  origin
    .modes
    .push(("src/dir1/file1.rs".to_owned(), 0o644));
  origin.xattrs.push((
    "src/dir2/file2.rs".to_owned(),
    "user.x".to_owned(),
    b"one".to_vec(),
  ));
  origin
}

/// A work over `green`'s head: `edits` as [`work`] builds them.
fn at_head(green: &Green, edits: &[Edit], id: u64) -> Increment {
  work(green, green.head(), edits, id)
}

/// A-534 (§4.17.1 item 4, "without walking it"). Do: seed a green with ten thousand files in ninety-seven directories, a
/// mode and an attribute; over three versions overwrite a file, then overwrite another and change the mode, then set
/// the attribute and remove a third file; make a lineage of it and branch it at version 1, then at its head. Expect:
/// neither branch visits a node of any map; the branch at version 1 sets back exactly the four keys changed after it
/// (two files, the mode, the attribute) and makes no more nodes than its path copies and inserts can (per key: one a
/// byte of its key, each edge holding at least one, the map's root, and the leaf and split an insert adds); the branch
/// at the head sets back nothing and makes no node; and each reads as a green seeded from a copy of the source at its
/// version.
#[test]
fn a_branch_walks_no_map_and_sets_back_only_what_changed_after_its_version() {
  let mut source = Green::with_origin(tree());
  let edits: [Vec<(OpKind, &str, Vec<u8>)>; 3] = [
    vec![(OpKind::Overwrite, "src/dir5/file5.rs", b"FIVE".to_vec())],
    vec![
      (OpKind::Overwrite, "src/dir6/file6.rs", b"SIX".to_vec()),
      (OpKind::SetMode, "src/dir1/file1.rs", Vec::new()),
    ],
    vec![
      (OpKind::SetXattr, "src/dir2/file2.rs", b"two".to_vec()),
      (OpKind::Unlink, "src/dir7/file7.rs", Vec::new()),
    ],
  ];
  for (version, ops) in edits.iter().enumerate() {
    let mut work = Work::default();
    for (kind, path, bytes) in ops {
      match kind {
        OpKind::Overwrite => {
          let src = work.stash(bytes);
          work.op(OpKind::Overwrite, path, 0, bytes.len() as u64, src);
        }
        OpKind::SetMode => work.op(OpKind::SetMode, path, 0, 0o755, 0),
        OpKind::SetXattr => {
          let name = work.name("user.x");
          let src = work.stash(bytes);
          work.op(OpKind::SetXattr, path, name, bytes.len() as u64, src);
        }
        _ => work.op(*kind, path, 0, 0, u64::MAX),
      }
    }
    let inc = Increment {
      id: [u8::try_from(version).unwrap() + 1; 32],
      base: version as u64,
      doc: work.doc,
      post_state: work.post,
      evidence: Vec::new(),
    };
    assert_eq!(
      source.submit(&inc),
      Outcome::Accepted {
        version: version as u64 + 1
      }
    );
  }
  let mut at_one = Green::with_origin(copy_at(&source, 1));
  let mut at_head = Green::with_origin(copy_at(&source, 3));
  let (expected_one, expected_head) = (view(&mut at_one), view(&mut at_head));
  // An attribute's key is its path, the two bytes that end a path, and its name (`crate::dims`).
  let changed_keys = [
    "src/dir6/file6.rs".len(),
    "src/dir1/file1.rs".len(),
    "src/dir2/file2.rs".len() + 2 + "user.x".len(),
    "src/dir7/file7.rs".len(),
  ];
  let (mut lineage, root) = Lineage::new(source);
  let (walked, live) = (lineage.walked(), lineage.live_nodes());
  let one = lineage.branch(root, 1).unwrap();
  assert_eq!(lineage.walked(), walked, "the branch walked a map");
  assert_eq!(lineage.reverted(), 4, "the keys changed after version 1");
  let made = lineage.live_nodes() - live;
  let bound: usize = changed_keys.iter().map(|len| len + 3).sum();
  assert!(
    made <= bound,
    "the branch made {made} nodes, its keys bound it at {bound}"
  );
  let head = lineage.branch(root, 3).unwrap();
  assert_eq!(lineage.walked(), walked, "the branch at the head walked a map");
  assert_eq!(
    lineage.reverted(),
    4,
    "the branch at the head set something back"
  );
  assert_eq!(
    lineage.live_nodes() - live,
    made,
    "the branch at the head made a node"
  );
  assert_eq!(lineage.with(one, view).unwrap(), expected_one);
  assert_eq!(lineage.with(head, view).unwrap(), expected_head);
}

/// A-534. Do: branch a green at its head, then write one file differently in the branch and in its source, remove one
/// in the branch only, and read both. Expect: each reads its own writes and never the other's, the writes copied shared
/// nodes (the copy count moved), and the source's identity is what a green fed only the source's works holds.
#[test]
fn changes_to_a_branch_and_its_source_never_cross() {
  let mut origin = Origin::default();
  for name in ["a", "a.b", "d/e", "d/f"] {
    origin.files.push((name.to_owned(), name.as_bytes().to_vec()));
  }
  let mut alone = Green::with_origin(origin.clone());
  let (mut lineage, source) = Lineage::new(Green::with_origin(origin));
  let branch = lineage.branch(source, 0).unwrap();
  let copied = lineage.copied();
  let into_source = at_head(&alone, &[Edit::Overwrite(2, 0, b"SS".to_vec())], 1);
  assert_eq!(
    lineage.with(source, |green| green.submit(&into_source)).unwrap(),
    Outcome::Accepted { version: 1 }
  );
  assert_eq!(alone.submit(&into_source), Outcome::Accepted { version: 1 });
  let into_branch = at_head(
    &alone,
    &[Edit::Overwrite(2, 0, b"BB".to_vec()), Edit::Unlink(1)],
    2,
  );
  let into_branch = Increment {
    base: 0,
    ..into_branch
  };
  assert_eq!(
    lineage.with(branch, |green| green.submit(&into_branch)).unwrap(),
    Outcome::Accepted { version: 1 }
  );
  assert!(lineage.copied() > copied, "the writes copied no shared node");
  let read = |lineage: &mut Lineage, green: LineageGreen, path: &str| {
    lineage
      .with(green, |green| green.content(path).map(<[u8]>::to_vec))
      .unwrap()
  };
  assert_eq!(read(&mut lineage, source, "d/e"), Some(b"SSe".to_vec()));
  assert_eq!(read(&mut lineage, branch, "d/e"), Some(b"BBe".to_vec()));
  assert_eq!(read(&mut lineage, source, "a.b"), Some(b"a.b".to_vec()));
  assert_eq!(read(&mut lineage, branch, "a.b"), None);
  assert_eq!(
    lineage.with(source, Green::head_identity).unwrap(),
    alone.head_identity()
  );
}

/// A-534. Do: branch a green past its head, below its fold floor, and through the handle of a green retired before.
/// Expect: each refused by its kind (`PastHead`, `Folded`, `Stale`), and a refused branch adds no green and sets nothing
/// back.
#[test]
fn a_branch_past_the_head_below_the_floor_or_of_a_retired_green_is_refused_by_its_kind() {
  let mut origin = Origin::default();
  origin.files.push(("a".to_owned(), b"0123".to_vec()));
  let (mut lineage, root) = Lineage::new(Green::with_origin(origin));
  for id in 1..=3u64 {
    let inc = lineage
      .with(root, |green| {
        at_head(green, &[Edit::Overwrite(0, 0, vec![b'0' + u8::try_from(id).unwrap()])], id)
      })
      .unwrap();
    assert_eq!(
      lineage.with(root, |green| green.submit(&inc)).unwrap(),
      Outcome::Accepted { version: id }
    );
  }
  assert_eq!(
    lineage.branch(root, 4),
    Err(LineageRefused::PastHead {
      version: 4,
      head: 3
    })
  );
  lineage
    .with(root, |green| green.fold_history_before(2))
    .unwrap();
  assert_eq!(
    lineage.branch(root, 1),
    Err(LineageRefused::Folded {
      version: 1,
      floor: 2
    })
  );
  assert_eq!(lineage.reverted(), 0);
  let branch = lineage.branch(root, 2).unwrap();
  assert_eq!(lineage.reverted(), 1, "the file changed at version 3");
  lineage.retire(branch).unwrap();
  assert_eq!(lineage.branch(branch, 0), Err(LineageRefused::Stale));
  assert_eq!(lineage.retire(branch), Err(LineageRefused::Stale));
  let again = lineage.branch(root, 3).unwrap();
  assert_ne!(again, branch, "a reused seat is a new handle");
  assert_eq!(lineage.with(branch, |_| ()), Err(LineageRefused::Stale));
}

/// One version's ops for [`a_branch_sets_back_every_dimension`]: each `(kind, path, argument)`, the argument a target or
/// attribute name where the kind takes one, else empty.
fn version_of(green: &Green, id: u8, ops: &[(OpKind, &str, &str)]) -> Increment {
  let mut work = Work::default();
  for (kind, path, argument) in ops {
    match kind {
      OpKind::Overwrite => {
        let src = work.stash(b"NEW");
        work.op(*kind, path, 0, 3, src);
      }
      OpKind::SetMode => work.op(*kind, path, 0, 0o755, 0),
      OpKind::Symlink | OpKind::Link => {
        let src = work.name(argument);
        work.op(*kind, path, 0, 0, src);
      }
      OpKind::SetXattr => {
        let name = work.name(argument);
        let src = work.stash(b"2");
        work.op(*kind, path, name, 1, src);
      }
      OpKind::RemoveXattr => {
        let name = work.name(argument);
        work.op(*kind, path, name, 0, 0);
      }
      OpKind::Mknod => {
        let node = fifo(2).encode();
        let src = work.stash(&node);
        work.op(*kind, path, 0, node.len() as u64, src);
      }
      _ => work.op(*kind, path, 0, 0, u64::MAX),
    }
  }
  Increment {
    id: [id; 32],
    base: green.head(),
    doc: work.doc,
    post_state: work.post,
    evidence: Vec::new(),
  }
}

/// A-534. Do: seed a green with a file of each kind of metadata (a mode, an attribute, a symbolic and a hard link naming
/// it), a file to remove, a directory, an IPC node and a second file's attribute; at version 2 overwrite the file,
/// remove the other, make a directory, remove the first and retarget the symbolic link; at version 3 change the mode,
/// retarget the hard link, remove the second file's attribute and replace the IPC node; at version 4 change the file's
/// attribute; then branch at versions 1, 2 and 4. Expect: each branch reads as a green seeded from a copy of the source
/// at its version, and sets back exactly the keys changed after it (ten, five and none) without a walk.
#[test]
fn a_branch_sets_back_every_dimension() {
  let origin = Origin {
    files: vec![
      ("f".to_owned(), b"0123".to_vec()),
      ("gone".to_owned(), b"x".to_vec()),
      ("keep".to_owned(), b"k".to_vec()),
    ],
    dirs: vec!["dir1".to_owned()],
    modes: vec![("f".to_owned(), 0o644)],
    symlinks: vec![("s".to_owned(), "f".to_owned())],
    hardlinks: vec![("h".to_owned(), "f".to_owned())],
    specials: vec![("p".to_owned(), fifo(1))],
    xattrs: vec![
      ("f".to_owned(), "user.x".to_owned(), b"1".to_vec()),
      ("keep".to_owned(), "user.y".to_owned(), b"y".to_vec()),
    ],
  };
  let mut source = Green::with_origin(origin);
  let versions: [&[(OpKind, &str, &str)]; 4] = [
    &[(OpKind::Mkdir, "unrelated", "")],
    &[
      (OpKind::Overwrite, "f", ""),
      (OpKind::Unlink, "gone", ""),
      (OpKind::Mkdir, "dir2", ""),
      (OpKind::Rmdir, "dir1", ""),
      (OpKind::Symlink, "s", "keep"),
    ],
    &[
      (OpKind::SetMode, "f", ""),
      (OpKind::Link, "h", "keep"),
      (OpKind::RemoveXattr, "keep", "user.y"),
      (OpKind::Mknod, "p", ""),
    ],
    &[(OpKind::SetXattr, "f", "user.x")],
  ];
  for (index, ops) in versions.iter().enumerate() {
    let id = u8::try_from(index).unwrap() + 1;
    let inc = version_of(&source, id, ops);
    assert_eq!(
      source.submit(&inc),
      Outcome::Accepted {
        version: u64::from(id)
      },
      "version {id}"
    );
  }
  let mut expected: Vec<View> = [1, 2, 4]
    .into_iter()
    .map(|version| view(&mut Green::with_origin(copy_at(&source, version))))
    .collect();
  let (mut lineage, root) = Lineage::new(source);
  for ((version, reverts), expected) in [(1, 10), (2, 5), (4, 0)].into_iter().zip(expected.drain(..)) {
    let (walked, before) = (lineage.walked(), lineage.reverted());
    let branch = lineage.branch(root, version).unwrap();
    assert_eq!(lineage.walked(), walked, "the branch at {version} walked a map");
    assert_eq!(
      lineage.reverted() - before,
      reverts,
      "the keys changed after {version}"
    );
    assert_eq!(lineage.with(branch, view).unwrap(), expected, "the branch at {version}");
  }
}

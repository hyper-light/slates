//! A refused namespace verb changes nothing (AUD-29-40; §4.2, §4.5, T-1.9). Every create, mknod,
//! mkdir, symlink, hard link and rename is driven against a store whose capacity in one dimension —
//! the volume's inode or entry allowance, or the shard's directory-node, directory-block or inode slab
//! — stops it at each of its allocation steps in turn, with and without a snapshot (which makes the
//! verb copy up its path first). Each refusal must be typed and leave the namespace, every attribute,
//! the volume's usage and accounting, the journal and every slab's usage as they were; a repeat must
//! refuse the same way; and once the dimension has room the verb succeeds, so a refusal never strands
//! capacity.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

mod common;

use common::{PAGE, REGION_PAGES, volume};
use slates_mem::Handle;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::dir::{Child, DirNode};
use slates_vfs::error::VfsError;
use slates_vfs::ids::SnapshotId;
use slates_vfs::inode::Kind;
use slates_vfs::volume::{Store, StoreConfig, Volume};
use slates_vfs::xattr::XattrSet;

/// Shape: the generous cap of every slab dimension not under test — far above what a scenario uses.
const ROOMY: usize = 1 << 16;
/// Shape: the small-directory cut-over: four entries, so the scenario's directories of five or more
/// names are indexed trees and both representations are exercised.
const CUTOVER: usize = 4;
/// Shape: the volume's quota: far above the scenario's bytes, so quota never refuses here.
const QUOTA: u64 = 1 << 24;
/// Shape: the most extra slots any verb here needs past the scenario's own use: a path copy of a few
/// directories, their inodes and trie paths, and a split per tree level. A verb still refused at this
/// much room has stranded capacity, which fails the test.
const MOST_EXTRA_SLOTS: usize = 64;
/// Shape: a write past the inline bound (two pages), so it takes a content chunk.
const CHUNKED_WRITE: usize = 2 * PAGE;
/// Shape: names in the wide directory: enough that its tree has split into more than one leaf, so an
/// insert there can need a split and a copy of more than one block.
const WIDE_ENTRIES: usize = 300;

/// The dimension a scenario's capacity is tight in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dimension {
  DirectoryNodes,
  DirectoryBlocks,
  /// Inode versions and inode-table nodes share one cap (`StoreConfig::max_inodes`).
  Inodes,
  /// Content chunk records.
  Chunks,
}

const DIMENSIONS: [Dimension; 4] = [
  Dimension::DirectoryNodes,
  Dimension::DirectoryBlocks,
  Dimension::Inodes,
  Dimension::Chunks,
];

/// The verbs under test, each on the scenario's names.
#[derive(Clone, Copy, Debug)]
enum Verb {
  Create,
  CreateInWide,
  Mknod,
  Mkdir,
  Symlink,
  Link,
  RenameAcross,
  RenameWithin,
  RenameOver,
  Unlink,
  Rmdir,
  Chmod,
  WriteInline,
  WriteChunked,
  Truncate,
  SetXattr,
}

const VERBS: [Verb; 16] = [
  Verb::Create,
  Verb::CreateInWide,
  Verb::Mknod,
  Verb::Mkdir,
  Verb::Symlink,
  Verb::Link,
  Verb::RenameAcross,
  Verb::RenameWithin,
  Verb::RenameOver,
  Verb::Unlink,
  Verb::Rmdir,
  Verb::Chmod,
  Verb::WriteInline,
  Verb::WriteChunked,
  Verb::Truncate,
  Verb::SetXattr,
];

/// A store over one RAM region with the chosen slab caps.
fn store_capped(max_dirs: usize, max_dir_blocks: usize, max_inodes: usize) -> Store {
  store_capped_chunks(max_dirs, max_dir_blocks, max_inodes, ROOMY)
}

/// [`store_capped`] with the chunk-record cap chosen too.
fn store_capped_chunks(
  max_dirs: usize,
  max_dir_blocks: usize,
  max_inodes: usize,
  max_chunks: usize,
) -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs,
      max_inodes,
      max_chunks,
      max_dir_blocks,
      dir_cutover: CUTOVER,
    },
    arena,
    0,
  )
}

/// The slots a store holds in each dimension, and the content bytes its arena has handed out.
fn used(store: &Store) -> [usize; 5] {
  [
    store.dirs.len(),
    store.blocks.len(),
    store.inodes.len().max(store.tries.len()),
    store.content.chunks(),
    store.content.allocated_bytes(),
  ]
}

fn index_of(dimension: Dimension) -> usize {
  match dimension {
    Dimension::DirectoryNodes => 0,
    Dimension::DirectoryBlocks => 1,
    Dimension::Inodes => 2,
    Dimension::Chunks => 3,
  }
}

/// A store whose `dimension` is capped at `cap` and every other is roomy.
fn store_for(dimension: Dimension, cap: usize) -> Store {
  match dimension {
    Dimension::DirectoryNodes => store_capped(cap, ROOMY, ROOMY),
    Dimension::DirectoryBlocks => store_capped(ROOMY, cap, ROOMY),
    Dimension::Inodes => store_capped(ROOMY, ROOMY, cap),
    Dimension::Chunks => store_capped_chunks(ROOMY, ROOMY, ROOMY, cap),
  }
}

/// `/a` (holding `f`), `/b`, the file `/g`, the symlink `/s`, and `/wide` with [`WIDE_ENTRIES`]
/// files; then, when asked, a snapshot. Refused when the store's caps cannot hold the scenario itself
/// (a split's worst case is reserved before it is known to be needed).
fn scenario(store: &mut Store, snapshot: bool) -> Result<(Volume, Option<SnapshotId>), VfsError> {
  let mut vol = volume(store, QUOTA);
  let root = vol.root();
  let a = vol.mkdir(store, root, "a", 0o755)?;
  vol.mkdir(store, root, "b", 0o755)?;
  let f = vol.create_file(store, a, "f", 0o644)?;
  vol.write(store, f, 0, b"f")?;
  let g = vol.create_file(store, root, "g", 0o644)?;
  vol.write(store, g, 0, b"g")?;
  vol.symlink(store, root, "s", "/g")?;
  let wide = vol.mkdir(store, root, "wide", 0o755)?;
  for n in 0..WIDE_ENTRIES {
    vol.create_file(store, wide, &format!("entry-{n:06}"), 0o644)?;
  }
  let snap = if snapshot {
    Some(vol.snapshot(store)?)
  } else {
    None
  };
  Ok((vol, snap))
}

fn dir(vol: &Volume, store: &Store, path: &str) -> Handle<DirNode> {
  if path == "/" {
    return vol.root();
  }
  match vol.resolve(store, path).unwrap().child {
    Child::Dir(handle) => handle,
    other => panic!("{path} is not a directory: {other:?}"),
  }
}

fn run(verb: Verb, vol: &mut Volume, store: &mut Store) -> Result<(), VfsError> {
  let root = vol.root();
  match verb {
    Verb::Create => vol.create_file(store, root, "new", 0o644).map(drop),
    Verb::CreateInWide => {
      let wide = dir(vol, store, "/wide");
      vol.create_file(store, wide, "a-new-name", 0o644).map(drop)
    }
    Verb::Mknod => {
      let a = vol.resolve(store, "/a").unwrap().inode;
      vol.mknod_no(store, a, "pipe", 0o644, Kind::Fifo).map(drop)
    }
    Verb::Mkdir => vol.mkdir(store, root, "newdir", 0o755).map(drop),
    Verb::Symlink => vol.symlink(store, root, "newlink", "/a/f").map(drop),
    Verb::Link => {
      let g = vol.resolve(store, "/g").unwrap().inode;
      vol.link(store, root, "hard", g)
    }
    Verb::RenameAcross => {
      let b = dir(vol, store, "/b");
      vol.rename(store, root, "g", b, "moved")
    }
    Verb::RenameWithin => {
      let a = dir(vol, store, "/a");
      vol.rename(store, a, "f", a, "renamed")
    }
    Verb::RenameOver => vol.rename(store, root, "g", root, "s"),
    Verb::Unlink => {
      let a = dir(vol, store, "/a");
      vol.unlink(store, a, "f")
    }
    Verb::Rmdir => vol.rmdir(store, root, "b"),
    Verb::Chmod => {
      let g = vol.resolve(store, "/g").unwrap().inode;
      vol.chmod(store, g, 0o600)
    }
    Verb::WriteInline => {
      let g = vol.resolve(store, "/g").unwrap().inode;
      vol.write(store, g, 1, b"+").map(drop)
    }
    Verb::WriteChunked => {
      let g = vol.resolve(store, "/g").unwrap().inode;
      vol.write(store, g, 0, &[7u8; CHUNKED_WRITE]).map(drop)
    }
    Verb::Truncate => {
      let g = vol.resolve(store, "/g").unwrap().inode;
      vol.truncate(store, g, 0)
    }
    Verb::SetXattr => {
      let g = vol.resolve(store, "/g").unwrap().inode;
      vol.xattr_set(store, g, b"user.key", b"value", XattrSet::Either)
    }
  }
}

/// Everything a client can observe of the volume: every path with its kind and full attributes,
/// file and symlink contents, the inode and entry usage, the byte accounting, and (with a snapshot)
/// the journal since it.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
  paths: Vec<String>,
  inode_usage: (u64, u64),
  entry_usage: (u64, u64),
  accounting: String,
  journal: Option<String>,
}

fn observe(vol: &Volume, store: &Store, snap: Option<SnapshotId>) -> Observed {
  let mut paths = Vec::new();
  let mut stack = vec![(String::new(), vol.root())];
  while let Some((prefix, handle)) = stack.pop() {
    let rows: Vec<_> = vol
      .readdir(store, handle)
      .unwrap()
      .iter()
      .map(|row| (row.name.to_owned(), row.kind, row.inode))
      .collect();
    for (name, kind, no) in rows {
      let path = format!("{prefix}/{name}");
      let attrs = vol.stat(store, no).unwrap();
      let body = match kind {
        Kind::File => {
          let mut buf = vec![0u8; usize::try_from(attrs.size).unwrap()];
          let read = vol.read(store, no, 0, &mut buf).unwrap();
          buf.truncate(read);
          format!("{buf:?}")
        }
        _ => String::new(),
      };
      paths.push(format!("{path} {kind:?} {no:?} {attrs:?} {body}"));
      if kind == Kind::Dir {
        stack.push((path.clone(), dir(vol, store, &path)));
      }
    }
  }
  paths.sort();
  Observed {
    paths,
    inode_usage: vol.inode_usage(),
    entry_usage: vol.entry_usage(),
    accounting: format!("{:?}", vol.accounting()),
    journal: snap.map(|id| format!("{:?}", vol.records_since(id).unwrap())),
  }
}

/// What differs between two observations, line by line, for a failure message.
fn difference(after: &Observed, before: &Observed) -> String {
  let mut out = Vec::new();
  for line in after
    .paths
    .iter()
    .filter(|line| !before.paths.contains(line))
  {
    out.push(format!("+ {line}"));
  }
  for line in before
    .paths
    .iter()
    .filter(|line| !after.paths.contains(line))
  {
    out.push(format!("- {line}"));
  }
  if after.inode_usage != before.inode_usage {
    out.push(format!(
      "inodes {:?} -> {:?}",
      before.inode_usage, after.inode_usage
    ));
  }
  if after.entry_usage != before.entry_usage {
    out.push(format!(
      "entries {:?} -> {:?}",
      before.entry_usage, after.entry_usage
    ));
  }
  if after.accounting != before.accounting {
    out.push(format!(
      "accounting {} -> {}",
      before.accounting, after.accounting
    ));
  }
  if after.journal != before.journal {
    out.push(format!(
      "journal {:?} -> {:?}",
      before.journal, after.journal
    ));
  }
  out.join("\n")
}

/// Asserts nothing a client sees changed.
fn unchanged(after: &Observed, before: &Observed, context: &str) {
  assert!(
    after == before,
    "{context}: changed what a client sees:\n{}",
    difference(after, before)
  );
}

fn typed_capacity_refusal(refusal: &VfsError) -> bool {
  matches!(refusal, VfsError::NoSpace | VfsError::Memory(_))
}

/// One verb against one tight dimension, at every step: the cap starts at the scenario's own use and
/// rises a slot at a time until the verb succeeds.
fn every_step_of(verb: Verb, dimension: Dimension, snapshot: bool) -> usize {
  let mut probe = store_capped(ROOMY, ROOMY, ROOMY);
  scenario(&mut probe, snapshot).unwrap();
  let own = used(&probe)[index_of(dimension)];
  let mut refusals = 0;
  for extra in 0..=MOST_EXTRA_SLOTS {
    let cap = own + extra;
    let mut store = store_for(dimension, cap);
    let Ok((mut vol, snap)) = scenario(&mut store, snapshot) else {
      // The cap is too tight for the scenario itself; the verb is reached at a larger one.
      continue;
    };
    let before = observe(&vol, &store, snap);
    let held = used(&store);
    let first = run(verb, &mut vol, &mut store);
    let Err(refusal) = first else {
      return refusals;
    };
    refusals += 1;
    let context = format!("{verb:?} with {dimension:?} at +{extra}, snapshot {snapshot}");
    assert!(
      typed_capacity_refusal(&refusal),
      "{context}: an untyped refusal {refusal:?}"
    );
    unchanged(
      &observe(&vol, &store, snap),
      &before,
      &format!("{context}: the refusal {refusal:?}"),
    );
    assert_eq!(
      used(&store),
      held,
      "{context}: the refusal {refusal:?} took slab slots"
    );
    let again = run(verb, &mut vol, &mut store);
    assert_eq!(
      again.as_ref().err(),
      Some(&refusal),
      "{context}: a repeat refused differently"
    );
    assert_eq!(
      used(&store),
      held,
      "{context}: a repeated refusal consumed capacity"
    );
    unchanged(
      &observe(&vol, &store, snap),
      &before,
      &format!("{context}: on repeat"),
    );
  }
  panic!(
    "{verb:?} with {dimension:?} (snapshot {snapshot}) was still refused at +{MOST_EXTRA_SLOTS} slots"
  );
}

/// AUD-29-40, T-1.9. Do: inject a capacity refusal at every allocation step of every namespace verb
/// (each slab dimension tightened from the scenario's own use upward), with and without a snapshot.
/// Expect: each refusal is typed and changes nothing a client can see, a repeat consumes nothing, and
/// the verb succeeds once the dimension has room.
#[test]
fn a_namespace_verb_refused_at_any_allocation_changes_nothing() {
  let mut refusals = 0;
  for verb in VERBS {
    for dimension in DIMENSIONS {
      for snapshot in [false, true] {
        let here = every_step_of(verb, dimension, snapshot);
        eprintln!("{verb:?} {dimension:?} snapshot {snapshot}: {here} refusals");
        refusals += here;
      }
    }
  }
  // Non-vacuity: the sweep must have refused at many steps, or it proved nothing.
  eprintln!("{refusals} refusals injected over the scratch scenario");
  assert!(
    refusals >= VERBS.len(),
    "only {refusals} refusals were injected"
  );
}

/// AUD-29-40 (the audit's reproduction). Do: on a volume with an inode allowance of four and an entry
/// allowance of zero, create three times; then allow one entry and create. Expect: each refused create
/// leaves the inode usage at one of four, and the create with an entry to spare succeeds.
#[test]
fn a_create_refused_for_its_entry_returns_its_inode_charge() {
  let mut store = store_capped(ROOMY, ROOMY, ROOMY);
  let mut vol = volume(&mut store, QUOTA);
  let root = vol.root();
  vol.set_inode_allowance(4).unwrap();
  vol.set_entry_allowance(0).unwrap();
  for name in ["one", "two", "three"] {
    assert_eq!(
      vol.create_file(&mut store, root, name, 0o644),
      Err(VfsError::NoSpace)
    );
    assert_eq!(vol.inode_usage(), (1, 4), "after refusing {name}");
  }
  vol.set_entry_allowance(1).unwrap();
  vol.create_file(&mut store, root, "four", 0o644).unwrap();
  assert_eq!(vol.inode_usage(), (2, 4));
  assert!(vol.lookup(&store, root, "four").is_ok());
}

/// AUD-29-40. Do: tighten each volume allowance to exactly the volume's use and run every verb, then
/// raise it by one and run the verb again. Expect: a verb that needs the dimension is refused
/// `NoSpace` with nothing changed, and succeeds once one more is allowed.
#[test]
fn a_verb_refused_by_an_allowance_changes_nothing_and_succeeds_with_room() {
  for verb in VERBS {
    for snapshot in [false, true] {
      for allowance in ["inode", "entry"] {
        let mut store = store_capped(ROOMY, ROOMY, ROOMY);
        let (mut vol, snap) = scenario(&mut store, snapshot).unwrap();
        let set = |vol: &mut Volume, extra: u64| match allowance {
          "inode" => vol.set_inode_allowance(vol.inode_usage().0 + extra),
          _ => vol.set_entry_allowance(vol.entry_usage().0 + extra),
        };
        set(&mut vol, 0).unwrap();
        let before = observe(&vol, &store, snap);
        match run(verb, &mut vol, &mut store) {
          Ok(()) => continue, // this verb takes nothing of this dimension (a rename's entry)
          Err(refusal) => {
            assert_eq!(
              refusal,
              VfsError::NoSpace,
              "{verb:?} at the {allowance} allowance"
            );
            unchanged(
              &observe(&vol, &store, snap),
              &before,
              &format!("{verb:?} at the {allowance} allowance (snapshot {snapshot})"),
            );
          }
        }
        set(&mut vol, 1).unwrap();
        run(verb, &mut vol, &mut store)
          .unwrap_or_else(|refusal| panic!("{verb:?} with one {allowance} to spare: {refusal:?}"));
      }
    }
  }
}

// ---------------------------------------------------------------- base entries (an overlay volume)

use slates_vfs::base::{BaseConfig, Overlay};
use slates_vfs::clock::StepClock;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::VolumeConfig;

/// Shape: the overlay's large-file class, one chunk window, as the base tests use (`tests/base.rs`).
const LARGE: u64 = 65_536;

/// The verbs on base (disk) entries of an overlay.
#[derive(Clone, Copy, Debug)]
enum BaseVerb {
  Create,
  Mknod,
  Mkdir,
  Symlink,
  LinkBaseFile,
  RenameBaseFile,
  RenameBaseFileOver,
  RenameBaseDirectory,
  RenameWithinBase,
  UnlinkBaseFile,
  RmdirBase,
}

const BASE_VERBS: [BaseVerb; 11] = [
  BaseVerb::Create,
  BaseVerb::Mknod,
  BaseVerb::Mkdir,
  BaseVerb::Symlink,
  BaseVerb::LinkBaseFile,
  BaseVerb::RenameBaseFile,
  BaseVerb::RenameBaseFileOver,
  BaseVerb::RenameBaseDirectory,
  BaseVerb::RenameWithinBase,
  BaseVerb::UnlinkBaseFile,
  BaseVerb::RmdirBase,
];

/// The disk: `/d0` holding `f0` and `f1`, `/d1` holding `g`, and the symlink `/s`.
fn disk() -> SimHost {
  let mut host = SimHost::new();
  host.mkdir("/d0");
  host.mkdir("/d1");
  host.replace_file("/d0/f0", b"zero");
  host.replace_file("/d0/f1", b"one");
  host.replace_file("/d1/g", b"gee");
  host.symlink("/s", "/d0/f0");
  host
}

/// An overlay of the disk, every directory loaded by one walk (so observing again loads nothing
/// new), then, when asked, a snapshot and a second walk.
fn base_scenario(
  host: &mut SimHost,
  store: &mut Store,
  snapshot: bool,
) -> Result<(Volume, Option<SnapshotId>), VfsError> {
  let root = host.root();
  let facts = host.facts(root).map_err(|_| VfsError::Invalid)?;
  let mut vol = Volume::create_overlay(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: QUOTA },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(1_000_000, 1_000)),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )?;
  observe_overlay(&mut vol.with_host(host), store)?;
  let snap = if snapshot {
    let id = vol.snapshot(store)?;
    // A read after a snapshot copies up the base entries it observes (the overlay records what it
    // saw), so the scenario is walked again to settle that before any verb runs.
    observe_overlay(&mut vol.with_host(host), store)?;
    Some(id)
  } else {
    None
  };
  Ok((vol, snap))
}

fn overlay_dir(
  o: &mut Overlay<'_>,
  store: &mut Store,
  path: &str,
) -> Result<Handle<DirNode>, VfsError> {
  match o.resolve(store, path)?.child {
    Child::Dir(handle) => Ok(handle),
    _ => Err(VfsError::NotDirectory),
  }
}

fn run_base(verb: BaseVerb, o: &mut Overlay<'_>, store: &mut Store) -> Result<(), VfsError> {
  let d0 = overlay_dir(o, store, "/d0")?;
  let d1 = overlay_dir(o, store, "/d1")?;
  let root = overlay_dir(o, store, "/")?;
  match verb {
    BaseVerb::Create => o.create_file(store, d0, "new", 0o644).map(drop),
    BaseVerb::Mknod => {
      let d1_no = o.resolve(store, "/d1")?.inode;
      o.mknod_no(store, d1_no, "pipe", 0o644, Kind::Fifo)
        .map(drop)
    }
    BaseVerb::Mkdir => o.mkdir(store, d0, "sub", 0o755).map(drop),
    BaseVerb::Symlink => o.symlink(store, d0, "ln", "/d1/g").map(drop),
    BaseVerb::LinkBaseFile => {
      let f0 = o.resolve(store, "/d0/f0")?.inode;
      o.link(store, d1, "hard", f0)
    }
    BaseVerb::RenameBaseFile => o.rename(store, d0, "f0", d1, "moved"),
    BaseVerb::RenameBaseFileOver => o.rename(store, d0, "f0", d1, "g"),
    BaseVerb::RenameBaseDirectory => o.rename(store, root, "d1", d0, "moved-dir"),
    BaseVerb::RenameWithinBase => o.rename(store, d0, "f1", d0, "f1-renamed"),
    BaseVerb::UnlinkBaseFile => o.unlink(store, d0, "f1"),
    BaseVerb::RmdirBase => {
      // `/d1` holds the base file `g`; empty it first so the rmdir itself is the verb under test.
      o.unlink(store, d1, "g")?;
      o.rmdir(store, root, "d1")
    }
  }
}

/// What a client sees through the overlay: every path with its kind and attributes, and file
/// contents.
fn observe_overlay(o: &mut Overlay<'_>, store: &mut Store) -> Result<Vec<String>, VfsError> {
  let mut paths = Vec::new();
  let mut stack = vec![String::new()];
  while let Some(prefix) = stack.pop() {
    let handle = overlay_dir(o, store, if prefix.is_empty() { "/" } else { &prefix })?;
    let rows: Vec<_> = o
      .readdir(store, handle)?
      .iter()
      .map(|row| (row.name.to_owned(), row.kind, row.inode))
      .collect();
    for (name, kind, no) in rows {
      let path = format!("{prefix}/{name}");
      let attrs = o.stat(store, no)?;
      let body = if kind == Kind::File {
        let mut buf = vec![0u8; usize::try_from(attrs.size).unwrap_or(0)];
        let read = o.read(store, no, 0, &mut buf)?;
        buf.truncate(read);
        format!("{buf:?}")
      } else {
        String::new()
      };
      paths.push(format!("{path} {kind:?} {no:?} {attrs:?} {body}"));
      if kind == Kind::Dir {
        stack.push(path);
      }
    }
  }
  paths.sort();
  Ok(paths)
}

/// The overlay's whole observable state: the client view, the volume's usage and accounting, the
/// diverged set (witnesses, whiteouts, redirects) and the journal since the snapshot.
fn observe_base(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  snap: Option<SnapshotId>,
) -> Result<Observed, VfsError> {
  let mut paths = observe_overlay(&mut vol.with_host(host), store)?;
  paths.push(format!("diverged {:?}", vol.diverged(store)));
  Ok(Observed {
    paths,
    inode_usage: vol.inode_usage(),
    entry_usage: vol.entry_usage(),
    accounting: format!("{:?}", vol.accounting()),
    journal: snap.map(|id| format!("{:?}", vol.records_since(id).unwrap())),
  })
}

fn every_base_step_of(verb: BaseVerb, dimension: Dimension, snapshot: bool) -> usize {
  let mut probe = store_capped(ROOMY, ROOMY, ROOMY);
  base_scenario(&mut disk(), &mut probe, snapshot).unwrap();
  let own = used(&probe)[index_of(dimension)];
  let mut refusals = 0;
  for extra in 0..=MOST_EXTRA_SLOTS {
    let cap = own + extra;
    let mut store = store_for(dimension, cap);
    let mut host = disk();
    let Ok((mut vol, snap)) = base_scenario(&mut host, &mut store, snapshot) else {
      continue;
    };
    let before = observe_base(&mut vol, &mut host, &mut store, snap)
      .unwrap_or_else(|refusal| panic!("observing before {verb:?} at +{extra}: {refusal:?}"));
    let held = used(&store);
    let first = run_base(verb, &mut vol.with_host(&mut host), &mut store);
    let Err(refusal) = first else {
      return refusals;
    };
    refusals += 1;
    let context = format!("base {verb:?} with {dimension:?} at +{extra}, snapshot {snapshot}");
    assert!(
      typed_capacity_refusal(&refusal),
      "{context}: an untyped refusal {refusal:?}"
    );
    assert_eq!(
      used(&store),
      held,
      "{context}: the refusal {refusal:?} took slab slots"
    );
    let after = observe_base(&mut vol, &mut host, &mut store, snap).unwrap_or_else(|second| {
      panic!("{context}: observing after the refusal {refusal:?} was refused {second:?}")
    });
    unchanged(
      &after,
      &before,
      &format!("{context}: the refusal {refusal:?}"),
    );
  }
  panic!(
    "base {verb:?} with {dimension:?} (snapshot {snapshot}) was still refused at +{MOST_EXTRA_SLOTS} slots"
  );
}

/// AUD-29-40, T-1.9, with base entries. Do: inject a capacity refusal at every allocation step of
/// every namespace verb on an overlay's disk entries (a base file linked or renamed, a base directory
/// renamed, names created in a merged directory), with and without a snapshot. Expect: each refusal is
/// typed and changes nothing a client sees — names, attributes, contents, whiteouts, witnesses and
/// redirects, usage, accounting, journal — nor any slab's usage.
#[test]
fn a_verb_on_base_entries_refused_at_any_allocation_changes_nothing() {
  let mut refusals = 0;
  for verb in BASE_VERBS {
    for dimension in DIMENSIONS {
      for snapshot in [false, true] {
        refusals += every_base_step_of(verb, dimension, snapshot);
      }
    }
  }
  eprintln!("{refusals} refusals injected over base entries");
  assert!(
    refusals >= BASE_VERBS.len(),
    "only {refusals} refusals were injected"
  );
}

//! The deriver's tests (Phase 1 task 14): T-1.18, net-apply equals raw replay on generated
//! histories with every hunk inside its sources; T-1.19, a whole-file rewrite by either route
//! is one hunk and the documents are byte-identical; AC-1.15, the same journal gives the same
//! document bytes and identity, pinned by a golden identity the Linux and macOS lanes both
//! reproduce.

// Test harness code: an unwrap here is a failed test, which is what it should be. proptest's
// strategy types carry `Arc` (D-8's harness exception).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_types)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use slates_vfs::algebra::{Hunk, apply_hunks};
use slates_vfs::derive::OpsDocument;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::drive::{PathState, apply_volume, head_state, snapshot_state};
use common::steps::{Step, step};
use common::{store, volume_with};

/// Applies `steps` to the volume, picking files by the head's sorted paths.
fn drive(vol: &mut Volume, store: &mut Store, steps: &[Step]) {
  for step in steps {
    let files = head_state(vol, store).file_paths();
    let _ = apply_volume(step, vol, store, &files);
  }
}

/// The reference applier of a document over a base tree: the post-state's files by path,
/// taking new bytes from the head (the sealed post-state) at the hunks' post offsets.
fn apply_document(doc: &OpsDocument, base: &PathState, head: &PathState) -> Option<PathState> {
  let mut out = base.clone();
  // The document's one order (the deriver's module doc): renamed subtrees are detached first; the
  // removals outside every rename target apply (a rename over a removed directory reads right);
  // the subtrees attach at their new paths; then the removals beneath a rename target apply — they
  // are named by their post-rename paths, which exist only once the subtree is attached (a removal
  // inside a renamed directory reads right).
  // Renames nest: an inner directory renamed inside a renamed parent has its source in base
  // coordinates and its target in post-rename coordinates. Detaching the deepest source first
  // takes the inner subtree out before the outer subtree is taken (so the outer carries no stale
  // copy of it), and attaching the shallowest target first puts the outer subtree in place before
  // the inner one attaches beneath its new name.
  let mut renames: Vec<&(Box<str>, Box<str>)> = doc.dirs_renamed.iter().collect();
  renames.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
  let mut detached = Vec::new();
  for (from, to) in renames {
    detached.push((detach_dir(&mut out, from), to.clone()));
  }
  detached.sort_by_key(|(_, to)| to.len());
  let under_target = |p: &str| {
    doc.dirs_renamed.iter().any(|(_, to)| {
      p.strip_prefix(to.as_ref())
        .is_some_and(|r| r.starts_with('/'))
    })
  };
  remove_paths(&mut out, doc, |p| !under_target(p));
  for (subtree, to) in detached {
    attach_dir(&mut out, subtree, &to);
  }
  remove_paths(&mut out, doc, under_target);
  for s in &doc.symlinks {
    out
      .symlinks
      .insert(s.path.to_string(), s.target.to_string());
  }
  for d in &doc.dirs_created {
    out.dirs.push((d.to_string(), Vec::new()));
  }
  // A file the document names replaces whatever the base had at that path; the rest keep
  // their base bytes.
  for (path, bytes) in rebuild_files(doc, base, head)? {
    let nlink = head.files.get(&path).map_or(1, |(_, n)| *n);
    out.files.insert(path, (bytes, nlink));
  }
  Some(out)
}

/// A directory and everything beneath it, with paths relative to it (the directory itself
/// is the empty relative path).
type Subtree = (
  Vec<(String, Vec<(String, char)>)>,
  BTreeMap<String, (Vec<u8>, u64)>,
  BTreeMap<String, String>,
);

/// Takes a directory and everything beneath it out of the view.
fn detach_dir(out: &mut PathState, from: &str) -> Subtree {
  let prefix = format!("{from}/");
  let rel = |k: &str| -> Option<String> {
    if k == from {
      Some(String::new())
    } else {
      k.strip_prefix(&prefix).map(str::to_owned)
    }
  };
  let mut sub: Subtree = Default::default();
  out.files = std::mem::take(&mut out.files)
    .into_iter()
    .filter_map(|(k, v)| match rel(&k) {
      Some(r) => {
        sub.1.insert(r, v);
        None
      }
      None => Some((k, v)),
    })
    .collect();
  out.symlinks = std::mem::take(&mut out.symlinks)
    .into_iter()
    .filter_map(|(k, v)| match rel(&k) {
      Some(r) => {
        sub.2.insert(r, v);
        None
      }
      None => Some((k, v)),
    })
    .collect();
  out.dirs = std::mem::take(&mut out.dirs)
    .into_iter()
    .filter_map(|(k, v)| match rel(&k) {
      Some(r) => {
        sub.0.push((r, v));
        None
      }
      None => Some((k, v)),
    })
    .collect();
  sub
}

/// Puts a detached subtree back at `to`, replacing whatever was there.
fn attach_dir(out: &mut PathState, sub: Subtree, to: &str) {
  let abs = |r: &str| -> String {
    if r.is_empty() {
      to.to_owned()
    } else {
      format!("{to}/{r}")
    }
  };
  let prefix = format!("{to}/");
  out.files.retain(|k, _| k != to && !k.starts_with(&prefix));
  out
    .symlinks
    .retain(|k, _| k != to && !k.starts_with(&prefix));
  out.dirs.retain(|(k, _)| k != to && !k.starts_with(&prefix));
  for (r, v) in sub.1 {
    out.files.insert(abs(&r), v);
  }
  for (r, v) in sub.2 {
    out.symlinks.insert(abs(&r), v);
  }
  for (r, v) in sub.0 {
    out.dirs.push((abs(&r), v));
  }
}

/// Applies the document's removals whose path `select` accepts (the two phases of the one order).
fn remove_paths(out: &mut PathState, doc: &OpsDocument, select: impl Fn(&str) -> bool) {
  for p in doc.removed.iter().filter(|p| select(p)) {
    out.files.remove(p.as_ref());
    out.symlinks.remove(p.as_ref());
  }
  for d in doc.dirs_removed.iter().filter(|d| select(d)) {
    let prefix = format!("{d}/");
    out.files.retain(|k, _| !k.starts_with(&prefix));
    out.symlinks.retain(|k, _| !k.starts_with(&prefix));
    out
      .dirs
      .retain(|(k, _)| k != d.as_ref() && !k.starts_with(&prefix));
  }
}

fn rebuild_files(
  doc: &OpsDocument,
  base: &PathState,
  head: &PathState,
) -> Option<BTreeMap<String, Vec<u8>>> {
  let mut rebuilt = BTreeMap::new();
  for f in &doc.files {
    let base_bytes: Vec<u8> = match &f.base {
      Some(b) => base.files.get(b.path.as_ref())?.0.clone(),
      None => Vec::new(),
    };
    if let Some(b) = &f.base {
      assert_eq!(
        b.len,
        u64::try_from(base_bytes.len()).unwrap(),
        "base length of {}",
        b.path
      );
    }
    let post = &head.files.get(f.path.as_ref())?.0;
    let bytes = apply_hunks(&base_bytes, post, &f.hunks)?;
    assert_eq!(
      u64::try_from(bytes.len()).unwrap(),
      f.post_len,
      "post length of {}",
      f.path
    );
    rebuilt.insert(f.path.to_string(), bytes);
  }
  Some(rebuilt)
}

fn same_files(a: &PathState, b: &PathState) -> bool {
  a.files.keys().eq(b.files.keys())
    && a
      .files
      .iter()
      .all(|(k, (bytes, _))| b.files.get(k).map(|(x, _)| x) == Some(bytes))
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(300), max_shrink_iters: 2000, .. ProptestConfig::default() }))]

  /// T-1.18 and AC-1.15: on random histories over a random base, applying the derived
  /// document to the base reproduces the head's files, symlinks and directories byte for
  /// byte and path for path; every hunk lies inside its sources; deriving twice gives the
  /// same bytes and identity.
  #[test]
  fn net_apply_equals_raw_replay(
    prefix in prop::collection::vec(step(), 0..25),
    suffix in prop::collection::vec(step(), 1..25),
  ) {
    let mut store = store();
    let mut vol = volume_with(&mut store, Quota::Bounded { limit: 1 << 30 }, NameEquivalence::Exact);
    drive(&mut vol, &mut store, &prefix);
    let base = vol.snapshot(&mut store).unwrap();
    let base_state = snapshot_state(&vol, &store, base);
    drive(&mut vol, &mut store, &suffix);
    let head = head_state(&vol, &store);
    let doc = vol.derive(&store, base).unwrap();
    let again = vol.derive(&store, base).unwrap();
    prop_assert_eq!(doc.encode(), again.encode(), "deterministic");
    prop_assert_eq!(doc.identity(), again.identity());
    let applied = apply_document(&doc, &base_state, &head);
    let applied = applied.expect("every hunk inside its sources");
    prop_assert!(same_files(&applied, &head), "files: applied {:?} head {:?} doc {:?}", applied.files.keys().collect::<Vec<_>>(), head.files.keys().collect::<Vec<_>>(), doc);
    prop_assert_eq!(&applied.symlinks, &head.symlinks, "symlinks {:?}", doc);
    let mut applied_dirs = applied.dir_paths();
    applied_dirs.sort();
    let mut head_dirs = head.dir_paths();
    head_dirs.sort();
    prop_assert_eq!(applied_dirs, head_dirs, "directories {:?}", doc);
    // An unchanged file is not in the document; a changed one appears once.
    let mut named: Vec<&str> = doc.files.iter().map(|f| f.path.as_ref()).collect();
    named.dedup();
    prop_assert_eq!(named.len(), doc.files.len());
  }
}

/// A base with `/f` holding 100 bytes that no later write repeats.
fn base_with_f() -> (Store, Volume, slates_vfs::ids::SnapshotId) {
  let mut store = store();
  let mut vol = volume_with(
    &mut store,
    Quota::Bounded { limit: 1 << 30 },
    NameEquivalence::Exact,
  );
  let root = vol.root();
  let f = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  let bytes: Vec<u8> = (0..100u8).map(|i| i.wrapping_mul(7)).collect();
  vol.write(&mut store, f, 0, &bytes).unwrap();
  let base = vol.snapshot(&mut store).unwrap();
  (store, vol, base)
}

/// T-1.19: a whole-file rewrite by truncate-and-write and by write-and-rename compose to one
/// delete of the base length plus one insert, with identical documents.
#[test]
fn a_whole_file_rewrite_by_either_route_is_one_hunk_and_the_same_document() {
  let fresh: Vec<u8> = (0..42u8).map(|i| 200u8.wrapping_sub(i)).collect();

  let (mut store, mut vol, base) = base_with_f();
  let f = vol.resolve(&store, "/f").unwrap().inode;
  vol.truncate(&mut store, f, 0).unwrap();
  vol.write(&mut store, f, 0, &fresh).unwrap();
  let by_truncate = vol.derive(&store, base).unwrap();

  let (mut store, mut vol, base) = base_with_f();
  let root = vol.root();
  let tmp = vol.create_file(&mut store, root, "f.tmp", 0o644).unwrap();
  vol.write(&mut store, tmp, 0, &fresh).unwrap();
  vol.rename(&mut store, root, "f.tmp", root, "f").unwrap();
  let by_rename = vol.derive(&store, base).unwrap();

  assert_eq!(by_truncate.files.len(), 1);
  assert_eq!(
    by_truncate.files[0].hunks,
    vec![Hunk {
      base_at: 0,
      base_len: 100,
      post_at: 0,
      new_len: 42
    }]
  );
  assert_eq!(
    by_truncate.encode(),
    by_rename.encode(),
    "the two routes: {by_truncate:?} vs {by_rename:?}"
  );
  assert_eq!(by_truncate.identity(), by_rename.identity());
  assert!(
    by_rename.removed.is_empty(),
    "the temporary name vanished and is not an event"
  );
}

/// AC-1.15: the same journal yields a byte-identical document on every platform; this history
/// pins the identity so the Linux and macOS lanes must both reproduce it.
#[test]
fn a_fixed_history_has_a_golden_identity() {
  let (mut store, mut vol, base) = base_with_f();
  let root = vol.root();
  let f = vol.resolve(&store, "/f").unwrap().inode;
  vol.write(&mut store, f, 10, b"hello").unwrap();
  vol.edit(&mut store, f, 50, 5, b"inserted").unwrap();
  let d = vol.mkdir(&mut store, root, "d", 0o755).unwrap();
  let g = vol.create_file(&mut store, d, "g", 0o644).unwrap();
  vol.write(&mut store, g, 0, b"new file").unwrap();
  vol.symlink(&mut store, root, "l", "f").unwrap();
  vol.rename(&mut store, root, "f", d, "moved").unwrap();
  let doc = vol.derive(&store, base).unwrap();
  let identity: String = doc.identity().iter().map(|b| format!("{b:02x}")).collect();
  println!("golden identity: {identity}");
  println!("{doc:?}");
  assert_eq!(doc.files.len(), 2, "{doc:?}");
  assert_eq!(doc.dirs_created, vec![Box::from("/d")]);
  assert_eq!(doc.removed, vec![Box::from("/f")]);
  assert_eq!(doc.symlinks.len(), 1);
  let moved = doc
    .files
    .iter()
    .find(|f| f.path.as_ref() == "/d/moved")
    .unwrap();
  assert_eq!(moved.base.as_ref().map(|b| b.path.as_ref()), Some("/f"));
  assert_eq!(moved.post_len, 103);
  assert_eq!(
    moved.hunks,
    vec![
      Hunk {
        base_at: 10,
        base_len: 5,
        post_at: 10,
        new_len: 5
      },
      Hunk {
        base_at: 50,
        base_len: 5,
        post_at: 50,
        new_len: 8
      },
    ]
  );
  assert_eq!(identity, GOLDEN_IDENTITY);
}

/// Format: the identity of the fixed history above, recorded on 2026-09-05 (macOS, aarch64).
const GOLDEN_IDENTITY: &str = "d69175fd995075ff3adc8df2abe0821f4aa9985b6456cb4f24f31b04650b7306";

/// Runs `prefix` to a base snapshot and `suffix` to a head, derives, and applies the document with
/// the reference applier; the applied files, symlinks and directory paths must equal the head's.
fn derive_and_apply_equals_head(prefix: &[Step], suffix: &[Step]) {
  let mut store = store();
  let mut vol = volume_with(
    &mut store,
    Quota::Bounded { limit: 1 << 30 },
    NameEquivalence::Exact,
  );
  drive(&mut vol, &mut store, prefix);
  let base = vol.snapshot(&mut store).unwrap();
  let base_state = snapshot_state(&vol, &store, base);
  drive(&mut vol, &mut store, suffix);
  let head = head_state(&vol, &store);
  let doc = vol.derive(&store, base).unwrap();
  let applied = apply_document(&doc, &base_state, &head).expect("every hunk inside its sources");
  assert!(
    same_files(&applied, &head),
    "files: applied {:?} head {:?} doc {doc:?}",
    applied.files.keys().collect::<Vec<_>>(),
    head.files.keys().collect::<Vec<_>>()
  );
  assert_eq!(applied.symlinks, head.symlinks, "symlinks {doc:?}");
  let mut applied_dirs = applied.dir_paths();
  applied_dirs.sort();
  let mut head_dirs = head.dir_paths();
  head_dirs.sort();
  assert_eq!(applied_dirs, head_dirs, "directories {doc:?}");
}

fn at(components: &[&str]) -> Vec<String> {
  components.iter().map(|c| (*c).to_owned()).collect()
}

/// T-1.18 (deriver; the applier's one order — renamed directories detach, then removals apply, then
/// the subtrees attach): a removal beneath a directory renamed in the same increment is named by
/// its **post-rename** path, because at the removal step the subtree is detached and the base path
/// no longer exists. Found by the generative oracle after 120,300 cases (2026-09-14): base `/A/e`;
/// unlink `/A/e`, rename `/A` → `/a`; the document said `removed: ["/A/e"]`, the applier removed
/// nothing and attached `/a/e`, and the head had no files
/// (`docs/bugs/2026-09-14-deriver-names-a-removal-under-a-rename-by-its-base-path.md`).
#[test]
fn a_removal_beneath_a_renamed_directory_is_named_by_its_post_rename_path() {
  use Step::*;
  derive_and_apply_equals_head(
    &[Mkdir(at(&[]), "A".into()), Create(at(&["A"]), "e".into())],
    &[
      Unlink(at(&["A"]), "e".into()),
      Rename(at(&[]), "A".into(), at(&[]), "a".into()),
    ],
  );
}

/// The siblings of the case above: a subdirectory removed beneath a renamed parent, a symlink removed
/// beneath one, and a removal beneath a directory renamed twice deep (the deepest rename maps it).
#[test]
fn removals_beneath_renamed_directories_follow_every_rename_shape() {
  use Step::*;
  derive_and_apply_equals_head(
    &[
      Mkdir(at(&[]), "A".into()),
      Mkdir(at(&["A"]), "b".into()),
      Symlink(at(&["A"]), "d".into()),
    ],
    &[
      Rmdir(at(&["A"]), "b".into()),
      Unlink(at(&["A"]), "d".into()),
      Rename(at(&[]), "A".into(), at(&[]), "a".into()),
    ],
  );
  derive_and_apply_equals_head(
    &[
      Mkdir(at(&[]), "A".into()),
      Mkdir(at(&["A"]), "b".into()),
      Create(at(&["A", "b"]), "e".into()),
    ],
    &[
      Unlink(at(&["A", "b"]), "e".into()),
      Rename(at(&["A"]), "b".into(), at(&["A"]), "C".into()),
      Rename(at(&[]), "A".into(), at(&[]), "a".into()),
    ],
  );
}

/// A directory renamed away and back within one increment (`/e` → `/a`, then `/a` → `/e`), with an
/// entry created beneath it while it was away: the document must carry the created entry at its
/// head path with no net rename. Found by the generative oracle after 49,500 cases on the first
/// fix of the post-rename removal (2026-09-14).
#[test]
fn a_directory_renamed_away_and_back_with_a_child_created_meanwhile_reads_right() {
  use Step::*;
  derive_and_apply_equals_head(
    &[Mkdir(at(&[]), "e".into())],
    &[
      Rename(at(&[]), "e".into(), at(&[]), "a".into()),
      Mkdir(at(&["a"]), "a".into()),
      Rename(at(&[]), "a".into(), at(&[]), "e".into()),
    ],
  );
}

/// A directory renamed away and its old name re-created as a fresh directory holding a new entry
/// at a path the base held a subdirectory at (`/d/C` a directory in the base; `/d` → `/a`, mkdir
/// `/d`, symlink `/d/C`): the document must remove nothing under the renamed source, create the
/// new `/d`, and place the symlink. Found by the generative oracle after 202,200 cases on the
/// second fix (2026-09-14).
#[test]
fn a_renamed_directorys_old_name_recreated_with_a_new_entry_reads_right() {
  use Step::*;
  derive_and_apply_equals_head(
    &[Mkdir(at(&[]), "d".into()), Mkdir(at(&["d"]), "C".into())],
    &[
      Rename(at(&[]), "d".into(), at(&[]), "a".into()),
      Mkdir(at(&[]), "d".into()),
      Symlink(at(&["d"]), "C".into()),
    ],
  );
}

/// Format: the records the short journal of
/// `a_derivation_over_a_journal_that_dropped_part_of_the_work_is_refused` keeps, fewer than the files its work creates.
const SHORT_JOURNAL_RECORDS: usize = 4;
/// Format: the files that test's work creates, four times what its short journal keeps.
const WORK_FILES: usize = 4 * SHORT_JOURNAL_RECORDS;

/// §4.16 (the ops document is derived from the journal since the base) and GAPS "the deriver reads the journal with no
/// completeness check" (2026-10-23): a derivation over a journal whose retention dropped records after the base snapshot
/// describes part of the work as all of it. Do: snapshot a volume whose journal keeps `SHORT_JOURNAL_RECORDS` records,
/// then create `WORK_FILES` files, so the oldest of the work's records leave. Expect: the derivation refused
/// [`VfsError::JournalIncomplete`], never a document naming only the files whose records survived; and the same work
/// under a journal that keeps it all derives a document naming every file (the control).
#[test]
fn a_derivation_over_a_journal_that_dropped_part_of_the_work_is_refused() {
  use slates_vfs::VfsError;
  use slates_vfs::clock::StepClock;
  use slates_vfs::volume::VolumeConfig;
  let derive_after_work = |journal_bytes: usize| {
    let mut store = store();
    let mut vol = Volume::create(
      &mut store,
      VolumeConfig {
        prefix: 7,
        names: NameEquivalence::Exact,
        quota: Quota::Bounded { limit: 1 << 30 },
        journal_bytes,
        clock: Box::new(StepClock::new(0, 1_000)),
      },
    )
    .unwrap();
    let base = vol.snapshot(&mut store).unwrap();
    let root = vol.root();
    for file in 0..WORK_FILES {
      vol
        .create_file(&mut store, root, &format!("f{file}"), 0o644)
        .unwrap();
    }
    vol.derive(&store, base)
  };
  let record = std::mem::size_of::<slates_vfs::journal::OpRecord>();
  let whole = derive_after_work(1 << 20).unwrap();
  assert_eq!(
    whole.files.len(),
    WORK_FILES,
    "the control names every file"
  );
  let short = derive_after_work(SHORT_JOURNAL_RECORDS * record);
  assert!(
    matches!(short, Err(VfsError::JournalIncomplete { .. })),
    "a journal that dropped part of the work derived {:?}",
    short.map(|doc| doc.files.len())
  );
}

/// A-262, stage 1 (the merge does not carry the bits yet). Do: from one base, derive four works: one that sets only a
/// file's Windows attribute bits, one that changes nothing, one that rewrites the file's first bytes, and one that
/// rewrites them and sets the bits too. Expect: the bits-only work's ops document is the unchanged work's, byte for
/// byte, and the rewrite's is the same with the bits or without. The merge reads only this document, so two increments
/// that differ only in the bits get the same verdict and never a conflict, until the merge's own dimension carries
/// them (A-262 stage 2).
#[test]
fn windows_attribute_bits_are_invisible_to_the_ops_document_until_the_merge_carries_them() {
  use slates_vfs::inode::{Kind, WindowsAttributes};
  let hidden = WindowsAttributes::kept(0x2 | 0x4, Kind::File);
  let derive_after = |set_bits: bool, rewrite: bool| {
    let (mut store, mut vol, base) = base_with_f();
    let f = vol.resolve(&store, "/f").unwrap().inode;
    if rewrite {
      vol.write(&mut store, f, 0, b"rewritten").unwrap();
    }
    if set_bits {
      vol.set_windows_attributes(&mut store, f, hidden).unwrap();
      assert_eq!(
        vol.windows_attributes(&store, f).unwrap(),
        hidden,
        "the bits were set"
      );
    }
    vol.derive(&store, base).unwrap()
  };
  let unchanged = derive_after(false, false);
  let bits_only = derive_after(true, false);
  assert_eq!(
    bits_only, unchanged,
    "a bits-only work derives the unchanged document"
  );
  assert_eq!(bits_only.encode(), unchanged.encode(), "byte for byte");
  let rewrite = derive_after(false, true);
  let rewrite_and_bits = derive_after(true, true);
  assert_ne!(
    rewrite, unchanged,
    "non-vacuous: a rewrite is in the document"
  );
  assert_eq!(
    rewrite_and_bits.encode(),
    rewrite.encode(),
    "the bits beside a rewrite change nothing in its document"
  );
}

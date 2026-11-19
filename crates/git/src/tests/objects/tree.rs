//! Trees by use: entries, modes and their text, git's order, the special blobs a check hands on, the admission, and
//! git's duplicate detection over sorted trees.

use core::cmp::Ordering;

use proptest::prelude::*;

use super::super::tree::{self, Entry, Mode, Tree};
use super::super::{FsckId, ObjectRefused, Refusal, Strictness};
use super::{FORMATS, ample, golden};
use crate::object_name::{Kind, ObjectFormat};
use crate::{Admission, PastAdmission};

/// A-590 (`decode_tree_entry`, object.h's `object_type`; `fixtures/fsck` `ok-tree`, written by `git mktree`). Do: read
/// the tree stock git wrote with every mode it writes, in both formats, and write each entry again from its mode, name
/// and id alone. Expect: its six entries in git's order, each with its mode, the text git writes for it (`40000` for
/// the subtree), the type of object it names, and an id of the format's length; the rewritten tree the same bytes.
#[test]
fn a_tree_is_read_in_every_mode_git_writes_and_written_again_from_its_parts() {
  for format in FORMATS {
    let golden = golden(format, "ok-tree");
    let tree = Tree::parse(golden.bytes, format, &mut ample()).unwrap();
    let read: Vec<(&[u8], Option<Mode>, Kind)> = tree
      .entries()
      .iter()
      .map(|entry| (entry.name, entry.standard_mode(), entry.kind()))
      .collect();
    assert_eq!(
      read,
      [
        (&b"lib-x"[..], Some(Mode::File), Kind::Blob),
        (&b"lib.rs"[..], Some(Mode::File), Kind::Blob),
        (&b"lib"[..], Some(Mode::Directory), Kind::Tree),
        (&b"link"[..], Some(Mode::Symlink), Kind::Blob),
        (&b"run"[..], Some(Mode::Executable), Kind::Blob),
        (&b"sub"[..], Some(Mode::Gitlink), Kind::Commit),
      ]
    );
    let mut written = Vec::new();
    for entry in tree.entries() {
      assert_eq!(entry.mode_text, entry.standard_mode().unwrap().text());
      assert_eq!(entry.is_dir(), entry.name == b"lib");
      assert_eq!(entry.is_symlink(), entry.name == b"link");
      let id = entry.object_id(format).unwrap();
      assert_eq!(id.as_bytes(), entry.id);
      Entry::new(entry.standard_mode().unwrap(), entry.name, id.as_bytes()).write(&mut written);
    }
    assert_eq!(written, golden.bytes);
    let root = golden(format, "ok-commit-root");
    let root = super::super::commit::Commit::parse(root.bytes, &mut ample()).unwrap();
    assert_eq!(tree.entries()[2].object_id(format), root.tree(format), "the subtree is the root commit's tree");
  }
}

/// A-590 (tree.c's `base_name_compare`). Do: compare the entries of the tree stock git wrote, pair by pair, and sort
/// them back from reversed. Expect: each before the next; the sort gives git's order back, a subtree after the names
/// it begins (`lib-x`, `lib.rs`, then `lib`); and a file named `lib` before both, where git puts it.
#[test]
fn git_orders_a_subtree_as_if_its_name_ended_with_a_slash() {
  let golden = golden(ObjectFormat::Sha1, "ok-tree");
  let tree = Tree::parse(golden.bytes, ObjectFormat::Sha1, &mut ample()).unwrap();
  let entries = tree.entries();
  for pair in entries.windows(2) {
    assert_eq!(tree::compare(&pair[0], &pair[1]), Ordering::Less);
  }
  let mut sorted: Vec<Entry<'_>> = entries.iter().rev().copied().collect();
  sorted.sort_by(tree::compare);
  assert_eq!(sorted, entries);
  let id = [0x11; 20];
  let file = Entry::new(Mode::File, b"lib", &id);
  let dir = Entry::new(Mode::Directory, b"lib", &id);
  let dashed = Entry::new(Mode::File, b"lib-x", &id);
  assert_eq!(tree::compare(&file, &dashed), Ordering::Less);
  assert_eq!(tree::compare(&dashed, &dir), Ordering::Less);
  assert_eq!(tree::compare(&file, &dir), Ordering::Less);
  assert_eq!(tree::compare(&dir, &dir), Ordering::Equal);
}

/// A-590 (object.h's `parse_mode`; fsck.c's standard modes). Do: read a mode of seven digits that wraps past 16 bits,
/// a zero-padded subtree, the old group-writable file and a bogus mode, each as stock git wrote it. Expect: `1100644`
/// read as a file (git keeps 16 bits) and written back as written; `040000` a subtree served with its zero; `100664`
/// accepted by `git fsck` and a warning under the strict check; `100000` no mode git writes.
#[test]
fn a_mode_is_read_as_git_reads_it_and_served_as_written() {
  for format in FORMATS {
    let entry_of = |name: &str| {
      let golden = golden(format, name);
      let tree = Tree::parse(golden.bytes, format, &mut ample()).unwrap();
      let entry = tree.entries()[0];
      let mut written = Vec::new();
      tree.write(&mut written);
      assert_eq!(written, golden.bytes);
      (entry.mode_text.to_vec(), entry.mode, entry.standard_mode())
    };
    assert_eq!(entry_of("ok-tree-mode-wraps"), (b"1100644".to_vec(), 0o100644, Some(Mode::File)));
    assert_eq!(entry_of("warn-tree-zero-padded"), (b"040000".to_vec(), 0o40000, Some(Mode::Directory)));
    assert_eq!(entry_of("ok-tree-mode-664"), (b"100664".to_vec(), 0o100664, None));
    assert_eq!(entry_of("info-tree-bogus-mode"), (b"100000".to_vec(), 0o100000, None));
    let group_writable = golden(format, "ok-tree-mode-664");
    let check = |strictness| tree::fsck(group_writable.bytes, format, strictness, &mut ample()).unwrap().report;
    assert_eq!(check(Strictness::Default).reported().count(), 0);
    assert_eq!(check(Strictness::Strict).warnings().collect::<Vec<_>>(), [FsckId::BadFilemode]);
    assert_eq!(check(Strictness::Strict).refusal(), None, "an info stays a warning under the strict check");
  }
  for mode in Mode::ALL {
    assert_eq!(Mode::from_bits(mode.bits()), Some(mode));
    assert!(!mode.text().starts_with(b"0"), "git writes no leading zero");
  }
}

/// A-590 (fsck.c's `gitmodules_found` and `gitattributes_found`). Do: check the tree stock git wrote with a
/// `.gitattributes` and a `.gitmodules` file, a tree with a `.gitmodules` symbolic link, and a tree whose
/// `.gitmodules` file is named after a backslash. Expect: each file's id handed on to be checked as that file, in the
/// order met; the link reported and not handed on.
#[test]
fn the_special_files_a_tree_names_are_handed_on_to_be_checked() {
  for format in FORMATS {
    let golden = golden(format, "ok-tree-special-blobs");
    let tree = Tree::parse(golden.bytes, format, &mut ample()).unwrap();
    let checked = tree::fsck(golden.bytes, format, Strictness::Strict, &mut ample()).unwrap();
    assert_eq!(checked.gitattributes, [tree.entries()[0].id]);
    assert_eq!(checked.gitmodules, [tree.entries()[1].id]);
    assert_eq!(checked.report.reported().count(), 0);
    let link = golden_tree(format, "bad-tree-gitmodules-symlink");
    let checked = tree::fsck(link, format, Strictness::Default, &mut ample()).unwrap();
    assert!(checked.gitmodules.is_empty());
    assert_eq!(checked.report.refusal(), Some(FsckId::GitmodulesSymlink));
  }
  let id = [0x22; 20];
  let mut bytes = Vec::new();
  Entry::new(Mode::File, b"x\\GITMOD~1", &id).write(&mut bytes);
  let checked = tree::fsck(&bytes, ObjectFormat::Sha1, Strictness::Strict, &mut ample()).unwrap();
  assert_eq!(checked.gitmodules, [&id[..]]);
}

/// The bytes of tree fixture `name`.
fn golden_tree(format: ObjectFormat, name: &str) -> &'static [u8] {
  golden(format, name).bytes
}

/// A-590 (§4.9: what a decode keeps is charged first). Do: parse a tree of six entries under an admission one byte
/// short of six entries' room, then under exactly that room; check a tree with two special files under an admission
/// with room for one id. Expect: the first parse refused past its admission with nothing charged, the second whole
/// with nothing left; the check refused past its admission.
#[test]
fn a_tree_past_its_admission_is_refused_before_anything_is_kept() {
  let golden = golden(ObjectFormat::Sha1, "ok-tree");
  let room = u64::try_from(core::mem::size_of::<Entry<'_>>() * 6).unwrap();
  let mut short = Admission::new(room - 1);
  assert_eq!(
    Tree::parse(golden.bytes, ObjectFormat::Sha1, &mut short),
    Err(ObjectRefused {
      at: 0,
      refusal: Refusal::PastAdmission(PastAdmission {
        asked: room,
        left: room - 1
      })
    })
  );
  assert_eq!(short.left(), room - 1);
  let mut exact = Admission::new(room);
  assert_eq!(Tree::parse(golden.bytes, ObjectFormat::Sha1, &mut exact).unwrap().entries().len(), 6);
  assert_eq!(exact.left(), 0);
  let special = golden_tree(ObjectFormat::Sha1, "ok-tree-special-blobs");
  let one_id = u64::try_from(core::mem::size_of::<&[u8]>()).unwrap();
  let refused = tree::fsck(special, ObjectFormat::Sha1, Strictness::Strict, &mut Admission::new(one_id));
  assert!(matches!(refused, Err(PastAdmission { .. })));
}

/// Format: names that sort around each other in git's order, bytes below `/` (`!`, `-`, `.`) and above it after a
/// shared start, so generated trees meet git's non-neighbouring duplicates (`a`, `a-b`, `a.b`, `a/`).
const NAMES: [&[u8]; 7] = [b"a", b"a!", b"a-b", b"a.b", b"a.b.c", b"ab", b"b"];

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-590 (fsck.c's `verify_ordered`, its stack of names a later subtree could repeat). Do: build trees of up to
  /// eight entries from names that sort around each other, each a file or a subtree, sorted in git's order, and check
  /// them. Expect: `duplicateEntries` exactly when two entries share a name, wherever they sit, and never
  /// `treeNotSorted`.
  #[test]
  fn a_sorted_trees_duplicates_are_found_wherever_they_sit(
    picks in proptest::collection::vec((0..NAMES.len(), any::<bool>()), 0..8)
  ) {
    let id = [0x33; 20];
    let mut entries: Vec<Entry<'_>> = picks
      .iter()
      .map(|(name, dir)| Entry::new(if *dir { Mode::Directory } else { Mode::File }, NAMES[*name], &id))
      .collect();
    entries.sort_by(tree::compare);
    let mut bytes = Vec::new();
    tree::write_entries(&entries, &mut bytes);
    let report = tree::fsck(&bytes, ObjectFormat::Sha1, Strictness::Default, &mut ample()).unwrap().report;
    let mut names: Vec<&[u8]> = entries.iter().map(|entry| entry.name).collect();
    names.sort_unstable();
    names.dedup();
    let repeats = names.len() < entries.len();
    prop_assert_eq!(report.findings().contains(&FsckId::DuplicateEntries), repeats, "{:?}", picks);
    prop_assert!(!report.findings().contains(&FsckId::TreeNotSorted), "{:?}", picks);
  }
}

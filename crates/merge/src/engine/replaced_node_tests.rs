//! A work never changes a node replaced after its base (A-631; A-612's siblings; §4.16 "The
//! verdict": "delete against modify in either direction", "rename against rename"). A file, a
//! symbolic link, a hard link's name or a directory removed after a work's base is gone for that
//! work, whatever stands at its path now: a mode change or an attribute set or removed there is
//! delete against modify, and a rename of it rename against rename, never a change or a move of
//! the node that took its place.
//!
//! A-643, A-631's siblings: a work never removes a node changed after its base in any dimension
//! (made again, its mode or an attribute changed), never makes a hard link to a file removed after
//! its base, and a link renamed over a file replaces the file, so the green's state stays whole:
//! one node at a path at most.

use std::cell::RefCell;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::{Green, Increment, Outcome};
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::origin::Origin;
use crate::special::{SpecialKind, SpecialNode};
use crate::verdict::MergeConflictClass;

/// An increment under construction, laid out as the deriver lays one out.
#[derive(Default)]
struct Work {
  doc: OpsDoc,
  post: Vec<u8>,
}

impl Work {
  /// Adds `kind` on `path` with `at`, `len` and the source `src` as given.
  fn op(mut self, kind: OpKind, path: &str, (at, len, src): (u64, u64, u64)) -> Work {
    let path = self.doc.paths.intern(path);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path,
      at,
      len,
      src,
    });
    self
  }

  /// Adds a namespace op on `path` naming `other` (a rename's source, a link's target).
  fn naming(mut self, kind: OpKind, path: &str, other: &str) -> Work {
    let other = self.doc.paths.intern(other);
    self.op(kind, path, (0, 0, other))
  }

  /// Adds a mode change of `path`.
  fn mode(self, path: &str, mode: u32) -> Work {
    self.op(OpKind::SetMode, path, (0, u64::from(mode), u64::MAX))
  }

  /// Adds `name` set to `value` on `path`.
  fn set_xattr(mut self, path: &str, name: &str, value: &[u8]) -> Work {
    let name = self.doc.paths.intern(name);
    let src = self.post.len() as u64;
    self.post.extend_from_slice(value);
    self.op(OpKind::SetXattr, path, (name, value.len() as u64, src))
  }

  /// Adds `name` removed from `path`.
  fn remove_xattr(mut self, path: &str, name: &str) -> Work {
    let name = self.doc.paths.intern(name);
    self.op(OpKind::RemoveXattr, path, (name, 0, u64::MAX))
  }

  /// Adds `path` made a file holding `bytes`.
  fn create(mut self, path: &str, bytes: &[u8]) -> Work {
    let src = self.post.len() as u64;
    self.post.extend_from_slice(bytes);
    self.op(OpKind::Create, path, (0, 0, u64::MAX)).op(
      OpKind::Insert,
      path,
      (0, bytes.len() as u64, src),
    )
  }

  /// The increment `id` of this work based at `base`.
  fn at(self, base: u64, id: u64) -> Increment {
    let mut identity = [0u8; 32];
    identity[..8].copy_from_slice(&id.to_le_bytes());
    Increment {
      id: identity,
      base,
      doc: self.doc,
      post_state: self.post,
      evidence: Vec::new(),
    }
  }
}

/// Shape: the file the tests change, the file the links name, the symlink, the hard link and
/// the directory.
const FILE: &str = "f";
/// Shape: see `FILE`.
const TARGET: &str = "g";
/// Shape: see `FILE`.
const SYMLINK: &str = "l";
/// Shape: see `FILE`.
const HARDLINK: &str = "h";
/// Shape: see `FILE`.
const DIR: &str = "d";
/// Shape: the attribute the tests set and remove.
const NAME: &str = "user.k";
/// Shape: the IPC node (a FIFO) the green holds.
const SPECIAL: &str = "p";

/// A green of `FILE` (mode 0o644, with `NAME`), `TARGET`, `SYMLINK` and `HARDLINK` to `TARGET`,
/// the empty `DIR` and the FIFO `SPECIAL`.
fn green() -> Green {
  let mut origin = Origin::default();
  origin.files.push((FILE.to_owned(), b"<f>\n".to_vec()));
  origin.files.push((TARGET.to_owned(), b"<g>\n".to_vec()));
  origin.modes.push((FILE.to_owned(), 0o644));
  origin
    .xattrs
    .push((FILE.to_owned(), NAME.to_owned(), b"v".to_vec()));
  origin
    .symlinks
    .push((SYMLINK.to_owned(), TARGET.to_owned()));
  origin
    .hardlinks
    .push((HARDLINK.to_owned(), TARGET.to_owned()));
  origin.dirs.push(DIR.to_owned());
  origin.specials.push((
    SPECIAL.to_owned(),
    SpecialNode {
      kind: SpecialKind::Fifo,
      uid: 1,
      gid: 1,
      atime: 1,
      mtime: 1,
      ctime: 1,
      btime: 1,
    },
  ));
  Green::with_origin(origin)
}

/// Submits `work` at the head of `green` as increment `id`, expecting it accepted.
fn accept(green: &mut Green, work: Work, id: u64) -> Result<(), String> {
  let head = green.head();
  match green.submit(&work.at(head, id)) {
    Outcome::Accepted { version } if version == head + 1 => Ok(()),
    other => Err(format!("a setup work was answered {other:?}")),
  }
}

/// The node at `path` removed and made again, as two works at the head: a file made with other
/// bytes, a link made to `TARGET` again, a directory made again.
fn remake(green: &mut Green, path: &str, id: u64) -> Result<(), String> {
  let removal = match path {
    DIR => OpKind::Rmdir,
    _ => OpKind::Unlink,
  };
  accept(
    green,
    Work::default().op(removal, path, (0, 0, u64::MAX)),
    id,
  )?;
  let made = match path {
    SYMLINK => Work::default().naming(OpKind::Symlink, path, TARGET),
    HARDLINK => Work::default().naming(OpKind::Link, path, TARGET),
    DIR => Work::default().op(OpKind::Mkdir, path, (0, 0, u64::MAX)),
    _ => Work::default().create(path, b"<made again>\n"),
  };
  accept(green, made, id + 1)
}

/// The classes `outcome`'s windows name on `path`, or `None` for an accept.
fn classes_on(outcome: &Outcome, path: &str) -> Option<Vec<MergeConflictClass>> {
  match outcome {
    Outcome::Conflict { windows } => Some(
      windows
        .iter()
        .filter(|window| window.path == path)
        .map(|window| window.class)
        .collect(),
    ),
    _ => None,
  }
}

/// What a green answers of `FILE`'s metadata: its mode and `NAME`'s value.
fn metadata(green: &Green) -> (Option<u32>, Option<Vec<u8>>) {
  (
    green.mode(FILE),
    green.xattr(FILE, NAME).map(<[u8]>::to_vec),
  )
}

/// The three metadata changes of `FILE` a work can make.
fn metadata_changes() -> [(&'static str, Work); 3] {
  [
    ("a mode change", Work::default().mode(FILE, 0o600)),
    (
      "an attribute set",
      Work::default().set_xattr(FILE, NAME, b"w"),
    ),
    (
      "an attribute removed",
      Work::default().remove_xattr(FILE, NAME),
    ),
  ]
}

/// Submits `change`, based at 0, to a fresh green whose `FILE` was made again after version 0, and
/// returns what it answered and whether `FILE`'s metadata stayed as the replacement left it.
fn change_after_remake(change: Work) -> Result<(Outcome, bool), String> {
  let mut green = green();
  remake(&mut green, FILE, 1)?;
  let (head, before) = (green.head(), metadata(&green));
  let answer = green.submit(&change.at(0, 9));
  let kept = green.head() == head && metadata(&green) == before;
  Ok((answer, kept))
}

/// A-631 (A-612's first sibling). Do: make the file again after version 0, then submit a work
/// based at 0 that changes the file's mode, sets its attribute or removes it, each to a fresh
/// green. Expect `DeleteModify` each time, the head unmoved and the made-again file's mode and
/// attributes as they were; and the same mode change based at the head accepted.
#[test]
fn a_metadata_change_based_before_its_file_was_replaced_conflicts() -> Result<(), String> {
  for (what, change) in metadata_changes() {
    let (answer, kept) = change_after_remake(change)?;
    if classes_on(&answer, FILE) != Some(vec![MergeConflictClass::DeleteModify]) || !kept {
      return Err(format!(
        "{what}: {answer:?}, the file's metadata kept: {kept}"
      ));
    }
  }
  let mut green = green();
  remake(&mut green, FILE, 1)?;
  accept(&mut green, Work::default().mode(FILE, 0o600), 9)
}

/// A-631 (A-612's second sibling). Do: remove the symlink, and on another green the hard link,
/// after version 0 and make each again (to the same target), then submit a work based at 0 that
/// renames it to a free path. Expect `RenameRename` each time, the link made again where it
/// stands and the free path still free: the rename never moves a link the work did not see.
#[test]
fn a_rename_based_before_its_link_was_replaced_conflicts() -> Result<(), String> {
  for link in [SYMLINK, HARDLINK] {
    let mut green = green();
    remake(&mut green, link, 1)?;
    let head = green.head();
    let answer = green.submit(
      &Work::default()
        .naming(OpKind::Rename, "moved", link)
        .at(0, 9),
    );
    let stands = green.symlink(link).is_some() || green.hardlink(link).is_some();
    let moved = green.symlink("moved").is_some() || green.hardlink("moved").is_some();
    let refused = classes_on(&answer, "moved") == Some(vec![MergeConflictClass::RenameRename]);
    if !refused || green.head() != head || !stands || moved {
      return Err(format!(
        "{link}: {answer:?}, stands: {stands}, moved: {moved}"
      ));
    }
  }
  Ok(())
}

/// How a generated work changes a node's path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Touch {
  Mode,
  SetXattr,
  RemoveXattr,
  Rename,
}

/// One generated step: the node at `path` removed and made again, or a work based `back`
/// versions behind the head touching `path`.
#[derive(Clone, Copy, Debug)]
enum Step {
  Remake(&'static str),
  Touch(&'static str, Touch, u8),
}

/// Shape: the paths the property changes, every kind a path can name.
const NODES: [&str; 4] = [FILE, SYMLINK, HARDLINK, DIR];
/// Shape: the touches, in the order the generator's selector names them.
const TOUCHES: [Touch; 4] = [
  Touch::Mode,
  Touch::SetXattr,
  Touch::RemoveXattr,
  Touch::Rename,
];

fn step() -> impl Strategy<Value = Step> {
  (0u8..10, 0..NODES.len(), 0..TOUCHES.len(), 0u8..6).prop_map(|(selector, node, touch, back)| {
    match selector {
      0..3 => Step::Remake(NODES[node]),
      _ => Step::Touch(NODES[node], TOUCHES[touch], back),
    }
  })
}

/// How often each answer was met over every case: the non-vacuity census.
#[derive(Debug, Default)]
struct Census {
  /// A metadata change of a node removed after its base, refused.
  metadata: u64,
  /// A rename of a link removed after its base, refused.
  link_rename: u64,
  /// A rename of the file removed after its base, refused.
  file_rename: u64,
  /// A touch of a node not removed after its base, accepted.
  accepted: u64,
}

/// A history under test: the green, which nodes stood at each version, and the last version each
/// path's node was removed at.
struct History {
  green: Green,
  standing: Vec<[bool; 4]>,
  removed: [Option<u64>; 4],
  works: u64,
}

impl History {
  fn new() -> History {
    let green = green();
    let standing = vec![History::stands(&green)];
    History {
      green,
      standing,
      removed: [None; 4],
      works: 0,
    }
  }

  /// Which of `NODES` stand in `green` now.
  fn stands(green: &Green) -> [bool; 4] {
    [
      green.content(FILE).is_some(),
      green.symlink(SYMLINK).is_some(),
      green.hardlink(HARDLINK).is_some(),
      green.is_dir(DIR),
    ]
  }

  /// Submits `work` based at `base`; when accepted, records which nodes stand at the new version
  /// and the version each node that stood before and no longer does was removed at.
  fn submit(&mut self, work: Work, base: u64) -> Outcome {
    self.works += 1;
    let answer = self.green.submit(&work.at(base, self.works));
    if let Outcome::Accepted { version } = answer {
      let now = History::stands(&self.green);
      let before = self.standing.last().copied().unwrap_or_default();
      for (index, (was, is)) in before.iter().zip(now).enumerate() {
        if *was && !is {
          self.removed[index] = Some(version);
        }
      }
      self.standing.push(now);
    }
    answer
  }

  fn remake(&mut self, path: &str) {
    let head = self.green.head();
    let removal = if path == DIR {
      OpKind::Rmdir
    } else {
      OpKind::Unlink
    };
    self.submit(Work::default().op(removal, path, (0, 0, u64::MAX)), head);
    let head = self.green.head();
    let made = match path {
      SYMLINK => Work::default().naming(OpKind::Symlink, path, TARGET),
      HARDLINK => Work::default().naming(OpKind::Link, path, TARGET),
      DIR => Work::default().op(OpKind::Mkdir, path, (0, 0, u64::MAX)),
      _ => Work::default().create(path, b"<made again>\n"),
    };
    self.submit(made, head);
  }

  /// A work touching `path` based `back` versions behind the head, where its node stood at that
  /// base; the rule (A-631, A-612) holds its answer: refused by its class when the node was
  /// removed after the base.
  fn touch(
    &mut self,
    path: &str,
    how: Touch,
    back: u8,
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let index = NODES
      .iter()
      .position(|node| *node == path)
      .unwrap_or_default();
    let base = self.green.head().saturating_sub(u64::from(back));
    let stood = usize::try_from(base)
      .ok()
      .and_then(|base| self.standing.get(base))
      .is_some_and(|stood| stood[index]);
    if !stood || (how == Touch::Rename && path == DIR) {
      return Ok(());
    }
    let destination = format!("moved-{}", self.works);
    let work = match how {
      Touch::Mode => Work::default().mode(path, 0o600),
      Touch::SetXattr => Work::default().set_xattr(path, NAME, b"w"),
      Touch::RemoveXattr => Work::default().remove_xattr(path, NAME),
      Touch::Rename => Work::default().naming(OpKind::Rename, &destination, path),
    };
    let ruled = self.removed[index].is_some_and(|version| version > base);
    let answer = self.submit(work, base);
    let (window, class) = match how {
      Touch::Rename => (destination.as_str(), MergeConflictClass::RenameRename),
      _ => (path, MergeConflictClass::DeleteModify),
    };
    if ruled {
      let classes = classes_on(&answer, window).unwrap_or_default();
      prop_assert!(
        classes.contains(&class),
        "{path} {how:?} from {base}: {answer:?}"
      );
      census.count(path, how);
    } else if matches!(answer, Outcome::Accepted { .. }) {
      census.accepted += 1;
    }
    Ok(())
  }
}

impl Census {
  /// Counts a refusal by the rule of `how` on `path`.
  fn count(&mut self, path: &str, how: Touch) {
    match (how, path) {
      (Touch::Rename, FILE) => self.file_rename += 1,
      (Touch::Rename, _) => self.link_rename += 1,
      _ => self.metadata += 1,
    }
  }
}

/// A-631's oracle (§4.16 "The verdict"). Do: run generated histories over a green of a file, a
/// symlink and a hard link to another file, and a directory: each node removed and made again at
/// the head, and works based behind the head changing a node's mode, setting or removing its
/// attribute, or renaming it, where it stood at the work's base. Expect every work that touches
/// a node removed after its base refused, `DeleteModify` for a metadata change and
/// `RenameRename` for a rename, whatever stands at the path now; the census shows each refusal
/// and accepted touches met.
#[test]
fn no_work_changes_a_node_removed_after_its_base() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let result = runner.run(&prop::collection::vec(step(), 1..32), |steps| {
    let mut history = History::new();
    for step in steps {
      match step {
        Step::Remake(path) => history.remake(path),
        Step::Touch(path, how, back) => {
          history.touch(path, how, back, &mut census.borrow_mut())?;
        }
      }
    }
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  let met = [
    census.metadata,
    census.link_rename,
    census.file_rename,
    census.accepted,
  ];
  assert!(
    met.iter().all(|&count| count > 0),
    "an answer never met: {census:?}"
  );
}

/// What stands at a path: a whole state holds one node a path at most (A-643).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Kind {
  #[default]
  Absent,
  File,
  Symlink,
  Hardlink,
  Dir,
  Special,
}

/// Every node standing at `path` in `green`, by kind: one at most in a whole state.
fn kinds_at(green: &Green, path: &str) -> Vec<Kind> {
  [
    (green.content(path).is_some(), Kind::File),
    (green.symlink(path).is_some(), Kind::Symlink),
    (green.hardlink(path).is_some(), Kind::Hardlink),
    (green.is_dir(path), Kind::Dir),
    (green.special(path).is_some(), Kind::Special),
  ]
  .into_iter()
  .filter_map(|(stands, kind)| stands.then_some(kind))
  .collect()
}

/// The kind of the green's link at `link`: `SYMLINK`'s or `HARDLINK`'s.
fn link_kind(link: &str) -> Kind {
  if link == SYMLINK {
    Kind::Symlink
  } else {
    Kind::Hardlink
  }
}

/// The removal of the node of kind `kind` at `path`: a directory's `Rmdir`, any other's `Unlink`.
fn removal_of(path: &str, kind: Kind) -> Work {
  let removal = if kind == Kind::Dir {
    OpKind::Rmdir
  } else {
    OpKind::Unlink
  };
  Work::default().op(removal, path, (0, 0, u64::MAX))
}

/// A-643 (A-631's first sibling; §4.16 "The verdict": a rename over a base file replaces it, as
/// POSIX `rename` does). Do: rename the symlink, and on another green the hard link, over the
/// unchanged file `FILE` by a work based at the head; then submit a work based before that rename
/// that renames `FILE` to a free path. Expect the first accepted and `FILE` holding the link alone,
/// the link's old path empty; the second refused `RenameRename`, the link where it stands: the
/// file it named was removed (A-612). Before, the file stayed beside the link at one path.
#[test]
fn a_link_renamed_over_a_file_replaces_it() -> Result<(), String> {
  for link in [SYMLINK, HARDLINK] {
    let mut green = green();
    accept(
      &mut green,
      Work::default().naming(OpKind::Rename, FILE, link),
      1,
    )?;
    let (at_file, at_link) = (kinds_at(&green, FILE), kinds_at(&green, link));
    let answer = green.submit(
      &Work::default()
        .naming(OpKind::Rename, "moved", FILE)
        .at(0, 2),
    );
    let refused = classes_on(&answer, "moved") == Some(vec![MergeConflictClass::RenameRename]);
    let after = (kinds_at(&green, FILE), kinds_at(&green, "moved"));
    let whole = at_file == vec![link_kind(link)] && at_link.is_empty();
    if !whole || !refused || after != (at_file.clone(), Vec::new()) {
      return Err(format!(
        "{link}: at the file {at_file:?}, at the link {at_link:?}; then {answer:?}, {after:?}"
      ));
    }
  }
  Ok(())
}

/// A-643. Do: in one work based at the head, rename `FILE` away to a free path and the symlink
/// over `FILE`. Expect it accepted: the file's bytes at the free path, the symlink alone at
/// `FILE`, its old path empty; the file is removed once, by its own rename away.
#[test]
fn a_link_renamed_over_a_file_the_same_work_renamed_away_takes_its_place() -> Result<(), String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default()
      .naming(OpKind::Rename, "kept", FILE)
      .naming(OpKind::Rename, FILE, SYMLINK),
    1,
  )?;
  let found = (
    kinds_at(&green, "kept"),
    kinds_at(&green, FILE),
    kinds_at(&green, SYMLINK),
    green.content("kept").map(<[u8]>::to_vec),
  );
  let expected = (
    vec![Kind::File],
    vec![Kind::Symlink],
    Vec::new(),
    Some(b"<f>\n".to_vec()),
  );
  if found != expected {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-643 (POSIX `rename`: when both names are links to one file it does nothing). Do: rename the
/// hard link `HARDLINK` over the file it names, `TARGET`, by a work based at the head. Expect it
/// accepted and nothing changed: `TARGET` still the file with its bytes, `HARDLINK` still the link
/// naming it. Before, the link left its path and `TARGET` held its file and a link to itself.
#[test]
fn a_hard_link_renamed_over_the_file_it_names_changes_nothing() -> Result<(), String> {
  let mut green = green();
  accept(
    &mut green,
    Work::default().naming(OpKind::Rename, TARGET, HARDLINK),
    1,
  )?;
  let found = (
    kinds_at(&green, TARGET),
    green.content(TARGET).map(<[u8]>::to_vec),
    kinds_at(&green, HARDLINK),
    green.hardlink(HARDLINK),
  );
  let expected = (
    vec![Kind::File],
    Some(b"<g>\n".to_vec()),
    vec![Kind::Hardlink],
    Some(TARGET),
  );
  if found != expected {
    return Err(format!("{found:?}"));
  }
  Ok(())
}

/// A-643 (A-631's second sibling; §4.16 "The verdict": delete against modify). Do: remove the
/// directory `DIR` and make it again after version 0, then submit a work based at 0 that removes
/// it. Expect `DeleteModify`, the directory made again still standing; and the same removal
/// based at the head accepted.
#[test]
fn a_directory_made_again_after_a_works_base_is_not_removed() -> Result<(), String> {
  let mut green = green();
  remake(&mut green, DIR, 1)?;
  let answer = green.submit(&removal_of(DIR, Kind::Dir).at(0, 9));
  let refused = classes_on(&answer, DIR) == Some(vec![MergeConflictClass::DeleteModify]);
  if !refused || !green.is_dir(DIR) {
    return Err(format!(
      "{answer:?}, the directory stands: {}",
      green.is_dir(DIR)
    ));
  }
  accept(&mut green, removal_of(DIR, Kind::Dir), 10)
}

/// What the green answers of the node at `path`: whether a file or directory stands there, its
/// mode and `NAME`'s value.
fn node_state(green: &Green, path: &str) -> (bool, Option<u32>, Option<Vec<u8>>) {
  (
    green.content(path).is_some() || green.is_dir(path),
    green.mode(path),
    green.xattr(path, NAME).map(<[u8]>::to_vec),
  )
}

/// A-643 (A-631's third sibling; §4.16 "The verdict": "delete against modify in either
/// direction"). Do: change the mode of `FILE`, and on other greens set its attribute, change the
/// mode of `DIR` and set its attribute, each after version 0; then submit a work based at 0 that
/// removes the node. Expect `DeleteModify` each time, the node standing with the change kept;
/// and the same removal based at the head accepted.
#[test]
fn a_removal_based_before_a_metadata_change_conflicts() -> Result<(), String> {
  for (path, kind) in [(FILE, Kind::File), (DIR, Kind::Dir)] {
    let changes = [
      Work::default().mode(path, 0o700),
      Work::default().set_xattr(path, NAME, b"w"),
    ];
    for change in changes {
      let mut green = green();
      accept(&mut green, change, 1)?;
      let before = node_state(&green, path);
      let answer = green.submit(&removal_of(path, kind).at(0, 9));
      let refused = classes_on(&answer, path) == Some(vec![MergeConflictClass::DeleteModify]);
      let after = node_state(&green, path);
      if !refused || after != before {
        return Err(format!("{path}: {answer:?}, {before:?} became {after:?}"));
      }
      accept(&mut green, removal_of(path, kind), 10)?;
    }
  }
  Ok(())
}

/// Submits a new hard link `linked` to `TARGET`, based at 0, to a green where `TARGET` was removed
/// after 0 and, when `made_again`, made again; returns what it answered and whether `linked` was
/// made.
fn link_after_removal(made_again: bool) -> Result<(Outcome, bool), String> {
  let mut green = green();
  accept(&mut green, removal_of(TARGET, Kind::File), 1)?;
  if made_again {
    accept(
      &mut green,
      Work::default().create(TARGET, b"<made again>\n"),
      2,
    )?;
  }
  let answer = green.submit(
    &Work::default()
      .naming(OpKind::Link, "linked", TARGET)
      .at(0, 9),
  );
  Ok((answer, green.hardlink("linked").is_some()))
}

/// A-643 (A-631's fourth sibling; §4.16 "The verdict": delete against modify). Do: remove the file
/// `TARGET` after version 0, on one green making it again; then submit a work based at 0 that makes
/// a new hard link to it. Expect `DeleteModify` on the link's path each time and no link made: the
/// file it named is gone, and the link never names the file made in its place. A work based at 0
/// that makes `TARGET` again itself and links to that is accepted, the link naming its file.
#[test]
fn a_hard_link_to_a_target_removed_after_its_base_conflicts() -> Result<(), String> {
  for made_again in [true, false] {
    let (answer, made) = link_after_removal(made_again)?;
    let refused = classes_on(&answer, "linked") == Some(vec![MergeConflictClass::DeleteModify]);
    if !refused || made {
      return Err(format!(
        "made again {made_again}: {answer:?}, linked: {made}"
      ));
    }
  }
  let mut green = green();
  accept(&mut green, removal_of(TARGET, Kind::File), 1)?;
  let own = Work::default()
    .op(OpKind::Unlink, TARGET, (0, 0, u64::MAX))
    .create(TARGET, b"<its own>\n")
    .naming(OpKind::Link, "linked", TARGET);
  let answer = green.submit(&own.at(0, 9));
  let linked = green.hardlink("linked");
  if !matches!(answer, Outcome::Accepted { .. }) || linked != Some(TARGET) {
    return Err(format!("its own target: {answer:?}, linked: {linked:?}"));
  }
  Ok(())
}

/// A-646 (A-643's first sibling; POSIX `rename` replaces any destination but a directory). Do:
/// rename the file, and on other greens the symlink, over an unchanged symlink, hard link or IPC
/// node, each by a work based at the head. Expect each accepted, the destination holding the
/// source's node alone and the source's path empty. Before, each was refused `TypeChanged` though
/// nothing had changed after the base.
#[test]
fn a_rename_over_an_unchanged_link_or_ipc_node_replaces_it() -> Result<(), String> {
  for (source, kind) in [(FILE, Kind::File), (SYMLINK, Kind::Symlink)] {
    for destination in [SYMLINK, HARDLINK, SPECIAL] {
      if source == destination {
        continue;
      }
      let mut green = green();
      let rename = Work::default().naming(OpKind::Rename, destination, source);
      let answer = green.submit(&rename.at(0, 1));
      let found = (kinds_at(&green, destination), kinds_at(&green, source));
      if !matches!(answer, Outcome::Accepted { .. }) || found != (vec![kind], Vec::new()) {
        return Err(format!(
          "{source} over {destination}: {answer:?}, {found:?}"
        ));
      }
    }
  }
  Ok(())
}

/// A-646. Do: remove the symlink and make it again after version 0, then submit works based at 0
/// renaming the file over it, and over the directory. Expect `TypeChanged` each time and nothing
/// moved: a link changed after the base, and a directory, are still refused.
#[test]
fn a_rename_over_a_link_changed_after_its_base_or_a_directory_is_refused() -> Result<(), String> {
  let mut green = green();
  remake(&mut green, SYMLINK, 1)?;
  for (id, destination) in [(9, SYMLINK), (10, DIR)] {
    let head = green.head();
    let answer = green.submit(
      &Work::default()
        .naming(OpKind::Rename, destination, FILE)
        .at(0, id),
    );
    let refused = classes_on(&answer, destination) == Some(vec![MergeConflictClass::TypeChanged]);
    if !refused || green.head() != head || kinds_at(&green, FILE) != vec![Kind::File] {
      return Err(format!("over {destination}: {answer:?}"));
    }
  }
  Ok(())
}

/// What a green answers of `TARGET` and the hard links naming it: its kinds and bytes, and the
/// file `HARDLINK` and `second` name.
type Names<'a> = (Vec<Kind>, Option<Vec<u8>>, Option<&'a str>, Option<&'a str>);

/// What `green` answers of `TARGET` and the hard links `HARDLINK` and `second`.
fn names_of(green: &Green) -> Names<'_> {
  (
    kinds_at(green, TARGET),
    green.content(TARGET).map(<[u8]>::to_vec),
    green.hardlink(HARDLINK),
    green.hardlink("second"),
  )
}

/// A-646 (POSIX `rename` does nothing for two names of one file; A-643 settled a hard link renamed
/// over the file it names). Do: rename `TARGET` over its hard link `HARDLINK`; and, on another
/// green with a second hard link to `TARGET`, rename `HARDLINK` over it; each by a work based at
/// the head. Expect each accepted and nothing changed: the file with its bytes, each link naming
/// it. Before, each was refused `TypeChanged`.
#[test]
fn two_names_of_one_file_renamed_one_over_the_other_change_nothing() -> Result<(), String> {
  let before = (
    vec![Kind::File],
    Some(b"<g>\n".to_vec()),
    Some(TARGET),
    None,
  );
  let mut over_its_link = green();
  accept(
    &mut over_its_link,
    Work::default().naming(OpKind::Rename, HARDLINK, TARGET),
    1,
  )?;
  if names_of(&over_its_link) != before {
    return Err(format!(
      "the file over its link: {:?}",
      names_of(&over_its_link)
    ));
  }
  let mut over_a_link = green();
  accept(
    &mut over_a_link,
    Work::default().naming(OpKind::Link, "second", TARGET),
    1,
  )?;
  accept(
    &mut over_a_link,
    Work::default().naming(OpKind::Rename, "second", HARDLINK),
    2,
  )?;
  let expected = (before.0, before.1, Some(TARGET), Some(TARGET));
  if names_of(&over_a_link) != expected {
    return Err(format!("a link over a link: {:?}", names_of(&over_a_link)));
  }
  Ok(())
}

/// Shape: the paths A-643's oracle changes, `FILE`, `TARGET` (the file the links name), the
/// symlink, the hard link and the directory, each made at the origin as `ORIGINAL` names.
const PATHS: [&str; 5] = [FILE, TARGET, SYMLINK, HARDLINK, DIR];
/// Shape: the kind each of `PATHS` is made as, at the origin and whenever it is made again.
const ORIGINAL: [Kind; 5] = [
  Kind::File,
  Kind::File,
  Kind::Symlink,
  Kind::Hardlink,
  Kind::Dir,
];
/// Shape: the indices in `PATHS` of the nodes with a mode and attributes, the files and the
/// directory.
const WITH_METADATA: [usize; 3] = [0, 1, 4];
/// Shape: the indices in `PATHS` of the files a new hard link names.
const LINKABLE: [usize; 2] = [0, 1];
/// Shape: the indices in `PATHS` of the nodes a work renames: `FILE` and the two links.
const RENAMEABLE: [usize; 3] = [0, 2, 3];
/// Shape: the indices in `PATHS` of the nodes a rename replaces: the files and the two links.
const REPLACEABLE: [usize; 4] = [0, 1, 2, 3];
/// Shape: the index in `PATHS` of `TARGET`, the file every hard link of `PATHS` names.
const LINKED: usize = 1;

/// One generated event of A-643's oracle; an index names a path of `PATHS`, and `back` the
/// versions behind the head a work is based.
#[derive(Clone, Copy, Debug)]
enum Event {
  /// At the head: the node at the path removed if one stands, then made again as `ORIGINAL`.
  Remake(usize),
  /// At the head: the mode of the file or directory at the path changed.
  Mode(usize),
  /// At the head: `NAME` set on the file or directory at the path.
  Attribute(usize),
  /// A work removing the node at the path.
  Remove(usize, u8),
  /// A work making a new hard link to the file at the path.
  Link(usize, u8),
  /// A work renaming the node at the first path over the node at the second.
  RenameOver(usize, usize, u8),
}

fn event() -> impl Strategy<Value = Event> {
  // Shape: twelve selectors weighing the events, two paths, and a base up to four versions back.
  let draw = (0u8..12, 0..PATHS.len(), 0..PATHS.len(), 0u8..5);
  draw.prop_map(|(selector, which, other, back)| match selector {
    0..2 => Event::Remake(which),
    2..4 => Event::Mode(WITH_METADATA[which % WITH_METADATA.len()]),
    4..5 => Event::Attribute(WITH_METADATA[which % WITH_METADATA.len()]),
    5..8 => Event::Remove(which, back),
    8..10 => Event::Link(LINKABLE[which % LINKABLE.len()], back),
    _ => Event::RenameOver(
      RENAMEABLE[which % RENAMEABLE.len()],
      REPLACEABLE[other % REPLACEABLE.len()],
      back,
    ),
  })
}

/// How often each answer was met over every case: A-643's non-vacuity census.
#[derive(Debug, Default)]
struct Answers {
  /// A removal of a file or a link changed after its base, refused.
  removal_refused: u64,
  /// A removal of a file or a link unchanged since its base, accepted.
  removal_accepted: u64,
  /// A directory's removal after it changed, refused.
  rmdir_refused: u64,
  /// A directory's removal, unchanged since its base, accepted.
  rmdir_accepted: u64,
  /// A new hard link to a file removed after its base, refused.
  link_refused: u64,
  /// A new hard link, accepted.
  link_accepted: u64,
  /// A link's rename over the file, refused.
  over_refused: u64,
  /// A link's rename over the file, accepted: the link replaced the file.
  over_accepted: u64,
  /// A rename of two names of one file, one over the other, accepted: nothing changed.
  over_unchanged: u64,
  /// A rename over a symlink or a hard link the base held unchanged, accepted: the node replaced
  /// it (A-646).
  over_link: u64,
}

impl Answers {
  /// Counts a removal answered: of a directory or not, refused or accepted.
  fn removal(&mut self, dir: bool, refused: bool) {
    let count = match (dir, refused) {
      (true, true) => &mut self.rmdir_refused,
      (true, false) => &mut self.rmdir_accepted,
      (false, true) => &mut self.removal_refused,
      (false, false) => &mut self.removal_accepted,
    };
    *count += 1;
  }

  /// Every count, in the order the fields name them.
  fn counts(&self) -> [u64; 10] {
    [
      self.removal_refused,
      self.removal_accepted,
      self.rmdir_refused,
      self.rmdir_accepted,
      self.link_refused,
      self.link_accepted,
      self.over_refused,
      self.over_accepted,
      self.over_unchanged,
      self.over_link,
    ]
  }
}

/// A-643's model of a green under generated events: what stands at each of `PATHS` at every
/// version; the last version each path changed at in any dimension, its bytes changed at, and its
/// node was removed at (0: never after the origin); and the hard links works made.
struct Whole {
  green: Green,
  kinds: Vec<[Kind; 5]>,
  changed: [u64; 5],
  bytes_changed: [u64; 5],
  removed: [u64; 5],
  links: Vec<(String, &'static str)>,
  works: u64,
}

impl Whole {
  fn new() -> Whole {
    Whole {
      green: green(),
      kinds: vec![ORIGINAL],
      changed: [0; 5],
      bytes_changed: [0; 5],
      removed: [0; 5],
      links: Vec::new(),
      works: 0,
    }
  }

  /// What stood at `PATHS[index]` at `version`.
  fn kind(&self, version: u64, index: usize) -> Kind {
    usize::try_from(version)
      .ok()
      .and_then(|version| self.kinds.get(version))
      .and_then(|kinds| kinds.get(index))
      .copied()
      .unwrap_or_default()
  }

  /// What stands at `PATHS[index]` at the head.
  fn now(&self, index: usize) -> Kind {
    self.kind(self.green.head(), index)
  }

  /// The version `back` versions behind the head.
  fn base(&self, back: u8) -> u64 {
    self.green.head().saturating_sub(u64::from(back))
  }

  /// Submits `work` based at `base`; when accepted, the new version stands as the head did until
  /// the caller records what the work changed.
  fn submit(&mut self, work: Work, base: u64) -> Outcome {
    self.works += 1;
    let answer = self.green.submit(&work.at(base, self.works));
    if matches!(answer, Outcome::Accepted { .. }) {
      let head = self.kinds.last().copied().unwrap_or(ORIGINAL);
      self.kinds.push(head);
    }
    answer
  }

  /// Records at the head version what stands at `PATHS[index]`.
  fn set_kind(&mut self, index: usize, kind: Kind) {
    if let Some(slot) = self.kinds.last_mut().and_then(|kinds| kinds.get_mut(index)) {
      *slot = kind;
    }
  }

  /// Records that the head version removed the node at `PATHS[index]`.
  fn removed_now(&mut self, index: usize) {
    let version = self.green.head();
    if self.now(index) == Kind::File {
      self.bytes_changed[index] = version;
    }
    self.set_kind(index, Kind::Absent);
    self.changed[index] = version;
    self.removed[index] = version;
  }

  /// Records that the head version made a node of `kind` at `PATHS[index]`.
  fn made_now(&mut self, index: usize, kind: Kind) {
    let version = self.green.head();
    if kind == Kind::File {
      self.bytes_changed[index] = version;
    }
    self.set_kind(index, kind);
    self.changed[index] = version;
  }

  fn apply(&mut self, event: Event, answers: &mut Answers) -> Result<(), TestCaseError> {
    match event {
      Event::Remake(index) => self.remake(index),
      Event::Mode(index) => self.metadata(index, false),
      Event::Attribute(index) => self.metadata(index, true),
      Event::Remove(index, back) => self.remove(index, back, answers),
      Event::Link(index, back) => self.link(index, back, answers),
      Event::RenameOver(index, over, back) => self.rename_over((index, over), back, answers),
    }
  }

  /// At the head: whatever stands at `PATHS[index]` removed, then the path made again as its
  /// original kind; both accepted, since nothing changed after their base.
  fn remake(&mut self, index: usize) -> Result<(), TestCaseError> {
    let path = PATHS[index];
    let standing = self.now(index);
    if standing != Kind::Absent {
      let head = self.green.head();
      let answer = self.submit(removal_of(path, standing), head);
      prop_assert!(
        matches!(answer, Outcome::Accepted { .. }),
        "removing {path} at the head: {answer:?}"
      );
      self.removed_now(index);
    }
    let made = match ORIGINAL[index] {
      Kind::Symlink => Work::default().naming(OpKind::Symlink, path, TARGET),
      Kind::Hardlink => Work::default().naming(OpKind::Link, path, TARGET),
      Kind::Dir => Work::default().op(OpKind::Mkdir, path, (0, 0, u64::MAX)),
      _ => Work::default().create(path, b"<made again>\n"),
    };
    let head = self.green.head();
    let answer = self.submit(made, head);
    prop_assert!(
      matches!(answer, Outcome::Accepted { .. }),
      "making {path} at the head: {answer:?}"
    );
    self.made_now(index, ORIGINAL[index]);
    Ok(())
  }

  /// At the head: the mode of the file or directory at `PATHS[index]` changed or, for an
  /// `attribute`, `NAME` set on it, each value a new one; accepted.
  fn metadata(&mut self, index: usize, attribute: bool) -> Result<(), TestCaseError> {
    if !matches!(self.now(index), Kind::File | Kind::Dir) {
      return Ok(());
    }
    let path = PATHS[index];
    let value = self.works.to_le_bytes();
    let mode = 0o600 | u32::try_from(self.works % 0o100).unwrap_or_default();
    let work = if attribute {
      Work::default().set_xattr(path, NAME, &value)
    } else {
      Work::default().mode(path, mode)
    };
    let head = self.green.head();
    let answer = self.submit(work, head);
    prop_assert!(
      matches!(answer, Outcome::Accepted { .. }),
      "changing {path}'s metadata at the head: {answer:?}"
    );
    self.changed[index] = self.green.head();
    Ok(())
  }

  /// A work based `back` behind the head removing the node at `PATHS[index]`, where one stood at
  /// that base: refused `DeleteModify` when the node standing now changed after the base in any
  /// dimension (A-643), accepted otherwise.
  fn remove(&mut self, index: usize, back: u8, answers: &mut Answers) -> Result<(), TestCaseError> {
    let base = self.base(back);
    let saw = self.kind(base, index);
    if saw == Kind::Absent {
      return Ok(());
    }
    let path = PATHS[index];
    let stands = self.now(index) != Kind::Absent;
    let ruled = stands && self.changed[index] > base;
    let answer = self.submit(removal_of(path, saw), base);
    if ruled {
      let classes = classes_on(&answer, path);
      prop_assert!(
        classes == Some(vec![MergeConflictClass::DeleteModify]),
        "removing {path} from {base}: {answer:?}"
      );
    } else {
      prop_assert!(
        matches!(answer, Outcome::Accepted { .. }),
        "removing {path} from {base}: {answer:?}"
      );
      if stands {
        self.removed_now(index);
      }
    }
    if stands {
      answers.removal(saw == Kind::Dir, ruled);
    }
    Ok(())
  }

  /// A work based `back` behind the head making a new hard link to the file at `PATHS[index]`,
  /// where the file stood at that base: refused `DeleteModify` on the link's path when the file
  /// was removed after the base (A-643), accepted otherwise.
  fn link(&mut self, index: usize, back: u8, answers: &mut Answers) -> Result<(), TestCaseError> {
    let base = self.base(back);
    if self.kind(base, index) != Kind::File {
      return Ok(());
    }
    let (path, name) = (PATHS[index], format!("linked-{}", self.works));
    let ruled = self.removed[index] > base;
    let answer = self.submit(Work::default().naming(OpKind::Link, &name, path), base);
    if ruled {
      let classes = classes_on(&answer, &name);
      prop_assert!(
        classes == Some(vec![MergeConflictClass::DeleteModify]),
        "linking {name} to {path} from {base}: {answer:?}"
      );
      answers.link_refused += 1;
    } else {
      prop_assert!(
        matches!(answer, Outcome::Accepted { .. }),
        "linking {name} to {path} from {base}: {answer:?}"
      );
      self.links.push((name, path));
      answers.link_accepted += 1;
    }
    Ok(())
  }

  /// The file the node at `PATHS[index]` names now: `TARGET` for a hard link, else its own path.
  fn file_named(&self, index: usize) -> usize {
    if self.now(index) == Kind::Hardlink {
      LINKED
    } else {
      index
    }
  }

  /// The class a rename of the node at `PATHS[index]` over the node at `PATHS[over]`, based at
  /// `base`, is refused by, or none when it is accepted: `merge_rename`'s rules in its order (the
  /// source gone or removed after the base; a link at the destination changed after the base; the
  /// destination's bytes changed after the base).
  fn over_answer(&self, (index, over): (usize, usize), base: u64) -> Option<MergeConflictClass> {
    if self.now(index) == Kind::Absent || self.removed[index] > base {
      return Some(MergeConflictClass::RenameRename);
    }
    let link = matches!(self.now(over), Kind::Symlink | Kind::Hardlink);
    if link && self.changed[over] > base {
      return Some(MergeConflictClass::TypeChanged);
    }
    (self.bytes_changed[over] > base).then_some(MergeConflictClass::RenameRename)
  }

  /// A work based `back` behind the head renaming the node at `PATHS[index]` (a file or a link)
  /// over the node at `PATHS[over]`, where both stood at that base. Two names of one file change
  /// nothing (POSIX `rename`); any other rename is refused as `over_answer` says, or accepted, the
  /// source's node replacing what stood at the destination (A-643, A-646).
  fn rename_over(
    &mut self,
    (index, over): (usize, usize),
    back: u8,
    answers: &mut Answers,
  ) -> Result<(), TestCaseError> {
    let base = self.base(back);
    let stood = |kind: Kind| matches!(kind, Kind::File | Kind::Symlink | Kind::Hardlink);
    if index == over || !stood(self.kind(base, index)) || !stood(self.kind(base, over)) {
      return Ok(());
    }
    let same_file = self.file_named(index) == self.file_named(over);
    let expected = if same_file {
      None
    } else {
      self.over_answer((index, over), base)
    };
    let (from, to) = (PATHS[index], PATHS[over]);
    let answer = self.submit(Work::default().naming(OpKind::Rename, to, from), base);
    if let Some(class) = expected {
      let classes = classes_on(&answer, to);
      prop_assert!(
        classes == Some(vec![class]),
        "renaming {from} over {to} from {base}: {answer:?}"
      );
      answers.over_refused += 1;
      return Ok(());
    }
    prop_assert!(
      matches!(answer, Outcome::Accepted { .. }),
      "renaming {from} over {to} from {base}: {answer:?}"
    );
    if same_file {
      answers.over_unchanged += 1;
    } else {
      self.replaced(index, over, answers);
    }
    Ok(())
  }

  /// Records that the head version moved the node at `PATHS[index]` over `PATHS[over]`, replacing
  /// what stood there.
  fn replaced(&mut self, index: usize, over: usize, answers: &mut Answers) {
    let (moved, stood) = (self.now(index), self.now(over));
    match stood {
      Kind::Absent => {}
      Kind::File => answers.over_accepted += 1,
      _ => answers.over_link += 1,
    }
    if stood != Kind::Absent {
      self.removed_now(over);
    }
    self.made_now(over, moved);
    self.removed_now(index);
  }

  /// The green whole and as the model holds it: each of `PATHS` holds the one node the model
  /// says, or none, never two; each link a work made holds its link alone.
  fn check(&self) -> Result<(), TestCaseError> {
    for (index, path) in PATHS.iter().enumerate() {
      let expected: Vec<Kind> = Some(self.now(index))
        .filter(|kind| *kind != Kind::Absent)
        .into_iter()
        .collect();
      let found = kinds_at(&self.green, path);
      prop_assert!(
        found == expected,
        "{path}: {found:?}, the model {expected:?}"
      );
    }
    for (name, target) in &self.links {
      let found = kinds_at(&self.green, name);
      let names = self.green.hardlink(name);
      prop_assert!(
        found == vec![Kind::Hardlink] && names == Some(*target),
        "{name}: {found:?} naming {names:?}"
      );
    }
    Ok(())
  }
}

/// A-643's oracle (§4.16 "The verdict"; the green's state whole). Do: run generated histories over
/// a green of two files (`FILE`, and `TARGET`, which a symlink and a hard link name) and a
/// directory: at the head, nodes removed and made again and their modes and attributes changed;
/// works based behind the head removing a node, making a new hard link to a file, or renaming a
/// link over `FILE`. Expect after every event each path holding the one node the model says, or
/// none, never two; a removal of a node changed after its base in any dimension refused
/// `DeleteModify`, a link to a file removed after its base `DeleteModify` on the link, a link's
/// rename over the file answered by `merge_rename`'s rules and, accepted, replacing the file; and
/// every other work accepted. The census shows each answer met.
#[test]
fn the_green_stays_whole_and_no_work_removes_or_links_a_node_changed_after_its_base() {
  let answers = RefCell::new(Answers::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let result = runner.run(&prop::collection::vec(event(), 1..48), |events| {
    let mut whole = Whole::new();
    for event in events {
      whole.apply(event, &mut answers.borrow_mut())?;
      whole.check()?;
    }
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let answers = answers.into_inner();
  assert!(
    answers.counts().iter().all(|&count| count > 0),
    "an answer never met: {answers:?}"
  );
}

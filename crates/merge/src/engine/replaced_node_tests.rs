//! A work never changes a node replaced after its base (A-631; A-612's siblings; §4.16 "The
//! verdict": "delete against modify in either direction", "rename against rename"). A file, a
//! symbolic link, a hard link's name or a directory removed after a work's base is gone for that
//! work, whatever stands at its path now: a mode change or an attribute set or removed there is
//! delete against modify, and a rename of it rename against rename, never a change or a move of
//! the node that took its place.

use std::cell::RefCell;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::{Green, Increment, Outcome};
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::origin::Origin;
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

/// A green of `FILE` (mode 0o644, with `NAME`), `TARGET`, `SYMLINK` and `HARDLINK` to `TARGET`,
/// and the empty `DIR`.
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

//! A rebased work's journal is restated so a volume with real inodes replays it as the green reads the work (A-663;
//! §4.16 "Rebase, the only corrective path"; A-646's decision, a hard link names its node; POSIX `link`, `unlink` and
//! `rename`). A clean rebase hands the work its declared operations restated over the head (`Rebased::journal`): the
//! work keeps them as its journal, a volume with real inodes would replay them, and the work's next submit composes
//! them again (`crate::increment`). A hard link names the node standing at its target when it is made, so a restated
//! link must still meet that node where the restatement makes it: after the work made the node, and before any rename
//! or removal of the work moves the node away from the name the link names it by, or removes it there.
//!
//! The oracle reads a state as a reader of a volume does: each name's bytes and every name of its node. The work's own
//! operations, applied in their own order over the head to a model with real inodes (every name names a node, every
//! name of a node reads its bytes, a node goes with its last name), give what three other readings must equal: the
//! green that accepts the work, the restated journal replayed over the head into the same model, and the green that
//! accepts that journal composed again (the rebased work's next submit, as the server makes it).
//!
//! The works keep to what the deriver composes as a volume with real inodes replays it. That leaves out one kind of
//! composition, filed open in GAPS with A-663: a name freed and then made a file again, and a file the work made
//! renamed over a name that holds something, compose to an edit of that name's node in place, so a hard link naming the
//! node reads the new bytes. The works make no rename onto a hard link the work made either, which the deriver refuses
//! (`PathKindConflict`).
//!
//! A file the work wrote to, then removed or renamed another over while a hard link names its node, keeps the writes at
//! the link (A-669; A-646's decision, the node passes to its link as the work left it). The oracle's first run found
//! them lost (Windows x86_64, jupiter, 2026-11-19: `[(31, 13, 0), (22, 88, 0)]`, an append to `TARGET` and its removal,
//! `LINK` read without the append in all three readings), and its works left the case out until A-669 kept the
//! writes; its census now counts it. When only a link the work made still names the node the deriver refuses the work
//! (`Unsupported`, owed), and so it does a file made again at its own name after the write.
//!
//! The head may also link `LATE` to a file at version 1, a link no work based at 0 saw (A-670): a work that removes the
//! file, or renames another over it, with no link of its base to it left, is refused, since its writes went with the
//! node in its own volume while `LATE` keeps it; one whose base link outlives it is accepted. The census is met by one
//! fixed history a case before the generated works run, so a count can neither fall to zero by luck nor be met by none.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::{Green, Increment, Outcome, Rebased};
use crate::increment::{DeriveError, VolumeOp, compose_volume_planned};
use crate::ops_doc::OpKind;
use crate::origin::Origin;
use crate::verdict::MergeConflictClass;

/// Shape: the fixture's files the works change, the base's hard link naming `TARGET`, and the file only the head
/// changes.
const FILE: &str = "f";
/// Shape: see `FILE`.
const TARGET: &str = "g";
/// Shape: see `FILE`.
const LINK: &str = "h";
/// Shape: see `FILE`.
const ELSEWHERE: &str = "z";
/// Shape: the names the generated works use: the base's `FILE`, `TARGET` and `LINK`, and three free at the base.
const NAMES: [&str; 6] = [FILE, TARGET, LINK, "l", "m", "n"];
/// Shape: the increment identities of the head's version, a work, and a work's restated journal composed again.
const HEAD_WORK: u8 = 0xEE;
/// Shape: see `HEAD_WORK`.
const WORK: u8 = 0x01;
/// Shape: see `HEAD_WORK`.
const RESTATED: u8 = 0x02;
/// Shape: the bytes A-669's cases append.
const WRITE: &[u8] = b"<+w>\n";
/// Shape: the name the head's version links to a file, which no generated work names (A-670).
const LATE: &str = "k";

/// What a reader of a volume sees: each name's bytes (`None` for a hard link whose home holds no file) and every name
/// of its node.
type Seen = BTreeMap<String, (Option<Vec<u8>>, BTreeSet<String>)>;

/// What `seen` holds at one name.
type Reading = (Option<Vec<u8>>, BTreeSet<String>);

/// A volume with real inodes, as POSIX names files: each name names a node, every name of a node reads its bytes, and
/// a node goes with its last name.
#[derive(Clone, Debug, Default)]
struct Inodes {
  /// Each name and the number of the node it names.
  names: BTreeMap<String, usize>,
  /// Each node's bytes, by number.
  nodes: Vec<Vec<u8>>,
}

impl Inodes {
  /// `green`'s head as a volume: each file a node, each hard link a name of its home's node.
  fn of(green: &Green) -> Inodes {
    let mut volume = Inodes::default();
    for (path, bytes) in green.files() {
      let node = volume.nodes.len();
      volume.names.insert(path, node);
      volume.nodes.push(bytes.to_vec());
    }
    for (link, home) in green.hardlinks.iter() {
      if let Some(node) = volume.names.get(home.as_str()).copied() {
        volume.names.insert(link, node);
      }
    }
    volume
  }

  /// The node `path` names.
  fn node(&self, path: &str) -> Result<usize, String> {
    self
      .names
      .get(path)
      .copied()
      .ok_or_else(|| format!("{path} names nothing"))
  }

  /// The bytes of the node `path` names.
  fn bytes(&self, path: &str) -> Result<Vec<u8>, String> {
    let node = self.node(path)?;
    self
      .nodes
      .get(node)
      .cloned()
      .ok_or_else(|| format!("{path}'s node is not held"))
  }

  /// Refuses a name already held, as `link` and `creat` with `O_EXCL` refuse one (`EEXIST`).
  fn free(&self, path: &str) -> Result<(), String> {
    if self.names.contains_key(path) {
      return Err(format!("{path} is held"));
    }
    Ok(())
  }

  /// A new node at the free name `path`, holding `bytes`.
  fn create(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
    self.free(path)?;
    let node = self.nodes.len();
    self.names.insert(path.to_owned(), node);
    self.nodes.push(bytes.to_vec());
    Ok(())
  }

  /// Node `node`'s bytes made `bytes`.
  fn write(&mut self, node: usize, bytes: Vec<u8>) -> Result<(), String> {
    *self
      .nodes
      .get_mut(node)
      .ok_or("a written node is not held")? = bytes;
    Ok(())
  }

  /// `bytes` written at the end of the node `path` names.
  fn append(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
    let node = self.node(path)?;
    let mut grown = self.bytes(path)?;
    grown.extend_from_slice(bytes);
    self.write(node, grown)
  }

  /// The free name `path` made a name of the node `target` names (`link`).
  fn link(&mut self, path: &str, target: &str) -> Result<(), String> {
    let node = self.node(target)?;
    self.free(path)?;
    self.names.insert(path.to_owned(), node);
    Ok(())
  }

  /// The name `path` removed (`unlink`): its node goes with its last name.
  fn unlink(&mut self, path: &str) -> Result<(), String> {
    match self.names.remove(path) {
      Some(_) => Ok(()),
      None => Err(format!("{path} names nothing to unlink")),
    }
  }

  /// The name `from` moved to `to`, replacing what `to` names (`rename`); nothing changes when both name one node.
  fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
    let node = self.node(from)?;
    if self.names.get(to) != Some(&node) {
      self.names.remove(from);
      self.names.insert(to.to_owned(), node);
    }
    Ok(())
  }

  /// A namespace operation of a restated journal, applied (its content operations are `replay`'s).
  fn apply(&mut self, op: &VolumeOp) -> Result<(), String> {
    match op {
      VolumeOp::Create { path } => self.create(path, &[]),
      VolumeOp::Unlink { path } => self.unlink(path),
      VolumeOp::Rename { from, to } => self.rename(from, to),
      VolumeOp::Link { path, target } => self.link(path, target),
      other => Err(format!("a restatement these works never need: {other:?}")),
    }
  }

  /// Each name's bytes, owned: the files a seal reads.
  fn files(&self) -> BTreeMap<String, Vec<u8>> {
    self
      .names
      .iter()
      .filter_map(|(name, node)| Some((name.clone(), self.nodes.get(*node)?.clone())))
      .collect()
  }

  /// What a reader sees.
  fn seen(&self) -> Seen {
    let mut names: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    for (name, node) in &self.names {
      names.entry(*node).or_default().insert(name.clone());
    }
    let mut seen = Seen::new();
    for (node, all) in names {
      let bytes = self.nodes.get(node).cloned();
      for name in &all {
        seen.insert(name.clone(), (bytes.clone(), all.clone()));
      }
    }
    seen
  }
}

/// What a reader of `green`'s head sees (A-646: a node is its home, the path holding its bytes, and the hard links
/// naming it there).
fn seen_in(green: &Green) -> Seen {
  let mut names: BTreeMap<String, BTreeSet<String>> = green
    .files()
    .map(|(path, _)| (path.clone(), BTreeSet::from([path])))
    .collect();
  for (link, home) in green.hardlinks.iter() {
    names.entry(home.clone()).or_default().insert(link);
  }
  let mut seen = Seen::new();
  for (home, all) in names {
    let bytes = green.content(&home).map(<[u8]>::to_vec);
    for name in &all {
      seen.insert(name.clone(), (bytes.clone(), all.clone()));
    }
  }
  seen
}

/// One operation of a work, as its agent made it in its own volume.
#[derive(Clone, Debug)]
enum Step {
  /// A new file at a free name, holding the bytes.
  Create(String, Vec<u8>),
  /// The bytes written at the end of the file a name names.
  Append(String, Vec<u8>),
  /// The name removed.
  Unlink(String),
  /// The first name moved to the second.
  Rename(String, String),
  /// The first name, free, made a hard link to the node the second names.
  Link(String, String),
}

impl Step {
  /// The step applied to `volume`.
  fn apply(&self, volume: &mut Inodes) -> Result<(), String> {
    match self {
      Step::Create(path, bytes) => volume.create(path, bytes),
      Step::Append(path, bytes) => volume.append(path, bytes),
      Step::Unlink(path) => volume.unlink(path),
      Step::Rename(from, to) => volume.rename(from, to),
      Step::Link(path, target) => volume.link(path, target),
    }
  }

  /// The operations the work's journal declares for the step over `volume` as it stands before it (§4.5): a write past
  /// a file's end is `Extend` at its length then.
  fn declared(&self, volume: &Inodes) -> Result<Vec<VolumeOp>, String> {
    Ok(match self {
      Step::Create(path, bytes) => vec![
        VolumeOp::Create { path: path.clone() },
        VolumeOp::Extend {
          path: path.clone(),
          at: 0,
          len: length(bytes)?,
        },
      ],
      Step::Append(path, bytes) => vec![VolumeOp::Extend {
        path: path.clone(),
        at: length(&volume.bytes(path)?)?,
        len: length(bytes)?,
      }],
      Step::Unlink(path) => vec![VolumeOp::Unlink { path: path.clone() }],
      Step::Rename(from, to) => vec![VolumeOp::Rename {
        from: from.clone(),
        to: to.clone(),
      }],
      Step::Link(path, target) => vec![VolumeOp::Link {
        path: path.clone(),
        target: target.clone(),
      }],
    })
  }
}

/// A length as a journal counts it.
fn length(bytes: &[u8]) -> Result<u64, String> {
  u64::try_from(bytes.len()).map_err(|error| error.to_string())
}

/// A work's steps applied in its own volume over `base`, and the journal they declare.
fn declare(base: &Inodes, steps: &[Step]) -> Result<(Inodes, Vec<VolumeOp>), String> {
  let mut volume = base.clone();
  let mut journal = Vec::new();
  for step in steps {
    journal.extend(step.declared(&volume)?);
    step.apply(&mut volume)?;
  }
  Ok((volume, journal))
}

/// The increment `id` composing `journal` over `green`'s version `base` (its restricted base, `Green::base_for`), each
/// run of its post-state read from `files` (each name's bytes at the seal) where the deriver's plan puts it, as the
/// server's seal assembles one; the deriver's refusal when it refuses the journal.
fn compose(
  green: &Green,
  (base, id): (u64, u8),
  journal: &[VolumeOp],
  files: &BTreeMap<String, Vec<u8>>,
) -> Result<Result<Increment, DeriveError>, String> {
  let (doc, plan) = match compose_volume_planned(&green.base_for(base, journal), journal) {
    Ok(planned) => planned,
    Err(refused) => return Ok(Err(refused)),
  };
  let mut post_state = Vec::new();
  for op in doc.ops.iter().filter(|op| adds_bytes(op.kind)) {
    let path = doc
      .paths
      .path(op.path)
      .ok_or("an operation names no path")?;
    let run = run_of(files.get(path), plan.get(&op.src), op.len)
      .ok_or_else(|| format!("{path}'s run is not in its file"))?;
    lay(&mut post_state, op.src, run)?;
  }
  Ok(Ok(Increment {
    id: [id; 32],
    base,
    doc,
    post_state,
    evidence: Vec::new(),
    named: Default::default(),
  }))
}

/// Whether an operation of `kind` adds bytes from the post-state.
fn adds_bytes(kind: OpKind) -> bool {
  matches!(kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend)
}

/// The `len` bytes at `from` in `file`: a run as a seal reads it.
fn run_of<'a>(file: Option<&'a Vec<u8>>, from: Option<&u64>, len: u64) -> Option<&'a [u8]> {
  let start = usize::try_from(*from?).ok()?;
  let end = start.checked_add(usize::try_from(len).ok()?)?;
  file?.get(start..end)
}

/// `run` laid into `post_state` at `at`, the post-state grown to hold it.
fn lay(post_state: &mut Vec<u8>, at: u64, run: &[u8]) -> Result<(), String> {
  let start = usize::try_from(at).map_err(|error| error.to_string())?;
  let end = start
    .checked_add(run.len())
    .ok_or("a run past the address space")?;
  if post_state.len() < end {
    post_state.resize(end, 0);
  }
  post_state
    .get_mut(start..end)
    .ok_or("a run outside the post-state")?
    .copy_from_slice(run);
  Ok(())
}

/// The fixture at version 0, the works' base: `FILE`, `TARGET` and `ELSEWHERE`, each holding its own name, and `LINK`
/// a hard link to `TARGET`.
fn base_green() -> Green {
  let mut origin = Origin::default();
  for path in [FILE, TARGET, ELSEWHERE] {
    origin
      .files
      .push((path.to_owned(), format!("<{path}>\n").into_bytes()));
  }
  origin.hardlinks.push((LINK.to_owned(), TARGET.to_owned()));
  Green::with_origin(origin)
}

/// The head the works are rebased over (A-670): version 1 appends to `ELSEWHERE`, and may also link `LATE` to a file,
/// a link no work based at version 0 saw: `FILE`, which the base links nothing to, or `TARGET`, which `LINK` names.
#[derive(Clone, Copy, Debug)]
enum Head {
  /// `ELSEWHERE` appended to.
  Plain,
  /// That, and `LATE` linked to `FILE`.
  LinksFile,
  /// That, and `LATE` linked to `TARGET`.
  LinksTarget,
}

impl Head {
  /// The head a generated byte picks.
  fn of(choice: u8) -> Head {
    match choice % 3 {
      0 => Head::Plain,
      1 => Head::LinksFile,
      _ => Head::LinksTarget,
    }
  }

  /// The file the head links `LATE` to, if it does.
  fn linked(self) -> Option<&'static str> {
    match self {
      Head::Plain => None,
      Head::LinksFile => Some(FILE),
      Head::LinksTarget => Some(TARGET),
    }
  }

  /// The head's work, version 1, over version 0.
  fn steps(self) -> Vec<Step> {
    let appended = Step::Append(ELSEWHERE.to_owned(), b"<z+>\n".to_vec());
    match self.linked() {
      Some(file) => vec![appended, link(LATE, file)],
      None => vec![appended],
    }
  }
}

/// The fixture at its head, version 1 (`head`), so a work based at 0 is rebased past a version it did not see; with the
/// volume at version 0, where the works start.
fn fixture(head: Head) -> Result<(Green, Inodes), String> {
  let mut green = base_green();
  let base = Inodes::of(&green);
  let (volume, journal) = declare(&base, &head.steps())?;
  let increment = compose(&green, (0, HEAD_WORK), &journal, &volume.files())?
    .map_err(|refused| format!("the head's work does not compose: {refused:?}"))?;
  match green.submit(&increment) {
    Outcome::Accepted { version: 1 } => Ok((green, base)),
    other => Err(format!("the head's work was answered {other:?}")),
  }
}

/// What a clean rebase hands the work: its new base, the files it holds over it, and its journal restated.
struct Restatement {
  version: u64,
  changed: BTreeMap<String, Option<Vec<u8>>>,
  journal: Vec<VolumeOp>,
}

impl Restatement {
  /// The bytes the rebased work holds at `path`: its own where it changed them, else the head's.
  fn bytes(&self, head: &Inodes, path: &str) -> Result<Vec<u8>, String> {
    match self.changed.get(path) {
      Some(Some(bytes)) => Ok(bytes.clone()),
      Some(None) => Err(format!("{path} is written and removed")),
      None => head.bytes(path),
    }
  }

  /// The rebased work's files: the head's, with its changes over them.
  fn files(&self, head: &Inodes) -> BTreeMap<String, Vec<u8>> {
    let mut files = head.files();
    for (path, after) in &self.changed {
      match after {
        Some(bytes) => files.insert(path.clone(), bytes.clone()),
        None => files.remove(path),
      };
    }
    files
  }
}

/// The name a content operation writes; `None` for a namespace operation.
fn written_name(op: &VolumeOp) -> Option<&str> {
  match op {
    VolumeOp::Overwrite { path, .. }
    | VolumeOp::Extend { path, .. }
    | VolumeOp::Truncate { path, .. }
    | VolumeOp::Insert { path, .. }
    | VolumeOp::Delete { path, .. } => Some(path),
    _ => None,
  }
}

/// The restated journal replayed over the head's volume `head`, as a volume with real inodes replays it, each node a
/// content operation writes taking the bytes the rebased work holds at the name written, or, where the journal then
/// removes that name, at the name the node is left to (A-669: writes handed on to a link); a node no name keeps is gone
/// with its writes.
fn replay(head: &Inodes, restated: &Restatement) -> Result<Inodes, String> {
  let mut volume = head.clone();
  let mut written = Vec::new();
  for op in &restated.journal {
    match written_name(op) {
      Some(path) => written.push((volume.node(path)?, path)),
      None => volume.apply(op)?,
    }
  }
  for (node, path) in written {
    write_held(&mut volume, (head, restated), (node, path))?;
  }
  Ok(volume)
}

/// Node `node`, which a content operation wrote at `path`, given the bytes the rebased work holds where the replayed
/// `volume` keeps it ([`holding_name`]); nothing where no name keeps it.
fn write_held(
  volume: &mut Inodes,
  (head, restated): (&Inodes, &Restatement),
  (node, path): (usize, &str),
) -> Result<(), String> {
  match holding_name(volume, restated, (node, path))? {
    Some(name) => volume.write(node, restated.bytes(head, &name)?),
    None => Ok(()),
  }
}

/// The name the rebased work reads node `node`'s bytes at once `volume` replayed the journal: `written`, the name a
/// content operation wrote, while it still names the node; else the name of the node whose bytes the rebased work
/// changed, the link the node was handed to; `None` where no name keeps the node.
fn holding_name(
  volume: &Inodes,
  restated: &Restatement,
  (node, written): (usize, &str),
) -> Result<Option<String>, String> {
  if volume.names.get(written) == Some(&node) {
    return Ok(Some(written.to_owned()));
  }
  let names: Vec<&String> = volume
    .names
    .iter()
    .filter(|(_, named)| **named == node)
    .map(|(name, _)| name)
    .collect();
  if names.is_empty() {
    return Ok(None);
  }
  names
    .into_iter()
    .find(|name| matches!(restated.changed.get(name.as_str()), Some(Some(_))))
    .map(|name| Some(name.clone()))
    .ok_or_else(|| format!("{written}'s node is left at no name the rebased work changed"))
}

/// What a reader sees of a second fixture's green, at `head`, once it accepts the restated journal composed again over
/// the head (`volume`), its post-state read from the rebased work's files: the rebased work's next submit, as the server
/// makes it.
fn resubmitted((head, volume): (Head, &Inodes), restated: &Restatement) -> Result<Seen, String> {
  let (mut green, _) = fixture(head)?;
  let head = volume;
  let files = restated.files(head);
  let increment = compose(
    &green,
    (restated.version, RESTATED),
    &restated.journal,
    &files,
  )?
  .map_err(|refused| format!("the restated journal does not compose: {refused:?}"))?;
  match green.submit(&increment) {
    Outcome::Accepted { .. } => Ok(seen_in(&green)),
    other => Err(format!("the restated journal was answered {other:?}")),
  }
}

/// The work's own operations applied in their own order over the head's volume `head`, as a reader sees the result.
fn own_order(head: &Inodes, steps: &[Step]) -> Result<Seen, String> {
  let mut volume = head.clone();
  for step in steps {
    step.apply(&mut volume)?;
  }
  Ok(volume.seen())
}

/// One reading of a rebased work: what it is, and what a reader saw there or why nothing could be read.
type Found<'a> = (&'a str, Result<Seen, String>);

/// Holds every reading to what the work's own order gives, naming each that differs or could not be read, so a red
/// run shows every reading a defect reaches.
fn agree(own: &Seen, readings: [Found<'_>; 3]) -> Result<(), String> {
  let differing: Vec<String> = readings
    .into_iter()
    .filter_map(|(reading, seen)| match seen {
      Ok(seen) if seen == *own => None,
      Ok(seen) => Some(format!("{reading} reads {seen:?}")),
      Err(error) => Some(format!("{reading}: {error}")),
    })
    .collect();
  if differing.is_empty() {
    return Ok(());
  }
  Err(format!(
    "{}; the work's own order reads {own:?}",
    differing.join("; ")
  ))
}

/// How one work came out of the oracle.
#[derive(Debug)]
enum Restated {
  /// The deriver refused its journal: a composition owed, typed (`DeriveError`).
  Refused,
  /// The verdict refused its rebase, and its submit alike.
  Conflict,
  /// It rebased, and the four readings agreed on what a reader sees, given here.
  Agreed(Seen),
}

/// The oracle on one work, its `steps` based at the fixture's version 0 and rebased past version 1 (`head`): the green
/// that accepts it, its restated journal replayed over the head, and the green that accepts that journal composed again
/// must each read as the work's own operations in their own order over the head.
fn restate(head: Head, steps: &[Step]) -> Result<Restated, String> {
  let (mut green, base) = fixture(head)?;
  let at_head = Inodes::of(&green);
  let (volume, journal) = declare(&base, steps)?;
  let Ok(increment) = compose(&green, (0, WORK), &journal, &volume.files())? else {
    return Ok(Restated::Refused);
  };
  let own = own_order(&at_head, steps)?;
  let rebased = green.rebase(&increment);
  match (rebased, green.submit(&increment)) {
    (
      Rebased::Rebased {
        version,
        changed,
        journal,
      },
      Outcome::Accepted { .. },
    ) => {
      let restated = Restatement {
        version,
        changed,
        journal,
      };
      agree(
        &own,
        [
          ("the green that accepts the work", Ok(seen_in(&green))),
          (
            "the restated journal replayed",
            replay(&at_head, &restated).map(|volume| volume.seen()),
          ),
          (
            "the green that accepts it composed again",
            resubmitted((head, &at_head), &restated),
          ),
        ],
      )?;
      Ok(Restated::Agreed(own))
    }
    (Rebased::Conflict { .. }, Outcome::Conflict { .. }) => Ok(Restated::Conflict),
    (rebased, submitted) => Err(format!(
      "the rebase answered {rebased:?} and the submit {submitted:?}"
    )),
  }
}

/// A link step.
fn link(path: &str, target: &str) -> Step {
  Step::Link(path.to_owned(), target.to_owned())
}

/// A rename step.
fn rename(from: &str, to: &str) -> Step {
  Step::Rename(from.to_owned(), to.to_owned())
}

/// A removal step.
fn unlink(path: &str) -> Step {
  Step::Unlink(path.to_owned())
}

/// The four readings of `steps`, agreed, over the plain head.
fn agreed(steps: &[Step]) -> Result<Seen, String> {
  agreed_over(Head::Plain, steps)
}

/// The four readings of `steps`, agreed, over `head`.
fn agreed_over(head: Head, steps: &[Step]) -> Result<Seen, String> {
  match restate(head, steps)? {
    Restated::Agreed(seen) => Ok(seen),
    other => Err(format!("{other:?}")),
  }
}

/// Holds what `seen` reads at `name`: `bytes`, on a node `names` alone name.
fn reads(seen: &Seen, name: &str, (bytes, names): (&[u8], &[&str])) -> Result<(), String> {
  let expected: Reading = (
    Some(bytes.to_vec()),
    names.iter().map(|other| (*other).to_owned()).collect(),
  );
  match seen.get(name) {
    Some(found) if *found == expected => Ok(()),
    found => Err(format!("{name} reads {found:?}, not {expected:?}")),
  }
}

/// Holds that nothing stands at `name`.
fn absent(seen: &Seen, name: &str) -> Result<(), String> {
  match seen.get(name) {
    None => Ok(()),
    Some(found) => Err(format!("{name} reads {found:?}, not nothing")),
  }
}

/// A-663, the first case (POSIX: `ln g l; mv f g` keeps `l` on `g`'s old file). Do: rebase a work, based behind the
/// head, that links `l` to `TARGET` and then renames `FILE` over `TARGET`. Expect the four readings to agree: `l` on
/// `TARGET`'s old node with `LINK`, its bytes `<g>`, and `TARGET` holding `FILE`'s bytes alone. Before, the link was
/// restated after the rename: the replay put `l` on `FILE`'s node, and the journal composed again named it too.
#[test]
fn a_link_made_before_a_rename_onto_its_target_stays_on_the_old_node() -> Result<(), String> {
  let seen = agreed(&[link("l", TARGET), rename(FILE, TARGET)])?;
  reads(&seen, "l", (b"<g>\n", &[LINK, "l"]))?;
  reads(&seen, TARGET, (b"<f>\n", &[TARGET]))
}

/// A-663, the second case (A-646: a link to the node a work renamed there names the rename's source). Do: rebase a
/// work that renames `FILE` to `m` and then links `l` to `m`. Expect the four readings to agree: `l` and `m` one node
/// holding `FILE`'s bytes, and nothing at `FILE`. Before, the link was restated after the rename, naming `FILE`, which
/// the rename had emptied, so a volume refused it.
#[test]
fn a_link_to_a_node_the_work_renamed_is_restated_before_the_rename() -> Result<(), String> {
  let seen = agreed(&[rename(FILE, "m"), link("l", "m")])?;
  reads(&seen, "l", (b"<f>\n", &["l", "m"]))?;
  absent(&seen, FILE)
}

/// A-663, the second case's other order (`ln f l; mv f m`: a link names the node, which the rename moves). Do: rebase a
/// work that links `l` to `FILE` and then renames `FILE` to `m`. Expect the four readings to agree: `l` and `m` one
/// node holding `FILE`'s bytes, and nothing at `FILE`. Before, the link was restated after the rename, naming the
/// emptied `FILE`.
#[test]
fn a_link_made_before_its_node_is_renamed_follows_it() -> Result<(), String> {
  let seen = agreed(&[link("l", FILE), rename(FILE, "m")])?;
  reads(&seen, "m", (b"<f>\n", &["l", "m"]))?;
  absent(&seen, FILE)
}

/// A-663, the third case (`ln f l; rm f`: a node stays with its last name). Do: rebase a work that links `l` to `FILE`
/// and then removes `FILE`. Expect the four readings to agree: `l` alone holding `FILE`'s bytes, and nothing at
/// `FILE`. Before, the link was restated after the removal, naming an emptied name.
#[test]
fn a_link_to_a_node_the_work_removes_keeps_the_node() -> Result<(), String> {
  let seen = agreed(&[link("l", FILE), unlink(FILE)])?;
  reads(&seen, "l", (b"<f>\n", &["l"]))?;
  absent(&seen, FILE)
}

/// A-663, a fourth case found with it (POSIX `link` refuses a name already held). Do: rebase a work that removes the
/// base's link `LINK` and links `LINK` to `FILE`. Expect the four readings to agree: `LINK` and `FILE` one node holding
/// `FILE`'s bytes, and `TARGET` alone with its own. Before, the link was restated with no removal of the head's
/// `LINK`: a volume refused the name, and the deriver refused the journal (`LinkOverExisting`), so the rebased work
/// could not submit again.
#[test]
fn a_link_made_again_over_the_heads_link_is_restated_with_its_removal() -> Result<(), String> {
  let seen = agreed(&[unlink(LINK), link(LINK, FILE)])?;
  reads(&seen, LINK, (b"<f>\n", &[FILE, LINK]))?;
  reads(&seen, TARGET, (b"<g>\n", &[TARGET]))
}

/// An append step: `WRITE` written at the end of the file `path` names.
fn append(path: &str) -> Step {
  Step::Append(path.to_owned(), WRITE.to_vec())
}

/// The deriver's refusal of a work of `steps` based at the fixture's version 0, or why there is none.
fn refusal(steps: &[Step]) -> Result<DeriveError, String> {
  let (green, base) = fixture(Head::Plain)?;
  let (volume, journal) = declare(&base, steps)?;
  match compose(&green, (0, WORK), &journal, &volume.files())? {
    Err(refused) => Ok(refused),
    Ok(_) => Err("the deriver composed the work".to_owned()),
  }
}

/// Holds that `refused` is `Unsupported(TARGET)`, a composition owed.
fn owed_at_target(refused: DeriveError) -> Result<(), String> {
  match refused {
    DeriveError::Unsupported(path) if path == TARGET => Ok(()),
    other => Err(format!("refused {other:?}, not Unsupported({TARGET})")),
  }
}

/// A-669, the first case (POSIX: `echo w >> g; rm g` leaves the written node to `g`'s other name). Do: rebase a work,
/// based behind the head, that appends to `TARGET` and then removes it while `LINK` names its node. Expect the four
/// readings to agree: `LINK` alone holds `<g>` and the write, and nothing stands at `TARGET`. Before, the deriver
/// dropped a removed file's writes and the green handed `LINK` the head's bytes: the work was accepted without them.
#[test]
fn a_write_to_a_file_a_link_names_is_kept_when_the_work_removes_the_file() -> Result<(), String> {
  let seen = agreed(&[append(TARGET), unlink(TARGET)])?;
  reads(&seen, LINK, (b"<g>\n<+w>\n", &[LINK]))?;
  absent(&seen, TARGET)
}

/// A-669, the second case (`echo w >> g; mv f g`: the rename takes the name, and the written node stays at its link).
/// Do: rebase a work that appends to `TARGET` and then renames `FILE` over it. Expect the four readings to agree:
/// `LINK` alone holds `<g>` and the write, `TARGET` holds `FILE`'s bytes, and nothing stands at `FILE`. Before, the
/// replaced file's writes were dropped with it and `LINK` read the head's bytes.
#[test]
fn a_write_to_a_file_a_link_names_is_kept_when_another_is_renamed_over_it() -> Result<(), String> {
  let seen = agreed(&[append(TARGET), rename(FILE, TARGET)])?;
  reads(&seen, LINK, (b"<g>\n<+w>\n", &[LINK]))?;
  reads(&seen, TARGET, (b"<f>\n", &[TARGET]))?;
  absent(&seen, FILE)
}

/// A-669, every name left reads the write: a link the work made before it keeps the node too. Do: rebase a work that
/// links `l` to `TARGET`, appends to `TARGET` and removes it. Expect the four readings to agree: `LINK` and `l` one
/// node holding `<g>` and the write. Before, both read the head's bytes.
#[test]
fn a_write_handed_on_is_read_at_every_name_left() -> Result<(), String> {
  let seen = agreed(&[link("l", TARGET), append(TARGET), unlink(TARGET)])?;
  reads(&seen, LINK, (b"<g>\n<+w>\n", &[LINK, "l"]))?;
  reads(&seen, "l", (b"<g>\n<+w>\n", &[LINK, "l"]))
}

/// A-669, refused rather than lost: the only name left is a link the work made. Do: compose a work that removes
/// `LINK`, links `l` to `TARGET`, appends to `TARGET` and removes it. Expect the deriver's typed refusal,
/// `Unsupported(TARGET)`, a composition owed. Before, it composed, and the green handed `l` the head's bytes.
#[test]
fn a_write_left_only_to_a_link_the_work_made_is_refused_typed() -> Result<(), String> {
  let steps = [
    unlink(LINK),
    link("l", TARGET),
    append(TARGET),
    unlink(TARGET),
  ];
  owed_at_target(refusal(&steps)?)
}

/// A-669, refused rather than lost: the work made the file again at its name after the write, which the deriver
/// composes as an edit of the old node in place (GAPS, open since A-663). Do: compose a work that appends to `TARGET`,
/// removes it and creates `TARGET` again while `LINK` names the old node. Expect `Unsupported(TARGET)`. Before, it
/// composed to an edit of `TARGET`, the write lost and `LINK` reading the new file.
#[test]
fn a_write_to_a_file_made_again_at_its_name_is_refused_typed() -> Result<(), String> {
  let made_again = Step::Create(TARGET.to_owned(), b"<g again>\n".to_vec());
  owed_at_target(refusal(&[append(TARGET), unlink(TARGET), made_again])?)
}

/// What a work of `steps`, based at version 0, is answered when submitted over `head`.
fn verdict_over(head: Head, steps: &[Step]) -> Result<Outcome, String> {
  let (mut green, base) = fixture(head)?;
  let (volume, journal) = declare(&base, steps)?;
  let increment = compose(&green, (0, WORK), &journal, &volume.files())?
    .map_err(|refused| format!("the work does not compose: {refused:?}"))?;
  Ok(green.submit(&increment))
}

/// Holds that `outcome` is a conflict with a window of `class` at `path`.
fn conflicts_at(outcome: Outcome, (path, class): (&str, MergeConflictClass)) -> Result<(), String> {
  match outcome {
    Outcome::Conflict { windows }
      if windows
        .iter()
        .any(|window| window.path == path && window.class == class) =>
    {
      Ok(())
    }
    other => Err(format!("answered {other:?}, not {class:?} at {path}")),
  }
}

/// A-670, the first case: a write to a file a later version linked, then the file removed. Do: over the head whose
/// version 1 linked `LATE` to `FILE`, which the work's base linked nothing to, submit a work based at 0 that appends to
/// `FILE` and removes it. Expect delete against modify at `FILE`: the work's volume took the write away with the node,
/// which `LATE` keeps at the head, and the increment cannot carry it. Before, the removal was accepted and `LATE` read
/// the head's bytes, the write lost.
#[test]
fn a_write_to_a_file_linked_later_is_refused_when_the_work_removes_it() -> Result<(), String> {
  let outcome = verdict_over(Head::LinksFile, &[append(FILE), unlink(FILE)])?;
  conflicts_at(outcome, (FILE, MergeConflictClass::DeleteModify))
}

/// A-670, the second case: that file replaced by a rename after the write. Do: over the same head, submit a work that
/// appends to `FILE` and renames `TARGET` over it. Expect rename against rename at `FILE`. Before, accepted, `LATE`
/// reading the head's bytes.
#[test]
fn a_write_to_a_file_linked_later_is_refused_when_renamed_over() -> Result<(), String> {
  let outcome = verdict_over(Head::LinksFile, &[append(FILE), rename(TARGET, FILE)])?;
  conflicts_at(outcome, (FILE, MergeConflictClass::RenameRename))
}

/// A-670, refused without a write too, since an increment does not say whether its work wrote. Do: over the same head,
/// submit a work that removes `FILE`. Expect delete against modify at `FILE`; the work rebased over the link removes it
/// again, the link keeping the node. Before, accepted.
#[test]
fn a_removal_of_a_file_linked_later_is_refused_without_a_write_too() -> Result<(), String> {
  let outcome = verdict_over(Head::LinksFile, &[unlink(FILE)])?;
  conflicts_at(outcome, (FILE, MergeConflictClass::DeleteModify))
}

/// A-670, the base link the work removed itself: its write went with the node in its own volume. Do: over the head whose
/// version 1 linked `LATE` to `TARGET`, submit a work that removes `LINK`, appends to `TARGET` and removes it. Expect
/// delete against modify at `TARGET`. Before, accepted, `LATE` reading the head's bytes.
#[test]
fn a_write_whose_base_link_the_work_removed_is_refused_when_linked_later() -> Result<(), String> {
  let steps = [unlink(LINK), append(TARGET), unlink(TARGET)];
  let outcome = verdict_over(Head::LinksTarget, &steps)?;
  conflicts_at(outcome, (TARGET, MergeConflictClass::DeleteModify))
}

/// A-670, kept: a base link that outlives the work declares its writes there (A-669), so a later link loses nothing. Do:
/// over the head whose version 1 linked `LATE` to `TARGET`, which `LINK` names at the base, rebase a work that removes
/// `TARGET`, and one that appends to it first. Expect the four readings to agree: `LINK` and `LATE` one node, without
/// the write and then with it.
#[test]
fn a_file_a_base_link_outlives_is_removed_whatever_is_linked_later() -> Result<(), String> {
  let seen = agreed_over(Head::LinksTarget, &[unlink(TARGET)])?;
  reads(&seen, LATE, (b"<g>\n", &[LATE, LINK]))?;
  let seen = agreed_over(Head::LinksTarget, &[append(TARGET), unlink(TARGET)])?;
  reads(&seen, LATE, (b"<g>\n<+w>\n", &[LATE, LINK]))
}

/// What the works' links met, counted over the works the oracle checked: the census that shows each case ran.
#[derive(Clone, Copy, Debug, Default)]
struct Census {
  /// A link to a node, then a rename over the name it was linked by (`ln g l; mv f g`).
  replaced: u64,
  /// A node renamed, then a link to it (`mv f m; ln m l`).
  renamed_then_linked: u64,
  /// A link to a node, then a rename of it (`ln f l; mv f m`).
  linked_then_renamed: u64,
  /// A link to a node, then a removal of its file's name (`ln f l; rm f`).
  removed: u64,
  /// The base's link removed and made again (`rm h; ln f h`).
  made_again: u64,
  /// A link to a file the work made.
  to_made: u64,
  /// A write to a file another name names, then the file removed (`echo w >> g; rm g`; A-669).
  handed_removed: u64,
  /// A write to a file another name names, then another file renamed over it (`echo w >> g; mv f g`; A-669).
  handed_replaced: u64,
  /// A work's own count, never a total's: removals and replacements of the file the head linked `LATE` to (A-670).
  late_removed: u64,
  /// Works that removed or replaced the file the head linked `LATE` to, refused by the verdict (A-670): no link the
  /// work's base had to the file outlived it.
  raced_refused: u64,
  /// Works that removed or replaced the file the head linked `LATE` to, accepted (A-670): a base link outlived them.
  raced_kept: u64,
  /// Works whose four readings agreed (shown, not required).
  agreed: u64,
  /// Works the deriver refused: compositions owed, typed (shown, not required).
  refused: u64,
  /// Works the verdict refused, rebased and submitted alike (shown, not required).
  conflicts: u64,
}

impl Census {
  /// Counts what the oracle found of one work: its own census when it agreed, its removals of the file the head linked
  /// late either way.
  fn count(&mut self, found: &Restated, work: &Census) {
    let raced = u64::from(work.late_removed > 0);
    match found {
      Restated::Agreed(_) => {
        self.agreed += 1;
        self.replaced += work.replaced;
        self.renamed_then_linked += work.renamed_then_linked;
        self.linked_then_renamed += work.linked_then_renamed;
        self.removed += work.removed;
        self.made_again += work.made_again;
        self.to_made += work.to_made;
        self.handed_removed += work.handed_removed;
        self.handed_replaced += work.handed_replaced;
        self.raced_kept += raced;
      }
      Restated::Refused => self.refused += 1,
      Restated::Conflict => {
        self.conflicts += 1;
        self.raced_refused += raced;
      }
    }
  }

  /// Whether every case was met.
  fn met(&self) -> bool {
    [
      self.replaced,
      self.renamed_then_linked,
      self.linked_then_renamed,
      self.removed,
      self.made_again,
      self.to_made,
      self.handed_removed,
      self.handed_replaced,
      self.raced_refused,
      self.raced_kept,
    ]
    .iter()
    .all(|&count| count > 0)
  }
}

/// Shape: a generated step, a selector (weighted in `Draft::take`) and two choices among the names the step may use.
type Raw = (u8, u8, u8);

/// The name `choice` picks among `names`.
fn pick(names: &[&'static str], choice: u8) -> Option<&'static str> {
  let count = names.len().max(1);
  names.get(usize::from(choice) % count).copied()
}

/// A work being generated: its own volume over the base, its steps, and what each name has held, so the work keeps to
/// what the deriver composes as a volume with real inodes replays it (the module docs say what that leaves out).
struct Draft {
  volume: Inodes,
  steps: Vec<Step>,
  /// Every name that has held something, at the base or in the work.
  used: BTreeSet<String>,
  /// The names holding a file the deriver tracks as one: a base file, one the work made, one renamed there.
  files: BTreeSet<String>,
  /// The names holding a hard link: the base's, or one the work made.
  links: BTreeSet<String>,
  /// Every name the work made a hard link at.
  made_links: BTreeSet<String>,
  /// The names free now whose last occupant was a hard link, where a link may be made again.
  link_freed: BTreeSet<String>,
  /// The nodes the work made.
  created: BTreeSet<usize>,
  /// The nodes the work appended to.
  edited: BTreeSet<usize>,
  /// The nodes the work linked to, and those it renamed, for the census.
  linked: BTreeSet<usize>,
  moved: BTreeSet<usize>,
  /// The node of the file the head links `LATE` to, if it links one (A-670), for the census.
  late: Option<usize>,
  census: Census,
}

impl Draft {
  /// An empty work over the fixture's base, rebased over `head`.
  fn new(head: Head) -> Draft {
    let owned = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    let volume = Inodes::of(&base_green());
    let late = head
      .linked()
      .and_then(|file| volume.names.get(file).copied());
    Draft {
      volume,
      steps: Vec::new(),
      used: owned(&[FILE, TARGET, LINK]),
      files: owned(&[FILE, TARGET]),
      links: owned(&[LINK]),
      made_links: BTreeSet::new(),
      link_freed: BTreeSet::new(),
      created: BTreeSet::new(),
      edited: BTreeSet::new(),
      linked: BTreeSet::new(),
      moved: BTreeSet::new(),
      late,
      census: Census::default(),
    }
  }

  /// The work `raws` generate over the fixture's base, rebased over `head`.
  fn of(head: Head, raws: &[Raw]) -> Result<Draft, String> {
    let mut draft = Draft::new(head);
    for raw in raws {
      draft.take(*raw)?;
    }
    Ok(draft)
  }

  /// The work of `steps` over the fixture's base, rebased over `head`, counted as a generated one is.
  fn of_steps(head: Head, steps: &[Step]) -> Result<Draft, String> {
    let mut draft = Draft::new(head);
    for step in steps {
      draft.push(step.clone())?;
    }
    Ok(draft)
  }

  /// The names of `NAMES` that `keep` keeps.
  fn names(&self, keep: impl Fn(&str) -> bool) -> Vec<&'static str> {
    NAMES.into_iter().filter(|name| keep(name)).collect()
  }

  /// Whether `name` holds something in the work's volume.
  fn holds(&self, name: &str) -> bool {
    self.volume.names.contains_key(name)
  }

  /// The step `raw` names, when the work has names it may use: counted, tracked, applied and kept.
  fn take(&mut self, (selector, first, second): Raw) -> Result<(), String> {
    let index = self.steps.len();
    let step = match selector % 10 {
      0 => self.create(first, index),
      1 => self.append(first, index),
      2 | 3 => self.unlink(first),
      4..=6 => self.rename(first, second),
      _ => self.link(first, second),
    };
    match step {
      Some(step) => self.push(step),
      None => Ok(()),
    }
  }

  /// `step`, counted, tracked, applied and kept.
  fn push(&mut self, step: Step) -> Result<(), String> {
    self.meet(&step);
    self.track(&step);
    step.apply(&mut self.volume)?;
    self.steps.push(step);
    Ok(())
  }

  /// A new file at a name never used.
  fn create(&self, choice: u8, index: usize) -> Option<Step> {
    let path = pick(&self.names(|name| !self.used.contains(name)), choice)?;
    let bytes = format!("<{path} {index}>\n").into_bytes();
    Some(Step::Create(path.to_owned(), bytes))
  }

  /// Bytes appended to a file (a write through a hard link is a composition owed, `ContentOnMissing`).
  fn append(&self, choice: u8, index: usize) -> Option<Step> {
    let path = pick(&self.names(|name| self.files.contains(name)), choice)?;
    Some(Step::Append(
      path.to_owned(),
      format!("<+{index}>\n").into_bytes(),
    ))
  }

  /// A name that holds something removed.
  fn unlink(&self, choice: u8) -> Option<Step> {
    let path = pick(&self.names(|name| self.holds(name)), choice)?;
    Some(Step::Unlink(path.to_owned()))
  }

  /// A file renamed (a link's rename is a composition owed, `RenameMissingSource`).
  fn rename(&self, first: u8, second: u8) -> Option<Step> {
    let from = pick(&self.names(|name| self.files.contains(name)), first)?;
    let to = pick(&self.names(|name| self.may_rename(from, name)), second)?;
    Some(Step::Rename(from.to_owned(), to.to_owned()))
  }

  /// A hard link made to what a name holds.
  fn link(&self, first: u8, second: u8) -> Option<Step> {
    let path = pick(&self.names(|name| self.may_link_at(name)), first)?;
    let target = pick(&self.names(|name| self.holds(name)), second)?;
    Some(Step::Link(path.to_owned(), target.to_owned()))
  }

  /// Whether the file at `from` may be renamed onto `to` as the deriver composes a volume's rename: onto a name never
  /// used, or onto a file or the base's link from a node the work did not make; never onto a link the work made. (A
  /// file the work made renamed over a name composes to an edit of that name's node in place, filed open; the deriver
  /// refuses a rename onto a link the work made, `PathKindConflict`, so such works would only be counted refused.)
  fn may_rename(&self, from: &str, to: &str) -> bool {
    if to == from || self.made_links.contains(to) {
      return false;
    }
    if !self.holds(to) {
      return !self.used.contains(to);
    }
    let made = self
      .volume
      .names
      .get(from)
      .is_some_and(|node| self.created.contains(node));
    !made && (self.files.contains(to) || self.links.contains(to))
  }

  /// Whether a hard link may be made at `name`: free, and never used or last a link's (a link at a name a file left
  /// is refused by the deriver, `UnlinkMissing`).
  fn may_link_at(&self, name: &str) -> bool {
    !self.holds(name) && (!self.used.contains(name) || self.link_freed.contains(name))
  }

  /// Whether `name` holds the file of the node the head links `LATE` to (A-670).
  fn holds_late(&self, name: &str) -> bool {
    self.late.is_some()
      && self.files.contains(name)
      && self.volume.names.get(name).copied() == self.late
  }

  /// Whether removing or replacing what `name` holds hands a node the work wrote to on to another name (A-669): `name`
  /// holds the node's file, the work appended to the node, and a hard link names it too. Counted, for the census.
  fn hands_on_a_write(&self, name: &str) -> bool {
    let Some(&node) = self.volume.names.get(name) else {
      return false;
    };
    let named_elsewhere = self
      .volume
      .names
      .iter()
      .any(|(other, &named)| other != name && named == node);
    self.files.contains(name) && self.edited.contains(&node) && named_elsewhere
  }

  /// What `step` makes of each name's kind, as the deriver reads them; before it applies.
  fn track(&mut self, step: &Step) {
    match step {
      Step::Create(path, _) => {
        self.created.insert(self.volume.nodes.len());
        self.used.insert(path.clone());
        self.files.insert(path.clone());
      }
      Step::Append(path, _) => self.edited.extend(self.volume.names.get(path).copied()),
      Step::Unlink(path) => {
        self.files.remove(path);
        if self.links.remove(path) {
          self.link_freed.insert(path.clone());
        }
      }
      Step::Rename(from, to) => self.track_rename(from, to),
      Step::Link(path, _) => {
        self.used.insert(path.clone());
        self.links.insert(path.clone());
        self.made_links.insert(path.clone());
        self.link_freed.remove(path);
      }
    }
  }

  /// A rename's kinds: the file moves to `to`, replacing what it held; nothing changes when both name one node.
  fn track_rename(&mut self, from: &str, to: &str) {
    if self.volume.names.get(from) == self.volume.names.get(to) {
      return;
    }
    self.files.remove(from);
    self.links.remove(to);
    self.files.insert(to.to_owned());
    self.used.insert(to.to_owned());
  }

  /// Counts what `step` meets of the work's links; before it applies.
  fn meet(&mut self, step: &Step) {
    let node = |name: &str| self.volume.names.get(name).copied();
    match step {
      Step::Link(path, target) => {
        let target = node(target);
        let again = path == LINK && self.link_freed.contains(LINK);
        self.meet_link(target, again);
      }
      Step::Rename(from, to) => {
        let handed = self.hands_on_a_write(to);
        let late = self.holds_late(to) && node(from) != node(to);
        let (from, to) = (node(from), node(to));
        self.meet_rename(from, to);
        self.census.handed_replaced += u64::from(handed);
        self.census.late_removed += u64::from(late);
      }
      Step::Unlink(path) => {
        let removed =
          self.files.contains(path) && node(path).is_some_and(|n| self.linked.contains(&n));
        let handed = self.hands_on_a_write(path);
        let late = self.holds_late(path);
        self.census.removed += u64::from(removed);
        self.census.handed_removed += u64::from(handed);
        self.census.late_removed += u64::from(late);
      }
      Step::Create(..) | Step::Append(..) => {}
    }
  }

  /// A link to `target`'s node; `again` where the base's link stood.
  fn meet_link(&mut self, target: Option<usize>, again: bool) {
    let moved = target.is_some_and(|n| self.moved.contains(&n));
    let made = target.is_some_and(|n| self.created.contains(&n));
    self.census.renamed_then_linked += u64::from(moved);
    self.census.to_made += u64::from(made);
    self.census.made_again += u64::from(again);
    self.linked.extend(target);
  }

  /// A rename of `from`'s node over `to`'s.
  fn meet_rename(&mut self, from: Option<usize>, to: Option<usize>) {
    let renamed = from.is_some_and(|n| self.linked.contains(&n));
    let replaced = to.is_some_and(|n| Some(n) != from && self.linked.contains(&n));
    self.census.linked_then_renamed += u64::from(renamed);
    self.census.replaced += u64::from(replaced);
    self.moved.extend(from);
  }
}

/// One history meeting each case the census counts, so every count is met on every run whatever the generator's seed
/// (A-670): the generated works add to the counts, and none can fall to zero by luck, nor be met by no history at all.
/// Until A-670 the census rested on the generated works alone, and a case met four times in one run of 256 could have
/// been met none in the next.
fn fixed_histories() -> Vec<(Head, Vec<Step>)> {
  let made = Step::Create("m".to_owned(), b"<m>\n".to_vec());
  vec![
    (Head::Plain, vec![link("l", TARGET), rename(FILE, TARGET)]),
    (Head::Plain, vec![rename(FILE, "m"), link("l", "m")]),
    (Head::Plain, vec![link("l", FILE), rename(FILE, "m")]),
    (Head::Plain, vec![link("l", FILE), unlink(FILE)]),
    (Head::Plain, vec![unlink(LINK), link(LINK, FILE)]),
    (Head::Plain, vec![made, link("l", "m")]),
    (Head::Plain, vec![append(TARGET), unlink(TARGET)]),
    (Head::Plain, vec![append(TARGET), rename(FILE, TARGET)]),
    (Head::LinksFile, vec![append(FILE), unlink(FILE)]),
    (Head::LinksTarget, vec![unlink(TARGET)]),
  ]
}

/// The census of [`fixed_histories`], each checked by the oracle as a generated work is.
fn fixed_census() -> Result<Census, String> {
  let mut census = Census::default();
  for (head, steps) in fixed_histories() {
    let draft = Draft::of_steps(head, &steps)?;
    census.count(&restate(head, &draft.steps)?, &draft.census);
  }
  Ok(census)
}

/// A-663's oracle, A-669's and A-670's. Do: check one fixed history for each case the census counts, then generate
/// works of up to eleven steps over the fixture's names (files made, appended to, removed and renamed, over files and
/// over the base's link; hard links made to files and to links, before and after their nodes are renamed, renamed over
/// or removed; the base's link removed and made again; files written to and then removed or renamed over while a link
/// names them), each based at version 0 and rebased past a head whose version 1 may also link `LATE` to a file. Expect
/// each work the deriver composes to rebase as it submits, and, when clean, the four readings to agree: its own
/// operations in their own order over the head, the green that accepts it, its restated journal replayed over the head,
/// and the green that accepts that journal composed again; and the fixed histories to meet every case: a link before a
/// rename over its name, before and after its node's rename, before its node's removal, made again where the base's
/// stood, and to a file the work made; a write handed on to a link when its file is removed or another is renamed over
/// it; and a removal of the file the head linked late, refused and kept.
#[test]
fn every_restated_work_reads_as_its_own_order() -> Result<(), String> {
  let fixed = fixed_census()?;
  if !fixed.met() {
    return Err(format!(
      "a fixed history no longer meets its case: {fixed:?}"
    ));
  }
  let census = RefCell::new(fixed);
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let works = (any::<u8>(), prop::collection::vec(any::<Raw>(), 1..12));
  let result = runner.run(&works, |(choice, raws)| {
    let head = Head::of(choice);
    let draft = Draft::of(head, &raws).map_err(TestCaseError::fail)?;
    let found = restate(head, &draft.steps).map_err(TestCaseError::fail)?;
    census.borrow_mut().count(&found, &draft.census);
    Ok(())
  });
  result.map_err(|failure| failure.to_string())?;
  // The figures the evidence quotes (`-- --nocapture`).
  eprintln!("restate oracle census: {:?}", census.into_inner());
  Ok(())
}

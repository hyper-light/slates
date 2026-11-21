//! A push's changegroup taken into a fork of its repository's index a revision at a time (A-720; §4.17.3 "Push";
//! §4.17.1's rule for a push: its working set bounded whatever its size, its content stored once as it arrives): the
//! sliced intake a server advances within its shard's step budget, in place of verifying the whole changegroup first
//! ([`super::verify::verify`], every arrived text held at once) and keeping it after ([`super::commit::fork_with`]).
//!
//! **A step** takes one unit: one revision, in 7.2's order (the changelog's group, the manifest log's, then each file
//! log's), or one of the checks after them. A revision the log holds, or the push brought earlier, is passed over, as
//! 7.2's `addgroup` passes it over. Any other is checked as verification checks it (its flags, its parents known), its
//! text rebuilt from its delta against its base (the step [`rebuilt`]: the text before it in its group where its delta
//! names that one, as 7.2's revision cache serves it, else the base's text read back from the fork), its node held to
//! its text, its text read as its log's reader reads it (a changeset's manifest, a manifest's entries, a file
//! revision's metadata block; a manifest's and a file revision's link a changeset the fork holds), and then kept: its
//! text put into the content store once and its row appended to the fork ([`super::commit::keep_one`]). The fork holds
//! the repository's revisions and the push's, so a later revision's parents and base are found there. After the logs,
//! each new changeset's manifest is held to be present, then each new manifest's changed files, a manifest a step.
//!
//! **Bounded.** What a step holds is its revision's delta, the base's text and the rebuilt text, charged to a copy of
//! the push's admission, since it is let go when the step ends; only the text before it is kept to the next step. What
//! the intake keeps across steps is per revision and fixed in size (a node, a changeset's manifest, a manifest's first
//! parent), charged to the admission itself. The texts are kept in the content store, charged to its budget as every
//! store's are, never held by the intake. So its working set is one revision's texts, whatever the push's size. The
//! changegroup's framing is decoded once at the start, a record of fixed size per revision (`Changegroup::decode`).
//!
//! **What (A-720's later part) changes.** The rebuild is one step on purpose: storage that keeps a revision as it
//! arrived (a delta against its base) removes it from the keeping, and leaves it only to hold the node to its text.
//!
//! **Refused**, the caller gives the fork back ([`Intake::abandon`]): the repository's index and the content store are
//! as they were. The refusals are verification's ([`Unverified`]) and the keeping's ([`Unindexed`]).

use core::ops::Range;
use std::borrow::Cow;
use std::collections::BTreeSet;

use slates_vcs::{ContentStore, IndexRefused};

use super::classify::ReadChangeset;
use super::commit::{Unindexed, keep_one};
use super::held::IndexHeld;
use super::verify::{Arrived, Held, LogOf, Unverified, audit};
use crate::changegroup::{Revision, Version};
use crate::framing::{Collect, Framed, Framing, Halt, Source, Wanted};
use crate::index::{HgIndex, Log, NONE};
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, changeset, delta, filerev, manifest, node};

/// Why an intake was refused; its caller gives its fork back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
  /// Verification's refusal, as [`super::verify::verify`] refuses it.
  Unverified(Unverified),
  /// The keeping's refusal, as [`super::commit::fork_with`] refuses it.
  Unindexed(Unindexed),
}

impl From<Unverified> for Refused {
  fn from(refused: Unverified) -> Self {
    Refused::Unverified(refused)
  }
}

impl From<Unindexed> for Refused {
  fn from(refused: Unindexed) -> Self {
    Refused::Unindexed(refused)
  }
}

impl From<crate::PastAdmission> for Refused {
  fn from(past: crate::PastAdmission) -> Self {
    Refused::Unverified(Unverified::PastAdmission(past))
  }
}

/// What an intake brought, without its texts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Intaken {
  /// The new changesets, in the order they arrived.
  pub changesets: Vec<Node>,
  /// The changesets the repository held already, in the order they arrived: what 7.2's `changegroup.apply` calls
  /// `duprevs`.
  pub held_changesets: Vec<Node>,
  /// The revisions passed over: held already, or brought earlier in the same log.
  pub passed_over: u64,
  /// The revisions kept.
  pub kept: u64,
  /// The bytes of the texts kept.
  pub bytes: u64,
  /// Each log the push kept revisions in, in the order kept, and the number of its first: its kept revisions are the
  /// fork's from that number on, which its record is written from.
  pub logs: Vec<(LogOf, u32)>,
  /// Each revision kept, as the changeset log's `Texts` section names it (A-767 C4; w-perf's format, agreed): written as
  /// it is kept, since the intake has rebuilt and checked its text then and naming it later would rebuild every chain.
  pub texts: Vec<KeptText>,
  /// The whole texts the intake rebuilt from the fork's chains (main's ruling of 2026-11-21 05:12 (c): a push's work
  /// and memory are to grow with what it brings, not with the history under it): a delta base it did not hold as the
  /// text kept last. A new manifest's files are checked from the lines its delta wrote, rebuilding nothing.
  pub rebuilt: u64,
}

/// Format: the path a `Texts` entry gives the changelog's revisions: no tracked path holds a NUL (7.2's
/// `checkfilename`), so it meets no file's (agreed with w-perf, 2026-11-20).
pub const CHANGELOG_PATH: &[u8] = b"\0changelog";
/// Format: the path a `Texts` entry gives the manifest log's revisions (see [`CHANGELOG_PATH`]).
pub const MANIFEST_PATH: &[u8] = b"\0manifest";

/// One revision the intake kept, as the changeset log's `Texts` section names it (A-767 C4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptText {
  /// Its file's path, or [`CHANGELOG_PATH`] or [`MANIFEST_PATH`].
  pub path: Vec<u8>,
  /// Its node: Mercurial's tool id.
  pub node: Node,
  /// Its whole text's identity: the plain BLAKE3 of its bytes.
  pub identity: [u8; 32],
  /// Its whole text's length.
  pub len: u64,
  /// The identity of the text its payload delta rests on; none for a delta against the null base (a whole text).
  pub base: Option<[u8; 32]>,
  /// Where its delta lies in the payload (the changegroup as received).
  pub place: Range<u64>,
}

impl Intaken {
  /// What the kept texts were charged to the intake's admission, each as it was kept: the `Texts` section the landing
  /// writes, held as the record is, O(revisions), apart from a step's working set.
  pub fn texts_charged(&self) -> u64 {
    let each = to_u64(core::mem::size_of::<KeptText>());
    self.texts.iter().fold(0, |total, kept| {
      total
        .saturating_add(each)
        .saturating_add(to_u64(kept.path.len()))
    })
  }
}

/// Whether there is more to take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stepped {
  /// More units follow.
  More,
  /// Every unit is taken: [`Intake::finish`] gives the fork.
  Done,
  /// The step needs these bytes of the changegroup, which its source does not hold (`crate::framing`): nothing was
  /// taken; put them in and step again. A whole changegroup in memory never wants.
  Wanted(Range<u64>),
}

/// The unit a step takes next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
  /// The changelog's revision at this position.
  Changelog(usize),
  /// The manifest log's revision at this position.
  Manifests(usize),
  /// The revision at `at` of the file group at `file`.
  Files {
    /// The file group.
    file: usize,
    /// The revision in it.
    at: usize,
  },
  /// The new changeset at this position: its manifest present.
  ManifestsPresent(usize),
  /// The new manifest at this position: its changed files present.
  FilesPresent(usize),
  /// Everything taken.
  Done,
}

/// The changegroup's framing as places in its bytes (`crate::framing::Framed`), so the intake borrows nothing between
/// steps.
#[derive(Clone, Debug, Default)]
struct Frame(Framed);

impl Frame {
  /// The framing of `payload`, a whole changegroup of `version` in memory, each plain delta checked, within
  /// `admission`.
  fn of(payload: &[u8], version: Version, admission: &mut Admission) -> Result<Frame, Refused> {
    let mut framed = Framed::default();
    let mut framing = Framing::new(version, true);
    loop {
      let mut sink = Collect {
        framed: &mut framed,
        admission: &mut *admission,
      };
      match framing.step(payload, &mut sink) {
        Ok(true) => {}
        Ok(false) => break,
        Err(Halt::Refused(refusal)) => return Err(Unverified::Changegroup(refusal).into()),
        // A slice holds every byte of itself: a want names bytes past its end, a truncation.
        Err(Halt::Wanted(wanted)) => {
          return Err(
            Unverified::Changegroup(crate::changegroup::Refusal::Truncated(crate::Truncated {
              what: "changegroup",
              at: wanted.range.start,
              needed: wanted.range.end.saturating_sub(wanted.range.start),
              left: to_u64(payload.len()).saturating_sub(wanted.range.start),
            }))
            .into(),
          );
        }
      }
    }
    Frame::framed(framed)
  }

  /// A framing its caller walked (`crate::framing::Framing` over a source read a piece at a time): a changegroup of
  /// tree manifests refused, as verification refuses one.
  fn framed(framed: Framed) -> Result<Frame, Refused> {
    if !framed.trees.is_empty() {
      return Err(Unverified::TreeManifests.into());
    }
    Ok(Frame(framed))
  }

  /// The revision at `at` of `range`, its delta read from `payload`, with where its delta lies there (A-767 C4: the
  /// `Texts` section's place); none past the group, the bytes `payload` lacks where it holds them not.
  fn placed<'p, S: Source + ?Sized>(
    &self,
    payload: &'p S,
    range: &Range<usize>,
    at: usize,
  ) -> Result<Option<(Revision<'p>, Range<u64>)>, Wanted> {
    let Some(found) = range
      .start
      .checked_add(at)
      .filter(|index| *index < range.end)
      .and_then(|index| self.0.revisions.get(index))
    else {
      return Ok(None);
    };
    Ok(Some((found.revision(payload)?, found.data.clone())))
  }
}

/// A push's changegroup being taken into a fork of its repository's index (module docs): the changegroup's framing, as
/// places in its bytes, which the caller keeps and lends each step (a push's driver, from the bundle; a replay, from
/// the record), and what taking it has made so far.
pub struct Intake {
  /// The changegroup's framing.
  frame: Frame,
  /// What taking it has made so far.
  taker: Taker,
}

/// A new manifest and the entries its delta wrote: each file's path and file revision.
type ManifestWrites = (Node, Vec<(Vec<u8>, Node)>);

/// What an intake has made so far: its fork, where it stands, and what it keeps across steps.
struct Taker {
  /// The fork: the repository's revisions, and the push's as they are kept.
  fork: HgIndex,
  /// The unit next.
  stage: Stage,
  /// The nodes the push brought in the log being taken, to tell a revision brought earlier from one held.
  brought: BTreeSet<Node>,
  /// Each new changeset and the manifest its text names.
  manifest_of: Vec<(Node, Node)>,
  /// Each new manifest and the entries its delta wrote (each file's path and file revision), which its files' check
  /// reads ([`Taker::files_present`]): what the push changed, never a whole manifest.
  manifests: Vec<ManifestWrites>,
  /// The text kept last, and its node: the next revision's base where its delta names it.
  last: Option<(Node, Vec<u8>)>,
  /// The identity of the text kept last, so the next revision's base is named without hashing it again.
  last_identity: Option<(Node, [u8; 32])>,
  /// Whether the log being taken has kept a revision yet.
  logged: bool,
  /// The path of the file group being taken, read at its opening, so each later step reads one range of the
  /// changegroup (its revision's delta) and a source read forward never wants bytes it let go.
  path: Option<Vec<u8>>,
  /// What was brought.
  intaken: Intaken,
}

impl Intake {
  /// Starts taking a changegroup of `version`, `payload` its bytes, into a fork of `index` (module docs), its framing
  /// decoded within `admission`. Every step is lent the same bytes.
  pub fn begin(
    (version, payload): (Version, &[u8]),
    index: &mut HgIndex,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Self, Refused> {
    let frame = Frame::of(payload, version, admission)?;
    Intake::with_frame(frame, index, store)
  }

  /// Starts taking a changegroup its caller framed (`crate::framing::Framing`, over a source read a piece at a time,
  /// each revision charged as it was placed) into a fork of `index`; every step is lent the same changegroup's bytes,
  /// as far as its source holds them ([`Stepped::Wanted`]).
  pub fn begin_framed(
    framed: Framed,
    index: &mut HgIndex,
    store: &mut ContentStore,
  ) -> Result<Self, Refused> {
    Intake::with_frame(Frame::framed(framed)?, index, store)
  }

  /// [`Intake::begin`]'s fork made over `frame`.
  fn with_frame(
    frame: Frame,
    index: &mut HgIndex,
    store: &mut ContentStore,
  ) -> Result<Self, Refused> {
    let fork = index.fork(store).map_err(Unindexed::Fork)?;
    Ok(Intake {
      frame,
      taker: Taker {
        fork,
        stage: Stage::Changelog(0),
        brought: BTreeSet::new(),
        manifest_of: Vec::new(),
        manifests: Vec::new(),
        last: None,
        last_identity: None,
        logged: false,
        path: None,
        intaken: Intaken::default(),
      },
    })
  }

  /// Takes the next unit of the changegroup whose bytes are `payload`, the ones it began with (module docs). Refused,
  /// the caller gives the fork back ([`Intake::abandon`]).
  pub fn step<S: Source + ?Sized>(
    &mut self,
    payload: &S,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let Intake { frame, taker } = self;
    match taker.stage {
      Stage::Changelog(at) => {
        let next = match frame.placed(payload, &frame.0.changelog, at) {
          Ok(next) => next,
          Err(wanted) => return Ok(Stepped::Wanted(wanted.range)),
        };
        taker.take_or(
          next,
          Log::Changelog,
          (store, admission),
          Stage::Manifests(0),
        )
      }
      Stage::Manifests(at) => {
        let next = match frame.placed(payload, &frame.0.manifests, at) {
          Ok(next) => next,
          Err(wanted) => return Ok(Stepped::Wanted(wanted.range)),
        };
        let after = Stage::Files { file: 0, at: 0 };
        taker.take_or(next, Log::Manifest, (store, admission), after)
      }
      Stage::Files { file, at } => taker.file_step((frame, payload), (file, at), store, admission),
      Stage::ManifestsPresent(at) => taker.manifest_present(at, store),
      Stage::FilesPresent(at) => taker.files_present(at, store),
      Stage::Done => Ok(Stepped::Done),
    }
  }

  /// The fork and what was brought, once every unit is taken; the intake itself (boxed: it is large) while there is
  /// more.
  pub fn finish(self) -> Result<(HgIndex, Intaken), Box<Self>> {
    if self.taker.stage == Stage::Done {
      Ok((self.taker.fork, self.taker.intaken))
    } else {
      Err(Box::new(self))
    }
  }

  /// Gives the fork back: the repository's index and the content store as they were before the intake began.
  pub fn abandon(self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    self.taker.fork.release(store)
  }
}

impl Taker {
  /// Takes `next` of `log`, or moves to `after` where its group has no more.
  fn take_or(
    &mut self,
    next: Option<(Revision<'_>, Range<u64>)>,
    log: Log<'_>,
    (store, admission): (&mut ContentStore, &mut Admission),
    after: Stage,
  ) -> Result<Stepped, Refused> {
    let Some((revision, place)) = next else {
      self.enter(after);
      return Ok(Stepped::More);
    };
    self.take(log, (&revision, place), store, admission)?;
    self.stage = match self.stage {
      Stage::Changelog(at) => Stage::Changelog(at.saturating_add(1)),
      Stage::Manifests(at) => Stage::Manifests(at.saturating_add(1)),
      other => other,
    };
    Ok(Stepped::More)
  }

  /// One step of the file groups: a group opened (its path read, audited and kept, charged to `admission`), then its
  /// revisions a step each; after the last group, the checks.
  fn file_step<S: Source + ?Sized>(
    &mut self,
    (frame, payload): (&Frame, &S),
    (file, at): (usize, usize),
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let Some((name, revisions)) = frame.0.files.get(file).cloned() else {
      self.enter(Stage::ManifestsPresent(0));
      return Ok(Stepped::More);
    };
    let Some(path) = self.path.take() else {
      let path = match payload.bytes(name) {
        Ok(path) => path,
        Err(wanted) => return Ok(Stepped::Wanted(wanted.range)),
      };
      audit(path)?;
      admission.charge(to_u64(path.len()))?;
      self.path = Some(path.to_vec());
      return Ok(Stepped::More);
    };
    let next = match frame.placed(payload, &revisions, at) {
      Ok(next) => next,
      Err(wanted) => {
        self.path = Some(path);
        return Ok(Stepped::Wanted(wanted.range));
      }
    };
    let Some((revision, place)) = next else {
      self.enter(Stage::Files {
        file: file.saturating_add(1),
        at: 0,
      });
      return Ok(Stepped::More);
    };
    let taken = self.take(Log::File(&path), (&revision, place), store, admission);
    self.path = Some(path);
    taken?;
    self.stage = Stage::Files {
      file,
      at: at.saturating_add(1),
    };
    Ok(Stepped::More)
  }

  /// Moves to `stage`, a new log's: what the last one brought is let go.
  fn enter(&mut self, stage: Stage) {
    self.path = None;
    self.brought.clear();
    self.last = None;
    self.last_identity = None;
    self.logged = false;
    self.stage = stage;
  }

  /// Takes one revision of `log` (module docs): passed over, or checked, rebuilt and kept.
  fn take(
    &mut self,
    log: Log<'_>,
    (revision, place): (&Revision<'_>, Range<u64>),
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<(), Refused> {
    if self.brought.contains(&revision.node) {
      self.intaken.passed_over = self.intaken.passed_over.saturating_add(1);
      return Ok(());
    }
    if self.fork.rev(store, log, &revision.node).is_some() {
      self.intaken.passed_over = self.intaken.passed_over.saturating_add(1);
      if log == Log::Changelog {
        admission.charge_items::<Node>(1)?;
        self.intaken.held_changesets.push(revision.node);
      }
      return Ok(());
    }
    self.check_known(log, revision, store)?;
    let mut room = *admission;
    let base_text = self.base_text(log, revision, store);
    let base_rebuilt = matches!(base_text, Some(Cow::Owned(_)));
    let base = match (&base_text, &self.last_identity) {
      _ if revision.base == NULL_NODE => None,
      (_, Some((node, identity))) if *node == revision.base => Some(*identity),
      (Some(text), _) => Some(*blake3::hash(text).as_bytes()),
      (None, _) => None,
    };
    let text = rebuilt(base_text, revision, log, &mut room)?;
    if base_rebuilt {
      self.intaken.rebuilt = self.intaken.rebuilt.saturating_add(1);
    }
    node::check(&revision.node, &revision.p1, &revision.p2, &text).map_err(|fault| {
      Unverified::Node {
        log: log_of(log),
        node: revision.node,
        fault,
      }
    })?;
    self.read_as_its_log(log, revision, &text, store, admission)?;
    let arrived = Arrived {
      node: revision.node,
      p1: revision.p1,
      p2: revision.p2,
      link: revision.link,
      text,
    };
    if !self.logged {
      let named = log_of(log);
      admission.charge_items::<(LogOf, u32)>(1)?;
      if let LogOf::File(path) = &named {
        admission.charge(to_u64(path.len()))?;
      }
      self.intaken.logs.push((named, self.fork.len(store, log)));
      self.logged = true;
    }
    let previous = self
      .last
      .as_ref()
      .map(|(node, text)| (node, text.as_slice()));
    keep_one(&mut self.fork, store, log, (&arrived, previous), &mut room)?;
    admission.charge_items::<Node>(1)?;
    self.brought.insert(revision.node);
    self.intaken.kept = self.intaken.kept.saturating_add(1);
    self.intaken.bytes = self
      .intaken
      .bytes
      .saturating_add(to_u64(arrived.text.len()));
    admission.charge_items::<KeptText>(1)?;
    let path = match log {
      Log::Changelog => CHANGELOG_PATH,
      Log::Manifest => MANIFEST_PATH,
      Log::File(path) => path,
    };
    admission.charge(to_u64(path.len()))?;
    let identity = *blake3::hash(&arrived.text).as_bytes();
    self.intaken.texts.push(KeptText {
      path: path.to_vec(),
      node: revision.node,
      identity,
      len: to_u64(arrived.text.len()),
      base,
      place,
    });
    self.last_identity = Some((revision.node, identity));
    self.last = Some((revision.node, arrived.text));
    Ok(())
  }

  /// Refuses a revision with a flag, or one whose parent the fork does not hold.
  fn check_known(
    &self,
    log: Log<'_>,
    revision: &Revision<'_>,
    store: &ContentStore,
  ) -> Result<(), Unverified> {
    if revision.flags != 0 {
      return Err(Unverified::Flags {
        log: log_of(log),
        node: revision.node,
        flags: revision.flags,
      });
    }
    let unknown = [revision.p1, revision.p2]
      .into_iter()
      .find(|parent| *parent != NULL_NODE && self.fork.rev(store, log, parent).is_none());
    match unknown {
      Some(parent) => Err(Unverified::UnknownParent {
        log: log_of(log),
        node: revision.node,
        parent,
      }),
      None => Ok(()),
    }
  }

  /// The text of `revision`'s delta base: empty for the null node, the text kept last where it is that one, else the
  /// fork's; `None` where the fork does not hold it.
  fn base_text<'s>(
    &'s self,
    log: Log<'_>,
    revision: &Revision<'_>,
    store: &'s ContentStore,
  ) -> Option<Cow<'s, [u8]>> {
    if revision.base == NULL_NODE {
      return Some(Cow::Borrowed(&[]));
    }
    if let Some((node, text)) = &self.last
      && *node == revision.base
    {
      return Some(Cow::Borrowed(text.as_slice()));
    }
    let held = IndexHeld::new(&self.fork, store);
    let text = match log {
      Log::Changelog => held.changeset_text(&revision.base),
      Log::Manifest => held.manifest_text(&revision.base),
      Log::File(path) => held.file_text(path, &revision.base),
    }?;
    Some(Cow::Owned(text.into_owned()))
  }

  /// Reads `text` as `log`'s reader reads it: a changeset's manifest noted; a manifest's or a file revision's link a
  /// changeset the fork holds, a manifest's entries read and its first parent noted, a file revision's block split.
  fn read_as_its_log(
    &mut self,
    log: Log<'_>,
    revision: &Revision<'_>,
    text: &[u8],
    store: &ContentStore,
    admission: &mut Admission,
  ) -> Result<(), Refused> {
    let node = revision.node;
    if log == Log::Changelog {
      let manifest = changeset::decode(text)
        .and_then(|read| read.manifest_node())
        .map_err(|fault| Unverified::Changeset { node, fault })?;
      admission.charge_items::<(Node, Node)>(2)?;
      self.manifest_of.push((node, manifest));
      self.intaken.changesets.push(node);
      return Ok(());
    }
    if self
      .fork
      .rev(store, Log::Changelog, &revision.link)
      .is_none()
    {
      return Err(
        Unverified::UnknownLink {
          log: log_of(log),
          node,
          link: revision.link,
        }
        .into(),
      );
    }
    match log {
      Log::File(path) => {
        filerev::split(text).map_err(|fault| Unverified::FileRevision {
          path: path.to_vec(),
          node,
          fault,
        })?;
      }
      _ => {
        manifest::decode(text).map_err(|fault| Unverified::Manifest { node, fault })?;
        let written = written_entries(text, revision).map_err(|fault| Unverified::Delta {
          log: log_of(log),
          node,
          fault,
        })?;
        admission.charge_items::<(Node, Vec<(Vec<u8>, Node)>)>(1)?;
        admission.charge_items::<(Vec<u8>, Node)>(written.len())?;
        admission.charge(written.iter().fold(0_u64, |bytes, (path, _)| {
          bytes.saturating_add(to_u64(path.len()))
        }))?;
        self.manifests.push((node, written));
      }
    }
    Ok(())
  }

  /// The check of the new changeset at `at`: its manifest the null node, or one the fork holds.
  fn manifest_present(&mut self, at: usize, store: &ContentStore) -> Result<Stepped, Refused> {
    let Some((changeset, manifest)) = self.manifest_of.get(at).copied() else {
      self.stage = Stage::FilesPresent(0);
      return Ok(Stepped::More);
    };
    if manifest != NULL_NODE && self.fork.rev(store, Log::Manifest, &manifest).is_none() {
      return Err(
        Unverified::MissingManifest {
          changeset,
          manifest,
        }
        .into(),
      );
    }
    self.stage = Stage::ManifestsPresent(at.saturating_add(1));
    Ok(Stepped::More)
  }

  /// The check of the new manifest at `at`: every entry its delta wrote audited and held by the fork's file log (main's
  /// ruling of 2026-11-21 05:12 (a)). An entry its delta left as its base had it was checked when that base was: held by
  /// the fork before the push, or checked earlier in it. Its text and its first parent's were rebuilt from their chains
  /// here before, two whole texts a manifest: 57% of hg-src's first push (w-perf's profile, base 94c1696bb).
  fn files_present(&mut self, at: usize, store: &ContentStore) -> Result<Stepped, Refused> {
    let Some((_, written)) = self.manifests.get(at) else {
      self.stage = Stage::Done;
      return Ok(Stepped::Done);
    };
    for (path, node) in written {
      audit(path)?;
      if self.fork.rev(store, Log::File(path), node).is_none() {
        return Err(
          Unverified::MissingFile {
            path: path.clone(),
            node: *node,
          }
          .into(),
        );
      }
    }
    self.stage = Stage::FilesPresent(at.saturating_add(1));
    Ok(Stepped::More)
  }
}

/// A push's fork re-based onto its repository's index as it is now (A-720 part 2b), when another push landed after the
/// fork was made: the fork's rows name their parents and links by number, so they hold only over the index they were
/// forked from. Each revision the push kept is appended again, a row a step, to a fork of the current index, from its
/// row in the stale fork (its node, parents and link by node, its flags, its length and the content it names: nothing
/// rebuilt, put or hashed again); one the index holds already (the other push brought it) is passed over, a changeset
/// among them counted held. Bookkeeping only: the push's verdict is still judged at its landing (D-27).
pub struct Rebase {
  /// The stale fork, read from.
  stale: HgIndex,
  /// The fork of the index as it is now, appended to.
  fork: HgIndex,
  /// The logs to re-append, each from its first kept number in the stale fork.
  logs: Vec<(LogOf, u32)>,
  /// The log and the stale number next.
  at: (usize, u32),
  /// Whether the log being re-appended has appended a row yet.
  logged: bool,
  /// What the push brings over the index as it is now.
  intaken: Intaken,
}

impl Rebase {
  /// Starts re-basing `stale` (what `intaken` names) onto a fork of `index`. Refused, `stale` comes back (boxed with the
  /// refusal: an index is large).
  pub fn begin(
    stale: HgIndex,
    intaken: &Intaken,
    index: &mut HgIndex,
    store: &mut ContentStore,
  ) -> Result<Self, Box<(Unindexed, HgIndex)>> {
    let fork = match index.fork(store) {
      Ok(fork) => fork,
      Err(refused) => return Err(Box::new((Unindexed::Fork(refused), stale))),
    };
    let logs = intaken.logs.clone();
    // The texts a push kept name its revisions' contents and their places in its payload, whatever index they are
    // re-based onto, so they carry over as they are.
    let intaken = Intaken {
      held_changesets: intaken.held_changesets.clone(),
      passed_over: intaken.passed_over,
      texts: intaken.texts.clone(),
      ..Intaken::default()
    };
    Ok(Rebase {
      stale,
      fork,
      logs,
      at: (0, 0),
      logged: false,
      intaken,
    })
  }

  /// Re-appends the next row (module docs); the caller gives both forks back on a refusal ([`Rebase::abandon`]).
  pub fn step(&mut self, store: &mut ContentStore) -> Result<Stepped, Unindexed> {
    let (at, rev) = self.at;
    let Some((named, first)) = self.logs.get(at).cloned() else {
      return Ok(Stepped::Done);
    };
    let log = log_named(&named);
    let rev = rev.max(first);
    if rev >= self.stale.len(store, log) {
      self.at = (at.saturating_add(1), 0);
      self.logged = false;
      return Ok(Stepped::More);
    }
    self.again(log, &named, rev, store)?;
    self.at = (at, rev.saturating_add(1));
    Ok(Stepped::More)
  }

  /// The fork of the index as it is now, holding the push, and what it brings over that index; the stale fork given
  /// back.
  pub fn finish(self, store: &mut ContentStore) -> Result<(HgIndex, Intaken), IndexRefused> {
    let Rebase {
      stale,
      fork,
      intaken,
      ..
    } = self;
    match stale.release(store) {
      Ok(()) => Ok((fork, intaken)),
      Err(refused) => Err(fork.release(store).err().unwrap_or(refused)),
    }
  }

  /// Gives both forks back.
  pub fn abandon(self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    let stale = self.stale.release(store);
    let fork = self.fork.release(store);
    stale.and(fork)
  }

  /// Appends the stale fork's revision `rev` of `log` to the fork, by node; passed over where the fork holds it.
  fn again(
    &mut self,
    log: Log<'_>,
    named: &LogOf,
    rev: u32,
    store: &mut ContentStore,
  ) -> Result<(), Unindexed> {
    let kept = crate::import::record::kept(&self.stale, store, log, rev).ok_or(
      Unindexed::Index(crate::index::Refused::MissingLink { node: NULL_NODE }),
    )?;
    let node = kept.node;
    if self.fork.rev(store, log, &node).is_some() {
      self.intaken.passed_over = self.intaken.passed_over.saturating_add(1);
      if log == Log::Changelog {
        self.intaken.held_changesets.push(node);
      }
      return Ok(());
    }
    if !self.logged {
      self
        .intaken
        .logs
        .push((named.clone(), self.fork.len(store, log)));
      self.logged = true;
    }
    if log == Log::Changelog {
      let row = self.stale.changeset(store, rev).ok_or(Unindexed::Index(
        crate::index::Refused::MissingLink { node },
      ))?;
      let branch = row.branch.to_vec();
      let again = crate::index::NewChangeset {
        node,
        p1: kept.p1,
        p2: kept.p2,
        flags: kept.flags,
        len: kept.len,
        text: row.text,
        manifest: row.manifest,
        closed: row.closed,
        branch: &branch,
      };
      self.fork.append_changesets(store, &[again])?;
      self.intaken.changesets.push(node);
    } else {
      let row = self
        .stale
        .revision(store, log, rev)
        .ok_or(Unindexed::Index(crate::index::Refused::MissingLink {
          node,
        }))?;
      // A manifest kept as a delta against the stale fork's previous one (A-729) is appended again by its text, which
      // the fork keeps as its own delta against its previous one.
      let rebuilt = if row.base == crate::index::NONE {
        None
      } else {
        let text = self
          .stale
          .revision_text(store, log, rev)
          .ok_or(Unindexed::Index(crate::index::Refused::Unread { rev }))?;
        Some(store.put_object(&text)?)
      };
      let again = crate::index::NewRevision {
        node,
        p1: kept.p1,
        p2: kept.p2,
        link: kept.link.unwrap_or(NULL_NODE),
        flags: kept.flags,
        len: kept.len,
        bytes: rebuilt.unwrap_or(row.bytes),
        block: row.block,
      };
      let appended = self.fork.append_revisions(store, log, &[again]);
      if let Some(put) = rebuilt {
        store.release(&put)?;
      }
      appended?;
    }
    self.intaken.kept = self.intaken.kept.saturating_add(1);
    Ok(())
  }
}

/// The index's log `named` names.
fn log_named(named: &LogOf) -> Log<'_> {
  match named {
    LogOf::Changelog => Log::Changelog,
    LogOf::Manifests => Log::Manifest,
    LogOf::File(path) => Log::File(path),
  }
}

/// The changesets `intaken` brought as their rows in `fork` keep them (A-720 part 2): node, parents, branch and close,
/// for classifying the push without its texts; each charged to `admission`. `None` where the fork lacks one.
pub fn read_changesets(
  fork: &HgIndex,
  store: &ContentStore,
  intaken: &Intaken,
  admission: &mut Admission,
) -> Result<Option<Vec<ReadChangeset>>, crate::PastAdmission> {
  admission.charge_items::<ReadChangeset>(intaken.changesets.len())?;
  let mut read = Vec::with_capacity(intaken.changesets.len());
  let node_of = |number: u32| match number {
    NONE => Some(NULL_NODE),
    number => fork.changeset(store, number).map(|row| row.node),
  };
  for node in &intaken.changesets {
    let Some(row) = fork
      .rev(store, Log::Changelog, node)
      .and_then(|rev| fork.changeset(store, rev))
    else {
      return Ok(None);
    };
    admission.charge(to_u64(row.branch.len()))?;
    let (Some(p1), Some(p2)) = (node_of(row.p1), node_of(row.p2)) else {
      return Ok(None);
    };
    read.push(ReadChangeset {
      node: *node,
      p1,
      p2,
      branch: row.branch.to_vec(),
      closes: row.closed,
    });
  }
  Ok(Some(read))
}

/// The step that rebuilds a revision's text: its delta applied to its base's text (module docs: the step a later
/// storage removes from the keeping), charged to `room`.
fn rebuilt(
  base: Option<Cow<'_, [u8]>>,
  revision: &Revision<'_>,
  log: Log<'_>,
  room: &mut Admission,
) -> Result<Vec<u8>, Unverified> {
  let base = base.ok_or_else(|| Unverified::UnknownDeltaBase {
    log: log_of(log),
    node: revision.node,
    base: revision.base,
  })?;
  room.charge(to_u64(base.len()))?;
  delta::apply(&base, revision.data, room).map_err(|fault| Unverified::Delta {
    log: log_of(log),
    node: revision.node,
    fault,
  })
}

/// The entries of manifest `text` its delta wrote (main's ruling of 2026-11-21 05:12 (a)): the whole lines each hunk's
/// bytes fall on in the new text, every line where the revision came whole or against the null base. O(the lines the
/// delta touched), never the whole manifest: a delta that rewrote nothing in a line leaves it out, and a hunk cut
/// mid-line takes the whole line in. `text` is the delta applied to its base, so the hunks are known to apply.
fn written_entries(
  text: &[u8],
  revision: &Revision<'_>,
) -> Result<Vec<(Vec<u8>, Node)>, delta::Refusal> {
  let entries_of = |lines: &[u8]| -> Vec<(Vec<u8>, Node)> {
    lines
      .split(|byte| *byte == b'\n')
      .filter_map(manifest::parse)
      .map(|entry| (entry.path.to_vec(), entry.node))
      .collect()
  };
  if revision.is_full_text() || revision.base == NULL_NODE {
    return Ok(entries_of(text));
  }
  let line_start = |at: usize| {
    text
      .get(..at)
      .and_then(|before| before.iter().rposition(|byte| *byte == b'\n'))
      .map_or(0, |newline| newline.saturating_add(1))
  };
  let line_end = |at: usize| {
    text
      .get(at..)
      .and_then(|after| after.iter().position(|byte| *byte == b'\n'))
      .map_or(text.len(), |newline| {
        at.saturating_add(newline).saturating_add(1)
      })
  };
  let mut written = Vec::new();
  let (mut produced, mut base_at, mut covered) = (0_usize, 0_usize, 0_usize);
  for hunk in delta::hunks(revision.data) {
    let hunk = hunk?;
    let start = usize::try_from(hunk.start).unwrap_or(usize::MAX);
    let end = usize::try_from(hunk.end).unwrap_or(usize::MAX);
    let new_start = produced.saturating_add(start.saturating_sub(base_at));
    let new_end = new_start.saturating_add(hunk.data.len());
    produced = new_end;
    base_at = end;
    if new_start >= text.len() {
      continue;
    }
    let lo = line_start(new_start).max(covered);
    let hi = line_end(new_end.saturating_sub(1).max(new_start));
    if lo < hi {
      written.extend(entries_of(text.get(lo..hi).unwrap_or_default()));
      covered = hi;
    }
  }
  Ok(written)
}

/// Verification's name of `log`.
fn log_of(log: Log<'_>) -> LogOf {
  match log {
    Log::Changelog => LogOf::Changelog,
    Log::Manifest => LogOf::Manifests,
    Log::File(path) => LogOf::File(path.to_vec()),
  }
}

#[cfg(test)]
#[path = "intake_tests.rs"]
mod tests;

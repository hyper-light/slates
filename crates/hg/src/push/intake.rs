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

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use slates_vcs::{ContentStore, IndexRefused};

use super::ChangegroupPart;
use super::classify::ReadChangeset;
use super::commit::{Unindexed, keep_one};
use super::held::IndexHeld;
use super::verify::{Arrived, Held, LogOf, Unverified, audit};
use crate::changegroup::{Changegroup, Revision};
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
}

/// Whether there is more to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stepped {
  /// More units follow.
  More,
  /// Every unit is taken: [`Intake::finish`] gives the fork.
  Done,
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

/// A push's changegroup being taken into a fork of its repository's index (module docs).
pub struct Intake<'g> {
  /// The changegroup's framing.
  group: Changegroup<'g>,
  /// The fork: the repository's revisions, and the push's as they are kept.
  fork: HgIndex,
  /// The unit next.
  stage: Stage,
  /// The nodes the push brought in the log being taken, to tell a revision brought earlier from one held.
  brought: BTreeSet<Node>,
  /// Each new changeset and the manifest its text names.
  manifest_of: Vec<(Node, Node)>,
  /// Each new manifest and its first parent.
  manifests: Vec<(Node, Node)>,
  /// The text kept last, and its node: the next revision's base where its delta names it.
  last: Option<(Node, Vec<u8>)>,
  /// Whether the log being taken has kept a revision yet.
  logged: bool,
  /// What was brought.
  intaken: Intaken,
}

impl<'g> Intake<'g> {
  /// Starts taking `part`'s changegroup into a fork of `index` (module docs), its framing decoded within `admission`.
  pub fn begin(
    part: &'g ChangegroupPart,
    index: &mut HgIndex,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Self, Refused> {
    let group = Changegroup::decode(&part.payload, part.version, admission)
      .map_err(Unverified::Changegroup)?;
    if !group.trees.is_empty() {
      return Err(Unverified::TreeManifests.into());
    }
    let fork = index.fork(store).map_err(Unindexed::Fork)?;
    Ok(Intake {
      group,
      fork,
      stage: Stage::Changelog(0),
      brought: BTreeSet::new(),
      manifest_of: Vec::new(),
      manifests: Vec::new(),
      last: None,
      logged: false,
      intaken: Intaken::default(),
    })
  }

  /// Takes the next unit (module docs). Refused, the caller gives the fork back ([`Intake::abandon`]).
  pub fn step(
    &mut self,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    match self.stage {
      Stage::Changelog(at) => {
        let next = self.group.group(&self.group.changelog).get(at).copied();
        self.take_or(next, Log::Changelog, store, admission, Stage::Manifests(0))
      }
      Stage::Manifests(at) => {
        let next = self.group.group(&self.group.manifests).get(at).copied();
        let after = Stage::Files { file: 0, at: 0 };
        self.take_or(next, Log::Manifest, store, admission, after)
      }
      Stage::Files { file, at } => self.file_step(file, at, store, admission),
      Stage::ManifestsPresent(at) => self.manifest_present(at, store),
      Stage::FilesPresent(at) => self.files_present(at, store, admission),
      Stage::Done => Ok(Stepped::Done),
    }
  }

  /// The fork and what was brought, once every unit is taken; the intake itself while there is more.
  pub fn finish(self) -> Result<(HgIndex, Intaken), Self> {
    if self.stage == Stage::Done {
      Ok((self.fork, self.intaken))
    } else {
      Err(self)
    }
  }

  /// Gives the fork back: the repository's index and the content store as they were before the intake began.
  pub fn abandon(self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    self.fork.release(store)
  }

  /// Takes `next` of `log`, or moves to `after` where its group has no more.
  fn take_or(
    &mut self,
    next: Option<Revision<'g>>,
    log: Log<'_>,
    store: &mut ContentStore,
    admission: &mut Admission,
    after: Stage,
  ) -> Result<Stepped, Refused> {
    let Some(revision) = next else {
      self.enter(after);
      return Ok(Stepped::More);
    };
    self.take(log, &revision, store, admission)?;
    self.stage = match self.stage {
      Stage::Changelog(at) => Stage::Changelog(at.saturating_add(1)),
      Stage::Manifests(at) => Stage::Manifests(at.saturating_add(1)),
      other => other,
    };
    Ok(Stepped::More)
  }

  /// Moves to `stage`, a new log's: what the last one brought is let go.
  fn enter(&mut self, stage: Stage) {
    self.brought.clear();
    self.last = None;
    self.logged = false;
    self.stage = stage;
  }

  /// One step of the file groups: a group's path audited at its first revision, then its revisions; after the last
  /// group, the checks.
  fn file_step(
    &mut self,
    file: usize,
    at: usize,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let Some(group) = self.group.files.get(file).cloned() else {
      self.enter(Stage::ManifestsPresent(0));
      return Ok(Stepped::More);
    };
    if at == 0 {
      audit(group.name)?;
    }
    let Some(revision) = self.group.group(&group.revisions).get(at).copied() else {
      self.enter(Stage::Files {
        file: file.saturating_add(1),
        at: 0,
      });
      return Ok(Stepped::More);
    };
    self.take(Log::File(group.name), &revision, store, admission)?;
    self.stage = Stage::Files {
      file,
      at: at.saturating_add(1),
    };
    Ok(Stepped::More)
  }

  /// Takes one revision of `log` (module docs): passed over, or checked, rebuilt and kept.
  fn take(
    &mut self,
    log: Log<'_>,
    revision: &Revision<'_>,
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
    let text = rebuilt(
      self.base_text(log, revision, store),
      revision,
      log,
      &mut room,
    )?;
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
    keep_one(&mut self.fork, store, log, &arrived, &mut room)?;
    admission.charge_items::<Node>(1)?;
    self.brought.insert(revision.node);
    self.intaken.kept = self.intaken.kept.saturating_add(1);
    self.intaken.bytes = self
      .intaken
      .bytes
      .saturating_add(to_u64(arrived.text.len()));
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
        admission.charge_items::<(Node, Node)>(1)?;
        self.manifests.push((node, revision.p1));
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

  /// The check of the new manifest at `at`: every file it names beside its first parent's manifest audited and held
  /// by the fork's file log.
  fn files_present(
    &mut self,
    at: usize,
    store: &ContentStore,
    admission: &Admission,
  ) -> Result<Stepped, Refused> {
    let Some((node, p1)) = self.manifests.get(at).copied() else {
      self.stage = Stage::Done;
      return Ok(Stepped::Done);
    };
    let mut room = *admission;
    let held = IndexHeld::new(&self.fork, store);
    let parent = match p1 {
      NULL_NODE => Cow::Borrowed(&[][..]),
      p1 => held.manifest_text(&p1).unwrap_or(Cow::Borrowed(&[][..])),
    };
    let previous: BTreeMap<&[u8], Node> = match manifest::decode(&parent) {
      Ok(entries) => {
        room.charge_items::<(&[u8], Node)>(entries.clone().count())?;
        entries.map(|entry| (entry.path, entry.node)).collect()
      }
      Err(_) => BTreeMap::new(),
    };
    let text = held
      .manifest_text(&node)
      .ok_or(Unverified::MissingManifest {
        changeset: NULL_NODE,
        manifest: node,
      })?;
    let entries = manifest::decode(&text).map_err(|fault| Unverified::Manifest { node, fault })?;
    for entry in entries.filter(|entry| previous.get(entry.path) != Some(&entry.node)) {
      audit(entry.path)?;
      if self
        .fork
        .rev(store, Log::File(entry.path), &entry.node)
        .is_none()
      {
        return Err(
          Unverified::MissingFile {
            path: entry.path.to_vec(),
            node: entry.node,
          }
          .into(),
        );
      }
    }
    self.stage = Stage::FilesPresent(at.saturating_add(1));
    Ok(Stepped::More)
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

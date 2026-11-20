//! A revision replayed as the delta editor's calls (A-703; §4.17.4 "5. The wire"): `svn_repos_replay2` with
//! `svn_delta_path_driver3` (`libsvn_repos/replay.c` and `libsvn_delta/path_driver.c`, Subversion 1.15.0), what
//! `svnserve`'s `replay` and `replay-range` drive over the connection's editor and `mod_dav_svn`'s replay report over
//! its XML. One [`Call`] at a time, from [`Replay::next`].
//!
//! **The calls.** `set_target_revision` first; revision 0 is that alone. Then the revision's changed paths the
//! session may read beneath its base path (or above it, its ancestors: `get_relevant_changes`), each as a relative path,
//! in `svn_sort_compare_paths`' order, driven as the path driver drives them: the root opened first unless the first
//! path is the root; at each path, the directories the last path opened closed down to the two paths' common ancestor,
//! each directory between that ancestor and the path opened, then the path's own calls (`path_driver_cb_func`):
//! - a deleted or replaced path's `delete_entry`, with no revision;
//! - an added or replaced path's `add_directory` or `add_file`, with its copy source when the source is readable, lies
//!   beneath the base path and was made at or after the low-water mark; a directory copied without its source (a copy
//!   from below the mark, or from outside or unreadable) is added whole, its source's tree as new nodes with the target's
//!   properties and texts (`add_subdir`), a path the revision itself changed there taken out of the later calls;
//! - a modified path's `open_directory`, `open_root` or `open_file`, with no revision;
//! - then, where the path's properties changed or its copy was made without its source, its properties' differences
//!   from its delta source (the copy's source, the source of the copy it lies inside, else the path in the revision
//!   before), or one property change with an empty name and no value when no deltas are sent;
//! - where a file's text changed or its copy was made without its source, `apply_textdelta` with the source's MD5, the
//!   delta's windows against the source (none when no deltas are sent), its end; and every file's `close_file` with its
//!   text's MD5.
//!
//! Every directory still open is closed at the end; there is no `close_edit` (the connection answers `finish-replay`).
//!
//! **Bounded work.** A step is one path's calls, one entry of a directory being added whole, or one window of a text:
//! the drive holds the calls of one step and the stack of directories open.
//!
//! **Where it differs from `svnserve`**, each in what a client reads alike: properties, and the entries of a directory
//! added whole, come in name order, where `svnserve`'s come in APR's per-table seeded hash order; and a deleted path's
//! readability is judged at the path itself, where `was_readable` also consults the copy it lay inside.
//!
//! No I/O, no clock, no randomness, no panic.

use std::collections::{BTreeMap, VecDeque};

use super::{
  Call, Delta, DriveFailure, LIST_STEP, Owned, OwnedFacts, WindowWork, property_changes,
  property_list, window_unit,
};
use crate::authz::{PrincipalRules, may_read};
use crate::index::{Action, ChangedPath, Index, NodeKind, Revision, Text};
use crate::reads::Repository;
use crate::svndiff::{Compression, Version};

/// A step of `add_subdir`'s listing (A-759): the source's next [`LIST_STEP`] entries in name order, read through the
/// index's cursor after the last one read, so no step lists a large directory whole.
fn read_whole_entries(index: &Index, whole: &mut Whole) -> Result<(), DriveFailure> {
  let (source, revision) = (&whole.source.0, whole.source.1);
  let (part, more) = index
    .listing_after(source, revision, whole.listed_after.as_deref(), LIST_STEP)
    .map_err(|_| DriveFailure::corrupt())?
    .unwrap_or_default();
  super::note_listed(part.len());
  whole.listed_after = part.last().map(|(name, _)| name.clone());
  whole.entries.extend(part.into_iter().map(|(name, _)| name));
  whole.more = more;
  Ok(())
}

/// The path separator.
const SEPARATOR: char = '/';

/// A copy the drive is inside (`struct copy_info`): the directory added and its copy source, none where it was added
/// without one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Copy {
  /// The directory's relative path.
  path: String,
  /// Its copy source: the source's repository path and revision.
  source: Option<(String, Revision)>,
}

/// A text being sent: its target, its delta source, how much of the target is sent, and the window in the making.
#[derive(Clone, Debug)]
struct Sending {
  /// The text in the revision.
  target: Text,
  /// The text it is a delta against; none sends it as new data.
  source: Option<Text>,
  /// The target's bytes sent.
  sent: u64,
  /// The window in the making, between its steps (A-751).
  window: Option<Box<WindowWork>>,
  /// Whether the delta's svndiff header is sent: with its first window's first write.
  header_sent: bool,
}

/// A directory being added whole (`add_subdir`): where it is added, where its entries are read, and its entries left.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Whole {
  /// Its relative path.
  path: String,
  /// Its source's repository path and revision.
  source: (String, Revision),
  /// Its source's entries read and not yet added, in name order: at most [`LIST_STEP`] of them (A-759).
  entries: VecDeque<String>,
  /// The last entry read from the source's listing, where its cursor goes on.
  listed_after: Option<String>,
  /// Whether the source's listing has entries not yet read.
  more: bool,
}

/// One revision's replay (module documentation).
#[derive(Clone, Debug)]
pub struct Replay {
  /// The revision.
  revision: Revision,
  /// The session's base path, relative (`svnserve`'s `fs_path` less its leading `/`).
  base: String,
  /// Copies from before it are sent without their source.
  low_water_mark: Revision,
  /// Whether texts and properties go as deltas from their sources.
  send_deltas: bool,
  /// Whether `set_target_revision` and the changed paths have been read.
  started: bool,
  /// The changed paths left, in the path driver's order.
  paths: VecDeque<String>,
  /// Each changed path's change, by its relative path; a path a whole directory's addition already sent is taken out.
  changes: BTreeMap<String, ChangedPath>,
  /// The directories open, outermost first, the root as the empty path.
  open: Vec<String>,
  /// The last path the driver drove: the path if it opened a directory, else its parent.
  last: Option<String>,
  /// The copies the drive is inside, innermost last.
  copies: Vec<Copy>,
  /// The directories being added whole, innermost last; the outermost stays open when its entries end.
  wholes: Vec<Whole>,
  /// A text being sent, and whether its file closes after it.
  sending: Option<Sending>,
  /// The calls of the step taken, not yet given.
  pending: VecDeque<Owned>,
  /// The call given last.
  given: Option<Owned>,
  /// The svndiff version and compression its deltas are encoded in (A-751).
  encoding: (Version, Compression),
}

/// What [`Replay::next_within`] made.
#[derive(Debug)]
pub enum Stepped<'a> {
  /// The replay's next call.
  Call(Call<'a>),
  /// The steps given were taken and no call is ready yet: ask again.
  Paused,
  /// The replay has made its last call.
  Ended,
}

impl Replay {
  /// A replay of `revision` for a session at `base` (a repository path), copies from before `low_water_mark` sent
  /// without their source, texts and properties as deltas when `send_deltas`.
  pub fn new(revision: Revision, base: &str, low_water_mark: Revision, send_deltas: bool) -> Self {
    Self {
      revision,
      base: base.trim_start_matches(SEPARATOR).to_owned(),
      low_water_mark,
      send_deltas,
      started: false,
      paths: VecDeque::new(),
      changes: BTreeMap::new(),
      open: Vec::new(),
      last: None,
      copies: Vec::new(),
      wholes: Vec::new(),
      sending: None,
      pending: VecDeque::new(),
      given: None,
      encoding: (Version::Zero, Compression::Stored),
    }
  }

  /// The replay with its deltas encoded in `version` with `compression`, as its consumer negotiated (version 0,
  /// stored, unless set).
  pub fn with_encoding(mut self, version: Version, compression: Compression) -> Self {
    self.encoding = (version, compression);
    self
  }

  /// The replay's next call made within `steps` steps, at least one (A-751): [`Stepped::Paused`] when they are taken
  /// first, so a caller holds its turn to its time; [`Stepped::Ended`] after the last call.
  ///
  /// # Errors
  /// [`Replay::next`]'s.
  pub fn next_within<R: Repository + ?Sized>(
    &mut self,
    (index, repository): (&Index, &R),
    rules: Option<&PrincipalRules>,
    steps: usize,
  ) -> Result<Stepped<'_>, DriveFailure> {
    let mut left = steps.max(1);
    while self.pending.is_empty() {
      let Some(rest) = left.checked_sub(1) else {
        self.given = None;
        return Ok(Stepped::Paused);
      };
      left = rest;
      if !self.step(index, repository, rules)? {
        self.given = None;
        return Ok(Stepped::Ended);
      }
    }
    self.given = self.pending.pop_front();
    Ok(
      self
        .given
        .as_ref()
        .map_or(Stepped::Ended, |given| Stepped::Call(given.lent())),
    )
  }

  /// The replay's next call; none at its end.
  ///
  /// # Errors
  /// [`DriveFailure`] for a revision the index does not hold ("No such revision"), and for an index and a green that
  /// disagree.
  pub fn next<R: Repository + ?Sized>(
    &mut self,
    index: &Index,
    repository: &R,
    rules: Option<&PrincipalRules>,
  ) -> Result<Option<Call<'_>>, DriveFailure> {
    while self.pending.is_empty() {
      if !self.step(index, repository, rules)? {
        self.given = None;
        return Ok(None);
      }
    }
    self.given = self.pending.pop_front();
    Ok(self.given.as_ref().map(Owned::lent))
  }

  /// One bounded step (module documentation); whether there is more.
  fn step<R: Repository + ?Sized>(
    &mut self,
    index: &Index,
    repository: &R,
    rules: Option<&PrincipalRules>,
  ) -> Result<bool, DriveFailure> {
    if !self.started {
      self.start(index, rules)?;
      return Ok(true);
    }
    if self.sending.is_some() {
      self.window(repository)?;
      return Ok(true);
    }
    if !self.wholes.is_empty() {
      self.whole_entry(index, repository, rules)?;
      return Ok(true);
    }
    if let Some(path) = self.paths.pop_front() {
      self.drive(index, repository, rules, &path)?;
      return Ok(true);
    }
    if self.open.pop().is_some() {
      self.pending.push_back(Owned::CloseDirectory);
      return Ok(true);
    }
    Ok(false)
  }

  /// `set_target_revision`, and the revision's relevant changes in the path driver's order (`get_relevant_changes`).
  fn start(&mut self, index: &Index, rules: Option<&PrincipalRules>) -> Result<(), DriveFailure> {
    self.started = true;
    if self.revision > index.youngest() {
      return Err(DriveFailure::no_such_revision(self.revision));
    }
    self.pending.push_back(Owned::TargetRevision(self.revision));
    if self.revision == 0 {
      return Ok(());
    }
    let changes = index
      .changes(self.revision)
      .map_err(|_| DriveFailure::corrupt())?;
    for change in changes {
      if !may_read(rules, &change.path) {
        continue;
      }
      let relative = change.path.trim_start_matches(SEPARATOR).to_owned();
      if beneath(&self.base, &relative) || beneath(&relative, &self.base) {
        self.changes.insert(relative, change.clone());
      }
    }
    let mut paths: Vec<String> = self.changes.keys().cloned().collect();
    paths.sort_by(|left, right| compare_paths(left, right));
    self.paths = paths.into();
    Ok(())
  }

  /// `svn_delta_path_driver_step` for `path`: the directories closed and opened down to it, then its own calls.
  fn drive<R: Repository + ?Sized>(
    &mut self,
    index: &Index,
    repository: &R,
    rules: Option<&PrincipalRules>,
    path: &str,
  ) -> Result<(), DriveFailure> {
    if self.last.is_none() && !path.is_empty() {
      let facts = self.facts(index, "")?;
      self.pending.push_back(Owned::OpenRoot(None, facts));
      self.open.push(String::new());
    }
    let common = self
      .last
      .as_deref()
      .map_or_else(String::new, |last| common_ancestor(last, path));
    if let Some(last) = self.last.clone() {
      let below = relative_to(&common, &last);
      for _ in 0..components(below) {
        if self.open.pop().is_some() {
          self.pending.push_back(Owned::CloseDirectory);
        }
      }
    }
    let parent = parent_of(path);
    if parent.len() > common.len() {
      let mut opened = common.clone();
      for name in relative_to(&common, parent).split(SEPARATOR) {
        opened = join(&opened, name);
        let facts = self.facts(index, &opened)?;
        self.pending.push_back(Owned::Open(
          NodeKind::Directory,
          opened.clone(),
          None,
          facts,
        ));
        self.open.push(opened.clone());
      }
    }
    let opened_directory = self.path_calls(index, repository, rules, path)?;
    if opened_directory {
      self.open.push(path.to_owned());
      self.last = Some(path.to_owned());
    } else {
      self.last = Some(parent.to_owned());
    }
    Ok(())
  }

  /// `path_driver_cb_func` for `path`: its calls queued; whether it left a directory open.
  fn path_calls<R: Repository + ?Sized>(
    &mut self,
    index: &Index,
    repository: &R,
    rules: Option<&PrincipalRules>,
    path: &str,
  ) -> Result<bool, DriveFailure> {
    while self
      .copies
      .last()
      .is_some_and(|copy| !beneath(&copy.path, path))
    {
      self.copies.pop();
    }
    let Some(change) = self.changes.remove(path) else {
      return Ok(false);
    };
    let (add, delete) = match change.action {
      Action::Add => (true, false),
      Action::Delete => (false, true),
      Action::Replace => (true, true),
      Action::Modify => (false, false),
    };
    if delete && may_read(rules, &fspath(path)) {
      self.pending.push_back(Owned::Delete(path.to_owned(), None));
    }
    if delete && !add {
      return Ok(false);
    }
    let directory = change.kind == NodeKind::Directory;
    let facts = self.facts(index, path)?;
    let (opened, source, downgraded) = if add {
      if path.is_empty() {
        return Err(DriveFailure::corrupt());
      }
      let sent = change.copy_from.as_ref().and_then(|copy| {
        let relative = copy.path.trim_start_matches(SEPARATOR);
        let readable = may_read(rules, &copy.path);
        (readable && beneath(&self.base, relative) && copy.revision >= self.low_water_mark)
          .then(|| (copy.path.clone(), copy.revision))
      });
      let downgraded = change.copy_from.is_some() && sent.is_none();
      if directory && downgraded {
        let copy = change
          .copy_from
          .as_ref()
          .ok_or_else(DriveFailure::corrupt)?;
        self.begin_whole(index, repository, path, (copy.path.clone(), copy.revision))?;
        return Ok(true);
      }
      let kind = change.kind;
      self
        .pending
        .push_back(Owned::Add(kind, path.to_owned(), sent.clone(), facts));
      if directory && (sent.is_some() || !self.copies.is_empty()) {
        self.copies.push(Copy {
          path: path.to_owned(),
          source: sent.clone(),
        });
      }
      (directory, sent, downgraded)
    } else {
      let call = if directory && path.is_empty() {
        Owned::OpenRoot(None, facts)
      } else {
        Owned::Open(change.kind, path.to_owned(), None, facts)
      };
      self.pending.push_back(call);
      (directory, self.modified_source(path), false)
    };
    self.node_changes(index, repository, (&change, path), (source, downgraded))?;
    Ok(opened)
  }

  /// A modified path's delta source: the source of the copy it lies inside, none inside a copy made without one, else
  /// the path in the revision before when deltas are sent.
  fn modified_source(&self, path: &str) -> Option<(String, Revision)> {
    match self.copies.last() {
      Some(copy) => copy.source.as_ref().map(|(source, revision)| {
        let below = relative_to(&copy.path, path);
        (join(source, below), *revision)
      }),
      None => self
        .send_deltas
        .then(|| (fspath(path), self.revision.saturating_sub(1))),
    }
  }

  /// A node's property changes, and a file's text and close (module documentation).
  fn node_changes<R: Repository + ?Sized>(
    &mut self,
    index: &Index,
    repository: &R,
    (change, path): (&ChangedPath, &str),
    (source, downgraded): (Option<(String, Revision)>, bool),
  ) -> Result<(), DriveFailure> {
    let target = node_at(index, &fspath(path), self.revision)?;
    let from = match &source {
      Some((source_path, revision)) if self.send_deltas => {
        Some(node_at(index, source_path, *revision)?)
      }
      _ => None,
    };
    if change.properties.is_some() || downgraded {
      if self.send_deltas {
        let old = match &from {
          Some(node) => property_list(repository, node.properties)?,
          None => Vec::new(),
        };
        let new = property_list(repository, target.properties)?;
        for (name, value) in property_changes(&old, &new) {
          self.pending.push_back(Owned::ChangeProperty(name, value));
        }
      } else {
        self
          .pending
          .push_back(Owned::ChangeProperty(String::new(), None));
      }
    }
    if change.kind != NodeKind::File {
      return Ok(());
    }
    let text = target.text.ok_or_else(DriveFailure::corrupt)?;
    if change.text.is_some() || downgraded {
      let base = from.and_then(|node| node.text);
      self
        .pending
        .push_back(Owned::ApplyTextDelta(base.map(|text| text.md5)));
      if self.send_deltas {
        self.sending = Some(Sending {
          target: text,
          source: base,
          sent: 0,
          window: None,
          header_sent: false,
        });
        return Ok(());
      }
      self.pending.push_back(Owned::TextDeltaEnd);
    }
    self.pending.push_back(Owned::CloseFile(text.md5));
    Ok(())
  }

  /// One step of the delta's next window, `txdelta_next_window`'s, made and sent a step at a time as the update drive's
  /// are (`super::window_unit`, A-751), or its end and the file's close.
  fn window<R: Repository + ?Sized>(&mut self, repository: &R) -> Result<(), DriveFailure> {
    let Some(mut sending) = self.sending.take() else {
      return Ok(());
    };
    if sending.sent >= sending.target.size {
      self.pending.push_back(Owned::TextDeltaEnd);
      self.pending.push_back(Owned::CloseFile(sending.target.md5));
      return Ok(());
    }
    let delta = Delta {
      text: sending.target,
      source: sending.source,
      sent: &mut sending.sent,
      header_sent: &mut sending.header_sent,
    };
    let pending = &mut self.pending;
    window_unit(
      &mut sending.window,
      delta,
      (self.encoding, repository),
      &mut |write, at, piece| pending.push_back(Owned::DeltaPiece(write, at, piece)),
    )?;
    self.sending = Some(sending);
    Ok(())
  }

  /// `add_subdir`'s start: the directory added with no source, the target's properties set on it, its source's
  /// entries to add.
  fn begin_whole<R: Repository + ?Sized>(
    &mut self,
    index: &Index,
    repository: &R,
    path: &str,
    source: (String, Revision),
  ) -> Result<(), DriveFailure> {
    let facts = self.facts(index, path)?;
    self.pending.push_back(Owned::Add(
      NodeKind::Directory,
      path.to_owned(),
      None,
      facts,
    ));
    let target = node_at(index, &fspath(path), self.revision)?;
    for (name, value) in property_list(repository, target.properties)? {
      self
        .pending
        .push_back(Owned::ChangeProperty(name, Some(value)));
    }
    self.wholes.push(Whole {
      path: path.to_owned(),
      source,
      entries: VecDeque::new(),
      listed_after: None,
      more: true,
    });
    Ok(())
  }

  /// `add_subdir`'s next entry: added whole, as a directory (its own entries next) or a file with its properties and
  /// text; a directory whose entries are all added closed, but the outermost, which the path driver closes.
  fn whole_entry<R: Repository + ?Sized>(
    &mut self,
    index: &Index,
    repository: &R,
    rules: Option<&PrincipalRules>,
  ) -> Result<(), DriveFailure> {
    let Some(whole) = self.wholes.last_mut() else {
      return Ok(());
    };
    if whole.entries.is_empty() && whole.more {
      return read_whole_entries(index, whole);
    }
    let Some(name) = whole.entries.pop_front() else {
      self.wholes.pop();
      if !self.wholes.is_empty() {
        self.pending.push_back(Owned::CloseDirectory);
      }
      return Ok(());
    };
    let path = join(&whole.path, &name);
    let mut source = (join(&whole.source.0, &name), whole.source.1);
    let change = self.changes.remove(&path);
    let mut replaced_without_source = false;
    if let Some(change) = &change {
      match change.action {
        Action::Delete => return Ok(()),
        Action::Replace => match &change.copy_from {
          Some(copy) => source = (copy.path.clone(), copy.revision),
          None => replaced_without_source = true,
        },
        Action::Add | Action::Modify => {}
      }
    }
    if !may_read(rules, &fspath(&path)) {
      return Ok(());
    }
    let node = node_at(index, &fspath(&path), self.revision)?;
    let facts = self.facts(index, &path)?;
    if node.kind == NodeKind::Directory {
      if replaced_without_source {
        self
          .pending
          .push_back(Owned::Add(NodeKind::Directory, path, None, facts));
        self.pending.push_back(Owned::CloseDirectory);
        return Ok(());
      }
      return self.begin_whole(index, repository, &path, source);
    }
    self
      .pending
      .push_back(Owned::Add(NodeKind::File, path, None, facts));
    for (property, value) in property_list(repository, node.properties)? {
      self
        .pending
        .push_back(Owned::ChangeProperty(property, Some(value)));
    }
    let text = node.text.ok_or_else(DriveFailure::corrupt)?;
    self.pending.push_back(Owned::ApplyTextDelta(None));
    self.sending = Some(Sending {
      target: text,
      source: None,
      sent: 0,
      window: None,
      header_sent: false,
    });
    Ok(())
  }

  /// The facts of the node at relative `path` in the revision.
  fn facts(&self, index: &Index, path: &str) -> Result<OwnedFacts, DriveFailure> {
    let node = node_at(index, &fspath(path), self.revision)?;
    Ok(OwnedFacts {
      repository_path: fspath(path),
      created_revision: node.created_revision,
      sha1: node.text.map(|text| text.sha1),
    })
  }
}

/// What the replay reads of a node.
#[derive(Clone, Copy, Debug)]
struct NodeFacts {
  /// Its kind.
  kind: NodeKind,
  /// The revision it was created in.
  created_revision: Revision,
  /// A file's text.
  text: Option<Text>,
  /// Its property list.
  properties: crate::index::PropertyList,
}

/// The node at repository path `path` in `revision`; refused as corrupt where the index holds none.
fn node_at(index: &Index, path: &str, revision: Revision) -> Result<NodeFacts, DriveFailure> {
  let node = index
    .node(path, revision)
    .map_err(|_| DriveFailure::corrupt())?
    .ok_or_else(DriveFailure::corrupt)?;
  Ok(NodeFacts {
    kind: node.kind(),
    created_revision: node.created_revision(),
    text: node.text(),
    properties: node.properties(),
  })
}

/// The repository path of relative `path`.
fn fspath(path: &str) -> String {
  format!("{SEPARATOR}{path}")
}

/// `parent` joined with `name`.
fn join(parent: &str, name: &str) -> String {
  if parent.is_empty() {
    name.to_owned()
  } else if name.is_empty() {
    parent.to_owned()
  } else if parent.ends_with(SEPARATOR) {
    format!("{parent}{name}")
  } else {
    format!("{parent}{SEPARATOR}{name}")
  }
}

/// Whether relative `path` is `ancestor` or beneath it (`svn_relpath_skip_ancestor` answering).
fn beneath(ancestor: &str, path: &str) -> bool {
  ancestor.is_empty()
    || path == ancestor
    || path
      .strip_prefix(ancestor)
      .is_some_and(|rest| rest.starts_with(SEPARATOR))
}

/// `path` relative to its ancestor `ancestor`: empty for the ancestor itself.
fn relative_to<'p>(ancestor: &str, path: &'p str) -> &'p str {
  if ancestor.is_empty() {
    return path;
  }
  path
    .strip_prefix(ancestor)
    .map_or(path, |rest| rest.trim_start_matches(SEPARATOR))
}

/// The parent of relative `path`; the empty path for a name at the root.
fn parent_of(path: &str) -> &str {
  path
    .rfind(SEPARATOR)
    .map_or("", |at| path.get(..at).unwrap_or(""))
}

/// The number of names in relative `path`.
fn components(path: &str) -> usize {
  if path.is_empty() {
    0
  } else {
    path.split(SEPARATOR).count()
  }
}

/// The longest common ancestor of two relative paths (`svn_relpath_get_longest_ancestor`).
fn common_ancestor(left: &str, right: &str) -> String {
  let mut common = Vec::new();
  for (one, other) in left.split(SEPARATOR).zip(right.split(SEPARATOR)) {
    if one != other || one.is_empty() {
      break;
    }
    common.push(one);
  }
  common.join("/")
}

/// `svn_sort_compare_paths`: byte by byte, a separator before every other byte.
fn compare_paths(left: &str, right: &str) -> std::cmp::Ordering {
  let key = |byte: u8| if byte == b'/' { 0 } else { u16::from(byte) + 1 };
  left.bytes().map(key).cmp(right.bytes().map(key))
}

#[cfg(test)]
pub(crate) mod tests;

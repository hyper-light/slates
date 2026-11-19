//! The export (A-628): a range of a repository's revisions written as the stream `svn_repos_dump_fs4` writes for the
//! same range (`dump.c` 2026-2247): a version record and a UUID record, then each revision's record with its
//! properties, and for every revision but 0 its changes, replayed as `svn_repos_replay2` drives `dump.c`'s editor
//! (`replay.c` 917-1060, `path_driver_cb_func` 452-796):
//! - the revision's changed paths in `svn_path_compare_paths` order, through the path driver (`path_driver.c`
//!   `svn_delta_path_driver_step`): the directories between the last path and this one closed, those down to this
//!   one's directory opened, then the path itself visited; a directory opened or added is the next step's last path,
//!   anything else its directory;
//! - a deletion remembered in its directory and written when the directory closes, its directory's deletions in byte
//!   order (`close_directory`, `svn_sort_compare_items_lexically`); a replacement a deletion and an addition, which
//!   `dump_node` writes as one `replace` record or, by a copy, as a deletion record and an addition;
//! - each record's sections decided as `dump_node` decides them (1118-1544): an addition without history writes its
//!   whole property list and a file's whole text; a change, or a copy, writes the property list when it differs from
//!   the node it is compared with and a file's text when that differs, where a node is compared with itself in the
//!   revision before, or with its copy source, or with the place beneath a directory copied in the same revision
//!   (`open_directory` and `open_file` take the parent's comparison and the name); a directory's change is written
//!   when its properties changed (`change_dir_prop` on a directory not yet written). FSFS compares representation keys
//!   (`svn_fs_props_changed`, `svn_fs_contents_changed`) and shares equal representations, so equal content
//!   identities are unchanged here.
//!
//! A range starting after revision 0 is written incrementally, every revision replayed (`--incremental`), which is what
//! a migration in parts loads; the whole tree `svnadmin dump` writes for the first revision of a range that is not
//! incremental is not offered. Format 3 is owed with Subversion's own deltifier ([`MigrationRefusal::DeltasOwed`]),
//! since only its windows make `svnadmin dump --deltas`'s bytes.

use std::collections::BTreeSet;

use super::MigrationRefusal;
use super::paths::{
  absolute, basename, compare_paths, dirname, join, longest_ancestor, names, relative,
};
use super::records::{Facts, Properties, Repository, node};
use crate::dump::{
  CopyFrom, Event, Format, NodeAction, NodeHeader, Property, PropertySection, RevisionHeader,
  TextSection, Writer, section_len,
};
use crate::index::{Action, ChangedPath, NodeKind, Revision};

/// What one call of [`Exporter::export`] wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exporting {
  /// The records of one revision were appended, the stream's version and UUID records before the range's first.
  Wrote(Revision),
  /// The range is written; nothing was appended.
  Done,
}

/// An export of a range of a repository's revisions (the module documentation).
#[derive(Clone, Debug)]
pub struct Exporter {
  /// The stream's writer.
  writer: Writer,
  /// The revision written next; none once the range is written.
  next: Option<Revision>,
  /// The range's last revision.
  last: Revision,
  /// Whether the stream's version and UUID records are written.
  started: bool,
  /// The refusal the export met, once it met one: every later call answers it.
  refused: Option<MigrationRefusal>,
}

impl Exporter {
  /// An export of revisions `first` to `last` of `repository` in `format`.
  ///
  /// # Errors
  /// [`MigrationRefusal::BadRange`] when `first` is past `last` or `last` past the youngest revision, as
  /// `svn_repos_dump_fs4` refuses them; [`MigrationRefusal::DeltasOwed`] for format 3.
  pub fn new(
    repository: &Repository,
    first: Revision,
    last: Revision,
    format: Format,
  ) -> Result<Self, MigrationRefusal> {
    if format == Format::Three {
      return Err(MigrationRefusal::DeltasOwed);
    }
    let youngest = repository.youngest();
    if first > last || last > youngest {
      return Err(MigrationRefusal::BadRange {
        first,
        last,
        youngest,
      });
    }
    Ok(Self {
      writer: Writer::new(),
      next: Some(first),
      last,
      started: false,
      refused: None,
    })
  }

  /// Appends the next revision's records to `out`, the stream's version and UUID records before the first; a revision
  /// is appended whole or not at all.
  ///
  /// # Errors
  /// [`MigrationRefusal::Corrupt`] for records that name what the repository does not hold, and the writer's refusal
  /// of anything it cannot write; every later call answers the same refusal.
  pub fn export(
    &mut self,
    repository: &Repository,
    out: &mut Vec<u8>,
  ) -> Result<Exporting, MigrationRefusal> {
    if let Some(refusal) = &self.refused {
      return Err(refusal.clone());
    }
    let Some(revision) = self.next else {
      return Ok(Exporting::Done);
    };
    let mut written = Vec::new();
    if let Err(refusal) = self.write(repository, revision, &mut written) {
      self.refused = Some(refusal.clone());
      return Err(refusal);
    }
    out.extend_from_slice(&written);
    self.next = if revision < self.last {
      revision.checked_add(1)
    } else {
      None
    };
    Ok(Exporting::Wrote(revision))
  }

  /// Writes `revision`'s records, the stream's first records before the range's first revision.
  fn write(
    &mut self,
    repository: &Repository,
    revision: Revision,
    out: &mut Vec<u8>,
  ) -> Result<(), MigrationRefusal> {
    let mut stream = Stream {
      writer: &mut self.writer,
      out,
    };
    if !self.started {
      stream.event(&Event::Format(Format::Two))?;
      stream.event(&Event::Uuid(repository.uuid()))?;
      self.started = true;
    }
    let properties = repository
      .revision_properties(revision)
      .ok_or(MigrationRefusal::Corrupt)?;
    stream.event(&Event::Revision(RevisionHeader {
      number: revision,
      properties: Some(properties_len(properties)?),
    }))?;
    stream.properties(properties)?;
    if revision == 0 {
      return Ok(());
    }
    let mut rows: Vec<&ChangedPath> = repository
      .index()
      .changes(revision)
      .map_err(MigrationRefusal::Index)?
      .iter()
      .collect();
    rows.sort_by(|first, second| {
      compare_paths(relative(&first.path).as_bytes(), relative(&second.path).as_bytes())
    });
    let mut drive = Drive {
      repository,
      revision,
      stream,
      stack: Vec::new(),
      last: None,
    };
    for row in rows {
      drive.step(row)?;
    }
    drive.finish()
  }
}

/// The writer and the bytes it appends to.
struct Stream<'a> {
  /// The stream's writer.
  writer: &'a mut Writer,
  /// The bytes written.
  out: &'a mut Vec<u8>,
}

impl Stream<'_> {
  /// Writes one event.
  fn event(&mut self, event: &Event<'_>) -> Result<(), MigrationRefusal> {
    self
      .writer
      .write(event, self.out)
      .map_err(MigrationRefusal::Stream)
  }

  /// Writes a property section's entries, in name order, and its end.
  fn properties(&mut self, properties: &Properties) -> Result<(), MigrationRefusal> {
    for (name, value) in properties {
      self.event(&Event::Property(Property::Set { name, value }))?;
    }
    self.event(&Event::PropertiesEnd)
  }
}

/// The length of a whole property section listing `properties`.
fn properties_len(properties: &Properties) -> Result<u64, MigrationRefusal> {
  section_len(
    properties
      .iter()
      .map(|(name, value)| Property::Set { name, value }),
  )
  .ok_or(MigrationRefusal::Corrupt)
}

/// A place a node is compared with: a relative path and a revision.
type Place = (String, Revision);

/// A directory the drive has open (`dump.c`'s `dir_baton`).
struct Directory {
  /// What it is compared with when not itself in the revision before: its copy source, or the place beneath a
  /// directory copied in this revision.
  compare: Option<Place>,
  /// Whether its record is written.
  written: bool,
  /// The paths deleted in it, written when it closes.
  deleted: BTreeSet<String>,
}

/// One revision's replay through the dump editor (the module documentation).
struct Drive<'a> {
  /// The repository.
  repository: &'a Repository,
  /// The revision replayed.
  revision: Revision,
  /// Where its records go.
  stream: Stream<'a>,
  /// The directories open, the root first.
  stack: Vec<Directory>,
  /// The last path visited, or its directory (`path_driver.c`'s `last_path`).
  last: Option<String>,
}

impl Drive<'_> {
  /// One changed path (`svn_delta_path_driver_step`).
  fn step(&mut self, row: &ChangedPath) -> Result<(), MigrationRefusal> {
    let path = relative(&row.path).to_owned();
    if self.last.is_none() && !path.is_empty() {
      self.stack.push(directory(None, false));
    }
    let common = self
      .last
      .as_deref()
      .map_or("", |last| longest_ancestor(last, &path))
      .to_owned();
    self.close_below(&common)?;
    self.open_down_to(dirname(&path), &common);
    let opened = self.visit(&path, row)?;
    self.last = Some(if opened {
      path
    } else {
      dirname(&path).to_owned()
    });
    Ok(())
  }

  /// Closes the directories the last path opened below `common` (step B).
  fn close_below(&mut self, common: &str) -> Result<(), MigrationRefusal> {
    let count = match self.last.as_deref() {
      Some(last) if last.len() > common.len() => {
        let rest = if common.is_empty() {
          last
        } else {
          last.get(common.len().saturating_add(1)..).unwrap_or("")
        };
        names(rest)
      }
      _ => 0,
    };
    for _ in 0..count {
      self.close()?;
    }
    Ok(())
  }

  /// Opens the directories from below `common` down to `directory` (step C).
  fn open_down_to(&mut self, directory: &str, common: &str) {
    if directory.len() <= common.len() {
      return;
    }
    let search_from = common.len().saturating_add(1);
    let ends: Vec<usize> = directory
      .match_indices('/')
      .map(|(end, _)| end)
      .filter(|&end| end >= search_from)
      .chain([directory.len()])
      .collect();
    for end in ends {
      if let Some(path) = directory.get(..end) {
        self.open(path);
      }
    }
  }

  /// Opens a directory below the open one (`open_directory`), its comparison the parent's with its name.
  fn open(&mut self, path: &str) {
    let compare = self.inherited(path);
    self.stack.push(directory(compare, false));
  }

  /// The comparison a node below the open directory takes from it.
  fn inherited(&self, path: &str) -> Option<Place> {
    self
      .stack
      .last()
      .and_then(|parent| parent.compare.as_ref())
      .map(|(source, revision)| (join(source, basename(path)), *revision))
  }

  /// Closes the open directory, writing its deletions in byte order (`close_directory`).
  fn close(&mut self) -> Result<(), MigrationRefusal> {
    let closed = self.stack.pop().ok_or(MigrationRefusal::Corrupt)?;
    for path in &closed.deleted {
      self.record(&NodeHeader {
        path: path.as_bytes().to_vec(),
        kind: None,
        action: NodeAction::Delete,
        copy_from: None,
        properties: None,
        text: None,
      })?;
    }
    Ok(())
  }

  /// Closes every directory still open.
  fn finish(mut self) -> Result<(), MigrationRefusal> {
    while !self.stack.is_empty() {
      self.close()?;
    }
    Ok(())
  }

  /// Visits one changed path (`path_driver_cb_func`); answers whether it opened a directory.
  fn visit(&mut self, path: &str, row: &ChangedPath) -> Result<bool, MigrationRefusal> {
    match row.action {
      Action::Delete => {
        self.deleted_in_parent(path)?;
        Ok(false)
      }
      Action::Replace => {
        self.deleted_in_parent(path)?;
        self.add(path, row)
      }
      Action::Add => self.add(path, row),
      Action::Modify => self.modify(path, row),
    }
  }

  /// Remembers a deletion in the open directory (`delete_entry`).
  fn deleted_in_parent(&mut self, path: &str) -> Result<(), MigrationRefusal> {
    let parent = self.stack.last_mut().ok_or(MigrationRefusal::Corrupt)?;
    parent.deleted.insert(path.to_owned());
    Ok(())
  }

  /// An addition, a replacement when its directory remembers the path deleted (`add_directory`, `add_file`).
  fn add(&mut self, path: &str, row: &ChangedPath) -> Result<bool, MigrationRefusal> {
    let parent = self.stack.last_mut().ok_or(MigrationRefusal::Corrupt)?;
    let action = if parent.deleted.remove(path) {
      NodeAction::Replace
    } else {
      NodeAction::Add
    };
    let copy = row
      .copy_from
      .as_ref()
      .map(|source| (relative(&source.path).to_owned(), source.revision));
    self.dump_node(path, row.kind, action, copy.as_ref())?;
    if row.kind == NodeKind::Directory {
      self.stack.push(directory(copy, true));
      return Ok(true);
    }
    Ok(false)
  }

  /// A modification: a file's record at once (`open_file`); a directory opened (`open_root`, `open_directory`) and
  /// its record written when its properties changed and it is not yet written (`change_dir_prop`).
  fn modify(&mut self, path: &str, row: &ChangedPath) -> Result<bool, MigrationRefusal> {
    if row.kind == NodeKind::File {
      let compare = self.inherited(path);
      self.change(path, row.kind, compare.as_ref())?;
      return Ok(false);
    }
    if self.stack.is_empty() {
      self.stack.push(directory(None, false));
    } else {
      self.open(path);
    }
    if row.properties.is_some() {
      let top = self.stack.last().ok_or(MigrationRefusal::Corrupt)?;
      if !top.written {
        let compare = top.compare.clone();
        self.change(path, row.kind, compare.as_ref())?;
      }
      if let Some(top) = self.stack.last_mut() {
        top.written = true;
      }
    }
    Ok(true)
  }

  /// A change record: compared with `compare`, or with the path in the revision before.
  fn change(
    &mut self,
    path: &str,
    kind: NodeKind,
    compare: Option<&Place>,
  ) -> Result<(), MigrationRefusal> {
    let now = self.node_at(path, self.revision)?;
    let before = match compare {
      Some((source, revision)) => self.node_at(source, *revision)?,
      None => self.node_at(path, self.revision.saturating_sub(1))?,
    };
    let (properties, text) = differences(kind, &before, &now);
    let header = self.header(path, kind, NodeAction::Change, None, (properties, text), &now)?;
    self.record_with_sections(&header, &now)
  }

  /// An addition or a replacement record (`dump_node`'s `add` and `replace`): a copy compared with its source, a new
  /// node written whole.
  fn dump_node(
    &mut self,
    path: &str,
    kind: NodeKind,
    action: NodeAction,
    copy: Option<&Place>,
  ) -> Result<(), MigrationRefusal> {
    let now = self.node_at(path, self.revision)?;
    let (sections, copy_from) = match copy {
      Some((source, revision)) => {
        let before = self.node_at(source, *revision)?;
        let copy_from = copy_header(kind, source, *revision, &before);
        (differences(kind, &before, &now), Some(copy_from))
      }
      None => ((true, kind == NodeKind::File), None),
    };
    let header = self.header(path, kind, action, copy_from, sections, &now)?;
    self.record_with_sections(&header, &now)
  }

  /// A node record's header with the sections decided.
  fn header(
    &self,
    path: &str,
    kind: NodeKind,
    action: NodeAction,
    copy_from: Option<CopyFrom>,
    (with_properties, with_text): (bool, bool),
    now: &Facts,
  ) -> Result<NodeHeader, MigrationRefusal> {
    let properties = if with_properties {
      let length = properties_len(&self.repository.properties(now.properties)?)?;
      Some(PropertySection {
        delta: false,
        length,
      })
    } else {
      None
    };
    let text = with_text.then(|| TextSection {
      delta: None,
      md5: now.text.map(|text| text.md5),
      sha1: now.text.map(|text| text.sha1),
      length: now.text.map_or(0, |text| text.size),
    });
    Ok(NodeHeader {
      path: path.as_bytes().to_vec(),
      kind: Some(kind),
      action,
      copy_from,
      properties,
      text,
    })
  }

  /// Writes a node record and its sections: the node's whole property list, then its whole text.
  fn record_with_sections(&mut self, header: &NodeHeader, now: &Facts) -> Result<(), MigrationRefusal> {
    self.record(header)?;
    if header.properties.is_some() {
      let properties = self.repository.properties(now.properties)?;
      self.stream.properties(&properties)?;
    }
    if header.text.is_some() {
      let bytes = match now.text {
        Some(text) => self
          .repository
          .content(&text.identity)
          .ok_or(MigrationRefusal::Corrupt)?,
        None => &[],
      };
      if !bytes.is_empty() {
        self.stream.event(&Event::Text(bytes))?;
      }
      self.stream.event(&Event::TextEnd)?;
    }
    Ok(())
  }

  /// Writes a node record's header.
  fn record(&mut self, header: &NodeHeader) -> Result<(), MigrationRefusal> {
    self.stream.event(&Event::Node(header.clone()))
  }

  /// The node at the relative path `path` in `revision`, which the repository must hold.
  fn node_at(&self, path: &str, revision: Revision) -> Result<Facts, MigrationRefusal> {
    node(self.repository, &absolute(path), revision)?.ok_or(MigrationRefusal::Corrupt)
  }
}

/// A directory the drive opens.
fn directory(compare: Option<Place>, written: bool) -> Directory {
  Directory {
    compare,
    written,
    deleted: BTreeSet::new(),
  }
}

/// Whether a node's property list and, for a file, its text differ from what it is compared with: equal content
/// identities are FSFS's shared representations (`svn_fs_props_changed`, `svn_fs_contents_changed`).
fn differences(kind: NodeKind, before: &Facts, now: &Facts) -> (bool, bool) {
  let text_identity = |facts: &Facts| facts.text.map(|text| text.identity);
  (
    before.properties != now.properties,
    kind == NodeKind::File && text_identity(before) != text_identity(now),
  )
}

/// A copy's source headers, with the source's text checksums for a file whose source has a text
/// (`dump_node`: `svn_fs_file_checksum` without `force` names none for a file with no representation).
fn copy_header(kind: NodeKind, source: &str, revision: Revision, before: &Facts) -> CopyFrom {
  let text = before.text.filter(|_| kind == NodeKind::File);
  CopyFrom {
    revision,
    path: source.as_bytes().to_vec(),
    md5: text.map(|text| text.md5),
    sha1: text.map(|text| text.sha1),
  }
}

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
//! incremental is not offered. Format 3 (`--deltas`) writes a change's or a copy's property section as a change to its
//! comparison's list (`svn_hash_write_incremental`: what is set or changed in name order, then what is deleted in name
//! order) and every text as the windows of Subversion's deltifier ([`crate::svndiff::deltas`], A-652) against its
//! comparison's text, the empty text for an addition without history, svndiff version 0 (`store_delta`), naming the
//! base's checksums where it has a text.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use super::MigrationRefusal;
use super::paths::{
  absolute, basename, compare_paths, dirname, join, longest_ancestor, names, relative,
};
use super::records::{Facts, Properties, Repository, node};
use crate::dump::{
  CopyFrom, DeltaBase, Event, Format, NodeAction, NodeHeader, Property, PropertySection,
  RevisionHeader, TextSection, Writer, section_len,
};
use crate::index::{Action, ChangedPath, NodeKind, Revision};
use crate::svndiff::{Compression, Encoder, HEADER_LEN, Version, WINDOW_LIMIT, Window, deltas};

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
  /// The stream's format.
  format: Format,
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
  /// `svn_repos_dump_fs4` refuses them.
  pub fn new(
    repository: &Repository,
    first: Revision,
    last: Revision,
    format: Format,
  ) -> Result<Self, MigrationRefusal> {
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
      format,
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
      stream.event(&Event::Format(self.format))?;
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
      compare_paths(
        relative(&first.path).as_bytes(),
        relative(&second.path).as_bytes(),
      )
    });
    let mut drive = Drive {
      repository,
      revision,
      deltas: deltas_room(self.format)?,
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
  /// The deltifier's window room when the stream carries deltas (format 3); none for whole texts and lists.
  deltas: Option<NonZeroUsize>,
  /// Where its records go.
  stream: Stream<'a>,
  /// The directories open, the root first.
  stack: Vec<Directory>,
  /// The last path visited, or its directory (`path_driver.c`'s `last_path`).
  last: Option<String>,
}

impl<'a> Drive<'a> {
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
    let sections = self.sections(differences(kind, &before, &now), Some(&before), &now)?;
    let header = header(path, kind, NodeAction::Change, None, &sections, &now)?;
    self.record_with_sections(&header, sections)
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
    let (before, copy_from, wanted) = match copy {
      Some((source, revision)) => {
        let before = self.node_at(source, *revision)?;
        let copy_from = copy_header(kind, source, *revision, &before);
        let wanted = differences(kind, &before, &now);
        (Some(before), Some(copy_from), wanted)
      }
      None => (None, None, (true, kind == NodeKind::File)),
    };
    let sections = self.sections(wanted, before.as_ref(), &now)?;
    let header = header(path, kind, action, copy_from, &sections, &now)?;
    self.record_with_sections(&header, sections)
  }

  /// The sections a record carries, as `wanted` says: its property list and its text, each whole, or (format 3) as a
  /// change to `before`, what the record compares with, when it has one.
  fn sections(
    &self,
    (with_properties, with_text): (bool, bool),
    before: Option<&Facts>,
    now: &Facts,
  ) -> Result<Sections<'a>, MigrationRefusal> {
    let properties = if with_properties {
      Some(self.property_plan(before, now)?)
    } else {
      None
    };
    let text = if with_text {
      Some(self.text_plan(before, now)?)
    } else {
      None
    };
    Ok(Sections { properties, text })
  }

  /// A property section: the node's whole list, or (format 3, against a comparison) what changed from `before`'s.
  fn property_plan(
    &self,
    before: Option<&Facts>,
    now: &Facts,
  ) -> Result<PropertyPlan, MigrationRefusal> {
    let list = self.repository.properties(now.properties)?;
    match (self.deltas, before) {
      (Some(_), Some(before)) => {
        let base = self.repository.properties(before.properties)?;
        Ok(PropertyPlan {
          delta: true,
          entries: changed_entries(&list, &base),
        })
      }
      _ => Ok(PropertyPlan {
        delta: false,
        entries: list.into_iter().map(|(name, value)| (name, Some(value))).collect(),
      }),
    }
  }

  /// A text section: the node's whole text, or (format 3) the deltifier's windows against `before`'s text, the empty
  /// text when there is no comparison or it has none (`store_delta`).
  fn text_plan(&self, before: Option<&Facts>, now: &Facts) -> Result<TextPlan<'a>, MigrationRefusal> {
    let target = self.text_bytes(now)?;
    let Some(room) = self.deltas else {
      return Ok(TextPlan::Whole(target));
    };
    let base_text = before.and_then(|before| before.text);
    let source = match before {
      Some(before) => self.text_bytes(before)?,
      None => &[],
    };
    let windows: Vec<Window> = deltas(source, target, room)
      .map_err(MigrationRefusal::Delta)?
      .collect();
    let length = encoded_len(&windows)?;
    Ok(TextPlan::Delta {
      base: DeltaBase {
        md5: base_text.map(|text| text.md5),
        sha1: base_text.map(|text| text.sha1),
      },
      windows,
      length,
    })
  }

  /// The bytes of a node's text: none for a node without one.
  fn text_bytes(&self, facts: &Facts) -> Result<&'a [u8], MigrationRefusal> {
    match facts.text {
      Some(text) => self
        .repository
        .content(&text.identity)
        .ok_or(MigrationRefusal::Corrupt),
      None => Ok(&[]),
    }
  }

  /// Writes a node record and its sections: its property entries, then its text whole or as its delta's windows.
  fn record_with_sections(
    &mut self,
    header: &NodeHeader,
    sections: Sections<'_>,
  ) -> Result<(), MigrationRefusal> {
    self.record(header)?;
    if let Some(properties) = sections.properties {
      for (name, value) in &properties.entries {
        let entry = match value {
          Some(value) => Property::Set { name, value },
          None => Property::Delete { name },
        };
        self.stream.event(&Event::Property(entry))?;
      }
      self.stream.event(&Event::PropertiesEnd)?;
    }
    match sections.text {
      Some(TextPlan::Whole(bytes)) => {
        if !bytes.is_empty() {
          self.stream.event(&Event::Text(bytes))?;
        }
        self.stream.event(&Event::TextEnd)
      }
      Some(TextPlan::Delta { windows, .. }) => {
        self.stream.event(&Event::Delta(Version::Zero))?;
        for window in windows {
          self.stream.event(&Event::Window(window))?;
        }
        self.stream.event(&Event::TextEnd)
      }
      None => Ok(()),
    }
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

/// A record's property section as it is written.
struct PropertyPlan {
  /// Whether it is a change to the comparison's list (`Prop-delta: true`).
  delta: bool,
  /// Its entries in order: a property set to a value, or (in a change) deleted.
  entries: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

/// A record's text as it is written.
enum TextPlan<'a> {
  /// Whole.
  Whole(&'a [u8]),
  /// As the deltifier's windows against a base the header names by its checksums, `length` bytes of svndiff.
  Delta {
    /// The base's checksums.
    base: DeltaBase,
    /// The windows.
    windows: Vec<Window>,
    /// The svndiff's length.
    length: u64,
  },
}

/// What a record carries after its headers.
struct Sections<'a> {
  /// Its property section.
  properties: Option<PropertyPlan>,
  /// Its text.
  text: Option<TextPlan<'a>>,
}

/// The deltifier's window room for a stream of `format`: Subversion's, `SVN_DELTA_WINDOW_SIZE`, for format 3; none for
/// format 2, which carries every text whole.
fn deltas_room(format: Format) -> Result<Option<NonZeroUsize>, MigrationRefusal> {
  if format == Format::Two {
    return Ok(None);
  }
  let room = usize::try_from(WINDOW_LIMIT).map_err(|_| MigrationRefusal::Corrupt)?;
  NonZeroUsize::new(room).map(Some).ok_or(MigrationRefusal::Corrupt)
}

/// The svndiff length of `windows`: the header and each window as svndiff version 0 writes it.
fn encoded_len(windows: &[Window]) -> Result<u64, MigrationRefusal> {
  let mut encoder = Encoder::new(Version::Zero);
  let mut scratch = Vec::new();
  let mut length = u64::try_from(HEADER_LEN).map_err(|_| MigrationRefusal::Corrupt)?;
  for window in windows {
    scratch.clear();
    encoder
      .window(window, Compression::Stored, &mut scratch)
      .map_err(MigrationRefusal::Delta)?;
    let bytes = u64::try_from(scratch.len()).map_err(|_| MigrationRefusal::Corrupt)?;
    length = length.checked_add(bytes).ok_or(MigrationRefusal::Corrupt)?;
  }
  Ok(length)
}

/// What changed from `base` to `list`, as `svn_hash_write_incremental` writes it (`hash.c` 266-315): every property
/// set or changed, in name order, then every property deleted, in name order.
fn changed_entries(list: &Properties, base: &Properties) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
  let set = list
    .iter()
    .filter(|(name, value)| base.get(*name) != Some(*value))
    .map(|(name, value)| (name.clone(), Some(value.clone())));
  let deleted = base
    .keys()
    .filter(|name| !list.contains_key(*name))
    .map(|name| (name.clone(), None));
  set.chain(deleted).collect()
}

/// A node record's header for `sections`.
fn header(
  path: &str,
  kind: NodeKind,
  action: NodeAction,
  copy_from: Option<CopyFrom>,
  sections: &Sections<'_>,
  now: &Facts,
) -> Result<NodeHeader, MigrationRefusal> {
  let properties = match &sections.properties {
    Some(plan) => Some(PropertySection {
      delta: plan.delta,
      length: section_len(plan.entries.iter().map(|(name, value)| match value {
        Some(value) => Property::Set { name, value },
        None => Property::Delete { name },
      }))
      .ok_or(MigrationRefusal::Corrupt)?,
    }),
    None => None,
  };
  let text = sections.text.as_ref().map(|plan| {
    let (delta, length) = match plan {
      TextPlan::Whole(bytes) => (None, u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
      TextPlan::Delta { base, length, .. } => (Some(*base), *length),
    };
    TextSection {
      delta,
      md5: now.text.map(|text| text.md5),
      sha1: now.text.map(|text| text.sha1),
      length,
    }
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

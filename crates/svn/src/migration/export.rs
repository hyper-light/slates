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
//!
//! **Bounded calls** (A-652's slicing). One call of [`Exporter::export`] writes a revision's record, or one changed
//! path's records up to its text, or one window of that text (a whole text's next [`WINDOW_LIMIT`] bytes, or the
//! deltifier's next window, taken up again from [`DeltasAt`]), or the closing of the revision's open directories. A
//! record's header names its text's length first: a whole text's is its own (`index::Text::size`), so it is walked once;
//! a delta's is its svndiff's, which nothing keeps, so a first walk measures it a window a call keeping no window,
//! before the record's call (the changed path's step run first on copies, its writes dropped, to learn which delta its
//! record carries), and a second writes it.

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
use crate::index::{Action, ChangedPath, NodeKind, Revision, Text};
use crate::svndiff::{
  Compression, DeltasAt, Encoder, HEADER_LEN, Version, WINDOW_LIMIT, deltas, deltas_at,
};

/// What one call of [`Exporter::export`] wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exporting {
  /// A part of this revision's records was appended: its revision record (the stream's version and UUID records
  /// before the range's first), one changed path's records, or the deletions its open directories wrote as they closed.
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
  /// The revision being written, between its calls; none before its revision record.
  drive: Option<Pending>,
}

/// A revision's replay between calls of [`Exporter::export`]: its changed paths in the path driver's order, how many
/// are written, and the drive's open directories and last path.
#[derive(Clone, Debug)]
struct Pending {
  /// The changed paths, in `svn_path_compare_paths` order.
  rows: Vec<ChangedPath>,
  /// How many are written.
  at: usize,
  /// The directories open, the root first.
  stack: Vec<Directory>,
  /// The last path visited, or its directory.
  last: Option<String>,
  /// The text of the record written last, while it is being written a window a call.
  text: Option<Texting>,
  /// The next changed path's delta being measured a window a call (format 3), before its record is written.
  measuring: Option<Measuring>,
  /// The next changed path's delta's svndiff length, once measured.
  measured: Option<u64>,
}

/// A delta's svndiff length being measured a window a call, no window kept: its texts, where the deltifier stands, and
/// the length so far (the svndiff header's and each window's made).
#[derive(Clone, Copy, Debug)]
struct Measuring {
  /// The comparison's text; none for the empty text.
  source: Option<Text>,
  /// The record's text; none for the empty text.
  target: Option<Text>,
  /// The deltifier's position; none before its first window.
  at: Option<DeltasAt>,
  /// The length so far.
  length: u64,
}

/// A record's text being written a window a call (the module documentation): its header and property section are
/// written, so the text's length was read before it (format 2: the text's own, `index::Text::size`; format 3: the
/// svndiff's, measured by a walk of its windows that kept none).
#[derive(Clone, Copy, Debug)]
enum Texting {
  /// A whole text: how many of its bytes are written.
  Whole {
    /// The text; none for a node without one, whose text is empty.
    text: Option<Text>,
    /// Its bytes written.
    sent: u64,
  },
  /// A delta against `source`'s text making `target`'s: where the deltifier stands, none before its first window.
  Delta {
    /// The comparison's text; none for the empty text.
    source: Option<Text>,
    /// The record's text; none for the empty text.
    target: Option<Text>,
    /// The deltifier's position.
    at: Option<DeltasAt>,
  },
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
      drive: None,
    })
  }

  /// Appends the next part of the range's records to `out` ([`Exporting::Wrote`]), so one call's work is one changed
  /// path's, never a whole revision's; the stream's version and UUID records before the first revision's.
  ///
  /// # Errors
  /// [`MigrationRefusal::Corrupt`] for records that name what the repository does not hold, and the writer's refusal
  /// of anything it cannot write; every later call answers the same refusal, and what the refused call appended is
  /// no part of a stream.
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
    let ended = match self.drive.is_some() {
      true => self.step(repository, revision, out),
      false => self.begin(repository, revision, out),
    };
    match ended {
      Err(refusal) => {
        self.refused = Some(refusal.clone());
        Err(refusal)
      }
      Ok(false) => Ok(Exporting::Wrote(revision)),
      Ok(true) => {
        self.drive = None;
        self.next = if revision < self.last {
          revision.checked_add(1)
        } else {
          None
        };
        Ok(Exporting::Wrote(revision))
      }
    }
  }

  /// Writes `revision`'s record and its properties, the stream's first records before the range's first revision, and
  /// takes its changed paths in the path driver's order; whether the revision is written (revision 0 has no changes).
  fn begin(
    &mut self,
    repository: &Repository,
    revision: Revision,
    out: &mut Vec<u8>,
  ) -> Result<bool, MigrationRefusal> {
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
      return Ok(true);
    }
    let mut rows: Vec<ChangedPath> = repository
      .index()
      .changes(revision)
      .map_err(MigrationRefusal::Index)?
      .to_vec();
    rows.sort_by(|first, second| {
      compare_paths(
        relative(&first.path).as_bytes(),
        relative(&second.path).as_bytes(),
      )
    });
    self.drive = Some(Pending {
      rows,
      at: 0,
      stack: Vec::new(),
      last: None,
      text: None,
      measuring: None,
      measured: None,
    });
    Ok(false)
  }

  /// Writes the pending revision's next changed path through the path driver, or, past its last, closes the
  /// directories still open; whether the revision is written.
  fn step(
    &mut self,
    repository: &Repository,
    revision: Revision,
    out: &mut Vec<u8>,
  ) -> Result<bool, MigrationRefusal> {
    let deltas = deltas_room(self.format)?;
    let Exporter { writer, drive, .. } = self;
    let pending = drive.as_mut().ok_or(MigrationRefusal::Corrupt)?;
    if let Some(texting) = pending.text {
      let mut stream = Stream { writer, out };
      pending.text = write_text(repository, &mut stream, texting)?;
      return Ok(false);
    }
    if let Some(measuring) = pending.measuring {
      match measure_window(repository, measuring)? {
        Some(more) => pending.measuring = Some(more),
        None => {
          pending.measuring = None;
          pending.measured = Some(measuring.length);
        }
      }
      return Ok(false);
    }
    let row = pending.rows.get(pending.at).cloned();
    // Format 3: the changed path's step run first on copies, its writes dropped, to learn whether its record carries a
    // delta, whose svndiff length its header names first; that length is measured by the calls after this one.
    if let (Some(row), Some(_), None) = (&row, deltas, pending.measured) {
      let mut copy = writer.clone();
      let mut dropped = Vec::new();
      let mut dry = Drive {
        repository,
        revision,
        deltas,
        stream: Stream {
          writer: &mut copy,
          out: &mut dropped,
        },
        stack: pending.stack.clone(),
        last: pending.last.clone(),
        text: None,
        measured: Measure::Wanted(std::cell::Cell::new(None)),
      };
      dry.step(row)?;
      if let Measure::Wanted(wanted) = &dry.measured
        && let Some((source, target)) = wanted.get()
      {
        pending.measuring = Some(Measuring {
          source,
          target,
          at: None,
          length: u64::try_from(HEADER_LEN).map_err(|_| MigrationRefusal::Corrupt)?,
        });
        return Ok(false);
      }
    }
    let mut drive = Drive {
      repository,
      revision,
      deltas,
      stream: Stream { writer, out },
      stack: std::mem::take(&mut pending.stack),
      last: pending.last.take(),
      text: None,
      measured: Measure::Given(pending.measured.take()),
    };
    let Some(row) = row else {
      drive.finish()?;
      return Ok(true);
    };
    drive.step(&row)?;
    pending.at = pending.at.saturating_add(1);
    pending.stack = drive.stack;
    pending.last = drive.last;
    pending.text = drive.text;
    Ok(false)
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
#[derive(Clone, Debug)]
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
  /// The text the record it wrote last owes, written by the calls after it.
  text: Option<Texting>,
  /// A delta's svndiff length: given, measured by the calls before this step; or wanted, by a step run only to learn
  /// which delta its record carries.
  measured: Measure,
}

/// A changed path's delta length, for its record's header (format 3).
enum Measure {
  /// Measured before the step, none where the step writes no delta.
  Given(Option<u64>),
  /// The step is run to learn it: the texts of the delta it writes, set once it plans one.
  Wanted(std::cell::Cell<Option<(Option<Text>, Option<Text>)>>),
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
  ) -> Result<Sections, MigrationRefusal> {
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
        entries: list
          .into_iter()
          .map(|(name, value)| (name, Some(value)))
          .collect(),
      }),
    }
  }

  /// A text section: the node's whole text, its length the record's (one walk, the writing); or (format 3) the
  /// deltifier's windows against `before`'s text, the empty text when there is no comparison or it has none
  /// (`store_delta`), their svndiff's length measured before the step by calls of their own (the first of two walks).
  fn text_plan(&self, before: Option<&Facts>, now: &Facts) -> Result<TextPlan, MigrationRefusal> {
    if self.deltas.is_none() {
      return Ok(TextPlan::Whole(now.text));
    }
    let base_text = before.and_then(|before| before.text);
    let length = match &self.measured {
      Measure::Given(Some(length)) => *length,
      // A stand-in its writes take (the svndiff header the record writes before its windows), dropped with them.
      Measure::Wanted(wanted) => {
        wanted.set(Some((base_text, now.text)));
        u64::try_from(HEADER_LEN).map_err(|_| MigrationRefusal::Corrupt)?
      }
      // Every format 3 step is run first to learn its delta, which is measured before it.
      Measure::Given(None) => return Err(MigrationRefusal::Corrupt),
    };
    Ok(TextPlan::Delta {
      base: DeltaBase {
        md5: base_text.map(|text| text.md5),
        sha1: base_text.map(|text| text.sha1),
      },
      length,
      source: base_text,
      target: now.text,
    })
  }

  /// Writes a node record and its sections: its property entries, then its text whole or as its delta's windows.
  fn record_with_sections(
    &mut self,
    header: &NodeHeader,
    sections: Sections,
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
    // The text itself is written by the calls after this one, a window a call (`write_text`).
    self.text = match sections.text {
      Some(TextPlan::Whole(text)) => Some(Texting::Whole { text, sent: 0 }),
      Some(TextPlan::Delta { source, target, .. }) => {
        self.stream.event(&Event::Delta(Version::Zero))?;
        Some(Texting::Delta {
          source,
          target,
          at: None,
        })
      }
      None => None,
    };
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

/// A record's property section as it is written.
struct PropertyPlan {
  /// Whether it is a change to the comparison's list (`Prop-delta: true`).
  delta: bool,
  /// Its entries in order: a property set to a value, or (in a change) deleted.
  entries: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

/// A record's text as it is written.
enum TextPlan {
  /// Whole: the text, none for the empty one.
  Whole(Option<Text>),
  /// As the deltifier's windows against a base the header names by its checksums, `length` bytes of svndiff.
  Delta {
    /// The base's checksums.
    base: DeltaBase,
    /// The svndiff's length.
    length: u64,
    /// The base's text, none for the empty one.
    source: Option<Text>,
    /// The record's text, none for the empty one.
    target: Option<Text>,
  },
}

/// What a record carries after its headers.
struct Sections {
  /// Its property section.
  properties: Option<PropertyPlan>,
  /// Its text.
  text: Option<TextPlan>,
}

/// The deltifier's window room for a stream of `format`: Subversion's, `SVN_DELTA_WINDOW_SIZE`, for format 3; none for
/// format 2, which carries every text whole.
fn deltas_room(format: Format) -> Result<Option<NonZeroUsize>, MigrationRefusal> {
  if format == Format::Two {
    return Ok(None);
  }
  window_room().map(Some)
}

/// The most bytes of a text one call writes: Subversion's window, `SVN_DELTA_WINDOW_SIZE` ([`WINDOW_LIMIT`]), a delta
/// window's most, so a whole text goes in pieces of the room a delta's window has.
fn window_room() -> Result<NonZeroUsize, MigrationRefusal> {
  let room = usize::try_from(WINDOW_LIMIT).map_err(|_| MigrationRefusal::Corrupt)?;
  NonZeroUsize::new(room).ok_or(MigrationRefusal::Corrupt)
}

/// The bytes of `text`, the empty text for none.
fn bytes_of(repository: &Repository, text: Option<Text>) -> Result<&[u8], MigrationRefusal> {
  match text {
    Some(text) => repository
      .content(&text.identity)
      .ok_or(MigrationRefusal::Corrupt),
    None => Ok(&[]),
  }
}

/// Writes the next window of a record's text, or its end: the texting still owed, none once its end is written.
fn write_text(
  repository: &Repository,
  stream: &mut Stream<'_>,
  texting: Texting,
) -> Result<Option<Texting>, MigrationRefusal> {
  let room = window_room()?;
  match texting {
    Texting::Whole { text, sent } => {
      let bytes = bytes_of(repository, text)?;
      let start = usize::try_from(sent).map_err(|_| MigrationRefusal::Corrupt)?;
      let rest = bytes.get(start..).unwrap_or_default();
      let piece = rest.get(..room.get()).unwrap_or(rest);
      if !piece.is_empty() {
        stream.event(&Event::Text(piece))?;
      }
      let sent = start.saturating_add(piece.len());
      if sent >= bytes.len() {
        stream.event(&Event::TextEnd)?;
        return Ok(None);
      }
      Ok(Some(Texting::Whole {
        text,
        sent: u64::try_from(sent).map_err(|_| MigrationRefusal::Corrupt)?,
      }))
    }
    Texting::Delta { source, target, at } => {
      let (from, to) = (bytes_of(repository, source)?, bytes_of(repository, target)?);
      let mut made = match at {
        Some(at) => deltas_at(from, to, room, at),
        None => deltas(from, to, room),
      }
      .map_err(MigrationRefusal::Delta)?;
      match made.next() {
        Some(window) => {
          stream.event(&Event::Window(window))?;
          Ok(Some(Texting::Delta {
            source,
            target,
            at: Some(made.at()),
          }))
        }
        None => {
          stream.event(&Event::TextEnd)?;
          Ok(None)
        }
      }
    }
  }
}

/// Measures the next window of a delta, adding its length as svndiff version 0 writes it, keeping no window: the
/// measuring still owed, none once the deltifier has made its last window.
fn measure_window(
  repository: &Repository,
  measuring: Measuring,
) -> Result<Option<Measuring>, MigrationRefusal> {
  let room = window_room()?;
  let (from, to) = (
    bytes_of(repository, measuring.source)?,
    bytes_of(repository, measuring.target)?,
  );
  let mut made = match measuring.at {
    Some(at) => deltas_at(from, to, room, at),
    None => deltas(from, to, room),
  }
  .map_err(MigrationRefusal::Delta)?;
  let Some(window) = made.next() else {
    return Ok(None);
  };
  let mut scratch = Vec::new();
  Encoder::new(Version::Zero)
    .window(&window, Compression::Stored, &mut scratch)
    .map_err(MigrationRefusal::Delta)?;
  let bytes = u64::try_from(scratch.len()).map_err(|_| MigrationRefusal::Corrupt)?;
  Ok(Some(Measuring {
    at: Some(made.at()),
    length: measuring
      .length
      .checked_add(bytes)
      .ok_or(MigrationRefusal::Corrupt)?,
    ..measuring
  }))
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
  sections: &Sections,
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
      TextPlan::Whole(text) => (None, text.map_or(0, |text| text.size)),
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

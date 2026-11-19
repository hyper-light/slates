//! What the migration tests share: a repository as `CreateRepository` leaves one, an import fed in pieces, an export of
//! a range, a repository's history as one value to compare, and dumps written record by record through the dump
//! codec's writer.

use std::collections::BTreeMap;

use crate::dump::{
  Admission, CopyFrom, DeltaBase, Digests, Event, Format, Md5, NodeAction, NodeHeader, Property,
  PropertySection, RevisionHeader, Sha1, TextSection, Uuid, Writer, section_len,
};
use crate::index::{ChangedPath, ContentIdentity, NodeKind, PropertyList};
use crate::migration::{
  Exporter, Exporting, Importer, Importing, MigrationRefusal, Properties, Repository, Room,
};
use crate::svndiff::{Compression, Encoder, Version, Window};

/// A room no test history comes near.
/// Shape: 2^20 index entries and 2^30 bytes, past every vector and every generated history.
pub(super) const ROOMY: Room = Room {
  entries: 1 << 20,
  bytes: 1 << 30,
};

/// An admission past the longest line, property entry and svndiff window of every dump the tests read.
/// Shape: 2^20 bytes, past svndiff's largest window (`svndiff::WINDOW_LIMIT`, 102,400 bytes, and its instructions).
pub(super) const ADMISSION: Admission = Admission {
  buffer_bytes: 1 << 20,
};

/// The UUID a repository is created with, before a stream names it.
pub(super) const CREATED_UUID: &[u8] = b"00000000-0000-4000-8000-000000000628";

/// The UUID the golden vectors' repository carries (`make-fixtures.sh`).
pub(super) const VECTOR_UUID: &[u8] = b"7bf7a5ef-cabf-0310-b7d4-93df341afa7e";

/// The time a repository is created at.
pub(super) const CREATED_DATE: &[u8] = b"2026-11-19T00:00:00.000000Z";

/// A repository as `CreateRepository` leaves one, within a room no test comes near.
pub(super) fn created() -> Repository {
  created_within(ROOMY)
}

/// A repository as `CreateRepository` leaves one, within `room`.
pub(super) fn created_within(room: Room) -> Repository {
  let uuid = Uuid::parse(CREATED_UUID).expect("a UUID");
  Repository::new(uuid, CREATED_DATE, room).expect("a repository within its room")
}

/// Imports `dump` into `repository`, its bytes arriving `piece` at a time.
pub(super) fn import(
  repository: &mut Repository,
  dump: &[u8],
  piece: usize,
) -> Result<(), MigrationRefusal> {
  import_with(&mut Importer::new(ADMISSION), repository, dump, piece)
}

/// Imports `dump` into `repository` through `importer`, its bytes arriving `piece` at a time.
pub(super) fn import_with(
  importer: &mut Importer,
  repository: &mut Repository,
  dump: &[u8],
  piece: usize,
) -> Result<(), MigrationRefusal> {
  let mut held: Vec<u8> = Vec::new();
  let mut next = 0;
  let mut needed = 1;
  loop {
    while held.len() < needed && next < dump.len() {
      let end = dump.len().min(next + piece.max(1));
      held.extend_from_slice(&dump[next..end]);
      next = end;
    }
    match importer.import(repository, &held, next == dump.len())? {
      Importing::Done => return Ok(()),
      Importing::NeedMore {
        consumed,
        needed: more,
      } => {
        held.drain(..consumed);
        needed = more.max(held.len() + 1);
      }
    }
  }
}

/// Imports the whole of `dump` into `repository` in one call.
pub(super) fn import_whole(
  repository: &mut Repository,
  dump: &[u8],
) -> Result<(), MigrationRefusal> {
  import(repository, dump, dump.len())
}

/// Exports revisions `first` to `last` of `repository` in format 2.
pub(super) fn export(
  repository: &Repository,
  first: u64,
  last: u64,
) -> Result<Vec<u8>, MigrationRefusal> {
  export_in(repository, first, last, Format::Two)
}

/// Exports revisions `first` to `last` of `repository` in `format`.
pub(super) fn export_in(
  repository: &Repository,
  first: u64,
  last: u64,
  format: Format,
) -> Result<Vec<u8>, MigrationRefusal> {
  let mut exporter = Exporter::new(repository, first, last, format)?;
  let mut out = Vec::new();
  while exporter.export(repository, &mut out)? != Exporting::Done {}
  Ok(out)
}

/// Exports every revision of `repository` in format 2.
pub(super) fn export_all(repository: &Repository) -> Result<Vec<u8>, MigrationRefusal> {
  export(repository, 0, repository.youngest())
}

/// Exports every revision of `repository` in format 3, every text a delta.
pub(super) fn export_deltas(repository: &Repository) -> Result<Vec<u8>, MigrationRefusal> {
  export_in(repository, 0, repository.youngest(), Format::Three)
}

/// One revision as a value: its properties, its transaction's name and its changed paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RevisionView {
  /// Its properties.
  pub(super) properties: Properties,
  /// Its transaction's name.
  pub(super) transaction: Option<String>,
  /// Its changed paths.
  pub(super) changes: Vec<ChangedPath>,
}

/// A repository's history as one value: its UUID, every revision, and every text and property list a changed path
/// names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct History {
  /// The UUID.
  pub(super) uuid: Vec<u8>,
  /// Every revision.
  pub(super) revisions: Vec<RevisionView>,
  /// The contents the changed paths name.
  pub(super) contents: BTreeMap<ContentIdentity, Vec<u8>>,
}

/// `repository`'s history.
pub(super) fn history(repository: &Repository) -> History {
  let mut revisions = Vec::new();
  let mut contents = BTreeMap::new();
  for revision in 0..=repository.youngest() {
    let changes = repository
      .index()
      .changes(revision)
      .expect("a revision")
      .to_vec();
    for change in &changes {
      for identity in named(change) {
        let bytes = repository
          .content(&identity)
          .expect("a content the index names");
        contents.insert(identity, bytes.to_vec());
      }
    }
    revisions.push(RevisionView {
      properties: repository
        .revision_properties(revision)
        .expect("a revision's properties")
        .clone(),
      transaction: repository
        .index()
        .transaction(revision)
        .expect("a revision")
        .map(str::to_owned),
      changes,
    });
  }
  History {
    uuid: repository.uuid().as_bytes().to_vec(),
    revisions,
    contents,
  }
}

/// The contents a changed path names.
fn named(change: &ChangedPath) -> Vec<ContentIdentity> {
  let text = change.text.map(|text| text.identity);
  let properties = match change.properties {
    Some(PropertyList::Held { identity, .. }) => Some(identity),
    _ => None,
  };
  text.into_iter().chain(properties).collect()
}

/// A property list from `entries`.
pub(super) fn properties(entries: &[(&str, &str)]) -> Properties {
  entries
    .iter()
    .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
    .collect()
}

/// The MD5 and SHA-1 of `bytes`.
pub(super) fn digests(bytes: &[u8]) -> (Md5, Sha1) {
  let mut digests = Digests::new().expect("a digest context");
  digests.update(bytes).expect("bytes digested");
  digests.finish().expect("digests")
}

/// A node record to write.
#[derive(Clone, Debug)]
pub(super) struct Node {
  /// Its path, as the stream gives it.
  path: Vec<u8>,
  /// Its stated kind.
  kind: Option<NodeKind>,
  /// Its action.
  action: NodeAction,
  /// Its copy source's revision and path.
  copy: Option<(u64, Vec<u8>)>,
  /// Its whole property list.
  properties: Option<Properties>,
  /// Its whole text.
  text: Option<Vec<u8>>,
}

impl Node {
  /// A record of `action` at `path`, of no stated kind and with no section.
  fn of(path: &str, action: NodeAction) -> Self {
    Self {
      path: path.as_bytes().to_vec(),
      kind: None,
      action,
      copy: None,
      properties: None,
      text: None,
    }
  }

  /// An addition of a `kind` at `path`.
  pub(super) fn add(path: &str, kind: NodeKind) -> Self {
    Self::of(path, NodeAction::Add).kind(kind)
  }

  /// A replacement at `path` by a `kind`.
  pub(super) fn replace(path: &str, kind: NodeKind) -> Self {
    Self::of(path, NodeAction::Replace).kind(kind)
  }

  /// A change at `path`.
  pub(super) fn change(path: &str) -> Self {
    Self::of(path, NodeAction::Change)
  }

  /// A deletion at `path`.
  pub(super) fn delete(path: &str) -> Self {
    Self::of(path, NodeAction::Delete)
  }

  /// The record with its path as raw bytes.
  pub(super) fn raw_path(mut self, path: &[u8]) -> Self {
    self.path = path.to_vec();
    self
  }

  /// The record stating `kind`.
  pub(super) fn kind(mut self, kind: NodeKind) -> Self {
    self.kind = Some(kind);
    self
  }

  /// The record copying `source` at `revision`.
  pub(super) fn copied(mut self, revision: u64, source: &str) -> Self {
    self.copy = Some((revision, source.as_bytes().to_vec()));
    self
  }

  /// The record with a whole property section listing `properties`.
  pub(super) fn properties(mut self, properties: Properties) -> Self {
    self.properties = Some(properties);
    self
  }

  /// The record with the whole text `text`.
  pub(super) fn text(mut self, text: &[u8]) -> Self {
    self.text = Some(text.to_vec());
    self
  }
}

/// A dump written record by record through the dump codec's writer.
#[derive(Clone, Debug)]
pub(super) struct Dump {
  /// The writer.
  writer: Writer,
  /// The bytes written.
  bytes: Vec<u8>,
}

impl Dump {
  /// A dump of `format`, its UUID record naming `uuid` when given.
  pub(super) fn new(format: Format, uuid: Option<&[u8]>) -> Self {
    let mut dump = Self {
      writer: Writer::new(),
      bytes: Vec::new(),
    };
    dump.event(&Event::Format(format));
    if let Some(uuid) = uuid {
      dump.event(&Event::Uuid(Uuid::parse(uuid).expect("a UUID")));
    }
    dump
  }

  /// A revision record with `properties`.
  pub(super) fn revision(&mut self, number: u64, properties: &Properties) -> &mut Self {
    let length = section_len(sets(properties)).expect("a section length");
    self.event(&Event::Revision(RevisionHeader {
      number,
      properties: Some(length),
    }));
    self.entries(properties);
    self
  }

  /// A node record with whole sections.
  pub(super) fn node(&mut self, node: &Node) -> &mut Self {
    let properties = node.properties.as_ref().map(|list| PropertySection {
      delta: false,
      length: section_len(sets(list)).expect("a section length"),
    });
    let text = node.text.as_ref().map(|text| {
      let (md5, sha1) = digests(text);
      TextSection {
        delta: None,
        md5: Some(md5),
        sha1: Some(sha1),
        length: text.len() as u64,
      }
    });
    self.header(NodeHeader {
      path: node.path.clone(),
      kind: node.kind,
      action: node.action,
      copy_from: node.copy.as_ref().map(|(revision, path)| CopyFrom {
        revision: *revision,
        path: path.clone(),
        md5: None,
        sha1: None,
      }),
      properties,
      text,
    });
    if let Some(list) = &node.properties {
      self.entries(list);
    }
    if let Some(text) = &node.text {
      if !text.is_empty() {
        self.event(&Event::Text(text));
      }
      self.event(&Event::TextEnd);
    }
    self
  }

  /// A node record's header alone, its sections the caller's to write.
  pub(super) fn header(&mut self, header: NodeHeader) -> &mut Self {
    self.event(&Event::Node(header));
    self
  }

  /// A change record of the file at `path` whose text is a delta of `windows` against a base its header names by
  /// `base`, its result named by `result`.
  pub(super) fn delta(
    &mut self,
    path: &str,
    base: DeltaBase,
    windows: &[Window],
    result: (Md5, Sha1),
  ) -> &mut Self {
    let mut encoder = Encoder::new(Version::Zero);
    let mut encoded = encoder.header().to_vec();
    for window in windows {
      encoder
        .window(window, Compression::Stored, &mut encoded)
        .expect("a window the encoder takes");
    }
    self.header(NodeHeader {
      path: path.as_bytes().to_vec(),
      kind: Some(NodeKind::File),
      action: NodeAction::Change,
      copy_from: None,
      properties: None,
      text: Some(TextSection {
        delta: Some(base),
        md5: Some(result.0),
        sha1: Some(result.1),
        length: encoded.len() as u64,
      }),
    });
    self.event(&Event::Delta(Version::Zero));
    for window in windows {
      self.event(&Event::Window(window.clone()));
    }
    self.event(&Event::TextEnd);
    self
  }

  /// The bytes written.
  pub(super) fn bytes(&self) -> Vec<u8> {
    self.bytes.clone()
  }

  /// Writes one event.
  fn event(&mut self, event: &Event<'_>) {
    self
      .writer
      .write(event, &mut self.bytes)
      .expect("an event the writer takes");
  }

  /// Writes a property section's entries and its end.
  fn entries(&mut self, properties: &Properties) {
    for (name, value) in properties {
      self.event(&Event::Property(Property::Set { name, value }));
    }
    self.event(&Event::PropertiesEnd);
  }
}

/// The entries that set `properties`.
fn sets(properties: &Properties) -> impl Iterator<Item = Property<'_>> {
  properties
    .iter()
    .map(|(name, value)| Property::Set { name, value })
}

/// Revision properties of the form every test revision carries: its number in its date's microseconds and its log.
pub(super) fn revision_properties(revision: u64) -> Properties {
  let date = format!("2026-11-19T00:00:00.{revision:06}Z");
  let log = format!("r{revision}");
  properties(&[
    ("svn:author", "migration"),
    ("svn:date", &date),
    ("svn:log", &log),
  ])
}

//! The import (A-628): a dump stream, read as its bytes arrive through A-593's reader, made into the repository's
//! revisions as `svnadmin load` makes them through `load-fs-vtable.c`, each record applied to a [`Transaction`] as FSFS
//! applies the call it makes:
//! - a revision record begins a transaction on the youngest revision (`new_revision_record`) and is committed when the
//!   next revision record or the stream's end comes (`close_revision`), with exactly the stream's revision properties;
//!   revision 0's properties are set into a repository that holds no revision but 0, as `close_revision` sets them;
//! - a node record's path is read through `svn_relpath_canonicalize` (`make_node_baton`); `change` makes no call of
//!   its own, `delete` deletes, `add` makes a node or copies (`maybe_add_with_history`), `replace` deletes then adds;
//! - a whole property section removes every property the node has, then sets each it lists (`remove_node_props`,
//!   `set_node_property`), a format-3 section deletes and sets what it names, and FSFS records a property change when a
//!   call was made (`tree.c` `fs_change_node_prop`); a text section sets the text whole or applies its delta to the
//!   node's text (`set_fulltext`, `apply_textdelta`), which FSFS records as a text change.
//!
//! Where `svnadmin load` renumbers or rewrites, the import keeps: a revision is the number the stream gives it, so a
//! stream continues the repository's history or is refused ([`MigrationRefusal::OutOfSequence`]); a UUID record naming
//! another repository than a non-empty one is refused ([`MigrationRefusal::ForeignRepository`]) rather than ignored;
//! properties are kept byte for byte, where `svnadmin load` validates `svn:` properties and rewrites `svn:mergeinfo`
//! into its canonical form by default, so the history is the one its source served. Every checksum the stream carries
//! is checked: a whole text's by the reader, a delta's base and result and a copy source's text here, SHA-1 as well as
//! MD5. Everything the import holds is charged to the repository's room as it arrives.

use super::MigrationRefusal;
use super::paths::{ROOT, absolute, canonical_relpath};
use super::records::{Facts, Properties, Repository, Transaction, node};
use crate::dump::{
  Admission, CopyFrom, Digest, Digests, DumpError, Event, Md5, NodeAction, NodeHeader, Property,
  Read, Reader, RevisionHeader, Sha1, TextSection, Uuid, check_text,
};
use crate::index::{ContentIdentity, CopySource, NodeKind, PropertyList, Text, is_canonical};
use crate::svndiff::{SvndiffError, Window, apply};

/// What one call of [`Importer::import`] made of its buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Importing {
  /// The buffer ends inside the stream: the caller drops the first `consumed` bytes, then calls again with at least
  /// `needed` bytes from the new start, or with the stream's last bytes and `ended` set.
  NeedMore {
    /// The bytes, from the start of the buffer, the import is done with.
    consumed: usize,
    /// How many bytes from the new start it needs at least, never more than the admission.
    needed: usize,
  },
  /// The stream ended, and every revision it carried is in the repository.
  Done,
}

/// An import of one dump stream into one repository (the module documentation).
#[derive(Clone, Debug)]
pub struct Importer {
  /// The dump's reader.
  reader: Reader,
  /// The revision being read.
  revision: Option<OpenRevision>,
  /// The node record being read.
  node: Option<OpenNode>,
  /// The property section being read.
  properties: Option<OpenProperties>,
  /// The text being read.
  text: Option<OpenText>,
  /// The bytes held outside the repository and its transaction: the open revision's properties, section and text.
  in_flight: u64,
  /// The stream's refusal, once there is one: every later call answers it.
  refused: Option<MigrationRefusal>,
  /// Whether the stream has ended and been committed.
  done: bool,
}

/// A revision being read: its properties and its transaction (none for revision 0).
#[derive(Clone, Debug)]
struct OpenRevision {
  /// Its properties as read.
  properties: Properties,
  /// Its transaction; revision 0 makes none (`new_revision_record`).
  transaction: Option<Transaction>,
}

/// A node record being read: the path its sections change.
#[derive(Clone, Debug)]
struct OpenNode {
  /// Its absolute path.
  path: String,
}

/// Whose property section is being read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
  /// The revision record's.
  Revision,
  /// The node record's.
  Node,
}

/// A property section being read.
#[derive(Clone, Debug)]
struct OpenProperties {
  /// Whose section it is.
  owner: Owner,
  /// The list it makes: the node's own when the section is a change to it, else what it sets.
  list: Properties,
  /// The property calls it makes: one per property removed by a whole section, one per entry.
  calls: u64,
  /// The bytes its entries were charged.
  charged: u64,
}

/// How a text section carries its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextForm {
  /// Whole, in pieces.
  Whole,
  /// As svndiff windows against a base: the node's text, by identity, or the empty text when it has none.
  Delta {
    /// The base's identity.
    base: Option<ContentIdentity>,
  },
}

/// A text being read.
#[derive(Clone, Debug)]
struct OpenText {
  /// How the section carries it.
  form: TextForm,
  /// The digests its header names for the result.
  expected: (Option<Md5>, Option<Sha1>),
  /// The text made so far.
  bytes: Vec<u8>,
}

impl Importer {
  /// An import that has read nothing, its reader within `admission`.
  pub fn new(admission: Admission) -> Self {
    Self {
      reader: Reader::new(admission),
      revision: None,
      node: None,
      properties: None,
      text: None,
      in_flight: 0,
      refused: None,
      done: false,
    }
  }

  /// Reads on from the start of `buffer` (the bytes the last call did not consume, then any received since) into
  /// `repository`, committing each revision once the next one or the stream's end comes; `ended` says `buffer` holds
  /// the stream's last bytes.
  ///
  /// # Errors
  /// Every refusal of [`MigrationRefusal`] the stream meets. The revisions committed before it stay; the one being
  /// read is dropped whole; every later call answers the same refusal.
  pub fn import(
    &mut self,
    repository: &mut Repository,
    buffer: &[u8],
    ended: bool,
  ) -> Result<Importing, MigrationRefusal> {
    if let Some(refusal) = &self.refused {
      return Err(refusal.clone());
    }
    if self.done {
      return Ok(Importing::Done);
    }
    let result = if buffer.len() < usize::MAX { Err(MigrationRefusal::Corrupt) } else { self.run(repository, buffer, ended) };
    if let Err(refusal) = &result {
      self.refused = Some(refusal.clone());
      self.revision = None;
    }
    result
  }

  /// Takes events until more bytes are needed or the stream ends.
  fn run(
    &mut self,
    repository: &mut Repository,
    buffer: &[u8],
    ended: bool,
  ) -> Result<Importing, MigrationRefusal> {
    let mut offset = 0usize;
    loop {
      let rest = buffer.get(offset..).unwrap_or_default();
      match self
        .reader
        .read(rest, ended)
        .map_err(MigrationRefusal::Stream)?
      {
        Read::Event { event, consumed } => {
          self.take(repository, event)?;
          offset = offset.checked_add(consumed).ok_or(MigrationRefusal::Corrupt)?;
        }
        Read::NeedMore { consumed, needed } => {
          let consumed = offset.checked_add(consumed).ok_or(MigrationRefusal::Corrupt)?;
          return Ok(Importing::NeedMore { consumed, needed });
        }
        Read::End => {
          self.close_revision(repository)?;
          self.done = true;
          return Ok(Importing::Done);
        }
      }
    }
  }

  /// Applies one event.
  fn take(&mut self, repository: &mut Repository, event: Event<'_>) -> Result<(), MigrationRefusal> {
    match event {
      Event::Format(_) | Event::Delta(_) => Ok(()),
      Event::Uuid(uuid) => uuid_record(repository, uuid),
      Event::Revision(header) => self.revision_record(repository, &header),
      Event::Node(header) => self.node_record(repository, &header),
      Event::Property(property) => self.property(repository, property),
      Event::PropertiesEnd => self.properties_end(repository),
      Event::Text(bytes) => self.text_piece(repository, bytes),
      Event::Window(window) => self.window(repository, &window),
      Event::TextEnd => self.text_end(repository),
    }
  }

  /// A revision record: the open revision committed, then this one begun if it continues the history.
  fn revision_record(
    &mut self,
    repository: &mut Repository,
    header: &RevisionHeader,
  ) -> Result<(), MigrationRefusal> {
    self.close_revision(repository)?;
    let youngest = repository.youngest();
    let expected = youngest.checked_add(1).ok_or(MigrationRefusal::Corrupt)?;
    let continues = header.number == expected || (header.number == 0 && youngest == 0);
    if !continues {
      return Err(MigrationRefusal::OutOfSequence {
        expected,
        found: header.number,
      });
    }
    let transaction = if header.number == 0 {
      None
    } else {
      Some(repository.begin()?)
    };
    self.revision = Some(OpenRevision {
      properties: Properties::new(),
      transaction,
    });
    if header.properties.is_some() {
      self.properties = Some(OpenProperties {
        owner: Owner::Revision,
        list: Properties::new(),
        calls: 0,
        charged: 0,
      });
    }
    Ok(())
  }

  /// Commits the open revision, or sets revision 0's properties.
  fn close_revision(&mut self, repository: &mut Repository) -> Result<(), MigrationRefusal> {
    self.node = None;
    let Some(open) = self.revision.take() else {
      return Ok(());
    };
    self.in_flight = 0;
    match open.transaction {
      None => repository.set_revision_zero(open.properties),
      Some(transaction) => repository
        .commit(transaction, open.properties)
        .map(drop),
    }
  }

  /// A node record: its action applied to the transaction, and its sections opened.
  fn node_record(
    &mut self,
    repository: &mut Repository,
    header: &NodeHeader,
  ) -> Result<(), MigrationRefusal> {
    self.node = None;
    let path = node_path(&header.path)?;
    let transaction = self
      .revision
      .as_mut()
      .and_then(|open| open.transaction.as_mut())
      .ok_or(MigrationRefusal::Corrupt)?;
    let Some(facts) = apply_action(repository, transaction, &path, header)? else {
      return Ok(());
    };
    if let Some(section) = header.properties {
      let before = list_of(repository, transaction, facts)?;
      let (list, calls) = if section.delta {
        (before, 0)
      } else {
        let count = u64::try_from(before.len()).map_err(|_| MigrationRefusal::Corrupt)?;
        (Properties::new(), count)
      };
      self.properties = Some(OpenProperties {
        owner: Owner::Node,
        list,
        calls,
        charged: 0,
      });
    }
    if let Some(section) = header.text {
      self.text = Some(open_text(&path, facts, section)?);
    }
    self.node = Some(OpenNode { path });
    Ok(())
  }

  /// One entry of the open property section.
  fn property(
    &mut self,
    repository: &Repository,
    property: Property<'_>,
  ) -> Result<(), MigrationRefusal> {
    let bytes = match property {
      Property::Set { name, value } => name.len().saturating_add(value.len()),
      Property::Delete { name } => name.len(),
    };
    let charged = self.charge(repository, bytes)?;
    let open = self.properties.as_mut().ok_or(MigrationRefusal::Corrupt)?;
    match property {
      Property::Set { name, value } => {
        open.list.insert(name.to_vec(), value.to_vec());
      }
      Property::Delete { name } => {
        open.list.remove(name);
      }
    }
    open.calls = open.calls.saturating_add(1);
    open.charged = open.charged.saturating_add(charged);
    Ok(())
  }

  /// The end of the open property section: the revision's properties kept, or the node's list set when the section
  /// made a call.
  fn properties_end(&mut self, repository: &Repository) -> Result<(), MigrationRefusal> {
    let open = self.properties.take().ok_or(MigrationRefusal::Corrupt)?;
    let revision = self.revision.as_mut().ok_or(MigrationRefusal::Corrupt)?;
    if open.owner == Owner::Revision {
      revision.properties = open.list;
      return Ok(());
    }
    let node = self.node.as_ref().ok_or(MigrationRefusal::Corrupt)?;
    let transaction = revision
      .transaction
      .as_mut()
      .ok_or(MigrationRefusal::Corrupt)?;
    if open.calls > 0 {
      transaction.set_properties(repository, &node.path, &open.list)?;
    }
    self.in_flight = self.in_flight.saturating_sub(open.charged);
    Ok(())
  }

  /// A piece of a whole text.
  fn text_piece(&mut self, repository: &Repository, bytes: &[u8]) -> Result<(), MigrationRefusal> {
    self.charge(repository, bytes.len())?;
    let open = self.text.as_mut().ok_or(MigrationRefusal::Corrupt)?;
    open.bytes.extend_from_slice(bytes);
    Ok(())
  }

  /// One window of a text delta, applied to the base's bytes in its source view.
  fn window(&mut self, repository: &Repository, window: &Window) -> Result<(), MigrationRefusal> {
    let made = usize::try_from(window.target_len).map_err(|_| MigrationRefusal::Corrupt)?;
    self.charge(repository, made)?;
    let open = self.text.as_mut().ok_or(MigrationRefusal::Corrupt)?;
    let transaction = self
      .revision
      .as_ref()
      .and_then(|revision| revision.transaction.as_ref())
      .ok_or(MigrationRefusal::Corrupt)?;
    let base: &[u8] = match open.form {
      TextForm::Delta {
        base: Some(identity),
      } => transaction
        .content(repository, &identity)
        .ok_or(MigrationRefusal::Corrupt)?,
      TextForm::Delta { base: None } => &[],
      TextForm::Whole => return Err(MigrationRefusal::Corrupt),
    };
    let view = source_view(base, window.source_offset, window.source_len)
      .ok_or(MigrationRefusal::Delta(SvndiffError::SourceShort))?;
    apply(window, view, &mut open.bytes).map_err(MigrationRefusal::Delta)
  }

  /// The end of a text: its digests checked against its header's, and the text set on the node.
  fn text_end(&mut self, repository: &Repository) -> Result<(), MigrationRefusal> {
    let open = self.text.take().ok_or(MigrationRefusal::Corrupt)?;
    let node = self.node.as_ref().ok_or(MigrationRefusal::Corrupt)?;
    let digests = digests_of(&open.bytes)?;
    let (md5, sha1) = open.expected;
    check_digests(md5, sha1, digests, |expected, actual| {
      MigrationRefusal::ChecksumMismatch {
        path: node.path.clone(),
        expected,
        actual,
      }
    })?;
    let size = u64::try_from(open.bytes.len()).map_err(|_| MigrationRefusal::Corrupt)?;
    let transaction = self
      .revision
      .as_mut()
      .and_then(|revision| revision.transaction.as_mut())
      .ok_or(MigrationRefusal::Corrupt)?;
    transaction.set_text(repository, &node.path, open.bytes, digests)?;
    self.in_flight = self.in_flight.saturating_sub(size);
    Ok(())
  }

  /// Charges `bytes` more to the repository's room, refused when the repository, its transaction and what the import
  /// holds would pass it; answers the bytes charged.
  fn charge(&mut self, repository: &Repository, bytes: usize) -> Result<u64, MigrationRefusal> {
    let bytes = u64::try_from(bytes).map_err(|_| MigrationRefusal::Corrupt)?;
    let in_flight = self
      .in_flight
      .checked_add(bytes)
      .ok_or(MigrationRefusal::Corrupt)?;
    let pending = self
      .revision
      .as_ref()
      .and_then(|revision| revision.transaction.as_ref())
      .map_or(0, Transaction::pending_bytes);
    let needed = repository
      .held()
      .checked_add(pending)
      .and_then(|held| held.checked_add(in_flight))
      .ok_or(MigrationRefusal::Corrupt)?;
    let room = repository.room().bytes;
    if needed > room {
      return Err(MigrationRefusal::Room { needed, room });
    }
    self.in_flight = in_flight;
    Ok(bytes)
  }
}

/// A UUID record: the repository named by it when it holds no revision but 0 (`uuid_record`), refused when it names
/// another repository than one that holds history.
fn uuid_record(repository: &mut Repository, uuid: Uuid) -> Result<(), MigrationRefusal> {
  if repository.youngest() == 0 {
    repository.set_uuid(uuid);
    return Ok(());
  }
  if repository.uuid() != uuid {
    return Err(MigrationRefusal::ForeignRepository {
      repository: repository.uuid(),
      stream: uuid,
    });
  }
  Ok(())
}

/// Applies a node record's action to the transaction and answers the node the record leaves at its path, or none
/// for a deletion.
fn apply_action(
  repository: &Repository,
  transaction: &mut Transaction,
  path: &str,
  header: &NodeHeader,
) -> Result<Option<Facts>, MigrationRefusal> {
  match header.action {
    NodeAction::Change => {
      let facts = present(repository, transaction, path)?;
      check_kind(header.kind, facts.kind, path)?;
      transaction.modify(path, facts);
      Ok(Some(facts))
    }
    NodeAction::Delete => {
      let facts = deletable(repository, transaction, path)?;
      check_kind(header.kind, facts.kind, path)?;
      transaction.delete(path, facts);
      Ok(None)
    }
    NodeAction::Add => add(repository, transaction, path, header).map(Some),
    NodeAction::Replace => {
      let facts = deletable(repository, transaction, path)?;
      transaction.delete(path, facts);
      add(repository, transaction, path, header).map(Some)
    }
  }
}

/// The node at `path` in the transaction, refused where the path is absent.
fn present(
  repository: &Repository,
  transaction: &Transaction,
  path: &str,
) -> Result<Facts, MigrationRefusal> {
  transaction
    .lookup(repository, path)?
    .ok_or_else(|| MigrationRefusal::NotFound {
      revision: None,
      path: path.to_owned(),
    })
}

/// The node at `path`, which a deletion or a replacement removes: refused at the root or where the path is absent.
fn deletable(
  repository: &Repository,
  transaction: &Transaction,
  path: &str,
) -> Result<Facts, MigrationRefusal> {
  if path == ROOT {
    return Err(MigrationRefusal::RootChange);
  }
  present(repository, transaction, path)
}

/// An addition: a new node of the record's kind or a copy, at an absent path whose directory stands.
fn add(
  repository: &Repository,
  transaction: &mut Transaction,
  path: &str,
  header: &NodeHeader,
) -> Result<Facts, MigrationRefusal> {
  if path == ROOT {
    return Err(MigrationRefusal::RootChange);
  }
  let kind = header.kind.ok_or(MigrationRefusal::Corrupt)?;
  let directory = parent(path);
  if present(repository, transaction, directory)?.kind != NodeKind::Directory {
    return Err(MigrationRefusal::NotDirectory {
      path: directory.to_owned(),
    });
  }
  if transaction.lookup(repository, path)?.is_some() {
    return Err(MigrationRefusal::AlreadyExists {
      path: path.to_owned(),
    });
  }
  let source = match &header.copy_from {
    Some(copy) => Some(copy_source(repository, transaction, path, kind, copy)?),
    None => None,
  };
  transaction.add(path, kind, source);
  present(repository, transaction, path)
}

/// A copy's source: in a revision before the transaction's, present there, of the record's kind, its text the one
/// the record's checksums name (`maybe_add_with_history`).
fn copy_source(
  repository: &Repository,
  transaction: &Transaction,
  path: &str,
  kind: NodeKind,
  copy: &CopyFrom,
) -> Result<(CopySource, Facts), MigrationRefusal> {
  let source = node_path(&copy.path)?;
  if copy.revision > transaction.base() {
    return Err(MigrationRefusal::CopyFromFuture {
      revision: copy.revision,
    });
  }
  let facts = node(repository, &source, copy.revision)?.ok_or_else(|| MigrationRefusal::NotFound {
    revision: Some(copy.revision),
    path: source.clone(),
  })?;
  check_kind(Some(kind), facts.kind, path)?;
  check_digests(copy.md5, copy.sha1, text_digests(facts.text)?, |expected, actual| {
    MigrationRefusal::CopySourceChecksumMismatch {
      path: path.to_owned(),
      expected,
      actual,
    }
  })?;
  let source = CopySource {
    path: source,
    revision: copy.revision,
  };
  Ok((source, facts))
}

/// The text a record's text section opens on the node `facts`: refused on a directory, and for a delta, refused
/// unless its base is the text its header's checksums name.
fn open_text(path: &str, facts: Facts, section: TextSection) -> Result<OpenText, MigrationRefusal> {
  if facts.kind != NodeKind::File {
    return Err(MigrationRefusal::KindMismatch {
      path: path.to_owned(),
    });
  }
  let form = match section.delta {
    Some(base) => {
      check_digests(base.md5, base.sha1, text_digests(facts.text)?, |expected, actual| {
        MigrationRefusal::BaseChecksumMismatch {
          path: path.to_owned(),
          expected,
          actual,
        }
      })?;
      TextForm::Delta {
        base: facts.text.map(|text| text.identity),
      }
    }
    None => TextForm::Whole,
  };
  Ok(OpenText {
    form,
    expected: (section.md5, section.sha1),
    bytes: Vec::new(),
  })
}

/// The properties of the node `facts`, as the transaction holds them.
fn list_of(
  repository: &Repository,
  transaction: &Transaction,
  facts: Facts,
) -> Result<Properties, MigrationRefusal> {
  match facts.properties {
    PropertyList::Empty => Ok(Properties::new()),
    PropertyList::Held { identity, .. } => transaction
      .content(repository, &identity)
      .and_then(super::properties::decode)
      .ok_or(MigrationRefusal::Corrupt),
  }
}

/// Refuses a record whose stated kind is not the node's.
fn check_kind(stated: Option<NodeKind>, actual: NodeKind, path: &str) -> Result<(), MigrationRefusal> {
  match stated {
    Some(kind) if kind != actual => Err(MigrationRefusal::KindMismatch {
      path: path.to_owned(),
    }),
    _ => Ok(()),
  }
}

/// Checks `actual` against the digests a header names (all zeros naming none, as `svn_checksum_parse_hex` reads them);
/// a mismatch is refused as `mismatch` makes it of the digest expected and the one found.
fn check_digests(
  md5: Option<Md5>,
  sha1: Option<Sha1>,
  actual: (Md5, Sha1),
  mismatch: impl FnOnce(Digest, Digest) -> MigrationRefusal,
) -> Result<(), MigrationRefusal> {
  check_text(md5, sha1, actual).map_err(|refusal| match refusal {
    DumpError::ChecksumMismatch { expected, actual } => mismatch(expected, actual),
    other => MigrationRefusal::Stream(other),
  })
}

/// The digests of a node's text: its own, or the empty text's when it has none.
fn text_digests(text: Option<Text>) -> Result<(Md5, Sha1), MigrationRefusal> {
  match text {
    Some(text) => Ok((text.md5, text.sha1)),
    None => digests_of(&[]),
  }
}

/// The MD5 and SHA-1 of `bytes`.
fn digests_of(bytes: &[u8]) -> Result<(Md5, Sha1), MigrationRefusal> {
  let mut digests = Digests::new().map_err(MigrationRefusal::Stream)?;
  digests.update(bytes).map_err(MigrationRefusal::Stream)?;
  digests.finish().map_err(MigrationRefusal::Stream)
}

/// The absolute path a dump's path names, read as `svnadmin load` reads it: refused unless it is UTF-8 and every name
/// in it is one FSFS stores.
fn node_path(path: &[u8]) -> Result<String, MigrationRefusal> {
  let refuse = || MigrationRefusal::PathSyntax {
    path: path.to_vec(),
  };
  let relpath = String::from_utf8(canonical_relpath(path)).map_err(|_| refuse())?;
  let absolute = absolute(&relpath);
  if !is_canonical(&absolute) {
    return Err(refuse());
  }
  Ok(absolute)
}

/// The directory an absolute path other than the root stands in.
fn parent(path: &str) -> &str {
  match path.rfind('/') {
    Some(0) | None => ROOT,
    Some(end) => path.get(..end).unwrap_or(ROOT),
  }
}

/// The bytes `[offset, offset + length)` of `base`, if all of them are there.
fn source_view(base: &[u8], offset: u64, length: u64) -> Option<&[u8]> {
  let start = usize::try_from(offset).ok()?;
  let end = start.checked_add(usize::try_from(length).ok()?)?;
  base.get(start..end)
}

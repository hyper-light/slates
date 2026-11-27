//! What a Subversion repository's owner records of it (A-655 part 2b; §4.17.4 item 3.5; §4.17.1 "Durability and
//! residency"; A-662): the bytes one entry of the repository's logs holds (`RepositoryEntry::objects`), from which a
//! recovery makes the repository's records again through the one commit step, and nothing derived.
//!
//! **What it holds.** [`Record::Created`]: the repository's UUID and revision 0's `svn:date`, which a Subversion
//! repository is made with, as `svnadmin create` gives it both (`svn_repos_create`, `svn_fs_fs__create`), held by the
//! entry that joins the repository's root. [`Record::Revision`]: one revision as the commit step committed it
//! ([`Revision`]: the youngest revision it was committed on, the transaction's name and its changed-path rows, the
//! revision properties, the texts and property lists the repository did not hold yet, the lock entries), held by the
//! entry that moves the repository's green to it. Node-revisions, histories, copy roots and mergeinfo counts are the
//! index's, which the replay's check and apply compute again from the rows; none is recorded. Beside them, what an
//! import sets that no revision carries (A-652 part 3), each in an entry that moves no green: [`Record::Uuid`], the
//! repository named by its stream's UUID record (`svn_fs_set_uuid`, `load-fs-vtable.c` `uuid_record`); and
//! [`Record::RevisionProperties`], a revision's properties set whole (`svn_fs_change_rev_prop2` for each difference),
//! as an import sets revision 0's to its stream's (`close_revision`); [`Record::Locks`], the lock entries a lock verb
//! made (`lock`, `lock-many`, `unlock`, `unlock-many`; A-693 part 2). The deltify job's record, a stored text kept as a
//! delta in RAM from then on, is retired (C2 stage B part 2 (b), 2026-11-21: history lives on the node's store, so no
//! record moves a text into RAM); its kind byte is refused [`RecordRefused::Retired`].
//!
//! **The encoding**, little-endian as a green's chain entry and A-662's git record are: the magic, the version and a
//! kind byte; a creation's UUID, its 36 bytes, and its date; a revision's head, its transaction, its rows, its
//! properties, its contents and its lock entries, each list as a count and its items, each string as a length and its
//! bytes, each optional part after a byte that marks it present or absent; a UUID's 36 bytes; a revision's number and
//! its properties, written as a revision's are; a lock verb's lock entries, a count and each (A-693 part 2); a deltified text's identity, its base after a
//! byte that marks it present or absent, and its delta's length and bytes. A row is its path, its action, its kind,
//! its copy source, its text (identity, MD5, SHA-1, size), its property list (none, empty, or held by identity with its
//! mergeinfo mark) and its mergeinfo mark. A lock entry is a tag, then a set lock whole (path, token, owner, comment,
//! the DAV comment's mark, creation and expiry) or a removal (path, token).
//!
//! **Decoding refuses, typed** ([`RecordRefused`]), never a panic: every length and count is checked against the bytes
//! that remain before anything is allocated (a count against the fewest bytes one item takes); a string that is not
//! UTF-8, a tag or a mark of no kind, a UUID not in RFC 4122's form and a lock token `svn_fs_lock_many` would refuse;
//! revision properties or contents out of their order or named twice (the encoding is canonical, so every record
//! decodes to one value and that value encodes to the same bytes); a content whose bytes are not the identity it is
//! held under (BLAKE3, as the records name content); and bytes after the last item.
//!
//! No I/O, no clock, no randomness, no panic.

use std::collections::BTreeMap;

use super::revision::Revision;
use crate::dump::Uuid;
use crate::index::{
  Action, ChangedPath, ContentIdentity, CopySource, NodeKind, PropertyList, RevisionRecord, Text,
};
use crate::lock::{AprTime, Lock, LockEntry, LockToken};
use crate::migration::Properties;

/// Format: the record's magic, "SSVR" (a slates Subversion repository's record) little-endian, so bytes of another
/// kind are refused at the first word.
const MAGIC: u32 = u32::from_le_bytes(*b"SSVR");

/// Format: the record's version, which the encoder writes: 2 since a revision names the texts its commit stored in the
/// content store (identity and length, no bytes; §4.17.1 "A push's intake", main's ruling of 2026-11-20).
const VERSION: u32 = 2;

/// Format: the version before stored texts, which a decoder still reads (a revision of it stores none).
const VERSION_WITHOUT_STORED: u32 = 1;

/// Format: the kind byte of a repository's creation.
const CREATED: u8 = 0;

/// Format: the kind byte of a revision.
const REVISION: u8 = 1;

/// Format: the kind byte of the repository's UUID set (A-652 part 3).
const UUID_SET: u8 = 2;

/// Format: the kind byte of a revision's properties set whole (A-652 part 3).
const PROPERTIES_SET: u8 = 3;
/// Format: the kind byte of lock entries made by a lock verb alone, no revision with them (A-693 part 2).
const LOCKS_SET: u8 = 4;
/// Format: the kind byte the retired deltify record had (main's ruling on A-761; retired by C2 stage B part 2 (b)): never
/// written, refused [`Retired::Deltified`] where a log written before holds one.
const DELTIFIED: u8 = 5;

/// Format: the byte that marks an optional part absent, and a mark unset.
const ABSENT: u8 = 0;

/// Format: the byte that marks an optional part present, and a mark set.
const PRESENT: u8 = 1;

/// Format: a property list's byte: the row leaves it unchanged.
const LIST_UNCHANGED: u8 = 0;

/// Format: a property list's byte: the row leaves no properties.
const LIST_EMPTY: u8 = 1;

/// Format: a property list's byte: the row leaves a list held by its identity.
const LIST_HELD: u8 = 2;

/// Format: an action's byte, in [`Action`]'s order.
const ACTIONS: [Action; 4] = [Action::Add, Action::Delete, Action::Replace, Action::Modify];

/// Format: a kind's byte, in [`NodeKind`]'s order.
const KINDS: [NodeKind; 2] = [NodeKind::File, NodeKind::Directory];

/// Format: a lock entry's tag: a lock set.
const LOCK_SET: u8 = 0;

/// Format: a lock entry's tag: a lock removed.
const LOCK_REMOVED: u8 = 1;

/// Format: the bytes of a count, a length, a revision, a size and a time.
const LONG_BYTES: usize = size_of::<u64>();

/// Format: the bytes of a kind, a tag or a mark.
const BYTE_BYTES: usize = size_of::<u8>();

/// Format: the bytes of a content identity.
const IDENTITY_BYTES: usize = size_of::<ContentIdentity>();

/// Format: the bytes of a UUID in RFC 4122's form.
const UUID_BYTES: usize = 36;

/// Format: the fewest bytes a row takes: its path's length, its action, its kind, and its copy source's, text's,
/// property list's and mergeinfo's bytes, each absent.
const ROW_FEWEST: usize = LONG_BYTES + 6 * BYTE_BYTES;

/// Format: the fewest bytes a revision property takes: its name's length and its value's.
const PROPERTY_FEWEST: usize = 2 * LONG_BYTES;

/// Format: the fewest bytes a content takes: its identity and its length.
const CONTENT_FEWEST: usize = IDENTITY_BYTES + LONG_BYTES;

/// Format: the fewest bytes a lock entry takes: a removal's tag, its path's length and its token's.
const LOCK_ENTRY_FEWEST: usize = BYTE_BYTES + 2 * LONG_BYTES;

/// One record of a Subversion repository (the module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
  /// The repository made: its UUID and revision 0's `svn:date`.
  Created {
    /// The UUID.
    uuid: Uuid,
    /// Revision 0's `svn:date`, as `svn_time_to_cstring` wrote it.
    date: Vec<u8>,
  },
  /// One revision committed through the step.
  Revision(Revision),
  /// The repository named by another UUID (`svn_fs_set_uuid`), as an import names a repository that holds no revision
  /// but 0 by its stream's UUID record (A-652 part 3).
  Uuid(Uuid),
  /// A revision's properties set whole (`svn_fs_change_rev_prop2` for each difference), as an import sets revision 0's
  /// to its stream's (A-652 part 3).
  RevisionProperties {
    /// The revision.
    revision: u64,
    /// Its properties, whole.
    properties: Properties,
  },
  /// Lock entries a lock verb made, in the order they are applied, no revision with them (`svn_fs_lock_many`,
  /// `svn_fs_unlock_many` through `lock`, `lock-many`, `unlock`, `unlock-many`; A-693 part 2).
  Locks(Vec<LockEntry>),
}

/// A record kind this build no longer reads: what a log written before its retirement holds ([`RecordRefused::Retired`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retired {
  /// The deltify job's record (main's ruling on A-761): a stored text kept by the records as a delta in RAM from then
  /// on. Retired by C2 stage B part 2 (b) (2026-11-21, main's ruling of 09:20): a text is its changeset's on the node's
  /// store and nothing keeps it in RAM, so such a record names a form no repository keeps.
  Deltified,
}

/// Why bytes were refused as a record. A refused record replays nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordRefused {
  /// The bytes end before what they declare.
  Truncated,
  /// Bytes of another kind: the magic is not a record's.
  Magic,
  /// A version this build does not read.
  Version {
    /// The version read.
    version: u32,
  },
  /// A kind, an action, a node kind, a tag or a mark of no kind.
  Tag {
    /// The byte read.
    byte: u8,
  },
  /// A count past what the bytes that remain could hold.
  Count,
  /// A path, a name or a lock's string that is not UTF-8.
  Utf8,
  /// A UUID not in RFC 4122's form.
  Uuid,
  /// A lock token `svn_fs_lock_many` would refuse.
  Token,
  /// Revision properties or contents out of their order, or one named twice.
  Order,
  /// A content whose bytes are not the identity it is held under.
  Identity,
  /// Bytes after the last item.
  Trailing,
  /// A record of a kind this build retired ([`Retired`]): the log was written before it.
  Retired(Retired),
}

impl Record {
  /// The record's bytes (the module documentation).
  pub fn encode(&self) -> Vec<u8> {
    match self {
      Self::Created { uuid, date } => {
        let mut out = header(CREATED);
        out.extend_from_slice(uuid.as_bytes());
        put_bytes(&mut out, date);
        out
      }
      Self::Revision(revision) => Self::revision_bytes(revision),
      Self::Uuid(uuid) => {
        let mut out = header(UUID_SET);
        out.extend_from_slice(uuid.as_bytes());
        out
      }
      Self::RevisionProperties {
        revision,
        properties,
      } => {
        let mut out = header(PROPERTIES_SET);
        put_long(&mut out, *revision);
        put_properties(&mut out, properties);
        out
      }
      Self::Locks(entries) => {
        let mut out = header(LOCKS_SET);
        put_count(&mut out, entries.len());
        for entry in entries {
          put_lock_entry(&mut out, entry);
        }
        out
      }
    }
  }

  /// The bytes of `revision`'s record, as [`Record::encode`] writes a [`Record::Revision`], without taking the
  /// revision: what the owner's step writes before it hands the revision to the check (A-655 part 2b).
  pub fn revision_bytes(revision: &Revision) -> Vec<u8> {
    // The one encoding, taken whole: [`RevisionEncoding`] run to its end with no slice to its contents' copies.
    let mut encoding = RevisionEncoding::default();
    while !encoding.step(revision, usize::MAX) {}
    encoding.into_bytes()
  }

  /// The record `bytes` hold.
  ///
  /// # Errors
  /// Each [`RecordRefused`], by kind (the module documentation).
  pub fn decode(bytes: &[u8]) -> Result<Self, RecordRefused> {
    let mut reader = Reader { rest: bytes };
    if reader.word()? != MAGIC {
      return Err(RecordRefused::Magic);
    }
    let version = reader.word()?;
    if version != VERSION && version != VERSION_WITHOUT_STORED {
      return Err(RecordRefused::Version { version });
    }
    let record = match reader.byte()? {
      CREATED => created(&mut reader)?,
      REVISION => Self::Revision(revision(&mut reader, version)?),
      UUID_SET => Self::Uuid(uuid(&mut reader)?),
      PROPERTIES_SET => Self::RevisionProperties {
        revision: reader.long()?,
        properties: properties(&mut reader)?,
      },
      LOCKS_SET => Self::Locks(lock_entries(&mut reader)?),
      DELTIFIED => return Err(RecordRefused::Retired(Retired::Deltified)),
      byte => return Err(RecordRefused::Tag { byte }),
    };
    if !reader.rest.is_empty() {
      return Err(RecordRefused::Trailing);
    }
    Ok(record)
  }
}

/// A record's first bytes: the magic, the version and `kind`.
fn header(kind: u8) -> Vec<u8> {
  let mut out = Vec::with_capacity(HEADER_LEN);
  put_header(&mut out, kind);
  out
}

/// Appends a record's first bytes ([`header`]).
fn put_header(out: &mut Vec<u8>, kind: u8) {
  put_word(out, MAGIC);
  put_word(out, VERSION);
  out.push(kind);
}

/// Format: a header's length: the magic and the version (two words) and the kind (a byte).
const HEADER_LEN: usize = 4 + 4 + 1;

/// Format: a count, a length, a revision or a size ([`put_long`]).
const LONG: usize = size_of::<u64>();

/// A length-prefixed byte string's encoded length ([`put_bytes`]).
fn bytes_len(bytes: &[u8]) -> usize {
  LONG.saturating_add(bytes.len())
}

/// The bytes [`put_row`] writes for `row`, exactly.
fn row_len(row: &ChangedPath) -> usize {
  let copy = row.copy_from.as_ref().map_or(0, |source| {
    bytes_len(source.path.as_bytes()).saturating_add(LONG)
  });
  let text = row.text.as_ref().map_or(0, |text| {
    [
      size_of_val(&text.identity),
      size_of_val(&text.md5),
      size_of_val(&text.sha1),
      LONG,
    ]
    .into_iter()
    .fold(0usize, usize::saturating_add)
  });
  let properties = match &row.properties {
    Some(PropertyList::Held { identity, .. }) => 1usize
      .saturating_add(size_of_val(identity))
      .saturating_add(1),
    None | Some(PropertyList::Empty) => 1,
  };
  // The path, the action and kind bytes, the copy's mark and the text's mark, the properties, the mergeinfo mark.
  [
    bytes_len(row.path.as_bytes()),
    2,
    1,
    copy,
    1,
    text,
    properties,
    1,
  ]
  .into_iter()
  .fold(0usize, usize::saturating_add)
}

/// The bytes [`put_lock_entry`] writes for `entry`, exactly.
fn lock_len(entry: &LockEntry) -> usize {
  match entry {
    LockEntry::Set(lock) => [
      1,
      bytes_len(lock.path.as_bytes()),
      bytes_len(lock.token.as_str().as_bytes()),
      bytes_len(lock.owner.as_bytes()),
      1,
      lock
        .comment
        .as_ref()
        .map_or(0, |comment| bytes_len(comment.as_bytes())),
      1,
      size_of_val(&lock.created.0),
      1,
      lock.expires.map_or(0, |expires| size_of_val(&expires.0)),
    ]
    .into_iter()
    .fold(0usize, usize::saturating_add),
    LockEntry::Removed { path, token } => 1usize
      .saturating_add(bytes_len(path.as_bytes()))
      .saturating_add(bytes_len(token.as_str().as_bytes())),
  }
}

/// Appends a word.
fn put_word(out: &mut Vec<u8>, word: u32) {
  out.extend_from_slice(&word.to_le_bytes());
}

/// Appends a count, a length, a revision or a size.
fn put_long(out: &mut Vec<u8>, long: u64) {
  out.extend_from_slice(&long.to_le_bytes());
}

/// Appends a count or a length: a `usize` always fits the 64 bits written, on every target slates builds for.
fn put_count(out: &mut Vec<u8>, count: usize) {
  put_long(out, u64::try_from(count).unwrap_or(u64::MAX));
}

/// Appends a length and its bytes.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
  put_count(out, bytes.len());
  out.extend_from_slice(bytes);
}

/// Appends a mark.
fn put_mark(out: &mut Vec<u8>, set: bool) {
  out.push(if set { PRESENT } else { ABSENT });
}

/// The byte `value` is written as: its place in `all`.
fn index_of<T: PartialEq>(all: &[T], value: &T) -> u8 {
  all
    .iter()
    .position(|each| each == value)
    .and_then(|place| u8::try_from(place).ok())
    .unwrap_or(u8::MAX)
}

/// Appends a revision's properties: their count, then each name and value.
fn put_properties(out: &mut Vec<u8>, properties: &Properties) {
  put_count(out, properties.len());
  for (name, value) in properties {
    put_bytes(out, name);
    put_bytes(out, value);
  }
}

/// Appends a row.
fn put_row(out: &mut Vec<u8>, row: &ChangedPath) {
  put_bytes(out, row.path.as_bytes());
  out.push(index_of(&ACTIONS, &row.action));
  out.push(index_of(&KINDS, &row.kind));
  put_mark(out, row.copy_from.is_some());
  if let Some(source) = &row.copy_from {
    put_bytes(out, source.path.as_bytes());
    put_long(out, source.revision);
  }
  put_mark(out, row.text.is_some());
  if let Some(text) = &row.text {
    out.extend_from_slice(&text.identity);
    out.extend_from_slice(&text.md5);
    out.extend_from_slice(&text.sha1);
    put_long(out, text.size);
  }
  match row.properties {
    None => out.push(LIST_UNCHANGED),
    Some(PropertyList::Empty) => out.push(LIST_EMPTY),
    Some(PropertyList::Held {
      identity,
      has_mergeinfo,
    }) => {
      out.push(LIST_HELD);
      out.extend_from_slice(&identity);
      put_mark(out, has_mergeinfo);
    }
  }
  put_mark(out, row.mergeinfo_modified);
}

/// Appends a lock entry.
fn put_lock_entry(out: &mut Vec<u8>, entry: &LockEntry) {
  match entry {
    LockEntry::Set(lock) => {
      out.push(LOCK_SET);
      put_bytes(out, lock.path.as_bytes());
      put_bytes(out, lock.token.as_str().as_bytes());
      put_bytes(out, lock.owner.as_bytes());
      put_mark(out, lock.comment.is_some());
      if let Some(comment) = &lock.comment {
        put_bytes(out, comment.as_bytes());
      }
      put_mark(out, lock.is_dav_comment);
      out.extend_from_slice(&lock.created.0.to_le_bytes());
      put_mark(out, lock.expires.is_some());
      if let Some(expires) = lock.expires {
        out.extend_from_slice(&expires.0.to_le_bytes());
      }
    }
    LockEntry::Removed { path, token } => {
      out.push(LOCK_REMOVED);
      put_bytes(out, path.as_bytes());
      put_bytes(out, token.as_str().as_bytes());
    }
  }
}

/// The bytes not yet read.
struct Reader<'b> {
  /// What remains.
  rest: &'b [u8],
}

impl<'b> Reader<'b> {
  /// The next `count` bytes.
  fn take(&mut self, count: usize) -> Result<&'b [u8], RecordRefused> {
    let (taken, rest) = self
      .rest
      .split_at_checked(count)
      .ok_or(RecordRefused::Truncated)?;
    self.rest = rest;
    Ok(taken)
  }

  /// The next `N` bytes, as an array.
  fn array<const N: usize>(&mut self) -> Result<[u8; N], RecordRefused> {
    <[u8; N]>::try_from(self.take(N)?).map_err(|_| RecordRefused::Truncated)
  }

  /// The next byte.
  fn byte(&mut self) -> Result<u8, RecordRefused> {
    Ok(u8::from_le_bytes(self.array()?))
  }

  /// The next word.
  fn word(&mut self) -> Result<u32, RecordRefused> {
    Ok(u32::from_le_bytes(self.array()?))
  }

  /// The next count, revision or size.
  fn long(&mut self) -> Result<u64, RecordRefused> {
    Ok(u64::from_le_bytes(self.array()?))
  }

  /// The next time.
  fn time(&mut self) -> Result<AprTime, RecordRefused> {
    Ok(AprTime(i64::from_le_bytes(self.array()?)))
  }

  /// The next mark.
  fn mark(&mut self) -> Result<bool, RecordRefused> {
    match self.byte()? {
      ABSENT => Ok(false),
      PRESENT => Ok(true),
      byte => Err(RecordRefused::Tag { byte }),
    }
  }

  /// The next count of items each at least `fewest` bytes long, refused past what the bytes that remain could hold.
  fn count(&mut self, fewest: usize) -> Result<usize, RecordRefused> {
    let count = usize::try_from(self.long()?).map_err(|_| RecordRefused::Count)?;
    let needed = count.checked_mul(fewest).ok_or(RecordRefused::Count)?;
    if needed > self.rest.len() {
      return Err(RecordRefused::Count);
    }
    Ok(count)
  }

  /// The next length and its bytes, the length checked against the bytes that remain before they are copied.
  fn bytes(&mut self) -> Result<Vec<u8>, RecordRefused> {
    let length = usize::try_from(self.long()?).map_err(|_| RecordRefused::Truncated)?;
    Ok(self.take(length)?.to_vec())
  }

  /// The next length and its bytes, as UTF-8.
  fn string(&mut self) -> Result<String, RecordRefused> {
    String::from_utf8(self.bytes()?).map_err(|_| RecordRefused::Utf8)
  }

  /// The next byte, as the item of `all` at its place.
  fn one_of<T: Copy>(&mut self, all: &[T]) -> Result<T, RecordRefused> {
    let byte = self.byte()?;
    all
      .get(usize::from(byte))
      .copied()
      .ok_or(RecordRefused::Tag { byte })
  }
}

/// A creation's record, after its kind.
fn created(reader: &mut Reader<'_>) -> Result<Record, RecordRefused> {
  let uuid = uuid(reader)?;
  let date = reader.bytes()?;
  Ok(Record::Created { uuid, date })
}

/// A UUID in RFC 4122's form.
fn uuid(reader: &mut Reader<'_>) -> Result<Uuid, RecordRefused> {
  Uuid::parse(reader.take(UUID_BYTES)?).ok_or(RecordRefused::Uuid)
}

/// A revision's record, after its kind.
fn revision(reader: &mut Reader<'_>, version: u32) -> Result<Revision, RecordRefused> {
  let head = reader.long()?;
  let transaction = reader.string()?;
  let rows = reader.count(ROW_FEWEST)?;
  let mut changes = Vec::with_capacity(rows);
  for _ in 0..rows {
    changes.push(row(reader)?);
  }
  let properties = properties(reader)?;
  let contents = contents(reader)?;
  let stored = if version == VERSION_WITHOUT_STORED {
    BTreeMap::new()
  } else {
    stored(reader)?
  };
  let lock_entries = lock_entries(reader)?;
  Ok(Revision {
    head,
    record: RevisionRecord {
      transaction,
      changes,
    },
    properties,
    contents,
    stored,
    lock_entries,
  })
}

/// The texts a revision stored in the content store: their count, then each identity and length, identities ascending.
fn stored(reader: &mut Reader<'_>) -> Result<BTreeMap<ContentIdentity, u64>, RecordRefused> {
  let count = reader.count(CONTENT_FEWEST)?;
  let mut stored = BTreeMap::new();
  for _ in 0..count {
    let identity: ContentIdentity = reader.array()?;
    let len = reader.long()?;
    if stored
      .last_key_value()
      .is_some_and(|(last, _)| *last >= identity)
    {
      return Err(RecordRefused::Order);
    }
    stored.insert(identity, len);
  }
  Ok(stored)
}

/// A row.
fn row(reader: &mut Reader<'_>) -> Result<ChangedPath, RecordRefused> {
  let path = reader.string()?;
  let action = reader.one_of(&ACTIONS)?;
  let kind = reader.one_of(&KINDS)?;
  let copy_from = if reader.mark()? {
    let path = reader.string()?;
    let revision = reader.long()?;
    Some(CopySource { path, revision })
  } else {
    None
  };
  let text = if reader.mark()? {
    Some(text(reader)?)
  } else {
    None
  };
  let properties = property_list(reader)?;
  let mergeinfo_modified = reader.mark()?;
  Ok(ChangedPath {
    path,
    action,
    kind,
    copy_from,
    text,
    properties,
    mergeinfo_modified,
  })
}

/// A row's text, after its mark.
fn text(reader: &mut Reader<'_>) -> Result<Text, RecordRefused> {
  Ok(Text {
    identity: reader.array()?,
    md5: reader.array()?,
    sha1: reader.array()?,
    size: reader.long()?,
  })
}

/// A row's property list.
fn property_list(reader: &mut Reader<'_>) -> Result<Option<PropertyList>, RecordRefused> {
  match reader.byte()? {
    LIST_UNCHANGED => Ok(None),
    LIST_EMPTY => Ok(Some(PropertyList::Empty)),
    LIST_HELD => Ok(Some(PropertyList::Held {
      identity: reader.array()?,
      has_mergeinfo: reader.mark()?,
    })),
    byte => Err(RecordRefused::Tag { byte }),
  }
}

/// A revision's properties, each name after the last.
fn properties(reader: &mut Reader<'_>) -> Result<Properties, RecordRefused> {
  let count = reader.count(PROPERTY_FEWEST)?;
  let mut properties = Properties::new();
  for _ in 0..count {
    let name = reader.bytes()?;
    let value = reader.bytes()?;
    if properties
      .last_key_value()
      .is_some_and(|(last, _)| *last >= name)
    {
      return Err(RecordRefused::Order);
    }
    properties.insert(name, value);
  }
  Ok(properties)
}

/// A revision's contents, each identity after the last and each the identity of its bytes.
fn contents(reader: &mut Reader<'_>) -> Result<BTreeMap<ContentIdentity, Vec<u8>>, RecordRefused> {
  let count = reader.count(CONTENT_FEWEST)?;
  let mut contents = BTreeMap::new();
  for _ in 0..count {
    let identity: ContentIdentity = reader.array()?;
    let bytes = reader.bytes()?;
    if contents
      .last_key_value()
      .is_some_and(|(last, _)| *last >= identity)
    {
      return Err(RecordRefused::Order);
    }
    if *blake3::hash(&bytes).as_bytes() != identity {
      return Err(RecordRefused::Identity);
    }
    contents.insert(identity, bytes);
  }
  Ok(contents)
}

/// A list of lock entries: its count, then each.
fn lock_entries(reader: &mut Reader<'_>) -> Result<Vec<LockEntry>, RecordRefused> {
  let entries = reader.count(LOCK_ENTRY_FEWEST)?;
  let mut lock_entries = Vec::with_capacity(entries);
  for _ in 0..entries {
    lock_entries.push(lock_entry(reader)?);
  }
  Ok(lock_entries)
}

/// A lock entry.
fn lock_entry(reader: &mut Reader<'_>) -> Result<LockEntry, RecordRefused> {
  match reader.byte()? {
    LOCK_SET => Ok(LockEntry::Set(lock(reader)?)),
    LOCK_REMOVED => {
      let path = reader.string()?;
      let token = token(reader)?;
      Ok(LockEntry::Removed { path, token })
    }
    byte => Err(RecordRefused::Tag { byte }),
  }
}

/// A lock set, after its tag.
fn lock(reader: &mut Reader<'_>) -> Result<Lock, RecordRefused> {
  let path = reader.string()?;
  let token = token(reader)?;
  let owner = reader.string()?;
  let comment = if reader.mark()? {
    Some(reader.string()?)
  } else {
    None
  };
  let is_dav_comment = reader.mark()?;
  let created = reader.time()?;
  let expires = if reader.mark()? {
    Some(reader.time()?)
  } else {
    None
  };
  Ok(Lock {
    path,
    token,
    owner,
    comment,
    is_dav_comment,
    created,
    expires,
  })
}

/// A lock token, as `svn_fs_lock_many` takes one.
fn token(reader: &mut Reader<'_>) -> Result<LockToken, RecordRefused> {
  LockToken::supplied(&reader.string()?).map_err(|_| RecordRefused::Token)
}

#[cfg(test)]
mod tests;

/// A revision's record written a part at a time ([`Record::revision_bytes`] taken in units; A-920: a revision of 9,000
/// rows, 1.2 MB, encoded whole in one step of its check, 3.8 ms debug). Its length is measured an entry a unit and
/// reserved exactly once, never grown (A-742: growing a buffer of a revision's size by doubling copied it again at each
/// doubling, 265 MiB of such copies in a 1,000-revision import); then its entries are written in the record's order an
/// entry a unit, each list's count as the list begins, a content's bytes a copy slice a unit. Its bytes are
/// [`Record::revision_bytes`]'s, which is this run to its end.
#[derive(Debug, Default)]
pub struct RevisionEncoding {
  /// The bytes written so far, reserved at the record's length once it is measured.
  out: Vec<u8>,
  /// The record's length measured so far.
  len: usize,
  /// Whether the length is measured and the entries are being written.
  writing: bool,
  /// The part under way.
  part: Part,
  /// The next entry of a list part.
  at: usize,
  /// The last key written or measured of a map part.
  after: Option<Vec<u8>>,
  /// The bytes of the content under way written so far.
  written: usize,
}

/// The parts of a revision's record, in its order ([`RevisionEncoding`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Part {
  /// The header, the head and the transaction's name.
  #[default]
  Head,
  /// The changed-path rows.
  Rows,
  /// The revision properties.
  Properties,
  /// The texts carried.
  Contents,
  /// The texts named stored.
  Stored,
  /// The lock entries.
  Locks,
  /// Every part.
  End,
}

impl RevisionEncoding {
  /// One unit of `revision`'s record, a content's bytes `slice` at most: whether the record is written whole.
  pub fn step(&mut self, revision: &Revision, slice: usize) -> bool {
    match self.part {
      Part::Head => self.head(revision),
      Part::Rows => self.row(revision),
      Part::Properties => self.property(revision),
      Part::Contents => self.content(revision, slice.max(1)),
      Part::Stored => self.stored(revision),
      Part::Locks => self.lock(revision),
      Part::End => return self.ended(),
    }
    false
  }

  /// The record's bytes, once written whole.
  pub fn into_bytes(self) -> Vec<u8> {
    self.out
  }

  /// [`Part::End`]: the length measured, the record's buffer reserved and its writing begun; or the record written.
  fn ended(&mut self) -> bool {
    if self.writing {
      return true;
    }
    self.out = Vec::with_capacity(self.len);
    self.writing = true;
    self.part = Part::Head;
    false
  }

  /// `part` begun, its list's `count` first.
  fn begin(&mut self, part: Part, count: usize) {
    if self.writing {
      put_count(&mut self.out, count);
    } else {
      self.len = self.len.saturating_add(LONG);
    }
    (self.part, self.at, self.after, self.written) = (part, 0, None, 0);
  }

  /// [`Part::Head`].
  fn head(&mut self, revision: &Revision) {
    let transaction = revision.record.transaction.as_bytes();
    if self.writing {
      put_header(&mut self.out, REVISION);
      put_long(&mut self.out, revision.head);
      put_bytes(&mut self.out, transaction);
    } else {
      self.len = [HEADER_LEN, LONG, bytes_len(transaction)]
        .into_iter()
        .fold(self.len, usize::saturating_add);
    }
    self.begin(Part::Rows, revision.record.changes.len());
  }

  /// [`Part::Rows`]: the next row.
  fn row(&mut self, revision: &Revision) {
    let Some(row) = revision.record.changes.get(self.at) else {
      // The properties' count is their part's first bytes ([`put_properties`]).
      self.begin(Part::Properties, revision.properties.len());
      return;
    };
    if self.writing {
      put_row(&mut self.out, row);
    } else {
      self.len = self.len.saturating_add(row_len(row));
    }
    self.at = self.at.saturating_add(1);
  }

  /// [`Part::Properties`]: the next property after the last.
  fn property(&mut self, revision: &Revision) {
    let Some((name, value)) = next_after(&revision.properties, self.after.as_deref()) else {
      self.begin(Part::Contents, revision.contents.len());
      return;
    };
    if self.writing {
      put_bytes(&mut self.out, name);
      put_bytes(&mut self.out, value);
    } else {
      self.len = self
        .len
        .saturating_add(bytes_len(name).saturating_add(bytes_len(value)));
    }
    self.after = Some(name.clone());
  }

  /// [`Part::Contents`]: the next content after the last, or `slice` more of the one under way.
  fn content(&mut self, revision: &Revision, slice: usize) {
    let Some((identity, bytes)) = next_after(&revision.contents, self.after.as_deref()) else {
      self.begin(Part::Stored, revision.stored.len());
      return;
    };
    if !self.writing {
      self.len = self
        .len
        .saturating_add(size_of_val(identity).saturating_add(bytes_len(bytes)));
      self.after = Some(identity.to_vec());
      return;
    }
    if self.written == 0 {
      self.out.extend_from_slice(identity);
      put_count(&mut self.out, bytes.len());
    }
    let end = self.written.saturating_add(slice).min(bytes.len());
    self
      .out
      .extend_from_slice(bytes.get(self.written..end).unwrap_or_default());
    self.written = end;
    if self.written >= bytes.len() {
      self.after = Some(identity.to_vec());
      self.written = 0;
    }
  }

  /// [`Part::Stored`]: the next text named stored after the last.
  fn stored(&mut self, revision: &Revision) {
    let Some((identity, len)) = next_after(&revision.stored, self.after.as_deref()) else {
      self.begin(Part::Locks, revision.lock_entries.len());
      return;
    };
    if self.writing {
      self.out.extend_from_slice(identity);
      put_long(&mut self.out, *len);
    } else {
      self.len = self.len.saturating_add(CONTENT_FEWEST);
    }
    self.after = Some(identity.to_vec());
  }

  /// [`Part::Locks`]: the next lock entry.
  fn lock(&mut self, revision: &Revision) {
    let Some(entry) = revision.lock_entries.get(self.at) else {
      self.part = Part::End;
      return;
    };
    if self.writing {
      put_lock_entry(&mut self.out, entry);
    } else {
      self.len = self.len.saturating_add(lock_len(entry));
    }
    self.at = self.at.saturating_add(1);
  }
}

/// The entry of `map` after the key `after` (its first without one), its key read as bytes.
fn next_after<'m, K: AsRef<[u8]> + Ord + std::borrow::Borrow<[u8]>, V>(
  map: &'m BTreeMap<K, V>,
  after: Option<&[u8]>,
) -> Option<(&'m K, &'m V)> {
  let from = after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
  map
    .range::<[u8], _>((from, std::ops::Bound::Unbounded))
    .next()
}

//! Changegroups, decoded and encoded (A-514; `internals.changegroups` [B]; `mercurial/changegroup.py` [C]).
//!
//! A changegroup is framed in chunks: a signed 32-bit length that counts its own four bytes, then the data. A length
//! of 0 is an empty chunk, and 7.2 refuses any other length up to 4 ("invalid chunk length"). The changelog's delta
//! group comes first, then the manifest's, then from version `03` a list of tree-manifest groups, and last a list of
//! file groups. A group is delta chunks ended by an empty chunk; a list is a name chunk and its group, again and again,
//! ended by an empty chunk. The tree-manifest list is in every changegroup from `03` on, empty (one empty chunk) when
//! no tree manifest travels: 7.2's `cg3unpacker` reads it unconditionally. (7.2's own `debugbundle --all` lists no file
//! of a `03` changegroup because it does not; the fixtures show it.)
//!
//! A delta chunk is a header, then the delta. The headers, from 7.2's code (`_CHANGEGROUPV1_DELTA_HEADER` to `V5`):
//! - `01`, 80 bytes: node, p1, p2, link node. The delta's base is the previous revision of the group, or p1 for the
//!   group's first (`cg1unpacker._deltaheader`).
//! - `02`, 100 bytes: node, p1, p2, delta base, link node.
//! - `03`, 102 bytes: `02`'s fields and the revision flags (16 bits).
//! - `04`, 130 bytes: `03`'s fields, then the snapshot level (a signed byte), the raw size (32 bits), the wire
//!   compression (a byte), the protocol flags (a byte), the delta's base in the sender's storage and that base's
//!   snapshot level. Protocol flag 2 says the chunk is the revision's full text, not a delta. 7.2's
//!   `internals.changegroups` still calls `04` the sidedata version; the code moved sidedata to `05`
//!   (`docs/wip/research/mercurial-server.md` §3.3).
//! - `05`, 103 bytes: the protocol flags first, then `03`'s fields; with protocol flag 1 a chunk of sidedata follows
//!   the delta's chunk.
//!
//! A delta that travels as plain hunks is checked by mpatch's decoding rules ([`delta::check`]) as it is read. One
//! that travels compressed (version `04`, wire compression zlib or zstd) is passed on as it came, for the store to
//! open (piece 3).

use core::ops::Range;

use crate::delta;
use crate::out::{Measure, Out};
use crate::read::{Cursor, to_u64};
use crate::{Admission, Node, PastAdmission, Truncated};

/// Format: a chunk's length counts its own four bytes (`chunkheader`: `length + 4`).
const LENGTH_LEN: usize = 4;

/// Format: the length of an empty chunk, which ends a group or a list (`closechunk`).
const EMPTY: i32 = 0;

/// Format: version 01's delta header: node, p1, p2 and link node, 20 bytes each (`>20s20s20s20s`).
const V01_HEADER: usize = 80;

/// Format: version 02's: node, p1, p2, delta base and link node (`>20s20s20s20s20s`).
const V02_HEADER: usize = 100;

/// Format: version 03's: version 02's fields and the revision flags (`>20s20s20s20s20sH`).
const V03_HEADER: usize = 102;

/// Format: version 04's (`>20s20s20s20s20sHbIBB20sb`): version 03's fields, the snapshot level, the raw size, the wire
/// compression, the protocol flags, the stored delta base and its snapshot level.
const V04_HEADER: usize = 130;

/// Format: version 05's (`>B20s20s20s20s20sH`): the protocol flags, then version 03's fields.
const V05_HEADER: usize = 103;

/// Format: protocol flag: a chunk of sidedata follows the delta's (`CG_FLAG_SIDEDATA`, `utils/storageutil.py`).
pub const FLAG_SIDEDATA: u8 = 1;

/// Format: protocol flag: the chunk is the revision's full text, not a delta (`CG_FLAG_FULL_TEXT`).
pub const FLAG_FULL_TEXT: u8 = 2;

/// Format: the wire compressions (`WireDeltaCompression`) under which a delta travels as plain hunks: 0, nothing
/// known of its compression, and 1, uncompressed. 2 (zlib) and 3 (zstd) carry compressed bytes.
const PLAIN: [u8; 2] = [0, 1];

/// A changegroup version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
  /// `01`: deltas against the previous revision of the group.
  V01,
  /// `02`: each delta names its base.
  V02,
  /// `03`: revision flags; the tree-manifest list.
  V03,
  /// `04`: delta information and full texts (7.2's code).
  V04,
  /// `05`: sidedata (7.2's code).
  V05,
}

impl Version {
  /// The version a name (`01` to `05`) means.
  pub fn from_name(name: &[u8]) -> Result<Self, Refusal> {
    [Self::V01, Self::V02, Self::V03, Self::V04, Self::V05]
      .into_iter()
      .find(|version| version.name() == name)
      .ok_or_else(|| Refusal::UnknownVersion {
        name: name.to_vec(),
      })
  }

  /// The version a bundle2 changegroup part's `version` parameter means: `01` when it has none, as 7.2 reads it
  /// (`inpart.params.get(b'version', b'01')`).
  pub fn from_part_param(value: Option<&[u8]>) -> Result<Self, Refusal> {
    value.map_or(Ok(Self::V01), Self::from_name)
  }

  /// The version's name on the wire.
  pub fn name(self) -> &'static [u8] {
    match self {
      Self::V01 => b"01",
      Self::V02 => b"02",
      Self::V03 => b"03",
      Self::V04 => b"04",
      Self::V05 => b"05",
    }
  }

  /// The length of the version's delta header.
  pub fn header_len(self) -> usize {
    match self {
      Self::V01 => V01_HEADER,
      Self::V02 => V02_HEADER,
      Self::V03 => V03_HEADER,
      Self::V04 => V04_HEADER,
      Self::V05 => V05_HEADER,
    }
  }

  /// Whether the version carries the tree-manifest list.
  fn has_tree_list(self) -> bool {
    self >= Self::V03
  }
}

/// Version 04's delta information.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeltaInfo {
  /// The revision's snapshot level in the sender's storage; −2 when it sent none.
  pub snapshot_level: i8,
  /// The length of the revision's full text.
  pub raw_size: u32,
  /// The delta's wire compression (`WireDeltaCompression`).
  pub compression: u8,
  /// The delta's base in the sender's storage.
  pub stored_base: Node,
  /// That base's snapshot level.
  pub stored_snapshot_level: i8,
}

/// One revision of a changegroup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revision<'a> {
  /// The revision's node.
  pub node: Node,
  /// Its first parent.
  pub p1: Node,
  /// Its second parent.
  pub p2: Node,
  /// The base its delta applies to: named from version 02, and in version 01 the previous revision of its group, or
  /// its p1 for a group's first, as 7.2 resolves it.
  pub base: Node,
  /// The changeset it belongs to.
  pub link: Node,
  /// Its revision flags (version 03 on; 0 before).
  pub flags: u16,
  /// Version 04's delta information.
  pub info: Option<DeltaInfo>,
  /// The protocol flags (versions 04 and 05; 0 before).
  pub protocol_flags: u8,
  /// The delta, or with [`FLAG_FULL_TEXT`] the full text.
  pub data: &'a [u8],
  /// Version 05's sidedata, with [`FLAG_SIDEDATA`].
  pub sidedata: Option<&'a [u8]>,
}

impl Revision<'_> {
  /// Whether [`Revision::data`] is the full text rather than a delta.
  pub fn is_full_text(&self) -> bool {
    self.protocol_flags & FLAG_FULL_TEXT != 0
  }

  /// Whether [`Revision::data`] is plain hunks: not a full text, and not compressed on the wire.
  pub fn is_plain_delta(&self) -> bool {
    !self.is_full_text()
      && self
        .info
        .is_none_or(|info| PLAIN.contains(&info.compression))
  }
}

/// A named group: one tree manifest's or one file's revisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group<'a> {
  /// The tree's directory or the file's path, as sent.
  pub name: &'a [u8],
  /// Its revisions in [`Changegroup::revisions`].
  pub revisions: Range<usize>,
}

/// A decoded changegroup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changegroup<'a> {
  /// Its version.
  pub version: Version,
  /// Every revision, in stream order.
  pub revisions: Vec<Revision<'a>>,
  /// The changelog's revisions.
  pub changelog: Range<usize>,
  /// The manifest's revisions (the root's, with tree manifests).
  pub manifests: Range<usize>,
  /// The tree-manifest groups (version 03 on).
  pub trees: Vec<Group<'a>>,
  /// The file groups.
  pub files: Vec<Group<'a>>,
}

/// Why a changegroup was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A version name Mercurial does not define.
  UnknownVersion {
    /// The name.
    name: Vec<u8>,
  },
  /// A length past the bytes left.
  Truncated(Truncated),
  /// A chunk length 7.2 refuses: negative (`0xFFFF_FFFF` is −1), or 1 to 4 ("invalid chunk length").
  ChunkLength {
    /// The length.
    value: i32,
  },
  /// A delta chunk shorter than its version's header.
  ShortChunk {
    /// The chunk's length.
    len: u64,
    /// The header's.
    header: u64,
  },
  /// A delta mpatch cannot decode.
  Delta {
    /// The revision's node.
    node: Node,
    /// Why.
    refusal: delta::Refusal,
  },
  /// Bytes after the changegroup's end.
  TrailingBytes {
    /// How many.
    bytes: u64,
  },
  /// More revisions or groups than the caller's admission allows.
  PastAdmission(PastAdmission),
}

impl From<Truncated> for Refusal {
  fn from(truncated: Truncated) -> Self {
    Self::Truncated(truncated)
  }
}

impl From<PastAdmission> for Refusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// Why a changegroup could not be encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeRefusal {
  /// A revision whose fields its version cannot carry.
  Unencodable {
    /// The revision's node.
    node: Node,
    /// Which field, and why.
    reason: &'static str,
  },
  /// A chunk longer than a signed 32-bit length holds.
  TooLong {
    /// Its length.
    len: u64,
  },
  /// A group's range outside [`Changegroup::revisions`].
  Range,
}

/// Which part of the changegroup a group belongs to.
#[derive(Clone, Copy)]
enum List {
  /// The changelog's group.
  Changelog,
  /// The manifest's group.
  Manifests,
  /// A tree manifest's group.
  Trees,
  /// A file's group.
  Files,
}

/// Where a walk puts what it finds: the counting pass and the building pass.
trait Sink<'a> {
  /// A revision.
  fn revision(&mut self, revision: Revision<'a>);
  /// The end of a group of `list`, named `name` (empty for the changelog's and the manifest's).
  fn end(&mut self, list: List, name: &'a [u8]);
}

/// The counting pass.
#[derive(Default)]
struct Count {
  /// Revisions.
  revisions: usize,
  /// Tree groups.
  trees: usize,
  /// File groups.
  files: usize,
}

impl<'a> Sink<'a> for Count {
  fn revision(&mut self, _revision: Revision<'a>) {
    self.revisions = self.revisions.saturating_add(1);
  }

  fn end(&mut self, list: List, _name: &'a [u8]) {
    match list {
      List::Trees => self.trees = self.trees.saturating_add(1),
      List::Files => self.files = self.files.saturating_add(1),
      List::Changelog | List::Manifests => {}
    }
  }
}

/// The building pass, into vectors the counting pass sized.
struct Build<'a> {
  /// The revisions.
  revisions: Vec<Revision<'a>>,
  /// The tree groups.
  trees: Vec<Group<'a>>,
  /// The file groups.
  files: Vec<Group<'a>>,
  /// Where the current group's revisions begin.
  start: usize,
  /// The changelog's range, once ended.
  changelog: Range<usize>,
  /// The manifest's range, once ended.
  manifests: Range<usize>,
}

impl<'a> Sink<'a> for Build<'a> {
  fn revision(&mut self, revision: Revision<'a>) {
    self.revisions.push(revision);
  }

  fn end(&mut self, list: List, name: &'a [u8]) {
    let revisions = self.start..self.revisions.len();
    self.start = self.revisions.len();
    match list {
      List::Changelog => self.changelog = revisions,
      List::Manifests => self.manifests = revisions,
      List::Trees => self.trees.push(Group { name, revisions }),
      List::Files => self.files.push(Group { name, revisions }),
    }
  }
}

impl<'a> Changegroup<'a> {
  /// Decodes a changegroup of `version`: walked once to check and count it, the exact total charged to `admission`,
  /// and walked again to build.
  pub fn decode(
    input: &'a [u8],
    version: Version,
    admission: &mut Admission,
  ) -> Result<Self, Refusal> {
    let cursor = Cursor::new(input);
    let mut count = Count::default();
    walk(&mut cursor.clone(), version, &mut count)?;
    admission.charge_items::<Revision<'_>>(count.revisions)?;
    admission.charge_items::<Group<'_>>(count.trees.saturating_add(count.files))?;
    let mut build = Build {
      revisions: Vec::with_capacity(count.revisions),
      trees: Vec::with_capacity(count.trees),
      files: Vec::with_capacity(count.files),
      start: 0,
      changelog: 0..0,
      manifests: 0..0,
    };
    walk(&mut cursor.clone(), version, &mut build)?;
    Ok(Self {
      version,
      revisions: build.revisions,
      changelog: build.changelog,
      manifests: build.manifests,
      trees: build.trees,
      files: build.files,
    })
  }

  /// The revisions of `range`, as a group names them; empty for a range outside the changegroup.
  pub fn group(&self, range: &Range<usize>) -> &[Revision<'a>] {
    self.revisions.get(range.clone()).unwrap_or_default()
  }

  /// The encoded changegroup's length, for a caller that charges it before encoding.
  pub fn encoded_len(&self) -> Result<u64, EncodeRefusal> {
    let mut measure = Measure(0);
    self.encode_into(&mut measure)?;
    Ok(measure.0)
  }

  /// Appends the encoded changegroup to `out`.
  pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeRefusal> {
    self.encode_into(out)
  }

  /// Writes the changelog's group, the manifest's, the tree list from version 03, and the file list.
  fn encode_into(&self, out: &mut impl Out) -> Result<(), EncodeRefusal> {
    for range in [&self.changelog, &self.manifests] {
      let revisions = self
        .revisions
        .get(range.clone())
        .ok_or(EncodeRefusal::Range)?;
      encode_group(revisions, self.version, out)?;
    }
    if self.version.has_tree_list() {
      self.encode_list(&self.trees, out)?;
    }
    self.encode_list(&self.files, out)
  }

  /// Writes a list: each group's name chunk and its group, then the empty chunk.
  fn encode_list(&self, groups: &[Group<'_>], out: &mut impl Out) -> Result<(), EncodeRefusal> {
    for group in groups {
      put_chunk_length(out, group.name.len())?;
      out.put(group.name);
      let revisions = self
        .revisions
        .get(group.revisions.clone())
        .ok_or(EncodeRefusal::Range)?;
      encode_group(revisions, self.version, out)?;
    }
    out.put(&EMPTY.to_be_bytes());
    Ok(())
  }
}

/// Walks the changelog's group, the manifest's, the tree list from version 03, the file list, and requires the input
/// to end where the changegroup does.
fn walk<'a>(
  cursor: &mut Cursor<'a>,
  version: Version,
  sink: &mut impl Sink<'a>,
) -> Result<(), Refusal> {
  group(cursor, version, sink)?;
  sink.end(List::Changelog, &[]);
  group(cursor, version, sink)?;
  sink.end(List::Manifests, &[]);
  if version.has_tree_list() {
    list(cursor, version, List::Trees, sink)?;
  }
  list(cursor, version, List::Files, sink)?;
  if cursor.is_done() {
    Ok(())
  } else {
    Err(Refusal::TrailingBytes {
      bytes: to_u64(cursor.left()),
    })
  }
}

/// Walks a list of named groups, to its empty chunk.
fn list<'a>(
  cursor: &mut Cursor<'a>,
  version: Version,
  kind: List,
  sink: &mut impl Sink<'a>,
) -> Result<(), Refusal> {
  while let Some(name) = chunk(cursor, "group name")? {
    group(cursor, version, sink)?;
    sink.end(kind, name);
  }
  Ok(())
}

/// Walks one group's delta chunks, to its empty chunk, each with version 05's sidedata chunk where its flag says so.
fn group<'a>(
  cursor: &mut Cursor<'a>,
  version: Version,
  sink: &mut impl Sink<'a>,
) -> Result<(), Refusal> {
  let mut previous: Option<Node> = None;
  while let Some(body) = chunk(cursor, "delta chunk")? {
    let mut revision = parse(body, version, previous)?;
    if version == Version::V05 && revision.protocol_flags & FLAG_SIDEDATA != 0 {
      revision.sidedata = Some(chunk(cursor, "sidedata chunk")?.unwrap_or_default());
    }
    previous = Some(revision.node);
    sink.revision(revision);
  }
  Ok(())
}

/// One chunk's data, or `None` for the empty chunk.
fn chunk<'a>(cursor: &mut Cursor<'a>, what: &'static str) -> Result<Option<&'a [u8]>, Refusal> {
  let value = cursor.i32(what)?;
  if value == EMPTY {
    return Ok(None);
  }
  let body = usize::try_from(value)
    .ok()
    .and_then(|len| len.checked_sub(LENGTH_LEN))
    .filter(|body| *body > 0)
    .ok_or(Refusal::ChunkLength { value })?;
  Ok(Some(cursor.take(body, what)?))
}

/// One delta chunk: its version's header, then the delta (or full text), checked when it is plain hunks.
fn parse(body: &[u8], version: Version, previous: Option<Node>) -> Result<Revision<'_>, Refusal> {
  if body.len() < version.header_len() {
    return Err(Refusal::ShortChunk {
      len: to_u64(body.len()),
      header: to_u64(version.header_len()),
    });
  }
  let mut cursor = Cursor::new(body);
  let leading_flags = if version == Version::V05 {
    cursor.u8("protocol flags")?
  } else {
    0
  };
  let node = cursor.node("node")?;
  let p1 = cursor.node("p1")?;
  let p2 = cursor.node("p2")?;
  let base = if version == Version::V01 {
    previous.unwrap_or(p1)
  } else {
    cursor.node("delta base")?
  };
  let link = cursor.node("link node")?;
  let flags = if version >= Version::V03 {
    cursor.u16("revision flags")?
  } else {
    0
  };
  let (info, protocol_flags) = if version == Version::V04 {
    let (info, flags) = parse_info(&mut cursor)?;
    (Some(info), flags)
  } else {
    (None, leading_flags)
  };
  let revision = Revision {
    node,
    p1,
    p2,
    base,
    link,
    flags,
    info,
    protocol_flags,
    data: cursor.rest(),
    sidedata: None,
  };
  check_data(&revision)?;
  Ok(revision)
}

/// Version 04's fields after the revision flags: the delta information and the protocol flags between them.
fn parse_info(cursor: &mut Cursor<'_>) -> Result<(DeltaInfo, u8), Truncated> {
  let snapshot_level = cursor.i8("snapshot level")?;
  let raw_size = cursor.u32("raw size")?;
  let compression = cursor.u8("wire compression")?;
  let protocol_flags = cursor.u8("protocol flags")?;
  let stored_base = cursor.node("stored delta base")?;
  let stored_snapshot_level = cursor.i8("stored snapshot level")?;
  Ok((
    DeltaInfo {
      snapshot_level,
      raw_size,
      compression,
      stored_base,
      stored_snapshot_level,
    },
    protocol_flags,
  ))
}

/// Checks a revision's delta by mpatch's decoding rules when it travels as plain hunks.
fn check_data(revision: &Revision<'_>) -> Result<(), Refusal> {
  if revision.is_plain_delta() {
    delta::check(revision.data).map_err(|refusal| Refusal::Delta {
      node: revision.node,
      refusal,
    })?;
  }
  Ok(())
}

/// Writes one group's revisions and its empty chunk.
fn encode_group(
  revisions: &[Revision<'_>],
  version: Version,
  out: &mut impl Out,
) -> Result<(), EncodeRefusal> {
  let mut previous: Option<Node> = None;
  for revision in revisions {
    check_encodable(revision, version, previous)?;
    let header = header_bytes(revision, version);
    put_chunk_length(out, header.len().saturating_add(revision.data.len()))?;
    out.put(&header);
    out.put(revision.data);
    if let Some(sidedata) = revision.sidedata {
      put_sidedata(out, sidedata)?;
    }
    previous = Some(revision.node);
  }
  out.put(&EMPTY.to_be_bytes());
  Ok(())
}

/// Writes a sidedata chunk: the empty chunk for none, as [`chunk`] reads it back.
fn put_sidedata(out: &mut impl Out, sidedata: &[u8]) -> Result<(), EncodeRefusal> {
  if sidedata.is_empty() {
    out.put(&EMPTY.to_be_bytes());
    return Ok(());
  }
  put_chunk_length(out, sidedata.len())?;
  out.put(sidedata);
  Ok(())
}

/// Writes a chunk's length: its data's and its own four bytes, as a signed 32-bit integer.
fn put_chunk_length(out: &mut impl Out, data: usize) -> Result<(), EncodeRefusal> {
  let len = data.saturating_add(LENGTH_LEN);
  let value = i32::try_from(len).map_err(|_| EncodeRefusal::TooLong { len: to_u64(len) })?;
  out.put(&value.to_be_bytes());
  Ok(())
}

/// Refuses a revision whose fields its version cannot carry.
fn check_encodable(
  revision: &Revision<'_>,
  version: Version,
  previous: Option<Node>,
) -> Result<(), EncodeRefusal> {
  let unencodable = |reason| EncodeRefusal::Unencodable {
    node: revision.node,
    reason,
  };
  if version == Version::V01 && revision.base != previous.unwrap_or(revision.p1) {
    return Err(unencodable(
      "version 01 deltas apply to the previous revision of their group, or the first's p1",
    ));
  }
  if version < Version::V03 && revision.flags != 0 {
    return Err(unencodable("revision flags travel from version 03"));
  }
  check_protocol_fields(revision, version).map_err(unencodable)
}

/// Refuses delta information outside version 04, protocol flags before it, and sidedata outside version 05's flag.
fn check_protocol_fields(revision: &Revision<'_>, version: Version) -> Result<(), &'static str> {
  if (version == Version::V04) != revision.info.is_some() {
    return Err("delta information travels in version 04, and always there");
  }
  if version < Version::V04 && revision.protocol_flags != 0 {
    return Err("protocol flags travel from version 04");
  }
  let flagged = version == Version::V05 && revision.protocol_flags & FLAG_SIDEDATA != 0;
  if flagged != revision.sidedata.is_some() {
    return Err("sidedata travels in version 05, behind its flag");
  }
  Ok(())
}

/// A revision's header bytes in its version's layout.
fn header_bytes(revision: &Revision<'_>, version: Version) -> Vec<u8> {
  let mut header = Vec::with_capacity(version.header_len());
  if version == Version::V05 {
    header.push(revision.protocol_flags);
  }
  for node in [revision.node, revision.p1, revision.p2] {
    header.extend_from_slice(&node);
  }
  if version != Version::V01 {
    header.extend_from_slice(&revision.base);
  }
  header.extend_from_slice(&revision.link);
  if version >= Version::V03 {
    header.extend_from_slice(&revision.flags.to_be_bytes());
  }
  if let Some(info) = revision.info {
    header.extend_from_slice(&info.snapshot_level.to_be_bytes());
    header.extend_from_slice(&info.raw_size.to_be_bytes());
    header.push(info.compression);
    header.push(revision.protocol_flags);
    header.extend_from_slice(&info.stored_base);
    header.extend_from_slice(&info.stored_snapshot_level.to_be_bytes());
  }
  header
}

#[cfg(test)]
#[path = "tests/changegroup.rs"]
mod tests;

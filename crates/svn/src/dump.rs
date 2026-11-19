//! The dump format (§4.17.4 "Pieces that follow", piece 6, its first part; A-593): Subversion's portable stream of a
//! repository's history, read as its bytes arrive within bounds the caller gives, and written byte for byte as
//! `svnadmin dump` writes it, in format 2 (every text whole) and format 3 (`--deltas`: texts as svndiff against their
//! base, property sections as changes to the base's list).
//!
//! The format (`notes/dump-load-format.txt`; its writer `libsvn_repos/dump.c`, its reader `load.c` and
//! `load-fs-vtable.c`, the property sections `libsvn_subr/hash.c`; Subversion's `1.14.x` branch at `d434dc14`, whose
//! `dump.c` and `hash.c` Subversion 1.15.0 keeps byte for byte; `docs/wip/research/vcs-servers-subversion.md` §11): a
//! stream is a version record, then records, each a block of `Name: value` lines ended by an empty line and, for some, a
//! content block whose length its headers state. A UUID record names the repository; a revision record carries the
//! revision's properties; each node record that follows it adds, deletes, replaces or changes one path, with a property
//! section, a text, or both. A property section is `K`, `V` and (format 3) `D` entries, each a length line and that
//! many bytes, ended by `PROPS-END`.
//!
//! What this module holds to:
//! - **Written as `svnadmin` writes.** [`Writer`] writes each record's headers in the order `dump.c` pushes them, the
//!   blank lines `dump.c` puts after each kind of record, property entries in the order the caller gives them
//!   (`svnadmin` gives them sorted by name, `hash.c` `svn_sort__hash`), checksums in lowercase hex, and a replacement by
//!   a copy as `svnadmin` does: a deletion record ended by its header block alone, then an addition record with the copy
//!   source (`dump.c` 1234-1274). The golden vectors in `fixtures/dump`, which stock `svnadmin` 1.15.0 wrote, are read
//!   and written again byte for byte.
//! - **Read as `svnadmin load` reads, and stricter where Subversion is silent.** [`Reader`] accepts what `load.c`
//!   accepts (headers in any order, unknown headers ignored, blank lines and lines that begin with whitespace between
//!   records, a version record in the middle of a stream, as two dumps written one after the other are) and refuses what
//!   it refuses, each with Subversion's own error number ([`DumpError::subversion_code`]). It refuses more where
//!   Subversion would read on and get the history wrong: a number that is not decimal digits (`load.c` reads lengths
//!   with `apr_atoi64` and revisions with `atol`, which stop at the first other byte); a node kind or a delta flag it
//!   does not know (`load-fs-vtable.c` reads an unknown kind as none, and then adds nothing); a header given twice; a NUL
//!   in a header or a property name (Subversion's C strings end there); a property section whose length ends anywhere
//!   but right after its `PROPS-END` (`load.c` stops at a `PROPS-END` wherever it comes, and at the stated length with
//!   none); a header block the stream's end cuts (`load.c` reads the end as the block's empty line); a version other
//!   than 2 or 3; and headers that contradict each other or the stream ([`Contradiction`]).
//! - **A replacement is one record.** A deletion followed by an addition of the same path is read as one replacement
//!   ([`NodeAction::Replace`]), which is how `load-fs-vtable.c` applies the two (a deletion, then the addition) and how
//!   FSFS records the revision's changed path (one replacement row, §4.17.4 item 2). So the reader holds a deletion
//!   until the next record shows whether it is the first half of one.
//! - **No number of its own.** The caller's [`Admission`] bounds the bytes the reader asks it to hold at once; every
//!   length the stream states is checked against it, and against the section that holds it, when it is read, before one
//!   of the bytes it names is waited for. Texts pass in pieces of whatever the caller holds and are never held whole;
//!   a property entry and an svndiff window are held whole, as Subversion holds them.
//! - **Checksums checked where the stream allows.** A whole text's MD5 and SHA-1 are computed as it passes and compared
//!   with its headers (an all-zero digest names none, as `svn_checksum_parse_hex` reads it); a delta's result is the
//!   applier's to check, with [`Digests`]. Both digests go through aws-lc-rs, the workspace's one cryptographic
//!   library, which exposes MD5 (`vendor/UPSTREAM.md`).
//!
//! No I/O, no clock, no randomness, no panic.

mod digest;
mod header;
mod read;
mod write;

#[cfg(test)]
mod tests;

pub use digest::{Digests, check_text};
pub use header::Header;
pub use read::Reader;
pub use write::Writer;

use crate::index::{NodeKind, Revision};
use crate::svndiff::{SvndiffError, Version, Window};

/// An MD5 digest, as `Text-content-md5` and its siblings carry it.
pub type Md5 = [u8; 16];

/// A SHA-1 digest, as `Text-content-sha1` and its siblings carry it.
pub type Sha1 = [u8; 20];

/// The line that ends every property section, its line feed included.
/// Format: `svn_hash_write2(..., "PROPS-END", ...)` writes the terminator and a line feed (`hash.c` 319;
/// `notes/dump-load-format.txt`: "The fixed trailer is "PROPS-END\n" and its length is included in the
/// Prop-content-length").
const PROPS_END_LINE: &[u8] = b"PROPS-END\n";

/// The byte that ends every line of the format.
/// Format: `notes/dump-load-format.txt`, "Encoding and delimiters"; `load.c` reads every line up to `"\n"`.
const LINE_FEED: u8 = b'\n';

/// The largest number the format carries: a revision number, a length or a format version.
/// Format: Subversion holds each in a signed 64-bit integer (`svn_filesize_t`, `apr_int64_t`; `svn_revnum_t`; and
/// `svn_cstring_strtoui64` reads property lengths through `apr_strtoi64`), so 2^63 - 1 is the most any reader takes.
const NUMBER_LIMIT: u64 = i64::MAX.unsigned_abs();

/// The most digits a number the format carries takes.
/// Format: 2^63 - 1, 9,223,372,036,854,775,807, has nineteen digits.
const NUMBER_DIGITS: usize = 19;

/// The number of format 2.
/// Format: `SVN_REPOS_DUMPFILE_FORMAT_VERSION` before deltas; `svnadmin dump` without `--deltas` writes it.
const FORMAT_TWO: u64 = 2;

/// The number of format 3, the newest.
/// Format: `SVN_REPOS_DUMPFILE_FORMAT_VERSION` and `SVN_REPOS_DUMPFILE_FORMAT_VERSION_DELTAS` (`svn_repos.h` 3086-3087).
const FORMAT_THREE: u64 = 3;

/// A UUID's length in its record.
/// Format: RFC 4122's form, which `apr_uuid_parse` reads: 32 hex digits and four `-` (`svn_fs_set_uuid`).
const UUID_LEN: usize = 36;

/// Where a UUID's four `-` stand.
/// Format: RFC 4122's 8-4-4-4-12 form (`apr_uuid_parse` checks the `-` at these places).
const UUID_DASHES: [usize; 4] = [8, 13, 18, 23];

/// A dump's format, the number its version record carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
  /// Format 2: every text whole, and every property section a node's whole list.
  Two,
  /// Format 3: a text may be an svndiff delta against its base, and a property section a change to the base's list.
  Three,
}

impl Format {
  /// The number the version record carries.
  pub fn number(self) -> u64 {
    match self {
      Self::Two => FORMAT_TWO,
      Self::Three => FORMAT_THREE,
    }
  }

  /// The format a version record's number names, if it is one this module reads.
  pub fn from_number(number: u64) -> Option<Self> {
    match number {
      FORMAT_TWO => Some(Self::Two),
      FORMAT_THREE => Some(Self::Three),
      _ => None,
    }
  }

  /// Whether the format carries deltas.
  fn has_deltas(self) -> bool {
    self == Self::Three
  }
}

/// A repository's UUID as its record carries it: 36 bytes, hex digits with a `-` at 8, 13, 18 and 23, the case kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uuid([u8; UUID_LEN]);

impl Uuid {
  /// The UUID `text` holds, if it is one: exactly 36 bytes in RFC 4122's form.
  pub fn parse(text: &[u8]) -> Option<Self> {
    let bytes: [u8; UUID_LEN] = text.try_into().ok()?;
    let well_formed = bytes.iter().enumerate().all(|(place, &byte)| {
      if UUID_DASHES.contains(&place) {
        byte == b'-'
      } else {
        byte.is_ascii_hexdigit()
      }
    });
    well_formed.then_some(Self(bytes))
  }

  /// The UUID's bytes, as its record carries them.
  pub fn as_bytes(&self) -> &[u8] {
    &self.0
  }
}

/// A digest a text was checked against: which algorithm and its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Digest {
  /// An MD5 digest.
  Md5(Md5),
  /// A SHA-1 digest.
  Sha1(Sha1),
}

/// A revision record's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionHeader {
  /// The revision's number.
  pub number: Revision,
  /// The length of its property section, `PROPS-END` and its line feed included, when it has one. `svnadmin` gives
  /// every revision one, empty or not.
  pub properties: Option<u64>,
}

/// What a node record does to its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAction {
  /// Changes the node's text, its properties or both.
  Change,
  /// Adds a node, or a copy, where there was none.
  Add,
  /// Deletes the node and everything beneath it.
  Delete,
  /// Deletes the node and adds another, or a copy, in its place.
  Replace,
}

impl NodeAction {
  /// The word `Node-action` carries.
  pub fn word(self) -> &'static [u8] {
    match self {
      Self::Change => b"change",
      Self::Add => b"add",
      Self::Delete => b"delete",
      Self::Replace => b"replace",
    }
  }

  /// The action a `Node-action` word names, if it is one.
  pub fn from_word(word: &[u8]) -> Option<Self> {
    [Self::Change, Self::Add, Self::Delete, Self::Replace]
      .into_iter()
      .find(|action| action.word() == word)
  }
}

/// Where a copy was made from, with the checksums of the source's text when it is a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyFrom {
  /// The source's revision (`Node-copyfrom-rev`).
  pub revision: Revision,
  /// The source's path (`Node-copyfrom-path`).
  pub path: Vec<u8>,
  /// The MD5 of the source's text (`Text-copy-source-md5`).
  pub md5: Option<Md5>,
  /// The SHA-1 of the source's text (`Text-copy-source-sha1`).
  pub sha1: Option<Sha1>,
}

/// A node record's property section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertySection {
  /// Whether the section is a change to the base's list (`Prop-delta: true`, format 3), which may delete entries; a
  /// section that is no delta is the node's whole list.
  pub delta: bool,
  /// The section's length, `PROPS-END` and its line feed included (`Prop-content-length`).
  pub length: u64,
}

/// The base a text delta applies to, by its checksums.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeltaBase {
  /// The base's MD5 (`Text-delta-base-md5`); `svnadmin` writes none when the base is the empty text of an addition.
  pub md5: Option<Md5>,
  /// The base's SHA-1 (`Text-delta-base-sha1`).
  pub sha1: Option<Sha1>,
}

/// A node record's text section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSection {
  /// The base, when the text is an svndiff delta against it (`Text-delta: true`, format 3); the whole text otherwise.
  pub delta: Option<DeltaBase>,
  /// The MD5 of the node's resulting text (`Text-content-md5`).
  pub md5: Option<Md5>,
  /// The SHA-1 of the node's resulting text (`Text-content-sha1`).
  pub sha1: Option<Sha1>,
  /// The section's length in the stream: the text's, or the delta's (`Text-content-length`).
  pub length: u64,
}

/// A node record's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeHeader {
  /// The node's path, relative to the repository's root; empty for the root (`Node-path`).
  pub path: Vec<u8>,
  /// The node's kind (`Node-kind`); `svnadmin` writes none for a deletion.
  pub kind: Option<NodeKind>,
  /// What the record does.
  pub action: NodeAction,
  /// The copy source of an addition or a replacement made by a copy.
  pub copy_from: Option<CopyFrom>,
  /// The property section, when the record has one.
  pub properties: Option<PropertySection>,
  /// The text section, when the record has one.
  pub text: Option<TextSection>,
}

/// One entry of a property section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property<'a> {
  /// Sets `name` to `value` (a `K` and a `V` entry).
  Set {
    /// The property's name.
    name: &'a [u8],
    /// Its value: any bytes.
    value: &'a [u8],
  },
  /// Deletes `name` from the base's list (a `D` entry, in a format-3 section that is a delta).
  Delete {
    /// The property's name.
    name: &'a [u8],
  },
}

impl Property<'_> {
  /// The property's name.
  pub fn name(&self) -> &[u8] {
    match self {
      Self::Set { name, .. } | Self::Delete { name } => name,
    }
  }

  /// The bytes the entry takes in its section, its length lines and line feeds included; `None` past 2^64 - 1.
  pub fn encoded_len(&self) -> Option<u64> {
    match self {
      Self::Set { name, value } => length_entry_len(name)?.checked_add(length_entry_len(value)?),
      Self::Delete { name } => length_entry_len(name),
    }
  }
}

/// The length of a property section holding `properties` in order, `PROPS-END` and its line feed included: what a
/// writer's caller states as the section's length. `None` past 2^64 - 1.
pub fn section_len<'a, I: IntoIterator<Item = Property<'a>>>(properties: I) -> Option<u64> {
  let mut length = to_u64(PROPS_END_LINE.len());
  for property in properties {
    length = length.checked_add(property.encoded_len()?)?;
  }
  Some(length)
}

/// The bytes one length entry takes: a two-byte prefix (`K `, `V ` or `D `), the length's digits, a line feed, the
/// bytes, and a line feed.
fn length_entry_len(bytes: &[u8]) -> Option<u64> {
  let count = to_u64(bytes.len());
  decimal_len(count)
    .checked_add(count)?
    .checked_add(LENGTH_ENTRY_FRAME)
}

/// The bytes a length entry adds around its digits and its bytes: its two-byte prefix and its two line feeds.
/// Format: `hash.c` 288-293 writes `K <length>\n<key>\nV <length>\n` and the value and `\n`; 312-314 `D <length>\n<key>\n`.
const LENGTH_ENTRY_FRAME: u64 = 4;

/// The decimal digits of `number`.
fn decimal_len(number: u64) -> u64 {
  number
    .checked_ilog10()
    .map_or(1, |exponent| u64::from(exponent).saturating_add(1))
}

/// One part of a dump, read or to be written. A record's header comes first; its property entries, then its text in
/// pieces or as a delta's windows, follow as the header's sections say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event<'a> {
  /// A version record: the format of what follows.
  Format(Format),
  /// A UUID record.
  Uuid(Uuid),
  /// A revision record's header.
  Revision(RevisionHeader),
  /// A node record's header.
  Node(NodeHeader),
  /// One entry of the current record's property section.
  Property(Property<'a>),
  /// The end of the property section (`PROPS-END`).
  PropertiesEnd,
  /// A piece of a whole text, in order.
  Text(&'a [u8]),
  /// The start of a text delta: its svndiff version.
  Delta(Version),
  /// One window of the text delta.
  Window(Window),
  /// The end of the text: a whole text's checksums were checked; a delta's svndiff ended between windows.
  TextEnd,
}

/// What one call of [`Reader::read`] made of the buffer it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Read<'a> {
  /// One event was read; the caller drops the first `consumed` bytes once it is done with the event, which borrows
  /// from them.
  Event {
    /// The event.
    event: Event<'a>,
    /// The bytes, from the start of the buffer, the reader is done with.
    consumed: usize,
  },
  /// The buffer ends before the next event: the caller drops the first `consumed` bytes, then calls again with at least
  /// `needed` bytes from the new start, or with the stream's last bytes and `ended` set.
  NeedMore {
    /// The bytes, from the start of the buffer, the reader is done with.
    consumed: usize,
    /// How many bytes from the new start the next event needs at least, never more than the admission.
    needed: usize,
  },
  /// The stream ended between records.
  End,
}

/// The bound a dump is read within, the caller's: the most bytes the reader may ask it to hold at once. The longest
/// line, property entry or svndiff window a dump may hold is read within it, and anything longer is refused when its
/// length is read. The caller derives it from its own room; nothing here sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admission {
  /// The most bytes the reader asks its caller to hold at once.
  pub buffer_bytes: usize,
}

/// Why a dump, or an event given to the writer, was refused. Each carries the number of the Subversion error
/// `svnadmin load` answers with ([`DumpError::subversion_code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpError {
  /// The stream ended inside a record (`SVN_ERR_INCOMPLETE_DATA`, "Premature end of content data in dumpstream",
  /// `load.c` `stream_ran_dry`).
  Truncated,
  /// Reading on would need the caller to hold more bytes at once than it admitted (`SVN_ERR_INCORRECT_PARAMS`, the
  /// error Subversion answers a property length past its range with).
  PastAdmission {
    /// The bytes the next part needs held.
    needed: usize,
    /// The bytes the caller admitted.
    admitted: usize,
  },
  /// The stream does not begin with a version line (`SVN_ERR_STREAM_MALFORMED_DATA`, "Malformed dumpfile header").
  MalformedVersion,
  /// A version record of a format other than 2 or 3 (`SVN_ERR_STREAM_MALFORMED_DATA`, "Unsupported dumpfile version").
  UnsupportedVersion {
    /// The version it names.
    version: u64,
  },
  /// A header line that is not a name, `: ` and a value, or that holds a NUL (`SVN_ERR_STREAM_MALFORMED_DATA`,
  /// "Dump stream contains a malformed header").
  MalformedHeader,
  /// A header given twice in one record (`SVN_ERR_STREAM_MALFORMED_DATA`).
  DuplicateHeader {
    /// The header.
    header: Header,
  },
  /// A header's value the format does not allow, or one the writer cannot write: a line feed or a NUL in it, or a
  /// number past 2^63 - 1 ([`DumpError::subversion_code`] names the error by header).
  MalformedValue {
    /// The header.
    header: Header,
  },
  /// A record without a header it must have: a node record's `Node-action` (`SVN_ERR_STREAM_UNRECOGNIZED_DATA`,
  /// "Unrecognized node-action on node"), an addition's or a replacement's `Node-kind`
  /// (`SVN_ERR_STREAM_MALFORMED_DATA`).
  MissingHeader {
    /// The header.
    header: Header,
  },
  /// A record that names none of the four kinds (`SVN_ERR_STREAM_MALFORMED_DATA`, "Unrecognized record type in
  /// stream").
  UnrecognizedRecord,
  /// A record whose headers contradict each other or the stream (`SVN_ERR_STREAM_MALFORMED_DATA`).
  Contradiction {
    /// How.
    fault: Contradiction,
  },
  /// A property section that does not hold together.
  Property {
    /// How.
    fault: PropertyFault,
  },
  /// A text whose digest differs from the one its header names (`SVN_ERR_CHECKSUM_MISMATCH`).
  ChecksumMismatch {
    /// The digest its header names.
    expected: Digest,
    /// The text's.
    actual: Digest,
  },
  /// A text delta svndiff refused (its own error).
  Svndiff(SvndiffError),
  /// aws-lc-rs could not compute a digest (`SVN_ERR_BASE`; it fails only where it cannot allocate its context).
  DigestFailed,
  /// An event the writer cannot take where the stream it writes stands: a node record outside a revision, a
  /// property or a text where no section is open or past its stated length, a section ended before its stated length
  /// (`SVN_ERR_STREAM_MALFORMED_DATA`, since what would be written is malformed).
  OutOfOrder,
}

/// How a record's headers contradict each other or the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contradiction {
  /// A record that names two kinds: two of `SVN-fs-dump-format-version`, `UUID`, `Revision-number` and `Node-path`.
  MixedRecord,
  /// A header of another kind of record, or a section on a version or UUID record (a text or a `Prop-delta` on a
  /// revision record among them).
  HeaderOutOfPlace,
  /// One of `Node-copyfrom-rev` and `Node-copyfrom-path` without the other.
  CopySourceIncomplete,
  /// A copy source on a change or a deletion, which the format's tables forbid (`notes/dump-load-format.txt`, "Can
  /// have copy source? no").
  CopySourceOutOfPlace,
  /// `Text-copy-source-md5` or `Text-copy-source-sha1` without a copy source.
  CopyChecksumWithoutCopy,
  /// A deletion with a property or text section, or a `Content-length`.
  ContentOnDelete,
  /// A text on a directory.
  TextOnDirectory,
  /// `Prop-delta: true` or `Text-delta: true` in a format-2 stream, which has no deltas.
  DeltaInFormatTwo,
  /// `Prop-delta` without `Prop-content-length`, or `Text-delta` without `Text-content-length`.
  DeltaWithoutSection,
  /// `Text-delta-base-md5` or `Text-delta-base-sha1` without `Text-delta: true`.
  DeltaBaseWithoutDelta,
  /// `Text-content-md5` or `Text-content-sha1` without `Text-content-length`.
  TextChecksumWithoutText,
  /// A `Content-length` below the sections' lengths (`load.c`: "Sum of subblock sizes larger than total block content
  /// length").
  ContentLengthShort,
  /// A property section too short to hold `PROPS-END`.
  PropertySectionTooShort,
  /// A node record before any revision record.
  NodeOutsideRevision,
  /// A node record in revision 0, which is the empty root (`load-fs-vtable.c`: "Revision 0 must not contain node
  /// records").
  NodeInRevisionZero,
}

/// How a property section does not hold together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyFault {
  /// The section's stated length ran out before `PROPS-END` (`load.c`: "Incomplete or unterminated property block").
  Unterminated,
  /// `PROPS-END` came before the section's stated length ran out.
  EndsEarly,
  /// An entry runs past the section's stated length.
  PastSection,
  /// A line other than a `K`, `V` or `D` entry's or `PROPS-END` where one is due (`load.c` `stream_malformed`).
  MalformedLine,
  /// A `K`, `V` or `D` length that is not decimal digits or is past 2^63 - 1 (`SVN_ERR_INCORRECT_PARAMS`, as
  /// `svn_cstring_strtoui64` answers).
  MalformedLength,
  /// A name or a value not followed by a line feed (`load.c` `read_key_or_val`).
  MalformedData,
  /// A `D` entry in a revision's properties or in a section that is no delta.
  DeletionOutOfPlace,
  /// A property name holding a NUL, which ends Subversion's C string of it.
  NameNul,
}

/// `SVN_ERR_BAD_UUID`.
/// Format: the bad category's start, 125,000, plus 8 (`svn_error_codes.h` 202-204; research notes §6).
const SVN_ERR_BAD_UUID: u32 = 125_008;
/// `SVN_ERR_BAD_CHECKSUM_PARSE`.
/// Format: the bad category's start plus 12 (`svn_error_codes.h` 219-221).
const SVN_ERR_BAD_CHECKSUM_PARSE: u32 = 125_012;
/// `SVN_ERR_STREAM_MALFORMED_DATA`.
/// Format: the stream category's start, 140,000, plus 1 (`svn_error_codes.h` 319-321).
const SVN_ERR_STREAM_MALFORMED_DATA: u32 = 140_001;
/// `SVN_ERR_STREAM_UNRECOGNIZED_DATA`.
/// Format: the stream category's start plus 2 (`svn_error_codes.h` 323-325).
const SVN_ERR_STREAM_UNRECOGNIZED_DATA: u32 = 140_002;
/// `SVN_ERR_BASE`.
/// Format: the miscellaneous category's start, 200,000 (`svn_error_codes.h` 1281-1283).
const SVN_ERR_BASE: u32 = 200_000;
/// `SVN_ERR_INCOMPLETE_DATA`.
/// Format: the miscellaneous category's start plus 3 (`svn_error_codes.h` 1293-1295).
const SVN_ERR_INCOMPLETE_DATA: u32 = 200_003;
/// `SVN_ERR_INCORRECT_PARAMS`.
/// Format: the miscellaneous category's start plus 4 (`svn_error_codes.h` 1297-1299).
const SVN_ERR_INCORRECT_PARAMS: u32 = 200_004;
/// `SVN_ERR_CHECKSUM_MISMATCH`.
/// Format: the miscellaneous category's start plus 14 (`svn_error_codes.h` 1337-1339).
const SVN_ERR_CHECKSUM_MISMATCH: u32 = 200_014;

impl DumpError {
  /// The number of the Subversion error this refusal is answered with, which `svnadmin` prints after an `E`.
  pub fn subversion_code(self) -> u32 {
    match self {
      Self::Truncated => SVN_ERR_INCOMPLETE_DATA,
      Self::PastAdmission { .. } => SVN_ERR_INCORRECT_PARAMS,
      Self::MalformedValue { header } => header.value_error(),
      Self::MissingHeader {
        header: Header::NodeAction,
      } => SVN_ERR_STREAM_UNRECOGNIZED_DATA,
      Self::Property {
        fault: PropertyFault::MalformedLength,
      } => SVN_ERR_INCORRECT_PARAMS,
      Self::ChecksumMismatch { .. } => SVN_ERR_CHECKSUM_MISMATCH,
      Self::Svndiff(refusal) => refusal.subversion_code(),
      Self::DigestFailed => SVN_ERR_BASE,
      Self::MalformedVersion
      | Self::UnsupportedVersion { .. }
      | Self::MalformedHeader
      | Self::DuplicateHeader { .. }
      | Self::MissingHeader { .. }
      | Self::UnrecognizedRecord
      | Self::Contradiction { .. }
      | Self::Property { .. }
      | Self::OutOfOrder => SVN_ERR_STREAM_MALFORMED_DATA,
    }
  }
}

impl Header {
  /// The Subversion error a malformed value of this header is answered with: the one Subversion's own parse of the
  /// value raises where it has one (a checksum's, `svn_checksum_parse_hex`; a UUID's, `svn_fs_set_uuid`; a node
  /// action's, `load-fs-vtable.c`), and the stream's otherwise.
  fn value_error(self) -> u32 {
    match self {
      Self::TextCopySourceMd5
      | Self::TextCopySourceSha1
      | Self::TextDeltaBaseMd5
      | Self::TextDeltaBaseSha1
      | Self::TextContentMd5
      | Self::TextContentSha1 => SVN_ERR_BAD_CHECKSUM_PARSE,
      Self::Uuid => SVN_ERR_BAD_UUID,
      Self::NodeAction | Self::NodeKind => SVN_ERR_STREAM_UNRECOGNIZED_DATA,
      Self::FormatVersion
      | Self::RevisionNumber
      | Self::NodePath
      | Self::NodeCopyfromRev
      | Self::NodeCopyfromPath
      | Self::PropDelta
      | Self::TextDelta
      | Self::PropContentLength
      | Self::TextContentLength
      | Self::ContentLength => SVN_ERR_STREAM_MALFORMED_DATA,
    }
  }
}

/// Refuses a revision header the format cannot carry: a number past 2^63 - 1, or a property section too short to hold
/// `PROPS-END` or past 2^63 - 1. What the reader reads and what the writer writes are held to the same rules.
fn check_revision(revision: &RevisionHeader) -> Result<(), DumpError> {
  check_number(revision.number, Header::RevisionNumber)?;
  if let Some(length) = revision.properties {
    check_property_length(length)?;
  }
  Ok(())
}

/// Refuses a node header whose parts contradict each other or the stream's `format`, or that the format cannot carry
/// (a line feed or a NUL in a path, a number past 2^63 - 1, sections whose lengths sum past it). What the reader reads
/// and what the writer writes are held to the same rules.
fn check_node(node: &NodeHeader, format: Format) -> Result<(), DumpError> {
  check_value(&node.path, Header::NodePath)?;
  if let Some(copy) = &node.copy_from {
    check_value(&copy.path, Header::NodeCopyfromPath)?;
    check_number(copy.revision, Header::NodeCopyfromRev)?;
  }
  check_action(node)?;
  check_sections(node, format)?;
  content_length(node).map(drop)
}

/// Refuses what a node's action forbids: a copy source on a change or a deletion, a section on a deletion, an addition
/// or a replacement of no kind.
fn check_action(node: &NodeHeader) -> Result<(), DumpError> {
  let refuse = |fault| Err(DumpError::Contradiction { fault });
  match node.action {
    NodeAction::Delete if node.properties.is_some() || node.text.is_some() => {
      refuse(Contradiction::ContentOnDelete)
    }
    NodeAction::Delete | NodeAction::Change if node.copy_from.is_some() => {
      refuse(Contradiction::CopySourceOutOfPlace)
    }
    NodeAction::Add | NodeAction::Replace if node.kind.is_none() => Err(DumpError::MissingHeader {
      header: Header::NodeKind,
    }),
    NodeAction::Change | NodeAction::Add | NodeAction::Delete | NodeAction::Replace => Ok(()),
  }
}

/// Refuses sections the node or the format cannot hold: a text on a directory, a delta in format 2, a property section
/// too short for `PROPS-END`, a length past 2^63 - 1.
fn check_sections(node: &NodeHeader, format: Format) -> Result<(), DumpError> {
  let refuse = |fault| Err(DumpError::Contradiction { fault });
  if let Some(properties) = node.properties {
    check_property_length(properties.length)?;
    if properties.delta && !format.has_deltas() {
      return refuse(Contradiction::DeltaInFormatTwo);
    }
  }
  if let Some(text) = node.text {
    check_number(text.length, Header::TextContentLength)?;
    if node.kind == Some(NodeKind::Directory) {
      return refuse(Contradiction::TextOnDirectory);
    }
    if text.delta.is_some() && !format.has_deltas() {
      return refuse(Contradiction::DeltaInFormatTwo);
    }
  }
  Ok(())
}

/// A count of bytes held in memory, as the format's numbers count: exact wherever `usize` is at most 64 bits, as on
/// every target slates builds for; past that it saturates, and every check of it then refuses.
fn to_u64(count: usize) -> u64 {
  u64::try_from(count).unwrap_or(u64::MAX)
}

/// Refuses a property section's length too short to hold `PROPS-END` or past 2^63 - 1.
fn check_property_length(length: u64) -> Result<(), DumpError> {
  check_number(length, Header::PropContentLength)?;
  if length < to_u64(PROPS_END_LINE.len()) {
    return Err(DumpError::Contradiction {
      fault: Contradiction::PropertySectionTooShort,
    });
  }
  Ok(())
}

/// The content block's length the node's sections make (`Content-length`), when it has any; refused past 2^63 - 1.
fn content_length(node: &NodeHeader) -> Result<Option<u64>, DumpError> {
  if node.properties.is_none() && node.text.is_none() {
    return Ok(None);
  }
  let properties = node.properties.map_or(0, |section| section.length);
  let text = node.text.map_or(0, |section| section.length);
  let total = properties
    .checked_add(text)
    .filter(|&total| total <= NUMBER_LIMIT)
    .ok_or(DumpError::MalformedValue {
      header: Header::ContentLength,
    })?;
  Ok(Some(total))
}

/// Refuses a number past 2^63 - 1 as `header`'s value.
fn check_number(number: u64, header: Header) -> Result<(), DumpError> {
  if number > NUMBER_LIMIT {
    return Err(DumpError::MalformedValue { header });
  }
  Ok(())
}

/// Refuses a header value that holds a line feed, which would end its line, or a NUL, which the reader refuses.
fn check_value(value: &[u8], header: Header) -> Result<(), DumpError> {
  if value.contains(&LINE_FEED) || value.contains(&0) {
    return Err(DumpError::MalformedValue { header });
  }
  Ok(())
}

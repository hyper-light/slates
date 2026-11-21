//! A revlog read as stock Mercurial 7.2 reads one (A-611; §4.17.3 piece 3, the import of an existing store;
//! `internals.revlogs` \[B\]; `revlog.py`, `revlogutils/constants.py`, `revlogutils/docket.py`,
//! `revlogutils/deltas.py` `delta_compression`, `pure/parsers.py` \[C\]): the index's entries, each revision's chunk
//! decompressed, its delta chain rebuilt into its text, and every text checked against its node.
//!
//! **The index.** Its first four bytes are a header (the low 16 bits the version, the high 16 its flags), which share
//! the first entry's offset field, so the first entry's offset reads as 0. Version 1's entry is 64 bytes: the data's
//! offset (48 bits) and the revision's flags (16), the chunk's length, the text's length, the delta base, the link, the
//! two parents (-1 for none) and the node, all big-endian. An inline revlog (flag bit 16) keeps each revision's chunk
//! right after its entry in the index file; any other keeps the chunks in a data file at their offsets. Generaldelta
//! (bit 17) makes the base a delta's parent; without it a delta is against the revision before. Version 2 (`0xDEAD`,
//! 7.2's `exp-revlogv2.2`) has a docket for its `.i` naming an index file of 96-byte entries (version 1's, then the
//! sidedata's place and a byte of compression modes) and a data file; the changelog's version 2 (`0xD34D`) keeps no
//! base and no link, every revision a full text.
//!
//! **A chunk** is routed by its first byte: `x` is a zlib stream (its header), `(` a zstd frame (its magic), `u` raw
//! bytes after the `u`, and NUL raw bytes, the NUL kept; empty is empty. Version 2 records a mode per chunk instead:
//! plain (raw, no header), the docket's default engine (the frame as written), or routed as version 1's. A compressed
//! chunk is inflated by the crate's one inflater, the one compressed bundles use (`compression::inflate`, A-598, on
//! A-607's engines): the decoder's memory and every byte of room charged to the caller's admission first, and a stream
//! cut short, corrupt, or not ending exactly at the chunk's end refused by kind.
//!
//! **A text** is its chain's base chunk patched by each later chunk in order (7.2's `_deltachain`: an empty delta
//! passed over), through slates-hg's own delta applier, every patched text charged to the admission; it must be as long
//! as its entry says and hash to its node with its parents' nodes (`node::check`, SHA-1 with collision detection).
//!
//! **Stricter than 7.2, on purpose.** Anything 7.2 would read as garbage or loop on is refused by kind: a negative
//! length (7.2's inline scan would step backwards), an inline entry whose offset disagrees with the chunks before it, a
//! base or a parent after its revision, a chunk past its file, bytes after a compressed stream, version 0, and revision
//! flags whose texts do not hash to their nodes (censored, ellipsis, externally stored: owed with their pieces).

use std::borrow::Cow;

use crate::compression::{self, Engine, StreamRefusal};
use crate::delta;
use crate::node;
use crate::{Admission, NODE_LEN, NULL_NODE, Node, PastAdmission};

/// Format: the header's bytes, which overlap the first entry.
const HEADER_BYTES: usize = 4;

/// Format: the header's version, its low 16 bits.
const VERSION_MASK: u32 = 0xFFFF;

/// Format: version 1.
const VERSION_1: u32 = 1;

/// Format: version 2 (`REVLOGV2`).
const VERSION_2: u32 = 0xDEAD;

/// Format: the changelog's version 2 (`CHANGELOGV2`).
const CHANGELOG_V2: u32 = 0xD34D;

/// Format: the inline flag.
const FLAG_INLINE: u32 = 1 << 16;

/// Format: the generaldelta flag.
const FLAG_GENERAL_DELTA: u32 = 1 << 17;

/// Format: the flags version 1 may carry (`REVLOGV1_FLAGS`).
const V1_FLAGS: u32 = FLAG_INLINE | FLAG_GENERAL_DELTA | (1 << 18) | (1 << 19);

/// Format: the flags version 2 may carry (`REVLOGV2_FLAGS`).
const V2_FLAGS: u32 = FLAG_INLINE;

/// Format: a version 1 entry's bytes (`INDEX_ENTRY_V1`).
const ENTRY_V1: usize = 64;

/// Format: a version 2 entry's bytes, the changelog's too (`INDEX_ENTRY_V2`, `INDEX_ENTRY_CL_V2`).
const ENTRY_V2: usize = 96;

/// Format: a version 1 or 2 entry's fields, as (offset, width) pairs: the offset and flags, the chunk's length, the
/// text's length, the base, the link, the parents and the node.
const V1_FIELDS: [usize; 8] = [0, 8, 12, 16, 20, 24, 28, 32];

/// Format: where a version 2 entry's compression modes are.
const V2_MODES: usize = 76;

/// Format: a changelog version 2 entry's fields: the offset and flags, the chunk's length, the text's length, the
/// parents, the node, and its compression modes.
const CL_V2_FIELDS: [usize; 7] = [0, 8, 12, 16, 20, 24, 68];

/// Format: the bits of the offset field that hold the revision's flags.
const FLAGS_BITS: u32 = 16;

/// Format: the data's compression mode, a modes byte's low two bits.
const MODE_MASK: u8 = 0b11;

/// Format: revision flags whose texts do not hash to their nodes (`REVIDX_ISCENSORED`, `REVIDX_ELLIPSIS`,
/// `REVIDX_EXTSTORED`).
const UNHASHED_FLAGS: u16 = (1 << 15) | (1 << 14) | (1 << 13);

/// Format: a chunk's zlib header.
const ZLIB: u8 = b'x';

/// Format: a chunk's zstd header, a frame's magic's first byte.
const ZSTD: u8 = b'(';

/// Format: a raw chunk's header, dropped.
const RAW: u8 = b'u';

/// Format: a raw chunk's header, kept.
const RAW_NUL: u8 = 0;

/// Format: a docket's counts and sizes: an id's length and its older ids' count for each of its three files, and each
/// file's size and pending size.
const DOCKET_FIELDS: usize = 6;

/// Format: the bytes of a docket's size.
const DOCKET_SIZE_BYTES: usize = 8;

/// Format: a docket's fixed part: the header, six counts, six sizes and the default compression header
/// (`S_HEADER`, `>IBBBBBBQQQQQQc`).
const DOCKET_FIXED: usize = HEADER_BYTES + DOCKET_FIELDS + DOCKET_FIELDS * DOCKET_SIZE_BYTES + 1;

/// Format: an older id's record in a docket (`S_OLD_UID`, `>BL`).
const DOCKET_OLD_ID: usize = 1 + 4;

/// Format: a revision number's "none" (7.2's `nullrev`, -1).
pub const NONE: u32 = u32::MAX;

/// A revlog's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
  /// Version 1.
  V1,
  /// Version 2, behind a docket.
  V2,
  /// The changelog's version 2, behind a docket.
  ChangelogV2,
}

/// How a chunk is stored (version 2's modes; every version 1 chunk is [`Mode::Inline`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
  /// Raw, with no header (`COMP_MODE_PLAIN`).
  Plain,
  /// Compressed by the docket's default engine (`COMP_MODE_DEFAULT`).
  Default,
  /// Routed by its own first byte (`COMP_MODE_INLINE`).
  Inline,
}

/// One revision's entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
  /// Where its chunk begins among the chunks.
  pub offset: u64,
  /// Its revision flags.
  pub flags: u16,
  /// Its chunk's length.
  pub compressed: u32,
  /// Its text's length.
  pub uncompressed: u32,
  /// Its delta base: itself for a full text.
  pub base: u32,
  /// The changeset that brought it.
  pub link: u32,
  /// Its first parent, or [`NONE`].
  pub p1: u32,
  /// Its second parent, or [`NONE`].
  pub p2: u32,
  /// Its node.
  pub node: Node,
  /// How its chunk is stored.
  pub mode: Mode,
}

/// A version 2 docket (`revlogutils/docket.py`): the files it names and how much of each is the revlog's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Docket<'a> {
  /// The version and flags, as an index header.
  pub header: u32,
  /// The index file's id: the file is `<radix>-<id>.idx`.
  pub index_id: &'a [u8],
  /// The data file's id: `<radix>-<id>.dat`.
  pub data_id: &'a [u8],
  /// The sidedata file's id: `<radix>-<id>.sda`.
  pub sidedata_id: &'a [u8],
  /// The index's bytes.
  pub index_end: u64,
  /// The data's bytes.
  pub data_end: u64,
  /// The sidedata's bytes.
  pub sidedata_end: u64,
  /// The default engine's chunk header.
  pub default_header: u8,
}

/// Why a revlog, or one of its revisions, was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
  /// A version or flags 7.2's reader would not take, or version 0.
  Version {
    /// The header.
    header: u32,
  },
  /// Bytes shorter than what they must hold.
  Truncated {
    /// What was cut.
    what: &'static str,
  },
  /// A structure that contradicts itself.
  Corrupt {
    /// What.
    what: &'static str,
    /// The revision, or the entry, it was found at.
    rev: u32,
  },
  /// No revision of that number.
  NoRevision {
    /// The number.
    rev: u32,
  },
  /// A chunk whose first byte names no engine.
  Compression {
    /// The revision.
    rev: u32,
    /// The byte.
    header: u8,
  },
  /// A compressed chunk its engine refused: cut short, corrupt, or with bytes after its stream
  /// ([`compression::inflate`]).
  Stream {
    /// The revision.
    rev: u32,
    /// The engine's refusal.
    refusal: StreamRefusal,
  },
  /// A delta that does not apply.
  Delta {
    /// The revision.
    rev: u32,
    /// The applier's refusal.
    refusal: delta::Refusal,
  },
  /// A text that is not as long as its entry says.
  Length {
    /// The revision.
    rev: u32,
  },
  /// A text that does not hash to its node.
  Node {
    /// The revision.
    rev: u32,
    /// The node rule's refusal.
    refusal: node::Refusal,
  },
  /// Revision flags whose texts do not hash to their nodes (censored, ellipsis, externally stored).
  Flags {
    /// The revision.
    rev: u32,
    /// The flags.
    flags: u16,
  },
  /// More than the caller's admission allows.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for Refused {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// A big-endian `u32` at `at`.
fn be32(bytes: &[u8], at: usize) -> Option<u32> {
  // Format: four bytes, an index entry's lengths, base, link and parents (`>i`) and the index's header (`>I`).
  let field = bytes.get(at..)?.first_chunk::<4>()?;
  Some(u32::from_be_bytes(*field))
}

/// A big-endian `u64` at `at`.
fn be64(bytes: &[u8], at: usize) -> Option<u64> {
  // Format: eight bytes, an index entry's offset and flags (`>Q`) and a docket's sizes (`S_HEADER`'s `Q`s).
  let field = bytes.get(at..)?.first_chunk::<8>()?;
  Some(u64::from_be_bytes(*field))
}

/// A signed 32-bit length at `at` that must not be negative.
fn length(bytes: &[u8], at: usize, rev: u32) -> Result<u32, Refused> {
  let value = be32(bytes, at).ok_or(Refused::Truncated { what: "an entry" })?;
  if value > i32::MAX.unsigned_abs() {
    return Err(Refused::Corrupt {
      what: "a negative length",
      rev,
    });
  }
  Ok(value)
}

/// A revision number at `at`: -1 is [`NONE`], any other negative refused.
fn number(bytes: &[u8], at: usize, rev: u32) -> Result<u32, Refused> {
  let value = be32(bytes, at).ok_or(Refused::Truncated { what: "an entry" })?;
  if value == NONE || value <= i32::MAX.unsigned_abs() {
    Ok(value)
  } else {
    Err(Refused::Corrupt {
      what: "a negative revision",
      rev,
    })
  }
}

/// Reads a version 2 docket.
pub fn read_docket(bytes: &[u8]) -> Result<Docket<'_>, Refused> {
  let fixed = bytes
    .get(..DOCKET_FIXED)
    .ok_or(Refused::Truncated { what: "a docket" })?;
  let header = be32(fixed, 0).ok_or(Refused::Truncated { what: "a docket" })?;
  let sizes = HEADER_BYTES.saturating_add(DOCKET_FIELDS);
  let counts = fixed
    .get(HEADER_BYTES..sizes)
    .ok_or(Refused::Truncated { what: "a docket" })?;
  let size = |at: usize| {
    be64(
      fixed,
      sizes.saturating_add(at.saturating_mul(DOCKET_SIZE_BYTES)),
    )
  };
  // Format: the docket's six sizes in order (`parse_docket_args`): the index's, its pending, the data's, its pending,
  // the sidedata's, its pending; 0, 2 and 4 are the committed ones.
  let (index_end, data_end, sidedata_end) = (size(0), size(2), size(4));
  let default_header = fixed.last().copied();
  let mut rest = bytes.get(DOCKET_FIXED..).unwrap_or_default();
  let mut ids: [&[u8]; 3] = [&[], &[], &[]];
  for (slot, pair) in ids.iter_mut().zip(counts.chunks(2)) {
    let &[id_len, older] = pair else {
      return Err(Refused::Truncated { what: "a docket" });
    };
    *slot = take(&mut rest, usize::from(id_len))?;
    let mut older_lengths = 0_usize;
    for _ in 0..older {
      let record = take(&mut rest, DOCKET_OLD_ID)?;
      older_lengths =
        older_lengths.saturating_add(usize::from(record.first().copied().unwrap_or_default()));
    }
    take(&mut rest, older_lengths)?;
  }
  let [index_id, data_id, sidedata_id] = ids;
  match (index_end, data_end, sidedata_end, default_header) {
    (Some(index_end), Some(data_end), Some(sidedata_end), Some(default_header)) => Ok(Docket {
      header,
      index_id,
      data_id,
      sidedata_id,
      index_end,
      data_end,
      sidedata_end,
      default_header,
    }),
    _ => Err(Refused::Truncated { what: "a docket" }),
  }
}

/// The next `count` bytes of a docket's ids.
fn take<'b>(rest: &mut &'b [u8], count: usize) -> Result<&'b [u8], Refused> {
  let (taken, after) = rest.split_at_checked(count).ok_or(Refused::Truncated {
    what: "a docket's ids",
  })?;
  *rest = after;
  Ok(taken)
}

/// Where a revlog's bytes come from (A-649 part 3): its index's and its data's, each read by offset and length. A
/// revlog's whole bytes are one source ([`Slices`], what [`Revlog`] reads); a revlog too large to hold is read by ranges
/// through another (the import's), so the reader's one algorithm ([`Outline`]) serves both.
pub trait Source {
  /// What the source refuses: the reader's refusals, and its own.
  type Error: From<Refused>;

  /// `len` bytes of the index from `at`, what the source holds charged to `admission`; fewer are refused.
  fn index(
    &mut self,
    at: u64,
    len: usize,
    admission: &mut Admission,
  ) -> Result<Cow<'_, [u8]>, Self::Error>;

  /// `len` bytes of the data from `at` (an inline revlog's index), likewise.
  fn data(
    &mut self,
    at: u64,
    len: usize,
    admission: &mut Admission,
  ) -> Result<Cow<'_, [u8]>, Self::Error>;
}

/// A revlog's whole bytes as its source, borrowed and charged nothing.
#[derive(Clone, Copy, Debug)]
pub struct Slices<'a> {
  /// The index file's bytes, an inline revlog's chunks among them.
  pub index: &'a [u8],
  /// The data file's bytes, or the index again for an inline revlog.
  pub data: &'a [u8],
}

impl Source for Slices<'_> {
  type Error = Refused;

  fn index(&mut self, at: u64, len: usize, _: &mut Admission) -> Result<Cow<'_, [u8]>, Refused> {
    span(self.index, at, len)
      .map(Cow::Borrowed)
      .ok_or(Refused::Truncated { what: "an entry" })
  }

  fn data(&mut self, at: u64, len: usize, _: &mut Admission) -> Result<Cow<'_, [u8]>, Refused> {
    span(self.data, at, len)
      .map(Cow::Borrowed)
      .ok_or(Refused::Truncated { what: "a chunk" })
  }
}

/// `len` bytes of `bytes` from `at`.
fn span(bytes: &[u8], at: u64, len: usize) -> Option<&[u8]> {
  let start = usize::try_from(at).ok()?;
  bytes.get(start..start.checked_add(len)?)
}

/// What a revlog is apart from its bytes (A-649 part 3): its format, whether it is inline and uses generaldelta, how
/// many revisions it holds, where an inline one's entries begin, and its docket's default engine. The reader's one
/// algorithm, over any [`Source`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outline {
  /// Its format.
  format: Format,
  /// Whether its chunks follow their entries in the index.
  inline: bool,
  /// Whether a delta's base is its entry's (else the revision before).
  general_delta: bool,
  /// How many revisions.
  count: u32,
  /// Where each entry of an inline index begins, found once by its scan; empty otherwise.
  positions: Vec<usize>,
  /// The docket's default engine's chunk header.
  default_header: Option<u8>,
}

/// A version 1 header's inline and generaldelta flags, or its refusal: another version, or flags version 1 lacks.
fn v1_flags(header: u32) -> Result<(bool, bool), Refused> {
  let flags = header & !VERSION_MASK;
  if header & VERSION_MASK != VERSION_1 || flags & !V1_FLAGS != 0 {
    return Err(Refused::Version { header });
  }
  Ok((flags & FLAG_INLINE != 0, flags & FLAG_GENERAL_DELTA != 0))
}

/// A docket's format, or its refusal: a version no docket holds, or flags version 2 lacks (inline among them).
fn docket_format(header: u32) -> Result<Format, Refused> {
  let format = match header & VERSION_MASK {
    VERSION_2 => Format::V2,
    CHANGELOG_V2 => Format::ChangelogV2,
    _ => return Err(Refused::Version { header }),
  };
  let flags = header & !VERSION_MASK;
  if flags & !V2_FLAGS != 0 || flags & FLAG_INLINE != 0 {
    return Err(Refused::Version { header });
  }
  Ok(format)
}

impl Outline {
  /// A version 1 revlog's outline from its whole index file: an inline one's entries found once by its scan, their
  /// positions charged to `admission`.
  pub fn of_index(index: &[u8], admission: &mut Admission) -> Result<Outline, Refused> {
    if index.is_empty() {
      return Ok(Outline {
        format: Format::V1,
        inline: true,
        general_delta: true,
        count: 0,
        positions: Vec::new(),
        default_header: None,
      });
    }
    let header = be32(index, 0).ok_or(Refused::Truncated { what: "a header" })?;
    let (inline, general_delta) = v1_flags(header)?;
    let (count, positions) = if inline {
      let count = scan_inline(index, None)?;
      admission.charge_items::<usize>(usize::try_from(count).unwrap_or(usize::MAX))?;
      let mut positions = Vec::with_capacity(usize::try_from(count).unwrap_or_default());
      scan_inline(index, Some(&mut positions))?;
      (count, positions)
    } else {
      (whole_entries(index.len(), ENTRY_V1)?, Vec::new())
    };
    Ok(Outline {
      format: Format::V1,
      inline,
      general_delta,
      count,
      positions,
      default_header: None,
    })
  }

  /// A version 1 revlog that is not inline, from its header and its index file's length alone, its entries read by
  /// ranges. An inline revlog's entries are found only by a scan of its whole file, so its header is refused here.
  pub fn of_split(header: u32, index_len: u64) -> Result<Outline, Refused> {
    let (inline, general_delta) = v1_flags(header)?;
    if inline {
      return Err(Refused::Corrupt {
        what: "an inline revlog outlined from its length",
        rev: NONE,
      });
    }
    let len = usize::try_from(index_len).map_err(|_| Refused::Corrupt {
      what: "more revisions than a number names",
      rev: NONE,
    })?;
    Ok(Outline {
      format: Format::V1,
      inline: false,
      general_delta,
      count: whole_entries(len, ENTRY_V1)?,
      positions: Vec::new(),
      default_header: None,
    })
  }

  /// A revlog behind a docket (version 2, or the changelog's version 2): its entries the docket's index's.
  pub fn of_docket(docket: &Docket<'_>) -> Result<Outline, Refused> {
    let format = docket_format(docket.header)?;
    let len = usize::try_from(docket.index_end).map_err(|_| Refused::Truncated {
      what: "an index file",
    })?;
    Ok(Outline {
      format,
      inline: false,
      general_delta: true,
      count: whole_entries(len, ENTRY_V2)?,
      positions: Vec::new(),
      default_header: Some(docket.default_header),
    })
  }

  /// Its format.
  pub fn format(&self) -> Format {
    self.format
  }

  /// Whether its chunks follow their entries in its index file.
  pub fn inline(&self) -> bool {
    self.inline
  }

  /// How many revisions it holds.
  pub fn len(&self) -> u32 {
    self.count
  }

  /// Whether it holds no revision.
  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  /// The size of one entry.
  fn entry_size(&self) -> usize {
    match self.format {
      Format::V1 => ENTRY_V1,
      Format::V2 | Format::ChangelogV2 => ENTRY_V2,
    }
  }

  /// Where revision `rev`'s entry begins in the index.
  fn entry_at(&self, rev: u32) -> Result<u64, Refused> {
    if rev >= self.count {
      return Err(Refused::NoRevision { rev });
    }
    let at = if self.inline {
      usize::try_from(rev)
        .ok()
        .and_then(|rev| self.positions.get(rev))
        .copied()
    } else {
      usize::try_from(rev)
        .ok()
        .and_then(|rev| rev.checked_mul(self.entry_size()))
    };
    at.map(crate::read::to_u64)
      .ok_or(Refused::NoRevision { rev })
  }

  /// Revision `rev`'s entry, read from `source`.
  pub fn entry<S: Source>(
    &self,
    source: &mut S,
    rev: u32,
    admission: &mut Admission,
  ) -> Result<Entry, S::Error> {
    let at = self.entry_at(rev)?;
    let bytes = source.index(at, self.entry_size(), admission)?;
    Ok(self.parse_entry(&bytes, rev)?)
  }

  /// An entry's fields from its bytes, checked as 7.2 would not read them otherwise.
  fn parse_entry(&self, bytes: &[u8], rev: u32) -> Result<Entry, Refused> {
    let entry = match self.format {
      Format::V1 | Format::V2 => {
        let mode = match self.format {
          Format::V1 => Mode::Inline,
          _ => mode(bytes.get(V2_MODES).copied().unwrap_or_default(), rev)?,
        };
        let [offset, compressed, uncompressed, base, link, p1, p2, node] = V1_FIELDS;
        Entry {
          offset: offset_of(bytes, offset, rev)?,
          flags: flags_of(bytes, offset)?,
          compressed: length(bytes, compressed, rev)?,
          uncompressed: length(bytes, uncompressed, rev)?,
          base: number(bytes, base, rev)?,
          link: number(bytes, link, rev)?,
          p1: number(bytes, p1, rev)?,
          p2: number(bytes, p2, rev)?,
          node: node_of(bytes, node)?,
          mode,
        }
      }
      Format::ChangelogV2 => {
        let [offset, compressed, uncompressed, p1, p2, node, modes] = CL_V2_FIELDS;
        Entry {
          offset: offset_of(bytes, offset, rev)?,
          flags: flags_of(bytes, offset)?,
          compressed: length(bytes, compressed, rev)?,
          uncompressed: length(bytes, uncompressed, rev)?,
          base: rev,
          link: rev,
          p1: number(bytes, p1, rev)?,
          p2: number(bytes, p2, rev)?,
          node: node_of(bytes, node)?,
          mode: mode(bytes.get(modes).copied().unwrap_or_default(), rev)?,
        }
      }
    };
    let after = |value: u32| value != NONE && value >= rev;
    if entry.base > rev || entry.base == NONE {
      return Err(Refused::Corrupt {
        what: "a base after its revision",
        rev,
      });
    }
    if after(entry.p1) || after(entry.p2) {
      return Err(Refused::Corrupt {
        what: "a parent after its revision",
        rev,
      });
    }
    Ok(entry)
  }

  /// Revision `rev`'s chunk as stored.
  fn segment<'s, S: Source>(
    &self,
    source: &'s mut S,
    rev: u32,
    entry: &Entry,
    admission: &mut Admission,
  ) -> Result<Cow<'s, [u8]>, S::Error> {
    let mut start = entry.offset;
    if self.inline {
      let before = u64::from(rev)
        .checked_add(1)
        .and_then(|count| count.checked_mul(crate::read::to_u64(ENTRY_V1)))
        .ok_or(Refused::Truncated { what: "a chunk" })?;
      start = start
        .checked_add(before)
        .ok_or(Refused::Truncated { what: "a chunk" })?;
    }
    let len =
      usize::try_from(entry.compressed).map_err(|_| Refused::Truncated { what: "a chunk" })?;
    source.data(start, len, admission)
  }

  /// Revision `rev`'s chunk, decompressed: a full text or a delta. Its bytes are charged to `admission` as they are
  /// made.
  pub fn chunk<S: Source>(
    &self,
    source: &mut S,
    rev: u32,
    admission: &mut Admission,
  ) -> Result<Vec<u8>, S::Error> {
    let entry = self.entry(source, rev, admission)?;
    self.chunk_of(source, rev, &entry, admission)
  }

  /// A chunk, decompressed, of an entry read already.
  fn chunk_of<S: Source>(
    &self,
    source: &mut S,
    rev: u32,
    entry: &Entry,
    admission: &mut Admission,
  ) -> Result<Vec<u8>, S::Error> {
    let default = self.default_header;
    let stored = self.segment(source, rev, entry, admission)?;
    let route = match entry.mode {
      Mode::Plain => None,
      Mode::Default => Some(default.ok_or(Refused::Corrupt {
        what: "a default mode with no docket",
        rev,
      })?),
      Mode::Inline => stored.first().copied(),
    };
    Ok(match route {
      None | Some(RAW_NUL) => copy(&stored, admission)?,
      Some(RAW) => copy(stored.get(1..).unwrap_or_default(), admission)?,
      Some(ZLIB) => inflate(rev, Engine::Zlib, &stored, admission)?,
      Some(ZSTD) => inflate(rev, Engine::Zstd, &stored, admission)?,
      Some(header) => return Err(Refused::Compression { rev, header }.into()),
    })
  }

  /// The revisions whose chunks rebuild `rev`'s text, the full text first (7.2's `_deltachain`), and whether the walk
  /// stopped at `stop` (7.2's `stoprev`), whose text then comes before them.
  pub fn chain<S: Source>(
    &self,
    source: &mut S,
    rev: u32,
    stop: Option<u32>,
    admission: &mut Admission,
  ) -> Result<(Vec<u32>, bool), S::Error> {
    let mut chain = Vec::new();
    let mut at = rev;
    let stopped = loop {
      if Some(at) == stop {
        break true;
      }
      let entry = self.entry(source, at, admission)?;
      if entry.base == at {
        admission.charge_items::<u32>(1).map_err(Refused::from)?;
        chain.push(at);
        break false;
      }
      if entry.compressed > 0 {
        admission.charge_items::<u32>(1).map_err(Refused::from)?;
        chain.push(at);
      }
      at = if self.general_delta {
        entry.base
      } else {
        at.checked_sub(1).ok_or(Refused::Corrupt {
          what: "a chain past the first revision",
          rev: at,
        })?
      };
    };
    chain.reverse();
    Ok((chain, stopped))
  }

  /// Revision `rev`'s text: its chain rebuilt, from `cached`'s text where the chain passes through that revision (7.2's
  /// revision cache), its length and its node checked; and whether the cached text began it. Every chunk and every
  /// patched text is charged to `admission`.
  pub fn text<S: Source>(
    &self,
    source: &mut S,
    rev: u32,
    cached: Option<(u32, &[u8])>,
    admission: &mut Admission,
  ) -> Result<(Vec<u8>, bool), S::Error> {
    let entry = self.entry(source, rev, admission)?;
    if entry.flags & UNHASHED_FLAGS != 0 {
      return Err(
        Refused::Flags {
          rev,
          flags: entry.flags,
        }
        .into(),
      );
    }
    let (chain, stopped) = self.chain(source, rev, cached.map(|(at, _)| at), admission)?;
    // The text so far and a spare it is patched into, swapped at each link, so the chain's deltas apply within two
    // buffers whatever its length (main's order of 2026-11-20: a copy a link charged the read a text a link).
    let mut cached_base = cached.filter(|_| stopped).map(|(_, text)| text);
    let mut text: Option<Vec<u8>> = None;
    let mut spare = Vec::new();
    for link in chain {
      let link_entry = self.entry(source, link, admission)?;
      let piece = self.chunk_of(source, link, &link_entry, admission)?;
      match (cached_base.take(), text.as_ref()) {
        (Some(base), _) => apply_into(base, &piece, &mut spare, link, admission)?,
        (None, Some(base)) => apply_into(base, &piece, &mut spare, link, admission)?,
        (None, None) => spare = piece,
      }
      spare = text.replace(spare).unwrap_or_default();
    }
    let text = text.unwrap_or_else(|| cached_base.map(<[u8]>::to_vec).unwrap_or_default());
    if u64::try_from(text.len()).ok() != Some(u64::from(entry.uncompressed)) {
      return Err(Refused::Length { rev }.into());
    }
    let p1 = self.node_of(source, entry.p1, admission)?;
    let p2 = self.node_of(source, entry.p2, admission)?;
    node::check(&entry.node, &p1, &p2, &text).map_err(|refusal| Refused::Node { rev, refusal })?;
    Ok((text, stopped))
  }

  /// The node of revision `rev`, the null node for none.
  fn node_of<S: Source>(
    &self,
    source: &mut S,
    rev: u32,
    admission: &mut Admission,
  ) -> Result<Node, S::Error> {
    if rev == NONE {
      return Ok(NULL_NODE);
    }
    Ok(self.entry(source, rev, admission)?.node)
  }
}

/// A delta applied by slates-hg's applier over `out`, its refusal in the reader's terms.
fn apply_into(
  base: &[u8],
  piece: &[u8],
  out: &mut Vec<u8>,
  rev: u32,
  admission: &mut Admission,
) -> Result<(), Refused> {
  delta::apply_into(base, piece, out, admission).map_err(|refusal| match refusal {
    delta::Refusal::PastAdmission(past) => Refused::PastAdmission(past),
    refusal => Refused::Delta { rev, refusal },
  })
}

/// A revlog read in place from its whole bytes: its [`Outline`] over its [`Slices`].
#[derive(Clone, Debug)]
pub struct Revlog<'a> {
  /// What it is.
  outline: Outline,
  /// Its bytes.
  source: Slices<'a>,
}

impl<'a> Revlog<'a> {
  /// Opens a version 1 revlog: its `.i` and, when it is not inline, its `.d` (empty otherwise). An inline index's
  /// entry positions are found once and charged to `admission`.
  pub fn open(
    index: &'a [u8],
    data: &'a [u8],
    admission: &mut Admission,
  ) -> Result<Revlog<'a>, Refused> {
    let outline = Outline::of_index(index, admission)?;
    let data = if outline.inline { index } else { data };
    Ok(Revlog {
      outline,
      source: Slices { index, data },
    })
  }

  /// Opens a version 2 revlog or a changelog of version 2 from its docket and the index and data files it names.
  pub fn open_docketed(
    docket: &Docket<'_>,
    index: &'a [u8],
    data: &'a [u8],
  ) -> Result<Revlog<'a>, Refused> {
    docket_format(docket.header)?;
    let end = |bytes: &'a [u8], end: u64, what: &'static str| {
      usize::try_from(end)
        .ok()
        .and_then(|end| bytes.get(..end))
        .ok_or(Refused::Truncated { what })
    };
    let index = end(index, docket.index_end, "an index file")?;
    let data = end(data, docket.data_end, "a data file")?;
    Ok(Revlog {
      outline: Outline::of_docket(docket)?,
      source: Slices { index, data },
    })
  }

  /// What it is.
  pub fn outline(&self) -> &Outline {
    &self.outline
  }

  /// Its format.
  pub fn format(&self) -> Format {
    self.outline.format
  }

  /// How many revisions it holds.
  pub fn len(&self) -> u32 {
    self.outline.count
  }

  /// Whether it holds no revision.
  pub fn is_empty(&self) -> bool {
    self.outline.count == 0
  }

  /// Revision `rev`'s entry.
  pub fn entry(&self, rev: u32) -> Result<Entry, Refused> {
    let mut source = self.source;
    self.outline.entry(&mut source, rev, &mut Admission::new(0))
  }

  /// Revision `rev`'s chunk, decompressed: a full text or a delta. Its bytes are charged to `admission` as they are
  /// made.
  pub fn chunk(&self, rev: u32, admission: &mut Admission) -> Result<Vec<u8>, Refused> {
    let mut source = self.source;
    self.outline.chunk(&mut source, rev, admission)
  }

  /// The revisions whose chunks rebuild `rev`'s text, the full text first (7.2's `_deltachain`).
  pub fn chain(&self, rev: u32, admission: &mut Admission) -> Result<Vec<u32>, Refused> {
    let mut source = self.source;
    Ok(self.outline.chain(&mut source, rev, None, admission)?.0)
  }

  /// Revision `rev`'s text: its chain rebuilt, its length and its node checked. Every chunk and every patched text is
  /// charged to `admission`.
  pub fn text(&self, rev: u32, admission: &mut Admission) -> Result<Vec<u8>, Refused> {
    let mut source = self.source;
    Ok(self.outline.text(&mut source, rev, None, admission)?.0)
  }
}

/// How many whole entries `len` bytes of index hold.
fn whole_entries(len: usize, size: usize) -> Result<u32, Refused> {
  if len.checked_rem(size) != Some(0) {
    return Err(Refused::Truncated { what: "an entry" });
  }
  len
    .checked_div(size)
    .and_then(|count| u32::try_from(count).ok())
    .filter(|count| *count != NONE)
    .ok_or(Refused::Corrupt {
      what: "more revisions than a number names",
      rev: NONE,
    })
}

/// An inline index's revisions, walked as 7.2's `_inline_scan` walks them: each entry then its chunk, every length
/// not negative, every offset the chunks' bytes before it, and the walk ending at the index's end; each entry's
/// position kept in `positions` when given.
fn scan_inline(index: &[u8], mut positions: Option<&mut Vec<usize>>) -> Result<u32, Refused> {
  let (mut at, mut rev, mut offset) = (0_usize, 0_u32, 0_u64);
  while at < index.len() {
    if let Some(positions) = positions.as_mut() {
      positions.push(at);
    }
    let entry = at
      .checked_add(ENTRY_V1)
      .and_then(|end| index.get(at..end))
      .ok_or(Refused::Truncated { what: "an entry" })?;
    let compressed = length(entry, V1_FIELDS[1], rev)?;
    if offset_of(entry, 0, rev)? != offset {
      return Err(Refused::Corrupt {
        what: "an offset unlike the chunks before it",
        rev,
      });
    }
    offset = offset.saturating_add(u64::from(compressed));
    at = at
      .checked_add(ENTRY_V1)
      .and_then(|at| at.checked_add(usize::try_from(compressed).ok()?))
      .ok_or(Refused::Truncated { what: "a chunk" })?;
    rev = rev
      .checked_add(1)
      .filter(|rev| *rev != NONE)
      .ok_or(Refused::Corrupt {
        what: "more revisions than a number names",
        rev,
      })?;
  }
  if at != index.len() {
    return Err(Refused::Truncated { what: "a chunk" });
  }
  Ok(rev)
}

/// An entry's offset: the offset field's top 48 bits, 0 for the first entry, whose field the header shares.
fn offset_of(bytes: &[u8], at: usize, rev: u32) -> Result<u64, Refused> {
  if rev == 0 {
    return Ok(0);
  }
  let field = be64(bytes, at).ok_or(Refused::Truncated { what: "an entry" })?;
  Ok(field.checked_shr(FLAGS_BITS).unwrap_or_default())
}

/// An entry's revision flags: the offset field's low 16 bits.
fn flags_of(bytes: &[u8], at: usize) -> Result<u16, Refused> {
  let field = be64(bytes, at).ok_or(Refused::Truncated { what: "an entry" })?;
  Ok(u16::try_from(field & u64::from(u16::MAX)).unwrap_or_default())
}

/// An entry's node.
fn node_of(bytes: &[u8], at: usize) -> Result<Node, Refused> {
  bytes
    .get(at..)
    .and_then(|rest| rest.first_chunk::<NODE_LEN>())
    .copied()
    .ok_or(Refused::Truncated { what: "an entry" })
}

/// A version 2 chunk's mode from its modes byte.
fn mode(modes: u8, rev: u32) -> Result<Mode, Refused> {
  match modes & MODE_MASK {
    0 => Ok(Mode::Plain),
    1 => Ok(Mode::Default),
    2 => Ok(Mode::Inline),
    _ => Err(Refused::Corrupt {
      what: "an unknown compression mode",
      rev,
    }),
  }
}

/// A raw chunk's bytes, charged first.
fn copy(stored: &[u8], admission: &mut Admission) -> Result<Vec<u8>, Refused> {
  admission.charge(crate::read::to_u64(stored.len()))?;
  Ok(stored.to_vec())
}

/// A compressed chunk inflated by the crate's one inflater, the one compressed bundles use ([`compression::inflate`]):
/// the decoder's memory and every byte of room charged to `admission` first, and a stream cut short, corrupt or
/// followed by more bytes refused by kind.
fn inflate(
  rev: u32,
  engine: Engine,
  stored: &[u8],
  admission: &mut Admission,
) -> Result<Vec<u8>, Refused> {
  let mut out = Vec::new();
  compression::inflate(engine, stored, admission, &mut out).map_err(|refusal| match refusal {
    StreamRefusal::PastAdmission { past, .. } => Refused::PastAdmission(past),
    refusal => Refused::Stream { rev, refusal },
  })?;
  Ok(out)
}

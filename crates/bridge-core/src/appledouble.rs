//! The AppleDouble codec (§4.6 "Extended attributes over NFSv3"): pure decoding and encoding of the
//! `._name` sidecar file in which macOS stores a file's extended attributes when the file system
//! offers none, as over NFSv3.
//!
//! The format is RFC 1740's AppleDouble with Apple's extended-attribute header inside the Finder Info
//! entry. The layout and every validity rule here are transcribed from xnu's classic in-kernel
//! implementation (`bsd/vfs/vfs_xattr.c` at `xnu-10063.141.1`: `check_and_swap_apple_double_header`,
//! `get_xattrinfo`, `check_and_swap_attrhdr`, `create_xattrfile`). From macOS 26 (`xnu-12377`) the
//! kernel hands the layout to the userspace `doubleagentd`, which still writes this layout: five
//! sidecars captured from macOS 26.4.1 on a slates NFSv3 mount decode here, and the four whose entry
//! order is byte order re-encode byte for byte (`tests/appledouble.rs`).
//!
//! Decoding reads what the file says: the Finder Info (`com.apple.FinderInfo`, present unless all
//! zeroes), the resource fork (`com.apple.ResourceFork`, present unless empty or the placeholder
//! header xnu writes), and each named attribute's `(offset, length)`. A file that is not yet a
//! complete AppleDouble file (a client's write sequence in progress) is [`Decoded::Incomplete`], so a
//! caller never mistakes a half-written header for "no attributes". Encoding lays out a canonical
//! file for a set of attributes: names in byte order, values packed after the entries, the empty
//! resource-fork placeholder at the end of a 4,096-byte file as xnu creates it. The encoding is a
//! list of [`Segment`]s, so a caller renders any byte range without materializing a large resource
//! fork. Every length read from the file is checked against the file's size before use (hostile input:
//! `tests/appledouble.rs`).

/// Format: the AppleDouble magic (RFC 1740 §4, xnu `ADH_MAGIC`).
const ADH_MAGIC: u32 = 0x0005_1607;
/// Format: the AppleDouble version 2 (xnu `ADH_VERSION`).
const ADH_VERSION: u32 = 0x0002_0000;
/// Format: the filler Mac OS X writes (xnu `ADH_MACOSX`), sixteen bytes.
const ADH_FILLER: &[u8; 16] = b"Mac OS X        ";
/// Format: the entry id of the resource fork (xnu `AD_RESOURCE`).
const AD_RESOURCE: u32 = 2;
/// Format: the entry id of the Finder Info (xnu `AD_FINDERINFO`).
const AD_FINDERINFO: u32 = 9;
/// Format: where the entry array begins: magic, version and filler, then the entry count
/// (xnu `offsetof(apple_double_header_t, entries)` = 4 + 4 + 16 + 2).
const ENTRIES_AT: usize = 26;
/// Format: one AppleDouble entry: type, offset, length, each a big-endian `u32`.
const ENTRY_BYTES: usize = 12;
/// Format: the most entries xnu accepts (`numEntries > 15` is refused).
const MAX_ENTRIES: usize = 15;
/// Format: the Finder Info's fixed size (xnu `FINDERINFOSIZE`).
const FINDER_INFO_BYTES: usize = 32;
/// Format: where the Finder Info sits in a two-entry file, `offsetof(apple_double_header_t, finfo)`:
/// the header plus two entries.
const FINDER_INFO_AT: usize = ENTRIES_AT + 2 * ENTRY_BYTES;
/// Format: where the extended-attribute header begins: after the Finder Info and two pad bytes.
const ATTR_HEADER_AT: usize = FINDER_INFO_AT + FINDER_INFO_BYTES + 2;
/// Format: the extended-attribute header's magic, `'ATTR'` (xnu `ATTR_HDR_MAGIC`).
const ATTR_MAGIC: u32 = 0x4154_5452;
/// Format: the extended-attribute header's size: magic, debug tag, total size, data start, data
/// length, three reserved words, flags and the attribute count.
const ATTR_HEADER_BYTES: usize = 4 * 8 + 2 + 2;
/// Format: where the first attribute entry begins (xnu `sizeof(attr_header_t)`).
const ATTR_ENTRIES_AT: usize = ATTR_HEADER_AT + ATTR_HEADER_BYTES;
/// Format: an attribute entry's fixed part before its name: offset, length, flags, name length
/// (xnu `sizeof(attr_entry_t) - 1`).
const ATTR_ENTRY_FIXED: usize = 4 + 4 + 2 + 1;
/// Format: attribute entries are aligned to four bytes (xnu `ATTR_ALIGN`).
const ATTR_ALIGN: usize = 4;
/// Format: the most attributes xnu accepts in one header (`count > 256` is refused).
const MAX_ATTRS: usize = 256;
/// Format: the header and every attribute entry lie within the first 64 KiB (xnu
/// `ATTR_MAX_HDR_SIZE`); values may extend past it.
pub const MAX_HEADER_BYTES: usize = 65_536;
/// Format: the size xnu creates an AppleDouble file at, and grows it by (xnu `ATTR_BUF_SIZE`).
const FILE_UNIT: u64 = 4_096;
/// Format: the empty resource-fork header's size (xnu `sizeof(rsrcfork_header_t)`).
const EMPTY_FORK_BYTES: usize = 286;
/// Format: where the empty resource fork's tag sits within its header (after four `u32`s).
const EMPTY_FORK_TAG_AT: usize = 16;
/// Format: the tag xnu writes into a placeholder resource fork, with its NUL (xnu `RF_EMPTY_TAG`).
const EMPTY_FORK_TAG: &[u8; 47] = b"This resource fork intentionally left blank   \0";
/// Format: the empty resource map's fields (xnu `init_empty_resource_fork`).
const FORK_FIRST_RESOURCE: u32 = 256;
/// Format: the null map's length (xnu `RF_NULL_MAP_LENGTH`).
const FORK_NULL_MAP_LENGTH: u32 = 30;

/// The extended attribute that carries the Finder Info.
pub const FINDER_INFO_NAME: &[u8] = b"com.apple.FinderInfo";
/// The extended attribute that carries the resource fork.
pub const RESOURCE_FORK_NAME: &[u8] = b"com.apple.ResourceFork";

/// A byte range of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
  /// The first byte.
  pub offset: u64,
  /// How many bytes.
  pub len: u64,
}

impl Span {
  /// The byte past the last.
  pub const fn end(self) -> u64 {
    self.offset + self.len
  }
}

/// The attributes a complete AppleDouble file holds: each name and the span of the file holding its
/// value, the Finder Info and resource fork included under their attribute names, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
  /// `(name, value span)` for every attribute present.
  pub attributes: Vec<(Vec<u8>, Span)>,
  /// The span holding the headers and entries: every byte before the first value.
  pub header: Span,
}

/// What a sidecar's bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decoded {
  /// A complete AppleDouble file and the attributes it holds.
  Complete(Layout),
  /// Not (yet) a complete AppleDouble file: empty, truncated, or mid-write. The attributes are
  /// whatever they were before.
  Incomplete,
}

/// Reads bytes of the sidecar at an offset into a buffer, returning how many it read; the decoder
/// uses it only for the resource fork's tag, past the first [`MAX_HEADER_BYTES`].
pub type ReadAt<'a> = &'a mut dyn FnMut(u64, &mut [u8]) -> usize;

/// Decodes a sidecar of `file_len` bytes from its first `min(file_len, MAX_HEADER_BYTES)` bytes
/// (`prefix`), reading the resource fork's tag through `read_at` when it lies past the prefix. The
/// rules are xnu's (the module doc); any breach is [`Decoded::Incomplete`].
pub fn decode(prefix: &[u8], file_len: u64, read_at: ReadAt<'_>) -> Decoded {
  decode_layout(prefix, file_len, read_at).map_or(Decoded::Incomplete, Decoded::Complete)
}

fn decode_layout(prefix: &[u8], file_len: u64, read_at: ReadAt<'_>) -> Option<Layout> {
  let raw = prefix.get(
    ..usize::try_from(file_len)
      .ok()?
      .min(MAX_HEADER_BYTES)
      .min(prefix.len()),
  )?;
  let (entries, header_end) = decode_entries(raw, file_len)?;
  let mut attributes = Vec::new();
  let mut header = Span {
    offset: 0,
    len: header_end,
  };
  let finder = entries
    .iter()
    .position(|(kind, span)| *kind == AD_FINDERINFO && span.len >= FINDER_INFO_BYTES as u64);
  if let Some(index) = finder {
    let (info, named) = decode_finder(raw, index, entries[index].1)?;
    attributes.extend(info);
    if let Some((named, data_start)) = named {
      header.len = data_start;
      attributes.extend(named);
    }
  }
  for (kind, span) in &entries {
    if *kind == AD_RESOURCE && span.len > 0 && !is_placeholder_fork(raw, *span, read_at) {
      attributes.push((RESOURCE_FORK_NAME.to_vec(), *span));
    }
  }
  Some(Layout { attributes, header })
}

/// The AppleDouble header's entries and where the entry array ends; `None` when the magic, version
/// or count is wrong, or an entry starts inside the array, runs past the file or overlaps another.
fn decode_entries(raw: &[u8], file_len: u64) -> Option<(Vec<(u32, Span)>, u64)> {
  if be32(raw, 0)? != ADH_MAGIC || be32(raw, 4)? != ADH_VERSION {
    return None;
  }
  let count = usize::from(be16(raw, 24)?);
  if count == 0 || count > MAX_ENTRIES {
    return None;
  }
  let header_end = u64::try_from(ENTRIES_AT + count * ENTRY_BYTES).ok()?;
  let mut entries: Vec<(u32, Span)> = Vec::with_capacity(count);
  for index in 0..count {
    let at = ENTRIES_AT + index * ENTRY_BYTES;
    let kind = be32(raw, at)?;
    let span = Span {
      offset: u64::from(be32(raw, at + 4)?),
      len: u64::from(be32(raw, at + 8)?),
    };
    let overlaps = entries
      .iter()
      .any(|(_, other)| span.end() > other.offset && other.end() > span.offset);
    if span.offset < header_end || span.end() > file_len || overlaps {
      return None;
    }
    entries.push((kind, span));
  }
  Some((entries, header_end))
}

/// The Finder Info attribute (absent when all zeroes) and, when the entry is the first and large
/// enough to hold one, the extended-attribute header's attributes and data start. A Finder Info that
/// could hold a header but holds no valid one is a write still in progress: `None`.
fn decode_finder(raw: &[u8], index: usize, span: Span) -> Option<Finder> {
  let at = usize::try_from(span.offset).ok()?;
  let info = raw.get(at..at + FINDER_INFO_BYTES)?;
  let finder = info.iter().any(|byte| *byte != 0).then(|| {
    (
      FINDER_INFO_NAME.to_vec(),
      Span {
        offset: span.offset,
        len: FINDER_INFO_BYTES as u64,
      },
    )
  });
  if index != 0 || span.len < ATTR_HEADER_BYTES as u64 {
    return Some((finder, None));
  }
  if span.offset != FINDER_INFO_AT as u64 {
    return None;
  }
  Some((finder, Some(decode_attributes(raw, span)?)))
}

/// An attribute's name and the span of the file holding its value.
type Named = (Vec<u8>, Span);

/// What the Finder Info entry holds: the Finder Info attribute, if present, and the extended-attribute
/// header's attributes with its data start, if the entry carries one.
type Finder = (Option<Named>, Option<(Vec<Named>, u64)>);

/// The named attributes of the extended-attribute header inside `finder` (the Finder Info entry),
/// and the header's data start; `None` when the header breaks one of xnu's rules.
fn decode_attributes(raw: &[u8], finder: Span) -> Option<(Vec<Named>, u64)> {
  if be32(raw, ATTR_HEADER_AT)? != ATTR_MAGIC {
    return None;
  }
  let total_size = u64::from(be32(raw, ATTR_HEADER_AT + 8)?);
  let data_start = u64::from(be32(raw, ATTR_HEADER_AT + 12)?);
  let data_length = u64::from(be32(raw, ATTR_HEADER_AT + 16)?);
  let count = usize::from(be16(raw, ATTR_HEADER_AT + 34)?);
  let data_end = data_start.checked_add(data_length)?;
  if total_size > finder.end()
    || data_start < ATTR_ENTRIES_AT as u64
    || data_end > total_size
    || count > MAX_ATTRS
  {
    return None;
  }
  let entries_end = usize::try_from(data_start).ok()?;
  if entries_end > raw.len() {
    return None;
  }
  let mut at = ATTR_ENTRIES_AT;
  let mut data_total = 0u64;
  let mut named = Vec::with_capacity(count);
  for _ in 0..count {
    let (name, span, next) = decode_attribute_entry(raw, at, entries_end)?;
    if span.offset < data_start || span.end() > data_end {
      return None;
    }
    data_total = data_total.checked_add(span.len)?;
    named.push((name, span));
    at = next;
  }
  // The entries end before the data, and the values fill the data area exactly.
  if at > entries_end || data_total != data_length {
    return None;
  }
  Some((named, data_start))
}

/// One attribute entry at `at`: its name (without the NUL), its value's span, and where the next
/// entry begins; `None` when the name runs past `entries_end`, is empty, or its length field
/// disagrees with its NUL.
fn decode_attribute_entry(
  raw: &[u8],
  at: usize,
  entries_end: usize,
) -> Option<(Vec<u8>, Span, usize)> {
  let name_len = usize::from(*raw.get(at + ATTR_ENTRY_FIXED - 1)?);
  let name_at = at + ATTR_ENTRY_FIXED;
  if name_len == 0 || name_at + name_len > entries_end {
    return None;
  }
  let name = raw.get(name_at..name_at + name_len)?;
  // The length counts the NUL, and the name holds no earlier NUL.
  if name.iter().position(|byte| *byte == 0) != Some(name_len - 1) {
    return None;
  }
  let span = Span {
    offset: u64::from(be32(raw, at)?),
    len: u64::from(be32(raw, at + 4)?),
  };
  Some((
    name[..name_len - 1].to_vec(),
    span,
    at + entry_len(name_len),
  ))
}

/// Whether the resource fork at `span` is the placeholder xnu writes (its size and tag).
fn is_placeholder_fork(raw: &[u8], span: Span, read_at: ReadAt<'_>) -> bool {
  if span.len != EMPTY_FORK_BYTES as u64 {
    return false;
  }
  let at = span.offset + EMPTY_FORK_TAG_AT as u64;
  let mut tag = [0u8; EMPTY_FORK_TAG.len()];
  let got = match usize::try_from(at)
    .ok()
    .and_then(|start| raw.get(start..start + tag.len()))
  {
    Some(bytes) => {
      tag.copy_from_slice(bytes);
      tag.len()
    }
    None => read_at(at, &mut tag),
  };
  got == tag.len() && &tag == EMPTY_FORK_TAG
}

/// The length an attribute entry with a `name_len`-byte name (NUL included) occupies, aligned.
const fn entry_len(name_len: usize) -> usize {
  (ATTR_ENTRY_FIXED + name_len + ATTR_ALIGN - 1) & !(ATTR_ALIGN - 1)
}

/// One attribute to encode: its name and its value's length. The value's bytes are fetched when a
/// range is rendered ([`Encoding::render`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
  /// The attribute name.
  pub name: Vec<u8>,
  /// The value's length in bytes.
  pub len: u64,
}

/// Where a run of the encoded file comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
  /// Bytes of the encoding itself: headers, entries, padding, the placeholder fork.
  Bytes(Vec<u8>),
  /// Zeroes.
  Zero,
  /// The value of the attribute at this index of the entries given to [`encode`].
  Value(usize),
}

/// A run of the encoded file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
  /// Where the run lies in the file.
  pub span: Span,
  /// Where its bytes come from.
  pub source: Source,
}

/// A canonical AppleDouble file for a set of attributes, as runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoding {
  /// The runs, contiguous from byte zero.
  pub segments: Vec<Segment>,
  /// The file's length.
  pub len: u64,
  /// The indices of the entries the encoding holds, in the order given; an entry left out could not
  /// be represented (see [`representable`]).
  pub held: Vec<usize>,
}

/// Whether an attribute can be held in an AppleDouble file: a name whose length with its NUL fits
/// the one-byte length field and holds no NUL, a value whose length fits the format's 32-bit fields,
/// and a Finder Info exactly 32 bytes long.
pub fn representable(entry: &Entry) -> bool {
  let name_fits =
    !entry.name.is_empty() && entry.name.len() < usize::from(u8::MAX) && !entry.name.contains(&0);
  let len_fits = u32::try_from(entry.len).is_ok();
  let finder_fits = entry.name != FINDER_INFO_NAME || entry.len == FINDER_INFO_BYTES as u64;
  name_fits && len_fits && finder_fits
}

/// Encodes a canonical AppleDouble file for `entries` (any order; each name once). The named
/// attributes are laid out in byte order of name; the Finder Info takes the Finder Info entry and the
/// resource fork the resource entry. Entries that are not [`representable`], or that would push the
/// header past [`MAX_HEADER_BYTES`] or the attribute count past xnu's limit, are left out and absent
/// from [`Encoding::held`].
pub fn encode(entries: &[Entry]) -> Encoding {
  let mut order: Vec<usize> = (0..entries.len())
    .filter(|index| representable(&entries[*index]))
    .collect();
  order.sort_by(|a, b| entries[*a].name.cmp(&entries[*b].name));
  let finder = order
    .iter()
    .copied()
    .find(|index| entries[*index].name == FINDER_INFO_NAME);
  let fork = order
    .iter()
    .copied()
    .find(|index| entries[*index].name == RESOURCE_FORK_NAME);
  let mut named: Vec<usize> = Vec::new();
  let mut entries_end = ATTR_ENTRIES_AT;
  for index in order
    .iter()
    .copied()
    .filter(|index| Some(*index) != finder && Some(*index) != fork)
  {
    let next = entries_end + entry_len(entries[index].name.len() + 1);
    if next > MAX_HEADER_BYTES || named.len() == MAX_ATTRS {
      continue;
    }
    entries_end = next;
    named.push(index);
  }
  let data_start = entries_end as u64;
  let data_length: u64 = named.iter().map(|index| entries[*index].len).sum();
  let data_end = data_start + data_length;
  let fork_len = fork.map_or(EMPTY_FORK_BYTES as u64, |index| entries[index].len);
  let fork_at = if fork.is_some() {
    data_end.div_ceil(FILE_UNIT).max(1) * FILE_UNIT
  } else {
    (data_end + fork_len).div_ceil(FILE_UNIT).max(1) * FILE_UNIT - fork_len
  };
  // The 32-bit fields must hold every offset; an encoding that cannot is left without the fork.
  let fits = u32::try_from(fork_at + fork_len).is_ok();
  let (fork, fork_len, fork_at) = if fits {
    (fork, fork_len, fork_at)
  } else {
    let len = EMPTY_FORK_BYTES as u64;
    (
      None,
      len,
      (data_end + len).div_ceil(FILE_UNIT) * FILE_UNIT - len,
    )
  };

  let mut head = Vec::with_capacity(entries_end);
  head.extend_from_slice(&ADH_MAGIC.to_be_bytes());
  head.extend_from_slice(&ADH_VERSION.to_be_bytes());
  head.extend_from_slice(ADH_FILLER);
  head.extend_from_slice(&2u16.to_be_bytes());
  push_entry(
    &mut head,
    AD_FINDERINFO,
    FINDER_INFO_AT as u64,
    fork_at - FINDER_INFO_AT as u64,
  );
  push_entry(&mut head, AD_RESOURCE, fork_at, fork_len);
  let mut segments = vec![Segment {
    span: Span {
      offset: 0,
      len: head.len() as u64,
    },
    source: Source::Bytes(head),
  }];
  let finder_span = Span {
    offset: FINDER_INFO_AT as u64,
    len: FINDER_INFO_BYTES as u64,
  };
  segments.push(Segment {
    span: finder_span,
    source: finder.map_or(Source::Zero, Source::Value),
  });
  let mut attr = Vec::with_capacity(entries_end - FINDER_INFO_AT - FINDER_INFO_BYTES);
  attr.extend_from_slice(&[0u8; 2]);
  attr.extend_from_slice(&ATTR_MAGIC.to_be_bytes());
  attr.extend_from_slice(&0u32.to_be_bytes()); // debug tag: doubleagentd writes zero
  attr.extend_from_slice(&be32_of(fork_at));
  attr.extend_from_slice(&be32_of(data_start));
  attr.extend_from_slice(&be32_of(data_length));
  attr.extend_from_slice(&[0u8; 12]); // reserved
  attr.extend_from_slice(&0u16.to_be_bytes()); // flags
  attr.extend_from_slice(&u16::try_from(named.len()).unwrap_or(0).to_be_bytes());
  let mut value_at = data_start;
  for index in &named {
    let name = &entries[*index].name;
    let len = entries[*index].len;
    let start = attr.len();
    attr.extend_from_slice(&be32_of(value_at));
    attr.extend_from_slice(&be32_of(len));
    attr.extend_from_slice(&0u16.to_be_bytes());
    attr.push(u8::try_from(name.len() + 1).unwrap_or(0));
    attr.extend_from_slice(name);
    attr.push(0);
    attr.resize(start + entry_len(name.len() + 1), 0);
    value_at += len;
  }
  segments.push(Segment {
    span: Span {
      offset: finder_span.end(),
      len: attr.len() as u64,
    },
    source: Source::Bytes(attr),
  });
  let mut at = data_start;
  for index in &named {
    let len = entries[*index].len;
    if len > 0 {
      segments.push(Segment {
        span: Span { offset: at, len },
        source: Source::Value(*index),
      });
    }
    at += len;
  }
  if fork_at > at {
    segments.push(Segment {
      span: Span {
        offset: at,
        len: fork_at - at,
      },
      source: Source::Zero,
    });
  }
  segments.push(Segment {
    span: Span {
      offset: fork_at,
      len: fork_len,
    },
    source: match fork {
      Some(index) => Source::Value(index),
      None => Source::Bytes(placeholder_fork()),
    },
  });
  let mut held: Vec<usize> = named;
  held.extend(finder);
  held.extend(fork);
  held.sort_unstable();
  Encoding {
    segments,
    len: fork_at + fork_len,
    held,
  }
}

impl Encoding {
  /// Renders bytes `[off, off + out.len())` of the encoded file into `out`, fetching a value's bytes
  /// through `value(index, value_offset, buffer)`; returns how many bytes were rendered (fewer at the
  /// file's end, zero past it).
  pub fn render(
    &self,
    off: u64,
    out: &mut [u8],
    value: &mut dyn FnMut(usize, u64, &mut [u8]),
  ) -> usize {
    if off >= self.len {
      return 0;
    }
    let want = usize::try_from((self.len - off).min(out.len() as u64)).unwrap_or(0);
    let end = off + want as u64;
    for segment in &self.segments {
      let start = segment.span.offset.max(off);
      let stop = segment.span.end().min(end);
      if start >= stop {
        continue;
      }
      let (Ok(from), Ok(to)) = (usize::try_from(start - off), usize::try_from(stop - off)) else {
        continue;
      };
      let dest = &mut out[from..to];
      let within = start - segment.span.offset;
      match &segment.source {
        Source::Bytes(bytes) => {
          let Ok(first) = usize::try_from(within) else {
            continue;
          };
          dest.copy_from_slice(&bytes[first..first + dest.len()]);
        }
        Source::Zero => dest.fill(0),
        Source::Value(index) => value(*index, within, dest),
      }
    }
    want
  }
}

fn push_entry(head: &mut Vec<u8>, kind: u32, offset: u64, len: u64) {
  head.extend_from_slice(&kind.to_be_bytes());
  head.extend_from_slice(&be32_of(offset));
  head.extend_from_slice(&be32_of(len));
}

/// The placeholder resource fork xnu writes into a fresh AppleDouble file.
fn placeholder_fork() -> Vec<u8> {
  let mut fork = vec![0u8; EMPTY_FORK_BYTES];
  let put32 = |fork: &mut Vec<u8>, at: usize, value: u32| {
    fork[at..at + 4].copy_from_slice(&value.to_be_bytes());
  };
  put32(&mut fork, 0, FORK_FIRST_RESOURCE);
  put32(&mut fork, 4, FORK_FIRST_RESOURCE);
  put32(&mut fork, 12, FORK_NULL_MAP_LENGTH);
  fork[EMPTY_FORK_TAG_AT..EMPTY_FORK_TAG_AT + EMPTY_FORK_TAG.len()].copy_from_slice(EMPTY_FORK_TAG);
  // The map header (xnu `rsrcfork_header_t`): after the file header's four words, 112 bytes of
  // system data and 128 of application data come mh_DataOffset, mh_MapOffset, mh_DataLength,
  // mh_MapLength and mh_Next (256..276), mh_RefNum (276), two attribute bytes, then mh_Types (280),
  // mh_Names (282) and typeCount (284).
  put32(&mut fork, 256, FORK_FIRST_RESOURCE);
  put32(&mut fork, 260, FORK_FIRST_RESOURCE);
  put32(&mut fork, 268, FORK_NULL_MAP_LENGTH);
  let put16 = |fork: &mut Vec<u8>, at: usize, value: u16| {
    fork[at..at + 2].copy_from_slice(&value.to_be_bytes());
  };
  let null_map = u16::try_from(FORK_NULL_MAP_LENGTH).unwrap_or(0);
  put16(&mut fork, 280, null_map - 2);
  put16(&mut fork, 282, null_map);
  put16(&mut fork, 284, u16::MAX); // typeCount = -1
  fork
}

/// A 32-bit big-endian field from a 64-bit value the encoder has already bounded to 32 bits.
fn be32_of(value: u64) -> [u8; 4] {
  u32::try_from(value).unwrap_or(u32::MAX).to_be_bytes()
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
  bytes
    .get(at..at + 4)
    .map(|word| u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
}

fn be16(bytes: &[u8], at: usize) -> Option<u16> {
  bytes
    .get(at..at + 2)
    .map(|word| u16::from_be_bytes([word[0], word[1]]))
}

//! The pack index, version 2 (A-591; §4.17.2 "Pieces (git)", pieces 2 and 3, and "The repository in git's terms",
//! item 4, "each entry's CRC32 kept").
//!
//! **The format** [B: git's `gitformat-pack.adoc`, "Version 2 pack-*.idx files support packs larger than 4 GiB"; C:
//! `pack-write.c`'s `write_idx_file` and `need_large_offset`, `packfile.c`'s `load_idx` and
//! `nth_packed_object_offset`, `pack-check.c`'s `verify_packfile`; git 2.50.1, read 2026-11-19]. For a pack of n
//! objects, every number in network order:
//! 1. the magic `\377tOc`, "an unreasonable fanout\[0\] value", and the version, 2;
//! 2. a fanout of 256 counts, the k-th the number of objects whose id's first byte is at most k, so the last is n;
//! 3. the n ids, sorted;
//! 4. their n CRC32s, zlib's, each over its object's bytes in the pack;
//! 5. their n offsets in the pack, four bytes each: one with its top bit set names instead an entry of the next table
//!    ("large offsets are encoded as an index into the next table with the msbit set"), which git writes for an
//!    offset past 2^31 - 1, numbering the entries in id order;
//! 6. that table of 8-byte offsets;
//! 7. a copy of the pack's trailer, then the index's own checksum of every byte before it.
//!
//! **What it does.**
//! - [`Idx::decode`] verifies the index's own checksum before it reads a byte that checksum guards. Then it checks the
//!   layout once and lends a view that answers by position and by id in place, building nothing. The checks: the
//!   fanout never goes down and agrees with the ids; the ids strictly ascend; the file is exactly as long as its count
//!   and its 8-byte table make it; every large offset names an entry of that table; and no offset lies inside a pack's
//!   header.
//! - [`Idx::check_pack`] holds an index to its pack as `git verify-pack` does, short of inflating: one count, one
//!   checksum, every offset inside the pack's objects, the objects back to back from the header to the trailer, and
//!   each one's CRC32 over its bytes, so compressed bytes can be copied from pack to pack as git copies them, "without
//!   undetected data corruption" [B: gitformat-pack, idx v2].
//! - [`write`] writes an index as git 2.50.1 writes it, byte for byte: the entries sorted by id, an offset past
//!   2^31 - 1 in the 8-byte table, and that table's entries numbered in id order.
//!
//! **Stricter than git where git trusts its own files.** A server reads indexes it did not write (an imported
//! repository's, §4.17.2 piece 15), so each of these is refused by kind:
//! - a large offset naming a slot past its table (git's `check_pack_index_ptr` keeps only the read inside the file);
//! - a fanout that disagrees with the ids, which would find the wrong range for a lookup;
//! - an 8-byte table that is not whole entries;
//! - an offset inside a pack's header;
//! - a duplicate id, both ways, as git refuses it when strict ("The same object %s appears twice in the pack"): a
//!   lookup has one answer.
//!
//! Version 1, which has no magic, is refused as [`Refusal::Magic`]. A server writes version 2 only, and remakes an
//! imported version 1 index from its pack. An index's ids are not checked against its pack's objects here, which
//! needs inflation (piece 3).
//!
//! **Checksums** are taken through [`crate::object_name::Hasher`] (A-600), as the pack's trailer is ([`super`]).
//! **CRC32s** are zlib's, through `crc32fast`, the crate flate2 computes zlib's CRC32 with (the workspace manifest
//! says why this crate).
//!
//! **Bounds (§4.9, §4.2).** Every length is checked against the file before a byte past it is read. [`Idx::decode`]
//! allocates nothing; [`Idx::check_pack`] orders the objects by offset in a table it charges to the caller's
//! [`Admission`] before it is built; [`write`] sorts the caller's entries in place and allocates nothing.

use super::{Checksummed, HEADER_LEN, Pack, checksum_of};
use crate::object_name::{CollisionAttack, ObjectFormat, ObjectId};
use crate::out::Out;
use crate::{Admission, PastAdmission, to_u64};

/// Format: the bytes a version 2 index opens with, `\377tOc` (`pack.h`'s `PACK_IDX_SIGNATURE`) [B: gitformat-pack].
pub const MAGIC: [u8; 4] = *b"\xfftOc";

/// Format: the version this module reads and writes [B: gitformat-pack: "A 4-byte version number (= 2)"].
pub const VERSION: u32 = 2;

/// Format: the fanout's entries, one for each value of an id's first byte [B: gitformat-pack: "A 256-entry fan-out
/// table"].
const FANOUT_ENTRIES: usize = 256;

/// Format: the bytes of the version, a fanout count, a CRC32 and a 4-byte offset [B: gitformat-pack].
const WORD: usize = core::mem::size_of::<u32>();

/// Format: the bytes of an entry of the table of large offsets [B: gitformat-pack: "A table of 8-byte offset
/// entries"].
const LARGE_ENTRY: usize = core::mem::size_of::<u64>();

/// Format: the top bit of a 4-byte offset, set when the other 31 name an entry of the 8-byte table [B:
/// gitformat-pack: "large offsets are encoded as an index into the next table with the msbit set"].
const LARGE: u32 = 0x8000_0000;

/// Derived: the largest offset a 4-byte entry holds beside its top bit, 2^31 - 1. git writes any larger offset in the
/// 8-byte table (`need_large_offset`, whose `off32_limit` is this by default), and it is also the largest index of an
/// entry of that table.
const SMALL_MAX: u32 = !LARGE;

/// Format: the hashes an index ends with, its pack's trailer and then its own checksum [B: gitformat-pack].
const TRAILING_HASHES: usize = 2;

/// Derived: where the fanout begins, after the magic and the version.
const FANOUT_AT: usize = MAGIC.len().saturating_add(WORD);

/// Derived: where the ids begin, after the fanout.
const IDS_AT: usize = FANOUT_AT.saturating_add(FANOUT_ENTRIES.saturating_mul(WORD));

/// One object as an index keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
  /// Its name.
  pub id: ObjectId,
  /// zlib's CRC32 of its bytes in the pack: its entry header, its base and its compressed data [B: gitformat-pack].
  pub crc32: u32,
  /// Where in the pack it begins.
  pub offset: u64,
}

/// Why bytes are not a version 2 pack index. Each names where it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// Shorter than an index of no objects: the magic, the version, the fanout and the two trailing hashes (git's
  /// "index file %s is too small").
  Truncated {
    /// The bytes an index of no objects takes.
    needed: u64,
    /// The bytes there were.
    left: u64,
  },
  /// The index's own checksum is not the checksum of the bytes before it (git's "Packfile index for %s hash
  /// mismatch").
  Checksum,
  /// The bytes the index's checksum guards carry a known SHA-1 collision attack's disturbance.
  Collision(CollisionAttack),
  /// The bytes do not open with `\377tOc`: version 1, or no index.
  Magic,
  /// A version other than 2 (git's "index file %s is version %u and is not supported by this binary").
  Version {
    /// The version the index names.
    version: u32,
  },
  /// A fanout count smaller than the one before it (git's "non-monotonic index").
  FanoutDown {
    /// The first byte value whose count went down.
    byte: u8,
  },
  /// The count the fanout ends with needs more bytes than the index has.
  CountPastFile {
    /// The objects the fanout counts.
    count: u64,
    /// The index's length.
    len: u64,
  },
  /// The bytes between the 4-byte offsets and the trailing hashes are not whole 8-byte entries, or are more of them
  /// than an index of its count holds: one fewer than its objects, since a pack's first object begins at its header's
  /// end (git's "wrong index v2 file size").
  LargeTable {
    /// The table's bytes.
    len: u64,
    /// The objects the fanout counts.
    count: u64,
  },
  /// An id smaller than the one before it.
  Unsorted {
    /// Its position.
    position: u64,
  },
  /// An id equal to the one before it.
  Duplicate {
    /// Its position.
    position: u64,
  },
  /// A fanout count that is not the number of ids whose first byte is at most its byte.
  FanoutMismatch {
    /// The byte value.
    byte: u8,
    /// The count the fanout says.
    says: u64,
    /// The ids there are.
    counted: u64,
  },
  /// A large offset naming an entry past the 8-byte table.
  LargeIndex {
    /// The object's position.
    position: u64,
    /// The entry it names.
    index: u32,
    /// The table's entries.
    entries: u64,
  },
  /// An offset inside a pack's header, where no object begins.
  OffsetInHeader {
    /// The object's position.
    position: u64,
    /// The offset.
    offset: u64,
  },
}

/// Why an index does not belong to a pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mismatch {
  /// The pack's trailer and the index's ids are in different hashes.
  Format,
  /// The pack's header counts other objects than the index (git's "packfile %s claims to have %u objects while index
  /// indicates %u objects").
  Count {
    /// The pack's count.
    pack: u64,
    /// The index's count.
    index: u64,
  },
  /// The index's copy of the pack's trailer is not the pack's trailer (git's "packfile %s does not match index").
  PackChecksum,
  /// An offset at or past the pack's trailer.
  OffsetOutside {
    /// The object's position in the index.
    position: u64,
    /// The offset.
    offset: u64,
  },
  /// Two objects at one offset.
  SharedOffset {
    /// The second object's position in the index.
    position: u64,
    /// The offset.
    offset: u64,
  },
  /// Pack bytes no object covers: the first object does not begin where the header ends.
  Unindexed {
    /// Where the first such byte is.
    at: u64,
  },
  /// An object whose bytes do not have the CRC32 the index keeps (git's "index CRC mismatch for object %s").
  Crc {
    /// The object's position in the index.
    position: u64,
  },
  /// Ordering the pack's objects by offset would take more than the admission had left; nothing was built.
  PastAdmission(PastAdmission),
}

/// Why entries cannot be written as an index. Except for a collision, nothing is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteRefusal {
  /// An entry whose id is in another hash than the pack's trailer.
  Format {
    /// Its position as given.
    position: u64,
  },
  /// More entries than a fanout count holds [B: gitformat-pack: the counts are 4-byte numbers].
  TooMany {
    /// The entries.
    count: u64,
  },
  /// Two entries with one id (git's strict "The same object %s appears twice in the pack").
  Duplicate {
    /// The second one's position in id order.
    position: u64,
  },
  /// More offsets past 2^31 - 1 than an index holds: one fewer than its entries, and at most 2^31.
  LargeOffsets {
    /// Those offsets.
    count: u64,
  },
  /// An entry placing its object inside a pack's header, which the reader refuses.
  OffsetInHeader {
    /// Its position as given.
    position: u64,
  },
  /// The index's bytes carry a known SHA-1 collision attack's disturbance: every byte but its own checksum was
  /// written, so it is no whole index and the caller lets it go.
  Collision(CollisionAttack),
}

/// A version 2 pack index whose layout is checked, lent in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Idx<'a> {
  /// The hash of its ids and checksums.
  format: ObjectFormat,
  /// Its objects.
  count: usize,
  /// The fanout, 256 counts.
  fanout: &'a [u8],
  /// The ids, sorted.
  ids: &'a [u8],
  /// The CRC32s, in id order.
  crcs: &'a [u8],
  /// The 4-byte offsets, in id order.
  offsets: &'a [u8],
  /// The 8-byte offsets.
  large: &'a [u8],
  /// The copy of its pack's trailer.
  pack_checksum: ObjectId,
}

/// The 4-byte number at entry `index` of `table`.
fn word_at(table: &[u8], index: usize) -> Option<u32> {
  let start = index.checked_mul(WORD)?;
  let digits: [u8; WORD] = table
    .get(start..start.checked_add(WORD)?)?
    .try_into()
    .ok()?;
  Some(u32::from_be_bytes(digits))
}

/// The 8-byte number at entry `index` of `table`.
fn large_at(table: &[u8], index: usize) -> Option<u64> {
  let start = index.checked_mul(LARGE_ENTRY)?;
  let digits: [u8; LARGE_ENTRY] = table
    .get(start..start.checked_add(LARGE_ENTRY)?)?
    .try_into()
    .ok()?;
  Some(u64::from_be_bytes(digits))
}

/// The `len` bytes at `*at` in `bytes`, moving `*at` past them.
fn take<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> Option<&'a [u8]> {
  let end = at.checked_add(len)?;
  let taken = bytes.get(*at..end)?;
  *at = end;
  Some(taken)
}

/// The bytes of an index of `count` objects without its 8-byte table, in a hash of `raw_len` bytes: `None` past any
/// length.
fn fixed_len(count: usize, raw_len: usize) -> Option<usize> {
  let per_object = raw_len.checked_add(WORD)?.checked_add(WORD)?;
  IDS_AT
    .checked_add(count.checked_mul(per_object)?)?
    .checked_add(raw_len.checked_mul(TRAILING_HASHES)?)
}

/// The count a fanout ends with, once each count is at least the one before it.
fn fanout_count(fanout: &[u8]) -> Result<usize, Refusal> {
  let mut before = 0;
  for (byte, digits) in (0..=u8::MAX).zip(fanout.chunks_exact(WORD)) {
    let count = digits
      .try_into()
      .map(u32::from_be_bytes)
      .unwrap_or_default();
    if count < before {
      return Err(Refusal::FanoutDown { byte });
    }
    before = count;
  }
  Ok(usize::try_from(before).unwrap_or(usize::MAX))
}

/// The 8-byte table's entries, once the bytes between the 4-byte offsets and the trailing hashes are whole entries and
/// no more of them than an index of `count` objects holds.
fn large_table_entries(large_len: usize, count: usize) -> Result<usize, Refusal> {
  let entries = large_len.checked_div(LARGE_ENTRY).unwrap_or_default();
  if large_len.checked_rem(LARGE_ENTRY) != Some(0) || entries > count.saturating_sub(1) {
    return Err(Refusal::LargeTable {
      len: to_u64(large_len),
      count: to_u64(count),
    });
  }
  Ok(entries)
}

impl<'a> Idx<'a> {
  /// Decodes a version 2 index of `format`'s hash: its checksum first, then its layout (the module's list).
  pub fn decode(bytes: &'a [u8], format: ObjectFormat) -> Result<Self, Refusal> {
    let raw_len = format.raw_len();
    let len = to_u64(bytes.len());
    let smallest = fixed_len(0, raw_len).unwrap_or(usize::MAX);
    let truncated = Refusal::Truncated {
      needed: to_u64(smallest),
      left: len,
    };
    if bytes.len() < smallest {
      return Err(truncated);
    }
    let guarded_len = bytes.len().checked_sub(raw_len).ok_or(truncated)?;
    let (guarded, own) = bytes.split_at_checked(guarded_len).ok_or(truncated)?;
    let checksum = checksum_of(format, guarded).map_err(Refusal::Collision)?;
    if checksum.as_bytes() != own {
      return Err(Refusal::Checksum);
    }
    if guarded.get(..MAGIC.len()) != Some(&MAGIC[..]) {
      return Err(Refusal::Magic);
    }
    let version = word_at(guarded, 1).ok_or(truncated)?;
    if version != VERSION {
      return Err(Refusal::Version { version });
    }
    let fanout = guarded.get(FANOUT_AT..IDS_AT).ok_or(truncated)?;
    let count = fanout_count(fanout)?;
    let count_past_file = Refusal::CountPastFile {
      count: to_u64(count),
      len,
    };
    let fixed = fixed_len(count, raw_len).ok_or(count_past_file)?;
    let large_len = bytes.len().checked_sub(fixed).ok_or(count_past_file)?;
    let large_entries = large_table_entries(large_len, count)?;
    let mut at = IDS_AT;
    let mut next = |len: usize| take(guarded, &mut at, len).ok_or(count_past_file);
    let ids = next(count.saturating_mul(raw_len))?;
    let crcs = next(count.saturating_mul(WORD))?;
    let offsets = next(count.saturating_mul(WORD))?;
    let large = next(large_len)?;
    let pack_checksum = ObjectId::from_bytes(format, next(raw_len)?).ok_or(truncated)?;
    let idx = Self {
      format,
      count,
      fanout,
      ids,
      crcs,
      offsets,
      large,
      pack_checksum,
    };
    idx.check_order()?;
    idx.check_fanout()?;
    idx.check_offsets(large_entries)?;
    Ok(idx)
  }

  /// The ids strictly ascending.
  fn check_order(&self) -> Result<(), Refusal> {
    let mut ids = self.ids.chunks_exact(self.format.raw_len());
    let Some(mut previous) = ids.next() else {
      return Ok(());
    };
    for (before, id) in ids.enumerate() {
      let position = to_u64(before.saturating_add(1));
      match previous.cmp(id) {
        core::cmp::Ordering::Less => previous = id,
        core::cmp::Ordering::Equal => return Err(Refusal::Duplicate { position }),
        core::cmp::Ordering::Greater => return Err(Refusal::Unsorted { position }),
      }
    }
    Ok(())
  }

  /// Each fanout count the number of ids whose first byte is at most its byte.
  fn check_fanout(&self) -> Result<(), Refusal> {
    let mut first_bytes = self
      .ids
      .chunks_exact(self.format.raw_len())
      .filter_map(|id| id.first())
      .peekable();
    let mut counted: usize = 0;
    for byte in 0..=u8::MAX {
      while first_bytes.next_if(|first| **first <= byte).is_some() {
        counted = counted.saturating_add(1);
      }
      let says = word_at(self.fanout, usize::from(byte)).unwrap_or_default();
      if usize::try_from(says).ok() != Some(counted) {
        return Err(Refusal::FanoutMismatch {
          byte,
          says: u64::from(says),
          counted: to_u64(counted),
        });
      }
    }
    Ok(())
  }

  /// Every large offset naming an entry of the 8-byte table, and no offset inside a pack's header.
  fn check_offsets(&self, large_entries: usize) -> Result<(), Refusal> {
    for position in 0..self.count {
      let word = word_at(self.offsets, position).unwrap_or_default();
      let index = word & SMALL_MAX;
      let in_table = usize::try_from(index).is_ok_and(|index| index < large_entries);
      if word & LARGE != 0 && !in_table {
        return Err(Refusal::LargeIndex {
          position: to_u64(position),
          index,
          entries: to_u64(large_entries),
        });
      }
      let offset = self.offset(position).unwrap_or_default();
      if offset < to_u64(HEADER_LEN) {
        return Err(Refusal::OffsetInHeader {
          position: to_u64(position),
          offset,
        });
      }
    }
    Ok(())
  }

  /// The hash of its ids and checksums.
  pub fn format(&self) -> ObjectFormat {
    self.format
  }

  /// Its objects.
  pub fn len(&self) -> usize {
    self.count
  }

  /// Whether it has no objects.
  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  /// The bytes of the id at `position` in id order.
  fn id_bytes(&self, position: usize) -> Option<&'a [u8]> {
    let raw_len = self.format.raw_len();
    let start = position.checked_mul(raw_len)?;
    self.ids.get(start..start.checked_add(raw_len)?)
  }

  /// The id at `position` in id order.
  pub fn id(&self, position: usize) -> Option<ObjectId> {
    ObjectId::from_bytes(self.format, self.id_bytes(position)?)
  }

  /// The CRC32 of the object at `position`.
  pub fn crc32(&self, position: usize) -> Option<u32> {
    word_at(self.crcs, position)
  }

  /// Where in the pack the object at `position` begins.
  pub fn offset(&self, position: usize) -> Option<u64> {
    let word = word_at(self.offsets, position)?;
    if word & LARGE == 0 {
      return Some(u64::from(word));
    }
    large_at(self.large, usize::try_from(word & SMALL_MAX).ok()?)
  }

  /// The object at `position`.
  pub fn entry(&self, position: usize) -> Option<Entry> {
    Some(Entry {
      id: self.id(position)?,
      crc32: self.crc32(position)?,
      offset: self.offset(position)?,
    })
  }

  /// Every object, in id order.
  pub fn entries(&self) -> impl Iterator<Item = Entry> + use<'a> {
    let idx = *self;
    (0..self.count).filter_map(move |position| idx.entry(position))
  }

  /// The position of `id`: the fanout narrows the search to the ids that share its first byte, then a binary search.
  pub fn find(&self, id: &ObjectId) -> Option<usize> {
    if id.format() != self.format {
      return None;
    }
    let wanted = id.as_bytes();
    let first = *wanted.first()?;
    let end = usize::try_from(word_at(self.fanout, usize::from(first))?).ok()?;
    let start = match first.checked_sub(1) {
      Some(before) => usize::try_from(word_at(self.fanout, usize::from(before))?).ok()?,
      None => 0,
    };
    let (mut low, mut high) = (start, end);
    while low < high {
      let middle = low.saturating_add(high.saturating_sub(low).checked_div(2)?);
      match self.id_bytes(middle)?.cmp(wanted) {
        core::cmp::Ordering::Less => low = middle.saturating_add(1),
        core::cmp::Ordering::Greater => high = middle,
        core::cmp::Ordering::Equal => return Some(middle),
      }
    }
    None
  }

  /// The copy of its pack's trailer.
  pub fn pack_checksum(&self) -> ObjectId {
    self.pack_checksum
  }

  /// Holds this index to `pack` (the module's list), charging the table of objects ordered by offset to `admission`
  /// before it is built.
  pub fn check_pack(&self, pack: &Pack<'_>, admission: &mut Admission) -> Result<(), Mismatch> {
    self.check_pairing(pack)?;
    let by_offset = self.by_offset(pack, admission)?;
    self.check_spans(pack, &by_offset)
  }

  /// One hash, one count and one trailer, checked in the order git's `open_packed_git_1` checks the last two.
  fn check_pairing(&self, pack: &Pack<'_>) -> Result<(), Mismatch> {
    if pack.format() != self.format {
      return Err(Mismatch::Format);
    }
    let objects = pack.header().objects;
    if usize::try_from(objects).ok() != Some(self.count) {
      return Err(Mismatch::Count {
        pack: u64::from(objects),
        index: to_u64(self.count),
      });
    }
    if pack.checksum() != self.pack_checksum {
      return Err(Mismatch::PackChecksum);
    }
    Ok(())
  }

  /// The objects ordered by offset, each before the pack's trailer.
  fn by_offset(
    &self,
    pack: &Pack<'_>,
    admission: &mut Admission,
  ) -> Result<Vec<(u64, usize)>, Mismatch> {
    admission
      .charge_items::<(u64, usize)>(self.count)
      .map_err(Mismatch::PastAdmission)?;
    let end = to_u64(pack.trailer_at());
    let mut by_offset = Vec::with_capacity(self.count);
    for position in 0..self.count {
      let offset = self.offset(position).unwrap_or(u64::MAX);
      if offset >= end {
        return Err(Mismatch::OffsetOutside {
          position: to_u64(position),
          offset,
        });
      }
      by_offset.push((offset, position));
    }
    by_offset.sort_unstable();
    Ok(by_offset)
  }

  /// The objects back to back from the header's end to the trailer, each with its CRC32 over its bytes: from its
  /// offset to the next object's, the last to the trailer, as `verify_packfile` takes them.
  fn check_spans(&self, pack: &Pack<'_>, by_offset: &[(u64, usize)]) -> Result<(), Mismatch> {
    let end = to_u64(pack.trailer_at());
    let first = by_offset.first().map_or(end, |(offset, _)| *offset);
    if first != to_u64(HEADER_LEN) {
      return Err(Mismatch::Unindexed {
        at: to_u64(HEADER_LEN),
      });
    }
    let mut objects = by_offset.iter().peekable();
    while let Some((offset, position)) = objects.next() {
      let next = match objects.peek() {
        Some((next, next_position)) if next == offset => {
          return Err(Mismatch::SharedOffset {
            position: to_u64(*next_position),
            offset: *offset,
          });
        }
        Some((next, _)) => *next,
        None => end,
      };
      let span = usize::try_from(*offset)
        .ok()
        .zip(usize::try_from(next).ok())
        .and_then(|(start, stop)| pack.bytes().get(start..stop));
      if span.map(crc32fast::hash) != self.crc32(*position) {
        return Err(Mismatch::Crc {
          position: to_u64(*position),
        });
      }
    }
    Ok(())
  }
}

/// The entries among `entries` whose offsets go to the 8-byte table: those past `small_max`.
fn large_count(entries: &[Entry], small_max: u64) -> usize {
  entries
    .iter()
    .filter(|entry| entry.offset > small_max)
    .count()
}

/// The bytes [`write`] writes for `entries` in `format`'s hash.
pub fn written_len(format: ObjectFormat, entries: &[Entry]) -> u64 {
  let large = large_count(entries, u64::from(SMALL_MAX));
  fixed_len(entries.len(), format.raw_len())
    .and_then(|fixed| fixed.checked_add(large.checked_mul(LARGE_ENTRY)?))
    .map_or(u64::MAX, to_u64)
}

/// Writes an index of `entries` for the pack whose trailer is `pack_checksum`, in that trailer's hash, as git 2.50.1
/// writes it. `entries` are sorted by id in place first, and every check is made before the first byte is written.
pub fn write(
  entries: &mut [Entry],
  pack_checksum: ObjectId,
  out: &mut impl Out,
) -> Result<(), WriteRefusal> {
  write_above(entries, pack_checksum, u64::from(SMALL_MAX), out)
}

/// [`write`], with every offset past `small_max` in the 8-byte table: git's `off32_limit`, which only git's own
/// `index-pack --index-version=2,<offset>` lowers ("to force 64-bit index entries on objects located above the given
/// offset"). The golden vector of a table in use is made so, since no small pack has an offset past 2^31 - 1.
pub(crate) fn write_above(
  entries: &mut [Entry],
  pack_checksum: ObjectId,
  small_max: u64,
  out: &mut impl Out,
) -> Result<(), WriteRefusal> {
  let format = pack_checksum.format();
  let small_max = small_max.min(u64::from(SMALL_MAX));
  check_entries(entries, format, small_max)?;
  let mut checksummed = Checksummed::new(format, out);
  checksummed.put(&MAGIC);
  checksummed.put(&VERSION.to_be_bytes());
  put_fanout(entries, &mut checksummed);
  for entry in entries.iter() {
    checksummed.put(entry.id.as_bytes());
  }
  for entry in entries.iter() {
    checksummed.put(&entry.crc32.to_be_bytes());
  }
  put_offsets(entries, small_max, &mut checksummed);
  checksummed.put(pack_checksum.as_bytes());
  checksummed
    .finish()
    .map(|_| ())
    .map_err(WriteRefusal::Collision)
}

/// Sorts `entries` by id and checks every rule of the format before a byte is written: one hash, no object inside a
/// pack's header, a count a fanout holds, one entry an id, and no more large offsets than an index holds.
fn check_entries(
  entries: &mut [Entry],
  format: ObjectFormat,
  small_max: u64,
) -> Result<(), WriteRefusal> {
  if let Some(position) = entries.iter().position(|entry| entry.id.format() != format) {
    return Err(WriteRefusal::Format {
      position: to_u64(position),
    });
  }
  if let Some(position) = entries
    .iter()
    .position(|entry| entry.offset < to_u64(HEADER_LEN))
  {
    return Err(WriteRefusal::OffsetInHeader {
      position: to_u64(position),
    });
  }
  if u32::try_from(entries.len()).is_err() {
    return Err(WriteRefusal::TooMany {
      count: to_u64(entries.len()),
    });
  }
  entries.sort_unstable_by(|left, right| left.id.as_bytes().cmp(right.id.as_bytes()));
  let mut ids = entries.iter().map(|entry| entry.id).enumerate().peekable();
  while let Some((_, id)) = ids.next() {
    if let Some((position, _)) = ids.next_if(|(_, next)| *next == id) {
      return Err(WriteRefusal::Duplicate {
        position: to_u64(position),
      });
    }
  }
  let large = large_count(entries, small_max);
  let most = entries.len().saturating_sub(1).min(
    usize::try_from(SMALL_MAX)
      .unwrap_or(usize::MAX)
      .saturating_add(1),
  );
  if large > most {
    return Err(WriteRefusal::LargeOffsets {
      count: to_u64(large),
    });
  }
  Ok(())
}

/// Writes the fanout of `entries`, sorted by id: for each byte value, the entries whose id's first byte is at most it.
fn put_fanout(entries: &[Entry], out: &mut impl Out) {
  let mut first_bytes = entries
    .iter()
    .filter_map(|entry| entry.id.as_bytes().first().copied())
    .peekable();
  let mut counted: u32 = 0;
  for byte in 0..=u8::MAX {
    while first_bytes.next_if(|first| *first <= byte).is_some() {
      counted = counted.saturating_add(1);
    }
    out.put(&counted.to_be_bytes());
  }
}

/// Writes the 4-byte offsets of `entries`, sorted by id, then the 8-byte table of those past `small_max`, numbered in
/// id order as `write_idx_file` numbers them.
fn put_offsets(entries: &[Entry], small_max: u64, out: &mut impl Out) {
  let mut next_large: u32 = 0;
  for entry in entries {
    let word = match u32::try_from(entry.offset) {
      Ok(small) if entry.offset <= small_max => small,
      _ => {
        let word = LARGE | next_large;
        next_large = next_large.saturating_add(1);
        word
      }
    };
    out.put(&word.to_be_bytes());
  }
  for entry in entries.iter().filter(|entry| entry.offset > small_max) {
    out.put(&entry.offset.to_be_bytes());
  }
}

#[cfg(test)]
#[path = "../tests/pack_idx.rs"]
mod tests;

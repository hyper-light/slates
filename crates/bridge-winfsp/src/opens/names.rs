//! The kernel's names in A-198's held ledger (§4.6 Windows). A namespace rename made through another attachment
//! does not rename WinFsp's cached node. Keep the name actually issued to that node, including every alias. Each
//! slot has two bounded UTF-16 records: publish the inactive record only after its generation, revision and CRC32C
//! are complete. A killed copy leaves the other version readable. The section reserves the format's maximum names
//! but commits only touched ranges; no path-sized allocation or namespace walk runs during a normal read.
//!
//! **Two regions, because Windows commits by the 64 KiB granule** (`slates_mem::SparseObject`). Windows' own limit on a
//! file node's name is 32,767 UTF-16 units: `UNICODE_STRING.Length` is a `USHORT` counting bytes. One record that long
//! per version would put every slot's two heads more than a granule apart, so each open would commit at least two
//! granules of its own; with the record a short name needs, neighbouring slots share theirs. So a record keeps its
//! first [`INLINE_BYTES`] in place, and only a longer name touches its tail in the second region, where each slot and
//! version has the rest of the limit reserved. A short path commits what it did before this split, a long one what
//! its length needs, and the reserve is bounded by the slot count (`bytes`). Until 2026-11-04 the record ended at 1,024
//! units and every open past it was refused
//! (`docs/bugs/2026-11-04-a-winfsp-open-past-1024-units-was-refused.md`).

use super::{Memory, Refusal, crc32c};

/// Shape: one complete name and one version being replaced; a single receiver serializes kernel renames.
const VERSIONS: usize = 2;
/// Format: the longest name a record holds, in bytes: [`crate::notify::MAX_PATH_UNITS`] UTF-16 units.
const NAME_BYTES: usize = crate::notify::MAX_PATH_UNITS * size_of::<u16>();
/// Derived: a name's bytes kept in the record itself: WinFsp's largest path in a transaction response
/// (`FSP_FSCTL_TRANSACT_PATH_SIZEMAX`, 1,024 units, `fsctl.h`), so a slot's record stays small enough for neighbouring
/// slots to share a commit granule.
const INLINE_BYTES: usize = 1024 * size_of::<u16>();
/// Derived: the rest of the longest name, reserved in the second region per slot and version.
const TAIL_BYTES: usize = NAME_BYTES - INLINE_BYTES;
/// Format: publication byte, slot generation, revision, the name's UTF-16 byte length and its CRC32C.
const HEAD: usize =
  size_of::<u8>() + size_of::<u32>() + size_of::<u64>() + size_of::<u16>() + size_of::<u32>();
/// Format: a complete record in the first region: its head and the name's first bytes.
const RECORD: usize = HEAD + INLINE_BYTES;
/// Format: the revision follows publication and slot generation.
const REVISION_AT: usize = size_of::<u8>() + size_of::<u32>();
/// Format: the byte length follows the revision.
const LENGTH_AT: usize = REVISION_AT + size_of::<u64>();
/// Format: the checksum follows the byte length, and covers everything after the publication byte but itself.
const CHECKSUM_AT: usize = LENGTH_AT + size_of::<u16>();

/// The reserve of `bound` slots: every version's record and its tail. At `MAX_OPEN_HANDLES` this is 8.6 GiB of a
/// `SEC_RESERVE` section per mount, charged to nothing until touched.
pub(super) fn bytes(bound: usize) -> Result<usize, Refusal> {
  bound
    .checked_mul(VERSIONS)
    .and_then(|slots| slots.checked_mul(RECORD.checked_add(TAIL_BYTES)?))
    .ok_or(Refusal::Bound)
}

/// Which record this slot's version is, and the first byte of the record region.
fn base(bound: usize, slot: usize, version: usize) -> Result<(usize, usize), Refusal> {
  if slot >= bound || version >= VERSIONS {
    return Err(Refusal::Bound);
  }
  let record = slot
    .checked_mul(VERSIONS)
    .and_then(|slot| slot.checked_add(version))
    .ok_or(Refusal::Bound)?;
  let region = bound
    .checked_mul(super::FRAME)
    .and_then(|body| body.checked_add(super::HEADER))
    .ok_or(Refusal::Bound)?;
  Ok((record, region))
}

fn offset(bound: usize, slot: usize, version: usize) -> Result<usize, Refusal> {
  let (record, region) = base(bound, slot, version)?;
  record
    .checked_mul(RECORD)
    .and_then(|at| region.checked_add(at))
    .ok_or(Refusal::Bound)
}

/// Where this record's tail lies: the second region follows every slot's records.
fn tail_offset(bound: usize, slot: usize, version: usize) -> Result<usize, Refusal> {
  let (record, region) = base(bound, slot, version)?;
  let tails = bound
    .checked_mul(VERSIONS)
    .and_then(|records| records.checked_mul(RECORD))
    .and_then(|records| region.checked_add(records))
    .ok_or(Refusal::Bound)?;
  record
    .checked_mul(TAIL_BYTES)
    .and_then(|at| tails.checked_add(at))
    .ok_or(Refusal::Bound)
}

/// The record's checksum over its head (past the publication byte, up to the checksum) and its whole name.
fn checksum(head: &[u8; HEAD], name: &[u8]) -> Result<u32, Refusal> {
  let mut covered = Vec::new();
  covered
    .try_reserve_exact(CHECKSUM_AT.saturating_add(name.len()))
    .map_err(|_| Refusal::Bound)?;
  covered.extend_from_slice(head.get(1..CHECKSUM_AT).ok_or(Refusal::Corrupt)?);
  covered.extend_from_slice(name);
  Ok(crc32c(&covered))
}

/// One record: its revision and name, or why it is not a complete name of this generation. Its head is read first, so
/// nothing past the name's own length is reached (and committed) by reading it.
fn decoded(
  memory: &impl Memory,
  (bound, slot, version): (usize, usize, usize),
  generation: u32,
) -> Result<(u64, String), Refusal> {
  let at = offset(bound, slot, version)?;
  let mut head = [0; HEAD];
  memory.read(at, &mut head)?;
  if head.first() != Some(&1) || super::read_u32(&head, 1)? != generation {
    return Err(Refusal::Corrupt);
  }
  let revision = super::read_u64(&head, REVISION_AT)?;
  let length = usize::from(u16::from_le_bytes(
    head
      .get(LENGTH_AT..CHECKSUM_AT)
      .ok_or(Refusal::Corrupt)?
      .try_into()
      .map_err(|_| Refusal::Corrupt)?,
  ));
  if revision == 0 || length == 0 || length > NAME_BYTES || !length.is_multiple_of(size_of::<u16>())
  {
    return Err(Refusal::Corrupt);
  }
  let mut name = Vec::new();
  name.try_reserve_exact(length).map_err(|_| Refusal::Bound)?;
  name.resize(length, 0);
  let (first, rest) = name
    .split_at_mut_checked(length.min(INLINE_BYTES))
    .ok_or(Refusal::Corrupt)?;
  memory.read(at.checked_add(HEAD).ok_or(Refusal::Bound)?, first)?;
  if !rest.is_empty() {
    memory.read(tail_offset(bound, slot, version)?, rest)?;
  }
  if checksum(&head, &name)? != super::read_u32(&head, CHECKSUM_AT)? {
    return Err(Refusal::Corrupt);
  }
  let units: Vec<u16> = name
    .as_chunks::<{ size_of::<u16>() }>()
    .0
    .iter()
    .map(|pair| u16::from_le_bytes(*pair))
    .collect();
  let name = String::from_utf16(&units).map_err(|_| Refusal::Corrupt)?;
  if !name.starts_with('\\') || name.contains('\0') {
    return Err(Refusal::Corrupt);
  }
  Ok((revision, name))
}

pub(super) fn read(
  memory: &impl Memory,
  bound: usize,
  slot: usize,
  generation: u32,
) -> Result<(u64, String), Refusal> {
  let mut current: Option<(u64, String)> = None;
  for version in 0..VERSIONS {
    if let Ok(candidate) = decoded(memory, (bound, slot, version), generation)
      && current
        .as_ref()
        .is_none_or(|(revision, _)| *revision < candidate.0)
    {
      current = Some(candidate);
    }
  }
  current.ok_or(Refusal::Corrupt)
}

fn write(
  memory: &mut impl Memory,
  bound: usize,
  slot: usize,
  generation: u32,
  revision: u64,
  name: &str,
) -> Result<(), Refusal> {
  let cap = crate::notify::MAX_PATH_UNITS
    .checked_add(1)
    .ok_or(Refusal::Bound)?;
  let count = name.encode_utf16().take(cap).count();
  if !name.starts_with('\\') || name.contains('\0') || count > crate::notify::MAX_PATH_UNITS {
    return Err(Refusal::Bound);
  }
  let mut encoded = Vec::new();
  encoded
    .try_reserve_exact(count.saturating_mul(size_of::<u16>()))
    .map_err(|_| Refusal::Bound)?;
  for unit in name.encode_utf16() {
    encoded.extend_from_slice(&unit.to_le_bytes());
  }
  let length = u16::try_from(encoded.len()).map_err(|_| Refusal::Bound)?;
  let mut head = [0; HEAD];
  head
    .get_mut(1..REVISION_AT)
    .ok_or(Refusal::Bound)?
    .copy_from_slice(&generation.to_le_bytes());
  head
    .get_mut(REVISION_AT..LENGTH_AT)
    .ok_or(Refusal::Bound)?
    .copy_from_slice(&revision.to_le_bytes());
  head
    .get_mut(LENGTH_AT..CHECKSUM_AT)
    .ok_or(Refusal::Bound)?
    .copy_from_slice(&length.to_le_bytes());
  let sum = checksum(&head, &encoded)?;
  head
    .get_mut(CHECKSUM_AT..HEAD)
    .ok_or(Refusal::Bound)?
    .copy_from_slice(&sum.to_le_bytes());
  let version = usize::try_from(
    revision
      .checked_rem(u64::try_from(VERSIONS).map_err(|_| Refusal::Bound)?)
      .ok_or(Refusal::Bound)?,
  )
  .map_err(|_| Refusal::Bound)?;
  let at = offset(bound, slot, version)?;
  let (first, rest) = encoded
    .split_at_checked(encoded.len().min(INLINE_BYTES))
    .ok_or(Refusal::Bound)?;
  // The publication byte goes first and last: until the last store this version is not a name at all, and the other
  // version stays the complete one.
  memory.write(at, &[0])?;
  if !rest.is_empty() {
    memory.write(tail_offset(bound, slot, version)?, rest)?;
  }
  memory.write(
    at.checked_add(1).ok_or(Refusal::Bound)?,
    head.get(1..).ok_or(Refusal::Bound)?,
  )?;
  memory.write(at.checked_add(HEAD).ok_or(Refusal::Bound)?, first)?;
  memory.write(at, &[1])
}

pub(super) fn initial(
  memory: &mut impl Memory,
  bound: usize,
  slot: usize,
  generation: u32,
  name: &str,
) -> Result<(), Refusal> {
  write(memory, bound, slot, generation, 1, name)
}

pub(super) fn rename(
  memory: &mut impl Memory,
  bound: usize,
  slot: usize,
  generation: u32,
  from: &str,
  to: &str,
) -> Result<(), Refusal> {
  let (revision, name) = read(memory, bound, slot, generation)?;
  let Some(rest) = name
    .strip_prefix(from)
    .filter(|rest| rest.is_empty() || rest.starts_with('\\'))
  else {
    return Ok(());
  };
  let renamed = format!("{to}{rest}");
  if renamed == name {
    return Ok(());
  }
  write(
    memory,
    bound,
    slot,
    generation,
    revision.checked_add(1).ok_or(Refusal::Exhausted)?,
    &renamed,
  )
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
  use super::*;
  #[derive(Clone)]
  struct Ram {
    bytes: Vec<u8>,
    left: usize,
    /// The furthest byte a read reached, to hold a short name to its own record.
    reached: std::cell::Cell<usize>,
  }
  impl Ram {
    fn of(bound: usize) -> Ram {
      Ram {
        bytes: vec![0; super::super::bytes(bound).unwrap()],
        left: usize::MAX,
        reached: std::cell::Cell::new(0),
      }
    }
  }
  impl Memory for Ram {
    fn len(&self) -> usize {
      self.bytes.len()
    }
    fn read(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal> {
      into.copy_from_slice(&self.bytes[at..at + into.len()]);
      self.reached.set(self.reached.get().max(at + into.len()));
      Ok(())
    }
    fn write(&mut self, at: usize, from: &[u8]) -> Result<(), Refusal> {
      let copied = from.len().min(self.left);
      self.bytes[at..at + copied].copy_from_slice(&from[..copied]);
      self.left -= copied;
      if copied == from.len() {
        Ok(())
      } else {
        Err(Refusal::Bound)
      }
    }
  }

  /// A-198. Do: kill every byte of a name replacement and resume the same rename. Expect: the old or complete new
  /// name remains readable, the resumed rename reaches the new name, and repeating it changes nothing.
  #[test]
  fn a_torn_kernel_rename_keeps_a_complete_name_and_resumes() {
    let mut baseline = Ram::of(1);
    initial(&mut baseline, 1, 0, 1, "\\old\\é.bin").unwrap();
    for killed_after in 0..RECORD {
      let mut crashed = baseline.clone();
      crashed.left = killed_after;
      let _crash = rename(&mut crashed, 1, 0, 1, "\\old", "\\new");
      let observed = read(&crashed, 1, 0, 1).unwrap().1;
      assert!(observed == "\\old\\é.bin" || observed == "\\new\\é.bin");
      crashed.left = usize::MAX;
      rename(&mut crashed, 1, 0, 1, "\\old", "\\new").unwrap();
      assert_eq!(read(&crashed, 1, 0, 1).unwrap().1, "\\new\\é.bin");
      rename(&mut crashed, 1, 0, 1, "\\old", "\\new").unwrap();
      assert_eq!(read(&crashed, 1, 0, 1).unwrap().1, "\\new\\é.bin");
    }
  }

  /// A-198. Do: reuse a slot, then rename a different path prefix. Expect: no former generation or partial component
  /// becomes the new open's name, and a path past Windows' own limit is refused without disturbing the complete name.
  #[test]
  fn names_follow_slot_generations_and_component_boundaries() {
    let mut memory = Ram::of(1);
    initial(&mut memory, 1, 0, 1, "\\old").unwrap();
    rename(&mut memory, 1, 0, 1, "\\old", "\\older").unwrap();
    initial(&mut memory, 1, 0, 2, "\\oldish").unwrap();
    rename(&mut memory, 1, 0, 2, "\\old", "\\new").unwrap();
    assert_eq!(read(&memory, 1, 0, 2).unwrap().1, "\\oldish");
    assert_eq!(
      initial(
        &mut memory,
        1,
        0,
        2,
        &format!("\\{}", "a".repeat(crate::notify::MAX_PATH_UNITS))
      ),
      Err(Refusal::Bound)
    );
    assert_eq!(read(&memory, 1, 0, 2).unwrap().1, "\\oldish");
  }

  /// A path of `units` UTF-16 units, every component within WinFsp's 255.
  fn path_of(units: usize) -> String {
    let mut path = String::with_capacity(units);
    while path.len() < units {
      path.push(if path.len().is_multiple_of(200) {
        '\\'
      } else {
        'n'
      });
    }
    path
  }

  /// A-198. Do: record names of 1, 1,024, 1,025, 2,000, 8,000 and 32,767 UTF-16 units in the last slot of three. Expect:
  /// each reads back whole and unchanged; the neighbouring slots' names are untouched, tails included.
  #[test]
  fn names_up_to_windows_own_limit_are_kept_whole() {
    for units in [1, 1024, 1025, 2000, 8000, crate::notify::MAX_PATH_UNITS] {
      let mut memory = Ram::of(3);
      let name = path_of(units);
      initial(&mut memory, 3, 0, 1, "\\first").unwrap();
      initial(
        &mut memory,
        3,
        1,
        1,
        &path_of(crate::notify::MAX_PATH_UNITS),
      )
      .unwrap();
      initial(&mut memory, 3, 2, 1, &name).unwrap();
      assert_eq!(read(&memory, 3, 2, 1).unwrap().1, name, "{units} units");
      assert_eq!(read(&memory, 3, 0, 1).unwrap().1, "\\first");
      assert_eq!(
        read(&memory, 3, 1, 1).unwrap().1,
        path_of(crate::notify::MAX_PATH_UNITS)
      );
    }
  }

  /// A-198. Do: kill every 97th byte of a long name's replacement, one that starts and ends past the first region, and
  /// resume it. Expect: the old or the new name whole, never a mix of their tails.
  #[test]
  fn a_torn_rename_of_a_long_name_keeps_a_complete_name() {
    let old = format!("\\old{}", path_of(3000));
    let new = format!("\\new{}", path_of(3000));
    let mut baseline = Ram::of(1);
    initial(&mut baseline, 1, 0, 1, &old).unwrap();
    for killed_after in (0..new.len() * 2 + HEAD).step_by(97) {
      let mut crashed = baseline.clone();
      crashed.left = killed_after;
      let _crash = rename(&mut crashed, 1, 0, 1, "\\old", "\\new");
      let observed = read(&crashed, 1, 0, 1).unwrap().1;
      assert!(observed == old || observed == new, "killed {killed_after}");
      crashed.left = usize::MAX;
      rename(&mut crashed, 1, 0, 1, "\\old", "\\new").unwrap();
      assert_eq!(read(&crashed, 1, 0, 1).unwrap().1, new);
    }
  }

  /// A-198. Do: flip one byte of a long name's tail. Expect: the checksum refuses it, so a torn or corrupt tail is never
  /// taken for the kernel's name.
  #[test]
  fn a_corrupt_tail_is_refused() {
    let mut memory = Ram::of(1);
    initial(&mut memory, 1, 0, 1, &path_of(2000)).unwrap();
    let tail = tail_offset(1, 0, 1).unwrap();
    memory.bytes[tail] ^= 1;
    assert_eq!(read(&memory, 1, 0, 1), Err(Refusal::Corrupt));
  }

  /// A-198, Windows' commit by the granule. Do: record and read a short name in the first of 16 slots. Expect: nothing is
  /// read past the first region: a short path's open reaches none of the long-name reserve, so it commits none of it.
  #[test]
  fn a_short_name_never_reaches_the_tail_region() {
    let mut memory = Ram::of(16);
    initial(&mut memory, 16, 0, 1, "\\short").unwrap();
    assert_eq!(read(&memory, 16, 0, 1).unwrap().1, "\\short");
    assert!(memory.reached.get() < tail_offset(16, 0, 0).unwrap());
    assert!(
      memory.reached.get() <= offset(16, 1, 0).unwrap(),
      "and no further than the slot's own records"
    );
  }
}

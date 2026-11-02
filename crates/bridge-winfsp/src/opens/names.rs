//! The kernel's names in A-198's held ledger (§4.6 Windows). A namespace rename made through another attachment
//! does not rename WinFsp's cached node. Keep the name actually issued to that node, including every alias. Each
//! slot has two bounded UTF-16 records: publish the inactive record only after its generation, revision and CRC32C
//! are complete. A killed copy leaves the other version readable. The section reserves the format's maximum names
//! but commits only touched ranges; no path-sized allocation or namespace walk runs during a normal read.

use super::{Memory, Refusal, crc32c};

/// Shape: one complete name and one version being replaced; a single receiver serializes kernel renames.
const VERSIONS: usize = 2;
/// Format: WinFsp's longest admitted path, measured in UTF-16 units (`FSP_FSCTL_TRANSACT_PATH_SIZEMAX`).
const PATH_BYTES: usize = crate::notify::MAX_PATH_UNITS * size_of::<u16>();
/// Format: publication byte, slot generation, revision and the UTF-16 byte length.
const HEAD: usize = size_of::<u8>() + size_of::<u32>() + size_of::<u64>() + size_of::<u16>();
/// Format: the maximum complete record, including its checksum.
const RECORD: usize = HEAD + PATH_BYTES + size_of::<u32>();
/// Format: the revision follows publication and slot generation.
const REVISION_AT: usize = size_of::<u8>() + size_of::<u32>();
/// Format: the byte length follows the revision.
const LENGTH_AT: usize = REVISION_AT + size_of::<u64>();

pub(super) fn bytes(bound: usize) -> Result<usize, Refusal> {
  bound
    .checked_mul(VERSIONS)
    .and_then(|slots| slots.checked_mul(RECORD))
    .ok_or(Refusal::Bound)
}

fn offset(bound: usize, slot: usize, version: usize) -> Result<usize, Refusal> {
  if slot >= bound || version >= VERSIONS {
    return Err(Refusal::Bound);
  }
  let base = bound
    .checked_mul(super::FRAME)
    .and_then(|body| body.checked_add(super::HEADER))
    .ok_or(Refusal::Bound)?;
  slot
    .checked_mul(VERSIONS)
    .and_then(|slot| slot.checked_add(version))
    .and_then(|record| record.checked_mul(RECORD))
    .and_then(|at| base.checked_add(at))
    .ok_or(Refusal::Bound)
}

fn decoded(bytes: &[u8], generation: u32) -> Result<(u64, String), Refusal> {
  if bytes.first() != Some(&1) || super::read_u32(bytes, 1)? != generation {
    return Err(Refusal::Corrupt);
  }
  let revision = super::read_u64(bytes, REVISION_AT)?;
  let length_end = LENGTH_AT
    .checked_add(size_of::<u16>())
    .ok_or(Refusal::Bound)?;
  let length = usize::from(u16::from_le_bytes(
    bytes
      .get(LENGTH_AT..length_end)
      .ok_or(Refusal::Corrupt)?
      .try_into()
      .map_err(|_| Refusal::Corrupt)?,
  ));
  if revision == 0 || length == 0 || length > PATH_BYTES || !length.is_multiple_of(size_of::<u16>())
  {
    return Err(Refusal::Corrupt);
  }
  let end = HEAD.checked_add(length).ok_or(Refusal::Bound)?;
  if crc32c(bytes.get(1..end).ok_or(Refusal::Corrupt)?) != super::read_u32(bytes, end)? {
    return Err(Refusal::Corrupt);
  }
  let units: Vec<u16> = bytes
    .get(HEAD..end)
    .ok_or(Refusal::Corrupt)?
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
  let mut record = [0; RECORD];
  for version in 0..VERSIONS {
    memory.read(offset(bound, slot, version)?, &mut record)?;
    if let Ok(candidate) = decoded(&record, generation)
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
  let cap = crate::notify::MAX_PATH_UNITS.checked_add(1).ok_or(Refusal::Bound)?;
  let count = name.encode_utf16().take(cap).count();
  if !name.starts_with('\\') || name.contains('\0') || count > crate::notify::MAX_PATH_UNITS {
    return Err(Refusal::Bound);
  }
  let units: Vec<u16> = name.encode_utf16().collect();
  let length = units
    .len()
    .checked_mul(size_of::<u16>())
    .and_then(|length| u16::try_from(length).ok())
    .ok_or(Refusal::Bound)?;
  let mut record = Vec::with_capacity(RECORD);
  record.extend_from_slice(&generation.to_le_bytes());
  record.extend_from_slice(&revision.to_le_bytes());
  record.extend_from_slice(&length.to_le_bytes());
  for unit in units {
    record.extend_from_slice(&unit.to_le_bytes());
  }
  record.extend_from_slice(&crc32c(&record).to_le_bytes());
  let version = usize::try_from(
    revision
      .checked_rem(u64::try_from(VERSIONS).map_err(|_| Refusal::Bound)?)
      .ok_or(Refusal::Bound)?,
  )
  .map_err(|_| Refusal::Bound)?;
  let at = offset(bound, slot, version)?;
  memory.write(at, &[0])?;
  memory.write(at.checked_add(1).ok_or(Refusal::Bound)?, &record)?;
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
  }
  impl Memory for Ram {
    fn len(&self) -> usize {
      self.bytes.len()
    }
    fn read(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal> {
      into.copy_from_slice(&self.bytes[at..at + into.len()]);
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
    let mut baseline = Ram {
      bytes: vec![0; super::super::bytes(1).unwrap()],
      left: usize::MAX,
    };
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
  /// becomes the new open's name, and an overlong path is refused without disturbing the complete name.
  #[test]
  fn names_follow_slot_generations_and_component_boundaries() {
    let mut memory = Ram {
      bytes: vec![0; super::super::bytes(1).unwrap()],
      left: usize::MAX,
    };
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
}

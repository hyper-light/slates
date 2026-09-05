//! Parsing and printing of the values the command line carries: sizes with binary units,
//! volume ids as hexadecimal, snapshot ids as numbers.

use slates_client::{SnapshotId, VolumeId};

/// Format: the binary units a size may carry, each 1024 times the last.
const UNITS: &[(&str, u32)] = &[("b", 0), ("kib", 10), ("mib", 20), ("gib", 30), ("tib", 40)];

/// Parses `4GiB`, `512MiB`, `1024` (bytes); refuses decimal units and non-numbers.
pub(crate) fn parse_size(text: &str) -> Result<u64, String> {
  let lower = text.trim().to_ascii_lowercase();
  let digits_end = lower
    .find(|c: char| !c.is_ascii_digit())
    .unwrap_or(lower.len());
  let (digits, unit) = lower.split_at(digits_end);
  if digits.is_empty() {
    return Err(format!("`{text}` has no number"));
  }
  let value: u64 = digits
    .parse()
    .map_err(|_| format!("`{digits}` is not a number"))?;
  let unit = unit.trim();
  let shift = if unit.is_empty() {
    0
  } else {
    UNITS
      .iter()
      .find(|(u, _)| *u == unit)
      .map(|(_, s)| *s)
      .ok_or_else(|| format!("`{unit}` is not a binary unit (B, KiB, MiB, GiB, TiB)"))?
  };
  value
    .checked_shl(shift)
    .filter(|v| v >> shift == value)
    .ok_or_else(|| format!("`{text}` overflows"))
}

/// Format: a volume id's text: its sixteen bytes as hexadecimal, two characters each.
const ID_HEX_CHARS: usize = 32;
/// Format: the radix of that text.
const HEX_RADIX: u32 = 16;

/// A volume id as 32 hexadecimal characters.
pub(crate) fn volume_id_text(id: VolumeId) -> String {
  id.bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses a volume id from its 32 hexadecimal characters.
pub(crate) fn parse_volume_id(text: &str) -> Result<VolumeId, String> {
  let text = text.trim();
  if text.len() != ID_HEX_CHARS {
    return Err(format!(
      "`{text}` is not {ID_HEX_CHARS} hexadecimal characters"
    ));
  }
  let mut bytes = [0u8; 16];
  for (index, byte) in bytes.iter_mut().enumerate() {
    let pair = text
      .get(index * 2..index * 2 + 2)
      .ok_or_else(|| format!("`{text}` is not hexadecimal"))?;
    *byte =
      u8::from_str_radix(pair, HEX_RADIX).map_err(|_| format!("`{text}` is not hexadecimal"))?;
  }
  Ok(VolumeId { bytes })
}

/// Parses a snapshot id.
pub(crate) fn parse_snapshot(text: &str) -> Result<SnapshotId, String> {
  text
    .trim()
    .parse::<u64>()
    .map(|value| SnapshotId { value })
    .map_err(|e| format!("`{text}`: {e}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Sizes: units, bare bytes, case, a space before the unit.
  #[test]
  fn sizes_parse_with_binary_units() {
    assert_eq!(parse_size("4GiB"), Ok(4 << 30));
    assert_eq!(parse_size("512mib"), Ok(512 << 20));
    assert_eq!(parse_size("1024"), Ok(1024));
    assert_eq!(parse_size("1 KiB"), Ok(1024));
  }

  /// Refusals: a decimal unit, no number, an overflow; ids and snapshots round-trip.
  #[test]
  fn refusals_are_named_and_ids_round_trip() {
    assert!(parse_size("4GB").is_err());
    assert!(parse_size("GiB").is_err());
    assert!(parse_size("99999999999999999999TiB").is_err());
    let id = VolumeId { bytes: [0xab; 16] };
    assert_eq!(parse_volume_id(&volume_id_text(id)), Ok(id));
    assert!(parse_volume_id("abc").is_err());
    assert_eq!(parse_snapshot("7"), Ok(SnapshotId { value: 7 }));
  }
}

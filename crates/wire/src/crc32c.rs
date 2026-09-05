//! CRC32C (Castagnoli): the checksum of control and metadata bodies. Hardware instructions
//! where the CPU has them (SSE4.2 `crc32` on x86-64, the `crc` extension on AArch64), detected
//! at run time, with a slicing-by-eight table for everything else [B: RFC 3720 appendix B.4;
//! C: Intel, "Fast CRC computation for iSCSI polynomial using CRC32 instruction"].

/// Format: the Castagnoli polynomial, reflected.
const POLYNOMIAL: u32 = 0x82F6_3B78;

/// Format: entries per table (one per byte value) and tables (slicing by eight).
const TABLE_ENTRIES: usize = 256;
/// Format: slices.
const SLICES: usize = 8;

/// The slicing-by-eight tables, built once at first use.
struct Tables([[u32; TABLE_ENTRIES]; SLICES]);

static TABLES: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();

fn tables() -> &'static Tables {
  TABLES.get_or_init(|| {
    let mut t = [[0u32; TABLE_ENTRIES]; SLICES];
    for (i, slot) in t[0].iter_mut().enumerate() {
      let mut crc = u32::try_from(i).unwrap_or(0);
      for _ in 0..u8::BITS {
        crc = if crc & 1 == 1 {
          (crc >> 1) ^ POLYNOMIAL
        } else {
          crc >> 1
        };
      }
      *slot = crc;
    }
    for i in 0..TABLE_ENTRIES {
      for k in 1..SLICES {
        let prev = t[k - 1][i];
        t[k][i] =
          (prev >> u8::BITS) ^ t[0][usize::try_from(prev & u32::from(u8::MAX)).unwrap_or(0)];
      }
    }
    Tables(t)
  })
}

/// The CRC32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
  crc32c_append(0, data)
}

/// Continues a CRC32C over more data.
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
  #[cfg(target_arch = "aarch64")]
  {
    if std::arch::is_aarch64_feature_detected!("crc") {
      // SAFETY: the feature was detected on this CPU.
      return unsafe { hardware_aarch64(crc, data) };
    }
  }
  #[cfg(target_arch = "x86_64")]
  {
    if std::arch::is_x86_feature_detected!("sse4.2") {
      // SAFETY: the feature was detected on this CPU.
      return unsafe { hardware_x86_64(crc, data) };
    }
  }
  software(crc, data)
}

fn software(crc: u32, data: &[u8]) -> u32 {
  let t = &tables().0;
  let mut crc = !crc;
  let (chunks, remainder) = data.as_chunks::<SLICES>();
  for chunk in chunks {
    // The eight bytes, the low four folded with the running CRC, each through its own slice.
    let folded = u64::from_le_bytes(*chunk) ^ u64::from(crc);
    let mut next = 0u32;
    for (k, table) in t.iter().rev().enumerate() {
      let shift = u32::try_from(k).unwrap_or(0) * u8::BITS;
      let index = usize::try_from((folded >> shift) & u64::from(u8::MAX)).unwrap_or(0);
      next ^= table[index];
    }
    crc = next;
  }
  for b in remainder {
    let index = usize::try_from((crc ^ u32::from(*b)) & u32::from(u8::MAX)).unwrap_or(0);
    crc = t[0][index] ^ (crc >> u8::BITS);
  }
  !crc
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "crc")]
unsafe fn hardware_aarch64(crc: u32, data: &[u8]) -> u32 {
  use std::arch::aarch64::{__crc32cb, __crc32cd};
  let mut crc = !crc;
  let (chunks, remainder) = data.as_chunks::<{ size_of::<u64>() }>();
  for chunk in chunks {
    crc = __crc32cd(crc, u64::from_le_bytes(*chunk));
  }
  for b in remainder {
    crc = __crc32cb(crc, *b);
  }
  !crc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn hardware_x86_64(crc: u32, data: &[u8]) -> u32 {
  use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
  let mut crc = u64::from(!crc);
  let (chunks, remainder) = data.as_chunks::<{ size_of::<u64>() }>();
  for chunk in chunks {
    crc = _mm_crc32_u64(crc, u64::from_le_bytes(*chunk));
  }
  let mut crc32 = u32::try_from(crc & u64::from(u32::MAX)).unwrap_or(0);
  for b in remainder {
    crc32 = _mm_crc32_u8(crc32, *b);
  }
  !crc32
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_check_value_matches_the_standard_and_hardware_matches_software() {
    // The CRC32C check value of "123456789" (RFC 3720 B.4).
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    assert_eq!(software(0, b"123456789"), 0xE306_9283);
    let data: Vec<u8> = (0..10_007u32)
      .map(|i| u8::try_from((i * 31 + 7) % 256).unwrap())
      .collect();
    assert_eq!(crc32c(&data), software(0, &data));
    assert_eq!(crc32c(&[]), 0);
    // Appending equals hashing the concatenation.
    let (a, b) = data.split_at(4_001);
    assert_eq!(crc32c_append(crc32c(a), b), crc32c(&data));
  }
}

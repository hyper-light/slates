//! Bundle1 (`HG10`): one changegroup of version `01` after a six-byte header (A-514; `internals.bundles` [B]).
//!
//! The header is `HG10` and two bytes naming the compression of everything after it: `UN` (none), `GZ` (zlib) or `BZ`
//! (bzip2, whose own leading `BZ` the header doubles as). This module reads and writes `UN` bundles and refuses the
//! compressed ones by kind until the engines are wired (D-17). Bundle1 is what clients without bundle2 push and pull,
//! where a server's policy still allows them (7.2's `server.bundle1`).

use crate::Truncated;
use crate::read::Cursor;

/// Format: bundle1's magic (`internals.bundles` "HG10").
pub const MAGIC: [u8; 4] = *b"HG10";

/// Format: the compression marker of an uncompressed bundle1.
pub const UNCOMPRESSED: [u8; 2] = *b"UN";

/// Format: the compression markers Mercurial defines for bundle1 besides `UN`: zlib and bzip2.
pub const COMPRESSED: [[u8; 2]; 2] = [*b"GZ", *b"BZ"];

/// Why a bundle1 was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The first four bytes are not `HG10`.
  ForeignMagic {
    /// The bytes found.
    found: [u8; 4],
  },
  /// A compressed bundle1, refused until the compression engines are wired (D-17).
  Compressed {
    /// The compression marker.
    engine: [u8; 2],
  },
  /// A compression marker Mercurial does not define.
  UnknownCompression {
    /// The marker found.
    engine: [u8; 2],
  },
  /// The input ended inside the header.
  Truncated(Truncated),
}

/// The changegroup of an uncompressed bundle1: every byte after its header.
pub fn changegroup(input: &[u8]) -> Result<&[u8], Refusal> {
  let mut cursor = Cursor::new(input);
  let magic: [u8; 4] = cursor.array("bundle1 magic").map_err(Refusal::Truncated)?;
  if magic != MAGIC {
    return Err(Refusal::ForeignMagic { found: magic });
  }
  let engine: [u8; 2] = cursor
    .array("bundle1 compression")
    .map_err(Refusal::Truncated)?;
  if engine == UNCOMPRESSED {
    Ok(cursor.rest())
  } else if COMPRESSED.contains(&engine) {
    Err(Refusal::Compressed { engine })
  } else {
    Err(Refusal::UnknownCompression { engine })
  }
}

/// Appends an uncompressed bundle1 holding `changegroup` (an encoded version `01` changegroup) to `out`.
pub fn encode(changegroup: &[u8], out: &mut Vec<u8>) {
  out.extend_from_slice(&MAGIC);
  out.extend_from_slice(&UNCOMPRESSED);
  out.extend_from_slice(changegroup);
}

#[cfg(test)]
mod tests {
  use super::{Refusal, changegroup, encode};
  use crate::fixtures;

  /// A-514 (golden): do read the bundle1 stock Mercurial 7.2 wrote (`hg bundle --all --type none-v1`) and write its
  /// changegroup back. Expect the six-byte header gone, and the same bytes again from `encode`.
  #[test]
  fn the_golden_bundle1_splits_and_joins_back_to_its_bytes() {
    let cg = changegroup(fixtures::CG01).unwrap();
    assert_eq!(cg.len() + 6, fixtures::CG01.len());
    let mut out = Vec::new();
    encode(cg, &mut out);
    assert_eq!(out, fixtures::CG01);
  }

  /// AC-HG.9, A-514 (hostile input): do give a bundle2's magic, a compressed bundle1, an unknown marker and a header
  /// cut short. Expect each refused by its kind.
  #[test]
  fn foreign_compressed_and_cut_headers_are_refused_by_kind() {
    assert_eq!(
      changegroup(fixtures::CG02),
      Err(Refusal::ForeignMagic { found: *b"HG20" })
    );
    assert_eq!(
      changegroup(b"HG10GZxyz"),
      Err(Refusal::Compressed { engine: *b"GZ" })
    );
    assert_eq!(
      changegroup(b"HG10BZh91AY"),
      Err(Refusal::Compressed { engine: *b"BZ" })
    );
    assert_eq!(
      changegroup(b"HG10ZS"),
      Err(Refusal::UnknownCompression { engine: *b"ZS" })
    );
    assert!(matches!(changegroup(b"HG1"), Err(Refusal::Truncated(_))));
    assert!(matches!(changegroup(b"HG10U"), Err(Refusal::Truncated(_))));
  }
}

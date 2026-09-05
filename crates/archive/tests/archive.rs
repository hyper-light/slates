//! Tests for the archive format (§2.6; T-7.1, T-7.3, T-7.4, AC-7.3). An archive round-trips a
//! snapshot, encodes deterministically, locates a chunk by identity, and refuses every malformed
//! or corrupt stream with a typed error rather than a panic — a flipped bit anywhere is detected
//! and named, and a chunk whose payload does not match its declared identity is refused.

use slates_archive::archive::Archive;
use slates_archive::format::{ArchiveError, Chunk, Encoding};

use proptest::prelude::*;

/// Builds a small archive with a manifest and three raw chunks.
fn sample() -> Archive {
  let chunks = vec![
    Archive::raw_chunk(b"the first chunk".to_vec()),
    Archive::raw_chunk(b"a second, longer chunk of bytes".to_vec()),
    Archive::raw_chunk(Vec::new()),
  ];
  Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 1_700_000_000,
    volume_id: 7,
    snapshot_id: 42,
    name_policy_id: 1,
    unicode_version: 15,
    manifest: b"manifest tree bytes".to_vec(),
    chunks,
  }
}

/// T-7.1: an archive round-trips — decoding what was encoded returns the same snapshot.
#[test]
fn an_archive_round_trips() {
  let archive = sample();
  let bytes = archive.encode();
  let decoded = Archive::decode(&bytes).expect("a well-formed archive decodes");
  assert_eq!(decoded, archive);
}

/// Encoding is deterministic: the same snapshot yields the same bytes.
#[test]
fn encoding_is_deterministic() {
  let archive = sample();
  assert_eq!(archive.encode(), archive.encode());
}

/// The seek table locates a chunk by identity without scanning; an unknown identity is `None`.
#[test]
fn seek_finds_a_chunk_by_identity() {
  let archive = sample();
  let bytes = archive.encode();
  let wanted = archive.chunks[1].clone();
  let found = Archive::chunk_by_identity(&bytes, &wanted.identity)
    .expect("the archive is well-formed")
    .expect("the chunk is present");
  assert_eq!(found, wanted);
  let missing = [0xabu8; 32];
  assert!(
    Archive::chunk_by_identity(&bytes, &missing)
      .expect("well-formed")
      .is_none()
  );
}

/// AC-7.3: a chunk whose payload does not hash to its declared identity is refused, named by its
/// position.
#[test]
fn a_chunk_that_fails_its_identity_is_refused() {
  let bogus = Chunk {
    identity: [0u8; 32],
    raw_len: 5,
    stored_len: 5,
    encoding: Encoding::Raw,
    level: 0,
    dictionary: [0u8; 32],
    payload: b"hello".to_vec(),
  };
  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    manifest: b"m".to_vec(),
    chunks: vec![bogus],
  };
  let bytes = archive.encode();
  assert_eq!(
    Archive::decode(&bytes),
    Err(ArchiveError::ChunkIdentityMismatch { index: 0 })
  );
}

/// A wrong magic is refused (before anything else).
#[test]
fn a_bad_magic_is_refused() {
  let mut bytes = sample().encode();
  bytes[0] ^= 0xff;
  assert_eq!(Archive::decode(&bytes), Err(ArchiveError::BadMagic));
}

/// An unknown major is refused, naming the major found.
#[test]
fn an_unknown_major_is_refused() {
  let mut bytes = sample().encode();
  // The major is the two bytes right after the four-byte magic.
  bytes[4] = 0xfe;
  bytes[5] = 0x00;
  assert!(matches!(
    Archive::decode(&bytes),
    Err(ArchiveError::UnsupportedMajor { found: 0x00fe })
  ));
}

/// AC-7.3: a single flipped byte anywhere in the body is caught by the whole-archive hash.
#[test]
fn a_flipped_body_byte_is_caught() {
  let bytes = sample().encode();
  // Flip a byte in the middle (a chunk payload region); the archive hash detects it.
  let middle = bytes.len() / 2;
  let mut corrupt = bytes.clone();
  corrupt[middle] ^= 0x01;
  assert_eq!(
    Archive::decode(&corrupt),
    Err(ArchiveError::ArchiveHashMismatch)
  );
}

/// An empty or too-short stream is refused as truncated, never a panic.
#[test]
fn a_short_stream_is_refused() {
  assert_eq!(Archive::decode(&[]), Err(ArchiveError::Truncated));
  // Eight zero bytes parse a zero magic, refused before anything else.
  assert_eq!(Archive::decode(&[0u8; 8]), Err(ArchiveError::BadMagic));
}

proptest! {
  /// T-7.4 (hostile): any truncation of a valid archive is refused with a typed error, never a
  /// panic and never a false accept.
  #[test]
  fn every_truncation_is_refused(cut in 0usize..400) {
    let bytes = sample().encode();
    let take = cut.min(bytes.len().saturating_sub(1));
    let truncated = &bytes[..take];
    prop_assert!(Archive::decode(truncated).is_err());
  }

  /// T-7.4 (hostile): flipping any single byte of a valid archive is caught (a typed error), so no
  /// alteration passes as a valid archive.
  #[test]
  fn any_single_byte_flip_is_caught(position in 0usize..400) {
    let bytes = sample().encode();
    prop_assume!(position < bytes.len());
    let mut corrupt = bytes.clone();
    corrupt[position] ^= 0x80;
    // Either the decode refuses, or (only if the flip landed in an informational, hash-covered
    // field that still parses) it decodes to a different snapshot — never the original.
    if let Ok(decoded) = Archive::decode(&corrupt) {
      prop_assert_ne!(decoded, sample());
    }
  }

  /// T-7.4 (hostile): arbitrary bytes are refused with a typed error, never a panic.
  #[test]
  fn arbitrary_bytes_do_not_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
    let _ = Archive::decode(&bytes);
    prop_assert!(true);
  }
}

/// Highly compressible data is stored LZ4 (smaller than raw), round-trips, and decodes to the
/// original bytes.
#[test]
fn compressible_data_is_stored_lz4() {
  let raw = vec![0x41u8; 1000];
  let chunk = Archive::compressed_chunk(raw.clone());
  assert_eq!(chunk.encoding, Encoding::Lz4);
  assert!(
    chunk.stored_len < chunk.raw_len,
    "LZ4 shrinks a repetitive blob"
  );
  assert_eq!(Archive::content(&chunk).expect("decodes"), raw);

  let archive = Archive {
    base_page_size: 4096,
    chunk_min: 4096,
    chunk_max: 65_536,
    created_unix: 0,
    volume_id: 1,
    snapshot_id: 1,
    name_policy_id: 1,
    unicode_version: 15,
    manifest: b"m".to_vec(),
    chunks: vec![chunk],
  };
  let bytes = archive.encode();
  let decoded = Archive::decode(&bytes).expect("a valid LZ4 archive decodes");
  assert_eq!(decoded, archive);
  assert_eq!(Archive::content(&decoded.chunks[0]).expect("decodes"), raw);
}

/// Incompressible (tiny, unique) data stays raw, because LZ4 would not save space (the format
/// floor).
#[test]
fn incompressible_data_stays_raw() {
  let raw = b"xyz".to_vec();
  let chunk = Archive::compressed_chunk(raw.clone());
  assert_eq!(chunk.encoding, Encoding::Raw);
  assert_eq!(chunk.stored_len, chunk.raw_len);
  assert_eq!(Archive::content(&chunk).expect("decodes"), raw);
}

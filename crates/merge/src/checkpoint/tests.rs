//! The checkpoint's codec, version 5 (A-626 part 3 and its continuation (1); A-621's procedure): golden vectors derived
//! outside the crate, for a checkpoint holding a copy and for one of a green that never copied, the determinism gate
//! over every table version 4 added, and hostile input refused typed.

use super::{Changed, Checkpoint, CopyRecord, SUM_BYTES, Sum, Tables};
use crate::chain_entry::Signature;
use crate::ops_doc::DocDecodeError;

/// A decode's answer.
type Decoded = Result<Checkpoint, DocDecodeError>;

/// Shape: the golden checkpoint's length in bytes, from the independent layout.
const GOLDEN_LEN: usize = 2499;
/// Shape: the golden checkpoint's identity, an independent BLAKE3's (module docs of the test below).
const GOLDEN_IDENTITY: &str = "05c1e7f6c7068d8e66f944fdc5ef42701dc35966ca5a96fc37d3c779a3865f09";
/// Shape: the length in bytes of the golden checkpoint of a green that never copied, from the independent layout.
const NEVER_COPIED_LEN: usize = 115;
/// Shape: its identity, an independent BLAKE3's, derived as [`GOLDEN_IDENTITY`] is.
const NEVER_COPIED_IDENTITY: &str =
  "dfa590d1e6984f5fcf917e4b0a9885e6f0953c091a41b6ae429883fd6de08c04";
/// Shape: the modulus of the golden sum's byte pattern (byte `j` is `j % 251`, the BLAKE3 test vectors' input).
const PATTERN: usize = 251;

/// A key's value at a version.
fn at<K, V>(key: K, version: u64, value: Option<V>) -> Changed<K, V> {
  Changed {
    key,
    at: version,
    value,
  }
}

/// The golden sum: byte `j` is `j % 251`.
fn patterned_sum() -> Sum {
  let mut bytes = Box::new([0u8; SUM_BYTES]);
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = u8::try_from(index % PATTERN).unwrap_or_default();
  }
  Sum::new(bytes)
}

/// The copied tables of the golden checkpoint: a file and the directory a copy at version 6 brought, and an attribute
/// it took away.
fn golden_copied() -> Tables {
  Tables {
    files: vec![at("tags/1/f".to_owned(), 6, Some(b"f".to_vec()))],
    dirs: vec![at("tags/1".to_owned(), 6, Some(()))],
    xattrs: vec![at(("tags/1/f".to_owned(), "user.k".to_owned()), 6, None)],
    ..Tables::default()
  }
}

/// A checkpoint touching every kind of table version 5 holds, a copy's record and the sum among them, as the golden
/// vectors state it.
fn golden() -> Checkpoint {
  Checkpoint {
    version: 7,
    accepted: vec![([1; 32], 5)],
    signatures: vec![(
      5,
      Signature {
        kind: 1,
        detail: 2,
        fingerprint: [3; 32],
      },
    )],
    files: vec![
      at("b".to_owned(), 5, Some(b"bytes".to_vec())),
      at("a".to_owned(), 3, None),
    ],
    dirs: vec![at("d".to_owned(), 2, Some(()))],
    modes: vec![at("b".to_owned(), 6, Some(0o640))],
    symlinks: vec![at("s".to_owned(), 4, Some("b".to_owned()))],
    hardlinks: vec![at("h".to_owned(), 7, None)],
    specials: Vec::new(),
    xattrs: vec![at(("b".to_owned(), "user.k".to_owned()), 6, Some(vec![9]))],
    copies: vec![CopyRecord {
      destination: "tags/1".to_owned(),
      version: 6,
      source: "trunk".to_owned(),
      source_version: 5,
    }],
    copied: golden_copied(),
    sum: Some(patterned_sum()),
    head_check: Some((9, [4; 32])),
  }
}

/// A length-prefixed run, as the format writes one.
fn blob(out: &mut Vec<u8>, bytes: &[u8]) {
  out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
  out.extend_from_slice(bytes);
}

/// A key and its version, as a table's entry begins.
fn keyed(out: &mut Vec<u8>, key: &[&[u8]], version: u64) {
  for part in key {
    blob(out, part);
  }
  out.extend_from_slice(&version.to_le_bytes());
}

/// The golden checkpoint's changed tables, field by field.
fn golden_changed(out: &mut Vec<u8>) {
  out.extend_from_slice(&2u32.to_le_bytes());
  keyed(out, &[b"a"], 3);
  out.push(0);
  keyed(out, &[b"b"], 5);
  out.push(1);
  blob(out, b"bytes");
  out.extend_from_slice(&1u32.to_le_bytes());
  keyed(out, &[b"d"], 2);
  out.push(1);
  out.extend_from_slice(&1u32.to_le_bytes());
  keyed(out, &[b"b"], 6);
  out.push(1);
  out.extend_from_slice(&0o640u32.to_le_bytes());
  out.extend_from_slice(&1u32.to_le_bytes());
  keyed(out, &[b"s"], 4);
  out.push(1);
  blob(out, b"b");
  out.extend_from_slice(&1u32.to_le_bytes());
  keyed(out, &[b"h"], 7);
  out.push(0);
  out.extend_from_slice(&0u32.to_le_bytes());
  out.extend_from_slice(&1u32.to_le_bytes());
  keyed(out, &[b"b", b"user.k"], 6);
  out.push(1);
  blob(out, &[9]);
}

/// The golden checkpoint's copies' section, field by field: its presence byte, the copies and the copied tables.
fn golden_copies(out: &mut Vec<u8>) {
  out.push(1);
  out.extend_from_slice(&1u32.to_le_bytes());
  blob(out, b"tags/1");
  out.extend_from_slice(&6u64.to_le_bytes());
  blob(out, b"trunk");
  out.extend_from_slice(&5u64.to_le_bytes());
  out.extend_from_slice(&1u32.to_le_bytes());
  keyed(out, &[b"tags/1/f"], 6);
  out.push(1);
  blob(out, b"f");
  out.extend_from_slice(&1u32.to_le_bytes());
  keyed(out, &[b"tags/1"], 6);
  out.push(1);
  for _ in 0..4 {
    out.extend_from_slice(&0u32.to_le_bytes());
  }
  out.extend_from_slice(&1u32.to_le_bytes());
  keyed(out, &[b"tags/1/f", b"user.k"], 6);
  out.push(0);
}

/// The golden checkpoint's bytes, laid out field by field as version 5 states them (the module docs of
/// `crate::checkpoint`), apart from the encoder.
fn golden_bytes() -> Vec<u8> {
  let mut out = b"GCKP".to_vec();
  out.extend_from_slice(&5u32.to_le_bytes());
  out.extend_from_slice(&7u64.to_le_bytes());
  out.extend_from_slice(&1u32.to_le_bytes());
  out.extend_from_slice(&[1; 32]);
  out.extend_from_slice(&5u64.to_le_bytes());
  out.extend_from_slice(&1u32.to_le_bytes());
  out.extend_from_slice(&5u64.to_le_bytes());
  out.extend_from_slice(&[1, 2]);
  out.extend_from_slice(&[3; 32]);
  golden_changed(&mut out);
  golden_copies(&mut out);
  out.push(1);
  out.extend_from_slice(patterned_sum().bytes());
  out.push(1);
  out.extend_from_slice(&9u64.to_le_bytes());
  out.extend_from_slice(&[4; 32]);
  out
}

/// Golden vectors (A-626 part 3, version 5 since its continuation (1); A-621's procedure): a fixed checkpoint touching
/// every table, a copy's record, its copied keys and the sum, encodes to recorded bytes and hashes to a pinned identity. Both were derived outside the crate:
/// the bytes by the format's layout, field by field, and the identity by a BLAKE3 written in Python from the BLAKE3
/// paper (no library), which reproduces the empty input's hash (`af1349b9…`), the official test vectors at 1, 1023,
/// 1024, 1025, 2048, 2049 and 3072 bytes (one, two and three chunks), and A-646's ops-document vector (218 bytes,
/// `553a422f…6c0d92`). Do: encode the golden checkpoint. Expect its bytes the layout's, 2,499 of them, and its identity
/// the pinned one. If the format changes deliberately, derive both again the same way.
#[test]
fn the_checkpoint_matches_its_golden_vectors() {
  let golden = golden();
  let bytes = golden.encode();
  assert_eq!(bytes.len(), GOLDEN_LEN, "the recorded checkpoint's length");
  assert_eq!(bytes, golden_bytes(), "the encoding changed");
  let hex: String = golden
    .identity()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  assert_eq!(hex, GOLDEN_IDENTITY, "the checkpoint's identity changed");
}

/// The checkpoint of a green that never copied: one increment, which set `f` to `v1`, at version 1.
fn never_copied() -> Checkpoint {
  Checkpoint {
    version: 1,
    accepted: vec![([2; 32], 1)],
    files: vec![at("f".to_owned(), 1, Some(b"v1".to_vec()))],
    ..Checkpoint::default()
  }
}

/// [`never_copied`]'s bytes, laid out field by field apart from the encoder: version 3's tables, then three absent
/// presence bytes (no copies' section, no sum, no head check).
fn never_copied_bytes() -> Vec<u8> {
  let mut out = b"GCKP".to_vec();
  out.extend_from_slice(&5u32.to_le_bytes());
  out.extend_from_slice(&1u64.to_le_bytes());
  out.extend_from_slice(&1u32.to_le_bytes());
  out.extend_from_slice(&[2; 32]);
  out.extend_from_slice(&1u64.to_le_bytes());
  out.extend_from_slice(&0u32.to_le_bytes());
  out.extend_from_slice(&1u32.to_le_bytes());
  keyed(&mut out, &[b"f"], 1);
  out.push(1);
  blob(&mut out, b"v1");
  for _ in 0..6 {
    out.extend_from_slice(&0u32.to_le_bytes());
  }
  out.extend_from_slice(&[0, 0, 0]);
  out
}

/// An identity as lower-case hex.
fn hex_of(identity: &[u8; 32]) -> String {
  identity.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `bytes` decoded with the presence byte `from_end` bytes before the end made 7, neither present nor absent.
fn with_presence_altered(bytes: &[u8], from_end: usize) -> Decoded {
  let mut altered = bytes.to_vec();
  let at = altered.len().saturating_sub(from_end);
  if let Some(byte) = altered.get_mut(at) {
    *byte = 7;
  }
  Checkpoint::decode(&altered)
}

/// A-626 part 3, continued (1): a green that never copied pays a presence byte for the copies' section and one for the
/// sum, not the sum's 2,048 bytes, which had held a small green's first versions unfolded (the service's doubling rule
/// waited on 194 bytes of increment against a 2,233-byte checkpoint, Mac Studio, 2026-11-19). Golden vectors derived
/// outside the crate as the golden checkpoint's are. Do: encode the checkpoint of a green that set one file at version
/// 1; decode it; decode it with the copies' section's presence byte, then the sum's, made neither present nor absent.
/// Expect its bytes the layout's, 115 of them, its identity the pinned one, its decode itself, and each altered presence
/// byte refused `BadKind`.
#[test]
fn a_checkpoint_of_a_green_that_never_copied_carries_no_copies_and_no_sum() {
  let checkpoint = never_copied();
  let bytes = checkpoint.encode();
  assert_eq!(bytes.len(), NEVER_COPIED_LEN, "the recorded length");
  assert_eq!(bytes, never_copied_bytes(), "the encoding changed");
  assert_eq!(hex_of(&checkpoint.identity()), NEVER_COPIED_IDENTITY);
  assert_eq!(Checkpoint::decode(&bytes), Ok(checkpoint));
  assert_eq!(
    (
      with_presence_altered(&bytes, 3),
      with_presence_altered(&bytes, 2)
    ),
    (Err(DocDecodeError::BadKind), Err(DocDecodeError::BadKind))
  );
}

/// The golden checkpoint with every table the version adds declared in another order.
fn reordered() -> Checkpoint {
  let mut reordered = golden();
  reordered.files.reverse();
  reordered.copies.push(CopyRecord {
    destination: "a".to_owned(),
    version: 2,
    source: "trunk".to_owned(),
    source_version: 1,
  });
  reordered.copies.reverse();
  reordered
    .copied
    .files
    .push(at("tags/1/a".to_owned(), 6, None));
  reordered.copied.files.reverse();
  reordered
}

/// Determinism gate (A-626 part 3). Do: encode a checkpoint with copies and copied keys twice, the second with each of
/// those tables declared in the reverse order. Expect: one byte sequence, and the decode equal to the canonical form.
#[test]
fn a_checkpoint_with_copies_encodes_to_one_byte_sequence_whatever_its_order() {
  let first = reordered();
  let mut second = first.clone();
  second.copies.reverse();
  second.copied.files.reverse();
  second.files.reverse();
  assert_eq!(first.encode(), second.encode());
  let mut canonical = first.clone();
  canonical.canonicalize();
  assert_eq!(Checkpoint::decode(&first.encode()), Ok(canonical));
}

/// The golden checkpoint's encoding with one copied file's key written twice, and with one copy's record written twice.
fn doubled() -> (Vec<u8>, Vec<u8>) {
  let mut twice_copied = golden();
  let file = twice_copied.copied.files.clone();
  twice_copied.copied.files.extend(file);
  let mut twice_recorded = golden();
  let record = twice_recorded.copies.clone();
  twice_recorded.copies.extend(record);
  (twice_copied.encode(), twice_recorded.encode())
}

/// Hostile input (A-626 part 3 and its continuation (1)). Do: decode every proper prefix of the golden checkpoint (the
/// sum cut short among them), a version 4 header (the sum in every checkpoint), a copied key written twice, a copy's
/// record written twice, and the encoding with a byte appended. Expect each refused typed: `Truncated` or another
/// refusal for a prefix, never a checkpoint; `BadVersion`; `DuplicatePath` twice; `TrailingBytes`.
#[test]
fn a_hostile_checkpoint_is_refused_typed() {
  let bytes = golden_bytes();
  let cut_short =
    (0..bytes.len()).find(|&cut| Checkpoint::decode(bytes.get(..cut).unwrap_or_default()).is_ok());
  assert_eq!(cut_short, None, "a prefix decoded");
  let mut older = bytes.clone();
  if let Some(version) = older.get_mut(4..8) {
    version.copy_from_slice(&4u32.to_le_bytes());
  }
  assert_eq!(Checkpoint::decode(&older), Err(DocDecodeError::BadVersion));
  let (twice_copied, twice_recorded) = doubled();
  assert_eq!(
    (
      Checkpoint::decode(&twice_copied),
      Checkpoint::decode(&twice_recorded)
    ),
    (
      Err(DocDecodeError::DuplicatePath),
      Err(DocDecodeError::DuplicatePath)
    )
  );
  let mut trailing = bytes;
  trailing.push(0);
  assert_eq!(
    Checkpoint::decode(&trailing),
    Err(DocDecodeError::TrailingBytes)
  );
}

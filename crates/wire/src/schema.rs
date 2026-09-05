//! Schema hashing: a compile-time structural hash of a type's reflection mixed with the hashes of
//! every field type, so a change anywhere in a message's tree changes its hash. The derive emits
//! the calls; both functions are `const fn` so the hash is a constant the header carries.
//!
//! The algorithm is 64-bit FNV-1a over the reflection text [C: Fowler, Noll, Vo], then a
//! multiply-xorshift mix with each field type's hash in order. It identifies schemas, not
//! content: content identity is BLAKE3 (D-17). A collision between two schemas of the same
//! protocol is a bug the golden schema test would show as two kinds sharing a hash.

/// Format: FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// Format: FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// Format: the mixing multiplier (the xorshift64* output multiplier).
const MIX_MULTIPLIER: u64 = 0x2545_f491_4f6c_dd1d;
/// Format: the rotation applied to each field hash before mixing, so field order matters.
const MIX_ROTATE: u32 = 17;
/// Format: the final shift.
const MIX_SHIFT: u32 = 29;

/// FNV-1a over the reflection text.
pub const fn fnv64(text: &str) -> u64 {
  let bytes = text.as_bytes();
  let mut hash = FNV_OFFSET;
  let mut i = 0;
  while i < bytes.len() {
    hash ^= bytes[i] as u64;
    hash = hash.wrapping_mul(FNV_PRIME);
    i += 1;
  }
  hash
}

/// Mixes a type's own hash with its field types' hashes in order.
pub const fn mix(own: u64, fields: &[u64]) -> u64 {
  let mut hash = own;
  let mut i = 0;
  while i < fields.len() {
    hash ^= fields[i].rotate_left(MIX_ROTATE);
    hash = hash.wrapping_mul(MIX_MULTIPLIER);
    hash ^= hash >> MIX_SHIFT;
    i += 1;
  }
  hash
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_reflection_text_and_every_field_hash_change_the_result() {
    let a = mix(fnv64("struct A{x:u32}"), &[fnv64("u32")]);
    let b = mix(fnv64("struct A{x:u64}"), &[fnv64("u64")]);
    let c = mix(fnv64("struct A{x:u32}"), &[fnv64("u64")]);
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_eq!(a, mix(fnv64("struct A{x:u32}"), &[fnv64("u32")]));
  }

  #[test]
  fn fnv_matches_the_published_check_value() {
    // The FNV-1a 64-bit hash of the empty string is the offset basis; of "a" it is documented.
    assert_eq!(fnv64(""), FNV_OFFSET);
    assert_eq!(fnv64("a"), 0xaf63_dc4c_8601_ec8c);
  }
}

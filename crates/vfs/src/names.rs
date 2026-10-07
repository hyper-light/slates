//! Names and the per-volume equivalence policy (D-4): byte-exact, or folded by Unicode
//! normalization and case the way APFS does, so `README` and `readme`, or a precomposed and a
//! decomposed `é`, are one entry on a folding volume (`EEXIST` for the second) and two on an
//! exact one [B: Apple APFS FAQ; B: git-config `core.ignoreCase`, `core.precomposeUnicode`].
//!
//! A lookup hashes the folded form and compares folded forms on a hash hit; the entry stores its
//! name as given, so `readdir` returns what was created.

use std::hash::Hasher;

use unicode_normalization::UnicodeNormalization;

/// How two names are judged equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum NameEquivalence {
  /// Bytes must match (Linux, NTFS in its POSIX mode).
  #[default]
  Exact,
  /// Normalization-insensitive and case-insensitive (APFS's default).
  Fold,
}

/// Format: the longest name a volume accepts, the POSIX `NAME_MAX` every target shares.
pub const NAME_MAX: usize = 255;

/// The folded characters of a name: the bytes as they are for the exact policy, the
/// ASCII-lowercased bytes for an ASCII name under folding (NFC is the identity on ASCII), and
/// the NFC-normalized, lowercased characters otherwise. No allocation on any path, so a lookup
/// costs the scan and nothing else (measured: the allocating fold cost 1.2 µs per lookup on
/// 49-byte names, 2026-09-05, `cargo run --release -p slates-vfs --example vfs_bench`).
pub enum Folded<'a> {
  /// Bytes as given.
  Exact(std::str::Chars<'a>),
  /// ASCII lowercase, byte by byte.
  Ascii(std::str::Bytes<'a>),
  /// NFC then full Unicode lowercase; boxed because the recomposition buffer is large and
  /// this path is the rare one (a non-ASCII name).
  Unicode(Box<UnicodeFold<'a>>),
}

/// NFC recomposition followed by full lowercase, character by character.
pub type UnicodeFold<'a> = std::iter::FlatMap<
  unicode_normalization::Recompositions<std::str::Chars<'a>>,
  std::char::ToLowercase,
  fn(char) -> std::char::ToLowercase,
>;

impl Iterator for Folded<'_> {
  type Item = char;

  fn next(&mut self) -> Option<char> {
    match self {
      Self::Exact(c) => c.next(),
      Self::Ascii(b) => b.next().map(|b| char::from(b.to_ascii_lowercase())),
      Self::Unicode(u) => u.next(),
    }
  }
}

impl NameEquivalence {
  /// Whether the policy distinguishes names that differ only in case (or normalization). `Exact`
  /// does — bytes must match — so it is case-sensitive; `Fold` treats such names as one, so it is
  /// not. A transport reports this to a client (NFS `PATHCONF`'s `case_insensitive`, WinFsp, FSKit).
  pub const fn case_sensitive(self) -> bool {
    matches!(self, NameEquivalence::Exact)
  }

  /// The folded characters of `name` under this policy (no allocation).
  pub fn folded(self, name: &str) -> Folded<'_> {
    match self {
      Self::Exact => Folded::Exact(name.chars()),
      Self::Fold if name.is_ascii() => Folded::Ascii(name.bytes()),
      Self::Fold => Folded::Unicode(Box::new(name.nfc().flat_map(char::to_lowercase))),
    }
  }

  /// The folded form of a name under this policy, as a string (allocates for a folded name;
  /// the hot paths use [`NameEquivalence::folded`]).
  pub fn fold(self, name: &str) -> std::borrow::Cow<'_, str> {
    match self {
      Self::Exact => std::borrow::Cow::Borrowed(name),
      Self::Fold => std::borrow::Cow::Owned(self.folded(name).collect()),
    }
  }

  /// The hash of a name's folded form (FNV-1a over the folded UTF-8 bytes; a fixed algorithm
  /// because the hash orders the directory and must not change between runs).
  pub fn hash(self, name: &str) -> u64 {
    let mut h = Fnv(FNV_OFFSET);
    match self.folded(name) {
      Folded::Exact(_) => h.write(name.as_bytes()),
      Folded::Ascii(bytes) => {
        for b in bytes {
          h.write_u8(b.to_ascii_lowercase());
        }
      }
      Folded::Unicode(chars) => {
        let mut utf8 = [0u8; 4];
        for c in chars {
          h.write(c.encode_utf8(&mut utf8).as_bytes());
        }
      }
    }
    h.finish()
  }

  /// Whether two names are the same entry under this policy.
  pub fn same(self, a: &str, b: &str) -> bool {
    match self {
      Self::Exact => a == b,
      Self::Fold if a.is_ascii() && b.is_ascii() => a.eq_ignore_ascii_case(b),
      Self::Fold => self.folded(a).eq(self.folded(b)),
    }
  }

  /// The order of two names' folded forms — exactly `self.folded(a).cmp(self.folded(b))` — without an iterator step per
  /// character in the common cases: an exact name compares as bytes (UTF-8's byte order is its code-point order), and
  /// two ASCII names under folding compare as lowercased bytes, then by length. A replayed delta binary-searches its
  /// directory's entries with this, and the iterator form was a fifth of a recovery's replay (2026-10-06,
  /// `recover_bench`).
  pub fn compare(self, a: &str, b: &str) -> std::cmp::Ordering {
    match self {
      Self::Exact => a.as_bytes().cmp(b.as_bytes()),
      Self::Fold if a.is_ascii() && b.is_ascii() => {
        for (x, y) in a.bytes().zip(b.bytes()) {
          let (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
          if x != y {
            return x.cmp(&y);
          }
        }
        a.len().cmp(&b.len())
      }
      Self::Fold => self.folded(a).cmp(self.folded(b)),
    }
  }
}

/// Checks a component name: not empty, not `.` or `..`, no separator, no NUL, within `NAME_MAX`.
pub fn check(name: &str) -> Result<(), crate::error::VfsError> {
  check_length(name)?;
  if name.is_empty() || name == "." || name == ".." || name.bytes().any(|b| b == b'/' || b == 0) {
    return Err(crate::error::VfsError::InvalidName);
  }
  Ok(())
}

/// Refuses a component longer than [`NAME_MAX`] bytes `NameTooLong`: the one rule a lookup shares with a
/// creation (POSIX.1-2017 §2.3, `ENAMETOOLONG` "the length of a component of a pathname is longer than
/// {NAME_MAX}"), so a long name is never answered "not found".
pub fn check_length(name: &str) -> Result<(), crate::error::VfsError> {
  if name.len() > NAME_MAX {
    return Err(crate::error::VfsError::NameTooLong);
  }
  Ok(())
}

/// Format: FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// Format: FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

struct Fnv(u64);

impl Hasher for Fnv {
  fn finish(&self) -> u64 {
    self.0
  }

  fn write(&mut self, bytes: &[u8]) {
    for b in bytes {
      self.write_u8(*b);
    }
  }

  fn write_u8(&mut self, b: u8) {
    self.0 ^= u64::from(b);
    self.0 = self.0.wrapping_mul(FNV_PRIME);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn folding_treats_case_and_normalization_as_one_name_and_exact_does_not() {
    assert!(NameEquivalence::Fold.same("README", "readme"));
    assert!(NameEquivalence::Fold.same("caf\u{e9}", "cafe\u{301}"));
    assert_eq!(
      NameEquivalence::Fold.hash("README"),
      NameEquivalence::Fold.hash("readme")
    );
    assert!(!NameEquivalence::Exact.same("README", "readme"));
    assert_ne!(
      NameEquivalence::Exact.hash("README"),
      NameEquivalence::Exact.hash("readme")
    );
    assert!(NameEquivalence::Exact.same("a", "a"));
    // The ASCII fast path and the Unicode path agree on ASCII text, and the hash is the FNV-1a
    // of the folded bytes on every path.
    assert_eq!(
      NameEquivalence::Fold.hash("Cargo.TOML"),
      NameEquivalence::Exact.hash("cargo.toml")
    );
    assert_eq!(
      NameEquivalence::Fold.hash("CAF\u{c9}"),
      NameEquivalence::Exact.hash("caf\u{e9}")
    );
    assert_eq!(NameEquivalence::Fold.fold("Stra\u{df}e"), "stra\u{df}e");
  }

  #[test]
  fn names_are_checked_for_shape() {
    assert!(check("ok.txt").is_ok());
    for bad in ["", ".", "..", "a/b", "nul\0"] {
      assert!(check(bad).is_err(), "{bad:?}");
    }
    assert!(check(&"x".repeat(256)).is_err());
    assert!(check(&"x".repeat(255)).is_ok());
  }

  use proptest::prelude::*;

  /// Names a generator draws for the comparison oracle: mostly ASCII with every case, sometimes non-ASCII (precomposed
  /// and combining forms), so both fast paths and the general one run.
  // proptest's `prop_oneof!` builds its union over `Arc` (D-8's harness exception; test code only).
  #[allow(clippy::disallowed_types)]
  fn name() -> impl Strategy<Value = String> {
    prop_oneof![
      "[a-zA-Z0-9._-]{0,12}",
      "[a-zA-Z\u{e9}\u{301}\u{c5}\u{3a3}\u{3c3}]{0,8}",
    ]
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: 4000, .. ProptestConfig::default() }))]

    /// The ordering oracle (2026-10-06, the comparison fast paths). Do: compare and equate generated names under both
    /// policies. Expect: `compare` equals the folded iterators' order and `same` their equality, every time — the fast
    /// paths change the cost, never the answer the tree's and the image's order rely on.
    #[test]
    fn compare_and_same_agree_with_the_folded_forms(a in name(), b in name()) {
      for policy in [NameEquivalence::Exact, NameEquivalence::Fold] {
        prop_assert_eq!(policy.compare(&a, &b), policy.folded(&a).cmp(policy.folded(&b)), "{:?} {:?} {:?}", policy, a, b);
        prop_assert_eq!(policy.same(&a, &b), policy.folded(&a).eq(policy.folded(&b)), "{:?} {:?} {:?}", policy, a, b);
      }
    }
  }
}

//! A text's MD5 and SHA-1, the two digests a dump's text headers carry, through aws-lc-rs (the workspace's one
//! cryptographic library; its MD5 is `vendor/UPSTREAM.md`'s addition, tested there against RFC 1321). Only the fallible
//! forms of its context are called (`try_new`, `try_update`, `try_finish`), so a failure is a refusal, never a panic.

use aws_lc_rs::digest::{Context, MD5_FOR_LEGACY_USE_ONLY, SHA1_FOR_LEGACY_USE_ONLY};

use super::{Digest, DumpError, Md5, Sha1};

/// A running MD5 and SHA-1 of one text: what a whole text's headers are checked against as it passes, and what an
/// applier checks a delta's result with.
#[derive(Clone)]
pub struct Digests {
  md5: Context,
  sha1: Context,
}

impl std::fmt::Debug for Digests {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("Digests")
  }
}

impl Digests {
  /// Digests of nothing yet.
  ///
  /// # Errors
  /// [`DumpError::DigestFailed`] when aws-lc-rs cannot make a context.
  pub fn new() -> Result<Self, DumpError> {
    Ok(Self {
      md5: Context::try_new(&MD5_FOR_LEGACY_USE_ONLY).map_err(|_| DumpError::DigestFailed)?,
      sha1: Context::try_new(&SHA1_FOR_LEGACY_USE_ONLY).map_err(|_| DumpError::DigestFailed)?,
    })
  }

  /// Takes the text's next bytes.
  ///
  /// # Errors
  /// [`DumpError::DigestFailed`] when aws-lc-rs refuses them (past its 2^64 - 1 bits).
  pub fn update(&mut self, bytes: &[u8]) -> Result<(), DumpError> {
    self
      .md5
      .try_update(bytes)
      .map_err(|_| DumpError::DigestFailed)?;
    self
      .sha1
      .try_update(bytes)
      .map_err(|_| DumpError::DigestFailed)
  }

  /// The text's MD5 and SHA-1.
  ///
  /// # Errors
  /// [`DumpError::DigestFailed`] when aws-lc-rs cannot finish a digest.
  pub fn finish(self) -> Result<(Md5, Sha1), DumpError> {
    let md5 = self.md5.try_finish().map_err(|_| DumpError::DigestFailed)?;
    let sha1 = self
      .sha1
      .try_finish()
      .map_err(|_| DumpError::DigestFailed)?;
    Ok((
      Md5::try_from(md5.as_ref()).map_err(|_| DumpError::DigestFailed)?,
      Sha1::try_from(sha1.as_ref()).map_err(|_| DumpError::DigestFailed)?,
    ))
  }
}

/// Refuses a text whose digests, `actual`, differ from the ones its headers name. A digest a header does not name, or
/// names as all zeros, checks nothing: `svn_checksum_parse_hex` reads an all-zero digest as none (`checksum.c`).
///
/// # Errors
/// [`DumpError::ChecksumMismatch`], the MD5's first, as Subversion checks the MD5.
pub fn check_text(
  expected_md5: Option<Md5>,
  expected_sha1: Option<Sha1>,
  actual: (Md5, Sha1),
) -> Result<(), DumpError> {
  let (md5, sha1) = actual;
  if let Some(expected) = expected_md5.filter(|digest| names_a_digest(digest))
    && expected != md5
  {
    return Err(DumpError::ChecksumMismatch {
      expected: Digest::Md5(expected),
      actual: Digest::Md5(md5),
    });
  }
  if let Some(expected) = expected_sha1.filter(|digest| names_a_digest(digest))
    && expected != sha1
  {
    return Err(DumpError::ChecksumMismatch {
      expected: Digest::Sha1(expected),
      actual: Digest::Sha1(sha1),
    });
  }
  Ok(())
}

/// Whether a digest names one: not all zeros (`svn_checksum_parse_hex` reads all zeros as none).
fn names_a_digest(digest: &[u8]) -> bool {
  digest.iter().any(|&byte| byte != 0)
}

/// Whether either expected digest names one, so a text must be hashed to be checked.
pub(super) fn checks_anything(expected_md5: Option<Md5>, expected_sha1: Option<Sha1>) -> bool {
  expected_md5.is_some_and(|digest| names_a_digest(&digest))
    || expected_sha1.is_some_and(|digest| names_a_digest(&digest))
}

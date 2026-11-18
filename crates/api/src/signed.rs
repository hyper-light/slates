//! Signed work (§4.12 "The consumer contract", A-299; Ada, 2026-11-18: "we can (and should) support signed work (signed
//! merges, etc.)"). A merge or a landing may carry a signature by its author's key over the work it names, and its
//! outcome says how the daemon judged it: verified (by which key), unsigned, or invalid (why). A volume's policy may
//! require signed merges ([`crate::WritePolicy::require_signed_merges`]).
//!
//! **Signatures never gate permission.** Whether a merge or a landing may write is the caller's write mode
//! ([`crate::WriteMode`]), set by the launcher or the harness; a signature only says who made the work. A volume that
//! requires signed merges refuses an unsigned or invalid one as a requirement on what it accepts
//! ([`crate::PolicyReason::SignatureRequired`]), never as a permission.
//!
//! **The format.** An SSH signature (OpenSSH `PROTOCOL.sshsig`, as `ssh-keygen -Y sign` makes it), the format the
//! daemon's verifier reads (`slates_server::grant_signature`, A-285): Ed25519 keys and FIDO Ed25519 keys, whose
//! signature must carry the authenticator's touch. The statement a signer signs names the work exactly (a merge's
//! increment, a landing's manifest); the daemon defines it, and the verbs that hand it out and verify a signed act are
//! the daemon's side (A-285's owner, agreed through g262's inbox). Until they land, an act that carries a signature is
//! refused `Unsupported(SignedWork)` before anything is sent, and every outcome reports [`Verification::Unsigned`],
//! which is the truth.

use std::fmt;

/// A signature a merge or a landing carries: the blob of an SSH signature (OpenSSH `PROTOCOL.sshsig`) by the signer's
/// key over the work's statement.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkSignature {
  /// The signature blob: the bytes between the armour lines of `ssh-keygen -Y sign`'s output, base64-decoded.
  blob: Vec<u8>,
}

impl WorkSignature {
  /// A signature from its blob (already dearmoured).
  pub fn from_blob(blob: Vec<u8>) -> WorkSignature {
    WorkSignature { blob }
  }

  /// A signature from `ssh-keygen -Y sign`'s armoured text (`-----BEGIN SSH SIGNATURE-----` ... `-----END SSH
  /// SIGNATURE-----`). Refused [`SignatureFault::Malformed`] without both armour lines or with bad base64 between them;
  /// the blob's own grammar is the verifier's to judge.
  pub fn from_armoured(text: &str) -> Result<WorkSignature, SignatureFault> {
    slates_server::grant_signature::dearmour(text)
      .map(WorkSignature::from_blob)
      .map_err(|_| SignatureFault::Malformed)
  }

  /// The signature blob.
  pub fn blob(&self) -> &[u8] {
    &self.blob
  }
}

impl fmt::Debug for WorkSignature {
  /// A signature is public, but its bytes say nothing to a reader of a log: its length stands for it.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "WorkSignature({} bytes)", self.blob.len())
  }
}

/// How the daemon judged the signature a merge or a landing carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verification {
  /// The signature verified over the work's statement; the key that made it.
  Verified {
    /// The signing key.
    signer: Signer,
  },
  /// The work carried no signature.
  Unsigned,
  /// The work carried a signature that did not verify; why.
  Invalid {
    /// Why it did not verify.
    reason: SignatureFault,
  },
}

/// The key that made a verified signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Signer {
  /// The key's type.
  pub key: SignerKey,
  /// The key's fingerprint: SHA-256 of its public key blob (`ssh-keygen -l -E sha256` prints it in base64).
  pub fingerprint: [u8; 32],
}

/// A signing key's type: the ones the daemon's verifier accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignerKey {
  /// An Ed25519 key (`ssh-ed25519`, RFC 8709).
  Ed25519,
  /// A FIDO security key's Ed25519 key (`sk-ssh-ed25519@openssh.com`), whose every signature carries the touch.
  SecurityKeyEd25519,
}

/// Why a signature did not verify: the daemon verifier's closed reasons (`slates_server::grant_signature`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignatureFault {
  /// The armour, the base64 or the blob's grammar is wrong, or a length runs past its bytes.
  Malformed,
  /// The signature was made under another namespace (for a git commit, a file), never for this work.
  Namespace,
  /// The key type, the signature type or the message hash is not one slates accepts.
  Unsupported,
  /// The signing key is not one the volume's owner registered.
  UnregisteredKey,
  /// The signature does not verify over the work's statement under its key.
  BadSignature,
  /// A security key signed without its holder's touch.
  NotTouched,
  /// The statement's expiry has passed.
  Expired,
}

impl fmt::Display for SignatureFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::Malformed => "the signature is malformed",
      Self::Namespace => "the signature was made for something else (another namespace)",
      Self::Unsupported => "the signature's key or hash is not one slates accepts",
      Self::UnregisteredKey => "the signing key is not registered",
      Self::BadSignature => "the signature does not verify over this work",
      Self::NotTouched => "the security key signed without a touch",
      Self::Expired => "the signed statement has expired",
    })
  }
}

impl From<slates_server::grant_signature::SignatureRefusal> for SignatureFault {
  /// The daemon verifier's refusal as the contract names it, one to one.
  fn from(refusal: slates_server::grant_signature::SignatureRefusal) -> SignatureFault {
    use slates_server::grant_signature::SignatureRefusal as Refusal;
    match refusal {
      Refusal::Malformed => Self::Malformed,
      Refusal::Namespace => Self::Namespace,
      Refusal::Unsupported => Self::Unsupported,
      Refusal::UnregisteredKey => Self::UnregisteredKey,
      Refusal::BadSignature => Self::BadSignature,
      Refusal::NotTouched => Self::NotTouched,
      Refusal::Expired => Self::Expired,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{SignatureFault, WorkSignature};

  /// A-299, signed work: do read a signature from `ssh-keygen -Y sign`'s armour, from text with no armour, and from
  /// armour around bad base64; expect the blob between the lines, then `Malformed` twice — a signature is taken only
  /// as the tool writes it.
  #[test]
  fn a_signature_is_read_from_its_armour_and_nothing_else() {
    let armoured = "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH\n-----END SSH SIGNATURE-----\n";
    assert_eq!(
      WorkSignature::from_armoured(armoured).map(|signature| signature.blob().to_vec()),
      Ok(b"SSHSIG".to_vec())
    );
    assert_eq!(
      WorkSignature::from_armoured("U1NIU0lH"),
      Err(SignatureFault::Malformed)
    );
    assert_eq!(
      WorkSignature::from_armoured(
        "-----BEGIN SSH SIGNATURE-----\n*not base64*\n-----END SSH SIGNATURE-----"
      ),
      Err(SignatureFault::Malformed)
    );
  }
}

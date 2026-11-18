//! Signed work (§4.12 "The consumer contract", A-299; A-321; Ada, 2026-11-18: "we can (and should) support signed work
//! (signed merges, etc.)"). A merge may carry a signature by its author's key over the merge's statement, and its
//! outcome says how the daemon judged it: verified (by which key), unsigned, or invalid (why). A volume's policy may
//! require signed merges ([`crate::WritePolicy::require_signed_merges`]), and its trusted signers are set by the launcher
//! ([`crate::Instance::set_signers`]). The verdict is recorded with the version, so a consumer reads it back
//! ([`crate::Instance::merge_record`]).
//!
//! **Signatures never gate permission.** Whether a merge or a landing may write is the caller's write mode
//! ([`crate::WriteMode`]), set by the launcher or the harness; a signature only says who made the work. A volume that
//! requires signed merges refuses an unsigned or invalid one as a requirement on what it accepts
//! ([`crate::PolicyReason::SignatureRequired`]), never as a permission.
//!
//! **The format.** An SSH signature (OpenSSH `PROTOCOL.sshsig`, as `ssh-keygen -Y sign -n slates-work` makes it), the
//! format the daemon's verifier reads (`slates_server::signed_work`): Ed25519 keys and FIDO Ed25519 keys, whose
//! signature must carry the authenticator's touch. A signer signs the merge's statement ([`MergeStatement`],
//! [`crate::Instance::merge_statement`]), which names the work, its green, its base and the increment's identity, so a
//! signature verifies for exactly the increment it was made over and never once the work changes.
//!
//! **Landings.** A landing's statement and its verification are the daemon's next piece (A-321, GAPS); until they land,
//! a landing that carries a signature is refused `Unsupported(SignedWork)` before anything is sent, and every landing's
//! outcome reports [`Verification::Unsigned`], which is the truth.

use std::fmt;

use slates_ipc::protocol as wire;

use crate::outcome::Work;

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
    slates_server::signed_work::dearmour(text)
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

/// What a signer signs to make a merge signed work: the merge's statement, as the daemon states it for the work's
/// increment now ([`crate::Instance::merge_statement`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeStatement {
  /// The work and its green.
  pub work: Work,
  /// The increment's identity the statement names: what the signature binds.
  pub identity: [u8; 32],
  /// The exact bytes to sign, under the namespace `slates-work` (`ssh-keygen -Y sign -n slates-work -f KEY FILE`):
  /// plain text a person can read, one field a line.
  pub statement: Vec<u8>,
}

/// A public key trusted to sign a volume's work ([`crate::Instance::set_signers`]): an Ed25519 or FIDO Ed25519 key, as
/// an `authorized_keys` line names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedSigner {
  /// The key in SSH wire form.
  blob: Vec<u8>,
  /// The signer a signature by the key is verified as.
  signer: Signer,
}

impl TrustedSigner {
  /// A key from one OpenSSH public key line (`TYPE BASE64 [COMMENT]`, as `ssh-keygen` writes `KEY.pub`). Refused
  /// [`SignatureFault::Malformed`] for a line that is not a key and [`SignatureFault::Unsupported`] for a key type the
  /// daemon does not verify.
  pub fn from_openssh(line: &str) -> Result<TrustedSigner, SignatureFault> {
    slates_server::signed_work::TrustedKey::from_openssh(line)
      .map(TrustedSigner::of_key)
      .map_err(SignatureFault::from)
  }

  /// A key from its SSH wire form, refused as [`TrustedSigner::from_openssh`] refuses.
  pub fn from_wire(blob: Vec<u8>) -> Result<TrustedSigner, SignatureFault> {
    slates_server::signed_work::TrustedKey::from_wire(blob)
      .map(TrustedSigner::of_key)
      .map_err(SignatureFault::from)
  }

  /// A key the verifier read.
  fn of_key(key: slates_server::signed_work::TrustedKey) -> TrustedSigner {
    TrustedSigner {
      signer: Signer::of_wire(key.signer()),
      blob: key.wire().to_vec(),
    }
  }

  /// The key in SSH wire form.
  pub fn wire(&self) -> &[u8] {
    &self.blob
  }

  /// The signer a signature by this key is verified as: its type and fingerprint.
  pub fn signer(&self) -> Signer {
    self.signer
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

impl Verification {
  /// The verdict as the wire carries it.
  pub(crate) fn of_wire(verification: wire::Verification) -> Verification {
    match verification {
      wire::Verification::Verified { signer } => Self::Verified {
        signer: Signer::of_wire(signer),
      },
      wire::Verification::Unsigned => Self::Unsigned,
      wire::Verification::Invalid { reason } => Self::Invalid {
        reason: SignatureFault::of_wire(reason),
      },
    }
  }
}

/// The key that made a verified signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Signer {
  /// The key's type.
  pub key: SignerKey,
  /// The key's fingerprint: SHA-256 of its public key blob (`ssh-keygen -l -E sha256` prints it in base64).
  pub fingerprint: [u8; 32],
}

impl Signer {
  /// The signer as the wire names it.
  pub(crate) fn of_wire(signer: wire::Signer) -> Signer {
    Signer {
      key: match signer.key {
        wire::SignerKey::Ed25519 => SignerKey::Ed25519,
        wire::SignerKey::SecurityKeyEd25519 => SignerKey::SecurityKeyEd25519,
      },
      fingerprint: signer.fingerprint,
    }
  }
}

/// A signing key's type: the ones the daemon's verifier accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignerKey {
  /// An Ed25519 key (`ssh-ed25519`, RFC 8709).
  Ed25519,
  /// A FIDO security key's Ed25519 key (`sk-ssh-ed25519@openssh.com`), whose every signature carries the touch.
  SecurityKeyEd25519,
}

/// Why a signature did not verify: the daemon verifier's closed reasons (`slates_server::signed_work`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignatureFault {
  /// The armour, the base64 or the blob's grammar is wrong, or a length runs past its bytes.
  Malformed,
  /// The signature was made under another namespace (for a git commit, a file), never for this work.
  Namespace,
  /// The key type, the signature type or the message hash is not one slates accepts.
  Unsupported,
  /// The signing key is not one of the volume's trusted signers.
  UnregisteredKey,
  /// The signature does not verify over the work's statement under its key.
  BadSignature,
  /// A security key signed without its holder's touch.
  NotTouched,
}

impl SignatureFault {
  /// The reason as the wire names it, one to one.
  pub(crate) fn of_wire(fault: wire::SignatureFault) -> SignatureFault {
    match fault {
      wire::SignatureFault::Malformed => Self::Malformed,
      wire::SignatureFault::Namespace => Self::Namespace,
      wire::SignatureFault::Unsupported => Self::Unsupported,
      wire::SignatureFault::UnregisteredKey => Self::UnregisteredKey,
      wire::SignatureFault::BadSignature => Self::BadSignature,
      wire::SignatureFault::NotTouched => Self::NotTouched,
    }
  }
}

impl fmt::Display for SignatureFault {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::Malformed => "the signature is malformed",
      Self::Namespace => "the signature was made for something else (another namespace)",
      Self::Unsupported => "the signature's key or hash is not one slates accepts",
      Self::UnregisteredKey => "the signing key is not one of the volume's trusted signers",
      Self::BadSignature => "the signature does not verify over this work",
      Self::NotTouched => "the security key signed without a touch",
    })
  }
}

impl From<slates_server::signed_work::SignatureRefusal> for SignatureFault {
  /// The daemon verifier's refusal as the contract names it, one to one.
  fn from(refusal: slates_server::signed_work::SignatureRefusal) -> SignatureFault {
    SignatureFault::of_wire(refusal.fault())
  }
}

#[cfg(test)]
mod tests {
  use super::{SignatureFault, SignerKey, TrustedSigner, WorkSignature};

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

  /// A-321: do read a trusted signer from an `ssh-keygen` public key line, from an RSA key's line and from a line that
  /// is not a key; expect the first read as an Ed25519 signer whose fingerprint is `ssh-keygen -l -E sha256`'s
  /// (`SHA256:fWZkGsfl3pAuiA/QPSOJ5Acit7RgzzYKLOMCxlO/D8Q`, OpenSSH 10.2p1, 2026-11-17), the RSA key `Unsupported` and
  /// the rest `Malformed`.
  #[test]
  fn a_trusted_signer_is_read_from_its_public_key_line() {
    let signer = TrustedSigner::from_openssh(
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGBhCTlwVQXKWkG3o39IqinnZCPCGAKgnb5+7b+P7WoV test",
    )
    .unwrap();
    assert_eq!(signer.signer().key, SignerKey::Ed25519);
    assert_eq!(
      signer.signer().fingerprint.to_vec(),
      slates_server::signed_work::base64_decode("fWZkGsfl3pAuiA/QPSOJ5Acit7RgzzYKLOMCxlO/D8Q=")
        .unwrap()
    );
    assert_eq!(
      TrustedSigner::from_wire(signer.wire().to_vec()),
      Ok(signer.clone())
    );
    assert_eq!(
      TrustedSigner::from_openssh("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAAQC7"),
      Err(SignatureFault::Unsupported)
    );
    assert_eq!(
      TrustedSigner::from_openssh("not a key"),
      Err(SignatureFault::Malformed)
    );
  }
}

//! Signed work (§4.12 "The consumer contract", §4.16 "Submission", §4.15; A-321, over A-285's verifier; Ada,
//! 2026-11-18: "we can (and should) support signed work (signed merges, etc.)"). A merge or a landing may carry an SSH
//! signature by its author's key over the act's **statement**; the daemon verifies it against the volume's trusted
//! signers, which only the launcher sets, records the result with the merge or the landing as provenance, and answers
//! it typed: verified (by which key), unsigned, or invalid (why). A signature never decides whether a write is
//! permitted — the write mode the user gave the agent does (R10, D-22, A-285). The one rule a signature feeds is a
//! volume's own requirement that its merges be signed (`WritePolicy::require_signed_merges`), a requirement on what
//! the volume accepts, set by the launcher as the write policy is.
//!
//! **The format.** OpenSSH's `PROTOCOL.sshsig` under the namespace [`NAMESPACE`] (`ssh-keygen -Y sign -n slates-work
//! -f KEY FILE`), by an `ssh-ed25519` key (RFC 8709) or a FIDO `sk-ssh-ed25519@openssh.com` key, whose signature must
//! carry the authenticator's user-presence flag, with the message hashed by `sha512` or `sha256`. A signature made for
//! anything else (a git commit signs under `git`, a file under `file`) names another namespace and never verifies as
//! signed work. The same verifier reads a git commit's SSH signature under git's own namespace ([`verify_in`],
//! [`GIT_NAMESPACE`]), against the keys its repository's policy trusts, where a protected branch requires signed
//! commits (A-644; §4.17.2 "Protected branches"; `crate::git_protect`).
//!
//! **What is signed: the statement.** Plain text, one field a line, so `ssh-keygen` signs it as a file and a person
//! can read what they sign. The first line names the act and the grammar's version, so a merge's statement is never a
//! landing's; with the namespace that is the domain separation.
//! - A merge ([`merge_statement`]): the work volume, its green, the base version and the increment's identity
//!   (`blake3` of the work, its base, its ops document and its post-state, §4.16), which binds every byte merged.
//! - A landing ([`landing_statement`]): the volume, the target directory as the request names it, prefixed by its
//!   length in bytes so no path can forge a later line, and the manifest hash (§4.15 step 2), which binds every entry
//!   written.
//!
//! A signer gets a merge's statement from a held submit (`SubmitHeld`, answered `MergeApprovalRequired` with the
//! increment's identity and the statement, nothing merged) and signs it. A statement names one act exactly, so it carries
//! no expiry: once the work changes, its signature no longer verifies.
//!
//! **Where it stands.** Merges are signed work: judged on the green's owner (`verbs::judge_signature`), the verdict
//! recorded in the version's chain entry and the green's accepted record, which a checkpoint keeps (`to_entry`,
//! `of_entry`), and read back by `MergeRecord`. Landings are signed work: judged on the volume's owner against the
//! manifest a presentation shows or a granted landing writes (`landing::landing_verdict`), the verdict recorded in the
//! landing's audit records (`to_record`, `of_record`) and read back by `Audit`. A presentation (`Present`,
//! `GrantRequired`) hands a signer the landing's statement.
//!
//! **Trusted signers.** A volume's signers are public keys the launcher sets (`SetSigners`, proven as a write policy
//! is, refused on an agent's own channel). A merge verifies under its work's signers and its green's together; a
//! landing under its volume's. A signature by any other key is `UnregisteredKey`, whatever its own validity.
//!
//! **Why here, with aws-lc-rs.** Ed25519 and the SHA-2 digests come from AWS-LC, slates' one cryptographic library
//! (A-66), which the server already links on every platform through rustls. The format code is pure. Its golden
//! vectors are real `ssh-keygen -Y sign` signatures over both statements (OpenSSH 10.2p1, 2026-11-17), each checked
//! with `ssh-keygen -Y verify`.
//!
//! **Refusals.** Every failure is a [`SignatureRefusal`]: malformed bytes, another namespace, an unsupported key or
//! hash, a key not trusted, a signature that does not verify, or a security key not touched. Each is a verdict on the
//! signature, recorded and answered, never a refusal of the act (except where a volume requires signed merges).
//!
//! **History.** The verifier was built for A-285's first reading, where a grant's proof was a signature by a key only
//! the human held (piece 1, 1c31d1c7). Ada withdrew that reading the same day; the verifier stayed for signed work, and
//! its grant statement, nonce and expiry went with A-321, replaced by the work statements above.

use aws_lc_rs::digest;
use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
use slates_ipc::protocol::{SignatureFault, Signer, SignerKey, Verification};

/// Format: the SSH signature namespace signed work is signed under (`ssh-keygen -Y sign -n slates-work`).
pub const NAMESPACE: &str = "slates-work";
/// Format: the SSH signature namespace git signs and verifies commits under (`ssh-keygen -Y sign -n git`;
/// gpg-interface.c's `sign_buffer_ssh` and `verify_ssh_signed_buffer`, git v2.50.1; A-644).
pub const GIT_NAMESPACE: &str = "git";
/// Format: a merge statement's first line, naming the act and its grammar's version.
const MERGE_HEADER: &str = "slates merge v1";
/// Format: a landing statement's first line, naming the act and its grammar's version.
const LANDING_HEADER: &str = "slates landing v1";
/// Format: the magic that opens an SSH signature blob and its signed data (`PROTOCOL.sshsig`).
const SSHSIG_MAGIC: &[u8] = b"SSHSIG";
/// Format: the only SSH signature blob version (`PROTOCOL.sshsig`).
const SSHSIG_VERSION: u32 = 1;
/// Format: the armour lines around an SSH signature's base64 (`PROTOCOL.sshsig`).
const ARMOUR_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
/// Format: see [`ARMOUR_BEGIN`].
const ARMOUR_END: &str = "-----END SSH SIGNATURE-----";
/// Format: the key type of an Ed25519 key (RFC 8709).
const KEY_ED25519: &str = "ssh-ed25519";
/// Format: the key type of a FIDO Ed25519 key (OpenSSH `PROTOCOL.u2f`).
const KEY_SK_ED25519: &str = "sk-ssh-ed25519@openssh.com";
/// Format: the bytes of an Ed25519 public key (RFC 8032).
const ED25519_PUBLIC_BYTES: usize = 32;
/// Format: the bytes of an Ed25519 signature (RFC 8032).
const ED25519_SIGNATURE_BYTES: usize = 64;
/// Format: the authenticator's user-presence flag, set when the human touched the key (CTAP2, `PROTOCOL.u2f`).
const SK_USER_PRESENT: u8 = 0x01;
/// Format: a key fingerprint's bytes: SHA-256 of the key's wire form, as `ssh-keygen -l -E sha256` prints it.
pub const FINGERPRINT_BYTES: usize = 32;

/// Why a signature did not verify. Each is a closed reason the daemon records and answers as
/// [`Verification::Invalid`]; none is ever a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureRefusal {
  /// The armour, the base64 or the blob's grammar is wrong, or a length runs past its bytes.
  Malformed,
  /// The signature was made under a namespace other than the one asked for: [`NAMESPACE`] for signed work,
  /// [`GIT_NAMESPACE`] for a git commit.
  Namespace,
  /// The key type, the signature type or the message hash is not one slates accepts.
  Unsupported,
  /// The signing key is not one of the volume's trusted signers.
  UnregisteredKey,
  /// The signature does not verify over this statement under its key.
  BadSignature,
  /// A security key signed without its holder's touch.
  NotTouched,
}

/// The counter a judged signature moves (`status` shows it): one for a verified signature, one per reason for an
/// invalid one; none for an act that carried no signature.
#[must_use]
pub fn counter(verification: &Verification) -> Option<&'static str> {
  match verification {
    Verification::Unsigned => None,
    Verification::Verified { .. } => Some("signed_work.verified"),
    Verification::Invalid { reason } => Some(match reason {
      SignatureFault::Malformed => "signed_work.invalid.malformed",
      SignatureFault::Namespace => "signed_work.invalid.namespace",
      SignatureFault::Unsupported => "signed_work.invalid.unsupported",
      SignatureFault::UnregisteredKey => "signed_work.invalid.unregistered_key",
      SignatureFault::BadSignature => "signed_work.invalid.bad_signature",
      SignatureFault::NotTouched => "signed_work.invalid.not_touched",
    }),
  }
}

impl SignatureRefusal {
  /// The reason as the wire names it, one to one.
  #[must_use]
  pub fn fault(self) -> SignatureFault {
    match self {
      SignatureRefusal::Malformed => SignatureFault::Malformed,
      SignatureRefusal::Namespace => SignatureFault::Namespace,
      SignatureRefusal::Unsupported => SignatureFault::Unsupported,
      SignatureRefusal::UnregisteredKey => SignatureFault::UnregisteredKey,
      SignatureRefusal::BadSignature => SignatureFault::BadSignature,
      SignatureRefusal::NotTouched => SignatureFault::NotTouched,
    }
  }
}

/// The exact bytes a merge's signature signs (module docs): the work, its green, the version the increment is based on
/// and the increment's identity, one field a line.
#[must_use]
pub fn merge_statement(
  work: &[u8; 16],
  green: &[u8; 16],
  base: u64,
  increment: &[u8; 32],
) -> Vec<u8> {
  format!(
    "{MERGE_HEADER}\nwork {}\ngreen {}\nbase {base}\nincrement {}\n",
    hex(work),
    hex(green),
    hex(increment),
  )
  .into_bytes()
}

/// The exact bytes a landing's signature signs (module docs): the volume, the target as the request names it (prefixed
/// by its length in bytes) and the manifest hash, one field a line.
#[must_use]
pub fn landing_statement(volume: &[u8; 16], target: &str, manifest: &[u8; 32]) -> Vec<u8> {
  format!(
    "{LANDING_HEADER}\nvolume {}\ntarget {}:{target}\nmanifest {}\n",
    hex(volume),
    target.len(),
    hex(manifest),
  )
  .into_bytes()
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
  use std::fmt::Write;
  bytes.iter().fold(
    String::with_capacity(bytes.len().saturating_mul(2)),
    |mut out, byte| {
      let _ = write!(out, "{byte:02x}");
      out
    },
  )
}

/// A public key trusted to sign a volume's work, in SSH wire form (the base64 field of an `authorized_keys` line,
/// decoded), as the launcher set it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedKey(Vec<u8>);

/// What kind of key a trusted key is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyKind {
  /// `ssh-ed25519`.
  Ed25519,
  /// `sk-ssh-ed25519@openssh.com`: a FIDO key, touched for each signature.
  SecurityKeyEd25519,
}

impl TrustedKey {
  /// Parses one OpenSSH public key line (`TYPE BASE64 [COMMENT]`), accepting only the key types slates verifies.
  ///
  /// # Errors
  /// [`SignatureRefusal::Malformed`] for a line that is not a key, [`SignatureRefusal::Unsupported`] for another type.
  pub fn from_openssh(line: &str) -> Result<TrustedKey, SignatureRefusal> {
    let mut words = line.split_whitespace();
    let kind = words.next().ok_or(SignatureRefusal::Malformed)?;
    let blob = base64_decode(words.next().ok_or(SignatureRefusal::Malformed)?)?;
    let key = SshKey::parse(&blob)?;
    if key.kind_name() != kind {
      return Err(SignatureRefusal::Malformed);
    }
    Ok(TrustedKey(blob))
  }

  /// A key in SSH wire form, accepting only the key types slates verifies.
  ///
  /// # Errors
  /// [`SignatureRefusal::Malformed`] or [`SignatureRefusal::Unsupported`].
  pub fn from_wire(blob: Vec<u8>) -> Result<TrustedKey, SignatureRefusal> {
    SshKey::parse(&blob)?;
    Ok(TrustedKey(blob))
  }

  /// The key's SSH wire form.
  #[must_use]
  pub fn wire(&self) -> &[u8] {
    &self.0
  }

  /// The key's kind (a trusted key is parsed when it is made, so it is always one slates verifies).
  #[must_use]
  pub fn kind(&self) -> KeyKind {
    match SshKey::parse(&self.0) {
      Ok(SshKey::SkEd25519(..)) => KeyKind::SecurityKeyEd25519,
      Ok(SshKey::Ed25519(_)) | Err(_) => KeyKind::Ed25519,
    }
  }

  /// The key's SHA-256 fingerprint over its wire form, as `ssh-keygen -l -E sha256` computes it (the base64 of these
  /// bytes is what it prints after `SHA256:`).
  #[must_use]
  pub fn fingerprint(&self) -> [u8; FINGERPRINT_BYTES] {
    let mut out = [0u8; FINGERPRINT_BYTES];
    out.copy_from_slice(digest::digest(&digest::SHA256, &self.0).as_ref());
    out
  }

  /// The signer this key names on the wire.
  #[must_use]
  pub fn signer(&self) -> Signer {
    Signer {
      key: match self.kind() {
        KeyKind::Ed25519 => SignerKey::Ed25519,
        KeyKind::SecurityKeyEd25519 => SignerKey::SecurityKeyEd25519,
      },
      fingerprint: self.fingerprint(),
    }
  }
}

/// A key a signature may be made with, parsed from its wire form.
enum SshKey<'a> {
  /// An Ed25519 key.
  Ed25519([u8; ED25519_PUBLIC_BYTES]),
  /// A FIDO Ed25519 key and its relying-party application string.
  SkEd25519([u8; ED25519_PUBLIC_BYTES], &'a [u8]),
}

impl<'a> SshKey<'a> {
  /// Parses a public key blob, the whole of it.
  fn parse(blob: &'a [u8]) -> Result<SshKey<'a>, SignatureRefusal> {
    let mut reader = Reader(blob);
    let kind = reader.string()?;
    let key = if kind == KEY_ED25519.as_bytes() {
      SshKey::Ed25519(reader.fixed()?)
    } else if kind == KEY_SK_ED25519.as_bytes() {
      let public = reader.fixed()?;
      SshKey::SkEd25519(public, reader.string()?)
    } else {
      return Err(SignatureRefusal::Unsupported);
    };
    reader.end()?;
    Ok(key)
  }

  /// The key's type name.
  fn kind_name(&self) -> &'static str {
    match self {
      SshKey::Ed25519(_) => KEY_ED25519,
      SshKey::SkEd25519(..) => KEY_SK_ED25519,
    }
  }
}

/// Reads SSH wire fields (RFC 4251 §5) from a byte slice, refusing any length past its end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
  /// The next `count` bytes.
  fn take(&mut self, count: usize) -> Result<&'a [u8], SignatureRefusal> {
    let (head, rest) = self
      .0
      .split_at_checked(count)
      .ok_or(SignatureRefusal::Malformed)?;
    self.0 = rest;
    Ok(head)
  }

  /// A big-endian `uint32`.
  fn u32(&mut self) -> Result<u32, SignatureRefusal> {
    let bytes: [u8; 4] = self
      .take(std::mem::size_of::<u32>())?
      .try_into()
      .map_err(|_| SignatureRefusal::Malformed)?;
    Ok(u32::from_be_bytes(bytes))
  }

  /// A `string`: a `uint32` length and that many bytes.
  fn string(&mut self) -> Result<&'a [u8], SignatureRefusal> {
    let length = usize::try_from(self.u32()?).map_err(|_| SignatureRefusal::Malformed)?;
    self.take(length)
  }

  /// A `string` of exactly `N` bytes.
  fn fixed<const N: usize>(&mut self) -> Result<[u8; N], SignatureRefusal> {
    self
      .string()?
      .try_into()
      .map_err(|_| SignatureRefusal::Malformed)
  }

  /// A single byte.
  fn byte(&mut self) -> Result<u8, SignatureRefusal> {
    self
      .take(1)?
      .first()
      .copied()
      .ok_or(SignatureRefusal::Malformed)
  }

  /// Refuses trailing bytes.
  fn end(&self) -> Result<(), SignatureRefusal> {
    if self.0.is_empty() {
      Ok(())
    } else {
      Err(SignatureRefusal::Malformed)
    }
  }
}

/// Appends an SSH `string` (a big-endian `uint32` length, then the bytes).
fn put_string(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SignatureRefusal> {
  let length = u32::try_from(bytes.len()).map_err(|_| SignatureRefusal::Malformed)?;
  out.extend_from_slice(&length.to_be_bytes());
  out.extend_from_slice(bytes);
  Ok(())
}

/// Format: the base64 alphabet (RFC 4648 §4, Table 1), each character standing for its index's six bits.
const BASE64_ALPHABET: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// Format: the characters of one base64 group (RFC 4648 §4: 24 bits as four characters of six bits each).
const BASE64_GROUP_SYMBOLS: usize = 4;
/// Format: the bytes one base64 group decodes to (24 bits).
const BASE64_GROUP_BYTES: usize = 3;
/// Format: the bits one base64 character stands for.
const BASE64_SYMBOL_BITS: u32 = 6;

/// Decodes standard base64 (RFC 4648 §4) with padding, ignoring whitespace (the armour wraps lines).
///
/// # Errors
/// [`SignatureRefusal::Malformed`] for any other character, a bad length or misplaced padding.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, SignatureRefusal> {
  /// The six bits a character stands for: its index in `BASE64_ALPHABET`.
  fn sextet(character: u8) -> Option<u32> {
    BASE64_ALPHABET
      .iter()
      .position(|symbol| *symbol == character)
      .and_then(|index| u32::try_from(index).ok())
  }
  let symbols: Vec<u8> = text
    .bytes()
    .filter(|byte| !byte.is_ascii_whitespace())
    .collect();
  if !symbols.len().is_multiple_of(BASE64_GROUP_SYMBOLS) {
    return Err(SignatureRefusal::Malformed);
  }
  let (groups, _) = symbols.as_chunks::<BASE64_GROUP_SYMBOLS>();
  let mut out = Vec::with_capacity(groups.len().saturating_mul(BASE64_GROUP_BYTES));
  let last = groups.len().saturating_sub(1);
  for (index, group) in groups.iter().enumerate() {
    let padding = group.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 || (padding > 0 && index != last) {
      return Err(SignatureRefusal::Malformed);
    }
    let mut word = 0u32;
    for (position, symbol) in group.iter().enumerate() {
      let value = if position.saturating_add(padding) >= BASE64_GROUP_SYMBOLS {
        0
      } else {
        sextet(*symbol).ok_or(SignatureRefusal::Malformed)?
      };
      word = word.wrapping_shl(BASE64_SYMBOL_BITS) | value;
    }
    out.extend(
      word
        .to_be_bytes()
        .iter()
        .skip(1)
        .take(BASE64_GROUP_BYTES.saturating_sub(padding)),
    );
  }
  Ok(out)
}

/// The signature blob inside an armoured SSH signature (`ssh-keygen -Y sign`'s output).
///
/// # Errors
/// [`SignatureRefusal::Malformed`] without both armour lines or with bad base64 between them.
pub fn dearmour(text: &str) -> Result<Vec<u8>, SignatureRefusal> {
  let inner = text
    .trim()
    .strip_prefix(ARMOUR_BEGIN)
    .and_then(|rest| rest.strip_suffix(ARMOUR_END))
    .ok_or(SignatureRefusal::Malformed)?;
  base64_decode(inner)
}

/// Verifies `signature` (an SSH signature blob, [`dearmour`]ed) over `statement`: made under [`NAMESPACE`], by one of
/// `trusted`, valid under that key, and touched if made by a security key. Answers the trusted key that signed.
///
/// # Errors
/// The [`SignatureRefusal`] that applies.
pub fn verify<'k>(
  signature: &[u8],
  statement: &[u8],
  trusted: &'k [TrustedKey],
) -> Result<&'k TrustedKey, SignatureRefusal> {
  verify_in(NAMESPACE, signature, statement, trusted)
}

/// Verifies `signature` over `statement` as [`verify`] does, made under `namespace`: [`GIT_NAMESPACE`] for a git commit,
/// whose statement is the commit without its signature headers (A-644).
///
/// # Errors
/// The [`SignatureRefusal`] that applies.
pub fn verify_in<'k>(
  namespace: &str,
  signature: &[u8],
  statement: &[u8],
  trusted: &'k [TrustedKey],
) -> Result<&'k TrustedKey, SignatureRefusal> {
  let mut reader = Reader(signature);
  if reader.take(SSHSIG_MAGIC.len())? != SSHSIG_MAGIC || reader.u32()? != SSHSIG_VERSION {
    return Err(SignatureRefusal::Malformed);
  }
  let public_blob = reader.string()?;
  let signed_namespace = reader.string()?;
  let reserved = reader.string()?;
  let hash_name = reader.string()?;
  let signature_blob = reader.string()?;
  reader.end()?;
  if signed_namespace != namespace.as_bytes() {
    return Err(SignatureRefusal::Namespace);
  }
  let key = SshKey::parse(public_blob)?;
  let algorithm = match hash_name {
    b"sha512" => &digest::SHA512,
    b"sha256" => &digest::SHA256,
    _ => return Err(SignatureRefusal::Unsupported),
  };
  let signer = trusted
    .iter()
    .find(|candidate| candidate.0 == public_blob)
    .ok_or(SignatureRefusal::UnregisteredKey)?;
  let mut signed = SSHSIG_MAGIC.to_vec();
  put_string(&mut signed, namespace.as_bytes())?;
  put_string(&mut signed, reserved)?;
  put_string(&mut signed, hash_name)?;
  put_string(&mut signed, digest::digest(algorithm, statement).as_ref())?;
  verify_under(&key, signature_blob, &signed)?;
  Ok(signer)
}

/// How the daemon judges an act's signature (module docs): [`Verification::Unsigned`] with none, else verified over
/// `statement` under `trusted`, the signer named, or invalid with its reason.
#[must_use]
pub fn judge(signature: Option<&[u8]>, statement: &[u8], trusted: &[TrustedKey]) -> Verification {
  match signature {
    None => Verification::Unsigned,
    Some(signature) => match verify(signature, statement, trusted) {
      Ok(key) => Verification::Verified {
        signer: key.signer(),
      },
      Err(refusal) => Verification::Invalid {
        reason: refusal.fault(),
      },
    },
  }
}

/// The trusted keys of `stored`, each as the launcher set it (`SetSigners` keeps only keys [`TrustedKey::from_wire`]
/// accepts, so none is dropped here but a record's corruption).
#[must_use]
pub fn trusted_of<'a>(stored: impl IntoIterator<Item = &'a Vec<u8>>) -> Vec<TrustedKey> {
  stored
    .into_iter()
    .filter_map(|blob| TrustedKey::from_wire(blob.clone()).ok())
    .collect()
}

/// Format: a recorded verdict's kind for a verified signature (`slates_merge::chain_entry::Signature::kind`).
const VERDICT_VERIFIED: u8 = 1;
/// Format: a recorded verdict's kind for an invalid signature.
const VERDICT_INVALID: u8 = 2;
/// Format: a verified verdict's detail is the signer's key type's place here, counted from 1.
const KEY_CODES: [SignerKey; 2] = [SignerKey::Ed25519, SignerKey::SecurityKeyEd25519];
/// Format: an invalid verdict's detail is its reason's place here, counted from 1.
const FAULT_CODES: [SignatureFault; 6] = [
  SignatureFault::Malformed,
  SignatureFault::Namespace,
  SignatureFault::Unsupported,
  SignatureFault::UnregisteredKey,
  SignatureFault::BadSignature,
  SignatureFault::NotTouched,
];

/// The detail of `item`: its place in `codes`, counted from 1 (every variant is in its table, so none is 0).
fn code_of<T: PartialEq>(codes: &[T], item: &T) -> u8 {
  codes
    .iter()
    .position(|code| code == item)
    .and_then(|place| u8::try_from(place.saturating_add(1)).ok())
    .unwrap_or(0)
}

/// The item a detail names in `codes`, if any.
fn item_of<T: Copy>(codes: &[T], code: u8) -> Option<T> {
  codes.get(usize::from(code).checked_sub(1)?).copied()
}

/// `verification` as a merge's chain entry records it (the version's provenance): `None` for an unsigned submit, which
/// records nothing.
#[must_use]
pub fn to_entry(verification: &Verification) -> Option<slates_merge::chain_entry::Signature> {
  match verification {
    Verification::Unsigned => None,
    Verification::Verified { signer } => Some(slates_merge::chain_entry::Signature {
      kind: VERDICT_VERIFIED,
      detail: code_of(&KEY_CODES, &signer.key),
      fingerprint: signer.fingerprint,
    }),
    Verification::Invalid { reason } => Some(slates_merge::chain_entry::Signature {
      kind: VERDICT_INVALID,
      detail: code_of(&FAULT_CODES, reason),
      fingerprint: [0u8; FINGERPRINT_BYTES],
    }),
  }
}

/// `verification` as a landing's audit record keeps it (A-321): `None` for a landing that carried no signature.
#[must_use]
pub fn to_record(verification: &Verification) -> Option<slates_db::catalog::SignatureVerdict> {
  to_entry(verification).map(shipped)
}

/// A chain entry's verdict in the form a record keeps and a merge record carries to holders and a takeover successor
/// (A-321): the same three fields, in a type the wire encodes.
#[must_use]
pub fn shipped(
  entry: slates_merge::chain_entry::Signature,
) -> slates_db::catalog::SignatureVerdict {
  slates_db::catalog::SignatureVerdict {
    kind: entry.kind,
    detail: entry.detail,
    fingerprint: entry.fingerprint,
  }
}

/// A shipped verdict as a chain entry records it (A-321): what a takeover successor's chain entry keeps for the version.
#[must_use]
pub fn adopted(
  record: slates_db::catalog::SignatureVerdict,
) -> slates_merge::chain_entry::Signature {
  slates_merge::chain_entry::Signature {
    kind: record.kind,
    detail: record.detail,
    fingerprint: record.fingerprint,
  }
}

/// A record's verdict, read back as the wire names it: `Unsigned` for a record that keeps none, `None` for a verdict no
/// code of this build names.
#[must_use]
pub fn of_record(recorded: Option<slates_db::catalog::SignatureVerdict>) -> Option<Verification> {
  of_entry(recorded.map(adopted))
}

/// A chain entry's recorded verdict, read back as the wire names it: `Unsigned` for an entry that records none, `None`
/// for a verdict no code of this build names (a record's corruption, which a caller treats as any malformed record).
#[must_use]
pub fn of_entry(recorded: Option<slates_merge::chain_entry::Signature>) -> Option<Verification> {
  let Some(recorded) = recorded else {
    return Some(Verification::Unsigned);
  };
  match recorded.kind {
    VERDICT_VERIFIED => Some(Verification::Verified {
      signer: Signer {
        key: item_of(&KEY_CODES, recorded.detail)?,
        fingerprint: recorded.fingerprint,
      },
    }),
    VERDICT_INVALID => Some(Verification::Invalid {
      reason: item_of(&FAULT_CODES, recorded.detail)?,
    }),
    _ => None,
  }
}

/// Verifies the inner signature (type, bytes and, for a security key, its flags and counter) over `signed`.
fn verify_under(
  key: &SshKey<'_>,
  signature_blob: &[u8],
  signed: &[u8],
) -> Result<(), SignatureRefusal> {
  let mut reader = Reader(signature_blob);
  let kind = reader.string()?;
  if kind != key.kind_name().as_bytes() {
    return Err(SignatureRefusal::Unsupported);
  }
  let raw: [u8; ED25519_SIGNATURE_BYTES] = reader.fixed()?;
  match key {
    SshKey::Ed25519(public) => {
      reader.end()?;
      ed25519_verify(public, signed, &raw)
    }
    SshKey::SkEd25519(public, application) => {
      let flags = reader.byte()?;
      let counter = reader.u32()?;
      reader.end()?;
      // PROTOCOL.u2f: the authenticator signs SHA-256(application) ‖ flags ‖ counter ‖ SHA-256(message).
      let mut assertion = digest::digest(&digest::SHA256, application)
        .as_ref()
        .to_vec();
      assertion.push(flags);
      assertion.extend_from_slice(&counter.to_be_bytes());
      assertion.extend_from_slice(digest::digest(&digest::SHA256, signed).as_ref());
      ed25519_verify(public, &assertion, &raw)?;
      if flags & SK_USER_PRESENT == 0 {
        return Err(SignatureRefusal::NotTouched);
      }
      Ok(())
    }
  }
}

/// Ed25519 verification (RFC 8032) through AWS-LC.
fn ed25519_verify(
  public: &[u8; ED25519_PUBLIC_BYTES],
  message: &[u8],
  signature: &[u8; ED25519_SIGNATURE_BYTES],
) -> Result<(), SignatureRefusal> {
  UnparsedPublicKey::new(&ED25519, public)
    .verify(message, signature)
    .map_err(|_| SignatureRefusal::BadSignature)
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

  use super::*;
  use aws_lc_rs::rand::SystemRandom;
  use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};

  /// A fresh software Ed25519 key.
  fn fresh_pair() -> Ed25519KeyPair {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap()
  }

  /// Golden: the merge statement `ssh-keygen -Y sign -f k -n slates-work` signed (OpenSSH 10.2p1, 2026-11-17).
  fn golden_merge() -> Vec<u8> {
    merge_statement(&[1; 16], &[2; 16], 7, &[3; 32])
  }

  /// Golden: the landing statement signed with the same key.
  fn golden_landing() -> Vec<u8> {
    landing_statement(&[4; 16], "/srv/project", &[5; 32])
  }

  /// Golden: the signing key's public line.
  const GOLDEN_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGBhCTlwVQXKWkG3o39IqinnZCPCGAKgnb5+7b+P7WoV test";

  /// Golden: `ssh-keygen -Y sign`'s output over [`golden_merge`]; `ssh-keygen -Y verify` accepts it.
  const GOLDEN_MERGE_SIGNATURE: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgYGEJOXBVBcpaQbejf0iqKedkI8
IYAqCdvn7tv4/tahUAAAALc2xhdGVzLXdvcmsAAAAAAAAABnNoYTUxMgAAAFMAAAALc3No
LWVkMjU1MTkAAABATejGEmfQXWFTl8dsCJhf+6mcHo4AWL44yJmTTeHKswt2tfQS1i/U9x
bfc99YEicFlwYkHE5AbVrrmXSSvgS0Dw==
-----END SSH SIGNATURE-----
";

  /// Golden: `ssh-keygen -Y sign`'s output over [`golden_landing`]; `ssh-keygen -Y verify` accepts it.
  const GOLDEN_LANDING_SIGNATURE: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgYGEJOXBVBcpaQbejf0iqKedkI8
IYAqCdvn7tv4/tahUAAAALc2xhdGVzLXdvcmsAAAAAAAAABnNoYTUxMgAAAFMAAAALc3No
LWVkMjU1MTkAAABAjUjHEFHLSetTvn70zQqlU692R6ly8o+QF2yvh4LZaAERHq+d/zrU4E
aOwABu4p/IYs9aMJUL1VbGJz7+npTkCw==
-----END SSH SIGNATURE-----
";

  /// A second key, not the signer.
  const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICtXkrWNoJHnIwVcdhhZUXwmDFji4IaL6/V47WRqgprO other";

  fn golden_keys() -> Vec<TrustedKey> {
    vec![TrustedKey::from_openssh(GOLDEN_KEY).unwrap()]
  }

  /// A-321, golden: do encode both statements; expect exactly the bytes `ssh-keygen` signed, one field a line, the
  /// target prefixed by its length.
  #[test]
  fn the_statements_encode_one_field_a_line() {
    assert_eq!(
      String::from_utf8(golden_merge()).unwrap(),
      "slates merge v1\nwork 01010101010101010101010101010101\ngreen 02020202020202020202020202020202\nbase 7\n\
       increment 0303030303030303030303030303030303030303030303030303030303030303\n"
    );
    assert_eq!(
      String::from_utf8(golden_landing()).unwrap(),
      "slates landing v1\nvolume 04040404040404040404040404040404\ntarget 12:/srv/project\n\
       manifest 0505050505050505050505050505050505050505050505050505050505050505\n"
    );
  }

  /// A-321, golden: do verify real `ssh-keygen -Y sign` signatures over a merge's and a landing's statement by a trusted
  /// key; expect each verified, naming that key; and the merge's signature presented as the landing's (or the other way)
  /// refused `BadSignature`: a statement names one act.
  #[test]
  fn a_trusted_keys_ssh_signature_over_each_statement_verifies() {
    let keys = golden_keys();
    let merge = dearmour(GOLDEN_MERGE_SIGNATURE).unwrap();
    let landing = dearmour(GOLDEN_LANDING_SIGNATURE).unwrap();
    assert_eq!(verify(&merge, &golden_merge(), &keys), Ok(&keys[0]));
    assert_eq!(verify(&landing, &golden_landing(), &keys), Ok(&keys[0]));
    assert_eq!(
      verify(&merge, &golden_landing(), &keys),
      Err(SignatureRefusal::BadSignature)
    );
    assert_eq!(
      verify(&landing, &golden_merge(), &keys),
      Err(SignatureRefusal::BadSignature)
    );
  }

  /// A-321, golden: do read the golden key's fingerprint and kind; expect `ssh-keygen -l -E sha256`'s
  /// `SHA256:fWZkGsfl3pAuiA/QPSOJ5Acit7RgzzYKLOMCxlO/D8Q` (OpenSSH 10.2p1, 2026-11-17) and Ed25519, and the judged
  /// verdict naming that signer.
  #[test]
  fn a_keys_fingerprint_and_kind_are_ssh_keygens() {
    let keys = golden_keys();
    let fingerprint = base64_decode("fWZkGsfl3pAuiA/QPSOJ5Acit7RgzzYKLOMCxlO/D8Q=").unwrap();
    assert_eq!(keys[0].fingerprint().to_vec(), fingerprint);
    assert_eq!(keys[0].kind(), KeyKind::Ed25519);
    let merge = dearmour(GOLDEN_MERGE_SIGNATURE).unwrap();
    assert_eq!(
      judge(Some(&merge), &golden_merge(), &keys),
      Verification::Verified {
        signer: Signer {
          key: SignerKey::Ed25519,
          fingerprint: fingerprint.try_into().unwrap(),
        }
      }
    );
    assert_eq!(judge(None, &golden_merge(), &keys), Verification::Unsigned);
  }

  /// A-321: do flip every bit of the golden signature in turn; expect each refused and none verified, and a flip in the
  /// raw signature refused `BadSignature`.
  #[test]
  fn a_forged_signature_is_refused() {
    let keys = golden_keys();
    let blob = dearmour(GOLDEN_MERGE_SIGNATURE).unwrap();
    let statement = golden_merge();
    let flips = (0..blob.len()).flat_map(|index| (0..8).map(move |bit| (index, bit)));
    for (index, bit) in flips {
      let mut forged = blob.clone();
      forged[index] ^= 1 << bit;
      assert!(
        verify(&forged, &statement, &keys).is_err(),
        "byte {index} bit {bit} flipped still verified"
      );
    }
    let raw_signature_start = blob.len() - ED25519_SIGNATURE_BYTES;
    let mut forged = blob.clone();
    forged[raw_signature_start] ^= 1;
    assert_eq!(
      verify(&forged, &statement, &keys),
      Err(SignatureRefusal::BadSignature)
    );
  }

  /// A-321: do verify the golden signature under another key alone, and under no key; expect `UnregisteredKey` both
  /// times, though the signature is valid under its own key.
  #[test]
  fn an_untrusted_key_is_refused() {
    let others = vec![TrustedKey::from_openssh(OTHER_KEY).unwrap()];
    let blob = dearmour(GOLDEN_MERGE_SIGNATURE).unwrap();
    assert_eq!(
      verify(&blob, &golden_merge(), &others),
      Err(SignatureRefusal::UnregisteredKey)
    );
    assert_eq!(
      verify(&blob, &golden_merge(), &[]),
      Err(SignatureRefusal::UnregisteredKey)
    );
  }

  /// A-321: do change each field of the merge statement and of the landing statement in turn; expect each refused
  /// `BadSignature`: the signature binds the whole statement.
  #[test]
  fn a_changed_field_of_either_statement_is_refused() {
    let keys = golden_keys();
    let merge = dearmour(GOLDEN_MERGE_SIGNATURE).unwrap();
    let merges = [
      merge_statement(&[9; 16], &[2; 16], 7, &[3; 32]),
      merge_statement(&[1; 16], &[9; 16], 7, &[3; 32]),
      merge_statement(&[1; 16], &[2; 16], 8, &[3; 32]),
      merge_statement(&[1; 16], &[2; 16], 7, &[9; 32]),
    ];
    for (index, statement) in merges.iter().enumerate() {
      assert_eq!(
        verify(&merge, statement, &keys),
        Err(SignatureRefusal::BadSignature),
        "merge field {index}"
      );
    }
    let landing = dearmour(GOLDEN_LANDING_SIGNATURE).unwrap();
    let landings = [
      landing_statement(&[9; 16], "/srv/project", &[5; 32]),
      landing_statement(&[4; 16], "/srv/project2", &[5; 32]),
      landing_statement(&[4; 16], "/srv/project", &[9; 32]),
    ];
    for (index, statement) in landings.iter().enumerate() {
      assert_eq!(
        verify(&landing, statement, &keys),
        Err(SignatureRefusal::BadSignature),
        "landing field {index}"
      );
    }
  }

  /// Builds an SSH signature blob as `ssh-keygen` would, signed by `sign` over the signed data.
  fn sshsig(
    public_blob: &[u8],
    namespace: &str,
    statement: &[u8],
    sign: impl Fn(&[u8]) -> Vec<u8>,
  ) -> Vec<u8> {
    let mut signed = SSHSIG_MAGIC.to_vec();
    put_string(&mut signed, namespace.as_bytes()).unwrap();
    put_string(&mut signed, b"").unwrap();
    put_string(&mut signed, b"sha512").unwrap();
    put_string(
      &mut signed,
      digest::digest(&digest::SHA512, statement).as_ref(),
    )
    .unwrap();
    let mut blob = SSHSIG_MAGIC.to_vec();
    blob.extend_from_slice(&SSHSIG_VERSION.to_be_bytes());
    put_string(&mut blob, public_blob).unwrap();
    put_string(&mut blob, namespace.as_bytes()).unwrap();
    put_string(&mut blob, b"").unwrap();
    put_string(&mut blob, b"sha512").unwrap();
    put_string(&mut blob, &sign(&signed)).unwrap();
    blob
  }

  /// A-321: do sign a statement under `slates-work` and under `git` and `slates-grant` (A-285's withdrawn grant
  /// namespace) with one trusted key; expect the first verified and the others refused `Namespace`, though each is a
  /// valid signature.
  #[test]
  fn a_signature_under_another_namespace_is_refused() {
    let pair = fresh_pair();
    let mut public_blob = Vec::new();
    put_string(&mut public_blob, KEY_ED25519.as_bytes()).unwrap();
    put_string(&mut public_blob, pair.public_key().as_ref()).unwrap();
    let keys = vec![TrustedKey::from_wire(public_blob.clone()).unwrap()];
    let sign = |signed: &[u8]| {
      let mut inner = Vec::new();
      put_string(&mut inner, KEY_ED25519.as_bytes()).unwrap();
      put_string(&mut inner, pair.sign(signed).as_ref()).unwrap();
      inner
    };
    let statement = golden_merge();
    let good = sshsig(&public_blob, NAMESPACE, &statement, sign);
    assert!(verify(&good, &statement, &keys).is_ok());
    for namespace in ["git", "slates-grant"] {
      let other = sshsig(&public_blob, namespace, &statement, sign);
      assert_eq!(
        verify(&other, &statement, &keys),
        Err(SignatureRefusal::Namespace),
        "{namespace}"
      );
    }
  }

  /// A-321: a FIDO key's signature verifies when the human touched it and is refused `NotTouched` when the
  /// authenticator reports no user presence. Built to `PROTOCOL.u2f` with a software key standing in for the
  /// authenticator; a real security key's signature is the end-to-end proof's (GAPS). Its kind is read as a security
  /// key's.
  #[test]
  fn a_security_keys_signature_needs_the_touch() {
    let pair = fresh_pair();
    let application = b"ssh:";
    let mut public_blob = Vec::new();
    put_string(&mut public_blob, KEY_SK_ED25519.as_bytes()).unwrap();
    put_string(&mut public_blob, pair.public_key().as_ref()).unwrap();
    put_string(&mut public_blob, application).unwrap();
    let keys = vec![TrustedKey::from_wire(public_blob.clone()).unwrap()];
    assert_eq!(keys[0].kind(), KeyKind::SecurityKeyEd25519);
    let statement = golden_landing();
    let signed_with = |flags: u8| {
      let pair = &pair;
      move |signed: &[u8]| {
        let counter = 9u32;
        let mut assertion = digest::digest(&digest::SHA256, application)
          .as_ref()
          .to_vec();
        assertion.push(flags);
        assertion.extend_from_slice(&counter.to_be_bytes());
        assertion.extend_from_slice(digest::digest(&digest::SHA256, signed).as_ref());
        let mut inner = Vec::new();
        put_string(&mut inner, KEY_SK_ED25519.as_bytes()).unwrap();
        put_string(&mut inner, pair.sign(&assertion).as_ref()).unwrap();
        inner.push(flags);
        inner.extend_from_slice(&counter.to_be_bytes());
        inner
      }
    };
    let touched = sshsig(
      &public_blob,
      NAMESPACE,
      &statement,
      signed_with(SK_USER_PRESENT),
    );
    assert!(verify(&touched, &statement, &keys).is_ok());
    let untouched = sshsig(&public_blob, NAMESPACE, &statement, signed_with(0));
    assert_eq!(
      verify(&untouched, &statement, &keys),
      Err(SignatureRefusal::NotTouched)
    );
  }

  /// A-321, hostile signature bytes: do verify the golden signature truncated at every length, with a length of
  /// `u32::MAX`, under foreign magic and with a trailing byte; expect each refused, never a panic.
  #[test]
  fn hostile_signature_bytes_are_refused() {
    let keys = golden_keys();
    let statement = golden_merge();
    let blob = dearmour(GOLDEN_MERGE_SIGNATURE).unwrap();
    for length in 0..blob.len() {
      assert!(verify(&blob[..length], &statement, &keys).is_err());
    }
    let mut huge = blob.clone();
    huge[10..14].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
      verify(&huge, &statement, &keys),
      Err(SignatureRefusal::Malformed)
    );
    let mut foreign = blob.clone();
    foreign[0] = b'X';
    assert_eq!(
      verify(&foreign, &statement, &keys),
      Err(SignatureRefusal::Malformed)
    );
    let mut trailing = blob.clone();
    trailing.push(0);
    assert_eq!(
      verify(&trailing, &statement, &keys),
      Err(SignatureRefusal::Malformed)
    );
  }

  /// A-321, hostile text: do read bad armour, base64 of a bad length, misplaced padding, a foreign character, a key of
  /// another type and a key line whose named type is not its blob's; expect each refused, and well-formed padding
  /// decoded.
  #[test]
  fn hostile_armour_base64_and_keys_are_refused() {
    assert_eq!(dearmour("no armour"), Err(SignatureRefusal::Malformed));
    assert_eq!(base64_decode("abc"), Err(SignatureRefusal::Malformed));
    assert_eq!(base64_decode("a=bc"), Err(SignatureRefusal::Malformed));
    assert_eq!(base64_decode("ab!c"), Err(SignatureRefusal::Malformed));
    assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
    assert_eq!(base64_decode("TQ==").unwrap(), b"M");
    assert_eq!(
      TrustedKey::from_openssh("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAAQC7"),
      Err(SignatureRefusal::Unsupported)
    );
    let mislabelled = GOLDEN_KEY.replacen("ssh-ed25519", "sk-ssh-ed25519@openssh.com", 1);
    assert_eq!(
      TrustedKey::from_openssh(&mislabelled),
      Err(SignatureRefusal::Malformed)
    );
    assert_eq!(
      TrustedKey::from_wire(b"not a key".to_vec()),
      Err(SignatureRefusal::Malformed)
    );
  }

  /// A-321, golden: do record each verdict as a chain entry keeps it and read it back; expect every verified and
  /// invalid verdict read back as it was, an unsigned submit recording nothing and read back unsigned, and a recorded
  /// verdict of an unknown kind, key or reason read as none.
  #[test]
  fn a_recorded_verdict_reads_back_as_it_was() {
    let signers =
      [SignerKey::Ed25519, SignerKey::SecurityKeyEd25519].map(|key| Verification::Verified {
        signer: Signer {
          key,
          fingerprint: [7; FINGERPRINT_BYTES],
        },
      });
    let faults = FAULT_CODES.map(|reason| Verification::Invalid { reason });
    for verdict in signers.iter().chain(faults.iter()) {
      let recorded = to_entry(verdict);
      assert!(recorded.is_some());
      assert_eq!(of_entry(recorded).as_ref(), Some(verdict));
    }
    assert_eq!(to_entry(&Verification::Unsigned), None);
    assert_eq!(of_entry(None), Some(Verification::Unsigned));
    let mut unknown = to_entry(&signers[0]).unwrap();
    unknown.kind = 0;
    assert_eq!(of_entry(Some(unknown)), None);
    unknown.kind = VERDICT_VERIFIED;
    unknown.detail = 9;
    assert_eq!(of_entry(Some(unknown)), None);
    unknown.kind = VERDICT_INVALID;
    assert_eq!(of_entry(Some(unknown)), None);
  }
}

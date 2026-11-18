//! SSH signatures over slates' statements, verified against a set of keys (A-285): the verifier signed work stands on
//! (Ada, 2026-11-18: "we can (and should) support signed work"). A merge or a landing may carry a signature that slates
//! verifies and records as provenance, and a volume may require one; it never decides whether a write is permitted,
//! which the write mode the user gave the agent does (R10, D-22). A signature is OpenSSH's `PROTOCOL.sshsig` under a
//! namespace, by an `ssh-ed25519` key or a FIDO `sk-ssh-ed25519` key, and [`verify_grant_signature`] refuses an `sk`
//! signature whose authenticator did not report the touch (the user-presence flag).
//!
//! **What is signed.** [`GrantStatement::encode`] is the exact text a key signs: a landing, its manifest hash, its
//! target, the scope and term, a nonce and an expiry in wall-clock seconds, plain text one field a line, so
//! `ssh-keygen -Y sign` signs it as a file and a person can read what they sign; the target carries its length, so no
//! path can forge a later line. It was built for A-285's first reading (a grant's proof, since withdrawn); signed
//! work's own statement (a merge's volume, base, head and manifest) replaces it when that piece lands (GAPS).
//!
//! **Why here, with aws-lc-rs.** Ed25519 and the SHA-2 digests come from AWS-LC, slates' one cryptographic library
//! (A-66), which the server already links on every platform through rustls. The format code is pure, and its golden
//! vector is a real `ssh-keygen -Y sign` signature (OpenSSH 10.0p2, 2026-11-18), checked with `ssh-keygen -Y verify`.
//!
//! **Refusals.** Every failure is a [`SignatureRefusal`]: malformed bytes, another namespace, an unsupported key or
//! hash, a key not registered, a signature that does not verify, a security key not touched, or a statement past its
//! expiry.

use aws_lc_rs::digest;
use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};

/// Format: the SSH signature namespace grants are signed under (`ssh-keygen -Y sign -n slates-grant`). A signature
/// made for any other purpose (a git commit, a file) names another namespace and never verifies as a grant.
pub const NAMESPACE: &str = "slates-grant";
/// Format: the statement's first line, naming its grammar's version.
const STATEMENT_HEADER: &str = "slates grant v1";
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
/// Format: the bytes of a nonce the daemon issues with a presentation: 256 bits, so a guess never meets one.
pub const NONCE_BYTES: usize = 32;

/// Why a signed grant was refused. Each is a closed reason the daemon counts and answers as
/// `GrantIssuerUnverified`; none is ever a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureRefusal {
  /// The armour, the base64 or the blob's grammar is wrong, or a length runs past its bytes.
  Malformed,
  /// The signature was made under a namespace other than [`NAMESPACE`].
  Namespace,
  /// The key type, the signature type or the message hash is not one slates accepts.
  Unsupported,
  /// The signing key is not one the human registered.
  UnregisteredKey,
  /// The signature does not verify over this statement under its key.
  BadSignature,
  /// A security key signed without the human's touch.
  NotTouched,
  /// The statement's expiry has passed.
  Expired,
}

impl SignatureRefusal {
  /// The counter this refusal moves (`status` shows it), one per reason.
  #[must_use]
  pub fn counter(self) -> &'static str {
    match self {
      SignatureRefusal::Malformed => "grant_signature_malformed",
      SignatureRefusal::Namespace => "grant_signature_namespace",
      SignatureRefusal::Unsupported => "grant_signature_unsupported",
      SignatureRefusal::UnregisteredKey => "grant_signature_unregistered_key",
      SignatureRefusal::BadSignature => "grant_signature_bad",
      SignatureRefusal::NotTouched => "grant_signature_not_touched",
      SignatureRefusal::Expired => "grant_signature_expired",
    }
  }
}

/// The scope a statement approves, as its line names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementScope {
  /// One landing.
  Once,
  /// Later snapshots and manifests of the volume into the target, for the term.
  Session,
}

/// What the human's key signs: the exact landing approved (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantStatement {
  /// The landing presented.
  pub landing: u64,
  /// The manifest hash the human saw.
  pub manifest: [u8; 32],
  /// The target directory as presented.
  pub target: String,
  /// The scope.
  pub scope: StatementScope,
  /// The term of a session grant, in nanoseconds; zero for one landing.
  pub term_ns: u64,
  /// The nonce the daemon issued with the presentation.
  pub nonce: [u8; NONCE_BYTES],
  /// The wall-clock second (Unix time) after which the statement is refused.
  pub expires_unix_s: u64,
}

impl GrantStatement {
  /// The exact bytes signed: one field a line, the target prefixed by its byte length.
  #[must_use]
  pub fn encode(&self) -> Vec<u8> {
    let scope = match self.scope {
      StatementScope::Once => "once",
      StatementScope::Session => "session",
    };
    format!(
      "{STATEMENT_HEADER}\nlanding {}\nmanifest {}\ntarget {}:{}\nscope {scope}\nterm_ns {}\nnonce {}\nexpires {}\n",
      self.landing,
      hex(&self.manifest),
      self.target.len(),
      self.target,
      self.term_ns,
      hex(&self.nonce),
      self.expires_unix_s,
    )
    .into_bytes()
  }

  /// The challenge a WebAuthn assertion over this statement signs: its SHA-256 (W3C WebAuthn Level 3 §13.4.3, a
  /// challenge of at least 16 bytes).
  #[must_use]
  pub fn challenge(&self) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(digest::digest(&digest::SHA256, &self.encode()).as_ref());
    out
  }
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

/// A public key the human registered, in SSH wire form (the base64 field of an `authorized_keys` line, decoded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredKey(pub Vec<u8>);

impl RegisteredKey {
  /// Parses one OpenSSH public key line (`TYPE BASE64 [COMMENT]`), accepting only the key types a grant may be
  /// signed with.
  ///
  /// # Errors
  /// [`SignatureRefusal::Malformed`] for a line that is not a key, [`SignatureRefusal::Unsupported`] for another type.
  pub fn from_openssh(line: &str) -> Result<RegisteredKey, SignatureRefusal> {
    let mut words = line.split_whitespace();
    let kind = words.next().ok_or(SignatureRefusal::Malformed)?;
    let blob = base64_decode(words.next().ok_or(SignatureRefusal::Malformed)?)?;
    let key = SshKey::parse(&blob)?;
    if key.kind() != kind {
      return Err(SignatureRefusal::Malformed);
    }
    Ok(RegisteredKey(blob))
  }
}

/// A key a grant may be signed with, parsed from its wire form.
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
  fn kind(&self) -> &'static str {
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
      .take(4)?
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

/// Decodes standard base64 (RFC 4648 §4) with padding, ignoring whitespace (the armour wraps lines).
///
/// # Errors
/// [`SignatureRefusal::Malformed`] for any other character, a bad length or misplaced padding.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, SignatureRefusal> {
  /// The six bits a character stands for.
  fn sextet(character: u8) -> Option<u32> {
    match character {
      b'A'..=b'Z' => Some(u32::from(character.wrapping_sub(b'A'))),
      b'a'..=b'z' => Some(u32::from(character.wrapping_sub(b'a')).wrapping_add(26)),
      b'0'..=b'9' => Some(u32::from(character.wrapping_sub(b'0')).wrapping_add(52)),
      b'+' => Some(62),
      b'/' => Some(63),
      _ => None,
    }
  }
  let symbols: Vec<u8> = text
    .bytes()
    .filter(|byte| !byte.is_ascii_whitespace())
    .collect();
  if !symbols.len().is_multiple_of(4) {
    return Err(SignatureRefusal::Malformed);
  }
  let (groups, _) = symbols.as_chunks::<4>();
  let mut out = Vec::with_capacity(groups.len().saturating_mul(3));
  let last = groups.len().saturating_sub(1);
  for (index, group) in groups.iter().enumerate() {
    let padding = group.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 || (padding > 0 && index != last) {
      return Err(SignatureRefusal::Malformed);
    }
    let mut word = 0u32;
    for (position, symbol) in group.iter().enumerate() {
      let value = if position.saturating_add(padding) >= 4 {
        0
      } else {
        sextet(*symbol).ok_or(SignatureRefusal::Malformed)?
      };
      word = word.wrapping_shl(6) | value;
    }
    out.extend(
      word
        .to_be_bytes()
        .iter()
        .skip(1)
        .take(3usize.saturating_sub(padding)),
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

/// Verifies `signature` (an SSH signature blob, [`dearmour`]ed) over `statement` at wall-clock second `now_unix_s`:
/// made under [`NAMESPACE`], by one of `registered`, valid under that key, touched if made by a security key, and not
/// past the statement's expiry. Answers the registered key that signed.
///
/// # Errors
/// The [`SignatureRefusal`] that applies; the expiry is judged last, so a forged statement is refused as forged.
pub fn verify_grant_signature<'k>(
  signature: &[u8],
  statement: &GrantStatement,
  registered: &'k [RegisteredKey],
  now_unix_s: u64,
) -> Result<&'k RegisteredKey, SignatureRefusal> {
  let mut reader = Reader(signature);
  if reader.take(SSHSIG_MAGIC.len())? != SSHSIG_MAGIC || reader.u32()? != SSHSIG_VERSION {
    return Err(SignatureRefusal::Malformed);
  }
  let public_blob = reader.string()?;
  let namespace = reader.string()?;
  let reserved = reader.string()?;
  let hash_name = reader.string()?;
  let signature_blob = reader.string()?;
  reader.end()?;
  if namespace != NAMESPACE.as_bytes() {
    return Err(SignatureRefusal::Namespace);
  }
  let key = SshKey::parse(public_blob)?;
  let algorithm = match hash_name {
    b"sha512" => &digest::SHA512,
    b"sha256" => &digest::SHA256,
    _ => return Err(SignatureRefusal::Unsupported),
  };
  let registration = registered
    .iter()
    .find(|candidate| candidate.0 == public_blob)
    .ok_or(SignatureRefusal::UnregisteredKey)?;
  let mut signed = SSHSIG_MAGIC.to_vec();
  put_string(&mut signed, NAMESPACE.as_bytes())?;
  put_string(&mut signed, reserved)?;
  put_string(&mut signed, hash_name)?;
  put_string(
    &mut signed,
    digest::digest(algorithm, &statement.encode()).as_ref(),
  )?;
  verify_under(&key, signature_blob, &signed)?;
  if now_unix_s > statement.expires_unix_s {
    return Err(SignatureRefusal::Expired);
  }
  Ok(registration)
}

/// Verifies the inner signature (type, bytes and, for a security key, its flags and counter) over `signed`.
fn verify_under(
  key: &SshKey<'_>,
  signature_blob: &[u8],
  signed: &[u8],
) -> Result<(), SignatureRefusal> {
  let mut reader = Reader(signature_blob);
  let kind = reader.string()?;
  if kind != key.kind().as_bytes() {
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

  /// Golden: the statement `ssh-keygen -Y sign -f k -n slates-grant` signed (OpenSSH 10.0p2, 2026-11-18).
  fn golden_statement() -> GrantStatement {
    GrantStatement {
      landing: 7,
      manifest: [1; 32],
      target: "/srv/project".to_owned(),
      scope: StatementScope::Once,
      term_ns: 0,
      nonce: [2; NONCE_BYTES],
      expires_unix_s: 1_795_000_000,
    }
  }

  /// Golden: the signing key's public line.
  const GOLDEN_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGOgU53Zpx5za3nc5y1W+PdV5Z+v1gjxPl5uPzbw7bEf test";

  /// Golden: `ssh-keygen -Y sign`'s output over [`golden_statement`]'s encoding; `ssh-keygen -Y verify` accepts it.
  const GOLDEN_SIGNATURE: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgY6BTndmnHnNredznLVb491Xln6
/WCPE+Xm4/NvDtsR8AAAAMc2xhdGVzLWdyYW50AAAAAAAAAAZzaGE1MTIAAABTAAAAC3Nz
aC1lZDI1NTE5AAAAQMqR0LFgP4528Ga5y0G8mf9hLUoSsAh/kMe0W9VzluaDVt6erA2Skx
PgowBcvcisBQ+rfA7nqv3MYxc2I5mR2Qs=
-----END SSH SIGNATURE-----
";

  /// A second registered key, not the signer.
  const OTHER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBvhoZchusdGdoVMHsajikHEeuGclyYt+7aOArbQCy1X other";

  /// A second-before-expiry clock.
  const BEFORE_EXPIRY: u64 = 1_794_999_999;

  fn golden_keys() -> Vec<RegisteredKey> {
    vec![RegisteredKey::from_openssh(GOLDEN_KEY).unwrap()]
  }

  /// The golden statement's bytes, pinned: what `ssh-keygen` signed is exactly what `encode` makes.
  #[test]
  fn the_statement_encodes_one_field_a_line() {
    assert_eq!(
      String::from_utf8(golden_statement().encode()).unwrap(),
      "slates grant v1\nlanding 7\nmanifest 0101010101010101010101010101010101010101010101010101010101010101\n\
       target 12:/srv/project\nscope once\nterm_ns 0\n\
       nonce 0202020202020202020202020202020202020202020202020202020202020202\nexpires 1795000000\n"
    );
  }

  /// R10/D-22: a real `ssh-keygen -Y sign` signature over the statement, by a registered key, verifies.
  #[test]
  fn a_registered_keys_ssh_signature_over_the_statement_verifies() {
    let keys = golden_keys();
    let blob = dearmour(GOLDEN_SIGNATURE).unwrap();
    let signer = verify_grant_signature(&blob, &golden_statement(), &keys, BEFORE_EXPIRY).unwrap();
    assert_eq!(signer, &keys[0]);
  }

  /// A forged signature is refused: every bit of the signature's bytes flipped in turn is refused, and none verifies.
  #[test]
  fn a_forged_signature_is_refused() {
    let keys = golden_keys();
    let blob = dearmour(GOLDEN_SIGNATURE).unwrap();
    let statement = golden_statement();
    let flips = (0..blob.len()).flat_map(|index| (0..8).map(move |bit| (index, bit)));
    for (index, bit) in flips {
      let mut forged = blob.clone();
      forged[index] ^= 1 << bit;
      assert!(
        verify_grant_signature(&forged, &statement, &keys, BEFORE_EXPIRY).is_err(),
        "byte {index} bit {bit} flipped still verified"
      );
    }
    let raw_signature_start = blob.len() - ED25519_SIGNATURE_BYTES;
    let mut forged = blob.clone();
    forged[raw_signature_start] ^= 1;
    assert_eq!(
      verify_grant_signature(&forged, &statement, &keys, BEFORE_EXPIRY),
      Err(SignatureRefusal::BadSignature)
    );
  }

  /// An unregistered key is refused, even though its signature is valid under it.
  #[test]
  fn an_unregistered_key_is_refused() {
    let others = vec![RegisteredKey::from_openssh(OTHER_KEY).unwrap()];
    let blob = dearmour(GOLDEN_SIGNATURE).unwrap();
    assert_eq!(
      verify_grant_signature(&blob, &golden_statement(), &others, BEFORE_EXPIRY),
      Err(SignatureRefusal::UnregisteredKey)
    );
    assert_eq!(
      verify_grant_signature(&blob, &golden_statement(), &[], BEFORE_EXPIRY),
      Err(SignatureRefusal::UnregisteredKey)
    );
  }

  /// A changed manifest is refused, and so is every other changed field: the signature binds the whole statement.
  #[test]
  fn a_changed_manifest_or_any_other_field_is_refused() {
    let keys = golden_keys();
    let blob = dearmour(GOLDEN_SIGNATURE).unwrap();
    let changes: [fn(&mut GrantStatement); 7] = [
      |s| s.manifest[31] ^= 1,
      |s| s.landing += 1,
      |s| s.target.push('x'),
      |s| s.scope = StatementScope::Session,
      |s| s.term_ns = 1,
      |s| s.nonce[0] ^= 1,
      |s| s.expires_unix_s += 1,
    ];
    for (index, change) in changes.iter().enumerate() {
      let mut statement = golden_statement();
      change(&mut statement);
      assert_eq!(
        verify_grant_signature(&blob, &statement, &keys, BEFORE_EXPIRY),
        Err(SignatureRefusal::BadSignature),
        "change {index}"
      );
    }
  }

  /// A statement past its expiry is refused; at the expiry second it still verifies.
  #[test]
  fn an_expired_statement_is_refused() {
    let keys = golden_keys();
    let blob = dearmour(GOLDEN_SIGNATURE).unwrap();
    let statement = golden_statement();
    assert!(verify_grant_signature(&blob, &statement, &keys, statement.expires_unix_s).is_ok());
    assert_eq!(
      verify_grant_signature(&blob, &statement, &keys, statement.expires_unix_s + 1),
      Err(SignatureRefusal::Expired)
    );
  }

  /// Builds an SSH signature blob as `ssh-keygen` would, signed by `sign` over the signed data.
  fn sshsig(
    public_blob: &[u8],
    namespace: &str,
    statement: &GrantStatement,
    sign: impl Fn(&[u8]) -> Vec<u8>,
  ) -> Vec<u8> {
    let mut signed = SSHSIG_MAGIC.to_vec();
    put_string(&mut signed, namespace.as_bytes()).unwrap();
    put_string(&mut signed, b"").unwrap();
    put_string(&mut signed, b"sha512").unwrap();
    put_string(
      &mut signed,
      digest::digest(&digest::SHA512, &statement.encode()).as_ref(),
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

  /// A signature made under another namespace (a git commit's, a file's) is refused as such, though valid.
  #[test]
  fn a_signature_under_another_namespace_is_refused() {
    let pair = fresh_pair();
    let mut public_blob = Vec::new();
    put_string(&mut public_blob, KEY_ED25519.as_bytes()).unwrap();
    put_string(&mut public_blob, pair.public_key().as_ref()).unwrap();
    let keys = vec![RegisteredKey(public_blob.clone())];
    let sign = |signed: &[u8]| {
      let mut inner = Vec::new();
      put_string(&mut inner, KEY_ED25519.as_bytes()).unwrap();
      put_string(&mut inner, pair.sign(signed).as_ref()).unwrap();
      inner
    };
    let statement = golden_statement();
    let good = sshsig(&public_blob, NAMESPACE, &statement, sign);
    assert!(verify_grant_signature(&good, &statement, &keys, BEFORE_EXPIRY).is_ok());
    let git = sshsig(&public_blob, "git", &statement, sign);
    assert_eq!(
      verify_grant_signature(&git, &statement, &keys, BEFORE_EXPIRY),
      Err(SignatureRefusal::Namespace)
    );
  }

  /// A FIDO key's signature verifies when the human touched it and is refused `NotTouched` when the authenticator
  /// reports no user presence. Built to `PROTOCOL.u2f` with a software key standing in for the authenticator; a real
  /// security key's signature is the end-to-end proof's (GAPS).
  #[test]
  fn a_security_keys_signature_needs_the_touch() {
    let pair = fresh_pair();
    let application = b"ssh:";
    let mut public_blob = Vec::new();
    put_string(&mut public_blob, KEY_SK_ED25519.as_bytes()).unwrap();
    put_string(&mut public_blob, pair.public_key().as_ref()).unwrap();
    put_string(&mut public_blob, application).unwrap();
    let keys = vec![RegisteredKey(public_blob.clone())];
    let statement = golden_statement();
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
    assert!(verify_grant_signature(&touched, &statement, &keys, BEFORE_EXPIRY).is_ok());
    let untouched = sshsig(&public_blob, NAMESPACE, &statement, signed_with(0));
    assert_eq!(
      verify_grant_signature(&untouched, &statement, &keys, BEFORE_EXPIRY),
      Err(SignatureRefusal::NotTouched)
    );
  }

  /// Hostile signature bytes: empty, truncated at every length, a length of `u32::MAX`, foreign magic and trailing
  /// bytes are each refused, never a panic.
  #[test]
  fn hostile_signature_bytes_are_refused() {
    let keys = golden_keys();
    let statement = golden_statement();
    let blob = dearmour(GOLDEN_SIGNATURE).unwrap();
    for length in 0..blob.len() {
      assert!(verify_grant_signature(&blob[..length], &statement, &keys, BEFORE_EXPIRY).is_err());
    }
    let mut huge = blob.clone();
    huge[10..14].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
      verify_grant_signature(&huge, &statement, &keys, BEFORE_EXPIRY),
      Err(SignatureRefusal::Malformed)
    );
    let mut foreign = blob.clone();
    foreign[0] = b'X';
    assert_eq!(
      verify_grant_signature(&foreign, &statement, &keys, BEFORE_EXPIRY),
      Err(SignatureRefusal::Malformed)
    );
    let mut trailing = blob.clone();
    trailing.push(0);
    assert_eq!(
      verify_grant_signature(&trailing, &statement, &keys, BEFORE_EXPIRY),
      Err(SignatureRefusal::Malformed)
    );
  }

  /// Hostile text: bad armour, base64 of a bad length, misplaced padding or a foreign character, and a key of
  /// another type are each refused; well-formed padding decodes.
  #[test]
  fn hostile_armour_base64_and_keys_are_refused() {
    assert_eq!(dearmour("no armour"), Err(SignatureRefusal::Malformed));
    assert_eq!(base64_decode("abc"), Err(SignatureRefusal::Malformed));
    assert_eq!(base64_decode("a=bc"), Err(SignatureRefusal::Malformed));
    assert_eq!(base64_decode("ab!c"), Err(SignatureRefusal::Malformed));
    assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
    assert_eq!(base64_decode("TQ==").unwrap(), b"M");
    assert_eq!(
      RegisteredKey::from_openssh("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAAQC7"),
      Err(SignatureRefusal::Unsupported)
    );
  }
}

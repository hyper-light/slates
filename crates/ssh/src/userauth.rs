//! User authentication by public key, the server's side (A-633; RFC 4252; RFC 8332; RFC 8308 §3.1; §4.17.2 "Access";
//! §4.13 principals).
//!
//! **What a client may do.** After `ssh-userauth` is accepted the client sends `SSH_MSG_USERAUTH_REQUEST`s naming a
//! user, the service to start (`ssh-connection`, the only one served) and a method:
//! - `none`, the probe "to get the list of supported methods" \[B: RFC 4252 §4\], answered with a failure naming
//!   `publickey` and never counted as a failure;
//! - `publickey` without a signature, the query, answered `SSH_MSG_USERAUTH_PK_OK` when the key is one a principal
//!   holds for the user, so the client signs only with a key that can work;
//! - `publickey` with a signature over the session id and the request (§7), checked through aws-lc-rs: the server
//!   "MUST check that the key is a valid authenticator for the user, and MUST check that the signature is valid";
//! - anything else, refused with a failure naming `publickey`.
//!
//! **Keys.** `ssh-ed25519` (RFC 8709) and RSA with SHA-2, `rsa-sha2-512` and `rsa-sha2-256` over the `ssh-rsa` key
//! blob (RFC 8332 §3), within the 2048 to 8192 bits aws-lc-rs's RSA verifiers take. `ssh-rsa` itself, RSA with SHA-1,
//! is refused, as OpenSSH 8.8 refused it by default; ECDSA and FIDO keys are owed (§4.17.2's owed reading). A signature
//! must name the algorithm its request named ("the client MUST encode the same algorithm name in the signature"
//! \[B: RFC 8332 §3.2\]); an RSA signature shorter than its modulus is padded with leading zeros first, which §3 allows.
//! The algorithms are announced in RFC 8308's `server-sig-algs` ([`server_sig_algs`]), which the transport sends after
//! its first NEWKEYS.
//!
//! **Who.** A key proves a principal only through [`KeyHolders`], the server's lookup over its principals (§4.13); this
//! crate keeps no keys. A principal's policy may require more than one key: until enough distinct keys of the same
//! principal have signed, each success is answered as a partial success (RFC 4252 §5.1), and a change of user name
//! drops what was proven ("MUST flush any accumulated authentication states if they change").
//!
//! **Bounds.** Failures are counted, the probe and partial successes apart, and past the caller's limit the server
//! disconnects (§4: "SHOULD limit the number of failed authentication attempts"); RFC 4252 recommends 20
//! ([`RECOMMENDED_FAILURE_LIMIT`]). A banner, when set, is sent once, before the first answer (§5.4). After success,
//! further requests are dropped (§5.1: "SHOULD be silently ignored"). A signature is checked only for a key a principal
//! holds, so a client can make the server verify nothing it was not going to accept.

use aws_lc_rs::digest::{SHA256, digest};
use aws_lc_rs::signature::{
  ED25519, RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA512, RsaParameters,
  RsaPublicKeyComponents, UnparsedPublicKey,
};

use crate::kex::HASH_LEN;
use crate::wire::{Reader, WireRefusal, put_boolean, put_byte, put_name_list, put_string};

/// Format: `SSH_MSG_USERAUTH_REQUEST` (RFC 4252 §6).
pub const USERAUTH_REQUEST: u8 = 50;
/// Format: `SSH_MSG_USERAUTH_FAILURE`.
pub const USERAUTH_FAILURE: u8 = 51;
/// Format: `SSH_MSG_USERAUTH_SUCCESS`.
pub const USERAUTH_SUCCESS: u8 = 52;
/// Format: `SSH_MSG_USERAUTH_BANNER`.
pub const USERAUTH_BANNER: u8 = 53;
/// Format: `SSH_MSG_USERAUTH_PK_OK` (RFC 4252 §7).
pub const USERAUTH_PK_OK: u8 = 60;
/// The service authentication starts (RFC 4254 §1).
pub const CONNECTION_SERVICE: &str = "ssh-connection";
/// The method the server runs.
pub const PUBLICKEY: &str = "publickey";
/// The probe method (RFC 4252 §5.2).
pub const NONE: &str = "none";
/// RFC 8308 §3.1's extension name.
pub const SERVER_SIG_ALGS: &str = "server-sig-algs";
/// Format: RFC 4252 §4's recommended limit on failed attempts ("the RECOMMENDED limit is 20 attempts").
pub const RECOMMENDED_FAILURE_LIMIT: u32 = 20;
/// Format: an Ed25519 public key's bytes (RFC 8032 §5.1.5).
const ED25519_PUBLIC_LEN: usize = 32;
/// Format: an Ed25519 signature's bytes (RFC 8032 §5.1.6).
const ED25519_SIGNATURE_LEN: usize = 64;
/// Derived: the fewest modulus bits aws-lc-rs's `RSA_PKCS1_2048_8192` verifiers take, the name's lower bound.
const RSA_MIN_BITS: u64 = 2048;
/// Derived: the most, the name's upper bound.
const RSA_MAX_BITS: u64 = 8192;
/// Format: bits in a byte.
const BITS_PER_BYTE: u64 = 8;

/// A signature algorithm for user keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureAlgorithm {
  /// `ssh-ed25519`.
  Ed25519,
  /// `rsa-sha2-512`.
  RsaSha512,
  /// `rsa-sha2-256`.
  RsaSha256,
}

impl SignatureAlgorithm {
  /// The algorithms the server accepts, as `server-sig-algs` lists them.
  pub const ACCEPTED: [SignatureAlgorithm; 3] = [
    SignatureAlgorithm::Ed25519,
    SignatureAlgorithm::RsaSha512,
    SignatureAlgorithm::RsaSha256,
  ];

  /// The algorithm's name in a request and a signature.
  pub fn name(self) -> &'static str {
    match self {
      SignatureAlgorithm::Ed25519 => "ssh-ed25519",
      SignatureAlgorithm::RsaSha512 => "rsa-sha2-512",
      SignatureAlgorithm::RsaSha256 => "rsa-sha2-256",
    }
  }

  /// The algorithm a name stands for.
  pub fn from_name(name: &[u8]) -> Option<Self> {
    Self::ACCEPTED
      .into_iter()
      .find(|algorithm| algorithm.name().as_bytes() == name)
  }

  /// The key format its key blob carries (RFC 8332 §3 keeps `ssh-rsa`'s).
  pub fn key_type(self) -> &'static str {
    match self {
      SignatureAlgorithm::Ed25519 => "ssh-ed25519",
      SignatureAlgorithm::RsaSha512 | SignatureAlgorithm::RsaSha256 => "ssh-rsa",
    }
  }
}

/// RFC 8308 §3.1's `server-sig-algs` value: the accepted algorithms' names, comma-separated.
pub fn server_sig_algs() -> Vec<u8> {
  let names: Vec<&str> = SignatureAlgorithm::ACCEPTED
    .iter()
    .map(|algorithm| algorithm.name())
    .collect();
  names.join(",").into_bytes()
}

/// A user's public key, read from its blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserKey {
  /// An Ed25519 key.
  Ed25519 {
    /// The 32-byte public key.
    public: [u8; ED25519_PUBLIC_LEN],
  },
  /// An RSA key.
  Rsa {
    /// The public exponent, big-endian with no leading zero.
    exponent: Vec<u8>,
    /// The modulus, big-endian with no leading zero.
    modulus: Vec<u8>,
  },
}

/// Why a key blob was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRefusal {
  /// Its fields did not read.
  Malformed(WireRefusal),
  /// Its format is not the one its algorithm takes.
  Type,
  /// An Ed25519 key that is not 32 bytes.
  Length,
  /// An RSA exponent or modulus that is zero or negative.
  RsaComponent,
  /// An RSA modulus outside the 2048 to 8192 bits the verifiers take.
  RsaModulusSize {
    /// The modulus's bits.
    bits: u64,
  },
}

impl UserKey {
  /// The key in `blob`, for `algorithm`.
  pub fn parse(algorithm: SignatureAlgorithm, blob: &[u8]) -> Result<Self, KeyRefusal> {
    let mut reader = Reader::new(blob);
    let key_type = reader.string().map_err(KeyRefusal::Malformed)?;
    if key_type != algorithm.key_type().as_bytes() {
      return Err(KeyRefusal::Type);
    }
    let key = match algorithm {
      SignatureAlgorithm::Ed25519 => {
        let public = reader.string().map_err(KeyRefusal::Malformed)?;
        UserKey::Ed25519 {
          public: public.try_into().map_err(|_| KeyRefusal::Length)?,
        }
      }
      SignatureAlgorithm::RsaSha512 | SignatureAlgorithm::RsaSha256 => {
        let exponent = positive(reader.string().map_err(KeyRefusal::Malformed)?)?;
        let modulus = positive(reader.string().map_err(KeyRefusal::Malformed)?)?;
        let bits = bit_length(modulus);
        if !(RSA_MIN_BITS..=RSA_MAX_BITS).contains(&bits) {
          return Err(KeyRefusal::RsaModulusSize { bits });
        }
        UserKey::Rsa {
          exponent: exponent.to_vec(),
          modulus: modulus.to_vec(),
        }
      }
    };
    reader.finish().map_err(KeyRefusal::Malformed)?;
    Ok(key)
  }
}

/// An `mpint` that must be positive, its leading zero bytes dropped (RFC 4251 §5).
fn positive(mpint: &[u8]) -> Result<&[u8], KeyRefusal> {
  if mpint.first().is_some_and(|byte| byte.leading_zeros() == 0) {
    return Err(KeyRefusal::RsaComponent);
  }
  let first = mpint
    .iter()
    .position(|byte| *byte != 0)
    .ok_or(KeyRefusal::RsaComponent)?;
  mpint.get(first..).ok_or(KeyRefusal::RsaComponent)
}

/// The bits of the positive big-endian number `bytes`, which has no leading zero byte.
fn bit_length(bytes: &[u8]) -> u64 {
  let leading = bytes
    .first()
    .map_or(0, |byte| u64::from(byte.leading_zeros()));
  crate::to_u64(bytes.len())
    .saturating_mul(BITS_PER_BYTE)
    .saturating_sub(leading)
}

/// The key's SHA-256 fingerprint as OpenSSH prints it: `SHA256:` and the hash of its blob in base64 without padding
/// (`ssh-keygen -l -E sha256`).
pub fn fingerprint(blob: &[u8]) -> String {
  let hash = digest(&SHA256, blob);
  format!("SHA256:{}", base64_unpadded(hash.as_ref()))
}

/// Format: RFC 4648 §4's alphabet.
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// Format: RFC 4648 §4 reads three bytes as four six-bit characters.
const BASE64_GROUP: usize = 3;
/// Format: the bits of one character.
const SIXTET_BITS: u32 = 6;
/// Format: the mask of one character's bits.
const SIXTET: u32 = 0x3f;

/// `bytes` in RFC 4648 §4's base64, without the padding OpenSSH's fingerprints leave off.
fn base64_unpadded(bytes: &[u8]) -> String {
  let byte_bits = u32::try_from(BITS_PER_BYTE).unwrap_or(u32::MAX);
  let mut text = String::new();
  for group in bytes.chunks(BASE64_GROUP) {
    let word = group.iter().fold(0u32, |word, byte| {
      word.checked_shl(byte_bits).unwrap_or(0) | u32::from(*byte)
    });
    let bits = u32::try_from(group.len())
      .unwrap_or(0)
      .saturating_mul(byte_bits);
    let characters = bits.div_ceil(SIXTET_BITS);
    let aligned = word
      .checked_shl(SIXTET_BITS.saturating_mul(characters).saturating_sub(bits))
      .unwrap_or(0);
    for index in (0..characters).rev() {
      let sixtet = aligned
        .checked_shr(SIXTET_BITS.saturating_mul(index))
        .unwrap_or(0)
        & SIXTET;
      let character = usize::try_from(sixtet)
        .ok()
        .and_then(|sixtet| BASE64.get(sixtet))
        .copied()
        .unwrap_or(b'=');
      text.push(char::from(character));
    }
  }
  text
}

/// The holders of user keys: the server's lookup over its principals (§4.13). This crate keeps no keys.
pub trait KeyHolders {
  /// A principal of §4.13, as the server names one.
  type Principal: Clone + PartialEq + core::fmt::Debug;

  /// Who holds `key` (its blob `blob`) for `user`, and how many of that principal's keys must sign.
  fn holding(&mut self, user: &[u8], key: &UserKey, blob: &[u8]) -> Holding<Self::Principal>;
}

/// What the holders say of a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holding<P> {
  /// No principal holds it for this user.
  Nobody,
  /// The holders cannot say yet: their record is somewhere a lookup must wait for (A-651). The request is answered
  /// once its caller is told ([`Answer::wanted`]); nothing about it is decided or counted until then.
  Unknown,
  /// `principal` holds it, and its policy wants `keys_required` distinct keys of its to sign (one at least).
  Holds {
    /// The principal.
    principal: P,
    /// The distinct keys that must sign before authentication succeeds.
    keys_required: u32,
  },
}

/// Why authentication ended the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthRefusal {
  /// A request's fields did not read.
  Malformed(WireRefusal),
  /// The request names a service the server does not start (RFC 4252 §5: "authentication MUST NOT be accepted").
  ServiceNotAvailable,
  /// The client failed more often than the limit allows (RFC 4252 §4).
  TooManyFailures {
    /// The failures counted.
    failures: u32,
  },
}

/// A success: who authenticated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authenticated<P> {
  /// The user name the client gave.
  pub user: Vec<u8>,
  /// The principal its keys proved.
  pub principal: P,
}

/// What one request produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer<P> {
  /// The messages to send, in order.
  pub replies: Vec<Vec<u8>>,
  /// The authentication, when this request completed it.
  pub authenticated: Option<Authenticated<P>>,
  /// The key the holders could not answer for yet ([`Holding::Unknown`]): the request is unanswered, and is asked again
  /// once the caller knows who holds the key.
  pub wanted: Option<Wanted>,
}

/// A key the holders could not answer for yet, and for whom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
  /// The user name the client gave.
  pub user: Vec<u8>,
  /// The key.
  pub key: UserKey,
  /// Its blob, as the client sent it.
  pub blob: Vec<u8>,
}

/// Keys proven so far for one user's principal.
#[derive(Clone, Debug)]
struct Partial<P> {
  /// The user name they were proven under.
  user: Vec<u8>,
  /// The principal.
  principal: P,
  /// The distinct key blobs that signed.
  proven: Vec<Vec<u8>>,
}

/// The server's side of RFC 4252 for one connection.
#[derive(Clone, Debug)]
pub struct Authenticator<P> {
  /// The session id every signature covers.
  session_id: [u8; HASH_LEN],
  /// The banner to send once, if any.
  banner: Option<String>,
  /// The failures allowed.
  failure_limit: u32,
  /// The failures counted.
  failures: u32,
  /// Keys proven toward a principal that wants more than one.
  partial: Option<Partial<P>>,
  /// Whether authentication succeeded.
  done: bool,
}

/// A request's method.
enum Method<'a> {
  /// The probe.
  None,
  /// A key, queried or signed.
  PublicKey {
    /// The algorithm's name.
    algorithm: &'a [u8],
    /// The key blob.
    blob: &'a [u8],
    /// The signature, when the request carries one.
    signature: Option<&'a [u8]>,
  },
  /// A method the server does not run.
  Other,
}

/// A request read.
struct Request<'a> {
  /// The user name.
  user: &'a [u8],
  /// The service name.
  service: &'a [u8],
  /// The method.
  method: Method<'a>,
}

impl<'a> Request<'a> {
  /// Reads `SSH_MSG_USERAUTH_REQUEST`.
  fn read(payload: &'a [u8]) -> Result<Self, WireRefusal> {
    let mut reader = Reader::new(payload);
    reader.byte()?;
    let user = reader.string()?;
    let service = reader.string()?;
    let method = reader.string()?;
    let method = if method == NONE.as_bytes() {
      reader.finish()?;
      Method::None
    } else if method == PUBLICKEY.as_bytes() {
      let signed = reader.boolean()?;
      let algorithm = reader.string()?;
      let blob = reader.string()?;
      let signature = if signed { Some(reader.string()?) } else { None };
      reader.finish()?;
      Method::PublicKey {
        algorithm,
        blob,
        signature,
      }
    } else {
      Method::Other
    };
    Ok(Self {
      user,
      service,
      method,
    })
  }
}

impl<P: Clone + PartialEq> Authenticator<P> {
  /// An authenticator for the session `session_id`, sending `banner` once and allowing `failure_limit` failures.
  pub fn new(session_id: [u8; HASH_LEN], banner: Option<String>, failure_limit: u32) -> Self {
    Self {
      session_id,
      banner,
      failure_limit,
      failures: 0,
      partial: None,
      done: false,
    }
  }

  /// Whether authentication succeeded.
  pub fn is_done(&self) -> bool {
    self.done
  }

  /// The failures counted so far.
  pub fn failures(&self) -> u32 {
    self.failures
  }

  /// Answers one `SSH_MSG_USERAUTH_REQUEST`.
  pub fn request<H: KeyHolders<Principal = P>>(
    &mut self,
    payload: &[u8],
    holders: &mut H,
  ) -> Result<Answer<P>, AuthRefusal> {
    let mut answer = Answer {
      replies: Vec::new(),
      authenticated: None,
      wanted: None,
    };
    if self.done {
      return Ok(answer);
    }
    let request = Request::read(payload).map_err(AuthRefusal::Malformed)?;
    if let Some(banner) = self.banner.take() {
      answer
        .replies
        .push(banner_message(&banner).map_err(AuthRefusal::Malformed)?);
    }
    if request.service != CONNECTION_SERVICE.as_bytes() {
      return Err(AuthRefusal::ServiceNotAvailable);
    }
    if self
      .partial
      .as_ref()
      .is_some_and(|partial| partial.user != request.user)
    {
      self.partial = None;
    }
    match request.method {
      Method::None => answer.replies.push(failure(false)?),
      Method::PublicKey {
        algorithm,
        blob,
        signature: None,
      } => self.query(request.user, algorithm, blob, holders, &mut answer)?,
      Method::PublicKey {
        algorithm,
        blob,
        signature: Some(signature),
      } => {
        let signed = Signed {
          user: request.user,
          service: request.service,
          algorithm,
          blob,
          signature,
        };
        self.signed(&signed, holders, &mut answer)?;
      }
      Method::Other => answer.replies.push(self.fail()?),
    }
    Ok(answer)
  }

  /// A query: `SSH_MSG_USERAUTH_PK_OK` for a key a principal holds, a counted failure otherwise, and nothing yet for a
  /// key the holders cannot answer for.
  fn query<H: KeyHolders<Principal = P>>(
    &mut self,
    user: &[u8],
    algorithm: &[u8],
    blob: &[u8],
    holders: &mut H,
    answer: &mut Answer<P>,
  ) -> Result<(), AuthRefusal> {
    match held(user, algorithm, blob, holders) {
      Held::Nobody => answer.replies.push(self.fail()?),
      Held::Wanted(wanted) => answer.wanted = Some(wanted),
      Held::Holds(..) => {
        let mut reply = Vec::new();
        put_byte(&mut reply, USERAUTH_PK_OK);
        put_string(&mut reply, algorithm).map_err(AuthRefusal::Malformed)?;
        put_string(&mut reply, blob).map_err(AuthRefusal::Malformed)?;
        answer.replies.push(reply);
      }
    }
    Ok(())
  }

  /// A signed request: success, a partial success, or a counted failure.
  fn signed<H: KeyHolders<Principal = P>>(
    &mut self,
    request: &Signed<'_>,
    holders: &mut H,
    answer: &mut Answer<P>,
  ) -> Result<(), AuthRefusal> {
    let (kind, key, holding) = match held(request.user, request.algorithm, request.blob, holders) {
      Held::Holds(kind, key, holding) => (kind, key, holding),
      Held::Wanted(wanted) => {
        answer.wanted = Some(wanted);
        return Ok(());
      }
      Held::Nobody => {
        answer.replies.push(self.fail()?);
        return Ok(());
      }
    };
    let data = signed_data(&self.session_id, request).map_err(AuthRefusal::Malformed)?;
    if !verify(kind, &key, &data, request.signature) {
      answer.replies.push(self.fail()?);
      return Ok(());
    }
    let Holding::Holds {
      principal,
      keys_required,
    } = holding
    else {
      answer.replies.push(self.fail()?);
      return Ok(());
    };
    let proven = self.prove(request.user, principal.clone(), request.blob);
    if proven < keys_required.max(1) {
      answer.replies.push(failure(true)?);
      return Ok(());
    }
    self.done = true;
    self.partial = None;
    answer.replies.push(vec![USERAUTH_SUCCESS]);
    answer.authenticated = Some(Authenticated {
      user: request.user.to_vec(),
      principal,
    });
    Ok(())
  }

  /// Records that `blob` signed for `principal` under `user`: how many distinct keys of the principal have.
  fn prove(&mut self, user: &[u8], principal: P, blob: &[u8]) -> u32 {
    let same = self
      .partial
      .as_ref()
      .is_some_and(|partial| partial.principal == principal);
    if !same {
      self.partial = Some(Partial {
        user: user.to_vec(),
        principal,
        proven: Vec::new(),
      });
    }
    let Some(partial) = self.partial.as_mut() else {
      return 0;
    };
    if !partial.proven.iter().any(|proven| proven == blob) {
      partial.proven.push(blob.to_vec());
    }
    u32::try_from(partial.proven.len()).unwrap_or(u32::MAX)
  }

  /// Counts a failure and gives its answer, or refuses past the limit.
  fn fail(&mut self) -> Result<Vec<u8>, AuthRefusal> {
    self.failures = self.failures.saturating_add(1);
    if self.failures > self.failure_limit {
      return Err(AuthRefusal::TooManyFailures {
        failures: self.failures,
      });
    }
    failure(false)
  }
}

/// A signed request's fields.
struct Signed<'a> {
  /// The user name.
  user: &'a [u8],
  /// The service name.
  service: &'a [u8],
  /// The algorithm's name.
  algorithm: &'a [u8],
  /// The key blob.
  blob: &'a [u8],
  /// The signature blob.
  signature: &'a [u8],
}

/// What the holders say of a request's key.
enum Held<P> {
  /// The key does not read, or nobody holds it for the user.
  Nobody,
  /// They cannot say yet.
  Wanted(Wanted),
  /// Someone holds it: the signature's algorithm, the key, and who.
  Holds(SignatureAlgorithm, UserKey, Holding<P>),
}

/// The key `blob` for `algorithm` and who holds it for `user`.
fn held<H: KeyHolders>(
  user: &[u8],
  algorithm: &[u8],
  blob: &[u8],
  holders: &mut H,
) -> Held<H::Principal> {
  let Some(kind) = SignatureAlgorithm::from_name(algorithm) else {
    return Held::Nobody;
  };
  let Ok(key) = UserKey::parse(kind, blob) else {
    return Held::Nobody;
  };
  match holders.holding(user, &key, blob) {
    Holding::Nobody => Held::Nobody,
    Holding::Unknown => Held::Nobody,
    holding => Held::Holds(kind, key, holding),
  }
}

/// What the signature of `request` covers (RFC 4252 §7).
fn signed_data(session_id: &[u8], request: &Signed<'_>) -> Result<Vec<u8>, WireRefusal> {
  let mut data = Vec::new();
  put_string(&mut data, session_id)?;
  put_byte(&mut data, USERAUTH_REQUEST);
  put_string(&mut data, request.user)?;
  put_string(&mut data, request.service)?;
  put_string(&mut data, PUBLICKEY.as_bytes())?;
  put_boolean(&mut data, true);
  put_string(&mut data, request.algorithm)?;
  put_string(&mut data, request.blob)?;
  Ok(data)
}

/// Whether `signature`, a signature blob, is `key`'s signature of `data` under `kind`.
fn verify(kind: SignatureAlgorithm, key: &UserKey, data: &[u8], signature: &[u8]) -> bool {
  let mut reader = Reader::new(signature);
  let (Ok(name), Ok(raw)) = (reader.string(), reader.string()) else {
    return false;
  };
  if name != kind.name().as_bytes() || reader.finish().is_err() {
    return false;
  }
  match (kind, key) {
    (SignatureAlgorithm::Ed25519, UserKey::Ed25519 { public }) => {
      raw.len() == ED25519_SIGNATURE_LEN
        && UnparsedPublicKey::new(&ED25519, public)
          .verify(data, raw)
          .is_ok()
    }
    (SignatureAlgorithm::RsaSha512, UserKey::Rsa { exponent, modulus }) => {
      verify_rsa(&RSA_PKCS1_2048_8192_SHA512, exponent, modulus, data, raw)
    }
    (SignatureAlgorithm::RsaSha256, UserKey::Rsa { exponent, modulus }) => {
      verify_rsa(&RSA_PKCS1_2048_8192_SHA256, exponent, modulus, data, raw)
    }
    _ => false,
  }
}

/// An RSA signature check, a short signature padded with leading zeros to the modulus's length (RFC 8332 §3).
fn verify_rsa(
  parameters: &RsaParameters,
  exponent: &[u8],
  modulus: &[u8],
  data: &[u8],
  raw: &[u8],
) -> bool {
  let Some(missing) = modulus.len().checked_sub(raw.len()) else {
    return false;
  };
  let mut padded = vec![0u8; missing];
  padded.extend_from_slice(raw);
  RsaPublicKeyComponents {
    n: modulus,
    e: exponent,
  }
  .verify(parameters, data, &padded)
  .is_ok()
}

/// `SSH_MSG_USERAUTH_FAILURE` naming `publickey`.
fn failure(partial_success: bool) -> Result<Vec<u8>, AuthRefusal> {
  let mut reply = Vec::new();
  put_byte(&mut reply, USERAUTH_FAILURE);
  put_name_list(&mut reply, &[PUBLICKEY]).map_err(AuthRefusal::Malformed)?;
  put_boolean(&mut reply, partial_success);
  Ok(reply)
}

/// `SSH_MSG_USERAUTH_BANNER` with `text` and no language tag.
fn banner_message(text: &str) -> Result<Vec<u8>, WireRefusal> {
  let mut message = Vec::new();
  put_byte(&mut message, USERAUTH_BANNER);
  put_string(&mut message, text.as_bytes())?;
  put_string(&mut message, b"")?;
  Ok(message)
}

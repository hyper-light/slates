//! A request's credentials and the challenges a 401 carries (RFC 9110 §11.6; §4.17.2 "Access": "HTTP: Basic with a
//! token as the password, or a bearer token, over TLS").
//!
//! **Read** ([`credential`]): the one `Authorization` line a request carries, or none. `Basic` (RFC 7617 §2) is a user
//! id and a password joined by the first colon, in base64 (RFC 4648 §4) read strictly: whole quanta, padding at the end
//! alone, no bits past the last byte, so one text names one credential. `Bearer` (RFC 6750 §2.1) is a `b64token`. The
//! scheme is compared without regard to case (RFC 9110 §11.1). Basic is refused on a connection that is not TLS (RFC
//! 7617 §4: "the Basic authentication scheme is not a secure method of user authentication"), Bearer accepted there only
//! because a cleartext listener is a loopback one.
//!
//! **What they prove** is the site's to decide: its authenticator maps a credential to a principal of §4.13 and its
//! policy decides what that principal may do (§4.17.1). This module reads the bytes and offers the challenges
//! ([`challenges`]): Basic with its charset (RFC 7617 §2.1) over TLS alone, and Bearer (RFC 6750 §3), each naming the
//! realm, as "A server generating a 401 (Unauthorized) response MUST send a WWW-Authenticate header field containing at
//! least one challenge" (RFC 9110 §11.6.1). A secret is compared in time independent of where it differs
//! ([`same_secret`]).

use slates_http::head::Head;
use slates_http::reply::Status;

/// Format: the field a 401's challenges are sent in (RFC 9110 §11.6.1).
pub const WWW_AUTHENTICATE: &[u8] = b"WWW-Authenticate";

/// A request's credentials, as the edge reads them.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
  /// `Basic` (RFC 7617): a user id and a password, the password a token issued to the user.
  Basic {
    /// The user id, before the first colon.
    user: Vec<u8>,
    /// The password, after it.
    password: Vec<u8>,
  },
  /// `Bearer` (RFC 6750): a token.
  Bearer(Vec<u8>),
}

impl std::fmt::Debug for Credential {
  /// A credential's scheme and user, never its secret: a log that shows one must not show the token.
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Credential::Basic { user, .. } => formatter
        .debug_struct("Basic")
        .field("user", &String::from_utf8_lossy(user))
        .finish_non_exhaustive(),
      Credential::Bearer(_) => formatter.debug_struct("Bearer").finish_non_exhaustive(),
    }
  }
}

/// Why a request's credentials are refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialRefused {
  /// More than one `Authorization` line: `400`.
  Repeated,
  /// A Basic or Bearer credential that breaks its scheme's syntax (RFC 6750 §3.1 `invalid_request`): `400`.
  Malformed,
  /// A scheme other than Basic and Bearer: `401` with the challenges this connection accepts.
  UnknownScheme,
  /// Basic on a connection that is not TLS: `401` with the challenges it accepts, which offer no Basic.
  BasicInCleartext,
}

impl CredentialRefused {
  /// The status the refusal is answered with.
  pub fn status(self) -> Status {
    match self {
      CredentialRefused::Repeated | CredentialRefused::Malformed => Status::BAD_REQUEST,
      CredentialRefused::UnknownScheme | CredentialRefused::BasicInCleartext => {
        Status::UNAUTHORIZED
      }
    }
  }
}

/// A realm a challenge cannot carry: it holds a control byte, which a quoted string cannot (RFC 9110 §5.6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadRealm;

/// Format: the field a request's credentials come in (RFC 9110 §11.6.2), compared without regard to case.
const AUTHORIZATION: &[u8] = b"authorization";

/// Format: the schemes read (RFC 7617 §2, RFC 6750 §2.1), compared without regard to case (RFC 9110 §11.1).
const BASIC: &[u8] = b"Basic";
/// Format: as [`BASIC`].
const BEARER: &[u8] = b"Bearer";

/// Format: base64's alphabet (RFC 4648 §4, Table 1), each character's value its place.
const BASE64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// Format: base64's pad character (RFC 4648 §4).
const PAD: u8 = b'=';
/// Format: the characters of one base64 quantum, and the bits each holds (RFC 4648 §4: a 24-bit group as "4 encoded
/// characters", each "6 bits").
const QUANTUM_CHARS: usize = 4;
/// Format: as [`QUANTUM_CHARS`].
const SEXTET_BITS: u32 = 6;
/// Format: the most pad characters a quantum ends with (RFC 4648 §4: "=" or "==").
const MOST_PADDING: usize = 2;

/// Format: a `b64token`'s characters besides letters and digits (RFC 6750 §2.1: `1*( ALPHA / DIGIT / "-" / "." / "_" /
/// "~" / "+" / "/" ) *"="`).
const TOKEN_MARKS: &[u8] = b"-._~+/";

/// Format: what joins a Basic credential's user id and password (RFC 7617 §2: `user-pass = user-id ":" password`).
const USER_PASS_SEPARATOR: u8 = b':';

/// Format: what a quoted string escapes, and how (RFC 9110 §5.6.4: `quoted-pair = "\" ( HTAB / SP / VCHAR / obs-text
/// )`, the quote and the backslash the two that must be).
const ESCAPE: u8 = b'\\';
/// Format: as [`ESCAPE`].
const QUOTE: u8 = b'"';

/// Reads the credentials `head` carries: none, or its one `Authorization` line's. `secure` says whether the connection
/// is TLS, which Basic requires.
pub fn credential(head: &Head<'_>, secure: bool) -> Result<Option<Credential>, CredentialRefused> {
  let Some(value) = head
    .field(AUTHORIZATION)
    .map_err(|_| CredentialRefused::Repeated)?
  else {
    return Ok(None);
  };
  let (scheme, rest) = split_scheme(value);
  if scheme.eq_ignore_ascii_case(BASIC) {
    if !secure {
      return Err(CredentialRefused::BasicInCleartext);
    }
    basic(rest).map(Some)
  } else if scheme.eq_ignore_ascii_case(BEARER) {
    bearer(rest).map(Some)
  } else if scheme.is_empty() {
    Err(CredentialRefused::Malformed)
  } else {
    Err(CredentialRefused::UnknownScheme)
  }
}

/// A credential's scheme and what follows it past its spaces (RFC 9110 §11.4: `credentials = auth-scheme [ 1*SP (
/// token68 / #auth-param ) ]`).
fn split_scheme(value: &[u8]) -> (&[u8], &[u8]) {
  match value.iter().position(|byte| *byte == b' ') {
    Some(at) => {
      let rest = value.get(at..).unwrap_or_default();
      let start = rest
        .iter()
        .position(|byte| *byte != b' ')
        .unwrap_or(rest.len());
      (
        value.get(..at).unwrap_or_default(),
        rest.get(start..).unwrap_or_default(),
      )
    }
    None => (value, value.get(value.len()..).unwrap_or_default()),
  }
}

/// A Basic credential: base64 read strictly, split at its first colon, neither part holding a control byte (RFC 7617
/// §2: "The user-id and password MUST NOT contain any control characters").
fn basic(text: &[u8]) -> Result<Credential, CredentialRefused> {
  let decoded = decode_base64(text).ok_or(CredentialRefused::Malformed)?;
  let colon = decoded
    .iter()
    .position(|byte| *byte == USER_PASS_SEPARATOR)
    .ok_or(CredentialRefused::Malformed)?;
  let user = decoded.get(..colon).unwrap_or_default();
  let password = decoded.get(colon.saturating_add(1)..).unwrap_or_default();
  if user.iter().chain(password).any(u8::is_ascii_control) {
    return Err(CredentialRefused::Malformed);
  }
  Ok(Credential::Basic {
    user: user.to_vec(),
    password: password.to_vec(),
  })
}

/// A Bearer credential: a `b64token`, at least one character before its padding.
fn bearer(text: &[u8]) -> Result<Credential, CredentialRefused> {
  let padding_from = text
    .iter()
    .position(|byte| *byte == PAD)
    .unwrap_or(text.len());
  let (token, padding) = (
    text.get(..padding_from).unwrap_or_default(),
    text.get(padding_from..).unwrap_or_default(),
  );
  let well_formed = !token.is_empty()
    && token
      .iter()
      .all(|byte| byte.is_ascii_alphanumeric() || TOKEN_MARKS.contains(byte))
    && padding.iter().all(|byte| *byte == PAD);
  if well_formed {
    Ok(Credential::Bearer(text.to_vec()))
  } else {
    Err(CredentialRefused::Malformed)
  }
}

/// `text` decoded from base64 (RFC 4648 §4), strictly: whole quanta, at least one; padding at the end alone, at most
/// two; and the bits past the last byte zero, so one text names one credential. `None` otherwise.
fn decode_base64(text: &[u8]) -> Option<Vec<u8>> {
  let (quanta, rest) = text.as_chunks::<QUANTUM_CHARS>();
  let padding = text.iter().rev().take_while(|byte| **byte == PAD).count();
  let body = text.get(..text.len().saturating_sub(padding))?;
  if !rest.is_empty() || quanta.is_empty() || padding > MOST_PADDING || body.contains(&PAD) {
    return None;
  }
  let mut out = Vec::new();
  for quantum in quanta {
    let value = quantum.iter().try_fold(0u32, |value, character| {
      let sextet = if *character == PAD {
        Some(0)
      } else {
        sextet(*character)
      }?;
      Some(value.wrapping_shl(SEXTET_BITS) | sextet)
    })?;
    let [_, high, middle, low] = value.to_be_bytes();
    out.extend_from_slice(&[high, middle, low]);
  }
  let dropped = out.split_off(out.len().saturating_sub(padding));
  dropped.iter().all(|byte| *byte == 0).then_some(out)
}

/// The value of one base64 character.
fn sextet(character: u8) -> Option<u32> {
  BASE64
    .iter()
    .position(|known| *known == character)
    .and_then(|place| u32::try_from(place).ok())
}

/// The `WWW-Authenticate` values a 401 on this connection carries: Basic with its charset over TLS alone, and Bearer,
/// each naming `realm` as a quoted string.
pub fn challenges(realm: &str, secure: bool) -> Result<Vec<Vec<u8>>, BadRealm> {
  let quoted = quoted(realm.as_bytes())?;
  let mut values = Vec::new();
  if secure {
    let mut basic = b"Basic realm=".to_vec();
    basic.extend_from_slice(&quoted);
    basic.extend_from_slice(b", charset=\"UTF-8\"");
    values.push(basic);
  }
  let mut bearer = b"Bearer realm=".to_vec();
  bearer.extend_from_slice(&quoted);
  values.push(bearer);
  Ok(values)
}

/// `text` as a quoted string, its quotes and backslashes escaped; refused when it holds a control byte other than HTAB.
fn quoted(text: &[u8]) -> Result<Vec<u8>, BadRealm> {
  if text
    .iter()
    .any(|byte| *byte != b'\t' && byte.is_ascii_control())
  {
    return Err(BadRealm);
  }
  let mut out = vec![QUOTE];
  for byte in text {
    if *byte == QUOTE || *byte == ESCAPE {
      out.push(ESCAPE);
    }
    out.push(*byte);
  }
  out.push(QUOTE);
  Ok(out)
}

/// Whether two secrets are equal, compared byte by byte to their end, so the time taken does not say where they differ
/// (their lengths, which a token's format fixes, are compared first).
pub fn same_secret(left: &[u8], right: &[u8]) -> bool {
  left.len() == right.len()
    && left
      .iter()
      .zip(right)
      .fold(0u8, |difference, (left, right)| difference | (left ^ right))
      == 0
}

#[cfg(test)]
#[path = "tests/credential.rs"]
mod tests;

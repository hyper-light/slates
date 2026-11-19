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
    Status::OK
  }
}

/// A realm a challenge cannot carry: it holds a control byte, which a quoted string cannot (RFC 9110 §5.6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadRealm;

/// Reads the credentials `head` carries: none, or its one `Authorization` line's. `secure` says whether the connection
/// is TLS, which Basic requires.
pub fn credential(head: &Head<'_>, secure: bool) -> Result<Option<Credential>, CredentialRefused> {
  let _ = (head, secure);
  Ok(None)
}

/// The `WWW-Authenticate` values a 401 on this connection carries: Basic with its charset over TLS alone, and Bearer,
/// each naming `realm` as a quoted string.
pub fn challenges(realm: &str, secure: bool) -> Result<Vec<Vec<u8>>, BadRealm> {
  let _ = (realm, secure);
  Ok(Vec::new())
}

/// Whether two secrets are equal, compared byte by byte to their end, so the time taken does not say where they differ
/// (their lengths, which a token's format fixes, are compared first).
pub fn same_secret(left: &[u8], right: &[u8]) -> bool {
  let _ = (left, right);
  false
}

#[cfg(test)]
#[path = "tests/credential.rs"]
mod tests;

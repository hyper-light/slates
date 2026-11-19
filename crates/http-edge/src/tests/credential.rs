use slates_http::Admission;
use slates_http::head::{Head, HeadReader, Progress};
use slates_http::reply::Status;

use super::{BadRealm, Credential, CredentialRefused, challenges, credential, same_secret};

/// A request head carrying `fields` after its `Host`.
fn request(fields: &[&str]) -> Vec<u8> {
  let mut out =
    String::from("GET /r.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: h\r\n");
  for field in fields {
    out.push_str(field);
    out.push_str("\r\n");
  }
  out.push_str("\r\n");
  out.into_bytes()
}

/// The head `bytes` hold whole.
fn head(bytes: &[u8]) -> Head<'_> {
  match HeadReader::new().read(bytes, &mut Admission::new(1 << 20)) {
    Ok(Progress::Complete { value, .. }) => value,
    other => panic!("{other:?}"),
  }
}

/// The credentials a request with `fields` carries, on a connection `secure` or not.
fn read(fields: &[&str], secure: bool) -> Result<Option<Credential>, CredentialRefused> {
  let bytes = request(fields);
  credential(&head(&bytes), secure)
}

/// A Basic credential of `user` and `password`.
fn basic(user: &[u8], password: &[u8]) -> Credential {
  Credential::Basic {
    user: user.to_vec(),
    password: password.to_vec(),
  }
}

/// A-625: do read RFC 7617 §2's own example, its scheme in another case, and a password holding a colon, over TLS.
/// Expect the user id before the first colon and the password after it.
#[test]
fn basic_over_tls_reads_rfc_7617s_own_example() {
  let cases: [(&str, Credential); 3] = [
    (
      "Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==",
      basic(b"Aladdin", b"open sesame"),
    ),
    (
      "authorization: bAsIc QWxhZGRpbjpvcGVuIHNlc2FtZQ==",
      basic(b"Aladdin", b"open sesame"),
    ),
    (
      "Authorization: Basic dXNlcjpwYTpzcw==",
      basic(b"user", b"pa:ss"),
    ),
  ];
  for (field, expected) in cases {
    assert_eq!(read(&[field], true), Ok(Some(expected)), "{field}");
  }
}

/// A-625: do read RFC 6750 §2.1's own example, a token with its padding, over TLS and over a loopback cleartext
/// connection. Expect the token each time.
#[test]
fn bearer_reads_its_token_over_tls_and_loopback() {
  for secure in [true, false] {
    assert_eq!(
      read(&["Authorization: Bearer mF_9.B5f-4.1JqM"], secure),
      Ok(Some(Credential::Bearer(b"mF_9.B5f-4.1JqM".to_vec())))
    );
    assert_eq!(
      read(&["Authorization: Bearer ab+/cd=="], secure),
      Ok(Some(Credential::Bearer(b"ab+/cd==".to_vec())))
    );
  }
}

/// A-625: do read a request with no `Authorization`. Expect no credentials, whatever the connection.
#[test]
fn a_request_without_authorization_carries_none() {
  assert_eq!(read(&[], true), Ok(None));
  assert_eq!(read(&["Accept: */*"], false), Ok(None));
}

/// A-625: do send every malformed Basic and Bearer credential: no token, a byte outside base64, a cut quantum, padding
/// first or inside, bits past the last byte, no colon, an empty or spaced or comma-joined bearer, `=` inside one.
/// Expect each refused as malformed, `400`.
#[test]
fn malformed_credentials_are_refused_400() {
  let cases = [
    "Authorization: Basic",
    "Authorization: Basic !!!!",
    "Authorization: Basic QWxhZGRpbg",
    "Authorization: Basic =QWxh",
    "Authorization: Basic QWx=ZA==",
    "Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZR==",
    "Authorization: Basic QWxhZGRpbg==",
    "Authorization: Bearer",
    "Authorization: Bearer a b",
    "Authorization: Bearer ab=c",
    "Authorization: Bearer a,b",
  ];
  for field in cases {
    assert_eq!(
      read(&[field], true),
      Err(CredentialRefused::Malformed),
      "{field}"
    );
  }
  assert_eq!(CredentialRefused::Malformed.status(), Status::BAD_REQUEST);
}

/// A-625: do send schemes this edge does not take. Expect each refused as unknown, `401`, so the client asks again with
/// one the challenge offers.
#[test]
fn an_unknown_scheme_is_refused_401() {
  for field in [
    "Authorization: Digest username=\"u\"",
    "Authorization: Negotiate YII=",
  ] {
    assert_eq!(
      read(&[field], true),
      Err(CredentialRefused::UnknownScheme),
      "{field}"
    );
  }
  assert_eq!(
    CredentialRefused::UnknownScheme.status(),
    Status::UNAUTHORIZED
  );
}

/// A-625: do send two `Authorization` lines. Expect the request refused as ambiguous, `400`.
#[test]
fn a_repeated_authorization_is_refused_400() {
  let fields = ["Authorization: Bearer abc", "Authorization: Bearer def"];
  assert_eq!(read(&fields, true), Err(CredentialRefused::Repeated));
  assert_eq!(CredentialRefused::Repeated.status(), Status::BAD_REQUEST);
}

/// A-625: do send Basic over a cleartext connection. Expect it refused, `401`, whatever it names (RFC 7617 §4).
#[test]
fn basic_in_cleartext_is_refused_401() {
  assert_eq!(
    read(
      &["Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="],
      false
    ),
    Err(CredentialRefused::BasicInCleartext)
  );
  assert_eq!(
    CredentialRefused::BasicInCleartext.status(),
    Status::UNAUTHORIZED
  );
}

/// A-625: do ask for the challenges over TLS and in cleartext, and with a realm holding a quote and a backslash, and one
/// holding a control byte. Expect Basic with its charset and Bearer over TLS, Bearer alone in cleartext, the realm a
/// quoted string with its quote and backslash escaped, and the control byte refused.
#[test]
fn challenges_name_the_realm_and_offer_basic_over_tls_alone() {
  assert_eq!(
    challenges("slates", true),
    Ok(vec![
      b"Basic realm=\"slates\", charset=\"UTF-8\"".to_vec(),
      b"Bearer realm=\"slates\"".to_vec(),
    ])
  );
  assert_eq!(
    challenges("slates", false),
    Ok(vec![b"Bearer realm=\"slates\"".to_vec()])
  );
  assert_eq!(
    challenges("a\"b\\c", false),
    Ok(vec![b"Bearer realm=\"a\\\"b\\\\c\"".to_vec()])
  );
  assert_eq!(challenges("a\nb", true), Err(BadRealm));
}

/// A-625: do compare secrets equal, differing at their last byte, one the other's prefix, and both empty. Expect equal
/// only when every byte and the length agree.
#[test]
fn secrets_are_equal_only_whole() {
  assert!(same_secret(b"0123456789abcdef", b"0123456789abcdef"));
  assert!(!same_secret(b"0123456789abcdef", b"0123456789abcdeF"));
  assert!(!same_secret(b"0123456789abcdef", b"0123456789abcde"));
  assert!(same_secret(b"", b""));
}

/// A-625: do format a Basic credential and a bearer for a log. Expect the scheme and the user, never a secret.
#[test]
fn a_credential_never_shows_its_secret() {
  let shown = format!(
    "{:?} {:?}",
    basic(b"ada", b"s3cr3t"),
    Credential::Bearer(b"t0k3n".to_vec())
  );
  assert!(shown.contains("ada"), "{shown}");
  assert!(
    !shown.contains("s3cr3t") && !shown.contains("t0k3n"),
    "{shown}"
  );
}

//! User authentication (A-633; RFC 4252, RFC 8332, RFC 8709): the authenticator against a key store of the tests' own,
//! its requests and signatures built here from RFC 4252 §7's layout, and an RSA key in OpenSSH's own encoding.

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{
  Ed25519KeyPair, KeyPair, RSA_PKCS1_SHA256, RSA_PKCS1_SHA512, RsaEncoding, RsaKeyPair,
};

use crate::client::put_string;
use crate::support::{base64, hex_array};
use crate::userauth::{
  Answer, AuthRefusal, Authenticated, Authenticator, Holding, KeyHolders, KeyRefusal,
  SignatureAlgorithm, UserKey, fingerprint,
};
use crate::wire::WireRefusal;

/// The session id the tests' signatures cover.
const SESSION_ID: [u8; 32] = [0x5e; 32];
/// RFC 8032 §7.1 test 2's secret key, a user's key here.
const TEST2_SEED: &str = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";
/// RFC 8032 §7.1 test 3's secret key.
const TEST3_SEED: &str = "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7";

/// The tests' key store: who holds which key blob for which user, and how many keys the principal wants.
pub(crate) struct Holders {
  pub(crate) entries: Vec<(Vec<u8>, Vec<u8>, u32, u32)>,
  /// Whether the holders cannot say yet of any key (A-651's deferred lookup).
  pub(crate) unknown: bool,
}

impl KeyHolders for Holders {
  type Principal = u32;

  fn holding(&mut self, user: &[u8], _key: &UserKey, blob: &[u8]) -> Holding<u32> {
    if self.unknown {
      return Holding::Unknown;
    }
    let found = self
      .entries
      .iter()
      .find(|(holder, held, _, _)| holder == user && held == blob);
    found.map_or(Holding::Nobody, |(_, _, principal, required)| {
      Holding::Holds {
        principal: *principal,
        keys_required: *required,
      }
    })
  }
}

/// An Ed25519 user key and its blob.
pub(crate) struct EdKey {
  pub(crate) pair: Ed25519KeyPair,
  pub(crate) blob: Vec<u8>,
}

/// The Ed25519 key with secret `seed`.
pub(crate) fn ed_key(seed: &str) -> EdKey {
  let pair = Ed25519KeyPair::from_seed_unchecked(&hex_array::<32>(seed)).unwrap();
  let mut blob = Vec::new();
  put_string(&mut blob, b"ssh-ed25519");
  put_string(&mut blob, pair.public_key().as_ref());
  EdKey { pair, blob }
}

/// RFC 8032 test 2's key, the first user key of the tests.
pub(crate) fn first_key() -> EdKey {
  ed_key(TEST2_SEED)
}

/// RFC 8032 test 3's key.
fn second_key() -> EdKey {
  ed_key(TEST3_SEED)
}

/// The OpenSSH fixture's RSA key: its blob, from ssh-keygen's public key line, and its key pair, from PKCS#8.
fn rsa_key() -> (Vec<u8>, RsaKeyPair) {
  let line = include_str!("../../fixtures/userauth/user_rsa.pub");
  let blob = base64(line.split_whitespace().nth(1).unwrap());
  let der = base64(include_str!("../../fixtures/userauth/user_rsa.pkcs8.b64"));
  (blob, RsaKeyPair::from_pkcs8(&der).unwrap())
}

/// RFC 4252 §5's request for `user` and `ssh-connection` by `method`.
pub(crate) fn request(user: &[u8], method: &str) -> Vec<u8> {
  let mut payload = vec![50];
  put_string(&mut payload, user);
  put_string(&mut payload, b"ssh-connection");
  put_string(&mut payload, method.as_bytes());
  payload
}

/// A query: `publickey`, no signature (RFC 4252 §7).
pub(crate) fn query(user: &[u8], algorithm: &str, blob: &[u8]) -> Vec<u8> {
  let mut payload = request(user, "publickey");
  payload.push(0);
  put_string(&mut payload, algorithm.as_bytes());
  put_string(&mut payload, blob);
  payload
}

/// What a signature covers (RFC 4252 §7).
pub(crate) fn signed_data(session_id: &[u8], user: &[u8], algorithm: &str, blob: &[u8]) -> Vec<u8> {
  let mut data = Vec::new();
  put_string(&mut data, session_id);
  data.push(50);
  put_string(&mut data, user);
  put_string(&mut data, b"ssh-connection");
  put_string(&mut data, b"publickey");
  data.push(1);
  put_string(&mut data, algorithm.as_bytes());
  put_string(&mut data, blob);
  data
}

/// A signed request carrying `signature`, a signature blob.
pub(crate) fn signed(user: &[u8], algorithm: &str, blob: &[u8], signature: &[u8]) -> Vec<u8> {
  let mut payload = request(user, "publickey");
  payload.push(1);
  put_string(&mut payload, algorithm.as_bytes());
  put_string(&mut payload, blob);
  put_string(&mut payload, signature);
  payload
}

/// `key`'s signature blob over `user`'s request under `session_id`.
pub(crate) fn ed_signature(key: &EdKey, session_id: &[u8], user: &[u8]) -> Vec<u8> {
  let raw = key
    .pair
    .sign(&signed_data(session_id, user, "ssh-ed25519", &key.blob));
  signature_blob("ssh-ed25519", raw.as_ref())
}

/// `string algorithm, string raw`.
fn signature_blob(algorithm: &str, raw: &[u8]) -> Vec<u8> {
  let mut blob = Vec::new();
  put_string(&mut blob, algorithm.as_bytes());
  put_string(&mut blob, raw);
  blob
}

/// The RSA key's raw PKCS#1 v1.5 signature over `data` with `encoding`.
fn rsa_raw(pair: &RsaKeyPair, encoding: &'static dyn RsaEncoding, data: &[u8]) -> Vec<u8> {
  let mut raw = vec![0u8; pair.public_modulus_len()];
  pair
    .sign(encoding, &SystemRandom::new(), data, &mut raw)
    .unwrap();
  raw
}

/// `SSH_MSG_USERAUTH_FAILURE` naming `publickey`.
pub(crate) fn failure(partial: bool) -> Vec<u8> {
  let mut reply = vec![51];
  put_string(&mut reply, b"publickey");
  reply.push(u8::from(partial));
  reply
}

/// A store where `user` holds `blob` alone as principal 7.
fn holding(user: &[u8], blob: &[u8]) -> Holders {
  Holders {
    entries: vec![(user.to_vec(), blob.to_vec(), 7, 1)],
    unknown: false,
  }
}

/// An authenticator over the tests' session id, with no banner and a limit no test reaches unless it means to.
fn authenticator(limit: u32) -> Authenticator<u32> {
  Authenticator::new(SESSION_ID, None, limit)
}

/// An answer that only replies.
fn replies(replies: Vec<Vec<u8>>) -> Answer<u32> {
  Answer {
    replies,
    authenticated: None,
    wanted: None,
  }
}

/// A-633 (RFC 4252 §5.2, §5.4): do probe with `none` twice under a banner and a failure limit of zero. Expect the banner
/// once, before the first answer, each probe answered with a failure naming `publickey`, and no failure counted, so a
/// limit of zero still lets the client learn the methods.
#[test]
fn the_none_probe_names_publickey_and_counts_nothing() {
  let mut auth = Authenticator::<u32>::new(SESSION_ID, Some("slates: git over SSH".to_string()), 0);
  let mut store = Holders {
    entries: Vec::new(),
    unknown: false,
  };
  let mut banner = vec![53];
  put_string(&mut banner, b"slates: git over SSH");
  put_string(&mut banner, b"");
  let first = auth.request(&request(b"git", "none"), &mut store);
  let second = auth.request(&request(b"git", "none"), &mut store);
  assert_eq!(
    (first, second),
    (
      Ok(replies(vec![banner, failure(false)])),
      Ok(replies(vec![failure(false)]))
    )
  );
  assert_eq!(auth.failures(), 0);
}

/// A-633 (RFC 4252 §7): do query a key the store holds and one it does not. Expect `SSH_MSG_USERAUTH_PK_OK` echoing the
/// algorithm and blob for the first, and a counted failure for the second.
#[test]
fn a_query_is_answered_pk_ok_only_for_a_held_key() {
  let key = first_key();
  let mut store = holding(b"git", &key.blob);
  let mut auth = authenticator(5);
  let mut ok = vec![60];
  put_string(&mut ok, b"ssh-ed25519");
  put_string(&mut ok, &key.blob);
  let held = auth.request(&query(b"git", "ssh-ed25519", &key.blob), &mut store);
  let other = auth.request(
    &query(b"git", "ssh-ed25519", &second_key().blob),
    &mut store,
  );
  assert_eq!(
    (held, other),
    (Ok(replies(vec![ok])), Ok(replies(vec![failure(false)])))
  );
  assert_eq!(auth.failures(), 1);
}

/// A-633 (RFC 4252 §5.1, §7): do sign a request with a held Ed25519 key, then send another request. Expect success
/// naming the user and the principal, and the later request dropped without a reply.
#[test]
fn a_signature_over_the_session_proves_its_principal() {
  let key = first_key();
  let mut store = holding(b"git", &key.blob);
  let mut auth = authenticator(5);
  let proof = signed(
    b"git",
    "ssh-ed25519",
    &key.blob,
    &ed_signature(&key, &SESSION_ID, b"git"),
  );
  let success = Answer {
    replies: vec![vec![52]],
    authenticated: Some(Authenticated {
      user: b"git".to_vec(),
      principal: 7,
    }),
    wanted: None,
  };
  assert_eq!(auth.request(&proof, &mut store), Ok(success));
  assert_eq!(
    auth.request(&request(b"git", "none"), &mut store),
    Ok(replies(Vec::new()))
  );
  assert!(auth.is_done());
}

/// A-633 hostile (RFC 4252 §7: "MUST check that the signature is valid"; RFC 8332 §3.2): do sign with a bit flipped,
/// over another session id, and with a signature blob naming another algorithm. Expect each a counted failure.
#[test]
fn forged_or_misplaced_signatures_fail() {
  let key = first_key();
  let mut flipped = ed_signature(&key, &SESSION_ID, b"git");
  if let Some(last) = flipped.last_mut() {
    *last ^= 0x01;
  }
  let elsewhere = ed_signature(&key, &[0x11; 32], b"git");
  let raw = key
    .pair
    .sign(&signed_data(&SESSION_ID, b"git", "ssh-ed25519", &key.blob));
  let renamed = signature_blob("rsa-sha2-256", raw.as_ref());
  let mut auth = authenticator(5);
  let mut store = holding(b"git", &key.blob);
  for signature in [flipped, elsewhere, renamed] {
    let answer = auth.request(
      &signed(b"git", "ssh-ed25519", &key.blob, &signature),
      &mut store,
    );
    assert_eq!(answer, Ok(replies(vec![failure(false)])));
  }
  assert_eq!(auth.failures(), 3);
}

/// A-633 golden (RFC 8332 §3.2; an RSA 3072 key OpenSSH 10.2p1 made, `fixtures/userauth`): do sign requests with the
/// key under `rsa-sha2-256` and `rsa-sha2-512`, each read from OpenSSH's own blob. Expect each to authenticate.
#[test]
fn an_openssh_rsa_key_signs_with_sha2() {
  let (blob, pair) = rsa_key();
  let encodings: [(&str, &'static dyn RsaEncoding); 2] = [
    ("rsa-sha2-256", &RSA_PKCS1_SHA256),
    ("rsa-sha2-512", &RSA_PKCS1_SHA512),
  ];
  for (algorithm, encoding) in encodings {
    let raw = rsa_raw(
      &pair,
      encoding,
      &signed_data(&SESSION_ID, b"git", algorithm, &blob),
    );
    let request = signed(b"git", algorithm, &blob, &signature_blob(algorithm, &raw));
    let answer = authenticator(5).request(&request, &mut holding(b"git", &blob));
    assert_eq!(
      answer.map(|answer| answer.authenticated.is_some()),
      Ok(true),
      "{algorithm}"
    );
  }
}

/// A-633 (RFC 8332 §3.2, "the client MUST encode the same algorithm name in the signature"; OpenSSH 8.8's refusal of
/// SHA-1 RSA): do sign with `ssh-rsa`, and with a `rsa-sha2-256` signature in a `rsa-sha2-512` request. Expect both
/// refused as failures.
#[test]
fn sha1_rsa_and_a_renamed_rsa_signature_fail() {
  let (blob, pair) = rsa_key();
  let raw = rsa_raw(
    &pair,
    &RSA_PKCS1_SHA256,
    &signed_data(&SESSION_ID, b"git", "rsa-sha2-512", &blob),
  );
  let renamed = signed(
    b"git",
    "rsa-sha2-512",
    &blob,
    &signature_blob("rsa-sha2-256", &raw),
  );
  let sha1 = signed(b"git", "ssh-rsa", &blob, &signature_blob("ssh-rsa", &raw));
  let mut auth = authenticator(5);
  let mut store = holding(b"git", &blob);
  let answers = (
    auth.request(&renamed, &mut store),
    auth.request(&sha1, &mut store),
  );
  assert_eq!(
    answers,
    (
      Ok(replies(vec![failure(false)])),
      Ok(replies(vec![failure(false)]))
    )
  );
}

/// A-633 (RFC 8332 §3: "A verifier MAY accept shorter encodings of S with one or more leading zeros omitted"): do find
/// a session id under which the RSA key's signature begins with a zero byte, and send it without that byte. Expect
/// authentication, the signature padded back to the modulus.
#[test]
fn a_short_rsa_signature_is_padded_back() {
  let (blob, pair) = rsa_key();
  let short = (0u16..8_192).find_map(|seed| {
    let mut session_id = [0u8; 32];
    session_id[..2].copy_from_slice(&seed.to_be_bytes());
    let raw = rsa_raw(
      &pair,
      &RSA_PKCS1_SHA256,
      &signed_data(&session_id, b"git", "rsa-sha2-256", &blob),
    );
    (raw.first() == Some(&0)).then(|| (session_id, raw[1..].to_vec()))
  });
  let (session_id, raw) = short.expect("one signature in 256 begins with a zero byte");
  let request = signed(
    b"git",
    "rsa-sha2-256",
    &blob,
    &signature_blob("rsa-sha2-256", &raw),
  );
  let answer =
    Authenticator::new(session_id, None, 5).request(&request, &mut holding(b"git", &blob));
  assert_eq!(
    answer.map(|answer| answer.authenticated.is_some()),
    Ok(true)
  );
}

/// A-633 golden (`ssh-keygen -l -E sha256` on the fixture, OpenSSH 10.2p1): do fingerprint the RSA key's blob. Expect
/// OpenSSH's fingerprint.
#[test]
fn the_fingerprint_is_openssh_s() {
  let (blob, _) = rsa_key();
  assert_eq!(
    fingerprint(&blob),
    "SHA256:Wymbjagh5qjwNh2R9f44I8A1HSB7TrTf6LdcnH26LQ8"
  );
}

/// A-633 (RFC 4252 §5.1: "partial success MUST be TRUE if the authentication request ... was successful"): do sign for a
/// principal that wants two keys, with one key twice and then the other. Expect partial success twice, the same key
/// not counted twice, then success, and no failure counted.
#[test]
fn partial_success_until_enough_distinct_keys_sign() {
  let (first, second) = (first_key(), second_key());
  let mut store = Holders {
    entries: vec![
      (b"ada".to_vec(), first.blob.clone(), 9, 2),
      (b"ada".to_vec(), second.blob.clone(), 9, 2),
    ],
    unknown: false,
  };
  let mut auth = authenticator(0);
  let by = |key: &EdKey| {
    signed(
      b"ada",
      "ssh-ed25519",
      &key.blob,
      &ed_signature(key, &SESSION_ID, b"ada"),
    )
  };
  let once = auth.request(&by(&first), &mut store);
  let again = auth.request(&by(&first), &mut store);
  let both = auth
    .request(&by(&second), &mut store)
    .map(|answer| answer.authenticated);
  let partial = Ok(replies(vec![failure(true)]));
  let proven = Ok(Some(Authenticated {
    user: b"ada".to_vec(),
    principal: 9,
  }));
  assert_eq!((once, again, both), (partial.clone(), partial, proven));
}

/// A-633 (RFC 4252 §5: "MUST flush any accumulated authentication states if they change"): do sign one key as `alice`,
/// the other as `bob`, then the first as `bob`, for a principal that wants two keys under both names. Expect the change
/// of name to drop alice's key, so bob needs both of his.
#[test]
fn a_new_user_name_drops_what_was_proven() {
  let (first, second) = (first_key(), second_key());
  let mut store = Holders {
    entries: Vec::new(),
    unknown: false,
  };
  for user in [&b"alice"[..], b"bob"] {
    store
      .entries
      .push((user.to_vec(), first.blob.clone(), 9, 2));
    store
      .entries
      .push((user.to_vec(), second.blob.clone(), 9, 2));
  }
  let mut auth = authenticator(0);
  let by = |key: &EdKey, user: &[u8]| {
    signed(
      user,
      "ssh-ed25519",
      &key.blob,
      &ed_signature(key, &SESSION_ID, user),
    )
  };
  let alice = auth.request(&by(&first, b"alice"), &mut store);
  let bob = auth.request(&by(&second, b"bob"), &mut store);
  let bob_again = auth
    .request(&by(&first, b"bob"), &mut store)
    .map(|answer| answer.authenticated.is_some());
  let partial = Ok(replies(vec![failure(true)]));
  assert_eq!(
    (alice, bob, bob_again),
    (partial.clone(), partial, Ok(true))
  );
}

/// A-633 hostile (RFC 4252 §4, "SHOULD limit the number of failed authentication attempts"): do fail twice under a limit
/// of two (a query for a key nobody holds, a method the server does not run), then once more. Expect the third refused
/// as too many failures.
#[test]
fn failures_past_the_limit_are_refused() {
  let mut auth = authenticator(2);
  let mut store = Holders {
    entries: Vec::new(),
    unknown: false,
  };
  let unknown = query(b"git", "ssh-ed25519", &first_key().blob);
  let first = auth.request(&unknown, &mut store);
  let second = auth.request(&request(b"git", "password"), &mut store);
  let third = auth.request(&unknown, &mut store);
  let failed = Ok(replies(vec![failure(false)]));
  assert_eq!(
    (first, second, third),
    (
      failed.clone(),
      failed,
      Err(AuthRefusal::TooManyFailures { failures: 3 })
    )
  );
}

/// A-633 hostile (RFC 4252 §5: "if the service does not exist, authentication MUST NOT be accepted"): do ask for another
/// service, send a request cut short, and a probe with a byte left over. Expect each refused by its kind.
#[test]
fn another_service_or_a_malformed_request_is_refused() {
  let mut store = Holders {
    entries: Vec::new(),
    unknown: false,
  };
  let mut other_service = vec![50];
  put_string(&mut other_service, b"git");
  put_string(&mut other_service, b"ssh-userauth");
  put_string(&mut other_service, b"none");
  let mut trailing = request(b"git", "none");
  trailing.push(0);
  let cut = request(b"git", "none");
  let outcomes = (
    authenticator(5).request(&other_service, &mut store),
    authenticator(5).request(&cut[..cut.len() - 1], &mut store),
    authenticator(5).request(&trailing, &mut store),
  );
  let expected = (
    Err(AuthRefusal::ServiceNotAvailable),
    Err(AuthRefusal::Malformed(WireRefusal::Truncated { at: 26 })),
    Err(AuthRefusal::Malformed(WireRefusal::TrailingBytes {
      at: 34,
    })),
  );
  assert_eq!(outcomes, expected);
}

/// A-633 hostile: do read key blobs whose Ed25519 key is 31 bytes, whose format is not the algorithm's, whose RSA
/// exponent is negative, whose modulus is 1024 bits, and one cut short. Expect each refused by its kind.
#[test]
fn key_blobs_are_checked_by_kind() {
  let blob = |fields: &[&[u8]]| {
    let mut blob = Vec::new();
    for field in fields {
      put_string(&mut blob, field);
    }
    blob
  };
  let small_modulus = [&[0x80u8][..], &[0u8; 127][..]].concat();
  let cases: [(SignatureAlgorithm, Vec<u8>, KeyRefusal); 5] = [
    (
      SignatureAlgorithm::Ed25519,
      blob(&[b"ssh-ed25519", &[1; 31]]),
      KeyRefusal::Length,
    ),
    (
      SignatureAlgorithm::RsaSha256,
      blob(&[b"ssh-ed25519", &[1; 32]]),
      KeyRefusal::Type,
    ),
    (
      SignatureAlgorithm::RsaSha512,
      blob(&[b"ssh-rsa", &[0x81], &[0x7f; 256]]),
      KeyRefusal::RsaComponent,
    ),
    (
      SignatureAlgorithm::RsaSha256,
      blob(&[
        b"ssh-rsa",
        &[1, 0, 1],
        &[&[0u8][..], &small_modulus[..]].concat(),
      ]),
      KeyRefusal::RsaModulusSize { bits: 1024 },
    ),
    (
      SignatureAlgorithm::Ed25519,
      blob(&[b"ssh-ed25519"]),
      KeyRefusal::Malformed(WireRefusal::Truncated { at: 15 }),
    ),
  ];
  for (algorithm, key, refusal) in cases {
    assert_eq!(
      UserKey::parse(algorithm, &key),
      Err(refusal),
      "{algorithm:?}"
    );
  }
}

/// A-651 (a key the holders must look up elsewhere). Do: under a failure limit of zero, query the first key and send its
/// signature, with holders that cannot say yet. Expect: neither request answered nor counted, each naming its user and key
/// as wanted, so the caller can look the key up and have the request asked again.
#[test]
fn a_key_the_holders_cannot_answer_for_yet_is_wanted_not_failed() {
  let mut auth = authenticator(0);
  let mut store = Holders {
    entries: Vec::new(),
    unknown: true,
  };
  let key = first_key();
  let asked = auth.request(&query(b"git", "ssh-ed25519", &key.blob), &mut store);
  let proof = signed(
    b"git",
    "ssh-ed25519",
    &key.blob,
    &ed_signature(&key, &SESSION_ID, b"git"),
  );
  let asked_signed = auth.request(&proof, &mut store);
  let wanted = |answer: Result<Answer<u32>, AuthRefusal>| {
    answer.map(|answer| {
      (
        answer.replies.len(),
        answer.authenticated.is_some(),
        answer.wanted.map(|wanted| (wanted.user, wanted.blob)),
      )
    })
  };
  let expected = Ok((0, false, Some((b"git".to_vec(), key.blob.clone()))));
  assert_eq!(
    (wanted(asked), wanted(asked_signed), auth.failures()),
    (expected.clone(), expected, 0)
  );
}

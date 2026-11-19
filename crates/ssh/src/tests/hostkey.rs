//! The host key (A-594; RFC 8709 §4, §6), held to RFC 8032 §7.1's Ed25519 vectors.

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{self, Ed25519KeyPair, UnparsedPublicKey};

use crate::client::put_string;
use crate::hostkey::{HostKey, HostKeyRejected};
use crate::support::{hex, hex_array};

/// RFC 8032 §7.1's tests 1 to 3: secret key, public key, message, signature.
const RFC8032: [(&str, &str, &str, &str); 3] = [
  (
    "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
    "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
    "",
    "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
  ),
  (
    "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
    "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
    "72",
    "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
  ),
  (
    "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
    "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
    "af82",
    "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
  ),
];

/// `string "ssh-ed25519", string value`.
fn blob(value: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  put_string(&mut out, b"ssh-ed25519");
  put_string(&mut out, value);
  out
}

/// A-594 golden (RFC 8032 §7.1 tests 1 to 3; RFC 8709 §4, §6): do build each test's key from its secret and sign its
/// message. Expect the public key blob around RFC 8032's public key, and the signature blob around its signature, byte
/// for byte, since Ed25519 is deterministic.
#[test]
fn host_keys_sign_as_rfc_8032_says() {
  for (secret, public, message, expected) in RFC8032 {
    let key = HostKey::ed25519_from_seed(&hex_array::<32>(secret)).unwrap();
    assert_eq!(key.algorithm(), "ssh-ed25519");
    assert_eq!(key.public_blob(), &blob(&hex(public))[..]);
    assert_eq!(key.sign(&hex(message)).unwrap(), blob(&hex(expected)));
  }
}

/// A-594: do build a host key from a PKCS#8 document AWS-LC made, sign with it and verify with its public key, and build
/// one from bytes that are no document. Expect the signature verified and the bytes refused.
#[test]
fn host_keys_load_from_pkcs8() {
  let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
  let key = HostKey::ed25519_from_pkcs8(document.as_ref()).unwrap();
  let signature = key.sign(b"exchange hash").unwrap();
  let public = &key.public_blob()[19..];
  let raw = &signature[19..];
  UnparsedPublicKey::new(&signature::ED25519, public)
    .verify(b"exchange hash", raw)
    .unwrap();
  assert_eq!(
    HostKey::ed25519_from_pkcs8(b"not a document").unwrap_err(),
    HostKeyRejected
  );
}

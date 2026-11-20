//! `curve25519-sha256` and `mlkem768x25519-sha256`, the exchange hash and key derivation (A-594, A-634; RFC 8731,
//! RFC 7748 §6.1, RFC 5656 §4, RFC 4253 §7.2, `draft-ietf-sshm-mlkem-hybrid-kex-10`), held to published vectors.

use aws_lc_rs::digest::{self, SHA1_FOR_LEGACY_USE_ONLY, SHA256};
use aws_lc_rs::kem::Ciphertext;

use crate::client::{mpint, put_string};
use crate::kex::{
  ExchangeInputs, KexRefusal, MLKEM768_CIPHERTEXT_LEN, curve25519_server, derive, ecdh_reply,
  exchange_hash, mlkem768x25519_server, new_keys, read_ecdh_init,
};
use crate::packet::Cipher;
use crate::support::{hex, hex_array, mlkem_key_pair, with_coefficient};
use crate::wire::WireRefusal;

/// RFC 7748 §6.1's Alice: her private key, the server's in the golden cases.
const ALICE_PRIVATE: &str = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a";
/// RFC 7748 §6.1's Alice: her public key.
const ALICE_PUBLIC: &str = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";
/// RFC 7748 §6.1's Bob: his public key, the client's in the golden cases.
const BOB_PUBLIC: &str = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f";
/// RFC 7748 §6.1's shared secret.
const RFC7748_SHARED: &str = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";

/// One NIST CAVS 14.1 SSH KDF case (AWS-LC's `crypto/evp_extra/sshkdf_tests.txt` at `02561621`, the vendored release's
/// commit).
struct KdfCase {
  k: &'static str,
  h: &'static str,
  iv_client_to_server: &'static str,
  iv_server_to_client: &'static str,
  key_client_to_server: &'static str,
  key_server_to_client: &'static str,
  integrity_client_to_server: &'static str,
  integrity_server_to_client: &'static str,
}

/// `[SHA-1]`, `[shared secret length = 1024]`, COUNT = 0: its 192-bit keys from a 160-bit hash need the extension.
const SHA1_1024: KdfCase = KdfCase {
  k: "0000008055bae931c07fd824bf10add1902b6fbc7c665347383498a686929ff5a25f8e40cb6645ea814fb1a5e0a11f852f86255641e5ed986e83a78bc8269480eac0b0dfd770cab92e7a28dd87ff452466d6ae867cead63b366b1c286e6c4811a9f14c27aea14c5171d49b78c06e3735d36e6a3be321dd5fc82308f34ee1cb17fba94a59",
  h: "a4ebd45934f56792b5112dcd75a1075fdc889245",
  iv_client_to_server: "e2f627c0b43f1ac1",
  iv_server_to_client: "58471445f342b181",
  key_client_to_server: "1ca9d310f86d51f6cb8e7007cb2b220d55c5281ce680b533",
  key_server_to_client: "2c60df8603d34cc1dbb03c11f725a44b44008851c73d6844",
  integrity_client_to_server: "472eb8a26166ae6aa8e06868e45c3b26e6eeed06",
  integrity_server_to_client: "e3e2fdb9d7bc21165a3dbe47e1eceb7764390bab",
};

/// `[SHA-256]`, `[shared secret length = 1024]`, COUNT = 0.
const SHA256_1024: KdfCase = KdfCase {
  k: "0000008100875c551cef526a4a8be1a7df27e9ed354bac9afb71f53dbae905679d14f9faf2469c53457cf80a366be278965ba6255276ca2d9f4a97d271f71e50d8a9ec46253a6a906ac2c5e4f48b27a63ce08d80390a492aa43bad9d882ccac23dac88bcada4b4d426a362083dab6569c54c224dd2d87643aa227693e141ad1630ce13144e",
  h: "0e683fc8a9ed7c2ff02def23b2745ebc99b267daa86a4aa7697239088253f642",
  iv_client_to_server: "41ff2ead1683f1e6",
  iv_server_to_client: "e619ecfd9edb50cd",
  key_client_to_server: "4a6314d2f7511bf88fad39fb6892f3f218cafd530e72fe43",
  key_server_to_client: "084c15fb7f99c65ff134eeb407cee5d540c341dea45a42a5",
  integrity_client_to_server: "41ec5a94fecce7707ea156a6ad29239a891621adacbedb8be70675008d6f9274",
  integrity_server_to_client: "47d3c20aba60981e47b30533623613ff1cacbcf1642fb4ad86ee712f2aed9af8",
};

/// `[SHA-256]`, `[shared secret length = 2048]`, COUNT = 0.
const SHA256_2048: KdfCase = KdfCase {
  k: "000001006ac382eaaca093e125e25c24bebc84640c11987507344b5c739ceb84a9e0b222b9a8b51c839e5ebe49cfadbfb39599764ed522099dc912751950dc7dc97fbdc06328b68f22781fd315af568009a5509e5b87a11bf527c056daffd82ab6cbc25cca37143459e7bc63bcde52757adeb7df01cf12173f1fef8102ec5ab142c213dd9d30696278a8d8bc32dde9592d28c078c6d92b947d825acaab6494846a49de24b9623f4889e8adc38e8c669effef176040ad945e90a7d3eec15efeee78ae71043c96511103a16ba7caf0acd0642efdbe809934faa1a5f1bd11043649b25ccd1fee2e38815d4d5f5fc6b4102969f21c22ae1b0e7d3603a556a13262ff628de222",
  h: "7b7001185e256d4493445f39a55fb905e6321f4b5dd8bbf3100d51ba0bda3d2d",
  iv_client_to_server: "81f0330ef6f05361b3823bfded6e1de9",
  iv_server_to_client: "3f6fd2065eeb2b0b1d93195a1fed48a5",
  key_client_to_server: "c35471034e6fd6547613178e23435f21",
  key_server_to_client: "7e9d79032090d99f98b015634dd9f462",
  integrity_client_to_server: "24ee559ad7ce712b685d0b2271e443c17ab1d1dceb5a360569d25d5dc243002f",
  integrity_server_to_client: "c3419c2b966235869d714ba5ac48ddb7d9e35c8c19aac73422337a373453607e",
};

/// A-594 golden (NIST CAVS 14.1 SSH KDF, SHA-1 and SHA-256): do derive each case's six values, letters "A" to "F", with
/// its K, H and session id (the first H). Expect NIST's bytes, the SHA-1 case's keys through RFC 4253 §7.2's extension.
#[test]
fn key_derivation_matches_nists_vectors() {
  let cases = [
    (&SHA1_FOR_LEGACY_USE_ONLY, &SHA1_1024),
    (&SHA256, &SHA256_1024),
    (&SHA256, &SHA256_2048),
  ];
  for (algorithm, case) in cases {
    let (k, h) = (hex(case.k), hex(case.h));
    let expected = [
      (b'A', case.iv_client_to_server),
      (b'B', case.iv_server_to_client),
      (b'C', case.key_client_to_server),
      (b'D', case.key_server_to_client),
      (b'E', case.integrity_client_to_server),
      (b'F', case.integrity_server_to_client),
    ];
    for (letter, value) in expected {
      let value = hex(value);
      let derived = derive(algorithm, &k, &h, letter, &h, value.len());
      assert_eq!(derived.bytes(), &value[..], "letter {}", letter as char);
    }
  }
}

/// A-594 golden (RFC 7748 §6.1): do run the server's half with Alice's private key against Bob's public key. Expect
/// Alice's public key as `Q_S`, and RFC 7748's shared secret as RFC 8731 §3.1's `mpint`: its 32 bytes read big-endian,
/// whose first byte, 0x4a, needs no leading zero.
#[test]
fn the_server_half_matches_rfc_7748() {
  let alice_private = hex_array::<32>(ALICE_PRIVATE);
  let bob_public = hex(BOB_PUBLIC);
  let half = curve25519_server(&alice_private, &bob_public).unwrap();
  assert_eq!(half.public, hex(ALICE_PUBLIC));
  let shared = hex(RFC7748_SHARED);
  assert_eq!(
    half.shared_secret.bytes(),
    &hex("00000020 4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742")[..]
  );
  assert_eq!(half.shared_secret.bytes(), &mpint(&shared)[..]);
}

/// A-594 hostile (RFC 8731 §3: "MUST check whether the computed Diffie-Hellman shared secret is the all-zero value and
/// abort", and "MUST also abort if the length of the received public keys are not the expected lengths"): do run the
/// server's half against public keys of small order (0, 1, two points of order 8, and p, a non-canonical 0) and of 31
/// and 33 bytes. Expect each refused by its kind.
#[test]
fn small_order_and_wrong_length_client_keys_are_refused() {
  let private = [0x42u8; 32];
  let small_order = [
    "0000000000000000000000000000000000000000000000000000000000000000",
    "0100000000000000000000000000000000000000000000000000000000000000",
    "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
    "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
    "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
  ];
  for public in small_order {
    assert_eq!(
      curve25519_server(&private, &hex(public)).unwrap_err(),
      KexRefusal::SharedSecretRejected,
      "{public}"
    );
  }
  assert_eq!(
    curve25519_server(&private, &[9; 31]).unwrap_err(),
    KexRefusal::PublicKeyLength { length: 31 }
  );
  assert_eq!(
    curve25519_server(&private, &[9; 33]).unwrap_err(),
    KexRefusal::PublicKeyLength { length: 33 }
  );
}

/// A-634 golden where the documents give one (the hybrid draft §2.1, §2.3.3, §2.4; RFC 7748 §6.1): do run the hybrid's
/// server half 64 times with Alice's private key against one `C_INIT` of a fresh ML-KEM-768 encapsulation key and Bob's
/// public key, and decapsulate each reply's ciphertext with the client's key. Expect each `S_REPLY` to be the 1088-byte
/// ciphertext and then Alice's public key, and each `K` the `string` of SHA-256 over the decapsulated secret and then
/// RFC 7748's shared secret, runs whose hash has its top bit set among them (there an `mpint` would add a zero byte, so
/// only those runs tell the two encodings apart; this counter is the test's non-vacuity); and 64 different ciphertexts,
/// since the draft's §6 says implementations "MUST NOT reuse randomness in the generation of ML-KEM ciphertexts".
#[test]
fn the_hybrid_server_half_agrees_with_a_decapsulating_client() {
  let alice_private = hex_array::<32>(ALICE_PRIVATE);
  let (client_key, encapsulation_key) = mlkem_key_pair();
  let init = [encapsulation_key, hex(BOB_PUBLIC)].concat();
  let mut ciphertexts = Vec::new();
  let mut top_bit_set = 0usize;
  for _ in 0..64 {
    let half = mlkem768x25519_server(&alice_private, &init).unwrap();
    let (ciphertext, server_public) = half.public.split_at(MLKEM768_CIPHERTEXT_LEN);
    let post_quantum = client_key
      .decapsulate(Ciphertext::from(ciphertext))
      .unwrap();
    let both = [post_quantum.as_ref(), &hex(RFC7748_SHARED)[..]].concat();
    let hashed = digest::digest(&SHA256, &both);
    let mut expected = Vec::new();
    put_string(&mut expected, hashed.as_ref());
    assert_eq!(
      (server_public.to_vec(), half.shared_secret.bytes().to_vec()),
      (hex(ALICE_PUBLIC), expected)
    );
    top_bit_set += usize::from(hashed.as_ref()[0] & 0x80 != 0);
    ciphertexts.push(ciphertext.to_vec());
  }
  ciphertexts.sort();
  ciphertexts.dedup();
  assert_eq!((ciphertexts.len(), top_bit_set > 0), (64, true));
}

/// A-634 hostile (the hybrid draft §2.1: the server "MUST check that the length of the C_INIT is the sum of the expected
/// length of each public key" and "MUST perform the encapsulation key checks defined in Section 7.2 of \[FIPS203\]";
/// RFC 8731 §3 for the X25519 half): do run the hybrid's server half on `C_INIT`s a byte short and a byte long, a
/// classical 32-byte key, encapsulation keys whose first or last coefficient is q (3329) or 4095, and a good key beside
/// the all-zero X25519 point; and, as controls, keys whose first or last coefficient is q - 1 and one whose seed bytes
/// are all ones. Expect each refused by its kind, AWS-LC's modulus check refusing the encapsulation, and the controls
/// served.
#[test]
fn bad_hybrid_client_values_are_refused() {
  let (_, key) = mlkem_key_pair();
  let bob = hex(BOB_PUBLIC);
  let init = |encapsulation: &[u8], x25519: &[u8]| [encapsulation, x25519].concat();
  let last = 3 * 256 - 1;
  let reseeded = [&key[..1152], &[0xff; 32][..]].concat();
  let cases: [(Vec<u8>, Result<(), KexRefusal>); 10] = [
    (
      vec![9; 1215],
      Err(KexRefusal::PublicKeyLength { length: 1215 }),
    ),
    (
      vec![9; 1217],
      Err(KexRefusal::PublicKeyLength { length: 1217 }),
    ),
    (bob.clone(), Err(KexRefusal::PublicKeyLength { length: 32 })),
    (
      init(&with_coefficient(&key, 0, 3329), &bob),
      Err(KexRefusal::EncapsulationFailed),
    ),
    (
      init(&with_coefficient(&key, last, 4095), &bob),
      Err(KexRefusal::EncapsulationFailed),
    ),
    (init(&key, &[0; 32]), Err(KexRefusal::SharedSecretRejected)),
    (init(&with_coefficient(&key, 0, 3328), &bob), Ok(())),
    (init(&with_coefficient(&key, last, 3328), &bob), Ok(())),
    (init(&reseeded, &bob), Ok(())),
    (init(&key, &bob), Ok(())),
  ];
  for (index, (init, expected)) in cases.into_iter().enumerate() {
    let outcome = mlkem768x25519_server(&[0x42; 32], &init).map(|_| ());
    assert_eq!(outcome, expected, "case {index}");
  }
}

/// A-594 (RFC 5656 §4): do hash an exchange's inputs, and hash by hand the same fields, each as a `string` but `K`,
/// already an `mpint`. Expect the same 32 bytes; and a change to any one field changes the hash.
#[test]
fn the_exchange_hash_covers_each_field_in_order() {
  let fields: [Vec<u8>; 7] = [
    b"SSH-2.0-OpenSSH_10.2".to_vec(),
    b"SSH-2.0-slates_0.1.0".to_vec(),
    vec![20; 40],
    vec![20; 41],
    b"\x00\x00\x00\x0bssh-ed25519\x00\x00\x00\x20aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec(),
    vec![1; 32],
    vec![2; 32],
  ];
  let k = mpint(&[0x80; 32]);
  let inputs = |fields: &[Vec<u8>; 7]| -> [u8; 32] {
    exchange_hash(&ExchangeInputs {
      client_identification: &fields[0],
      server_identification: &fields[1],
      client_kexinit: &fields[2],
      server_kexinit: &fields[3],
      host_key: &fields[4],
      client_public: &fields[5],
      server_public: &fields[6],
      shared_secret: &k,
    })
    .unwrap()
  };
  let mut by_hand = Vec::new();
  for field in &fields {
    put_string(&mut by_hand, field);
  }
  by_hand.extend_from_slice(&k);
  let expected: [u8; 32] = aws_lc_rs::digest::digest(&SHA256, &by_hand)
    .as_ref()
    .try_into()
    .unwrap();
  assert_eq!(inputs(&fields), expected);
  for index in 0..fields.len() {
    let mut changed = fields.clone();
    changed[index].push(0);
    assert_ne!(inputs(&changed), expected, "field {index}");
  }
}

/// A-594: do derive both directions' keys for each cipher. Expect ChaCha20-Poly1305's 64-byte key and no IV, AES-GCM's
/// 32-byte key and 12-byte IV, each the letter's derivation, and the two directions' keys different.
#[test]
fn new_keys_have_each_ciphers_lengths() {
  let (k, h) = (mpint(&[7; 32]), [3u8; 32]);
  let keys = new_keys(&k, &h, &h, Cipher::ChaCha20Poly1305, Cipher::Aes256Gcm);
  assert_eq!(keys.client_to_server.key.bytes().len(), 64);
  assert!(keys.client_to_server.iv.bytes().is_empty());
  assert_eq!(keys.server_to_client.key.bytes().len(), 32);
  assert_eq!(keys.server_to_client.iv.bytes().len(), 12);
  assert_eq!(
    keys.client_to_server.key.bytes(),
    &crate::client::kdf(&k, &h, b'C', &h, 64)[..]
  );
  assert_eq!(
    keys.server_to_client.iv.bytes(),
    &crate::client::kdf(&k, &h, b'B', &h, 12)[..]
  );
  assert_ne!(
    &keys.client_to_server.key.bytes()[..32],
    keys.server_to_client.key.bytes()
  );
}

/// A-594 hostile: do read `SSH_MSG_KEX_ECDH_INIT` whole, cut short and with a byte left over, and write a reply. Expect
/// the client's key, typed refusals, and the reply's three strings in order.
#[test]
fn ecdh_messages_read_and_write() {
  let mut init = vec![30];
  put_string(&mut init, &[5; 32]);
  assert_eq!(read_ecdh_init(&init), Ok(&[5u8; 32][..]));
  assert_eq!(
    read_ecdh_init(&init[..20]),
    Err(WireRefusal::Truncated { at: 1 })
  );
  let mut trailing = init.clone();
  trailing.push(1);
  assert_eq!(
    read_ecdh_init(&trailing),
    Err(WireRefusal::TrailingBytes { at: 37 })
  );
  let reply = ecdh_reply(b"key", b"public", b"signature").unwrap();
  let mut expected = vec![31];
  for field in [&b"key"[..], &b"public"[..], &b"signature"[..]] {
    put_string(&mut expected, field);
  }
  assert_eq!(reply, expected);
}

//! Algorithm negotiation (A-594, A-634; RFC 4253 §7, §7.1; the strict-kex draft §3.1; the AES-GCM draft §2).

use proptest::prelude::*;

use crate::client::put_string;
use crate::kexinit::{KexInit, KexInitRefusal, KexMethod, List, negotiate, server_kexinit};
use crate::packet::Cipher;
use crate::wire::WireRefusal;

/// A KEXINIT payload with these ten lists and guess flag.
fn kexinit(lists: [&str; 10], first_kex_packet_follows: bool) -> Vec<u8> {
  let mut payload = vec![20];
  payload.extend_from_slice(&[0x11; 16]);
  for list in lists {
    put_string(&mut payload, list.as_bytes());
  }
  payload.push(u8::from(first_kex_packet_follows));
  payload.extend_from_slice(&0u32.to_be_bytes());
  payload
}

/// A client's lists like OpenSSH 10's, with `kex` and `ciphers` as given.
fn client(kex: &str, ciphers: &str) -> [String; 10] {
  [
    kex.to_string(),
    "ssh-ed25519-cert-v01@openssh.com,ssh-ed25519,rsa-sha2-512".to_string(),
    ciphers.to_string(),
    ciphers.to_string(),
    "hmac-sha2-256-etm@openssh.com,umac-128-etm@openssh.com".to_string(),
    "hmac-sha2-256-etm@openssh.com,umac-128-etm@openssh.com".to_string(),
    "none,zlib@openssh.com".to_string(),
    "none,zlib@openssh.com".to_string(),
    String::new(),
    String::new(),
  ]
}

fn as_strs(lists: &[String; 10]) -> [&str; 10] {
  std::array::from_fn(|index| lists[index].as_str())
}

/// A-594, A-634: do write the server's first KEXINIT and a later one, and read them back. Expect `mlkem768x25519-sha256`
/// then `curve25519-sha256`, with both strict markers in the first and none in the later one; `ssh-ed25519`; the two
/// AEAD ciphers each way; `none` as the MAC and the compression; empty language lists; no guess.
#[test]
fn the_server_offers_its_methods_and_its_markers_first_only() {
  the_first_kexinit_offers_the_methods_and_the_markers();
  the_later_kexinit_offers_no_markers();
}

/// The first KEXINIT's lists, the strict markers among them, and its cookie.
fn the_first_kexinit_offers_the_methods_and_the_markers() {
  let first = server_kexinit(&[1; 16], true).unwrap();
  let read = KexInit::read(&first).unwrap();
  let names = |list: List| -> Vec<String> {
    read
      .list(list)
      .names()
      .map(|name| String::from_utf8(name.to_vec()).unwrap())
      .collect()
  };
  assert_eq!(
    names(List::Kex),
    [
      "mlkem768x25519-sha256",
      "curve25519-sha256",
      "kex-strict-s-v00@openssh.com",
      "kex-strict-s"
    ]
  );
  assert_eq!(names(List::HostKey), ["ssh-ed25519"]);
  assert_eq!(
    names(List::CipherClientToServer),
    ["chacha20-poly1305@openssh.com", "aes256-gcm@openssh.com"]
  );
  assert_eq!(
    names(List::CipherServerToClient),
    ["chacha20-poly1305@openssh.com", "aes256-gcm@openssh.com"]
  );
  assert_eq!(names(List::MacClientToServer), ["none"]);
  assert_eq!(names(List::CompressionServerToClient), ["none"]);
  assert!(names(List::LanguageClientToServer).is_empty());
  assert!(!read.first_kex_packet_follows);
  assert_eq!(read.cookie, &[1; 16]);
}

/// A later KEXINIT's key exchange list: the methods alone.
fn the_later_kexinit_offers_no_markers() {
  let later = server_kexinit(&[2; 16], false).unwrap();
  let read_later = KexInit::read(&later).unwrap();
  let kex: Vec<&[u8]> = read_later.list(List::Kex).names().collect();
  assert_eq!(
    kex,
    [&b"mlkem768x25519-sha256"[..], &b"curve25519-sha256"[..]]
  );
}

/// A-594: do negotiate clients that prefer each cipher, and one that prefers different ciphers each way. Expect the
/// client's first cipher the server runs, chosen per direction, and the MAC lists ignored though they share nothing
/// with the server's (the AES-GCM draft §2: "MUST NOT cause connection failure").
#[test]
fn the_clients_first_choice_wins_per_direction() {
  let mut lists = client(
    "curve25519-sha256,ext-info-c",
    "aes128-ctr,aes256-gcm@openssh.com,chacha20-poly1305@openssh.com",
  );
  let negotiated = negotiate(
    &KexInit::read(&kexinit(as_strs(&lists), false)).unwrap(),
    true,
  )
  .unwrap();
  assert_eq!(negotiated.client_to_server, Cipher::Aes256Gcm);
  assert_eq!(negotiated.server_to_client, Cipher::Aes256Gcm);
  lists[3] = "chacha20-poly1305@openssh.com".to_string();
  let negotiated = negotiate(
    &KexInit::read(&kexinit(as_strs(&lists), false)).unwrap(),
    true,
  )
  .unwrap();
  assert_eq!(negotiated.client_to_server, Cipher::Aes256Gcm);
  assert_eq!(negotiated.server_to_client, Cipher::ChaCha20Poly1305);
  assert!(negotiated.client_ext_info);
}

/// A-594 hostile: do negotiate clients that list markers first: their own, RFC 8308's and the server's own markers, and
/// one whose key exchange list holds markers alone. Expect `curve25519-sha256` chosen past every marker, and the
/// markers-only list refused as having no key exchange in common: a marker is never a method.
#[test]
fn markers_are_never_chosen_as_methods() {
  let lists = client(
    "kex-strict-s-v00@openssh.com,kex-strict-s,ext-info-c,kex-strict-c-v00@openssh.com,curve25519-sha256",
    "chacha20-poly1305@openssh.com",
  );
  assert!(
    negotiate(
      &KexInit::read(&kexinit(as_strs(&lists), false)).unwrap(),
      true
    )
    .is_ok()
  );
  let markers_only = client(
    "kex-strict-c-v00@openssh.com,ext-info-c,kex-strict-s-v00@openssh.com",
    "chacha20-poly1305@openssh.com",
  );
  assert_eq!(
    negotiate(
      &KexInit::read(&kexinit(as_strs(&markers_only), false)).unwrap(),
      true
    ),
    Err(KexInitRefusal::NoCommonAlgorithm(List::Kex))
  );
}

/// A-594 (the strict-kex draft §3.1): do negotiate a client offering the pre-standard client marker, one offering the
/// standard one, one offering none, one offering only the server's markers, and the first client again in a later
/// exchange. Expect strict key exchange on for the first two, which pair with the server's markers of the same form;
/// off for the rest, since markers count in the first exchange only.
#[test]
fn strict_key_exchange_is_judged_by_the_client_marker_in_the_first_exchange() {
  let strict = |kex: &str, initial: bool| {
    let lists = client(kex, "chacha20-poly1305@openssh.com");
    negotiate(
      &KexInit::read(&kexinit(as_strs(&lists), false)).unwrap(),
      initial,
    )
    .unwrap()
    .strict
  };
  assert!(strict(
    "curve25519-sha256,kex-strict-c-v00@openssh.com",
    true
  ));
  assert!(strict("curve25519-sha256,kex-strict-c", true));
  assert!(!strict("curve25519-sha256,ext-info-c", true));
  assert!(!strict(
    "curve25519-sha256,kex-strict-s-v00@openssh.com,kex-strict-s",
    true
  ));
  assert!(!strict(
    "curve25519-sha256,kex-strict-c-v00@openssh.com",
    false
  ));
}

/// A-594 hostile (RFC 4253 §7.1, "If there is no such algorithm, both sides MUST disconnect"): do negotiate a client
/// sharing no key exchange, no host key, no cipher one way or the other, or no compression. Expect each refused naming
/// its list.
#[test]
fn no_common_algorithm_is_refused_by_list() {
  let base = client("curve25519-sha256", "chacha20-poly1305@openssh.com");
  let cases: [(usize, &str, List); 6] = [
    (
      0,
      "diffie-hellman-group14-sha256,sntrup761x25519-sha512@openssh.com",
      List::Kex,
    ),
    (1, "rsa-sha2-512,ecdsa-sha2-nistp256", List::HostKey),
    (2, "aes128-ctr,aes256-cbc", List::CipherClientToServer),
    (3, "aes128-gcm@openssh.com", List::CipherServerToClient),
    (6, "zlib@openssh.com", List::CompressionClientToServer),
    (7, "zlib", List::CompressionServerToClient),
  ];
  for (index, value, list) in cases {
    let mut lists = base.clone();
    lists[index] = value.to_string();
    let payload = kexinit(as_strs(&lists), false);
    let read = KexInit::read(&payload).unwrap();
    assert_eq!(
      negotiate(&read, true),
      Err(KexInitRefusal::NoCommonAlgorithm(list)),
      "{list:?}"
    );
  }
  let mut empty_cipher = base.clone();
  empty_cipher[2] = String::new();
  let payload = kexinit(as_strs(&empty_cipher), false);
  let read = KexInit::read(&payload).unwrap();
  assert_eq!(
    negotiate(&read, true),
    Err(KexInitRefusal::NoCommonAlgorithm(
      List::CipherClientToServer
    ))
  );
}

/// A-594, A-634 (RFC 4253 §7): do negotiate a guessing client whose preferred key exchange and host key are the
/// server's (the hybrid, as OpenSSH 10 lists it), one that prefers another key exchange the server also runs, one that
/// prefers another host key, and one that does not guess. Expect the guessed packet kept for the first, skipped for the
/// next two, and nothing to skip for the last.
#[test]
fn a_wrong_guess_is_skipped() {
  let skip = |kex: &str, host_keys: &str, guess: bool| {
    let mut lists = client(kex, "chacha20-poly1305@openssh.com");
    lists[1] = host_keys.to_string();
    negotiate(
      &KexInit::read(&kexinit(as_strs(&lists), guess)).unwrap(),
      true,
    )
    .unwrap()
    .skip_guessed_packet
  };
  let hybrid_first = "mlkem768x25519-sha256,curve25519-sha256,ext-info-c";
  let curve_first = "curve25519-sha256,mlkem768x25519-sha256";
  let observed = [
    skip(hybrid_first, "ssh-ed25519", true),
    skip(curve_first, "ssh-ed25519", true),
    skip(hybrid_first, "rsa-sha2-512,ssh-ed25519", true),
    skip(curve_first, "ssh-ed25519", false),
  ];
  assert_eq!(observed, [false, true, true, false]);
}

/// A-634 (RFC 4253 §7.1: "the first algorithm on the client's name-list that is also on the server's name-list"): do
/// negotiate clients listing the hybrid first, `curve25519-sha256` before the hybrid, a method the server does not run
/// before the hybrid, and `curve25519-sha256` after a name the server does not run. Expect the hybrid,
/// `curve25519-sha256`, the hybrid and `curve25519-sha256`: the client's choice, whatever the server prefers.
#[test]
fn the_clients_first_key_exchange_is_chosen() {
  let chosen = |kex: &str| {
    let lists = client(kex, "chacha20-poly1305@openssh.com");
    negotiate(
      &KexInit::read(&kexinit(as_strs(&lists), false)).unwrap(),
      true,
    )
    .unwrap()
    .kex
  };
  let observed = [
    chosen("mlkem768x25519-sha256,curve25519-sha256,ext-info-c"),
    chosen("curve25519-sha256,mlkem768x25519-sha256"),
    chosen("sntrup761x25519-sha512,mlkem768x25519-sha256,curve25519-sha256"),
    chosen("curve25519-sha256@libssh.org,curve25519-sha256"),
  ];
  assert_eq!(
    observed,
    [
      KexMethod::Mlkem768X25519Sha256,
      KexMethod::Curve25519Sha256,
      KexMethod::Mlkem768X25519Sha256,
      KexMethod::Curve25519Sha256
    ]
  );
}

/// A-594 hostile: do read KEXINITs cut short, with a bad name-list, with a byte left over, and with another message's
/// number. Expect each refused by its kind.
#[test]
fn malformed_kexinits_are_refused() {
  let good = kexinit(
    as_strs(&client(
      "curve25519-sha256",
      "chacha20-poly1305@openssh.com",
    )),
    false,
  );
  assert!(matches!(
    KexInit::read(&good[..good.len() - 1]),
    Err(KexInitRefusal::Malformed(WireRefusal::Truncated { .. }))
  ));
  let mut trailing = good.clone();
  trailing.push(0);
  assert!(matches!(
    KexInit::read(&trailing),
    Err(KexInitRefusal::Malformed(WireRefusal::TrailingBytes { .. }))
  ));
  let mut bad_name = vec![20];
  bad_name.extend_from_slice(&[0; 16]);
  put_string(&mut bad_name, b"curve25519-sha256,,x");
  assert!(matches!(
    KexInit::read(&bad_name),
    Err(KexInitRefusal::Malformed(WireRefusal::BadNameList { .. }))
  ));
  let mut other = good.clone();
  other[0] = 21;
  assert_eq!(KexInit::read(&other), Err(KexInitRefusal::NotKexInit));
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-594 hostile: do read and negotiate any bytes after the KEXINIT number. Expect a negotiation or a typed refusal,
  /// never a panic, and never a marker or an unknown name chosen.
  #[test]
  fn any_kexinit_negotiates_or_refuses(tail in proptest::collection::vec(any::<u8>(), 0..400)) {
    let mut payload = vec![20];
    payload.extend_from_slice(&tail);
    if let Ok(read) = KexInit::read(&payload)
      && let Ok(negotiated) = negotiate(&read, true)
    {
      let method = match negotiated.kex {
        KexMethod::Mlkem768X25519Sha256 => "mlkem768x25519-sha256",
        KexMethod::Curve25519Sha256 => "curve25519-sha256",
      };
      prop_assert!(read.list(List::Kex).contains(method));
      prop_assert!(Cipher::OFFERED.contains(&negotiated.client_to_server));
    }
  }
}

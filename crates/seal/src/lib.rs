//! The rendezvous seal (§4.7, A-187): what every Linux rendezvous's messages cross under, implementing
//! `slates_ipc::seal`'s interface with aws-lc-rs, the one crypto library (A-66). `slates-ipc` builds for targets with no
//! C toolchain, so it defines the interface and the daemon and the client install this implementation ([`install`]).
//!
//! **The exchange.** Each side makes a fresh X25519 key pair (RFC 7748) for the rendezvous and sends its public key.
//! Each computes the shared secret, and derives two AES-256-GCM keys by HKDF-SHA256 (RFC 5869) from it, salted with the
//! client's public key and then the daemon's: one keys what the client sends, one what the daemon sends, each named by
//! its own info string. The exchange refuses a public key whose shared secret is all zeros (a point of small order,
//! RFC 7748 §6.1), which aws-lc-rs refuses for it.
//!
//! **Each message.** Sealed with its direction's key at a nonce that counts that direction's messages from zero (the
//! count's eight bytes, big-endian, after four zeros), the label and the message's class as additional data. A key
//! never seals two messages at one nonce (NIST SP 800-38D §8.2.1), and since each side opens at the count it expects,
//! a message replayed, dropped or reordered does not open, nor one of another class, nor one changed in any byte. A
//! count that would wrap is refused rather than reused.
//!
//! **Why fresh keys each rendezvous.** No key outlives its connection, so nothing keyed is kept, at rest or anywhere,
//! and a recorded connection opens under no later one's keys.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero; test builds are exempt.
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519};
use aws_lc_rs::hkdf;
use aws_lc_rs::rand::SystemRandom;
use slates_ipc::error::IpcError;
use slates_ipc::seal::{Class, PUBLIC_KEY_BYTES, RendezvousSeal, Role, SealedChannel, TAG_BYTES};

/// Format: the label every sealed message carries as additional data, naming the protocol and its version.
const LABEL: &[u8; 20] = b"slates rendezvous v1";
/// Format: the info string of the key that seals what the client sends.
const CLIENT_TO_DAEMON: &[u8] = b"slates rendezvous v1 client to daemon";
/// Format: the info string of the key that seals what the daemon sends.
const DAEMON_TO_CLIENT: &[u8] = b"slates rendezvous v1 daemon to client";
/// Format: an AES-256-GCM key's bytes.
const KEY_BYTES: usize = 32;
/// Format: the zero bytes before the count in a nonce: a nonce is [`NONCE_LEN`] bytes and the count eight.
const NONCE_PAD_BYTES: usize = NONCE_LEN - size_of::<u64>();

/// Installs this crate's seal as the process's (`slates_ipc::seal::install`), before its first rendezvous: whether it is
/// the one installed. The daemon and the client call it; a second call changes nothing.
pub fn install() -> bool {
  slates_ipc::seal::install(fresh)
}

/// A fresh half of an exchange: a new X25519 key pair from the system's generator.
pub fn fresh() -> Result<Box<dyn RendezvousSeal>, IpcError> {
  let private = EphemeralPrivateKey::generate(&X25519, &SystemRandom::new()).map_err(|_| {
    IpcError::SealRefused {
      why: "an X25519 key pair could not be made",
    }
  })?;
  let public = private
    .compute_public_key()
    .ok()
    .and_then(|public| <[u8; PUBLIC_KEY_BYTES]>::try_from(public.as_ref()).ok())
    .ok_or(IpcError::SealRefused {
      why: "an X25519 public key could not be computed",
    })?;
  Ok(Box::new(Exchange { private, public }))
}

/// One side's half of an exchange: its key pair.
struct Exchange {
  private: EphemeralPrivateKey,
  public: [u8; PUBLIC_KEY_BYTES],
}

impl RendezvousSeal for Exchange {
  fn public_key(&self) -> [u8; PUBLIC_KEY_BYTES] {
    self.public
  }

  fn agree(
    self: Box<Self>,
    peer: &[u8; PUBLIC_KEY_BYTES],
    role: Role,
  ) -> Result<Box<dyn SealedChannel>, IpcError> {
    let Exchange { private, public } = *self;
    let (client, daemon) = match role {
      Role::Client => (public, *peer),
      Role::Daemon => (*peer, public),
    };
    let refused = IpcError::SealRefused {
      why: "the other side's public key does not agree (a point of small order, or not a point)",
    };
    agreement::agree_ephemeral(
      private,
      UnparsedPublicKey::new(&X25519, peer),
      refused,
      |shared| channel(shared, (&client, &daemon), role),
    )
    .map(|channel| Box::new(channel) as Box<dyn SealedChannel>)
  }
}

/// The channel an exchange's shared secret keys, for `role`, salted with the client's and the daemon's public keys.
fn channel(
  shared: &[u8],
  (client, daemon): (&[u8; PUBLIC_KEY_BYTES], &[u8; PUBLIC_KEY_BYTES]),
  role: Role,
) -> Result<Channel, IpcError> {
  let mut salt = [0u8; 2 * PUBLIC_KEY_BYTES];
  let (first, second) = salt.split_at_mut(PUBLIC_KEY_BYTES);
  first.copy_from_slice(client);
  second.copy_from_slice(daemon);
  let to_daemon = key(shared, &salt, CLIENT_TO_DAEMON)?;
  let to_client = key(shared, &salt, DAEMON_TO_CLIENT)?;
  let (sends, opens) = match role {
    Role::Client => (to_daemon, to_client),
    Role::Daemon => (to_client, to_daemon),
  };
  Ok(Channel {
    sends,
    opens,
    sent: 0,
    opened: 0,
  })
}

/// The AES-256-GCM key HKDF-SHA256 derives from `shared`, salted with `salt`, for `info`.
fn key(shared: &[u8], salt: &[u8], info: &[u8]) -> Result<LessSafeKey, IpcError> {
  let bytes = key_bytes(shared, salt, info)?;
  let unbound = UnboundKey::new(&AES_256_GCM, &bytes).map_err(|_| IpcError::SealRefused {
    why: "an AES-256-GCM key could not be made",
  })?;
  Ok(LessSafeKey::new(unbound))
}

/// The key's bytes HKDF-SHA256 derives from `shared`, salted with `salt`, for `info` (RFC 5869).
fn key_bytes(shared: &[u8], salt: &[u8], info: &[u8]) -> Result<[u8; KEY_BYTES], IpcError> {
  /// The derived key's length, as HKDF asks it.
  struct Length;
  impl hkdf::KeyType for Length {
    fn len(&self) -> usize {
      KEY_BYTES
    }
  }
  let refused = |_| IpcError::SealRefused {
    why: "HKDF-SHA256 did not derive a key",
  };
  let mut bytes = [0u8; KEY_BYTES];
  let info = [info];
  hkdf::Salt::new(hkdf::HKDF_SHA256, salt)
    .extract(shared)
    .expand(&info, Length)
    .map_err(refused)?
    .fill(&mut bytes)
    .map_err(refused)?;
  Ok(bytes)
}

/// The keys an exchange agreed, and each direction's count of messages.
struct Channel {
  sends: LessSafeKey,
  opens: LessSafeKey,
  sent: u64,
  opened: u64,
}

impl SealedChannel for Channel {
  fn seal(&mut self, class: Class, plaintext: &[u8]) -> Result<Vec<u8>, IpcError> {
    let next = self.sent.checked_add(1).ok_or(IpcError::SealRefused {
      why: "this side's count of messages is spent",
    })?;
    let mut sealed = Vec::with_capacity(plaintext.len().saturating_add(TAG_BYTES));
    sealed.extend_from_slice(plaintext);
    let tag = self
      .sends
      .seal_in_place_separate_tag(nonce(self.sent), additional(class), &mut sealed)
      .map_err(|_| IpcError::SealRefused {
        why: "AES-256-GCM did not seal",
      })?;
    sealed.extend_from_slice(tag.as_ref());
    self.sent = next;
    Ok(sealed)
  }

  fn open(&mut self, class: Class, sealed: &[u8]) -> Result<Vec<u8>, IpcError> {
    let next = self.opened.checked_add(1).ok_or(IpcError::SealRefused {
      why: "the other side's count of messages is spent",
    })?;
    let mut copy = sealed.to_vec();
    let opened = self
      .opens
      .open_in_place(nonce(self.opened), additional(class), &mut copy)
      .map_err(|_| IpcError::SealRefused {
        why: "a message that does not open (another class or key, replayed, dropped, reordered or changed)",
      })?
      .len();
    copy.truncate(opened);
    self.opened = next;
    Ok(copy)
  }
}

/// The nonce of the message counted `count`: four zero bytes, then the count, big-endian.
fn nonce(count: u64) -> Nonce {
  let mut bytes = [0u8; NONCE_LEN];
  let (_, tail) = bytes.split_at_mut(NONCE_PAD_BYTES);
  tail.copy_from_slice(&count.to_be_bytes());
  Nonce::assume_unique_for_key(bytes)
}

/// The additional data a message of `class` is sealed with: the label and the class's byte.
fn additional(class: Class) -> Aad<[u8; LABEL.len() + 1]> {
  let mut bytes = [0u8; LABEL.len() + 1];
  let (label, kind) = bytes.split_at_mut(LABEL.len());
  label.copy_from_slice(LABEL);
  if let Some(byte) = kind.first_mut() {
    *byte = class.byte();
  }
  Aad::from(bytes)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Both halves of one exchange, agreed: the client's channel and the daemon's.
  fn agreed() -> (Box<dyn SealedChannel>, Box<dyn SealedChannel>) {
    let client = fresh().unwrap();
    let daemon = fresh().unwrap();
    let (client_key, daemon_key) = (client.public_key(), daemon.public_key());
    (
      client.agree(&daemon_key, Role::Client).unwrap(),
      daemon.agree(&client_key, Role::Daemon).unwrap(),
    )
  }

  /// A-187: a sealed message opens on the other side, in order, both ways. Do: agree an exchange; seal a hello from the
  /// client and a handoff and two deliveries from the daemon; open each on the other side in the order sent. Expect:
  /// each opens to its plaintext, and a sealed message is its plaintext's length and a tag longer, none of its plaintext
  /// showing in it.
  #[test]
  fn a_sealed_message_opens_on_the_other_side_in_order() {
    let (mut client, mut daemon) = agreed();
    let hello = 7u32.to_le_bytes();
    let sealed = client.seal(Class::Hello, &hello).unwrap();
    assert_eq!(sealed.len(), hello.len() + TAG_BYTES);
    assert_eq!(daemon.open(Class::Hello, &sealed).unwrap(), hello);
    for (class, plaintext) in [
      (Class::Handoff, &b"handoff of twelve"[..12]),
      (Class::Delivery, &[0x5a; 48][..]),
      (Class::Delivery, &[0xa5; 48][..]),
    ] {
      let sealed = daemon.seal(class, plaintext).unwrap();
      assert!(
        !sealed
          .windows(plaintext.len())
          .any(|window| window == plaintext),
        "the plaintext does not show"
      );
      assert_eq!(client.open(class, &sealed).unwrap(), plaintext);
    }
  }

  /// A-187: a message replayed, reordered or dropped does not open. Do: seal three deliveries; open the first twice,
  /// then open the third before the second. Expect: the replay refused, the third refused while the second is due, and
  /// the second then opens, since a refusal does not move the count.
  #[test]
  fn a_message_replayed_reordered_or_dropped_does_not_open() {
    let (mut client, mut daemon) = agreed();
    let sealed: Vec<Vec<u8>> = (0u8..3)
      .map(|index| daemon.seal(Class::Delivery, &[index; 48]).unwrap())
      .collect();
    assert_eq!(client.open(Class::Delivery, &sealed[0]).unwrap(), [0u8; 48]);
    assert!(matches!(
      client.open(Class::Delivery, &sealed[0]),
      Err(IpcError::SealRefused { .. })
    ));
    assert!(matches!(
      client.open(Class::Delivery, &sealed[2]),
      Err(IpcError::SealRefused { .. })
    ));
    assert_eq!(client.open(Class::Delivery, &sealed[1]).unwrap(), [1u8; 48]);
  }

  /// A-187: a message of another class, or changed in any byte, does not open. Do: seal a hello; open it as a delivery,
  /// then each copy of it with one bit flipped. Expect: every one refused, and the hello itself then opens.
  #[test]
  fn a_message_of_another_class_or_changed_does_not_open() {
    let (mut client, mut daemon) = agreed();
    let sealed = client.seal(Class::Hello, &[1, 2, 3, 4]).unwrap();
    assert!(matches!(
      daemon.open(Class::Delivery, &sealed),
      Err(IpcError::SealRefused { .. })
    ));
    for at in 0..sealed.len() {
      let mut changed = sealed.clone();
      changed[at] ^= 1;
      assert!(
        matches!(
          daemon.open(Class::Hello, &changed),
          Err(IpcError::SealRefused { .. })
        ),
        "a bit flipped at byte {at}"
      );
    }
    assert_eq!(daemon.open(Class::Hello, &sealed).unwrap(), [1, 2, 3, 4]);
  }

  /// A-187: a third party's exchange keys nothing the two sides use. Do: the client agrees with a third party's key in
  /// place of the daemon's, and seals a hello. Expect: the daemon, agreed with the client's real key, cannot open it.
  #[test]
  fn a_third_partys_key_opens_nothing() {
    let client = fresh().unwrap();
    let daemon = fresh().unwrap();
    let third = fresh().unwrap();
    let client_key = client.public_key();
    let mut client = client.agree(&third.public_key(), Role::Client).unwrap();
    let mut daemon = daemon.agree(&client_key, Role::Daemon).unwrap();
    let sealed = client.seal(Class::Hello, &[9; 4]).unwrap();
    assert!(matches!(
      daemon.open(Class::Hello, &sealed),
      Err(IpcError::SealRefused { .. })
    ));
  }

  /// A-187, RFC 7748 §6.1: a public key of small order is refused, so no exchange agrees on a shared secret an
  /// attacker can predict. Do: agree with the all-zero key. Expect: `SealRefused`.
  #[test]
  fn a_public_key_of_small_order_is_refused() {
    let client = fresh().unwrap();
    assert!(matches!(
      client.agree(&[0u8; PUBLIC_KEY_BYTES], Role::Client),
      Err(IpcError::SealRefused { .. })
    ));
  }

  /// A-187: each rendezvous has keys of its own. Do: agree two exchanges and seal one plaintext as the first message of
  /// each. Expect: the two sealed messages differ.
  #[test]
  fn each_rendezvous_has_keys_of_its_own() {
    let (mut first, _) = agreed();
    let (mut second, _) = agreed();
    assert_ne!(
      first.seal(Class::Hello, &[0; 4]).unwrap(),
      second.seal(Class::Hello, &[0; 4]).unwrap()
    );
  }

  /// Format: RFC 7748 §6.1's test vector: Alice's and Bob's public keys and their shared secret.
  const ALICE_PUBLIC: &str = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";
  const BOB_PUBLIC: &str = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f";
  const SHARED: &str = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";

  fn bytes<const N: usize>(hex: &str) -> [u8; N] {
    let mut out = [0u8; N];
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
      *byte = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    out
  }

  fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
  }

  /// A-187's golden vector: the keys and the first message are pinned, so the labels, the salt's order, the nonce's
  /// layout and the additional data cannot change unseen. Do: key a client and a daemon from RFC 7748's shared secret,
  /// Alice the client and Bob the daemon, and seal the hello `07000000` from the client and the handoff of twelve zero
  /// bytes from the daemon. Expect: the pinned bytes, each opening on the other side.
  #[test]
  fn the_golden_vector_holds() {
    let (alice, bob, shared) = (
      bytes::<32>(ALICE_PUBLIC),
      bytes::<32>(BOB_PUBLIC),
      bytes::<32>(SHARED),
    );
    let mut client = channel(&shared, (&alice, &bob), Role::Client).unwrap();
    let mut daemon = channel(&shared, (&alice, &bob), Role::Daemon).unwrap();
    let hello = client.seal(Class::Hello, &7u32.to_le_bytes()).unwrap();
    let handoff = daemon.seal(Class::Handoff, &[0u8; 12]).unwrap();
    assert_eq!(hex(&hello), GOLDEN_HELLO);
    assert_eq!(hex(&handoff), GOLDEN_HANDOFF);
    assert_eq!(
      daemon.open(Class::Hello, &hello).unwrap(),
      7u32.to_le_bytes()
    );
    assert_eq!(client.open(Class::Handoff, &handoff).unwrap(), [0u8; 12]);
  }

  /// A-187: the keys are HKDF-SHA256 as RFC 5869 defines it, checked against an implementation that shares no code
  /// with aws-lc-rs: Python's standard `hmac` and `hashlib`, RFC 5869's two steps written out, run on 2026-10-28 over
  /// RFC 7748's shared secret salted with Alice's then Bob's public key. Do: derive both keys here. Expect: the bytes
  /// that computation gave.
  #[test]
  fn the_keys_are_rfc_5869s_hkdf() {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(&bytes::<32>(ALICE_PUBLIC));
    salt[32..].copy_from_slice(&bytes::<32>(BOB_PUBLIC));
    let shared = bytes::<32>(SHARED);
    assert_eq!(
      hex(&key_bytes(&shared, &salt, CLIENT_TO_DAEMON).unwrap()),
      "f01fc67444927df8bde347ee23ce3ee3c23094d3a0829df1a7979dc2653f83f1"
    );
    assert_eq!(
      hex(&key_bytes(&shared, &salt, DAEMON_TO_CLIENT).unwrap()),
      "f9a8671c68f4565735830ac034a8547725ed502dfae0345cedc6efcb1983ff02"
    );
  }

  /// Format: the golden hello, sealed as [`the_golden_vector_holds`] says.
  const GOLDEN_HELLO: &str = "12847133d3ab48e955c0d58d9b2c2437dc834b21";
  /// Format: the golden handoff, sealed as [`the_golden_vector_holds`] says.
  const GOLDEN_HANDOFF: &str = "9eded3aa75035087b187d44808fdd925323b0ee9512afe82eb24e1bb";
}

//! A client written for the tests from the RFCs alone (A-594). It shares no code with the server's identification,
//! KEXINIT writer, negotiation, exchange hash, key derivation or packet sealing: it builds its own messages, pads and
//! seals its own packets over AWS-LC's primitives, computes the exchange hash from RFC 5656 §4's list, derives keys by
//! RFC 4253 §7.2's formula, and verifies the server's Ed25519 signature over the hash it computed itself. A server that
//! completes an exchange with it, and whose packets it opens, agrees with the documents rather than with itself. It
//! panics on anything it does not expect, which fails the test that drove it.

use aws_lc_rs::aead::chacha20_poly1305_openssh::{
  OpeningKey as ChaChaOpen, SealingKey as ChaChaSeal,
};
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use aws_lc_rs::agreement::{self, PrivateKey, UnparsedPublicKey as AgreementPublicKey, X25519};
use aws_lc_rs::digest::{self, SHA256};
use aws_lc_rs::signature::{self, UnparsedPublicKey};

/// What the client offers, and how it behaves.
#[derive(Clone, Debug)]
pub(crate) struct ClientOptions {
  /// `V_C`, without CR LF.
  pub(crate) identification: Vec<u8>,
  pub(crate) kex: Vec<String>,
  pub(crate) host_keys: Vec<String>,
  pub(crate) ciphers_client_to_server: Vec<String>,
  pub(crate) ciphers_server_to_client: Vec<String>,
  pub(crate) macs: Vec<String>,
  pub(crate) compression: Vec<String>,
  /// The ephemeral private key of each exchange, in order; later exchanges use keys of their own.
  pub(crate) private_keys: Vec<[u8; 32]>,
  pub(crate) cookie: [u8; 16],
  /// Whether the first KEXINIT says a guessed packet follows.
  pub(crate) first_kex_packet_follows: bool,
  /// The guessed packet's payload; none sends the real `SSH_MSG_KEX_ECDH_INIT` as the guess.
  pub(crate) guess: Option<Vec<u8>>,
  /// Whether the client answers a KEXINIT the server starts; when false it ignores the server's rekeying.
  pub(crate) answer_kexinit: bool,
}

impl ClientOptions {
  /// Like OpenSSH 10's client choosing `cipher` both ways: `curve25519-sha256`, `ext-info-c`, and the strict marker when
  /// `strict`.
  pub(crate) fn openssh_like(cipher: &str, strict: bool) -> Self {
    let mut kex = vec!["curve25519-sha256".to_string(), "ext-info-c".to_string()];
    if strict {
      kex.push("kex-strict-c-v00@openssh.com".to_string());
    }
    Self {
      identification: b"SSH-2.0-OpenSSH_10.2".to_vec(),
      kex,
      host_keys: vec!["ssh-ed25519".to_string()],
      ciphers_client_to_server: vec![cipher.to_string()],
      ciphers_server_to_client: vec![cipher.to_string()],
      macs: vec!["hmac-sha2-256-etm@openssh.com".to_string()],
      compression: vec!["none".to_string(), "zlib@openssh.com".to_string()],
      private_keys: vec![RFC7748_BOB_PRIVATE],
      cookie: [0x5a; 16],
      first_kex_packet_follows: false,
      guess: None,
      answer_kexinit: true,
    }
  }
}

/// RFC 7748 §6.1's Bob: the client's first ephemeral private key in the tests.
pub(crate) const RFC7748_BOB_PRIVATE: [u8; 32] = [
  0x5d, 0xab, 0x08, 0x7e, 0x62, 0x4a, 0x8a, 0x4b, 0x79, 0xe1, 0x7f, 0x8b, 0x83, 0x80, 0x0e, 0xe6,
  0x6f, 0x3b, 0xb1, 0x29, 0x26, 0x18, 0xb6, 0xfd, 0x1c, 0x2f, 0x8b, 0x27, 0xff, 0x88, 0xe0, 0xeb,
];

/// How the client seals one direction.
enum Seal {
  Clear,
  Gcm {
    key: LessSafeKey,
    fixed: [u8; 4],
    counter: u64,
  },
  ChaCha(ChaChaSeal),
}

/// How the client opens one direction.
enum Open {
  Clear,
  Gcm {
    key: LessSafeKey,
    fixed: [u8; 4],
    counter: u64,
  },
  ChaCha(ChaChaOpen),
}

/// The server's KEXINIT as the client reads it.
#[derive(Clone, Debug)]
struct ServerLists {
  lists: Vec<Vec<String>>,
}

/// The client.
pub(crate) struct Client {
  options: ClientOptions,
  output: Vec<u8>,
  inbound: Vec<u8>,
  pub(crate) server_identification: Option<Vec<u8>>,
  pub(crate) banner_lines: Vec<Vec<u8>>,
  send_sequence: u32,
  receive_sequence: u32,
  seal: Seal,
  open: Open,
  next_open: Option<Open>,
  kexinit: Vec<u8>,
  pub(crate) server_kexinit: Option<Vec<u8>>,
  in_exchange: bool,
  exchanges: usize,
  private: [u8; 32],
  ecdh_sent: bool,
  guessed: bool,
  pub(crate) strict: bool,
  pub(crate) session_id: Option<[u8; 32]>,
  pub(crate) exchange_hashes: Vec<[u8; 32]>,
  pub(crate) host_key: Option<Vec<u8>>,
  /// Payloads the client opened that it does not handle itself (everything but KEXINIT, the reply and NEWKEYS).
  pub(crate) received: Vec<Vec<u8>>,
  /// For each payload in `received`, how many of the server's NEWKEYS had come before it.
  pub(crate) received_epochs: Vec<usize>,
  pub(crate) server_newkeys: usize,
  pub(crate) cipher_client_to_server: String,
  pub(crate) cipher_server_to_client: String,
}

impl Client {
  /// A client that has sent its identification and first KEXINIT (and its guess, when it guesses).
  pub(crate) fn new(options: ClientOptions) -> Self {
    let mut client = Self {
      options,
      output: Vec::new(),
      inbound: Vec::new(),
      server_identification: None,
      banner_lines: Vec::new(),
      send_sequence: 0,
      receive_sequence: 0,
      seal: Seal::Clear,
      open: Open::Clear,
      next_open: None,
      kexinit: Vec::new(),
      server_kexinit: None,
      in_exchange: false,
      exchanges: 0,
      private: [0; 32],
      ecdh_sent: false,
      guessed: false,
      strict: false,
      session_id: None,
      exchange_hashes: Vec::new(),
      host_key: None,
      received: Vec::new(),
      received_epochs: Vec::new(),
      server_newkeys: 0,
      cipher_client_to_server: String::new(),
      cipher_server_to_client: String::new(),
    };
    let identification = client.options.identification.clone();
    client.output.extend_from_slice(&identification);
    client.output.extend_from_slice(b"\r\n");
    client.send_kexinit();
    if client.options.first_kex_packet_follows {
      client.guessed = true;
      match client.options.guess.clone() {
        Some(guess) => client.send(&guess),
        None => client.send_ecdh_init(),
      }
    }
    client
  }

  /// Bytes the client has sent since the last call.
  pub(crate) fn take_output(&mut self) -> Vec<u8> {
    core::mem::take(&mut self.output)
  }

  /// Starts a key exchange of the client's own.
  pub(crate) fn start_rekey(&mut self) {
    self.send_kexinit();
  }

  /// Reads the server's bytes.
  pub(crate) fn feed(&mut self, bytes: &[u8]) {
    self.inbound.extend_from_slice(bytes);
    if self.server_identification.is_none() {
      self.read_lines();
    }
    if self.server_identification.is_some() {
      while self.read_packet() {}
    }
  }

  fn read_lines(&mut self) {
    while let Some(end) = self.inbound.iter().position(|byte| *byte == b'\n') {
      let line: Vec<u8> = self.inbound.drain(..=end).collect();
      assert!(
        line.ends_with(b"\r\n"),
        "every server line ends CR LF: {line:?}"
      );
      let line = line[..line.len() - 2].to_vec();
      if line.starts_with(b"SSH-") {
        self.server_identification = Some(line);
        return;
      }
      self.banner_lines.push(line);
    }
  }

  /// Opens one whole packet from the inbound bytes, if there is one.
  fn read_packet(&mut self) -> bool {
    if self.inbound.len() < 4 {
      return false;
    }
    let head: [u8; 4] = self.inbound[..4].try_into().unwrap();
    let (length, tag_len, alignment, length_aligned) = match &self.open {
      Open::Clear => (u32::from_be_bytes(head) as usize, 0, 8, true),
      Open::Gcm { .. } => (u32::from_be_bytes(head) as usize, 16, 16, false),
      Open::ChaCha(key) => (
        u32::from_be_bytes(key.decrypt_packet_length(self.receive_sequence, head)) as usize,
        16,
        8,
        false,
      ),
    };
    let aligned = if length_aligned { length + 4 } else { length };
    assert_eq!(aligned % alignment, 0, "the server's packet is aligned");
    let total = 4 + length + tag_len;
    if self.inbound.len() < total {
      return false;
    }
    let mut packet: Vec<u8> = self.inbound.drain(..total).collect();
    self.open_packet(&mut packet, length);
    let padding = packet[4] as usize;
    assert!(padding >= 4, "the server pads with at least four bytes");
    let payload = packet[5..4 + length - padding].to_vec();
    self.receive_sequence = self.receive_sequence.wrapping_add(1);
    self.on_payload(payload);
    true
  }

  fn open_packet(&mut self, packet: &mut [u8], length: usize) {
    match &mut self.open {
      Open::Clear => {}
      Open::Gcm {
        key,
        fixed,
        counter,
      } => {
        let nonce = gcm_nonce(fixed, *counter);
        *counter = counter.wrapping_add(1);
        let head: [u8; 4] = packet[..4].try_into().unwrap();
        let (body, tag) = packet[4..].split_at_mut(length);
        key
          .open_in_place_separate_tag(nonce, Aad::from(head), tag, body)
          .expect("the server's GCM packet opens");
      }
      Open::ChaCha(key) => {
        let (sealed, tag) = packet.split_at_mut(4 + length);
        let tag: &[u8; 16] = (&*tag).try_into().unwrap();
        key
          .open_in_place(self.receive_sequence, sealed, tag)
          .expect("the server's ChaCha20-Poly1305 packet opens");
      }
    }
  }

  fn on_payload(&mut self, payload: Vec<u8>) {
    match payload[0] {
      20 => self.on_server_kexinit(payload),
      31 => self.on_reply(&payload),
      21 => self.on_server_newkeys(),
      _ => {
        self.received.push(payload);
        self.received_epochs.push(self.server_newkeys);
      }
    }
  }

  fn on_server_kexinit(&mut self, payload: Vec<u8>) {
    if !self.options.answer_kexinit && self.exchanges > 0 && !self.in_exchange {
      self.received.push(payload);
      self.received_epochs.push(self.server_newkeys);
      return;
    }
    let server = parse_kexinit(&payload);
    self.server_kexinit = Some(payload);
    if !self.in_exchange {
      self.send_kexinit();
    }
    let ours = self.lists();
    let (Some(client_to_server), Some(server_to_client)) = (
      first_shared(&ours[2], &server.lists[2]),
      first_shared(&ours[3], &server.lists[3]),
    ) else {
      return;
    };
    self.cipher_client_to_server = client_to_server;
    self.cipher_server_to_client = server_to_client;
    if self.exchanges == 1 {
      let pairs = [
        (
          "kex-strict-c-v00@openssh.com",
          "kex-strict-s-v00@openssh.com",
        ),
        ("kex-strict-c", "kex-strict-s"),
      ];
      self.strict = pairs.iter().any(|(client, server_marker)| {
        ours[0].iter().any(|name| name == client)
          && server.lists[0].iter().any(|name| name == server_marker)
      });
    }
    let guess_right =
      ours[0].first() == server.lists[0].first() && ours[1].first() == server.lists[1].first();
    let guess_stands = self.guessed && guess_right && self.options.guess.is_none();
    if !self.ecdh_sent || !guess_stands {
      self.send_ecdh_init();
    }
    self.guessed = false;
  }

  fn on_reply(&mut self, payload: &[u8]) {
    let mut at = 1;
    let host_key = read_string(payload, &mut at);
    let server_public = read_string(payload, &mut at);
    let signature = read_string(payload, &mut at);
    assert_eq!(at, payload.len(), "the reply has no trailing bytes");
    assert_eq!(
      server_public.len(),
      32,
      "the server's X25519 key is 32 bytes"
    );
    let private = PrivateKey::from_private_key(&X25519, &self.private).unwrap();
    let client_public = private.compute_public_key().unwrap().as_ref().to_vec();
    let shared = agreement::agree(
      &private,
      AgreementPublicKey::new(&X25519, &server_public),
      (),
      |x| Ok(x.to_vec()),
    )
    .unwrap();
    let k = mpint(&shared);
    let mut hashed = Vec::new();
    let fields: [&[u8]; 7] = [
      &self.options.identification[..],
      self.server_identification.as_deref().unwrap(),
      &self.kexinit[..],
      self.server_kexinit.as_deref().unwrap(),
      &host_key[..],
      &client_public[..],
      &server_public[..],
    ];
    for field in fields {
      put_string(&mut hashed, field);
    }
    hashed.extend_from_slice(&k);
    let hash: [u8; 32] = digest::digest(&SHA256, &hashed)
      .as_ref()
      .try_into()
      .unwrap();
    verify_host_signature(&host_key, &hash, &signature);
    let session_id = *self.session_id.get_or_insert(hash);
    self.exchange_hashes.push(hash);
    self.host_key = Some(host_key);
    self.send(&[21]);
    let c2s = self.cipher_client_to_server.clone();
    let s2c = self.cipher_server_to_client.clone();
    self.seal = seal_for(
      &c2s,
      &kdf(&k, &hash, b'C', &session_id, key_len(&c2s)),
      &kdf(&k, &hash, b'A', &session_id, iv_len(&c2s)),
    );
    self.next_open = Some(open_for(
      &s2c,
      &kdf(&k, &hash, b'D', &session_id, key_len(&s2c)),
      &kdf(&k, &hash, b'B', &session_id, iv_len(&s2c)),
    ));
    if self.strict {
      self.send_sequence = 0;
    }
  }

  fn on_server_newkeys(&mut self) {
    self.open = self
      .next_open
      .take()
      .expect("the server's NEWKEYS follows its reply");
    if self.strict {
      self.receive_sequence = 0;
    }
    self.in_exchange = false;
    self.ecdh_sent = false;
    self.server_newkeys += 1;
  }

  /// The client's lists, in RFC 4253 §7.1's order (markers left out after the first exchange).
  fn lists(&self) -> Vec<Vec<String>> {
    let markers = |name: &String| {
      !(self.exchanges > 1 && (name.starts_with("kex-strict") || name.starts_with("ext-info")))
    };
    vec![
      self
        .options
        .kex
        .iter()
        .filter(|name| markers(name))
        .cloned()
        .collect(),
      self.options.host_keys.clone(),
      self.options.ciphers_client_to_server.clone(),
      self.options.ciphers_server_to_client.clone(),
      self.options.macs.clone(),
      self.options.macs.clone(),
      self.options.compression.clone(),
      self.options.compression.clone(),
      Vec::new(),
      Vec::new(),
    ]
  }

  fn send_kexinit(&mut self) {
    self.exchanges += 1;
    self.in_exchange = true;
    self.ecdh_sent = false;
    self.private = self
      .options
      .private_keys
      .get(self.exchanges - 1)
      .copied()
      .unwrap_or([self.exchanges as u8; 32]);
    let mut payload = vec![20];
    let mut cookie = self.options.cookie;
    cookie[0] ^= self.exchanges as u8;
    payload.extend_from_slice(&cookie);
    for list in self.lists() {
      put_string(&mut payload, list.join(",").as_bytes());
    }
    payload.push(u8::from(
      self.exchanges == 1 && self.options.first_kex_packet_follows,
    ));
    payload.extend_from_slice(&0u32.to_be_bytes());
    self.kexinit = payload.clone();
    self.send(&payload);
  }

  fn send_ecdh_init(&mut self) {
    let private = PrivateKey::from_private_key(&X25519, &self.private).unwrap();
    let public = private.compute_public_key().unwrap().as_ref().to_vec();
    let mut payload = vec![30];
    put_string(&mut payload, &public);
    self.ecdh_sent = true;
    self.send(&payload);
  }

  /// Seals `payload` as the client's next packet.
  pub(crate) fn send(&mut self, payload: &[u8]) {
    let (alignment, length_aligned) = match self.seal {
      Seal::Clear => (8, true),
      Seal::Gcm { .. } => (16, false),
      Seal::ChaCha(_) => (8, false),
    };
    let aligned = 1 + payload.len() + if length_aligned { 4 } else { 0 };
    let mut padding = alignment - aligned % alignment;
    if padding < 4 {
      padding += alignment;
    }
    let length = (1 + payload.len() + padding) as u32;
    let mut packet = length.to_be_bytes().to_vec();
    packet.push(padding as u8);
    packet.extend_from_slice(payload);
    packet.extend(std::iter::repeat_n(0u8, padding));
    match &mut self.seal {
      Seal::Clear => {}
      Seal::Gcm {
        key,
        fixed,
        counter,
      } => {
        let nonce = gcm_nonce(fixed, *counter);
        *counter = counter.wrapping_add(1);
        let tag = key
          .seal_in_place_separate_tag(nonce, Aad::from(length.to_be_bytes()), &mut packet[4..])
          .unwrap();
        packet.extend_from_slice(tag.as_ref());
      }
      Seal::ChaCha(key) => {
        let mut tag = [0u8; 16];
        key.seal_in_place(self.send_sequence, &mut packet, &mut tag);
        packet.extend_from_slice(&tag);
      }
    }
    self.send_sequence = self.send_sequence.wrapping_add(1);
    self.output.extend_from_slice(&packet);
  }

  /// The sequence number of the client's next packet.
  pub(crate) fn send_sequence(&self) -> u32 {
    self.send_sequence
  }
}

/// RFC 5647 §7.1's nonce: the fixed field, then the invocation counter.
fn gcm_nonce(fixed: &[u8; 4], counter: u64) -> Nonce {
  let mut bytes = [0u8; 12];
  bytes[..4].copy_from_slice(fixed);
  bytes[4..].copy_from_slice(&counter.to_be_bytes());
  Nonce::assume_unique_for_key(bytes)
}

fn key_len(cipher: &str) -> usize {
  match cipher {
    "chacha20-poly1305@openssh.com" => 64,
    "aes256-gcm@openssh.com" => 32,
    other => panic!("the client runs no cipher {other}"),
  }
}

fn iv_len(cipher: &str) -> usize {
  match cipher {
    "chacha20-poly1305@openssh.com" => 0,
    _ => 12,
  }
}

fn seal_for(cipher: &str, key: &[u8], iv: &[u8]) -> Seal {
  match cipher {
    "chacha20-poly1305@openssh.com" => Seal::ChaCha(ChaChaSeal::new(key.try_into().unwrap())),
    _ => Seal::Gcm {
      key: LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).unwrap()),
      fixed: iv[..4].try_into().unwrap(),
      counter: u64::from_be_bytes(iv[4..].try_into().unwrap()),
    },
  }
}

fn open_for(cipher: &str, key: &[u8], iv: &[u8]) -> Open {
  match cipher {
    "chacha20-poly1305@openssh.com" => Open::ChaCha(ChaChaOpen::new(key.try_into().unwrap())),
    _ => Open::Gcm {
      key: LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).unwrap()),
      fixed: iv[..4].try_into().unwrap(),
      counter: u64::from_be_bytes(iv[4..].try_into().unwrap()),
    },
  }
}

/// RFC 4253 §7.2: `HASH(K || H || X || session_id)`, extended by `HASH(K || H || key so far)`.
pub(crate) fn kdf(k: &[u8], h: &[u8], letter: u8, session_id: &[u8], length: usize) -> Vec<u8> {
  let mut input = Vec::new();
  input.extend_from_slice(k);
  input.extend_from_slice(h);
  input.push(letter);
  input.extend_from_slice(session_id);
  let mut key = digest::digest(&SHA256, &input).as_ref().to_vec();
  while key.len() < length {
    let mut input = Vec::new();
    input.extend_from_slice(k);
    input.extend_from_slice(h);
    input.extend_from_slice(&key);
    key.extend_from_slice(digest::digest(&SHA256, &input).as_ref());
  }
  key.truncate(length);
  key
}

/// RFC 4251 §5's `mpint` of an unsigned big-endian magnitude, its length included.
pub(crate) fn mpint(magnitude: &[u8]) -> Vec<u8> {
  let first = magnitude
    .iter()
    .position(|byte| *byte != 0)
    .unwrap_or(magnitude.len());
  let mut body = Vec::new();
  if magnitude.get(first).is_some_and(|byte| byte & 0x80 != 0) {
    body.push(0);
  }
  body.extend_from_slice(&magnitude[first..]);
  let mut out = (body.len() as u32).to_be_bytes().to_vec();
  out.extend_from_slice(&body);
  out
}

pub(crate) fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
  out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
  out.extend_from_slice(bytes);
}

pub(crate) fn read_string(payload: &[u8], at: &mut usize) -> Vec<u8> {
  let length = u32::from_be_bytes(payload[*at..*at + 4].try_into().unwrap()) as usize;
  let value = payload[*at + 4..*at + 4 + length].to_vec();
  *at += 4 + length;
  value
}

fn parse_kexinit(payload: &[u8]) -> ServerLists {
  assert_eq!(payload[0], 20);
  let mut at = 17;
  let lists = (0..10)
    .map(|_| {
      let list = read_string(payload, &mut at);
      if list.is_empty() {
        Vec::new()
      } else {
        String::from_utf8(list)
          .unwrap()
          .split(',')
          .map(str::to_string)
          .collect()
      }
    })
    .collect();
  assert_eq!(
    payload.len(),
    at + 5,
    "a KEXINIT ends with its boolean and reserved word"
  );
  ServerLists { lists }
}

fn first_shared(client: &[String], server: &[String]) -> Option<String> {
  client.iter().find(|name| server.contains(name)).cloned()
}

/// Checks the reply's host key and signature blobs (RFC 8709 §4, §6) and the signature over `hash`.
fn verify_host_signature(host_key: &[u8], hash: &[u8], signature: &[u8]) {
  let mut at = 0;
  assert_eq!(read_string(host_key, &mut at), b"ssh-ed25519");
  let public = read_string(host_key, &mut at);
  assert_eq!(at, host_key.len());
  let mut at = 0;
  assert_eq!(read_string(signature, &mut at), b"ssh-ed25519");
  let raw = read_string(signature, &mut at);
  assert_eq!(at, signature.len());
  UnparsedPublicKey::new(&signature::ED25519, &public)
    .verify(hash, &raw)
    .expect("the server's host key signs the exchange hash the client computed");
}

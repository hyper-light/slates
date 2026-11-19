//! Algorithm negotiation (A-594; RFC 4253 §7.1; `draft-ietf-sshm-strict-kex-03` §3.1; `draft-miller-sshm-aes-gcm-01` §2;
//! `draft-ietf-sshm-chacha20-poly1305-04` §5; RFC 8308 §2.1).
//!
//! Each side sends `SSH_MSG_KEXINIT`: a random cookie, ten name-lists in order of preference, whether a guessed exchange
//! packet follows, and a reserved word. For each list, the algorithm used is "the first algorithm on the client's
//! name-list that is also on the server's name-list" [B: RFC 4253 §7.1], and a list with none ends the connection, which
//! [`negotiate`] refuses by the list's kind.
//!
//! **What the server offers.** Key exchange `curve25519-sha256` (RFC 8731); host key `ssh-ed25519` (RFC 8709); ciphers
//! `chacha20-poly1305@openssh.com` and `aes256-gcm@openssh.com`, both AEADs, so MAC negotiation is skipped: "When an AEAD
//! mode is selected ..., MAC negotiation MUST be skipped and the contents of the "mac_algorithms" field ignored" [B: the
//! AES-GCM draft §2; the chacha draft §5 says the same]. Its own MAC lists still name one algorithm, since RFC 4253 §7.1
//! says "Each name-list MUST contain at least one algorithm name", and that name is `none`: the server runs no MAC apart
//! from its ciphers, so `none` is the honest entry, and it is never in use. Compression is `none` alone, and the language
//! lists are empty, as §7.1 says they "SHOULD" be.
//!
//! **Markers are not methods.** The first KEXINIT's key exchange list also carries markers: the server's
//! `kex-strict-s-v00@openssh.com` and `kex-strict-s`, both, as the strict-kex draft §3.1 advises ("SHOULD offer both the
//! standard and pre-standard names"); and the client may list `kex-strict-c-v00@openssh.com`, `kex-strict-c` and
//! RFC 8308's `ext-info-c`. A marker is matched against the methods the server runs, never against its advertised list,
//! so no marker is ever chosen as a method, even one a client lists first or a client that lists the server's own
//! markers. Strict key exchange is on when the client lists either client marker, each paired with the server marker of
//! its own form, which the server always offers; markers in a later KEXINIT are ignored (§3.1, "MUST be ignored").
//!
//! **A guess.** A client may send its first exchange packet before seeing the server's KEXINIT. The guess "is considered
//! wrong if ... the kex algorithm and/or the host key algorithm is guessed wrong (server and client have different
//! preferred algorithm)" [B: RFC 4253 §7], judged as OpenSSH judges it, by the first names of the two lists
//! (`proposals_match` in `kex.c` at `fa4b2a82`) [C]; a wrong guess's packet is ignored.

use crate::hostkey::ED25519;
use crate::packet::Cipher;
use crate::wire::{
  NameList, Reader, WireRefusal, put_boolean, put_byte, put_name_list, put_uint32,
};

/// `SSH_MSG_KEXINIT` (RFC 4250 §4.1.2).
pub const KEXINIT: u8 = 20;

/// The bytes of a KEXINIT's cookie (RFC 4253 §7.1, `byte[16] cookie`).
pub const COOKIE_LEN: usize = 16;

/// The key exchange method the server runs (RFC 8731 §3).
pub const CURVE25519_SHA256: &str = "curve25519-sha256";

/// The server's markers of strict key exchange: the pre-standard name, then the standard one (strict-kex draft §3.1).
pub const STRICT_SERVER: [&str; 2] = ["kex-strict-s-v00@openssh.com", "kex-strict-s"];

/// The client's markers, each paired with the server's of the same form.
pub const STRICT_CLIENT: [&str; 2] = ["kex-strict-c-v00@openssh.com", "kex-strict-c"];

/// The client's marker of RFC 8308's extension negotiation (§2.1).
pub const EXT_INFO_CLIENT: &str = "ext-info-c";

/// The algorithm named "none" (RFC 4253 §6.2, §6.4).
pub const NONE: &str = "none";

/// The key exchange methods the server runs, in its order of preference.
const KEX_METHODS: [&str; 1] = [CURVE25519_SHA256];

/// The host key algorithms the server has keys for.
const HOST_KEYS: [&str; 1] = [ED25519];

/// The compression the server runs.
const COMPRESSION: [&str; 1] = [NONE];

/// The MAC lists' one entry: every cipher the server offers is an AEAD, so no MAC is ever negotiated.
const MACS: [&str; 1] = [NONE];

/// The ten name-lists of a KEXINIT, in RFC 4253 §7.1's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum List {
  /// `kex_algorithms`.
  Kex,
  /// `server_host_key_algorithms`.
  HostKey,
  /// `encryption_algorithms_client_to_server`.
  CipherClientToServer,
  /// `encryption_algorithms_server_to_client`.
  CipherServerToClient,
  /// `mac_algorithms_client_to_server`.
  MacClientToServer,
  /// `mac_algorithms_server_to_client`.
  MacServerToClient,
  /// `compression_algorithms_client_to_server`.
  CompressionClientToServer,
  /// `compression_algorithms_server_to_client`.
  CompressionServerToClient,
  /// `languages_client_to_server`.
  LanguageClientToServer,
  /// `languages_server_to_client`.
  LanguageServerToClient,
}

impl List {
  /// The lists in their order on the wire.
  pub const ALL: [List; 10] = [
    List::Kex,
    List::HostKey,
    List::CipherClientToServer,
    List::CipherServerToClient,
    List::MacClientToServer,
    List::MacServerToClient,
    List::CompressionClientToServer,
    List::CompressionServerToClient,
    List::LanguageClientToServer,
    List::LanguageServerToClient,
  ];
}

/// Why a KEXINIT was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KexInitRefusal {
  /// Its fields did not read.
  Malformed(WireRefusal),
  /// Its message number is not `SSH_MSG_KEXINIT`.
  NotKexInit,
  /// No algorithm on the list is one both sides have.
  NoCommonAlgorithm(List),
}

/// A KEXINIT read from its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KexInit<'a> {
  /// The sender's cookie.
  pub cookie: &'a [u8],
  /// The ten name-lists, in [`List::ALL`]'s order.
  lists: [NameList<'a>; 10],
  /// Whether a guessed exchange packet follows.
  pub first_kex_packet_follows: bool,
}

impl<'a> KexInit<'a> {
  /// Reads a KEXINIT payload, the message number included.
  pub fn read(payload: &'a [u8]) -> Result<Self, KexInitRefusal> {
    let mut reader = Reader::new(payload);
    if reader.byte().map_err(KexInitRefusal::Malformed)? != KEXINIT {
      return Err(KexInitRefusal::NotKexInit);
    }
    let cookie = reader.take(COOKIE_LEN).map_err(KexInitRefusal::Malformed)?;
    let mut read_list = || reader.name_list().map_err(KexInitRefusal::Malformed);
    let lists = [
      read_list()?,
      read_list()?,
      read_list()?,
      read_list()?,
      read_list()?,
      read_list()?,
      read_list()?,
      read_list()?,
      read_list()?,
      read_list()?,
    ];
    let first_kex_packet_follows = reader.boolean().map_err(KexInitRefusal::Malformed)?;
    reader.uint32().map_err(KexInitRefusal::Malformed)?;
    reader.finish().map_err(KexInitRefusal::Malformed)?;
    Ok(Self {
      cookie,
      lists,
      first_kex_packet_follows,
    })
  }

  /// One of its name-lists.
  pub fn list(&self, list: List) -> NameList<'a> {
    let [
      kex,
      host_key,
      cipher_in,
      cipher_out,
      mac_in,
      mac_out,
      compression_in,
      compression_out,
      language_in,
      language_out,
    ] = self.lists;
    match list {
      List::Kex => kex,
      List::HostKey => host_key,
      List::CipherClientToServer => cipher_in,
      List::CipherServerToClient => cipher_out,
      List::MacClientToServer => mac_in,
      List::MacServerToClient => mac_out,
      List::CompressionClientToServer => compression_in,
      List::CompressionServerToClient => compression_out,
      List::LanguageClientToServer => language_in,
      List::LanguageServerToClient => language_out,
    }
  }
}

/// The server's KEXINIT payload with `cookie`; the first one carries the strict key exchange markers.
pub fn server_kexinit(cookie: &[u8; COOKIE_LEN], initial: bool) -> Result<Vec<u8>, WireRefusal> {
  let mut kex: Vec<&str> = KEX_METHODS.to_vec();
  if initial {
    kex.extend_from_slice(&STRICT_SERVER);
  }
  let ciphers: Vec<&str> = Cipher::OFFERED.iter().map(|cipher| cipher.name()).collect();
  let mut payload = Vec::new();
  put_byte(&mut payload, KEXINIT);
  payload.extend_from_slice(cookie);
  put_name_list(&mut payload, &kex)?;
  put_name_list(&mut payload, &HOST_KEYS)?;
  put_name_list(&mut payload, &ciphers)?;
  put_name_list(&mut payload, &ciphers)?;
  put_name_list(&mut payload, &MACS)?;
  put_name_list(&mut payload, &MACS)?;
  put_name_list(&mut payload, &COMPRESSION)?;
  put_name_list(&mut payload, &COMPRESSION)?;
  put_name_list(&mut payload, &[])?;
  put_name_list(&mut payload, &[])?;
  put_boolean(&mut payload, false);
  put_uint32(&mut payload, 0);
  Ok(payload)
}

/// The key exchange method agreed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KexMethod {
  /// `curve25519-sha256`.
  Curve25519Sha256,
}

/// What a client's KEXINIT and the server's agreed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negotiated {
  /// The key exchange method.
  pub kex: KexMethod,
  /// The host key algorithm.
  pub host_key: &'static str,
  /// The cipher of what the client sends.
  pub client_to_server: Cipher,
  /// The cipher of what the server sends.
  pub server_to_client: Cipher,
  /// Whether strict key exchange is on (judged on the first exchange only).
  pub strict: bool,
  /// Whether the client listed `ext-info-c` (judged on the first exchange only).
  pub client_ext_info: bool,
  /// Whether the client sent a guessed packet that guessed wrong, so its next packet is ignored.
  pub skip_guessed_packet: bool,
}

/// Negotiates the client's KEXINIT against what the server runs; `initial` says whether this is the first exchange.
pub fn negotiate(client: &KexInit<'_>, initial: bool) -> Result<Negotiated, KexInitRefusal> {
  shared(client, List::Kex, &KEX_METHODS)?;
  shared(client, List::HostKey, &HOST_KEYS)?;
  let client_to_server = cipher(client, List::CipherClientToServer)?;
  let server_to_client = cipher(client, List::CipherServerToClient)?;
  shared(client, List::CompressionClientToServer, &COMPRESSION)?;
  shared(client, List::CompressionServerToClient, &COMPRESSION)?;
  let kex_list = client.list(List::Kex);
  Ok(Negotiated {
    kex: KexMethod::Curve25519Sha256,
    host_key: ED25519,
    client_to_server,
    server_to_client,
    strict: initial && STRICT_CLIENT.iter().any(|marker| kex_list.contains(marker)),
    client_ext_info: initial && kex_list.contains(EXT_INFO_CLIENT),
    skip_guessed_packet: client.first_kex_packet_follows && !guessed_right(client),
  })
}

/// The client's first name on `list` that `ours` also holds, or the list's refusal.
fn shared<'a>(client: &KexInit<'a>, list: List, ours: &[&str]) -> Result<&'a [u8], KexInitRefusal> {
  first_shared(client.list(list), ours).ok_or(KexInitRefusal::NoCommonAlgorithm(list))
}

/// The client's first cipher on `list` that the server runs, or the list's refusal.
fn cipher(client: &KexInit<'_>, list: List) -> Result<Cipher, KexInitRefusal> {
  client
    .list(list)
    .names()
    .find_map(Cipher::from_name)
    .ok_or(KexInitRefusal::NoCommonAlgorithm(list))
}

/// Whether the client's guess was right: its preferred key exchange and host key are the server's.
fn guessed_right(client: &KexInit<'_>) -> bool {
  let kex_first = client.list(List::Kex).first() == KEX_METHODS.first().map(|name| name.as_bytes());
  let host_key_first =
    client.list(List::HostKey).first() == HOST_KEYS.first().map(|name| name.as_bytes());
  kex_first && host_key_first
}

/// The client's first name that `ours` also holds.
fn first_shared<'a>(client: NameList<'a>, ours: &[&str]) -> Option<&'a [u8]> {
  client
    .names()
    .find(|name| ours.iter().any(|own| own.as_bytes() == *name))
}

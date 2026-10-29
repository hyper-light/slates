//! The seal a Linux rendezvous's messages cross under (§4.7, A-187). Every rendezvous, whether at the abstract name or
//! at a socket file in an operator's directory, opens with an exchange of ephemeral public keys, and every message after
//! it (the client's hello, the daemon's handoff, each delivery on the control socket) crosses sealed under a key for its
//! direction. Ada authorized socket files for DAX when what crosses them is encrypted end to end and at rest
//! (2026-10-22): a socket file holds no bytes at rest, and nothing crosses it in the clear. The abstract name takes the
//! same exchange, so the protocol is one, with no plain mode beside it.
//!
//! **Why an interface.** This crate builds for targets with no C toolchain (CI's cross-lint), and the one crypto library
//! (A-66, aws-lc-rs) is C. So the crate defines what a seal does and takes the implementation from its caller: the
//! daemon and the client install `slates-seal`'s once per process ([`install`]), before their first rendezvous. A
//! rendezvous with none installed is refused ([`IpcError::Unsupported`]), never sent in the clear.
//!
//! **What a seal does.** [`RendezvousSeal`] is one side's half of an exchange: its public key, and the agreement with the
//! other side's into a [`SealedChannel`], which seals what this side sends and opens what it receives, each direction
//! under its own key at a nonce that counts the messages, so a message replayed, dropped or reordered does not open.
//! Each message names its class ([`Class`]) as additional data, so a sealed message of one class never opens as another.

use std::sync::OnceLock;

use crate::error::IpcError;

/// Format: the bytes of one side's public key in the exchange (an X25519 point, RFC 7748).
pub const PUBLIC_KEY_BYTES: usize = 32;
/// Format: the bytes a seal adds to each message (an AES-256-GCM tag, NIST SP 800-38D).
pub const TAG_BYTES: usize = 16;

/// Which side of the rendezvous a seal is: the keys are derived per direction, so each side seals with the key the other
/// opens with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
  /// The side that connects.
  Client,
  /// The side that listens.
  Daemon,
}

/// The class of a sealed message, which it carries as additional data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
  /// The client's hello: the id it wants.
  Hello,
  /// The daemon's handoff, or its refusal of the claim.
  Handoff,
  /// A delivery on the control socket: a direct map's copy or a stream's end.
  Delivery,
}

/// Format: the byte a hello is sealed under.
const HELLO_CLASS: u8 = 1;
/// Format: the byte a handoff, or the daemon's refusal of the claim, is sealed under.
const HANDOFF_CLASS: u8 = 2;
/// Format: the byte a delivery is sealed under.
const DELIVERY_CLASS: u8 = 3;

impl Class {
  /// The byte the class is sealed under.
  #[must_use]
  pub fn byte(self) -> u8 {
    match self {
      Class::Hello => HELLO_CLASS,
      Class::Handoff => HANDOFF_CLASS,
      Class::Delivery => DELIVERY_CLASS,
    }
  }
}

/// One side's half of an exchange: made fresh for each rendezvous.
pub trait RendezvousSeal: Send {
  /// This side's public key, sent first.
  fn public_key(&self) -> [u8; PUBLIC_KEY_BYTES];

  /// Completes the exchange with the other side's public key, as `role`: the channel that seals this side's messages and
  /// opens the other's. Refused `SealRefused` where the key is not one the exchange accepts.
  fn agree(
    self: Box<Self>,
    peer: &[u8; PUBLIC_KEY_BYTES],
    role: Role,
  ) -> Result<Box<dyn SealedChannel>, IpcError>;
}

/// The two keys an exchange agreed, and where each direction's count of messages stands.
pub trait SealedChannel: Send {
  /// Seals `plaintext` of `class` as the next message this side sends: the plaintext's bytes and [`TAG_BYTES`] more.
  /// Refused `SealRefused` where the count of messages is spent.
  fn seal(&mut self, class: Class, plaintext: &[u8]) -> Result<Vec<u8>, IpcError>;

  /// Opens `sealed` of `class` as the next message the other side sent: its plaintext. Refused `SealRefused` where it
  /// does not open (another class, another key, a message replayed, dropped or reordered, any byte changed).
  fn open(&mut self, class: Class, sealed: &[u8]) -> Result<Vec<u8>, IpcError>;
}

/// Makes a fresh half of an exchange for each rendezvous.
pub type SealMaker = fn() -> Result<Box<dyn RendezvousSeal>, IpcError>;

/// The maker this process installed.
static MAKER: OnceLock<SealMaker> = OnceLock::new();

/// Installs `maker` as this process's seal maker, before its first rendezvous: whether it is the one installed (the first
/// install wins; a second of the same maker changes nothing).
pub fn install(maker: SealMaker) -> bool {
  let installed = MAKER.get_or_init(|| maker);
  std::ptr::fn_addr_eq(*installed, maker)
}

/// A fresh half of an exchange from the maker this process installed. Refused `Unsupported` where none is: a rendezvous
/// is never made in the clear.
pub fn fresh() -> Result<Box<dyn RendezvousSeal>, IpcError> {
  let maker = MAKER.get().ok_or(IpcError::Unsupported {
    feature: "a rendezvous seal (the process installed none; slates-seal installs one)",
  })?;
  maker()
}

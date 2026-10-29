//! A stand-in seal for this crate's own tests (§4.7, A-187). It frames, counts and refuses as the real one does: each
//! message carries a tag naming its class, its direction, the two public keys it was agreed between and its place in
//! its direction's count, and a message whose tag is not the next one expected does not open. It encrypts nothing; it
//! only scrambles each body, so a reader that skipped the open would read garbage rather than pass.
//!
//! Why a stand-in: this crate builds for targets with no C toolchain (CI's cross-lint, `--all-targets`), where the real
//! seal (`slates-seal`, aws-lc-rs) cannot build, so its tests cannot depend on it. The real seal's own tests hold its
//! cryptography to RFC 7748 and RFC 5869 vectors, and the client and daemon suites run every exchange under it.
//!
//! Shared by the module tests (`src/lib.rs` names this file) and the integration tests (`tests/rendezvous.rs`), each of
//! which uses only part of it.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};

use slates_ipc::IpcError;
use slates_ipc::seal::{
  self, Class, PUBLIC_KEY_BYTES, RendezvousSeal, Role, SealedChannel, TAG_BYTES,
};

/// The halves made in this process, which tells each one's key from every other's.
static MADE: AtomicU64 = AtomicU64::new(0);

/// Format: the byte each body is scrambled with.
const SCRAMBLE: u8 = 0x5a;

/// Installs the stand-in as this process's seal maker; every test that rendezvous calls it first.
pub(crate) fn install() {
  let _ = seal::install(make);
}

/// A fresh half: its key is this process's id and the count of halves made before it.
pub(crate) fn make() -> Result<Box<dyn RendezvousSeal>, IpcError> {
  let mut key = [0x11u8; PUBLIC_KEY_BYTES];
  let made = MADE.fetch_add(1, Ordering::Relaxed);
  let words = u64::from(std::process::id())
    .to_le_bytes()
    .into_iter()
    .chain(made.to_le_bytes());
  for (byte, word) in key.iter_mut().zip(words) {
    *byte = word;
  }
  Ok(Box::new(ToySeal { key }))
}

/// Two channels agreed with each other: the daemon's, then the client's.
pub(crate) type ChannelPair = (Box<dyn SealedChannel>, Box<dyn SealedChannel>);

/// Two channels agreed with each other, the daemon's first.
pub(crate) fn channel_pair() -> Result<ChannelPair, IpcError> {
  let daemon = make()?;
  let client = make()?;
  let (daemon_key, client_key) = (daemon.public_key(), client.public_key());
  Ok((
    daemon.agree(&client_key, Role::Daemon)?,
    client.agree(&daemon_key, Role::Client)?,
  ))
}

struct ToySeal {
  key: [u8; PUBLIC_KEY_BYTES],
}

impl RendezvousSeal for ToySeal {
  fn public_key(&self) -> [u8; PUBLIC_KEY_BYTES] {
    self.key
  }

  fn agree(
    self: Box<Self>,
    peer: &[u8; PUBLIC_KEY_BYTES],
    role: Role,
  ) -> Result<Box<dyn SealedChannel>, IpcError> {
    if peer.iter().all(|byte| *byte == 0) {
      return Err(IpcError::SealRefused {
        why: "an all-zero public key",
      });
    }
    let (client, daemon) = match role {
      Role::Client => (self.key, *peer),
      Role::Daemon => (*peer, self.key),
    };
    // The client's process and the daemon's count of halves: two exchanges in one process differ in the second.
    let mut binding = [0u8; 4];
    let sides = client.iter().zip(daemon.iter().skip(8));
    for (byte, (client_byte, daemon_byte)) in binding.iter_mut().zip(sides) {
      *byte = client_byte ^ daemon_byte.rotate_left(3);
    }
    Ok(Box::new(ToyChannel {
      role,
      binding,
      sent: 0,
      opened: 0,
    }))
  }
}

struct ToyChannel {
  role: Role,
  binding: [u8; 4],
  sent: u64,
  opened: u64,
}

impl ToyChannel {
  /// The tag of the message of `class` that `from` sends as its `count`th: the class, the direction, the binding and
  /// the count, in that order, the rest zero.
  fn tag(&self, class: Class, from: Role, count: u64) -> [u8; TAG_BYTES] {
    let mut tag = [0u8; TAG_BYTES];
    let fields = [class.byte(), u8::from(from == Role::Daemon)]
      .into_iter()
      .chain(self.binding)
      .chain(count.to_le_bytes());
    for (byte, field) in tag.iter_mut().zip(fields) {
      *byte = field;
    }
    tag
  }
}

impl SealedChannel for ToyChannel {
  fn seal(&mut self, class: Class, plaintext: &[u8]) -> Result<Vec<u8>, IpcError> {
    let mut sealed: Vec<u8> = plaintext.iter().map(|byte| byte ^ SCRAMBLE).collect();
    sealed.extend_from_slice(&self.tag(class, self.role, self.sent));
    self.sent = self.sent.saturating_add(1);
    Ok(sealed)
  }

  fn open(&mut self, class: Class, sealed: &[u8]) -> Result<Vec<u8>, IpcError> {
    let peer = match self.role {
      Role::Client => Role::Daemon,
      Role::Daemon => Role::Client,
    };
    let split = sealed
      .len()
      .checked_sub(TAG_BYTES)
      .ok_or(IpcError::SealRefused {
        why: "a message shorter than its tag",
      })?;
    let (body, tag) = sealed.split_at(split);
    if tag != self.tag(class, peer, self.opened) {
      return Err(IpcError::SealRefused {
        why: "a message that does not open",
      });
    }
    self.opened = self.opened.saturating_add(1);
    Ok(body.iter().map(|byte| byte ^ SCRAMBLE).collect())
  }
}

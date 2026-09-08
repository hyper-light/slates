//! The control-plane acceptance path (§4.10a §7, the enforcement order). Every datagram a host
//! receives runs the same ordered gauntlet, and the order is the security property: a length cap
//! before anything is parsed, then the cleartext prologue, then a **key lookup that drops an unknown
//! sender for free** (no AEAD is attempted on a datagram from a peer we hold no key for — garbage
//! without a key dies here, not at the tag), then AEAD verify-and-decrypt, then the envelope and the
//! replay check ([`crate::seal::Opener`] holds the replay high-water). Every refusal is typed and
//! names where it stopped.
//!
//! The keys come from an injected [`Keyring`] — enrollment (§4.13, owed) populates it, exactly as
//! the schedule takes an injected control secret and the seal an injected key. **Fencing** (the
//! epoch/term check that a datagram's `envelope.epoch` is current for its sender) is owed with the
//! membership state that knows each sender's current epoch; it slots in after AEAD and before the
//! datagram is handed up.

use crate::ControlDatagram;
use crate::seal::{Opener, SealError};

/// The receive-side key store: the opener for a `(sender, key_epoch)`, or `None` when this host holds
/// no key for that peer/epoch (so the datagram is dropped before any crypto). Enrollment populates
/// it; the acceptance path only reads it. An opener is stateful (it holds the replay high-water), so
/// the lookup returns `&mut`.
pub trait Keyring {
  /// The opener for `(sender, key_epoch)`, or `None` if this host is not keyed for it.
  fn opener(&mut self, sender: u64, key_epoch: u32) -> Option<&mut Opener>;
}

/// Why a received datagram was not accepted, naming the stage it stopped at (§4.10a §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The datagram exceeds the path's datagram cap; dropped before it is parsed.
  TooLong {
    /// The received length.
    len: usize,
    /// The cap.
    max: usize,
  },
  /// No key for the datagram's sender/epoch; dropped before any crypto is spent.
  UnknownSender {
    /// The cleartext sender.
    sender: u64,
    /// The cleartext key epoch.
    key_epoch: u32,
  },
  /// The datagram reached the crypto and was rejected (torn framing, bad tag, replay, malformed
  /// envelope) — the sealed path's typed error.
  Seal(SealError),
}

impl std::fmt::Display for Refusal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Refusal::TooLong { len, max } => {
        write!(f, "datagram of {len} bytes exceeds the {max}-byte cap")
      }
      Refusal::UnknownSender { sender, key_epoch } => {
        write!(f, "no key for sender {sender:#x} epoch {key_epoch}")
      }
      Refusal::Seal(e) => write!(f, "{e}"),
    }
  }
}

impl std::error::Error for Refusal {}

/// Runs the enforcement order over one received datagram: length cap → prologue → key lookup
/// (unknown ⇒ drop, zero crypto) → AEAD verify+decrypt+replay → the datagram. `max_len` is the
/// path's datagram cap (derived from the path MTU by the caller — not a constant here). The keyring
/// is consulted for the sender's opener; a miss is [`Refusal::UnknownSender`], returned before any
/// crypto touches the sealed region.
pub fn accept<K: Keyring>(
  bytes: &[u8],
  max_len: usize,
  keyring: &mut K,
) -> Result<ControlDatagram, Refusal> {
  if bytes.len() > max_len {
    return Err(Refusal::TooLong {
      len: bytes.len(),
      max: max_len,
    });
  }
  let (sender, key_epoch) = ControlDatagram::peek_routing(bytes).map_err(Refusal::Seal)?;
  let opener = keyring
    .opener(sender, key_epoch)
    .ok_or(Refusal::UnknownSender { sender, key_epoch })?;
  ControlDatagram::decode_sealed(bytes, opener).map_err(Refusal::Seal)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::schedule::{Direction, KeySchedule};
  use crate::{ControlDatagram, Envelope};
  use std::collections::BTreeMap;

  /// A test keyring: openers by `(sender, key_epoch)`, populated the way enrollment eventually will.
  #[derive(Default)]
  struct MapKeyring {
    openers: BTreeMap<(u64, u32), Opener>,
  }

  impl super::Keyring for MapKeyring {
    fn opener(&mut self, sender: u64, key_epoch: u32) -> Option<&mut Opener> {
      self.openers.get_mut(&(sender, key_epoch))
    }
  }

  const CHANNEL: u32 = 1;
  /// A generous datagram cap for the tests (a real cap is derived from the path MTU).
  const MAX_LEN: usize = 1 << 16;

  fn schedule() -> KeySchedule {
    KeySchedule::from_control_secret(&[0x5au8; 32])
  }

  fn datagram(sender: u64, key_epoch: u32) -> ControlDatagram {
    ControlDatagram {
      sender,
      key_epoch,
      envelope: Envelope {
        kind: 1,
        class: 1,
        flags: 0,
        epoch: 9,
        hlc: 0x2222,
        request_id: 7,
      },
      body: b"an accepted datagram".to_vec(),
    }
  }

  /// A datagram from a keyed sender runs the whole order and is accepted, its content intact.
  #[test]
  fn a_keyed_sender_is_accepted() {
    let sched = schedule();
    let (sender, epoch) = (0xAABB_CCDDu64, 4u32);
    let mut sealer = sched.sealer(sender, epoch, Direction::Initiator, CHANNEL);
    let wire = datagram(sender, epoch).encode_sealed(&mut sealer).unwrap();

    let mut keyring = MapKeyring::default();
    keyring.openers.insert(
      (sender, epoch),
      sched.opener(sender, epoch, Direction::Initiator, CHANNEL),
    );
    assert_eq!(
      accept(&wire, MAX_LEN, &mut keyring),
      Ok(datagram(sender, epoch))
    );
  }

  /// An unknown sender is dropped at the lookup, before any crypto — proven by handing `accept` a
  /// datagram whose body would fail the tag if opened: the result is UnknownSender, not BadSeal.
  #[test]
  fn an_unknown_sender_is_dropped_before_crypto() {
    let sched = schedule();
    let (sender, epoch) = (0x1234u64, 1u32);
    let mut sealer = sched.sealer(sender, epoch, Direction::Initiator, CHANNEL);
    let mut wire = datagram(sender, epoch).encode_sealed(&mut sealer).unwrap();
    // Corrupt the ciphertext so any AEAD attempt would be BadSeal.
    let last = wire.len() - 1;
    wire[last] ^= 0xFF;

    // The keyring holds no key for this sender.
    let mut keyring = MapKeyring::default();
    assert_eq!(
      accept(&wire, MAX_LEN, &mut keyring),
      Err(Refusal::UnknownSender {
        sender,
        key_epoch: epoch
      }),
      "an unknown sender is dropped before the tag is ever checked"
    );
  }

  /// An over-long datagram is dropped before it is even parsed.
  #[test]
  fn an_over_long_datagram_is_refused() {
    let sched = schedule();
    let (sender, epoch) = (1u64, 1u32);
    let mut sealer = sched.sealer(sender, epoch, Direction::Initiator, CHANNEL);
    let wire = datagram(sender, epoch).encode_sealed(&mut sealer).unwrap();
    let mut keyring = MapKeyring::default();
    keyring.openers.insert(
      (sender, epoch),
      sched.opener(sender, epoch, Direction::Initiator, CHANNEL),
    );
    assert_eq!(
      accept(&wire, wire.len() - 1, &mut keyring),
      Err(Refusal::TooLong {
        len: wire.len(),
        max: wire.len() - 1
      })
    );
  }

  /// A keyed sender whose datagram was tampered reaches the crypto and is a typed BadSeal.
  #[test]
  fn a_tampered_datagram_from_a_keyed_sender_is_bad_seal() {
    let sched = schedule();
    let (sender, epoch) = (5u64, 2u32);
    let mut sealer = sched.sealer(sender, epoch, Direction::Initiator, CHANNEL);
    let mut wire = datagram(sender, epoch).encode_sealed(&mut sealer).unwrap();
    let last = wire.len() - 1;
    wire[last] ^= 1;
    let mut keyring = MapKeyring::default();
    keyring.openers.insert(
      (sender, epoch),
      sched.opener(sender, epoch, Direction::Initiator, CHANNEL),
    );
    assert_eq!(
      accept(&wire, MAX_LEN, &mut keyring),
      Err(Refusal::Seal(SealError::BadSeal))
    );
  }

  /// A replayed datagram from a keyed sender is refused by the opener's replay high-water.
  #[test]
  fn a_replayed_datagram_is_refused() {
    let sched = schedule();
    let (sender, epoch) = (8u64, 3u32);
    let mut sealer = sched.sealer(sender, epoch, Direction::Initiator, CHANNEL);
    let wire = datagram(sender, epoch).encode_sealed(&mut sealer).unwrap();
    let mut keyring = MapKeyring::default();
    keyring.openers.insert(
      (sender, epoch),
      sched.opener(sender, epoch, Direction::Initiator, CHANNEL),
    );
    assert!(accept(&wire, MAX_LEN, &mut keyring).is_ok());
    assert_eq!(
      accept(&wire, MAX_LEN, &mut keyring),
      Err(Refusal::Seal(SealError::Replay)),
      "a replay is refused on the second acceptance"
    );
  }
}

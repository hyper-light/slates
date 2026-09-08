//! Generative hostile-input hardening for the control-plane parsers (§4.10a; the hostile-input rule,
//! D-20). Every parser of external bytes — the plaintext codec [`ControlDatagram::decode`], the
//! sealed decoder [`ControlDatagram::decode_sealed`], and the acceptance path [`accept`] — must, on
//! *any* byte sequence, return `Ok` or a typed refusal and **never panic** (no out-of-bounds, no
//! wild allocation, no unwrap). proptest drives arbitrary and near-valid bytes; a panic fails the
//! run. Where a parse succeeds, a further property is asserted so a silently-wrong accept cannot pass
//! (the plaintext codec round-trips; a sealed accept reproduces the sender/epoch it decoded).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use slates_transport::accept::{Keyring, accept};
use slates_transport::schedule::{Direction, KeySchedule};
use slates_transport::seal::Opener;
use slates_transport::{ControlDatagram, Envelope};

const SECRET: [u8; 32] = [0x5c; 32];
const SENDER: u64 = 0x0102_0304_0506_0708;
const EPOCH: u32 = 7;
const CHANNEL: u32 = 1;
const MAX_LEN: usize = 1 << 16;

struct MapKeyring {
  openers: BTreeMap<(u64, u32), Opener>,
}

impl Keyring for MapKeyring {
  fn opener(&mut self, sender: u64, key_epoch: u32) -> Option<&mut Opener> {
    self.openers.get_mut(&(sender, key_epoch))
  }
}

fn keyring() -> MapKeyring {
  let schedule = KeySchedule::from_control_secret(&SECRET);
  let mut openers = BTreeMap::new();
  openers.insert(
    (SENDER, EPOCH),
    schedule.opener(SENDER, EPOCH, Direction::Initiator, CHANNEL),
  );
  MapKeyring { openers }
}

/// A valid sealed datagram from the keyed sender — the seed for near-valid mutations.
fn valid_sealed() -> Vec<u8> {
  let schedule = KeySchedule::from_control_secret(&SECRET);
  let mut sealer = schedule.sealer(SENDER, EPOCH, Direction::Initiator, CHANNEL);
  let datagram = ControlDatagram {
    sender: SENDER,
    key_epoch: EPOCH,
    envelope: Envelope {
      kind: 3,
      class: 1,
      flags: 0,
      epoch: 42,
      hlc: 0xDEAD_BEEF,
      request_id: 99,
    },
    body: b"a canonical body for fuzzing".to_vec(),
  };
  datagram.encode_sealed(&mut sealer).unwrap()
}

proptest! {
  /// The plaintext codec never panics on arbitrary bytes, and any datagram it accepts round-trips.
  #[test]
  fn plaintext_decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
    if let Ok(datagram) = ControlDatagram::decode(&bytes) {
      prop_assert_eq!(ControlDatagram::decode(&datagram.encode()), Ok(datagram),
        "an accepted plaintext datagram must re-encode and decode to itself");
    }
  }

  /// The sealed decoder never panics on arbitrary bytes, opener state notwithstanding.
  #[test]
  fn sealed_decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
    let schedule = KeySchedule::from_control_secret(&SECRET);
    let mut opener = schedule.opener(SENDER, EPOCH, Direction::Initiator, CHANNEL);
    let _ = ControlDatagram::decode_sealed(&bytes, &mut opener);
  }

  /// The acceptance path never panics on arbitrary bytes; a datagram it accepts carries the sender
  /// and epoch the keyring was consulted for (never a mismatched identity).
  #[test]
  fn accept_never_panics_on_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
    let mut keyring = keyring();
    if let Ok(datagram) = accept(&bytes, MAX_LEN, &mut keyring) {
      prop_assert_eq!(datagram.sender, SENDER, "an accepted datagram's sender is the keyed one");
      prop_assert_eq!(datagram.key_epoch, EPOCH, "an accepted datagram's epoch is the keyed one");
    }
  }

  /// The session-plane frame codec never panics on arbitrary bytes, and any frame sequence it
  /// accepts re-encodes to itself (an accepted decode round-trips).
  #[test]
  fn session_decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
    if let Ok(frames) = slates_transport::session::decode_frames(&bytes) {
      prop_assert_eq!(
        slates_transport::session::decode_frames(&slates_transport::session::encode_frames(&frames)),
        Ok(frames),
        "an accepted frame sequence must re-encode and decode to itself"
      );
    }
  }

  /// Near-valid inputs — a real sealed datagram with one byte flipped at an arbitrary offset — never
  /// panic through the acceptance path; they are accepted (a flip the tag tolerates cannot exist for
  /// the authenticated region) or a typed refusal, never a crash. This exercises the deep paths a
  /// purely-random vector rarely reaches (a well-formed prologue, a real key lookup, the AEAD).
  #[test]
  fn a_bit_flipped_sealed_datagram_never_panics(
    flip_at in 0usize..256,
    xor in 1u8..=255,
  ) {
    let mut wire = valid_sealed();
    if flip_at < wire.len() {
      wire[flip_at] ^= xor;
    }
    let mut keyring = keyring();
    let _ = accept(&wire, MAX_LEN, &mut keyring);
  }
}

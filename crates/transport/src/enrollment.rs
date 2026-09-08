//! Fleet enrollment — turning an admitted-membership record into a node's control-plane keys
//! (§4.13 × §4.10a; design `docs/wip/enrollment.md`, a draft to ratify). A node cannot speak on the
//! fleet control plane until it holds the shared **control secret** (from which `crate::schedule`
//! derives its per-channel keys) and knows which peers are enrolled (so it accepts their sealed
//! datagrams and no one else's). This module is that step's local, pure half: given the secret, this
//! node's id, the key epoch, and the enrolled member ids, it builds this node's [`Sealer`] and the
//! [`Keyring`] of [`Opener`]s the acceptance order (`crate::accept`) consults — one opener per member,
//! so an un-enrolled sender is dropped before any crypto runs.
//!
//! What this is **not**: the distribution of the secret and the membership list. That is admission to
//! the §4.8 configuration group, authorized by a human or trusted harness (§4.13 — never an ambient
//! admin channel, never a uid), and it is owed with the configuration-group protocol (GAPS §8h). The
//! session-plane identity (the pinned certificate) rides the same admission and is held by
//! [`crate::handshake::Identity`]; its distribution is likewise owed. This half needs only local key
//! derivation, so it is built and tested end to end against the control plane with no network.

use std::collections::BTreeMap;

use crate::accept::Keyring;
use crate::schedule::{Direction, KeySchedule};
use crate::seal::{Opener, Sealer};

/// The control-plane channel enrolled peers speak membership/configuration traffic on.
/// Shape: this slice enrolls the one control channel; per-class channels (§4.10a's reliable classes)
/// are owed with the classes that use them.
const CONTROL_CHANNEL: u32 = 0;

/// A node's fleet enrollment: the key schedule derived from the shared control secret, this node's
/// sender id and key epoch, and the enrolled member ids. From it a node builds its control-plane
/// [`Sealer`] (it seals as its own id) and the [`EnrolledKeyring`] that accepts exactly the enrolled
/// peers. See `docs/wip/enrollment.md`.
pub struct Enrollment {
  schedule: KeySchedule,
  local_id: u64,
  key_epoch: u32,
  members: Vec<u64>,
}

impl Enrollment {
  /// Enrolls this node (`local_id`) from an admitted-membership record: the shared `control_secret`,
  /// the current `key_epoch`, and `members` — every enrolled sender id (this node included; a peer not
  /// listed is unreachable). The secret and the list come from configuration-group admission (owed);
  /// this derives the keys they imply.
  pub fn from_membership(
    control_secret: &[u8],
    local_id: u64,
    key_epoch: u32,
    members: Vec<u64>,
  ) -> Enrollment {
    Enrollment {
      schedule: KeySchedule::from_control_secret(control_secret),
      local_id,
      key_epoch,
      members,
    }
  }

  /// This node's control-plane sealer: it seals control datagrams as its own id, on the control
  /// channel, at the enrolled key epoch.
  pub fn sealer(&self) -> Sealer {
    self.schedule.sealer(
      self.local_id,
      self.key_epoch,
      Direction::Initiator,
      CONTROL_CHANNEL,
    )
  }

  /// The keyring populated from the membership: an [`Opener`] for every enrolled peer, keyed by
  /// `(sender, key_epoch)` exactly as the acceptance order looks it up — so a datagram from an
  /// enrolled peer opens and one from an un-enrolled sender is refused `UnknownSender` before crypto.
  pub fn keyring(&self) -> EnrolledKeyring {
    let mut openers = BTreeMap::new();
    for &peer in &self.members {
      let opener =
        self
          .schedule
          .opener(peer, self.key_epoch, Direction::Initiator, CONTROL_CHANNEL);
      openers.insert((peer, self.key_epoch), opener);
    }
    EnrolledKeyring { openers }
  }
}

/// A keyring populated by enrollment: an opener per enrolled peer, keyed by `(sender, key_epoch)`.
pub struct EnrolledKeyring {
  openers: BTreeMap<(u64, u32), Opener>,
}

impl Keyring for EnrolledKeyring {
  fn opener(&mut self, sender: u64, key_epoch: u32) -> Option<&mut Opener> {
    self.openers.get_mut(&(sender, key_epoch))
  }
}

#[cfg(test)]
mod tests {
  // Test harness: an unwrap here is a failed test.
  #![allow(clippy::unwrap_used)]

  use super::*;
  use crate::accept::{Refusal, accept};
  use crate::{ControlDatagram, Envelope};

  const SECRET: [u8; 32] = [0x5b; 32];
  const OTHER_SECRET: [u8; 32] = [0x77; 32];
  const EPOCH: u32 = 4;
  const MAX_LEN: usize = 2048;
  const NODE_A: u64 = 0xA000;
  const NODE_B: u64 = 0xB000;
  const NODE_C: u64 = 0xC000;

  /// A control datagram from `sender`, at the enrolled epoch, carrying `body`.
  fn datagram(sender: u64, body: &[u8]) -> ControlDatagram {
    ControlDatagram {
      sender,
      key_epoch: EPOCH,
      envelope: Envelope {
        kind: 1,
        class: 1,
        flags: 0,
        epoch: 11,
        hlc: 0,
        request_id: 1,
      },
      body: body.to_vec(),
    }
  }

  /// AC (§4.10a/§4.13): two nodes enrolled from the same membership can speak — a control datagram A
  /// seals with its enrolled sealer is accepted by B's enrolled keyring, recovering exactly what was
  /// sent. This is the "Keyring population is owed with enrollment" gap, closed and proven end to end.
  #[test]
  fn enrolled_peers_can_speak() {
    let members = vec![NODE_A, NODE_B];
    let node_a = Enrollment::from_membership(&SECRET, NODE_A, EPOCH, members.clone());
    let node_b = Enrollment::from_membership(&SECRET, NODE_B, EPOCH, members);

    let body = b"membership heartbeat from A";
    let wire = datagram(NODE_A, body)
      .encode_sealed(&mut node_a.sealer())
      .unwrap();

    let mut keyring = node_b.keyring();
    let recovered = accept(&wire, MAX_LEN, &mut keyring).unwrap();
    assert_eq!(recovered.sender, NODE_A);
    assert_eq!(
      recovered.body, body,
      "B recovered exactly what A enrolled and sent"
    );
  }

  /// AC (§4.10a/§4.13, non-vacuity): an **un-enrolled** sender is refused before crypto — B's keyring
  /// (members A and B) has no opener for C, so a datagram C seals is dropped `UnknownSender`. Proves
  /// enrollment actually gates the plane by membership (a dead keyring would accept nothing, but this
  /// asserts a *typed* refusal for the outsider while `enrolled_peers_can_speak` accepts the insider).
  #[test]
  fn an_unenrolled_sender_is_refused() {
    // C enrolls itself (it has the secret shape) but is not in B's membership.
    let node_c = Enrollment::from_membership(&SECRET, NODE_C, EPOCH, vec![NODE_C]);
    let node_b = Enrollment::from_membership(&SECRET, NODE_B, EPOCH, vec![NODE_A, NODE_B]);

    let wire = datagram(NODE_C, b"intruder")
      .encode_sealed(&mut node_c.sealer())
      .unwrap();
    let mut keyring = node_b.keyring();
    let outcome = accept(&wire, MAX_LEN, &mut keyring);
    assert!(
      matches!(outcome, Err(Refusal::UnknownSender { sender: NODE_C, .. })),
      "an un-enrolled sender must be dropped as UnknownSender, got {outcome:?}"
    );
  }

  /// AC (§4.10a/§4.13, hostile): a node enrolled under a **different** control secret is refused even
  /// though its id is in the membership — the id is not a bearer token; the shared secret is what
  /// authorizes. B has an opener for A's id, but A's keys (from the other secret) do not match, so the
  /// AEAD tag fails and the datagram is refused `Seal`, never silently accepted.
  #[test]
  fn a_wrong_secret_is_refused() {
    let impostor = Enrollment::from_membership(&OTHER_SECRET, NODE_A, EPOCH, vec![NODE_A]);
    let node_b = Enrollment::from_membership(&SECRET, NODE_B, EPOCH, vec![NODE_A, NODE_B]);

    let wire = datagram(NODE_A, b"forged")
      .encode_sealed(&mut impostor.sealer())
      .unwrap();
    let mut keyring = node_b.keyring();
    let outcome = accept(&wire, MAX_LEN, &mut keyring);
    assert!(
      matches!(outcome, Err(Refusal::Seal(_))),
      "a datagram under the wrong control secret must fail the seal, got {outcome:?}"
    );
  }
}

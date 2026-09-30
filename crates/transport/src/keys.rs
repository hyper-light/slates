//! The 1-RTT key schedule (§4.10a; RFC 9001 §6 key update, §6.6 usage limits; AUD-29-48).
//!
//! A session's 1-RTT packets are sealed under a **generation** of packet keys. The AEAD bounds how many
//! packets one key may seal (its confidentiality limit) and how many forged packets one may be asked to
//! open (its integrity limit) before its security guarantees lapse — for AES-GCM 2^23 sealed packets, far
//! below the packet-number space. So a sender seals at most the confidentiality limit under a generation and
//! then **updates**: it moves to the next generation, derived from the TLS secrets
//! (`rustls::quic::Secrets::next_packet_keys`), and flips the key-phase bit its short header carries, so the
//! peer knows which generation opens the packet. The peer, seeing the other phase on a packet numbered past
//! its current generation's first, tries the next generation; a packet that opens under it is the update,
//! and the peer moves too — sending and receiving — so both ends stay one generation apart at most. A
//! packet in the other phase numbered below that first is a reordered straggler of the previous generation,
//! opened under the previous key, which is kept for exactly one generation (bounded).
//!
//! **When an update may start.** A sender initiates an update only once the peer has acknowledged a packet
//! sealed under the current generation (RFC 9001 §6.1: the peer has then moved to it, or is able to), so the
//! ends never drift more than one generation apart. At the confidentiality limit with no such
//! acknowledgement, the session cannot continue safely: sealing is refused
//! [`KeyRefusal::ConfidentialityExhausted`], a terminal outcome.
//!
//! **Forgeries.** Every packet that fails to open — under any key — counts toward the session's integrity
//! budget; at the integrity limit the session ends [`KeyRefusal::IntegrityExhausted`] (RFC 9001 §6.6: across
//! all keys, for the connection). Below it a failure is one discarded packet.
//!
//! **What does not change.** The header-protection keys stay the first generation's for the session's life
//! (RFC 9001 §6: header protection is not updated), and three packet-key sets at most are held — current,
//! next and previous — so a session's key memory is bounded whatever its length.
//!
//! Until 2026-09-30 the first generation protected a session for its whole life, the key-phase bit stayed
//! zero, and no usage or failure was counted.

use rustls::quic::{HeaderProtectionKey, Keys, PacketKey, PacketKeySet, Secrets};

/// Why a key schedule refused: both are terminal for the session (RFC 9001 §6.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRefusal {
  /// The current generation has sealed its limit and the peer has not acknowledged any packet sealed under
  /// it, so no update may start: no further packet can be sealed safely.
  ConfidentialityExhausted,
  /// The session has been asked to open its integrity limit of packets that did not authenticate.
  IntegrityExhausted,
}

/// A packet that did not open: one discarded packet, or — at the integrity limit — the session's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenRefusal {
  /// The packet did not authenticate under any key it could have been sealed under; discard it.
  Unauthenticated,
  /// The integrity limit is reached.
  Exhausted(KeyRefusal),
}

/// A session's 1-RTT keys over their generations (see the module doc).
pub struct OneRtt {
  /// Header protection, the first generation's for the session's life.
  local_header: Box<dyn HeaderProtectionKey>,
  remote_header: Box<dyn HeaderProtectionKey>,
  /// The generation in use.
  current: PacketKeySet,
  /// The generation after it, derived ahead so a peer's update is recognized on its first packet.
  next: PacketKeySet,
  /// The previous generation's receive key, for reordered packets sealed before an update; one only.
  previous_remote: Option<Box<dyn PacketKey>>,
  /// The TLS secrets the generation after `next` is derived from.
  secrets: Secrets,
  /// The key-phase bit of the current generation.
  phase: bool,
  /// Packets sealed under the current generation.
  sealed: u64,
  /// The first packet number sealed under the current generation.
  first_sealed: Option<u64>,
  /// The first packet number opened under the current generation.
  first_opened: Option<u64>,
  /// Whether the peer has acknowledged a packet sealed under the current generation.
  confirmed: bool,
  /// Packets that failed to open, across every key.
  failures: u64,
  /// The most packets one generation seals: the AEAD's confidentiality limit, or a smaller cap.
  packet_limit: u64,
  /// The most failed opens the session tolerates: the AEAD's integrity limit, or a smaller cap.
  failure_limit: u64,
  /// Generations moved past (either end's update).
  updates: u64,
}

impl OneRtt {
  /// The schedule of a session whose handshake produced `keys` (the first generation) and `secrets` (what
  /// every later generation is derived from), with its limits the AEAD's own — capped at `packet_cap`
  /// sealed packets per generation and `failure_cap` failed opens when those are smaller.
  pub fn new(keys: Keys, mut secrets: Secrets, packet_cap: u64, failure_cap: u64) -> OneRtt {
    let packet_limit = keys.local.packet.confidentiality_limit().min(packet_cap);
    let failure_limit = keys.remote.packet.integrity_limit().min(failure_cap);
    let next = secrets.next_packet_keys();
    OneRtt {
      local_header: keys.local.header,
      remote_header: keys.remote.header,
      current: PacketKeySet {
        local: keys.local.packet,
        remote: keys.remote.packet,
      },
      next,
      previous_remote: None,
      secrets,
      phase: false,
      sealed: 0,
      first_sealed: None,
      first_opened: None,
      confirmed: false,
      failures: 0,
      packet_limit,
      failure_limit,
      updates: 0,
    }
  }

  /// The header-protection key this end masks its packets with.
  pub fn local_header(&self) -> &dyn HeaderProtectionKey {
    self.local_header.as_ref()
  }

  /// The header-protection key this end unmasks the peer's packets with.
  pub fn remote_header(&self) -> &dyn HeaderProtectionKey {
    self.remote_header.as_ref()
  }

  /// Takes one sealing of packet `pn` under the current generation ([`local_packet`](Self::local_packet)),
  /// returning the key-phase bit it carries: updating first when the current generation has sealed its
  /// limit and the peer has acknowledged it; refused when it has sealed its limit and cannot update.
  pub fn seal(&mut self, pn: u64) -> Result<bool, KeyRefusal> {
    if self.sealed >= self.packet_limit {
      if !self.confirmed {
        return Err(KeyRefusal::ConfidentialityExhausted);
      }
      self.advance();
      self.first_opened = None;
    }
    if self.first_sealed.is_none() {
      self.first_sealed = Some(pn);
    }
    self.sealed = self.sealed.saturating_add(1);
    Ok(self.phase)
  }

  /// The packet key this end seals with: the current generation's.
  pub fn local_packet(&self) -> &dyn PacketKey {
    self.current.local.as_ref()
  }

  /// Opens packet `pn`, which carried key-phase bit `phase`, whose header is `aad`, in place in `body`:
  /// the plaintext length. A packet in the current phase opens under the current key; one in the other
  /// phase numbered below the current generation's first opened packet (or before any) is a straggler of
  /// the previous generation; otherwise it is the peer's update, tried under the next generation and — when
  /// it opens — adopted by both directions.
  pub fn open(
    &mut self,
    pn: u64,
    phase: bool,
    aad: &[u8],
    body: &mut [u8],
  ) -> Result<usize, OpenRefusal> {
    if phase == self.phase {
      let opened = self
        .current
        .remote
        .decrypt_in_place(pn, aad, body)
        .map(|plain| plain.len());
      return match opened {
        Ok(len) => {
          self.first_opened.get_or_insert(pn);
          Ok(len)
        }
        Err(_) => Err(self.failed()),
      };
    }
    let straggler = self.first_opened.is_none_or(|first| pn < first);
    if straggler && let Some(previous) = self.previous_remote.as_ref() {
      return previous
        .decrypt_in_place(pn, aad, body)
        .map(|plain| plain.len())
        .map_err(|_| self.failed());
    }
    match self
      .next
      .remote
      .decrypt_in_place(pn, aad, body)
      .map(|plain| plain.len())
    {
      Ok(len) => {
        self.advance();
        self.first_opened = Some(pn);
        Ok(len)
      }
      Err(_) => Err(self.failed()),
    }
  }

  /// Notes that the peer acknowledged every packet up to `largest`: an acknowledgement of a packet sealed
  /// under the current generation confirms it, which lets the next update start.
  pub fn on_acknowledged(&mut self, largest: u64) {
    if self.first_sealed.is_some_and(|first| largest >= first) {
      self.confirmed = true;
    }
  }

  /// Generations this session has moved past.
  pub fn updates(&self) -> u64 {
    self.updates
  }

  /// Packets that failed to open, across every key.
  pub fn failures(&self) -> u64 {
    self.failures
  }

  /// Moves to the next generation: the current receive key becomes the previous one, the next becomes
  /// current, the one after is derived, and the phase flips.
  fn advance(&mut self) {
    let next = self.secrets.next_packet_keys();
    let current = std::mem::replace(&mut self.next, next);
    let retired = std::mem::replace(&mut self.current, current);
    self.previous_remote = Some(retired.remote);
    self.phase = !self.phase;
    self.sealed = 0;
    self.first_sealed = None;
    self.confirmed = false;
    self.updates = self.updates.saturating_add(1);
  }

  /// Counts one failed open: a discard, or the session's end at the integrity limit.
  fn failed(&mut self) -> OpenRefusal {
    self.failures = self.failures.saturating_add(1);
    if self.failures >= self.failure_limit {
      OpenRefusal::Exhausted(KeyRefusal::IntegrityExhausted)
    } else {
      OpenRefusal::Unauthenticated
    }
  }
}

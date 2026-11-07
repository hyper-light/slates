//! Live shipping's log (§4.10 "Live shipping", A-200; §4.8 the register rules it rides): the owner of a
//! `live-shipped` volume ships each of its publications to the volume's other candidates as a fenced, hash-chained log,
//! and a publication is durable once the owner and `f` holders hold it. This module is the protocol alone — sans-io,
//! deterministic, no clock — so the same code is the laptop (`f = 0`: no holders, every entry durable at once) and the
//! fleet (R8), and the simulation in `crates/db/tests/live.rs` drives it under a nemesis.
//!
//! **The chain.** Entry `n` names the identity of entry `n − 1` ([`Entry::previous`]); its own identity is the BLAKE3 of
//! its canonical bytes, so a holder that reports holding position `n` with identity `x` holds exactly the owner's
//! prefix up to `n` (the chain is Raft's log matching, made by hashes). A holder's reply is always the head it holds
//! ([`Head`]) or a fence refusal; the owner reads acknowledgement and catch-up from that one fact
//! ([`Shipper::on_reply`]). A holder never rewrites what it applied: an entry that does not extend its head is refused
//! with its head ([`LiveError::Behind`]), and the owner answers with more entries from its retained tail or, when the
//! holder is off its chain or behind the tail, an [`Install`] of its whole state at its current head (Raft's
//! InstallSnapshot, Ongaro thesis §5.3).
//!
//! **Fencing.** A holder keeps one [`Fence`] for the owner's host: an entry or install under a lower epoch is refused
//! `StaleEpoch`, and phase one ([`Replica::prepare`]) raises it. So once a takeover has fenced `f + 1` candidates the
//! superseded owner reaches at most `f − 1` other holders and can never make another entry durable
//! (StaleNeverCommits).
//!
//! **Takeover.** Every holder reports its head under the new epoch; the successor adopts the greatest
//! `(epoch, position)` among `f + 1` reports ([`adopt`]). Along any chain `(epoch, position)` only grows, and a durable
//! entry is held by `f + 1` candidates, which meet any `f + 1` reports, so the adopted head extends every durable entry
//! (NoLoss; Raft's up-to-date rule, thesis §3.6.1). The `f + 1` are counted among the holders the shipper being taken
//! over committed before its first entry (A-240), never among the members placed now: a member that joined after the
//! writes holds none of them, and counted in a holder's place it let a stale head be adopted and a fenced-out owner's
//! entry commit (10 of 400 seeds in `tests/live.rs`, 2026-11-06). Each shipper, a successor's too, commits its own
//! holders before it ships.
//!
//! **Bounds.** The owner holds at most `credit` entries not yet durable ([`LiveError::NoCredit`] past it: the writer
//! waits, AC-8.9) and retains at most `retain` entries for laggards; a holder behind the retained tail is installed.
//! Both are the caller's to derive (A-200: Little's law over the measured apply rate and holder round trip).
//!
//! Guard-then-apply, as `crate::partition`: [`Replica::check`] refuses before the caller applies a payload, and
//! [`Replica::commit`] is unconditional, so a payload the caller could not apply leaves the replica unchanged.

use std::collections::{BTreeMap, VecDeque};

use crate::register::{Fence, HostEpoch, HostId, ObjectId, Quorum};

/// A chain identity: the BLAKE3 of an entry's canonical bytes.
pub type Identity = [u8; 32];

/// Format: the identity before position 0, which the first entry of a volume's log names as its predecessor.
pub const GENESIS: Identity = [0; 32];

/// Format: the bytes of an object id on the wire.
const OBJECT_BYTES: usize = 16;
/// Format: the bytes of a [`Head`] on the wire: its epoch and position (u64 LE each) and its identity.
const HEAD_BYTES: usize = 2 * size_of::<u64>() + size_of::<Identity>();
/// Format: an entry's fixed prefix: owner (u64), object, position (u64), epoch (u64), previous identity, then the
/// payload's length (u32 LE); the payload follows.
const ENTRY_PREFIX_BYTES: usize =
  3 * size_of::<u64>() + OBJECT_BYTES + size_of::<Identity>() + size_of::<u32>();
/// Format: an install's fixed prefix: owner (u64), object, the shipping epoch (u64), the head it installs, then the
/// state's length (u32 LE); the state follows.
const INSTALL_PREFIX_BYTES: usize =
  2 * size_of::<u64>() + OBJECT_BYTES + HEAD_BYTES + size_of::<u32>();
/// Format: a whole-state message's first byte: an install (owner to holder).
const TAG_INSTALL: u8 = 1;
/// Format: a whole-state message's first byte: a takeover's fetch (successor to holder).
const TAG_FETCH: u8 = 2;
/// Format: a reply's first byte: a head held, a holder with nothing, or a fence refusal.
const REPLY_HEAD: u8 = 1;
/// Format: see [`REPLY_HEAD`].
const REPLY_EMPTY: u8 = 2;
/// Format: see [`REPLY_HEAD`].
const REPLY_STALE: u8 = 3;

/// A refusal of the live log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveError {
  /// The message's epoch is below the holder's fence for its owner's host; `current` is the fence.
  StaleEpoch {
    /// The fence the holder has promised.
    current: HostEpoch,
  },
  /// The entry does not extend what the holder holds (a gap, or another chain); `head` is what it holds (`None`: it
  /// holds nothing). The owner sends what follows `head` or an install.
  Behind {
    /// The holder's head.
    head: Option<Head>,
  },
  /// The message names another owner than the one the holder serves for this object.
  Unauthorized,
  /// The message names another object than the replica's.
  ForeignObject,
  /// Bytes that are not a well-formed message, or a payload over the caller's cap.
  Malformed,
  /// The owner already holds `credit` entries that are not durable; `durable` is the durable position (`None`:
  /// nothing yet). The writer waits for acknowledgements (AC-8.9).
  NoCredit {
    /// The durable position.
    durable: Option<u64>,
  },
}

/// What a holder holds: the epoch its last entry was made under, that entry's position and its identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
  /// The epoch of the owner that made the entry.
  pub epoch: HostEpoch,
  /// The entry's position.
  pub position: u64,
  /// The entry's identity.
  pub identity: Identity,
}

impl Head {
  /// The order phase one adopts by and an install must advance: epoch first, then position. Along any chain this only
  /// grows, since positions grow within an epoch and a successor's epoch is higher.
  pub fn rank(&self) -> (u64, u64) {
    (self.epoch.0, self.position)
  }

  fn encode_into(&self, out: &mut Vec<u8>) {
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out.extend_from_slice(&self.position.to_le_bytes());
    out.extend_from_slice(&self.identity);
  }

  fn decode_from(reader: &mut Reader<'_>) -> Result<Head, LiveError> {
    Ok(Head {
      epoch: HostEpoch(reader.word()?),
      position: reader.word()?,
      identity: reader.identity()?,
    })
  }
}

/// One publication on its way to the holders: its place in the chain and its payload (a delta or an image, opaque
/// here; the caller's applier knows which).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
  /// The owner that made it.
  pub owner: HostId,
  /// The volume.
  pub object: ObjectId,
  /// Its position, dense from 0.
  pub position: u64,
  /// The owner's host epoch.
  pub epoch: HostEpoch,
  /// The identity of the entry at `position − 1` ([`GENESIS`] at 0).
  pub previous: Identity,
  /// The publication's bytes.
  pub payload: Vec<u8>,
}

impl Entry {
  /// The canonical bytes, little-endian throughout, so every host computes one identity.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENTRY_PREFIX_BYTES.saturating_add(self.payload.len()));
    out.extend_from_slice(&self.owner.0.to_le_bytes());
    out.extend_from_slice(&self.object.0);
    out.extend_from_slice(&self.position.to_le_bytes());
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out.extend_from_slice(&self.previous);
    let length = u32::try_from(self.payload.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&self.payload);
    out
  }

  /// Parses bytes that crossed the network: every length is checked before it is read, and a payload longer than
  /// `payload_cap` is refused before anything is allocated for it.
  pub fn decode(bytes: &[u8], payload_cap: usize) -> Result<Entry, LiveError> {
    let mut reader = Reader { bytes };
    let owner = HostId(reader.word()?);
    let object = reader.object()?;
    let position = reader.word()?;
    let epoch = HostEpoch(reader.word()?);
    let previous = reader.identity()?;
    let payload = reader.tail(payload_cap)?;
    Ok(Entry {
      owner,
      object,
      position,
      epoch,
      previous,
      payload,
    })
  }

  /// The entry's identity: the BLAKE3 of its canonical bytes, binding its place in the chain and its payload.
  pub fn identity(&self) -> Identity {
    *blake3::hash(&self.encode()).as_bytes()
  }

  /// The head a holder holds once it has applied this entry.
  pub fn head(&self) -> Head {
    Head {
      epoch: self.epoch,
      position: self.position,
      identity: self.identity(),
    }
  }
}

/// The owner's whole state at its current head, for a holder off the chain or behind the retained tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
  /// The owner.
  pub owner: HostId,
  /// The volume.
  pub object: ObjectId,
  /// The epoch the owner ships under now (the fence it must pass), which may be above `head.epoch`.
  pub epoch: HostEpoch,
  /// The head the state is the state at.
  pub head: Head,
  /// The state's bytes (an image; opaque here).
  pub state: Vec<u8>,
}

impl Install {
  /// The canonical bytes: its tag (it rides the whole-state kind with [`Fetch`]), then its fields.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(
      INSTALL_PREFIX_BYTES
        .saturating_add(self.state.len())
        .saturating_add(1),
    );
    out.push(TAG_INSTALL);
    out.extend_from_slice(&self.owner.0.to_le_bytes());
    out.extend_from_slice(&self.object.0);
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    self.head.encode_into(&mut out);
    let length = u32::try_from(self.state.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&self.state);
    out
  }

  /// Parses bytes that crossed the network, refusing a state longer than `state_cap` before allocating it.
  pub fn decode(bytes: &[u8], state_cap: usize) -> Result<Install, LiveError> {
    let mut reader = Reader { bytes };
    if reader.byte()? != TAG_INSTALL {
      return Err(LiveError::Malformed);
    }
    let owner = HostId(reader.word()?);
    let object = reader.object()?;
    let epoch = HostEpoch(reader.word()?);
    let head = Head::decode_from(&mut reader)?;
    let state = reader.tail(state_cap)?;
    Ok(Install {
      owner,
      object,
      epoch,
      head,
      state,
    })
  }
}

/// The object an encoded entry or install names, read without decoding the rest: both begin with the owner's word and
/// then the object (the formats above). `None` for bytes too short to hold it.
pub fn object_of(bytes: &[u8]) -> Option<ObjectId> {
  let mut reader = Reader { bytes };
  reader.word().ok()?;
  reader.object().ok()
}

/// The epoch an entry was made under, read without decoding the rest: what a holder that will not serve the sender (the
/// configuration names another acting owner) compares with its fence, to tell a superseded owner so (`StaleEpoch`)
/// rather than refuse it as a stranger. `None` for bytes too short.
pub fn epoch_of(bytes: &[u8]) -> Option<HostEpoch> {
  let mut reader = Reader { bytes };
  reader.word().ok()?;
  reader.object().ok()?;
  reader.word().ok()?;
  reader.word().ok().map(HostEpoch)
}

/// The epoch an [`Install`] names, as [`epoch_of`] reads an entry's. `None` for bytes too short or of another tag.
pub fn epoch_of_install(bytes: &[u8]) -> Option<HostEpoch> {
  let mut reader = Reader { bytes };
  if reader.byte().ok()? != TAG_INSTALL {
    return None;
  }
  reader.word().ok()?;
  reader.object().ok()?;
  reader.word().ok().map(HostEpoch)
}

/// The object a whole-state message (an [`Install`] or a [`Fetch`]) names, read without decoding the rest. `None` for
/// bytes too short or of another tag.
pub fn object_of_state(bytes: &[u8]) -> Option<ObjectId> {
  let mut reader = Reader { bytes };
  match reader.byte().ok()? {
    TAG_INSTALL => {
      reader.word().ok()?;
      reader.object().ok()
    }
    TAG_FETCH => reader.object().ok(),
    _ => None,
  }
}

/// Whether a whole-state message is a [`Fetch`] (otherwise an [`Install`], or malformed).
pub fn is_fetch(bytes: &[u8]) -> bool {
  bytes.first() == Some(&TAG_FETCH)
}

/// A takeover's phase one for a live volume (A-200): the successor asks a holder to raise its fence to `epoch` and send
/// its head and what it keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fetch {
  /// The volume.
  pub object: ObjectId,
  /// The epoch the successor ships under.
  pub epoch: HostEpoch,
}

impl Fetch {
  /// The canonical bytes: its tag, the object, the epoch.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(TAG_FETCH);
    out.extend_from_slice(&self.object.0);
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out
  }

  /// Parses bytes that crossed the network.
  pub fn decode(bytes: &[u8]) -> Result<Fetch, LiveError> {
    let mut reader = Reader { bytes };
    if reader.byte()? != TAG_FETCH {
      return Err(LiveError::Malformed);
    }
    let fetch = Fetch {
      object: reader.object()?,
      epoch: HostEpoch(reader.word()?),
    };
    if reader.bytes.is_empty() {
      Ok(fetch)
    } else {
      Err(LiveError::Malformed)
    }
  }
}

/// A holder's answer to a [`Fetch`]: its reply (the head, or the fence refusing a stale successor) and the payloads it
/// keeps, in order, each `u32`-length-prefixed.
pub fn encode_fetched(reply: &Result<Option<Head>, LiveError>, payloads: &[Vec<u8>]) -> Vec<u8> {
  let head = encode_reply(reply);
  let mut out = Vec::new();
  out.extend_from_slice(&u32::try_from(head.len()).unwrap_or(u32::MAX).to_le_bytes());
  out.extend_from_slice(&head);
  for payload in payloads {
    out.extend_from_slice(
      &u32::try_from(payload.len())
        .unwrap_or(u32::MAX)
        .to_le_bytes(),
    );
    out.extend_from_slice(payload);
  }
  out
}

/// Parses a holder's answer to a [`Fetch`], refusing any payload over `cap` before it is copied.
pub fn decode_fetched(bytes: &[u8], cap: usize) -> Result<(Option<Head>, Vec<Vec<u8>>), LiveError> {
  let mut reader = Reader { bytes };
  let head = decode_reply(&reader.prefixed(cap)?)?;
  let mut payloads = Vec::new();
  while !reader.bytes.is_empty() {
    payloads.push(reader.prefixed(cap)?);
  }
  Ok((head, payloads))
}

/// A holder's answer to an entry or an install: the head it holds after it (`Ok(None)`: nothing), or a refusal.
pub fn encode_reply(reply: &Result<Option<Head>, LiveError>) -> Vec<u8> {
  let mut out = Vec::with_capacity(1 + HEAD_BYTES);
  match reply {
    Ok(Some(head)) | Err(LiveError::Behind { head: Some(head) }) => {
      out.push(REPLY_HEAD);
      head.encode_into(&mut out);
    }
    Ok(None) | Err(LiveError::Behind { head: None }) => out.push(REPLY_EMPTY),
    Err(LiveError::StaleEpoch { current }) => {
      out.push(REPLY_STALE);
      out.extend_from_slice(&current.0.to_le_bytes());
    }
    // A refusal that names nothing the owner can act on is an empty answer: it counts as no acknowledgement.
    Err(_) => {}
  }
  out
}

/// Parses a holder's answer. A head's answer is `Ok(Some(head))` whether the holder applied the message or was behind:
/// the owner judges both by the head ([`Shipper::on_reply`]).
pub fn decode_reply(bytes: &[u8]) -> Result<Option<Head>, LiveError> {
  let mut reader = Reader { bytes };
  let kind = reader.byte()?;
  let reply = match kind {
    REPLY_HEAD => Ok(Some(Head::decode_from(&mut reader)?)),
    REPLY_EMPTY => Ok(None),
    REPLY_STALE => Err(LiveError::StaleEpoch {
      current: HostEpoch(reader.word()?),
    }),
    _ => Err(LiveError::Malformed),
  };
  if reader.bytes.is_empty() {
    reply
  } else {
    Err(LiveError::Malformed)
  }
}

/// A bounds-checked little-endian reader over bytes that crossed the network.
struct Reader<'a> {
  bytes: &'a [u8],
}

impl<'a> Reader<'a> {
  fn take(&mut self, length: usize) -> Result<&'a [u8], LiveError> {
    let (taken, rest) = self
      .bytes
      .split_at_checked(length)
      .ok_or(LiveError::Malformed)?;
    self.bytes = rest;
    Ok(taken)
  }

  fn byte(&mut self) -> Result<u8, LiveError> {
    self.take(1)?.first().copied().ok_or(LiveError::Malformed)
  }

  fn word(&mut self) -> Result<u64, LiveError> {
    let bytes = self.take(size_of::<u64>())?;
    Ok(u64::from_le_bytes(
      bytes.try_into().map_err(|_| LiveError::Malformed)?,
    ))
  }

  fn identity(&mut self) -> Result<Identity, LiveError> {
    self
      .take(size_of::<Identity>())?
      .try_into()
      .map_err(|_| LiveError::Malformed)
  }

  fn object(&mut self) -> Result<ObjectId, LiveError> {
    Ok(ObjectId(
      self
        .take(OBJECT_BYTES)?
        .try_into()
        .map_err(|_| LiveError::Malformed)?,
    ))
  }

  /// A u32-length-prefixed body, refused over `cap` before it is copied; more may follow.
  fn prefixed(&mut self, cap: usize) -> Result<Vec<u8>, LiveError> {
    let length = self.take(size_of::<u32>())?;
    let length = u32::from_le_bytes(length.try_into().map_err(|_| LiveError::Malformed)?);
    let length = usize::try_from(length).map_err(|_| LiveError::Malformed)?;
    if length > cap {
      return Err(LiveError::Malformed);
    }
    Ok(self.take(length)?.to_vec())
  }

  /// A u32-length-prefixed body that must be the rest of the bytes, refused over `cap` before it is copied.
  fn tail(&mut self, cap: usize) -> Result<Vec<u8>, LiveError> {
    let length = self.take(size_of::<u32>())?;
    let length = u32::from_le_bytes(length.try_into().map_err(|_| LiveError::Malformed)?);
    let length = usize::try_from(length).map_err(|_| LiveError::Malformed)?;
    if length > cap || self.bytes.len() != length {
      return Err(LiveError::Malformed);
    }
    Ok(self.take(length)?.to_vec())
  }
}

/// What a holder will do with a message it has checked ([`Replica::check`]): apply the payload and then
/// [`Replica::commit`], or only answer (a duplicate or a re-delivery of what it already holds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admitted {
  /// Apply the payload (an entry's delta onto the state; an install's state in place of it), then commit this head.
  Apply(Head),
  /// Nothing to apply: the holder already holds this head or a later one on the same chain; answer with its head.
  Held,
}

/// A holder's replica of one live-shipped volume: who may ship to it, its fence for that owner's host, and the head
/// it holds. The state the head stands for is the caller's (the replica image), kept beside it.
#[derive(Clone, Debug)]
pub struct Replica {
  owner: HostId,
  object: ObjectId,
  fence: Fence,
  head: Option<Head>,
}

impl Replica {
  /// An empty replica of `object`, shipped to by `owner`.
  pub fn new(owner: HostId, object: ObjectId) -> Replica {
    Replica {
      owner,
      object,
      fence: Fence::new(),
      head: None,
    }
  }

  /// The head it holds.
  pub fn head(&self) -> Option<Head> {
    self.head
  }

  /// The fence it has promised.
  pub fn fence(&self) -> HostEpoch {
    self.fence.seen
  }

  /// Hands the replica to a new owner (a takeover named it): later messages must come from `owner`, under a fence at
  /// least the one already promised.
  pub fn reassign(&mut self, owner: HostId) {
    self.owner = owner;
  }

  fn admit_sender(
    &self,
    owner: HostId,
    object: ObjectId,
    epoch: HostEpoch,
  ) -> Result<(), LiveError> {
    if object != self.object {
      return Err(LiveError::ForeignObject);
    }
    // The fence before the owner: a superseded owner must hear `StaleEpoch`, which tells it to stop. Refused as
    // unauthorized (an empty answer) it never learned it was superseded and shipped on; the model's counter of stale
    // refusals stayed at zero at `f = 1`, where phase one fences and reassigns every surviving holder.
    if epoch < self.fence.seen {
      return Err(LiveError::StaleEpoch {
        current: self.fence.seen,
      });
    }
    if owner != self.owner {
      return Err(LiveError::Unauthorized);
    }
    Ok(())
  }

  /// The guard for an entry: whether it extends the held head, is already held, or is refused.
  pub fn check(&self, entry: &Entry) -> Result<Admitted, LiveError> {
    self.admit_sender(entry.owner, entry.object, entry.epoch)?;
    let extends = match self.head {
      None => entry.position == 0 && entry.previous == GENESIS,
      Some(head) => {
        head.position.checked_add(1) == Some(entry.position) && head.identity == entry.previous
      }
    };
    if extends {
      return Ok(Admitted::Apply(entry.head()));
    }
    match self.head {
      // The same entry again (a duplicate), or an older one of the same epoch's stream re-delivered: what the holder
      // holds already answers it. An older entry can only be judged as held when its epoch is the head's and its
      // position below, since one owner makes one chain per epoch and the head extends it.
      Some(head)
        if head == entry.head()
          || (entry.epoch == head.epoch && entry.position < head.position) =>
      {
        Ok(Admitted::Held)
      }
      head => Err(LiveError::Behind { head }),
    }
  }

  /// The guard for an install: admitted when its head ranks above the held head, held when it is the held head.
  pub fn check_install(&self, install: &Install) -> Result<Admitted, LiveError> {
    self.admit_sender(install.owner, install.object, install.epoch)?;
    match self.head {
      Some(head) if head == install.head => Ok(Admitted::Held),
      Some(head) if head.rank() >= install.head.rank() => {
        Err(LiveError::Behind { head: Some(head) })
      }
      _ => Ok(Admitted::Apply(install.head)),
    }
  }

  /// Records a checked message whose payload the caller has applied: the fence rises to `epoch` and the head becomes
  /// `head`. Unconditional (guard-then-apply).
  pub fn commit(&mut self, epoch: HostEpoch, head: Head) {
    if epoch > self.fence.seen {
      self.fence.seen = epoch;
    }
    self.head = Some(head);
  }

  /// Checks and commits an entry whose payload needs no applying (the model's, or a caller that applies inside its
  /// own step): the head the holder answers with.
  pub fn accept(&mut self, entry: &Entry) -> Result<Option<Head>, LiveError> {
    match self.check(entry)? {
      Admitted::Apply(head) => {
        self.commit(entry.epoch, head);
        Ok(Some(head))
      }
      Admitted::Held => Ok(self.head),
    }
  }

  /// Phase one of a takeover: raises the fence to `epoch` and reports the held head. Refused when `epoch` is below
  /// the fence, so a stale successor learns nothing it could act on.
  pub fn prepare(&mut self, epoch: HostEpoch) -> Result<Option<Head>, LiveError> {
    self
      .fence
      .accept(epoch)
      .map_err(|_| LiveError::StaleEpoch {
        current: self.fence.seen,
      })?;
    Ok(self.head)
  }
}

/// Phase one's choice (A-200, A-240): the holder whose head ranks highest among `reports` (each candidate's head;
/// `None` for one that holds nothing), and that head, once at least `f + 1` of the reports come from `holders`, the
/// holder set the shipper being taken over committed before its first entry. A durable entry is held by the owner and
/// `f` of those `2f` holders, so `f + 1` of them meet every durable entry; a report from any other member (one that
/// joined after the writes) counts for nothing, since it can stand in for a holder that was never asked. `None` when
/// fewer than `f + 1` holders reported, or when every report is empty (nothing was ever shipped).
pub fn adopt(
  reports: &[(HostId, Option<Head>)],
  holders: &[HostId],
  quorum: Quorum,
) -> Option<(HostId, Head)> {
  let needed = usize::try_from(quorum.f).ok()?.checked_add(1)?;
  let counted = reports
    .iter()
    .filter(|(host, _)| holders.contains(host))
    .count();
  if counted < needed {
    return None;
  }
  reports
    .iter()
    .filter_map(|(holder, head)| head.map(|head| (*holder, head)))
    .max_by_key(|(_, head)| head.rank())
}

/// What the owner sends a holder next ([`Shipper::on_reply`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
  /// Nothing: it holds everything appended.
  Nothing,
  /// These retained entries, in order.
  Entries(Vec<Entry>),
  /// An install of the owner's state at [`Shipper::head`]: the holder is off the chain or behind the retained tail.
  Install,
  /// The holder has promised a later epoch: this owner is superseded and must stop shipping and serving.
  Fenced(HostEpoch),
}

/// The owner's side of one live-shipped volume: its chain, the entries it retains, what each holder holds of it, and
/// the durable position.
#[derive(Clone, Debug)]
pub struct Shipper {
  owner: HostId,
  object: ObjectId,
  epoch: HostEpoch,
  quorum: Quorum,
  holders: Vec<HostId>,
  credit: u64,
  retain: usize,
  /// The payload bytes kept for laggards beyond `retain` entries ([`Shipper::retain_up_to`]); zero keeps `retain` alone.
  retain_bytes: usize,
  /// The head the owner started from (a takeover's adopted head), which it holds and every holder may report.
  start: Option<Head>,
  /// The owner's head: its last entry, or `start` before its first.
  head: Option<Head>,
  /// The retained entries, oldest first, dense in position.
  retained: VecDeque<Entry>,
  /// What each holder holds on this chain, as last reported.
  held: BTreeMap<HostId, Head>,
  /// The highest position held by the owner and `f` holders.
  durable: Option<u64>,
  /// The head of the last whole state the owner shipped ([`Shipper::mark_checkpoint`]): on the chain, and what an
  /// install names, so a holder installed there is credited even once the retained tail no longer reaches it.
  checkpoint: Option<Head>,
  /// The holders whose last answer said they need an install (off the chain, or behind the retained tail), until one
  /// answers with a head on the chain: [`Shipper::pending`] answers `Install` for them.
  owed_install: std::collections::BTreeSet<HostId>,
}

impl Shipper {
  /// The owner's side for `object` under `epoch`, shipping to `holders` (the other `2f` candidates), starting from
  /// `start` (`None` for a new volume; a takeover's adopted head otherwise). `credit` bounds the entries appended here
  /// that are not yet durable and `retain` the entries kept for laggards; each is at least one.
  pub fn new(
    (owner, object, epoch): (HostId, ObjectId, HostEpoch),
    quorum: Quorum,
    holders: &[HostId],
    start: Option<Head>,
    (credit, retain): (u64, usize),
  ) -> Shipper {
    let mut shipper = Shipper {
      owner,
      object,
      epoch,
      quorum,
      holders: holders
        .iter()
        .copied()
        .filter(|holder| *holder != owner)
        .collect(),
      credit: credit.max(1),
      retain: retain.max(1),
      retain_bytes: 0,
      start,
      head: start,
      retained: VecDeque::new(),
      held: BTreeMap::new(),
      durable: None,
      checkpoint: None,
      owed_install: std::collections::BTreeSet::new(),
    };
    shipper.durable = shipper.recompute_durable();
    shipper
  }

  /// The owner it ships as.
  pub fn owner(&self) -> HostId {
    self.owner
  }

  /// The owner's head.
  pub fn head(&self) -> Option<Head> {
    self.head
  }

  /// The epoch it ships under.
  pub fn epoch(&self) -> HostEpoch {
    self.epoch
  }

  /// The highest position the owner and `f` holders hold (`None`: none yet). A barrier covering an entry at or below
  /// it may answer (A-200).
  pub fn durable(&self) -> Option<u64> {
    self.durable
  }

  /// The holders it ships to.
  pub fn holders(&self) -> &[HostId] {
    &self.holders
  }

  /// The entries appended here that are not yet durable.
  pub fn in_flight(&self) -> u64 {
    let Some(head) = self.head else {
      return 0;
    };
    let floor = match (self.durable, self.start) {
      (Some(durable), Some(start)) => Some(durable.max(start.position)),
      (Some(durable), None) => Some(durable),
      (None, Some(start)) => Some(start.position),
      (None, None) => None,
    };
    match floor {
      Some(floor) => head.position.saturating_sub(floor),
      None => head.position.saturating_add(1),
    }
  }

  /// Appends the next publication to the chain: the entry to send to every holder. Refused [`LiveError::NoCredit`]
  /// while `credit` entries are not yet durable; the writer waits for acknowledgements (AC-8.9).
  pub fn append(&mut self, payload: Vec<u8>) -> Result<Entry, LiveError> {
    if self.in_flight() >= self.credit {
      return Err(LiveError::NoCredit {
        durable: self.durable,
      });
    }
    self.append_unchecked(payload)
  }

  /// Appends a whole state (a checkpoint, which [`Shipper::mark_checkpoint`] then names), allowed one entry past the
  /// credit: a checkpoint is how holders that fell behind the retained tail are caught up, and when every holder has,
  /// nothing becomes durable to free the credit, so a checkpoint that waited for it would never come (both candidates
  /// stalled through 30 writes waited 19 minutes, 2026-11-05, `live_fleet`). Still bounded: refused past `credit + 1`.
  pub fn append_checkpoint(&mut self, payload: Vec<u8>) -> Result<Entry, LiveError> {
    if self.in_flight() > self.credit {
      return Err(LiveError::NoCredit {
        durable: self.durable,
      });
    }
    self.append_unchecked(payload)
  }

  fn append_unchecked(&mut self, payload: Vec<u8>) -> Result<Entry, LiveError> {
    let (position, previous) = match self.head {
      Some(head) => (
        head.position.checked_add(1).ok_or(LiveError::Malformed)?,
        head.identity,
      ),
      None => (0, GENESIS),
    };
    let entry = Entry {
      owner: self.owner,
      object: self.object,
      position,
      epoch: self.epoch,
      previous,
      payload,
    };
    self.head = Some(entry.head());
    self.retained.push_back(entry.clone());
    self.trim();
    self.durable = self.recompute_durable();
    Ok(entry)
  }

  /// An install of `state`, which must be the owner's state at [`Shipper::head`]; `None` before anything was shipped.
  pub fn install(&self, state: Vec<u8>) -> Option<Install> {
    Some(Install {
      owner: self.owner,
      object: self.object,
      epoch: self.epoch,
      head: self.head?,
      state,
    })
  }

  /// Folds a holder's answer (to an entry or an install) and says what to send it next. A head on this chain is an
  /// acknowledgement of every position up to it; a head off the chain or behind the retained tail draws an install; a
  /// fence above this epoch means the owner has been superseded.
  pub fn on_reply(&mut self, holder: HostId, reply: Result<Option<Head>, LiveError>) -> Next {
    if !self.holders.contains(&holder) {
      return Next::Nothing;
    }
    let held = match reply {
      Err(LiveError::StaleEpoch { current }) if current > self.epoch => {
        return Next::Fenced(current);
      }
      Err(LiveError::Behind { head }) | Ok(head) => head,
      Err(_) => return Next::Nothing,
    };
    if let Some(head) = held
      && head.epoch > self.epoch
    {
      return Next::Fenced(head.epoch);
    }
    let Some(own) = self.head else {
      return Next::Nothing;
    };
    let Some(head) = held else {
      return self.owe(holder, self.entries_after(None));
    };
    if !self.on_chain(&head) {
      self.owed_install.insert(holder);
      return Next::Install;
    }
    self.owed_install.remove(&holder);
    let newer = self
      .held
      .get(&holder)
      .is_none_or(|known| known.position < head.position);
    if newer {
      self.held.insert(holder, head);
      self.trim();
      self.durable = self.recompute_durable();
    }
    if head == own {
      Next::Nothing
    } else {
      let next = self.entries_after(Some(head.position));
      self.owe(holder, next)
    }
  }

  /// Notes an `Install` answer as owed to `holder`, so [`Shipper::pending`] keeps answering it until the holder is back
  /// on the chain.
  fn owe(&mut self, holder: HostId, next: Next) -> Next {
    if next == Next::Install {
      self.owed_install.insert(holder);
    }
    next
  }

  /// What `holder` lacks by the head it last reported (everything, when it has reported none; an install when its last
  /// answer was off the chain or behind the tail): the owner's period re-ships this, so a lost entry or reply is
  /// recovered without waiting on the next write. Until 2026-11-05 a holder off the chain was offered entries from its
  /// last on-chain head, which it refused, forever: a takeover's successor never caught the surviving candidate up and
  /// every write it took waited (`live_fleet`, a write stuck 21 minutes; the model, driven as the daemon drives it,
  /// failed Convergence).
  pub fn pending(&self, holder: HostId) -> Next {
    if self.head.is_none() || !self.holders.contains(&holder) {
      return Next::Nothing;
    }
    if self.owed_install.contains(&holder) {
      return Next::Install;
    }
    match self.held.get(&holder) {
      Some(head) if Some(*head) == self.head => Next::Nothing,
      Some(head) => self.entries_after(Some(head.position)),
      None => self.entries_after(None),
    }
  }

  /// Records `head` as the last whole state shipped: the entry at it carries the owner's whole state, which an install
  /// sends to a holder off the chain or behind the retained tail. Ignored unless it is the owner's head now.
  pub fn mark_checkpoint(&mut self, head: Head) {
    if Some(head) == self.head {
      self.checkpoint = Some(head);
    }
  }

  /// The last whole state shipped ([`Shipper::mark_checkpoint`]).
  pub fn checkpoint(&self) -> Option<Head> {
    self.checkpoint
  }

  /// What `holder` last reported holding on this chain (`None`: nothing yet, or never on it).
  pub fn held_by(&self, holder: HostId) -> Option<Head> {
    self.held.get(&holder).copied()
  }

  /// Whether `head` is on this owner's chain: its head, its start, its last checkpoint, or a retained entry.
  fn on_chain(&self, head: &Head) -> bool {
    if Some(*head) == self.head || Some(*head) == self.start || Some(*head) == self.checkpoint {
      return true;
    }
    self
      .retained
      .iter()
      .find(|entry| entry.position == head.position)
      .is_some_and(|entry| entry.head() == *head)
  }

  /// The retained entries after `position` (all of them from position 0 when `None`), or an install when the tail no
  /// longer reaches back that far.
  fn entries_after(&self, position: Option<u64>) -> Next {
    let first = match position {
      Some(position) => position.checked_add(1),
      None => Some(0),
    };
    let Some(first) = first else {
      return Next::Nothing;
    };
    match self.retained.front() {
      Some(oldest) if oldest.position <= first => Next::Entries(
        self
          .retained
          .iter()
          .filter(|entry| entry.position >= first)
          .cloned()
          .collect(),
      ),
      // Nothing retained reaches `first`: the holder is behind the tail (or the owner started from an adopted head and
      // has appended nothing yet), so it takes the owner's whole state.
      _ => Next::Install,
    }
  }

  /// Drops retained entries every holder holds, then the oldest past `retain` (a laggard behind them is installed).
  fn trim(&mut self) {
    let everyone = self
      .holders
      .iter()
      .map(|holder| self.held.get(holder).map(|head| head.position))
      .min()
      .flatten();
    let mut bytes = self
      .retained
      .iter()
      .fold(0usize, |sum, entry| sum.saturating_add(entry.payload.len()));
    while let Some(oldest) = self.retained.front() {
      let held_by_all = everyone.is_some_and(|position| oldest.position <= position);
      let over = self.retained.len() > self.retain && bytes > self.retain_bytes;
      if !(held_by_all || over) {
        break;
      }
      bytes = bytes.saturating_sub(oldest.payload.len());
      self.retained.pop_front();
    }
  }

  /// Keeps entries for laggards up to `bytes` of payload as well as the `retain` entries: whichever keeps more.
  /// Derived (by the caller): `bytes` is the size of the whole state an install would send, the owner's last whole
  /// record; then a laggard is sent the entries it lacks for as long as they cost no more than an install. Kept to a count alone, an owner writing faster than an install's
  /// round trip left a holder it had just installed behind the tail again, and installed it again without end
  /// (2026-11-06, `crates/cli/tests/live_chaos.rs`: 803 installs, 571 MB, in one run).
  pub fn retain_up_to(&mut self, bytes: usize) {
    self.retain_bytes = bytes;
    self.trim();
  }

  /// The highest position the owner and `f` holders hold: the `f`-th highest holder position, capped at the owner's
  /// head. At `f = 0` the owner's head.
  fn recompute_durable(&self) -> Option<u64> {
    let own = self.head?.position;
    let needed = usize::try_from(self.quorum.f).ok()?;
    let Some(rank) = needed.checked_sub(1) else {
      return Some(own);
    };
    let mut positions: Vec<u64> = self
      .holders
      .iter()
      .filter_map(|holder| self.held.get(holder).map(|head| head.position))
      .collect();
    positions.sort_unstable_by(|a, b| b.cmp(a));
    let reached = positions.get(rank).copied()?.min(own);
    // Durability only advances: an older answer arriving late never lowers it.
    Some(self.durable.map_or(reached, |durable| durable.max(reached)))
  }
}

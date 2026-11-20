//! Live shipping's log (§4.10 "Live shipping", A-200; §4.8 the register rules it rides): the owner of a
//! `live-shipped` volume ships each of its publications to the volume's other candidates as a fenced, hash-chained log,
//! and a publication is durable once the owner and enough holders hold it that every `f + 1` of them a takeover can count
//! meets one and `f` failures leave a copy (`max(n − f, f)` of `n`: `f` of the usual `2f`). This module is the protocol alone — sans-io,
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
//! **Batches (A-200 amended 2026-11-17).** The entries a holder lacks may travel as one message ([`encode_batch`],
//! [`decode_batch`]): the holder applies them in order exactly as it applies each alone, and its one reply is the head it
//! reached, which the owner reads as it reads any reply. A batch changes how many exchanges a catch-up costs, not what a
//! holder accepts: one exchange an entry made a burst of `n` entries reach a holder in `n` round trips.
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
//! **Holder change.** The holders follow the volume's candidates by a joint transition, as Vertical Paxos changes a
//! configuration (Lamport, Malkhi and Zhou, PODC 2009): [`Shipper::begin_transition`] ships to the old and the new set
//! and counts a position durable only once `f` of each hold it; [`Shipper::transferred`] says the new set holds every
//! durable entry and the head the shipper started from; the record may name it only then, and
//! [`Shipper::activate`] switches once it does. Replacing the holders outright let a takeover count a set that held none
//! of the durable entries (NoLoss broken in `tests/live.rs`, 2026-11-07).
//!
//! **Bounds (AC-8.9, A-200 amended 2026-11-17).** Both bounds are bytes of entries, each entry counted at its wire size
//! ([`Entry::wire_bytes`]): the entries appended and not yet durable are held within [`Bounds::credit`]
//! ([`LiveError::NoCredit`] past it: the writer waits for acknowledgements, or for the whole state its turned-away
//! change is then shipped in), and the entries retained for laggards within [`Bounds::retain`]; a holder behind the
//! retained tail is installed. One entry is always admitted while nothing is in flight, and the newest entry is never
//! dropped for size, so a bound never stops the log. The caller derives both from the data ([`Bounds::of_whole`]: the whole state
//! an install would send), so the deltas outstanding never cost more than the install or whole record that replaces
//! them. A count was the wrong unit: the entries in flight are the publication rate times the time to acknowledge
//! (Little's law), and a burst of barriers on one volume puts several publications inside one round trip, so the
//! daemon's fixed credit of two turned the third into a copy of the whole volume.
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
/// Format: a batch's fixed prefix: the owner (u64), the object, the epoch its entries were made under (u64), then the
/// entry count (u32 LE); each entry follows as a u32-length-prefixed encoded [`Entry`] ([`encode_batch`]).
const BATCH_PREFIX_BYTES: usize = 2 * size_of::<u64>() + OBJECT_BYTES + size_of::<u32>();
/// Format: the least an entry takes in a batch: its length prefix and its fixed prefix, an empty payload. A count
/// claiming more entries than the bytes that follow could hold is refused before anything is allocated for them.
const BATCHED_ENTRY_MIN_BYTES: usize = size_of::<u32>() + ENTRY_PREFIX_BYTES;
/// Format: a whole-state message's first byte: an install (owner to holder).
const TAG_INSTALL: u8 = 1;
/// Format: a whole-state message's first byte: a takeover's fetch (successor to holder).
const TAG_FETCH: u8 = 2;
/// Format: a whole-state message's first byte: phase one's report (successor to holder; A-240).
const TAG_REPORT: u8 = 3;
/// Format: a whole-state message's first byte: a claim's admission (owner to candidate; §4.2, A-401).
const TAG_ADMIT: u8 = 4;
/// Format: an [`Admit`]'s size kind: a bounded claim.
const SIZE_BOUNDED: u8 = 0;
/// Format: an [`Admit`]'s size kind: a dynamic volume.
const SIZE_DYNAMIC: u8 = 1;
/// Format: a reply's first byte: a head held, a holder with nothing, or a fence refusal.
const REPLY_HEAD: u8 = 1;
/// Format: see [`REPLY_HEAD`].
const REPLY_EMPTY: u8 = 2;
/// Format: see [`REPLY_HEAD`].
const REPLY_STALE: u8 = 3;
/// Format: see [`REPLY_HEAD`]: the holder has no room for the volume's admission ([`LiveError::NoRoom`]).
const REPLY_NO_ROOM: u8 = 4;

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
  /// The entries the owner holds that are not durable would pass its credit's bytes with this one ([`Bounds::credit`]);
  /// `durable` is the durable position (`None`: nothing yet). The writer waits for acknowledgements (AC-8.9).
  NoCredit {
    /// The durable position.
    durable: Option<u64>,
  },
  /// The holder cannot make the volume's admission against its own machine (§4.2 "a remote holder makes the same
  /// admission against its own machine before acknowledging placement"): the claim a successor's restore takes, or the
  /// held log's bytes past it. `available` is what the holder could admit now. The holder takes nothing; the owner
  /// counts it as no acknowledgement.
  NoRoom {
    /// The bytes the holder could admit now.
    available: u64,
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

  /// The bytes the entry takes on the wire, its fixed prefix and its payload: what the owner's bounds count it at
  /// ([`Bounds`]).
  pub fn wire_bytes(&self) -> usize {
    ENTRY_PREFIX_BYTES.saturating_add(self.payload.len())
  }

  /// The entry's identity: the BLAKE3 of its canonical bytes ([`Entry::encode`]), binding its place in the chain and its
  /// payload. Hashed as they stream, not from a copy of them: an append of a 2.4 MB payload copied it into the canonical
  /// bytes once for each identity asked (w-forming, 2026-11-20).
  pub fn identity(&self) -> Identity {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&self.owner.0.to_le_bytes());
    hasher.update(&self.object.0);
    hasher.update(&self.position.to_le_bytes());
    hasher.update(&self.epoch.0.to_le_bytes());
    hasher.update(&self.previous);
    let length = u32::try_from(self.payload.len()).unwrap_or(u32::MAX);
    hasher.update(&length.to_le_bytes());
    hasher.update(&self.payload);
    *hasher.finalize().as_bytes()
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

/// The object an encoded entry or batch names, read without decoding the rest: both begin with the owner's word and
/// then the object (the formats above; an install's tag comes first, [`object_of_state`]). `None` for bytes too short
/// to hold it.
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

/// Several consecutive entries of one owner's log as one message to one holder (A-200 amended 2026-11-17): the
/// catch-up a holder lacks, sent in one exchange rather than one exchange an entry. `encoded` are entries as
/// [`Entry::encode`] made them, in chain order, all of one owner, object and epoch (a shipper's retained entries are: it
/// appends only under its own epoch). The holder applies them in order exactly as it applies one entry
/// ([`Replica::check`], [`decode_batch`]) and answers once, with the head it reached. `None` when `encoded` is empty or
/// its first entry is too short to name its owner, object and epoch.
pub fn encode_batch(encoded: &[&[u8]]) -> Option<Vec<u8>> {
  let first = encoded.first()?;
  let mut reader = Reader { bytes: first };
  let owner = reader.word().ok()?;
  let object = reader.object().ok()?;
  let epoch = epoch_of(first)?;
  let bytes = encoded.iter().fold(BATCH_PREFIX_BYTES, |sum, entry| {
    sum
      .saturating_add(size_of::<u32>())
      .saturating_add(entry.len())
  });
  let mut out = Vec::with_capacity(bytes);
  out.extend_from_slice(&owner.to_le_bytes());
  out.extend_from_slice(&object.0);
  out.extend_from_slice(&epoch.0.to_le_bytes());
  out.extend_from_slice(&u32::try_from(encoded.len()).ok()?.to_le_bytes());
  for entry in encoded {
    out.extend_from_slice(&u32::try_from(entry.len()).ok()?.to_le_bytes());
    out.extend_from_slice(entry);
  }
  Some(out)
}

/// Parses a batch that crossed the network ([`encode_batch`]): every entry, in order. Each is refused over
/// `payload_cap` before it is copied, as [`Entry::decode`] refuses one; a count that claims more entries than the bytes
/// that follow could hold, an entry that names another owner, object or epoch than the batch, an empty batch, and
/// trailing bytes are each refused [`LiveError::Malformed`].
pub fn decode_batch(bytes: &[u8], payload_cap: usize) -> Result<Vec<Entry>, LiveError> {
  let mut reader = Reader { bytes };
  let owner = HostId(reader.word()?);
  let object = reader.object()?;
  let epoch = HostEpoch(reader.word()?);
  let count = reader.count()?;
  let room = reader.bytes.len() / BATCHED_ENTRY_MIN_BYTES;
  if count == 0 || count > room {
    return Err(LiveError::Malformed);
  }
  let entry_cap = ENTRY_PREFIX_BYTES.saturating_add(payload_cap);
  let mut entries = Vec::with_capacity(count);
  for _ in 0..count {
    let entry = Entry::decode(&reader.prefixed(entry_cap)?, payload_cap)?;
    if entry.owner != owner || entry.object != object || entry.epoch != epoch {
      return Err(LiveError::Malformed);
    }
    entries.push(entry);
  }
  if reader.bytes.is_empty() {
    Ok(entries)
  } else {
    Err(LiveError::Malformed)
  }
}

/// The epoch a batch's entries were made under, read from its header alone ([`encode_batch`]), as [`epoch_of`] reads
/// an entry's. `None` for bytes too short.
pub fn epoch_of_batch(bytes: &[u8]) -> Option<HostEpoch> {
  let mut reader = Reader { bytes };
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
    TAG_FETCH | TAG_REPORT => reader.object().ok(),
    _ => None,
  }
}

/// Whether a whole-state message is a [`Report`] (phase one's question, A-240).
pub fn is_report(bytes: &[u8]) -> bool {
  bytes.first() == Some(&TAG_REPORT)
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

/// Phase one's question for a live volume (A-200, A-240): the successor asks a holder to raise its fence to `epoch` and
/// report its head and how many bytes it keeps, nothing more. Every answer is a few bytes, so phase one runs within the
/// liveness budget over any path; the log itself is fetched afterwards from the one holder adopted ([`Fetch`]), under a
/// budget its reported bytes derive. Asked in one exchange, every holder's whole log missed a 1 s budget over KIND's
/// shaped paths, and no takeover finished (run 42357611870, `wan`, `wan-loss` and `ceiling`, 2026-11-07).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
  /// The volume.
  pub object: ObjectId,
  /// The epoch the successor ships under.
  pub epoch: HostEpoch,
}

impl Report {
  /// The canonical bytes: its tag, the object, the epoch.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(TAG_REPORT);
    out.extend_from_slice(&self.object.0);
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out
  }

  /// Parses bytes that crossed the network.
  pub fn decode(bytes: &[u8]) -> Result<Report, LiveError> {
    let mut reader = Reader { bytes };
    if reader.byte()? != TAG_REPORT {
      return Err(LiveError::Malformed);
    }
    let report = Report {
      object: reader.object()?,
      epoch: HostEpoch(reader.word()?),
    };
    if reader.bytes.is_empty() {
      Ok(report)
    } else {
      Err(LiveError::Malformed)
    }
  }
}

/// An owner's ask, before it turns a volume's live shipping on, that a candidate make the admission a successor's restore
/// would make (§4.2: "a remote holder makes the same admission against its own machine before acknowledging placement";
/// A-401): the volume, its size class and whether it must live in locked RAM, as its catalog records them. Answered with
/// the empty head when the candidate holds the claim ([`encode_reply`]'s `Ok(None)`), [`LiveError::NoRoom`] when it
/// cannot make it, and nothing when it will not serve the owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admit {
  /// The volume.
  pub object: ObjectId,
  /// Its size class.
  pub size: crate::catalog::SizeClass,
  /// Whether its content must live in locked RAM.
  pub require_locked: bool,
}

impl Admit {
  /// The canonical bytes: its tag, the object, the size's kind and bytes, and the locked flag.
  pub fn encode(&self) -> Vec<u8> {
    let (kind, bytes) = match self.size {
      crate::catalog::SizeClass::Bounded { limit } => (SIZE_BOUNDED, limit),
      crate::catalog::SizeClass::Dynamic { max } => (SIZE_DYNAMIC, max),
    };
    let mut out = Vec::new();
    out.push(TAG_ADMIT);
    out.extend_from_slice(&self.object.0);
    out.push(kind);
    out.extend_from_slice(&bytes.to_le_bytes());
    out.push(u8::from(self.require_locked));
    out
  }

  /// Parses bytes that crossed the network: `Malformed` for another tag, an unknown size kind or locked flag, or any
  /// length but the canonical one.
  pub fn decode(bytes: &[u8]) -> Result<Admit, LiveError> {
    let mut reader = Reader { bytes };
    if reader.byte()? != TAG_ADMIT {
      return Err(LiveError::Malformed);
    }
    let object = reader.object()?;
    let kind = reader.byte()?;
    let bytes = reader.word()?;
    let size = match kind {
      SIZE_BOUNDED => crate::catalog::SizeClass::Bounded { limit: bytes },
      SIZE_DYNAMIC => crate::catalog::SizeClass::Dynamic { max: bytes },
      _ => return Err(LiveError::Malformed),
    };
    let require_locked = match reader.byte()? {
      0 => false,
      1 => true,
      _ => return Err(LiveError::Malformed),
    };
    if reader.bytes.is_empty() {
      Ok(Admit {
        object,
        size,
        require_locked,
      })
    } else {
      Err(LiveError::Malformed)
    }
  }
}

/// Whether a whole-state message is an [`Admit`].
pub fn is_admit(bytes: &[u8]) -> bool {
  bytes.first() == Some(&TAG_ADMIT)
}

/// A holder's answer to a [`Report`]: the bytes it keeps of the log, then its reply (the head, or the fence refusing a
/// stale successor).
pub fn encode_reported(reply: &Result<Option<Head>, LiveError>, kept: u64) -> Vec<u8> {
  let mut out = kept.to_le_bytes().to_vec();
  out.extend_from_slice(&encode_reply(reply));
  out
}

/// Parses a holder's answer to a [`Report`]: its reply and the bytes it keeps.
pub fn decode_reported(bytes: &[u8]) -> Result<(Result<Option<Head>, LiveError>, u64), LiveError> {
  let mut reader = Reader { bytes };
  let kept = reader.word()?;
  Ok((decode_reply(reader.bytes), kept))
}

/// Format: the bytes of the length that prefixes each payload of a holder's answer to a [`Fetch`] (a `u32` LE).
pub const FETCHED_PREFIX_BYTES: usize = size_of::<u32>();

/// The start of a holder's answer to a [`Fetch`]: its reply (the head, or the fence refusing a stale successor),
/// `u32`-length-prefixed, in a buffer with room for `payload_bytes` more. The payloads it keeps follow in order, each
/// appended by [`push_fetched`], so a candidate lays a long log out a slice at a time (g251) and no payload is copied
/// twice: laid out whole in one turn, after a clone of every entry, a 300 MB log was two copies of it in the turn.
pub fn fetched_reply(reply: &Result<Option<Head>, LiveError>, payload_bytes: usize) -> Vec<u8> {
  let head = encode_reply(reply);
  let mut out = Vec::with_capacity(
    FETCHED_PREFIX_BYTES
      .saturating_add(head.len())
      .saturating_add(payload_bytes),
  );
  out.extend_from_slice(&u32::try_from(head.len()).unwrap_or(u32::MAX).to_le_bytes());
  out.extend_from_slice(&head);
  out
}

/// Appends one payload to an answer [`fetched_reply`] began: its length and its bytes.
pub fn push_fetched(out: &mut Vec<u8>, payload: &[u8]) {
  out.extend_from_slice(
    &u32::try_from(payload.len())
      .unwrap_or(u32::MAX)
      .to_le_bytes(),
  );
  out.extend_from_slice(payload);
}

/// A holder's answer to a [`Fetch`] as it crossed the network (g251): its reply decoded, its payloads left in place in
/// the answer's bytes and read one at a time ([`FetchedLog::next_payload`]), each refused over the cap before it is
/// used. Copied out whole, a 300 MB log's payloads took one turn of the successor's control shard; read in place, each
/// costs only its own apply, in whichever turn the rebuild applies it.
#[derive(Debug)]
pub struct FetchedLog {
  /// The answer's bytes.
  bytes: Vec<u8>,
  /// Where the next payload's length begins.
  next: usize,
  /// The largest payload taken.
  cap: usize,
  /// The payloads taken so far.
  taken: usize,
}

impl FetchedLog {
  /// Parses a holder's answer: its reply, refused [`LiveError::Malformed`] when cut short or over `cap`, and the log of
  /// its payloads, each read and checked against `cap` only when it is taken.
  pub fn decode(bytes: Vec<u8>, cap: usize) -> Result<(Option<Head>, FetchedLog), LiveError> {
    let mut reader = Reader { bytes: &bytes };
    let head = decode_reply(&reader.prefixed(cap)?)?;
    let next = bytes
      .len()
      .checked_sub(reader.bytes.len())
      .ok_or(LiveError::Malformed)?;
    Ok((
      head,
      FetchedLog {
        bytes,
        next,
        cap,
        taken: 0,
      },
    ))
  }

  /// The next payload, `None` once every one was taken. Refused [`LiveError::Malformed`] when its length runs past the
  /// answer or over the cap, after which the log is spent.
  pub fn next_payload(&mut self) -> Option<Result<&[u8], LiveError>> {
    match self.peek()? {
      Ok(length) => {
        let start = self.next.saturating_add(FETCHED_PREFIX_BYTES);
        let end = start.saturating_add(length);
        self.next = end;
        self.taken = self.taken.saturating_add(1);
        self.bytes.get(start..end).map(Ok)
      }
      Err(refusal) => {
        self.next = self.bytes.len();
        Some(Err(refusal))
      }
    }
  }

  /// The next payload without taking it (a log's first, whose sealing a takeover reads before its rebuild begins);
  /// `None` when every one was taken or the next is malformed.
  pub fn first(&self) -> Option<&[u8]> {
    let length = self.peek()?.ok()?;
    let start = self.next.saturating_add(FETCHED_PREFIX_BYTES);
    self.bytes.get(start..start.saturating_add(length))
  }

  /// Whether every payload was taken.
  pub fn is_spent(&self) -> bool {
    self.next >= self.bytes.len()
  }

  /// The payloads taken so far.
  pub fn taken(&self) -> usize {
    self.taken
  }

  /// The next payload's length, checked to lie within the answer and the cap; `None` when none is left.
  fn peek(&self) -> Option<Result<usize, LiveError>> {
    let rest = self
      .bytes
      .get(self.next..)
      .filter(|rest| !rest.is_empty())?;
    let mut reader = Reader { bytes: rest };
    Some(reader.count().and_then(|length| {
      if length > self.cap || reader.bytes.len() < length {
        return Err(LiveError::Malformed);
      }
      Ok(length)
    }))
  }
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
    Err(LiveError::NoRoom { available }) => {
      out.push(REPLY_NO_ROOM);
      out.extend_from_slice(&available.to_le_bytes());
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
    REPLY_NO_ROOM => Err(LiveError::NoRoom {
      available: reader.word()?,
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

  /// A u32 LE count, as a `usize`.
  fn count(&mut self) -> Result<usize, LiveError> {
    let bytes = self.take(size_of::<u32>())?;
    let count = u32::from_le_bytes(bytes.try_into().map_err(|_| LiveError::Malformed)?);
    usize::try_from(count).map_err(|_| LiveError::Malformed)
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

  /// A replica rebuilt from what its holder kept of it ([`Replica::owner`], [`Replica::fence`], [`Replica::head`]): a
  /// warm restart keeps every replica the holder acknowledged, its fence with it (§4.8 "Recovery"), or a holder that
  /// restarted would report less than it acknowledged, and promise less than it had.
  pub fn recovered(
    owner: HostId,
    object: ObjectId,
    fence: HostEpoch,
    head: Option<Head>,
  ) -> Replica {
    Replica {
      owner,
      object,
      fence: Fence { seen: fence },
      head,
    }
  }

  /// The owner it takes messages from.
  pub fn owner(&self) -> HostId {
    self.owner
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
/// `W` of those `n` holders, and `W + f + 1 > n` ([`Shipper::reached_by`]), so `f + 1` of them meet every durable entry;
/// a report from any other member (one that
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

/// Whether a live takeover's phase one is past tolerance (A-309): fewer than `f + 1` of the recorded `holders` are still
/// members (`is_member`), so [`adopt`]'s count can never be met. More of the log's holders failed than the region
/// tolerates; A-240's count waited for good (g240's finding: at `f = 1` a set recorded as {survivor, destroyed node}).
pub fn past_tolerance(
  holders: &[HostId],
  is_member: impl Fn(HostId) -> bool,
  quorum: Quorum,
) -> bool {
  let Some(needed) = usize::try_from(quorum.f)
    .ok()
    .and_then(|f| f.checked_add(1))
  else {
    return false;
  };
  holders.iter().filter(|host| is_member(**host)).count() < needed
}

/// Phase one's choice past tolerance ([`adopt_past_tolerance`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PastTolerance {
  /// A surviving host that may hold the log has not reported yet.
  Waiting,
  /// Every surviving host that may hold the log has reported: the holder of the greatest head and that head, `None`
  /// when none holds any of the log.
  Adopt(Option<(HostId, Head)>),
}

/// Phase one's choice past tolerance (A-309): once every host of `wait` has reported, the greatest head among `reports`.
/// `wait` is every surviving member that may hold the log: the recorded holders still members and the members of the
/// retired owner's cohorts for the volume, settled and current (where a joint transition shipped). No count of
/// recorded holders can meet every durable entry past tolerance; waiting for every surviving holder means the newest
/// head any of them keeps is never passed over, the replacement that caught up on the log among them.
pub fn adopt_past_tolerance(reports: &[(HostId, Option<Head>)], wait: &[HostId]) -> PastTolerance {
  if !wait
    .iter()
    .all(|host| reports.iter().any(|(reporter, _)| reporter == host))
  {
    return PastTolerance::Waiting;
  }
  PastTolerance::Adopt(
    reports
      .iter()
      .filter_map(|(holder, head)| head.map(|head| (*holder, head)))
      .max_by_key(|(_, head)| head.rank()),
  )
}

/// What the owner keeps of its log beyond its own state, in bytes of entries at their wire size
/// ([`Entry::wire_bytes`]; AC-8.9).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bounds {
  /// The deltas appended and not yet durable, at most, counting the one being appended ([`Shipper::append`]); a whole
  /// state may go one entry past it ([`Shipper::append_checkpoint`]) and is not counted against the deltas behind it
  /// (A-371). One delta is admitted whatever its size while no delta is in flight, so zero still lets the log go one
  /// entry at a time.
  pub credit: usize,
  /// The entries retained for laggards, at most; past it the oldest is dropped and a holder that lacks it is installed.
  /// The newest entry is never dropped for size alone, and an entry every holder holds is not kept at all.
  pub retain: usize,
}

impl Bounds {
  /// Derived (A-200 amended 2026-11-17): both bounds are `whole_bytes`, the size of the whole state an install would
  /// send (the caller's measurement: its last whole record or install image). A delta is then turned away only when the
  /// deltas outstanding would already cost more than shipping the volume whole, which is what a turned-away delta
  /// leads to; the owner's memory for them stays within one whole state; and a laggard is sent the entries it lacks
  /// only while they cost no more than installing it. The bounds follow the volume as it grows, at each whole record
  /// and install image the caller makes.
  pub fn of_whole(whole_bytes: usize) -> Bounds {
    Bounds {
      credit: whole_bytes,
      retain: whole_bytes,
    }
  }
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

/// What a holder's answer leaves it lacking, as the owner's fold names it ([`Shipper::on_reply`] builds it).
enum Folded {
  /// Nothing, or nothing to send it (a refusal, a host it does not ship to).
  Nothing,
  /// The retained entries after this position (all of them when `None`).
  After(Option<u64>),
  /// An install: it is off the chain or behind the retained tail.
  Install,
  /// A later epoch has superseded this owner.
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
  /// The bytes it holds in flight and retains for laggards ([`Shipper::set_bounds`]).
  bounds: Bounds,
  /// The entries appended here and not yet durable, oldest first: each one's position and wire bytes, and whether it is
  /// a whole state ([`Shipper::append_checkpoint`]). Kept apart from `retained`, which a checkpoint past the credit can
  /// trim below them, so the credit counts exactly what is in flight. Bounded: the deltas within the credit, and the
  /// whole states admitted one past it.
  unsettled: VecDeque<(u64, usize, bool)>,
  /// The sum of `unsettled`'s bytes.
  unsettled_bytes: usize,
  /// The sum of the bytes of `unsettled`'s whole states: a whole state rides one past the credit, so the credit a delta
  /// is held to counts the rest alone (A-371).
  unsettled_whole_bytes: usize,
  /// The head the owner started from (a takeover's adopted head), which it holds and every holder may report.
  start: Option<Head>,
  /// The position its first entry takes when it has no `start`: 0 for a new log, and for an owner restarted over its
  /// own log one past every position it may have shipped before ([`Shipper::resumed`], A-344).
  first: u64,
  /// The owner's head: its last entry, or `start` before its first.
  head: Option<Head>,
  /// The retained entries, oldest first, dense in position.
  retained: VecDeque<Entry>,
  /// What each holder holds on this chain, as last reported.
  /// One head a host, for hosts it ships to only: a reply from any other is dropped ([`Shipper::on_reply`]) and a host
  /// leaving the holder set is forgotten ([`Shipper::activate`]), so at most the holders and the transition's target.
  held: BTreeMap<HostId, Head>,
  /// The highest position held by the owner and enough holders that every `f + 1` a takeover counts meet it.
  durable: Option<u64>,
  /// The head of the last whole state the owner shipped ([`Shipper::mark_checkpoint`]): on the chain, and what an
  /// install names, so a holder installed there is credited even once the retained tail no longer reaches it.
  checkpoint: Option<Head>,
  /// The holders whose last answer said they need an install (off the chain, or behind the retained tail), until one
  /// answers with a head on the chain: [`Shipper::pending`] answers `Install` for them.
  owed_install: std::collections::BTreeSet<HostId>,
  /// The holder set it is moving to ([`Shipper::begin_transition`]), sorted, until the record names it.
  transition: Option<Vec<HostId>>,
}

/// The identity of the entry a [`Shipper`] will append next, hashed a slice of its payload at a time
/// ([`Shipper::entry_hasher`]): BLAKE3 hashes incrementally, and the entry's place in the chain is fixed while the owner
/// feeds it, so an owner shard hashes a whole state across its turns instead of in one (A-757 part 2, w-forming,
/// 2026-11-20: 0.94 ms of release CPU for 2.4 MB in one turn, against seal quanta of 14 to 784 us).
#[derive(Clone, Debug)]
pub struct EntryHasher {
  hasher: blake3::Hasher,
  position: u64,
  previous: Identity,
  epoch: HostEpoch,
  length: usize,
  fed: usize,
}

impl EntryHasher {
  /// Feeds the next `bytes` of the payload, in order.
  pub fn feed(&mut self, bytes: &[u8]) {
    self.hasher.update(bytes);
    self.fed = self.fed.saturating_add(bytes.len());
  }

  /// The payload bytes fed so far.
  pub fn fed(&self) -> usize {
    self.fed
  }

  /// The payload's length the hasher was seeded with.
  pub fn length(&self) -> usize {
    self.length
  }
}

/// Whether an append returns its entry to the caller as well as keeping it in the retained tail
/// ([`Shipper::append_kept`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kept {
  /// Kept and returned: a copy of the entry back.
  Returned,
  /// Kept alone: its head back.
  Retained,
}

impl Shipper {
  /// The owner's side for `object` under `epoch`, shipping to `holders` (the candidates other than the owner: `2f` of
  /// them usually, `2f + 1` when the placement moved off the owner, fewer while a fleet forms), starting from
  /// `start` (`None` for a new volume; a takeover's adopted head otherwise). `bounds` holds the entries appended here
  /// that are not yet durable and the entries kept for laggards ([`Bounds`]); [`Shipper::set_bounds`] moves them as the
  /// caller measures the whole state.
  pub fn new(
    (owner, object, epoch): (HostId, ObjectId, HostEpoch),
    quorum: Quorum,
    holders: &[HostId],
    start: Option<Head>,
    bounds: Bounds,
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
      bounds,
      unsettled: VecDeque::new(),
      unsettled_bytes: 0,
      unsettled_whole_bytes: 0,
      start,
      first: 0,
      head: start,
      retained: VecDeque::new(),
      held: BTreeMap::new(),
      durable: None,
      checkpoint: None,
      owed_install: std::collections::BTreeSet::new(),
      transition: None,
    };
    shipper.refresh_durable();
    shipper
  }

  /// The owner's side after its own restart over its log (A-344; §4.10): the log it shipped before is gone with its
  /// memory, and `past` is the greatest position it may have shipped under `epoch` (`None`: none), which its recovery
  /// image kept. Its first entry takes position `past + 1` under the same epoch, so no position it may already have
  /// shipped is used again: a holder holds the old chain or nothing, and each is installed from the new chain's first
  /// entry, which ranks above every entry of the old. Nothing before that entry is settled or durable here: no holder
  /// has acknowledged anything of this chain. Before A-344 a restarted owner began again at position 0 under the same
  /// epoch, and the old chain and the new were two values at the same positions (Agreement; a takeover adopted the
  /// old chain's greater head and lost what the new one had made durable). Refused [`LiveError::Malformed`] at the
  /// last position.
  pub fn resumed(
    (owner, object, epoch): (HostId, ObjectId, HostEpoch),
    quorum: Quorum,
    holders: &[HostId],
    past: Option<u64>,
    bounds: Bounds,
  ) -> Result<Shipper, LiveError> {
    let first = match past {
      Some(past) => past.checked_add(1).ok_or(LiveError::Malformed)?,
      None => 0,
    };
    let mut shipper = Shipper::new((owner, object, epoch), quorum, holders, None, bounds);
    shipper.first = first;
    shipper.refresh_durable();
    Ok(shipper)
  }

  /// The position the next entry takes ([`Shipper::append`]): what an owner's recovery image keeps, so a restart
  /// resumes past it (A-344). `None` past the last position.
  pub fn next_position(&self) -> Option<u64> {
    match self.head {
      Some(head) => head.position.checked_add(1),
      None => Some(self.first),
    }
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

  /// The highest position the owner and enough holders hold that every `f + 1` of them a takeover can count meets it
  /// (`n − f` of `n` holders; `None`: none yet). A barrier covering an entry at or below it may answer (A-200).
  pub fn durable(&self) -> Option<u64> {
    self.durable
  }

  /// The holders it ships to.
  pub fn holders(&self) -> &[HostId] {
    &self.holders
  }

  /// The holder set it is moving to, while a transition is under way ([`Shipper::begin_transition`]).
  pub fn transition(&self) -> Option<&[HostId]> {
    self.transition.as_deref()
  }

  /// Every host it ships to: the holders, then the transition's members not among them.
  pub fn targets(&self) -> impl Iterator<Item = HostId> + '_ {
    let joining = self
      .transition
      .iter()
      .flatten()
      .filter(|host| !self.holders.contains(host));
    self.holders.iter().chain(joining).copied()
  }

  /// Begins moving to `next` (the volume's other candidates as the configuration now names them; the owner is left
  /// out; refused for a set of `f` or fewer, from which no takeover counts `f + 1`), a change of holder set in the manner of Vertical Paxos (Lamport, Malkhi and Zhou, PODC 2009): until the
  /// record names `next` ([`Shipper::activate`]), the shipper ships to both sets and counts a position durable only once
  /// `f` of each hold it, so a takeover counting either set meets every durable entry. A member of `next` is owed
  /// everything it lacks (entries, or an install once the retained tail no longer reaches back). Whether the target
  /// changed.
  ///
  /// A target that already holds every durable entry ([`Shipper::transferred`]) is kept until it is activated, since its
  /// record may be in flight; before that a new target replaces it, and naming the holders again ends the transition.
  /// Replacing the holders outright let a takeover count a new set that held none of the durable entries: 0 of `f`
  /// common members at `f = 1` broke NoLoss in the model (`tests/live.rs`, 2026-11-07).
  pub fn begin_transition(&mut self, next: &[HostId]) -> bool {
    let named = self.named(next);
    // A set of `f` or fewer members is never moved to: no takeover can count `f + 1` reports from it, so a log recorded
    // on it could not be taken over, and while the record named it every write waited on its few members. The current
    // holders keep making `W` with the members still alive. Moving to such a set (a candidate found dead leaves one
    // other at `f = 1`) stalled writes for a partition's length when the lone member stopped before the move back was
    // recorded (live-durability's rolling partition flow, 4 of 6 runs on each arch at 8b0f0341).
    let tolerated = usize::try_from(self.quorum.f).unwrap_or(usize::MAX);
    if tolerated > 0 && named.len() <= tolerated {
      return false;
    }
    if self.transition.as_deref() == Some(named.as_slice())
      || (self.transition.is_some() && self.transferred())
    {
      return false;
    }
    let mut active = self.holders.clone();
    active.sort_unstable();
    let changed = if named == active {
      self.transition.take().is_some()
    } else {
      self.transition = Some(named);
      true
    };
    self.forget_unnamed();
    self.refresh_durable();
    changed
  }

  /// Whether the transition's target holds every durable entry: `f` of its members each hold the durable position and
  /// the head the shipper started from (always, while neither exists). Only then may the record name it, and the shipper activate it. `false` with no
  /// transition.
  pub fn transferred(&self) -> bool {
    let Some(next) = &self.transition else {
      return false;
    };
    // A successor's start may hold writes made durable under the owner it took over, which its own durable position
    // has not counted yet: the target holds them too before the record may name it. Without this a successor moved to
    // a set disjoint from the old at once, and the next takeover adopted nothing (the model, f = 2, 2026-11-07).
    let start = self.start.map(|start| start.position);
    // A restarted owner's first entry carries every durable entry of the chain before it (A-344): the target holds
    // them once it holds that entry.
    let resumed = (self.first > 0).then_some(self.first);
    let Some(floor) = self.durable.max(start).max(resumed) else {
      return true;
    };
    self
      .reached_by(next)
      .is_some_and(|reached| reached >= floor)
  }

  /// Ends the transition once the record names its target: `recorded` is the holder set the volume's record now holds.
  /// Refused (`false`) unless it is the transition's target and that target holds every durable entry
  /// ([`Shipper::transferred`]); from then on the shipper ships to it alone and counts durability over it alone.
  pub fn activate(&mut self, recorded: &[HostId]) -> bool {
    let named = self.named(recorded);
    if self.transition.as_deref() != Some(named.as_slice()) || !self.transferred() {
      return false;
    }
    self.holders = named;
    self.transition = None;
    self.forget_unnamed();
    self.refresh_durable();
    true
  }

  /// `hosts` without the owner, sorted, each once.
  fn named(&self, hosts: &[HostId]) -> Vec<HostId> {
    let mut named: Vec<HostId> = hosts
      .iter()
      .copied()
      .filter(|host| *host != self.owner)
      .collect();
    named.sort_unstable();
    named.dedup();
    named
  }

  /// Whether the shipper ships to `host` (a holder, or a member of the transition's target).
  fn ships_to(&self, host: HostId) -> bool {
    self.holders.contains(&host)
      || self
        .transition
        .as_ref()
        .is_some_and(|next| next.contains(&host))
  }

  /// Forgets what hosts it no longer ships to reported, and trims for those it does.
  fn forget_unnamed(&mut self) {
    let named: Vec<HostId> = self.targets().collect();
    self.held.retain(|host, _| named.contains(host));
    self.owed_install.retain(|host| named.contains(host));
    self.trim();
  }

  /// The entries appended here that are not yet durable.
  pub fn in_flight(&self) -> u64 {
    let Some(head) = self.head else {
      return 0;
    };
    match self.settled_through() {
      Some(floor) => head.position.saturating_sub(floor),
      None => head.position.saturating_add(1),
    }
  }

  /// The last position nothing appended here waits on: the durable position, the head it started from, or the last
  /// position before a restarted owner's first (A-344), whichever is highest (`None`: none of them).
  fn settled_through(&self) -> Option<u64> {
    let before_first = self.first.checked_sub(1);
    [
      self.durable,
      self.start.map(|start| start.position),
      before_first,
    ]
    .into_iter()
    .flatten()
    .max()
  }

  /// The wire bytes of the entries appended here that are not yet durable, whole states included: [`Bounds::credit`]
  /// holds the deltas among them, and a whole state is admitted only while all of them are within it.
  pub fn in_flight_bytes(&self) -> usize {
    self.unsettled_bytes
  }

  /// The bounds it holds its log to.
  pub fn bounds(&self) -> Bounds {
    self.bounds
  }

  /// Appends the next publication to the chain: the entry to send to every holder. Refused [`LiveError::NoCredit`]
  /// when deltas are in flight and their bytes with this one's would pass [`Bounds::credit`]; the writer waits for
  /// acknowledgements (AC-8.9). A whole state in flight is not counted: it rides one past the credit
  /// ([`Shipper::append_checkpoint`], A-371). With no delta in flight it is admitted whatever its size, so the log always
  /// moves.
  pub fn append(&mut self, payload: Vec<u8>) -> Result<Entry, LiveError> {
    self.admit(payload.len())?;
    let (_, entry) = self.append_unchecked(payload, false, Kept::Returned)?;
    entry.ok_or(LiveError::Malformed)
  }

  /// Whether a delta of `payload_len` bytes is admitted now ([`Shipper::append`]'s credit).
  fn admit(&self, payload_len: usize) -> Result<(), LiveError> {
    let bytes = ENTRY_PREFIX_BYTES.saturating_add(payload_len);
    // The deltas in flight, a whole state's bytes left out: a whole state rides one past the credit (A-371). Counted
    // with it, the whole entry the daemon has just appended filled the credit it sets from that entry's size, and every
    // delta after it was refused until the whole state was durable.
    let deltas = self
      .unsettled_bytes
      .saturating_sub(self.unsettled_whole_bytes);
    if deltas > 0 && deltas.saturating_add(bytes) > self.bounds.credit {
      return Err(LiveError::NoCredit {
        durable: self.durable,
      });
    }
    Ok(())
  }

  /// Whether a whole state would be admitted now ([`Shipper::append_checkpoint`]): what is in flight is within the
  /// credit.
  pub fn admits_checkpoint(&self) -> bool {
    self.unsettled_bytes <= self.bounds.credit
  }

  /// Appends a whole state (a checkpoint, which [`Shipper::mark_checkpoint`] then names), allowed one entry past the
  /// credit: a checkpoint is how holders that fell behind the retained tail are caught up, and when every holder has,
  /// nothing becomes durable to free the credit, so a checkpoint that waited for it would never come (both candidates
  /// stalled through 30 writes waited 19 minutes, 2026-11-05, `live_fleet`). Still bounded: refused once what is in
  /// flight already passes the credit, so at most the credit and one whole state are ever in flight.
  pub fn append_checkpoint(&mut self, payload: Vec<u8>) -> Result<Entry, LiveError> {
    if !self.admits_checkpoint() {
      return Err(LiveError::NoCredit {
        durable: self.durable,
      });
    }
    let (_, entry) = self.append_unchecked(payload, true, Kept::Returned)?;
    entry.ok_or(LiveError::Malformed)
  }

  /// [`Shipper::append`] for an owner that needs only the head it appended: the entry goes into the retained tail, not
  /// also back to the caller, and its identity is hashed once. `live_ship` appended through [`Shipper::append`] and asked
  /// the entry's head twice more, each a copy and a hash of the payload: 3.3 to 4.4 ms for 2.4 MB in release, against
  /// seal quanta of 14 to 784 us (w-forming, 2026-11-20).
  pub fn append_kept(&mut self, payload: Vec<u8>) -> Result<Head, LiveError> {
    self.admit(payload.len())?;
    self
      .append_unchecked(payload, false, Kept::Retained)
      .map(|(head, _)| head)
  }

  /// [`Shipper::append_checkpoint`] as [`Shipper::append_kept`] appends: the head alone back.
  pub fn append_checkpoint_kept(&mut self, payload: Vec<u8>) -> Result<Head, LiveError> {
    if !self.admits_checkpoint() {
      return Err(LiveError::NoCredit {
        durable: self.durable,
      });
    }
    self
      .append_unchecked(payload, true, Kept::Retained)
      .map(|(head, _)| head)
  }

  /// Appends `payload` at the next position, kept unsettled as a whole state when `whole`: its head, and the entry too
  /// when `kept` returns it.
  fn append_unchecked(
    &mut self,
    payload: Vec<u8>,
    whole: bool,
    kept: Kept,
  ) -> Result<(Head, Option<Entry>), LiveError> {
    let (position, previous) = self.next_link()?;
    let entry = Entry {
      owner: self.owner,
      object: self.object,
      position,
      epoch: self.epoch,
      previous,
      payload,
    };
    let head = entry.head();
    Ok(self.push_entry(entry, head, whole, kept))
  }

  /// The position and previous identity the next entry takes.
  fn next_link(&self) -> Result<(u64, Identity), LiveError> {
    Ok(match self.head {
      Some(head) => (
        head.position.checked_add(1).ok_or(LiveError::Malformed)?,
        head.identity,
      ),
      None => (self.first, GENESIS),
    })
  }

  /// A hasher of the next entry's identity, seeded with its place in the chain and a payload of `payload_len` bytes,
  /// for an owner that feeds the payload a slice a turn ([`EntryHasher`]) and appends it with
  /// [`Shipper::append_checkpoint_hashed`]. `Malformed` past the last position or for a payload whose length the
  /// canonical bytes cannot carry.
  pub fn entry_hasher(&self, payload_len: usize) -> Result<EntryHasher, LiveError> {
    let (position, previous) = self.next_link()?;
    let length = u32::try_from(payload_len).map_err(|_| LiveError::Malformed)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&self.owner.0.to_le_bytes());
    hasher.update(&self.object.0);
    hasher.update(&position.to_le_bytes());
    hasher.update(&self.epoch.0.to_le_bytes());
    hasher.update(&previous);
    hasher.update(&length.to_le_bytes());
    Ok(EntryHasher {
      hasher,
      position,
      previous,
      epoch: self.epoch,
      length: payload_len,
      fed: 0,
    })
  }

  /// [`Shipper::append_checkpoint_kept`] with the identity `hashed` hashed over `payload` in turns: no hash here. Refused
  /// `Behind` when the hasher was seeded for another place than the next one (the chain moved since) or fed other than
  /// `payload`'s length, so the owner hashes again; `NoCredit` as a checkpoint is. The owner feeds `hashed` exactly
  /// `payload`, in order: an identity over other bytes is one no holder's chain admits.
  pub fn append_checkpoint_hashed(
    &mut self,
    payload: Vec<u8>,
    hashed: EntryHasher,
  ) -> Result<Head, LiveError> {
    if !self.admits_checkpoint() {
      return Err(LiveError::NoCredit {
        durable: self.durable,
      });
    }
    self.append_hashed_as(payload, hashed, true)
  }

  /// [`Shipper::append_kept`] with the identity `hashed` hashed over `payload` in turns (A-757 part 2b: a large delta built
  /// in turns of its own); refused as [`Shipper::append_checkpoint_hashed`] is, and for credit as a delta is.
  pub fn append_hashed(
    &mut self,
    payload: Vec<u8>,
    hashed: EntryHasher,
  ) -> Result<Head, LiveError> {
    self.admit(payload.len())?;
    self.append_hashed_as(payload, hashed, false)
  }

  /// Whether a delta of `payload_len` bytes would be admitted now ([`Shipper::append`]'s credit).
  pub fn admits_delta(&self, payload_len: usize) -> bool {
    self.admit(payload_len).is_ok()
  }

  /// The append both hashed appends share, a whole state when `whole`.
  fn append_hashed_as(
    &mut self,
    payload: Vec<u8>,
    hashed: EntryHasher,
    whole: bool,
  ) -> Result<Head, LiveError> {
    let (position, previous) = self.next_link()?;
    if hashed.position != position
      || hashed.previous != previous
      || hashed.epoch != self.epoch
      || hashed.length != payload.len()
      || hashed.fed != payload.len()
    {
      return Err(LiveError::Behind { head: self.head });
    }
    let head = Head {
      epoch: self.epoch,
      position,
      identity: *hashed.hasher.finalize().as_bytes(),
    };
    let entry = Entry {
      owner: self.owner,
      object: self.object,
      position,
      epoch: self.epoch,
      previous,
      payload,
    };
    Ok(self.push_entry(entry, head, whole, Kept::Retained).0)
  }

  /// Keeps `entry`, whose head is `head`, at the end of the chain, unsettled as a whole state when `whole`: its head,
  /// and the entry too when `kept` returns it.
  fn push_entry(
    &mut self,
    entry: Entry,
    head: Head,
    whole: bool,
    kept: Kept,
  ) -> (Head, Option<Entry>) {
    let position = entry.position;
    self.head = Some(head);
    let bytes = entry.wire_bytes();
    self.unsettled.push_back((position, bytes, whole));
    self.unsettled_bytes = self.unsettled_bytes.saturating_add(bytes);
    if whole {
      self.unsettled_whole_bytes = self.unsettled_whole_bytes.saturating_add(bytes);
    }
    let returned = match kept {
      Kept::Returned => Some(entry.clone()),
      Kept::Retained => None,
    };
    self.retained.push_back(entry);
    self.trim();
    self.refresh_durable();
    (head, returned)
  }

  /// Recomputes the durable position and settles the entries it now covers, giving their bytes back to the credit.
  fn refresh_durable(&mut self) {
    self.durable = self.recompute_durable();
    let Some(floor) = self.settled_through() else {
      return;
    };
    while let Some((position, bytes, whole)) = self.unsettled.front().copied() {
      if position > floor {
        break;
      }
      self.unsettled.pop_front();
      self.unsettled_bytes = self.unsettled_bytes.saturating_sub(bytes);
      if whole {
        self.unsettled_whole_bytes = self.unsettled_whole_bytes.saturating_sub(bytes);
      }
    }
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

  /// Folds a holder's answer as [`Shipper::on_reply`] does, building nothing to send: for a caller that sends from
  /// [`Shipper::pending`] when it sends. `Some(epoch)` when a later epoch has superseded this owner. Through
  /// `on_reply`, every answer of a holder far behind cloned each retained entry it lacked, for the daemon to drop: 1.4 ms
  /// a reply 1,000 entries behind, so the owner shard spent itself on one resumed holder's replies, writes fell to about
  /// 210 a second, and the holder never came within 50 entries of the head in 6 of 24 x86 runs
  /// (diag-live-durability-73).
  pub fn fold_reply(
    &mut self,
    holder: HostId,
    reply: Result<Option<Head>, LiveError>,
  ) -> Option<HostEpoch> {
    match self.fold(holder, reply) {
      Folded::Fenced(current) => Some(current),
      Folded::Nothing | Folded::After(_) | Folded::Install => None,
    }
  }

  /// Folds a holder's answer (to an entry or an install) and says what to send it next. A head on this chain is an
  /// acknowledgement of every position up to it; a head off the chain or behind the retained tail draws an install; a
  /// fence above this epoch means the owner has been superseded.
  pub fn on_reply(&mut self, holder: HostId, reply: Result<Option<Head>, LiveError>) -> Next {
    match self.fold(holder, reply) {
      Folded::Fenced(current) => Next::Fenced(current),
      Folded::Nothing => Next::Nothing,
      Folded::Install => Next::Install,
      Folded::After(position) => self.entries_after(position),
    }
  }

  /// The fold of [`Shipper::on_reply`] and [`Shipper::fold_reply`]: what the holder's answer leaves it lacking, named
  /// and not built.
  fn fold(&mut self, holder: HostId, reply: Result<Option<Head>, LiveError>) -> Folded {
    if !self.ships_to(holder) {
      return Folded::Nothing;
    }
    let held = match reply {
      Err(LiveError::StaleEpoch { current }) if current > self.epoch => {
        return Folded::Fenced(current);
      }
      Err(LiveError::Behind { head }) | Ok(head) => head,
      Err(_) => return Folded::Nothing,
    };
    if let Some(head) = held
      && head.epoch > self.epoch
    {
      return Folded::Fenced(head.epoch);
    }
    let Some(own) = self.head else {
      return Folded::Nothing;
    };
    let Some(head) = held else {
      return self.owe_after(holder, None);
    };
    if !self.on_chain(holder, &head) {
      self.owed_install.insert(holder);
      return Folded::Install;
    }
    self.owed_install.remove(&holder);
    let newer = self
      .held
      .get(&holder)
      .is_none_or(|known| known.position < head.position);
    if newer {
      self.held.insert(holder, head);
      self.trim();
      self.refresh_durable();
    }
    if head == own {
      Folded::Nothing
    } else {
      self.owe_after(holder, Some(head.position))
    }
  }

  /// What `holder` lacks after `position`, named: an install is noted owed to it, so [`Shipper::pending`] keeps answering
  /// it until the holder is back on the chain.
  fn owe_after(&mut self, holder: HostId, position: Option<u64>) -> Folded {
    match self.reaches(position) {
      None => Folded::Nothing,
      Some(false) => {
        self.owed_install.insert(holder);
        Folded::Install
      }
      Some(true) => Folded::After(position),
    }
  }

  /// Whether the retained tail reaches the entry after `position` (from position 0 when `None`): `None` when there is no
  /// position after it.
  fn reaches(&self, position: Option<u64>) -> Option<bool> {
    let first = match position {
      Some(position) => position.checked_add(1)?,
      None => 0,
    };
    Some(
      self
        .retained
        .front()
        .is_some_and(|oldest| oldest.position <= first),
    )
  }

  /// What `holder` lacks by the head it last reported (everything, when it has reported none; an install when its last
  /// answer was off the chain or behind the tail): the owner's period re-ships this, so a lost entry or reply is
  /// recovered without waiting on the next write. Until 2026-11-05 a holder off the chain was offered entries from its
  /// last on-chain head, which it refused, forever: a takeover's successor never caught the surviving candidate up and
  /// every write it took waited (`live_fleet`, a write stuck 21 minutes; the model, driven as the daemon drives it,
  /// failed Convergence).
  pub fn pending(&self, holder: HostId) -> Next {
    if self.head.is_none() || !self.ships_to(holder) {
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

  /// Records `head` as the last whole state made: the owner's whole state as of the entry at it (that entry itself when
  /// it is whole, or an image of the state it left, made beside the log), which an install sends to a holder off the
  /// chain or behind the retained tail. Ignored unless it is the owner's head now.
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

  /// Whether `head`, reported by `holder`, is on this owner's chain: its head, its start, its last checkpoint, a retained
  /// entry, or the head this holder last acknowledged, which the tail may have passed: an entry every holder holds is
  /// trimmed, so a holder repeating the head it last acknowledged (refusing an entry it is not ready for) was taken for
  /// one off the chain and answered with an install, though the entries after it were all retained: a restarted holder's
  /// catch-up took 42.8 s (`live_chaos`, diag-live-durability-62).
  fn on_chain(&self, holder: HostId, head: &Head) -> bool {
    if Some(*head) == self.head
      || Some(*head) == self.start
      || Some(*head) == self.checkpoint
      || self.held.get(&holder) == Some(head)
    {
      return true;
    }
    self
      .retained
      .iter()
      .find(|entry| entry.position == head.position)
      .is_some_and(|entry| entry.head() == *head)
  }

  /// Whether the retained tail still holds every entry after `position` (A-428): a holder installed from a whole state at
  /// `position` then catches up by those entries alone, so [`Shipper::pending`] answers it entries, never a second
  /// install. The owner owes an install image at its head only for a whole state the tail no longer reaches past. Asked
  /// without building the entries ([`Shipper::fold_reply`]'s sibling): through `entries_after` it cloned every entry a
  /// lagging holder lacked on each write's sends.
  pub fn retains_after(&self, position: u64) -> bool {
    self.reaches(Some(position)) != Some(false)
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

  /// Drops retained entries every host it ships to holds, then the oldest while the tail's bytes pass
  /// [`Bounds::retain`] (a laggard behind them is installed), never the newest for size alone.
  fn trim(&mut self) {
    let everyone = self
      .targets()
      .map(|holder| self.held.get(&holder).map(|head| head.position))
      .min()
      .flatten();
    let mut bytes = self
      .retained
      .iter()
      .fold(0usize, |sum, entry| sum.saturating_add(entry.wire_bytes()));
    while let Some(oldest) = self.retained.front() {
      let held_by_all = everyone.is_some_and(|position| oldest.position <= position);
      let over = self.retained.len() > 1 && bytes > self.bounds.retain;
      if !(held_by_all || over) {
        break;
      }
      bytes = bytes.saturating_sub(oldest.wire_bytes());
      self.retained.pop_front();
    }
  }

  /// Moves its bounds (the caller's derivation, [`Bounds::of_whole`] in the daemon: the size of the whole state an
  /// install would send, measured at each whole record and install image), and trims the retained tail to them. Kept to
  /// a count of entries, an owner writing faster than an install's round trip left a holder it had just installed
  /// behind the tail again, and installed it again without end (2026-11-06, `crates/cli/tests/live_chaos.rs`: 803
  /// installs, 571 MB, in one run); and a credit of two entries turned a third publication inside one round trip into
  /// a copy of the whole volume (2026-11-17).
  pub fn set_bounds(&mut self, bounds: Bounds) {
    self.bounds = bounds;
    self.trim();
  }

  /// The highest position the owner and enough holders hold, capped at the owner's head: over the holders, every `f + 1`
  /// of them a takeover can count meet it ([`Shipper::reached_by`]); during a transition, the lower of that over the
  /// holders and over the target, so a takeover counting either meets it. At `f = 0` the owner's head. It never falls:
  /// an older answer arriving late, or a set where too few hold anything, leaves it where it was.
  fn recompute_durable(&self) -> Option<u64> {
    let own = self.head?.position;
    if self.quorum.f == 0 {
      return Some(own);
    }
    let reached = match &self.transition {
      None => self.reached_by(&self.holders),
      Some(next) => self
        .reached_by(&self.holders)
        .zip(self.reached_by(next))
        .map(|(holders, next)| holders.min(next)),
    };
    match (reached.map(|reached| reached.min(own)), self.durable) {
      (Some(reached), Some(durable)) => Some(durable.max(reached)),
      (reached, durable) => reached.or(durable),
    }
  }

  /// The highest position held by `W = max(n − f, f)` of `set`'s `n` members: every `f + 1` of them a takeover's phase
  /// one can count meets one holding it (`W + f + 1 > n`), and the owner and `W` holders are `f + 1` copies, which
  /// survive `f` failures. `f` of `2f`; `f + 1` of `2f + 1` (a set without the owner, whose placement moved off it); all
  /// `f` of `f`; never for fewer than `f`. `None` too while fewer hold anything. At `f = 0`, the owner's head (R8, the
  /// same formula). Counting the `f`-th highest whatever the set's size let a write held by one of three holders count as
  /// durable, and a successor counted the other two (raft-chaos, CI diag-20 run 7); `n − f` alone answered a write on the
  /// owner and one holder at `f = 2` with three holders, two copies that `f` failures lose (raft's review).
  fn reached_by(&self, set: &[HostId]) -> Option<u64> {
    let tolerated = usize::try_from(self.quorum.f).ok()?;
    if tolerated == 0 {
      return self.head.map(|head| head.position);
    }
    if set.len() < tolerated {
      return None;
    }
    let needed = set.len().saturating_sub(tolerated).max(tolerated);
    let rank = needed.checked_sub(1)?;
    let mut positions: Vec<u64> = set
      .iter()
      .filter_map(|holder| self.held.get(holder).map(|head| head.position))
      .collect();
    positions.sort_unstable_by(|a, b| b.cmp(a));
    positions.get(rank).copied()
  }
}

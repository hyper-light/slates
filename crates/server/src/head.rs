//! A volume's head register value and the owner-side state that produces it (§4.8 mechanism 1,
//! "the acknowledging set is written into the object's head record, so a reader learns where the
//! copies are from the record it reads anyway"; §4.10 "Content replication"). The head register of a
//! volume is written by its owner at each sequence — sequence 0 at creation (the "epoch-one head
//! record", §4.4 create) and sequence `k` when the volume's `k`-th snapshot has its content placed —
//! and its value is what a successor adopts on a takeover and serves from.
//!
//! The value names the snapshot's **content** (the archive manifest's identity, D-17) and the
//! candidates that acknowledged holding it, so a reader — a takeover successor materializing the
//! volume, a remote attach — fetches the content by identity from a recorded holder. What a successor
//! serves the content *as* — the name, size class, name policy, owner, locked policy and access list — is
//! the volume's catalog register (`crate::catalog`), shipped on every catalog change; a head ships only once
//! its catalog's current version is placed, so an adopted head always has an adopted catalog. Until
//! 2026-09-30 those fields rode this value, which ships only when a seal places, and a later change was lost
//! at a takeover (AUD-29-17).
//!
//! The encoding is the daemon's canonical wire codec ([`slates_wire::Wire`]) — the same codec the
//! catalog records use — so the value is deterministic across hosts (the record's identity, and so
//! every acknowledgement's binding, depends on it), bounds-checked on the way in, and refused typed on
//! any malformation; a value with trailing bytes is refused too, so a record is exactly one value.

use slates_archive::Archive;
use slates_db::catalog::SnapshotId;
use slates_db::register::{HostEpoch, HostId, Placement};
use slates_vfs::export::SnapshotArchiver;
use slates_wire::Wire;

/// What a volume's head register holds at one sequence.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HeadValue {
  /// The archive manifest identity of the snapshot this head names, or none at sequence 0 (creation:
  /// nothing sealed yet) — the content a reader fetches by identity.
  pub manifest: Option<[u8; 32]>,
  /// The candidate holders that acknowledged holding the content whole (host ids), the "acknowledging
  /// set" a reader fetches from; empty when there is no content.
  pub content_holders: Vec<u64>,
  /// What a successor opens the volume's sealed content with (A-92 piece 4c): the lineage key wrapped under each pair
  /// key the owner's shard delivered; `None` with no content, or while sealing is unavailable. Appended.
  pub sealing: Option<HeadSealing>,
}

/// The volume's lineage key as a head carries it for successors (A-92 piece 4c): who wrapped it (the owner's stable
/// anchor and partition, which name the pair key a successor holds), the lineage key's id, and one entry per neighbour.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HeadSealing {
  /// The owner's stable anchor.
  pub owner_anchor: u64,
  /// The owner shard's partition.
  pub partition: u16,
  /// The lineage key's id.
  pub lineage: [u8; 16],
  /// The lineage key wrapped under each neighbour's pair key, in anchor order.
  pub keys: Vec<HeadKey>,
  /// The volume's naming key, as the owner recorded it (wrapped under the lineage key), so a successor verifies the
  /// names the owner made; `None` before the volume sealed a named chunk.
  pub naming: Option<HeadNaming>,
}

/// A volume's naming key as a head carries it: its id and its record wrapped under the volume's lineage key.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HeadNaming {
  /// The naming key's id.
  pub id: [u8; 16],
  /// The key wrapped under the lineage key (AES-KW, 61 bytes).
  pub wrapped: Vec<u8>,
}

/// One neighbour's copy of the lineage key: its stable anchor and the key wrapped under its pair key (AES-KW, 61 bytes).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct HeadKey {
  /// The neighbour's stable anchor.
  pub anchor: u64,
  /// The lineage key wrapped under the pair key.
  pub wrapped: Vec<u8>,
}

impl HeadValue {
  /// The value's canonical bytes, the head record's `value`.
  pub fn to_record_bytes(&self) -> Vec<u8> {
    crate::catalog::tagged(crate::catalog::HEAD_CLASS, self.to_bytes())
  }

  /// Parses a head record's value; `None` for another register class or for bytes that are not exactly
  /// one value (truncated, malformed, or with trailing bytes).
  pub fn from_record_bytes(bytes: &[u8]) -> Option<HeadValue> {
    let mut input = crate::catalog::untagged(crate::catalog::HEAD_CLASS, bytes)?;
    let value = <HeadValue as Wire>::decode(&mut input).ok()?;
    if input.is_empty() { Some(value) } else { None }
  }

  /// The recorded content holders as host ids.
  pub fn holders(&self) -> Vec<HostId> {
    self.content_holders.iter().copied().map(HostId).collect()
  }
}

/// The placement the record plane has recorded for a volume's head at one sequence: the head's
/// acknowledging candidates so far (merged across rounds), and the epoch the head was written under. A
/// newer sequence supersedes an older one — the head register's newest position is the head.
///
/// The epoch matters after a takeover: the successor promoted the object at an epoch above the dead
/// owner's, and every holder raised its fence for the object to it (§4.8 "every holder raises its fence
/// for that host to the new epoch"), so the successor's later writes to that object — its next seals — must
/// carry at least that epoch or be refused `StaleEpoch`. The record plane writes each head at the greater
/// of the configuration's host epoch and the epoch recorded here, so a taken-over object keeps writing
/// under its promotion epoch and a fresh object under the host's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedHead {
  /// The head sequence this placement is for.
  pub sequence: u64,
  /// The epoch the head at this sequence was written (or adopted) under.
  pub epoch: HostEpoch,
  /// The acknowledging candidates.
  pub placement: Placement,
}

/// A seal in progress for a volume this node owns (§4.10): the snapshot being archived in bounded
/// slices, then its archive being put to the content candidates until `f + 1` hold it, after which the
/// head record naming it is shipped and the snapshot recorded placed. One per object; a newer snapshot
/// supersedes the job for an older one (the newest seal is what the head must name).
pub struct SealJob {
  /// The snapshot being sealed.
  pub snapshot: SnapshotId,
  /// The head sequence the seal places for (the snapshot's epoch).
  pub sequence: u64,
  /// The walk in progress, until the archive is complete.
  pub archiver: Option<SnapshotArchiver>,
  /// The complete archive, kept until its content places (then dropped — the volume holds the bytes).
  pub archive: Option<Archive>,
  /// The manifest identity, once the archive is complete.
  pub manifest: Option<[u8; 32]>,
  /// The content's placement so far: the candidates and those that acknowledged holding it (the owner
  /// among them from the start — it holds its own content).
  pub content: Placement,
  /// Content rounds run so far: the first goes to `f + 1` candidates (the owner and `f` more), later
  /// ones hedge to the rest (§4.8 "hedged to the remaining candidates").
  pub rounds: u32,
  /// When the first content round was dispatched (the shard clock, nanoseconds), once it has been: the
  /// hedge round to the remaining candidates is held until this is older than the **measured** p95 put
  /// latency (§4.8 "hedged to the remaining candidates after the measured p95 put latency"), so a first
  /// round that is merely a little slow is not doubled up on, while one that has fallen into the tail is.
  /// `None` until the first round goes out.
  pub first_round_at_ns: Option<u64>,
  /// Whether this job is the **healer's** re-offer of an already-placed snapshot (§4.10 "anti-entropy …
  /// the healer") rather than a fresh seal: the snapshot's placement is already recorded, so the job is
  /// kept past the "already placed" drop, and a holder that answers the offer with chunks it lacked is a
  /// **repair**, counted. Its rounds are the ordinary content rounds — the same put, hedge and record path.
  pub healing: bool,
}

#[cfg(test)]
mod tests {
  use super::*;

  fn value() -> HeadValue {
    HeadValue {
      manifest: Some([7u8; 32]),
      content_holders: vec![3, 9],
      sealing: None,
    }
  }

  #[test]
  fn a_head_value_round_trips_and_is_deterministic() {
    let bytes = value().to_record_bytes();
    assert_eq!(
      bytes,
      value().to_record_bytes(),
      "the same value encodes to the same bytes"
    );
    assert_eq!(HeadValue::from_record_bytes(&bytes), Some(value()));
    assert_eq!(value().holders(), vec![HostId(3), HostId(9)]);
    let creation = HeadValue {
      manifest: None,
      content_holders: Vec::new(),
      sealing: None,
    };
    assert_eq!(
      HeadValue::from_record_bytes(&creation.to_record_bytes()),
      Some(creation)
    );
  }

  /// Hostile input (§4.9): every truncation and any trailing byte is refused, never a panic.
  #[test]
  fn truncated_and_padded_values_are_refused() {
    let bytes = value().to_record_bytes();
    for cut in 0..bytes.len() {
      assert!(
        HeadValue::from_record_bytes(&bytes[..cut]).is_none(),
        "cut to {cut} bytes"
      );
    }
    let mut padded = bytes.clone();
    padded.push(0);
    assert!(HeadValue::from_record_bytes(&padded).is_none());
  }
}

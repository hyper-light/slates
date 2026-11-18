//! A Shards stream's DAX objects held by the anchor (§4.6, the Shards form; A-365): what lets a stream whose guest maps
//! files with DAX outlive its daemon, as A-316's resync lets one that maps none.
//!
//! A guest's stores through a DAX map land in its file's object (`slates_mem::DaxObject`), which the device maps; they
//! reach the file only when slates ingests them, at the guest's syncs and the stream's end. A daemon's death leaves the
//! device mapping the object, its stores since the last sync in it, and a successor that knows nothing of it. So:
//! - **The anchor holds each backing's object** ([`hold`]), with its record ([`DaxRecord`]: the attachment and the
//!   inode), sent before its first descriptor goes to the device, and released when the backing ends ([`release`]). Its
//!   id is the backing's handle in the shard's registry, encoded with its partition, as A-197 names a writable copy, so
//!   no two held copies ever share one.
//! - **Each answered range is recorded** in the shard's range table in anchor RAM (`crate::dax_ranges`), so the
//!   successor knows which ranges of each object are the file's and which the guest may have stored into.
//! - **A successor takes the objects back while recovery rebuilds the shard** ([`adopt_inherited`]), for each Shards
//!   stream recovery kept: each object kept again in the store at the very handle its backing had (beside A-197's
//!   copies, in one ascending order), its ranges filled and writable as the table says, and dormant: no write reaches it
//!   until its stream takes it. Its object already holds every write the dead daemon made to its ranges, in order with
//!   its guest's stores, so the write log's replay must not write a logged write over a later store (measured: it did,
//!   and the store never reached the file). The stream's adoption then takes them into its session
//!   (`crate::shards::adopt_one`), wakes them, ingests their writable ranges before it serves anything, and answers the
//!   device's asks with the same objects, so the guest's maps stay right. A backing no kept stream claims is let go:
//!   its ranges forgotten, its object released.
//!
//! A hold or a record the anchor or the content object refuses gives the stream's resync up, as before A-365: its
//! device learns at once that its share ends with this daemon. Unix: Windows' streams hold no resync yet.

use std::os::fd::{AsFd, OwnedFd};

use slates_bridge_core::dax_backing::{filled_charge, most_views};
use slates_mem::{DaxLayout, DaxObject, Encoded, Handle};
use slates_vfs::ids::InodeNo;
use slates_vfs::shards_backing::ShardsBacking;

use super::ShardsStream;
use crate::anchor_hold::HandedCopy;
use crate::state::ShardState;

/// Format: the tag a held DAX object's record opens with, "DAXHELD" and version 1. A writable direct map's record
/// (A-197) is two words long and opens with no tag.
const TAG: u64 = u64::from_le_bytes(*b"DAXHELD\x01");
/// Format: a held DAX object's record: the tag, the attachment and the inode, each a little-endian `u64`.
const RECORD_BYTES: usize = 3 * size_of::<u64>();

/// Counter: a DAX object the anchor refused to hold, or whose ranges the range table refused: its stream gives its
/// resync up, so its device learns at once that its share ends with this daemon.
const HOLD_REFUSED: &str = "shards.dax_hold_refused";
/// Counter: a DAX object a successor took back from its anchor (A-365), the adoption's non-vacuity counter.
const ADOPTED: &str = "shards.dax_adopted";
/// Counter: a held DAX object a successor could not take back (its stream not kept, its volume, its file, its ranges or
/// the object refused), let go: what its guest stored since its last sync reaches nothing.
const ADOPT_REFUSED: &str = "shards.dax_adopt_refused";

/// A held DAX object's record: the attachment whose backing it is, and the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DaxRecord {
  /// The attachment, by its durable id.
  pub(crate) attachment: u64,
  /// The file, its inode number in the attachment's volume.
  pub(crate) inode: u64,
}

impl DaxRecord {
  /// Its bytes ([`RECORD_BYTES`]).
  pub(crate) fn encode(self) -> [u8; RECORD_BYTES] {
    let mut bytes = [0u8; RECORD_BYTES];
    let words = [TAG, self.attachment, self.inode].map(u64::to_le_bytes);
    for (slot, byte) in bytes.iter_mut().zip(words.iter().flatten()) {
      *slot = *byte;
    }
    bytes
  }

  /// The record `bytes` hold, or none for bytes of another length or tag (another kind's record, or damage).
  pub(crate) fn decode(bytes: &[u8]) -> Option<DaxRecord> {
    let whole = <&[u8; RECORD_BYTES]>::try_from(bytes).ok()?;
    let (words, _) = whole.as_chunks::<{ size_of::<u64>() }>();
    let [tag, attachment, inode] = <[[u8; 8]; 3]>::try_from(words)
      .ok()?
      .map(u64::from_le_bytes);
    (tag == TAG).then_some(DaxRecord { attachment, inode })
  }
}

/// A held DAX object the anchor handed this shard back.
#[derive(Debug)]
pub(crate) struct InheritedDax {
  /// Its id: the dead daemon's backing's handle, encoded with its partition.
  pub(crate) copy: u64,
  /// Its record.
  pub(crate) record: DaxRecord,
  /// The object.
  pub(crate) object: OwnedFd,
}

/// Splits the copies the anchor handed back: each DAX object (A-365) to the partition its attachment names; every other
/// copy returned for its own kind (A-197's writable copies, `crate::held_copy::split`, Linux). A DAX object whose
/// partition this daemon has not is let go at once.
pub(crate) fn split(
  handed: Vec<HandedCopy>,
  partitions: usize,
) -> (Vec<HandedCopy>, Vec<Vec<InheritedDax>>) {
  let mut others = Vec::new();
  let mut daxes: Vec<Vec<InheritedDax>> = (0..partitions).map(|_| Vec::new()).collect();
  for handed in handed {
    let Some(record) = DaxRecord::decode(&handed.record) else {
      others.push(handed);
      continue;
    };
    let partition = usize::from(crate::verbs::owner_of_attachment(record.attachment));
    match daxes.get_mut(partition) {
      Some(list) => list.push(InheritedDax {
        copy: handed.copy,
        record,
        object: handed.object,
      }),
      None => release(handed.copy),
    }
  }
  (others, daxes)
}

/// Asks the anchor to hold `stream`'s object of `inode`, its backing `backing`, before its first descriptor goes to the
/// device: its copy id, or none when no anchor holds it (the hold refused, counted, or no anchor at all).
fn hold(
  s: &mut ShardState,
  stream: &ShardsStream,
  inode: u64,
  backing: Handle<ShardsBacking>,
) -> Option<u64> {
  let copy = backing.encode(s.partition).map(Encoded::word)?;
  let record = DaxRecord {
    attachment: stream.attachment,
    inode,
  };
  let object = s.store.shards_backings.get(backing)?.object().descriptor();
  match crate::anchor_hold::hold_copy(copy, &record.encode(), object.as_fd()) {
    Some(Ok(())) => Some(copy),
    Some(Err(refusal)) => {
      slates_machine::log_line!(
        "slates-server: the anchor did not hold DAX object {copy:x}: {refusal}"
      );
      s.count(HOLD_REFUSED, 1);
      None
    }
    None => None,
  }
}

/// Tells the anchor DAX object `copy` was let go, so it closes its copy. A refusal leaves nothing held (the anchor's end
/// is gone, and every copy with it), so it is not retried.
fn release(copy: u64) {
  let _sent = crate::anchor_hold::release_copy(copy);
}

/// Keeps what a successor needs of `stream`'s backing of `inode`, just answered: the anchor holds its object (once), and
/// the range table records every range it has answered, writable or not. Whether the stream may keep its resync: false
/// when either is refused, and then nothing of the backing stays held or recorded.
pub(super) fn keep(s: &mut ShardState, stream: &mut ShardsStream, inode: u64) -> bool {
  let Some(backing) = stream.dax.backing_of(inode) else {
    return false;
  };
  if !stream.held_dax.contains_key(&inode) {
    let Some(copy) = hold(s, stream, inode, backing) else {
      return false;
    };
    stream.held_dax.insert(inode, copy);
  }
  let ranges = s.store.shards_backings.get(backing).map(|held| {
    (
      held.filled().iter().copied().collect::<Vec<u64>>(),
      held.writable().clone(),
    )
  });
  let recorded = match (ranges, s.dax_ranges.as_mut(), s.content.as_mut()) {
    (Some((filled, writable)), Some(table), Some(object)) => filled.iter().all(|range| {
      table
        .record(
          object,
          (stream.attachment, inode),
          *range,
          writable.contains(range),
        )
        .is_ok()
    }),
    _ => false,
  };
  if !recorded {
    s.count(HOLD_REFUSED, 1);
    let_go(s, stream, inode);
  }
  recorded
}

/// Lets go of what is kept of `stream`'s backing of `inode` (it ended: the guest forgot the node, or the resync was given
/// up): its object released by the anchor, its ranges forgotten.
pub(super) fn let_go(s: &mut ShardState, stream: &mut ShardsStream, inode: u64) {
  if let Some(copy) = stream.held_dax.remove(&inode) {
    release(copy);
  }
  if let (Some(table), Some(object)) = (s.dax_ranges.as_mut(), s.content.as_mut()) {
    let _forgotten = table.forget(object, (stream.attachment, inode));
  }
}

/// Lets go of what is kept of every backing of `stream` ([`let_go`] for each): its end, or its resync given up.
pub(super) fn let_go_all(s: &mut ShardState, stream: &mut ShardsStream) {
  for (_, copy) in std::mem::take(&mut stream.held_dax) {
    release(copy);
  }
  if let (Some(table), Some(object)) = (s.dax_ranges.as_mut(), s.content.as_mut()) {
    let _forgotten = table.forget_attachment(object, stream.attachment);
  }
}

/// A held DAX object kept again in the store for a kept stream, until its stream's adoption takes it
/// (`crate::shards::adopt_one`).
#[derive(Debug)]
pub(crate) struct AdoptedDax {
  /// The file.
  pub(crate) inode: u64,
  /// Its backing in the store.
  pub(crate) backing: Handle<ShardsBacking>,
  /// The anchor's copy id of its object.
  pub(crate) copy: u64,
}

/// Takes back one DAX object the anchor handed this shard while recovery rebuilds it ([`adopt_inherited`]): kept again
/// in the store at its backing's handle, its ranges as the table says, for the stream recovery kept. Whether it was.
fn adopt_one(state: &mut ShardState, dax: InheritedDax) -> bool {
  let InheritedDax {
    copy,
    record,
    object,
  } = dax;
  let kept = state
    .adopt_shards
    .iter()
    .any(|(kept, _)| kept.id == record.attachment);
  let Some(found) = state.db.partition().attachment(record.attachment) else {
    return false;
  };
  let Some(&handle) = state.by_id.get(&found.volume).filter(|_| kept) else {
    return false;
  };
  let Ok(slot) = state.volumes.get(handle) else {
    return false;
  };
  let inode = InodeNo(record.inode);
  if slot.volume.stat(&state.store, inode).is_err() {
    return false;
  }
  let prefix = slot.volume.prefix();
  let ranges = state
    .dax_ranges
    .as_ref()
    .map(|table| table.ranges_of((record.attachment, record.inode)))
    .unwrap_or_default();
  if ranges.filled.is_empty() {
    return false;
  }
  let Ok(object) = DaxObject::adopt(
    object,
    most_views(&state.store.shards_backings),
    DaxLayout::Identity,
  ) else {
    return false;
  };
  let charge = filled_charge(DaxLayout::Identity, ranges.filled.iter().copied());
  let encoded = Encoded::from_word(copy);
  let Ok(backing) = state.store.shards_backings.adopt_vmm(
    object,
    (prefix, inode),
    (encoded.slot(), encoded.generation()),
    (ranges.filled, ranges.writable),
    charge,
  ) else {
    return false;
  };
  state
    .adopted_dax
    .entry(record.attachment)
    .or_default()
    .push(AdoptedDax {
      inode: record.inode,
      backing,
      copy,
    });
  true
}

/// One held backing of either kind, for the one ascending order of handles the store's registry takes them back in.
enum Held {
  /// A Shards stream's DAX object (A-365).
  Dax(InheritedDax),
  /// A writable direct map's copy (A-197).
  #[cfg(target_os = "linux")]
  Map(crate::held_copy::InheritedCopy),
}

/// Takes back every object the anchor handed this shard, while recovery rebuilds it and before the write log's replay,
/// each dormant through the replay, since it already holds every write the dead daemon made, in order with the stores
/// made through it: A-197's writable copies (Linux), woken once the replay is done (`crate::held_copy::wake_adopted`,
/// A-389), and the Shards streams' DAX objects, woken when their streams take them ([`claim`]); together in ascending
/// order of the handles their backings had (the registry's `insert_at` contract). One that cannot be taken back is let go and counted; then
/// every range the table names for a backing not taken back is forgotten. How many were taken back.
pub(crate) fn adopt_inherited(state: &mut ShardState) -> usize {
  let mut order: Vec<(u32, Held)> = std::mem::take(&mut state.inherited_dax)
    .into_iter()
    .map(|dax| (Encoded::from_word(dax.copy).slot(), Held::Dax(dax)))
    .collect();
  #[cfg(target_os = "linux")]
  let deadline = crate::held_copy::claim_deadline(state);
  #[cfg(target_os = "linux")]
  order.extend(
    std::mem::take(&mut state.inherited_copies)
      .into_iter()
      .map(|map| (Encoded::from_word(map.copy).slot(), Held::Map(map))),
  );
  order.sort_by_key(|(slot, _)| *slot);
  let mut adopted: usize = 0;
  for (_, held) in order {
    let taken = match held {
      Held::Dax(dax) => {
        let (copy, record) = (dax.copy, dax.record);
        let taken = adopt_one(state, dax);
        state.count(if taken { ADOPTED } else { ADOPT_REFUSED }, 1);
        if !taken {
          release(copy);
          if let (Some(table), Some(object)) = (state.dax_ranges.as_mut(), state.content.as_mut()) {
            let _forgotten = table.forget(object, (record.attachment, record.inode));
          }
        }
        taken
      }
      #[cfg(target_os = "linux")]
      Held::Map(map) => crate::held_copy::adopt_or_release(state, map, deadline),
    };
    if taken {
      adopted = adopted.saturating_add(1);
    }
  }
  forget_unadopted(state);
  adopted
}

/// Forgets every range the table names for a backing recovery did not take back: no object of it is held, so nothing
/// will ask for it.
fn forget_unadopted(state: &mut ShardState) {
  let taken: std::collections::BTreeSet<(u64, u64)> = state
    .adopted_dax
    .iter()
    .flat_map(|(attachment, list)| list.iter().map(move |dax| (*attachment, dax.inode)))
    .collect();
  if let (Some(table), Some(object)) = (state.dax_ranges.as_mut(), state.content.as_mut()) {
    for backing in table.backings() {
      if !taken.contains(&backing) {
        let _forgotten = table.forget(object, backing);
      }
    }
  }
}

/// Takes the DAX objects recovery kept for `stream`'s attachment into its session, and starts the ingest of their
/// writable ranges, which runs before the stream serves anything: the guest's stores since its last sync reach the file.
/// Whether an ingest was started.
pub(super) fn claim(s: &mut ShardState, stream: &mut ShardsStream) -> bool {
  let Some(kept) = s.adopted_dax.remove(&stream.attachment) else {
    return false;
  };
  for dax in kept {
    if stream.dax.adopt(dax.inode, dax.backing) {
      // Woken now, after the write log's replay: every change of its file reaches it again from here on.
      s.store.shards_backings.wake(dax.backing);
      stream.held_dax.insert(dax.inode, dax.copy);
    } else {
      let_go_unclaimed(s, stream.attachment, dax);
    }
  }
  stream.dax.begin_ingest_all(
    &s.store.shards_backings,
    slates_bridge_core::dax_backing::Then::Resume,
  )
}

/// Lets go of a DAX object recovery kept for `attachment` that no stream took: its backing removed from the store, its
/// charge given back, its ranges forgotten, its object released, counted.
fn let_go_unclaimed(s: &mut ShardState, attachment: u64, dax: AdoptedDax) {
  if let Some(gone) = s.store.shards_backings.remove(dax.backing) {
    s.store.shards_backings.uncharge(filled_charge(
      DaxLayout::Identity,
      gone.filled().iter().copied(),
    ));
  }
  release(dax.copy);
  if let (Some(table), Some(object)) = (s.dax_ranges.as_mut(), s.content.as_mut()) {
    let _forgotten = table.forget(object, (attachment, dax.inode));
  }
  s.count(ADOPT_REFUSED, 1);
}

/// Lets go of every DAX object recovery kept that no stream's adoption took (its stream refused, or gone): run after
/// every kept stream was adopted.
pub(crate) fn let_go_every_unclaimed(s: &mut ShardState) {
  for (attachment, kept) in std::mem::take(&mut s.adopted_dax) {
    for dax in kept {
      let_go_unclaimed(s, attachment, dax);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-365. Do: encode a record and decode it; decode one byte short, one long, a record of A-197's length, and one
  /// whose tag is another's. Expect the same record back, and none for the rest.
  #[test]
  fn a_dax_record_round_trips_and_refuses_another_kinds() {
    let record = DaxRecord {
      attachment: 0x0102_0304_0506_0708,
      inode: 0xA0B0_C0D0_E0F0_0011,
    };
    let bytes = record.encode();
    assert_eq!(DaxRecord::decode(&bytes), Some(record));
    assert_eq!(DaxRecord::decode(&bytes[..RECORD_BYTES - 1]), None);
    let mut long = bytes.to_vec();
    long.push(0);
    assert_eq!(DaxRecord::decode(&long), None);
    assert_eq!(DaxRecord::decode(&bytes[..2 * size_of::<u64>()]), None);
    let mut foreign = bytes;
    foreign[0] ^= 1;
    assert_eq!(DaxRecord::decode(&foreign), None);
  }
}

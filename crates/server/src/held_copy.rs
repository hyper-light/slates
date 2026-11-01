//! A writable direct map's copy that outlives its daemon (§4.6, A-186, A-197): what lets a pmem VMM's guest, mapping a
//! copy `slates pmem` handed it, go on through a daemon takeover with nothing lost that its launcher has synced and
//! nothing missed that another path writes.
//!
//! The copy is a sealed memory object the daemon made (`slates_mem::PeerObject`); its client and the VMM hold it, so it
//! outlives the daemon's process whatever this module does. What would not outlive it is the daemon's knowledge of it:
//! the map's record, the write-through that keeps it equal to the file, and the attachment its stores are written back
//! through, which recovery drops for every SDK client ("the SDK's client attaches again"). So:
//! - **The anchor holds each writable copy** with its record ([`CopyRecord`]: the file's inode and the writer
//!   attachment), sent before the copy is handed to its client ([`hold`]), so no client holds a copy the anchor was not
//!   given, and released when the copy is let go ([`release`]). A refused hold is counted and the map works on, ending
//!   with its daemon as before A-197.
//! - **The successor takes each back while recovery rebuilds the shard** ([`adopt_inherited`]): its writer attachment
//!   is kept ([`keeps`]), as a FUSE mount whose device the anchor held is (A-61); the copy is adopted at the exact slot
//!   and generation its backing had, so the map id its client holds names it again; and its map is held under its client
//!   once more. Adoption runs before the write log's replay (A-63), so every write the replay applies is written into
//!   the copy by the volume's write boundary, as every change is: whatever recovery changes beneath the copy reaches it.
//! - **Its holder claims it** by naming it in a sync or a release ([`claimed`]): a client reconnects under its own id
//!   (`slates_client::Client::reconnect`), and `slates pmem` syncs as soon as it sees its daemon go. A map no holder
//!   claims within the client's reconnect budget, counted from the shard's start, is written back and let go with its
//!   attachment ([`reclaim_unclaimed`]), as a dead client's maps are reaped: its holder died with the daemon, and nothing
//!   it held may stay held for good.
//!
//! **What it does not hold.** A change neither published nor logged when the daemon died, which the volume's write
//! boundary had written into the copy, is in the copy and not in the successor's file: the next sync writes it back as
//! the guest's own (it was in the guest's map, so the file then shows what the guest saw). Linux alone: a macOS copy
//! keeps a second descriptor the anchor does not hold, and Windows' anchor holds no descriptors.

use std::os::fd::{BorrowedFd, OwnedFd};

use slates_db::catalog::Consumer;
use slates_ipc::protocol::VolumeId;
use slates_machine::derived;
use slates_mem::{Encoded, Handle, PeerObject};
use slates_vfs::clock::Clock;
use slates_vfs::ids::InodeNo;
use slates_vfs::shards_backing::ShardsBacking;

use crate::anchor_hold::HandedCopy;
use crate::state::ShardState;

/// Format: a held copy's record, what a successor needs beside the copy and its id (the map id its client holds): the
/// inode number of the file it copies and the write attachment its stores are written back under, each little-endian.
/// The volume, the client and its principal come from the attachment's record, the length from the copy itself.
pub(crate) const RECORD_BYTES: usize = 16;

/// Counter: a writable copy the anchor refused to hold (its bound, or its end gone): that map ends with its daemon.
const HOLD_REFUSED: &str = "dax.local_map_hold_refused";
/// Counter: a writable copy a successor took back from its anchor (A-197), the takeover's non-vacuity counter.
const ADOPTED: &str = "dax.local_map_adopted";
/// Counter: a copy the anchor handed back that could not be taken back (its record, its attachment, its volume, its
/// file or the object refused), let go: its stores since its last sync reach nothing.
const ADOPT_REFUSED: &str = "dax.local_map_adopt_refused";
/// Counter: a taken-back map no holder claimed within the reconnect budget, written back and let go.
const RECLAIMED: &str = "dax.local_map_unclaimed_reclaimed";

/// A held copy's record ([`RECORD_BYTES`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CopyRecord {
  /// The inode number of the file the copy copies.
  pub(crate) inode: u64,
  /// The write attachment its stores are written back under.
  pub(crate) attachment: u64,
}

impl CopyRecord {
  /// Its bytes.
  pub(crate) fn encode(self) -> [u8; RECORD_BYTES] {
    let mut bytes = [0u8; RECORD_BYTES];
    let (inode, attachment) = bytes.split_at_mut(RECORD_BYTES / 2);
    inode.copy_from_slice(&self.inode.to_le_bytes());
    attachment.copy_from_slice(&self.attachment.to_le_bytes());
    bytes
  }

  /// The record `bytes` spell: exactly [`RECORD_BYTES`] of them, else none (the anchor's bytes are external input).
  pub(crate) fn decode(bytes: &[u8]) -> Option<CopyRecord> {
    let bytes: [u8; RECORD_BYTES] = bytes.try_into().ok()?;
    let (inode, attachment) = bytes.split_at(RECORD_BYTES / 2);
    Some(CopyRecord {
      inode: u64::from_le_bytes(inode.try_into().ok()?),
      attachment: u64::from_le_bytes(attachment.try_into().ok()?),
    })
  }
}

/// A copy the anchor handed this shard back, its record decoded.
#[derive(Debug)]
pub(crate) struct InheritedCopy {
  /// The map id its client holds.
  pub(crate) copy: u64,
  /// The object.
  pub(crate) object: OwnedFd,
  /// Its record.
  pub(crate) record: CopyRecord,
}

/// Splits the copies the anchor handed back by the partition their id names (the shard field of the map id). One whose
/// record does not decode, or whose partition this daemon has not, is let go at once: the anchor closes its copy, and
/// its stores since its last sync reach nothing, since no attachment to write them through is known.
pub(crate) fn split(handed: Vec<HandedCopy>, partitions: usize) -> Vec<Vec<InheritedCopy>> {
  let mut split: Vec<Vec<InheritedCopy>> = (0..partitions).map(|_| Vec::new()).collect();
  for handed in handed {
    let partition = usize::from(Encoded::from_word(handed.copy).shard());
    match (CopyRecord::decode(&handed.record), split.get_mut(partition)) {
      (Some(record), Some(list)) => list.push(InheritedCopy {
        copy: handed.copy,
        object: handed.object,
        record,
      }),
      _ => release(handed.copy),
    }
  }
  split
}

/// Asks the anchor to hold `object`, a writable map's copy, as `copy` with `record`, before its client is handed it. A
/// refusal is counted on `state` and the map works on, ending with its daemon as before A-197; without an anchor (a
/// test harness, a standalone start) nothing is held or counted.
pub(crate) fn hold(state: &mut ShardState, copy: u64, record: CopyRecord, object: BorrowedFd<'_>) {
  if let Some(Err(refusal)) = crate::anchor_hold::hold_copy(copy, &record.encode(), object) {
    eprintln!("slates-server: the anchor did not hold writable copy {copy:x}: {refusal}");
    state.count(HOLD_REFUSED, 1);
  }
}

/// Tells the anchor `copy` was let go, so it closes its copy of it. A refusal leaves nothing held (the anchor's end is
/// gone, and every copy with it), so it is not retried.
pub(crate) fn release(copy: u64) {
  let _sent = crate::anchor_hold::release_copy(copy);
}

/// Whether recovery keeps `attachment`: it writes back one of the copies this shard inherited (`inherited`, the shard's
/// `inherited_copies`), which it must outlive the restart to do, as a FUSE mount whose device the anchor held does
/// (A-61).
pub(crate) fn keeps(inherited: &[InheritedCopy], attachment: u64) -> bool {
  inherited
    .iter()
    .any(|copy| copy.record.attachment == attachment)
}

/// Takes back every copy the anchor handed this shard (A-197), while recovery rebuilds it and before the write log's
/// replay: each copy adopted at the slot and generation its backing had (in ascending slot order, as the backings'
/// registry takes them), whole, its map held under its client with its writer, its claim owed by the reconnect budget.
/// One that cannot be taken back is let go and counted. How many were taken back.
pub(crate) fn adopt_inherited(state: &mut ShardState) -> usize {
  let mut inherited = std::mem::take(&mut state.inherited_copies);
  inherited.sort_by_key(|copy| Encoded::from_word(copy.copy).slot());
  let deadline = state
    .clock
    .monotonic_ns()
    .saturating_add(reconnect_budget_ns());
  let mut adopted: usize = 0;
  for copy in inherited {
    let id = copy.copy;
    if adopt_one(state, copy, deadline) {
      adopted = adopted.saturating_add(1);
      state.count(ADOPTED, 1);
    } else {
      release(id);
      state.count(ADOPT_REFUSED, 1);
    }
  }
  adopted
}

/// Takes back one copy ([`adopt_inherited`]): whether it was.
fn adopt_one(state: &mut ShardState, copy: InheritedCopy, deadline: u64) -> bool {
  let InheritedCopy {
    copy,
    object,
    record,
  } = copy;
  let Some(found) = state.db.partition().attachment(record.attachment) else {
    return false;
  };
  let Consumer::Sdk { client } = found.consumer else {
    return false;
  };
  let (volume, principal) = (found.volume, found.principal.clone());
  let Some(&handle) = state.by_id.get(&volume) else {
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
  let Ok(object) = PeerObject::adopt(object) else {
    return false;
  };
  let encoded = Encoded::from_word(copy);
  let Ok(backing) = state.store.shards_backings.adopt_peer(
    object,
    (prefix, inode),
    (encoded.slot(), encoded.generation()),
  ) else {
    return false;
  };
  state
    .direct_map
    .held
    .entry(client)
    .or_insert_with(|| crate::direct_map::ClientMaps {
      principal,
      maps: Vec::new(),
    })
    .maps
    .push(crate::direct_map::HeldMap {
      backing,
      writer: Some((
        VolumeId {
          bytes: volume.bytes,
        },
        record.attachment,
      )),
    });
  state.direct_map.unclaimed.insert(copy, (client, deadline));
  true
}

/// The client's reconnect budget (`slates_client::Deadlines::reconnect_ns`, derived the same way): how long a client
/// of the dead daemon may take to reach its successor, past which its taken-back maps are reclaimed.
fn reconnect_budget_ns() -> u64 {
  derived!(
    slates_db::replay::RECOVERY_BUDGET_NS.saturating_add(crate::daemon::LIVENESS_BUDGET_NS),
    "recovery_budget_ns + liveness_budget_ns (the client's reconnect budget)",
    ["RECOVERY_BUDGET_NS", "LIVENESS_BUDGET_NS"]
  )
  .get()
}

/// Marks the map `mapping_id` names claimed: its holder named it in a sync or a release since this shard took it back.
pub(crate) fn claimed(state: &mut ShardState, mapping_id: u64) {
  state.direct_map.unclaimed.remove(&mapping_id);
}

/// Reclaims, on this shard, every taken-back map whose claim is past its deadline: each of its holder's writable maps
/// here written back (`crate::direct_map::write_back_held`), its attachments here removed, and its copies let go, as a
/// dead client's are reaped (`crate::reap`). How many holders were reclaimed.
pub(crate) async fn reclaim_unclaimed() -> usize {
  let expired: Vec<u32> = crate::state::with_state(|s| {
    let now = s.clock.monotonic_ns();
    let mut clients: Vec<u32> = s
      .direct_map
      .unclaimed
      .values()
      .filter(|(_, deadline)| now >= *deadline)
      .map(|(client, _)| *client)
      .collect();
    clients.sort_unstable();
    clients.dedup();
    clients
  })
  .unwrap_or_default();
  let mut reclaimed: usize = 0;
  for client in expired {
    crate::direct_map::write_back_held(client).await;
    let done = crate::state::with_state_counted(|s| {
      let attachments = s.db.partition().attachments_of_client(client);
      if let Err(refusal) = crate::reap::remove_batch(s, client, &attachments) {
        eprintln!(
          "slates-server: reclaiming client {client}'s attachments was refused: {refusal:?}"
        );
      }
      crate::direct_map::let_go_held(s, client);
      s.direct_map
        .unclaimed
        .retain(|_, (holder, _)| *holder != client);
      s.count(RECLAIMED, 1);
    });
    if done.is_some() {
      reclaimed = reclaimed.saturating_add(1);
    }
  }
  reclaimed
}

/// The map id of `backing` on `state`'s partition, for the anchor's hold and release.
pub(crate) fn copy_of(state: &ShardState, backing: Handle<ShardsBacking>) -> Option<u64> {
  backing
    .encode(state.partition)
    .map(slates_mem::Encoded::word)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-197: a record round-trips its bytes; a record of any other length is refused (external input). Do: encode a
  /// record and decode it; decode one byte short, one long and none. Expect: the same record back; none for the rest.
  #[test]
  fn a_copy_record_round_trips_and_refuses_any_other_length() {
    let record = CopyRecord {
      inode: 0x0102_0304_0506_0708,
      attachment: 0xA0B0_C0D0_E0F0_0011,
    };
    assert_eq!(CopyRecord::decode(&record.encode()), Some(record));
    let bytes = record.encode();
    assert_eq!(CopyRecord::decode(&bytes[..RECORD_BYTES - 1]), None);
    let mut long = bytes.to_vec();
    long.push(0);
    assert_eq!(CopyRecord::decode(&long), None);
    assert_eq!(CopyRecord::decode(&[]), None);
  }
}

//! The message placer's DAX windows over per-file backings (§4.6; the backings of A-163, shared through
//! `slates_bridge_core::dax_backing`; goal G-vhost). An out-of-process VMM (crosvm) maps what the device sends into the
//! guest's DAX window and keeps it there, and nothing afterwards can change what a window maps without the guest seeing
//! wrong bytes:
//! - a Linux guest records each 2 MiB range it mapped and serves every access from it without asking again
//!   (`fs/fuse/dax.c`);
//! - the vhost-user protocol forbids a map over a live one;
//! - crosvm's unmap leaves read-only zero pages, which a real guest read as the file's bytes
//!   (`docs/bugs/2026-10-24-slates-own-dax-window-shows-stale-bytes-then-zeros-after-another-paths-write.md`).
//!
//! So a window maps the file's backing: one object per attachment and file, which never moves and which the volume's
//! single write boundary keeps equal to the file before any change returns. Nothing the daemon does to the file reaches
//! the window as a message, and no window is ever torn down under a guest that still holds it.
//!
//! What the device keeps here, and drives a step a turn (`Device::dax_turn`):
//! - **The windows**: each a range of the guest's window over a range of one file's backing, recorded when the front end
//!   acknowledges its map and removed at the guest's REMOVEMAPPING.
//! - **The session**: the backings, their fills, the ingest of the guest's stores and the refill of a backing the disk
//!   changed beneath.
//! - **What it holds of the guest's requests**, one at a time, since the device serves nothing past a held request:
//!   - a SETUPMAPPING whose backing is being filled, then whose map awaits the front end's acknowledgement;
//!   - a request that asks for the guest's stores to reach the volume first ([`ingest_asked`]), served once its ingest is
//!     done, with the write-back's refusal as its answer where its asker reads one;
//!   - a reply that could reveal a change the disk made beneath a backing, published once the backing is refilled.
//!
//! **Writable at the front end.** Every window is mapped writable when the attachment may write, whatever the guest
//! asked. A guest upgrades a range from read-only to writable with a second SETUPMAPPING over the same window
//! (`fuse_upgrade_dax_mapping`); since no map may go over a live one, the upgrade is answered with no message. The
//! guest's own page tables keep its read-only mappings read-only, and an ingest compares only the ranges asked writable.
#![cfg(target_os = "linux")]

use slates_bridge_core::dax_backing::{Counts, DaxBackingSession};
use slates_bridge_fuse::abi::Opcode;
use slates_bridge_fuse::bridge::{KernelChange, kernel_changes};
use slates_bridge_fuse::request::InHeader;
use slates_mem::DAX_OBJECT_BYTES;

use crate::virtqueue::DescriptorChain;

/// A live DAX window over a file's backing: where it lies in the guest's window, and what it maps there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackedWindow {
  /// The offset in the guest's DAX window.
  pub(crate) moffset: u64,
  /// The length mapped.
  pub(crate) len: u64,
  /// The file, as the volume numbers it (the guest's node resolved).
  pub(crate) inode: u64,
  /// The file offset the window starts at: its offset in the backing's object too, which holds the file at the file's
  /// own offsets.
  pub(crate) foffset: u64,
  /// Whether the front end maps it writable: whether the attachment may write.
  pub(crate) writable: bool,
}

impl BackedWindow {
  /// Whether it overlaps the guest's window range `[moffset, moffset + len)`.
  pub(crate) fn overlaps(&self, moffset: u64, len: u64) -> bool {
    self.moffset < moffset.saturating_add(len) && moffset < self.moffset.saturating_add(self.len)
  }
}

/// A SETUPMAPPING the device holds: its chain, and the window it asks for.
#[derive(Debug)]
pub(crate) struct HeldMap {
  /// The queue the chain came from.
  pub(crate) queue: u16,
  /// The chain, its reply unscattered.
  pub(crate) chain: DescriptorChain,
  /// The request's `unique`, which its reply carries.
  pub(crate) unique: u64,
  /// The window it asks for.
  pub(crate) window: BackedWindow,
  /// Whether the window is sent to the front end once the fill is done: not when the front end maps it as asked
  /// already (a repeat, or a writable upgrade of a window mapped writable).
  pub(crate) sends: bool,
  /// Whether the map is sent and awaits the front end's acknowledgement.
  pub(crate) sent: bool,
}

/// A request held, unserved, for the ingest of the guest's stores it asks for.
#[derive(Debug)]
pub(crate) struct HeldRequest {
  /// The queue the chain came from.
  pub(crate) queue: u16,
  /// The chain, gathered again when it is served.
  pub(crate) chain: DescriptorChain,
}

/// A served chain whose used element waits for a refill: its reply could reveal the disk's change (a size, a time)
/// before the guest's map shows it.
#[derive(Debug)]
pub(crate) struct HeldReply {
  /// The queue the chain came from.
  pub(crate) queue: u16,
  /// The chain, its reply scattered.
  pub(crate) chain: DescriptorChain,
  /// The reply's length, the used element's.
  pub(crate) written: u32,
}

/// What the message placer's backed DAX did: non-vacuity witnesses for the tests and the status report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackedCounts {
  /// Maps sent to the front end.
  pub maps_sent: u64,
  /// SETUPMAPPINGs answered with no message: a window the front end maps as asked already.
  pub maps_kept: u64,
  /// Unmaps sent to the front end.
  pub unmaps_sent: u64,
  /// Unmaps the front end refused: the guest's window range still maps the backing, which stays equal to the file.
  pub unmaps_refused: u64,
  /// Requests held for an ingest of the guest's stores.
  pub ingests_held: u64,
  /// Replies held for a refill.
  pub replies_held: u64,
  /// Backings let go: their nodes forgotten, the guest unmounted, or the device ended.
  pub released: u64,
  /// The session's own counts, summed since the device began.
  pub session: Counts,
}

impl BackedCounts {
  /// Adds the session's counts since it was last asked.
  pub(crate) fn add_session(&mut self, counts: Counts) {
    let summed = &mut self.session;
    summed.answered = summed.answered.saturating_add(counts.answered);
    summed.ingested = summed.ingested.saturating_add(counts.ingested);
    summed.ingest_refused = summed.ingest_refused.saturating_add(counts.ingest_refused);
    summed.refilled = summed.refilled.saturating_add(counts.refilled);
    summed.refill_refused = summed.refill_refused.saturating_add(counts.refill_refused);
    summed.refill_writable = summed
      .refill_writable
      .saturating_add(counts.refill_writable);
  }
}

/// The message placer's windows, its session and what it holds of the guest's requests.
#[derive(Debug, Default)]
pub(crate) struct BackedDax {
  /// The live windows, bounded by the guest's window: one per granule at most.
  pub(crate) windows: Vec<BackedWindow>,
  /// The backings, their fills, ingests and refills.
  pub(crate) session: DaxBackingSession,
  /// A SETUPMAPPING held for its fill and its acknowledgement.
  pub(crate) map: Option<HeldMap>,
  /// A request held for its ingest.
  pub(crate) request: Option<HeldRequest>,
  /// A reply held for a refill.
  pub(crate) reply: Option<HeldReply>,
  /// What it did.
  pub(crate) counts: BackedCounts,
}

impl BackedDax {
  /// Whether it holds a request of the guest's, so the device serves nothing more until it is answered.
  pub(crate) fn holds(&self) -> bool {
    self.map.is_some() || self.request.is_some() || self.reply.is_some()
  }

  /// Whether a held SETUPMAPPING's map is sent and awaits the front end's acknowledgement.
  pub(crate) fn acking(&self) -> bool {
    self.map.as_ref().is_some_and(|held| held.sent)
  }

  /// Adds what the session did since it was last asked to the counts.
  pub(crate) fn take_session_counts(&mut self) {
    let counts = self.session.take_counts();
    self.counts.add_session(counts);
  }
}

/// Whether a SETUPMAPPING's file range fits the backing's object: it starts at a whole granule, since the front end maps
/// the object from there, and ends within the object. The guest's window range is the placer's to check.
pub(crate) fn file_range_fits(foffset: u64, len: u64, granule: u64) -> bool {
  granule > 0
    && foffset.is_multiple_of(granule)
    && foffset
      .checked_add(len)
      .is_some_and(|end| end <= DAX_OBJECT_BYTES)
}

/// Which backings a request asks the guest's stores of to reach the volume before it is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IngestAsk {
  /// Those of these nodes.
  Nodes(Vec<u64>),
  /// Every backing: the guest unmounts.
  All,
}

/// The backings `request` asks the guest's stores of to reach the volume first, if it is a request that does:
/// - an `FSYNC`, `FLUSH` or `RELEASE` of a node, the guest's own sync points;
/// - a `READ` of a node, so a read through the queue never contradicts the guest's stores through its map;
/// - a `FORGET` or `BATCH_FORGET`, whose nodes' backings are let go once it is served;
/// - a `DESTROY`, after which nothing of the guest's remains.
///
/// The same requests as the Shards route's (`crates/server/src/shards/dax.rs`), with the unmount added, since a guest
/// device sees it and a stream does not.
pub(crate) fn ingest_asked(request: &[u8]) -> Option<IngestAsk> {
  let header = InHeader::parse(request).ok()?;
  match Opcode::from_wire(header.opcode)? {
    Opcode::FSync | Opcode::Flush | Opcode::Release | Opcode::Read => {
      Some(IngestAsk::Nodes(vec![header.nodeid]))
    }
    Opcode::Forget | Opcode::BatchForget => Some(IngestAsk::Nodes(forgotten(request, &[]))),
    Opcode::Destroy => Some(IngestAsk::All),
    _ => None,
  }
}

/// The nodes a `FORGET` or `BATCH_FORGET` request forgets, as the kernel-change reader parses them.
pub(crate) fn forgotten(request: &[u8], reply: &[u8]) -> Vec<u64> {
  let mut nodes = Vec::new();
  kernel_changes(request, reply, &mut |change| {
    if let KernelChange::Forgot { node, .. } = change {
      nodes.push(node);
    }
  });
  nodes
}

#[cfg(test)]
mod tests {
  // Test harness code: an unwrap here is a failed test.
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
  use super::*;

  /// A FUSE request: the in-header (len, opcode, unique, nodeid, uid, gid, pid, padding) and `body`.
  fn request(opcode: Opcode, nodeid: u64, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(slates_bridge_fuse::abi::IN_HEADER_LEN + body.len()).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&opcode.to_wire().to_le_bytes());
    bytes.extend_from_slice(&7u64.to_le_bytes());
    bytes.extend_from_slice(&nodeid.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    bytes.extend_from_slice(body);
    bytes
  }

  /// §4.6 — which requests ask for the guest's stores first. Do: parse an FSYNC, a READ, a FORGET, a BATCH_FORGET of two
  /// nodes, a DESTROY and a GETATTR. Expect the node of each sync point and read, the forgotten nodes, every backing for
  /// the unmount, and nothing for the GETATTR.
  #[test]
  fn sync_points_reads_forgets_and_the_unmount_ask_for_an_ingest_and_nothing_else_does() {
    let fsync = request(Opcode::FSync, 9, &[0u8; 16]);
    assert_eq!(ingest_asked(&fsync), Some(IngestAsk::Nodes(vec![9])));
    let read = request(Opcode::Read, 11, &[0u8; 40]);
    assert_eq!(ingest_asked(&read), Some(IngestAsk::Nodes(vec![11])));
    let forget = request(Opcode::Forget, 13, &1u64.to_le_bytes());
    assert_eq!(ingest_asked(&forget), Some(IngestAsk::Nodes(vec![13])));
    let mut batch = Vec::new();
    batch.extend_from_slice(&2u32.to_le_bytes());
    batch.extend_from_slice(&0u32.to_le_bytes());
    for node in [21u64, 22] {
      batch.extend_from_slice(&node.to_le_bytes());
      batch.extend_from_slice(&1u64.to_le_bytes());
    }
    let batch = request(Opcode::BatchForget, 0, &batch);
    assert_eq!(ingest_asked(&batch), Some(IngestAsk::Nodes(vec![21, 22])));
    let destroy = request(Opcode::Destroy, 0, &[]);
    assert_eq!(ingest_asked(&destroy), Some(IngestAsk::All));
    let getattr = request(Opcode::GetAttr, 9, &[0u8; 16]);
    assert_eq!(ingest_asked(&getattr), None);
  }

  /// §4.6 — a SETUPMAPPING's file range. Do: check ranges at and off a granule, at the backing's end, past it, one
  /// whose end overflows, and a zero granule. Expect only a range at a granule that ends within the object to fit.
  #[test]
  fn a_file_range_must_start_at_a_granule_and_end_within_the_object() {
    let granule = 4096;
    assert!(file_range_fits(0, 2 << 20, granule));
    assert!(!file_range_fits(1, 2 << 20, granule), "off a granule");
    assert!(
      file_range_fits(DAX_OBJECT_BYTES - granule, granule, granule),
      "ending at the object's end"
    );
    assert!(
      !file_range_fits(DAX_OBJECT_BYTES, granule, granule),
      "past the object"
    );
    assert!(
      !file_range_fits(u64::MAX - granule + 1, granule, granule),
      "an end that overflows"
    );
    assert!(!file_range_fits(0, granule, 0), "a zero granule");
  }
}

//! The DAX half of the Shards route on Windows, before Windows has one (§4.6; A-179; plan G-shards-windows step 5 owes
//! the real one). It has the Unix arm's surface (`dax.rs`), so the stream's serve loop is one code path. Here no file
//! is offered for DAX: the guest's `INIT` reply keeps neither `FUSE_HAS_INODE_DAX` nor `FUSE_MAP_ALIGNMENT`
//! ([`DaxOffer::Withheld`]), so a guest asks for no mapping. A DESCRIPTOR ask that comes anyway is refused `ENOSYS` and
//! counted `shards.dax_not_built`: the answer Shards' device reads as "this backend offers no window", which its guest's
//! mapping returns. With nothing ever backed, there is never anything to ingest or refill.
//!
//! Windows will answer with a section of the file's DAX object duplicated into the VM's process, once the daemon can
//! check that the asker holds this attachment's stream (step 5's claim check, by the pipe's name).

use slates_bridge_core::DaxOffer;
use slates_bridge_fuse::bridge::ENOSYS;
use slates_bridge_fuse::shards_fs::descriptor_refusal;

use super::{Done, ShardsStream, owe};
use crate::state::ShardState;

/// Format: the refusal-ledger name of a descriptor ask on a platform whose Shards route has no DAX window yet.
const NOT_BUILT: &str = "shards.dax_not_built";

/// A descriptor a reply would carry: none exists here, so this type has no values and no reply owes one.
#[derive(Debug)]
pub(crate) enum Descriptor {}

/// A stream's DAX session: empty for its whole life, since no file is backed here.
#[derive(Debug, Default)]
pub(crate) struct Session;

impl Session {
  /// An empty session.
  pub(crate) fn new() -> Session {
    Session
  }

  /// Whether no file is backed: always.
  pub(crate) fn is_empty(&self) -> bool {
    true
  }

  /// Whether a fill is under way: never.
  pub(crate) fn filling(&self) -> bool {
    false
  }

  /// Whether an ingest is under way: never.
  pub(crate) fn ingesting(&self) -> bool {
    false
  }
}

/// The DAX offer a stream's requests are answered under: withheld.
pub(super) fn offer(_s: &ShardState) -> DaxOffer {
  DaxOffer::Withheld
}

/// Answers a DESCRIPTOR ask with `ENOSYS`, counted.
pub(super) fn begin(s: &mut ShardState, stream: &mut ShardsStream, _body: &[u8]) {
  owe(stream, &descriptor_refusal(ENOSYS));
  s.count(NOT_BUILT, 1);
}

/// No fill is ever pending.
pub(super) fn step(_s: &mut ShardState, _stream: &mut ShardsStream) {}

/// No node is backed, so none is let go.
pub(super) fn release(_s: &mut ShardState, _stream: &mut ShardsStream, _inode: u64) {}

/// Nothing is backed, so nothing is let go.
pub(super) fn release_all(_s: &mut ShardState, _stream: &mut ShardsStream) {}

/// No request is held for an ingest: nothing is backed.
pub(super) fn hold_for_ingest(
  _s: &ShardState,
  _stream: &mut ShardsStream,
  _request: &[u8],
  _total: usize,
) -> bool {
  false
}

/// No ingest before the end: nothing is backed.
pub(super) fn ingest_before_end(_s: &ShardState, _stream: &mut ShardsStream) -> bool {
  false
}

/// No ingest is ever under way.
pub(super) fn ingest_step(_s: &mut ShardState, _stream: &mut ShardsStream) -> Option<Done> {
  None
}

/// No refill is ever owed.
pub(super) fn refill_owed(_s: &ShardState, _stream: &mut ShardsStream) -> bool {
  false
}

/// No refill is ever owed, so none steps.
pub(super) fn refill_step(_s: &mut ShardState, _stream: &mut ShardsStream) {}

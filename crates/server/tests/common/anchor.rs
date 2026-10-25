//! The test plays the anchor (§2.6 boot step 2, §4.8 "Recovery"): it holds one anchor segment and its
//! content object across two in-process daemons, so a restart recovers exactly what the first daemon
//! published — a daemon's `stop` publishes nothing, so what survives is what its barriers made durable.
//! It holds the client rendezvous too, as the anchor does (A-114), and every daemon it starts adopts it.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_anchor::AnchorSegment;
use slates_ipc::HeldRendezvous;
use slates_machine::MachineProfile;
use slates_server::{DaemonConfig, SegmentSource};

/// What the test holds as the anchor does: the segment with its content object, and the instance's rendezvous.
pub(crate) struct Anchor {
  /// The segment and its content object.
  pub(crate) segment: AnchorSegment,
  /// The rendezvous every daemon started over [`source_of`] adopts.
  rendezvous: HeldRendezvous,
}

/// The anchor's segment and content object for a test, the content object sized as the anchor sizes it
/// (`DaemonConfig::content_bytes`: per shard, the write log, the two image slots and the arena range; lazily
/// backed, so the unused tail costs no RAM), and the rendezvous of `config`'s instance.
pub(crate) fn held_anchor(name: &str, profile: &MachineProfile, config: &DaemonConfig) -> Anchor {
  let content_bytes = config.content_bytes();
  let segment = AnchorSegment::create(
    &format!("slates-seg-{name}"),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(&format!("slates-con-{name}"), content_bytes)
  .unwrap();
  Anchor {
    segment,
    rendezvous: HeldRendezvous::hold(&config.instance).unwrap(),
  }
}

/// The handoff a daemon attaches by: the segment, its content object and the rendezvous.
pub(crate) fn source_of(anchor: &Anchor) -> SegmentSource {
  let (handoff, len) = anchor.segment.handoff().unwrap();
  let content = anchor.segment.content_handoff().unwrap();
  SegmentSource::Handoff {
    handoff,
    len,
    content,
    rendezvous: anchor.rendezvous.adoption(),
  }
}

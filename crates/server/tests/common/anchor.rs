//! The test plays the anchor (§2.6 boot step 2, §4.8 "Recovery"): it holds one anchor segment and its
//! content object across two in-process daemons, so a restart recovers exactly what the first daemon
//! published — a daemon's `stop` publishes nothing, so what survives is what its barriers made durable.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_anchor::AnchorSegment;
use slates_machine::MachineProfile;
use slates_server::{DaemonConfig, SegmentSource};

/// Shape: the content object's slots per shard — the recovery image is a double buffer (the committed
/// image and the one being published), so a torn publish preserves the committed one (§4.8).
const PUBLISH_SLOTS: usize = 2;

/// The anchor's segment and content object for a test: the content object is [`PUBLISH_SLOTS`]
/// reserve-sized slots per shard times the partitions (lazily backed, so the unused tail costs no RAM).
pub(crate) fn anchor_segment(
  name: &str,
  profile: &MachineProfile,
  config: &DaemonConfig,
) -> AnchorSegment {
  let content_bytes = usize::try_from(config.reserve_per_shard).unwrap_or(usize::MAX)
    * PUBLISH_SLOTS
    * usize::from(config.geometry.partitions.max(1));
  AnchorSegment::create(
    &format!("slates-seg-{name}"),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(&format!("slates-con-{name}"), content_bytes)
  .unwrap()
}

/// The handoff a daemon attaches by: the segment and its content object.
pub(crate) fn source_of(segment: &AnchorSegment) -> SegmentSource {
  let (handoff, len) = segment.handoff().unwrap();
  let content = segment.content_handoff().unwrap();
  SegmentSource::Handoff {
    handoff,
    len,
    content,
  }
}

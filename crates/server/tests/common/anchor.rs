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
    &super::fixture_object("slates-seg", name),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(&super::fixture_object("slates-con", name), content_bytes)
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

/// A daemon started over an anchor the test holds (A-232): the test plays the anchor, so the daemon's beat is read
/// (`SegmentSource::Handoff` is anchored) and a stuck shard withholds the beat rather than ending the test's process.
/// A test that keeps a shard stuck past a window while it watches the daemon go on (its beat stop, an observation's
/// deadline, a refusal by the wedged shard's peers) is about an anchored daemon, the only kind that lives stuck, until
/// its anchor kills it; this test ends it itself. Derefs to the daemon.
pub(crate) struct Anchored {
  /// Declared first, so it drops first: the daemon stops before the anchor lets its segment and rendezvous go.
  daemon: slates_server::Daemon,
  anchor: Anchor,
}

impl Anchored {
  /// Starts a daemon of `config` over a fresh anchor named for `name`.
  pub(crate) fn start(name: &str, profile: &MachineProfile, config: DaemonConfig) -> Anchored {
    let anchor = held_anchor(name, profile, &config);
    let daemon = slates_server::Daemon::start(profile, config, source_of(&anchor)).unwrap();
    Anchored { daemon, anchor }
  }

  /// The anchor the test holds.
  pub(crate) fn anchor(&self) -> &Anchor {
    &self.anchor
  }

  /// Stops the daemon, then lets the anchor go.
  pub(crate) fn stop(self) {
    let Anchored { daemon, anchor } = self;
    daemon.stop();
    drop(anchor);
  }
}

impl std::ops::Deref for Anchored {
  type Target = slates_server::Daemon;

  fn deref(&self) -> &slates_server::Daemon {
    &self.daemon
  }
}

//! The kernel-coherence rounds (§4.6 "Cache posture"; AUD-02) over a real volume bridge with a
//! recording sink, on every host: a change through **another attachment** is delivered as the
//! kernel invalidation the seam names while the transport's own change is not; a seam refusal to
//! gather keeps the cursor, so the round the transport then serves does not skip the missed change
//! and the next round delivers it; and a sink refusal keeps the cursor, so the round is delivered
//! again. The Linux lane runs the same rules against a real mount (`tests/coherence_mount.rs`).
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{
  AttachmentId, Attachments, Bridge, Invalidation, ObjectId, OpContext, Rights, SetAttr, View,
};
use slates_bridge_fuse::coherence::{Coherence, Delivered};
use slates_bridge_fuse::volume_bridge::VolumeBridge;
use slates_db::catalog::{Principal, VolumeId};

mod common;
use common::{FailingGather, store, volume};

/// Shape: the volume id every test attaches to, and the two attachments' subjects.
const VOLUME: VolumeId = VolumeId { bytes: [7; 16] };
const TRANSPORT_UID: u32 = 1000;
const OTHER_UID: u32 = 1001;
/// Shape: a file created at four bytes and truncated to nine, then to two.
const CREATED_MODE: u32 = 0o644;

/// A read-write current-view attachment for `uid` on the volume.
fn attach(attachments: &mut Attachments, uid: u32) -> AttachmentId {
  attachments
    .attach(
      VOLUME,
      View::Current,
      Principal::Uid { uid },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap()
}

/// Creates `name` in the root through `cx`; the file's object.
fn create(bridge: &mut dyn Bridge, cx: &OpContext, name: &str) -> ObjectId {
  let root = bridge.root(cx).unwrap();
  let (attr, _) = bridge
    .create(ObjectId::new(root, 0), cx, name, CREATED_MODE, 0)
    .unwrap();
  ObjectId::new(attr.ino, attr.generation)
}

/// Truncates `object` to `size` through `cx` — the other attachment's change.
fn truncate(bridge: &mut dyn Bridge, cx: &OpContext, object: ObjectId, size: u64) {
  bridge
    .setattr(
      object,
      cx,
      SetAttr {
        size: Some(size),
        ..SetAttr::default()
      },
    )
    .unwrap();
}

/// One round with a recording sink: what was delivered, and the invalidations written.
fn round(
  coherence: &mut Coherence,
  bridge: &mut dyn Bridge,
  cx: &OpContext,
) -> (Delivered, Vec<Invalidation>) {
  let mut written = Vec::new();
  let delivered = coherence
    .deliver(bridge, cx, &mut |invalidation: &Invalidation| {
      written.push(invalidation.clone());
      Ok::<(), ()>(())
    })
    .unwrap();
  (delivered, written)
}

/// Whether `written` names `object`'s inode as stale.
fn invalidates(written: &[Invalidation], object: ObjectId) -> bool {
  written
    .iter()
    .any(|i| matches!(i, Invalidation::Inode { ino, .. } if *ino == object.inode))
}

/// A change through another attachment reaches the transport's kernel as an inode invalidation;
/// the transport's own change does not (its kernel made it), and a round with nothing new writes
/// nothing.
#[test]
fn a_change_through_another_attachment_is_delivered_and_the_transports_own_is_not() {
  let mut store = store();
  let mut volume = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut volume, &mut store);
  let mut attachments = Attachments::new();
  let transport = attach(&mut attachments, TRANSPORT_UID);
  let other = attach(&mut attachments, OTHER_UID);
  let cx_transport = attachments.context(transport).unwrap();
  let cx_other = attachments.context(other).unwrap();
  let mut coherence = Coherence::new();

  // The transport's own request: the create is its kernel's doing.
  let (before, _) = round(&mut coherence, &mut bridge, &cx_transport);
  let file = create(&mut bridge, &cx_transport, "f");
  coherence.served_own_request(&mut bridge, &cx_transport, before);
  let (own, written) = round(&mut coherence, &mut bridge, &cx_transport);
  assert_eq!(
    own.written, 0,
    "the transport's own create is not invalidated in its own kernel: {written:?}"
  );

  // Another attachment's change is.
  truncate(&mut bridge, &cx_other, file, 9);
  let (delivered, written) = round(&mut coherence, &mut bridge, &cx_transport);
  assert!(
    invalidates(&written, file),
    "the other attachment's truncate invalidates the file's inode: {written:?}"
  );
  assert_eq!(delivered.written, written.len());
  assert!(!delivered.gather_refused);

  let (again, written) = round(&mut coherence, &mut bridge, &cx_transport);
  assert_eq!(
    again.written, 0,
    "nothing new: nothing written: {written:?}"
  );
  assert_eq!(coherence.gather_refusals(), 0);
}

/// A seam refusal to gather keeps the cursor: the request the transport serves under it does not
/// move the cursor past the missed change, and the next round delivers it (AUD-02's injected
/// collection failure). Before the fix the cursor was taken past the request unconditionally, so
/// the change made before the refused round was never delivered.
#[test]
fn a_refused_gather_keeps_the_cursor_and_the_next_round_delivers_the_missed_change() {
  let mut store = store();
  let mut volume = volume(&mut store);
  let mut bridge = FailingGather {
    inner: VolumeBridge::new(VOLUME, &mut volume, &mut store),
    refuse_gathers: 0,
  };
  let mut attachments = Attachments::new();
  let transport = attach(&mut attachments, TRANSPORT_UID);
  let other = attach(&mut attachments, OTHER_UID);
  let cx_transport = attachments.context(transport).unwrap();
  let cx_other = attachments.context(other).unwrap();
  let mut coherence = Coherence::new();

  let (before, _) = round(&mut coherence, &mut bridge, &cx_transport);
  let file = create(&mut bridge, &cx_transport, "f");
  coherence.served_own_request(&mut bridge, &cx_transport, before);
  let cursor = coherence.cursor();

  // The other attachment changes the file; the seam then refuses the gather of the round the
  // transport runs before its next request, and the transport serves that request (a create of its
  // own) under the refusal.
  truncate(&mut bridge, &cx_other, file, 9);
  bridge.refuse_gathers = 1;
  let (refused, written) = round(&mut coherence, &mut bridge, &cx_transport);
  assert!(refused.gather_refused, "the seam refused: {written:?}");
  assert_eq!(refused.written, 0);
  assert_eq!(coherence.gather_refusals(), 1, "the refusal is counted");
  assert_eq!(
    coherence.cursor(),
    cursor,
    "a refused gather leaves the cursor where it was"
  );
  let own = create(&mut bridge, &cx_transport, "g");
  coherence.served_own_request(&mut bridge, &cx_transport, refused);
  assert_eq!(
    coherence.cursor(),
    cursor,
    "a request served under a refused round does not move the cursor past the missed change"
  );

  // The next round delivers the missed change (and, harmlessly, the transport's own create).
  let (delivered, written) = round(&mut coherence, &mut bridge, &cx_transport);
  assert!(!delivered.gather_refused);
  assert!(
    invalidates(&written, file),
    "the change made before the refused round is delivered at the next: {written:?}"
  );
  assert!(
    coherence.cursor().unwrap().journal_seq > cursor.unwrap().journal_seq,
    "the cursor moved past the delivered round"
  );
  let _ = own;
}

/// A sink refusal (the transport could not write the notification) keeps the cursor, so the round is
/// delivered again on a re-established transport; an invalidation delivered twice is harmless.
#[test]
fn a_sink_refusal_keeps_the_cursor_so_the_round_is_delivered_again() {
  let mut store = store();
  let mut volume = volume(&mut store);
  let mut bridge = VolumeBridge::new(VOLUME, &mut volume, &mut store);
  let mut attachments = Attachments::new();
  let transport = attach(&mut attachments, TRANSPORT_UID);
  let other = attach(&mut attachments, OTHER_UID);
  let cx_transport = attachments.context(transport).unwrap();
  let cx_other = attachments.context(other).unwrap();
  let mut coherence = Coherence::new();

  let (before, _) = round(&mut coherence, &mut bridge, &cx_transport);
  let file = create(&mut bridge, &cx_transport, "f");
  coherence.served_own_request(&mut bridge, &cx_transport, before);
  let cursor = coherence.cursor();
  truncate(&mut bridge, &cx_other, file, 2);

  let refused = coherence.deliver(&mut bridge, &cx_transport, &mut |_: &Invalidation| Err(()));
  assert_eq!(refused, Err(()), "the sink's refusal is the transport's");
  assert_eq!(
    coherence.cursor(),
    cursor,
    "a round the transport could not write leaves the cursor where it was"
  );
  let (delivered, written) = round(&mut coherence, &mut bridge, &cx_transport);
  assert!(
    invalidates(&written, file) && delivered.written == written.len(),
    "the round is delivered again: {written:?}"
  );
}

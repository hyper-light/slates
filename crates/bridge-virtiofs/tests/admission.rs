//! Device admission and credits proven by use (§4.6 A-9: "Device admission authenticates the
//! consumer before creating a queue, mapping guest memory or publishing a tag. ... In-flight
//! requests, mapped bytes, copy buffers and replies consume the attachment's credits; cancellation
//! and revocation reclaim them under an owned terminal step"; §4.13; AC-4.12/T-4.14: "revoke ...
//! while requests ... are active. Expect refusal before access ... and eventual reclamation"). A
//! simulated VMM seam records the order the device touches it in, so the ordering the contract
//! demands is asserted, not assumed; the simulated guest memory's access log proves refusals came
//! before any access; a real `VolumeBridge` on a scratch volume shows the references reclaimed.
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use common::{SEAM_RELEASES, SeamCall, SimVmm, message, reply_error, store, vid, volume};
use slates_bridge_core::{Attachments, Bridge, ObjectId, VolumeBridge};
use slates_bridge_fuse::abi::{OUT_HEADER_LEN, Opcode};
use slates_bridge_virtiofs::admission::{
  AdmissionError, AdmittedDevice, GuestAttachRequest, GuestTransport, ReclaimError, SeamError,
  ServeError, UnsupportedReason, admit,
};
use slates_bridge_virtiofs::capability::{Conformance, ReadWritePolicy, TargetPath};
use slates_bridge_virtiofs::credit::{AttachmentCredits, CreditError, CreditKind};
use slates_bridge_virtiofs::device::{
  DeviceConfig, DeviceError, FIRST_REQUEST_QUEUE, FsTag, Serviced,
};
use slates_bridge_virtiofs::memory::{GuestMemory, GuestRange};
use slates_db::catalog::Principal;
use slates_machine::derived;
use slates_vfs::error::VfsError;

/// Shape: the reply room posted for every request in these tests (a GETATTR reply is 120 bytes).
const REPLY_CAP: u32 = 256;
/// Shape: the queue sizes: a small hiprio queue and a request queue with room for the scripts.
const QUEUE_SIZES: [u16; 2] = [8, 32];

/// Credits with room for `requests` chains in flight and `bytes` copy bytes.
fn credits(requests: u32, bytes: u64) -> AttachmentCredits {
  AttachmentCredits {
    requests: derived!(requests, "the test's request credit", ["test"]),
    bytes: derived!(bytes, "the test's byte credit", ["test"]),
  }
}

/// Credits generous enough that nothing in these tests is capped by them.
fn roomy() -> AttachmentCredits {
  credits(64, 1 << 20)
}

fn request(transport: GuestTransport) -> GuestAttachRequest {
  GuestAttachRequest {
    transport,
    volume: vid(),
    dax: false,
    notification_queue: false,
  }
}

/// The access list of these tests: every authenticated consumer may read and write.
fn rw(_consumer: &Principal) -> slates_bridge_core::Rights {
  slates_bridge_core::Rights {
    read: true,
    write: true,
  }
}

/// A seam whose guest driver has published `count` GETATTR-of-root requests.
fn seam_with_requests(count: u64) -> SimVmm {
  let mut seam = SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 501 }));
  for unique in 1..=count {
    seam.guest_mut().submit(
      usize::from(FIRST_REQUEST_QUEUE),
      &message(Opcode::GetAttr.to_wire(), unique, 1, &[0u8; 16]),
      REPLY_CAP,
      1,
    );
  }
  seam
}

/// The device's tag for every test.
fn config() -> DeviceConfig {
  DeviceConfig::new(FsTag::new("slates").unwrap())
}

/// Admission calls the seam in the contract's order — consumer, then queues and memory, then the
/// tag — and a seam that cannot establish the consumer sees nothing else touched: no queue, no
/// mapping, no tag; the seam is released.
#[test]
fn admission_authenticates_the_consumer_before_any_queue_memory_or_tag() {
  let mut registry = Attachments::new();
  let seam = SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 501 }));
  let admitted = admit(
    request(GuestTransport::InProcess),
    seam,
    config(),
    roomy(),
    rw,
    &mut registry,
  )
  .unwrap();
  assert_eq!(
    admitted.seam().calls(),
    vec![
      SeamCall::Consumer,
      SeamCall::Queues,
      SeamCall::Memory,
      SeamCall::Publish
    ],
    "the consumer is authenticated before a queue, a mapping or a tag"
  );
  assert_eq!(admitted.seam().published().unwrap().tag.as_str(), "slates");
  assert_eq!(
    admitted.context(&registry).unwrap().subject,
    Principal::Uid { uid: 501 }
  );

  let seam = SimVmm::new(&QUEUE_SIZES, Err(SeamError::ConsumerUnverified));
  let refused = admit(
    request(GuestTransport::InProcess),
    seam,
    config(),
    roomy(),
    rw,
    &mut registry,
  )
  .unwrap_err();
  assert_eq!(
    refused.error,
    AdmissionError::ConsumerRefused(SeamError::ConsumerUnverified)
  );
  assert_eq!(
    refused.seam.calls(),
    vec![SeamCall::Consumer],
    "nothing but the consumer question was asked"
  );
  assert!(refused.seam.published().is_none(), "no tag was published");
  assert!(
    refused.seam.released(),
    "the failed admission released the seam"
  );
}

/// §4.6: "Requesting an unsupported form returns `AttachmentUnsupported{transport, reason}`" — a
/// DAX request is refused with the contract's reason, a notification-queue request with the
/// feature not offered, and the inherited-descriptor transport with its binding not built; each
/// before the seam is touched at all.
#[test]
fn an_unsupported_form_is_refused_typed_before_the_seam_is_touched() {
  let mut registry = Attachments::new();
  let dax = GuestAttachRequest {
    dax: true,
    ..request(GuestTransport::InProcess)
  };
  let refused = admit(
    dax,
    SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 0 })),
    config(),
    roomy(),
    rw,
    &mut registry,
  )
  .unwrap_err();
  assert_eq!(
    refused.error,
    AdmissionError::AttachmentUnsupported {
      transport: GuestTransport::InProcess,
      reason: UnsupportedReason::DaxNotEstablished,
    }
  );
  assert!(refused.seam.calls().is_empty(), "the seam was not touched");

  let notify = GuestAttachRequest {
    notification_queue: true,
    ..request(GuestTransport::InProcess)
  };
  assert!(matches!(
    admit(
      notify,
      SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 0 })),
      config(),
      roomy(),
      rw,
      &mut registry
    )
    .unwrap_err()
    .error,
    AdmissionError::AttachmentUnsupported {
      reason: UnsupportedReason::NotificationQueueNotOffered,
      ..
    }
  ));

  // The inherited-descriptor form is the vhost-user binding, built on Linux (AUD-29-68); elsewhere it is refused
  // before the seam is touched.
  let inherited = request(GuestTransport::InheritedDescriptor);
  let admitted = admit(
    inherited,
    SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 0 })),
    config(),
    roomy(),
    rw,
    &mut registry,
  );
  if cfg!(target_os = "linux") {
    assert!(admitted.is_ok(), "the binding is served on Linux");
  } else {
    let refused = admitted.unwrap_err();
    assert_eq!(
      refused.error,
      AdmissionError::AttachmentUnsupported {
        transport: GuestTransport::InheritedDescriptor,
        reason: UnsupportedReason::BindingNotBuilt,
      }
    );
    assert!(refused.seam.calls().is_empty());
  }
}

/// Every chain is charged against the attachment's request and byte credits before it is touched, and its
/// request released once its used element is published: a request credit of two serves two of five per pass,
/// no request is in flight after a pass, the bytes in flight are exactly the copy buffers the device keeps
/// (AUD-29-77), and the guest is notified once per pass.
#[test]
fn requests_are_charged_against_the_credits_and_released_on_completion() {
  let mut registry = Attachments::new();
  let seam = seam_with_requests(5);
  let mut admitted = admit(
    request(GuestTransport::InProcess),
    seam,
    config(),
    credits(2, 1 << 20),
    rw,
    &mut registry,
  )
  .unwrap();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);

  let first = admitted.service(&mut bridge, &registry).unwrap();
  assert_eq!((first.served, first.more_pending), (2, true));
  let ledger = admitted.ledger();
  assert_eq!(
    ledger.in_flight(),
    (0, admitted.device().retained_copy_bytes()),
    "requests released on completion; the kept buffers stay charged"
  );
  assert_eq!(ledger.counters().charged, 2);
  assert_eq!(ledger.counters().released, 2);
  assert_eq!(admitted.seam().notified(), vec![FIRST_REQUEST_QUEUE]);

  let second = admitted.service(&mut bridge, &registry).unwrap();
  assert_eq!((second.served, second.more_pending), (2, true));
  let third = admitted.service(&mut bridge, &registry).unwrap();
  assert_eq!((third.served, third.more_pending), (1, false));
  assert_eq!(admitted.ledger().counters().charged, 5);
  assert_all_answered(&mut admitted, 5);
}

/// AC-3.3 (A-196): a pass notifies a queue only for the replies it published. Do: over a published volume holding a
/// file, switch the device's told hold on; publish a write to the file and serve it while the volume's FUSE mounts are
/// untold; then publish a GETATTR and serve it; then settle once with the mounts still untold and once told. Expect:
/// the first pass serves the write, publishes nothing and notifies nothing; the second publishes the GETATTR's reply and
/// notifies once, for it; the untold settle publishes and notifies nothing; the told settle publishes the write's reply
/// and notifies once more. A notification with nothing new in the used ring wakes the guest for nothing, and a front end
/// that reaps once per interrupt finds no reply: the daemon's vhost-user tests (`crates/server/tests/virtiofs.rs`,
/// Linux) found the pass doing so.
#[test]
fn a_pass_notifies_a_queue_only_for_the_replies_it_published() {
  let mut registry = Attachments::new();
  let seam = SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 501 }));
  let mut admitted = admit(
    request(GuestTransport::InProcess),
    seam,
    config(),
    roomy(),
    rw,
    &mut registry,
  )
  .unwrap();
  let mut store = store();
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let node = vol
    .create_file_no(&mut store, root, "told", 0o644)
    .unwrap()
    .0;
  // Published, so no reply waits for the owner's barrier (D-18) and the write's goes straight to the told hold.
  vol.mark_published(&store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  admitted.hold_replies_for_told();

  let write_head = submit_to_the_request_queue(&mut admitted, Opcode::Write, node, &write_body());
  let (held, notified) = serve_one_pass(&mut admitted, &mut bridge, &registry);
  assert_eq!(
    (held.served, held.published, notified),
    (1, 0, 0),
    "the write served and its reply held: nothing published, nothing notified"
  );
  let getattr_head = submit_to_the_request_queue(&mut admitted, Opcode::GetAttr, node, &[0u8; 16]);
  let (pass, notified) = serve_one_pass(&mut admitted, &mut bridge, &registry);
  assert_eq!(
    (pass.served, pass.published, notified),
    (1, 1, 1),
    "the GETATTR's reply published, and one notification for it"
  );
  assert_eq!(
    reaped(&mut admitted),
    vec![getattr_head],
    "the write's reply still waits"
  );
  settles_notify_for_what_they_publish(&mut admitted, write_head);
}

/// A `fuse_write_in` with no handle, at the start, then its 64 bytes: fh, offset, size, write_flags, lock_owner, flags
/// and padding.
fn write_body() -> Vec<u8> {
  let bytes = [0x5Au8; 64];
  let mut write = 0u64.to_le_bytes().to_vec();
  write.extend_from_slice(&0u64.to_le_bytes());
  write.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_le_bytes());
  write.extend_from_slice(&[0u8; 20]);
  write.extend_from_slice(&bytes);
  write
}

/// The guest publishes one `opcode` request on node `node` on the request queue: its chain's head.
fn submit_to_the_request_queue(
  admitted: &mut AdmittedDevice<SimVmm>,
  opcode: Opcode,
  node: u64,
  body: &[u8],
) -> u16 {
  let unique = u64::from(opcode.to_wire());
  admitted.seam_mut().guest_mut().submit(
    usize::from(FIRST_REQUEST_QUEUE),
    &message(opcode.to_wire(), unique, node, body),
    REPLY_CAP,
    1,
  )
}

/// One service pass, and how many notifications it sent.
fn serve_one_pass(
  admitted: &mut AdmittedDevice<SimVmm>,
  bridge: &mut VolumeBridge<'_>,
  registry: &Attachments,
) -> (Serviced, usize) {
  let before = admitted.seam().notified().len();
  let pass = admitted.service(bridge, registry).unwrap();
  (pass, admitted.seam().notified().len() - before)
}

/// The heads of every used element the guest can reap on the request queue now.
fn reaped(admitted: &mut AdmittedDevice<SimVmm>) -> Vec<u16> {
  let guest = admitted.seam_mut().guest_mut();
  std::iter::from_fn(|| guest.reap(usize::from(FIRST_REQUEST_QUEUE)))
    .map(|(head, _)| head)
    .collect()
}

/// Settles the held write's reply untold, then told: the first publishes and notifies nothing, the second publishes
/// `write_head`'s reply, a success, and notifies once.
fn settles_notify_for_what_they_publish(admitted: &mut AdmittedDevice<SimVmm>, write_head: u16) {
  let before = admitted.seam().notified().len();
  let untold = admitted.settle_told(&mut |_| false, (0, u64::MAX)).unwrap();
  assert_eq!(
    (
      untold.completed.len(),
      admitted.seam().notified().len() - before
    ),
    (0, 0),
    "nothing published or notified while untold"
  );
  let told = admitted.settle_told(&mut |_| true, (1, u64::MAX)).unwrap();
  assert_eq!(
    (
      told.completed.len(),
      told.waited,
      admitted.seam().notified().len() - before
    ),
    (1, 1, 1),
    "the told settle publishes the write's reply and notifies for it"
  );
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let guest = admitted.seam_mut().guest_mut();
  let (head, len) = guest.reap(rq).expect("the write's reply, once told");
  assert_eq!(
    (head, reply_error(&guest.reply_of(rq, head, len))),
    (write_head, 0),
    "the write's reply, a success"
  );
}

/// Every one of `count` requests on the request queue was answered with a reply larger than a
/// bare header.
fn assert_all_answered(admitted: &mut AdmittedDevice<SimVmm>, count: usize) {
  let guest = admitted.seam_mut().guest_mut();
  let replies: Vec<(u16, u32)> = (0..count)
    .filter_map(|_| guest.reap(usize::from(FIRST_REQUEST_QUEUE)))
    .collect();
  assert_eq!(replies.len(), count, "every request was answered");
  assert!(
    replies
      .iter()
      .all(|(_, len)| *len > u32::try_from(OUT_HEADER_LEN).unwrap())
  );
}

/// A chain whose copy buffers exceed the attachment's byte credit is refused typed before any
/// buffer is accessed, and the refusal faults the device (the VMM resets it).
#[test]
fn a_chain_beyond_the_byte_credit_is_refused_before_access_and_faults_the_device() {
  let mut registry = Attachments::new();
  let mut seam = seam_with_requests(1);
  seam.guest_mut().memory.record_accesses(true);
  let mut admitted = admit(
    request(GuestTransport::InProcess),
    seam,
    config(),
    credits(4, 100),
    rw,
    &mut registry,
  )
  .unwrap();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let wanted = 40 + 16 + u64::from(REPLY_CAP);
  let refused = admitted.service(&mut bridge, &registry).unwrap_err();
  assert_eq!(
    refused,
    ServeError::Device(DeviceError::CreditRefused(CreditError::Exhausted {
      kind: CreditKind::Bytes,
      wanted,
      available: 100,
    }))
  );
  let buffers = admitted
    .seam()
    .guest()
    .writable_of(usize::from(FIRST_REQUEST_QUEUE), 0);
  assert_eq!(
    admitted.seam().guest().memory.accesses_dropped(),
    0,
    "the access log holds every access this test reasons about"
  );
  let accesses = admitted.seam().guest().memory.accesses();
  assert!(
    !accesses
      .iter()
      .any(|a| buffers.iter().any(|b| a.range.overlaps(b))),
    "the refused chain's reply buffer was never touched"
  );
  assert_eq!(
    admitted.service(&mut bridge, &registry).unwrap_err(),
    refused,
    "the fault persists until the driver reconfigures"
  );
}

/// AC-4.12/T-4.14: revoke while requests are pending. Admission stops at once — a later pass is
/// refused before it touches the ring, and the pending chains' buffers keep the driver's fill —
/// and the owned terminal step reclaims: the attachment's references are swept (an unlinked file
/// the guest never forgot is reclaimed), the credits return whole, the attachment can mint no
/// further context, and the seam is released.
#[test]
fn revocation_refuses_before_access_and_the_terminal_step_reclaims() {
  let mut registry = Attachments::new();
  let seam = SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 501 }));
  let mut admitted = admit(
    request(GuestTransport::InProcess),
    seam,
    config(),
    credits(1, 1 << 20),
    rw,
    &mut registry,
  )
  .unwrap();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let probe = common::context();
  let ino = create_and_unlink_orphan(&mut admitted, &mut bridge, &registry);
  assert!(
    bridge.getattr(ObjectId::new(ino, 0), &probe).is_ok(),
    "alive: the guest still references it"
  );
  let pending = publish_pending_getattrs(&mut admitted, 2);
  admitted.revoke();
  assert_revoked_touches_nothing(&mut admitted, &mut bridge, &registry, &pending);
  let reclaimed = admitted.reclaim(&mut bridge, &mut registry).unwrap();
  assert!(reclaimed.references_swept);
  assert_eq!(reclaimed.credits_restored, (1, 1 << 20));
  assert_reclaimed(&mut admitted, &mut bridge, &registry, ino, &probe);
}

/// One pass as the daemon's loop runs it: service, then — when a mutation's reply waits for the owner's
/// barrier (AUD-29-82) — the barrier, which this owner always captures, and the completion. Returns the chains
/// whose used element was published.
fn owner_pass(
  admitted: &mut AdmittedDevice<SimVmm>,
  bridge: &mut VolumeBridge<'_>,
  registry: &Attachments,
) -> u32 {
  let pass = admitted.service(bridge, registry).unwrap();
  if !pass.barrier_owed {
    return pass.served;
  }
  let completed = admitted.complete_barrier(bridge, registry, true).unwrap();
  assert!(completed.is_some_and(|done| !done.refused));
  pass.served + 1
}

/// The guest creates a file and unlinks it without releasing or forgetting it, so only the
/// attachment's references keep the inode alive; returns its number.
fn create_and_unlink_orphan(
  admitted: &mut AdmittedDevice<SimVmm>,
  bridge: &mut VolumeBridge<'_>,
  registry: &Attachments,
) -> u64 {
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  // fuse_create_in: flags, mode, umask, open_flags, then the name.
  let mut create = Vec::new();
  create.extend_from_slice(&2u32.to_le_bytes());
  create.extend_from_slice(&0o644u32.to_le_bytes());
  create.extend_from_slice(&[0u8; 8]);
  create.extend_from_slice(b"orphan\0");
  let head = admitted.seam_mut().guest_mut().submit(
    rq,
    &message(Opcode::Create.to_wire(), 1, 1, &create),
    REPLY_CAP,
    1,
  );
  assert_eq!(owner_pass(admitted, bridge, registry), 1);
  let (id, len) = admitted.seam_mut().guest_mut().reap(rq).unwrap();
  assert_eq!(id, head);
  let created = admitted.seam().guest().reply_of(rq, head, len);
  assert_eq!(reply_error(&created), 0);
  let ino = u64::from_le_bytes(
    created[OUT_HEADER_LEN..OUT_HEADER_LEN + 8]
      .try_into()
      .unwrap(),
  );
  admitted.seam_mut().guest_mut().submit(
    rq,
    &message(Opcode::Unlink.to_wire(), 2, 1, b"orphan\0"),
    REPLY_CAP,
    1,
  );
  assert_eq!(owner_pass(admitted, bridge, registry), 1);
  let _ = admitted.seam_mut().guest_mut().reap(rq);
  ino
}

/// Publishes `count` GETATTR requests the device has not served; returns their reply buffers.
fn publish_pending_getattrs(admitted: &mut AdmittedDevice<SimVmm>, count: u64) -> Vec<GuestRange> {
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  for unique in 0..count {
    admitted.seam_mut().guest_mut().submit(
      rq,
      &message(Opcode::GetAttr.to_wire(), 100 + unique, 1, &[0u8; 16]),
      REPLY_CAP,
      1,
    );
  }
  let pending = admitted.seam().guest().pending_writable(rq);
  assert_eq!(
    pending.len(),
    usize::try_from(count).unwrap(),
    "the chains await service"
  );
  pending
}

/// After revocation a pass is refused before it touches the ring, and every pending reply buffer
/// still holds the driver's fill.
fn assert_revoked_touches_nothing(
  admitted: &mut AdmittedDevice<SimVmm>,
  bridge: &mut VolumeBridge<'_>,
  registry: &Attachments,
  pending: &[GuestRange],
) {
  admitted.seam_mut().guest_mut().memory.record_accesses(true);
  assert_eq!(
    admitted.service(bridge, registry).unwrap_err(),
    ServeError::Revoked
  );
  assert_eq!(
    admitted.seam().guest().memory.accesses_dropped(),
    0,
    "an empty log means nothing happened only if nothing was dropped"
  );
  assert!(
    admitted.seam().guest().memory.accesses().is_empty(),
    "a revoked device touches nothing, not even the ring"
  );
  for range in pending {
    let mut bytes = vec![0u8; usize::try_from(range.len()).unwrap()];
    admitted
      .seam()
      .guest()
      .memory
      .read(*range, &mut bytes)
      .unwrap();
    assert!(
      bytes.iter().all(|b| *b == 0xEE),
      "the pending reply buffer keeps the driver's fill"
    );
  }
}

/// After the terminal step: the credits are whole, the attachment mints no context, the seam is
/// released, the orphan is reclaimed, and service stays refused.
fn assert_reclaimed(
  admitted: &mut AdmittedDevice<SimVmm>,
  bridge: &mut VolumeBridge<'_>,
  registry: &Attachments,
  ino: u64,
  probe: &slates_bridge_core::OpContext,
) {
  assert_eq!(admitted.ledger().in_flight(), (0, 0));
  assert_eq!(
    admitted.context(registry).unwrap_err(),
    VfsError::NotPermitted,
    "the attachment mints no further context"
  );
  assert!(admitted.seam().released(), "the seam was released last");
  assert_eq!(
    bridge.getattr(ObjectId::new(ino, 0), probe).unwrap_err(),
    VfsError::NotFound,
    "the sweep reclaimed the orphan the guest never forgot"
  );
  assert_eq!(
    admitted.service(bridge, registry).unwrap_err(),
    ServeError::Revoked
  );
}

/// The capability report says what is true (§4.6: "supported transport, target-path constraints,
/// read/write policy, sharing/cache semantics, residency boundary and conformance evidence"): the
/// in-process seam is supported, the target is the guest's tag, the policy follows the rights,
/// DAX is not advertised with the contract's reason, and the conformance evidence is the simulated
/// driver — never a claim of a live guest.
#[test]
fn the_capability_report_is_truthful() {
  let mut registry = Attachments::new();
  let seam = SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 0 }));
  let admitted = admit(
    request(GuestTransport::InProcess),
    seam,
    config(),
    roomy(),
    rw,
    &mut registry,
  )
  .unwrap();
  let report = admitted.capability();
  assert_eq!(report.transport, GuestTransport::InProcess);
  assert!(report.supported);
  assert_eq!(
    report.target_path,
    TargetPath::GuestTag {
      tag: "slates".to_owned()
    }
  );
  assert_eq!(report.read_write, ReadWritePolicy::ReadWrite);
  assert!(!report.dax.advertised);
  assert!(
    report.dax.reason.contains("AC-9.7"),
    "the in-process seam advertises no DAX until a live guest has run over it: {}",
    report.dax.reason
  );
  assert_eq!(report.conformance, Conformance::SimulatedGuestDriver);
  assert!(
    !report.sharing.writeback_cache,
    "no INIT yet: nothing negotiated"
  );
}

/// The host-level report (before any attach) names the inherited-descriptor form supported on Linux (the
/// vhost-user binding, AUD-29-68) and unsupported with its reason elsewhere, and the in-process form supported
/// with the tag assigned at attach.
#[test]
fn the_host_report_names_where_the_binding_is_built() {
  let host =
    slates_bridge_virtiofs::capability::host_capability(GuestTransport::InheritedDescriptor);
  let expected = (!cfg!(target_os = "linux")).then_some(UnsupportedReason::BindingNotBuilt);
  assert_eq!(host.supported, expected.is_none());
  assert_eq!(host.unsupported_reason, expected);
  assert_eq!(
    host.dax.advertised,
    cfg!(target_os = "linux"),
    "the vhost-user binding offers DAX where it is built, its windows over per-file backings (A-168)"
  );
  let in_process = slates_bridge_virtiofs::capability::host_capability(GuestTransport::InProcess);
  assert!(in_process.supported);
  assert_eq!(in_process.target_path, TargetPath::GuestTagAssignedAtAttach);
  assert!(!in_process.dax.advertised);
}

/// AUD-29-69. Do: admit a device whose VMM fails at each setup step after the consumer is authenticated —
/// its queues not configured, its memory not mapped (the configuration step), its configuration space not
/// published — and then admit one whose VMM succeeds. Expect: each admission refused with that step's seam
/// error and its seam released, and the owner's barrier over the volume afterwards closes no attachment —
/// the registry as it was before; the successful admission's barrier closes exactly one. Before, each
/// refusal left a live attachment behind (the §7.5 probe: one per refused admission).
#[test]
fn a_refused_admission_leaves_no_attachment_behind() {
  let mut registry = Attachments::new();
  for (step, expected) in [
    (SeamCall::Queues, SeamError::QueuesUnavailable),
    (SeamCall::Memory, SeamError::MemoryUnavailable),
    (SeamCall::Publish, SeamError::PublishRefused),
  ] {
    let seam = SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 501 })).failing(step);
    let refused = admit(
      request(GuestTransport::InProcess),
      seam,
      config(),
      roomy(),
      rw,
      &mut registry,
    )
    .unwrap_err();
    assert_eq!(refused.error, AdmissionError::Seam(expected), "{step:?}");
    assert!(refused.seam.released(), "{step:?}: the seam was released");
    let barrier = registry.barrier(vid()).unwrap();
    assert!(
      barrier.closed.is_empty(),
      "{step:?}: no attachment left behind for the barrier to close"
    );
  }
  let admitted = admit(
    request(GuestTransport::InProcess),
    SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 501 })),
    config(),
    roomy(),
    rw,
    &mut registry,
  );
  assert!(admitted.is_ok());
  assert_eq!(registry.barrier(vid()).unwrap().closed.len(), 1);
}

/// An admitted device over a fresh simulated VMM in `registry`.
fn admitted_in(registry: &mut Attachments) -> AdmittedDevice<SimVmm> {
  admit(
    request(GuestTransport::InProcess),
    SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 501 })),
    config(),
    roomy(),
    rw,
    registry,
  )
  .unwrap()
}

/// AUD-29-70. Do: reclaim an admitted device through a bridge over another volume, so its sweep is refused;
/// abandon a second (its volume gone); drop a third without any terminal step. Expect: the refused sweep is
/// named in what was reclaimed, yet the registry attachment, the credits and the seam are released all the
/// same — the owner's barrier closes nothing and the seam saw its release; the abandoned one likewise, its
/// sweep named `VolumeGone`; the dropped one released its seam. Before, a refused sweep returned before the
/// registry was drained and the seam released, and a dropped device released neither.
#[test]
fn the_terminal_step_releases_everything_whatever_the_sweep_did() {
  let mut registry = Attachments::new();
  let mut store = store();
  let mut other = volume(&mut store);
  let mut admitted = admitted_in(&mut registry);
  let elsewhere = slates_db::catalog::VolumeId { bytes: [0xEE; 16] };
  let mut wrong_bridge = VolumeBridge::new(elsewhere, &mut other, &mut store);
  let reclaimed = admitted.reclaim(&mut wrong_bridge, &mut registry).unwrap();
  assert!(!reclaimed.references_swept);
  assert!(
    matches!(reclaimed.sweep_refused, Some(ReclaimError::Sweep(_))),
    "{reclaimed:?}"
  );
  assert!(
    admitted.seam().released(),
    "the seam was released after the refused sweep"
  );
  assert!(
    registry.barrier(vid()).unwrap().closed.is_empty(),
    "no attachment stranded"
  );

  let mut abandoned = admitted_in(&mut registry);
  let gone = abandoned.abandon(&mut registry).unwrap();
  assert_eq!(gone.sweep_refused, Some(ReclaimError::VolumeGone));
  assert!(abandoned.seam().released());
  assert!(registry.barrier(vid()).unwrap().closed.is_empty());

  let dropped = admitted_in(&mut registry);
  let before = SEAM_RELEASES.with(std::cell::Cell::get);
  drop(dropped);
  assert_eq!(
    SEAM_RELEASES.with(std::cell::Cell::get),
    before + 1,
    "a dropped device released its seam"
  );
}

/// Shape: the passes the backed-route test drives before it judges the device stuck: far more than its fill takes.
#[cfg(unix)]
const DAX_TURNS: usize = 4096;

/// Passes as the shard's device loop does (`serve.rs`, `one_pass`) until the guest's request on the request queue is
/// answered: the backed DAX's turn, then, unless it was busy, a service pass, a reply held for the owner's barrier
/// completed as an owner whose barrier captures. The reply's errno.
#[cfg(unix)]
fn served_through_the_loop(
  admitted: &mut AdmittedDevice<SimVmm>,
  bridge: &mut VolumeBridge<'_>,
  registry: &Attachments,
  slice: u64,
) -> i32 {
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  for _ in 0..DAX_TURNS {
    let turned = admitted.dax_turn(bridge, registry, slice).unwrap();
    if turned.barrier_owed {
      admitted.complete_barrier(bridge, registry, true).unwrap();
    }
    if !turned.busy {
      let pass = admitted.service(bridge, registry).unwrap();
      if pass.barrier_owed {
        admitted.complete_barrier(bridge, registry, true).unwrap();
      }
    }
    if let Some((id, len)) = admitted.seam_mut().guest_mut().reap(rq) {
      return reply_error(&admitted.seam_mut().guest_mut().reply_of(rq, id, len));
    }
  }
  panic!("the request was not answered within {DAX_TURNS} passes");
}

/// §4.6 (A-168, A-195), the backed route through the production entry points, with no VMM: an admitted device whose
/// VMM shares a DAX window maps the file's backing through `AdmittedDevice::dax_turn` and `service`, as the shard's
/// device loop drives them. Do: admit a device offering a cache window; serve FUSE_INIT, then a writable SETUPMAPPING of
/// a page's file over a whole 2 MiB range; write the file through the volume; store through the window and FSYNC; run
/// the terminal step. Expect: the SETUPMAPPING answered once the device mapped the window, which shows the file; the
/// write shown in the window at once, with no pass and no window torn; the guest's store in the volume by the FSYNC's
/// answer; and the terminal step unmapping the window and letting the backing go, so nothing is held or counted after.
#[cfg(unix)]
#[test]
fn an_admitted_devices_window_maps_the_files_backing_through_the_loops_entry_points() {
  use common::{context, dax_init_body, dax_setup_body};
  const ROOT: u64 = 1;
  let range = slates_mem::DAX_RANGE_BYTES;
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let page = rustix::param::page_size().max(1);
  let slice = 2 * u64::try_from(page).unwrap();
  let mut store = store();
  store.shards_backings.set_budget(4 * range);
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x5A; page]).unwrap();
  // The setup stands for an SDK verb, whose publication commits before the guest's first request: the file's inode
  // number is reserved (A-150), so the guest's replies need no barrier for it.
  vol.mark_published(&store);
  let mut registry = Attachments::new();
  let seam =
    SimVmm::new(&QUEUE_SIZES, Ok(Principal::Uid { uid: 0 })).with_dax_window("admit-backed", range);
  let mut admitted = admit(
    request(GuestTransport::InProcess),
    seam,
    config(),
    roomy(),
    rw,
    &mut registry,
  )
  .unwrap();
  assert!(
    admitted.device().dax_offered(),
    "the admitted device offers DAX: the seam's cache window was installed"
  );
  let mut handles = slates_bridge_core::volume_bridge::new_handle_store();
  let mut bridge = VolumeBridge::attached(vid(), &mut vol, &mut store, &mut handles, None);
  let guest = |admitted: &mut AdmittedDevice<SimVmm>, request: Vec<u8>| {
    admitted
      .seam_mut()
      .guest_mut()
      .submit(rq, &request, 4096, 1);
  };
  guest(
    &mut admitted,
    message(Opcode::Init.to_wire(), 1, ROOT, &dax_init_body()),
  );
  assert_eq!(
    served_through_the_loop(&mut admitted, &mut bridge, &registry, slice),
    0,
    "INIT served"
  );
  guest(
    &mut admitted,
    message(
      Opcode::SetupMapping.to_wire(),
      2,
      no.0,
      &dax_setup_body(0, range, true, 0),
    ),
  );
  assert_eq!(
    served_through_the_loop(&mut admitted, &mut bridge, &registry, slice),
    0,
    "SETUPMAPPING answered once the device mapped the window"
  );
  assert_eq!(
    admitted.device().dax_window_bytes(0, 4),
    Some(vec![0x5A; 4]),
    "the window shows the file"
  );
  bridge
    .write(ObjectId::new(no.0, 0), &context(), 0, b"another")
    .unwrap();
  assert_eq!(
    (
      admitted.device().dax_window_bytes(0, 7),
      admitted.device().dax_window_count()
    ),
    (Some(b"another".to_vec()), 1),
    "another path's write shows in the window at once, and no window is torn"
  );
  assert!(
    admitted.dax_window_store(16, b"the guest's store"),
    "the guest stores through its window"
  );
  let mut fsync = 0u64.to_le_bytes().to_vec();
  fsync.extend_from_slice(&[0u8; 8]);
  guest(
    &mut admitted,
    message(Opcode::FSync.to_wire(), 3, no.0, &fsync),
  );
  assert_eq!(
    served_through_the_loop(&mut admitted, &mut bridge, &registry, slice),
    0,
    "FSYNC answered"
  );
  let mut read = Vec::new();
  bridge
    .read(ObjectId::new(no.0, 0), &context(), 16, 17, &mut read)
    .unwrap();
  assert_eq!(
    read,
    b"the guest's store".to_vec(),
    "the guest's store is in the volume by the FSYNC's answer"
  );
  admitted.reclaim(&mut bridge, &mut registry).unwrap();
  drop(bridge);
  assert_eq!(
    (
      admitted.device().dax_window_count(),
      store.shards_backings.vmms_held()
    ),
    (0, (0, 0)),
    "the terminal step unmapped the window and let the backing go"
  );
}

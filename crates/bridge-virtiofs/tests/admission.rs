//! Device admission and credits proven by use (§4.6 A-9: "Device admission authenticates the
//! consumer before creating a queue, mapping guest memory or publishing a tag. ... In-flight
//! requests, mapped bytes, copy buffers and replies consume the attachment's credits; cancellation
//! and revocation reclaim them under an owned terminal step"; §4.13; AC-4.12/T-4.14: "revoke ...
//! while requests ... are active. Expect refusal before access ... and eventual reclamation"). A
//! simulated VMM seam records the order the device touches it in, so the ordering the contract
//! demands is asserted, not assumed; the simulated guest memory's access log proves refusals came
//! before any access; a real `VolumeBridge` on a scratch volume shows the references reclaimed.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{SeamCall, SimVmm, message, reply_error, store, vid, volume};
use slates_bridge_core::{Attachments, Bridge, ObjectId, VolumeBridge};
use slates_bridge_fuse::abi::{OUT_HEADER_LEN, Opcode};
use slates_bridge_virtiofs::admission::{
  AdmissionError, AdmittedDevice, GuestAttachRequest, GuestTransport, SeamError, ServeError,
  UnsupportedReason, admit,
};
use slates_bridge_virtiofs::capability::{Conformance, ReadWritePolicy, TargetPath};
use slates_bridge_virtiofs::credit::{AttachmentCredits, CreditError, CreditKind};
use slates_bridge_virtiofs::device::{DeviceConfig, DeviceError, FIRST_REQUEST_QUEUE, FsTag};
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

  let inherited = request(GuestTransport::InheritedDescriptor);
  let refused = admit(
    inherited,
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
      transport: GuestTransport::InheritedDescriptor,
      reason: UnsupportedReason::BindingNotBuilt,
    }
  );
  assert!(refused.seam.calls().is_empty());
}

/// Every chain is charged against the attachment's request and byte credits before it is touched
/// and released once its used element is published: a request credit of two serves two of five
/// per pass, the ledger is balanced after each pass, and the guest is notified once per pass.
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
  assert_eq!(ledger.in_flight(), (0, 0), "released on completion");
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
  assert_eq!(admitted.service(bridge, registry).unwrap().served, 1);
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
  assert_eq!(admitted.service(bridge, registry).unwrap().served, 1);
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
    report.dax.reason.contains("mapping isolation"),
    "{}",
    report.dax.reason
  );
  assert_eq!(report.conformance, Conformance::SimulatedGuestDriver);
  assert!(
    !report.sharing.writeback_cache,
    "no INIT yet: nothing negotiated"
  );
}

/// The host-level report (before any attach) names the inherited-descriptor form unsupported with
/// its reason, and the in-process form supported with the tag assigned at attach.
#[test]
fn the_host_report_names_the_unbuilt_binding() {
  let host =
    slates_bridge_virtiofs::capability::host_capability(GuestTransport::InheritedDescriptor);
  assert!(!host.supported);
  assert_eq!(
    host.unsupported_reason,
    Some(UnsupportedReason::BindingNotBuilt)
  );
  let in_process = slates_bridge_virtiofs::capability::host_capability(GuestTransport::InProcess);
  assert!(in_process.supported);
  assert_eq!(in_process.target_path, TargetPath::GuestTagAssignedAtAttach);
  assert!(!in_process.dax.advertised);
}

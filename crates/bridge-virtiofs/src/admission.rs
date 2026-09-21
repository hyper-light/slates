//! Device admission and the attachment's life (§4.6 A-9: "Device admission authenticates the
//! consumer before creating a queue, mapping guest memory or publishing a tag. ... In-flight
//! requests, mapped bytes, copy buffers and replies consume the attachment's credits; cancellation
//! and revocation reclaim them under an owned terminal step"; "Requesting an unsupported form
//! returns `AttachmentUnsupported{transport, reason}`"; §4.13: rights are checked "before resource
//! admission, ... queue creation or any VFS/device effect").
//!
//! The host VMM is a seam ([`VmmSeam`]) with two forms the design names and one trait models: the
//! **in-process** interface (Hecate's libkrun integration, the reference: the VMM runs in this
//! process and calls the device) and an **inherited descriptor** (a vhost-user-style handoff: the
//! memory as descriptors to map, the kick and call as eventfds). Both present the same things —
//! the consumer identity the harness established, the guest memory, the queue layouts the driver
//! configured, a doorbell to wait on and a notification to raise — so the device is written once
//! against the trait; the simulated implementation lives with the tests (the `SimHost` pattern),
//! and the real bindings are the later leg the crate documentation records. This build serves the
//! in-process form; a request for the inherited-descriptor form is refused typed
//! (`AttachmentUnsupported { reason: BindingNotBuilt }`) rather than half-served.
//!
//! [`admit`] is the one way a device comes to exist, and its order is the contract's: the
//! unsupported forms are refused before the seam is touched at all; the consumer is authenticated
//! (`seam.consumer()`, the §4.13 credential — an inherited endpoint's peer, or the in-process
//! harness's own enrollment — never a per-request claim); the attachment is admitted into the
//! **caller's** authority registry ([`Attachments`] — the owner shard's, the one record every
//! transport on the volume rides, so every effect the device makes is checked against it and the
//! owner's barriers close the device's generation with the mounts' (GAP-A9-4); the device keeps only
//! its id, and every service pass and the terminal step take the registry from the caller); only then are the queues read, the memory mapped and the
//! queues validated; and the tag is published last. A failure at any step releases the seam and
//! returns it with the typed error, so nothing is left mapped or half-built.
//!
//! [`AdmittedDevice::revoke`] stops admission at once: no later pass touches the ring. The owned
//! terminal step, [`AdmittedDevice::reclaim`], runs under the still-valid authority: it sweeps the
//! attachment's references (the guest may never send its FORGETs), revokes and drains the
//! attachment so it can mint no further context, returns the credits whole, and releases the seam
//! last. It is idempotent in effect (a second call is a typed `AlreadyReclaimed`), and a sweep that
//! fails leaves the device revoked for a retry rather than silently dropping references.

use std::fmt;

use slates_bridge_core::{AttachmentId, Attachments, Bridge, OpContext, Rights, View};
use slates_bridge_fuse::init::InitNegotiation;
use slates_db::catalog::{Principal, VolumeId};
use slates_vfs::error::VfsError;

use crate::capability::{TransportCapability, attached_capability};
use crate::credit::{AttachmentCredits, CreditLedger};
use crate::device::{Device, DeviceConfig, DeviceError, Serviced};
use crate::memory::GuestMemory;
use crate::virtqueue::QueueLayout;

/// The two forms of the VMM seam (D-2: "an in-process or inherited-descriptor integration seam").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestTransport {
  /// The VMM runs in this process and calls the device (Hecate's libkrun integration).
  InProcess,
  /// The VMM hands the device its memory and doorbells as inherited descriptors.
  InheritedDescriptor,
}

/// How the device learns the driver published buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doorbell {
  /// The VMM calls the device's service pass itself (the in-process form).
  InProcess,
  /// An OS descriptor (an eventfd, or a pipe's read end) that becomes readable on a kick; the
  /// device's loop awaits it through the shard's driver (§4.3) and asks the seam to drain it. The
  /// seam owns the descriptor and keeps it non-blocking; the loop only ever holds its number.
  Descriptor(i32),
}

/// What draining the doorbell found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drained {
  /// One or more kicks were pending and are now consumed.
  Kicked,
  /// Nothing was pending (a spurious wake).
  Nothing,
  /// The VMM closed its end: the guest is gone, which is a revocation.
  HungUp,
}

/// A typed refusal from the seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeamError {
  /// No consumer credential could be established for the guest.
  ConsumerUnverified,
  /// A credential was presented but names no enrolled consumer (§4.13 `ConsumerNotEnrolled`).
  ConsumerNotEnrolled,
  /// The VMM could not map the guest's memory for the device.
  MemoryUnavailable,
  /// The driver has not configured the queues.
  QueuesUnavailable,
  /// The configuration space could not be published.
  PublishRefused,
  /// The used-buffer notification could not be raised.
  NotifyRefused {
    /// The queue.
    queue: u16,
  },
}

impl fmt::Display for SeamError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::ConsumerUnverified => f.write_str("no consumer credential could be established"),
      Self::ConsumerNotEnrolled => f.write_str("the consumer is not enrolled"),
      Self::MemoryUnavailable => f.write_str("the guest memory could not be mapped"),
      Self::QueuesUnavailable => f.write_str("the driver has not configured the queues"),
      Self::PublishRefused => f.write_str("the configuration space could not be published"),
      Self::NotifyRefused { queue } => write!(f, "queue {queue} could not be notified"),
    }
  }
}

impl std::error::Error for SeamError {}

/// What the host VMM presents to the device and takes from it. Every method may refuse typed.
pub trait VmmSeam {
  /// The consumer identity the harness established for this guest (§4.13: delivered through a
  /// capability outside other agents' reach). Asked first; nothing else is touched until it answers.
  fn consumer(&mut self) -> Result<Principal, SeamError>;
  /// The guest's memory, mapped for the device (on this call, never before admission).
  fn memory(&mut self) -> Result<&mut dyn GuestMemory, SeamError>;
  /// The queue layouts the driver configured, in queue order.
  fn queues(&mut self) -> Result<Vec<QueueLayout>, SeamError>;
  /// Publishes the configuration space — the tag and queue count — to the guest.
  fn publish(&mut self, config: &DeviceConfig) -> Result<(), SeamError>;
  /// Raises the used-buffer notification for `queue` (the interrupt, or the call eventfd).
  fn notify_used(&mut self, queue: u16) -> Result<(), SeamError>;
  /// How the device learns of new buffers.
  fn doorbell(&self) -> Doorbell;
  /// Consumes every pending kick on a descriptor doorbell (the eventfd's counter, a pipe's bytes)
  /// without blocking, reporting a hangup; the in-process form has nothing to drain.
  fn drain_doorbell(&mut self) -> Result<Drained, SeamError>;
  /// Tears the guest's view of the device down: unmaps, closes, forgets. The terminal step's last
  /// action, and the cleanup of a failed admission. Idempotent.
  fn release(&mut self);
}

/// Why a requested form is unsupported (the `reason` of `AttachmentUnsupported`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedReason {
  /// DAX was requested; the baseline contract does not require it and it cannot be advertised
  /// until mapping isolation, pinning and teardown are established for the VMM (§4.6 A-9).
  DaxNotEstablished,
  /// A notification queue was requested; `VIRTIO_FS_F_NOTIFICATION` is not offered.
  NotificationQueueNotOffered,
  /// The inherited-descriptor binding (vhost-user / libkrun) is not built in this version.
  BindingNotBuilt,
}

impl UnsupportedReason {
  /// The reason in the contract's words.
  pub const fn reason(self) -> &'static str {
    match self {
      Self::DaxNotEstablished => crate::capability::DAX_NOT_ADVERTISED_REASON,
      Self::NotificationQueueNotOffered => {
        "the notification queue (VIRTIO_FS_F_NOTIFICATION) is not offered by this device"
      }
      Self::BindingNotBuilt => {
        "the inherited-descriptor VMM binding is not built; the in-process seam is the supported form"
      }
    }
  }
}

/// What a caller asks for when it attaches a guest device to a volume. The consumer's rights are
/// not here: they are a function of the authenticated consumer (the volume's access list, §4.13),
/// which [`admit`] takes separately and consults only after the seam has established who the
/// consumer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestAttachRequest {
  /// The seam form.
  pub transport: GuestTransport,
  /// The volume the device serves.
  pub volume: VolumeId,
  /// Whether DAX is requested (refused).
  pub dax: bool,
  /// Whether a notification queue is requested (refused).
  pub notification_queue: bool,
}

/// The closed refusal taxonomy of admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
  /// §4.6: the requested form is not supported here.
  AttachmentUnsupported {
    /// The transport requested.
    transport: GuestTransport,
    /// Why.
    reason: UnsupportedReason,
  },
  /// The seam could not establish the consumer; nothing else was touched.
  ConsumerRefused(SeamError),
  /// The authority registry refused the attachment.
  Authority(VfsError),
  /// The seam refused after authentication (memory, queues, or the tag).
  Seam(SeamError),
  /// The queues the driver configured were refused.
  Device(DeviceError),
}

impl fmt::Display for AdmissionError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::AttachmentUnsupported { transport, reason } => {
        write!(
          f,
          "attachment unsupported over {transport:?}: {}",
          reason.reason()
        )
      }
      Self::ConsumerRefused(e) => write!(f, "consumer refused: {e}"),
      Self::Authority(e) => write!(f, "authority refused: {e:?}"),
      Self::Seam(e) => write!(f, "seam refused: {e}"),
      Self::Device(e) => write!(f, "device refused: {e}"),
    }
  }
}

impl std::error::Error for AdmissionError {}

/// A refused admission: the typed error and the seam, already released, handed back.
#[derive(Debug)]
pub struct AdmissionRefused<S> {
  /// Why.
  pub error: AdmissionError,
  /// The seam, released.
  pub seam: S,
}

/// The closed refusal taxonomy of a service pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServeError {
  /// The device is revoked (or reclaimed): nothing is served and nothing is touched.
  Revoked,
  /// The attachment can mint no context (revoked, drained, or fenced by an epoch).
  Authority(VfsError),
  /// The seam refused (the memory or a notification).
  Seam(SeamError),
  /// The device refused (a malformed chain, a credit, a fault).
  Device(DeviceError),
}

impl fmt::Display for ServeError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Revoked => f.write_str("the device is revoked"),
      Self::Authority(e) => write!(f, "authority refused: {e:?}"),
      Self::Seam(e) => write!(f, "seam refused: {e}"),
      Self::Device(e) => write!(f, "device refused: {e}"),
    }
  }
}

impl std::error::Error for ServeError {}

/// The closed refusal taxonomy of the terminal step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReclaimError {
  /// The device was already reclaimed.
  AlreadyReclaimed,
  /// The attachment could mint no context for the sweep.
  Authority(VfsError),
  /// The sweep refused; the device stays revoked for a retry.
  Sweep(VfsError),
}

impl fmt::Display for ReclaimError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::AlreadyReclaimed => f.write_str("the device was already reclaimed"),
      Self::Authority(e) => write!(f, "authority refused: {e:?}"),
      Self::Sweep(e) => write!(f, "the sweep refused: {e:?}"),
    }
  }
}

impl std::error::Error for ReclaimError {}

/// What the terminal step reclaimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reclaimed {
  /// The attachment's references were swept.
  pub references_swept: bool,
  /// The credits, whole again: `(requests, bytes)`.
  pub credits_restored: (u32, u64),
  /// Requests that were in flight when revocation took effect.
  pub requests_in_flight_at_revoke: u32,
}

/// Where the attachment is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentState {
  /// Admitted; serving.
  Live,
  /// Admission stopped; the terminal step has not run.
  Revoked,
  /// The terminal step has run.
  Reclaimed,
}

/// The admitted device's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdmissionCounters {
  /// Service passes run.
  pub passes: u64,
  /// Used-buffer notifications raised.
  pub notifications: u64,
}

/// A device admitted for one attachment: the seam, the device, the authority record, the credit
/// ledger, and where it is in its life.
pub struct AdmittedDevice<S: VmmSeam> {
  seam: S,
  device: Device,
  attachment: AttachmentId,
  transport: GuestTransport,
  rights: Rights,
  ledger: CreditLedger,
  state: AttachmentState,
  counters: AdmissionCounters,
}

impl<S: VmmSeam> fmt::Debug for AdmittedDevice<S> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("AdmittedDevice")
      .field("device", &self.device)
      .field("state", &self.state)
      .finish()
  }
}

/// Releases `seam` and hands it back with `error`.
fn refuse<S: VmmSeam>(mut seam: S, error: AdmissionError) -> AdmissionRefused<S> {
  seam.release();
  AdmissionRefused { error, seam }
}

/// The unsupported form a request names, if any (checked before the seam is touched).
fn unsupported(request: &GuestAttachRequest) -> Option<UnsupportedReason> {
  if request.transport == GuestTransport::InheritedDescriptor {
    return Some(UnsupportedReason::BindingNotBuilt);
  }
  if request.dax {
    return Some(UnsupportedReason::DaxNotEstablished);
  }
  if request.notification_queue {
    return Some(UnsupportedReason::NotificationQueueNotOffered);
  }
  None
}

/// Maps the memory and validates the queues (the device's configuration), after authentication.
fn configure<S: VmmSeam>(
  seam: &mut S,
  device: &mut Device,
  layouts: &[QueueLayout],
) -> Result<(), AdmissionError> {
  let memory = seam.memory().map_err(AdmissionError::Seam)?;
  device
    .configure(layouts, memory)
    .map_err(AdmissionError::Device)
}

/// Admits a guest device for `request` over `seam`, in the contract's order: unsupported forms are
/// refused untouched; the consumer is authenticated; its rights are read (`rights`, the volume's
/// access list for that consumer, §4.13 — asked only once the consumer is known); the attachment is
/// admitted; the queues are read, the memory mapped and the queues validated; the tag is published.
/// A failure releases the seam and returns it with the typed error.
pub fn admit<S: VmmSeam>(
  request: GuestAttachRequest,
  mut seam: S,
  config: DeviceConfig,
  credits: AttachmentCredits,
  rights: impl FnOnce(&Principal) -> Rights,
  registry: &mut Attachments,
) -> Result<AdmittedDevice<S>, AdmissionRefused<S>> {
  if let Some(reason) = unsupported(&request) {
    return Err(refuse(
      seam,
      AdmissionError::AttachmentUnsupported {
        transport: request.transport,
        reason,
      },
    ));
  }
  let consumer = match seam.consumer() {
    Ok(consumer) => consumer,
    Err(e) => return Err(refuse(seam, AdmissionError::ConsumerRefused(e))),
  };
  let granted = rights(&consumer);
  let attachment = match registry.attach(request.volume, View::Current, consumer, granted) {
    Ok(id) => id,
    Err(e) => return Err(refuse(seam, AdmissionError::Authority(e))),
  };
  let layouts = match seam.queues() {
    Ok(layouts) => layouts,
    Err(e) => return Err(refuse(seam, AdmissionError::Seam(e))),
  };
  let mut device = Device::new(config);
  if let Err(e) = configure(&mut seam, &mut device, &layouts) {
    return Err(refuse(seam, e));
  }
  if let Err(e) = seam.publish(device.config()) {
    return Err(refuse(seam, AdmissionError::Seam(e)));
  }
  Ok(AdmittedDevice {
    seam,
    device,
    attachment,
    transport: request.transport,
    rights: granted,
    ledger: CreditLedger::new(credits),
    state: AttachmentState::Live,
    counters: AdmissionCounters::default(),
  })
}

impl<S: VmmSeam> AdmittedDevice<S> {
  /// The seam.
  pub fn seam(&self) -> &S {
    &self.seam
  }

  /// The seam, mutably (a simulated VMM's guest is driven through it).
  pub fn seam_mut(&mut self) -> &mut S {
    &mut self.seam
  }

  /// The device.
  pub fn device(&self) -> &Device {
    &self.device
  }

  /// The credit ledger.
  pub fn ledger(&self) -> &CreditLedger {
    &self.ledger
  }

  /// Where the attachment is in its life.
  pub fn state(&self) -> AttachmentState {
    self.state
  }

  /// The transport.
  pub fn transport(&self) -> GuestTransport {
    self.transport
  }

  /// The counters.
  pub fn counters(&self) -> AdmissionCounters {
    self.counters
  }

  /// The FUSE connection negotiated so far.
  pub fn negotiated(&self) -> Option<InitNegotiation> {
    self.device.negotiated()
  }

  /// The doorbell the device waits on.
  pub fn doorbell(&self) -> Doorbell {
    self.seam.doorbell()
  }

  /// Drains the doorbell through the seam.
  pub fn drain_doorbell(&mut self) -> Result<Drained, SeamError> {
    self.seam.drain_doorbell()
  }

  /// The authenticated context an operation would run under, or the registry's refusal (after
  /// reclaim, `NotPermitted`).
  pub fn context(&self, registry: &Attachments) -> Result<OpContext, VfsError> {
    registry.context(self.attachment)
  }

  /// The capability report for this attachment (§4.6: what `attach` and `status` say).
  pub fn capability(&self) -> TransportCapability {
    attached_capability(
      self.transport,
      self.device.config().tag.as_str(),
      self.rights,
      self.device.negotiated(),
    )
  }

  /// One service pass over every queue under the attachment's authority and credits: each chain is
  /// charged before it is touched and released when its used element is published; a queue that
  /// completed work and asks for it is notified. Bounded by the request credit per queue. Refused
  /// typed — and nothing touched — once the device is revoked.
  pub fn service(
    &mut self,
    bridge: &mut dyn Bridge,
    registry: &Attachments,
  ) -> Result<Serviced, ServeError> {
    if self.state != AttachmentState::Live {
      return Err(ServeError::Revoked);
    }
    let cx = registry
      .context(self.attachment)
      .map_err(ServeError::Authority)?;
    let batch = self.ledger.credits().requests.get();
    let mut total = Serviced::default();
    let mut to_notify: Vec<u16> = Vec::new();
    {
      let memory = self.seam.memory().map_err(ServeError::Seam)?;
      for queue in 0..self.device.queue_count() {
        let pass = self
          .device
          .service_queue(queue, memory, bridge, &cx, batch, &mut self.ledger)
          .map_err(ServeError::Device)?;
        total.served = total.served.saturating_add(pass.served);
        total.more_pending |= pass.more_pending;
        total.interrupt_wanted |= pass.interrupt_wanted;
        if pass.served > 0 && pass.interrupt_wanted {
          to_notify.push(queue);
        }
      }
    }
    for queue in to_notify {
      self.seam.notify_used(queue).map_err(ServeError::Seam)?;
      self.counters.notifications = self.counters.notifications.saturating_add(1);
    }
    self.counters.passes = self.counters.passes.saturating_add(1);
    Ok(total)
  }

  /// Stops admission now: no later pass touches the ring. Returns the requests in flight at this
  /// moment (none, in this device, which completes each before taking the next). The terminal step
  /// is [`AdmittedDevice::reclaim`].
  pub fn revoke(&mut self) -> u32 {
    if self.state == AttachmentState::Live {
      self.state = AttachmentState::Revoked;
    }
    self.ledger.in_flight().0
  }

  /// The owned terminal step: sweeps the attachment's references through `bridge` under the
  /// still-valid authority, revokes and drains the attachment (no further context), restores the
  /// credits whole, and releases the seam last. Revokes first if the device is still live.
  pub fn reclaim(
    &mut self,
    bridge: &mut dyn Bridge,
    registry: &mut Attachments,
  ) -> Result<Reclaimed, ReclaimError> {
    if self.state == AttachmentState::Reclaimed {
      return Err(ReclaimError::AlreadyReclaimed);
    }
    let in_flight_at_revoke = self.revoke();
    let cx = registry
      .context(self.attachment)
      .map_err(ReclaimError::Authority)?;
    bridge.sweep_attachment(&cx).map_err(ReclaimError::Sweep)?;
    registry.revoke(self.attachment);
    registry.drain(self.attachment);
    let _ = self.ledger.reclaim();
    let credits = self.ledger.credits();
    self.seam.release();
    self.state = AttachmentState::Reclaimed;
    Ok(Reclaimed {
      references_swept: true,
      credits_restored: (credits.requests.get(), credits.bytes.get()),
      requests_in_flight_at_revoke: in_flight_at_revoke,
    })
  }
}

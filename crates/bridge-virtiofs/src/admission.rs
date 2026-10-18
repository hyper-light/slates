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
use crate::device::{Completed, Device, DeviceConfig, DeviceError, Serviced};
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
  /// A DAX cache-region map or unmap was refused: the seam offers no DAX window, the requested range lies outside it,
  /// or the host `mmap`/`munmap` refused. The guest's SETUPMAPPING is answered `EOPNOTSUPP` (it falls back to ordinary
  /// READ/WRITE), and a revoke's teardown that cannot unmap is surfaced, never silently left mapped.
  DaxMapRefused,
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
      Self::DaxMapRefused => f.write_str("the DAX cache-region map/unmap was refused"),
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

  /// The DAX cache region this seam offers (§4.6): the VMM's cache-window descriptor and its length in bytes, or
  /// `None` when the VMM set up none — then DAX cannot be served and is not advertised. The device maps this fd once
  /// to build a [`crate::dax::DaxCacheRegion`] and places content blocks' pages into it with `MAP_FIXED`. Asked once
  /// at admission. Default: none (the in-process form and any seam that has not wired a cache region offer no DAX).
  /// Unix only (the DAX window is a Linux/vhost-user feature).
  #[cfg(unix)]
  fn dax_window(&mut self) -> Option<(std::os::fd::BorrowedFd<'_>, u64)> {
    None
  }

  /// The back-end-request channel a message-based VMM (crosvm/QEMU/cloud-hypervisor) hands over for DAX (§4.6),
  /// with the cache region's size in bytes: admission builds the device's [`crate::dax::DaxPlacer::Message`] from
  /// it. `None` for the fd-sharing form and the default — then message-based DAX is not served. Linux only.
  #[cfg(target_os = "linux")]
  fn dax_message_channel(&mut self) -> Option<(crate::vhost_user::BackendReqChannel, u64)> {
    None
  }
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
  /// The sweep refused.
  Sweep(VfsError),
  /// The volume is gone, so there was nothing to sweep: its references went with it.
  VolumeGone,
}

impl fmt::Display for ReclaimError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::AlreadyReclaimed => f.write_str("the device was already reclaimed"),
      Self::Authority(e) => write!(f, "authority refused: {e:?}"),
      Self::Sweep(e) => write!(f, "the sweep refused: {e:?}"),
      Self::VolumeGone => f.write_str("the volume is gone; its references went with it"),
    }
  }
}

impl std::error::Error for ReclaimError {}

/// What the terminal step reclaimed. The registry attachment, the credits and the seam are released whatever
/// the sweep did (AUD-29-70); `sweep_refused` names a sweep that could not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reclaimed {
  /// The attachment's references were swept.
  pub references_swept: bool,
  /// Why the sweep could not run, when it could not: no context for it, a refused sweep, or a volume gone.
  pub sweep_refused: Option<ReclaimError>,
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
  /// Guest DAX windows torn down by a daemon-initiated revoke (§4.6 step 5): the non-vacuity counter for the
  /// daemon→device coherence channel, so a silently-dead drain cannot pass for a working one. Bumped by
  /// [`AdmittedDevice::request_revoke_inode`], whether called directly or from the per-pass drain in
  /// [`AdmittedDevice::service`].
  pub dax_window_revokes: u64,
}

/// A device admitted for one attachment: the seam, the device, the authority record, the credit
/// ledger, and where it is in its life.
pub struct AdmittedDevice<S: VmmSeam> {
  seam: S,
  device: Device,
  attachment: AttachmentId,
  /// The consumer the seam authenticated, whose revocation ends the device (AUD-29-73).
  consumer: Principal,
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
  // The inherited-descriptor binding is vhost-user (`crate::vhost_user`), built on Linux only.
  if request.transport == GuestTransport::InheritedDescriptor && !cfg!(target_os = "linux") {
    return Some(UnsupportedReason::BindingNotBuilt);
  }
  // The DAX advertisement gate. **Before flipping this, read
  // `docs/bugs/2026-10-18-the-message-based-dax-map-hands-the-vmm-the-whole-arena.md`:** the
  // message-based placer sends the frontend the arena's WHOLE shared-object descriptor, not the
  // authorized block — measured at 64× the window it may map, with another tenant's blocks inside it.
  // `fd_offset`/`len` scope the request, never the descriptor, so with this gate open a VMM's
  // confinement would rest on the frontend honouring them rather than on what slates handed it. The
  // fd-sharing placer does not have this property (the backend maps; the VMM gets no descriptor).
  //
  // That is one of six blocking items. The full list — descriptor confinement, B2's wiring, L2's missing
  // device-side remap, L1's zones, `dax_revoke_block` for landing/takeover, and the destroy ordering — is the
  // gate checklist in `docs/wip/GAPS.md` (2026-10-18, "what must be true before DAX is advertised"). Read it
  // before flipping this, not just the record above.
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
  let attachment = match registry.attach(request.volume, View::Current, consumer.clone(), granted) {
    Ok(id) => id,
    Err(e) => return Err(refuse(seam, AdmissionError::Authority(e))),
  };
  // From here every refusal withdraws the attachment it took (AUD-29-69): a refused admission leaves the
  // registry as it found it — no live attachment for the owner's barrier to close, no slot consumed.
  let layouts = match seam.queues() {
    Ok(layouts) => layouts,
    Err(e) => {
      withdraw(registry, attachment);
      return Err(refuse(seam, AdmissionError::Seam(e)));
    }
  };
  let mut device = Device::new(config);
  if let Err(e) = configure(&mut seam, &mut device, &layouts) {
    withdraw(registry, attachment);
    return Err(refuse(seam, e));
  }
  if let Err(e) = seam.publish(device.config()) {
    withdraw(registry, attachment);
    return Err(refuse(seam, AdmissionError::Seam(e)));
  }
  // Install the DAX cache region on the device from the window the VMM offered (if any), once, now the device is up.
  // The window fd is borrowed only for the mapping; `seam` is moved into the attachment afterwards. On a seam with no
  // window this is a no-op, and SETUPMAPPING stays declined.
  #[cfg(unix)]
  {
    let cache = seam
      .dax_window()
      .and_then(|(fd, len)| crate::dax::DaxCacheRegion::new(fd, len).ok());
    device.install_dax_window(cache);
    // A message-based VMM (crosvm/QEMU) offers no fd-sharing window; it hands over the back-end-request channel,
    // which becomes the device's message placer (§4.6). Only when no fd-sharing window was installed.
    #[cfg(target_os = "linux")]
    if !device.dax_offered()
      && let Some((channel, region)) = seam.dax_message_channel()
    {
      let granule = rustix::param::page_size().max(1);
      device.install_dax_message_placer(crate::vhost_user::MessagePlacer::new(
        channel, granule, region,
      ));
    }
  }
  Ok(AdmittedDevice {
    seam,
    device,
    attachment,
    consumer,
    transport: request.transport,
    rights: granted,
    ledger: CreditLedger::new(credits),
    state: AttachmentState::Live,
    counters: AdmissionCounters::default(),
  })
}

/// Withdraws an attachment an admission took and then refused: revoked, so nothing is admitted under it, and
/// drained, so its slot is free — it never served a request, so nothing is in flight to wait for.
fn withdraw(registry: &mut Attachments, attachment: AttachmentId) {
  registry.revoke(attachment);
  registry.drain(attachment);
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

  /// The consumer the seam authenticated at admission.
  pub fn consumer(&self) -> &Principal {
    &self.consumer
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
        if pass.barrier_owed {
          // A chain waits for the owner's barrier: the pass ends here, and the owner completes it
          // ([`AdmittedDevice::complete_barrier`]) before any later chain is served (AUD-29-82).
          total.barrier_owed = true;
          break;
        }
      }
    }
    for queue in to_notify {
      self.seam.notify_used(queue).map_err(ServeError::Seam)?;
      self.counters.notifications = self.counters.notifications.saturating_add(1);
    }
    self.counters.passes = self.counters.passes.saturating_add(1);
    // §4.6 step 5: before the pass returns, honour any daemon-initiated DAX revokes the bridge enqueued since the
    // last pass (a write, truncate or move of a mapped inode on this shard — [`slates_vfs::volume::Store`] enqueues
    // the inode, and the bridge hands the queue over here). Draining in the same pass that served the guest's own
    // requests tears each now-stale window down under this attachment's authority, with no separate wake channel for
    // the in-process seam the device loop drives itself. Bounded: a mutation enqueues at most one inode, and each
    // inode's live windows are finite. An empty queue — the common case, since DAX is unadvertised — is a no-op.
    for (no, writable_only) in bridge.take_pending_dax_revokes() {
      self.tear_down_inode_windows(no, writable_only, bridge, &cx);
    }
    Ok(total)
  }

  /// Revokes every live DAX window of inode `no` at the **daemon's** request (§4.6 step 5 — the daemon-initiated
  /// counterpart of a guest's REMOVEMAPPING), under the attachment's authority (derived from `registry` as every
  /// service pass derives it). Returns how many windows were torn down, or the registry's refusal if the
  /// attachment's authority is gone. A no-op (`Ok(0)`) once the device is not live: a revoked device's windows are
  /// swept by the terminal step ([`Self::reclaim`]). [`Self::service`] calls the same teardown from its per-pass
  /// drain; this is the explicit verb a daemon (or a test) uses to force a revoke without a service pass.
  pub fn request_revoke_inode(
    &mut self,
    no: u64,
    bridge: &mut dyn Bridge,
    registry: &Attachments,
  ) -> Result<usize, ServeError> {
    if self.state != AttachmentState::Live {
      return Ok(0);
    }
    let cx = registry
      .context(self.attachment)
      .map_err(ServeError::Authority)?;
    Ok(self.tear_down_inode_windows(no, false, bridge, &cx))
  }

  /// Re-validates the device's live DAX windows against its current view and tears any the view no longer backs
  /// (§4.6 "revoke on advance"), under the attachment's authority. A snapshot reader's `advance` kicks its device
  /// here, so its windows onto changed inodes are torn and the guest re-maps against the new view; a device whose
  /// view did not change finds all windows current and tears none, so a volume-wide kick is self-scoping. Returns
  /// how many were torn, or the registry's refusal if the attachment's authority is gone; `Ok(0)` once not live.
  pub fn revalidate_windows(
    &mut self,
    bridge: &mut dyn Bridge,
    registry: &Attachments,
  ) -> Result<usize, ServeError> {
    if self.state != AttachmentState::Live {
      return Ok(0);
    }
    let cx = registry
      .context(self.attachment)
      .map_err(ServeError::Authority)?;
    let torn = self.device.revalidate_windows(bridge, &cx);
    self.counters.dax_window_revokes = self
      .counters
      .dax_window_revokes
      .saturating_add(u64::try_from(torn).unwrap_or(u64::MAX));
    Ok(torn)
  }

  /// Tears down inode `no`'s live windows on the device under the already-derived `cx` and records them in the
  /// non-vacuity counter. `writable_only` false tears every window (a write/truncate makes all stale), true tears
  /// only the writable ones and keeps the read-only (a snapshot shares the block read-only). The shared core of the
  /// per-pass drain and the explicit [`Self::request_revoke_inode`].
  fn tear_down_inode_windows(
    &mut self,
    no: u64,
    writable_only: bool,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
  ) -> usize {
    let torn = if writable_only {
      self.device.request_revoke_writable_inode(no, bridge, cx)
    } else {
      self.device.request_revoke_inode(no, bridge, cx)
    };
    self.counters.dax_window_revokes = self
      .counters
      .dax_window_revokes
      .saturating_add(u64::try_from(torn).unwrap_or(u64::MAX));
    torn
  }

  /// Completes the chain a pass held for the owner's barrier (AUD-29-82): `captured` says whether the owner's
  /// recovery publication covered the volume. Captured, the guest learns its reply; refused, it is answered
  /// `EIO` and the reply's grants are given back. The queue is notified when its driver asks. `Ok(None)` when
  /// no chain is held. Refused typed — and nothing touched — once the device is revoked (the terminal step
  /// owns a held chain then).
  pub fn complete_barrier(
    &mut self,
    bridge: &mut dyn Bridge,
    registry: &Attachments,
    captured: bool,
  ) -> Result<Option<Completed>, ServeError> {
    if self.state != AttachmentState::Live {
      return Err(ServeError::Revoked);
    }
    let cx = registry
      .context(self.attachment)
      .map_err(ServeError::Authority)?;
    let completed = {
      let memory = self.seam.memory().map_err(ServeError::Seam)?;
      self
        .device
        .complete_awaiting(memory, bridge, &cx, captured, &mut self.ledger)
        .map_err(ServeError::Device)?
    };
    if let Some(done) = completed
      && done.interrupt_wanted
    {
      self
        .seam
        .notify_used(done.queue)
        .map_err(ServeError::Seam)?;
      self.counters.notifications = self.counters.notifications.saturating_add(1);
    }
    Ok(completed)
  }

  /// Drains the message-based DAX placer's reply-acks (§4.6) under the attachment's authority: a parked
  /// SETUPMAPPING's ack answers the guest and records the window; a teardown unmap's ack unpins the block (phase
  /// two). The serve loop calls this after awaiting [`AdmittedDevice::dax_ack_raw_fd`]. Notifies the completed
  /// chain's queue when its driver asks; `Ok(None)` when nothing completed or the device is not live. Linux only.
  #[cfg(target_os = "linux")]
  pub fn complete_dax_map(
    &mut self,
    bridge: &mut dyn Bridge,
    registry: &Attachments,
  ) -> Result<Option<Completed>, ServeError> {
    if self.state != AttachmentState::Live {
      return Ok(None);
    }
    let cx = registry
      .context(self.attachment)
      .map_err(ServeError::Authority)?;
    let completed = {
      let memory = self.seam.memory().map_err(ServeError::Seam)?;
      self
        .device
        .complete_dax_awaiting(memory, bridge, &cx, &mut self.ledger)
        .map_err(ServeError::Device)?
    };
    if let Some(done) = completed
      && done.interrupt_wanted
    {
      self
        .seam
        .notify_used(done.queue)
        .map_err(ServeError::Seam)?;
      self.counters.notifications = self.counters.notifications.saturating_add(1);
    }
    Ok(completed)
  }

  /// Whether a message-based SETUPMAPPING is parked for the front end's map-ack; the loop awaits the ack and
  /// completes it before serving more. Linux only.
  #[cfg(target_os = "linux")]
  #[must_use]
  pub fn dax_map_ack_owed(&self) -> bool {
    self.device.dax_map_ack_owed()
  }

  /// The raw descriptor the message-based placer's reply-acks arrive on, for the loop to await; `None` without a
  /// message placer. Linux only.
  #[cfg(target_os = "linux")]
  #[must_use]
  pub fn dax_ack_raw_fd(&self) -> Option<std::os::fd::RawFd> {
    self.device.dax_ack_raw_fd()
  }

  /// Whether the message-based placer has any request still awaiting a reply-ack (a map or a deferred unmap).
  /// Linux only.
  #[cfg(target_os = "linux")]
  #[must_use]
  pub fn dax_outstanding(&self) -> bool {
    self.device.dax_outstanding()
  }

  /// Stops admission now: no later pass touches the ring. Returns the requests in flight at this
  /// moment: none, or the one chain held for the owner's barrier (whose grants the terminal step's sweep
  /// releases and whose charge it restores). The terminal step is [`AdmittedDevice::reclaim`].
  pub fn revoke(&mut self) -> u32 {
    if self.state == AttachmentState::Live {
      self.state = AttachmentState::Revoked;
    }
    self.ledger.in_flight().0
  }

  /// The owned terminal step: sweeps the attachment's references through `bridge` under the
  /// still-valid authority, then — whatever the sweep did — revokes and drains the attachment (no further
  /// context), restores the credits whole, and releases the seam last (AUD-29-70: a refused sweep no longer
  /// strands the registry attachment or the seam). Revokes first if the device is still live. Refused only
  /// when the device was already reclaimed.
  pub fn reclaim(
    &mut self,
    bridge: &mut dyn Bridge,
    registry: &mut Attachments,
  ) -> Result<Reclaimed, ReclaimError> {
    if self.state == AttachmentState::Reclaimed {
      return Err(ReclaimError::AlreadyReclaimed);
    }
    let in_flight_at_revoke = self.revoke();
    let swept = match registry.context(self.attachment) {
      Ok(cx) => {
        // Tear every live DAX window down (revoke, unmap, unpin) before the attachment sweep: a guest that
        // disconnected or crashed without FUSE_DESTROY left its mapped blocks pinned, and only this terminal step
        // still has the bridge to release them (§4.6 teardown). A clean unmount already drained them, so a device
        // with no live window drains nothing here.
        self.device.release_dax(bridge, &cx);
        #[cfg(target_os = "linux")]
        self.device.drain_dax_terminal(bridge, &cx);
        bridge.sweep_attachment(&cx).map_err(ReclaimError::Sweep)
      }
      Err(e) => Err(ReclaimError::Authority(e)),
    };
    Ok(self.release_all(registry, in_flight_at_revoke, swept.err()))
  }

  /// The terminal step for a device whose volume is gone (AUD-29-70): there is no bridge to sweep through, and
  /// nothing to sweep — the references went with the volume — so the registry attachment is revoked and
  /// drained, the credits restored and the seam released, as [`AdmittedDevice::reclaim`] does after its sweep.
  pub fn abandon(&mut self, registry: &mut Attachments) -> Result<Reclaimed, ReclaimError> {
    if self.state == AttachmentState::Reclaimed {
      return Err(ReclaimError::AlreadyReclaimed);
    }
    let in_flight_at_revoke = self.revoke();
    Ok(self.release_all(
      registry,
      in_flight_at_revoke,
      Some(ReclaimError::VolumeGone),
    ))
  }

  /// Releases everything the device holds besides its references: the registry attachment (revoked, then
  /// drained), the credits, and the seam, last; the device is reclaimed.
  fn release_all(
    &mut self,
    registry: &mut Attachments,
    in_flight_at_revoke: u32,
    sweep_refused: Option<ReclaimError>,
  ) -> Reclaimed {
    registry.revoke(self.attachment);
    registry.drain(self.attachment);
    let _ = self.ledger.reclaim();
    let credits = self.ledger.credits();
    self.seam.release();
    self.state = AttachmentState::Reclaimed;
    Reclaimed {
      references_swept: sweep_refused.is_none(),
      sweep_refused,
      credits_restored: (credits.requests.get(), credits.bytes.get()),
      requests_in_flight_at_revoke: in_flight_at_revoke,
    }
  }
}

/// A device dropped without its terminal step (a cancelled serve future, a failed spawn) still releases its
/// seam: the VMM's side never outlives the device. The registry attachment is the owner's, reached only
/// through [`AdmittedDevice::reclaim`] or [`AdmittedDevice::abandon`], which the serve loop always runs.
impl<S: VmmSeam> Drop for AdmittedDevice<S> {
  fn drop(&mut self) {
    if self.state != AttachmentState::Reclaimed {
      self.seam.release();
      self.state = AttachmentState::Reclaimed;
    }
  }
}

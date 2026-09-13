//! The FUSE-over-virtio request cycle (virtio 1.2 §5.11 "File System Device"; §4.6 A-9). A
//! guest's virtio-fs driver places each FUSE request on a virtqueue as one descriptor chain: the
//! device-readable part is the `fuse_in_header` and the request's in-payload, the device-writable
//! part is room for the `fuse_out_header` and the out-payload (§5.11.6, `struct virtio_fs_req`).
//! The device takes a chain ([`Virtqueue::pop`], every check made before access), gathers the
//! readable ranges into one request buffer, runs it through the FUSE codec's dispatch
//! ([`slates_bridge_fuse::bridge::dispatch`]) against the shared [`Bridge`] — the same function the
//! `/dev/fuse` transport calls, so a guest and a host mount get byte-identical semantics — scatters
//! the reply into the writable ranges, and publishes the used element with the bytes written.
//!
//! Queue 0 is the high-priority queue (§5.11.2): `FUSE_FORGET` and `FUSE_BATCH_FORGET` ride it with
//! no writable buffer and no reply (the used element carries length 0), and `FUSE_INTERRUPT` is
//! answered `ENOSYS` by the codec's unserved-opcode path, which is the ABI's "interrupts are not
//! supported" signal (`fs/fuse/dev.c`, `fuse_dev_do_write`: an `ENOSYS` reply to an interrupt sets
//! `no_interrupt`) — correct for a device that completes every request before it takes the next.
//! Queues 1..n are request queues; this device offers one (the derivation is on
//! [`DeviceConfig::new`]). `FUSE_INIT` is negotiated by the codec and observed here so the device
//! knows the connection is up; `FUSE_DESTROY` is served by the codec (it sweeps the attachment's
//! references) and observed here so the device knows the guest unmounted.
//!
//! DAX is not advertised. The codec's negotiation keeps only the flags it wants, and
//! `FUSE_MAP_ALIGNMENT` — the flag a DAX-capable device sets, with `map_alignment` in
//! `fuse_init_out` — is not among them, so a guest that offers DAX gets a reply without it and never
//! sends `FUSE_SETUPMAPPING`. The contract's reason is recorded on the capability report: a DAX
//! capability cannot be advertised until mapping isolation, pinning and teardown are established for
//! the VMM (§4.6 A-9; AC-4.12).
//!
//! Two caps bound every request before access, derived below: the readable bytes from the FUSE
//! connection's negotiated `max_write` (the largest request a guest kernel can send is one full
//! write) and the writable bytes from the largest reply a guest kernel asks for when the daemon
//! leaves `max_pages` at its default (`FUSE_DEFAULT_MAX_PAGES_PER_REQ` pages of the largest guest
//! base page). A chain past either is refused typed by the queue walk. A malformed request the caps
//! cannot catch — shorter than the FUSE header, or a request-queue chain with no room for a reply
//! header — faults the device (the VMM resets it), the same policy as a malformed chain.

use std::fmt;

use slates_bridge_core::{Bridge, OpContext};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::bridge::{EIO, dispatch};
use slates_bridge_fuse::init::{InitNegotiation, MAX_WRITE, negotiate};
use slates_bridge_fuse::reply::ReplyHeader;
use slates_bridge_fuse::request::InHeader;
use slates_machine::{Derived, derived};

use crate::credit::{ChainAdmission, CreditError};
use crate::memory::{GuestMemory, GuestMemoryError, GuestRange};
use crate::virtqueue::{
  ChainCaps, DescriptorChain, QueueLayout, Virtqueue, VirtqueueCounters, VirtqueueError,
};

/// Format: virtio 1.2 §5.11.1 — the file system device's id.
pub const VIRTIO_ID_FS: u32 = 26;
/// Format: §5.11.4 — the configuration's `tag` is 36 bytes: UTF-8, NUL-padded when shorter, not
/// NUL-terminated when it fills the field.
pub const TAG_LEN: usize = 36;
/// Format: §5.11.2 — virtqueue 0 is the high-priority queue.
pub const HIPRIO_QUEUE: u16 = 0;
/// Format: §5.11.2 — the request queues follow the high-priority queue (queues 1..=n when
/// `VIRTIO_FS_F_NOTIFICATION` is not negotiated, which this device does not offer).
pub const FIRST_REQUEST_QUEUE: u16 = 1;
/// Format: `include/uapi/linux/fuse.h` `struct fuse_write_in`: fh (8), offset (8), size (4),
/// write_flags (4), lock_owner (8), flags (4), padding (4) — 40 bytes before the data.
const WRITE_IN_LEN: u64 = 40;
/// Format: `fs/fuse/fuse_i.h` `FUSE_DEFAULT_MAX_PAGES_PER_REQ` — the pages one request may span
/// when the daemon does not set `max_pages` at INIT (slates leaves the kernel default).
const FUSE_DEFAULT_MAX_PAGES_PER_REQ: u64 = 32;
/// Format: the largest base page a Linux guest that mounts virtio-fs runs with — 64 KiB (arm64
/// `CONFIG_ARM64_64K_PAGES`, ppc64 `CONFIG_PPC_64K_PAGES`); x86-64 guests use 4 KiB. The cap must
/// hold for any guest, so it takes the largest.
const LARGEST_GUEST_PAGE: u64 = 64 * 1024;

/// The tag a guest mounts (`mount -t virtiofs <tag> /mnt`), validated for the §5.11.4 field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsTag {
  bytes: [u8; TAG_LEN],
  len: usize,
}

impl FsTag {
  /// A tag from `name`: non-empty, at most [`TAG_LEN`] bytes of UTF-8, no NUL (the field's padding
  /// byte would end the name early).
  pub fn new(name: &str) -> Result<FsTag, DeviceError> {
    if name.is_empty() {
      return Err(DeviceError::TagEmpty);
    }
    if name.len() > TAG_LEN {
      return Err(DeviceError::TagTooLong {
        bytes: name.len(),
        max: TAG_LEN,
      });
    }
    if name.bytes().any(|b| b == 0) {
      return Err(DeviceError::TagHasNul);
    }
    let mut bytes = [0u8; TAG_LEN];
    bytes[..name.len()].copy_from_slice(name.as_bytes());
    Ok(FsTag {
      bytes,
      len: name.len(),
    })
  }

  /// The tag as the configuration space carries it: NUL-padded to the field, not terminated when
  /// it fills it.
  pub fn config_bytes(&self) -> [u8; TAG_LEN] {
    self.bytes
  }

  /// The tag's name.
  pub fn as_str(&self) -> &str {
    std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
  }
}

/// What the device publishes in its configuration space (§5.11.4 `struct virtio_fs_config`):
/// the tag and the number of request queues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceConfig {
  /// The tag the guest mounts.
  pub tag: FsTag,
  /// The request queues offered (`num_request_queues`), at least one (§5.11.5).
  pub num_request_queues: Derived<u32>,
}

impl DeviceConfig {
  /// The configuration for `tag`, with the request-queue count derived: one. The device serves its
  /// volume on the volume's one owning shard (D-7), as one task; a second request queue would be
  /// serviced by that same task in turn and add no parallelism, only a second ring to poll.
  pub fn new(tag: FsTag) -> DeviceConfig {
    DeviceConfig {
      tag,
      num_request_queues: derived!(
        1,
        "one request queue per device task: the volume's one owning shard serves the device (D-7)",
        ["D-7 one owning shard per volume"]
      ),
    }
  }

  /// The queues the driver must configure: the high-priority queue plus the request queues.
  pub fn queue_count(&self) -> usize {
    usize::try_from(self.num_request_queues.get())
      .unwrap_or(usize::MAX)
      .saturating_add(1)
  }
}

/// The readable-bytes cap of one request: the FUSE in-header, `fuse_write_in`, and the negotiated
/// `max_write` — the largest request the guest kernel can send is one full write.
pub fn readable_cap() -> Derived<u64> {
  derived!(
    u64::try_from(IN_HEADER_LEN).unwrap_or(u64::MAX) + WRITE_IN_LEN + u64::from(MAX_WRITE),
    "fuse_in_header + fuse_write_in + max_write (the largest request a guest kernel can send is a full write)",
    ["fuse.in_header_len", "fuse.write_in_len", "fuse.max_write"]
  )
}

/// The writable-bytes cap of one request: the FUSE out-header and the largest READ reply a guest
/// kernel asks for when the daemon leaves `max_pages` at its default — the default page count times
/// the largest guest base page.
pub fn writable_cap() -> Derived<u64> {
  derived!(
    u64::try_from(OUT_HEADER_LEN).unwrap_or(u64::MAX)
      + FUSE_DEFAULT_MAX_PAGES_PER_REQ * LARGEST_GUEST_PAGE,
    "fuse_out_header + FUSE_DEFAULT_MAX_PAGES_PER_REQ × the largest guest base page (the largest READ reply a guest kernel asks for at the default max_pages)",
    [
      "fuse.out_header_len",
      "fuse.default_max_pages_per_req",
      "guest.largest_base_page"
    ]
  )
}

/// The chain caps for a queue of `size`: the descriptor cap is the queue size itself (§2.7.5.2:
/// a chain is never longer than the queue), the byte caps the two derivations above.
pub fn chain_caps(size: u16) -> ChainCaps {
  ChainCaps {
    max_descriptors: size,
    max_readable_bytes: readable_cap().get(),
    max_writable_bytes: writable_cap().get(),
  }
}

/// The device's counters: non-vacuity witnesses for the tests and the status report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceCounters {
  /// Request-queue chains served.
  pub requests_served: u64,
  /// High-priority-queue chains served.
  pub hiprio_served: u64,
  /// Chains that produced no reply (FORGET, BATCH_FORGET): the used element carried length 0.
  pub no_reply: u64,
  /// Request bytes gathered from guest memory.
  pub bytes_gathered: u64,
  /// Reply bytes scattered into guest memory.
  pub bytes_scattered: u64,
  /// Replies the guest's writable buffers could not hold, answered `EIO`.
  pub replies_truncated: u64,
  /// `FUSE_INIT` requests observed.
  pub init_seen: u64,
  /// `FUSE_DESTROY` requests observed.
  pub destroy_seen: u64,
}

/// What one service pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Serviced {
  /// Chains served in the pass.
  pub served: u32,
  /// Whether the driver has published more chains than the pass took (the caller yields and
  /// returns rather than running unbounded).
  pub more_pending: bool,
  /// Whether the driver asks to be interrupted for used buffers (§2.7.7).
  pub interrupt_wanted: bool,
}

/// The device's closed refusal taxonomy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceError {
  /// The tag is empty.
  TagEmpty,
  /// The tag exceeds the configuration field.
  TagTooLong {
    /// The tag's bytes.
    bytes: usize,
    /// The field's size.
    max: usize,
  },
  /// The tag contains a NUL byte.
  TagHasNul,
  /// A service pass was asked for before the driver configured the queues.
  NotConfigured,
  /// The driver configured a different number of queues than the device publishes.
  QueueCountMismatch {
    /// Queues offered by the driver.
    offered: usize,
    /// Queues the device requires.
    required: usize,
  },
  /// The queue index names no queue.
  QueueIndexOutOfRange {
    /// The index.
    queue: u16,
    /// The queues configured.
    queues: u16,
  },
  /// A chain's readable part is shorter than the FUSE in-header.
  RequestTooShort {
    /// The queue.
    queue: u16,
    /// The readable bytes.
    readable: u64,
    /// The bytes a header needs.
    need: u64,
  },
  /// A request-queue chain offers no room for the FUSE out-header.
  ReplyBufferTooSmall {
    /// The queue.
    queue: u16,
    /// The writable bytes.
    writable: u64,
    /// The bytes a header needs.
    need: u64,
  },
  /// A copy the host cannot address (cannot happen under the caps; carried typed regardless).
  BufferTooLarge {
    /// The bytes.
    bytes: u64,
  },
  /// A chain the attachment's credits refused (§4.6 A-9), before it was consumed or touched.
  CreditRefused(CreditError),
  /// A queue refusal.
  Virtqueue(VirtqueueError),
  /// A guest-memory refusal.
  Memory(GuestMemoryError),
}

impl fmt::Display for DeviceError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::TagEmpty => f.write_str("the tag is empty"),
      Self::TagTooLong { bytes, max } => {
        write!(f, "the tag is {bytes} bytes; the field holds {max}")
      }
      Self::TagHasNul => f.write_str("the tag contains a NUL byte"),
      Self::NotConfigured => f.write_str("the driver has not configured the queues"),
      Self::QueueCountMismatch { offered, required } => {
        write!(
          f,
          "the driver configured {offered} queues; the device requires {required}"
        )
      }
      Self::QueueIndexOutOfRange { queue, queues } => {
        write!(f, "queue {queue} does not exist ({queues} configured)")
      }
      Self::RequestTooShort {
        queue,
        readable,
        need,
      } => write!(
        f,
        "queue {queue}: {readable} readable bytes, a FUSE header needs {need}"
      ),
      Self::ReplyBufferTooSmall {
        queue,
        writable,
        need,
      } => write!(
        f,
        "queue {queue}: {writable} writable bytes, a FUSE reply header needs {need}"
      ),
      Self::BufferTooLarge { bytes } => write!(f, "a {bytes}-byte copy is not addressable"),
      Self::CreditRefused(e) => write!(f, "credit refused: {e}"),
      Self::Virtqueue(e) => write!(f, "virtqueue: {e}"),
      Self::Memory(e) => write!(f, "guest memory: {e}"),
    }
  }
}

impl std::error::Error for DeviceError {}

impl From<VirtqueueError> for DeviceError {
  fn from(e: VirtqueueError) -> Self {
    Self::Virtqueue(e)
  }
}

impl From<GuestMemoryError> for DeviceError {
  fn from(e: GuestMemoryError) -> Self {
    Self::Memory(e)
  }
}

/// The virtio-fs device: its configuration, the queues the driver configured, the reusable copy
/// buffers, the negotiated FUSE connection, the fault, and the counters.
pub struct Device {
  config: DeviceConfig,
  queues: Vec<Virtqueue>,
  request: Vec<u8>,
  reply: Vec<u8>,
  negotiated: Option<InitNegotiation>,
  fault: Option<DeviceError>,
  counters: DeviceCounters,
}

impl fmt::Debug for Device {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Device")
      .field("tag", &self.config.tag.as_str())
      .field("queues", &self.queues.len())
      .field("negotiated", &self.negotiated.is_some())
      .field("fault", &self.fault)
      .finish()
  }
}

impl Device {
  /// A device publishing `config`, with no queues until the driver configures them.
  pub fn new(config: DeviceConfig) -> Device {
    Device {
      config,
      queues: Vec::new(),
      request: Vec::new(),
      reply: Vec::new(),
      negotiated: None,
      fault: None,
      counters: DeviceCounters::default(),
    }
  }

  /// The configuration space.
  pub fn config(&self) -> &DeviceConfig {
    &self.config
  }

  /// Installs the driver's queue layouts — exactly the high-priority queue and the request queues
  /// the configuration publishes, in queue order — validating each (the ring walk's configuration
  /// checks) before any queue exists. Configuring is a reset: the FUSE connection and any fault are
  /// cleared; the counters are kept.
  pub fn configure(
    &mut self,
    layouts: &[QueueLayout],
    memory: &dyn GuestMemory,
  ) -> Result<(), DeviceError> {
    let required = self.config.queue_count();
    if layouts.len() != required {
      return Err(DeviceError::QueueCountMismatch {
        offered: layouts.len(),
        required,
      });
    }
    let mut queues = Vec::with_capacity(required);
    for layout in layouts {
      queues.push(Virtqueue::new(*layout, chain_caps(layout.size), memory)?);
    }
    self.queues = queues;
    self.negotiated = None;
    self.fault = None;
    Ok(())
  }

  /// The FUSE connection the guest negotiated, once `FUSE_INIT` has been served.
  pub fn negotiated(&self) -> Option<InitNegotiation> {
    self.negotiated
  }

  /// The fault that stopped the device, if one has.
  pub fn fault(&self) -> Option<&DeviceError> {
    self.fault.as_ref()
  }

  /// The counters.
  pub fn counters(&self) -> DeviceCounters {
    self.counters
  }

  /// The queues configured.
  pub fn queue_count(&self) -> u16 {
    u16::try_from(self.queues.len()).unwrap_or(u16::MAX)
  }

  /// A queue's counters, if it exists.
  pub fn queue_counters(&self, queue: u16) -> Option<VirtqueueCounters> {
    self.queues.get(usize::from(queue)).map(Virtqueue::counters)
  }

  /// Serves up to `batch` chains available on `queue`, in ring order, each through `bridge` under
  /// `cx`: peek (the walk validates the chain), admit (the attachment's credits are charged, before
  /// any buffer is touched), advance, gather, dispatch, scatter, publish, complete (the charge is
  /// released). Bounded by `batch` so a pass never runs past the shard's step budget (§4.3 "bounded
  /// work everywhere"); `more_pending` tells the caller to come back. A malformed request, or a
  /// chain the credits refuse, faults the device: the refusal is returned now and on every later
  /// pass until the driver reconfigures.
  pub fn service_queue(
    &mut self,
    queue: u16,
    memory: &mut dyn GuestMemory,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    batch: u32,
    admission: &mut dyn ChainAdmission,
  ) -> Result<Serviced, DeviceError> {
    if let Some(fault) = &self.fault {
      return Err(fault.clone());
    }
    let index = self.queue_index(queue)?;
    let mut served: u32 = 0;
    while served < batch {
      let Some(chain) = self.queues[index].peek(memory)? else {
        break;
      };
      if let Err(refusal) = admission.admit(&chain) {
        return Err(self.record_fault(DeviceError::CreditRefused(refusal)));
      }
      self.queues[index].advance();
      let written = match self.serve_chain(queue, &chain, memory, bridge, cx) {
        Ok(written) => written,
        Err(refusal) => return Err(self.record_fault(refusal)),
      };
      self.queues[index].push_used(memory, &chain, written)?;
      admission.complete(&chain, written);
      served = served.saturating_add(1);
    }
    Ok(Serviced {
      served,
      more_pending: self.queues[index].pending(memory)? > 0,
      interrupt_wanted: self.queues[index].interrupts_wanted(memory)?,
    })
  }

  /// Records the fault that stops the device and hands the refusal back.
  fn record_fault(&mut self, refusal: DeviceError) -> DeviceError {
    self.fault = Some(refusal.clone());
    refusal
  }

  /// The position of `queue` in the configured queues, or the typed refusal.
  fn queue_index(&self, queue: u16) -> Result<usize, DeviceError> {
    if self.queues.is_empty() {
      return Err(DeviceError::NotConfigured);
    }
    let index = usize::from(queue);
    if index >= self.queues.len() {
      return Err(DeviceError::QueueIndexOutOfRange {
        queue,
        queues: self.queue_count(),
      });
    }
    Ok(index)
  }

  /// Serves one chain: the FUSE request is gathered, dispatched and its reply scattered; returns
  /// the bytes written into the chain's writable buffers (the used element's length).
  fn serve_chain(
    &mut self,
    queue: u16,
    chain: &DescriptorChain,
    memory: &mut dyn GuestMemory,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
  ) -> Result<u32, DeviceError> {
    let hiprio = queue == HIPRIO_QUEUE;
    let header_len = u64::try_from(IN_HEADER_LEN).unwrap_or(u64::MAX);
    if chain.readable_bytes < header_len {
      return Err(DeviceError::RequestTooShort {
        queue,
        readable: chain.readable_bytes,
        need: header_len,
      });
    }
    let reply_header_len = u64::try_from(OUT_HEADER_LEN).unwrap_or(u64::MAX);
    if !hiprio && chain.writable_bytes < reply_header_len {
      return Err(DeviceError::ReplyBufferTooSmall {
        queue,
        writable: chain.writable_bytes,
        need: reply_header_len,
      });
    }
    gather(
      memory,
      &chain.readable,
      chain.readable_bytes,
      &mut self.request,
    )?;
    let reply_room =
      usize::try_from(chain.writable_bytes).map_err(|_| DeviceError::BufferTooLarge {
        bytes: chain.writable_bytes,
      })?;
    self.reply.clear();
    self.reply.resize(reply_room, 0);
    let header = InHeader::parse(&self.request).ok();
    self.observe(header.as_ref());
    let mut written = dispatch(&self.request, bridge, cx, &mut self.reply);
    if written == 0 && expects_reply(header.as_ref()) && reply_room >= OUT_HEADER_LEN {
      // The reply did not fit the guest's buffers (a driver posted less room than the ABI's reply
      // needs): answer the request with EIO rather than leaving it waiting forever.
      written = header
        .map(|h| ReplyHeader::write_error(h.unique, EIO, &mut self.reply).unwrap_or(0))
        .unwrap_or(0);
      self.counters.replies_truncated = self.counters.replies_truncated.saturating_add(1);
    }
    scatter(memory, &chain.writable, &self.reply[..written])?;
    self.count(hiprio, chain.readable_bytes, written);
    Ok(u32::try_from(written).unwrap_or(u32::MAX))
  }

  /// Records what the request was, when it is one the device tracks (INIT, DESTROY).
  fn observe(&mut self, header: Option<&InHeader>) {
    match header.and_then(|h| Opcode::from_wire(h.opcode)) {
      Some(Opcode::Init) => {
        self.negotiated = negotiate(&self.request[IN_HEADER_LEN..]).ok();
        self.counters.init_seen = self.counters.init_seen.saturating_add(1);
      }
      Some(Opcode::Destroy) => {
        self.counters.destroy_seen = self.counters.destroy_seen.saturating_add(1);
      }
      _ => {}
    }
  }

  fn count(&mut self, hiprio: bool, gathered: u64, written: usize) {
    let counters = &mut self.counters;
    if hiprio {
      counters.hiprio_served = counters.hiprio_served.saturating_add(1);
    } else {
      counters.requests_served = counters.requests_served.saturating_add(1);
    }
    if written == 0 {
      counters.no_reply = counters.no_reply.saturating_add(1);
    }
    counters.bytes_gathered = counters.bytes_gathered.saturating_add(gathered);
    counters.bytes_scattered = counters
      .bytes_scattered
      .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
  }
}

/// Whether a request of this header expects a reply: every opcode but the forgets.
fn expects_reply(header: Option<&InHeader>) -> bool {
  !matches!(
    header.and_then(|h| Opcode::from_wire(h.opcode)),
    Some(Opcode::Forget | Opcode::BatchForget)
  )
}

/// Copies the chain's readable ranges, in order, into one contiguous request buffer of `total`
/// bytes (the walk proved the ranges lie in guest memory and sum to `total`).
fn gather(
  memory: &dyn GuestMemory,
  ranges: &[GuestRange],
  total: u64,
  into: &mut Vec<u8>,
) -> Result<(), DeviceError> {
  let total_len =
    usize::try_from(total).map_err(|_| DeviceError::BufferTooLarge { bytes: total })?;
  into.clear();
  into.resize(total_len, 0);
  let mut at = 0usize;
  for range in ranges {
    let len = usize::try_from(range.len())
      .map_err(|_| DeviceError::BufferTooLarge { bytes: range.len() })?;
    let end = at.saturating_add(len);
    let Some(slot) = into.get_mut(at..end) else {
      return Err(DeviceError::BufferTooLarge { bytes: total });
    };
    memory.read(*range, slot)?;
    at = end;
  }
  Ok(())
}

/// Copies `bytes` across the chain's writable ranges in order, filling each before the next; the
/// last range written may be partial, and ranges past the reply are left untouched.
fn scatter(
  memory: &mut dyn GuestMemory,
  ranges: &[GuestRange],
  bytes: &[u8],
) -> Result<(), DeviceError> {
  let mut at = 0usize;
  for range in ranges {
    if at >= bytes.len() {
      break;
    }
    let room = usize::try_from(range.len())
      .map_err(|_| DeviceError::BufferTooLarge { bytes: range.len() })?;
    let piece = room.min(bytes.len() - at);
    let target = GuestRange::new(range.start(), u64::try_from(piece).unwrap_or(u64::MAX))?;
    memory.write(target, &bytes[at..at + piece])?;
    at += piece;
  }
  Ok(())
}

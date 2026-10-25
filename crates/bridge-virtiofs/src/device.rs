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
use slates_bridge_fuse::bridge::{
  EIO, dispatch, needs_barrier, reclaim_unreported, reply_error, success_reply_bytes, takes_reply,
};
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
    if let Some(field) = bytes.get_mut(..name.len()) {
      field.copy_from_slice(name.as_bytes());
    }
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
    std::str::from_utf8(self.bytes.get(..self.len).unwrap_or_default()).unwrap_or("")
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
    u64::try_from(IN_HEADER_LEN)
      .unwrap_or(u64::MAX)
      .saturating_add(WRITE_IN_LEN)
      .saturating_add(u64::from(MAX_WRITE)),
    "fuse_in_header + fuse_write_in + max_write (the largest request a guest kernel can send is a full write)",
    ["fuse.in_header_len", "fuse.write_in_len", "fuse.max_write"]
  )
}

/// The writable-bytes cap of one request: the FUSE out-header and the largest READ reply a guest kernel asks
/// for — the `max_pages` **this daemon negotiates** times the largest guest base page.
///
/// It must follow the negotiation, not the kernel's default. Until 2026-10-19 slates left `max_pages` alone and
/// this cap was the default 32 pages; now it offers `MAX_WRITE / page_size` (64 on a 4 KiB host), and a guest
/// with 64 KiB base pages could ask for 64 × 64 KiB where the old cap allowed 32 × 64 KiB — the device would
/// have faulted the queue on a request its own INIT had invited. The guest's page size is not the daemon's,
/// which is why the largest guest page multiplies here while the daemon's own page size sets the count.
pub fn writable_cap() -> Derived<u64> {
  derived!(
    u64::try_from(OUT_HEADER_LEN)
      .unwrap_or(u64::MAX)
      .saturating_add(
        u64::from(slates_bridge_fuse::init::max_pages()).saturating_mul(LARGEST_GUEST_PAGE)
      ),
    "fuse_out_header + the negotiated max_pages × the largest guest base page (the largest READ reply a guest kernel asks for)",
    [
      "fuse.out_header_len",
      "fuse.negotiated_max_pages",
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
  /// References and handles given back for success replies that could not be scattered into the guest's
  /// buffers (AUD-29-85): what the guest never learned it held.
  pub reclaimed: u64,
  /// Barriers the owner refused: the held chain answered `EIO`, its grants given back (AUD-29-82).
  pub barriers_refused: u64,
  /// Chains whose used element waited for the owner's barrier (AUD-29-82): the non-vacuity counter of the
  /// barrier path.
  pub barriers_awaited: u64,
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
  /// Whether the pass stopped at a chain whose reply waits for the owner's barrier (§4.8, D-18): its effect
  /// is in the volume and its reply in the guest's buffers, but its used element is unpublished until
  /// [`Device::complete_awaiting`] says whether the volume was captured (AUD-29-82).
  pub barrier_owed: bool,
}

/// A chain whose request changed what survives a restart: dispatched, its reply scattered, its used element
/// held back for the owner's barrier (AUD-29-82). The request's opcode and node id name what its reply granted,
/// for a refused barrier's reclaim.
#[derive(Clone, Debug)]
struct AwaitingBarrier {
  queue: u16,
  chain: DescriptorChain,
  written: u32,
  opcode: Option<Opcode>,
  nodeid: u64,
  unique: u64,
}

/// What `serve_dax` did with a FUSE_SETUPMAPPING / FUSE_REMOVEMAPPING (§4.6): not a DAX verb (dispatch answers),
/// a reply already in the out buffer, or — for the message placer's SETUPMAPPING — the window asked for, which the
/// device holds the chain for while the window's backing is filled and its map acknowledged (`crate::backed`).
enum DaxServed {
  /// Not a DAX verb, or no window offered: the FUSE codec's dispatch answers.
  NotHandled,
  /// This many reply bytes are in the out buffer; scatter and publish as usual.
  Replied(usize),
  /// The message placer's SETUPMAPPING was taken: its chain is held for the fill and the front end's ack.
  #[cfg(target_os = "linux")]
  MapHeld {
    /// The request's `unique`, which its reply carries.
    unique: u64,
    /// The window it asks for.
    window: crate::backed::BackedWindow,
    /// Whether the window is sent to the front end once the fill is done.
    sends: bool,
  },
}

/// What `serve_chain` did: a reply of this many bytes was scattered, or the chain is held unanswered.
enum ChainOutcome {
  /// A reply was scattered into the guest's buffers; the used element's length.
  Replied(u32),
  /// The message placer's SETUPMAPPING is held for its window's fill and the front end's ack.
  #[cfg(target_os = "linux")]
  DaxMapHeld,
  /// A request that asks for the guest's stores to reach the volume first is held, unserved, for their ingest.
  #[cfg(target_os = "linux")]
  IngestHeld,
}

/// How the ingest a held request asked for came out (§4.6, [`crate::backed`]): whether it wrote, so the request's reply
/// waits for the owner's barrier as a mutation's does, and the errno of the first write-back the volume refused, which
/// an FSYNC, FLUSH or READ answers in place of its own reply.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct Ingested {
  wrote: bool,
  refused: Option<i32>,
}

/// What one step of the message placer's backed DAX did ([`Device::dax_turn`]).
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DaxTurned {
  /// Whether the pass is to serve no chain: the step did a slice of work, or a held request still waits.
  pub busy: bool,
  /// A held chain the step completed, whose queue is notified when its driver asks.
  pub completed: Option<Completed>,
  /// Whether a request the step served waits for the owner's barrier before its used element is published
  /// ([`Device::complete_awaiting`]), as a mutation's does.
  pub barrier_owed: bool,
}

/// A held chain the owner has completed: the queue it was on and whether that queue's driver asks to be
/// interrupted for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completed {
  /// The queue whose used ring took the element.
  pub queue: u16,
  /// Whether the driver asks to be interrupted for used buffers (§2.7.7).
  pub interrupt_wanted: bool,
  /// Whether the barrier was refused, so the guest was answered `EIO` in place of the reply.
  pub refused: bool,
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
  /// Two of the driver's queues share ring bytes (AUD-29-71): a used-ring publication on one would overwrite
  /// the other's protocol state.
  QueuesOverlap {
    /// The first queue.
    first: u16,
    /// The second queue.
    second: u16,
  },
  /// The driver configured a different number of queues than the device publishes.
  QueueCountMismatch {
    /// Queues offered by the driver.
    offered: usize,
    /// Queues the device requires.
    required: usize,
  },
  /// The message-based DAX back-end-request channel refused or closed while completing a map/unmap ack.
  #[cfg(target_os = "linux")]
  DaxPlacement,
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
      #[cfg(target_os = "linux")]
      Self::DaxPlacement => f.write_str("the DAX back-end-request channel refused or closed"),
      Self::TagTooLong { bytes, max } => {
        write!(f, "the tag is {bytes} bytes; the field holds {max}")
      }
      Self::TagHasNul => f.write_str("the tag contains a NUL byte"),
      Self::NotConfigured => f.write_str("the driver has not configured the queues"),
      Self::QueuesOverlap { first, second } => {
        write!(f, "queues {first} and {second} share ring bytes")
      }
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
  awaiting: Option<AwaitingBarrier>,
  counters: DeviceCounters,
  /// How the device places content blocks into the guest's DAX window ([`crate::dax::DaxPlacer`]), installed once at
  /// admission ([`Device::install_dax_window`]); `None` when the VMM offered no window (then FUSE_SETUPMAPPING is
  /// declined and DAX is not advertised). The fd-sharing form (`Local`) overlays blocks into a cache region the device
  /// owns (§4.6); the message-based form is its sibling (Piece C). Unix only (the DAX window is a Linux/vhost-user feature).
  #[cfg(unix)]
  dax_placer: Option<crate::dax::DaxPlacer>,
  /// The fd-sharing placer's live DAX windows over pinned content blocks, so REMOVEMAPPING (and the terminal teardown)
  /// finds each mapping to revoke and its window range to unmap. Bounded by the cache region's pages; empty when no
  /// window is offered or the message placer serves.
  #[cfg(unix)]
  dax_windows: Vec<DaxWindowEntry>,
  /// The message placer's windows over per-file backings, its backed session, and the request it holds (§4.6,
  /// [`crate::backed`]). Empty unless the message placer serves.
  #[cfg(target_os = "linux")]
  backed: crate::backed::BackedDax,
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
      awaiting: None,
      counters: DeviceCounters::default(),
      #[cfg(unix)]
      dax_placer: None,
      #[cfg(unix)]
      dax_windows: Vec::new(),
      #[cfg(target_os = "linux")]
      backed: crate::backed::BackedDax::default(),
    }
  }

  /// The configuration space.
  pub fn config(&self) -> &DeviceConfig {
    &self.config
  }

  /// Installs the fd-sharing DAX cache region the VMM offered (§4.6) as the device's placer
  /// ([`crate::dax::DaxPlacer::Local`]), once at admission; `None` leaves DAX unserved (SETUPMAPPING stays declined).
  /// From here the device maps content blocks' pages into it per FUSE_SETUPMAPPING. Unix only.
  #[cfg(unix)]
  pub fn install_dax_window(&mut self, cache: Option<crate::dax::DaxCacheRegion>) {
    self.dax_placer = cache.map(crate::dax::DaxPlacer::Local);
  }

  /// Installs a message-based DAX placer (§4.6): the device sends SHMEM_MAP/UNMAP over the front end's
  /// back-end-request channel and completes each on its ack, in place of owning the window. Its windows map per-file
  /// backings ([`crate::backed`]). An out-of-process VMM (crosvm) uses this instead of [`Device::install_dax_window`]; a
  /// device gets one form or neither. Linux only.
  #[cfg(target_os = "linux")]
  pub fn install_dax_message_placer(
    &mut self,
    placer: crate::vhost_user::MessagePlacer<crate::backed::BackedWindow>,
  ) {
    self.dax_placer = Some(crate::dax::DaxPlacer::Message(placer));
  }

  /// Whether a held SETUPMAPPING's map is sent and awaits the front end's ack. Linux only.
  #[cfg(target_os = "linux")]
  #[must_use]
  pub fn dax_map_ack_owed(&self) -> bool {
    self.backed.acking()
  }

  /// What the message placer's backed DAX did since the device began (non-vacuity for tests). Linux only.
  #[cfg(target_os = "linux")]
  #[must_use]
  pub fn dax_backed_counts(&self) -> crate::backed::BackedCounts {
    self.backed.counts
  }

  /// The raw descriptor the message-based placer's reply-acks arrive on, or `None` without a message placer.
  #[cfg(target_os = "linux")]
  #[must_use]
  pub fn dax_ack_raw_fd(&self) -> Option<std::os::fd::RawFd> {
    match &self.dax_placer {
      Some(crate::dax::DaxPlacer::Message(message)) => Some(message.ack_raw_fd()),
      _ => None,
    }
  }

  /// Whether the message-based placer has any request still awaiting a reply-ack. Linux only.
  #[cfg(target_os = "linux")]
  #[must_use]
  pub fn dax_outstanding(&self) -> bool {
    matches!(
      &self.dax_placer,
      Some(crate::dax::DaxPlacer::Message(message)) if message.has_outstanding()
    )
  }

  /// Whether the device serves DAX (the VMM offered a cache region). Unix only.
  #[cfg(unix)]
  #[must_use]
  pub fn dax_offered(&self) -> bool {
    self.dax_placer.is_some()
  }

  /// A copy of the DAX window bytes at `[moffset, moffset+len)` — a diagnostic/test read of what a guest's CPU would
  /// see through its page table (a live guest reads the window directly, not through this). `None` without a window, or
  /// for an out-of-window range. The caller reads only a range it mapped (a `PROT_NONE` hole faults). Unix only.
  #[cfg(unix)]
  #[must_use]
  pub fn dax_window_bytes(&self, moffset: u64, len: u64) -> Option<Vec<u8>> {
    let Some(crate::dax::DaxPlacer::Local(cache)) = self.dax_placer.as_ref() else {
      return None; // no window, or a message-based placer, which keeps no local window bytes
    };
    cache.window_bytes(moffset, len).map(<[u8]>::to_vec)
  }

  /// The live DAX window count, of either placer (non-vacuity for tests). Unix only.
  #[cfg(unix)]
  #[must_use]
  pub fn dax_window_count(&self) -> usize {
    #[cfg(target_os = "linux")]
    let backed = self.backed.windows.len();
    #[cfg(not(target_os = "linux"))]
    let backed = 0;
    self.dax_windows.len().saturating_add(backed)
  }

  /// Tears every live DAX window down (§4.6 terminal teardown). The fd-sharing placer's windows take the two-phase
  /// revoke per mapping, so no block is left pinned; the message placer's are unmapped and their backings let go. The
  /// terminal step of a device whose guest never sent FUSE_DESTROY — a VMM that disconnected or crashed — calls this
  /// with the attachment's bridge, which FUSE_DESTROY does for a clean unmount; the message placer's guest stores have
  /// been ingested before it (the serve loop's end). Idempotent: a device with no live window drains nothing.
  #[cfg(unix)]
  pub fn release_dax(
    &mut self,
    bridge: &mut dyn slates_bridge_core::Bridge,
    cx: &slates_bridge_core::OpContext,
  ) {
    let windows = std::mem::take(&mut self.dax_windows);
    if let Some(crate::dax::DaxPlacer::Local(cache)) = self.dax_placer.as_mut() {
      for entry in &windows {
        teardown_dax_window(cache, bridge, cx, entry);
      }
    }
    #[cfg(target_os = "linux")]
    self.release_backed(bridge);
  }

  /// Unmaps every window over a backing and lets every backing go: the guest unmounted or the device ends. Linux only.
  #[cfg(target_os = "linux")]
  fn release_backed(&mut self, bridge: &mut dyn slates_bridge_core::Bridge) {
    if let Some(crate::dax::DaxPlacer::Message(placer)) = self.dax_placer.as_mut() {
      for window in std::mem::take(&mut self.backed.windows) {
        unmap_backed(placer, &mut self.backed.counts, &window);
      }
    }
    if let Some(state) = bridge.dax_state() {
      self.release_backings(state.backings);
    }
  }

  /// Lets every backing of the message placer go, and its fill, ingest and refill under way, giving their charge back:
  /// the device ends. The terminal step reaches the store's backings through this when the volume itself is gone
  /// ([`crate::admission::AdmittedDevice::abandon`]'s sibling). Linux only.
  #[cfg(target_os = "linux")]
  pub fn release_backings(&mut self, backings: &mut slates_vfs::shards_backing::ShardsBackings) {
    let held = u64::try_from(self.backed.session.len()).unwrap_or(u64::MAX);
    self.backed.session.release_all(backings);
    self.backed.counts.released = self.backed.counts.released.saturating_add(held);
  }

  /// No DAX window exists off Unix, so the terminal teardown drains nothing.
  #[cfg(not(unix))]
  pub fn release_dax(
    &mut self,
    _bridge: &mut dyn slates_bridge_core::Bridge,
    _cx: &slates_bridge_core::OpContext,
  ) {
  }

  /// Revokes every live DAX window of inode `no` at the **daemon's** request (§4.6 step 5 — the daemon-initiated
  /// counterpart of a guest's REMOVEMAPPING). Before the daemon moves, writes, shares, lands or takes over an
  /// inode's content it signals the device here, which munmaps the guest's window and completes the two-phase revoke
  /// (unregister, unmap, then unpin) so the block is unmapped from the guest's page table **before** it is unpinned —
  /// a reused block is never reachable through a stale TLB (the async-revoke hazard §4.6 #1 names). Returns how many
  /// windows were torn down (0 if the inode has none). The daemon holds the inode's pin until this returns, so the
  /// window never outlives the pin. This is the device half; the VFS half (`Store::dax_revoke_inode`) stays the
  /// synchronous unpin for the no-live-window case, and the per-shard channel that routes a daemon revoke to this
  /// (so a `Volume::write`/`truncate`/dir-mutation on a mapped inode calls it) is the remaining integration.
  #[cfg(unix)]
  pub fn request_revoke_inode(
    &mut self,
    no: u64,
    bridge: &mut dyn slates_bridge_core::Bridge,
    cx: &slates_bridge_core::OpContext,
  ) -> usize {
    self.revoke_inode_windows(no, bridge, cx, false)
  }

  /// Revokes only the **writable** live DAX windows of inode `no` at the daemon's request (§4.6): what a snapshot or
  /// clone of the volume needs — a writable mapping of a block about to be shared-frozen must be torn down (its next
  /// write would reach the frozen version), while **read-only** mappings survive (immutable shared content is the
  /// gate's allowed DAX case, and read-only windows cannot corrupt a snapshot). Unlike [`Self::request_revoke_inode`]
  /// (which tears every window, as a write or truncate makes all of them stale), this keeps the inode's read-only
  /// windows. Returns how many writable windows were torn. The device half for the snapshot teardown path; the
  /// synchronous server-side recall that drives it (so `snapshot` tears a live guest's writable windows before it
  /// freezes) is the remaining cross-task integration (`docs/wip/GAPS.md`).
  #[cfg(unix)]
  pub fn request_revoke_writable_inode(
    &mut self,
    no: u64,
    bridge: &mut dyn slates_bridge_core::Bridge,
    cx: &slates_bridge_core::OpContext,
  ) -> usize {
    self.revoke_inode_windows(no, bridge, cx, true)
  }

  /// Tears down inode `no`'s live DAX windows — every one, or only the writable ones when `writable_only` — munmap
  /// then unpin (the two-phase revoke), so each block is unmapped from the guest's page table before it is unpinned
  /// (the async-revoke stale-TLB hazard §4.6 #1). The daemon holds each inode pin until this returns, so a window
  /// never outlives its pin. Returns how many were torn.
  #[cfg(unix)]
  fn revoke_inode_windows(
    &mut self,
    no: u64,
    bridge: &mut dyn slates_bridge_core::Bridge,
    cx: &slates_bridge_core::OpContext,
    writable_only: bool,
  ) -> usize {
    // A window over a backing is kept equal to the file by the write boundary, so nothing here revokes one: these are
    // the fd-sharing placer's windows over pinned blocks alone.
    let mut torn = 0usize;
    if let Some(crate::dax::DaxPlacer::Local(cache)) = self.dax_placer.as_mut() {
      let mut index = 0;
      while index < self.dax_windows.len() {
        match self.dax_windows.get(index).copied() {
          Some(entry)
            if entry.mapping.inode.0 == no && (!writable_only || entry.mapping.writable) =>
          {
            teardown_dax_window(cache, bridge, cx, &entry);
            self.dax_windows.swap_remove(index);
            torn = torn.saturating_add(1);
          }
          _ => index = index.saturating_add(1),
        }
      }
    }
    torn
  }

  /// Re-validates every live DAX window against the device's **current** view and tears any the view no longer backs
  /// (§4.6 "revoke on advance"): after a snapshot reader advances, its windows onto inodes whose content changed
  /// name blocks the new view does not hold, so [`slates_bridge_core::Bridge::dax_window_current`] fails and the
  /// two-phase revoke (munmap then unpin) tears them — the guest re-maps against the new view — while windows onto
  /// unchanged, block-shared content stay current and survive. Unlike [`Self::request_revoke_inode`] (a write's
  /// inode-global teardown across every reader), this is the moving reader's own device re-checking its own windows,
  /// so no other reader's are touched. Returns how many were torn.
  ///
  /// The message placer's windows are never torn: each maps its file's backing, and the view's move marks every
  /// backing stale, so each is refilled from the new view before the guest sees anything more (`Device::dax_turn`).
  #[cfg(unix)]
  pub fn revalidate_windows(
    &mut self,
    bridge: &mut dyn slates_bridge_core::Bridge,
    cx: &slates_bridge_core::OpContext,
  ) -> usize {
    let mut torn = 0usize;
    match self.dax_placer.as_mut() {
      Some(crate::dax::DaxPlacer::Local(cache)) => {
        let mut index = 0;
        while index < self.dax_windows.len() {
          match self.dax_windows.get(index).copied() {
            Some(entry) if !bridge.dax_window_current(&entry.mapping, cx) => {
              teardown_dax_window(cache, bridge, cx, &entry);
              self.dax_windows.swap_remove(index);
              torn = torn.saturating_add(1);
            }
            _ => index = index.saturating_add(1),
          }
        }
      }
      #[cfg(target_os = "linux")]
      Some(crate::dax::DaxPlacer::Message(_)) => {
        if let Some(state) = bridge.dax_state() {
          self.backed.session.mark_all_stale(state.backings);
        }
      }
      None => {}
    }
    torn
  }

  /// No DAX window exists off Unix, so a daemon-initiated revoke tears nothing down.
  #[cfg(not(unix))]
  pub fn request_revoke_inode(
    &mut self,
    _no: u64,
    _bridge: &mut dyn slates_bridge_core::Bridge,
    _cx: &slates_bridge_core::OpContext,
  ) -> usize {
    0
  }

  /// No DAX window exists off Unix, so a daemon-initiated writable revoke tears nothing down.
  #[cfg(not(unix))]
  pub fn request_revoke_writable_inode(
    &mut self,
    _no: u64,
    _bridge: &mut dyn slates_bridge_core::Bridge,
    _cx: &slates_bridge_core::OpContext,
  ) -> usize {
    0
  }

  /// No DAX window exists off Unix, so an advance re-validate tears nothing down.
  #[cfg(not(unix))]
  pub fn revalidate_windows(
    &mut self,
    _bridge: &mut dyn slates_bridge_core::Bridge,
    _cx: &slates_bridge_core::OpContext,
  ) -> usize {
    0
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
    // The device's queues are validated together (AUD-29-71): no ring of one queue may share a byte with a
    // ring of another, and each queue refuses a buffer aliasing any other queue's rings as it refuses one
    // aliasing its own.
    for (first, queue) in queues.iter().enumerate() {
      for (second, other) in queues.iter().enumerate().skip(first.saturating_add(1)) {
        if queue
          .rings()
          .iter()
          .any(|ring| other.rings().iter().any(|o| ring.overlaps(o)))
        {
          return Err(DeviceError::QueuesOverlap {
            first: u16::try_from(first).unwrap_or(u16::MAX),
            second: u16::try_from(second).unwrap_or(u16::MAX),
          });
        }
      }
    }
    let all: Vec<[GuestRange; 3]> = queues.iter().map(|q| *q.rings()).collect();
    for (index, queue) in queues.iter_mut().enumerate() {
      let foreign = all
        .iter()
        .enumerate()
        .filter(|(other, _)| *other != index)
        .flat_map(|(_, rings)| rings.iter().copied())
        .collect();
      queue.guard_foreign_rings(foreign);
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
    if self.awaiting.is_some() {
      // Nothing is served past a held chain: its completion is the owner's next step.
      return Ok(Serviced {
        served: 0,
        more_pending: true,
        interrupt_wanted: false,
        barrier_owed: true,
      });
    }
    #[cfg(target_os = "linux")]
    if self.backed.holds() {
      // A request is held for a DAX fill, an ack, an ingest or a refill: serve nothing until the turn completes it
      // ([`Device::dax_turn`]).
      return Ok(Serviced {
        served: 0,
        more_pending: true,
        interrupt_wanted: false,
        barrier_owed: false,
      });
    }
    let mut served: u32 = 0;
    while served < batch {
      let Some(chain) = self.queue_at(index)?.peek(memory)? else {
        break;
      };
      if let Err(refusal) = self.charge_copy(&chain, admission) {
        return Err(self.record_fault(refusal));
      }
      self.queue_at(index)?.advance();
      let written = match self.serve_chain(
        queue,
        &chain,
        memory,
        bridge,
        cx,
        #[cfg(target_os = "linux")]
        None,
      ) {
        Ok(ChainOutcome::Replied(written)) => written,
        #[cfg(target_os = "linux")]
        Ok(ChainOutcome::DaxMapHeld | ChainOutcome::IngestHeld) => {
          // The chain is held for a fill and an ack, or for an ingest; the pass ends, and the turns complete it.
          return Ok(Serviced {
            served,
            more_pending: true,
            interrupt_wanted: self.queue_at(index)?.interrupts_wanted(memory)?,
            barrier_owed: false,
          });
        }
        Err(refusal) => return Err(self.record_fault(refusal)),
      };
      if let Some(awaiting) =
        self.awaits_barrier(queue, &chain, written, bridge.awaits_publication())
      {
        // The reply is in the guest's buffers, but the guest learns of it only from the used element,
        // which waits for the owner's barrier (§4.8, D-18).
        self.awaiting = Some(awaiting);
        self.counters.barriers_awaited = self.counters.barriers_awaited.saturating_add(1);
        return Ok(Serviced {
          served,
          more_pending: true,
          interrupt_wanted: self.queue_at(index)?.interrupts_wanted(memory)?,
          barrier_owed: true,
        });
      }
      #[cfg(target_os = "linux")]
      if self.refill_owed(bridge) {
        // Serving the request marked a backing stale (the disk changed beneath it): its reply could reveal the change
        // before the guest's map shows it, so its used element waits for the refill.
        self.hold_reply(queue, &chain, written);
        return Ok(Serviced {
          served,
          more_pending: true,
          interrupt_wanted: self.queue_at(index)?.interrupts_wanted(memory)?,
          barrier_owed: false,
        });
      }
      self.queue_at(index)?.push_used(memory, &chain, written)?;
      admission.complete();
      served = served.saturating_add(1);
    }
    Ok(Serviced {
      served,
      more_pending: self.queue_at(index)?.pending(memory)? > 0,
      interrupt_wanted: self.queue_at(index)?.interrupts_wanted(memory)?,
      barrier_owed: false,
    })
  }

  /// The bytes the device's copy buffers keep between chains (their capacity): what its attachment's byte
  /// credit holds charged for it (AUD-29-77).
  pub fn retained_copy_bytes(&self) -> u64 {
    let kept = self
      .request
      .capacity()
      .saturating_add(self.reply.capacity());
    u64::try_from(kept).unwrap_or(u64::MAX)
  }

  /// Charges `chain` before any buffer is touched (AUD-29-77): one request, and the bytes the kept request
  /// and reply buffers must grow by to hold it, then reserves exactly that, so what the buffers keep is what
  /// is charged. When the growth does not fit beside what is kept, the kept buffers are given back and the
  /// chain is charged whole — a large request kept from one chain never refuses a large reply the next needs.
  /// A chain that alone exceeds the credit is refused.
  fn charge_copy(
    &mut self,
    chain: &DescriptorChain,
    admission: &mut dyn ChainAdmission,
  ) -> Result<(), DeviceError> {
    let need =
      |bytes: u64| usize::try_from(bytes).map_err(|_| DeviceError::BufferTooLarge { bytes });
    let (request_need, reply_need) = (need(chain.readable_bytes)?, need(chain.writable_bytes)?);
    let growth = request_need
      .saturating_sub(self.request.capacity())
      .saturating_add(reply_need.saturating_sub(self.reply.capacity()));
    match admission.admit(u64::try_from(growth).unwrap_or(u64::MAX)) {
      Ok(()) => {}
      Err(CreditError::Exhausted { .. }) if self.retained_copy_bytes() > 0 => {
        let kept = self.retained_copy_bytes();
        self.request = Vec::new();
        self.reply = Vec::new();
        admission.return_bytes(kept);
        admission
          .admit(crate::credit::chain_bytes(chain))
          .map_err(DeviceError::CreditRefused)?;
      }
      Err(refusal) => return Err(DeviceError::CreditRefused(refusal)),
    }
    self.request.clear();
    self.request.reserve_exact(request_need);
    self.reply.clear();
    self.reply.reserve_exact(reply_need);
    Ok(())
  }

  /// The held form of a chain just served, when its request changed what survives a restart and succeeded
  /// ([`needs_barrier`]), or when an effect the guest keeps across a restart waits for the volume's next publication
  /// (`awaits_publication`: an unreserved inode number, A-150; a release the committed publication still counts as an
  /// open, A-154); `None` for every other chain (reads, unstable writes, refusals, the high-priority queue's FORGETs).
  fn awaits_barrier(
    &self,
    queue: u16,
    chain: &DescriptorChain,
    written: u32,
    awaits_publication: bool,
  ) -> Option<AwaitingBarrier> {
    let header = InHeader::parse(&self.request).ok()?;
    let opcode = Opcode::from_wire(header.opcode);
    let reply = self
      .reply
      .get(..usize::try_from(written).ok()?)
      .unwrap_or(&[]);
    (needs_barrier(opcode, reply_error(reply)) || awaits_publication).then(|| AwaitingBarrier {
      queue,
      chain: chain.clone(),
      written,
      opcode,
      nodeid: header.nodeid,
      unique: header.unique,
    })
  }

  /// Completes the chain held for the owner's barrier (AUD-29-82): when `captured`, its used element is
  /// published as served; otherwise the guest is answered `EIO` in its place — the effect is in the volume
  /// but not promised to survive, as a refused NFS publication — and what the reply granted is given back
  /// ([`reclaim_unreported`]), since the guest never learns it. The chain's charge is released either way.
  /// `None` when no chain is held.
  pub fn complete_awaiting(
    &mut self,
    memory: &mut dyn GuestMemory,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    captured: bool,
    admission: &mut dyn ChainAdmission,
  ) -> Result<Option<Completed>, DeviceError> {
    let Some(held) = self.awaiting.take() else {
      return Ok(None);
    };
    let index = self.queue_index(held.queue)?;
    let mut written = held.written;
    if !captured {
      let reply = self
        .reply
        .get(..usize::try_from(held.written).unwrap_or(0))
        .unwrap_or(&[]);
      let reclaimed = reclaim_unreported(held.opcode, held.nodeid, reply, bridge, cx);
      self.counters.reclaimed = self
        .counters
        .reclaimed
        .saturating_add(reclaimed.references.saturating_add(reclaimed.handles));
      let mut refusal = [0u8; OUT_HEADER_LEN];
      let n = ReplyHeader::write_error(held.unique, EIO, &mut refusal).unwrap_or(0);
      if let Err(refused) = scatter(
        memory,
        &held.chain.writable,
        refusal.get(..n).unwrap_or(&[]),
      ) {
        return Err(self.record_fault(refused));
      }
      written = u32::try_from(n).unwrap_or(u32::MAX);
      self.counters.barriers_refused = self.counters.barriers_refused.saturating_add(1);
    }
    #[cfg(target_os = "linux")]
    if captured && self.refill_owed(bridge) {
      // The reply could reveal the disk's change before the guest's map shows it: it waits for the refill too.
      self.hold_reply(held.queue, &held.chain, written);
      return Ok(None);
    }
    self
      .queue_at(index)?
      .push_used(memory, &held.chain, written)?;
    admission.complete();
    Ok(Some(Completed {
      queue: held.queue,
      interrupt_wanted: self.queue_at(index)?.interrupts_wanted(memory)?,
      refused: !captured,
    }))
  }

  /// Drains the message placer's reply-acks (§4.6) and acts on each: a held SETUPMAPPING's map-ack records its window
  /// and answers the guest, and a refused map answers `EIO`. An unmap's ack settles nothing, since a window over a
  /// backing holds no pin; a refused one is counted (the window range still maps the backing, which stays equal to the
  /// file). Returns the completed chain when the map-ack landed here, so the caller notifies its queue. The fd-sharing
  /// placer never has acks, so this is Linux only.
  #[cfg(target_os = "linux")]
  pub fn complete_dax_awaiting(
    &mut self,
    memory: &mut dyn GuestMemory,
    admission: &mut dyn ChainAdmission,
  ) -> Result<Option<Completed>, DeviceError> {
    // Drain the acks into a list first, releasing the placer borrow before touching windows/queues/reply.
    let completions = match self.dax_placer.as_mut() {
      Some(crate::dax::DaxPlacer::Message(message)) => {
        message.poll_acks().map_err(|_| DeviceError::DaxPlacement)?
      }
      _ => return Ok(None),
    };
    let mut completed = None;
    for completion in completions {
      match completion {
        crate::vhost_user::DaxCompletion::Mapped(window) => {
          self.backed.windows.push(window);
          completed = self.answer_held_map(memory, None, admission)?;
        }
        crate::vhost_user::DaxCompletion::MapRefused { .. } => {
          completed = self.answer_held_map(memory, Some(EIO), admission)?;
        }
        crate::vhost_user::DaxCompletion::Unmapped(_) => {}
        crate::vhost_user::DaxCompletion::UnmapRefused { .. } => {
          self.backed.counts.unmaps_refused = self.backed.counts.unmaps_refused.saturating_add(1);
        }
      }
    }
    Ok(completed)
  }

  /// Answers the held SETUPMAPPING ([`crate::backed::HeldMap`]): success (`errno` `None`) or that errno. Scatters the
  /// reply and publishes the used element, as [`Device::complete_awaiting`] does for the barrier, and releases the
  /// charge the chain held since its service pass. `None` when no SETUPMAPPING is held.
  #[cfg(target_os = "linux")]
  fn answer_held_map(
    &mut self,
    memory: &mut dyn GuestMemory,
    errno: Option<i32>,
    admission: &mut dyn ChainAdmission,
  ) -> Result<Option<Completed>, DeviceError> {
    let Some(held) = self.backed.map.take() else {
      return Ok(None);
    };
    let index = self.queue_index(held.queue)?;
    let n = match errno {
      None => ReplyHeader::write_ok(held.unique, &[], &mut self.reply).unwrap_or(0),
      Some(e) => ReplyHeader::write_error(held.unique, e, &mut self.reply).unwrap_or(0),
    };
    let reply = self.reply.get(..n).unwrap_or(&[]);
    if let Err(e) = scatter(memory, &held.chain.writable, reply) {
      return Err(self.record_fault(e));
    }
    let written = u32::try_from(n).unwrap_or(u32::MAX);
    self
      .queue_at(index)?
      .push_used(memory, &held.chain, written)?;
    admission.complete();
    Ok(Some(Completed {
      queue: held.queue,
      interrupt_wanted: self.queue_at(index)?.interrupts_wanted(memory)?,
      refused: errno.is_some(),
    }))
  }

  /// The terminal drain for the message placer (§4.6 teardown): the guest is ending, so what the device holds of its
  /// requests is dropped (the guest will not read them), and the requests still awaiting a reply-ack are forgotten,
  /// since a window over a backing pins nothing.
  #[cfg(target_os = "linux")]
  pub fn drain_dax_terminal(&mut self) {
    self.backed.map = None;
    self.backed.request = None;
    self.backed.reply = None;
    if let Some(crate::dax::DaxPlacer::Message(message)) = self.dax_placer.as_mut() {
      // Nothing waits on these acknowledgements: no block is pinned behind a window over a backing.
      drop(message.take_pending());
    }
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

  /// The queue at an index [`Self::queue_index`] checked; a miss is the configuration it checked against.
  fn queue_at(&mut self, index: usize) -> Result<&mut Virtqueue, DeviceError> {
    self.queues.get_mut(index).ok_or(DeviceError::NotConfigured)
  }

  /// Serves one chain: the FUSE request is gathered, dispatched and its reply scattered; returns
  /// the bytes written into the chain's writable buffers (the used element's length). With the message placer, a
  /// request that asks for the guest's stores first is held unless `ingested` says how their ingest came out.
  fn serve_chain(
    &mut self,
    queue: u16,
    chain: &DescriptorChain,
    memory: &mut dyn GuestMemory,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    #[cfg(target_os = "linux")] ingested: Option<Ingested>,
  ) -> Result<ChainOutcome, DeviceError> {
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
    let opcode = header.as_ref().and_then(|h| Opcode::from_wire(h.opcode));
    // A request that asks for the guest's stores through its maps to reach the volume first is held, unserved, while
    // they are written back ([`crate::backed`]); the turn serves it when the ingest is done, with how it came out.
    #[cfg(target_os = "linux")]
    if ingested.is_none() && self.hold_for_ingest(bridge, queue) {
      self.backed.request = Some(crate::backed::HeldRequest {
        queue,
        chain: chain.clone(),
      });
      self.backed.counts.ingests_held = self.backed.counts.ingests_held.saturating_add(1);
      return Ok(ChainOutcome::IngestHeld);
    }
    self.observe(header.as_ref());
    // The guest unmounting (FUSE_DESTROY) with live DAX windows: tear each down (revoke, unmap, unpin) so a block
    // mapped and never explicitly REMOVEMAPPING'd is not left pinned — otherwise an unmount would leak every mapped
    // block's pin and the deferred free could never reclaim it (§4.6 terminal teardown). The message placer's windows
    // are unmapped and its backings let go, their stores ingested while the unmount was held.
    #[cfg(unix)]
    if matches!(opcode, Some(Opcode::Destroy)) {
      self.release_dax(bridge, cx);
    }
    // A request whose success reply cannot fit the room the guest posted is not dispatched (AUD-29-85): its
    // effect — a created file, an open handle, a reference, changed bytes — would happen and its reply, naming
    // what it granted, would be lost. It is answered `EIO` below, with no effect.
    let room_short = header
      .as_ref()
      .and_then(|h| Opcode::from_wire(h.opcode))
      .and_then(success_reply_bytes)
      .is_some_and(|need| reply_room < need);
    let mut written = if room_short {
      0
    } else {
      // On Unix, FUSE_SETUPMAPPING / FUSE_REMOVEMAPPING go through the DAX cache region when the VMM offered one
      // (§4.6); every other opcode — and these when no window is offered — are answered by the FUSE codec's dispatch.
      #[cfg(unix)]
      {
        match serve_dax(
          &self.request,
          bridge,
          cx,
          &mut self.dax_placer,
          &mut self.dax_windows,
          #[cfg(target_os = "linux")]
          &mut self.backed,
          &mut self.reply,
        ) {
          DaxServed::NotHandled => dispatch(&self.request, bridge, cx, &mut self.reply),
          DaxServed::Replied(n) => n,
          // The message placer's SETUPMAPPING was taken: hold the chain while its window's backing is filled and its
          // map acknowledged. No reply is scattered now; the turn and the ack answer the guest.
          #[cfg(target_os = "linux")]
          DaxServed::MapHeld {
            unique,
            window,
            sends,
          } => {
            self.backed.map = Some(crate::backed::HeldMap {
              queue,
              chain: chain.clone(),
              unique,
              window,
              sends,
              sent: false,
            });
            return Ok(ChainOutcome::DaxMapHeld);
          }
        }
      }
      #[cfg(not(unix))]
      {
        dispatch(&self.request, bridge, cx, &mut self.reply)
      }
    };
    // A write-back the volume refused before this request is answered where its asker reads an answer, so a guest's
    // fsync or close fails rather than claim its stores durable (A-163's rule; a RELEASE's answer the kernel ignores,
    // and a FORGET has none).
    #[cfg(target_os = "linux")]
    if let Some(Ingested {
      refused: Some(errno),
      ..
    }) = ingested
      && matches!(opcode, Some(Opcode::FSync | Opcode::Flush | Opcode::Read))
      && let Some(h) = header.as_ref()
    {
      written = ReplyHeader::write_error(h.unique, errno, &mut self.reply).unwrap_or(0);
    }
    if written == 0 && expects_reply(header.as_ref()) && reply_room >= OUT_HEADER_LEN {
      // The reply does not fit the guest's buffers (a driver posted less room than the ABI's reply
      // needs): answer the request with EIO rather than leaving it waiting forever.
      written = header
        .map(|h| ReplyHeader::write_error(h.unique, EIO, &mut self.reply).unwrap_or(0))
        .unwrap_or(0);
      self.counters.replies_truncated = self.counters.replies_truncated.saturating_add(1);
    }
    let reply = self.reply.get(..written).unwrap_or(&[]);
    if let Err(e) = scatter(memory, &chain.writable, reply) {
      // The reply never reached the guest, so the guest can never send the FORGET or RELEASE for what it
      // granted; the device gives it back here (AUD-29-85).
      let reclaimed = reclaim_unreported(
        header.as_ref().and_then(|h| Opcode::from_wire(h.opcode)),
        header.as_ref().map_or(0, |h| h.nodeid),
        reply,
        bridge,
        cx,
      );
      self.counters.reclaimed = self
        .counters
        .reclaimed
        .saturating_add(reclaimed.references.saturating_add(reclaimed.handles));
      return Err(e);
    }
    #[cfg(target_os = "linux")]
    if matches!(opcode, Some(Opcode::Forget | Opcode::BatchForget)) {
      self.release_forgotten(bridge);
    }
    self.count(hiprio, chain.readable_bytes, written);
    Ok(ChainOutcome::Replied(
      u32::try_from(written).unwrap_or(u32::MAX),
    ))
  }

  /// Records what the request was, when it is one the device tracks (INIT, DESTROY).
  fn observe(&mut self, header: Option<&InHeader>) {
    match header.and_then(|h| Opcode::from_wire(h.opcode)) {
      Some(Opcode::Init) => {
        self.negotiated = negotiate(
          self.request.get(IN_HEADER_LEN..).unwrap_or_default(),
          slates_bridge_core::CacheCoherence::Revalidated,
        )
        .ok();
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

/// The message placer's backed DAX, a step a turn (§4.6; [`crate::backed`]). Linux only.
#[cfg(target_os = "linux")]
impl Device {
  /// One step of the message placer's backed DAX, taken before each service pass. In the order a guest must see them:
  /// 1. a refill a backing owes (the disk changed beneath it), a slice of it, before anything more reaches the guest;
  /// 2. a reply held for that refill, published once it is done;
  /// 3. a request held for the ingest of the guest's stores, a slice of the ingest, then the request served;
  /// 4. a SETUPMAPPING held for its backing's fill, a slice of the fill, then its map sent or its answer published;
  /// 5. a map awaiting the front end's acknowledgement, which the loop awaits ([`Device::complete_dax_awaiting`]).
  ///
  /// `busy` when the pass is to serve no chain; a completion names the queue to notify; `barrier_owed` when a request it
  /// served waits for the owner's barrier. Each slice copies at most `slice_bytes`. Nothing to do without the message
  /// placer.
  pub fn dax_turn(
    &mut self,
    memory: &mut dyn GuestMemory,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    slice_bytes: u64,
    admission: &mut dyn ChainAdmission,
  ) -> Result<DaxTurned, DeviceError> {
    if let Some(fault) = &self.fault {
      return Err(fault.clone());
    }
    if !matches!(self.dax_placer, Some(crate::dax::DaxPlacer::Message(_))) {
      return Ok(DaxTurned::default());
    }
    let turned = self.backed_step(memory, bridge, cx, slice_bytes, admission);
    self.backed.take_session_counts();
    turned
  }

  /// [`Device::dax_turn`]'s steps, in order.
  fn backed_step(
    &mut self,
    memory: &mut dyn GuestMemory,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    slice_bytes: u64,
    admission: &mut dyn ChainAdmission,
  ) -> Result<DaxTurned, DeviceError> {
    let busy = DaxTurned {
      busy: true,
      ..DaxTurned::default()
    };
    if self.refill_owed(bridge) {
      self.backed.session.refill_step(bridge, cx, slice_bytes);
      return Ok(busy);
    }
    if let Some(held) = self.backed.reply.take() {
      let completed =
        self.publish_held(memory, held.queue, &held.chain, held.written, admission)?;
      return Ok(DaxTurned {
        completed: Some(completed),
        ..DaxTurned::default()
      });
    }
    if self.backed.request.is_some() {
      return self.ingest_turn(memory, bridge, cx, slice_bytes, admission);
    }
    match self.backed.map.as_ref() {
      Some(held) if held.sent => Ok(busy),
      Some(_) => self.fill_turn(memory, bridge, cx, slice_bytes, admission),
      None => Ok(DaxTurned::default()),
    }
  }

  /// A slice of the held request's ingest; when it is done, the request served with how it came out.
  fn ingest_turn(
    &mut self,
    memory: &mut dyn GuestMemory,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    slice_bytes: u64,
    admission: &mut dyn ChainAdmission,
  ) -> Result<DaxTurned, DeviceError> {
    let busy = DaxTurned {
      busy: true,
      ..DaxTurned::default()
    };
    let Some(done) = self.backed.session.ingest_step(bridge, cx, slice_bytes) else {
      return Ok(busy);
    };
    let Some(held) = self.backed.request.take() else {
      return Ok(DaxTurned::default());
    };
    let ingested = Ingested {
      wrote: done.wrote,
      refused: done
        .refused
        .as_ref()
        .map(slates_bridge_fuse::bridge::dax_failure_errno),
    };
    let written =
      match self.serve_chain(held.queue, &held.chain, memory, bridge, cx, Some(ingested)) {
        Ok(ChainOutcome::Replied(written)) => written,
        // A held request is one that asks for an ingest, never a SETUPMAPPING, and with its ingest done it is not held
        // again: serving it holds nothing more.
        Ok(ChainOutcome::DaxMapHeld | ChainOutcome::IngestHeld) => return Ok(busy),
        Err(refusal) => return Err(self.record_fault(refusal)),
      };
    self.after_served(
      memory,
      bridge,
      (held.queue, &held.chain, written),
      ingested.wrote,
      admission,
    )
  }

  /// What follows a request served out of turn: its used element held for the owner's barrier when it changed what
  /// survives a restart or its ingest wrote, held for a refill when serving it marked a backing stale, or published.
  fn after_served(
    &mut self,
    memory: &mut dyn GuestMemory,
    bridge: &mut dyn Bridge,
    (queue, chain, written): (u16, &DescriptorChain, u32),
    wrote: bool,
    admission: &mut dyn ChainAdmission,
  ) -> Result<DaxTurned, DeviceError> {
    if let Some(awaiting) =
      self.awaits_barrier(queue, chain, written, bridge.awaits_publication() || wrote)
    {
      self.awaiting = Some(awaiting);
      self.counters.barriers_awaited = self.counters.barriers_awaited.saturating_add(1);
      return Ok(DaxTurned {
        busy: true,
        barrier_owed: true,
        completed: None,
      });
    }
    if self.refill_owed(bridge) {
      self.hold_reply(queue, chain, written);
      return Ok(DaxTurned {
        busy: true,
        ..DaxTurned::default()
      });
    }
    let completed = self.publish_held(memory, queue, chain, written, admission)?;
    Ok(DaxTurned {
      completed: Some(completed),
      ..DaxTurned::default()
    })
  }

  /// A slice of the held SETUPMAPPING's fill; when it is done, its map sent to the front end, or the guest answered:
  /// success for a window the front end maps as asked already, the refusal's errno for a fill refused.
  fn fill_turn(
    &mut self,
    memory: &mut dyn GuestMemory,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    slice_bytes: u64,
    admission: &mut dyn ChainAdmission,
  ) -> Result<DaxTurned, DeviceError> {
    let busy = DaxTurned {
      busy: true,
      ..DaxTurned::default()
    };
    let answered = match self.backed.session.fill_step(bridge, cx, slice_bytes) {
      slates_bridge_core::dax_backing::FillStep::Filling => return Ok(busy),
      slates_bridge_core::dax_backing::FillStep::Done(descriptor) => {
        match (self.backed.map.as_mut(), self.dax_placer.as_mut()) {
          (Some(held), _) if !held.sends => {
            self.backed.counts.maps_kept = self.backed.counts.maps_kept.saturating_add(1);
            None
          }
          (Some(held), Some(crate::dax::DaxPlacer::Message(placer))) => {
            use std::os::fd::AsFd as _;
            let window = held.window;
            match placer.begin_map(
              window.moffset,
              window.len,
              descriptor.as_fd(),
              window.foffset,
              window.writable,
              window,
            ) {
              Ok(()) => {
                held.sent = true;
                self.backed.counts.maps_sent = self.backed.counts.maps_sent.saturating_add(1);
                return Ok(busy);
              }
              Err(_) => Some(EIO),
            }
          }
          _ => Some(EIO),
        }
      }
      slates_bridge_core::dax_backing::FillStep::Refused(refused) => {
        Some(slates_bridge_fuse::bridge::dax_refused_errno(&refused))
      }
      slates_bridge_core::dax_backing::FillStep::Idle => Some(EIO),
    };
    let completed = self.answer_held_map(memory, answered, admission)?;
    Ok(DaxTurned {
      completed,
      ..DaxTurned::default()
    })
  }

  /// Publishes the used element of a chain served out of turn, its reply already scattered, and releases its charge.
  fn publish_held(
    &mut self,
    memory: &mut dyn GuestMemory,
    queue: u16,
    chain: &DescriptorChain,
    written: u32,
    admission: &mut dyn ChainAdmission,
  ) -> Result<Completed, DeviceError> {
    let index = self.queue_index(queue)?;
    self.queue_at(index)?.push_used(memory, chain, written)?;
    admission.complete();
    Ok(Completed {
      queue,
      interrupt_wanted: self.queue_at(index)?.interrupts_wanted(memory)?,
      refused: false,
    })
  }

  /// Begins the ingest the request just gathered asks for, if it is one that asks for the guest's stores first and any
  /// of the backings it names holds ranges asked writable ([`crate::backed::ingest_asked`]): whether one began.
  fn hold_for_ingest(&mut self, bridge: &mut dyn Bridge, queue: u16) -> bool {
    if self.backed.session.is_empty()
      || !matches!(self.dax_placer, Some(crate::dax::DaxPlacer::Message(_)))
    {
      return false;
    }
    let Some(asked) = crate::backed::ingest_asked(&self.request) else {
      return false;
    };
    let Some(state) = bridge.dax_state() else {
      return false;
    };
    let then = slates_bridge_core::dax_backing::Then::Serve(u64::from(queue));
    match asked {
      crate::backed::IngestAsk::Nodes(nodes) => {
        let inodes = self.backed.session.worth_ingesting(state.backings, nodes);
        self.backed.session.begin_ingest(inodes, then)
      }
      crate::backed::IngestAsk::All => self.backed.session.begin_ingest_all(state.backings, then),
    }
  }

  /// Lets go of the backing of every node the FORGET or BATCH_FORGET just served forgot. A Linux guest removes a DAX
  /// inode's mappings, by waited FUSE_REMOVEMAPPING requests, before it queues the FORGET (`fs/fuse/inode.c`); a window
  /// a guest left anyway is unmapped first, and a backing whose window could not be unmapped is kept until the device
  /// ends, so no window outlives its backing.
  fn release_forgotten(&mut self, bridge: &mut dyn Bridge) {
    if self.backed.session.is_empty() {
      return;
    }
    let nodes = crate::backed::forgotten(&self.request, &[]);
    let Some(crate::dax::DaxPlacer::Message(placer)) = self.dax_placer.as_mut() else {
      return;
    };
    let Some(state) = bridge.dax_state() else {
      return;
    };
    for node in nodes {
      if !self.backed.session.backs(node) {
        continue;
      }
      let mut unmapped = true;
      let mut index = 0;
      while index < self.backed.windows.len() {
        match self.backed.windows.get(index).copied() {
          Some(window) if window.inode == node => {
            if unmap_backed(placer, &mut self.backed.counts, &window) {
              self.backed.windows.swap_remove(index);
            } else {
              unmapped = false;
              index = index.saturating_add(1);
            }
          }
          _ => index = index.saturating_add(1),
        }
      }
      if unmapped {
        self.backed.session.release(state.backings, node);
        self.backed.counts.released = self.backed.counts.released.saturating_add(1);
      }
    }
  }

  /// Whether the message placer owes its guest a refill before anything more: a backing of its marked stale (the disk
  /// changed beneath it, or the view moved) or a refill under way.
  fn refill_owed(&mut self, bridge: &mut dyn Bridge) -> bool {
    !self.backed.session.is_empty()
      && bridge
        .dax_state()
        .is_some_and(|state| self.backed.session.refill_owed(state.backings))
  }

  /// Holds a served chain's used element until the refill its reply waits for is done.
  fn hold_reply(&mut self, queue: u16, chain: &DescriptorChain, written: u32) {
    self.backed.reply = Some(crate::backed::HeldReply {
      queue,
      chain: chain.clone(),
      written,
    });
    self.backed.counts.replies_held = self.backed.counts.replies_held.saturating_add(1);
  }

  /// Starts the ingest the guest's stores through its maps take before the device ends: every backing with ranges asked
  /// writable. A request held for an ingest goes with the guest, its ingest folded into this one. Whether there is one.
  pub fn begin_end_ingest(&mut self, bridge: &mut dyn Bridge) -> bool {
    if !matches!(self.dax_placer, Some(crate::dax::DaxPlacer::Message(_))) {
      return false;
    }
    self.backed.request = None;
    match bridge.dax_state() {
      Some(state) => self.backed.session.ingest_before_end(state.backings),
      None => false,
    }
  }

  /// One slice of the end's ingest, of at most `slice_bytes`: `Some(wrote)` when it is done, so the caller publishes
  /// the volume before the attachment ends when it wrote.
  pub fn end_ingest_step(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    slice_bytes: u64,
  ) -> Option<bool> {
    let done = self.backed.session.ingest_step(bridge, cx, slice_bytes);
    self.backed.take_session_counts();
    done.map(|done| done.wrote)
  }

  /// Ends the end's ingest unwritten, counted: the attachment's bridge or authority is gone, so the guest's stores
  /// cannot reach the volume.
  pub fn abort_end_ingest(&mut self) {
    let aborted =
      self
        .backed
        .session
        .abort_ingest(slates_bridge_core::dax_backing::Failure::Volume(
          slates_vfs::error::VfsError::NotFound,
        ));
    self.backed.take_session_counts();
    // What the ingest was for is the end itself, which goes on: nothing waits on its outcome beyond the count.
    let _for_the_end = aborted;
  }
}

/// Whether a request of this header expects a reply: every opcode but the forgets ([`takes_reply`]).
fn expects_reply(header: Option<&InHeader>) -> bool {
  takes_reply(header.and_then(|h| Opcode::from_wire(h.opcode)))
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
  let mut rest = bytes;
  for range in ranges {
    if rest.is_empty() {
      break;
    }
    let room = usize::try_from(range.len())
      .map_err(|_| DeviceError::BufferTooLarge { bytes: range.len() })?;
    let (piece, after) = rest
      .split_at_checked(room.min(rest.len()))
      .unwrap_or((rest, &[]));
    let target = GuestRange::new(
      range.start(),
      u64::try_from(piece.len()).unwrap_or(u64::MAX),
    )?;
    memory.write(target, piece)?;
    rest = after;
  }
  Ok(())
}

/// A live window of the fd-sharing placer (§4.6): the guest-window offset and length it occupies, and the gate mapping
/// whose pinned block it overlays — so REMOVEMAPPING (and a teardown) finds the mapping to revoke and the window range
/// to unmap. Bounded by the cache region's pages (one per live SETUPMAPPING). The message placer's windows map per-file
/// backings instead (`crate::backed::BackedWindow`).
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
pub struct DaxWindowEntry {
  /// The offset in the guest's DAX window.
  moffset: u64,
  /// The length mapped.
  len: u64,
  /// The gate mapping whose block backs the window range.
  mapping: slates_vfs::dax::Mapping,
}

/// Linux errno values a DAX reply carries (a guest reads them as Linux, whatever the host).
#[cfg(unix)]
mod dax_errno {
  pub(super) const ENOENT: i32 = 2;
  pub(super) const EPERM: i32 = 1;
  /// Format: the Linux errno ABI values a DAX reply carries — invalid argument, no space, unsupported
  /// operation (a guest reads them as Linux, whatever the host).
  pub(super) const EINVAL: i32 = 22;
  pub(super) const ENOSPC: i32 = 28;
  pub(super) const EOPNOTSUPP: i32 = 95;
  /// Format: Linux `EBUSY`, a window that cannot be unmapped yet, which a SETUPMAPPING over it waits for.
  #[cfg(target_os = "linux")]
  pub(super) const EBUSY: i32 = 16;
}

/// The Linux errno a DAX reply carries for a volume-core refusal.
#[cfg(unix)]
fn vfs_dax_errno(e: &slates_vfs::error::VfsError) -> i32 {
  use slates_vfs::error::VfsError as E;
  match e {
    E::NotPermitted | E::LinkProtected => dax_errno::EPERM,
    E::NotFound | E::StaleHandle => dax_errno::ENOENT,
    E::NoSpace | E::FileTooLarge => dax_errno::ENOSPC,
    // Both are "this host will not do that", not "you asked wrongly": the second is §4.6 L1 refusing a shift on
    // a host that cannot back a physically-contiguous sandbox for it.
    E::SpecialFileOperation | E::NoPhysicalIsolation => dax_errno::EOPNOTSUPP,
    _ => dax_errno::EINVAL,
  }
}

/// Tears one of the fd-sharing placer's live DAX windows down (§4.6): the two-phase revoke — unregister so the hardware
/// path is inert, unmap the window so the guest's page table loses it, then unpin the block so a deferred free can
/// reclaim it. Shared by REMOVEMAPPING, a remap over the same window region, and the terminal teardown, so no path
/// leaves a pinned block held.
#[cfg(unix)]
fn teardown_dax_window(
  cache: &mut crate::dax::DaxCacheRegion,
  bridge: &mut dyn slates_bridge_core::Bridge,
  cx: &slates_bridge_core::OpContext,
  entry: &DaxWindowEntry,
) {
  // Phase one: unregister so the hardware path is inert and no new access is authorized.
  let _ = bridge.dax_revoke_pending(&entry.mapping, cx);
  // Phase two now: the back end owns the window, so the munmap is synchronous, then the block is unpinned — **only if
  // the unmap actually happened.** A refused unmap leaves the window still mapped to the block, and unpinning then
  // returns a block a guest can still reach to the arena's free lists, for the next tenant to be given. That is the
  // cross-tenant disclosure this two-phase revoke exists to prevent: never unpin before the window is gone.
  //
  // `DaxCacheRegion::unmap` overlays the slot with an anonymous `PROT_NONE` mapping, and `mmap(MAP_FIXED)` over a
  // sub-range splits a VMA — so it can fail with `ENOMEM` once the process reaches `vm.max_map_count`, which a device
  // with many live windows approaches by doing exactly this. On that path the pin is held instead: the block stays
  // charged and unreusable, which leaks memory where unpinning would leak content.
  if cache.unmap(entry.moffset, entry.len).is_ok() {
    let _ = bridge.dax_release_after_unmap(&entry.mapping, cx);
  }
}

/// Whether two window byte ranges `[off, off+len)` overlap, saturating so no bound can overflow.
#[cfg(unix)]
fn dax_ranges_overlap(a_off: u64, a_len: u64, b_off: u64, b_len: u64) -> bool {
  a_off < b_off.saturating_add(b_len) && b_off < a_off.saturating_add(a_len)
}

/// Tears down every window a SETUPMAPPING mapped before it failed partway through its range (§4.6 N-map rollback), so a
/// refused SETUPMAPPING backs nothing and leaks no pin: each of the fd-sharing placer's windows in `[win_off,
/// win_off+win_len)` this request pushed is torn down.
#[cfg(unix)]
fn rollback_dax_setup(
  cache: &mut crate::dax::DaxCacheRegion,
  bridge: &mut dyn slates_bridge_core::Bridge,
  cx: &slates_bridge_core::OpContext,
  dax_windows: &mut Vec<DaxWindowEntry>,
  win_off: u64,
  win_len: u64,
) {
  let mut index = 0;
  while index < dax_windows.len() {
    match dax_windows.get(index).copied() {
      Some(held) if dax_ranges_overlap(held.moffset, held.len, win_off, win_len) => {
        teardown_dax_window(cache, bridge, cx, &held);
        dax_windows.swap_remove(index);
      }
      _ => index = index.saturating_add(1),
    }
  }
}

/// What one chunk's map attempt did in the N-map loop (§4.6): advanced the cursor by `this` bytes, ran past the
/// file's content (stop the loop), or refused with an errno (tear down and refuse the whole SETUPMAPPING).
#[cfg(unix)]
enum MapStep {
  Advanced { this: usize },
  PastFile,
  Refused(i32),
}

/// The window offset, arena-object offset and mapped length for chunk `mapping` at file offset `cur` (§4.6): bounded
/// to the chunk's authorized window (`file_len` rounded up to `granule`, capped at the block), and never past the
/// request's `end`. `None` on any overflow (a hostile offset or length), which the caller turns into a refusal.
#[cfg(unix)]
#[allow(
  clippy::too_many_arguments,
  reason = "the per-chunk window geometry is a flat set of offsets and lengths; a struct would only move the same fields"
)]
fn chunk_map_params(
  mapping: &slates_vfs::dax::Mapping,
  region_off: usize,
  block_len: usize,
  cur: usize,
  foffset: usize,
  base_moffset: u64,
  end: usize,
  granule: usize,
) -> Option<(u64, u64, u64)> {
  let intra = cur.checked_sub(mapping.block_file_start)?;
  let authorized = mapping
    .file_len
    .div_ceil(granule)
    .saturating_mul(granule)
    .min(block_len);
  let room = authorized.checked_sub(intra).filter(|room| *room > 0)?;
  let this = end.saturating_sub(cur).min(room);
  let win_off = base_moffset.checked_add(u64::try_from(cur.saturating_sub(foffset)).ok()?)?;
  let object_off = u64::try_from(region_off.checked_add(intra)?).ok()?;
  let mapped_len = u64::try_from(this).ok()?;
  Some((win_off, object_off, mapped_len))
}

/// Maps the one chunk covering file offset `cur` into the fd-sharing placer's window (§4.6 N-map loop): the gate returns
/// the open chunk (zero-copy) or the arm's sealed-chunk backing, which the cache overlays synchronously. A `NotFound`
/// or a chunk that does not cover `cur` (a writable single-block map re-returns block 0) means the range ran past the
/// file's content — [`MapStep::PastFile`]. Any pin a refused step took is undone before it returns, so a refusal leaves
/// nothing held for the caller to roll back beyond the earlier chunks.
#[cfg(unix)]
#[allow(
  clippy::too_many_arguments,
  reason = "the per-chunk map needs the window geometry, the cache, the bridge and the cursor; a struct would only move the same fields"
)]
fn map_chunk_at(
  cache: &mut crate::dax::DaxCacheRegion,
  bridge: &mut dyn slates_bridge_core::Bridge,
  cx: &slates_bridge_core::OpContext,
  dax_windows: &mut Vec<DaxWindowEntry>,
  object: slates_bridge_core::ObjectId,
  writable: bool,
  cur: usize,
  foffset: usize,
  base_moffset: u64,
  end: usize,
  granule: usize,
) -> MapStep {
  let mapping = match bridge.dax_map(object, cx, writable, cur) {
    Ok(mapping) => mapping,
    // No chunk at this offset: the range ran past the file's content.
    Err(slates_vfs::error::VfsError::NotFound) => return MapStep::PastFile,
    Err(e) => return MapStep::Refused(vfs_dax_errno(&e)),
  };
  let Some((fd, region_off, block_len)) = bridge.dax_backing(&mapping) else {
    let _ = bridge.dax_unmap(&mapping, cx);
    return MapStep::Refused(dax_errno::EOPNOTSUPP);
  };
  // A chunk that does not cover `cur` (a writable single-block map re-returns block 0) means the range ran past the
  // file's content.
  if cur >= mapping.block_file_start.saturating_add(mapping.file_len) {
    let _ = bridge.dax_unmap(&mapping, cx);
    return MapStep::PastFile;
  }
  let Some((win_off, object_off, mapped_len)) = chunk_map_params(
    &mapping,
    region_off,
    block_len,
    cur,
    foffset,
    base_moffset,
    end,
    granule,
  ) else {
    let _ = bridge.dax_unmap(&mapping, cx);
    return MapStep::Refused(dax_errno::EINVAL);
  };
  if cache
    .map(win_off, mapped_len, fd, object_off, writable)
    .is_err()
  {
    let _ = bridge.dax_unmap(&mapping, cx);
    return MapStep::Refused(dax_errno::EINVAL);
  }
  dax_windows.push(DaxWindowEntry {
    moffset: win_off,
    len: mapped_len,
    mapping,
  });
  MapStep::Advanced {
    this: usize::try_from(mapped_len).unwrap_or(0),
  }
}

/// Serves FUSE_SETUPMAPPING / FUSE_REMOVEMAPPING through the placer the VMM offered (§4.6): the fd-sharing cache region
/// over pinned content blocks ([`serve_local_dax`]), or the message placer over per-file backings
/// (`serve_backed_dax`, Linux). [`DaxServed::NotHandled`] when it is not a DAX verb or no window is offered — then the FUSE
/// codec's `dispatch` answers (EOPNOTSUPP for setup, a no-op for remove). The guest reaches the bytes through its own
/// page table after this; the daemon is not called per access.
#[cfg(unix)]
fn serve_dax(
  request: &[u8],
  bridge: &mut dyn slates_bridge_core::Bridge,
  cx: &slates_bridge_core::OpContext,
  dax_placer: &mut Option<crate::dax::DaxPlacer>,
  dax_windows: &mut Vec<DaxWindowEntry>,
  #[cfg(target_os = "linux")] backed: &mut crate::backed::BackedDax,
  out: &mut [u8],
) -> DaxServed {
  use slates_bridge_fuse::request::Request;

  // No window offered: let dispatch answer EOPNOTSUPP (setup) / no-op (remove).
  let Some(placer) = dax_placer.as_mut() else {
    return DaxServed::NotHandled;
  };
  let Ok(req) = Request::parse(request) else {
    return DaxServed::NotHandled;
  };
  let Some(opcode) = Opcode::from_wire(req.header.opcode) else {
    return DaxServed::NotHandled;
  };
  if !matches!(opcode, Opcode::SetupMapping | Opcode::RemoveMapping) {
    return DaxServed::NotHandled;
  }
  match placer {
    crate::dax::DaxPlacer::Local(cache) => {
      serve_local_dax(&req, opcode, bridge, cx, cache, dax_windows, out)
    }
    #[cfg(target_os = "linux")]
    crate::dax::DaxPlacer::Message(message) => {
      serve_backed_dax(&req, opcode, bridge, cx, (message, backed), out)
    }
  }
}

/// The fd-sharing placer's SETUPMAPPING and REMOVEMAPPING (§4.6): the cache region overlays each content block of the
/// range, pinned by the gate while mapped, and answers at once.
#[cfg(unix)]
fn serve_local_dax(
  req: &slates_bridge_fuse::request::Request<'_>,
  opcode: Opcode,
  bridge: &mut dyn slates_bridge_core::Bridge,
  cx: &slates_bridge_core::OpContext,
  cache: &mut crate::dax::DaxCacheRegion,
  dax_windows: &mut Vec<DaxWindowEntry>,
  out: &mut [u8],
) -> DaxServed {
  use slates_bridge_core::ObjectId;
  use slates_bridge_fuse::request::{RemoveMappingIn, SetupMappingIn};

  let unique = req.header.unique;
  let err = |errno: i32, out: &mut [u8]| {
    DaxServed::Replied(ReplyHeader::write_error(unique, errno, out).unwrap_or(0))
  };
  match opcode {
    Opcode::SetupMapping => {
      let Ok(m) = SetupMappingIn::parse(Opcode::SetupMapping.to_wire(), req.body) else {
        return err(dax_errno::EINVAL, out);
      };
      let writable = m.writable();
      // Remap: this SETUPMAPPING may cover a window region that already holds mappings. Tear each overlapping one
      // down (revoke, unmap, unpin) before establishing the new one — otherwise its pin leaks and its stale entry
      // shadows the new mapping when REMOVEMAPPING looks the window offset up (adversarial: a guest remapping a
      // window slot to another block). Done before the new block is pinned, so no backing descriptor is borrowed
      // across the `&mut bridge` teardown calls.
      let mut index = 0;
      while index < dax_windows.len() {
        match dax_windows.get(index).copied() {
          Some(held) if dax_ranges_overlap(held.moffset, held.len, m.moffset, m.len) => {
            teardown_dax_window(cache, bridge, cx, &held);
            dax_windows.swap_remove(index);
          }
          _ => index = index.saturating_add(1),
        }
      }
      let object = ObjectId::new(req.header.nodeid, 0);
      let (Ok(foffset), Ok(requested)) = (usize::try_from(m.foffset), usize::try_from(m.len))
      else {
        return err(dax_errno::EINVAL, out);
      };
      let granule = cache.page().max(1);
      // Map EVERY chunk covering [foffset, foffset+requested). A guest's window range can span several chunks, each a
      // distinct arena block mapped at its own window sub-offset; a one-chunk map leaves the rest of the window stale
      // (a multi-block guest mmap then mismatches, or SIGBUSes on a refused sealed head). So one SETUPMAPPING becomes
      // N maps, answered as a whole (§4.6 "DAX maps the open frontier"; the N-map rule the live A/B surfaced). The
      // gate backs each chunk — the open tail zero-copy, a sealed chunk per the arm.
      let mut cur = foffset;
      let end = foffset.saturating_add(requested);
      // Whether at least one chunk has been mapped — so a later `NotFound` (the range ran past the file's content)
      // ends the loop cleanly with what is backed, rather than refusing the whole request.
      let mut mapped_any = false;
      while cur < end {
        match map_chunk_at(
          cache,
          bridge,
          cx,
          dax_windows,
          object,
          writable,
          cur,
          foffset,
          m.moffset,
          end,
          granule,
        ) {
          MapStep::Advanced { this } => {
            mapped_any = true;
            cur = cur.saturating_add(this.max(1));
          }
          // The range ran past the file's content: answer with what is backed (the authorized-window cap leaves the
          // rest unmapped). Nothing mapped yet means the request began past the file — refuse.
          MapStep::PastFile => {
            if mapped_any {
              break;
            }
            rollback_dax_setup(cache, bridge, cx, dax_windows, m.moffset, m.len);
            return err(dax_errno::EINVAL, out);
          }
          // A chunk within the file could not be backed: tear down what this request mapped and refuse the whole
          // SETUPMAPPING — never a partially-backed window.
          MapStep::Refused(errno) => {
            rollback_dax_setup(cache, bridge, cx, dax_windows, m.moffset, m.len);
            return err(errno, out);
          }
        }
      }
      // The whole range is mapped, each chunk synchronously: answer now. An empty range answers too.
      DaxServed::Replied(ReplyHeader::write_ok(unique, &[], out).unwrap_or(0))
    }
    Opcode::RemoveMapping => {
      let Ok(m) = RemoveMappingIn::parse(Opcode::RemoveMapping.to_wire(), req.body) else {
        return err(dax_errno::EINVAL, out);
      };
      for one in &m.entries {
        if let Some(index) = dax_windows.iter().position(|w| w.moffset == one.moffset) {
          if let Some(entry) = dax_windows.get(index).copied() {
            teardown_dax_window(cache, bridge, cx, &entry);
          }
          dax_windows.swap_remove(index);
        }
        // A range the device holds no mapping for is a no-op: nothing of the device's is mapped there.
      }
      DaxServed::Replied(ReplyHeader::write_ok(unique, &[], out).unwrap_or(0))
    }
    _ => DaxServed::NotHandled,
  }
}

/// The message placer's SETUPMAPPING and REMOVEMAPPING (§4.6, [`crate::backed`]).
///
/// A SETUPMAPPING's window range must be whole granules inside the guest's window and its file range must start at a
/// granule inside the backing's object, or it is refused `EINVAL`. Its node is asked of the backed session (which
/// resolves it, checks it may be backed, copies a writable ask's file up, and leaves the asked ranges' fill pending),
/// and a refusal answers that refusal's errno. Then the window is held:
/// - a window the front end maps as asked already (a repeat, or a writable upgrade of a window mapped writable) is
///   answered with no message once the fill is done;
/// - any other window has every window over its range unmapped first, since no map may go over a live one, and is sent
///   once the fill is done. A window that cannot be unmapped yet refuses the SETUPMAPPING `EBUSY`.
///
/// A REMOVEMAPPING unmaps each window it names and answers at once: the front end takes the unmap before any later
/// map on the same channel, and the guest has dropped the range from its page tables before it asks.
#[cfg(target_os = "linux")]
fn serve_backed_dax(
  req: &slates_bridge_fuse::request::Request<'_>,
  opcode: Opcode,
  bridge: &mut dyn slates_bridge_core::Bridge,
  cx: &slates_bridge_core::OpContext,
  (placer, backed): (
    &mut crate::vhost_user::MessagePlacer<crate::backed::BackedWindow>,
    &mut crate::backed::BackedDax,
  ),
  out: &mut [u8],
) -> DaxServed {
  use slates_bridge_fuse::request::{RemoveMappingIn, SetupMappingIn};

  let unique = req.header.unique;
  let err = |errno: i32, out: &mut [u8]| {
    DaxServed::Replied(ReplyHeader::write_error(unique, errno, out).unwrap_or(0))
  };
  match opcode {
    Opcode::SetupMapping => {
      let Ok(m) = SetupMappingIn::parse(Opcode::SetupMapping.to_wire(), req.body) else {
        return err(dax_errno::EINVAL, out);
      };
      let granule = u64::try_from(placer.page()).unwrap_or(u64::MAX);
      if placer.check_window(m.moffset, m.len).is_err()
        || !crate::backed::file_range_fits(m.foffset, m.len, granule)
      {
        return err(dax_errno::EINVAL, out);
      }
      let writable = m.writable();
      let asked = slates_bridge_core::dax_backing::Ask {
        node: req.header.nodeid,
        writable,
        range: Some((m.foffset, m.len)),
      };
      let inode = match backed.session.ask(bridge, cx, asked) {
        Ok(inode) => inode,
        Err(refused) => {
          return err(slates_bridge_fuse::bridge::dax_refused_errno(&refused), out);
        }
      };
      let window = crate::backed::BackedWindow {
        moffset: m.moffset,
        len: m.len,
        inode,
        foffset: m.foffset,
        writable: cx.rights.write,
      };
      let kept = backed.windows.contains(&window) && (window.writable || !writable);
      if !kept && !unmap_overlapping(placer, backed, m.moffset, m.len) {
        if let Some(state) = bridge.dax_state() {
          backed.session.abandon_fill(state.backings);
        }
        return err(dax_errno::EBUSY, out);
      }
      DaxServed::MapHeld {
        unique,
        window,
        sends: !kept,
      }
    }
    Opcode::RemoveMapping => {
      let Ok(m) = RemoveMappingIn::parse(Opcode::RemoveMapping.to_wire(), req.body) else {
        return err(dax_errno::EINVAL, out);
      };
      for one in &m.entries {
        let named = backed.windows.iter().position(|w| w.moffset == one.moffset);
        if let Some(index) = named
          && let Some(window) = backed.windows.get(index).copied()
          && unmap_backed(placer, &mut backed.counts, &window)
        {
          backed.windows.swap_remove(index);
        }
        // A range the device holds no window for is a no-op: nothing of the device's is mapped there.
      }
      DaxServed::Replied(ReplyHeader::write_ok(unique, &[], out).unwrap_or(0))
    }
    _ => DaxServed::NotHandled,
  }
}

/// Asks the front end to unmap `window` (SHMEM_UNMAP): whether the device may forget it. A sent unmap and a dead
/// channel answer yes (nothing maps through a channel that is gone); a ring at its bound answers no, so the window is
/// kept and its unmap asked again by whatever needs it gone. A window over a backing pins nothing, so nothing waits on
/// the unmap's acknowledgement.
#[cfg(target_os = "linux")]
fn unmap_backed(
  placer: &mut crate::vhost_user::MessagePlacer<crate::backed::BackedWindow>,
  counts: &mut crate::backed::BackedCounts,
  window: &crate::backed::BackedWindow,
) -> bool {
  match placer.begin_unmap(window.moffset, window.len, *window) {
    Ok(()) => {
      counts.unmaps_sent = counts.unmaps_sent.saturating_add(1);
      true
    }
    Err(crate::vhost_user::MessagePlaceError::Channel(_)) => true,
    Err(_) => false,
  }
}

/// Unmaps every window over the guest's window range `[moffset, moffset + len)`: whether none is left there.
#[cfg(target_os = "linux")]
fn unmap_overlapping(
  placer: &mut crate::vhost_user::MessagePlacer<crate::backed::BackedWindow>,
  backed: &mut crate::backed::BackedDax,
  moffset: u64,
  len: u64,
) -> bool {
  let mut cleared = true;
  let mut index = 0;
  while index < backed.windows.len() {
    match backed.windows.get(index).copied() {
      Some(window) if window.overlaps(moffset, len) => {
        if unmap_backed(placer, &mut backed.counts, &window) {
          backed.windows.swap_remove(index);
        } else {
          cleared = false;
          index = index.saturating_add(1);
        }
      }
      _ => index = index.saturating_add(1),
    }
  }
  cleared
}

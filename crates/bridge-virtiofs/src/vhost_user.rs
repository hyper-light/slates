//! The inherited-descriptor binding of the VMM seam (§4.6 A-9: "the seam accepts guest-memory/queue capabilities
//! and completion notification from the host VMM through an in-process interface or an inherited descriptor";
//! D-2; AUD-29-68): the vhost-user protocol's back end, spoken over a connected stream socket the harness hands
//! the daemon (a `socketpair` end it inherited or received — never a socket on disk, R1). A VMM with a
//! vhost-user-fs front end (QEMU's `vhost-user-fs-pci` with `-chardev socket,fd=N`, cloud-hypervisor) is
//! the other end.
//!
//! What it is. The front end negotiates features, hands over the guest's memory as sealed memory objects
//! (`SET_MEM_TABLE`, the descriptors arriving by `SCM_RIGHTS`), and configures each virtqueue: its size, the
//! three ring addresses in the front end's own address space, its starting index, its kick eventfd (the guest's
//! notification to the device) and its call eventfd (the device's interrupt to the guest). This module answers
//! the protocol, maps each region through `slates_mem::SharedObject` (copies in and out through the mapping;
//! no reference into memory the guest writes is ever made), translates ring addresses to guest-physical ones,
//! and presents the result as a [`VmmSeam`], so the device and its admission are the very ones the in-process
//! form uses.
//!
//! The rules it holds:
//! - **The consumer is the peer** (§4.13): the socket's `SO_PEERCRED` uid, read before any message — the
//!   kernel's word for who holds the other end, never a claim the front end makes.
//! - **Memory is mapped only when it cannot fault**: a region's object must be sealed against shrinking
//!   (`F_SEAL_SHRINK`, which QEMU's `memory-backend-memfd` sets by default) and already as long as the region
//!   claims, so no later access can meet a truncated page (`SIGBUS`). Regions must not overlap. At most
//!   [`MAX_REGIONS`] (the protocol's baseline table).
//! - **Only what is offered is accepted**: `VIRTIO_F_VERSION_1` and the protocol-features bit; of the protocol
//!   features, multiqueue and reply-ack. Anything else — an unoffered feature, an unknown request, an oversized
//!   message, a queue index past the device's queues, a kick without a descriptor (polling mode) — is a typed
//!   protocol refusal that ends the device.
//! - **The device starts at index zero**: a queue whose starting index is not zero (a reconnect or a
//!   migration) is refused at admission, since the device's ring position starts there.
//! - **Stopping a ring ends the device**: `GET_VRING_BASE` is answered with the ring's position — the used
//!   ring's index, which equals the next available index the device would read because the device completes
//!   each request before it takes the next (`docs/wip/virtiofs.md` §6) — and the ring is stopped: the doorbell
//!   reports a hangup, and the loop runs the terminal step. A new start is a new device.
//! - **One doorbell**: an epoll descriptor holding the socket and every kick eventfd, so the loop waits on one
//!   number; draining it serves pending control messages and consumes every kick.
//!
//! Evidence: the vhost-user protocol (QEMU `docs/interop/vhost-user.rst`: the message header, the request
//! codes, the memory-table and vring payloads, the reply-ack rule; the codes below are as that document numbers
//! them, recorded from it, to be re-checked against the pinned QEMU when the live guest runs); virtio 1.2
//! §2.7 (ring layouts) and §5.11 (the file-system device, two queues by default: high priority and one request
//! queue). QEMU's memfd backend seals by default (`memory-backend-memfd`, property `seal`, default on).

use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use rustix::event::epoll::{self, CreateFlags, EventData, EventFlags};
use rustix::io::Errno;
use rustix::net::{
  RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer, SendFlags,
};
use slates_db::catalog::Principal;
use slates_mem::shared::{Handoff, SharedObject};
use slates_mem::words::Words;

use crate::admission::{Doorbell, Drained, SeamError, VmmSeam};
use crate::device::DeviceConfig;
use crate::memory::{Edge, GuestAddr, GuestMemory, GuestMemoryError, GuestRange};
use crate::virtqueue::QueueLayout;

/// Format: vhost-user's message header: request (`u32`), flags (`u32`), payload size (`u32`).
pub const HEADER_LEN: usize = 12;
/// Format: the protocol version carried in the low two bits of every message's flags.
pub const VERSION: u32 = 1;
/// Format: the flags bit marking a reply.
pub const FLAG_REPLY: u32 = 1 << 2;
/// Format: the flags bit asking for a reply-ack (`REPLY_ACK` negotiated).
pub const FLAG_NEED_REPLY: u32 = 1 << 3;
/// Format: the version mask of the flags.
const VERSION_MASK: u32 = 0x3;

/// Format: the protocol's baseline memory table: at most eight regions (`VHOST_MEMORY_BASELINE_NREGIONS`).
pub const MAX_REGIONS: usize = 8;
/// Format: one memory-table entry: guest address, size, front-end address, mapping offset (four `u64`).
pub const REGION_ENTRY_LEN: usize = 32;
/// Format: the memory table's head: the region count (`u32`) and padding (`u32`).
pub const MEM_TABLE_HEAD_LEN: usize = 8;
/// Derived: the largest payload this back end accepts — the memory table with every baseline region; every
/// other request it serves is smaller.
pub const MAX_PAYLOAD: usize = MEM_TABLE_HEAD_LEN + MAX_REGIONS * REGION_ENTRY_LEN;

/// Format: request codes, as `vhost-user.rst` numbers them.
pub mod request {
  /// Format: `VHOST_USER_GET_FEATURES`.
  pub const GET_FEATURES: u32 = 1;
  /// Format: `VHOST_USER_SET_FEATURES`.
  pub const SET_FEATURES: u32 = 2;
  /// Format: `VHOST_USER_SET_OWNER`.
  pub const SET_OWNER: u32 = 3;
  /// Format: `VHOST_USER_RESET_OWNER`.
  pub const RESET_OWNER: u32 = 4;
  /// Format: `VHOST_USER_SET_MEM_TABLE`.
  pub const SET_MEM_TABLE: u32 = 5;
  /// Format: `VHOST_USER_SET_VRING_NUM`.
  pub const SET_VRING_NUM: u32 = 8;
  /// Format: `VHOST_USER_SET_VRING_ADDR`.
  pub const SET_VRING_ADDR: u32 = 9;
  /// Format: `VHOST_USER_SET_VRING_BASE`.
  pub const SET_VRING_BASE: u32 = 10;
  /// Format: `VHOST_USER_GET_VRING_BASE`.
  pub const GET_VRING_BASE: u32 = 11;
  /// Format: `VHOST_USER_SET_VRING_KICK`.
  pub const SET_VRING_KICK: u32 = 12;
  /// Format: `VHOST_USER_SET_VRING_CALL`.
  pub const SET_VRING_CALL: u32 = 13;
  /// Format: `VHOST_USER_SET_VRING_ERR`.
  pub const SET_VRING_ERR: u32 = 14;
  /// Format: `VHOST_USER_GET_PROTOCOL_FEATURES`.
  pub const GET_PROTOCOL_FEATURES: u32 = 15;
  /// Format: `VHOST_USER_SET_PROTOCOL_FEATURES`.
  pub const SET_PROTOCOL_FEATURES: u32 = 16;
  /// Format: `VHOST_USER_GET_QUEUE_NUM`.
  pub const GET_QUEUE_NUM: u32 = 17;
  /// Format: `VHOST_USER_SET_VRING_ENABLE`.
  pub const SET_VRING_ENABLE: u32 = 18;
}

/// Format: virtio's `VIRTIO_F_VERSION_1` feature bit.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// Format: vhost-user's `VHOST_USER_F_PROTOCOL_FEATURES` feature bit.
pub const F_PROTOCOL_FEATURES: u64 = 1 << 30;
/// Format: the protocol feature `VHOST_USER_PROTOCOL_F_MQ`.
pub const PROTOCOL_F_MQ: u64 = 1 << 0;
/// Format: the protocol feature `VHOST_USER_PROTOCOL_F_REPLY_ACK`.
pub const PROTOCOL_F_REPLY_ACK: u64 = 1 << 3;
/// The device features offered: version 1 and the protocol-features extension (no event index, no indirect
/// descriptors: the virtqueue implements neither, `docs/wip/virtiofs.md` §6).
pub const OFFERED_FEATURES: u64 = VIRTIO_F_VERSION_1 | F_PROTOCOL_FEATURES;
/// The protocol features offered.
pub const OFFERED_PROTOCOL_FEATURES: u64 = PROTOCOL_F_MQ | PROTOCOL_F_REPLY_ACK;
/// Format: a vring descriptor message's queue index (its low byte).
const VRING_INDEX_MASK: u64 = 0xff;
/// Format: a vring descriptor message's "no descriptor" bit.
const VRING_NOFD: u64 = 1 << 8;

/// A typed refusal of the vhost-user binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VhostError {
  /// The socket's peer could not be identified (`SO_PEERCRED` refused).
  PeerUnverified,
  /// A socket, epoll or eventfd call failed.
  Os {
    /// The call.
    call: &'static str,
    /// The errno.
    errno: i32,
  },
  /// The front end closed its end.
  Closed,
  /// The front end broke the protocol (the reason names the rule).
  Protocol {
    /// The rule broken.
    reason: &'static str,
  },
  /// A memory region could not be mapped safely (the reason names the rule).
  Memory {
    /// The rule broken.
    reason: &'static str,
  },
  /// The memory table is inconsistent.
  Table(GuestMemoryError),
}

impl std::fmt::Display for VhostError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::PeerUnverified => f.write_str("the socket's peer could not be identified"),
      Self::Os { call, errno } => write!(f, "{call} failed: errno {errno}"),
      Self::Closed => f.write_str("the front end closed its end"),
      Self::Protocol { reason } => write!(f, "protocol: {reason}"),
      Self::Memory { reason } => write!(f, "memory: {reason}"),
      Self::Table(e) => write!(f, "memory table: {e}"),
    }
  }
}

impl std::error::Error for VhostError {}

fn os(call: &'static str) -> impl Fn(Errno) -> VhostError {
  move |e| VhostError::Os {
    call,
    errno: e.raw_os_error(),
  }
}

const fn protocol(reason: &'static str) -> VhostError {
  VhostError::Protocol { reason }
}

// ------------------------------------------------------------------------------------------ guest memory

/// One guest memory region as the front end described it, mapped.
pub struct MappedRegion {
  /// The guest-physical base.
  guest_phys: u64,
  /// The size in bytes.
  size: u64,
  /// The front end's own address of the region's base (ring addresses arrive in this space).
  user_addr: u64,
  /// Where the region begins in its memory object.
  offset: u64,
  /// The mapping: the object from its start to the region's end.
  object: SharedObject,
}

impl std::fmt::Debug for MappedRegion {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MappedRegion")
      .field("guest_phys", &self.guest_phys)
      .field("size", &self.size)
      .finish()
  }
}

/// One entry of a memory table, as received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionEntry {
  /// The guest-physical base.
  pub guest_phys: u64,
  /// The size in bytes.
  pub size: u64,
  /// The front end's own address of the base.
  pub user_addr: u64,
  /// The offset of the region in its memory object.
  pub offset: u64,
}

impl MappedRegion {
  /// Maps `entry` from the memory object `fd`: refused unless the object is sealed against shrinking and
  /// already holds the region whole.
  pub fn map(entry: RegionEntry, fd: &OwnedFd) -> Result<MappedRegion, VhostError> {
    let end = entry
      .offset
      .checked_add(entry.size)
      .ok_or(VhostError::Memory {
        reason: "the region's end overflows its object",
      })?;
    entry
      .guest_phys
      .checked_add(entry.size)
      .ok_or(VhostError::Table(GuestMemoryError::LengthOverflow {
        start: entry.guest_phys,
        len: entry.size,
      }))?;
    entry
      .user_addr
      .checked_add(entry.size)
      .ok_or(VhostError::Memory {
        reason: "the region's front-end address range overflows",
      })?;
    let seals = rustix::fs::fcntl_get_seals(fd).map_err(|_| VhostError::Memory {
      reason: "the region's object is not a sealable memory object",
    })?;
    if !seals.contains(rustix::fs::SealFlags::SHRINK) {
      return Err(VhostError::Memory {
        reason: "the region's object is not sealed against shrinking",
      });
    }
    let length = rustix::fs::fstat(fd).map_err(os("fstat"))?.st_size;
    if u64::try_from(length).unwrap_or(0) < end {
      return Err(VhostError::Memory {
        reason: "the region's object is shorter than the region",
      });
    }
    let mapped = usize::try_from(end).map_err(|_| VhostError::Memory {
      reason: "the region does not fit this address space",
    })?;
    let object = SharedObject::open(
      &Handoff::Descriptor(fd.as_raw_fd()),
      mapped,
      Words::default(),
    )
    .map_err(|_| VhostError::Memory {
      reason: "the region could not be mapped",
    })?;
    Ok(MappedRegion {
      guest_phys: entry.guest_phys,
      size: entry.size,
      user_addr: entry.user_addr,
      offset: entry.offset,
      object,
    })
  }

  /// Whether `range` lies wholly in this region.
  fn holds(&self, range: GuestRange) -> bool {
    range.start().0 >= self.guest_phys && range.end() <= self.guest_phys.saturating_add(self.size)
  }

  /// The object offset of guest address `at` (in this region).
  fn object_offset(&self, at: u64) -> Option<usize> {
    let within = at.checked_sub(self.guest_phys)?;
    usize::try_from(self.offset.checked_add(within)?).ok()
  }
}

/// The guest's memory as the front end mapped it: the regions, non-overlapping, reached by copy.
#[derive(Debug, Default)]
pub struct GuestRegions {
  regions: Vec<MappedRegion>,
}

impl GuestRegions {
  /// The regions, refused when two overlap in guest-physical space.
  pub fn new(regions: Vec<MappedRegion>) -> Result<GuestRegions, VhostError> {
    for (position, first) in regions.iter().enumerate() {
      for second in regions.iter().skip(position.saturating_add(1)) {
        let first_end = first.guest_phys.saturating_add(first.size);
        let second_end = second.guest_phys.saturating_add(second.size);
        if first.guest_phys < second_end && second.guest_phys < first_end {
          return Err(VhostError::Table(GuestMemoryError::RegionsOverlap {
            first: first.guest_phys,
            second: second.guest_phys,
          }));
        }
      }
    }
    Ok(GuestRegions { regions })
  }

  /// Whether any region is mapped.
  pub fn is_empty(&self) -> bool {
    self.regions.is_empty()
  }

  /// The guest-physical address of the front end's address `user` (ring addresses arrive in its space).
  pub fn guest_of_user(&self, user: u64) -> Option<GuestAddr> {
    self.regions.iter().find_map(|region| {
      let within = user.checked_sub(region.user_addr)?;
      (within < region.size)
        .then(|| region.guest_phys.checked_add(within))
        .flatten()
        .map(GuestAddr)
    })
  }

  /// The region holding `range` and the object offset of its start.
  fn locate(&self, range: GuestRange) -> Result<(usize, usize), GuestMemoryError> {
    let outside = GuestMemoryError::OutsideGuestMemory {
      start: range.start().0,
      len: range.len(),
    };
    let (index, region) = self
      .regions
      .iter()
      .enumerate()
      .find(|(_, region)| region.holds(range))
      .ok_or(outside.clone())?;
    let offset = region.object_offset(range.start().0).ok_or(outside)?;
    Ok((index, offset))
  }
}

impl GuestMemory for GuestRegions {
  fn check(&self, range: GuestRange) -> Result<(), GuestMemoryError> {
    self.locate(range).map(|_| ())
  }

  fn read(&self, range: GuestRange, out: &mut [u8]) -> Result<(), GuestMemoryError> {
    if u64::try_from(out.len()).ok() != Some(range.len()) {
      return Err(GuestMemoryError::BufferMismatch {
        wanted: range.len(),
        have: out.len(),
      });
    }
    let (index, offset) = self.locate(range)?;
    let region = self
      .regions
      .get(index)
      .ok_or(GuestMemoryError::OutsideGuestMemory {
        start: range.start().0,
        len: range.len(),
      })?;
    region
      .object
      .read(offset, out)
      .map_err(|_| GuestMemoryError::OutsideGuestMemory {
        start: range.start().0,
        len: range.len(),
      })
  }

  fn write(&mut self, range: GuestRange, bytes: &[u8]) -> Result<(), GuestMemoryError> {
    if u64::try_from(bytes.len()).ok() != Some(range.len()) {
      return Err(GuestMemoryError::BufferMismatch {
        wanted: range.len(),
        have: bytes.len(),
      });
    }
    let (index, offset) = self.locate(range)?;
    let region = self
      .regions
      .get_mut(index)
      .ok_or(GuestMemoryError::OutsideGuestMemory {
        start: range.start().0,
        len: range.len(),
      })?;
    region
      .object
      .write(offset, bytes)
      .map_err(|_| GuestMemoryError::OutsideGuestMemory {
        start: range.start().0,
        len: range.len(),
      })
  }

  /// The guest's CPUs write this memory concurrently through their own mapping, so each edge is a hardware
  /// fence on this side (virtio 1.2 §2.7.13: the driver pairs its own barriers with these).
  fn order(&self, edge: Edge) {
    use std::sync::atomic::{Ordering, fence};
    fence(match edge {
      Edge::Acquire => Ordering::Acquire,
      Edge::Release => Ordering::Release,
      Edge::Full => Ordering::SeqCst,
    });
  }
}

// ------------------------------------------------------------------------------------------ the rings

/// The three ring addresses of a queue, in the front end's address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RingAddresses {
  descriptor: u64,
  used: u64,
  available: u64,
}

/// One queue as the front end configured it.
#[derive(Debug, Default)]
struct Vring {
  size: Option<u16>,
  addresses: Option<RingAddresses>,
  base: u16,
  kick: Option<OwnedFd>,
  call: Option<OwnedFd>,
  enabled: bool,
}

/// Format: the payload of a vring-state message: index (`u32`) and number (`u32`).
const VRING_STATE_LEN: usize = 8;
/// Format: the payload of a vring-address message: index, flags (`u32` each), then the descriptor, used,
/// available and log addresses (`u64` each).
const VRING_ADDR_LEN: usize = 40;
/// Format: a `u64` payload or field.
const U64_LEN: usize = 8;
/// Format: a `u32` field.
const U32_LEN: usize = 4;
/// Format: the header's flags follow its request word.
const HEADER_AT_FLAGS: usize = 4;
/// Format: the header's payload size follows its flags.
const HEADER_AT_SIZE: usize = 8;
/// Format: a memory-table entry's size follows its guest address.
const REGION_AT_SIZE: usize = 8;
/// Format: a memory-table entry's front-end address follows its size.
const REGION_AT_USER: usize = 16;
/// Format: a memory-table entry's mapping offset follows its front-end address.
const REGION_AT_OFFSET: usize = 24;
/// Format: a vring-address payload's descriptor-table address follows its index and flags.
const VRING_AT_DESCRIPTOR: usize = 8;
/// Format: the used ring's address follows the descriptor table's.
const VRING_AT_USED: usize = 16;
/// Format: the available ring's address follows the used ring's.
const VRING_AT_AVAILABLE: usize = 24;
/// Format: virtio 1.2 §2.7.8 — the used ring's index follows its flags.
const USED_IDX_OFFSET: u64 = 2;

// ------------------------------------------------------------------------------------------ the seam

/// The back end of one vhost-user connection: the seam a device is admitted over.
pub struct VhostUserSeam {
  socket: OwnedFd,
  doorbell: OwnedFd,
  consumer: Principal,
  memory: GuestRegions,
  vrings: Vec<Vring>,
  features: u64,
  protocol_features: u64,
  inbound: Vec<u8>,
  inbound_fds: Vec<OwnedFd>,
  stopped: bool,
}

impl std::fmt::Debug for VhostUserSeam {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("VhostUserSeam")
      .field("consumer", &self.consumer)
      .field("queues", &self.vrings.len())
      .field("stopped", &self.stopped)
      .finish()
  }
}

/// Format: the epoll key of the control socket (every kick is keyed by its queue index, which is smaller).
const SOCKET_KEY: u64 = u64::MAX;

impl VhostUserSeam {
  /// Adopts `socket`, a connected stream socket whose other end is the VMM's vhost-user front end, for a
  /// device of `config`'s queues. The consumer is the peer's uid, read now, before any message.
  pub fn adopt(socket: OwnedFd, config: &DeviceConfig) -> Result<VhostUserSeam, VhostError> {
    let credentials =
      rustix::net::sockopt::socket_peercred(&socket).map_err(|_| VhostError::PeerUnverified)?;
    rustix::io::ioctl_fionbio(&socket, true).map_err(os("ioctl(FIONBIO)"))?;
    let doorbell = epoll::create(CreateFlags::CLOEXEC).map_err(os("epoll_create1"))?;
    epoll::add(
      &doorbell,
      &socket,
      EventData::new_u64(SOCKET_KEY),
      EventFlags::IN | EventFlags::RDHUP,
    )
    .map_err(os("epoll_ctl"))?;
    let queues = config.queue_count();
    Ok(VhostUserSeam {
      socket,
      doorbell,
      consumer: Principal::Uid {
        uid: credentials.uid.as_raw(),
      },
      memory: GuestRegions::default(),
      vrings: (0..queues).map(|_| Vring::default()).collect(),
      features: 0,
      protocol_features: 0,
      inbound: Vec::with_capacity(HEADER_LEN.saturating_add(MAX_PAYLOAD)),
      inbound_fds: Vec::new(),
      stopped: false,
    })
  }

  /// Whether the front end has configured every queue: memory mapped; each queue's size, addresses, kick and
  /// call given; and, with the protocol-features extension, each queue enabled.
  pub fn ready(&self) -> bool {
    let needs_enable = self.features & F_PROTOCOL_FEATURES != 0;
    !self.memory.is_empty()
      && self.vrings.iter().all(|vring| {
        vring.size.is_some()
          && vring.addresses.is_some()
          && vring.kick.is_some()
          && vring.call.is_some()
          && (vring.enabled || !needs_enable)
      })
  }

  /// The doorbell's descriptor number: readable when a control message or a kick is pending.
  pub fn doorbell_fd(&self) -> i32 {
    self.doorbell.as_raw_fd()
  }

  /// Serves every control message already received, without blocking. `Ok(false)` once the front end has
  /// closed its end or stopped a ring.
  pub fn serve_control(&mut self) -> Result<bool, VhostError> {
    loop {
      if self.stopped {
        return Ok(false);
      }
      if let Some((header, payload)) = self.next_message()? {
        self.handle(header, &payload)?;
        continue;
      }
      match self.receive()? {
        Received::Bytes => {}
        Received::Nothing => return Ok(true),
        Received::Closed => return Ok(false),
      }
    }
  }

  /// Negotiates until every queue is configured, awaiting the socket through the shard's driver. Refused
  /// typed when the front end closes, stops, or breaks the protocol first; the caller bounds the wait.
  pub async fn negotiate(mut self) -> Result<VhostUserSeam, VhostError> {
    loop {
      if !self.serve_control()? {
        return Err(VhostError::Closed);
      }
      if self.ready() {
        return Ok(self);
      }
      slates_rt::readiness::readable(self.doorbell_fd())
        .await
        .map_err(|_| VhostError::Os {
          call: "readiness",
          errno: 0,
        })?;
    }
  }

  // -------------------------------------------------------------------------------- the wire

  /// Receives what the socket holds now into the inbound buffer, with any descriptors.
  fn receive(&mut self) -> Result<Received, VhostError> {
    let mut chunk = [0u8; HEADER_LEN + MAX_PAYLOAD];
    let room = HEADER_LEN
      .saturating_add(MAX_PAYLOAD)
      .saturating_sub(self.inbound.len());
    if room == 0 {
      return Err(protocol(
        "a message exceeds the largest this back end accepts",
      ));
    }
    let take = chunk.get_mut(..room).ok_or(protocol("buffer"))?;
    let mut space =
      [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(MAX_REGIONS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let received = match rustix::net::recvmsg(
      &self.socket,
      &mut [std::io::IoSliceMut::new(take)],
      &mut control,
      RecvFlags::CMSG_CLOEXEC | RecvFlags::DONTWAIT,
    ) {
      Ok(received) => received,
      Err(Errno::AGAIN | Errno::INTR) => return Ok(Received::Nothing),
      Err(Errno::CONNRESET | Errno::PIPE | Errno::NOTCONN) => return Ok(Received::Closed),
      Err(e) => return Err(os("recvmsg")(e)),
    };
    for message in control.drain() {
      if let RecvAncillaryMessage::ScmRights(fds) = message {
        self.inbound_fds.extend(fds);
      }
    }
    if self.inbound_fds.len() > MAX_REGIONS {
      return Err(protocol("more descriptors than any message carries"));
    }
    if received.bytes == 0 {
      return Ok(Received::Closed);
    }
    let bytes = chunk
      .get(..received.bytes)
      .ok_or(protocol("short receive"))?;
    self.inbound.extend_from_slice(bytes);
    Ok(Received::Bytes)
  }

  /// The next whole message in the inbound buffer, removed from it.
  fn next_message(&mut self) -> Result<Option<(Header, Vec<u8>)>, VhostError> {
    let Some(head) = self.inbound.get(..HEADER_LEN) else {
      return Ok(None);
    };
    let header = Header::parse(head)?;
    let size = usize::try_from(header.size).map_err(|_| protocol("payload size"))?;
    if size > MAX_PAYLOAD {
      return Err(protocol(
        "a message exceeds the largest this back end accepts",
      ));
    }
    let total = HEADER_LEN.saturating_add(size);
    let Some(payload) = self.inbound.get(HEADER_LEN..total) else {
      return Ok(None);
    };
    let payload = payload.to_vec();
    self.inbound.drain(..total);
    Ok(Some((header, payload)))
  }

  /// Sends a reply to `header` with `payload`.
  fn reply(&self, header: Header, payload: &[u8]) -> Result<(), VhostError> {
    let size = u32::try_from(payload.len()).map_err(|_| protocol("reply size"))?;
    let mut message = Vec::with_capacity(HEADER_LEN.saturating_add(payload.len()));
    message.extend_from_slice(&header.request.to_le_bytes());
    message.extend_from_slice(&(VERSION | FLAG_REPLY).to_le_bytes());
    message.extend_from_slice(&size.to_le_bytes());
    message.extend_from_slice(payload);
    let mut control = SendAncillaryBuffer::default();
    let mut sent = 0usize;
    while sent < message.len() {
      let rest = message.get(sent..).ok_or(protocol("reply"))?;
      match rustix::net::sendmsg(
        &self.socket,
        &[std::io::IoSlice::new(rest)],
        &mut control,
        SendFlags::NOSIGNAL,
      ) {
        Ok(0) => return Err(VhostError::Closed),
        Ok(written) => sent = sent.saturating_add(written),
        Err(Errno::INTR) => {}
        // A reply is a few dozen bytes into an empty socket buffer: a full buffer means the front end has
        // stopped reading, which ends the device.
        Err(Errno::AGAIN) => return Err(protocol("the front end stopped reading replies")),
        Err(Errno::PIPE | Errno::CONNRESET) => return Err(VhostError::Closed),
        Err(e) => return Err(os("sendmsg")(e)),
      }
    }
    Ok(())
  }

  /// Answers the reply-ack a message asked for (`status` zero for success).
  fn acknowledge(&self, header: Header, status: u64) -> Result<(), VhostError> {
    if header.flags & FLAG_NEED_REPLY != 0 && self.protocol_features & PROTOCOL_F_REPLY_ACK != 0 {
      self.reply(header, &status.to_le_bytes())?;
    }
    Ok(())
  }

  /// Takes the descriptors the message carried.
  fn take_fds(&mut self) -> Vec<OwnedFd> {
    std::mem::take(&mut self.inbound_fds)
  }

  // -------------------------------------------------------------------------------- the requests

  /// Serves one message.
  fn handle(&mut self, header: Header, payload: &[u8]) -> Result<(), VhostError> {
    let outcome = self.serve_request(header, payload);
    // Descriptors a message carried and its handler did not keep are closed with it.
    drop(self.take_fds());
    match outcome {
      Ok(Answer::Replied) => Ok(()),
      Ok(Answer::Acknowledge) => self.acknowledge(header, 0),
      Err(error) => {
        let _ = self.acknowledge(header, 1);
        Err(error)
      }
    }
  }

  fn serve_request(&mut self, header: Header, payload: &[u8]) -> Result<Answer, VhostError> {
    match header.request {
      request::GET_FEATURES => self.answer_u64(header, OFFERED_FEATURES),
      request::SET_FEATURES => {
        let features = u64_of(payload)?;
        if features & !OFFERED_FEATURES != 0 {
          return Err(protocol("a feature that was not offered"));
        }
        self.features = features;
        Ok(Answer::Acknowledge)
      }
      request::GET_PROTOCOL_FEATURES => self.answer_u64(header, OFFERED_PROTOCOL_FEATURES),
      request::SET_PROTOCOL_FEATURES => {
        let features = u64_of(payload)?;
        if features & !OFFERED_PROTOCOL_FEATURES != 0 {
          return Err(protocol("a protocol feature that was not offered"));
        }
        self.protocol_features = features;
        Ok(Answer::Acknowledge)
      }
      request::GET_QUEUE_NUM => {
        let queues = u64::try_from(self.vrings.len()).map_err(|_| protocol("queues"))?;
        self.answer_u64(header, queues)
      }
      request::SET_OWNER | request::RESET_OWNER => Ok(Answer::Acknowledge),
      request::SET_MEM_TABLE => self.set_mem_table(payload),
      request::SET_VRING_NUM => {
        let (vring, num) = self.vring_state(payload)?;
        let size = u16::try_from(num).map_err(|_| protocol("a queue size past 16 bits"))?;
        vring.size = Some(size);
        Ok(Answer::Acknowledge)
      }
      request::SET_VRING_ADDR => self.set_vring_addr(payload),
      request::SET_VRING_BASE => {
        let (vring, num) = self.vring_state(payload)?;
        vring.base = u16::try_from(num).map_err(|_| protocol("a ring index past 16 bits"))?;
        Ok(Answer::Acknowledge)
      }
      request::GET_VRING_BASE => self.get_vring_base(header, payload),
      request::SET_VRING_KICK => self.set_vring_fd(payload, FdRole::Kick),
      request::SET_VRING_CALL => self.set_vring_fd(payload, FdRole::Call),
      request::SET_VRING_ERR => self.set_vring_fd(payload, FdRole::Error),
      request::SET_VRING_ENABLE => {
        let (vring, num) = self.vring_state(payload)?;
        vring.enabled = num != 0;
        Ok(Answer::Acknowledge)
      }
      _ => Err(protocol("a request this back end does not serve")),
    }
  }

  fn answer_u64(&self, header: Header, value: u64) -> Result<Answer, VhostError> {
    self.reply(header, &value.to_le_bytes())?;
    Ok(Answer::Replied)
  }

  /// The queue a vring-state payload names, and its number.
  fn vring_state(&mut self, payload: &[u8]) -> Result<(&mut Vring, u32), VhostError> {
    if payload.len() != VRING_STATE_LEN {
      return Err(protocol("a vring state of the wrong size"));
    }
    let index = u32_at(payload, 0)?;
    let num = u32_at(payload, U32_LEN)?;
    let vring = usize::try_from(index)
      .ok()
      .and_then(|index| self.vrings.get_mut(index))
      .ok_or(protocol("a queue index past the device's queues"))?;
    Ok((vring, num))
  }

  fn set_mem_table(&mut self, payload: &[u8]) -> Result<Answer, VhostError> {
    if !self.memory.is_empty() && self.vrings.iter().any(|vring| vring.kick.is_some()) {
      // Memory replaced under configured rings (hotplug) would move what the device's layouts name.
      return Err(protocol("the memory table changed under configured rings"));
    }
    let count = usize::try_from(u32_at(payload, 0)?).map_err(|_| protocol("region count"))?;
    if count == 0 || count > MAX_REGIONS {
      return Err(protocol(
        "a memory table of no regions or past the baseline",
      ));
    }
    let expected = MEM_TABLE_HEAD_LEN.saturating_add(count.saturating_mul(REGION_ENTRY_LEN));
    if payload.len() < expected {
      return Err(protocol("a memory table shorter than its region count"));
    }
    let fds = self.take_fds();
    if fds.len() != count {
      return Err(protocol(
        "a memory table whose descriptors do not match its regions",
      ));
    }
    let mut regions = Vec::with_capacity(count);
    for (position, fd) in fds.iter().enumerate() {
      let at = MEM_TABLE_HEAD_LEN.saturating_add(position.saturating_mul(REGION_ENTRY_LEN));
      let entry = RegionEntry {
        guest_phys: u64_at(payload, at)?,
        size: u64_at(payload, at.saturating_add(REGION_AT_SIZE))?,
        user_addr: u64_at(payload, at.saturating_add(REGION_AT_USER))?,
        offset: u64_at(payload, at.saturating_add(REGION_AT_OFFSET))?,
      };
      regions.push(MappedRegion::map(entry, fd)?);
    }
    self.memory = GuestRegions::new(regions)?;
    Ok(Answer::Acknowledge)
  }

  fn set_vring_addr(&mut self, payload: &[u8]) -> Result<Answer, VhostError> {
    if payload.len() != VRING_ADDR_LEN {
      return Err(protocol("a vring address of the wrong size"));
    }
    let index = usize::try_from(u32_at(payload, 0)?).map_err(|_| protocol("queue index"))?;
    let addresses = RingAddresses {
      descriptor: u64_at(payload, VRING_AT_DESCRIPTOR)?,
      used: u64_at(payload, VRING_AT_USED)?,
      available: u64_at(payload, VRING_AT_AVAILABLE)?,
    };
    let vring = self
      .vrings
      .get_mut(index)
      .ok_or(protocol("a queue index past the device's queues"))?;
    vring.addresses = Some(addresses);
    Ok(Answer::Acknowledge)
  }

  /// `GET_VRING_BASE`: the ring stops. Its position is the used ring's index (see the module's rules).
  fn get_vring_base(&mut self, header: Header, payload: &[u8]) -> Result<Answer, VhostError> {
    let (vring, _) = self.vring_state(payload)?;
    let used = vring.addresses.map(|addresses| addresses.used);
    let index = u32_at(payload, 0)?;
    let position = used
      .and_then(|used| self.memory.guest_of_user(used))
      .and_then(|used| GuestRange::new(GuestAddr(used.0.checked_add(USED_IDX_OFFSET)?), 2).ok())
      .and_then(|range| {
        let mut bytes = [0u8; 2];
        self.memory.read(range, &mut bytes).ok()?;
        Some(u16::from_le_bytes(bytes))
      })
      .unwrap_or(0);
    let mut reply = Vec::with_capacity(VRING_STATE_LEN);
    reply.extend_from_slice(&index.to_le_bytes());
    reply.extend_from_slice(&u32::from(position).to_le_bytes());
    self.reply(header, &reply)?;
    self.stopped = true;
    Ok(Answer::Replied)
  }

  fn set_vring_fd(&mut self, payload: &[u8], role: FdRole) -> Result<Answer, VhostError> {
    let word = u64_of(payload)?;
    let index = usize::try_from(word & VRING_INDEX_MASK).map_err(|_| protocol("queue index"))?;
    let fd = if word & VRING_NOFD == 0 {
      let mut fds = self.take_fds();
      if fds.len() != 1 {
        return Err(protocol(
          "a vring descriptor message without exactly one descriptor",
        ));
      }
      fds.pop()
    } else {
      None
    };
    if index >= self.vrings.len() {
      return Err(protocol("a queue index past the device's queues"));
    }
    match role {
      FdRole::Kick => {
        let fd = fd.ok_or(protocol(
          "a kick without a descriptor (polling is not served)",
        ))?;
        rustix::io::ioctl_fionbio(&fd, true).map_err(os("ioctl(FIONBIO)"))?;
        let key = u64::try_from(index).map_err(|_| protocol("queue index"))?;
        epoll::add(&self.doorbell, &fd, EventData::new_u64(key), EventFlags::IN)
          .map_err(os("epoll_ctl"))?;
        if let Some(vring) = self.vrings.get_mut(index)
          && let Some(old) = vring.kick.replace(fd)
        {
          // The registration is on the open file, which the front end still holds: remove it explicitly.
          let _ = epoll::delete(&self.doorbell, &old);
        }
      }
      FdRole::Call => {
        if let Some(fd) = &fd {
          rustix::io::ioctl_fionbio(fd, true).map_err(os("ioctl(FIONBIO)"))?;
        }
        if let Some(vring) = self.vrings.get_mut(index) {
          vring.call = fd;
        }
      }
      // The device never signals ring errors (a fault ends it); the descriptor is closed.
      FdRole::Error => drop(fd),
    }
    Ok(Answer::Acknowledge)
  }

  /// Consumes every kick pending on the queues' eventfds; whether any was pending.
  fn drain_kicks(&mut self) -> Result<bool, VhostError> {
    let mut kicked = false;
    for vring in &self.vrings {
      let Some(kick) = &vring.kick else { continue };
      let mut counter = [0u8; U64_LEN];
      match rustix::io::read(kick, &mut counter) {
        Ok(_) => kicked = true,
        Err(Errno::AGAIN | Errno::INTR) => {}
        Err(e) => return Err(os("read(kick)")(e)),
      }
    }
    Ok(kicked)
  }
}

/// What a request's handler answered.
enum Answer {
  /// It replied itself.
  Replied,
  /// It succeeded; a reply-ack, if asked for, reports success.
  Acknowledge,
}

/// Which vring descriptor a message sets.
#[derive(Clone, Copy)]
enum FdRole {
  Kick,
  Call,
  Error,
}

/// What one receive found.
enum Received {
  Bytes,
  Nothing,
  Closed,
}

/// A message header.
#[derive(Clone, Copy, Debug)]
struct Header {
  request: u32,
  flags: u32,
  size: u32,
}

impl Header {
  fn parse(bytes: &[u8]) -> Result<Header, VhostError> {
    let header = Header {
      request: u32_at(bytes, 0)?,
      flags: u32_at(bytes, HEADER_AT_FLAGS)?,
      size: u32_at(bytes, HEADER_AT_SIZE)?,
    };
    if header.flags & VERSION_MASK != VERSION {
      return Err(protocol("a protocol version this back end does not speak"));
    }
    if header.flags & FLAG_REPLY != 0 {
      return Err(protocol("a reply where a request belongs"));
    }
    Ok(header)
  }
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, VhostError> {
  let end = at.checked_add(U32_LEN).ok_or(protocol("field"))?;
  let field = bytes
    .get(at..end)
    .ok_or(protocol("a field past the payload"))?;
  let mut word = [0u8; 4];
  word.copy_from_slice(field);
  Ok(u32::from_le_bytes(word))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, VhostError> {
  let end = at.checked_add(U64_LEN).ok_or(protocol("field"))?;
  let field = bytes
    .get(at..end)
    .ok_or(protocol("a field past the payload"))?;
  let mut word = [0u8; U64_LEN];
  word.copy_from_slice(field);
  Ok(u64::from_le_bytes(word))
}

fn u64_of(payload: &[u8]) -> Result<u64, VhostError> {
  if payload.len() != U64_LEN {
    return Err(protocol("a u64 payload of the wrong size"));
  }
  u64_at(payload, 0)
}

impl VmmSeam for VhostUserSeam {
  fn consumer(&mut self) -> Result<Principal, SeamError> {
    Ok(self.consumer.clone())
  }

  fn memory(&mut self) -> Result<&mut dyn GuestMemory, SeamError> {
    if self.memory.is_empty() {
      return Err(SeamError::MemoryUnavailable);
    }
    Ok(&mut self.memory)
  }

  fn queues(&mut self) -> Result<Vec<QueueLayout>, SeamError> {
    if !self.ready() {
      return Err(SeamError::QueuesUnavailable);
    }
    self
      .vrings
      .iter()
      .map(|vring| {
        let (Some(size), Some(addresses)) = (vring.size, vring.addresses) else {
          return Err(SeamError::QueuesUnavailable);
        };
        // The device's ring position starts at zero; a ring resumed elsewhere is not this device's.
        if vring.base != 0 {
          return Err(SeamError::QueuesUnavailable);
        }
        let guest = |user| {
          self
            .memory
            .guest_of_user(user)
            .ok_or(SeamError::QueuesUnavailable)
        };
        Ok(QueueLayout {
          size,
          descriptor_table: guest(addresses.descriptor)?,
          available_ring: guest(addresses.available)?,
          used_ring: guest(addresses.used)?,
        })
      })
      .collect()
  }

  /// The tag and queue count are the front end's to present (`vhost-user-fs` carries them in the VMM's own
  /// configuration space); the harness gives the VMM the same tag it gave the daemon.
  fn publish(&mut self, _config: &DeviceConfig) -> Result<(), SeamError> {
    Ok(())
  }

  fn notify_used(&mut self, queue: u16) -> Result<(), SeamError> {
    let call = self
      .vrings
      .get(usize::from(queue))
      .and_then(|vring| vring.call.as_ref())
      .ok_or(SeamError::NotifyRefused { queue })?;
    match rustix::io::write(call, &1u64.to_le_bytes()) {
      // A saturated counter already holds a pending interrupt.
      Ok(_) | Err(Errno::AGAIN) => Ok(()),
      Err(_) => Err(SeamError::NotifyRefused { queue }),
    }
  }

  fn doorbell(&self) -> Doorbell {
    Doorbell::Descriptor(self.doorbell.as_raw_fd())
  }

  fn drain_doorbell(&mut self) -> Result<Drained, SeamError> {
    match self.serve_control() {
      Ok(true) => {}
      Ok(false) | Err(_) => return Ok(Drained::HungUp),
    }
    match self.drain_kicks() {
      Ok(true) => Ok(Drained::Kicked),
      Ok(false) => Ok(Drained::Nothing),
      Err(_) => Ok(Drained::HungUp),
    }
  }

  fn release(&mut self) {
    for vring in &mut self.vrings {
      if let Some(kick) = vring.kick.take() {
        let _ = epoll::delete(&self.doorbell, &kick);
      }
      vring.call = None;
    }
    self.memory = GuestRegions::default();
    self.stopped = true;
    let _ = rustix::net::shutdown(self.socket.as_fd(), rustix::net::Shutdown::Both);
  }
}

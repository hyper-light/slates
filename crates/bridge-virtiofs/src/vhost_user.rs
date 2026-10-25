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

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use rustix::event::epoll::{self, CreateFlags, EventData, EventFlags};
use rustix::io::Errno;
use rustix::net::{
  RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage,
  SendFlags,
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
  /// Format: `VHOST_USER_SET_BACKEND_REQ_FD` (formerly `SET_SLAVE_REQ_FD`) — the front end hands over the
  /// back-end-request socket in ancillary data, the channel the back end sends `SHMEM_MAP` on (§4.6 DAX).
  pub const SET_BACKEND_REQ_FD: u32 = 21;
  /// Format: `VHOST_USER_GET_CONFIG` — the front end reads the device's config space (the virtio-fs tag and
  /// queue count); offered via `PROTOCOL_F_CONFIG`.
  pub const GET_CONFIG: u32 = 24;
  /// Format: `VHOST_USER_SET_CONFIG` — a config-space write; the virtio-fs config is read-only, so it is refused.
  pub const SET_CONFIG: u32 = 25;
  /// Format: `VHOST_USER_GET_SHMEM_CONFIG` — the front end asks the back end how many VIRTIO shared-memory
  /// regions it needs and each size, to allocate the DAX cache BAR (crosvm's frontend calls this when `SHMEM`
  /// is negotiated, `devices/src/virtio/vhost_user_frontend/mod.rs::get_shared_memory_region`).
  pub const GET_SHMEM_CONFIG: u32 = 44;
}

/// Format: the back-end-request codes (`BackendReq`), a namespace distinct from the front-end `request` codes
/// above (a number means a different message on each channel — `9` is `SET_VRING_ADDR` from the front end and
/// `SHMEM_MAP` from the back end). As crosvm's `vmm_vhost::message::BackendReq` numbers them (re-checked against
/// the pinned VMM when the live guest runs); crosvm's older `FS_MAP` is `DEPRECATED__` and its `EXTERNAL_MAP`
/// (1007) is the in-process `ptr` form slates does not use — the out-of-process back end sends `SHMEM_MAP` (9)
/// with the arena descriptor, the standardized code QEMU and cloud-hypervisor also speak.
pub mod backend_request {
  /// Format: `VHOST_USER_BACKEND_SHMEM_MAP` — ask the front end to map a descriptor's pages into a VIRTIO
  /// shared-memory region (the DAX cache window) at a given offset.
  pub const SHMEM_MAP: u32 = 9;
  /// Format: `VHOST_USER_BACKEND_SHMEM_UNMAP` — ask the front end to remove a mapped range from the region.
  pub const SHMEM_UNMAP: u32 = 10;
}

/// Format: the `struct VhostUserMMap` body of a shmem map/unmap: `shmid` (`u8`), seven padding bytes, then
/// `fd_offset`, `shm_offset`, `len` and `flags` (four `u64`) — 40 bytes, as crosvm's
/// `vmm_vhost::message::VhostUserMMap` lays it out (`#[repr(C)]`, the u64s 8-aligned after the padded `shmid`).
pub const MMAP_LEN: usize = 40;
/// Format: the `shmid` field's byte offset in a `VhostUserMMap` body.
const MMAP_AT_SHMID: usize = 0;
/// Format: the `fd_offset` field's byte offset (after `shmid` and its seven padding bytes).
const MMAP_AT_FD_OFFSET: usize = 8;
/// Format: the `shm_offset` field's byte offset.
const MMAP_AT_SHM_OFFSET: usize = 16;
/// Format: the `len` field's byte offset.
const MMAP_AT_LEN: usize = 24;
/// Format: the `flags` field's byte offset.
const MMAP_AT_FLAGS: usize = 32;
/// Format: the `VhostUserMMap` flag bit asking for a read-write mapping (`MAP_RW`); clear means read-only.
pub const MMAP_FLAG_MAP_RW: u64 = 1;
/// Format: a back-end-request reply-ack's body — a single `u64` status, zero for success (`VHOST_USER_NEED_REPLY`).
pub const ACK_LEN: usize = 8;

/// Format: virtio's `VIRTIO_F_VERSION_1` feature bit.
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// Format: vhost-user's `VHOST_USER_F_PROTOCOL_FEATURES` feature bit.
pub const F_PROTOCOL_FEATURES: u64 = 1 << 30;
/// Format: the protocol feature `VHOST_USER_PROTOCOL_F_MQ`.
pub const PROTOCOL_F_MQ: u64 = 1 << 0;
/// Format: the protocol feature `VHOST_USER_PROTOCOL_F_REPLY_ACK`.
pub const PROTOCOL_F_REPLY_ACK: u64 = 1 << 3;
/// Format: the protocol feature `VHOST_USER_PROTOCOL_F_CONFIG` — the device configuration space, read with
/// `GET_CONFIG` (bit 9). A real VMM (crosvm/QEMU) reads the virtio-fs tag and queue count this way.
pub const PROTOCOL_F_CONFIG: u64 = 1 << 9;
/// Format: the protocol feature `VHOST_USER_PROTOCOL_F_BACKEND_REQ` — the back-end-request channel (bit 5).
pub const PROTOCOL_F_BACKEND_REQ: u64 = 1 << 5;
/// Format: the protocol feature `VHOST_USER_PROTOCOL_F_SHMEM` — VIRTIO shared-memory regions, the DAX cache (bit 22).
pub const PROTOCOL_F_SHMEM: u64 = 1 << 22;
/// The protocol features a DAX device offers in addition to the baseline: the back-end-request channel and the
/// shared-memory regions it needs to place content with `SHMEM_MAP`. Offered only when the device serves DAX, so a
/// non-DAX device and production (where DAX stays unadvertised, §4.6) offer exactly `OFFERED_PROTOCOL_FEATURES`.
pub const DAX_PROTOCOL_FEATURES: u64 = PROTOCOL_F_BACKEND_REQ | PROTOCOL_F_SHMEM;

/// Format: the shmid of the single VIRTIO shared-memory region slates uses — the DAX cache window. crosvm maps
/// exactly one region (it refuses more than one), so every `SHMEM_MAP` the back end sends names shmid 0.
pub const SHMEM_DAX_SHMID: u8 = 0;
/// Format: the byte offset of the `sizes` array in a `VhostUserShMemConfig` (after `nregions` u32 and padding u32).
const SHMEM_CONFIG_SIZES: usize = 8;
/// Format: the number of per-region sizes a `VhostUserShMemConfig` carries (`sizes[256]`, indexed by shmid), as
/// crosvm's `vmm_vhost::message::VhostUserShMemConfig` lays it out.
const SHMEM_CONFIG_REGIONS: usize = 256;
/// Format: a `VhostUserShMemConfig` reply's size — `nregions`/padding (8 bytes) then 256 `u64` sizes (2056 bytes).
const SHMEM_CONFIG_LEN: usize = SHMEM_CONFIG_SIZES + SHMEM_CONFIG_REGIONS * U64_LEN;
/// The device features offered: version 1 and the protocol-features extension (no event index, no indirect
/// descriptors: the virtqueue implements neither, `docs/wip/virtiofs.md` §6).
pub const OFFERED_FEATURES: u64 = VIRTIO_F_VERSION_1 | F_PROTOCOL_FEATURES;
/// The protocol features offered. Includes `CONFIG` so a real VMM can read the virtio-fs config space
/// (`GET_CONFIG`); the in-process form did not need it, which is why real-VMM interop was owed.
pub const OFFERED_PROTOCOL_FEATURES: u64 = PROTOCOL_F_MQ | PROTOCOL_F_REPLY_ACK | PROTOCOL_F_CONFIG;

/// Format: `struct virtio_fs_config` (§5.11.4) — the tag (36 bytes, NUL-padded) then `num_request_queues`
/// (`u32`, little-endian): 40 bytes. The guest reads it via `GET_CONFIG` to learn the mount tag and how many
/// request queues to set up (it adds the one high-priority queue itself).
const FS_CONFIG_LEN: usize = crate::device::TAG_LEN + U32_LEN;
/// Format: the offset of `num_request_queues` in `virtio_fs_config` — just past the 36-byte tag.
const FS_CONFIG_AT_QUEUES: usize = crate::device::TAG_LEN;
/// Format: a `vhost_user_config` message's fixed head — `offset`, `size`, `flags` (three `u32`) before the
/// `size` config bytes.
const CONFIG_MSG_HEAD_LEN: usize = 3 * U32_LEN;
/// Format: the byte offset of `offset` in a `vhost_user_config` head.
const CONFIG_AT_OFFSET: usize = 0;
/// Format: the byte offset of `size` in a `vhost_user_config` head.
const CONFIG_AT_SIZE: usize = U32_LEN;

/// Builds `struct virtio_fs_config` (§5.11.4): the mount `tag` (36 bytes, already NUL-padded) then
/// `num_request_queues` (`u32`, little-endian). 40 bytes, what the guest reads via `GET_CONFIG`.
fn fs_config_bytes(
  tag: &[u8; crate::device::TAG_LEN],
  num_request_queues: u32,
) -> [u8; FS_CONFIG_LEN] {
  let mut config = [0u8; FS_CONFIG_LEN];
  if let Some(field) = config.get_mut(0..crate::device::TAG_LEN) {
    field.copy_from_slice(tag);
  }
  if let Some(field) =
    config.get_mut(FS_CONFIG_AT_QUEUES..FS_CONFIG_AT_QUEUES.saturating_add(U32_LEN))
  {
    field.copy_from_slice(&num_request_queues.to_le_bytes());
  }
  config
}
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
pub const HEADER_AT_FLAGS: usize = 4;
/// Format: the header's payload size follows its flags.
pub const HEADER_AT_SIZE: usize = 8;
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
  /// The back-end-request channel the front end handed over `SET_BACKEND_REQ_FD`; present once DAX is negotiated.
  backend_req: Option<BackendReqChannel>,
  /// Whether this device serves DAX: it then offers `BACKEND_REQ | SHMEM` and accepts the back-end-request fd.
  /// Default false — production and non-DAX devices behave exactly as before (DAX stays unadvertised, §4.6).
  dax_enabled: bool,
  /// The DAX cache region's size to report to the front end, in bytes (derived by the caller; 0 when no DAX).
  dax_region_bytes: u64,
  /// The device's `virtio_fs_config` tag (36 bytes, NUL-padded), captured at adopt so `GET_CONFIG` can answer a
  /// real front end during negotiation (before the device is admitted and `publish` runs).
  config_tag: [u8; crate::device::TAG_LEN],
  /// The device's `num_request_queues`, captured at adopt for `GET_CONFIG` (the guest adds the high-priority queue).
  num_request_queues: u32,
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
    let consumer = Principal::Uid {
      uid: credentials.uid.as_raw(),
    };
    VhostUserSeam::adopt_for(socket, config, consumer)
  }

  /// Adopts `socket` as [`VhostUserSeam::adopt`] does, for a front end that holds the other end of a pair this process
  /// made and handed out (the attach verb's form, G-vhost). The peer's credentials there are this process's own, so the
  /// consumer is the one the caller authenticated before handing the end out, named here.
  pub fn adopt_for(
    socket: OwnedFd,
    config: &DeviceConfig,
    consumer: Principal,
  ) -> Result<VhostUserSeam, VhostError> {
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
      consumer,
      memory: GuestRegions::default(),
      vrings: (0..queues).map(|_| Vring::default()).collect(),
      features: 0,
      protocol_features: 0,
      inbound: Vec::with_capacity(HEADER_LEN.saturating_add(MAX_PAYLOAD)),
      inbound_fds: Vec::new(),
      backend_req: None,
      dax_enabled: false,
      dax_region_bytes: 0,
      config_tag: config.tag.config_bytes(),
      num_request_queues: config.num_request_queues.get(),
      stopped: false,
    })
  }

  /// Enables DAX for this device: it will offer `BACKEND_REQ | SHMEM`, accept the back-end-request channel over
  /// `SET_BACKEND_REQ_FD`, and (§4.6) ask the front end to place content blocks into a VIRTIO shared-memory region
  /// of `region_bytes`. Gated by the caller to the DAX-authorized path; a device that never calls this behaves
  /// exactly as a non-DAX one (production stays unadvertised). `region_bytes` is the caller's derived cache size.
  pub fn enable_dax(&mut self, region_bytes: u64) {
    self.dax_enabled = true;
    self.dax_region_bytes = region_bytes;
  }

  /// The protocol features offered: the baseline, plus the DAX features when this device serves DAX.
  fn offered_protocol_features(&self) -> u64 {
    if self.dax_enabled {
      OFFERED_PROTOCOL_FEATURES | DAX_PROTOCOL_FEATURES
    } else {
      OFFERED_PROTOCOL_FEATURES
    }
  }

  /// The back-end-request channel, once the front end has handed it over (DAX only); `None` otherwise. The device
  /// sends `SHMEM_MAP`/`SHMEM_UNMAP` on it and awaits each ack before answering the guest's `FUSE_SETUPMAPPING`.
  pub fn backend_req(&self) -> Option<&BackendReqChannel> {
    self.backend_req.as_ref()
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
  ///
  /// Never past the current message: the header alone, then exactly the payload it names. The descriptors a
  /// message carries arrive with its first byte, so reading one message at a time is what ties them to their
  /// message; a receive spanning two messages would deliver the second's descriptors while the first is
  /// served, which closed them (QEMU's back-to-back `SET_PROTOCOL_FEATURES` and `SET_MEM_TABLE`, 2026-10-01).
  fn receive(&mut self) -> Result<Received, VhostError> {
    let mut chunk = [0u8; HEADER_LEN + MAX_PAYLOAD];
    let wanted = match self.inbound.get(..HEADER_LEN) {
      None => HEADER_LEN,
      Some(head) => {
        let size =
          usize::try_from(Header::parse(head)?.size).map_err(|_| protocol("payload size"))?;
        if size > MAX_PAYLOAD {
          return Err(protocol(
            "a message exceeds the largest this back end accepts",
          ));
        }
        HEADER_LEN.saturating_add(size)
      }
    };
    let room = wanted.saturating_sub(self.inbound.len());
    if room == 0 {
      return Ok(Received::Bytes);
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
      request::GET_PROTOCOL_FEATURES => {
        let offered = self.offered_protocol_features();
        self.answer_u64(header, offered)
      }
      request::SET_PROTOCOL_FEATURES => {
        let features = u64_of(payload)?;
        if features & !self.offered_protocol_features() != 0 {
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
      request::SET_BACKEND_REQ_FD => self.set_backend_req_fd(),
      request::GET_SHMEM_CONFIG => self.get_shmem_config(header),
      request::GET_CONFIG => self.get_config(header, payload),
      request::SET_CONFIG => Err(protocol("the virtio-fs configuration space is read-only")),
      _ => Err(protocol("a request this back end does not serve")),
    }
  }

  /// Answers `GET_SHMEM_CONFIG` (§4.6 DAX): the back end reports the VIRTIO shared-memory regions it needs and
  /// each size, so the front end can allocate the DAX cache BAR. slates needs exactly one region — the cache
  /// window — of `dax_region_bytes` (crosvm refuses more than one; shmid 0 names it); a non-DAX device reports
  /// none. The reply is a `VhostUserShMemConfig`: region count (`u32`), padding (`u32`), then 256 `u64` sizes
  /// indexed by shmid.
  fn get_shmem_config(&self, header: Header) -> Result<Answer, VhostError> {
    let mut body = vec![0u8; SHMEM_CONFIG_LEN];
    if self.dax_enabled {
      if let Some(field) = body.get_mut(0..U32_LEN) {
        field.copy_from_slice(&1u32.to_le_bytes()); // one region: the DAX cache window
      }
      if let Some(field) =
        body.get_mut(SHMEM_CONFIG_SIZES..SHMEM_CONFIG_SIZES.saturating_add(U64_LEN))
      {
        field.copy_from_slice(&self.dax_region_bytes.to_le_bytes()); // sizes[0]
      }
    }
    self.reply(header, &body)?;
    Ok(Answer::Replied)
  }

  /// Answers `GET_CONFIG` (§5.11.4): the front end reads the device's `virtio_fs_config` — the mount tag and the
  /// request-queue count — during negotiation, before the device is admitted. The message is a `vhost_user_config`
  /// head (`offset`, `size`, `flags`); the reply echoes the head and carries the `size` config bytes at `offset`.
  /// A real VMM (crosvm/QEMU) needs this to learn the mount tag; the in-process sim did not. The config is
  /// [`FS_CONFIG_LEN`] bytes; a range past it is refused, so a confused front end cannot read beyond the space.
  fn get_config(&self, header: Header, payload: &[u8]) -> Result<Answer, VhostError> {
    if payload.len() < CONFIG_MSG_HEAD_LEN {
      return Err(protocol("a GET_CONFIG message shorter than its head"));
    }
    let offset = usize::try_from(u32_at(payload, CONFIG_AT_OFFSET)?)
      .map_err(|_| protocol("a GET_CONFIG offset"))?;
    let size = usize::try_from(u32_at(payload, CONFIG_AT_SIZE)?)
      .map_err(|_| protocol("a GET_CONFIG size"))?;
    let end = offset
      .checked_add(size)
      .ok_or(protocol("a GET_CONFIG range that overflows"))?;
    if end > FS_CONFIG_LEN {
      return Err(protocol(
        "a GET_CONFIG range past the virtio-fs config space",
      ));
    }
    let config = fs_config_bytes(&self.config_tag, self.num_request_queues);
    let mut body = vec![0u8; CONFIG_MSG_HEAD_LEN.saturating_add(size)];
    if let (Some(dst), Some(src)) = (
      body.get_mut(0..CONFIG_MSG_HEAD_LEN),
      payload.get(0..CONFIG_MSG_HEAD_LEN),
    ) {
      dst.copy_from_slice(src); // echo the head (offset, size, flags) back
    }
    if let (Some(dst), Some(src)) = (body.get_mut(CONFIG_MSG_HEAD_LEN..), config.get(offset..end)) {
      dst.copy_from_slice(src);
    }
    self.reply(header, &body)?;
    Ok(Answer::Replied)
  }

  /// Adopts the back-end-request socket the front end handed in ancillary data (`SET_BACKEND_REQ_FD`): the channel
  /// on which the device later sends `SHMEM_MAP`/`SHMEM_UNMAP` (§4.6 DAX). Refused unless DAX was negotiated — the
  /// feature, and so this request, is offered only then — and unless exactly one descriptor came with it.
  fn set_backend_req_fd(&mut self) -> Result<Answer, VhostError> {
    if !self.dax_enabled {
      return Err(protocol("a back-end-request fd without the shmem feature"));
    }
    let mut fds = self.take_fds();
    if fds.len() != 1 {
      return Err(protocol(
        "a back-end-request fd message without exactly one descriptor",
      ));
    }
    let socket = fds.pop().ok_or(protocol(
      "a back-end-request fd message without a descriptor",
    ))?;
    self.backend_req = Some(BackendReqChannel::adopt(socket));
    Ok(Answer::Acknowledge)
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

/// A vhost-user refusal as the device seam reports it, keeping the words the wire produced (§4.6, CLAUDE.md #9:
/// no refusal is swallowed). [`VhostError::Closed`] never reaches here — a clean close is a hang-up, not a refusal,
/// and its caller answers it before asking for this.
fn seam_refusal(error: VhostError) -> SeamError {
  match error {
    VhostError::PeerUnverified => SeamError::ConsumerUnverified,
    VhostError::Os { call, errno } => SeamError::SeamCallFailed { call, errno },
    VhostError::Protocol { reason } | VhostError::Memory { reason } => {
      SeamError::FrontEndBroke { reason }
    }
    VhostError::Table(_) => SeamError::FrontEndBroke {
      reason: "the memory table is inconsistent",
    },
    // A close reaching here would be a caller that did not answer it; report it as what it is rather than
    // inventing a protocol reason for it.
    VhostError::Closed => SeamError::FrontEndBroke {
      reason: "the front end closed its end",
    },
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

// --------------------------------------------------------------- the back-end-request channel (DAX, §4.6 A-9)

/// Encodes a `VhostUserMMap` body — the payload of `SHMEM_MAP` and `SHMEM_UNMAP`. `fd_offset` is the byte offset
/// of the mapped range within the supplied descriptor; `shm_offset` its byte offset within region `shmid`; `len`
/// its length; `writable` sets `MAP_RW`. An unmap carries `fd_offset = 0` and `writable = false` (the front end
/// reads only `shmid`, `shm_offset` and `len` for it). The seven bytes after `shmid` are padding, left zero.
fn encode_mmap(
  shmid: u8,
  fd_offset: u64,
  shm_offset: u64,
  len: u64,
  writable: bool,
) -> [u8; MMAP_LEN] {
  let mut body = [0u8; MMAP_LEN];
  if let Some(byte) = body.get_mut(MMAP_AT_SHMID) {
    *byte = shmid;
  }
  let flags = if writable { MMAP_FLAG_MAP_RW } else { 0 };
  let fields = [
    (MMAP_AT_FD_OFFSET, fd_offset.to_le_bytes()),
    (MMAP_AT_SHM_OFFSET, shm_offset.to_le_bytes()),
    (MMAP_AT_LEN, len.to_le_bytes()),
    (MMAP_AT_FLAGS, flags.to_le_bytes()),
  ];
  for (at, bytes) in fields {
    if let Some(field) = body.get_mut(at..at.saturating_add(bytes.len())) {
      field.copy_from_slice(&bytes);
    }
  }
  body
}

/// Encodes a back-end-request header: `request`, the flags (the version plus `NEED_REPLY`, since the back end
/// waits for the front end's reply-ack before it answers the guest), and the payload `size`. Laid out exactly as
/// [`Header::parse`] reads a header. `None` if `size` does not fit a `u32`.
fn encode_request_header(request: u32, size: usize) -> Option<[u8; HEADER_LEN]> {
  let size = u32::try_from(size).ok()?;
  let mut header = [0u8; HEADER_LEN];
  let fields = [
    (0usize, request.to_le_bytes()),
    (HEADER_AT_FLAGS, (VERSION | FLAG_NEED_REPLY).to_le_bytes()),
    (HEADER_AT_SIZE, size.to_le_bytes()),
  ];
  for (at, bytes) in fields {
    let field = header.get_mut(at..at.saturating_add(bytes.len()))?;
    field.copy_from_slice(&bytes);
  }
  Some(header)
}

/// Decodes a back-end-request reply-ack: a reply header (the version correct, `FLAG_REPLY` set) then a `u64`
/// status. Returns **which request the front end is answering** and its status; a malformed or wrong-sized reply
/// is a protocol refusal, never a panic.
///
/// The request code is returned rather than ignored because the caller must check it against the request it
/// actually sent. The channel is strictly in order, so for a conforming front end the code is redundant — and
/// for one that is not, it is the difference between completing the right outstanding request and the wrong one.
/// See [`MessagePlacer::poll_acks`] for what a mismatch would otherwise cost.
fn parse_ack(bytes: &[u8]) -> Result<(u32, u64), VhostError> {
  if bytes.len() != HEADER_LEN.saturating_add(ACK_LEN) {
    return Err(protocol("a back-end-request reply-ack of the wrong size"));
  }
  let flags = u32_at(bytes, HEADER_AT_FLAGS)?;
  if flags & VERSION_MASK != VERSION {
    return Err(protocol(
      "a reply-ack in a protocol version this back end does not speak",
    ));
  }
  if flags & FLAG_REPLY == 0 {
    return Err(protocol("a back-end-request reply without the reply flag"));
  }
  Ok((u32_at(bytes, 0)?, u64_at(bytes, HEADER_LEN)?))
}

/// Format: the one descriptor a `SHMEM_MAP` carries in ancillary data — the content arena's backing object.
pub const MAP_FDS: usize = 1;

/// The back-end-request channel (§4.6, A-9): the stream socket the front end hands the back end over
/// `VHOST_USER_SET_BACKEND_REQ_FD`, on which the back end asks the front end to place a content block's pages into
/// a VIRTIO shared-memory region with [`BackendReqChannel::send_map`] (`SHMEM_MAP`) and to remove them with
/// [`BackendReqChannel::send_unmap`] (`SHMEM_UNMAP`). This is the **message-based** DAX delivery an out-of-process
/// VMM (crosvm `--vhost-user-fs`, QEMU `vhost-user-fs-pci`, cloud-hypervisor) uses: the back end cannot reach the
/// guest's BAR itself, so it hands the front end the arena descriptor
/// ([`slates_bridge_core::Bridge::dax_backing`]) and the front end `mmap`s it into the region. (The fd-sharing
/// form, where the back end maps a window it was handed, is [`crate::dax::DaxCacheRegion`], used by an in-process
/// VMM like libkrun.)
///
/// A map carries `NEED_REPLY`, so the front end acknowledges once the mapping is live; the caller reads the ack
/// with [`parse_ack`] before answering the guest's `FUSE_SETUPMAPPING`, so the guest never faults a page the front
/// end has not yet mapped. Awaiting that ack over the shard's driver is the async-wiring step; the send below is
/// the wire.
pub struct BackendReqChannel {
  /// The socket to the front end's back-end-request handler.
  socket: OwnedFd,
}

impl std::fmt::Debug for BackendReqChannel {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("BackendReqChannel").finish()
  }
}

impl BackendReqChannel {
  /// Adopts `socket`, the back-end-request socket the front end handed over `SET_BACKEND_REQ_FD`.
  pub fn adopt(socket: OwnedFd) -> BackendReqChannel {
    BackendReqChannel { socket }
  }

  /// This end's descriptor, for registering the ack's readiness with the shard's driver.
  pub fn fd(&self) -> BorrowedFd<'_> {
    self.socket.as_fd()
  }

  /// Asks the front end to map `len` bytes of `fd` (from `fd_offset`) into region `shmid` at `shm_offset`,
  /// read-write when `writable`. Sends `SHMEM_MAP` with the descriptor in ancillary data and `NEED_REPLY` set; the
  /// ack is read separately. The front end expects the whole message, with its descriptor, in one `sendmsg`.
  pub fn send_map(
    &self,
    shmid: u8,
    shm_offset: u64,
    fd: BorrowedFd<'_>,
    fd_offset: u64,
    len: u64,
    writable: bool,
  ) -> Result<(), VhostError> {
    let header = encode_request_header(backend_request::SHMEM_MAP, MMAP_LEN)
      .ok_or(protocol("a shmem-map payload larger than a u32"))?;
    let body = encode_mmap(shmid, fd_offset, shm_offset, len, writable);
    self.send(&header, &body, Some(fd))
  }

  /// Asks the front end to unmap `len` bytes at `shm_offset` in region `shmid` (`SHMEM_UNMAP`, no descriptor).
  pub fn send_unmap(&self, shmid: u8, shm_offset: u64, len: u64) -> Result<(), VhostError> {
    let header = encode_request_header(backend_request::SHMEM_UNMAP, MMAP_LEN)
      .ok_or(protocol("a shmem-unmap payload larger than a u32"))?;
    let body = encode_mmap(shmid, 0, shm_offset, len, false);
    self.send(&header, &body, None)
  }

  /// Reads the front end's reply-ack for the oldest unanswered map or unmap, without blocking. `Ok(Some((request,
  /// status)))` once an ack has arrived — the request it answers and the front end's status, zero for success —
  /// and `Ok(None)` while none has (the caller awaits this channel's readiness on the shard's driver and asks
  /// again). The back end reads this before it answers the guest's `FUSE_SETUPMAPPING`, so the guest never faults
  /// a page the front end has not mapped. A reply-ack carries no descriptor.
  pub fn recv_ack(&self) -> Result<Option<(u32, u64)>, VhostError> {
    let mut buf = [0u8; HEADER_LEN + ACK_LEN];
    let mut none: [std::mem::MaybeUninit<u8>; 0] = [];
    let mut control = RecvAncillaryBuffer::new(&mut none);
    let received = match rustix::net::recvmsg(
      &self.socket,
      &mut [std::io::IoSliceMut::new(&mut buf)],
      &mut control,
      RecvFlags::DONTWAIT,
    ) {
      Ok(received) => received,
      Err(Errno::AGAIN | Errno::INTR) => return Ok(None),
      Err(Errno::CONNRESET | Errno::PIPE | Errno::NOTCONN) => return Err(VhostError::Closed),
      Err(e) => return Err(os("recvmsg")(e)),
    };
    if received.bytes == 0 {
      return Err(VhostError::Closed);
    }
    if received.bytes != buf.len() {
      return Err(protocol("a short back-end-request reply-ack"));
    }
    parse_ack(&buf).map(Some)
  }

  /// Sends one back-end request — header then body, with `fd` in ancillary data on the `sendmsg` that carries the
  /// message's first byte (the protocol attaches descriptors there). The message is 52 bytes into the front end's
  /// socket, so a short write means the front end has stopped reading (a typed refusal, never a split that would
  /// strand the descriptor). The ancillary buffer is rebuilt each attempt, so an `INTR` retry re-sends the
  /// descriptor.
  fn send(
    &self,
    header: &[u8; HEADER_LEN],
    body: &[u8; MMAP_LEN],
    fd: Option<BorrowedFd<'_>>,
  ) -> Result<(), VhostError> {
    let mut message = [0u8; HEADER_LEN + MMAP_LEN];
    if let Some(slot) = message.get_mut(..HEADER_LEN) {
      slot.copy_from_slice(header);
    }
    if let Some(slot) = message.get_mut(HEADER_LEN..) {
      slot.copy_from_slice(body);
    }
    let held = fd.map(|fd| [fd]);
    let mut space =
      [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(MAP_FDS))];
    loop {
      let mut control = SendAncillaryBuffer::new(&mut space);
      let pushed = match held.as_ref() {
        Some(held) => control.push(SendAncillaryMessage::ScmRights(held)),
        None => true,
      };
      if !pushed {
        return Err(protocol(
          "the map descriptor did not fit the ancillary buffer",
        ));
      }
      match rustix::net::sendmsg(
        &self.socket,
        &[std::io::IoSlice::new(&message)],
        &mut control,
        SendFlags::NOSIGNAL,
      ) {
        Ok(sent) if sent == message.len() => return Ok(()),
        Ok(_) => {
          return Err(protocol(
            "the front end did not take a whole back-end request",
          ));
        }
        Err(Errno::INTR) => {}
        Err(Errno::AGAIN) => {
          return Err(protocol("the front end stopped reading back-end requests"));
        }
        Err(Errno::PIPE | Errno::CONNRESET | Errno::NOTCONN) => return Err(VhostError::Closed),
        Err(e) => return Err(os("sendmsg")(e)),
      }
    }
  }
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

  /// Hands the admitted device the back-end-request channel for message-based DAX (§4.6): a DAX device that
  /// negotiated the channel gives it up here, with the cache region's size, so admission builds the device's
  /// [`crate::dax::DaxPlacer::Message`]. A non-DAX device (and the default) has none, so DAX stays unserved.
  fn dax_message_channel(&mut self) -> Option<(BackendReqChannel, u64)> {
    if self.dax_enabled {
      self
        .backend_req
        .take()
        .map(|channel| (channel, self.dax_region_bytes))
    } else {
      None
    }
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
    // A clean close is a hang-up; anything else is the refusal the wire already named, reported as one. Collapsing
    // both into `HungUp` (the shape until 2026-10-18) told whoever reads the end record that a guest shut down when
    // in fact a VMM had sent a message past the back end's cap or a vring address of the wrong size — a reason this
    // layer had produced and then dropped. The serve loop already carries a seam refusal
    // (`EndReason::Faulted(ServeError::Seam(..))`); it was never given one.
    match self.serve_control() {
      Ok(true) => {}
      Ok(false) | Err(VhostError::Closed) => return Ok(Drained::HungUp),
      Err(refused) => return Err(seam_refusal(refused)),
    }
    match self.drain_kicks() {
      Ok(true) => Ok(Drained::Kicked),
      Ok(false) => Ok(Drained::Nothing),
      Err(VhostError::Closed) => Ok(Drained::HungUp),
      Err(refused) => Err(seam_refusal(refused)),
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

/// Which placement an outstanding back-end request is, so its reply-ack completes the right action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaxOp {
  /// A `SHMEM_MAP` (FUSE_SETUPMAPPING): its ack lets the guest be told the window is live.
  Map,
  /// A `SHMEM_UNMAP` (FUSE_REMOVEMAPPING, or a content-path teardown): its ack is the signal the front end has
  /// removed the window from the guest's BAR, so the block may be unpinned (§4.6 phase two).
  Unmap,
}

/// A back-end request sent and awaiting the front end's reply-ack. Replies arrive in send order on the channel, so
/// these are matched FIFO. `token` is what the caller acts on when the ack lands (the device threads its DAX window
/// entry through here).
#[derive(Clone, Copy, Debug)]
struct OutstandingDax<T: Copy> {
  op: DaxOp,
  token: T,
}

/// What a drained reply-ack completed — the action the device loop now takes. `token` is the request's caller value
/// (the device's DAX window entry). A map records the window and answers the guest; a refused map unpins and
/// answers an errno; an unmap unpins the block (§4.6 phase two); a refused unmap is a front-end protocol fault.
#[derive(Clone, Copy, Debug)]
pub enum DaxCompletion<T: Copy> {
  /// The front end mapped the window: the caller records it and answers the guest's SETUPMAPPING with success.
  Mapped(T),
  /// The front end refused the map: the caller unpins the block (no window was established) and answers an errno.
  MapRefused {
    /// The request's token.
    token: T,
    /// The front end's non-zero reply-ack status.
    status: u64,
  },
  /// The front end removed the window: the caller unpins the block (phase two — safe now the guest cannot reach it).
  Unmapped(T),
  /// The front end refused an unmap: a protocol fault; the block stays pinned and the caller faults the device.
  UnmapRefused {
    /// The request's token.
    token: T,
    /// The front end's non-zero reply-ack status.
    status: u64,
  },
}

/// Why a message-based placement was refused (a typed refusal, never a panic).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessagePlaceError {
  /// The `[moffset, moffset+len)` range was zero, not a whole number of granules, or outside the cache region.
  BadRange,
  /// The outstanding-request ring is at its derived ceiling, so a confused or hostile guest cannot grow it without
  /// bound (banned: unbounded growth). `bound` is the region's whole-page capacity plus one in-flight map.
  TooManyOutstanding {
    /// The ceiling the ring reached.
    bound: usize,
  },
  /// The back-end-request channel refused the send, or reported a closed or garbled reply.
  Channel(VhostError),
}

impl std::fmt::Display for MessagePlaceError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::BadRange => {
        f.write_str("a DAX map range was zero, unaligned, or outside the cache region")
      }
      Self::TooManyOutstanding { bound } => {
        write!(
          f,
          "the back-end-request ring is full at its bound of {bound}"
        )
      }
      Self::Channel(e) => write!(f, "the back-end-request channel refused: {e}"),
    }
  }
}

impl std::error::Error for MessagePlaceError {}

/// The **message-based** DAX placer (§4.6, A-9): the out-of-process form of placing content blocks into a guest's
/// DAX window. Unlike the fd-sharing [`crate::dax::DaxCacheRegion`] (where the back end owns the window and overlays
/// blocks itself, for an in-process VMM like libkrun), here the back end does not own the window — it asks the front
/// end (crosvm `--vhost-user-fs`, QEMU, cloud-hypervisor) to place each block over the [`BackendReqChannel`]: a
/// SETUPMAPPING is a [`BackendReqChannel::send_map`] whose reply-ack the device waits for before answering the guest
/// (so the guest never faults a page the front end has not mapped), and a REMOVEMAPPING or a content-path teardown
/// is a [`BackendReqChannel::send_unmap`] whose ack is the signal the window is gone, so the block may be unpinned —
/// never before, or a stale guest page table could reach a reused block (§4.6 #1, phase two).
///
/// Requests are pipelined (sent as they arise) and their reply-acks matched FIFO, since the channel replies in send
/// order. [`MessagePlacer::poll_acks`] drains the acks when the channel is readable and returns the completions the
/// device acts on. `T` is the per-request token the device threads through (its DAX window entry); it is generic so
/// the placer's wire and FIFO are proven by use with a plain marker, independent of the un-fabricable gate mapping.
pub struct MessagePlacer<T: Copy> {
  /// The back-end-request socket to the front end.
  channel: BackendReqChannel,
  /// The granule a map offset and length must be a multiple of (the host page, or a negotiated FUSE_MAP_ALIGNMENT),
  /// matching the fd-sharing placer's [`crate::dax::DaxCacheRegion::page`].
  page: usize,
  /// The cache region's size in bytes (the window the front end advertised): a map must lie inside it.
  region_bytes: u64,
  /// Requests sent and awaiting the front end's reply-ack, in send order (acks arrive in that order, matched FIFO).
  /// Bounded by [`MessagePlacer::outstanding_bound`] so growth is never unbounded.
  outstanding: std::collections::VecDeque<OutstandingDax<T>>,
}

impl<T: Copy> std::fmt::Debug for MessagePlacer<T> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MessagePlacer")
      .field("region_bytes", &self.region_bytes)
      .field("outstanding", &self.outstanding.len())
      .finish()
  }
}

impl<T: Copy> MessagePlacer<T> {
  /// Adopts `channel` as the device's message-based DAX placer for a cache region of `region_bytes` whose map
  /// granule is `page` (clamped to at least one, so the modulo and bound are always well-defined).
  pub fn new(channel: BackendReqChannel, page: usize, region_bytes: u64) -> MessagePlacer<T> {
    MessagePlacer {
      channel,
      page: page.max(1),
      region_bytes,
      outstanding: std::collections::VecDeque::new(),
    }
  }

  /// The map granule (as [`crate::dax::DaxCacheRegion::page`]).
  #[must_use]
  pub fn page(&self) -> usize {
    self.page
  }

  /// The descriptor reply-acks arrive on, for registering readiness with the shard's driver (the same edge the
  /// doorbell is awaited on).
  #[must_use]
  pub fn ack_fd(&self) -> BorrowedFd<'_> {
    self.channel.fd()
  }

  /// Whether any request still awaits a reply-ack — the loop keeps draining while true, and the terminal step
  /// drains to empty before it unpins the last block.
  #[must_use]
  pub fn has_outstanding(&self) -> bool {
    !self.outstanding.is_empty()
  }

  /// The raw descriptor reply-acks arrive on, for awaiting readiness on the shard's driver. A `RawFd` (not a
  /// borrow), so nothing of the placer is held across the loop's `.await`.
  #[must_use]
  pub fn ack_raw_fd(&self) -> std::os::fd::RawFd {
    std::os::fd::AsRawFd::as_raw_fd(&self.channel.fd())
  }

  /// Drains every still-outstanding request, clearing the ring, as `(is_map, token)` pairs. The terminal step
  /// takes these to release the blocks whose reply-ack never arrived (the front end is gone): a map's pin is
  /// undone, an unmap's block is unpinned (§4.6 phase two), so no block is left pinned after the device ends.
  pub fn take_pending(&mut self) -> Vec<(bool, T)> {
    self
      .outstanding
      .drain(..)
      .map(|op| (matches!(op.op, DaxOp::Map), op.token))
      .collect()
  }

  /// The granule as a `u64` (lossless; `usize` is at most 64-bit on every target slates builds for).
  fn page_bytes(&self) -> u64 {
    u64::try_from(self.page).unwrap_or(u64::MAX)
  }

  /// Derived: the most requests outstanding at once — one per whole page of the cache region (the most windows that
  /// can be live, each awaiting at most one unmap) plus one in-flight map while a teardown's unmaps drain. A request
  /// past this is refused, never grown (banned: unbounded growth).
  fn outstanding_bound(&self) -> usize {
    let per_page = self
      .region_bytes
      .checked_div(self.page_bytes())
      .unwrap_or(0);
    let pages = usize::try_from(per_page).unwrap_or(usize::MAX);
    pages.saturating_add(1)
  }

  /// Checks a guest's window range `[moffset, moffset+len)` against the region and granule before anything is asked for
  /// it: non-empty, whole granules, inside the region (defence in depth; the front end bounds it against the BAR too).
  pub fn check_window(&self, moffset: u64, len: u64) -> Result<(), MessagePlaceError> {
    let page = self.page_bytes();
    if len == 0 || !moffset.is_multiple_of(page) || !len.is_multiple_of(page) {
      return Err(MessagePlaceError::BadRange);
    }
    let end = moffset
      .checked_add(len)
      .ok_or(MessagePlaceError::BadRange)?;
    if end > self.region_bytes {
      return Err(MessagePlaceError::BadRange);
    }
    Ok(())
  }

  /// Refuses when the outstanding ring is already at its bound.
  fn room(&self) -> Result<(), MessagePlaceError> {
    let bound = self.outstanding_bound();
    if self.outstanding.len() >= bound {
      return Err(MessagePlaceError::TooManyOutstanding { bound });
    }
    Ok(())
  }

  /// Sends a `SHMEM_MAP` placing `len` bytes of `fd` (from `fd_offset`) into the window at `moffset`, read-write per
  /// `writable`, and records the request awaiting the front end's ack under `token`. The guest's SETUPMAPPING is
  /// answered only when [`MessagePlacer::poll_acks`] yields this request's [`DaxCompletion::Mapped`] (or
  /// `MapRefused`). The send happens before the request is recorded, so a failed send records nothing and the FIFO
  /// stays consistent with the wire.
  pub fn begin_map(
    &mut self,
    moffset: u64,
    len: u64,
    fd: BorrowedFd<'_>,
    fd_offset: u64,
    writable: bool,
    token: T,
  ) -> Result<(), MessagePlaceError> {
    self.check_window(moffset, len)?;
    self.room()?;
    self
      .channel
      .send_map(SHMEM_DAX_SHMID, moffset, fd, fd_offset, len, writable)
      .map_err(MessagePlaceError::Channel)?;
    self.outstanding.push_back(OutstandingDax {
      op: DaxOp::Map,
      token,
    });
    Ok(())
  }

  /// Sends a `SHMEM_UNMAP` removing the window at `[moffset, moffset+len)` and records the request awaiting the ack
  /// under `token`. The block stays pinned until [`MessagePlacer::poll_acks`] yields this request's
  /// [`DaxCompletion::Unmapped`], at which point the caller unpins it (§4.6 phase two). Send-before-record, as
  /// [`MessagePlacer::begin_map`].
  pub fn begin_unmap(&mut self, moffset: u64, len: u64, token: T) -> Result<(), MessagePlaceError> {
    self.check_window(moffset, len)?;
    self.room()?;
    self
      .channel
      .send_unmap(SHMEM_DAX_SHMID, moffset, len)
      .map_err(MessagePlaceError::Channel)?;
    self.outstanding.push_back(OutstandingDax {
      op: DaxOp::Unmap,
      token,
    });
    Ok(())
  }

  /// Drains every reply-ack the front end has sent (non-blocking), completing the matching requests FIFO, and
  /// returns the completions in the order the front end acknowledged — for the device loop to act on. Stops at the
  /// first ack not yet arrived; the loop awaits [`MessagePlacer::ack_fd`] readiness and drains again. A closed or
  /// garbled channel, or a reply with no request outstanding, is a typed refusal.
  pub fn poll_acks(&mut self) -> Result<Vec<DaxCompletion<T>>, MessagePlaceError> {
    let mut completed = Vec::new();
    loop {
      let (request, status) = match self.channel.recv_ack() {
        Ok(None) => break,
        Ok(Some(answered)) => answered,
        Err(e) => return Err(MessagePlaceError::Channel(e)),
      };
      let waiting = self
        .outstanding
        .front()
        .ok_or(MessagePlaceError::Channel(protocol(
          "a back-end-request reply-ack with no request outstanding",
        )))?;
      let expected = match waiting.op {
        DaxOp::Map => backend_request::SHMEM_MAP,
        DaxOp::Unmap => backend_request::SHMEM_UNMAP,
      };
      // The channel answers in order, so for a conforming front end this always holds. For one that does not,
      // it is the whole of the safety: completing a map's ack against an outstanding *unmap* would report that
      // window gone and the caller would unpin a block the guest still has in its page tables — the
      // use-after-free the pin discipline exists to prevent (§4.6, A-99). A mismatch is the front end breaking
      // the wire, which ends the device; it is never completed as though it answered the request we sent.
      if request != expected {
        return Err(MessagePlaceError::Channel(protocol(
          "a reply-ack answering a back-end request other than the one awaiting it",
        )));
      }
      let op = self
        .outstanding
        .pop_front()
        .ok_or(MessagePlaceError::Channel(protocol(
          "a back-end-request reply-ack with no request outstanding",
        )))?;
      completed.push(match (op.op, status) {
        (DaxOp::Map, 0) => DaxCompletion::Mapped(op.token),
        (DaxOp::Unmap, 0) => DaxCompletion::Unmapped(op.token),
        (DaxOp::Map, status) => DaxCompletion::MapRefused {
          token: op.token,
          status,
        },
        (DaxOp::Unmap, status) => DaxCompletion::UnmapRefused {
          token: op.token,
          status,
        },
      });
    }
    Ok(completed)
  }
}

#[cfg(test)]
mod shmem_tests {
  //! The back-end-request channel's wire, by use (§4.6, A-9, AC-4.12): a `SHMEM_MAP` body matches crosvm's
  //! `VhostUserMMap` byte for byte, and a real descriptor plus the map fields cross a socket to a stand-in front
  //! end, which reads the block's bytes back through the descriptor it received and acknowledges — the
  //! message-based DAX wire, end to end, with no VMM.
  #![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cognitive_complexity
  )]

  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

  use rustix::fs::{MemfdFlags, memfd_create};
  use rustix::net::{AddressFamily, SocketFlags, SocketType};

  use super::*;

  /// A socketpair of stream sockets (the front-end end, then the back-end end).
  /// Shape: the DAX cache window these tests give a device (64 MiB), large enough that no case here is refused for
  /// want of a window rather than for the rule under test.
  const DAX_REGION_FOR_TESTS: u64 = 0x0400_0000;

  fn stream_pair() -> (OwnedFd, OwnedFd) {
    rustix::net::socketpair(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC,
      None,
    )
    .unwrap()
  }

  /// Sends a bare front-end request (`request`, version flags, `size`) with an optional descriptor in ancillary
  /// data, the way a VMM front end drives the control socket.
  fn send_front_message(socket: &OwnedFd, request: u32, size: u32, fd: Option<BorrowedFd<'_>>) {
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&request.to_le_bytes());
    header[HEADER_AT_FLAGS..HEADER_AT_FLAGS + 4].copy_from_slice(&VERSION.to_le_bytes());
    header[HEADER_AT_SIZE..HEADER_AT_SIZE + 4].copy_from_slice(&size.to_le_bytes());
    let held = fd.map(|f| [f]);
    let mut space =
      [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(MAP_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if let Some(held) = held.as_ref() {
      assert!(control.push(SendAncillaryMessage::ScmRights(held)));
    }
    rustix::net::sendmsg(
      socket,
      &[std::io::IoSlice::new(&header)],
      &mut control,
      SendFlags::NOSIGNAL,
    )
    .unwrap();
  }

  /// Sends raw bytes at the front end's socket, for inputs no well-formed sender would produce.
  fn send_front_raw(socket: &OwnedFd, bytes: &[u8]) {
    rustix::net::sendmsg(
      socket,
      &[std::io::IoSlice::new(bytes)],
      &mut SendAncillaryBuffer::default(),
      SendFlags::NOSIGNAL,
    )
    .unwrap();
  }

  /// A header with arbitrary request, flags and size — including combinations the protocol forbids.
  fn raw_header(request: u32, flags: u32, size: u32) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&request.to_le_bytes());
    header[HEADER_AT_FLAGS..HEADER_AT_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
    header[HEADER_AT_SIZE..HEADER_AT_SIZE + 4].copy_from_slice(&size.to_le_bytes());
    header
  }

  /// Hostile input on the front-end wire (AC-4.12; CLAUDE.md's "hostile-input tests on every parser of external
  /// bytes"). The VMM is the only writer of these bytes and slates does not control it, so every malformed shape a
  /// front end can produce is driven here against a real socket. Do: send each malformed message at a fresh device
  /// and drain its doorbell the way the serve loop does. Expect: a typed refusal naming the rule broken — not a
  /// hang-up, not a panic, not an answer — and no back-end channel established by any of them.
  ///
  /// The reasons are asserted verbatim because they are what reaches the operator through
  /// `EndReason::Faulted(ServeError::Seam(..))`; a refusal that loses its reason is the defect this test was
  /// written with (`SeamError::FrontEndBroke`, 2026-10-18).
  #[test]
  fn a_hostile_front_end_is_refused_by_name_and_establishes_nothing() {
    let cap = u32::try_from(MAX_PAYLOAD).unwrap();
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
      (
        "a payload size of u32::MAX",
        raw_header(request::SET_MEM_TABLE, VERSION, u32::MAX).to_vec(),
        "a message exceeds the largest this back end accepts",
      ),
      (
        "a payload one byte past the cap",
        raw_header(request::SET_MEM_TABLE, VERSION, cap.saturating_add(1)).to_vec(),
        "a message exceeds the largest this back end accepts",
      ),
      (
        "a request this back end does not know",
        raw_header(0xDEAD_BEEF, VERSION, 0).to_vec(),
        "a request this back end does not serve",
      ),
      (
        "a reply where a request belongs",
        raw_header(request::GET_FEATURES, VERSION | FLAG_REPLY, 0).to_vec(),
        "a reply where a request belongs",
      ),
      (
        "a protocol version this back end does not speak",
        raw_header(request::GET_FEATURES, 2, 0).to_vec(),
        "a protocol version this back end does not speak",
      ),
      (
        "a vring address of the wrong size",
        {
          let mut m = raw_header(request::SET_VRING_ADDR, VERSION, 4).to_vec();
          m.extend_from_slice(&[0xFF; 4]);
          m
        },
        "a vring address of the wrong size",
      ),
      (
        "a memory table claiming regions it did not send",
        {
          let mut m = raw_header(request::SET_MEM_TABLE, VERSION, 8).to_vec();
          m.extend_from_slice(&u32::MAX.to_le_bytes());
          m.extend_from_slice(&0u32.to_le_bytes());
          m
        },
        "a memory table of no regions or past the baseline",
      ),
      (
        "a config read with no fields",
        raw_header(request::GET_CONFIG, VERSION, 0).to_vec(),
        "a GET_CONFIG message shorter than its head",
      ),
      (
        "a vring state with no payload",
        raw_header(request::SET_VRING_NUM, VERSION, 0).to_vec(),
        "a vring state of the wrong size",
      ),
      (
        "a queue index past every queue",
        {
          let mut m = raw_header(request::SET_VRING_NUM, VERSION, 8).to_vec();
          m.extend_from_slice(&u32::MAX.to_le_bytes());
          m.extend_from_slice(&u32::MAX.to_le_bytes());
          m
        },
        "a queue index past the device's queues",
      ),
    ];
    for (name, bytes, reason) in cases {
      let (frontend, backend) = stream_pair();
      let config = crate::device::DeviceConfig::new(crate::device::FsTag::new("slates").unwrap());
      let mut seam = VhostUserSeam::adopt(backend, &config).unwrap();
      seam.enable_dax(DAX_REGION_FOR_TESTS);
      send_front_raw(&frontend, &bytes);
      drop(frontend);
      assert_eq!(
        seam.drain_doorbell(),
        Err(SeamError::FrontEndBroke { reason }),
        "{name}: the refusal must name the rule the front end broke"
      );
      assert!(
        seam.backend_req().is_none(),
        "{name}: a refused front end establishes no back-end channel"
      );
    }
  }

  /// The other side of the discriminator above: a front end that closes without breaking anything — including one
  /// that dies part-way through a header — is a hang-up, which is the guest going away, not a protocol fault. Do:
  /// close the front end's socket (cleanly, and after half a header) and drain. Expect `Drained::HungUp` both
  /// times. Without this, "every outcome is a refusal" would pass the hostile test above just as well.
  #[test]
  fn a_front_end_that_closes_is_a_hang_up_and_not_a_fault() {
    for (name, prefix) in [
      ("a clean close", Vec::new()),
      (
        "a close part-way through a header",
        raw_header(request::GET_FEATURES, VERSION, 0)[..7].to_vec(),
      ),
    ] {
      let (frontend, backend) = stream_pair();
      let config = crate::device::DeviceConfig::new(crate::device::FsTag::new("slates").unwrap());
      let mut seam = VhostUserSeam::adopt(backend, &config).unwrap();
      seam.enable_dax(DAX_REGION_FOR_TESTS);
      if !prefix.is_empty() {
        send_front_raw(&frontend, &prefix);
      }
      drop(frontend);
      assert_eq!(
        seam.drain_doorbell(),
        Ok(Drained::HungUp),
        "{name}: a front end that stops talking is gone, not faulty"
      );
    }
  }

  /// Reads a `u64`-payload reply (header then eight bytes), as `answer_u64` sends.
  fn read_front_u64(socket: &OwnedFd) -> u64 {
    let mut buf = [0u8; HEADER_LEN + 8];
    let mut none: [std::mem::MaybeUninit<u8>; 0] = [];
    let got = rustix::net::recvmsg(
      socket,
      &mut [std::io::IoSliceMut::new(&mut buf)],
      &mut RecvAncillaryBuffer::new(&mut none),
      RecvFlags::empty(),
    )
    .unwrap();
    assert_eq!(got.bytes, buf.len());
    u64_at(&buf, HEADER_LEN).unwrap()
  }

  /// Reads one back-end request (header + the 40-byte body, plus any descriptor it carries) the way a front end
  /// would, drops any received descriptor, and returns the request code.
  fn front_read_request(socket: &OwnedFd) -> u32 {
    let mut buf = [0u8; HEADER_LEN + MMAP_LEN];
    let mut space =
      [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(MAP_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let got = rustix::net::recvmsg(
      socket,
      &mut [std::io::IoSliceMut::new(&mut buf)],
      &mut control,
      RecvFlags::empty(),
    )
    .unwrap();
    assert_eq!(got.bytes, buf.len());
    let mut fds: Vec<OwnedFd> = Vec::new();
    for message in control.drain() {
      if let RecvAncillaryMessage::ScmRights(iter) = message {
        fds.extend(iter);
      }
    }
    drop(fds);
    u32_at(&buf, 0).unwrap()
  }

  /// Answers a `NEED_REPLY` back-end request with a reply-ack carrying `status` (zero for success), the way a front
  /// end acknowledges a map or unmap.
  fn front_send_ack(socket: &OwnedFd, request: u32, status: u64) {
    let mut ack = [0u8; HEADER_LEN + ACK_LEN];
    ack[0..4].copy_from_slice(&request.to_le_bytes());
    ack[HEADER_AT_FLAGS..HEADER_AT_FLAGS + 4]
      .copy_from_slice(&(VERSION | FLAG_REPLY).to_le_bytes());
    ack[HEADER_AT_SIZE..HEADER_AT_SIZE + 4]
      .copy_from_slice(&u32::try_from(ACK_LEN).unwrap().to_le_bytes());
    ack[HEADER_LEN..].copy_from_slice(&status.to_le_bytes());
    rustix::net::sendmsg(
      socket,
      &[std::io::IoSlice::new(&ack)],
      &mut SendAncillaryBuffer::default(),
      SendFlags::NOSIGNAL,
    )
    .unwrap();
  }

  /// A-9 / AC-4.12, hostile front end. Do: leave one `SHMEM_UNMAP` outstanding and have the front end reply-ack
  /// a `SHMEM_MAP` instead. Expect: a typed protocol refusal — which ends the device — and the unmap still
  /// outstanding, completed by nothing.
  ///
  /// The channel answers in order, so a conforming VMM never does this and the check costs a comparison. What it
  /// buys is the case where the VMM is wrong: completing a map's ack against an outstanding unmap would report
  /// that window removed, and the caller unpins on an `Unmapped` — handing a block back to the arena while the
  /// guest still has it in its page tables (§4.6 phase two, A-99). Until 2026-10-18 the ack's request code was
  /// parsed and discarded, so the two were indistinguishable.
  #[test]
  fn a_reply_ack_for_a_request_that_is_not_the_one_awaiting_it_is_refused() {
    let (frontend, backend) = stream_pair();
    let page = rustix::param::page_size().max(1);
    let pg = u64::try_from(page).unwrap();
    let region = u64::try_from(page.saturating_mul(8)).unwrap();
    let mut placer: MessagePlacer<u64> =
      MessagePlacer::new(BackendReqChannel::adopt(backend), page, region);

    // One unmap is outstanding; the front end answers as though it had been asked for a map.
    placer.begin_unmap(0, pg, 300).unwrap();
    assert_eq!(front_read_request(&frontend), backend_request::SHMEM_UNMAP);
    front_send_ack(&frontend, backend_request::SHMEM_MAP, 0);

    let refused = placer.poll_acks();
    assert!(
      matches!(
        &refused,
        Err(MessagePlaceError::Channel(VhostError::Protocol { reason }))
          if reason.contains("other than the one awaiting")
      ),
      "a mismatched reply-ack is the front end breaking the wire: {refused:?}"
    );
    assert!(
      placer.has_outstanding(),
      "the unmap is still outstanding: a wrong ack completes nothing"
    );
  }

  /// A-9 / AC-4.12 (message-based DAX placement, by use): the placer pipelines a SETUPMAPPING and a REMOVEMAPPING
  /// over the channel and matches the front end's reply-acks FIFO. Do: begin a map (token 100) and an unmap (token
  /// 200), acknowledge each at a stand-in front end, and drain. Expect: nothing completes before the acks; then the
  /// completions come back in send order carrying each token — the map as `Mapped`, the unmap as `Unmapped` — and
  /// the outstanding ring empties. Proven over a real socket with no VMM. The token is a plain marker here; the
  /// device threads its window entry, and the placer's FIFO is agnostic to it.
  #[test]
  fn the_message_placer_matches_pipelined_acks_fifo_and_reports_each_completion() {
    let (frontend, backend) = stream_pair();
    let mem = memfd_create("slates-placer-fifo", MemfdFlags::CLOEXEC).unwrap();
    rustix::io::pwrite(&mem, &[0x5Au8; 64], 0).unwrap();
    let page = rustix::param::page_size().max(1);
    let pg = u64::try_from(page).unwrap();
    let region = u64::try_from(page.saturating_mul(8)).unwrap();
    let mut placer: MessagePlacer<u64> =
      MessagePlacer::new(BackendReqChannel::adopt(backend), page, region);

    placer.begin_map(0, pg, mem.as_fd(), 0, false, 100).unwrap();
    placer.begin_unmap(pg, pg, 200).unwrap();
    assert!(placer.has_outstanding());
    assert!(
      placer.poll_acks().unwrap().is_empty(),
      "nothing completes before the front end acknowledges"
    );

    // the front end reads both requests in send order and acknowledges them.
    assert_eq!(front_read_request(&frontend), backend_request::SHMEM_MAP);
    assert_eq!(front_read_request(&frontend), backend_request::SHMEM_UNMAP);
    front_send_ack(&frontend, backend_request::SHMEM_MAP, 0);
    front_send_ack(&frontend, backend_request::SHMEM_UNMAP, 0);

    let done = placer.poll_acks().unwrap();
    assert_eq!(done.len(), 2, "both acks drained");
    match done[0] {
      DaxCompletion::Mapped(token) => {
        assert_eq!(token, 100, "the first completion is the map's token")
      }
      _ => panic!("the first completion must be the map"),
    }
    match done[1] {
      DaxCompletion::Unmapped(token) => {
        assert_eq!(token, 200, "the second completion is the unmap's token");
      }
      _ => panic!("the second completion must be the unmap"),
    }
    assert!(!placer.has_outstanding(), "the ring emptied");
  }

  /// A non-zero reply-ack is a typed refusal the placer surfaces per request kind, never a silent success. Do: begin
  /// a map, have the front end refuse it with status 22. Expect: the completion is `MapRefused` carrying the token
  /// and the status, and the ring empties (the device will unpin the block and answer the guest an errno).
  #[test]
  fn a_refused_map_ack_is_a_typed_map_refusal_completion() {
    let (frontend, backend) = stream_pair();
    let mem = memfd_create("slates-placer-refuse", MemfdFlags::CLOEXEC).unwrap();
    rustix::io::pwrite(&mem, &[0u8; 64], 0).unwrap();
    let page = rustix::param::page_size().max(1);
    let pg = u64::try_from(page).unwrap();
    let mut placer: MessagePlacer<u64> = MessagePlacer::new(
      BackendReqChannel::adopt(backend),
      page,
      u64::try_from(page.saturating_mul(4)).unwrap(),
    );

    placer.begin_map(0, pg, mem.as_fd(), 0, true, 7).unwrap();
    assert_eq!(front_read_request(&frontend), backend_request::SHMEM_MAP);
    front_send_ack(&frontend, backend_request::SHMEM_MAP, 22);

    let done = placer.poll_acks().unwrap();
    assert_eq!(done.len(), 1);
    match done[0] {
      DaxCompletion::MapRefused { token, status } => {
        assert_eq!(token, 7);
        assert_eq!(status, 22);
      }
      _ => panic!("a non-zero ack must be a typed map refusal"),
    }
    assert!(!placer.has_outstanding());
  }

  /// §4.6 bounds. Do: ask the placer for an unaligned map, a map past the cache region, and one request past its
  /// derived outstanding bound. Expect: each refused typed — `BadRange` for the ranges, `TooManyOutstanding` at the
  /// bound — so a confused or hostile guest reaches neither outside the region nor unbounded growth.
  #[test]
  fn the_message_placer_refuses_a_bad_range_and_growth_past_its_bound() {
    let (_frontend, backend) = stream_pair();
    let mem = memfd_create("slates-placer-bound", MemfdFlags::CLOEXEC).unwrap();
    rustix::io::pwrite(&mem, &[0u8; 64], 0).unwrap();
    let page = rustix::param::page_size().max(1);
    let pg = u64::try_from(page).unwrap();
    // A one-page region: its bound is one whole page plus one in-flight map = 2.
    let mut placer: MessagePlacer<u64> =
      MessagePlacer::new(BackendReqChannel::adopt(backend), page, pg);

    assert_eq!(
      placer.begin_map(1, pg, mem.as_fd(), 0, false, 0),
      Err(MessagePlaceError::BadRange),
      "an unaligned offset is refused"
    );
    assert_eq!(
      placer.begin_map(pg, pg, mem.as_fd(), 0, false, 0),
      Err(MessagePlaceError::BadRange),
      "a range past the region is refused"
    );
    placer.begin_map(0, pg, mem.as_fd(), 0, false, 1).unwrap();
    placer.begin_unmap(0, pg, 2).unwrap();
    assert_eq!(
      placer.begin_unmap(0, pg, 3),
      Err(MessagePlaceError::TooManyOutstanding { bound: 2 }),
      "a request past the bound is refused, never grown"
    );
    // `_frontend` is kept alive so the socket stays open for the sends above.
  }

  /// §5.11.4 golden vector: the `virtio_fs_config` a real front end reads via GET_CONFIG — the mount tag padded
  /// with NULs to 36 bytes, then `num_request_queues` as a little-endian `u32`. The seam's GET_CONFIG handler
  /// replies exactly these bytes; a real crosvm front end reads the tag from them to mount the volume.
  #[test]
  fn t_5_11_4_virtio_fs_config_is_the_tag_then_num_request_queues_golden_vector() {
    let tag = crate::device::FsTag::new("slates").unwrap();
    let config = fs_config_bytes(&tag.config_bytes(), 1);
    let mut want = [0u8; FS_CONFIG_LEN];
    want[0..6].copy_from_slice(b"slates");
    want[crate::device::TAG_LEN..].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(config, want);
    assert_eq!(
      &config[6..crate::device::TAG_LEN],
      &[0u8; crate::device::TAG_LEN - 6],
      "the tag is NUL-padded to 36 bytes"
    );
  }

  /// AC-4.12 / A-9: a `SHMEM_MAP` body encodes exactly as crosvm's `VhostUserMMap` — a golden vector over every
  /// field (`shmid`, the seven zero padding bytes, `fd_offset`, `shm_offset`, `len`, `flags`).
  #[test]
  fn t_4_13_shmem_map_body_is_the_vhost_user_mmap_golden_vector() {
    let body = encode_mmap(7, 0x1122_3344_5566_7788, 0x2000, 0x0001_0000, true);
    let mut want = [0u8; MMAP_LEN];
    want[MMAP_AT_SHMID] = 7;
    want[MMAP_AT_FD_OFFSET..MMAP_AT_FD_OFFSET + 8]
      .copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
    want[MMAP_AT_SHM_OFFSET..MMAP_AT_SHM_OFFSET + 8].copy_from_slice(&0x2000u64.to_le_bytes());
    want[MMAP_AT_LEN..MMAP_AT_LEN + 8].copy_from_slice(&0x0001_0000u64.to_le_bytes());
    want[MMAP_AT_FLAGS..MMAP_AT_FLAGS + 8].copy_from_slice(&MMAP_FLAG_MAP_RW.to_le_bytes());
    assert_eq!(body, want);
    assert_eq!(
      &body[1..8],
      &[0u8; 7],
      "the seven bytes after shmid are padding"
    );
  }

  /// A reply-ack is decoded as the request it answers and the front end's status, so the caller can both check
  /// that the front end answered the request awaiting it and tell a refusal from a success. A non-zero status is
  /// never a silent success.
  #[test]
  fn a_reply_ack_decodes_as_the_request_it_answers_and_its_status() {
    let mut ack = [0u8; HEADER_LEN + ACK_LEN];
    ack[0..4].copy_from_slice(&backend_request::SHMEM_MAP.to_le_bytes());
    ack[HEADER_AT_FLAGS..HEADER_AT_FLAGS + 4]
      .copy_from_slice(&(VERSION | FLAG_REPLY).to_le_bytes());
    ack[HEADER_AT_SIZE..HEADER_AT_SIZE + 4]
      .copy_from_slice(&u32::try_from(ACK_LEN).unwrap().to_le_bytes());
    ack[HEADER_LEN..].copy_from_slice(&22u64.to_le_bytes());
    assert_eq!(parse_ack(&ack), Ok((backend_request::SHMEM_MAP, 22)));
  }

  /// AC-4.12 / A-9: the back end sends a real descriptor and the map fields over the channel; a stand-in front
  /// end receives exactly those bytes and that descriptor, reads the block's bytes back through the fd it got,
  /// then acknowledges success, and the back end accepts the ack. The message-based DAX wire, end to end.
  #[test]
  fn a_shmem_map_carries_the_block_descriptor_and_fields_and_the_ack_returns() {
    let (backend, frontend) = rustix::net::socketpair(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC,
      None,
    )
    .unwrap();
    let channel = BackendReqChannel::adopt(backend);

    // a content-block stand-in: a memfd of known bytes.
    let block = [0xABu8; 64];
    let mem = memfd_create("slates-shmem-test", MemfdFlags::CLOEXEC).unwrap();
    let wrote = rustix::io::pwrite(&mem, &block, 0).unwrap();
    assert_eq!(wrote, block.len());
    let block_len = u64::try_from(block.len()).unwrap();

    // the back end asks the front end to map the block read-only into region 5 at shm_offset 0x2000.
    channel
      .send_map(5, 0x2000, mem.as_fd(), 0, block_len, false)
      .unwrap();

    // the stand-in front end receives the whole 52-byte message and the one descriptor.
    let mut buf = [0u8; HEADER_LEN + MMAP_LEN];
    let mut space =
      [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(MAP_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let received = rustix::net::recvmsg(
      &frontend,
      &mut [std::io::IoSliceMut::new(&mut buf)],
      &mut control,
      RecvFlags::empty(),
    )
    .unwrap();
    assert_eq!(received.bytes, buf.len());

    // the header names SHMEM_MAP, asks for a reply, and sizes the body.
    assert_eq!(u32_at(&buf, 0).unwrap(), backend_request::SHMEM_MAP);
    assert_ne!(u32_at(&buf, HEADER_AT_FLAGS).unwrap() & FLAG_NEED_REPLY, 0);
    assert_eq!(
      u32_at(&buf, HEADER_AT_SIZE).unwrap(),
      u32::try_from(MMAP_LEN).unwrap()
    );
    // the body carries the map fields.
    let body = &buf[HEADER_LEN..];
    assert_eq!(body[MMAP_AT_SHMID], 5);
    assert_eq!(u64_at(body, MMAP_AT_SHM_OFFSET).unwrap(), 0x2000);
    assert_eq!(u64_at(body, MMAP_AT_LEN).unwrap(), block_len);
    assert_eq!(
      u64_at(body, MMAP_AT_FLAGS).unwrap(),
      0,
      "read-only clears MAP_RW"
    );

    // the descriptor really crossed: the block's bytes read back through the fd the front end received.
    let mut fds = Vec::new();
    for message in control.drain() {
      if let RecvAncillaryMessage::ScmRights(iter) = message {
        fds.extend(iter);
      }
    }
    assert_eq!(fds.len(), 1);
    let got_fd = fds.into_iter().next().unwrap();
    let mut echo = [0u8; 64];
    let read = rustix::io::pread(&got_fd, &mut echo, 0).unwrap();
    assert_eq!(read, echo.len());
    assert_eq!(
      echo, block,
      "the mapped descriptor must be the block the back end handed over"
    );

    // the front end acknowledges success (status 0); the back end reads and accepts the ack.
    let mut ack = [0u8; HEADER_LEN + ACK_LEN];
    ack[0..4].copy_from_slice(&backend_request::SHMEM_MAP.to_le_bytes());
    ack[HEADER_AT_FLAGS..HEADER_AT_FLAGS + 4]
      .copy_from_slice(&(VERSION | FLAG_REPLY).to_le_bytes());
    ack[HEADER_AT_SIZE..HEADER_AT_SIZE + 4]
      .copy_from_slice(&u32::try_from(ACK_LEN).unwrap().to_le_bytes());
    rustix::net::sendmsg(
      &frontend,
      &[std::io::IoSlice::new(&ack)],
      &mut SendAncillaryBuffer::default(),
      SendFlags::NOSIGNAL,
    )
    .unwrap();
    assert_eq!(
      channel.recv_ack().unwrap(),
      Some((backend_request::SHMEM_MAP, 0)),
      "the back end reads the ack as answering the map it sent, with a success status"
    );
  }

  /// A-9 / AC-4.12: a DAX device offers `BACKEND_REQ | SHMEM` and adopts the back-end-request channel the front
  /// end hands over `SET_BACKEND_REQ_FD`, driven through the production control loop over a real socket; the
  /// adopted channel then carries a `SHMEM_MAP` to the handed-in peer — the negotiation end to end, no VMM.
  #[test]
  fn a_dax_device_negotiates_the_backend_req_channel_and_it_carries_a_map() {
    let (frontend, backend) = stream_pair();
    let config = crate::device::DeviceConfig::new(crate::device::FsTag::new("slates").unwrap());
    let mut seam = VhostUserSeam::adopt(backend, &config).unwrap();
    seam.enable_dax(0x1000_0000); // a 256 MiB cache window (test value; Piece B only stores it)

    // GET_PROTOCOL_FEATURES: the reply carries the DAX features.
    send_front_message(&frontend, request::GET_PROTOCOL_FEATURES, 0, None);
    seam.serve_control().unwrap();
    let offered = read_front_u64(&frontend);
    assert_ne!(
      offered & PROTOCOL_F_BACKEND_REQ,
      0,
      "a DAX device offers BACKEND_REQ"
    );
    assert_ne!(offered & PROTOCOL_F_SHMEM, 0, "a DAX device offers SHMEM");

    // SET_BACKEND_REQ_FD: a fresh pair, its 'a' end handed to the seam, 'b' kept as the front end's handler.
    let (req_a, req_b) = stream_pair();
    send_front_message(
      &frontend,
      request::SET_BACKEND_REQ_FD,
      0,
      Some(req_a.as_fd()),
    );
    seam.serve_control().unwrap();
    drop(req_a); // the seam holds a dup; this end is no longer needed
    assert!(
      seam.backend_req().is_some(),
      "the seam adopted the back-end-request channel"
    );

    // the adopted channel is req_a's peer: a SHMEM_MAP the device sends arrives on req_b.
    let mem = memfd_create("slates-reqfd-test", MemfdFlags::CLOEXEC).unwrap();
    rustix::io::pwrite(&mem, &[0u8; 16], 0).unwrap();
    seam
      .backend_req()
      .unwrap()
      .send_map(0, 0, mem.as_fd(), 0, 16, false)
      .unwrap();
    let mut buf = [0u8; HEADER_LEN + MMAP_LEN];
    let mut none: [std::mem::MaybeUninit<u8>; 0] = [];
    let got = rustix::net::recvmsg(
      &req_b,
      &mut [std::io::IoSliceMut::new(&mut buf)],
      &mut RecvAncillaryBuffer::new(&mut none),
      RecvFlags::empty(),
    )
    .unwrap();
    assert_eq!(got.bytes, buf.len());
    assert_eq!(u32_at(&buf, 0).unwrap(), backend_request::SHMEM_MAP);
  }

  /// A-9 / AC-4.12: a DAX device reports exactly one shared-memory region — the cache window — of the configured
  /// size over GET_SHMEM_CONFIG, the region crosvm allocates as the DAX BAR and the back end maps into (shmid 0).
  #[test]
  fn a_dax_device_reports_one_cache_region_of_the_configured_size() {
    let (frontend, backend) = stream_pair();
    let config = crate::device::DeviceConfig::new(crate::device::FsTag::new("slates").unwrap());
    let mut seam = VhostUserSeam::adopt(backend, &config).unwrap();
    let region = 0x0400_0000u64; // a 64 MiB cache window (test value)
    seam.enable_dax(region);

    send_front_message(&frontend, request::GET_SHMEM_CONFIG, 0, None);
    seam.serve_control().unwrap();

    let mut buf = vec![0u8; HEADER_LEN + SHMEM_CONFIG_LEN];
    let mut none: [std::mem::MaybeUninit<u8>; 0] = [];
    let got = rustix::net::recvmsg(
      &frontend,
      &mut [std::io::IoSliceMut::new(&mut buf)],
      &mut RecvAncillaryBuffer::new(&mut none),
      RecvFlags::empty(),
    )
    .unwrap();
    assert_eq!(got.bytes, buf.len());
    assert_eq!(
      u32_at(&buf, HEADER_LEN).unwrap(),
      1,
      "exactly one DAX cache region"
    );
    assert_eq!(
      u64_at(&buf, HEADER_LEN + SHMEM_CONFIG_SIZES).unwrap(),
      region,
      "the region is the configured cache size"
    );
  }

  /// A non-DAX device offers neither DAX feature and refuses a back-end-request fd — production and any non-DAX
  /// device behave exactly as before (DAX stays unadvertised, §4.6).
  #[test]
  fn a_non_dax_device_offers_no_dax_features_and_refuses_the_backend_req_fd() {
    let (frontend, backend) = stream_pair();
    let config = crate::device::DeviceConfig::new(crate::device::FsTag::new("slates").unwrap());
    let mut seam = VhostUserSeam::adopt(backend, &config).unwrap();
    // no enable_dax

    send_front_message(&frontend, request::GET_PROTOCOL_FEATURES, 0, None);
    seam.serve_control().unwrap();
    let offered = read_front_u64(&frontend);
    assert_eq!(
      offered & DAX_PROTOCOL_FEATURES,
      0,
      "a non-DAX device offers no DAX features"
    );

    // an unsolicited SET_BACKEND_REQ_FD is refused (the feature was never offered): the control loop ends.
    let (req_a, _req_b) = stream_pair();
    send_front_message(
      &frontend,
      request::SET_BACKEND_REQ_FD,
      0,
      Some(req_a.as_fd()),
    );
    assert!(
      seam.serve_control().is_err(),
      "a back-end-request fd without the shmem feature is refused"
    );
    assert!(seam.backend_req().is_none());
  }
}

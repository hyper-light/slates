//! The simulated guest driver the virtqueue and device tests share: it lays the three rings of each
//! queue out in a simulated guest memory the way a Linux virtio driver does (virtio 1.2 §2.7), builds
//! descriptor chains over guest buffers, publishes them through the available ring, and reads the
//! used ring back. It is the driver half of the protocol whose device half is under test; every byte
//! it writes is what a guest kernel would write.
// Each test binary uses a subset of the driver's surface; the items are `pub` for the sibling test
// binaries that include this module, which the crate-level reachability lint cannot see.
#![allow(
  dead_code,
  unreachable_pub,
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic
)]

use std::collections::BTreeMap;

use slates_bridge_core::{Attachments, OpContext, Rights, View};
use slates_bridge_fuse::abi::IN_HEADER_LEN;
use slates_bridge_virtiofs::admission::{Doorbell, SeamError, VmmSeam};
use slates_bridge_virtiofs::device::DeviceConfig;
use slates_bridge_virtiofs::memory::{GuestAddr, GuestMemory, GuestRange};
use slates_bridge_virtiofs::sim::SimGuestMemory;
use slates_bridge_virtiofs::virtqueue::{
  DESCRIPTOR_LEN, QueueLayout, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Format: virtio 1.2 §2.7.6 — the available ring's `idx` follows its `flags` (two `le16`).
pub const AVAIL_IDX_OFFSET: u64 = 2;
/// Format: §2.7.6 — the available ring's entries follow `flags` and `idx`.
pub const AVAIL_RING_OFFSET: u64 = 4;
/// Format: §2.7.8 — the used ring's `idx` follows its `flags`.
pub const USED_IDX_OFFSET: u64 = 2;
/// Format: §2.7.8 — the used ring's elements (`{id: le32, len: le32}`) follow `flags` and `idx`.
pub const USED_RING_OFFSET: u64 = 4;
/// Format: §2.7.8 — one used element is eight bytes.
pub const USED_ELEM_LEN: u64 = 8;

/// Shape: the simulated guest's memory: 4 MiB, room for the rings of the largest queue these tests
/// configure and the buffers of the longest script.
pub const GUEST_RAM: u64 = 4 << 20;
/// Shape: where the first queue's descriptor table sits (page-aligned, past a zero page).
pub const DESC_BASE: u64 = 0x1000;
/// Shape: the stride between queues' ring areas: one page-aligned 64 KiB block per queue, which
/// holds the rings of a queue up to 2048 entries.
pub const QUEUE_STRIDE: u64 = 0x10000;
/// Shape: where the guest's buffers begin (past the ring areas of the queues these tests use).
pub const BUFFER_BASE: u64 = 0x100000;

/// The ring layout for a queue of `size` whose descriptor table sits at `desc`, each ring at its
/// §2.7 alignment.
pub fn layout_at(desc: u64, size: u16) -> QueueLayout {
  let desc_len = u64::from(size) * DESCRIPTOR_LEN;
  let avail = desc + desc_len;
  let avail_len = 6 + 2 * u64::from(size);
  let used = (avail + avail_len).next_multiple_of(4);
  QueueLayout {
    size,
    descriptor_table: GuestAddr(desc),
    available_ring: GuestAddr(avail),
    used_ring: GuestAddr(used),
  }
}

/// One chain the driver published: its descriptor indices (freed on reap) and its writable ranges
/// (read back for the reply).
struct ChainRecord {
  indices: Vec<u16>,
  writable: Vec<GuestRange>,
}

/// One queue as the driver sees it.
struct SimQueue {
  layout: QueueLayout,
  avail_idx: u16,
  used_seen: u16,
  free: Vec<u16>,
  chains: BTreeMap<u16, ChainRecord>,
  /// Every head made available, in order; the device completes in order, so the heads past
  /// `used_seen` are the chains still pending.
  published: Vec<u16>,
}

/// The simulated guest driver.
pub struct SimDriver {
  /// The guest's memory: the rings and the buffers.
  pub memory: SimGuestMemory,
  queues: Vec<SimQueue>,
  next_buffer: u64,
}

impl SimDriver {
  /// Queues of the given sizes, each with its rings in its own area.
  pub fn new(sizes: &[u16]) -> SimDriver {
    let queues = sizes
      .iter()
      .enumerate()
      .map(|(position, size)| SimQueue {
        layout: layout_at(
          DESC_BASE + u64::try_from(position).unwrap() * QUEUE_STRIDE,
          *size,
        ),
        avail_idx: 0,
        used_seen: 0,
        free: (0..*size).rev().collect(),
        chains: BTreeMap::new(),
        published: Vec::new(),
      })
      .collect();
    SimDriver {
      memory: SimGuestMemory::new(GUEST_RAM),
      queues,
      next_buffer: BUFFER_BASE,
    }
  }

  /// Every queue's layout, in queue order.
  pub fn layouts(&self) -> Vec<QueueLayout> {
    self.queues.iter().map(|q| q.layout).collect()
  }

  /// One queue's layout.
  pub fn layout(&self, queue: usize) -> QueueLayout {
    self.queues[queue].layout
  }

  /// Writes one raw descriptor (`addr`, `len`, `flags`, `next`) at `index` of `queue`.
  pub fn descriptor(
    &mut self,
    queue: usize,
    index: u16,
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
  ) {
    let at = self.queues[queue].layout.descriptor_table.0 + u64::from(index) * DESCRIPTOR_LEN;
    let mut bytes = Vec::with_capacity(usize::try_from(DESCRIPTOR_LEN).unwrap());
    bytes.extend_from_slice(&addr.to_le_bytes());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&next.to_le_bytes());
    self.write(at, &bytes);
  }

  /// Allocates a guest buffer of `len` bytes filled with `fill`, returning its address.
  pub fn buffer(&mut self, len: u32, fill: u8) -> u64 {
    let at = self.next_buffer;
    self.next_buffer += u64::from(len);
    let bytes = vec![fill; usize::try_from(len).unwrap()];
    self.write(at, &bytes);
    at
  }

  /// Takes a free descriptor index of `queue`.
  pub fn take_descriptor(&mut self, queue: usize) -> u16 {
    self.queues[queue].free.pop().expect("a free descriptor")
  }

  /// Builds a chain of fresh descriptors over fresh buffers on `queue`, `parts` being `(len,
  /// device-writable)` in chain order; returns the head index. Not yet made available.
  pub fn chain(&mut self, queue: usize, parts: &[(u32, bool)]) -> u16 {
    let indices: Vec<u16> = parts.iter().map(|_| self.take_descriptor(queue)).collect();
    let mut writable = Vec::new();
    for (position, (len, is_writable)) in parts.iter().enumerate() {
      let index = indices[position];
      let addr = self.buffer(*len, if *is_writable { 0xEE } else { 0xAA });
      let last = position + 1 == parts.len();
      let mut flags = if *is_writable { VIRTQ_DESC_F_WRITE } else { 0 };
      if !last {
        flags |= VIRTQ_DESC_F_NEXT;
      }
      let next = if last { 0 } else { indices[position + 1] };
      self.descriptor(queue, index, addr, *len, flags, next);
      if *is_writable {
        writable.push(GuestRange::new(GuestAddr(addr), u64::from(*len)).unwrap());
      }
    }
    let head = indices[0];
    self.queues[queue]
      .chains
      .insert(head, ChainRecord { indices, writable });
    head
  }

  /// Publishes `head` through `queue`'s available ring: the entry, then the index (§2.7.13).
  pub fn make_available(&mut self, queue: usize, head: u16) {
    let q = &self.queues[queue];
    let slot = u64::from(q.avail_idx % q.layout.size);
    let entry = q.layout.available_ring.0 + AVAIL_RING_OFFSET + slot * 2;
    self.write(entry, &head.to_le_bytes());
    let idx = self.queues[queue].avail_idx.wrapping_add(1);
    self.queues[queue].avail_idx = idx;
    self.queues[queue].published.push(head);
    self.write_avail_idx(queue, idx);
  }

  /// The writable ranges of every chain on `queue` published but not yet reaped, in order.
  pub fn pending_writable(&self, queue: usize) -> Vec<GuestRange> {
    let q = &self.queues[queue];
    q.published
      .iter()
      .skip(usize::from(q.used_seen))
      .flat_map(|head| q.chains[head].writable.iter().copied())
      .collect()
  }

  /// Writes `queue`'s available index outright (a hostile driver's move).
  pub fn write_avail_idx(&mut self, queue: usize, idx: u16) {
    let at = self.queues[queue].layout.available_ring.0 + AVAIL_IDX_OFFSET;
    self.write(at, &idx.to_le_bytes());
  }

  /// Submits one FUSE request on `queue` as a real chain: the request bytes over `split` readable
  /// descriptors and `reply_capacity` bytes over `split` writable ones (none when zero), then makes
  /// it available. Returns the head.
  pub fn submit(&mut self, queue: usize, request: &[u8], reply_capacity: u32, split: usize) -> u16 {
    let mut parts: Vec<(u32, bool)> = pieces(u32::try_from(request.len()).unwrap(), split)
      .into_iter()
      .map(|len| (len, false))
      .collect();
    parts.extend(
      pieces(reply_capacity, split)
        .into_iter()
        .map(|len| (len, true)),
    );
    let head = self.chain(queue, &parts);
    // Fill the readable buffers with the request bytes, piece by piece.
    let mut at = 0usize;
    let record = &self.queues[queue].chains[&head];
    let readable: Vec<(u16, u32)> = record
      .indices
      .iter()
      .zip(parts.iter())
      .filter(|(_, (_, w))| !*w)
      .map(|(index, (len, _))| (*index, *len))
      .collect();
    for (index, len) in readable {
      let addr = self.descriptor_addr(queue, index);
      let len = usize::try_from(len).unwrap();
      self.write(addr, &request[at..at + len]);
      at += len;
    }
    self.make_available(queue, head);
    head
  }

  /// The next used element of `queue` the driver has not seen, freeing its chain's descriptors.
  pub fn reap(&mut self, queue: usize) -> Option<(u16, u32)> {
    let q = &self.queues[queue];
    let idx = self.read_u16(q.layout.used_ring.0 + USED_IDX_OFFSET);
    if idx == q.used_seen {
      return None;
    }
    let slot = u64::from(q.used_seen % q.layout.size);
    let at = q.layout.used_ring.0 + USED_RING_OFFSET + slot * USED_ELEM_LEN;
    let id = u16::try_from(self.read_u32(at)).unwrap();
    let len = self.read_u32(at + 4);
    let q = &mut self.queues[queue];
    q.used_seen = q.used_seen.wrapping_add(1);
    if let Some(record) = q.chains.get(&id) {
      q.free.extend(record.indices.iter().copied());
    }
    Some((id, len))
  }

  /// The first `len` bytes the device wrote into the writable buffers of chain `head` on `queue`.
  pub fn reply_of(&self, queue: usize, head: u16, len: u32) -> Vec<u8> {
    let record = &self.queues[queue].chains[&head];
    let mut out = Vec::new();
    let mut left = usize::try_from(len).unwrap();
    for range in &record.writable {
      if left == 0 {
        break;
      }
      let take = usize::try_from(range.len()).unwrap().min(left);
      let mut bytes = vec![0u8; take];
      self
        .memory
        .read(
          GuestRange::new(range.start(), u64::try_from(take).unwrap()).unwrap(),
          &mut bytes,
        )
        .unwrap();
      out.extend_from_slice(&bytes);
      left -= take;
    }
    out
  }

  /// The writable ranges of chain `head` on `queue` (for "never touched" assertions).
  pub fn writable_of(&self, queue: usize, head: u16) -> Vec<GuestRange> {
    self.queues[queue].chains[&head].writable.clone()
  }

  /// The used ring's elements as the driver reads them: `(id, len)` up to the published index.
  pub fn used(&self, queue: usize) -> Vec<(u32, u32)> {
    let q = &self.queues[queue];
    let idx = self.read_u16(q.layout.used_ring.0 + USED_IDX_OFFSET);
    (0..idx)
      .map(|position| {
        let slot = u64::from(position % q.layout.size);
        let at = q.layout.used_ring.0 + USED_RING_OFFSET + slot * USED_ELEM_LEN;
        (self.read_u32(at), self.read_u32(at + 4))
      })
      .collect()
  }

  fn descriptor_addr(&self, queue: usize, index: u16) -> u64 {
    let at = self.queues[queue].layout.descriptor_table.0 + u64::from(index) * DESCRIPTOR_LEN;
    let mut bytes = [0u8; 8];
    self
      .memory
      .read(GuestRange::new(GuestAddr(at), 8).unwrap(), &mut bytes)
      .unwrap();
    u64::from_le_bytes(bytes)
  }

  /// Writes raw bytes into guest memory.
  pub fn write(&mut self, at: u64, bytes: &[u8]) {
    let range = GuestRange::new(GuestAddr(at), u64::try_from(bytes.len()).unwrap()).unwrap();
    self.memory.write(range, bytes).unwrap();
  }

  pub fn read_u16(&self, at: u64) -> u16 {
    let mut bytes = [0u8; 2];
    self
      .memory
      .read(GuestRange::new(GuestAddr(at), 2).unwrap(), &mut bytes)
      .unwrap();
    u16::from_le_bytes(bytes)
  }

  pub fn read_u32(&self, at: u64) -> u32 {
    let mut bytes = [0u8; 4];
    self
      .memory
      .read(GuestRange::new(GuestAddr(at), 4).unwrap(), &mut bytes)
      .unwrap();
    u32::from_le_bytes(bytes)
  }
}

/// Splits `total` bytes into `split` pieces (fewer when `total` is smaller; none when zero), the
/// remainder on the last.
fn pieces(total: u32, split: usize) -> Vec<u32> {
  if total == 0 {
    return Vec::new();
  }
  let split = u32::try_from(split.max(1)).unwrap().min(total);
  let each = total / split;
  let mut out: Vec<u32> = (0..split).map(|_| each).collect();
  if let Some(last) = out.last_mut() {
    *last += total - each * split;
  }
  out
}

// ------------------------------------------------------------------- the scratch-volume fixture

/// Shape: the page and a small arena for the test volumes.
pub const PAGE: usize = 4096;
pub const REGION_PAGES: usize = 4096;

/// A store over a small RAM arena.
pub fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

/// A scratch volume on a deterministic clock, so two volumes driven identically agree byte for byte.
pub fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(StepClock::new(1_000_000_000, 1)),
    },
  )
  .unwrap()
}

/// The volume id every test attaches.
pub fn vid() -> VolumeId {
  VolumeId { bytes: [7; 16] }
}

/// A read-write current-view context, minted through the attachment registry (the only way).
pub fn context() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      vid(),
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

/// A FUSE request: the header then the body, `len` set to the total.
pub fn message(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

/// The errno a FUSE reply carries (negated on the wire).
pub fn reply_error(reply: &[u8]) -> i32 {
  i32::from_le_bytes(reply[4..8].try_into().unwrap())
}

// --------------------------------------------------------------------- the simulated VMM seam

/// One call the device made on the seam, in the order made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeamCall {
  Consumer,
  Memory,
  Queues,
  Publish,
}

/// A simulated host VMM (the in-process form): it owns the guest — its memory and the driver
/// that writes the rings — answers the consumer question the way the harness configured it,
/// records every call the device makes and its order, and notes the notifications raised and
/// whether it was released.
pub struct SimVmm {
  guest: SimDriver,
  consumer: Result<Principal, SeamError>,
  calls: Vec<SeamCall>,
  published: Option<DeviceConfig>,
  notified: Vec<u16>,
  released: bool,
}

impl std::fmt::Debug for SimVmm {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SimVmm")
      .field("calls", &self.calls)
      .field("released", &self.released)
      .finish()
  }
}

impl SimVmm {
  /// A VMM whose guest has queues of `sizes` and whose harness established `consumer`.
  pub fn new(sizes: &[u16], consumer: Result<Principal, SeamError>) -> SimVmm {
    SimVmm {
      guest: SimDriver::new(sizes),
      consumer,
      calls: Vec::new(),
      published: None,
      notified: Vec::new(),
      released: false,
    }
  }

  pub fn guest(&self) -> &SimDriver {
    &self.guest
  }

  pub fn guest_mut(&mut self) -> &mut SimDriver {
    &mut self.guest
  }

  pub fn calls(&self) -> Vec<SeamCall> {
    self.calls.clone()
  }

  pub fn published(&self) -> Option<DeviceConfig> {
    self.published
  }

  pub fn notified(&self) -> Vec<u16> {
    self.notified.clone()
  }

  pub fn released(&self) -> bool {
    self.released
  }
}

impl VmmSeam for SimVmm {
  fn consumer(&mut self) -> Result<Principal, SeamError> {
    self.calls.push(SeamCall::Consumer);
    self.consumer.clone()
  }

  fn memory(&mut self) -> Result<&mut dyn GuestMemory, SeamError> {
    self.calls.push(SeamCall::Memory);
    Ok(&mut self.guest.memory)
  }

  fn queues(&mut self) -> Result<Vec<QueueLayout>, SeamError> {
    self.calls.push(SeamCall::Queues);
    Ok(self.guest.layouts())
  }

  fn publish(&mut self, config: &DeviceConfig) -> Result<(), SeamError> {
    self.calls.push(SeamCall::Publish);
    self.published = Some(*config);
    Ok(())
  }

  fn notify_used(&mut self, queue: u16) -> Result<(), SeamError> {
    self.notified.push(queue);
    Ok(())
  }

  fn doorbell(&self) -> Doorbell {
    Doorbell::InProcess
  }

  fn release(&mut self) {
    self.released = true;
  }
}

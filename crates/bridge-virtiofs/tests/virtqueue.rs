//! The split-virtqueue machine driven by a simulated guest driver (§4.6 "virtio-fs and OCI
//! attachment contract": "queue descriptors, scatter/gather ranges, arithmetic and chained lengths
//! are validated within derived caps before access"; AC-4.12/T-4.14). The driver lays real virtio
//! 1.2 §2.7 rings out in a simulated guest memory, publishes descriptor chains through the
//! available ring, and reads the used ring back — the byte layouts a Linux guest's virtio driver
//! writes. Every hostile case the contract names is refused typed, and the simulated memory's
//! access log proves the refusal came before any buffer was touched.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_virtiofs::memory::{GuestAddr, GuestMemory, GuestRange};
use slates_bridge_virtiofs::sim::SimGuestMemory;
use slates_bridge_virtiofs::virtqueue::{
  ChainCaps, DESCRIPTOR_LEN, DescriptorChain, QueueLayout, Ring, VIRTQ_DESC_F_INDIRECT,
  VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE, Virtqueue, VirtqueueError,
};

/// Format: virtio 1.2 §2.7.6 — the available ring's `idx` follows its `flags` (two `le16`).
const AVAIL_IDX_OFFSET: u64 = 2;
/// Format: §2.7.6 — the available ring's entries follow `flags` and `idx`.
const AVAIL_RING_OFFSET: u64 = 4;
/// Format: §2.7.8 — the used ring's `idx` follows its `flags`.
const USED_IDX_OFFSET: u64 = 2;
/// Format: §2.7.8 — the used ring's elements (`{id: le32, len: le32}`) follow `flags` and `idx`.
const USED_RING_OFFSET: u64 = 4;
/// Format: §2.7.8 — one used element is eight bytes.
const USED_ELEM_LEN: u64 = 8;

/// Shape: the simulated guest's memory: 1 MiB, a page-multiple large enough for the rings of the
/// largest queue these tests configure and a few hundred KiB of buffers.
const GUEST_RAM: u64 = 1 << 20;
/// Shape: where the descriptor table sits in the simulated guest (page-aligned, past a zero page).
const DESC_BASE: u64 = 0x1000;
/// Shape: where the guest's buffers begin (past the rings of any queue size these tests use).
const BUFFER_BASE: u64 = 0x100000 / 2;

/// A simulated guest driver: lays the three rings out in guest memory the way a virtio driver does
/// and publishes chains through the available ring.
struct SimDriver {
  memory: SimGuestMemory,
  layout: QueueLayout,
  avail_idx: u16,
  next_buffer: u64,
  next_desc: u16,
}

impl SimDriver {
  /// Rings for a queue of `size`: the descriptor table at `DESC_BASE` (16-byte aligned), the
  /// available ring right after it (2-byte aligned), the used ring after that (4-byte aligned).
  fn new(size: u16) -> SimDriver {
    let memory = SimGuestMemory::new(GUEST_RAM);
    SimDriver {
      memory,
      layout: layout_for(size),
      avail_idx: 0,
      next_buffer: BUFFER_BASE,
      next_desc: 0,
    }
  }

  fn queue(&self, caps: ChainCaps) -> Result<Virtqueue, VirtqueueError> {
    Virtqueue::new(self.layout, caps, &self.memory)
  }

  /// Writes one raw descriptor (`addr`, `len`, `flags`, `next`) at `index`.
  fn descriptor(&mut self, index: u16, addr: u64, len: u32, flags: u16, next: u16) {
    let at = self.layout.descriptor_table.0 + u64::from(index) * DESCRIPTOR_LEN;
    let mut bytes = Vec::with_capacity(usize::try_from(DESCRIPTOR_LEN).unwrap());
    bytes.extend_from_slice(&addr.to_le_bytes());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&flags.to_le_bytes());
    bytes.extend_from_slice(&next.to_le_bytes());
    self.write(at, &bytes);
  }

  /// Allocates a guest buffer of `len` bytes filled with `fill`, returning its address.
  fn buffer(&mut self, len: u32, fill: u8) -> u64 {
    let at = self.next_buffer;
    self.next_buffer += u64::from(len);
    let bytes = vec![fill; usize::try_from(len).unwrap()];
    self.write(at, &bytes);
    at
  }

  /// Builds a chain of fresh descriptors over fresh buffers, `parts` being `(len, device-writable)`
  /// in chain order; returns the head index. The chain is not yet made available.
  fn chain(&mut self, parts: &[(u32, bool)]) -> u16 {
    let head = self.next_desc;
    for (position, (len, writable)) in parts.iter().enumerate() {
      let index = self.next_desc;
      self.next_desc += 1;
      let addr = self.buffer(*len, if *writable { 0xEE } else { 0xAA });
      let last = position + 1 == parts.len();
      let mut flags = if *writable { VIRTQ_DESC_F_WRITE } else { 0 };
      if !last {
        flags |= VIRTQ_DESC_F_NEXT;
      }
      self.descriptor(index, addr, *len, flags, if last { 0 } else { index + 1 });
    }
    head
  }

  /// Publishes `head` through the available ring: the entry, then the index (§2.7.13).
  fn make_available(&mut self, head: u16) {
    let slot = u64::from(self.avail_idx % self.layout.size);
    let entry = self.layout.available_ring.0 + AVAIL_RING_OFFSET + slot * 2;
    self.write(entry, &head.to_le_bytes());
    self.avail_idx = self.avail_idx.wrapping_add(1);
    self.write_avail_idx(self.avail_idx);
  }

  fn write_avail_idx(&mut self, idx: u16) {
    let at = self.layout.available_ring.0 + AVAIL_IDX_OFFSET;
    self.write(at, &idx.to_le_bytes());
  }

  /// The used ring's elements as the driver reads them: `(id, len)` up to the published index.
  fn used(&self) -> Vec<(u32, u32)> {
    let idx = self.read_u16(self.layout.used_ring.0 + USED_IDX_OFFSET);
    (0..idx)
      .map(|position| {
        let slot = u64::from(position % self.layout.size);
        let at = self.layout.used_ring.0 + USED_RING_OFFSET + slot * USED_ELEM_LEN;
        (self.read_u32(at), self.read_u32(at + 4))
      })
      .collect()
  }

  fn write(&mut self, at: u64, bytes: &[u8]) {
    let range = GuestRange::new(GuestAddr(at), u64::try_from(bytes.len()).unwrap()).unwrap();
    self.memory.write(range, bytes).unwrap();
  }

  fn read_u16(&self, at: u64) -> u16 {
    let mut bytes = [0u8; 2];
    self
      .memory
      .read(GuestRange::new(GuestAddr(at), 2).unwrap(), &mut bytes)
      .unwrap();
    u16::from_le_bytes(bytes)
  }

  fn read_u32(&self, at: u64) -> u32 {
    let mut bytes = [0u8; 4];
    self
      .memory
      .read(GuestRange::new(GuestAddr(at), 4).unwrap(), &mut bytes)
      .unwrap();
    u32::from_le_bytes(bytes)
  }
}

/// The ring layout for a queue of `size`, each ring at its §2.7 alignment.
fn layout_for(size: u16) -> QueueLayout {
  let desc_len = u64::from(size) * DESCRIPTOR_LEN;
  let avail = DESC_BASE + desc_len;
  let avail_len = 6 + 2 * u64::from(size);
  let used = (avail + avail_len).next_multiple_of(4);
  QueueLayout {
    size,
    descriptor_table: GuestAddr(DESC_BASE),
    available_ring: GuestAddr(avail),
    used_ring: GuestAddr(used),
  }
}

/// Caps generous enough that a well-formed test chain is never capped: every descriptor of the
/// queue, and a byte cap above any buffer these tests allocate.
fn roomy(size: u16) -> ChainCaps {
  ChainCaps {
    max_descriptors: size,
    max_readable_bytes: 1 << 18,
    max_writable_bytes: 1 << 18,
  }
}

/// The ranges of a chain's buffers, for the access-log assertions.
fn buffers_of(chain: &DescriptorChain) -> Vec<GuestRange> {
  chain
    .readable
    .iter()
    .chain(chain.writable.iter())
    .copied()
    .collect()
}

/// Whether the simulated memory logged any access touching one of `ranges`.
fn touched(memory: &SimGuestMemory, ranges: &[GuestRange]) -> bool {
  memory
    .accesses()
    .iter()
    .any(|access| ranges.iter().any(|r| access.range.overlaps(r)))
}

/// AC-4.12/T-4.14: two chains published in ring order are walked in that order; each chain's
/// device-readable ranges precede its device-writable ones with the byte totals summed; a third pop
/// finds nothing; a zero-length descriptor contributes no bytes; the pop counter moved.
#[test]
fn well_formed_chains_are_walked_in_ring_order_with_readable_then_writable_ranges() {
  let mut driver = SimDriver::new(8);
  let first = driver.chain(&[(40, false), (100, false), (16, true), (500, true)]);
  let second = driver.chain(&[(48, false), (0, false), (16, true)]);
  driver.make_available(first);
  driver.make_available(second);
  let mut queue = driver.queue(roomy(8)).unwrap();

  let chain = queue.pop(&driver.memory).unwrap().expect("the first chain");
  assert_first_chain(&chain, first);
  let chain = queue
    .pop(&driver.memory)
    .unwrap()
    .expect("the second chain");
  assert_second_chain(&chain, second);
  assert!(
    queue.pop(&driver.memory).unwrap().is_none(),
    "nothing more is available"
  );
  assert_eq!(queue.counters().chains_popped, 2);
  assert_eq!(queue.counters().chains_refused, 0);
}

/// The first chain of the ring-order test: two readable then two writable ranges, totals summed,
/// ranges in chain order.
fn assert_first_chain(chain: &DescriptorChain, head: u16) {
  assert_eq!(chain.head, head);
  assert_eq!(chain.readable.len(), 2);
  assert_eq!(chain.writable.len(), 2);
  assert_eq!(chain.readable_bytes, 140);
  assert_eq!(chain.writable_bytes, 516);
  assert!(
    chain.readable[0].start() < chain.readable[1].start(),
    "ranges come back in chain order"
  );
}

/// The second chain of the ring-order test: a zero-length descriptor adds nothing to the totals.
fn assert_second_chain(chain: &DescriptorChain, head: u16) {
  assert_eq!(chain.head, head);
  assert_eq!(
    chain.readable_bytes, 48,
    "a zero-length descriptor adds nothing"
  );
  assert_eq!(chain.writable_bytes, 16);
}

/// §2.7.8.2: the device writes the used element (`id`, `len`) before it publishes the used index,
/// and the driver reads exactly that element back; the used counter moved.
#[test]
fn a_used_element_is_published_after_its_id_and_length_are_written() {
  let mut driver = SimDriver::new(4);
  let head = driver.chain(&[(40, false), (64, true)]);
  driver.make_available(head);
  let mut queue = driver.queue(roomy(4)).unwrap();
  let chain = queue.pop(&driver.memory).unwrap().unwrap();

  driver.memory.record_accesses(true);
  queue.push_used(&mut driver.memory, &chain, 7).unwrap();
  assert_eq!(
    queue.push_used(&mut driver.memory, &chain, 65).unwrap_err(),
    VirtqueueError::UsedLengthOverChain {
      len: 65,
      writable: 64
    },
    "a used length past the chain's writable bytes is refused"
  );
  let writes: Vec<GuestRange> = driver
    .memory
    .accesses()
    .iter()
    .filter(|a| a.write)
    .map(|a| a.range)
    .collect();
  let element_at = driver.layout.used_ring.0 + USED_RING_OFFSET;
  let idx_at = driver.layout.used_ring.0 + USED_IDX_OFFSET;
  assert_eq!(
    writes.len(),
    2,
    "one element write, then one index write: {writes:?}"
  );
  assert_eq!(
    writes[0].start().0,
    element_at,
    "the element is written first"
  );
  assert_eq!(writes[1].start().0, idx_at, "the index is published second");
  assert_eq!(driver.used(), vec![(u32::from(head), 7)]);
  assert_eq!(queue.counters().used_published, 1);
}

/// Ring indices wrap at the queue size: six chains through a four-entry queue land in the used ring
/// in order, the fifth and sixth reusing the first two slots.
#[test]
fn ring_indices_wrap_at_the_queue_size() {
  let mut driver = SimDriver::new(4);
  let mut queue = driver.queue(roomy(4)).unwrap();
  let mut heads = Vec::new();
  for round in 0..6u32 {
    // Each round reuses descriptor slot (round % 4) so the table never overflows.
    let index = u16::try_from(round % 4).unwrap();
    driver.next_desc = index;
    let head = driver.chain(&[(8, false)]);
    driver.make_available(head);
    let chain = queue.pop(&driver.memory).unwrap().unwrap();
    queue.push_used(&mut driver.memory, &chain, 0).unwrap();
    heads.push((u32::from(head), 0));
  }
  // The driver has consumed nothing, so it reads the last four elements (the ring holds four).
  let used = driver.used();
  assert_eq!(used.len(), 6, "the used index counts every publication");
  assert_eq!(
    &used[4..],
    &heads[4..],
    "the wrapped slots hold the last two chains"
  );
}

/// AC-4.12/T-4.14 "malformed descriptor chains": an indirect descriptor is refused typed (this
/// device does not offer `VIRTIO_F_INDIRECT_DESC`, §2.7.5.3.1) before its buffer is touched, and
/// the queue stays faulted — a later pop repeats the refusal rather than skipping the chain.
#[test]
fn an_indirect_descriptor_is_refused_typed_before_any_buffer_access() {
  let mut driver = SimDriver::new(4);
  let table = driver.buffer(64, 0x11);
  driver.descriptor(0, table, 64, VIRTQ_DESC_F_INDIRECT, 0);
  driver.make_available(0);
  let mut queue = driver.queue(roomy(4)).unwrap();
  driver.memory.record_accesses(true);

  let refused = queue.pop(&driver.memory).unwrap_err();
  assert_eq!(refused, VirtqueueError::IndirectNotNegotiated { at: 0 });
  let table_range = GuestRange::new(GuestAddr(table), 64).unwrap();
  assert!(
    !touched(&driver.memory, &[table_range]),
    "the indirect table was never read"
  );
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    refused,
    "the queue stays faulted"
  );
  assert!(queue.is_faulted());
  assert_eq!(
    queue.counters().chains_refused,
    1,
    "one refusal, counted once"
  );
}

/// A chain that loops back on itself is refused as a loop, not walked until the length cap.
#[test]
fn a_looping_chain_is_refused() {
  let mut driver = SimDriver::new(8);
  let a = driver.buffer(8, 0);
  let b = driver.buffer(8, 0);
  driver.descriptor(0, a, 8, VIRTQ_DESC_F_NEXT, 1);
  driver.descriptor(1, b, 8, VIRTQ_DESC_F_NEXT, 0);
  driver.make_available(0);
  let mut queue = driver.queue(roomy(8)).unwrap();
  driver.memory.record_accesses(true);
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::DescriptorLoop { at: 0 }
  );
  let buffers = [GuestRange::new(GuestAddr(a), 16).unwrap()];
  assert!(!touched(&driver.memory, &buffers));
}

/// §2.7.5.2: a chain longer than the descriptor cap is refused; the cap is the queue size at most.
#[test]
fn a_chain_longer_than_the_descriptor_cap_is_refused() {
  let mut driver = SimDriver::new(8);
  let head = driver.chain(&[(8, false), (8, false), (8, false), (8, false), (8, true)]);
  driver.make_available(head);
  let caps = ChainCaps {
    max_descriptors: 4,
    ..roomy(8)
  };
  let mut queue = driver.queue(caps).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::ChainTooLong { walked: 5, cap: 4 }
  );
  assert!(
    Virtqueue::new(
      driver.layout,
      ChainCaps {
        max_descriptors: 9,
        ..roomy(8)
      },
      &driver.memory
    )
    .is_err(),
    "a descriptor cap above the queue size is refused at configuration"
  );
}

/// A `next` past the queue and a head past the queue are each refused typed.
#[test]
fn a_next_or_head_index_past_the_queue_is_refused() {
  let mut driver = SimDriver::new(4);
  let a = driver.buffer(8, 0);
  driver.descriptor(0, a, 8, VIRTQ_DESC_F_NEXT, 4);
  driver.make_available(0);
  let mut queue = driver.queue(roomy(4)).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::NextOutOfRange {
      at: 0,
      next: 4,
      size: 4
    }
  );

  let mut driver = SimDriver::new(4);
  driver.make_available(7);
  let mut queue = driver.queue(roomy(4)).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::HeadOutOfRange { head: 7, size: 4 }
  );
}

/// AC-4.12/T-4.14 "overflow lengths": `addr + len` past the end of the address space is refused
/// before any access, as is a `len = u32::MAX` descriptor whose range leaves guest memory.
#[test]
fn an_overflowing_or_oversized_length_is_refused_before_access() {
  let mut driver = SimDriver::new(4);
  driver.descriptor(0, u64::MAX - 8, u32::MAX, 0, 0);
  driver.make_available(0);
  let mut queue = driver.queue(roomy(4)).unwrap();
  driver.memory.record_accesses(true);
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::LengthOverflow {
      at: 0,
      address: u64::MAX - 8,
      len: u32::MAX
    }
  );

  let mut driver = SimDriver::new(4);
  driver.descriptor(0, BUFFER_BASE, u32::MAX, 0, 0);
  driver.make_available(0);
  let mut queue = driver.queue(roomy(4)).unwrap();
  driver.memory.record_accesses(true);
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::BufferOutsideGuestMemory {
      at: 0,
      address: BUFFER_BASE,
      len: u32::MAX
    }
  );
  let outside = [GuestRange::new(GuestAddr(BUFFER_BASE), u64::from(u32::MAX)).unwrap()];
  assert!(
    !touched(&driver.memory, &outside),
    "nothing was read from the buffer"
  );
}

/// AC-4.12/T-4.14 "unauthorized adjacent-page ranges": a buffer on the page just past the guest's
/// memory, and one that starts inside but runs off the end, are refused before any access; a
/// buffer that spans the gap between two mapped regions is refused too.
#[test]
fn a_buffer_outside_or_straddling_guest_memory_is_refused_before_access() {
  let mut driver = SimDriver::new(4);
  driver.descriptor(0, GUEST_RAM, 0x1000, 0, 0);
  driver.make_available(0);
  let mut queue = driver.queue(roomy(4)).unwrap();
  driver.memory.record_accesses(true);
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::BufferOutsideGuestMemory {
      at: 0,
      address: GUEST_RAM,
      len: 0x1000
    }
  );

  let mut driver = SimDriver::new(4);
  driver.descriptor(0, GUEST_RAM - 16, 32, VIRTQ_DESC_F_WRITE, 0);
  driver.make_available(0);
  let mut queue = driver.queue(roomy(4)).unwrap();
  driver.memory.record_accesses(true);
  assert!(matches!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::BufferOutsideGuestMemory { at: 0, .. }
  ));
  let straddle = [GuestRange::new(GuestAddr(GUEST_RAM - 16), 32).unwrap()];
  assert!(
    !touched(&driver.memory, &straddle),
    "the in-bounds half was not touched either"
  );

  // Two regions with a hole between them: a range across the hole is not contiguous guest memory.
  let mut driver = SimDriver::new(4);
  driver.memory =
    SimGuestMemory::with_regions(&[(0, GUEST_RAM), (2 * GUEST_RAM, GUEST_RAM)]).unwrap();
  driver.descriptor(0, GUEST_RAM - 8, 16, 0, 0);
  driver.make_available(0);
  let mut queue = driver.queue(roomy(4)).unwrap();
  assert!(matches!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::BufferOutsideGuestMemory { at: 0, .. }
  ));
}

/// A buffer aliasing one of the queue's own rings is refused: a device-writable descriptor over the
/// used ring would let the guest's reply overwrite the device's bookkeeping.
#[test]
fn a_buffer_aliasing_a_ring_is_refused() {
  let mut driver = SimDriver::new(4);
  let used = driver.layout.used_ring.0;
  driver.descriptor(0, used, 8, VIRTQ_DESC_F_WRITE, 0);
  driver.make_available(0);
  let mut queue = driver.queue(roomy(4)).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::BufferOverlapsRing {
      at: 0,
      ring: Ring::UsedRing
    }
  );
}

/// §2.7.4.2: a device-readable descriptor after a device-writable one is refused (the driver must
/// place every writable element after every readable one).
#[test]
fn a_readable_descriptor_after_a_writable_one_is_refused() {
  let mut driver = SimDriver::new(4);
  let head = driver.chain(&[(40, false), (16, true), (8, false)]);
  driver.make_available(head);
  let mut queue = driver.queue(roomy(4)).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::ReadableAfterWritable { at: head + 2 }
  );
}

/// The per-request byte caps: readable bytes past the cap and writable bytes past the cap are each
/// refused with the cap named, before any access.
#[test]
fn bytes_past_the_derived_caps_are_refused() {
  let mut driver = SimDriver::new(4);
  let head = driver.chain(&[(600, false), (500, false), (16, true)]);
  driver.make_available(head);
  let caps = ChainCaps {
    max_readable_bytes: 1000,
    ..roomy(4)
  };
  let mut queue = driver.queue(caps).unwrap();
  driver.memory.record_accesses(true);
  let chain_buffers = {
    let mut probe = driver.queue(roomy(4)).unwrap();
    buffers_of(&probe.pop(&driver.memory).unwrap().unwrap())
  };
  driver.memory.clear_accesses();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::ReadableBytesOverCap {
      bytes: 1100,
      cap: 1000
    }
  );
  assert!(!touched(&driver.memory, &chain_buffers));

  let mut driver = SimDriver::new(4);
  let head = driver.chain(&[(40, false), (700, true), (400, true)]);
  driver.make_available(head);
  let caps = ChainCaps {
    max_writable_bytes: 1000,
    ..roomy(4)
  };
  let mut queue = driver.queue(caps).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::WritableBytesOverCap {
      bytes: 1100,
      cap: 1000
    }
  );
}

/// An available index more than the queue size ahead of what the device consumed is a corrupt
/// ring, refused rather than walked.
#[test]
fn an_available_index_more_than_the_queue_size_ahead_is_refused() {
  let mut driver = SimDriver::new(4);
  driver.write_avail_idx(5);
  let mut queue = driver.queue(roomy(4)).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::AvailableIndexAhead {
      pending: 5,
      size: 4
    }
  );
}

/// The layout is validated at configuration: a queue size that is not a power of two (or zero, or
/// past the spec's 32768), a misaligned ring, a ring outside guest memory, and rings that overlap
/// are each refused typed before the queue exists.
#[test]
fn a_queue_layout_is_validated_at_configuration() {
  let driver = SimDriver::new(4);
  let layout = driver.layout;
  let caps = roomy(4);
  let bad_size =
    |size| Virtqueue::new(QueueLayout { size, ..layout }, caps, &driver.memory).unwrap_err();
  assert_eq!(bad_size(3), VirtqueueError::QueueSizeInvalid { size: 3 });
  assert_eq!(bad_size(0), VirtqueueError::QueueSizeInvalid { size: 0 });
  assert!(matches!(
    Virtqueue::new(
      QueueLayout {
        descriptor_table: GuestAddr(DESC_BASE + 1),
        ..layout
      },
      caps,
      &driver.memory
    )
    .unwrap_err(),
    VirtqueueError::RingMisaligned {
      ring: Ring::DescriptorTable,
      ..
    }
  ));
  assert!(matches!(
    Virtqueue::new(
      QueueLayout {
        used_ring: GuestAddr(GUEST_RAM - 4),
        ..layout
      },
      caps,
      &driver.memory
    )
    .unwrap_err(),
    VirtqueueError::RingOutsideGuestMemory {
      ring: Ring::UsedRing,
      ..
    }
  ));
  assert_eq!(
    Virtqueue::new(
      QueueLayout {
        used_ring: GuestAddr(layout.available_ring.0),
        ..layout
      },
      caps,
      &driver.memory
    )
    .unwrap_err(),
    VirtqueueError::RingsOverlap {
      first: Ring::AvailableRing,
      second: Ring::UsedRing
    }
  );
  // The spec's ceiling itself is accepted; one power of two above it is not representable in the
  // u16 the ring carries, so the ceiling is the largest configurable queue.
  let big = layout_for(32768);
  let big_memory = SimGuestMemory::new(4 * GUEST_RAM);
  assert!(Virtqueue::new(big, roomy(32768), &big_memory).is_ok());
}

//! The split-virtqueue machine driven by a simulated guest driver (§4.6 "virtio-fs and OCI
//! attachment contract": "queue descriptors, scatter/gather ranges, arithmetic and chained lengths
//! are validated within derived caps before access"; AC-4.12/T-4.14). The driver lays real virtio
//! 1.2 §2.7 rings out in a simulated guest memory, publishes descriptor chains through the
//! available ring, and reads the used ring back — the byte layouts a Linux guest's virtio driver
//! writes. Every hostile case the contract names is refused typed, and the simulated memory's
//! access log proves the refusal came before any buffer was touched.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{
  BUFFER_BASE, DESC_BASE, GUEST_RAM, SimDriver, USED_IDX_OFFSET, USED_RING_OFFSET, layout_at,
};
use slates_bridge_virtiofs::memory::{GuestAddr, GuestRange};
use slates_bridge_virtiofs::sim::SimGuestMemory;
use slates_bridge_virtiofs::virtqueue::{
  ChainCaps, DescriptorChain, QueueLayout, Ring, VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT,
  VIRTQ_DESC_F_WRITE, Virtqueue, VirtqueueError,
};

/// The one queue these tests drive.
const Q: usize = 0;

/// A driver with one queue of `size`.
fn sim_driver(size: u16) -> SimDriver {
  SimDriver::new(&[size])
}

/// The queue under test over the driver's rings.
fn queue_over(driver: &SimDriver, caps: ChainCaps) -> Result<Virtqueue, VirtqueueError> {
  Virtqueue::new(driver.layout(Q), caps, &driver.memory)
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
  let mut driver = sim_driver(8);
  let first = driver.chain(Q, &[(40, false), (100, false), (16, true), (500, true)]);
  let second = driver.chain(Q, &[(48, false), (0, false), (16, true)]);
  driver.make_available(Q, first);
  driver.make_available(Q, second);
  let mut queue = queue_over(&driver, roomy(8)).unwrap();

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
/// and the driver reads exactly that element back; a used length past the chain's writable bytes
/// is refused; the used counter moved.
#[test]
fn a_used_element_is_published_after_its_id_and_length_are_written() {
  let mut driver = sim_driver(4);
  let head = driver.chain(Q, &[(40, false), (64, true)]);
  driver.make_available(Q, head);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
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
  let used_ring = driver.layout(Q).used_ring.0;
  assert_eq!(
    writes.len(),
    2,
    "one element write, then one index write: {writes:?}"
  );
  assert_eq!(
    writes[0].start().0,
    used_ring + USED_RING_OFFSET,
    "the element is written first"
  );
  assert_eq!(
    writes[1].start().0,
    used_ring + USED_IDX_OFFSET,
    "the index is published second"
  );
  assert_eq!(driver.used(Q), vec![(u32::from(head), 7)]);
  assert_eq!(queue.counters().used_published, 1);
}

/// Ring indices wrap at the queue size: six chains through a four-entry queue land in the used ring
/// in order, the fifth and sixth reusing the first two slots.
#[test]
fn ring_indices_wrap_at_the_queue_size() {
  let mut driver = sim_driver(4);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
  let mut heads = Vec::new();
  for _ in 0..6 {
    let head = driver.chain(Q, &[(8, false)]);
    driver.make_available(Q, head);
    let chain = queue.pop(&driver.memory).unwrap().unwrap();
    queue.push_used(&mut driver.memory, &chain, 0).unwrap();
    let (id, len) = driver.reap(Q).expect("the used element");
    assert_eq!((id, len), (head, 0));
    heads.push((u32::from(head), 0));
  }
  let used = driver.used(Q);
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
  let mut driver = sim_driver(4);
  let table = driver.buffer(64, 0x11);
  driver.descriptor(Q, 0, table, 64, VIRTQ_DESC_F_INDIRECT, 0);
  driver.make_available(Q, 0);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
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
  let mut driver = sim_driver(8);
  let a = driver.buffer(8, 0);
  let b = driver.buffer(8, 0);
  driver.descriptor(Q, 0, a, 8, VIRTQ_DESC_F_NEXT, 1);
  driver.descriptor(Q, 1, b, 8, VIRTQ_DESC_F_NEXT, 0);
  driver.make_available(Q, 0);
  let mut queue = queue_over(&driver, roomy(8)).unwrap();
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
  let mut driver = sim_driver(8);
  let head = driver.chain(
    Q,
    &[(8, false), (8, false), (8, false), (8, false), (8, true)],
  );
  driver.make_available(Q, head);
  let caps = ChainCaps {
    max_descriptors: 4,
    ..roomy(8)
  };
  let mut queue = queue_over(&driver, caps).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::ChainTooLong { walked: 5, cap: 4 }
  );
  assert_eq!(
    Virtqueue::new(
      driver.layout(Q),
      ChainCaps {
        max_descriptors: 9,
        ..roomy(8)
      },
      &driver.memory
    )
    .unwrap_err(),
    VirtqueueError::ChainCapInvalid { cap: 9, size: 8 },
    "a descriptor cap above the queue size is refused at configuration"
  );
}

/// A `next` past the queue and a head past the queue are each refused typed.
#[test]
fn a_next_or_head_index_past_the_queue_is_refused() {
  let mut driver = sim_driver(4);
  let a = driver.buffer(8, 0);
  driver.descriptor(Q, 0, a, 8, VIRTQ_DESC_F_NEXT, 4);
  driver.make_available(Q, 0);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::NextOutOfRange {
      at: 0,
      next: 4,
      size: 4
    }
  );

  let mut driver = sim_driver(4);
  driver.make_available(Q, 7);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::HeadOutOfRange { head: 7, size: 4 }
  );
}

/// AC-4.12/T-4.14 "overflow lengths": `addr + len` past the end of the address space is refused
/// before any access, as is a `len = u32::MAX` descriptor whose range leaves guest memory.
#[test]
fn an_overflowing_or_oversized_length_is_refused_before_access() {
  let mut driver = sim_driver(4);
  driver.descriptor(Q, 0, u64::MAX - 8, u32::MAX, 0, 0);
  driver.make_available(Q, 0);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
  driver.memory.record_accesses(true);
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::LengthOverflow {
      at: 0,
      address: u64::MAX - 8,
      len: u32::MAX
    }
  );

  let mut driver = sim_driver(4);
  driver.descriptor(Q, 0, BUFFER_BASE, u32::MAX, 0, 0);
  driver.make_available(Q, 0);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
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
  let mut driver = sim_driver(4);
  driver.descriptor(Q, 0, GUEST_RAM, 0x1000, 0, 0);
  driver.make_available(Q, 0);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
  driver.memory.record_accesses(true);
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::BufferOutsideGuestMemory {
      at: 0,
      address: GUEST_RAM,
      len: 0x1000
    }
  );

  let mut driver = sim_driver(4);
  driver.descriptor(Q, 0, GUEST_RAM - 16, 32, VIRTQ_DESC_F_WRITE, 0);
  driver.make_available(Q, 0);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
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
  let mut driver = sim_driver(4);
  driver.memory =
    SimGuestMemory::with_regions(&[(0, GUEST_RAM), (2 * GUEST_RAM, GUEST_RAM)]).unwrap();
  driver.descriptor(Q, 0, GUEST_RAM - 8, 16, 0, 0);
  driver.make_available(Q, 0);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
  assert!(matches!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::BufferOutsideGuestMemory { at: 0, .. }
  ));
}

/// A buffer aliasing one of the queue's own rings is refused: a device-writable descriptor over the
/// used ring would let the guest's reply overwrite the device's bookkeeping.
#[test]
fn a_buffer_aliasing_a_ring_is_refused() {
  let mut driver = sim_driver(4);
  let used = driver.layout(Q).used_ring.0;
  driver.descriptor(Q, 0, used, 8, VIRTQ_DESC_F_WRITE, 0);
  driver.make_available(Q, 0);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
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
  let mut driver = sim_driver(4);
  let head = driver.chain(Q, &[(40, false), (16, true), (8, false)]);
  driver.make_available(Q, head);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::ReadableAfterWritable { at: head + 2 }
  );
}

/// The per-request byte caps: readable bytes past the cap and writable bytes past the cap are each
/// refused with the cap named, before any access.
#[test]
fn bytes_past_the_derived_caps_are_refused() {
  let mut driver = sim_driver(4);
  let head = driver.chain(Q, &[(600, false), (500, false), (16, true)]);
  driver.make_available(Q, head);
  let chain_buffers = {
    let mut probe = queue_over(&driver, roomy(4)).unwrap();
    buffers_of(&probe.pop(&driver.memory).unwrap().unwrap())
  };
  let caps = ChainCaps {
    max_readable_bytes: 1000,
    ..roomy(4)
  };
  let mut queue = queue_over(&driver, caps).unwrap();
  driver.memory.record_accesses(true);
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::ReadableBytesOverCap {
      bytes: 1100,
      cap: 1000
    }
  );
  assert!(!touched(&driver.memory, &chain_buffers));

  let mut driver = sim_driver(4);
  let head = driver.chain(Q, &[(40, false), (700, true), (400, true)]);
  driver.make_available(Q, head);
  let caps = ChainCaps {
    max_writable_bytes: 1000,
    ..roomy(4)
  };
  let mut queue = queue_over(&driver, caps).unwrap();
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
  let mut driver = sim_driver(4);
  driver.write_avail_idx(Q, 5);
  let mut queue = queue_over(&driver, roomy(4)).unwrap();
  assert_eq!(
    queue.pop(&driver.memory).unwrap_err(),
    VirtqueueError::AvailableIndexAhead {
      pending: 5,
      size: 4
    }
  );
}

/// The layout is validated at configuration: a queue size that is not a power of two (or zero), a
/// misaligned ring, a ring outside guest memory, and rings that overlap are each refused typed
/// before the queue exists; the specification's largest queue is accepted.
#[test]
fn a_queue_layout_is_validated_at_configuration() {
  let driver = sim_driver(4);
  let layout = driver.layout(Q);
  let caps = roomy(4);
  let configure = |layout: QueueLayout| Virtqueue::new(layout, caps, &driver.memory).unwrap_err();
  assert_eq!(
    configure(QueueLayout { size: 3, ..layout }),
    VirtqueueError::QueueSizeInvalid { size: 3 }
  );
  assert_eq!(
    configure(QueueLayout { size: 0, ..layout }),
    VirtqueueError::QueueSizeInvalid { size: 0 }
  );
  assert!(matches!(
    configure(QueueLayout {
      descriptor_table: GuestAddr(DESC_BASE + 1),
      ..layout
    }),
    VirtqueueError::RingMisaligned {
      ring: Ring::DescriptorTable,
      ..
    }
  ));
  assert!(matches!(
    configure(QueueLayout {
      used_ring: GuestAddr(GUEST_RAM - 4),
      ..layout
    }),
    VirtqueueError::RingOutsideGuestMemory {
      ring: Ring::UsedRing,
      ..
    }
  ));
  assert_eq!(
    configure(QueueLayout {
      used_ring: GuestAddr(layout.available_ring.0),
      ..layout
    }),
    VirtqueueError::RingsOverlap {
      first: Ring::AvailableRing,
      second: Ring::UsedRing
    }
  );
  let big = layout_at(DESC_BASE, 32768);
  let big_memory = SimGuestMemory::new(4 * GUEST_RAM);
  assert!(Virtqueue::new(big, roomy(32768), &big_memory).is_ok());
}

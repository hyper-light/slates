//! The split-virtqueue machine (virtio 1.2 §2.7 "Split Virtqueues"), sans-io over the
//! [`GuestMemory`] seam: the three rings a driver lays out in guest memory (the descriptor table,
//! the available ring, the used ring), the walk of one descriptor chain from the available ring
//! into a validated [`DescriptorChain`], and the publication of a used element. This is the device
//! half of the protocol; the driver half lives in the guest kernel (and, for the tests, in a
//! simulated driver that writes the same bytes).
//!
//! Every check the contract names (§4.6 A-9: "queue descriptors, scatter/gather ranges, arithmetic
//! and chained lengths are validated within derived caps before access") happens in the walk,
//! before any buffer byte is read or written: the chain length against the descriptor cap (the
//! queue size at most, §2.7.5.2); a loop (a descriptor visited twice); a `next` or a head past the
//! queue; an indirect descriptor (this device does not offer `VIRTIO_F_INDIRECT_DESC`, so one is a
//! driver violation of §2.7.5.3.1); `addr + len` overflowing; a buffer outside the mapped guest
//! memory, straddling its edge, or aliasing one of the queue's own rings; a device-readable buffer
//! after a device-writable one (§2.7.4.2); and the readable and writable byte totals against the
//! per-request caps the FUSE negotiation derives. A refusal faults the queue — the device does not
//! skip the chain and carry on, it stops consuming that queue until the driver resets it — which is
//! what the specification's `DEVICE_NEEDS_RESET` status is for (§2.1.2). Nothing here retries.
//!
//! The ring formats are read and written field by field through little-endian byte arrays, never a
//! cast: the crate holds no `unsafe`. The evidence for the shapes is the specification itself, cited
//! on each `Format:` constant; the derived caps are computed by the device module and recorded in
//! `docs/wip/virtiofs.md`.

use std::fmt;

use crate::memory::{
  GuestAddr, GuestMemory, GuestMemoryError, GuestRange, read_u16, read_u32, read_u64, write_bytes,
  write_u16,
};

/// Format: virtio 1.2 §2.7.5 — one descriptor is `addr` (le64), `len` (le32), `flags` (le16),
/// `next` (le16): 16 bytes.
pub const DESCRIPTOR_LEN: u64 = 16;
/// Format: §2.7.5 `VIRTQ_DESC_F_NEXT` — the buffer continues in the descriptor `next` names.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Format: §2.7.5 `VIRTQ_DESC_F_WRITE` — the buffer is device-writable (else device-readable).
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// Format: §2.7.5 `VIRTQ_DESC_F_INDIRECT` — the buffer holds a table of descriptors.
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;
/// Format: §2.7.6 `VIRTQ_AVAIL_F_NO_INTERRUPT` — the driver asks the device not to interrupt it.
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
/// Format: §2.7 — "Queue Size ... is always a power of 2. The maximum Queue Size value is 32768."
pub const MAX_QUEUE_SIZE: u16 = 32768;
/// Format: §2.7 — the descriptor table's alignment.
const DESCRIPTOR_TABLE_ALIGN: u64 = 16;
/// Format: §2.7 — the available ring's alignment.
const AVAILABLE_RING_ALIGN: u64 = 2;
/// Format: §2.7 — the used ring's alignment.
const USED_RING_ALIGN: u64 = 4;
/// Format: §2.7.6 — the available ring is `flags` (le16), `idx` (le16), `ring[size]` (le16 each),
/// `used_event` (le16): 6 + 2 × size bytes.
const AVAILABLE_RING_FIXED_LEN: u64 = 6;
/// Format: §2.7.6 — one available-ring entry (a head index) is two bytes.
const AVAILABLE_ENTRY_LEN: u64 = 2;
/// Format: §2.7.6 — `idx` follows `flags`.
const AVAILABLE_IDX_OFFSET: u64 = 2;
/// Format: §2.7.6 — the entries follow `flags` and `idx`.
const AVAILABLE_RING_OFFSET: u64 = 4;
/// Format: §2.7.8 — the used ring is `flags` (le16), `idx` (le16), `ring[size]` (`{id: le32,
/// len: le32}` each), `avail_event` (le16): 6 + 8 × size bytes.
const USED_RING_FIXED_LEN: u64 = 6;
/// Format: §2.7.8 — one used element (`id`, `len`) is eight bytes.
const USED_ELEMENT_BYTES: usize = 8;
/// Format: the same eight bytes as a guest-address stride (a widening of `USED_ELEMENT_BYTES`).
const USED_ELEMENT_LEN: u64 = USED_ELEMENT_BYTES as u64;
/// Format: §2.7.5 — the byte offsets of `len`, `flags` and `next` inside one descriptor (after
/// the eight-byte `addr`, the four-byte `len`, the two-byte `flags`).
const DESCRIPTOR_LEN_OFFSET: u64 = 8;
const DESCRIPTOR_FLAGS_OFFSET: u64 = 12;
const DESCRIPTOR_NEXT_OFFSET: u64 = 14;
/// Format: §2.7.8 — `idx` follows `flags`.
const USED_IDX_OFFSET: u64 = 2;
/// Format: §2.7.8 — the elements follow `flags` and `idx`.
const USED_RING_OFFSET: u64 = 4;

/// Which of a queue's three rings a refusal is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ring {
  /// The descriptor table (§2.7.5).
  DescriptorTable,
  /// The available ring (§2.7.6).
  AvailableRing,
  /// The used ring (§2.7.8).
  UsedRing,
}

impl Ring {
  /// The specification's alignment for the ring.
  const fn alignment(self) -> u64 {
    match self {
      Ring::DescriptorTable => DESCRIPTOR_TABLE_ALIGN,
      Ring::AvailableRing => AVAILABLE_RING_ALIGN,
      Ring::UsedRing => USED_RING_ALIGN,
    }
  }

  /// The ring's length in bytes for a queue of `size`.
  const fn len(self, size: u16) -> u64 {
    let size = size as u64;
    match self {
      Ring::DescriptorTable => size * DESCRIPTOR_LEN,
      Ring::AvailableRing => AVAILABLE_RING_FIXED_LEN + size * AVAILABLE_ENTRY_LEN,
      Ring::UsedRing => USED_RING_FIXED_LEN + size * USED_ELEMENT_LEN,
    }
  }
}

/// Where a driver laid a queue's rings out (the transport's queue registers name these addresses:
/// `queue_desc`, `queue_driver`, `queue_device` in §4.1.4.3 for PCI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLayout {
  /// The queue size: a power of two, at most [`MAX_QUEUE_SIZE`].
  pub size: u16,
  /// The descriptor table.
  pub descriptor_table: GuestAddr,
  /// The available ring (the driver area).
  pub available_ring: GuestAddr,
  /// The used ring (the device area).
  pub used_ring: GuestAddr,
}

impl QueueLayout {
  const fn base(&self, ring: Ring) -> GuestAddr {
    match ring {
      Ring::DescriptorTable => self.descriptor_table,
      Ring::AvailableRing => self.available_ring,
      Ring::UsedRing => self.used_ring,
    }
  }
}

/// The caps one request's chain is validated within (derived by the device from the FUSE
/// negotiation and the queue size; see the device module and `docs/wip/virtiofs.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainCaps {
  /// The most descriptors one chain may have: the queue size at most (§2.7.5.2).
  pub max_descriptors: u16,
  /// The most device-readable bytes one chain may carry (the largest FUSE request).
  pub max_readable_bytes: u64,
  /// The most device-writable bytes one chain may offer (the largest FUSE reply).
  pub max_writable_bytes: u64,
}

/// The queue's counters: non-vacuity witnesses for the tests and the status report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VirtqueueCounters {
  /// Chains taken from the available ring.
  pub chains_popped: u64,
  /// Chains refused (each faults the queue; counted once per fault).
  pub chains_refused: u64,
  /// Used elements published.
  pub used_published: u64,
}

/// One validated descriptor chain: the head index the used element will name, the device-readable
/// ranges in chain order, then the device-writable ones, and their byte totals. Every range lies
/// inside guest memory, off the rings, and within the caps — the walk proved it, so the device may
/// gather and scatter without a further check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DescriptorChain {
  /// The head descriptor's index.
  pub head: u16,
  /// The device-readable buffers, in chain order.
  pub readable: Vec<GuestRange>,
  /// The device-writable buffers, in chain order.
  pub writable: Vec<GuestRange>,
  /// The readable total.
  pub readable_bytes: u64,
  /// The writable total.
  pub writable_bytes: u64,
}

/// The closed refusal taxonomy of the virtqueue machine. Each names where in the chain it was found
/// (`at`, a descriptor index) so a VMM log can point at the driver's bug; none is ever a panic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VirtqueueError {
  /// The queue size is zero, not a power of two, or above the specification's ceiling.
  QueueSizeInvalid {
    /// The size offered.
    size: u16,
  },
  /// The descriptor cap is zero or above the queue size.
  ChainCapInvalid {
    /// The cap.
    cap: u16,
    /// The queue size.
    size: u16,
  },
  /// A ring's address does not meet its alignment.
  RingMisaligned {
    /// Which ring.
    ring: Ring,
    /// Its address.
    address: u64,
    /// The alignment required.
    alignment: u64,
  },
  /// A ring does not lie inside guest memory (or overflows the address space).
  RingOutsideGuestMemory {
    /// Which ring.
    ring: Ring,
    /// Its start.
    start: u64,
    /// Its length.
    len: u64,
  },
  /// Two rings share bytes.
  RingsOverlap {
    /// The first ring.
    first: Ring,
    /// The second ring.
    second: Ring,
  },
  /// The driver's available index is more than the queue size ahead of what the device consumed.
  AvailableIndexAhead {
    /// Entries the index claims are pending.
    pending: u16,
    /// The queue size.
    size: u16,
  },
  /// An available-ring entry names a descriptor past the table.
  HeadOutOfRange {
    /// The head index.
    head: u16,
    /// The queue size.
    size: u16,
  },
  /// A descriptor's `next` names a descriptor past the table.
  NextOutOfRange {
    /// The descriptor whose `next` is bad.
    at: u16,
    /// The `next` it carries.
    next: u16,
    /// The queue size.
    size: u16,
  },
  /// The chain revisits a descriptor.
  DescriptorLoop {
    /// The descriptor reached twice.
    at: u16,
  },
  /// The chain has more descriptors than the cap.
  ChainTooLong {
    /// Descriptors walked when the cap was passed.
    walked: u32,
    /// The cap.
    cap: u16,
  },
  /// A descriptor carries `VIRTQ_DESC_F_INDIRECT`, which this device does not negotiate.
  IndirectNotNegotiated {
    /// The descriptor.
    at: u16,
  },
  /// `addr + len` overflows the address space.
  LengthOverflow {
    /// The descriptor.
    at: u16,
    /// Its address.
    address: u64,
    /// Its length.
    len: u32,
  },
  /// The buffer is outside guest memory, or straddles a region's edge.
  BufferOutsideGuestMemory {
    /// The descriptor.
    at: u16,
    /// Its address.
    address: u64,
    /// Its length.
    len: u32,
  },
  /// The buffer aliases one of the queue's own rings.
  BufferOverlapsRing {
    /// The descriptor.
    at: u16,
    /// The ring it aliases.
    ring: Ring,
  },
  /// A device-readable descriptor follows a device-writable one (§2.7.4.2).
  ReadableAfterWritable {
    /// The offending descriptor.
    at: u16,
  },
  /// The chain's readable bytes exceed the cap.
  ReadableBytesOverCap {
    /// The bytes reached.
    bytes: u64,
    /// The cap.
    cap: u64,
  },
  /// The chain's writable bytes exceed the cap.
  WritableBytesOverCap {
    /// The bytes reached.
    bytes: u64,
    /// The cap.
    cap: u64,
  },
  /// A used element's `len` exceeds what the chain offered to be written.
  UsedLengthOverChain {
    /// The length claimed.
    len: u32,
    /// The chain's writable bytes.
    writable: u64,
  },
  /// A ring access the seam refused (cannot happen after validation; carried typed regardless).
  Memory(GuestMemoryError),
}

impl fmt::Display for VirtqueueError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::QueueSizeInvalid { size } => {
        write!(f, "queue size {size} is not a power of two in range")
      }
      Self::ChainCapInvalid { cap, size } => {
        write!(
          f,
          "descriptor cap {cap} is zero or above the queue size {size}"
        )
      }
      Self::RingMisaligned {
        ring,
        address,
        alignment,
      } => write!(
        f,
        "{ring:?} at {address:#x} is not {alignment}-byte aligned"
      ),
      Self::RingOutsideGuestMemory { ring, start, len } => {
        write!(f, "{ring:?} at {start:#x}+{len:#x} is outside guest memory")
      }
      Self::RingsOverlap { first, second } => write!(f, "{first:?} overlaps {second:?}"),
      Self::AvailableIndexAhead { pending, size } => {
        write!(
          f,
          "available index claims {pending} pending on a queue of {size}"
        )
      }
      Self::HeadOutOfRange { head, size } => write!(f, "head {head} past the queue of {size}"),
      Self::NextOutOfRange { at, next, size } => {
        write!(
          f,
          "descriptor {at} names next {next} past the queue of {size}"
        )
      }
      Self::DescriptorLoop { at } => write!(f, "descriptor {at} is reached twice"),
      Self::ChainTooLong { walked, cap } => {
        write!(f, "chain reached {walked} descriptors, cap {cap}")
      }
      Self::IndirectNotNegotiated { at } => {
        write!(
          f,
          "descriptor {at} is indirect; VIRTIO_F_INDIRECT_DESC was not negotiated"
        )
      }
      Self::LengthOverflow { at, address, len } => {
        write!(f, "descriptor {at}: {address:#x}+{len:#x} overflows")
      }
      Self::BufferOutsideGuestMemory { at, address, len } => {
        write!(
          f,
          "descriptor {at}: {address:#x}+{len:#x} is outside guest memory"
        )
      }
      Self::BufferOverlapsRing { at, ring } => {
        write!(f, "descriptor {at}'s buffer aliases the {ring:?}")
      }
      Self::ReadableAfterWritable { at } => {
        write!(
          f,
          "descriptor {at} is device-readable after a device-writable one"
        )
      }
      Self::ReadableBytesOverCap { bytes, cap } => {
        write!(f, "readable bytes {bytes} exceed the cap {cap}")
      }
      Self::WritableBytesOverCap { bytes, cap } => {
        write!(f, "writable bytes {bytes} exceed the cap {cap}")
      }
      Self::UsedLengthOverChain { len, writable } => {
        write!(
          f,
          "used length {len} exceeds the chain's {writable} writable bytes"
        )
      }
      Self::Memory(e) => write!(f, "guest memory: {e}"),
    }
  }
}

impl std::error::Error for VirtqueueError {}

impl From<GuestMemoryError> for VirtqueueError {
  fn from(e: GuestMemoryError) -> Self {
    Self::Memory(e)
  }
}

/// One descriptor as read from the table.
struct Descriptor {
  addr: u64,
  len: u32,
  flags: u16,
  next: u16,
}

/// Format: the bits one visited-set word (a `u64`) holds.
const VISITED_WORD_BITS: u16 = 64;

/// A split virtqueue the device consumes: its validated layout and caps, the device's copies of the
/// two indices it owns (§2.7.6, §2.7.8: the driver owns `avail.idx`, the device owns `used.idx` and
/// its own "next available" position), the fault, and the counters.
pub struct Virtqueue {
  layout: QueueLayout,
  rings: [GuestRange; 3],
  caps: ChainCaps,
  next_avail: u16,
  next_used: u16,
  fault: Option<VirtqueueError>,
  visited: Vec<u64>,
  counters: VirtqueueCounters,
}

impl fmt::Debug for Virtqueue {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Virtqueue")
      .field("size", &self.layout.size)
      .field("next_avail", &self.next_avail)
      .field("next_used", &self.next_used)
      .field("fault", &self.fault)
      .finish()
  }
}

/// The rings, in the order the overlap check reports them.
const RINGS: [Ring; 3] = [Ring::DescriptorTable, Ring::AvailableRing, Ring::UsedRing];

impl Virtqueue {
  /// A queue over `layout` within `memory`, validated before it exists: the size, the descriptor
  /// cap, each ring's alignment and residence in guest memory, and that the rings are disjoint.
  pub fn new(
    layout: QueueLayout,
    caps: ChainCaps,
    memory: &dyn GuestMemory,
  ) -> Result<Virtqueue, VirtqueueError> {
    validate_size(layout.size)?;
    if caps.max_descriptors == 0 || caps.max_descriptors > layout.size {
      return Err(VirtqueueError::ChainCapInvalid {
        cap: caps.max_descriptors,
        size: layout.size,
      });
    }
    let rings = [
      ring_range(&layout, Ring::DescriptorTable, memory)?,
      ring_range(&layout, Ring::AvailableRing, memory)?,
      ring_range(&layout, Ring::UsedRing, memory)?,
    ];
    check_disjoint(&rings)?;
    let words = usize::from(layout.size.div_ceil(VISITED_WORD_BITS));
    Ok(Virtqueue {
      layout,
      rings,
      caps,
      next_avail: 0,
      next_used: 0,
      fault: None,
      visited: vec![0; words],
      counters: VirtqueueCounters::default(),
    })
  }

  /// The layout.
  pub fn layout(&self) -> QueueLayout {
    self.layout
  }

  /// The caps.
  pub fn caps(&self) -> ChainCaps {
    self.caps
  }

  /// The counters.
  pub fn counters(&self) -> VirtqueueCounters {
    self.counters
  }

  /// Whether a refusal faulted the queue (it consumes nothing more until the driver resets it).
  pub fn is_faulted(&self) -> bool {
    self.fault.is_some()
  }

  /// The fault, when the queue is faulted.
  pub fn fault(&self) -> Option<&VirtqueueError> {
    self.fault.as_ref()
  }

  /// Chains the driver has published that the device has not consumed, as the available index
  /// says (a corrupt index past the queue size is reported as-is here; `pop` refuses it).
  pub fn pending(&self, memory: &dyn GuestMemory) -> Result<u16, VirtqueueError> {
    let avail_idx = read_u16(
      memory,
      self.ring_field(Ring::AvailableRing, AVAILABLE_IDX_OFFSET),
    )?;
    Ok(avail_idx.wrapping_sub(self.next_avail))
  }

  /// Whether the driver wants an interrupt for used buffers (§2.7.7: it clears
  /// `VIRTQ_AVAIL_F_NO_INTERRUPT` to ask for one; without `VIRTIO_F_EVENT_IDX` that flag is the
  /// whole rule).
  pub fn interrupts_wanted(&self, memory: &dyn GuestMemory) -> Result<bool, VirtqueueError> {
    let flags = read_u16(memory, self.layout.available_ring)?;
    Ok(flags & VIRTQ_AVAIL_F_NO_INTERRUPT == 0)
  }

  /// Takes the next available chain, validated whole before any buffer is touched: `Ok(None)`
  /// when the driver has published nothing new, `Ok(Some(chain))` when one was taken, and a typed
  /// refusal that faults the queue (repeated on every later pop) when the chain is malformed.
  /// [`Virtqueue::peek`] then [`Virtqueue::advance`].
  pub fn pop(
    &mut self,
    memory: &dyn GuestMemory,
  ) -> Result<Option<DescriptorChain>, VirtqueueError> {
    let chain = self.peek(memory)?;
    if chain.is_some() {
      self.advance();
    }
    Ok(chain)
  }

  /// Validates the next available chain without consuming it, so a caller can admit it (charge
  /// its credits, §4.6 A-9) before it is taken; the same chain comes back until [`Virtqueue::advance`]
  /// consumes it. `Ok(None)` when nothing is published; a malformed chain faults the queue.
  pub fn peek(
    &mut self,
    memory: &dyn GuestMemory,
  ) -> Result<Option<DescriptorChain>, VirtqueueError> {
    if let Some(fault) = &self.fault {
      return Err(fault.clone());
    }
    let pending = self.pending(memory)?;
    if pending == 0 {
      return Ok(None);
    }
    if pending > self.layout.size {
      return Err(self.record_fault(VirtqueueError::AvailableIndexAhead {
        pending,
        size: self.layout.size,
      }));
    }
    let slot = u64::from(self.next_avail % self.layout.size);
    let entry = self.ring_field(
      Ring::AvailableRing,
      AVAILABLE_RING_OFFSET + slot * AVAILABLE_ENTRY_LEN,
    );
    let head = read_u16(memory, entry)?;
    if head >= self.layout.size {
      return Err(self.record_fault(VirtqueueError::HeadOutOfRange {
        head,
        size: self.layout.size,
      }));
    }
    match self.walk(memory, head) {
      Ok(chain) => Ok(Some(chain)),
      Err(refusal) => Err(self.record_fault(refusal)),
    }
  }

  /// Consumes the chain the last [`Virtqueue::peek`] returned: the device's available position
  /// moves past it and the pop counter moves.
  pub fn advance(&mut self) {
    self.next_avail = self.next_avail.wrapping_add(1);
    self.counters.chains_popped = self.counters.chains_popped.saturating_add(1);
  }

  /// Publishes a used element for `chain`: the element (`id`, `len`) is written whole, then the
  /// used index (§2.7.8.2: "the device MUST set len prior to updating the used idx"). `written`
  /// must not exceed the chain's writable bytes.
  pub fn push_used(
    &mut self,
    memory: &mut dyn GuestMemory,
    chain: &DescriptorChain,
    written: u32,
  ) -> Result<(), VirtqueueError> {
    if u64::from(written) > chain.writable_bytes {
      return Err(VirtqueueError::UsedLengthOverChain {
        len: written,
        writable: chain.writable_bytes,
      });
    }
    let slot = u64::from(self.next_used % self.layout.size);
    let element = self.ring_field(Ring::UsedRing, USED_RING_OFFSET + slot * USED_ELEMENT_LEN);
    let mut bytes = [0u8; USED_ELEMENT_BYTES];
    bytes[..size_of::<u32>()].copy_from_slice(&u32::from(chain.head).to_le_bytes());
    bytes[size_of::<u32>()..].copy_from_slice(&written.to_le_bytes());
    write_bytes(memory, element, &bytes)?;
    self.next_used = self.next_used.wrapping_add(1);
    write_u16(
      memory,
      self.ring_field(Ring::UsedRing, USED_IDX_OFFSET),
      self.next_used,
    )?;
    self.counters.used_published = self.counters.used_published.saturating_add(1);
    Ok(())
  }

  /// Records the first fault and counts it; returns the refusal for the caller to hand back.
  fn record_fault(&mut self, refusal: VirtqueueError) -> VirtqueueError {
    self.fault = Some(refusal.clone());
    self.counters.chains_refused = self.counters.chains_refused.saturating_add(1);
    refusal
  }

  /// The address of a field at `offset` inside `ring` (inside the validated ring, so it cannot
  /// overflow).
  fn ring_field(&self, ring: Ring, offset: u64) -> GuestAddr {
    GuestAddr(self.layout.base(ring).0 + offset)
  }

  /// Walks the chain from `head`, validating each descriptor before the next is followed.
  fn walk(
    &mut self,
    memory: &dyn GuestMemory,
    head: u16,
  ) -> Result<DescriptorChain, VirtqueueError> {
    self.visited.fill(0);
    let mut chain = DescriptorChain {
      head,
      readable: Vec::new(),
      writable: Vec::new(),
      readable_bytes: 0,
      writable_bytes: 0,
    };
    let mut index = head;
    let mut walked: u32 = 0;
    loop {
      if self.mark_visited(index) {
        return Err(VirtqueueError::DescriptorLoop { at: index });
      }
      walked = walked.saturating_add(1);
      if walked > u32::from(self.caps.max_descriptors) {
        return Err(VirtqueueError::ChainTooLong {
          walked,
          cap: self.caps.max_descriptors,
        });
      }
      let descriptor = self.read_descriptor(memory, index)?;
      let range = self.check_descriptor(index, &descriptor, memory)?;
      self.account(&mut chain, index, &descriptor, range)?;
      if descriptor.flags & VIRTQ_DESC_F_NEXT == 0 {
        return Ok(chain);
      }
      if descriptor.next >= self.layout.size {
        return Err(VirtqueueError::NextOutOfRange {
          at: index,
          next: descriptor.next,
          size: self.layout.size,
        });
      }
      index = descriptor.next;
    }
  }

  /// Marks `index` visited; true when it already was.
  fn mark_visited(&mut self, index: u16) -> bool {
    let word = usize::from(index / VISITED_WORD_BITS);
    let bit = 1u64 << (index % VISITED_WORD_BITS);
    let Some(slot) = self.visited.get_mut(word) else {
      return true;
    };
    let seen = *slot & bit != 0;
    *slot |= bit;
    seen
  }

  /// Reads descriptor `index` from the table (inside the validated table, so the reads are in
  /// bounds; a seam refusal is carried typed regardless).
  fn read_descriptor(
    &self,
    memory: &dyn GuestMemory,
    index: u16,
  ) -> Result<Descriptor, VirtqueueError> {
    // Inside the validated table, so the field addresses cannot overflow.
    let at = self.ring_field(Ring::DescriptorTable, u64::from(index) * DESCRIPTOR_LEN);
    let addr = read_u64(memory, at)?;
    let len = read_u32(memory, GuestAddr(at.0 + DESCRIPTOR_LEN_OFFSET))?;
    let flags = read_u16(memory, GuestAddr(at.0 + DESCRIPTOR_FLAGS_OFFSET))?;
    let next = read_u16(memory, GuestAddr(at.0 + DESCRIPTOR_NEXT_OFFSET))?;
    Ok(Descriptor {
      addr,
      len,
      flags,
      next,
    })
  }

  /// Validates one descriptor's buffer: not indirect, no overflow, inside guest memory, off the
  /// rings. Returns the buffer's range; touches no buffer byte.
  fn check_descriptor(
    &self,
    at: u16,
    descriptor: &Descriptor,
    memory: &dyn GuestMemory,
  ) -> Result<GuestRange, VirtqueueError> {
    if descriptor.flags & VIRTQ_DESC_F_INDIRECT != 0 {
      return Err(VirtqueueError::IndirectNotNegotiated { at });
    }
    let range =
      GuestRange::new(GuestAddr(descriptor.addr), u64::from(descriptor.len)).map_err(|_| {
        VirtqueueError::LengthOverflow {
          at,
          address: descriptor.addr,
          len: descriptor.len,
        }
      })?;
    memory
      .check(range)
      .map_err(|_| VirtqueueError::BufferOutsideGuestMemory {
        at,
        address: descriptor.addr,
        len: descriptor.len,
      })?;
    for (ring, ring_range) in RINGS.iter().zip(self.rings.iter()) {
      if range.overlaps(ring_range) {
        return Err(VirtqueueError::BufferOverlapsRing { at, ring: *ring });
      }
    }
    Ok(range)
  }

  /// Adds a validated buffer to the chain, checking the readable-before-writable order and the
  /// byte caps.
  fn account(
    &self,
    chain: &mut DescriptorChain,
    at: u16,
    descriptor: &Descriptor,
    range: GuestRange,
  ) -> Result<(), VirtqueueError> {
    if descriptor.flags & VIRTQ_DESC_F_WRITE != 0 {
      chain.writable_bytes = chain.writable_bytes.saturating_add(range.len());
      if chain.writable_bytes > self.caps.max_writable_bytes {
        return Err(VirtqueueError::WritableBytesOverCap {
          bytes: chain.writable_bytes,
          cap: self.caps.max_writable_bytes,
        });
      }
      chain.writable.push(range);
      return Ok(());
    }
    if !chain.writable.is_empty() {
      return Err(VirtqueueError::ReadableAfterWritable { at });
    }
    chain.readable_bytes = chain.readable_bytes.saturating_add(range.len());
    if chain.readable_bytes > self.caps.max_readable_bytes {
      return Err(VirtqueueError::ReadableBytesOverCap {
        bytes: chain.readable_bytes,
        cap: self.caps.max_readable_bytes,
      });
    }
    chain.readable.push(range);
    Ok(())
  }
}

/// §2.7: a power of two, nonzero, at most the ceiling.
fn validate_size(size: u16) -> Result<(), VirtqueueError> {
  if size == 0 || !size.is_power_of_two() || size > MAX_QUEUE_SIZE {
    return Err(VirtqueueError::QueueSizeInvalid { size });
  }
  Ok(())
}

/// The range of `ring` under `layout`, validated for alignment and residence in guest memory.
fn ring_range(
  layout: &QueueLayout,
  ring: Ring,
  memory: &dyn GuestMemory,
) -> Result<GuestRange, VirtqueueError> {
  let base = layout.base(ring);
  if !base.0.is_multiple_of(ring.alignment()) {
    return Err(VirtqueueError::RingMisaligned {
      ring,
      address: base.0,
      alignment: ring.alignment(),
    });
  }
  let len = ring.len(layout.size);
  let outside = VirtqueueError::RingOutsideGuestMemory {
    ring,
    start: base.0,
    len,
  };
  let range = GuestRange::new(base, len).map_err(|_| outside.clone())?;
  memory.check(range).map_err(|_| outside)?;
  Ok(range)
}

/// The three rings must not share bytes.
fn check_disjoint(rings: &[GuestRange; 3]) -> Result<(), VirtqueueError> {
  for first in 0..rings.len() {
    for second in first + 1..rings.len() {
      if rings[first].overlaps(&rings[second]) {
        return Err(VirtqueueError::RingsOverlap {
          first: RINGS[first],
          second: RINGS[second],
        });
      }
    }
  }
  Ok(())
}

//! The simulated guest memory the tests own (the `SimHost` pattern of `crates/vfs/src/host`): a set
//! of `Vec<u8>` regions at guest-physical bases, behind the same [`GuestMemory`] seam a real mapping
//! implements, plus what an oracle needs and a real mapping cannot give — an access log, so a test
//! can prove that a refused chain's buffers were never touched (AC-4.12/T-4.14 "refusal before
//! access"), and read/write counters as non-vacuity witnesses. The read-side bookkeeping sits in
//! `Cell`/`RefCell` because the seam's `read` takes `&self` (single-threaded state, never a lock).

use std::cell::{Cell, RefCell};

use crate::memory::{GuestAddr, GuestMemory, GuestMemoryError, GuestRange};

/// Shape: the access log keeps this many entries before it stops recording (and counts what it
/// dropped): 65 536, twice the largest queue's descriptor count, so any single test's history
/// fits; the log exists for oracles, not for production, so an overflow is counted, never fatal.
const ACCESS_LOG_CAP: usize = 1 << 16;

/// One region of simulated guest memory, its range proven at construction.
struct SimRegion {
  range: GuestRange,
  bytes: Vec<u8>,
}

/// One recorded access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
  /// The range touched.
  pub range: GuestRange,
  /// Whether it was a write (else a read).
  pub write: bool,
}

/// Simulated guest memory: regions the test lays out, an optional access log, and counters.
pub struct SimGuestMemory {
  regions: Vec<SimRegion>,
  log: RefCell<Option<Vec<Access>>>,
  dropped: Cell<u64>,
  reads: Cell<u64>,
  writes: Cell<u64>,
}

impl std::fmt::Debug for SimGuestMemory {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SimGuestMemory")
      .field("regions", &self.regions.len())
      .field("reads", &self.reads.get())
      .field("writes", &self.writes.get())
      .finish()
  }
}

impl SimGuestMemory {
  /// One region of `len` zeroed bytes at guest-physical address zero. A `len` beyond what the host
  /// can address saturates to the addressable maximum (a simulator's input is the test's, not a
  /// guest's, so no cap is derived here).
  pub fn new(len: u64) -> SimGuestMemory {
    // A single region at zero of a host-addressable length cannot overflow; the refusal arm is
    // unreachable for such input and yields an empty memory rather than a panic.
    SimGuestMemory::with_regions(&[(
      0,
      u64::try_from(usize::try_from(len).unwrap_or(usize::MAX)).unwrap_or(u64::MAX),
    )])
    .unwrap_or_else(|_| SimGuestMemory::empty())
  }

  /// No regions at all: every access is refused (the fallback of an impossible construction).
  fn empty() -> SimGuestMemory {
    SimGuestMemory {
      regions: Vec::new(),
      log: RefCell::new(None),
      dropped: Cell::new(0),
      reads: Cell::new(0),
      writes: Cell::new(0),
    }
  }

  /// Regions at the given `(base, len)` pairs (a guest with a hole in its physical map, as x86
  /// guests have below 4 GiB); refused when a region overflows the address space or two overlap.
  pub fn with_regions(regions: &[(u64, u64)]) -> Result<SimGuestMemory, GuestMemoryError> {
    let mut built: Vec<SimRegion> = Vec::with_capacity(regions.len());
    for &(base, len) in regions {
      let range = GuestRange::new(GuestAddr(base), len)?;
      if let Some(existing) = built.iter().find(|r| r.range.overlaps(&range)) {
        return Err(GuestMemoryError::RegionsOverlap {
          first: existing.range.start().0,
          second: base,
        });
      }
      built.push(SimRegion {
        range,
        bytes: vec![0; usize::try_from(len).unwrap_or(usize::MAX)],
      });
    }
    let mut memory = SimGuestMemory::empty();
    memory.regions = built;
    Ok(memory)
  }

  /// Starts (or stops) recording every access; starting clears the log.
  pub fn record_accesses(&mut self, on: bool) {
    *self.log.borrow_mut() = on.then(Vec::new);
    self.dropped.set(0);
  }

  /// The recorded accesses so far, oldest first (empty when not recording).
  pub fn accesses(&self) -> Vec<Access> {
    self.log.borrow().clone().unwrap_or_default()
  }

  /// Forgets the recorded accesses (recording continues).
  pub fn clear_accesses(&mut self) {
    if let Some(log) = self.log.borrow_mut().as_mut() {
      log.clear();
    }
    self.dropped.set(0);
  }

  /// Accesses the log could not hold.
  pub fn accesses_dropped(&self) -> u64 {
    self.dropped.get()
  }

  /// Reads served.
  pub fn reads(&self) -> u64 {
    self.reads.get()
  }

  /// Writes served.
  pub fn writes(&self) -> u64 {
    self.writes.get()
  }

  fn record(&self, range: GuestRange, write: bool) {
    if let Some(log) = self.log.borrow_mut().as_mut() {
      if log.len() < ACCESS_LOG_CAP {
        log.push(Access { range, write });
      } else {
        self.dropped.set(self.dropped.get().saturating_add(1));
      }
    }
  }

  /// The region holding all of `range` and the offset of the range within it.
  fn locate(&self, range: GuestRange) -> Result<(usize, usize), GuestMemoryError> {
    let outside = GuestMemoryError::OutsideGuestMemory {
      start: range.start().0,
      len: range.len(),
    };
    let (index, region) = self
      .regions
      .iter()
      .enumerate()
      .find(|(_, r)| r.range.contains(&range))
      .ok_or(outside.clone())?;
    let offset = usize::try_from(range.start().0 - region.range.start().0).map_err(|_| outside)?;
    Ok((index, offset))
  }

  /// The buffer length a range needs, refused when the caller's differs.
  fn buffer_len(range: GuestRange, have: usize) -> Result<usize, GuestMemoryError> {
    let want = usize::try_from(range.len()).unwrap_or(usize::MAX);
    if have != want {
      return Err(GuestMemoryError::BufferMismatch {
        wanted: range.len(),
        have,
      });
    }
    Ok(want)
  }
}

impl GuestMemory for SimGuestMemory {
  fn check(&self, range: GuestRange) -> Result<(), GuestMemoryError> {
    if range.is_empty() {
      return Ok(());
    }
    self.locate(range).map(|_| ())
  }

  fn read(&self, range: GuestRange, out: &mut [u8]) -> Result<(), GuestMemoryError> {
    let want = Self::buffer_len(range, out.len())?;
    if range.is_empty() {
      return Ok(());
    }
    let (index, offset) = self.locate(range)?;
    out.copy_from_slice(&self.regions[index].bytes[offset..offset + want]);
    self.reads.set(self.reads.get().saturating_add(1));
    self.record(range, false);
    Ok(())
  }

  fn write(&mut self, range: GuestRange, bytes: &[u8]) -> Result<(), GuestMemoryError> {
    let want = Self::buffer_len(range, bytes.len())?;
    if range.is_empty() {
      return Ok(());
    }
    let (index, offset) = self.locate(range)?;
    self.regions[index].bytes[offset..offset + want].copy_from_slice(bytes);
    self.writes.set(self.writes.get().saturating_add(1));
    self.record(range, true);
    Ok(())
  }
}

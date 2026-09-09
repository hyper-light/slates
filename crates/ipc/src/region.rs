//! The client region (§4.7 `ClientRegion`): one shared object per client holding a header,
//! the wake word, the parked flags, the command ring, the completion ring and the bulk area.
//! The daemon creates it, locks it, and hands it to one client; the client writes only the
//! command ring's slots, its parked flag and the daemon-doorbell; the daemon writes only the
//! completion ring's slots, the wake word and its own parked flag.

use std::sync::atomic::{AtomicU32, Ordering};

use slates_mem::{Handoff, SharedObject};

use crate::error::IpcError;
use crate::slot::Ring;

/// Format: the region's magic, `SLCR` in little-endian ASCII.
pub const MAGIC: u32 = 0x5243_4C53;
/// Format: the layout version.
pub const LAYOUT_VERSION: u32 = 1;
/// Format: the header's size (two cache lines): magic (4), version (4), client id (4), shard
/// (2), padding (2), slots per ring (4), spin ns (4), command ring offset (8), completion
/// ring offset (8), bulk offset (8), bulk length (8), padding.
pub const HEADER_BYTES: usize = 128;
/// Format: the magic's offset.
const AT_MAGIC: usize = 0;
/// Format: the version's offset.
const AT_VERSION: usize = 4;
/// Format: the client id's offset.
const AT_CLIENT: usize = 8;
/// Format: the shard's offset.
const AT_SHARD: usize = 12;
/// Format: the slots-per-ring offset.
const AT_SLOTS: usize = 16;
/// Format: the spin window's offset.
const AT_SPIN: usize = 20;
/// Format: the command ring offset's offset.
const AT_CMD: usize = 24;
/// Format: the completion ring offset's offset.
const AT_CPL: usize = 32;
/// Format: the bulk offset's offset.
const AT_BULK: usize = 40;
/// Format: the bulk length's offset.
const AT_BULK_LEN: usize = 48;
/// Format: the words block after the header, one cache line each: the wake word (the daemon
/// bumps it per reply), the client's parked flag, the daemon's parked flag (a shard parked
/// in its driver), the client's doorbell (the client bumps it per request when the daemon is
/// parked; the doorbell thread or the eventfd carries it on).
pub const WORDS_OFFSET: usize = HEADER_BYTES;
/// Format: the wake word's offset.
const AT_WAKE: usize = WORDS_OFFSET;
/// Format: the client parked flag's offset.
const AT_CLIENT_PARKED: usize = WORDS_OFFSET + 64;
/// Format: the daemon parked flag's offset.
const AT_DAEMON_PARKED: usize = WORDS_OFFSET + 128;
/// Format: the doorbell's offset.
const AT_DOORBELL: usize = WORDS_OFFSET + 192;
/// Format: where the rings start.
const RINGS_OFFSET: usize = WORDS_OFFSET + 256;

/// The region's geometry, the daemon's derivation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionGeometry {
  /// Derived: slots per ring, a power of two from Little's law on the measured per-client
  /// request rate and the p99 service time (§4.7).
  pub slots: u32,
  /// Measured: the spin window, the wake cost's p99.
  pub spin_ns: u32,
  /// Derived: the bulk area's bytes (the largest inline read the SDK offers, page-rounded).
  pub bulk_bytes: u64,
  /// Derived: the page the region is aligned to.
  pub page: u64,
}

impl RegionGeometry {
  fn cmd_offset(&self) -> usize {
    RINGS_OFFSET
  }

  fn cpl_offset(&self) -> usize {
    self.cmd_offset() + Ring::bytes(usize::try_from(self.slots).unwrap_or(0))
  }

  fn bulk_offset(&self) -> usize {
    let after_rings = self.cpl_offset() + Ring::bytes(usize::try_from(self.slots).unwrap_or(0));
    let page = usize::try_from(self.page.max(1)).unwrap_or(1);
    after_rings.next_multiple_of(page)
  }

  /// The region's total bytes.
  pub fn total_bytes(&self) -> usize {
    let page = usize::try_from(self.page.max(1)).unwrap_or(1);
    (self.bulk_offset() + usize::try_from(self.bulk_bytes).unwrap_or(0)).next_multiple_of(page)
  }
}

/// A client region: the object and its rings.
pub struct ClientRegion {
  object: SharedObject,
  client_id: u32,
  shard: u16,
  spin_ns: u32,
  cmd: Ring,
  cpl: Ring,
  bulk: (usize, usize),
  /// The Windows cross-process wake: a named auto-reset Event derived from the region's object name
  /// (`{name}-wake`), so both ends open the same one with no handle passing. `WaitOnAddress` on the
  /// wake word wakes only threads of this process (D-10), so the endpoint waits on and signals this
  /// Event instead. Absent on Linux/macOS, where the futex / `os_sync` wake word crosses processes.
  #[cfg(windows)]
  wake_event: crate::wake::Event,
}

impl std::fmt::Debug for ClientRegion {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ClientRegion")
      .field("client_id", &self.client_id)
      .field("shard", &self.shard)
      .field("slots", &self.cmd.slots())
      .finish()
  }
}

/// The named auto-reset Event a region waits on and signals on Windows, derived from the region's
/// object name so both ends open the one Event with no handle passing (`{name}-wake`).
#[cfg(windows)]
fn windows_wake_event(object: &SharedObject) -> Result<crate::wake::Event, IpcError> {
  let base = match object.handoff()? {
    Handoff::Name(name) => name,
    Handoff::Descriptor(_) => {
      return Err(IpcError::Layout {
        reason: "a windows region hands off a name for its wake Event",
      });
    }
  };
  crate::wake::Event::open(&format!("{base}-wake"))
}

impl ClientRegion {
  /// The daemon creates a region for `client_id` on `shard` with `geometry`.
  pub fn create(
    name: &str,
    client_id: u32,
    shard: u16,
    geometry: RegionGeometry,
  ) -> Result<ClientRegion, IpcError> {
    if !geometry.slots.is_power_of_two() {
      return Err(IpcError::Layout {
        reason: "slots per ring must be a power of two",
      });
    }
    let total = geometry.total_bytes();
    let mut object = SharedObject::create(name, total)?;
    let slots = usize::try_from(geometry.slots).unwrap_or(0);
    let cmd = Ring::at(geometry.cmd_offset(), slots);
    let cpl = Ring::at(geometry.cpl_offset(), slots);
    let bulk = (
      geometry.bulk_offset(),
      usize::try_from(geometry.bulk_bytes).unwrap_or(0),
    );
    {
      let bytes = object.bytes_mut();
      put(bytes, AT_MAGIC, &MAGIC.to_le_bytes());
      put(bytes, AT_VERSION, &LAYOUT_VERSION.to_le_bytes());
      put(bytes, AT_CLIENT, &client_id.to_le_bytes());
      put(bytes, AT_SHARD, &shard.to_le_bytes());
      put(bytes, AT_SLOTS, &geometry.slots.to_le_bytes());
      put(bytes, AT_SPIN, &geometry.spin_ns.to_le_bytes());
      put(
        bytes,
        AT_CMD,
        &u64::try_from(cmd_offset_of(&geometry))
          .unwrap_or(0)
          .to_le_bytes(),
      );
      put(
        bytes,
        AT_CPL,
        &u64::try_from(geometry.cpl_offset())
          .unwrap_or(0)
          .to_le_bytes(),
      );
      put(
        bytes,
        AT_BULK,
        &u64::try_from(bulk.0).unwrap_or(0).to_le_bytes(),
      );
      put(bytes, AT_BULK_LEN, &geometry.bulk_bytes.to_le_bytes());
    }
    cmd.init(&object)?;
    cpl.init(&object)?;
    for at in [AT_WAKE, AT_CLIENT_PARKED, AT_DAEMON_PARKED, AT_DOORBELL] {
      object.atomic_u32(at)?.store(0, Ordering::Release);
    }
    #[cfg(windows)]
    let wake_event = windows_wake_event(&object)?;
    Ok(ClientRegion {
      object,
      client_id,
      shard,
      spin_ns: geometry.spin_ns,
      cmd,
      cpl,
      bulk,
      #[cfg(windows)]
      wake_event,
    })
  }

  /// The client opens the region the daemon handed over.
  pub fn open(handoff: &Handoff, len: usize) -> Result<ClientRegion, IpcError> {
    let object = SharedObject::open(handoff, len)?;
    let bytes = object.bytes();
    if bytes.len() < RINGS_OFFSET || read_u32(bytes, AT_MAGIC) != MAGIC {
      return Err(IpcError::Layout {
        reason: "wrong magic",
      });
    }
    if read_u32(bytes, AT_VERSION) != LAYOUT_VERSION {
      return Err(IpcError::Layout {
        reason: "wrong layout version",
      });
    }
    let slots = usize::try_from(read_u32(bytes, AT_SLOTS)).unwrap_or(0);
    if !slots.is_power_of_two() {
      return Err(IpcError::Layout {
        reason: "slots per ring not a power of two",
      });
    }
    let cmd_at = usize::try_from(read_u64(bytes, AT_CMD)).unwrap_or(usize::MAX);
    let cpl_at = usize::try_from(read_u64(bytes, AT_CPL)).unwrap_or(usize::MAX);
    let bulk_at = usize::try_from(read_u64(bytes, AT_BULK)).unwrap_or(usize::MAX);
    let bulk_len = usize::try_from(read_u64(bytes, AT_BULK_LEN)).unwrap_or(usize::MAX);
    let ring_bytes = Ring::bytes(slots);
    let inside = |at: usize, len: usize| at.checked_add(len).is_some_and(|end| end <= bytes.len());
    if !inside(cmd_at, ring_bytes) || !inside(cpl_at, ring_bytes) || !inside(bulk_at, bulk_len) {
      return Err(IpcError::Layout {
        reason: "a ring or the bulk area lies outside the region",
      });
    }
    let client_id = read_u32(bytes, AT_CLIENT);
    let shard = u16::from_le_bytes([bytes[AT_SHARD], bytes[AT_SHARD + 1]]);
    let spin_ns = read_u32(bytes, AT_SPIN);
    #[cfg(windows)]
    let wake_event = windows_wake_event(&object)?;
    Ok(ClientRegion {
      object,
      client_id,
      shard,
      spin_ns,
      cmd: Ring::at(cmd_at, slots),
      cpl: Ring::at(cpl_at, slots),
      bulk: (bulk_at, bulk_len),
      #[cfg(windows)]
      wake_event,
    })
  }

  /// What to hand the client, with the length.
  pub fn handoff(&self) -> Result<(Handoff, usize), IpcError> {
    Ok((self.object.handoff()?, self.object.len()))
  }

  /// Locks the region into RAM (D-12).
  pub fn lock(&mut self) -> Result<(), IpcError> {
    Ok(self.object.lock()?)
  }

  /// The client id.
  pub fn client_id(&self) -> u32 {
    self.client_id
  }

  /// The shard the client is pinned to.
  pub fn shard(&self) -> u16 {
    self.shard
  }

  /// The spin window the daemon published.
  pub fn spin_ns(&self) -> u32 {
    self.spin_ns
  }

  /// The command ring (client → daemon).
  pub fn cmd(&self) -> Ring {
    self.cmd
  }

  /// The completion ring (daemon → client).
  pub fn cpl(&self) -> Ring {
    self.cpl
  }

  /// The object, for the rings' pushes and pops.
  pub fn object(&self) -> &SharedObject {
    &self.object
  }

  /// The object, mutably.
  pub fn object_mut(&mut self) -> &mut SharedObject {
    &mut self.object
  }

  /// Waits on the region's cross-process wake for up to `timeout_ns` (Windows: the named Event). The
  /// caller has already published the parked flag and re-checked the reply, as on Linux/macOS, so a
  /// signal made before this wait is not lost (the auto-reset Event stays signaled until consumed).
  /// `true` when signaled, `false` at the timeout.
  #[cfg(windows)]
  pub fn wake_wait(&self, timeout_ns: Option<u64>) -> Result<bool, IpcError> {
    self.wake_event.wait(timeout_ns)
  }

  /// Signals the region's cross-process wake (Windows: `SetEvent` on the named Event), waking a parked
  /// client.
  #[cfg(windows)]
  pub fn wake_signal(&self) -> Result<(), IpcError> {
    self.wake_event.signal()
  }

  /// The wake word.
  pub fn wake_word(&self) -> Result<&AtomicU32, IpcError> {
    Ok(self.object.atomic_u32(AT_WAKE)?)
  }

  /// The client's parked flag.
  pub fn client_parked(&self) -> Result<&AtomicU32, IpcError> {
    Ok(self.object.atomic_u32(AT_CLIENT_PARKED)?)
  }

  /// The daemon's parked flag.
  pub fn daemon_parked(&self) -> Result<&AtomicU32, IpcError> {
    Ok(self.object.atomic_u32(AT_DAEMON_PARKED)?)
  }

  /// The doorbell the client rings when the daemon is parked.
  pub fn doorbell(&self) -> Result<&AtomicU32, IpcError> {
    Ok(self.object.atomic_u32(AT_DOORBELL)?)
  }

  /// The bulk area.
  pub fn bulk(&self) -> &[u8] {
    &self.object.bytes()[self.bulk.0..self.bulk.0 + self.bulk.1]
  }

  /// The bulk area, mutably.
  pub fn bulk_mut(&mut self) -> &mut [u8] {
    let (at, len) = self.bulk;
    &mut self.object.bytes_mut()[at..at + len]
  }
}

fn cmd_offset_of(geometry: &RegionGeometry) -> usize {
  geometry.cmd_offset()
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
  bytes[at..at + value.len()].copy_from_slice(value);
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
  let mut w = [0u8; size_of::<u32>()];
  let n = w.len();
  w.copy_from_slice(&bytes[at..at + n]);
  u32::from_le_bytes(w)
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
  let mut w = [0u8; size_of::<u64>()];
  let n = w.len();
  w.copy_from_slice(&bytes[at..at + n]);
  u64::from_le_bytes(w)
}

//! Opens that survive a daemon (A-198, §4.6 Windows). A kernel context is a generation and a slot, never a pointer.
//! The owning shard alone reads or changes the ledger; dispatcher threads send the context to it. The anchor holds
//! the backing memory without mapping it, and a successor opens it only after its predecessor has exited. This is
//! ownership by move, with no per-open atomic or lock. Each fixed record has a CRC32C; a torn record is quarantined,
//! never recycled beneath a kernel context. The live mount oracle is `crates/cli/tests/winfsp.rs` (Step 9.3).

use slates_bridge_core::ObjectId;
use slates_mem::MemError;
use slates_wire::crc32c::crc32c;

mod names;

/// Format: identifies version 3 of the Windows ledger: open records, two checked kernel-name versions per slot,
/// then the retained receive journal. The unnamed sparse section commits touched ranges only.
const MAGIC: &[u8; 4] = b"WFO3";
/// Format: the immutable header carries the first epoch, the slot count, and a CRC32C after the magic.
const HEADER: usize = MAGIC.len() + size_of::<u64>() + size_of::<u32>() + size_of::<u32>();
/// Format: generation, live/purge flags, daemon epoch, inode, inode generation, bridge handle and request hint,
/// then CRC32C and one confirmation byte. Confirming an issued context changes no checksummed identity bytes.
const FRAME: usize =
  size_of::<u32>() + size_of::<u8>() + size_of::<u64>() * 5 + size_of::<u32>() + size_of::<u8>();
/// Format: the last byte is zero before the driver accepted the open, one afterwards. A one-byte store cannot tear.
const CONFIRMED_AT: usize = FRAME - size_of::<u8>();
/// Format: live is bit zero, purge is bit one; no other flag is defined.
const LIVE: u8 = 1;
/// Format: see [`LIVE`]. A purge's writes must never be applied, including after takeover.
const PURGE: u8 = 2;
/// Format: the packed bridge handle with slot index `u32::MAX`, beyond the bounded `MAX_OPEN_HANDLES` store.
/// Unlike zero (its first live slot), this word cannot address a successor's handle. Releasing it settles a restored
/// reference by file identity through `VolumeBridge::release` (A-148, A-152).
pub const RESTORED_HANDLE: u64 = u64::MAX;

/// A typed refusal at the open-ledger boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
  /// The requested slot count, allocation, or byte range cannot be represented.
  Bound,
  /// A header or record has an invalid checksum or shape.
  Corrupt,
  /// The kernel context is closed, quarantined, or belongs to an older generation.
  Stale,
  /// A generation or daemon epoch cannot advance without reuse.
  Exhausted,
  /// The memory backing refused a checked copy.
  Memory(MemError),
}

impl std::fmt::Display for Refusal {
  fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(out, "open ledger: {self:?}")
  }
}

impl std::error::Error for Refusal {}

/// The copy-only backing. The active daemon is its only reader and writer; a held handle alone grants no borrow.
pub trait Memory {
  /// Its fixed byte capacity.
  fn len(&self) -> usize;
  /// Whether it has no storage.
  fn is_empty(&self) -> bool {
    self.len() == 0
  }
  /// Copies a checked span out.
  fn read(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal>;
  /// Replaces a checked span.
  fn write(&mut self, at: usize, from: &[u8]) -> Result<(), Refusal>;
}

impl Memory for slates_mem::SharedObject {
  fn len(&self) -> usize {
    self.len()
  }
  fn read(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal> {
    self.read(at, into).map_err(Refusal::Memory)
  }
  fn write(&mut self, at: usize, from: &[u8]) -> Result<(), Refusal> {
    self.write(at, from).map_err(Refusal::Memory)
  }
}

impl Memory for slates_mem::SparseObject {
  fn len(&self) -> usize {
    self.len()
  }
  fn read(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal> {
    self.read(at, into).map_err(Refusal::Memory)
  }
  fn write(&mut self, at: usize, from: &[u8]) -> Result<(), Refusal> {
    self.write(at, from).map_err(Refusal::Memory)
  }
}

/// A file a kernel context still holds. An older daemon's bridge handle is returned as [`RESTORED_HANDLE`], so it cannot alias a
/// handle the successor has issued. `Bridge::release` then returns the reference recovery restored for this inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Open {
  /// Stable file identity, including an unlinked file.
  pub object: ObjectId,
  /// This daemon's bridge handle, or [`RESTORED_HANDLE`] for a predecessor's open.
  pub fh: u64,
  /// This open belongs to a coherent purge, whose write is only an echo.
  pub purge: bool,
}

#[derive(Clone, Copy)]
struct Slot {
  generation: u32,
  epoch: u64,
  open: Option<Open>,
  quarantined: bool,
  hint: u64,
  confirmed: bool,
}

/// The bytes an open ledger and its retained receive journal need. The bound is the bridge's admitted open cap.
pub fn bytes(bound: usize) -> Result<usize, Refusal> {
  capture_offset(bound)?
    .checked_add(crate::inflight::capture::BYTES)
    .ok_or(Refusal::Bound)
}

/// The retained receive region's first byte, after the fixed open frames and padding to the wire's eight-byte
/// alignment. Only the receive worker touches that region; the shard's frame copies never overlap it.
pub fn capture_offset(bound: usize) -> Result<usize, Refusal> {
  if bound == 0 || u32::try_from(bound).is_err() {
    return Err(Refusal::Bound);
  }
  bound
    .checked_mul(FRAME)
    .and_then(|body| body.checked_add(HEADER))
    .and_then(|body| {
      names::bytes(bound)
        .ok()
        .and_then(|names| body.checked_add(names))
    })
    .and_then(|body| body.checked_next_multiple_of(align_of::<u64>()))
    .ok_or(Refusal::Bound)
}

/// The bounded ledger, with process-local free slots and a checksum-protected frame for every issued context.
/// Create and recovery run off the shard on the mount's already owned thread; serving moves this value to the shard.
pub struct Ledger<M> {
  memory: M,
  slots: Vec<Slot>,
  free: Vec<usize>,
  epoch: u64,
  quarantined: usize,
  pending: Option<(u64, u64)>,
}

impl<M: Memory> Ledger<M> {
  /// Uses freshly zeroed memory; only the header is written. New paging-file sections are zero-filled by Windows.
  pub fn create(mut memory: M, bound: usize) -> Result<Self, Refusal> {
    if memory.len() != bytes(bound)? {
      return Err(Refusal::Bound);
    }
    let epoch = 1;
    memory.write(0, &header(bound, epoch)?)?;
    let mut slots = Vec::new();
    let mut free = Vec::new();
    slots.try_reserve_exact(bound).map_err(|_| Refusal::Bound)?;
    free.try_reserve_exact(bound).map_err(|_| Refusal::Bound)?;
    for index in 0..bound {
      slots.push(Slot {
        generation: 0,
        epoch: 0,
        open: None,
        quarantined: false,
        hint: 0,
        confirmed: false,
      });
      free.push(index);
    }
    Ok(Self {
      memory,
      slots,
      free,
      epoch,
      quarantined: 0,
      pending: None,
    })
  }

  /// Recovers the held memory after the old daemon exited. An invalid slot stays occupied and reports a stale
  /// context; it is never silently reused. An invalid header refuses the whole ledger.
  pub fn recover(memory: M, bound: usize) -> Result<Self, Refusal> {
    if memory.len() != bytes(bound)? {
      return Err(Refusal::Bound);
    }
    let mut head = [0; HEADER];
    memory.read(0, &mut head)?;
    checked(&head)?;
    if head.get(..MAGIC.len()) != Some(MAGIC.as_slice())
      || read_u32(&head, MAGIC.len().saturating_add(size_of::<u64>()))?
        != u32::try_from(bound).map_err(|_| Refusal::Bound)?
    {
      return Err(Refusal::Corrupt);
    }
    let first_epoch = read_u64(&head, MAGIC.len())?;
    if first_epoch != 1 {
      return Err(Refusal::Corrupt);
    }
    let mut slots = Vec::new();
    let mut free = Vec::new();
    slots.try_reserve_exact(bound).map_err(|_| Refusal::Bound)?;
    free.try_reserve_exact(bound).map_err(|_| Refusal::Bound)?;
    let mut quarantined = 0usize;
    let mut pending = None;
    for index in 0..bound {
      let mut frame = [0; FRAME];
      memory.read(offset(index)?, &mut frame)?;
      let slot = match decode(&frame) {
        Ok(slot) => slot,
        Err(_) => {
          quarantined = quarantined.saturating_add(1);
          Slot {
            generation: 0,
            epoch: 0,
            open: None,
            quarantined: true,
            hint: 0,
            confirmed: false,
          }
        }
      };
      if slot.open.is_none() && !slot.quarantined && slot.generation < u32::MAX {
        free.push(index);
      }
      if slot.open.is_some() && !slot.confirmed {
        if pending.is_some() {
          return Err(Refusal::Corrupt);
        }
        pending = Some((token(index, slot.generation)?, slot.hint));
      }
      slots.push(slot);
    }
    // No recovery write can tear the shared header. Every live frame records its issuing epoch, so the successor
    // takes the checked successor of the greatest epoch still represented. An epoch with no remaining live open
    // needs no unique successor: there is no bridge handle from it left to alias.
    let epoch = slots
      .iter()
      .map(|slot| slot.epoch)
      .max()
      .unwrap_or(first_epoch)
      .max(first_epoch)
      .checked_add(1)
      .ok_or(Refusal::Exhausted)?;
    Ok(Self {
      memory,
      slots,
      free,
      epoch,
      quarantined,
      pending,
    })
  }

  /// Whether another open can be recorded before asking the bridge to allocate it.
  pub fn has_room(&self) -> bool {
    !self.free.is_empty() && self.pending.is_none()
  }

  /// Records an open before the context is handed to Windows. A full or exhausted ledger refuses admission.
  pub fn opened(&mut self, open: Open, name: &str) -> Result<u64, Refusal> {
    self.insert(open, name, 0, true)
  }

  fn insert(&mut self, open: Open, name: &str, hint: u64, confirmed: bool) -> Result<u64, Refusal> {
    if self.pending.is_some() {
      return Err(Refusal::Bound);
    }
    let index = self.free.last().copied().ok_or(Refusal::Bound)?;
    let old = self.slots.get(index).ok_or(Refusal::Bound)?;
    let generation = old.generation.checked_add(1).ok_or(Refusal::Exhausted)?;
    let slot = Slot {
      generation,
      epoch: self.epoch,
      open: Some(open),
      quarantined: false,
      hint,
      confirmed,
    };
    names::initial(&mut self.memory, self.slots.len(), index, generation, name)?;
    self.memory.write(offset(index)?, &encode(slot)?)?;
    *self.slots.get_mut(index).ok_or(Refusal::Bound)? = slot;
    self.free.pop();
    let context = token(index, generation)?;
    if !confirmed {
      self.pending = Some((context, hint));
    }
    Ok(context)
  }

  /// Records the one open whose kernel response has not yet been acknowledged, tied to that request's hint.
  pub fn opened_for(&mut self, open: Open, name: &str, hint: u64) -> Result<u64, Refusal> {
    if hint == 0 {
      return Err(Refusal::Corrupt);
    }
    self.insert(open, name, hint, false)
  }

  /// The name of this open's kernel file node, independent of the file's current namespace home.
  pub fn name(&self, context: u64) -> Result<String, Refusal> {
    self.get(context)?;
    let (index, generation) = unpack(context)?;
    names::read(&self.memory, self.slots.len(), index, generation).map(|(_, name)| name)
  }

  /// Records one kernel-name change after the receiver saved a successful rename response. Two checksummed
  /// versions keep its previous complete name through a torn copy; repeating the same update is harmless.
  pub fn rename_name(&mut self, context: u64, from: &str, to: &str) -> Result<(), Refusal> {
    self.get(context)?;
    let (index, generation) = unpack(context)?;
    names::rename(
      &mut self.memory,
      self.slots.len(),
      index,
      generation,
      from,
      to,
    )
  }

  /// Marks an open handed only after its response was accepted by the driver, before another callback is admitted.
  pub fn confirmed(&mut self, context: u64) -> Result<(), Refusal> {
    self.get(context)?;
    let (index, _) = unpack(context)?;
    if self.slots.get(index).ok_or(Refusal::Stale)?.confirmed {
      return Ok(());
    }
    if self.pending.map(|(pending, _)| pending) != Some(context) {
      return Err(Refusal::Corrupt);
    }
    let at = offset(index)?
      .checked_add(CONFIRMED_AT)
      .ok_or(Refusal::Bound)?;
    self.memory.write(at, &[1])?;
    self.slots.get_mut(index).ok_or(Refusal::Stale)?.confirmed = true;
    self.pending = None;
    Ok(())
  }

  /// Before restoring reference counts, retain an unhanded open only when the journal has its successful response.
  /// Otherwise retire its ledger context; its unpublished/local bridge handle is not a restored sibling's reference.
  pub fn settle_pending(&mut self, saved_open: Option<(u64, u64)>) -> Result<(), Refusal> {
    if let Some((context, hint)) = self.pending {
      if saved_open == Some((hint, context)) {
        self.confirmed(context)?;
      } else {
        self.closed(context)?;
      }
    }
    Ok(())
  }

  /// Resolves the context on the shard. The returned bridge handle is usable only in the epoch that issued it.
  pub fn get(&self, context: u64) -> Result<Open, Refusal> {
    let (index, generation) = unpack(context)?;
    let slot = self.slots.get(index).ok_or(Refusal::Stale)?;
    if slot.generation != generation || slot.quarantined {
      return Err(Refusal::Stale);
    }
    let mut open = slot.open.ok_or(Refusal::Stale)?;
    if slot.epoch != self.epoch {
      open.fh = RESTORED_HANDLE;
    }
    Ok(open)
  }

  /// Retires a context after its bridge release succeeded. Repeated closes and old generations are typed misses.
  pub fn closed(&mut self, context: u64) -> Result<(), Refusal> {
    self.get(context)?;
    let (index, generation) = unpack(context)?;
    let slot = Slot {
      generation,
      epoch: 0,
      open: None,
      quarantined: false,
      hint: 0,
      confirmed: false,
    };
    self.memory.write(offset(index)?, &encode(slot)?)?;
    *self.slots.get_mut(index).ok_or(Refusal::Stale)? = slot;
    if self.pending.map(|(pending, _)| pending) == Some(context) {
      self.pending = None;
    }
    if generation < u32::MAX {
      self.free.push(index);
    }
    Ok(())
  }

  /// Live contexts, including those issued before the takeover, for rebuilding the mount's coherence names.
  pub fn live(&self) -> impl Iterator<Item = (u64, Open)> + '_ {
    self.slots.iter().enumerate().filter_map(|(index, slot)| {
      let context = token(index, slot.generation).ok()?;
      self.get(context).ok().map(|open| (context, open))
    })
  }

  /// Torn records kept out of reuse; the owner counts them as recovery refusals.
  pub fn quarantined(&self) -> usize {
    self.quarantined
  }

  /// Returns the owned storage when this process stops using the ledger.
  pub fn into_memory(self) -> M {
    self.memory
  }

  /// Its storage, lent only while the ledger owns it; used to ask the anchor to hold the section.
  pub fn memory(&self) -> &M {
    &self.memory
  }
}

fn token(index: usize, generation: u32) -> Result<u64, Refusal> {
  let slot = u32::try_from(index)
    .ok()
    .and_then(|index| index.checked_add(1))
    .ok_or(Refusal::Bound)?;
  if generation == 0 {
    return Err(Refusal::Stale);
  }
  Ok((u64::from(generation) << u32::BITS) | u64::from(slot))
}

fn unpack(context: u64) -> Result<(usize, u32), Refusal> {
  let slot = u32::try_from(context & u64::from(u32::MAX)).map_err(|_| Refusal::Stale)?;
  let index = slot
    .checked_sub(1)
    .and_then(|slot| usize::try_from(slot).ok())
    .ok_or(Refusal::Stale)?;
  let generation = u32::try_from(context >> u32::BITS).map_err(|_| Refusal::Stale)?;
  if generation == 0 {
    return Err(Refusal::Stale);
  }
  Ok((index, generation))
}

fn offset(index: usize) -> Result<usize, Refusal> {
  index
    .checked_mul(FRAME)
    .and_then(|at| at.checked_add(HEADER))
    .ok_or(Refusal::Bound)
}

fn header(bound: usize, epoch: u64) -> Result<[u8; HEADER], Refusal> {
  let mut encoded = Vec::with_capacity(HEADER);
  encoded.extend_from_slice(MAGIC);
  encoded.extend_from_slice(&epoch.to_le_bytes());
  encoded.extend_from_slice(
    &u32::try_from(bound)
      .map_err(|_| Refusal::Bound)?
      .to_le_bytes(),
  );
  encoded.extend_from_slice(&crc32c(&encoded).to_le_bytes());
  encoded.try_into().map_err(|_| Refusal::Corrupt)
}

fn encode(slot: Slot) -> Result<[u8; FRAME], Refusal> {
  let mut encoded = Vec::with_capacity(FRAME);
  encoded.extend_from_slice(&slot.generation.to_le_bytes());
  let open = slot.open.unwrap_or(Open {
    object: ObjectId::new(0, 0),
    fh: 0,
    purge: false,
  });
  encoded.push(if slot.open.is_some() {
    LIVE | if open.purge { PURGE } else { 0 }
  } else {
    0
  });
  for value in [
    slot.epoch,
    open.object.inode,
    open.object.generation,
    open.fh,
    slot.hint,
  ] {
    encoded.extend_from_slice(&value.to_le_bytes());
  }
  encoded.extend_from_slice(&crc32c(&encoded).to_le_bytes());
  encoded.push(u8::from(slot.confirmed));
  encoded.try_into().map_err(|_| Refusal::Corrupt)
}

fn decode(frame: &[u8; FRAME]) -> Result<Slot, Refusal> {
  if frame.iter().all(|byte| *byte == 0) {
    return Ok(Slot {
      generation: 0,
      epoch: 0,
      open: None,
      quarantined: false,
      hint: 0,
      confirmed: false,
    });
  }
  checked(frame.get(..CONFIRMED_AT).ok_or(Refusal::Corrupt)?)?;
  let confirmed = match frame.get(CONFIRMED_AT) {
    Some(0) => false,
    Some(1) => true,
    _ => return Err(Refusal::Corrupt),
  };
  let generation = read_u32(frame, 0)?;
  let flags_at = size_of::<u32>();
  let flags = *frame.get(flags_at).ok_or(Refusal::Corrupt)?;
  if generation == 0 || flags & !(LIVE | PURGE) != 0 || flags == PURGE {
    return Err(Refusal::Corrupt);
  }
  let mut at = flags_at.saturating_add(1);
  let epoch = next_u64(frame, &mut at)?;
  let inode = next_u64(frame, &mut at)?;
  let object_generation = next_u64(frame, &mut at)?;
  let fh = next_u64(frame, &mut at)?;
  let hint = next_u64(frame, &mut at)?;
  if (flags & LIVE != 0 && epoch == 0)
    || (flags == 0
      && [
        epoch,
        inode,
        object_generation,
        fh,
        hint,
        u64::from(confirmed),
      ]
      .iter()
      .any(|value| *value != 0))
  {
    return Err(Refusal::Corrupt);
  }
  Ok(Slot {
    generation,
    epoch,
    open: (flags & LIVE != 0).then_some(Open {
      object: ObjectId::new(inode, object_generation),
      fh,
      purge: flags & PURGE != 0,
    }),
    quarantined: false,
    hint,
    confirmed,
  })
}

fn checked(frame: &[u8]) -> Result<(), Refusal> {
  let at = frame
    .len()
    .checked_sub(size_of::<u32>())
    .ok_or(Refusal::Corrupt)?;
  if crc32c(frame.get(..at).ok_or(Refusal::Corrupt)?) != read_u32(frame, at)? {
    return Err(Refusal::Corrupt);
  }
  Ok(())
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, Refusal> {
  let end = at.checked_add(size_of::<u32>()).ok_or(Refusal::Corrupt)?;
  Ok(u32::from_le_bytes(
    bytes
      .get(at..end)
      .and_then(|bytes| bytes.try_into().ok())
      .ok_or(Refusal::Corrupt)?,
  ))
}

fn read_u64(bytes: &[u8], at: usize) -> Result<u64, Refusal> {
  let end = at.checked_add(size_of::<u64>()).ok_or(Refusal::Corrupt)?;
  Ok(u64::from_le_bytes(
    bytes
      .get(at..end)
      .and_then(|bytes| bytes.try_into().ok())
      .ok_or(Refusal::Corrupt)?,
  ))
}

fn next_u64(bytes: &[u8], at: &mut usize) -> Result<u64, Refusal> {
  let value = read_u64(bytes, *at)?;
  *at = at.checked_add(size_of::<u64>()).ok_or(Refusal::Corrupt)?;
  Ok(value)
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
  use super::*;

  struct Ram(Vec<u8>);
  impl Memory for Ram {
    fn len(&self) -> usize {
      self.0.len()
    }
    fn read(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal> {
      into.copy_from_slice(self.0.get(at..at + into.len()).ok_or(Refusal::Bound)?);
      Ok(())
    }
    fn write(&mut self, at: usize, from: &[u8]) -> Result<(), Refusal> {
      self
        .0
        .get_mut(at..at + from.len())
        .ok_or(Refusal::Bound)?
        .copy_from_slice(from);
      Ok(())
    }
  }

  fn file(inode: u64) -> Open {
    Open {
      object: ObjectId::new(inode, 7),
      fh: 42,
      purge: false,
    }
  }

  /// A-198. Do: die after recording an open, before recording its kernel response. Expect: recovery retires only the
  /// unhanded context, while a sibling open of the same file remains usable and releases independently.
  #[test]
  fn an_open_never_handed_to_the_kernel_cannot_pin_its_files_sibling() {
    let memory = Ram(vec![0; bytes(3).unwrap()]);
    let mut ledger = Ledger::create(memory, 3).unwrap();
    let sibling = ledger.opened(file(10), "\\file.bin").unwrap();
    let unhanded = ledger.opened_for(file(10), "\\file.bin", 41).unwrap();
    let mut recovered = Ledger::recover(ledger.into_memory(), 3).unwrap();
    recovered.settle_pending(None).unwrap();
    assert_eq!(recovered.get(unhanded), Err(Refusal::Stale));
    assert_eq!(recovered.get(sibling).unwrap().object, file(10).object);
    recovered.closed(sibling).unwrap();
    assert_eq!(recovered.live().count(), 0);
  }

  /// A-198. Do: die with a complete successful open response, then die again after its replay. Expect: the context
  /// stays live across both recoveries; confirming it changes no checksummed identity bytes.
  #[test]
  fn a_saved_open_is_retained_and_a_confirmed_open_needs_no_old_hint() {
    let mut ledger = Ledger::create(Ram(vec![0; bytes(2).unwrap()]), 2).unwrap();
    let context = ledger.opened_for(file(10), "\\file.bin", 41).unwrap();
    let mut recovered = Ledger::recover(ledger.into_memory(), 2).unwrap();
    recovered.settle_pending(Some((41, context))).unwrap();
    assert_eq!(recovered.get(context).unwrap().object, file(10).object);
    let mut again = Ledger::recover(recovered.into_memory(), 2).unwrap();
    again.settle_pending(None).unwrap();
    assert_eq!(again.get(context).unwrap().object, file(10).object);
    again.closed(context).unwrap();
    assert_eq!(again.live().count(), 0);
  }

  struct HeldReadOnly(Ram);
  impl Memory for HeldReadOnly {
    fn len(&self) -> usize {
      self.0.len()
    }
    fn read(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal> {
      self.0.read(at, into)
    }
    fn write(&mut self, _at: usize, _from: &[u8]) -> Result<(), Refusal> {
      Err(Refusal::Bound)
    }
  }

  /// A-198. Do: recover a predecessor's live opens with every storage write refused. Expect: reads still resolve
  /// their file identities. Killing a successor during recovery must not tear the header shared by all opens.
  #[test]
  fn recovering_opens_never_rewrites_the_shared_header() {
    let mut first = Ledger::create(Ram(vec![0; bytes(2).unwrap()]), 2).unwrap();
    let old = first.opened(file(11), "\\file.bin").unwrap();
    let mut second = Ledger::recover(first.into_memory(), 2).unwrap();
    let newer = second.opened(file(12), "\\file.bin").unwrap();
    let third = Ledger::recover(HeldReadOnly(second.into_memory()), 2).unwrap();
    assert_eq!(
      third.get(old).unwrap(),
      Open {
        fh: RESTORED_HANDLE,
        ..file(11)
      }
    );
    assert_eq!(
      third.get(newer).unwrap(),
      Open {
        fh: RESTORED_HANDLE,
        ..file(12)
      }
    );
  }

  /// A-198, A-148. Do: recover an old open, then open that same file in a fresh bridge handle store. Expect: the
  /// recovered handle cannot resolve to the successor's open, so releasing the old open cannot close the new one.
  #[test]
  fn a_recovered_handle_cannot_name_the_successors_first_open_of_the_same_file() {
    let mut first = Ledger::create(Ram(vec![0; bytes(1).unwrap()]), 1).unwrap();
    let context = first.opened(file(11), "\\file.bin").unwrap();
    let recovered = Ledger::recover(first.into_memory(), 1).unwrap();
    let mut successor = slates_bridge_core::volume_bridge::new_handle_store();
    let fresh = successor.insert(11).unwrap();
    let old = recovered.get(context).unwrap();
    let addressed = slates_mem::handle::Handle::from_raw(
      u32::try_from(old.fh >> u32::BITS).unwrap(),
      u32::try_from(old.fh & u64::from(u32::MAX)).unwrap(),
    );
    assert!(
      successor.get(addressed).is_err(),
      "the old open must not address the new one"
    );
    assert_eq!(successor.get(fresh).unwrap(), &11);
  }

  /// A-198. Do: record two opens, reopen their memory in a successor. Expect: file identity and purge marking
  /// survive, while the old process's bridge handles cannot alias any successor handle.
  #[test]
  fn an_open_and_a_purge_open_survive_the_process_that_issued_them() {
    let mut first = Ledger::create(Ram(vec![0; bytes(2).unwrap()]), 2).unwrap();
    let context = first.opened(file(11), "\\file.bin").unwrap();
    let purge = first
      .opened(
        Open {
          purge: true,
          ..file(12)
        },
        "\\purge.bin",
      )
      .unwrap();
    let next = Ledger::recover(first.into_memory(), 2).unwrap();
    assert_eq!(
      next.get(context).unwrap(),
      Open {
        fh: RESTORED_HANDLE,
        ..file(11)
      }
    );
    assert_eq!(
      next.get(purge).unwrap(),
      Open {
        fh: RESTORED_HANDLE,
        purge: true,
        ..file(12)
      }
    );
  }

  /// A-198. Do: close, reuse and restart a slot. Expect: its former contexts stay stale and a full ledger refuses.
  #[test]
  fn a_reused_slot_never_revives_its_previous_context() {
    let mut ledger = Ledger::create(Ram(vec![0; bytes(1).unwrap()]), 1).unwrap();
    let old = ledger.opened(file(11), "\\file.bin").unwrap();
    assert_eq!(ledger.opened(file(12), "\\file.bin"), Err(Refusal::Bound));
    ledger.closed(old).unwrap();
    let mut ledger = Ledger::recover(ledger.into_memory(), 1).unwrap();
    let new = ledger.opened(file(12), "\\file.bin").unwrap();
    assert_ne!(old, new);
    assert_eq!(ledger.get(old), Err(Refusal::Stale));
    assert_eq!(ledger.get(new).unwrap(), file(12));
    assert_eq!(ledger.closed(old), Err(Refusal::Stale));
  }

  /// A-198. Do: interrupt each prefix of a changed record. Expect: a complete old or new record, or a quarantined
  /// context; no interrupted frame resolves to a different file or re-enters the free list.
  #[test]
  fn a_torn_record_is_quarantined_instead_of_becoming_some_other_open() {
    let mut ledger = Ledger::create(Ram(vec![0; bytes(1).unwrap()]), 1).unwrap();
    let context = ledger.opened(file(11), "\\file.bin").unwrap();
    let original = ledger.into_memory().0;
    for cut in 1..FRAME {
      let mut interrupted = original.clone();
      interrupted[HEADER..HEADER + cut].fill(0xff);
      let resumed = Ledger::recover(Ram(interrupted), 1).unwrap();
      assert_eq!(resumed.get(context), Err(Refusal::Stale));
      assert!(!resumed.has_room());
      assert_eq!(resumed.quarantined(), 1);
    }
  }
}

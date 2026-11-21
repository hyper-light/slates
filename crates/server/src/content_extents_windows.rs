//! The content pool's extents as sections of their own on Windows (A-789's Windows shape; main's ruling of 2026-11-20
//! 18:40, R2: every platform gets the same give-back).
//!
//! **Why.** g226 measured on jupiter (BENCHMARKS "Content give-back per platform") that a shared section's commit comes
//! back only when the section closes: `VirtualFree(MEM_DECOMMIT)` on a view is refused (error 87), and
//! `DiscardVirtualMemory` trims only the caller's working set. A pool extent that was a range of the anchor's one content
//! section kept its commit charge after it was freed, for the daemon's whole life. So each extent is a reserved, unnamed
//! section of its own (`slates_mem::make_extent_section`), closed once the extent is wholly free.
//!
//! **Held across a restart.** The anchor holds a duplicate of every made extent's section (`crate::anchor_hold`, kind
//! `WHAT_CONTENT_EXTENT`, its record the extent's id), so a successor maps the same extent, and its bytes, after a
//! daemon's death; the handoff table hands them back and [`install`] places them before any shard claims.
//!
//! **Made ahead of need, never on the provisioning path (condition (a)).** [`install`] makes every extent the layout
//! names that the anchor did not hand back, before the shards serve, and each shard's start asks the anchor to hold its
//! slice's ([`hold_slice`]). A claim takes only an extent that is made and held ([`ready`]), so data never lands in a
//! section a daemon's death would lose; it duplicates the handle and maps it ([`section`]).
//!
//! **Given back whole.** A release closes this process's handle ([`take`]) and a task of the shard has the anchor let
//! its duplicate go, which closes the section and returns its commit, then makes the extent again and has it held
//! ([`remake`]). Until then the extent is not ready, and a claim passes it over.
//!
//! **One table a process, no lock.** A shard may claim another partition's extent, so the handles live in one table
//! for the process: a word for each extent's handle value and a flag for whether the anchor holds it, each written only
//! by whoever holds the extent's owner word in the anchor segment (its claimant, or its maker before any claim), so no
//! two writers meet. Bounded: `DaemonConfig::held_extent_bound` slots, one per extent the layout can name.

use std::os::windows::io::{BorrowedHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slates_anchor::layout::POOL_EXTENTS_PER_PARTITION;

use crate::config::DaemonConfig;

/// Format: the top sixteen bits of a content extent's id in the anchor's table: `0xFFFE`. A copy's id carries its
/// partition there (below 1,023) and a ledger's `0xFFFF` (`crate::held_send`), so no extent's id meets another kind's.
const EXTENT_HOLD_MARK: u64 = 0xFFFE_0000_0000_0000;

/// Format: a held extent's record: its id, two little-endian bytes.
const RECORD_BYTES: usize = size_of::<u16>();

/// One extent's slot: its section's handle value in this process (zero for none) and whether the anchor holds it.
struct Slot {
  handle: AtomicU64,
  held: AtomicBool,
}

/// The process's extents, one slot per id the layout can name ([`install`]).
static SLOTS: OnceLock<Box<[Slot]>> = OnceLock::new();

/// Counter: extents the anchor would not hold or let go, or the OS would not make: each stays unready (never claimed)
/// until the next try, and a section the anchor still holds goes with the anchor.
pub(crate) const EXTENT_REFUSED: &str = "content.extent.refused";

/// A held extent's handle as the anchor handed it back after a restart.
#[derive(Debug)]
pub(crate) struct HandedExtent {
  /// The extent's id.
  pub(crate) id: u16,
  /// Its section, this process's own.
  pub(crate) section: OwnedHandle,
}

/// The anchor's id of extent `id`'s hold.
pub(crate) fn hold_id(id: u16) -> u64 {
  EXTENT_HOLD_MARK | u64::from(id)
}

/// A held extent's record: its id.
pub(crate) fn record(id: u16) -> Vec<u8> {
  id.to_le_bytes().to_vec()
}

/// The extent id a held record names, `None` for a record of another shape.
pub(crate) fn id_of(record: &[u8]) -> Option<u16> {
  let bytes: [u8; RECORD_BYTES] = record.try_into().ok()?;
  Some(u16::from_le_bytes(bytes))
}

/// The length of extent `id` in `config`'s layout: its part of its slice's arena range, `None` for an id the layout
/// has no part for.
pub(crate) fn extent_len(config: &DaemonConfig, id: u16) -> Option<usize> {
  let part = usize::from(id).checked_rem(POOL_EXTENTS_PER_PARTITION)?;
  let slice = usize::from(id).checked_div(POOL_EXTENTS_PER_PARTITION)?;
  if slice >= usize::from(config.geometry.partitions.max(1)) {
    return None;
  }
  crate::content_pool::parts_of(config)
    .get(part)
    .map(|(_, len)| *len)
}

/// Places the extents the anchor handed back and makes every other one the layout names, before any shard claims: run
/// once, at the daemon's start. An extent the OS will not make is counted in the log and left unready.
pub(crate) fn install(config: &DaemonConfig, handed: Vec<HandedExtent>) {
  let slots: Box<[Slot]> = (0..config.held_extent_bound())
    .map(|_| Slot {
      handle: AtomicU64::new(0),
      held: AtomicBool::new(false),
    })
    .collect();
  for extent in handed {
    match slots.get(usize::from(extent.id)) {
      Some(slot) if slot.handle.load(Ordering::Acquire) == 0 => {
        slot
          .handle
          .store(value_of(extent.section), Ordering::Release);
        slot.held.store(true, Ordering::Release);
      }
      // An id the layout has no slot for, or one handed twice: its section closes here, and the anchor's duplicate goes
      // when the slice's holds are made again (`hold_slice` releases before it holds).
      _ => drop(extent.section),
    }
  }
  let mut refused = 0u64;
  for (index, slot) in slots.iter().enumerate() {
    let Some(len) = u16::try_from(index)
      .ok()
      .and_then(|id| extent_len(config, id))
    else {
      continue;
    };
    if slot.handle.load(Ordering::Acquire) != 0 {
      continue;
    }
    match slates_mem::make_extent_section(len) {
      Ok(section) => slot.handle.store(value_of(section), Ordering::Release),
      Err(_) => refused = refused.saturating_add(1),
    }
  }
  if refused > 0 {
    slates_machine::log_line!(
      "slates-server: {refused} content extents were not made; their ids stay unready until a release makes them"
    );
  }
  let _ = SLOTS.set(slots);
}

/// Whether this process's extents are sections of their own: the table is installed (a daemon under an anchor). A
/// daemon with no content object maps private extents, as before.
pub(crate) fn installed() -> bool {
  SLOTS.get().is_some()
}

/// [`install`] where the daemon runs under an anchor that holds handles (`anchored`: its channel was handed over); a
/// daemon with none keeps its extents as ranges of its content object, and the anchor's handed extents close here.
pub(crate) fn install_if_anchored(
  config: &DaemonConfig,
  anchored: bool,
  handed: Vec<HandedExtent>,
) {
  if anchored {
    install(config, handed);
  }
}

/// Has the anchor hold `partition`'s slice of extents ([`hold_slice`]) as a task of the calling shard, once the table is
/// installed. A task the runtime refuses leaves the slice's extents unready, said in the log.
pub(crate) fn spawn_slice_holds(config: &DaemonConfig, partition: u16) {
  if !installed() {
    return;
  }
  if slates_rt::futures::spawn(hold_slice(config.clone(), partition))
    .and_then(slates_rt::futures::detach)
    .is_err()
  {
    slates_machine::log_line!(
      "slates-server: shard {partition}: the content extents' hold task was refused; its extents stay unready"
    );
  }
}

/// Whether extent `id` may be claimed: made, and held by the anchor.
pub(crate) fn ready(id: u16) -> bool {
  SLOTS
    .get()
    .and_then(|slots| slots.get(usize::from(id)))
    .is_some_and(|slot| {
      slot.handle.load(Ordering::Acquire) != 0 && slot.held.load(Ordering::Acquire)
    })
}

/// A handle of extent `id`'s section for the claimant to map: a duplicate of the table's, which stays. `None` when the
/// extent is not made or the duplicate is refused.
pub(crate) fn section(id: u16) -> Option<OwnedHandle> {
  let value = SLOTS
    .get()?
    .get(usize::from(id))?
    .handle
    .load(Ordering::Acquire);
  if value == 0 {
    return None;
  }
  let raw = slates_anchor::held_windows::handle_from_value(value)?;
  // SAFETY: `raw` is the section handle this table owns for extent `id`, and only the holder of the extent's owner
  // word, which the caller is, takes it out of the table (`take`), so it stays open for this borrow.
  let borrowed = unsafe { BorrowedHandle::borrow_raw(raw) };
  borrowed.try_clone_to_owned().ok()
}

/// Takes extent `id`'s section out of the table, unready: the caller closes it (a release gives its commit back once
/// the anchor's duplicate goes too).
pub(crate) fn take(id: u16) -> Option<OwnedHandle> {
  let slot = SLOTS.get()?.get(usize::from(id))?;
  slot.held.store(false, Ordering::Release);
  let value = slot.handle.swap(0, Ordering::AcqRel);
  owned(value)
}

/// Has the anchor hold every made, unheld extent of `partition`'s slice: a task of the shard's, at its start.
pub(crate) async fn hold_slice(config: DaemonConfig, partition: u16) {
  let first = usize::from(partition).saturating_mul(POOL_EXTENTS_PER_PARTITION);
  for index in first..first.saturating_add(POOL_EXTENTS_PER_PARTITION) {
    let Ok(id) = u16::try_from(index) else {
      continue;
    };
    if extent_len(&config, id).is_none() {
      continue;
    }
    hold(id).await;
  }
}

/// Lets the anchor's duplicate of released extent `id` go, which closes its section and returns its commit, then makes
/// the extent again, `len` bytes, and has the anchor hold it: a task of the releasing shard's.
pub(crate) async fn remake(id: u16, len: usize) {
  if crate::anchor_hold::release_extent(hold_id(id))
    .await
    .is_err()
  {
    let _ = crate::state::with_state_counted(|s| s.count(EXTENT_REFUSED, 1));
    return;
  }
  let Some(slot) = SLOTS.get().and_then(|slots| slots.get(usize::from(id))) else {
    return;
  };
  match slates_mem::make_extent_section(len) {
    Ok(section) => {
      let value = value_of(section);
      if slot
        .handle
        .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
      {
        drop(owned(value));
        return;
      }
    }
    Err(_) => {
      let _ = crate::state::with_state_counted(|s| s.count(EXTENT_REFUSED, 1));
      return;
    }
  }
  hold(id).await;
}

/// Has the anchor hold extent `id`'s section if it is made and not yet held, and marks it held once the anchor says so.
async fn hold(id: u16) {
  let Some(slot) = SLOTS.get().and_then(|slots| slots.get(usize::from(id))) else {
    return;
  };
  let value = slot.handle.load(Ordering::Acquire);
  if value == 0 || slot.held.load(Ordering::Acquire) {
    return;
  }
  match crate::anchor_hold::hold_extent(hold_id(id), value, record(id)).await {
    Ok(()) => slot.held.store(true, Ordering::Release),
    Err(_) => {
      let _ = crate::state::with_state_counted(|s| s.count(EXTENT_REFUSED, 1));
    }
  }
}

/// A section's handle value, the table's own from here.
fn value_of(section: OwnedHandle) -> u64 {
  slates_anchor::held_windows::handle_value(section.into_raw_handle())
}

/// The table's handle `value` as an owned handle, closed when dropped; `None` for zero.
fn owned(value: u64) -> Option<OwnedHandle> {
  if value == 0 {
    return None;
  }
  let raw = slates_anchor::held_windows::handle_from_value(value)?;
  // SAFETY: `value` was taken out of the table by a swap or a lost exchange, so this process holds the only owner of
  // the handle it names, and nothing else closes it.
  Some(unsafe { OwnedHandle::from_raw_handle(raw) })
}

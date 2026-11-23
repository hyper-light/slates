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
//! names that the anchor did not hand back, before the shards serve, and the control shard, which holds the anchor's
//! channel and answers every claim, has the anchor hold every one before it answers its first ([`hold_every_slice`]).
//! A claim takes only an extent that is made and held ([`ready`]), so data never lands in a section a daemon's death
//! would lose; it duplicates the handle and maps it ([`section`]). Until 2026-11-23 each shard's start spawned its
//! slice's holds as a task of its own and served at once, so a create sent the moment a daemon served met its extents
//! made but not yet held, one anchor exchange each, and was refused `BudgetExceeded` with the whole pool counted as
//! room: every Node SDK suite's first create on CI's Windows runners, 2 of 25 extents held on windows-latest and 1 of
//! 22 on windows-11-arm, where the holds took 117.5 ms and 141.9 ms (diag run 45238179939).
//!
//! **Given back whole.** A release closes this process's handle and marks the extent being made again
//! ([`take_for_remake`]), and a task of the shard has the anchor let its duplicate go, which closes the section and
//! returns its commit, then makes the extent again and has it held ([`remake`]). Until then the extent is not ready: a
//! claim passes it over, the pool counts it pending rather than free (`crate::content_pool`), and an admission whose
//! room only such extents would make waits for their remakes, bounded by their exchanges ([`remake_bound_ns`]), and is
//! woken as each ends (`crate::room_wait`).
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

/// One extent's slot: its section's handle value in this process (zero for none), whether the anchor holds it, and
/// whether it is being made again after a release ([`take_for_remake`] to the end of [`remake`]).
struct Slot {
  handle: AtomicU64,
  held: AtomicBool,
  remaking: AtomicBool,
}

/// Shape: a remake's exchanges with the anchor: the release of the old section's duplicate, then the hold of the new.
const REMAKE_EXCHANGES: u64 = 2;

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
      remaking: AtomicBool::new(false),
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
      // An id the layout has no slot for, or one handed twice: its section closes here.
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

/// Takes extent `id`'s section out of the table for its remake ([`remake`]), unready and marked being made again: the
/// caller closes it (a release gives its commit back once the anchor's duplicate goes too) and starts the remake, or
/// says it could not ([`end_remake`]).
pub(crate) fn take_for_remake(id: u16) -> Option<OwnedHandle> {
  let slot = SLOTS.get()?.get(usize::from(id))?;
  slot.held.store(false, Ordering::Release);
  slot.remaking.store(true, Ordering::Release);
  let value = slot.handle.swap(0, Ordering::AcqRel);
  owned(value)
}

/// Extent `id` is no longer being made again: its remake ended, ready unless the OS or the anchor refused it, or could
/// not start (the shard's task arena refused it), when it stays unready until a release makes it.
pub(crate) fn end_remake(id: u16) {
  if let Some(slot) = SLOTS.get().and_then(|slots| slots.get(usize::from(id))) {
    slot.remaking.store(false, Ordering::Release);
  }
}

/// Whether extent `id` is being made again after a release: unready now, ready once its remake ends unless the OS or
/// the anchor refuses it.
pub(crate) fn remaking(id: u16) -> bool {
  SLOTS
    .get()
    .and_then(|slots| slots.get(usize::from(id)))
    .is_some_and(|slot| slot.remaking.load(Ordering::Acquire))
}

/// The extents being made again now, over the whole process.
pub(crate) fn remakes_in_flight() -> usize {
  SLOTS.get().map_or(0, |slots| {
    slots
      .iter()
      .filter(|slot| slot.remaking.load(Ordering::Acquire))
      .count()
  })
}

/// Derived: the longest one remake may take, nanoseconds: its [`REMAKE_EXCHANGES`] exchanges with the anchor, each given
/// up at the answer bound the hold channel has learned (`crate::anchor_hold::answer_bound_ns`, A-552).
pub(crate) fn remake_bound_ns() -> u64 {
  crate::anchor_hold::answer_bound_ns().saturating_mul(REMAKE_EXCHANGES)
}

/// [`hold_every_slice`] once the extents are sections of their own ([`installed`]: the daemon runs under an anchor that
/// holds handles, its channel installed on the control shard, whose control loop calls this before it answers a claim);
/// nothing for a daemon no anchor holds handles for.
pub(crate) async fn hold_before_serving(config: &DaemonConfig) {
  if installed() {
    hold_every_slice(config).await;
  }
}

/// Has the anchor hold every made, unheld extent the layout names, every partition's: run by the control shard, which
/// holds the anchor's channel, once that channel is installed and before it answers its first claim, so no admission
/// meets an extent made but not yet held. One exchange an extent, each within the learned answer bound (A-552); a
/// refusal is counted ([`EXTENT_REFUSED`]) and its extent stays unready. Logged once: how many the anchor holds of those
/// made, and how long the holds took.
pub(crate) async fn hold_every_slice(config: &DaemonConfig) {
  let begun = slates_machine::clock::monotonic_ns();
  for index in 0..config.held_extent_bound() {
    let Ok(id) = u16::try_from(index) else {
      continue;
    };
    if extent_len(config, id).is_none() {
      continue;
    }
    hold(id).await;
  }
  let took =
    std::time::Duration::from_nanos(slates_machine::clock::monotonic_ns().saturating_sub(begun));
  let (made, held) = SLOTS.get().map_or((0, 0), |slots| {
    let made = slots
      .iter()
      .filter(|slot| slot.handle.load(Ordering::Acquire) != 0)
      .count();
    let held = slots
      .iter()
      .filter(|slot| slot.held.load(Ordering::Acquire))
      .count();
    (made, held)
  });
  slates_machine::log_line!(
    "slates-server: the anchor holds {held} of the {made} content extents made, held in {took:?}, before the control shard answers a claim"
  );
}

/// Lets the anchor's duplicate of released extent `id` go, which closes its section and returns its commit, then makes
/// the extent again, `len` bytes, and has the anchor hold it: a task of the releasing shard's. However it ends, the
/// extent is no longer being made again, and every admission waiting for room looks again (`crate::room_wait`).
pub(crate) async fn remake(id: u16, len: usize) {
  made_again(id, len).await;
  end_remake(id);
  crate::room_wait::remade();
}

/// [`remake`]'s exchanges and the new section.
async fn made_again(id: u16, len: usize) {
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

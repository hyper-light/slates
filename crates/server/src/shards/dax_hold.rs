//! A Shards stream's DAX objects held by the anchor (§4.6, the Shards form; A-365, and Windows' sections, A-448): what
//! lets a stream whose guest maps files with DAX outlive its daemon, as A-316's resync lets one that maps none.
//!
//! A guest's stores through a DAX map land in its file's object (`slates_mem::DaxObject`), which the device maps; they
//! reach the file only when slates ingests them, at the guest's syncs and the stream's end. A daemon's death leaves the
//! device mapping the object, its stores since the last sync in it, and a successor that knows nothing of it. So:
//! - **The anchor holds each backing's object** ([`keep`]), with its record ([`DaxRecord`]: the attachment and the
//!   inode), before its first descriptor goes to the device, and releases it when the backing ends ([`let_go`]). Unix
//!   sends the object one way with its message, so the hold is made as the answer is owed. Windows' hold is an exchange
//!   (the anchor duplicates the section out of this process and answers, `crate::anchor_hold`), which the answer waits
//!   for: the serve task asks it (`hold_owed`) before the answer goes out, as it tells the anchor a stream's session
//!   (A-427). Its id is the backing's handle in the shard's registry, encoded with its partition, as A-197 names a
//!   writable copy, so no two held copies ever share one; on Windows, where one table holds every kind and a release
//!   names an entry by id alone, it is marked apart from every attachment's id ([`marked`]).
//! - **Each answered range is recorded** in the shard's range table in anchor RAM (`crate::dax_ranges`), so the
//!   successor knows which ranges of each object are the file's and which the guest may have stored into.
//! - **A successor takes the objects back while recovery rebuilds the shard** ([`adopt_inherited`]), for each Shards
//!   stream recovery kept: each object kept again in the store at the very handle its backing had (beside A-197's
//!   copies, in one ascending order), its ranges filled and writable as the table says, and dormant: no write reaches it
//!   until its stream takes it. Its object already holds every write the dead daemon made to its ranges, in order with
//!   its guest's stores, so the write log's replay must not write a logged write over a later store (measured: it did,
//!   and the store never reached the file). The stream's adoption then takes them into its session ([`claim`]), wakes
//!   them, ingests their writable ranges before it serves anything, and answers the device's asks with the same objects,
//!   so the guest's maps stay right: on Windows each ask after the `Resync` duplicates the same section into the holder
//!   again. A backing no kept stream claims is let go: its ranges forgotten, its object released. A file whose object the
//!   anchor did not hand back is asked for again at its next answer ([`hold_asked`]), never assumed held.
//!
//! A hold or a record the anchor or the content object refuses gives the stream's resync up, as before A-365: its
//! device learns at once that its share ends with this daemon. On Windows that includes a section past the anchor's
//! bound for sections (`HoldBounds::sections`, `shards × dax_range_slots`), refused `Bound`.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;

use slates_bridge_core::dax_backing::{filled_charge, most_views};
use slates_mem::{DaxLayout, DaxObject, Encoded, Handle};
use slates_vfs::ids::InodeNo;
use slates_vfs::shards_backing::ShardsBacking;

use super::ShardsStream;
use crate::anchor_hold::HandedCopy;
use crate::state::ShardState;

/// The handle a held DAX object is reached by: a descriptor on Unix, a section's handle on Windows (A-448).
#[cfg(unix)]
type HeldObject = OwnedFd;
/// See the Unix arm.
#[cfg(windows)]
type HeldObject = OwnedHandle;

/// Format: the tag a held DAX object's record opens with, "DAXHELD" and version 1. A writable direct map's record
/// (A-197) is two words long and opens with no tag.
const TAG: u64 = u64::from_le_bytes(*b"DAXHELD\x01");
/// Format: a held DAX object's record: the tag, the attachment and the inode, each a little-endian `u64`.
const RECORD_BYTES: usize = 3 * size_of::<u64>();

/// Format: the bit that names a Windows DAX section's anchor id apart from every attachment's (A-448). The Windows anchor
/// keeps one table of ids for every kind it holds, and a release and its answer name an entry by id alone; an
/// attachment's id carries its partition in its top 16 bits (`crate::verbs::attachment_id`), as a backing's encoded
/// handle does (`slates_mem::Encoded`), so the two could be equal. Under a partition below 2^15 this bit is clear in
/// both, and a section's id sets it.
#[cfg_attr(not(windows), allow(dead_code))]
const SECTION_MARK: u64 = 1 << 63;

/// Counter: a DAX object the anchor refused to hold, or whose ranges the range table refused: its stream gives its
/// resync up, so its device learns at once that its share ends with this daemon.
const HOLD_REFUSED: &str = "shards.dax_hold_refused";
/// Counter: a DAX object a successor took back from its anchor (A-365), the adoption's non-vacuity counter.
const ADOPTED: &str = "shards.dax_adopted";
/// Counter: a held DAX object a successor could not take back (its stream not kept, its volume, its file, its ranges or
/// the object refused), let go: what its guest stored since its last sync reaches nothing.
const ADOPT_REFUSED: &str = "shards.dax_adopt_refused";
/// Counter: a Windows DAX section whose release the runtime or the anchor refused (A-448): the anchor keeps it until the
/// next daemon lets it go as one no kept stream claims.
#[cfg(windows)]
const RELEASE_REFUSED: &str = "shards.dax_release_refused";

/// A held DAX object's record: the attachment whose backing it is, and the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DaxRecord {
  /// The attachment, by its durable id.
  pub(crate) attachment: u64,
  /// The file, its inode number in the attachment's volume.
  pub(crate) inode: u64,
}

impl DaxRecord {
  /// Its bytes ([`RECORD_BYTES`]).
  pub(crate) fn encode(self) -> [u8; RECORD_BYTES] {
    let mut bytes = [0u8; RECORD_BYTES];
    let words = [TAG, self.attachment, self.inode].map(u64::to_le_bytes);
    for (slot, byte) in bytes.iter_mut().zip(words.iter().flatten()) {
      *slot = *byte;
    }
    bytes
  }

  /// The record `bytes` hold, or none for bytes of another length or tag (another kind's record, or damage).
  pub(crate) fn decode(bytes: &[u8]) -> Option<DaxRecord> {
    let whole = <&[u8; RECORD_BYTES]>::try_from(bytes).ok()?;
    let (words, _) = whole.as_chunks::<{ size_of::<u64>() }>();
    let [tag, attachment, inode] = <[[u8; size_of::<u64>()]; 3]>::try_from(words)
      .ok()?
      .map(u64::from_le_bytes);
    (tag == TAG).then_some(DaxRecord { attachment, inode })
  }
}

/// The anchor id of a Windows DAX section whose backing's handle encodes to `word` with its partition (A-448): `word`
/// with [`SECTION_MARK`] set, so no attachment's id is ever one, and its slot and generation unchanged, so a successor
/// takes the section back at its backing's handle. None where the partition's own bits reach the mark (a partition of
/// 2^15 or more), under which no section is held: its stream gives its resync up instead.
#[cfg_attr(not(windows), allow(dead_code))]
fn marked(word: u64) -> Option<u64> {
  (word & SECTION_MARK == 0).then_some(word | SECTION_MARK)
}

/// Whether the answer of `inode` on a stream whose resync the anchor holds must have the anchor hold its object first,
/// `held` being the objects held for the stream by inode: whenever none of `inode` is. A successor's stream holds only
/// what its adoption claimed ([`claim`]), so a file whose object the anchor did not hand back (refused at its bound, or
/// never held) is asked for again at its next answer (A-448), never taken for held.
fn hold_asked(held: &BTreeMap<u64, u64>, inode: u64) -> bool {
  !held.contains_key(&inode)
}

/// A held DAX object the anchor handed this shard back.
#[derive(Debug)]
pub(crate) struct InheritedDax {
  /// Its id: the dead daemon's backing's handle, encoded with its partition.
  pub(crate) copy: u64,
  /// Its record.
  pub(crate) record: DaxRecord,
  /// The object.
  pub(crate) object: HeldObject,
}

/// Splits the copies the anchor handed back: each DAX object (A-365, A-448) to the partition its attachment names;
/// every other copy returned for its own kind (A-197's writable copies, `crate::held_copy::split`, Linux). A DAX object
/// whose partition this daemon has not is let go ([`unroutable`]).
pub(crate) fn split(
  handed: Vec<HandedCopy>,
  partitions: usize,
) -> (Vec<HandedCopy>, Vec<Vec<InheritedDax>>) {
  let mut others = Vec::new();
  let mut daxes: Vec<Vec<InheritedDax>> = (0..partitions).map(|_| Vec::new()).collect();
  for handed in handed {
    let Some(record) = DaxRecord::decode(&handed.record) else {
      others.push(handed);
      continue;
    };
    let partition = usize::from(crate::verbs::owner_of_attachment(record.attachment));
    let inherited = InheritedDax {
      copy: handed.copy,
      record,
      object: handed.object,
    };
    match daxes.get_mut(partition) {
      Some(list) => list.push(inherited),
      None => unroutable(&mut daxes, inherited),
    }
  }
  (others, daxes)
}

/// Lets go of a held DAX object whose attachment names a partition this daemon has not: at once on Unix, where the
/// release is a send.
#[cfg(unix)]
fn unroutable(_daxes: &mut [Vec<InheritedDax>], dax: InheritedDax) {
  let _sent = crate::anchor_hold::release_copy(dax.copy);
}

/// Lets go of a held DAX section whose attachment names a partition this daemon has not (Windows): its release is an
/// exchange only a shard can make, so it goes to the first partition, whose recovery lets it go as one no kept stream
/// claims.
#[cfg(windows)]
fn unroutable(daxes: &mut [Vec<InheritedDax>], dax: InheritedDax) {
  if let Some(first) = daxes.first_mut() {
    first.push(dax);
  }
}

/// Asks the anchor to hold `stream`'s object of `inode`, its backing `backing`, before its first descriptor goes to the
/// device (Unix: the object rides the message, so the hold is made now), and keeps its copy id. Whether it is held: not
/// where the anchor refused it, counted, or no anchor holds at all.
#[cfg(unix)]
fn hold(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  inode: u64,
  backing: Handle<ShardsBacking>,
) -> bool {
  let Some(copy) = backing.encode(s.partition).map(Encoded::word) else {
    return false;
  };
  let record = DaxRecord {
    attachment: stream.attachment,
    inode,
  };
  let Some(object) = s
    .store
    .shards_backings
    .get(backing)
    .map(|held| held.object().descriptor())
  else {
    return false;
  };
  match crate::anchor_hold::hold_copy(copy, &record.encode(), object.as_fd()) {
    Some(Ok(())) => {
      stream.held_dax.insert(inode, copy);
      true
    }
    Some(Err(refusal)) => {
      slates_machine::log_line!(
        "slates-server: the anchor did not hold DAX object {copy:x}: {refusal}"
      );
      s.count(HOLD_REFUSED, 1);
      false
    }
    None => false,
  }
}

/// Owes the anchor `stream`'s section of `inode` before its answer goes out (Windows, A-448): the hold is an exchange,
/// which the serve task asks (`hold_owed`) before the answer it waits for. The device sends nothing more until it has
/// its answer, so at most one is owed at a time.
#[cfg(windows)]
fn hold(
  _s: &mut ShardState,
  stream: &mut ShardsStream,
  inode: u64,
  _backing: Handle<ShardsBacking>,
) -> bool {
  stream.section_owed = Some(inode);
  true
}

/// Tells the anchor DAX object `copy` was let go, so it closes its copy (Unix: a send). A refusal leaves nothing held
/// (the anchor's end is gone, and every copy with it), so it is not retried.
#[cfg(unix)]
fn release(_s: &mut ShardState, copy: u64) {
  let _sent = crate::anchor_hold::release_copy(copy);
}

/// Tells the anchor DAX section `copy` was let go, so it closes its copy (Windows, A-448): an exchange, so a task of the
/// shard. One the runtime refuses is counted, and the next daemon lets the section go as one no kept stream claims.
#[cfg(windows)]
fn release(s: &mut ShardState, copy: u64) {
  if slates_rt::futures::spawn(released(copy))
    .and_then(slates_rt::futures::detach)
    .is_err()
  {
    s.count(RELEASE_REFUSED, 1);
  }
}

/// The release of DAX section `copy` (Windows, A-448), one attempt: the anchor answers a release of a section it never
/// took the same, so a refusal is the channel's, counted.
#[cfg(windows)]
async fn released(copy: u64) {
  if crate::anchor_hold::release_section(copy).await.is_err() {
    let _ = crate::state::with_state_counted(|s| s.count(RELEASE_REFUSED, 1));
  }
}

/// Keeps what a successor needs of `stream`'s backing of `inode`, just answered: the anchor holds its object (once,
/// asked again whenever none is held, [`hold_asked`]; on Windows before the answer goes out, `hold_owed`), and the
/// range table records every range it has answered, writable or not. Whether the stream may keep its resync: false
/// when either is refused, and then nothing of the backing stays held or recorded.
pub(super) fn keep(s: &mut ShardState, stream: &mut ShardsStream, inode: u64) -> bool {
  let Some(backing) = stream.dax.backing_of(inode) else {
    return false;
  };
  if hold_asked(&stream.held_dax, inode) && !hold(s, stream, inode, backing) {
    return false;
  }
  let ranges = s.store.shards_backings.get(backing).map(|held| {
    (
      held.filled().iter().copied().collect::<Vec<u64>>(),
      held.writable().clone(),
    )
  });
  let recorded = match (ranges, s.dax_ranges.as_mut(), s.content.as_mut()) {
    (Some((filled, writable)), Some(table), Some(object)) => filled.iter().all(|range| {
      table
        .record(
          object,
          (stream.attachment, inode),
          *range,
          writable.contains(range),
        )
        .is_ok()
    }),
    _ => false,
  };
  if !recorded {
    s.count(HOLD_REFUSED, 1);
    let_go(s, stream, inode);
  }
  recorded
}

/// Whether `stream`'s owed answer waits for the anchor to hold a DAX section first (Windows, A-448). An ending stream
/// holds nothing more: its backings are let go as it ends.
#[cfg(windows)]
pub(super) fn owed(stream: &ShardsStream) -> bool {
  !stream.ending && stream.section_owed.is_some()
}

/// What the anchor is asked to hold for a stream's owed answer (Windows, A-448): the file, the section's id, a duplicate
/// of the section to name to the anchor, and its record.
#[cfg(windows)]
struct SectionAsk {
  inode: u64,
  id: u64,
  section: OwnedHandle,
  record: Vec<u8>,
}

/// Asks the anchor to hold the DAX section `attachment`'s owed answer waits for (Windows, A-448), then settles what it
/// answered ([`settle_section`]). The duplicate named to the anchor closes once the exchange is over: the anchor's own
/// is then the only one besides the object's. The answer goes out at the next turn whatever the anchor said.
#[cfg(windows)]
pub(super) async fn hold_owed(attachment: u64) {
  use std::os::windows::io::AsRawHandle;
  let Some(Some(asked)) = crate::state::with_state_counted(|s| section_ask(s, attachment)) else {
    return;
  };
  let value = slates_anchor::held_windows::handle_value(asked.section.as_raw_handle());
  let answered = crate::anchor_hold::hold_section(asked.id, value, asked.record).await;
  drop(asked.section);
  let may_hold = match &answered {
    Ok(()) => true,
    Err(refusal) => {
      slates_machine::log_line!(
        "slates-server: the anchor did not hold DAX section {:x}: {refusal}",
        asked.id
      );
      refusal.may_hold()
    }
  };
  let _ = crate::state::with_state_counted(|s| {
    settle_section(
      s,
      attachment,
      (asked.inode, asked.id),
      (answered.is_ok(), may_hold),
    );
  });
}

/// The hold `attachment`'s stream owes, if one is owed and can be asked (Windows, A-448). One that cannot (its resync
/// given up meanwhile, its backing gone, the section's duplicate refused, a partition past [`SECTION_MARK`]) is settled
/// at once as refused: nothing of the file is held for a successor, so the stream keeps no resync.
#[cfg(windows)]
fn section_ask(s: &mut ShardState, attachment: u64) -> Option<SectionAsk> {
  let mut stream = s.shards_streams.remove(&attachment)?;
  let asked = match stream.section_owed {
    Some(inode) => {
      let ask = ask_of(s, &stream, inode);
      if ask.is_none() {
        refuse_section(s, &mut stream, inode);
      }
      ask
    }
    None => None,
  };
  s.shards_streams.insert(attachment, stream);
  asked
}

/// What the anchor is asked to hold for `stream`'s section of `inode`, or none where it cannot be asked.
#[cfg(windows)]
fn ask_of(s: &ShardState, stream: &ShardsStream, inode: u64) -> Option<SectionAsk> {
  if !super::resync_handle::held(stream) {
    return None;
  }
  let backing = stream.dax.backing_of(inode)?;
  let id = backing
    .encode(s.partition)
    .map(Encoded::word)
    .and_then(marked)?;
  let section = s.store.shards_backings.get(backing)?.object().held().ok()?;
  let record = DaxRecord {
    attachment: stream.attachment,
    inode,
  }
  .encode()
  .to_vec();
  Some(SectionAsk {
    inode,
    id,
    section,
    record,
  })
}

/// Settles the hold of `attachment`'s section of `inode` as entry `id` by the anchor's answer: `held`, or not when the
/// anchor `may_hold` it all the same (Windows, A-448). Held while its stream still wants it (the stream serving, its
/// resync held, the hold still owed, the backing kept), the section is the stream's to release at its backing's end.
/// Otherwise what the anchor may hold is released, and where the stream still wanted it the refusal gives its resync up
/// ([`refuse_section`]).
#[cfg(windows)]
fn settle_section(
  s: &mut ShardState,
  attachment: u64,
  (inode, id): (u64, u64),
  (held, may_hold): (bool, bool),
) {
  let Some(mut stream) = s.shards_streams.remove(&attachment) else {
    if may_hold {
      release(s, id);
    }
    return;
  };
  let wanted = stream.section_owed == Some(inode)
    && super::resync_handle::held(&stream)
    && stream.dax.backing_of(inode).is_some();
  if held && wanted {
    stream.section_owed = None;
    stream.held_dax.insert(inode, id);
  } else {
    if may_hold {
      release(s, id);
    }
    if wanted {
      refuse_section(s, &mut stream, inode);
    }
  }
  s.shards_streams.insert(attachment, stream);
}

/// Settles `stream`'s section of `inode` as one the anchor does not hold (Windows, A-448), counted: its ranges forgotten
/// and the stream's resync given up, every other section released with it (`super::give_up_resync_for_dax`), so its
/// device learns at once that its share ends with this daemon.
#[cfg(windows)]
fn refuse_section(s: &mut ShardState, stream: &mut ShardsStream, inode: u64) {
  if super::resync_handle::held(stream) {
    s.count(HOLD_REFUSED, 1);
  }
  let_go(s, stream, inode);
  super::give_up_resync_for_dax(s, stream);
}

/// Lets go of what is kept of `stream`'s backing of `inode` (it ended: the guest forgot the node, or the resync was given
/// up): its object released by the anchor, its ranges forgotten, and on Windows a hold still owed for it no longer.
pub(super) fn let_go(s: &mut ShardState, stream: &mut ShardsStream, inode: u64) {
  #[cfg(windows)]
  if stream.section_owed == Some(inode) {
    stream.section_owed = None;
  }
  if let Some(copy) = stream.held_dax.remove(&inode) {
    release(s, copy);
  }
  if let (Some(table), Some(object)) = (s.dax_ranges.as_mut(), s.content.as_mut()) {
    let _forgotten = table.forget(object, (stream.attachment, inode));
  }
}

/// Lets go of what is kept of every backing of `stream` ([`let_go`] for each): its end, or its resync given up.
pub(super) fn let_go_all(s: &mut ShardState, stream: &mut ShardsStream) {
  #[cfg(windows)]
  {
    stream.section_owed = None;
  }
  for (_, copy) in std::mem::take(&mut stream.held_dax) {
    release(s, copy);
  }
  if let (Some(table), Some(object)) = (s.dax_ranges.as_mut(), s.content.as_mut()) {
    let _forgotten = table.forget_attachment(object, stream.attachment);
  }
}

/// A held DAX object kept again in the store for a kept stream, until its stream's adoption takes it
/// (`crate::shards::adopt_one`).
#[derive(Debug)]
pub(crate) struct AdoptedDax {
  /// The file.
  pub(crate) inode: u64,
  /// Its backing in the store.
  pub(crate) backing: Handle<ShardsBacking>,
  /// The anchor's copy id of its object.
  pub(crate) copy: u64,
}

/// Takes back one DAX object the anchor handed this shard while recovery rebuilds it ([`adopt_inherited`]): kept again
/// in the store at its backing's handle, its ranges as the table says, for the stream recovery kept. Whether it was.
fn adopt_one(state: &mut ShardState, dax: InheritedDax) -> bool {
  let InheritedDax {
    copy,
    record,
    object,
  } = dax;
  let kept = state
    .adopt_shards
    .iter()
    .any(|(kept, _)| kept.id == record.attachment);
  let Some(found) = state.db.partition().attachment(record.attachment) else {
    return false;
  };
  let Some(&handle) = state.by_id.get(&found.volume).filter(|_| kept) else {
    return false;
  };
  let Ok(slot) = state.volumes.get(handle) else {
    return false;
  };
  let inode = InodeNo(record.inode);
  if slot.volume.stat(&state.store, inode).is_err() {
    return false;
  }
  let prefix = slot.volume.prefix();
  let ranges = state
    .dax_ranges
    .as_ref()
    .map(|table| table.ranges_of((record.attachment, record.inode)))
    .unwrap_or_default();
  if ranges.filled.is_empty() {
    return false;
  }
  let Ok(object) = DaxObject::adopt(
    object,
    most_views(&state.store.shards_backings),
    DaxLayout::Identity,
  ) else {
    return false;
  };
  let charge = filled_charge(DaxLayout::Identity, ranges.filled.iter().copied());
  let encoded = Encoded::from_word(copy);
  let Ok(backing) = state.store.shards_backings.adopt_vmm(
    object,
    (prefix, inode),
    (encoded.slot(), encoded.generation()),
    (ranges.filled, ranges.writable),
    charge,
  ) else {
    return false;
  };
  state
    .adopted_dax
    .entry(record.attachment)
    .or_default()
    .push(AdoptedDax {
      inode: record.inode,
      backing,
      copy,
    });
  true
}

/// One held backing of either kind, for the one ascending order of handles the store's registry takes them back in.
enum Held {
  /// A Shards stream's DAX object (A-365).
  Dax(InheritedDax),
  /// A writable direct map's copy (A-197).
  #[cfg(target_os = "linux")]
  Map(crate::held_copy::InheritedCopy),
}

/// Takes back every object the anchor handed this shard, while recovery rebuilds it and before the write log's replay,
/// each dormant through the replay, since it already holds every write the dead daemon made, in order with the stores
/// made through it: A-197's writable copies (Linux), woken once the replay is done (`crate::held_copy::wake_adopted`,
/// A-389), and the Shards streams' DAX objects (Windows' sections too, A-448), woken when their streams take them
/// ([`claim`]); together in ascending order of the handles their backings had (the registry's `insert_at` contract). One
/// that cannot be taken back is let go and counted; then every range the table names for a backing not taken back is
/// forgotten. How many were taken back.
pub(crate) fn adopt_inherited(state: &mut ShardState) -> usize {
  let mut order: Vec<(u32, Held)> = std::mem::take(&mut state.inherited_dax)
    .into_iter()
    .map(|dax| (Encoded::from_word(dax.copy).slot(), Held::Dax(dax)))
    .collect();
  #[cfg(target_os = "linux")]
  let deadline = crate::held_copy::claim_deadline(state);
  #[cfg(target_os = "linux")]
  order.extend(
    std::mem::take(&mut state.inherited_copies)
      .into_iter()
      .map(|map| (Encoded::from_word(map.copy).slot(), Held::Map(map))),
  );
  order.sort_by_key(|(slot, _)| *slot);
  let mut adopted: usize = 0;
  for (_, held) in order {
    let taken = match held {
      Held::Dax(dax) => {
        let (copy, record) = (dax.copy, dax.record);
        let taken = adopt_one(state, dax);
        state.count(if taken { ADOPTED } else { ADOPT_REFUSED }, 1);
        if !taken {
          release(state, copy);
          if let (Some(table), Some(object)) = (state.dax_ranges.as_mut(), state.content.as_mut()) {
            let _forgotten = table.forget(object, (record.attachment, record.inode));
          }
        }
        taken
      }
      #[cfg(target_os = "linux")]
      Held::Map(map) => crate::held_copy::adopt_or_release(state, map, deadline),
    };
    if taken {
      adopted = adopted.saturating_add(1);
    }
  }
  forget_unadopted(state);
  adopted
}

/// Forgets every range the table names for a backing recovery did not take back: no object of it is held, so nothing
/// will ask for it.
fn forget_unadopted(state: &mut ShardState) {
  let taken: std::collections::BTreeSet<(u64, u64)> = state
    .adopted_dax
    .iter()
    .flat_map(|(attachment, list)| list.iter().map(move |dax| (*attachment, dax.inode)))
    .collect();
  if let (Some(table), Some(object)) = (state.dax_ranges.as_mut(), state.content.as_mut()) {
    for backing in table.backings() {
      if !taken.contains(&backing) {
        let _forgotten = table.forget(object, backing);
      }
    }
  }
}

/// Takes the DAX objects recovery kept for `stream`'s attachment into its session, and starts the ingest of their
/// writable ranges, which runs before the stream serves anything: the guest's stores since its last sync reach the file.
/// Whether an ingest was started.
pub(super) fn claim(s: &mut ShardState, stream: &mut ShardsStream) -> bool {
  let Some(kept) = s.adopted_dax.remove(&stream.attachment) else {
    return false;
  };
  for dax in kept {
    if stream.dax.adopt(dax.inode, dax.backing) {
      // Woken now, after the write log's replay: every change of its file reaches it again from here on.
      s.store.shards_backings.wake(dax.backing);
      stream.held_dax.insert(dax.inode, dax.copy);
    } else {
      let_go_unclaimed(s, stream.attachment, dax);
    }
  }
  stream.dax.begin_ingest_all(
    &s.store.shards_backings,
    slates_bridge_core::dax_backing::Then::Resume,
  )
}

/// Lets go of a DAX object recovery kept for `attachment` that no stream took: its backing removed from the store, its
/// charge given back, its ranges forgotten, its object released, counted.
fn let_go_unclaimed(s: &mut ShardState, attachment: u64, dax: AdoptedDax) {
  if let Some(gone) = s.store.shards_backings.remove(dax.backing) {
    s.store.shards_backings.uncharge(filled_charge(
      DaxLayout::Identity,
      gone.filled().iter().copied(),
    ));
  }
  release(s, dax.copy);
  if let (Some(table), Some(object)) = (s.dax_ranges.as_mut(), s.content.as_mut()) {
    let _forgotten = table.forget(object, (attachment, dax.inode));
  }
  s.count(ADOPT_REFUSED, 1);
}

/// Lets go of every DAX object recovery kept that no stream's adoption took (its stream refused, or gone): run after
/// every kept stream was adopted.
pub(crate) fn let_go_every_unclaimed(s: &mut ShardState) {
  for (attachment, kept) in std::mem::take(&mut s.adopted_dax) {
    for dax in kept {
      let_go_unclaimed(s, attachment, dax);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-365. Do: encode a record and decode it; decode one byte short, one long, a record of A-197's length, and one
  /// whose tag is another's. Expect the same record back, and none for the rest.
  #[test]
  fn a_dax_record_round_trips_and_refuses_another_kinds() {
    let record = DaxRecord {
      attachment: 0x0102_0304_0506_0708,
      inode: 0xA0B0_C0D0_E0F0_0011,
    };
    let bytes = record.encode();
    assert_eq!(DaxRecord::decode(&bytes), Some(record));
    assert_eq!(DaxRecord::decode(&bytes[..RECORD_BYTES - 1]), None);
    let mut long = bytes.to_vec();
    long.push(0);
    assert_eq!(DaxRecord::decode(&long), None);
    assert_eq!(DaxRecord::decode(&bytes[..2 * size_of::<u64>()]), None);
    let mut foreign = bytes;
    foreign[0] ^= 1;
    assert_eq!(DaxRecord::decode(&foreign), None);
  }

  /// A-448. Do: name the section of each backing handle of several partitions, beside the ids of those partitions'
  /// attachments, from the first counter to the last a partition's 48 bits hold; then name one under a partition that
  /// reaches the mark. Expect no section's id ever an attachment's, each section's id keeping its backing's slot and
  /// generation (the handle a successor takes it back at), and none under the partition past the mark.
  #[test]
  fn a_sections_id_is_never_an_attachments_and_keeps_its_handle() {
    let counters = [
      0,
      1,
      2,
      0x00FF_FFFF,
      0x0100_0000,
      0x7FFF_FFFF_FFFF,
      0xFFFF_FFFF_FFFF,
    ];
    let handles = [
      (0, 0),
      (0, 1),
      (1, 0),
      (7, 3),
      (Encoded::MAX_TASK_SLOT, Encoded::MAX_GENERATION),
    ];
    for partition in [0u16, 1, 7, 0x7FFF] {
      let attachments: Vec<u64> = (0..=partition.min(8))
        .chain([partition])
        .flat_map(|owner| {
          counters
            .iter()
            .map(move |counter| crate::verbs::attachment_id(owner, *counter))
        })
        .collect();
      for (slot, generation) in handles {
        let word = Encoded::pack(partition, slot, generation).unwrap().word();
        let id = marked(word).unwrap();
        assert!(
          !attachments.contains(&id),
          "section {slot}@{generation} of partition {partition} named {id:x}, an attachment's id"
        );
        let decoded = Encoded::from_word(id);
        assert_eq!((decoded.slot(), decoded.generation()), (slot, generation));
      }
    }
    let past = Encoded::pack(0x8000, 1, 1).unwrap().word();
    assert_eq!(marked(past), None, "a partition whose bits reach the mark");
  }

  /// A-448. Do: as a successor's stream holds after its adoption claimed the one object its anchor handed back (inode
  /// 5), ask whether answering each file must have the anchor hold its object first; then record the hold of the other
  /// (inode 7) and let the first go. Expect the claimed file not asked again, the file whose object was not handed back
  /// asked for (the successor re-asks, never assuming it held), the recorded one not asked again, and the let-go one
  /// asked for once more.
  #[test]
  fn a_successor_that_finds_no_held_section_asks_the_anchor_again() {
    let mut held = BTreeMap::new();
    held.insert(5u64, marked(0x0001_0000_0100_0002).unwrap());
    assert!(!hold_asked(&held, 5), "the object its adoption claimed");
    assert!(
      hold_asked(&held, 7),
      "a file whose object was not handed back"
    );
    held.insert(7, marked(0x0001_0000_0200_0001).unwrap());
    assert!(!hold_asked(&held, 7), "once the anchor holds it");
    held.remove(&5);
    assert!(hold_asked(&held, 5), "a file let go and mapped again");
  }
}

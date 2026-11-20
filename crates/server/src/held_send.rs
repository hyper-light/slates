//! Whether a held NFS connection a dead daemon left can be served on by its successor, judged from the send it had in
//! flight, and the ledger that records each send for that judgment (§4.6, the loopback listener; A-113's Linux half;
//! A-550).
//!
//! The anchor holds each loopback connection across a restart, so a restart is a slow reply and never a dropped
//! connection (A-113). That rests on one fact: no reply is ever torn on the wire. A request leaves the kernel only once
//! its reply is in it, so a successor answers every request still queued; a reply the dead daemon sent whole may be sent
//! again, and an ONC RPC client drops a reply whose xid it no longer awaits (RFC 5531). A reply cut part-way would
//! instead hand the client the next reply's bytes as the rest of the torn one.
//!
//! macOS makes every send all or nothing (`SO_SNDLOWAT` at a record's size, A-113). Linux keeps `SO_SNDLOWAT` fixed and
//! copies what fits, so a send can end mid-record. A Linux connection is held instead under a check at adoption: the
//! bytes the kernel says the connection's writer handed it (`tcpi_bytes_acked` plus `SIOCOUTQ`,
//! `slates_rt::tcp::TcpStream::bytes_handed`) against a ledger of the send in flight, the bytes sent whole before it
//! and its own length ([`resume`], [`adoption`]). Exactly either boundary serves the connection on; any other count ends
//! it, and its client reconnects as every Linux client did before.
//!
//! **The ledger** ([`LedgerView`]) lives in a shared memory object the anchor holds as one more copy beside the DAX
//! objects and the writable maps' copies ([`LedgerRecord`] is its record), so it outlives the daemon that writes it.
//! Its header word is how many slots were ever claimed (how far a successor reads); each held connection has a slot on
//! a cache line of its own (each is written by the shard serving its connection), holding the connection's id, two
//! records of its stream and a selector naming the one in force. An update writes the idle record whole and then flips
//! the selector, one store, so a death between any two stores leaves a record that was written whole in force, and the
//! judgment of it is right at every instant: before a send the record says nothing is in flight; from its begin to its
//! commit it names the send; after, the bytes behind it. [`LedgerView`] makes each update as an explicit list of stores
//! ([`Stores`]), which is what lets its oracle stop a writer after every one of them.
//!
//! The ledger is told every send on a held Linux connection, before and after (`crate::nfs`'s `send_ledgered`, the
//! only writer of such a stream): a byte written past it would leave the record behind the stream, and a later partial
//! send could then land on a recorded boundary by chance. A FIN a daemon queues to end a connection counts one byte,
//! and no run of records is one byte (a record's marker alone is four), so a connection ended mid-way is never judged
//! whole.
//!
//! **A start** ([`Handover`]): the successor judges each connection the anchor handed back by the newest ledger handed
//! back, fills a fresh ledger with the connections it serves on (the bytes behind each, nothing in flight), has the
//! anchor hold the fresh one, lets every inherited one go, and only then serves. A death anywhere in that leaves either
//! the inherited ledger alone (nothing was sent since, so it still judges right) or the fresh one as the newest (filled
//! before it was held, nothing sent since). At most two ledgers are held at once: every inherited one but the newest is
//! let go before the fresh one is held.

use std::sync::atomic::{AtomicU64, Ordering};

/// A held connection's send ledger as the daemon wrote it before its last send (A-550): the bytes its replies had put in
/// the kernel whole before that send, and that send's length (zero when none was in flight).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SendLedger {
  /// The bytes of whole replies the kernel took before the send in flight.
  pub(crate) committed: u64,
  /// The length of the send in flight, a whole reply record; zero when none was.
  pub(crate) in_flight: u64,
}

/// How a held connection's stream stands at its adoption ([`resume`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resume {
  /// The send in flight put nothing in the kernel: the successor answers its request afresh.
  Untouched,
  /// The send in flight put all of its reply in the kernel: the successor may send it again, which the client drops.
  Whole,
  /// The kernel holds part of the send in flight, or a count the ledger cannot account for: the stream is torn, so the
  /// connection ends and its client reconnects.
  Torn,
}

/// Judges a held connection at its adoption (A-550): `written`, the bytes the kernel says its writer handed it, against
/// the `ledger` of the send that was in flight. Exactly the bytes before that send, or exactly those and the whole send,
/// leaves the stream on a record's boundary and the connection may be served on; any other count is a torn stream.
pub(crate) fn resume(written: u64, ledger: SendLedger) -> Resume {
  if written == ledger.committed {
    return Resume::Untouched;
  }
  match ledger.committed.checked_add(ledger.in_flight) {
    Some(whole) if ledger.in_flight > 0 && written == whole => Resume::Whole,
    _ => Resume::Torn,
  }
}

/// Why a successor ends a connection the anchor handed back rather than serve it on ([`adoption`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unjudged {
  /// No ledger handed back records it: it was held under none (a ledger the anchor would not hold), or it was being
  /// ended when its daemon died.
  Unrecorded,
  /// The kernel gave no count of the bytes handed (refused, or a kernel without one).
  Uncounted,
  /// Its stream stops inside a send ([`Resume::Torn`]).
  Torn,
}

/// What a successor does with a connection the anchor handed back (A-550).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Adoption {
  /// Serve it on: its stream stands on a record's boundary with `committed` bytes behind it, how [`resume`] said.
  Serve {
    /// The bytes behind the stream: its fresh ledger record starts here, nothing in flight.
    committed: u64,
    /// Which boundary it stands on.
    resume: Resume,
  },
  /// End it: it cannot be judged, or it is torn.
  End(Unjudged),
}

/// Judges a connection the anchor handed back (A-550) from its `record` in the newest ledger handed back and the bytes
/// the kernel says its writer handed it (`written`).
pub(crate) fn adoption(record: Option<SendLedger>, written: Option<u64>) -> Adoption {
  let Some(ledger) = record else {
    return Adoption::End(Unjudged::Unrecorded);
  };
  let Some(written) = written else {
    return Adoption::End(Unjudged::Uncounted);
  };
  match resume(written, ledger) {
    Resume::Torn => Adoption::End(Unjudged::Torn),
    resume => Adoption::Serve {
      committed: written,
      resume,
    },
  }
}

// ============================================================================== the ledger's words

/// Format: a slot's word naming the connection it records, by the connection's id; zero marks the slot free (no
/// connection id is zero: ids are the host's monotonic clock, `crate::nfs::next_connection_id`).
const SLOT_CONNECTION: usize = 0;
/// Format: a slot's word naming the record in force: 0 for the first, 1 for the second.
const SLOT_SELECTOR: usize = 1;
/// Format: the words of a slot's two records, by the selector's value: the bytes sent whole before the send in flight,
/// then that send's length.
const SLOT_RECORDS: [[usize; 2]; 2] = [[2, 3], [4, 5]];
/// Format: the words a slot uses ([`SLOT_CONNECTION`], [`SLOT_SELECTOR`] and [`SLOT_RECORDS`]).
const SLOT_WORDS: usize = 6;
/// Format: the header's word holding how many slots were ever claimed, from the first: how far a successor reads.
const HEADER_CLAIMED: usize = 0;

/// Where a ledger's words live: the shared object a daemon writes, or a test's array (A-550).
pub(crate) trait LedgerWords {
  /// Word `index` of the ledger, or none past its end.
  fn word(&self, index: usize) -> Option<&AtomicU64>;
}

impl LedgerWords for [AtomicU64] {
  fn word(&self, index: usize) -> Option<&AtomicU64> {
    self.get(index)
  }
}

/// A ledger's shape (A-550): its slots, and the words from one slot's start to the next's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
  /// The slots: one per connection the anchor may hold for the daemon.
  pub(crate) slots: usize,
  /// The words from one slot's start to the next's, and the header's: [`SLOT_WORDS`] rounded up to whole cache lines,
  /// so no two slots share a line.
  pub(crate) stride: usize,
}

impl Geometry {
  /// Derived: a ledger of `slots` slots on a machine whose cache line is `cache_line` bytes (the profile's): each slot
  /// spans its six words rounded up to whole lines.
  pub(crate) fn new(slots: usize, cache_line: usize) -> Geometry {
    let line_words = cache_line.div_ceil(size_of::<u64>()).max(1);
    Geometry {
      slots,
      stride: SLOT_WORDS.div_ceil(line_words).saturating_mul(line_words),
    }
  }

  /// The ledger's words in all: one stride of header, then each slot's; none past the address space.
  pub(crate) fn words(self) -> Option<usize> {
    self.slots.checked_add(1)?.checked_mul(self.stride)
  }

  /// The index of word `field` of slot `slot`, or none past the slots or the slot's words.
  fn at(self, slot: usize, field: usize) -> Option<usize> {
    if slot >= self.slots || field >= SLOT_WORDS || self.stride < SLOT_WORDS {
      return None;
    }
    slot
      .checked_add(1)?
      .checked_mul(self.stride)?
      .checked_add(field)
  }
}

/// One word store of a ledger update: the word's index and its value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Store {
  /// The word.
  index: usize,
  /// What is stored.
  value: u64,
}

/// An update's stores, in the order they are made ([`LedgerView::apply`]): the last is the one that makes the update
/// seen (the selector naming a record written whole, or the connection word naming a claimed slot). Held inline, as a
/// slot's words at most, so a send's update allocates nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stores {
  /// The stores, the first [`Stores::len`] of them made.
  stores: [Store; SLOT_WORDS],
  /// How many there are.
  len: usize,
}

impl Stores {
  /// Adds `value` at `index` after the stores already planned; none past a slot's words.
  fn then(mut self, index: usize, value: u64) -> Option<Stores> {
    let next = self.stores.get_mut(self.len)?;
    *next = Store { index, value };
    self.len = self.len.checked_add(1)?;
    Some(self)
  }

  /// The stores, in order.
  pub(crate) fn as_slice(&self) -> &[Store] {
    self.stores.get(..self.len).unwrap_or_default()
  }
}

/// A ledger over its words (A-550): a slot per held connection, each holding the connection's id, two records of its
/// stream (the bytes sent whole before the send in flight, and that send's length) and a selector naming the one in
/// force. A slot is written by one thread at a time (the shard serving its connection; the connection moves between
/// shards as a message), slots are claimed by one thread (the listener's shard), and a successor reads them only once
/// the daemon that wrote them is gone. Each word is reached as an atomic, since another process reads it.
pub(crate) struct LedgerView<'w, W: LedgerWords + ?Sized> {
  words: &'w W,
  geometry: Geometry,
}

impl<'w, W: LedgerWords + ?Sized> LedgerView<'w, W> {
  /// The ledger in `words`, shaped by `geometry`.
  pub(crate) fn new(words: &'w W, geometry: Geometry) -> LedgerView<'w, W> {
    LedgerView { words, geometry }
  }

  /// Word `field` of slot `slot`.
  fn slot_word(&self, slot: usize, field: usize) -> Option<&'w AtomicU64> {
    self.words.word(self.geometry.at(slot, field)?)
  }

  /// The plan of a claim of the lowest free slot for `connection`, its stream standing at `committed` bytes with
  /// nothing in flight: the slot and its stores (the header raised to cover it, the first record and the selector,
  /// then the connection word that makes the slot claimed). None for connection zero (which marks a free slot) or with
  /// every slot taken.
  pub(crate) fn claim_plan(&self, connection: u64, committed: u64) -> Option<(usize, Stores)> {
    if connection == 0 {
      return None;
    }
    let slot = (0..self.geometry.slots).find(|slot| {
      self
        .slot_word(*slot, SLOT_CONNECTION)
        .is_some_and(|word| word.load(Ordering::Acquire) == 0)
    })?;
    let reach = u64::try_from(slot.checked_add(1)?).ok()?;
    let claimed = self.words.word(HEADER_CLAIMED)?.load(Ordering::Acquire);
    let [committed_at, in_flight_at] = *SLOT_RECORDS.first()?;
    let mut stores = Stores::default();
    if claimed < reach {
      stores = stores.then(HEADER_CLAIMED, reach)?;
    }
    let stores = stores
      .then(self.geometry.at(slot, committed_at)?, committed)?
      .then(self.geometry.at(slot, in_flight_at)?, 0)?
      .then(self.geometry.at(slot, SLOT_SELECTOR)?, 0)?
      .then(self.geometry.at(slot, SLOT_CONNECTION)?, connection)?;
    Some((slot, stores))
  }

  /// Claims the lowest free slot for `connection` ([`LedgerView::claim_plan`]): the slot, or none.
  pub(crate) fn claim(&self, connection: u64, committed: u64) -> Option<usize> {
    let (slot, stores) = self.claim_plan(connection, committed)?;
    self.apply(&stores).then_some(slot)
  }

  /// The plan of recording on `slot` that a send of `len` bytes begins: the idle record takes the bytes sent whole so
  /// far and `len`, then the selector names it.
  pub(crate) fn begin_plan(&self, slot: usize, len: u64) -> Option<Stores> {
    let current = self.in_force(slot)?;
    self.write_plan(
      slot,
      SendLedger {
        committed: current.committed,
        in_flight: len,
      },
    )
  }

  /// Records on `slot` that a send of `len` bytes begins ([`LedgerView::begin_plan`]); whether it was recorded.
  pub(crate) fn begin(&self, slot: usize, len: u64) -> bool {
    self
      .begin_plan(slot, len)
      .is_some_and(|stores| self.apply(&stores))
  }

  /// The plan of recording on `slot` that its send in flight is wholly in the kernel: the idle record takes the bytes
  /// behind the stream now, nothing in flight, then the selector names it. None where the sum would pass the counter:
  /// the record naming the send stays in force, and a successor judges it torn.
  pub(crate) fn commit_plan(&self, slot: usize) -> Option<Stores> {
    let current = self.in_force(slot)?;
    self.write_plan(
      slot,
      SendLedger {
        committed: current.committed.checked_add(current.in_flight)?,
        in_flight: 0,
      },
    )
  }

  /// Records on `slot` that its send in flight is wholly in the kernel ([`LedgerView::commit_plan`]); whether it was.
  pub(crate) fn commit(&self, slot: usize) -> bool {
    self
      .commit_plan(slot)
      .is_some_and(|stores| self.apply(&stores))
  }

  /// Frees `slot`: its connection word goes back to zero.
  pub(crate) fn free(&self, slot: usize) {
    if let Some(word) = self.slot_word(slot, SLOT_CONNECTION) {
      word.store(0, Ordering::Release);
    }
  }

  /// The record in force for `connection`, read by a successor once the daemon that wrote it is gone: the first slot of
  /// those ever claimed that names it, or none.
  pub(crate) fn find(&self, connection: u64) -> Option<SendLedger> {
    if connection == 0 {
      return None;
    }
    let claimed = self.words.word(HEADER_CLAIMED)?.load(Ordering::Acquire);
    let claimed = usize::try_from(claimed).ok()?.min(self.geometry.slots);
    let slot = (0..claimed).find(|slot| {
      self
        .slot_word(*slot, SLOT_CONNECTION)
        .is_some_and(|word| word.load(Ordering::Acquire) == connection)
    })?;
    self.in_force(slot)
  }

  /// Makes `stores` in order, each a release so the last publishes every one before it; whether every word was there.
  pub(crate) fn apply(&self, stores: &Stores) -> bool {
    stores
      .as_slice()
      .iter()
      .all(|store| match self.words.word(store.index) {
        Some(word) => {
          word.store(store.value, Ordering::Release);
          true
        }
        None => false,
      })
  }

  /// The record `slot`'s selector names, or none for a selector this ledger never writes.
  fn in_force(&self, slot: usize) -> Option<SendLedger> {
    let selector = self.slot_word(slot, SLOT_SELECTOR)?.load(Ordering::Acquire);
    let [committed_at, in_flight_at] = *SLOT_RECORDS.get(usize::try_from(selector).ok()?)?;
    Some(SendLedger {
      committed: self.slot_word(slot, committed_at)?.load(Ordering::Acquire),
      in_flight: self.slot_word(slot, in_flight_at)?.load(Ordering::Acquire),
    })
  }

  /// The plan of writing `ledger` into `slot`'s idle record, then naming it.
  fn write_plan(&self, slot: usize, ledger: SendLedger) -> Option<Stores> {
    let selector = self.slot_word(slot, SLOT_SELECTOR)?.load(Ordering::Acquire);
    let idle: u64 = match selector {
      0 => 1,
      1 => 0,
      _ => return None,
    };
    let [committed_at, in_flight_at] = *SLOT_RECORDS.get(usize::try_from(idle).ok()?)?;
    Stores::default()
      .then(self.geometry.at(slot, committed_at)?, ledger.committed)?
      .then(self.geometry.at(slot, in_flight_at)?, ledger.in_flight)?
      .then(self.geometry.at(slot, SLOT_SELECTOR)?, idle)
  }
}

// ============================================================================== the ledger's record, held by the anchor

/// Format: the tag a held ledger's record opens with, "NFSLEDG" and version 1. A DAX object's record is three words
/// long and opens with its own tag, a writable map's copy's is two words with none (`crate::shards::dax_hold`,
/// `crate::held_copy`); this one is four, so no record of one kind reads as another's.
const LEDGER_TAG: u64 = u64::from_le_bytes(*b"NFSLEDG\x01");
/// Format: a held ledger's record ([`LedgerRecord`]): the tag, the generation, the slots and the stride, each a
/// little-endian word.
const LEDGER_RECORD_BYTES: usize = 32;
/// Format: the top sixteen bits of a ledger's copy id, all ones. Every other copy's id is an encoded handle, whose top
/// sixteen bits are its shard's partition (`slates_mem::Encoded`), and a partition is below 65,535, since the shards
/// are counted in a `u16`: the anchor keeps every kind in one table by id, and no ledger's id meets another's.
const LEDGER_ID_MARK: u64 = 0xFFFF_0000_0000_0000;
/// Format: the bits of a ledger's copy id below [`LEDGER_ID_MARK`]: its generation's low bits, so two ledgers held at
/// once (consecutive generations) never share an id.
const LEDGER_GENERATION_BITS: u64 = 0x0000_FFFF_FFFF_FFFF;

/// A held ledger's record, what the anchor keeps beside its object (A-550): which ledger is newest, and its shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LedgerRecord {
  /// One past the newest generation it was made after; the newest held judges a successor's connections.
  pub(crate) generation: u64,
  /// Its shape.
  pub(crate) geometry: Geometry,
}

impl LedgerRecord {
  /// Its bytes ([`LEDGER_RECORD_BYTES`]).
  pub(crate) fn encode(self) -> [u8; LEDGER_RECORD_BYTES] {
    let mut bytes = [0u8; LEDGER_RECORD_BYTES];
    let words = [
      LEDGER_TAG,
      self.generation,
      u64::try_from(self.geometry.slots).unwrap_or(u64::MAX),
      u64::try_from(self.geometry.stride).unwrap_or(u64::MAX),
    ]
    .map(u64::to_le_bytes);
    for (slot, byte) in bytes.iter_mut().zip(words.iter().flatten()) {
      *slot = *byte;
    }
    bytes
  }

  /// The record `bytes` spell, or none for bytes of another length or tag (another kind's record, or damage), or a
  /// shape that has no ledger (a stride shorter than a slot's words, or more words than the address space holds). The
  /// anchor's bytes are external input.
  pub(crate) fn decode(bytes: &[u8]) -> Option<LedgerRecord> {
    let whole = <&[u8; LEDGER_RECORD_BYTES]>::try_from(bytes).ok()?;
    let (words, _) = whole.as_chunks::<{ size_of::<u64>() }>();
    // Shape: a ledger record's four words, its tag, generation, slot count and stride.
    let [tag, generation, slots, stride] = <[[u8; size_of::<u64>()]; 4]>::try_from(words)
      .ok()?
      .map(u64::from_le_bytes);
    if tag != LEDGER_TAG {
      return None;
    }
    let geometry = Geometry {
      slots: usize::try_from(slots).ok()?,
      stride: usize::try_from(stride).ok()?,
    };
    if geometry.stride < SLOT_WORDS {
      return None;
    }
    geometry.words()?;
    Some(LedgerRecord {
      generation,
      geometry,
    })
  }

  /// Its copy id: [`LEDGER_ID_MARK`] and its generation's low bits.
  pub(crate) fn copy_id(self) -> u64 {
    LEDGER_ID_MARK | (self.generation & LEDGER_GENERATION_BITS)
  }

  /// Its object's bytes; none past the address space.
  pub(crate) fn object_bytes(self) -> Option<usize> {
    self.geometry.words()?.checked_mul(size_of::<u64>())
  }
}

/// The index of the newest of `records`, the one a successor judges by: the highest generation, the first of equals.
pub(crate) fn newest(records: &[LedgerRecord]) -> Option<usize> {
  records
    .iter()
    .enumerate()
    .rev()
    .max_by_key(|(_, record)| record.generation)
    .map(|(index, _)| index)
}

/// The generation of a ledger made after `records`: one past the newest, so it is newest itself; the first is one.
pub(crate) fn next_generation(records: &[LedgerRecord]) -> u64 {
  records
    .iter()
    .map(|record| record.generation)
    .max()
    .unwrap_or(0)
    .saturating_add(1)
}

// ============================================================================== this daemon's ledger (Linux)

/// Counter: a connection a successor served on with nothing of its send in flight in the kernel (A-550), one of the
/// judgment's two non-vacuity counters.
const SERVED_UNTOUCHED: &str = "nfs.held_send.served_untouched";
/// Counter: a connection a successor served on with all of its send in flight in the kernel, whose reply goes again
/// and is dropped by its client (A-550), the other.
const SERVED_WHOLE: &str = "nfs.held_send.served_whole";
/// Counter: a handed-back connection ended because no ledger handed back records it.
const ENDED_UNRECORDED: &str = "nfs.held_send.ended_unrecorded";
/// Counter: a handed-back connection ended because the kernel gave no count of its bytes.
const ENDED_UNCOUNTED: &str = "nfs.held_send.ended_uncounted";
/// Counter: a handed-back connection ended because its stream stops inside a send.
const ENDED_TORN: &str = "nfs.held_send.ended_torn";
/// The status refusal count of ledgers this daemon could not make or the anchor would not hold: its Linux connections
/// are then served unheld, as before A-550.
const LEDGER_REFUSED: &str = "nfs.held_send.ledger_refused";
/// The status refusal count of inherited ledgers that could not be read (a record or an object that does not match):
/// every connection they recorded ends, unrecorded.
const INHERITED_REFUSED: &str = "nfs.held_send.inherited_refused";

/// Counts `counter` once on this shard.
fn note(counter: &'static str) {
  let _ = crate::state::with_state_counted(|s| s.count(counter, 1));
}

/// The counter of an ended connection's reason.
fn ended_counter(why: Unjudged) -> &'static str {
  match why {
    Unjudged::Unrecorded => ENDED_UNRECORDED,
    Unjudged::Uncounted => ENDED_UNCOUNTED,
    Unjudged::Torn => ENDED_TORN,
  }
}

#[cfg(target_os = "linux")]
mod linux {
  use std::os::fd::OwnedFd;
  use std::sync::OnceLock;
  use std::sync::atomic::AtomicU64;

  use slates_mem::{RunId, SharedObject, Width, WordRun, Words};

  use super::{Geometry, LedgerRecord, LedgerView, LedgerWords};

  /// Format: the label of a ledger's memory object (Linux names a `memfd` for its holders' listings only).
  const LEDGER_LABEL: &str = "slates-nfs-send-ledger";

  /// The ledger this daemon writes (A-550): set once, when its NFS listener starts and the anchor holds it; read by
  /// every shard that sends on a held connection.
  pub(super) static LEDGER: OnceLock<Ledger> = OnceLock::new();

  /// A ledger in a shared memory object the anchor holds (A-550): its object, the run its words are reached through,
  /// and its record.
  pub(crate) struct Ledger {
    object: SharedObject,
    run: RunId,
    pub(super) record: LedgerRecord,
  }

  impl LedgerWords for Ledger {
    fn word(&self, index: usize) -> Option<&AtomicU64> {
      self.object.run_u64(self.run, index).ok()
    }
  }

  impl Ledger {
    /// The object's bytes and the run declaring its words, every word a 64-bit atomic.
    fn layout(record: LedgerRecord) -> Option<(usize, WordRun)> {
      let words = record.geometry.words()?;
      let bytes = record.object_bytes()?;
      Some((
        bytes,
        WordRun::strided(0, size_of::<u64>(), words, Width::U64),
      ))
    }

    /// A fresh ledger shaped by `record`: every word zero, so every slot is free.
    pub(crate) fn create(record: LedgerRecord) -> Option<Ledger> {
      let (bytes, run) = Self::layout(record)?;
      let object = SharedObject::create(LEDGER_LABEL, bytes, Words::new().with(run)).ok()?;
      let run = object.resolve(&run).ok()?;
      Some(Ledger {
        object,
        run,
        record,
      })
    }

    /// The ledger a dead daemon left, handed back by the anchor as `object` with `record`: refused unless the object is
    /// exactly the record's size, since a smaller one would fault where the record says words are.
    pub(crate) fn adopt(record: LedgerRecord, object: OwnedFd) -> Option<Ledger> {
      let (bytes, run) = Self::layout(record)?;
      let size = rustix::fs::fstat(&object).ok()?.st_size;
      if u64::try_from(size).ok()? != u64::try_from(bytes).ok()? {
        return None;
      }
      let object = SharedObject::from_descriptor(object, bytes, Words::new().with(run)).ok()?;
      let run = object.resolve(&run).ok()?;
      Some(Ledger {
        object,
        run,
        record,
      })
    }

    /// Its slots and records.
    pub(crate) fn view(&self) -> LedgerView<'_, Ledger> {
      LedgerView::new(self, self.record.geometry)
    }

    /// Its object's descriptor, lent: what the anchor is handed to hold.
    pub(super) fn object_fd(&self) -> std::os::fd::BorrowedFd<'_> {
      self.object.as_fd()
    }

    /// Asks the anchor to hold it; whether the anchor does.
    pub(crate) fn hold(&self) -> bool {
      matches!(
        crate::anchor_hold::hold_copy(
          self.record.copy_id(),
          &self.record.encode(),
          self.object_fd(),
        ),
        Some(Ok(()))
      )
    }
  }

  /// A ledger the anchor handed back: its record and object.
  #[derive(Debug)]
  pub(crate) struct HandedLedger {
    pub(super) record: LedgerRecord,
    pub(super) object: OwnedFd,
  }

  /// The geometry a ledger is made with for this daemon: `slots` connections on a machine with this cache line.
  pub(super) fn geometry(slots: usize, cache_line: usize) -> Geometry {
    Geometry::new(slots, cache_line)
  }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::HandedLedger;

/// The ledgers the anchor handed back (A-550): Linux alone keeps any.
#[derive(Debug, Default)]
pub(crate) struct Inherited {
  /// Each ledger's record and object.
  #[cfg(target_os = "linux")]
  ledgers: Vec<HandedLedger>,
}

/// Splits the ledgers out of the copies the anchor handed back (A-550): every copy whose record is a ledger's; the
/// rest returned for their own kinds.
#[cfg(target_os = "linux")]
pub(crate) fn split_ledgers(
  handed: Vec<crate::anchor_hold::HandedCopy>,
) -> (Vec<crate::anchor_hold::HandedCopy>, Inherited) {
  let mut others = Vec::new();
  let mut inherited = Inherited::default();
  for copy in handed {
    match LedgerRecord::decode(&copy.record) {
      Some(record) if record.copy_id() == copy.copy => inherited.ledgers.push(HandedLedger {
        record,
        object: copy.object,
      }),
      _ => others.push(copy),
    }
  }
  (others, inherited)
}

impl Inherited {
  /// Lets every inherited ledger go (a daemon with no listener serves none of their connections).
  pub(crate) fn release(self) {
    #[cfg(target_os = "linux")]
    for ledger in self.ledgers {
      let _sent = crate::anchor_hold::release_copy(ledger.record.copy_id());
    }
  }
}

/// What a start does with one connection the anchor handed back ([`Handover::judge`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
  /// Serve it on, its fresh ledger slot (none where no ledger is kept: a platform whose sends are whole, A-113).
  Serve(Option<usize>),
  /// End it: its client reconnects (why is counted, [`Unjudged`]).
  End,
}

/// A start's ledger handover (A-550; the module's "A start"): the newest inherited ledger, read to judge the
/// connections the anchor handed back, and this daemon's fresh one, filled with the connections it serves on before
/// the anchor holds it.
pub(crate) struct Handover {
  /// The newest inherited ledger, mapped.
  #[cfg(target_os = "linux")]
  newest: Option<linux::Ledger>,
  /// The copy ids of the inherited ledgers still held: let go once the fresh one is.
  #[cfg(target_os = "linux")]
  inherited: Vec<u64>,
  /// This daemon's ledger, filled before it is held.
  #[cfg(target_os = "linux")]
  fresh: Option<linux::Ledger>,
}

impl Handover {
  /// Begins the handover over `inherited` for a daemon whose anchor holds up to `slots` connections on a machine whose
  /// cache line is `cache_line` bytes: every inherited ledger but the newest is let go at once (only the newest
  /// judges), the newest is mapped, and a fresh ledger is made after it. Under no anchor nothing is kept.
  pub(crate) fn start(inherited: Inherited, slots: usize, cache_line: usize) -> Handover {
    #[cfg(target_os = "linux")]
    {
      let records: Vec<LedgerRecord> = inherited
        .ledgers
        .iter()
        .map(|ledger| ledger.record)
        .collect();
      let keep = newest(&records);
      let generation = next_generation(&records);
      let mut newest_ledger = None;
      let mut held = Vec::new();
      for (index, ledger) in inherited.ledgers.into_iter().enumerate() {
        let copy = ledger.record.copy_id();
        if Some(index) != keep {
          let _sent = crate::anchor_hold::release_copy(copy);
          continue;
        }
        held.push(copy);
        newest_ledger = linux::Ledger::adopt(ledger.record, ledger.object);
        if newest_ledger.is_none() {
          crate::fleet::count_refusal(INHERITED_REFUSED);
        }
      }
      let fresh = if crate::anchor_hold::anchored() {
        let made = linux::Ledger::create(LedgerRecord {
          generation,
          geometry: linux::geometry(slots, cache_line),
        });
        if made.is_none() {
          crate::fleet::count_refusal(LEDGER_REFUSED);
        }
        made
      } else {
        None
      };
      Handover {
        newest: newest_ledger,
        inherited: held,
        fresh,
      }
    }
    #[cfg(not(target_os = "linux"))]
    {
      let _ = (inherited, slots, cache_line);
      Handover {}
    }
  }

  /// Judges the connection `connection` the anchor handed back, served by `stream` (A-550). On Linux: by its record in
  /// the newest inherited ledger and the kernel's count of the bytes handed ([`adoption`]); served on, it takes a slot
  /// in the fresh ledger at the bytes behind it. Elsewhere it is served on (its sends were whole, A-113).
  pub(crate) fn judge(&self, connection: u64, stream: &slates_rt::tcp::TcpStream) -> Verdict {
    #[cfg(target_os = "linux")]
    {
      let record = self
        .newest
        .as_ref()
        .and_then(|ledger| ledger.view().find(connection));
      let written = stream.bytes_handed().ok().flatten();
      match adoption(record, written) {
        Adoption::Serve { committed, resume } => {
          note(match resume {
            Resume::Whole => SERVED_WHOLE,
            Resume::Untouched | Resume::Torn => SERVED_UNTOUCHED,
          });
          Verdict::Serve(
            self
              .fresh
              .as_ref()
              .and_then(|ledger| ledger.view().claim(connection, committed)),
          )
        }
        Adoption::End(why) => {
          note(ended_counter(why));
          Verdict::End
        }
      }
    }
    #[cfg(not(target_os = "linux"))]
    {
      let _ = (connection, stream);
      Verdict::Serve(None)
    }
  }

  /// Ends the handover: the anchor holds the fresh ledger, then every inherited one is let go, and the fresh one becomes
  /// the ledger this daemon writes. A fresh ledger the anchor would not hold is dropped, and this daemon holds no Linux
  /// connection it accepts (its adopted ones stay held unrecorded, so a successor ends them).
  pub(crate) fn finish(self) {
    #[cfg(target_os = "linux")]
    {
      // A start hands over once, so the ledger is set once; a second would be refused and counted, never kept unheld.
      if let Some(fresh) = self.fresh
        && !(fresh.hold() && linux::LEDGER.set(fresh).is_ok())
      {
        crate::fleet::count_refusal(LEDGER_REFUSED);
      }
      for copy in self.inherited {
        let _sent = crate::anchor_hold::release_copy(copy);
      }
    }
  }
}

/// Whether this daemon keeps a ledger, so a Linux connection it accepts may be held (A-550).
pub(crate) fn kept() -> bool {
  #[cfg(target_os = "linux")]
  {
    linux::LEDGER.get().is_some()
  }
  #[cfg(not(target_os = "linux"))]
  {
    false
  }
}

/// Claims a slot of this daemon's ledger for `connection`, accepted with nothing written to it (A-550): the slot, or
/// none where no ledger is kept or every slot is taken. Called on the listener's shard alone.
pub(crate) fn claim(connection: u64) -> Option<usize> {
  #[cfg(target_os = "linux")]
  {
    linux::LEDGER.get()?.view().claim(connection, 0)
  }
  #[cfg(not(target_os = "linux"))]
  {
    let _ = connection;
    None
  }
}

/// The status refusal count of sends a held connection's ledger could not record (a length or a sum past the counter):
/// the record before stays in force, so a successor that finds the stream moved judges it torn, never whole.
const UNRECORDED_SEND: &str = "nfs.held_send.unrecorded_send";

/// Records that a send of `len` bytes begins on the held connection whose slot is `slot` (A-550). Nothing without a
/// slot or a ledger. A send it could not record is counted ([`UNRECORDED_SEND`]).
pub(crate) fn begin(slot: Option<usize>, len: usize) {
  #[cfg(target_os = "linux")]
  {
    if let (Some(slot), Some(ledger)) = (slot, linux::LEDGER.get())
      && !u64::try_from(len).is_ok_and(|len| ledger.view().begin(slot, len))
    {
      crate::fleet::count_refusal(UNRECORDED_SEND);
    }
  }
  #[cfg(not(target_os = "linux"))]
  {
    let _ = (slot, len);
  }
}

/// Records that the send in flight on the held connection whose slot is `slot` is wholly in the kernel (A-550). A
/// commit it could not record is counted ([`UNRECORDED_SEND`]).
pub(crate) fn commit(slot: Option<usize>) {
  #[cfg(target_os = "linux")]
  {
    if let (Some(slot), Some(ledger)) = (slot, linux::LEDGER.get())
      && !ledger.view().commit(slot)
    {
      crate::fleet::count_refusal(UNRECORDED_SEND);
    }
  }
  #[cfg(not(target_os = "linux"))]
  {
    let _ = slot;
  }
}

/// Frees the slot `slot` of a held connection that ended (A-550).
pub(crate) fn free(slot: Option<usize>) {
  #[cfg(target_os = "linux")]
  {
    if let (Some(slot), Some(ledger)) = (slot, linux::LEDGER.get()) {
      ledger.view().free(slot);
    }
  }
  #[cfg(not(target_os = "linux"))]
  {
    let _ = slot;
  }
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicU64, Ordering};

  use super::{
    Adoption, Geometry, LEDGER_RECORD_BYTES, LedgerRecord, LedgerView, Resume, SLOT_WORDS,
    SendLedger, Stores, Unjudged, adoption, newest, next_generation, resume,
  };

  /// §4.6, A-113's Linux half, A-550: a held connection is served on only when its stream stands on a record's boundary.
  /// Do: judge a connection whose kernel holds the bytes before the send in flight, those and the whole send, part of
  /// it, fewer bytes than the ledger committed, more than the send, a ledger with nothing in flight, and one whose sum
  /// would overflow. Expect `Untouched` and `Whole` on the two boundaries and `Torn` everywhere else. Until A-550 no Linux
  /// connection was judged: none was held, so every restart dropped every Linux client's connection.
  #[test]
  fn a_held_connection_is_served_on_only_on_a_records_boundary() {
    let sending = SendLedger {
      committed: 1_000,
      in_flight: 300,
    };
    assert_eq!(
      resume(1_000, sending),
      Resume::Untouched,
      "nothing of the send"
    );
    assert_eq!(resume(1_300, sending), Resume::Whole, "the whole send");
    assert_eq!(resume(1_150, sending), Resume::Torn, "part of the send");
    assert_eq!(resume(999, sending), Resume::Torn, "fewer than committed");
    assert_eq!(resume(1_301, sending), Resume::Torn, "more than the send");
    let idle = SendLedger {
      committed: 1_000,
      in_flight: 0,
    };
    assert_eq!(resume(1_000, idle), Resume::Untouched, "nothing in flight");
    assert_eq!(
      resume(1_001, idle),
      Resume::Torn,
      "bytes no send accounts for"
    );
    let overflowing = SendLedger {
      committed: u64::MAX,
      in_flight: 1,
    };
    assert_eq!(
      resume(0, overflowing),
      Resume::Torn,
      "a sum past the counter's range"
    );
  }

  /// The words of a ledger of `geometry`, every one zero.
  fn words(geometry: Geometry) -> Vec<AtomicU64> {
    (0..geometry.words().unwrap())
      .map(|_| AtomicU64::new(0))
      .collect()
  }

  /// A copy of `words`, as a successor would find them.
  fn snapshot(words: &[AtomicU64]) -> Vec<AtomicU64> {
    words
      .iter()
      .map(|word| AtomicU64::new(word.load(Ordering::Acquire)))
      .collect()
  }

  /// What a successor judging a ledger stopped after the stores so far should do, with the kernel's count at that
  /// moment, for a stream whose earlier sends total `behind` and whose send of `len` bytes the kernel has taken `taken`
  /// of; `claimed` says whether the claim's last store was made.
  fn expected(claimed: bool, behind: u64, len: u64, taken: u64) -> Adoption {
    if !claimed {
      return Adoption::End(Unjudged::Unrecorded);
    }
    if taken == 0 {
      return Adoption::Serve {
        committed: behind,
        resume: Resume::Untouched,
      };
    }
    if taken == len {
      return Adoption::Serve {
        committed: behind + len,
        resume: Resume::Whole,
      };
    }
    Adoption::End(Unjudged::Torn)
  }

  /// Applies `stores` one at a time to `words`, judging `connection` after each as a successor would (with `written`
  /// bytes handed), and checks each judgment's kind against `expected` (the boundary a whole record names may be either
  /// of a send's two, which a successor serves on alike).
  fn judge_each_store(
    words: &[AtomicU64],
    geometry: Geometry,
    connection: u64,
    stores: &Stores,
    written: u64,
    expect: impl Fn(usize) -> Adoption,
    judged: &mut usize,
  ) {
    for (made, store) in stores.as_slice().iter().enumerate() {
      words[store.index].store(store.value, Ordering::Release);
      let seen = snapshot(words);
      let view = LedgerView::new(seen.as_slice(), geometry);
      let verdict = adoption(view.find(connection), Some(written));
      let wanted = expect(made + 1);
      match (verdict, wanted) {
        (
          Adoption::Serve { committed, .. },
          Adoption::Serve {
            committed: want, ..
          },
        ) => {
          assert_eq!(committed, want, "store {made}: served at the bytes handed")
        }
        (got, want) => assert_eq!(got, want, "store {made}"),
      }
      *judged += 1;
    }
  }

  /// §4.6, A-113's Linux half, A-550: a successor judges a held connection rightly whatever store its writer died after.
  /// Do: claim a slot and send runs of 5, 1, 7 and 300 bytes, each taken by the kernel whole or in pieces, stopping the
  /// writer after every store of every update and after every piece the kernel takes; judge the connection from a copy
  /// of the words and the kernel's count at that moment. Expect: unrecorded until the claim's last store; served on, at
  /// exactly the bytes handed, whenever nothing or all of the send in flight is in the kernel; torn whenever part is.
  /// A single record written in place could be read half old and half new; one written whole and then named never is.
  #[test]
  fn a_successor_judges_a_writer_stopped_after_any_store_rightly() {
    let geometry = Geometry::new(4, 64);
    let words = words(geometry);
    let ledger = LedgerView::new(words.as_slice(), geometry);
    let connection = 0x1234_5678;
    let mut judged = 0;
    let (slot, stores) = ledger.claim_plan(connection, 0).unwrap();
    judge_each_store(
      &words,
      geometry,
      connection,
      &stores,
      0,
      |made| expected(made == stores.as_slice().len(), 0, 0, 0),
      &mut judged,
    );
    let sends: [(u64, &[u64]); 4] = [(5, &[5]), (1, &[1]), (7, &[3, 4]), (300, &[1, 200, 99])];
    let mut behind = 0;
    for (len, pieces) in sends {
      let begin = ledger.begin_plan(slot, len).unwrap();
      judge_each_store(
        &words,
        geometry,
        connection,
        &begin,
        behind,
        |_| expected(true, behind, len, 0),
        &mut judged,
      );
      let mut taken = 0;
      for piece in pieces {
        taken += piece;
        let seen = snapshot(&words);
        let view = LedgerView::new(seen.as_slice(), geometry);
        let verdict = adoption(view.find(connection), Some(behind + taken));
        match expected(true, behind, len, taken) {
          Adoption::Serve { committed, .. } => assert!(
            matches!(verdict, Adoption::Serve { committed: got, .. } if got == committed),
            "{verdict:?}"
          ),
          wanted => assert_eq!(verdict, wanted),
        }
        judged += 1;
      }
      let commit = ledger.commit_plan(slot).unwrap();
      judge_each_store(
        &words,
        geometry,
        connection,
        &commit,
        behind + len,
        |_| expected(true, behind, len, len),
        &mut judged,
      );
      behind += len;
    }
    assert_eq!(
      ledger.find(connection),
      Some(SendLedger {
        committed: behind,
        in_flight: 0
      })
    );
    assert!(judged > 30, "the oracle judged {judged} moments");
  }

  /// §4.6, A-550: a slot is claimed lowest first, freed for the next connection, and found only by its own connection,
  /// within the slots ever claimed; no two slots share a cache line. Do: claim three slots, free the middle, claim
  /// again; find each connection, a freed one and one never claimed; fill every slot and claim once more. Expect slots
  /// 0, 1, 2, then 1 again; each found at its own bytes; the freed and the stranger not found; a full ledger refusing;
  /// and every slot's first word a whole number of 64- and 128-byte lines from the next's.
  #[test]
  fn slots_are_claimed_lowest_first_and_found_only_by_their_connection() {
    for line in [64, 128] {
      let geometry = Geometry::new(4, line);
      assert!(geometry.stride >= SLOT_WORDS);
      assert_eq!((geometry.stride * size_of::<u64>()) % line, 0, "{line}");
      let words = words(geometry);
      let ledger = LedgerView::new(words.as_slice(), geometry);
      assert_eq!(ledger.claim(11, 100), Some(0));
      assert_eq!(ledger.claim(22, 200), Some(1));
      assert_eq!(ledger.claim(33, 300), Some(2));
      ledger.free(1);
      assert_eq!(ledger.find(22), None, "freed");
      assert_eq!(ledger.claim(44, 400), Some(1), "the lowest free");
      assert_eq!(ledger.claim(0, 0), None, "zero marks a free slot");
      for (connection, committed) in [(11, 100), (44, 400), (33, 300)] {
        assert_eq!(
          ledger.find(connection),
          Some(SendLedger {
            committed,
            in_flight: 0
          })
        );
      }
      assert_eq!(ledger.find(99), None, "never claimed");
      assert_eq!(ledger.claim(55, 0), Some(3));
      assert_eq!(ledger.claim(66, 0), None, "full");
    }
  }

  /// A-550, golden vector: a ledger's record is four little-endian words, tag first, and no record of another kind or
  /// shape reads as one. Do: encode a record and compare it byte for byte; decode it back; decode it with its tag, its
  /// length or its stride wrong, and the 16- and 24-byte records of the other kinds. Expect the exact bytes, the same
  /// record back, and none for every other.
  #[test]
  fn a_ledger_record_reads_back_and_nothing_else_reads_as_one() {
    let record = LedgerRecord {
      generation: 7,
      geometry: Geometry {
        slots: 4096,
        stride: 16,
      },
    };
    let mut golden = Vec::new();
    golden.extend_from_slice(b"NFSLEDG\x01");
    golden.extend_from_slice(&7u64.to_le_bytes());
    golden.extend_from_slice(&4096u64.to_le_bytes());
    golden.extend_from_slice(&16u64.to_le_bytes());
    assert_eq!(record.encode().as_slice(), golden.as_slice());
    assert_eq!(LedgerRecord::decode(&golden), Some(record));
    let mut tagged = golden.clone();
    tagged[0] ^= 1;
    assert_eq!(LedgerRecord::decode(&tagged), None, "another tag");
    assert_eq!(
      LedgerRecord::decode(&golden[..LEDGER_RECORD_BYTES - 1]),
      None
    );
    let mut narrow = golden.clone();
    narrow[24..].copy_from_slice(&5u64.to_le_bytes());
    assert_eq!(LedgerRecord::decode(&narrow), None, "a stride under a slot");
    assert_eq!(
      LedgerRecord::decode(&golden[..16]),
      None,
      "a writable copy's"
    );
    assert_eq!(LedgerRecord::decode(&golden[..24]), None, "a DAX object's");
  }

  /// A-550: two ledgers held at once never share a copy id, and no ledger's id can be another kind's. Do: take the ids of
  /// consecutive generations, including across the 48-bit wrap. Expect distinct ids whose top sixteen bits are all
  /// ones, which no encoded handle's partition (a `u16` below 65,535) reaches.
  #[test]
  fn ledger_copy_ids_are_their_own() {
    let id = |generation| {
      LedgerRecord {
        generation,
        geometry: Geometry {
          slots: 1,
          stride: SLOT_WORDS,
        },
      }
      .copy_id()
    };
    for generation in [1, 2, 0xFFFF_FFFF_FFFF, 0x1_0000_0000_0000] {
      assert_eq!(id(generation) >> 48, 0xFFFF);
      assert_ne!(id(generation), id(generation + 1));
    }
  }

  /// A-550: a start judges by the newest ledger handed back and makes its own newer still. Do: pick the newest of none,
  /// one, and three out of order, and the next generation after each. Expect none and generation 1; the one and 2; the
  /// highest and one past it.
  #[test]
  fn the_newest_ledger_judges_and_the_next_is_newer() {
    let record = |generation| LedgerRecord {
      generation,
      geometry: Geometry {
        slots: 1,
        stride: SLOT_WORDS,
      },
    };
    assert_eq!(newest(&[]), None);
    assert_eq!(next_generation(&[]), 1);
    assert_eq!(newest(&[record(1)]), Some(0));
    assert_eq!(next_generation(&[record(1)]), 2);
    let three = [record(4), record(9), record(2)];
    assert_eq!(newest(&three), Some(1));
    assert_eq!(next_generation(&three), 10);
  }

  /// A-550: what a successor does with each handed-back connection. Do: judge one no ledger records, one the kernel gave
  /// no count for, one torn, and one on each boundary. Expect it ended for the first three, each with its reason, and
  /// served on at exactly the bytes handed for the two boundaries.
  #[test]
  fn a_handed_back_connection_is_served_on_or_ended_with_its_reason() {
    let sending = SendLedger {
      committed: 40,
      in_flight: 8,
    };
    assert_eq!(
      adoption(None, Some(40)),
      Adoption::End(Unjudged::Unrecorded)
    );
    assert_eq!(
      adoption(Some(sending), None),
      Adoption::End(Unjudged::Uncounted)
    );
    assert_eq!(
      adoption(Some(sending), Some(44)),
      Adoption::End(Unjudged::Torn)
    );
    assert_eq!(
      adoption(Some(sending), Some(40)),
      Adoption::Serve {
        committed: 40,
        resume: Resume::Untouched
      }
    );
    assert_eq!(
      adoption(Some(sending), Some(48)),
      Adoption::Serve {
        committed: 48,
        resume: Resume::Whole
      }
    );
  }

  /// A-550, Linux: a ledger's object outlives the daemon that wrote it. Do: make a ledger, claim a slot, begin a send
  /// and commit it, begin another; hand a duplicate of the object to a successor and drop the writer; adopt it with its
  /// record, and with a record of another size. Expect the successor to read the second send in flight behind the
  /// first, and the mismatched record refused.
  #[cfg(target_os = "linux")]
  #[test]
  fn a_ledger_object_outlives_its_writer() {
    use super::linux::Ledger;
    let record = LedgerRecord {
      generation: 3,
      geometry: Geometry::new(8, 64),
    };
    let writer = Ledger::create(record).unwrap();
    let slot = writer.view().claim(77, 0).unwrap();
    assert!(writer.view().begin(slot, 120));
    assert!(writer.view().commit(slot));
    assert!(writer.view().begin(slot, 64));
    let copy = |ledger: &Ledger| rustix::io::fcntl_dupfd_cloexec(ledger.object_fd(), 0).unwrap();
    let handed = copy(&writer);
    let mismatched = copy(&writer);
    drop(writer);
    let successor = Ledger::adopt(record, handed).unwrap();
    assert_eq!(
      successor.view().find(77),
      Some(SendLedger {
        committed: 120,
        in_flight: 64
      })
    );
    let other = LedgerRecord {
      generation: 3,
      geometry: Geometry::new(9, 64),
    };
    assert!(Ledger::adopt(other, mismatched).is_none(), "another size");
  }
}

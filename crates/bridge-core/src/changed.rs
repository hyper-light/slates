//! The data changes a volume's journal records, as a transport consumes them to know what its kernel holds stale (§4.6
//! "Cache posture"; `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`).
//!
//! **Why a transport needs them.** A FUSE kernel keeps a page a process dirtied through a shared map until it writes
//! the page back: periodically, at `msync`, or while it obeys an invalidation, since it launders a dirty page before it
//! drops it (Linux 6.12 `mm/truncate.c` → `fuse_launder_folio`). That page is the kernel's copy as fetched, so its bytes
//! are stale exactly where another path changed the file since; applied whole, it undid the later write (on 2026-10-21,
//! 4,095 of a page's 4,096 bytes reverted on Linux, 16,383 of 16,384 on macOS). The transport keeps, for every byte of a
//! file open for writing, the journal point its kernel's copy is as of and whether another path changed it since, fed
//! by these changes as it consumes the journal (`slates-bridge-fuse`'s `PageStates`).
//!
//! **What each record changes.** An overwrite or an extension, its range; a truncation, everything from the new
//! length; an insertion or a deletion, everything from its offset, since the rest shifted; a drift, the whole file. A
//! record of metadata or of the namespace changes no bytes, and a witness (a copy-up) copies the bytes the kernel
//! already held.
//!
//! **Why consumed, not walked back.** A first version walked the journal back from each page's fetch point at
//! write-back. The journal keeps only what its live readers have yet to read (A-157; it then kept 10‰ of the quota,
//! some 850 records for an 8 MiB volume), so a long-lived map — exactly DAX — outlived its records and its write-backs
//! were refused: measured 2026-10-21, 2 runs of 3. Consumed as they arrive, as the transport already consumes the
//! journal for its invalidations, a fetch of any age is judged.

use std::ops::Range;

use slates_vfs::journal::{Op, OpRecord};

/// Format: the end of a range that reaches the end of the file however long it is (a truncation's or a shift's tail).
pub const TO_THE_END: u64 = u64::MAX;

/// One change to a file's bytes the journal recorded: which file, which bytes (an end of [`TO_THE_END`] reaches the end
/// of the file), and the record's sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataChange {
  /// The record's sequence in the volume's journal.
  pub seq: u64,
  /// The file.
  pub ino: u64,
  /// The bytes it changed, half-open.
  pub range: Range<u64>,
}

/// The data change `record` makes, as the module doc says; `None` for a record that changes no bytes.
pub fn data_change(record: &OpRecord) -> Option<DataChange> {
  let ino = record.inode?.0;
  let range = match record.op {
    Op::Overwrite { at, len } | Op::Extend { at, len } => at..at.saturating_add(len),
    Op::Truncate { len } => len..TO_THE_END,
    Op::Insert { at, .. } | Op::Delete { at, .. } => at..TO_THE_END,
    Op::Drift => 0..TO_THE_END,
    _ => return None,
  };
  (range.start < range.end).then_some(DataChange {
    seq: record.seq,
    ino,
    range,
  })
}

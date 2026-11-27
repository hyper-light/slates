//! The log ring's released pages given back (A-746's owed item, A-926 part 2), measured as the process's memory: alone
//! in its binary, since the figures are the process's.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::print_stdout)]

use slates_anchor::layout::RegionKind;
use slates_anchor::{AnchorSegment, Geometry};
use slates_db::Op;
use slates_db::catalog::VolumeId;
use slates_db::record::{LogEntry, LogRing};
use slates_machine::facts::{Facts, Identity};

/// Shape: the segment page.
const PAGE: u64 = 4096;
/// Shape: the log ring: past the 88 MiB A-746 found resident past its live tail.
const LOG_BYTES: u64 = 128 << 20;
/// Shape: the bytes logged before the release: 96 MiB, about A-746's.
const LOGGED: usize = 96 << 20;
/// Shape: each record's entry: large, so the ring fills in few records.
const ENTRY: usize = 64 << 10;

/// The process's memory the OS charges it: the physical footprint on macOS (where a page given back leaves it at once,
/// though the resident size keeps counting it), the resident size elsewhere (where a give-back leaves it at once).
fn charged() -> u64 {
  Facts::footprint_now()
    .or_else(Facts::resident_now)
    .unwrap_or(0)
}

/// A-746's owed give-back, A-926 part 2 (main's ruling of 2026-11-25). Do: on a fresh segment, log 96 MiB of work-log
/// records, release the ring as a snapshot covering them does (to its first byte, A-71), read the process's charged
/// memory, give back the
/// partition's dead pages a slice at a time, and read it again. Expect the charged memory down by most of what was logged
/// (the ring's pages past its tail), and the ring's mark at its tail. Before A-926 part 2 nothing gave them back: they
/// stayed resident past the live tail for the segment's life (88 MiB after a Subversion import, A-746).
#[test]
fn a_released_rings_pages_go_back_to_the_os() {
  let identity = Identity {
    cpu: "test".into(),
    os: "test".into(),
    arch: "test".into(),
    cores: 4,
    memory: 1,
    page: PAGE,
  };
  let mut seg = AnchorSegment::create(
    &format!("slates-db-ring-give-back-{}", std::process::id()),
    &identity,
    Geometry {
      partitions: 1,
      page: PAGE,
      profile_bytes: PAGE,
      log_bytes: LOG_BYTES,
      snapshot_bytes: 1 << 20,
      landing_slots: 1,
      landing_slot_bytes: PAGE,
    },
  )
  .unwrap();
  let ring = LogRing::new(RegionKind::Log(0));
  let before = charged();
  let mut seq = 0u64;
  for n in 0..LOGGED / ENTRY {
    let entry = LogEntry {
      ops: vec![Op::WorkLogged {
        work: VolumeId { bytes: [7; 16] },
        entry: vec![u8::try_from(n % 251).unwrap() | 1; ENTRY],
      }],
    };
    ring.write(&mut seg, seq, &LogRing::encode(&entry)).unwrap();
    seq += 1;
  }
  // A snapshot covering them published: the ring released to its first byte (A-71), as `Db::snapshot` releases it.
  ring.release_all(&seg, seq).unwrap();
  let released = charged();
  let mut given = 0;
  loop {
    let step = seg.give_back_dead(0, (1 << 20, None)).unwrap();
    given += step.bytes;
    if !step.more {
      break;
    }
  }
  let after = charged();
  let mark = seg.region_marks(0).unwrap().ring;
  let tail =
    seg.ring_words(RegionKind::Log(0)).unwrap()[1].load(std::sync::atomic::Ordering::Acquire);
  println!(
    "segment_give_back: charged {} MiB before, {} MiB with the released ring, {} MiB after its give-back ({} MiB given \
     back; the ring's mark {mark}, its tail {tail})",
    before >> 20,
    released >> 20,
    after >> 20,
    given >> 20
  );
  assert!(
    given >= LOGGED * 9 / 10,
    "most of the ring given back: {given} of {LOGGED}"
  );
  assert!(
    released.saturating_sub(after) >= (LOGGED as u64) * 8 / 10,
    "the charged memory down by most of it: {released} to {after}"
  );
}

//! The changeset log over a simulated file with power-loss semantics (A-767 item 2): records round-trip whole,
//! committed records survive every crash, a record interrupted before its commit never does, and a log cut or damaged
//! anywhere recovers exactly the committed records before the damage.

use hyper_block::buf::Alignment;
use hyper_block::sim::{Crash, Fault, SimFile};

use super::*;

/// A simulated buffered file: transfers of any byte, writes torn at 512-byte sectors.
fn file(seed: u64) -> SimFile {
  SimFile::new(Alignment::BYTE, Alignment::BYTE, seed).unwrap()
}

fn header(sequence: u64, sections: u16) -> Header {
  Header {
    tool: Tool::Mercurial,
    repository: 9,
    sequence,
    judged_over: sequence.saturating_sub(1),
    grant: 3,
    work: 100 + sequence,
    time_ns: 1_000 * sequence,
    sections,
  }
}

/// Appends a record of two sections, the payload in pieces of `piece` bytes.
fn append(log: &mut ChangesetLog<SimFile>, sequence: u64, payload: &[u8], piece: usize) -> Located {
  let mut record = log.begin(header(sequence, 2)).unwrap();
  record.section(SectionKind::Record, 5).unwrap();
  record.piece(b"heads").unwrap();
  record
    .section(SectionKind::Payload, payload.len() as u64)
    .unwrap();
  for chunk in payload.chunks(piece.max(1)) {
    record.piece(chunk).unwrap();
  }
  record.commit().unwrap()
}

fn recovered(file: SimFile) -> (ChangesetLog<SimFile>, Vec<Located>) {
  let mut found = Vec::new();
  let log = ChangesetLog::recover(file, 0, |_, located| found.push(*located)).unwrap();
  (log, found)
}

fn payload(seed: u8, len: usize) -> Vec<u8> {
  (0..len)
    .map(|at| at.to_le_bytes()[0].wrapping_mul(31).wrapping_add(seed))
    .collect()
}

/// Do: append two records, the second's payload in uneven pieces, and recover. Expect: both found with their headers
/// and identities, their sections read back byte for byte, and the log's end where it was.
#[test]
fn records_round_trip_whole() {
  let (mut log, found) = recovered(file(1));
  assert!(found.is_empty());
  let first = append(&mut log, 1, &payload(1, 3_000), 1_000);
  let second = append(&mut log, 2, &payload(2, 70_001), 4_093);
  assert_ne!(first.identity, second.identity);
  let end = log.end();
  let (log, found) = recovered(log.into_file());
  assert_eq!(found, vec![first, second]);
  assert_eq!(log.end(), end);
  let sections = log.sections(&second).unwrap();
  assert_eq!(
    sections.iter().map(|s| s.kind).collect::<Vec<_>>(),
    [SectionKind::Record, SectionKind::Payload]
  );
  let mut read = vec![0u8; usize::try_from(sections[1].len).unwrap()];
  log.read_at(&mut read, sections[1].offset).unwrap();
  assert_eq!(read, payload(2, 70_001));
}

/// Do: append and commit records, then lose power three ways. Expect: every committed record survives each.
#[test]
fn committed_records_survive_every_crash() {
  for (seed, crash) in [(2, Crash::LoseAll), (3, Crash::Random), (4, Crash::KeepAll)] {
    let (mut log, _) = recovered(file(seed));
    let a = append(&mut log, 1, &payload(3, 9_000), 777);
    let b = append(&mut log, 2, &payload(4, 100), 100);
    let file = log.into_file();
    file.crash(crash).unwrap();
    let (_, found) = recovered(file);
    assert_eq!(found, vec![a, b], "{crash:?}");
  }
}

/// Do: commit a record, begin a second and write its sections, lose power before its commit, recover, append a third.
/// Expect: the second is never found, whatever survived of it, and the third takes its place.
#[test]
fn a_record_interrupted_before_its_commit_is_never_recovered() {
  for seed in 10..30 {
    let (mut log, _) = recovered(file(seed));
    let first = append(&mut log, 1, &payload(5, 5_000), 512);
    {
      let mut record = log.begin(header(2, 2)).unwrap();
      record.section(SectionKind::Record, 5).unwrap();
      record.piece(b"heads").unwrap();
      record.section(SectionKind::Payload, 4_000).unwrap();
      record.piece(&payload(6, 4_000)).unwrap();
      // Dropped: no trailer, no flush.
    }
    assert_eq!(
      log.end(),
      first.end(),
      "an uncommitted record leaves the end where it was"
    );
    let file = log.into_file();
    file.crash(Crash::Random).unwrap();
    let (mut log, found) = recovered(file);
    assert_eq!(found, vec![first], "seed {seed}");
    let third = append(&mut log, 2, &payload(7, 300), 300);
    let (_, found) = recovered(log.into_file());
    assert_eq!(found, vec![first, third], "seed {seed}");
  }
}

/// Do: write two committed records, then for every length of the file from the first record's end to the second's,
/// recover a copy cut there. Expect: only the first, until the cut is the whole second record.
#[test]
fn a_log_cut_anywhere_recovers_the_records_before_the_cut() {
  let (mut log, _) = recovered(file(40));
  let first = append(&mut log, 1, &payload(8, 600), 64);
  let second = append(&mut log, 2, &payload(9, 1_500), 128);
  let image = log.into_file().durable_image().unwrap();
  for cut in first.end()..=second.end() {
    let copy = file(41);
    copy
      .write_all_at(&image[..usize::try_from(cut).unwrap()], 0)
      .unwrap();
    copy.sync_data().unwrap();
    let (log, found) = recovered(copy);
    let expected = if cut == second.end() {
      vec![first, second]
    } else {
      vec![first]
    };
    assert_eq!(found, expected, "cut at {cut}");
    assert_eq!(log.end(), expected.last().unwrap().end());
  }
}

/// Do: flip one stored bit in the second record's payload, its header, its trailer. Expect: the log ends before the
/// second record each time.
#[test]
fn a_damaged_record_ends_the_log_before_it() {
  for (index, at_end) in [(0u64, false), (40, false), (0, true)] {
    let (mut log, _) = recovered(file(50));
    let first = append(&mut log, 1, &payload(10, 200), 200);
    let second = append(&mut log, 2, &payload(11, 900), 300);
    let sections = log.sections(&second).unwrap();
    let offset = if at_end {
      second.end() - 10
    } else if index == 0 {
      sections[1].offset + 450
    } else {
      second.offset + index
    };
    let file = log.into_file();
    file
      .inject(Fault::BitFlip {
        offset,
        bit: 3,
        stored: true,
      })
      .unwrap();
    let (_, found) = recovered(file);
    assert_eq!(found, vec![first], "flip at {offset}");
  }
}

/// A record of two sections committed with one: refused out of order.
fn a_short_record_is_refused(log: &mut ChangesetLog<SimFile>) {
  let mut short = log.begin(header(1, 2)).unwrap();
  short.section(SectionKind::Work, 1).unwrap();
  short.piece(b"a").unwrap();
  assert!(matches!(short.commit(), Err(LogError::OutOfOrder(_))));
}

/// Do: append out of order: bytes with no section, a section past the header's count, bytes past a section's length,
/// a commit with a section missing. Expect: each refused, and nothing committed.
#[test]
fn out_of_order_appends_are_refused_and_commit_nothing() {
  let (mut log, _) = recovered(file(60));
  let mut record = log.begin(header(1, 1)).unwrap();
  assert!(matches!(record.piece(b"x"), Err(LogError::OutOfOrder(_))));
  record.section(SectionKind::Work, 2).unwrap();
  assert!(matches!(record.piece(b"abc"), Err(LogError::OutOfOrder(_))));
  record.piece(b"ab").unwrap();
  assert!(matches!(
    record.section(SectionKind::Names, 1),
    Err(LogError::OutOfOrder(_))
  ));
  drop(record);
  a_short_record_is_refused(&mut log);
  assert_eq!(log.end(), 0);
  let (_, found) = recovered(log.into_file());
  assert!(found.is_empty());
}

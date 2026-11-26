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

/// Appends a record for a specific `tool`, `repository`, and `sequence`.
fn append_for(
  log: &mut ChangesetLog<SimFile>,
  tool: Tool,
  repository: u128,
  sequence: u64,
  payload_bytes: &[u8],
) -> Located {
  let mut record = log
    .begin(Header {
      tool,
      repository,
      sequence,
      judged_over: sequence.saturating_sub(1),
      grant: 3,
      work: 100 + sequence,
      time_ns: 1_000 * sequence,
      sections: 2,
    })
    .unwrap();
  record.section(SectionKind::Record, 5).unwrap();
  record.piece(b"heads").unwrap();
  record
    .section(
      SectionKind::Payload,
      u64::try_from(payload_bytes.len()).unwrap(),
    )
    .unwrap();
  record.piece(payload_bytes).unwrap();
  record.commit().unwrap()
}

/// Reads a committed record's payload section back whole.
fn read_payload(log: &ChangesetLog<SimFile>, located: &Located) -> Vec<u8> {
  let sections = log.sections(located).unwrap();
  let payload_section = sections
    .iter()
    .find(|section| section.kind == SectionKind::Payload)
    .unwrap();
  let mut out = vec![0u8; usize::try_from(payload_section.len).unwrap()];
  log.read_at(&mut out, payload_section.offset).unwrap();
  out
}

/// SEAM-1: cross-protocol blast-radius isolation for a payload bit-flip.
/// Do: commit records for Git (1), Mercurial (2), and Subversion (3); flip 1 bit in Git's payload;
/// recover, read Hg and SVN back, append record 4, and recover again.
/// Expect: Git's corrupt record is NOT indexed; Hg and SVN ARE recovered intact with exact bytes;
/// `log.end()` is past record 3; record 4 appends past record 3 without overwriting Hg or SVN.
#[test]
fn mid_file_payload_bitflip_does_not_take_down_following_records() {
  let (mut log, _) = recovered(file(70));
  let git_bytes = payload(1, 4_096);
  let hg_bytes = payload(2, 2_048);
  let svn_bytes = payload(3, 3_072);
  let r1 = append_for(&mut log, Tool::Git, 101, 1, &git_bytes);
  let r2 = append_for(&mut log, Tool::Mercurial, 202, 1, &hg_bytes);
  let r3 = append_for(&mut log, Tool::Subversion, 303, 1, &svn_bytes);
  let r1_payload = log.sections(&r1).unwrap()[1];
  let file = log.into_file();
  file
    .inject(Fault::BitFlip {
      offset: r1_payload.offset + 512,
      bit: 3,
      stored: true,
    })
    .unwrap();
  let (mut log, found) = recovered(file);
  assert_eq!(found, vec![r2, r3]);
  assert_eq!(log.end(), r3.end());
  assert_eq!(log.skipped(), 1);
  assert_eq!(read_payload(&log, &r2), hg_bytes);
  assert_eq!(read_payload(&log, &r3), svn_bytes);
  let r4_bytes = payload(4, 1_024);
  let r4 = append_for(&mut log, Tool::Git, 101, 2, &r4_bytes);
  assert_eq!(r4.offset, r3.end());
  let (log, found) = recovered(log.into_file());
  assert_eq!(found, vec![r2, r3, r4]);
  assert_eq!(log.end(), r4.end());
  assert_eq!(read_payload(&log, &r2), hg_bytes);
  assert_eq!(read_payload(&log, &r3), svn_bytes);
  assert_eq!(read_payload(&log, &r4), r4_bytes);
}

/// SEAM-1: cross-protocol isolation for mid-file header and trailer bit-flips, including a record
/// whose follower starts right across a `VERIFY_PIECE` chunk boundary.
/// Do: flip 1 bit in record 1's magic, header body, and commit trailer while valid Hg (2) and
/// SVN (3) records follow (including when record 2's magic straddles `VERIFY_PIECE`).
/// Expect: record 1 is never recovered, records 2 and 3 always recover whole with exact bytes,
/// and `log.end()` advances to record 3's end.
#[test]
fn mid_file_header_and_trailer_bitflips_still_recover_following_records() {
  #[derive(Clone, Copy)]
  enum FlipTarget {
    Magic,
    HeaderField,
    Trailer,
  }
  // Choose a payload size so record 2's `SLCS` magic straddles `VERIFY_PIECE` (65,536 bytes):
  // header (72) + sec0 head (16) + 5 + sec1 head (16) + payload + trailer (48) = 157 + payload.
  let straddle_len = VERIFY_PIECE - 159;
  for (case_name, r1_len, flip_kind) in [
    ("magic", 1_024usize, FlipTarget::Magic),
    ("header-field", 1_024, FlipTarget::HeaderField),
    ("trailer", 1_024, FlipTarget::Trailer),
    ("chunk-boundary-magic", straddle_len, FlipTarget::Magic),
  ] {
    let (mut log, _) = recovered(file(71));
    let git_bytes = payload(11, r1_len);
    let hg_bytes = payload(12, 2_000);
    let svn_bytes = payload(13, 1_500);
    let r1 = append_for(&mut log, Tool::Git, 101, 1, &git_bytes);
    let r2 = append_for(&mut log, Tool::Mercurial, 202, 1, &hg_bytes);
    let r3 = append_for(&mut log, Tool::Subversion, 303, 1, &svn_bytes);
    let flip_at = match flip_kind {
      FlipTarget::Magic => r1.offset,
      FlipTarget::HeaderField => r1.offset + 20,
      FlipTarget::Trailer => r1.end() - 8,
    };
    let file = log.into_file();
    file
      .inject(Fault::BitFlip {
        offset: flip_at,
        bit: 2,
        stored: true,
      })
      .unwrap();
    let (log, found) = recovered(file);
    assert_eq!(found, vec![r2, r3], "case {case_name}");
    assert_eq!(log.end(), r3.end(), "case {case_name}");
    assert_eq!(log.skipped(), 1, "case {case_name}");
    assert_eq!(read_payload(&log, &r2), hg_bytes, "case {case_name}");
    assert_eq!(read_payload(&log, &r3), svn_bytes, "case {case_name}");
  }
}

/// SEAM-1: a torn uncommitted tail after valid records (even after an earlier skipped corrupt
/// record) must stop `log.end()` right at the last valid committed record so the next append
/// cleanly overwrites the uncommitted tail.
#[test]
fn torn_uncommitted_tail_after_valid_records_stops_at_last_valid_end() {
  let (mut log, _) = recovered(file(72));
  let r1 = append_for(&mut log, Tool::Git, 101, 1, &payload(21, 512));
  let r2 = append_for(&mut log, Tool::Mercurial, 202, 1, &payload(22, 600));
  let r3 = append_for(&mut log, Tool::Subversion, 303, 1, &payload(23, 700));
  {
    let mut torn = log
      .begin(Header {
        tool: Tool::Git,
        repository: 101,
        sequence: 2,
        judged_over: 1,
        grant: 3,
        work: 104,
        time_ns: 4_000,
        sections: 2,
      })
      .unwrap();
    torn.section(SectionKind::Record, 5).unwrap();
    torn.piece(b"heads").unwrap();
    torn.section(SectionKind::Payload, 1_200).unwrap();
    torn.piece(&payload(24, 1_200)).unwrap();
    // Dropped without commit: leaves an uncommitted torn tail past `r3.end()`.
  }
  let file = log.into_file();
  file
    .inject(Fault::BitFlip {
      offset: r1.offset + 20,
      bit: 1,
      stored: true,
    })
    .unwrap();
  let (mut log, found) = recovered(file);
  assert_eq!(found, vec![r2, r3]);
  assert_eq!(log.end(), r3.end());
  assert_eq!(log.skipped(), 1);
  let r4_bytes = payload(25, 400);
  let r4 = append_for(&mut log, Tool::Git, 101, 2, &r4_bytes);
  assert_eq!(r4.offset, r3.end());
  let (log, found) = recovered(log.into_file());
  assert_eq!(found, vec![r2, r3, r4]);
  assert_eq!(log.end(), r4.end());
  assert_eq!(read_payload(&log, &r4), r4_bytes);
}

/// SEAM-1: a mid-file corrupt header whose payload embeds multiple fake `SLCS` magic occurrences
/// (e.g. a repository containing slates' own source) still resynchronizes cleanly onto the real
/// following records, and skips consecutive damaged records in a single linear pass.
#[test]
fn mid_file_corrupt_header_with_embedded_magic_and_consecutive_damage_recovers_cleanly() {
  let (mut log, _) = recovered(file(73));
  let mut embedded = Vec::new();
  for _ in 0..32 {
    embedded.extend_from_slice(b"SLCS\x01\x00\x01\x00fake-header-bytes-not-a-valid-crc");
  }
  let r1 = append_for(&mut log, Tool::Git, 101, 1, &embedded);
  let r2 = append_for(&mut log, Tool::Git, 101, 2, &payload(30, 1_024));
  let hg_bytes = payload(31, 2_048);
  let r3 = append_for(&mut log, Tool::Mercurial, 202, 1, &hg_bytes);
  let r2_payload = log.sections(&r2).unwrap()[1];
  let file = log.into_file();
  // Corrupt r1's header AND r2's payload; r3 (Hg) must still recover whole.
  file
    .inject(Fault::BitFlip {
      offset: r1.offset + 2,
      bit: 0,
      stored: true,
    })
    .unwrap();
  file
    .inject(Fault::BitFlip {
      offset: r2_payload.offset + 100,
      bit: 5,
      stored: true,
    })
    .unwrap();
  let (log, found) = recovered(file);
  assert_eq!(found, vec![r3]);
  assert_eq!(log.end(), r3.end());
  assert_eq!(read_payload(&log, &r3), hg_bytes);
}

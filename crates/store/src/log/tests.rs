//! The changeset log over a simulated file with power-loss semantics (A-767 item 2): records round-trip whole,
//! committed records survive every crash, a record interrupted before its commit never does, a truncated or torn
//! uncommitted EOF tail stops right after the last valid committed record without scanning its payload, and mid-file
//! damage is skipped or resynchronized linearly so healthy records after it still recover.

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

/// Verifies recovered record locations, log end offset, and exact payload bytes for each record.
fn assert_recovered_payloads(
  log: &ChangesetLog<SimFile>,
  found: &[Located],
  expected: &[(Located, &[u8])],
) {
  let expected_located: Vec<Located> = expected.iter().map(|(located, _)| *located).collect();
  assert_eq!(found, expected_located.as_slice());
  let last_end = expected.last().unwrap().0.end();
  assert_eq!(log.end(), last_end);
  for (located, bytes) in expected {
    assert_eq!(read_payload(log, located), *bytes);
  }
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
  assert_eq!(log.skipped(), 1);
  assert_recovered_payloads(&log, &found, &[(r2, &hg_bytes), (r3, &svn_bytes)]);
  let r4_bytes = payload(4, 1_024);
  let r4 = append_for(&mut log, Tool::Git, 101, 2, &r4_bytes);
  assert_eq!(r4.offset, r3.end());
  let (log, found) = recovered(log.into_file());
  assert_recovered_payloads(
    &log,
    &found,
    &[(r2, &hg_bytes), (r3, &svn_bytes), (r4, &r4_bytes)],
  );
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
  // Without untrusted `SkipTo` jumps during resync, r1 (broken header) and r2 (damaged payload)
  // are bypassed as one contiguous corrupt span `r1.offset..r3.offset`.
  assert_eq!(log.skipped(), 1);
  assert_eq!(read_payload(&log, &r3), hg_bytes);
}

/// Counting wrapper over [`SimFile`] so complexity tests prove linear I/O bounds by counting
/// `read_exact_at` calls rather than measuring wall-clock time.
struct CountingFile {
  inner: SimFile,
  reads: std::sync::atomic::AtomicU64,
}

impl CountingFile {
  fn new(inner: SimFile) -> Self {
    Self {
      inner,
      reads: std::sync::atomic::AtomicU64::new(0),
    }
  }

  fn reads(&self) -> u64 {
    self.reads.load(std::sync::atomic::Ordering::Relaxed)
  }
}

impl BlockFile for CountingFile {
  fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), DiskError> {
    self
      .reads
      .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    self.inner.read_exact_at(buf, offset)
  }

  fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<(), DiskError> {
    self.inner.write_all_at(buf, offset)
  }

  fn sync_data(&self) -> Result<(), DiskError> {
    self.inner.sync_data()
  }

  fn len(&self) -> Result<u64, DiskError> {
    self.inner.len()
  }

  fn layout_block(&self) -> Alignment {
    self.inner.layout_block()
  }

  fn alignment(&self) -> Alignment {
    self.inner.alignment()
  }
}

/// Builds the serialized bytes of a valid committed record for `repository` with `grant`.
fn encoded_record(tool: Tool, repository: u128, grant: u64, payload_bytes: &[u8]) -> Vec<u8> {
  let (mut log, _) = recovered(file(80));
  let mut record = log
    .begin(Header {
      tool,
      repository,
      sequence: 1,
      judged_over: 0,
      grant,
      work: 77,
      time_ns: 777,
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
  let located = record.commit().unwrap();
  let image = log.into_file().durable_image().unwrap();
  image[usize::try_from(located.offset).unwrap()..usize::try_from(located.end()).unwrap()].to_vec()
}

/// Builds a valid `SLCS` header declaring `sections` sections and a valid section head whose
/// length equals [`HEADER_BYTES`], so `[header][head]` chains force overlapping framing scans.
fn chained_header_and_head(sections: u16) -> (Vec<u8>, Vec<u8>) {
  let (mut log, _) = recovered(file(81));
  let mut record = log.begin(header(1, sections)).unwrap();
  for _ in 0..sections {
    record.section(SectionKind::Record, 0).unwrap();
  }
  let located = record.commit().unwrap();
  let image = log.into_file().durable_image().unwrap();
  let start = usize::try_from(located.offset).unwrap();
  let header_bytes = image[start..start + HEADER_BYTES].to_vec();
  let mut head = image[start + HEADER_BYTES..start + HEADER_BYTES + SECTION_HEAD_BYTES].to_vec();
  head[S_LEN..S_CRC].copy_from_slice(&(HEADER_BYTES as u64).to_le_bytes());
  (header_bytes, head)
}

/// Asserts that recovering `sim` keeps only `expected` with `log.end() == expected.end()` and
/// `log.skipped() == 0`.
fn assert_torn_ignored(sim: SimFile, expected: Located, label: &str) {
  let (log, found) = recovered(sim);
  assert_eq!(found, vec![expected], "{label}");
  assert_eq!(log.end(), expected.end(), "{label}");
  assert_eq!(log.skipped(), 0, "{label}");
}

/// Blocker 1: Do: place a whole valid committed record for repository 202 inside repository 101's
/// torn record at EOF — in three variants: (a) trailer zeroed, (b) truncated mid-payload after the
/// embedded record, and (c) uncommitted `Appending` dropped mid-payload — and recover.
/// Expect: repository 202's forged record is NEVER recovered, `log.end()` stops right after
/// repository 101's committed first record, and `log.skipped()` is 0.
#[test]
fn torn_record_carrying_valid_forged_record_never_indexes_it() {
  let forged = encoded_record(
    Tool::Mercurial,
    202,
    3,
    b"FORGED: bytes repository 202's owner never pushed",
  );

  // Variant A: trailer zeroed at EOF.
  let (mut log, _) = recovered(file(82));
  let r1 = append_for(&mut log, Tool::Git, 101, 1, b"legit git push one");
  let torn = append_for(&mut log, Tool::Git, 101, 2, &forged);
  let sim = log.into_file();
  sim
    .write_all_at(&[0u8; TRAILER_BYTES], torn.end() - TRAILER_BYTES as u64)
    .unwrap();
  sim.sync_data().unwrap();
  assert_torn_ignored(sim, r1, "trailer-zeroed variant must ignore forged payload");

  // Variant B: truncated mid-payload (after the complete embedded forged record, before r2's end).
  let mut padded = forged.clone();
  padded.extend_from_slice(&[0x55; 256]);
  let (mut log, _) = recovered(file(83));
  let r1 = append_for(&mut log, Tool::Git, 101, 1, b"legit git push one");
  let torn = append_for(&mut log, Tool::Git, 101, 2, &padded);
  let image = log.into_file().durable_image().unwrap();
  let cut = usize::try_from(torn.end()).unwrap() - 128;
  let truncated = file(84);
  truncated.write_all_at(&image[..cut], 0).unwrap();
  truncated.sync_data().unwrap();
  assert_torn_ignored(
    truncated,
    r1,
    "truncated-mid-payload variant must ignore forged payload",
  );

  // Variant C: uncommitted Appending dropped mid-payload (section 1 retains its authenticated
  // open-section head, SECTION_OPEN_FLAG = 0x80).
  let (mut log, _) = recovered(file(85));
  let r1 = append_for(&mut log, Tool::Git, 101, 1, b"legit git push one");
  {
    let mut open = log.begin(header(2, 2)).unwrap();
    open.section(SectionKind::Record, 5).unwrap();
    open.piece(b"heads").unwrap();
    open
      .section(SectionKind::Payload, u64::try_from(forged.len()).unwrap())
      .unwrap();
    open.piece(&forged).unwrap();
  }
  assert_torn_ignored(
    log.into_file(),
    r1,
    "dropped uncommitted tail must ignore forged payload",
  );
}

/// Builds a payload of `count` chained valid `[header][head]` pairs declaring `sections` sections.
fn chained_headers_payload(sections: u16, count: usize) -> Vec<u8> {
  let (many_header, one_head) = chained_header_and_head(sections);
  let mut chained = Vec::with_capacity((many_header.len() + one_head.len()) * count);
  for _ in 0..count {
    chained.extend_from_slice(&many_header);
    chained.extend_from_slice(&one_head);
  }
  chained
}

/// Blocker 2: Do: fill a record's payload with `K` chained valid `[header][section_head]` pairs
/// (declaring 4096 sections each) under both (a) a closed-section torn trailer at EOF and (b) a
/// mid-file corrupted header, and count `BlockFile::read_exact_at` calls during recovery.
/// Expect: both (a) and (b) stay strictly within their linear scan bounds (rejecting embedded
/// chained headers in memory by `(offset, seal)` CRC without quadratic blowup; O(1) non-scanning
/// applies to authenticated open-section tails tested separately in
/// `ordinary_interrupted_uncommitted_tail_reads_constant_calls_across_tail_sizes`).
#[test]
fn resync_over_chained_valid_headers_does_linear_work() {
  let chained_payload = chained_headers_payload(4_096, 400);

  // Case A: closed-section torn trailer at EOF carrying chained valid headers -> scans linearly
  // in `VERIFY_PIECE` chunks while rejecting embedded headers in memory, stopping at r1.
  let (mut log, _) = recovered(file(86));
  let r1 = append_for(&mut log, Tool::Git, 101, 1, b"legit");
  let torn = append_for(&mut log, Tool::Git, 101, 2, &chained_payload);
  let sim = log.into_file();
  sim
    .write_all_at(&[0u8; TRAILER_BYTES], torn.end() - TRAILER_BYTES as u64)
    .unwrap();
  sim.sync_data().unwrap();
  let total_len_a = sim.len().unwrap();
  let mut found = Vec::new();
  let log =
    ChangesetLog::recover(CountingFile::new(sim), 0, |_, located| found.push(*located)).unwrap();
  let linear_chunks_a = total_len_a.div_ceil(VERIFY_PIECE as u64) + 16;
  assert_eq!((found, log.end()), (vec![r1], r1.end()));
  assert!(
    log.file().reads() <= linear_chunks_a,
    "closed-section torn trailer must stay within linear chunk scan bound (<= {linear_chunks_a} reads); got {} reads",
    log.file().reads()
  );

  // Case B: mid-file corrupted header ahead of chained valid headers -> strictly linear reads.
  let (mut log, _) = recovered(file(87));
  let bad = append_for(&mut log, Tool::Git, 101, 1, &chained_payload);
  let sim = log.into_file();
  sim
    .inject(Fault::BitFlip {
      offset: bad.offset + 2,
      bit: 0,
      stored: true,
    })
    .unwrap();
  let total_len = sim.len().unwrap();
  let mut found = Vec::new();
  let log =
    ChangesetLog::recover(CountingFile::new(sim), 0, |_, located| found.push(*located)).unwrap();
  let linear_cap = (total_len / SECTION_HEAD_BYTES as u64) * 3 + 64;
  assert!(
    found.is_empty() && log.file().reads() <= linear_cap,
    "corrupted-header resync must stay linear (<= {linear_cap} reads), got {} reads",
    log.file().reads()
  );
}

/// Blocker 1 (grant filter): Do: corrupt record 1's header mid-file while record 1's payload holds
/// a whole valid record carrying a foreign `grant` (999) and record 2 follows with victim `grant`
/// (3); recover via `recover_for_grant(..., Some(3))`.
/// Expect: the foreign-grant candidate is rejected and record 2 recovers with exact payload bytes.
#[test]
fn recover_for_grant_rejects_foreign_candidates_in_corrupt_span() {
  let foreign = encoded_record(Tool::Mercurial, 202, 999, b"foreign store record");
  let (mut log, _) = recovered(file(88));
  let r1 = append_for(&mut log, Tool::Git, 101, 1, &foreign);
  let real_bytes = payload(44, 512);
  let r2 = append_for(&mut log, Tool::Subversion, 303, 1, &real_bytes);
  let sim = log.into_file();
  sim
    .inject(Fault::BitFlip {
      offset: r1.offset + 2,
      bit: 0,
      stored: true,
    })
    .unwrap();
  let mut found = Vec::new();
  let log =
    ChangesetLog::recover_for_grant(sim, 0, Some(3), |_, located| found.push(*located)).unwrap();
  assert_recovered_payloads(&log, &found, &[(r2, &real_bytes)]);
}

/// Blocker 1 (mid-file bad-trailer isolation): Do: corrupt BOTH record 1's trailer (whose payload
/// holds a whole valid record with victim grant 3) AND record 2's header, while record 3 follows.
/// Expect: scanning begins at record 2 (never inside record 1's payload), so the forged record
/// inside record 1 is never indexed and record 3 recovers whole.
#[test]
fn mid_file_bad_trailer_followed_by_broken_header_never_scans_own_payload() {
  let same_grant_forged = encoded_record(Tool::Mercurial, 202, 3, b"forged same-grant payload");
  let (mut log, _) = recovered(file(89));
  let r1 = append_for(&mut log, Tool::Git, 101, 1, &same_grant_forged);
  let r2 = append_for(
    &mut log,
    Tool::Git,
    101,
    2,
    b"second record with broken header",
  );
  let r3_bytes = payload(55, 640);
  let r3 = append_for(&mut log, Tool::Subversion, 303, 1, &r3_bytes);
  let sim = log.into_file();
  sim
    .inject(Fault::BitFlip {
      offset: r1.end() - 8,
      bit: 1,
      stored: true,
    })
    .unwrap();
  sim
    .inject(Fault::BitFlip {
      offset: r2.offset + 2,
      bit: 0,
      stored: true,
    })
    .unwrap();
  let mut found = Vec::new();
  let log =
    ChangesetLog::recover_for_grant(sim, 0, Some(3), |_, located| found.push(*located)).unwrap();
  assert_recovered_payloads(&log, &found, &[(r3, &r3_bytes)]);
}

/// Round-2 Blocker 2: Do: write a 3-record log (Git, Mercurial, Subversion) and flip EVERY single
/// bit of the middle record (`0..r2.len * 8`, covering header, every section descriptor, payload,
/// and trailer); recover each faulted copy.
/// Expect: 100% of single-bit faults leave both healthy siblings (Git and Subversion) recovered
/// with exact bytes, `log.end() == r3.end()`, and `log.skipped() == 1`.
#[test]
fn every_single_bit_flip_in_middle_record_preserves_both_healthy_siblings() {
  let (mut log, _) = recovered(file(90));
  let git_bytes = payload(61, 32);
  let hg_bytes = vec![0x5Au8];
  let svn_bytes = payload(63, 48);
  let r1 = append_for(&mut log, Tool::Git, 101, 1, &git_bytes);
  let r2 = append_for(&mut log, Tool::Mercurial, 202, 1, &hg_bytes);
  let r3 = append_for(&mut log, Tool::Subversion, 303, 1, &svn_bytes);
  assert_eq!(r2.len * 8, 1_264);
  let image = log.into_file().durable_image().unwrap();
  let start = usize::try_from(r2.offset).unwrap();
  let count = usize::try_from(r2.len).unwrap();
  for byte_idx in 0..count {
    for bit in 0..8u8 {
      let copy = file(91);
      copy.write_all_at(&image, 0).unwrap();
      copy.sync_data().unwrap();
      copy
        .inject(Fault::BitFlip {
          offset: r2.offset + u64::try_from(byte_idx).unwrap(),
          bit,
          stored: true,
        })
        .unwrap();
      let (log, found) = recovered(copy);
      assert_eq!(
        found,
        vec![r1, r3],
        "bit flip at r2+{byte_idx} (file offset {}) bit {bit} lost sibling",
        start + byte_idx
      );
      assert_eq!(
        log.end(),
        r3.end(),
        "bit flip at r2+{byte_idx} bit {bit} rewound log.end()"
      );
      assert_eq!(log.skipped(), 1, "bit flip at r2+{byte_idx} bit {bit}");
      assert_eq!(read_payload(&log, &r1), git_bytes);
      assert_eq!(read_payload(&log, &r3), svn_bytes);
    }
  }
}

/// Encodes a committed record at `target_offset` with `seal` and `grant`.
fn encoded_record_at(
  seal: [u8; SEAL_BYTES],
  target_offset: u64,
  tool: Tool,
  repository: u128,
  grant: u64,
  payload_bytes: &[u8],
) -> Vec<u8> {
  let sim = file(95);
  if target_offset > 0 {
    let pad = vec![0u8; usize::try_from(target_offset).unwrap()];
    sim.write_all_at(&pad, 0).unwrap();
    sim.sync_data().unwrap();
  }
  let mut log = ChangesetLog::recover_sealed(sim, target_offset, seal, None, |_, _| {}).unwrap();
  let mut record = log
    .begin(Header {
      tool,
      repository,
      sequence: 1,
      judged_over: 0,
      grant,
      work: 77,
      time_ns: 777,
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
  let located = record.commit().unwrap();
  let image = log.into_file().durable_image().unwrap();
  image[usize::try_from(located.offset).unwrap()..usize::try_from(located.end()).unwrap()].to_vec()
}

/// Checks C1 (mid-file broken header followed by healthy record 3) against `forged` under `seal`.
fn assert_c1_forged_rejected(seed: u64, seal: [u8; SEAL_BYTES], forged: &[u8], label: &str) {
  let mut log = ChangesetLog::recover_sealed(file(seed), 0, seal, Some(3), |_, _| {}).unwrap();
  let r1 = append_for(&mut log, Tool::Git, 101, 1, forged);
  let r3_bytes = payload(64, 128);
  let r3 = append_for(&mut log, Tool::Subversion, 303, 1, &r3_bytes);
  let sim = log.into_file();
  sim
    .inject(Fault::BitFlip {
      offset: r1.offset + 8,
      bit: 2,
      stored: true,
    })
    .unwrap();
  let mut found = Vec::new();
  let log = ChangesetLog::recover_sealed(sim, 0, seal, Some(3), |_, located| {
    found.push(*located);
  })
  .unwrap();
  assert_eq!(found, vec![r3], "{label}");
  assert_eq!(log.end(), r3.end(), "{label}");
  assert_eq!(read_payload(&log, &r3), r3_bytes, "{label}");
}

/// Checks C2 (zeroed header at EOF) and C3 (zeroed header + trailer at EOF) against `forged`.
fn assert_c2_c3_forged_rejected(
  seed: u64,
  seal: [u8; SEAL_BYTES],
  forged: &[u8],
  zero_trailer: bool,
  label: &str,
) {
  let mut log = ChangesetLog::recover_sealed(file(seed), 0, seal, Some(3), |_, _| {}).unwrap();
  let r1 = append_for(&mut log, Tool::Git, 101, 1, b"legit commit");
  let carrier = append_for(&mut log, Tool::Git, 101, 2, forged);
  let sim = log.into_file();
  sim
    .write_all_at(&[0u8; HEADER_BYTES], carrier.offset)
    .unwrap();
  if zero_trailer {
    sim
      .write_all_at(
        &[0u8; TRAILER_BYTES],
        carrier.end() - u64::try_from(TRAILER_BYTES).unwrap(),
      )
      .unwrap();
  }
  sim.sync_data().unwrap();
  let mut found = Vec::new();
  let log = ChangesetLog::recover_sealed(sim, 0, seal, Some(3), |_, located| {
    found.push(*located);
  })
  .unwrap();
  assert_eq!(found, vec![r1], "{label}");
  assert_eq!(log.end(), r1.end(), "{label}");
}

/// Round-2 Blocker 1 (`SimFile` C1/C2/C3): Do: under a non-zero per-store secret `seal`, embed a
/// same-grant `SLCS..SLCC` record for repo 202 — both (a) a verbatim offset-0 record sealed with
/// the SAME secret and (b) an unsealed record freshly encoded for the exact nested offset with the
/// same grant — inside repo 101's carrier payload under C1 (mid-file broken header + healthy
/// sibling 3), C2 (zeroed header at EOF), and C3 (zeroed header + trailer torn tail at EOF).
/// Expect: repo 202 is NEVER indexed, repo 303 recovers cleanly on C1, and `log.end()` stops at
/// the last genuine committed record on C2 and C3.
#[test]
fn nested_same_grant_forged_record_never_indexes_in_c1_c2_c3() {
  let seal = [0xA5u8; SEAL_BYTES];
  let payload_prefix =
    u64::try_from(HEADER_BYTES + SECTION_HEAD_BYTES + 5 + SECTION_HEAD_BYTES).unwrap();
  let verbatim_same_seal = encoded_record_at(seal, 0, Tool::Mercurial, 202, 3, b"FORGED same-seal");
  let exact_c1_unsealed = encoded_record_at(
    UNSEALED,
    payload_prefix,
    Tool::Mercurial,
    202,
    3,
    b"FORGED exact-offset c1",
  );
  assert_c1_forged_rejected(92, seal, &verbatim_same_seal, "C1 verbatim same-seal");
  assert_c1_forged_rejected(93, seal, &exact_c1_unsealed, "C1 exact-offset unsealed");

  let r1_len =
    u64::try_from(HEADER_BYTES + SECTION_HEAD_BYTES + 5 + SECTION_HEAD_BYTES + 12 + TRAILER_BYTES)
      .unwrap();
  let exact_eof_unsealed = encoded_record_at(
    UNSEALED,
    r1_len + payload_prefix,
    Tool::Mercurial,
    202,
    3,
    b"FORGED exact-offset eof",
  );
  assert_c2_c3_forged_rejected(
    94,
    seal,
    &verbatim_same_seal,
    false,
    "C2 verbatim same-seal",
  );
  assert_c2_c3_forged_rejected(95, seal, &verbatim_same_seal, true, "C3 verbatim same-seal");
  assert_c2_c3_forged_rejected(
    96,
    seal,
    &exact_eof_unsealed,
    false,
    "C2 exact-offset unsealed",
  );
  assert_c2_c3_forged_rejected(
    97,
    seal,
    &exact_eof_unsealed,
    true,
    "C3 exact-offset unsealed",
  );
}

/// Appends a single-section committed record with `kind` and `contents`.
fn append_one_section(
  log: &mut ChangesetLog<SimFile>,
  tool: Tool,
  repository: u128,
  sequence: u64,
  kind: SectionKind,
  contents: &[u8],
) -> Located {
  let mut record = log
    .begin(Header {
      tool,
      repository,
      sequence,
      judged_over: sequence.saturating_sub(1),
      grant: 3,
      work: sequence,
      time_ns: sequence,
      sections: 1,
    })
    .unwrap();
  record
    .section(kind, u64::try_from(contents.len()).unwrap())
    .unwrap();
  if !contents.is_empty() {
    record.piece(contents).unwrap();
  }
  record.commit().unwrap()
}

/// Layout of an ordinary interrupted uncommitted tail at EOF.
#[derive(Clone, Copy, Debug)]
enum UnwrittenTailShape {
  /// One declared section, interrupted mid-payload before `close_section`.
  SingleSection,
  /// One declared section whose full payload arrived before the writer dropped uncommitted.
  SingleSectionFullPayloadNoTrailer,
  /// Two declared sections, interrupted during section 0 before section 1 begins.
  TwoSectionsFirstOpen,
  /// Two declared sections, section 0 (`Record`) closed, section 1 (`Payload`) interrupted mid-write.
  TwoSectionsSecondOpen,
}

/// Runs one ordinary interrupted uncommitted tail recovery with `tail_bytes` written into an
/// unclosed section in `shape` and returns the exact `SimFile` read call count consumed by recovery.
fn measure_unwritten_tail_reads(tail_bytes: u64, shape: UnwrittenTailShape) -> u64 {
  let (mut log, _) = recovered(file(98));
  let committed = append_one_section(
    &mut log,
    Tool::Git,
    11,
    1,
    SectionKind::Payload,
    b"committed control",
  );
  let sections = match shape {
    UnwrittenTailShape::SingleSection | UnwrittenTailShape::SingleSectionFullPayloadNoTrailer => 1,
    UnwrittenTailShape::TwoSectionsFirstOpen | UnwrittenTailShape::TwoSectionsSecondOpen => 2,
  };
  let declared = match shape {
    UnwrittenTailShape::SingleSectionFullPayloadNoTrailer => tail_bytes,
    _ => tail_bytes + 1,
  };
  {
    let mut uncommitted = log
      .begin(Header {
        tool: Tool::Git,
        repository: 11,
        sequence: 2,
        judged_over: 1,
        grant: 3,
        work: 2,
        time_ns: 2,
        sections,
      })
      .unwrap();
    if matches!(shape, UnwrittenTailShape::TwoSectionsSecondOpen) {
      uncommitted.section(SectionKind::Record, 5).unwrap();
      uncommitted.piece(b"heads").unwrap();
    }
    uncommitted.section(SectionKind::Payload, declared).unwrap();
    uncommitted
      .piece(&vec![b'd'; usize::try_from(tail_bytes).unwrap()])
      .unwrap();
  }
  let disk = log.into_file();
  let before = disk.stats().unwrap().reads;
  let (reopened, records) = recovered(disk);
  assert_eq!(records, vec![committed]);
  assert_eq!(reopened.end(), committed.end());
  assert_eq!(reopened.skipped(), 0);
  reopened.file().stats().unwrap().reads - before
}

/// Asserts that recovery of `shape` performs the exact same `SimFile` read call count across
/// 4 KiB, 64 KiB, and 1 MiB uncommitted tail payloads.
fn assert_constant_tail_reads(shape: UnwrittenTailShape) {
  let reads_4k = measure_unwritten_tail_reads(4_096, shape);
  let reads_64k = measure_unwritten_tail_reads(65_536, shape);
  let reads_1m = measure_unwritten_tail_reads(1_048_576, shape);
  assert!(reads_4k > 0, "{shape:?}: recovery must read the log");
  assert_eq!(
    reads_4k, reads_64k,
    "{shape:?}: 4 KiB ({reads_4k}) vs 64 KiB ({reads_64k}) reads"
  );
  assert_eq!(
    reads_4k, reads_1m,
    "{shape:?}: 4 KiB ({reads_4k}) vs 1 MiB ({reads_1m}) reads"
  );
}

/// Do: commit one record, append an interrupted uncommitted tail (unclosed section head) with
/// payloads of 4 KiB, 64 KiB, and 1 MiB across 1-section and 2-section layouts, and recover.
/// Expect: only the committed prefix recovers, `skipped() == 0`, and `SimFile` read call counts
/// are strictly identical across all three payload sizes (O(1) reads, never scanning the tail).
#[test]
fn ordinary_interrupted_uncommitted_tail_reads_constant_calls_across_tail_sizes() {
  for shape in [
    UnwrittenTailShape::SingleSection,
    UnwrittenTailShape::SingleSectionFullPayloadNoTrailer,
    UnwrittenTailShape::TwoSectionsFirstOpen,
    UnwrittenTailShape::TwoSectionsSecondOpen,
  ] {
    assert_constant_tail_reads(shape);
  }
}

/// Verifies that flipping `flip_bit` at `head_offset` turns the 16-byte section head to all zeros
/// and that recovery still resynchronizes past `middle` to recover `prefix` and `trailing`.
fn assert_zeroed_empty_section_recovers_siblings(
  sim: SimFile,
  head_offset: u64,
  flip_bit: u8,
  (prefix, trailing): (Located, Located),
  trailing_bytes: &[u8],
  label: &str,
) {
  sim
    .inject(Fault::BitFlip {
      offset: head_offset,
      bit: flip_bit,
      stored: true,
    })
    .unwrap();
  let mut head = [0u8; SECTION_HEAD_BYTES];
  sim.read_exact_at(&mut head, head_offset).unwrap();
  assert_eq!(
    head, [0u8; SECTION_HEAD_BYTES],
    "{label}: bit flip must produce an all-zero section head"
  );
  let (log, found) = recovered(sim);
  assert_eq!(found, vec![prefix, trailing], "{label}");
  assert_eq!(log.end(), trailing.end(), "{label}");
  assert_eq!(log.skipped(), 1, "{label}");
  assert_eq!(read_payload(&log, &trailing), trailing_bytes, "{label}");
}

/// Do: commit `[prefix, middle, trailing]` where `middle` contains a 0-byte section whose single
/// set kind bit (`Record=1`, `Payload=2`, `Names=4`) is flipped so its 16-byte head becomes all
/// zeros — covering single-section records, a 0-byte first section followed by another section,
/// and a 0-byte last section after a closed first section — and recover.
/// Expect: recovery never mistakes the flipped head for an unwritten tail; linear resync skips
/// `middle` (`skipped() == 1`) and recovers both `prefix` and `trailing` intact.
#[test]
fn single_bit_flip_zeroing_empty_section_head_still_recovers_trailing_sibling() {
  let trailing_bytes = payload(77, 64);

  // Single-section 0-byte middle records (last section -> commit trailer sits at `bytes_at`).
  for (kind, bit, label) in [
    (SectionKind::Record, 0u8, "1-sec empty Record"),
    (SectionKind::Payload, 1, "1-sec empty Payload"),
    (SectionKind::Names, 2, "1-sec empty Names"),
  ] {
    let (mut log, _) = recovered(file(99));
    let prefix = append_for(&mut log, Tool::Git, 101, 1, b"prefix");
    let middle = append_one_section(&mut log, Tool::Git, 101, 2, kind, &[]);
    let trailing = append_for(&mut log, Tool::Subversion, 303, 1, &trailing_bytes);
    let head_offset = middle.offset + u64::try_from(HEADER_BYTES).unwrap();
    assert_zeroed_empty_section_recovers_siblings(
      log.into_file(),
      head_offset,
      bit,
      (prefix, trailing),
      &trailing_bytes,
      label,
    );
  }

  // Two-section middle record: 0-byte first section (`remaining_sections == 1`, next head at `bytes_at`).
  let (mut log, _) = recovered(file(100));
  let prefix = append_for(&mut log, Tool::Git, 101, 1, b"prefix");
  let middle = {
    let mut rec = log.begin(header(2, 2)).unwrap();
    rec.section(SectionKind::Record, 0).unwrap();
    rec.section(SectionKind::Payload, 5).unwrap();
    rec.piece(b"after").unwrap();
    rec.commit().unwrap()
  };
  let trailing = append_for(&mut log, Tool::Subversion, 303, 1, &trailing_bytes);
  let first_head = middle.offset + u64::try_from(HEADER_BYTES).unwrap();
  assert_zeroed_empty_section_recovers_siblings(
    log.into_file(),
    first_head,
    0,
    (prefix, trailing),
    &trailing_bytes,
    "2-sec empty first Record",
  );

  // Two-section middle record: 0-byte last section (`remaining_sections == 0`, trailer at `bytes_at`).
  let (mut log, _) = recovered(file(101));
  let prefix = append_for(&mut log, Tool::Git, 101, 1, b"prefix");
  let middle = append_for(&mut log, Tool::Mercurial, 202, 1, &[]);
  let trailing = append_for(&mut log, Tool::Subversion, 303, 1, &trailing_bytes);
  let second_head = middle.offset + u64::try_from(HEADER_BYTES + SECTION_HEAD_BYTES + 5).unwrap();
  assert_zeroed_empty_section_recovers_siblings(
    log.into_file(),
    second_head,
    1,
    (prefix, trailing),
    &trailing_bytes,
    "2-sec empty last Payload",
  );
}

/// Do: commit `[prefix, middle, trailing]` where `middle` has a 5-byte `Record` section followed
/// by a 256-byte all-zero `Payload` section; flip bit 6 of section 0's length (`5 -> 69`) so
/// framing lands inside section 1's zero payload bytes.
/// Expect: recovery verifies section 0's CRC before trusting the all-zero head at section 1,
/// falls back to linear resync, skips `middle` (`skipped() == 1`), and recovers `trailing`.
#[test]
fn single_bit_flip_in_section_len_landing_in_zero_payload_still_recovers_trailing_sibling() {
  let trailing_bytes = payload(78, 96);
  let (mut log, _) = recovered(file(102));
  let prefix = append_for(&mut log, Tool::Git, 101, 1, b"prefix");
  let middle = append_for(&mut log, Tool::Mercurial, 202, 1, &[0u8; 256]);
  let trailing = append_for(&mut log, Tool::Subversion, 303, 1, &trailing_bytes);
  let len_byte_offset = middle.offset + u64::try_from(HEADER_BYTES + S_LEN).unwrap();
  let sim = log.into_file();
  sim
    .inject(Fault::BitFlip {
      offset: len_byte_offset,
      bit: 6,
      stored: true,
    })
    .unwrap();
  let (log, found) = recovered(sim);
  assert_eq!(found, vec![prefix, trailing]);
  assert_eq!(log.end(), trailing.end());
  assert_eq!(log.skipped(), 1);
  assert_eq!(read_payload(&log, &trailing), trailing_bytes);
}

/// Appends a record with arbitrary `(kind, payload)` sections under `seal`.
fn append_sections(
  log: &mut ChangesetLog<SimFile>,
  tool: Tool,
  repository: u128,
  sections: &[(SectionKind, &[u8])],
) -> Located {
  let mut record = log
    .begin(Header {
      tool,
      repository,
      sequence: 1,
      judged_over: 0,
      grant: 3,
      work: 1,
      time_ns: 1,
      sections: u16::try_from(sections.len()).unwrap(),
    })
    .unwrap();
  for (kind, bytes) in sections {
    record
      .section(*kind, u64::try_from(bytes.len()).unwrap())
      .unwrap();
    if !bytes.is_empty() {
      record.piece(bytes).unwrap();
    }
  }
  record.commit().unwrap()
}

/// Zeroes `[fault_start, fault_start + fault_len)` in `image` and verifies recovery skips the
/// damaged middle record, keeps `[prefix, hg, svn]`, preserves `log.end() == svn.end()`, and
/// appending a 5th record leaves `hg` and `svn` bytes intact on disk across reopen.
fn assert_zeroed_mid_section_recovers_siblings(
  image: &[u8],
  seal: [u8; SEAL_BYTES],
  (fault_start, fault_len): (u64, usize),
  (prefix, hg, svn): (Located, Located, Located),
  label: &str,
) {
  let mut damaged = image.to_vec();
  let start = usize::try_from(fault_start).unwrap();
  damaged[start..start + fault_len].fill(0);
  let sim = file(105);
  sim.write_all_at(&damaged, 0).unwrap();
  sim.sync_data().unwrap();
  let mut found = Vec::new();
  let mut log =
    ChangesetLog::recover_sealed(sim, 0, seal, Some(3), |_, entry| found.push(*entry)).unwrap();
  assert_eq!(found, vec![prefix, hg, svn], "{label}: lost siblings");
  assert_eq!(log.end(), svn.end(), "{label}: rewound log.end()");
  assert_eq!(log.skipped(), 1, "{label}: skipped count");
  let next = append_sections(
    &mut log,
    Tool::Git,
    55,
    &[(SectionKind::Payload, &payload(99, 2_048))],
  );
  assert_eq!(next.offset, svn.end(), "{label}: next append offset");
  let after = log.into_file().durable_image().unwrap();
  let hg_start = usize::try_from(hg.offset).unwrap();
  let svn_end = usize::try_from(svn.end()).unwrap();
  assert_eq!(
    &after[hg_start..svn_end],
    &image[hg_start..svn_end],
    "{label}: next append overwrote healthy siblings"
  );
}

/// Round-3 Blocker 1 (`Z1..Z5`, `media_sector`): Do: zero 16 B, 512 B, and 4 KiB spans starting at
/// a committed middle record's section head (single-section, 2-section last head, 3-section middle
/// head, and aligned 512 B / 4 KiB storage sectors), recover, and append a new record.
/// Expect: all-zero section heads never trigger `UnwrittenTail`; linear resync recovers 100% of
/// trailing Hg/SVN siblings, keeps `log.end() == svn.end()`, and the next append preserves healthy
/// sibling bytes intact.
#[test]
fn zeroed_section_head_mid_file_recovers_trailing_siblings_and_preserves_them_on_append() {
  let seal = [0x53u8; SEAL_BYTES];
  let hg_bytes = payload(9, 300);
  let svn_bytes = payload(11, 300);

  // Z1/Z4 (16 B last head of 2-section record) & Z2 (4 KiB page starting at last head).
  for (label, pad_len, payload_len, fault_len) in [
    (
      "Z1 16B last head",
      1_000usize,
      8_192usize,
      SECTION_HEAD_BYTES,
    ),
    ("Z2 4KiB page at last head", 4_096, 65_536, 4_096),
  ] {
    let mut log = ChangesetLog::recover_sealed(file(103), 0, seal, Some(3), |_, _| {}).unwrap();
    let prefix = append_sections(
      &mut log,
      Tool::Git,
      11,
      &[(SectionKind::Payload, &payload(7, pad_len))],
    );
    let rec_part = payload(1, 200);
    let pay_part = payload(2, payload_len);
    let middle = append_sections(
      &mut log,
      Tool::Git,
      22,
      &[
        (SectionKind::Record, &rec_part),
        (SectionKind::Payload, &pay_part),
      ],
    );
    let hg = append_sections(
      &mut log,
      Tool::Mercurial,
      33,
      &[(SectionKind::Payload, &hg_bytes)],
    );
    let svn = append_sections(
      &mut log,
      Tool::Subversion,
      44,
      &[(SectionKind::Payload, &svn_bytes)],
    );
    let last_head = middle.offset + u64::try_from(HEADER_BYTES + SECTION_HEAD_BYTES + 200).unwrap();
    let image = log.into_file().durable_image().unwrap();
    assert_zeroed_mid_section_recovers_siblings(
      &image,
      seal,
      (last_head, fault_len),
      (prefix, hg, svn),
      label,
    );
  }

  // Z3 (single-section record, prior_sections == 0) & Z5 (middle head of 3-section record).
  {
    let mut log = ChangesetLog::recover_sealed(file(104), 0, seal, Some(3), |_, _| {}).unwrap();
    let prefix = append_sections(
      &mut log,
      Tool::Git,
      11,
      &[(SectionKind::Payload, &payload(7, 512))],
    );
    let middle = append_sections(
      &mut log,
      Tool::Git,
      22,
      &[(SectionKind::Payload, &payload(2, 4_096))],
    );
    let hg = append_sections(
      &mut log,
      Tool::Mercurial,
      33,
      &[(SectionKind::Payload, &hg_bytes)],
    );
    let svn = append_sections(
      &mut log,
      Tool::Subversion,
      44,
      &[(SectionKind::Payload, &svn_bytes)],
    );
    let head = middle.offset + u64::try_from(HEADER_BYTES).unwrap();
    let image = log.into_file().durable_image().unwrap();
    assert_zeroed_mid_section_recovers_siblings(
      &image,
      seal,
      (head, SECTION_HEAD_BYTES),
      (prefix, hg, svn),
      "Z3 single-section head",
    );
  }
  {
    let mut log = ChangesetLog::recover_sealed(file(106), 0, seal, Some(3), |_, _| {}).unwrap();
    let prefix = append_sections(
      &mut log,
      Tool::Git,
      11,
      &[(SectionKind::Payload, &payload(7, 512))],
    );
    let middle = append_sections(
      &mut log,
      Tool::Git,
      22,
      &[
        (SectionKind::Record, &payload(1, 200)),
        (SectionKind::Payload, &payload(2, 4_096)),
        (SectionKind::Work, &payload(4, 512)),
      ],
    );
    let hg = append_sections(
      &mut log,
      Tool::Mercurial,
      33,
      &[(SectionKind::Payload, &hg_bytes)],
    );
    let svn = append_sections(
      &mut log,
      Tool::Subversion,
      44,
      &[(SectionKind::Payload, &svn_bytes)],
    );
    let mid_head = middle.offset + u64::try_from(HEADER_BYTES + SECTION_HEAD_BYTES + 200).unwrap();
    let image = log.into_file().durable_image().unwrap();
    assert_zeroed_mid_section_recovers_siblings(
      &image,
      seal,
      (mid_head, SECTION_HEAD_BYTES),
      (prefix, hg, svn),
      "Z5 3-section middle head",
    );
  }

  // Aligned 512 B and 4 KiB storage sector loss right at middle record's section head (media_sector).
  let overhead = HEADER_BYTES + SECTION_HEAD_BYTES + TRAILER_BYTES;
  for sector in [512usize, 4_096] {
    let mut log = ChangesetLog::recover_sealed(file(107), 0, seal, Some(3), |_, _| {}).unwrap();
    let first_payload = vec![b'p'; sector - overhead - HEADER_BYTES];
    let prefix = append_sections(
      &mut log,
      Tool::Git,
      1,
      &[(SectionKind::Payload, &first_payload)],
    );
    let _middle = append_sections(
      &mut log,
      Tool::Git,
      2,
      &[(SectionKind::Payload, &vec![b'm'; 2 * sector])],
    );
    let hg = append_sections(
      &mut log,
      Tool::Mercurial,
      3,
      &[(SectionKind::Payload, b"healthy Hg")],
    );
    let svn = append_sections(
      &mut log,
      Tool::Subversion,
      4,
      &[(SectionKind::Payload, b"healthy SVN")],
    );
    let image = log.into_file().durable_image().unwrap();
    assert_zeroed_mid_section_recovers_siblings(
      &image,
      seal,
      (u64::try_from(sector).unwrap(), sector),
      (prefix, hg, svn),
      &format!("media_sector {sector}B"),
    );
  }
}

/// Verifies every single-bit flip across all 48 bytes of `encoded` (including `P_FORMAT`)
/// classifies as `SealPreamble::Damaged` so media bit-flips can recover from `rows`'s backup seal.
fn assert_preamble_bitflips_rejected(encoded: [u8; SEAL_PREAMBLE_BYTES]) {
  for byte_idx in 0..SEAL_PREAMBLE_BYTES {
    for bit in 0..8u8 {
      let mut flipped = encoded;
      flipped[byte_idx] ^= 1u8 << bit;
      let decoded = decode_seal_preamble(&flipped);
      assert_eq!(
        decoded,
        SealPreamble::Damaged,
        "flip at preamble+{byte_idx} bit {bit}"
      );
    }
  }
}

/// Verifies `ChangesetLog::recover_sealed` from offset 0 steps cleanly past a matching `SLSE`
/// preamble with `skipped() == 0` and recovers trailing records intact.
fn assert_recover_from_zero_steps_past_preamble(
  seal: [u8; SEAL_BYTES],
  encoded: &[u8; SEAL_PREAMBLE_BYTES],
) {
  let sim = file(108);
  sim.write_all_at(encoded, 0).unwrap();
  sim.sync_data().unwrap();
  let start = u64::try_from(SEAL_PREAMBLE_BYTES).unwrap();
  let mut log = ChangesetLog::recover_sealed(sim, start, seal, Some(3), |_, _| {}).unwrap();
  let first = append_sections(
    &mut log,
    Tool::Git,
    11,
    &[(SectionKind::Payload, b"after preamble")],
  );
  assert_eq!(first.offset, start);
  let mut found = Vec::new();
  let reopened = ChangesetLog::recover_sealed(log.into_file(), 0, seal, Some(3), |_, entry| {
    found.push(*entry);
  })
  .unwrap();
  assert_eq!(
    (found, reopened.end(), reopened.skipped()),
    (vec![first], first.end(), 0)
  );
}

/// Round-3 Blocker 2 / Round-4 B2: Do: round-trip `encode_seal_preamble` / `decode_seal_preamble`,
/// flip every single bit across all 48 bytes, feed all-zeroes, legacy `FORMAT = 1` `SLCS`, valid-CRC
/// future `FORMAT + 1` `SLSE`, and truncated `SLSE`, and recover a `ChangesetLog` starting at
/// offset 0 over an `SLSE`-prefixed file.
/// Expect: valid `FORMAT = 2` preambles decode identically; `SLCS` v1 yields `UnsupportedFormat(1)`;
/// checksummed `SLSE` `FORMAT + 1` yields `UnsupportedFormat(FORMAT + 1)`; all-zeroes, truncated,
/// and EVERY single-bit flip across all 48 bytes (including `P_FORMAT`) yield `Damaged`; and
/// `recover_sealed` from 0 steps past a matching `SLSE` preamble cleanly with `skipped() == 0`.
#[test]
fn seal_preamble_round_trips_and_refuses_foreign_formats_and_damage() {
  let seal = [0x6Bu8; SEAL_BYTES];
  let encoded = encode_seal_preamble(&seal);
  assert_eq!(
    (
      encoded.len(),
      decode_seal_preamble(&encoded),
      decode_seal_preamble(&[0u8; SEAL_PREAMBLE_BYTES]),
      decode_seal_preamble(&encoded[..SEAL_PREAMBLE_BYTES - 1]),
    ),
    (
      SEAL_PREAMBLE_BYTES,
      SealPreamble::Valid(seal),
      SealPreamble::Damaged,
      SealPreamble::Damaged,
    )
  );
  assert_preamble_bitflips_rejected(encoded);

  let mut legacy_slcs = [0u8; SEAL_PREAMBLE_BYTES];
  legacy_slcs[..4].copy_from_slice(b"SLCS");
  legacy_slcs[4..6].copy_from_slice(&1u16.to_le_bytes());
  let mut future_slse = encoded;
  future_slse[P_FORMAT..P_RESERVED].copy_from_slice(&(FORMAT + 1).to_le_bytes());
  let future_crc = mantle_crc::crc32c(&future_slse[..SEAL_PREAMBLE_COVERED]);
  future_slse[SEAL_PREAMBLE_COVERED..P_CRC_COMPLEMENT].copy_from_slice(&future_crc.to_le_bytes());
  future_slse[P_CRC_COMPLEMENT..SEAL_PREAMBLE_BYTES].copy_from_slice(&(!future_crc).to_le_bytes());
  assert_eq!(
    (
      decode_seal_preamble(&legacy_slcs),
      decode_seal_preamble(&future_slse),
    ),
    (
      SealPreamble::UnsupportedFormat(1),
      SealPreamble::UnsupportedFormat(FORMAT + 1),
    )
  );
  assert_recover_from_zero_steps_past_preamble(seal, &encoded);
}

/// Round-4 Truncation Check: Do: call `ChangesetLog::recover_sealed` with `from > file.len()`
/// (both a 20-byte file with `from = 48` and a 48-byte file with `from = 256`).
/// Expect: `Err(LogError::Corrupt(_))` without mutating the file.
#[test]
fn recover_sealed_refuses_recovery_offset_past_log_end() {
  let seal = [0x5Au8; SEAL_BYTES];
  let preamble = encode_seal_preamble(&seal);

  let short = file(109);
  short.write_all_at(&preamble[..20], 0).unwrap();
  short.sync_data().unwrap();
  let err_short = ChangesetLog::recover_sealed(
    short,
    u64::try_from(SEAL_PREAMBLE_BYTES).unwrap(),
    seal,
    None,
    |_, _| {},
  );
  assert!(
    matches!(err_short, Err(LogError::Corrupt(_))),
    "from=48 on 20-byte log must refuse Corrupt, got {err_short:?}"
  );

  let only_preamble = file(110);
  only_preamble.write_all_at(&preamble, 0).unwrap();
  only_preamble.sync_data().unwrap();
  let err_applied = ChangesetLog::recover_sealed(only_preamble, 256, seal, None, |_, _| {});
  assert!(
    matches!(err_applied, Err(LogError::Corrupt(_))),
    "from=256 on 48-byte log must refuse Corrupt, got {err_applied:?}"
  );
}

/// Round-4 `close_section` Ordering: Do: open a 1-section record declaring 16 bytes, write only 4
/// bytes, trigger `close_section` via a failing `open_section` call, and then call `commit_open`.
/// Expect: both `open_section` and the subsequent `commit_open` refuse `LogError::OutOfOrder`
/// instead of committing a record with an unclosed `0x80` section head, leaving `log.end()` at 0.
#[test]
fn close_section_refuses_commit_after_incomplete_section_error() {
  let (mut log, _) = recovered(file(111));
  log.open_record(header(1, 1)).unwrap();
  log.open_section(SectionKind::Payload, 16).unwrap();
  log.open_piece(b"part").unwrap();
  let next_err = log.open_section(SectionKind::Record, 4);
  assert!(
    matches!(next_err, Err(LogError::OutOfOrder(_))),
    "opening a second section before finishing the first must refuse, got {next_err:?}"
  );
  let commit_err = log.commit_open();
  assert!(
    matches!(commit_err, Err(LogError::OutOfOrder(_))),
    "commit after failed close_section must refuse instead of committing unclosed 0x80 head, got {commit_err:?}"
  );
  assert_eq!(log.end(), 0);
}

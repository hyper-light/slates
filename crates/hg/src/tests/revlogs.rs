//! Golden and hostile tests of the revlog reader (A-611): every revlog of one history stock Mercurial 7.2 stored in five
//! repositories (`fixtures/revlogs`), each entry held to 7.2's `debugindex --debug`, each text to its `debugdata`, and
//! the reader's refusals on damaged and hostile bytes.

use crate::compression::StreamRefusal;
use crate::fixtures::revlogs::{file, find};
use crate::revlog::{self, Entry, Format, Mode, NONE, Refused, Revlog};
use crate::{Admission, Node};
use proptest::prelude::*;

/// The repositories of version 1 and those behind dockets.
const V1_REPOSITORIES: [&str; 3] = ["zstd", "zlib", "split"];

/// The repositories of version 2.
const V2_REPOSITORIES: [&str; 2] = ["v2", "clv2"];

/// Each revlog: its store file's stem in the fixtures, and its listing's name.
const LOGS: [(&str, &str); 5] = [
  ("00changelog", "00changelog"),
  ("00manifest", "00manifest"),
  ("data_grow", "grow"),
  ("data_rand", "rand"),
  ("data_empty", "empty"),
];

/// A generous room for one revlog's texts.
fn room() -> Admission {
  Admission::new(1 << 24)
}

/// A node from its hexadecimal.
fn node(hex: &str) -> Node {
  crate::fixtures::node(hex)
}

/// One row of a listing: each column's name and value.
type Row = Vec<(String, String)>;

/// Every text of a revlog, in order.
type Texts = Vec<Vec<u8>>;

/// An entry as `debugindex` lists it: offset, flags, chunk and text lengths, base, link, parents, node, mode.
type Listed = (u64, u16, u32, u32, u32, u32, u32, u32, Node, &'static str);

/// A listing's rows, each a column's name to its value.
fn listing(name: &str) -> Vec<Row> {
  let text = core::str::from_utf8(file(name)).unwrap();
  let mut lines = text.lines();
  let header: Vec<String> = lines
    .next()
    .unwrap()
    .split_whitespace()
    .map(str::to_string)
    .collect();
  lines
    .map(|line| {
      let fields: Vec<&str> = line.split_whitespace().collect();
      assert_eq!(fields.len(), header.len(), "{name}: {line}");
      header
        .iter()
        .cloned()
        .zip(fields.iter().map(|field| field.to_string()))
        .collect()
    })
    .collect()
}

/// A row's column.
fn column<'r>(row: &'r [(String, String)], name: &str) -> &'r str {
  &row.iter().find(|(known, _)| known == name).unwrap().1
}

/// A listing's revision number: -1 is none.
fn rev_of(value: &str) -> u32 {
  if value == "-1" {
    NONE
  } else {
    value.parse().unwrap()
  }
}

/// A repository's revlog, opened as its format asks.
fn open(repository: &str, stem: &str) -> Revlog<'static> {
  if V2_REPOSITORIES.contains(&repository) {
    let docket = revlog::read_docket(file(&format!("{repository}-{stem}.i"))).unwrap();
    let index = file(&format!("{repository}-{stem}.idx"));
    let data = file(&format!("{repository}-{stem}.dat"));
    Revlog::open_docketed(&docket, index, data).unwrap()
  } else {
    let index = file(&format!("{repository}-{stem}.i"));
    let data = find(&format!("{repository}-{stem}.d")).unwrap_or_default();
    Revlog::open(index, data, &mut room()).unwrap()
  }
}

/// What the golden walk saw: every chunk's first stored byte, the longest chain, and whether some delta's base was not
/// the revision before it.
#[derive(Default)]
struct Seen {
  headers: Vec<u8>,
  longest_chain: usize,
  generaldelta: bool,
}

/// The entry `row` lists, as `Listed`.
fn listed(row: &[(String, String)]) -> Listed {
  let mode = match column(row, "comp-mode") {
    "0" => "0",
    "1" => "1",
    "2" => "2",
    _ => "unknown",
  };
  (
    column(row, "data-offset").parse().unwrap(),
    column(row, "flags").parse().unwrap(),
    column(row, "chunk-size").parse().unwrap(),
    column(row, "full-size").parse().unwrap(),
    rev_of(column(row, "delta-base")),
    rev_of(column(row, "linkrev")),
    rev_of(column(row, "p1-rev")),
    rev_of(column(row, "p2-rev")),
    node(column(row, "nodeid")),
    mode,
  )
}

/// An entry read, as `Listed`.
fn read(entry: &Entry) -> Listed {
  let mode = match entry.mode {
    Mode::Plain => "0",
    Mode::Default => "1",
    Mode::Inline => "2",
  };
  (
    entry.offset,
    entry.flags,
    entry.compressed,
    entry.uncompressed,
    entry.base,
    entry.link,
    entry.p1,
    entry.p2,
    entry.node,
    mode,
  )
}

/// Holds one revlog of one repository to 7.2's listing and texts, noting what its chunks and chains showed.
fn check_log(repository: &str, stem: &str, name: &str, seen: &mut Seen) {
  let log = open(repository, stem);
  let rows = listing(&format!("{repository}-{name}.index"));
  assert_eq!(
    usize::try_from(log.len()).unwrap(),
    rows.len(),
    "{repository} {name}"
  );
  for row in &rows {
    let rev: u32 = column(row, "rev").parse().unwrap();
    let entry = log.entry(rev).unwrap();
    assert_eq!(read(&entry), listed(row), "{repository} {name} {rev}");
    let text = log.text(rev, &mut room()).unwrap();
    assert_eq!(
      text,
      file(&format!("{name}-{rev}.txt")),
      "{repository} {name} {rev}"
    );
    seen.longest_chain = seen
      .longest_chain
      .max(log.chain(rev, &mut room()).unwrap().len());
    seen.generaldelta |= entry.base != rev && entry.base.checked_add(1) != Some(rev);
    if V1_REPOSITORIES.contains(&repository) && entry.compressed > 0 {
      seen
        .headers
        .push(first_stored_byte(repository, stem, &log, rev));
    }
  }
}

/// A-611 (golden): do open every revlog of the five repositories and read each revision's entry and text. Expect each
/// entry as 7.2's `debugindex --debug` lists it (offset, flags, chunk and text lengths, base, link, parents, node, and
/// version 2's compression mode), each text equal to its `debugdata` and hashing to its node, and every shape the
/// fixtures were made for met: zlib and zstd chunks, raw chunks, deltas over chains longer than one, generaldelta bases
/// that are not the revision before, and sparse-revlog's intermediate snapshots.
#[test]
fn every_revision_reads_as_7_2_stored_it() {
  let mut seen = Seen::default();
  for repository in V1_REPOSITORIES.iter().chain(V2_REPOSITORIES.iter()) {
    for (stem, name) in LOGS {
      check_log(repository, stem, name, &mut seen);
    }
  }
  assert!(
    seen.headers.contains(&b'x') && seen.headers.contains(&b'(') && seen.headers.contains(&b'u')
  );
  assert!(seen.longest_chain > 1 && seen.generaldelta);
  let snapshots = core::str::from_utf8(file("zstd-grow.chain"))
    .unwrap()
    .lines()
    .filter(|line| line.split_whitespace().last() == Some("snap"))
    .count();
  assert!(snapshots >= 2, "sparse-revlog's intermediate snapshots");
}

/// The first byte of revision `rev`'s stored chunk, read from the fixture's bytes by its entry.
fn first_stored_byte(repository: &str, stem: &str, log: &Revlog<'_>, rev: u32) -> u8 {
  let entry = log.entry(rev).unwrap();
  let offset = usize::try_from(entry.offset).unwrap();
  let inline_index = file(&format!("{repository}-{stem}.i"));
  match find(&format!("{repository}-{stem}.d")) {
    Some(data) => data[offset],
    None => inline_index[offset + (usize::try_from(rev).unwrap() + 1) * 64],
  }
}

/// A-611: do read the dockets of the version 2 repositories. Expect each one's index and data ids eight bytes, its index
/// end the `.idx` file's length and its data end the `.dat` file's, and zstd's `(` its default header.
#[test]
fn dockets_name_their_files() {
  for repository in V2_REPOSITORIES {
    for (stem, _) in LOGS {
      let docket = revlog::read_docket(file(&format!("{repository}-{stem}.i"))).unwrap();
      assert_eq!((docket.index_id.len(), docket.data_id.len()), (8, 8));
      let index = file(&format!("{repository}-{stem}.idx"));
      let data = file(&format!("{repository}-{stem}.dat"));
      assert_eq!(
        (docket.index_end, docket.data_end, docket.default_header),
        (
          u64::try_from(index.len()).unwrap(),
          u64::try_from(data.len()).unwrap(),
          b'('
        )
      );
    }
  }
  let changelog = open("clv2", "00changelog");
  assert_eq!(changelog.format(), Format::ChangelogV2);
  assert_eq!(open("v2", "00changelog").format(), Format::V2);
}

/// An inline revlog of version 1, its bytes owned so a test may damage them.
fn inline_bytes() -> Vec<u8> {
  file("zstd-data_grow.i").to_vec()
}

/// Every text of a revlog, or the first refusal.
fn read_all(index: &[u8], data: &[u8], admission: &mut Admission) -> Result<Texts, Refused> {
  let log = Revlog::open(index, data, admission)?;
  (0..log.len()).map(|rev| log.text(rev, admission)).collect()
}

/// AC-HG.9 (hostile input), A-611: do open an index whose header names version 0, an unknown version and an unknown
/// flag, a docket naming version 1, and a v1 index whose entries are not whole. Expect each refused by kind.
#[test]
fn versions_7_2_would_not_read_are_refused() {
  let mut index = inline_bytes();
  for header in [0x0003_0000_u32, 0x0003_0007, 0x0013_0001] {
    index[..4].copy_from_slice(&header.to_be_bytes());
    assert_eq!(
      Revlog::open(&index, &[], &mut room()).map(|log| log.len()),
      Err(Refused::Version { header })
    );
  }
  let mut docket = file("v2-data_grow.i").to_vec();
  docket[..4].copy_from_slice(&1_u32.to_be_bytes());
  let read = revlog::read_docket(&docket).unwrap();
  assert_eq!(
    Revlog::open_docketed(&read, file("v2-data_grow.idx"), file("v2-data_grow.dat"))
      .map(|log| log.len()),
    Err(Refused::Version { header: 1 })
  );
  let split = file("split-data_grow.i");
  assert!(matches!(
    Revlog::open(
      &split[..split.len() - 1],
      file("split-data_grow.d"),
      &mut room()
    ),
    Err(Refused::Truncated { .. })
  ));
}

/// Where an inline fixture revlog's second entry begins.
fn second_entry() -> usize {
  let clean = inline_bytes();
  let log = Revlog::open(&clean, &[], &mut room()).unwrap();
  usize::try_from(log.entry(0).unwrap().compressed).unwrap() + 64
}

/// The inline fixture revlog with `change` made to its bytes, given where its second entry begins.
fn damaged(change: impl FnOnce(&mut [u8], usize)) -> Vec<u8> {
  let mut bytes = inline_bytes();
  change(&mut bytes, second_entry());
  bytes
}

/// AC-HG.9, A-611: do damage an inline revlog's second entry: its chunk's length made negative, its offset made unlike
/// the chunks before it, and its base and its first parent made later than itself. Expect each refused as corrupt at
/// revision 1.
#[test]
fn damaged_entries_are_refused_by_kind() {
  let negative = damaged(|bytes, second| {
    bytes[second + 8..second + 12].copy_from_slice(&(-5_i32).to_be_bytes());
  });
  assert!(matches!(
    Revlog::open(&negative, &[], &mut room()),
    Err(Refused::Corrupt { rev: 1, .. })
  ));
  let moved = damaged(|bytes, second| bytes[second + 5] ^= 1);
  assert!(matches!(
    Revlog::open(&moved, &[], &mut room()),
    Err(Refused::Corrupt { rev: 1, .. })
  ));
  for field in [16, 24] {
    let later = damaged(|bytes, second| {
      bytes[second + field..second + field + 4].copy_from_slice(&5_i32.to_be_bytes());
    });
    let read = Revlog::open(&later, &[], &mut room()).unwrap();
    assert!(matches!(
      read.entry(1),
      Err(Refused::Corrupt { rev: 1, .. })
    ));
  }
}

/// AC-HG.9, A-611: do damage an inline revlog's first revision: a byte of its node flipped, its chunk's header made
/// unknown, and the censored flag set. Expect its text refused as a node mismatch, an unknown compression and
/// unhashed flags.
#[test]
fn damaged_texts_are_refused_by_kind() {
  let wrong_node = damaged(|bytes, _| bytes[32] ^= 1);
  let read = Revlog::open(&wrong_node, &[], &mut room()).unwrap();
  assert!(matches!(
    read.text(0, &mut room()),
    Err(Refused::Node { rev: 0, .. })
  ));
  let unknown = damaged(|bytes, _| bytes[64] = b'?');
  let read = Revlog::open(&unknown, &[], &mut room()).unwrap();
  assert_eq!(
    read.text(0, &mut room()),
    Err(Refused::Compression {
      rev: 0,
      header: b'?'
    })
  );
  let censored = damaged(|bytes, _| bytes[6] |= 0x80);
  let read = Revlog::open(&censored, &[], &mut room()).unwrap();
  assert_eq!(
    read.text(0, &mut room()),
    Err(Refused::Flags {
      rev: 0,
      flags: 0x8000
    })
  );
}

/// An inline revlog of one revision whose chunk is `chunk` (a full text of `len` bytes, no parents).
fn one_revision(chunk: &[u8], len: u32) -> Vec<u8> {
  let mut index = Vec::new();
  index.extend_from_slice(&0x0003_0001_u32.to_be_bytes());
  index.extend_from_slice(&[0; 4]);
  for field in [
    i32::try_from(chunk.len()).unwrap(),
    i32::try_from(len).unwrap(),
    0,
    0,
    -1,
    -1,
  ] {
    index.extend_from_slice(&field.to_be_bytes());
  }
  index.extend_from_slice(&[0; 32]);
  index.extend_from_slice(chunk);
  index
}

/// The first revision's chunk of a fixture's inline revlog, as stored, and its text's length.
fn first_chunk(name: &str) -> (Vec<u8>, u32) {
  let index = file(name);
  let log = Revlog::open(index, &[], &mut room()).unwrap();
  let entry = log.entry(0).unwrap();
  let len = usize::try_from(entry.compressed).unwrap();
  (index[64..64 + len].to_vec(), entry.uncompressed)
}

/// AC-HG.9, A-611: do read one-revision revlogs whose chunk is 7.2's own zlib stream or zstd frame with a byte after
/// it, or cut short by a byte. Expect the byte after refused as one trailing byte and the cut stream as cut short, both
/// by the inflater compressed bundles use; the chunk as stored read to its text's length.
#[test]
fn a_compressed_chunk_must_end_exactly() {
  for name in ["zlib-data_grow.i", "zstd-data_grow.i"] {
    let (chunk, len) = first_chunk(name);
    let whole = one_revision(&chunk, len);
    let log = Revlog::open(&whole, &[], &mut room()).unwrap();
    assert_eq!(
      log.chunk(0, &mut room()).map(|text| text.len()),
      Ok(usize::try_from(len).unwrap())
    );
    let trailing = one_revision(&[chunk.as_slice(), b"!"].concat(), len);
    let log = Revlog::open(&trailing, &[], &mut room()).unwrap();
    assert!(
      matches!(
        log.chunk(0, &mut room()),
        Err(Refused::Stream {
          rev: 0,
          refusal: StreamRefusal::TrailingBytes { bytes: 1, .. }
        })
      ),
      "{name}"
    );
    let cut = one_revision(&chunk[..chunk.len() - 1], len);
    let log = Revlog::open(&cut, &[], &mut room()).unwrap();
    assert!(
      matches!(
        log.chunk(0, &mut room()),
        Err(Refused::Stream {
          rev: 0,
          refusal: StreamRefusal::Truncated { .. }
        })
      ),
      "{name}"
    );
  }
}

/// AC-HG.9, A-611: do read every text of a revlog under admissions from nothing to its need. Expect each refused past
/// its admission with nothing panicking, and the whole read once the admission holds it.
#[test]
fn reading_is_held_to_the_admission() {
  let index = inline_bytes();
  let mut measure = room();
  read_all(&index, &[], &mut measure).unwrap();
  let need = (1 << 24) - measure.left();
  for given in [0, 1, need / 2, need - 1] {
    assert!(matches!(
      read_all(&index, &[], &mut Admission::new(given)),
      Err(Refused::PastAdmission(_))
    ));
  }
  assert!(read_all(&index, &[], &mut Admission::new(need)).is_ok());
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// AC-HG.9, A-611: do change any byte of an inline revlog, or of a split revlog's index or data, to any value, or cut
  /// one at any byte, and read every text. Expect a refusal or texts that hash to their nodes; never a panic.
  #[test]
  fn damaged_bytes_never_panic(
    which in 0_u8..3,
    at in any::<proptest::sample::Index>(),
    byte in any::<u8>(),
    cut in any::<bool>(),
  ) {
    let mut index = if which == 0 { inline_bytes() } else { file("split-data_grow.i").to_vec() };
    let mut data = if which == 0 { Vec::new() } else { file("split-data_grow.d").to_vec() };
    let target = if which == 2 { &mut data } else { &mut index };
    let place = at.index(target.len());
    if cut { target.truncate(place) } else { target[place] = byte }
    let _read = read_all(&index, &data, &mut room());
  }

  /// AC-HG.9, A-611: do read a docket cut at any byte or with any byte changed. Expect a docket or a refusal; never a
  /// panic.
  #[test]
  fn damaged_dockets_never_panic(
    at in any::<proptest::sample::Index>(),
    byte in any::<u8>(),
    cut in any::<bool>(),
  ) {
    let mut docket = file("v2-00changelog.i").to_vec();
    let place = at.index(docket.len());
    if cut { docket.truncate(place) } else { docket[place] = byte }
    let _read = revlog::read_docket(&docket);
  }
}

/// An inline revlog with general delta of `links` revisions: revision 0 the whole `text` (stored `u`-marked), each later
/// one a delta against the one before it that changes one byte, so the last revision's chain is every revision.
fn one_chain(text: &[u8], links: u32) -> Vec<u8> {
  let mut index = Vec::new();
  let mut current = text.to_vec();
  let mut previous = crate::NULL_NODE;
  let mut offset = 0_u64;
  for rev in 0..links {
    let chunk = if rev == 0 {
      [b"u".as_slice(), text].concat()
    } else {
      let at = usize::try_from(rev).unwrap() % current.len();
      current[at] = current[at].wrapping_add(1);
      let mut delta = Vec::new();
      crate::delta::encode_hunk(
        &crate::delta::Hunk {
          start: u32::try_from(at).unwrap(),
          end: u32::try_from(at + 1).unwrap(),
          data: &current[at..=at],
        },
        &mut delta,
      )
      .unwrap();
      delta
    };
    let node = crate::node::compute(&previous, &crate::NULL_NODE, &current).unwrap();
    let mut entry = Vec::new();
    if rev == 0 {
      entry.extend_from_slice(&0x0003_0001_u32.to_be_bytes());
      entry.extend_from_slice(&[0; 4]);
    } else {
      entry.extend_from_slice(&(offset << 16).to_be_bytes());
    }
    let base = if rev == 0 {
      0
    } else {
      i32::try_from(rev - 1).unwrap()
    };
    let p1 = if rev == 0 {
      -1
    } else {
      i32::try_from(rev - 1).unwrap()
    };
    for field in [
      i32::try_from(chunk.len()).unwrap(),
      i32::try_from(current.len()).unwrap(),
      base,
      i32::try_from(rev).unwrap(),
      p1,
      -1,
    ] {
      entry.extend_from_slice(&field.to_be_bytes());
    }
    entry.extend_from_slice(&node);
    entry.extend_from_slice(&[0; 12]);
    index.extend_from_slice(&entry);
    index.extend_from_slice(&chunk);
    previous = node;
    offset += u64::try_from(chunk.len()).unwrap();
  }
  index
}

/// A-630, main's order of 2026-11-20 15:40 (the hg import's memory): do rebuild the last revision of a 64-link chain over
/// a 4 KiB text. Expect it read under a room of its chunks and a few texts: a chain's deltas apply into the two texts it
/// alternates between, not a new text a link charged on top of every text before it.
#[test]
fn a_chains_deltas_are_applied_within_two_texts_not_one_a_link() {
  let text: Vec<u8> = (0..4096_u32)
    .map(|n| b'a' + u8::try_from(n % 26).unwrap())
    .collect();
  let links = 64;
  let index = one_chain(&text, links);
  let log = Revlog::open(&index, &[], &mut room()).unwrap();
  let last = links - 1;
  assert_eq!(
    log.chain(last, &mut room()).unwrap().len(),
    64,
    "the chain is every revision"
  );
  let mut measured = room();
  let rebuilt = log.text(last, &mut measured).unwrap();
  assert_eq!(rebuilt.len(), text.len());
  let need = (1 << 24) - measured.left();
  // Derived: every chunk charged twice at most (its stored segment and its copy), two texts, and a revision's entry and
  // chain slot each; one text a link would be 64 texts.
  let text_len = u64::try_from(text.len()).unwrap();
  let chunks = u64::try_from(index.len()).unwrap();
  let bound = 2 * chunks + 2 * text_len + 64 * u64::from(links);
  assert!(
    need <= bound,
    "a 64-link chain's read charged {need} bytes, past {bound}"
  );
  assert!(log.text(last, &mut Admission::new(bound)).is_ok());
}

//! Golden vectors: the dumps stock `svnadmin` 1.15.0 (r1937767) wrote of one repository (A-593's `fixtures/dump`, made
//! by `fixtures/dump/make-fixtures.sh`, whose header names the command, the machine and the date), each checked against
//! its recorded SHA-256 before it is read, and `svnlook changed`'s account of every revision beside them. Stock
//! `svnadmin load` of each of them and `svnadmin dump` again gives `full.dump` byte for byte (recorded on the parked
//! branch `agents/g264-a628-svnadmin`, `fixtures/svnadmin/redumps.txt`), which is the bar the export is held to.

use std::fmt::Write as _;

use super::support::{
  ADMISSION, CALL_HEADROOM, History, VECTOR_UUID, created, export, export_all, export_deltas,
  export_in, history, import, import_whole,
};
use crate::commit::record::Record;
use crate::dump::Format;
use crate::index::{Action, ChangedPath, NodeKind};
use crate::migration::paths::{compare_paths, relative};
use crate::migration::{Importer, Importing, MigrationRefusal, Repository};

/// Format 2, every revision whole (`svnadmin dump`).
const FULL: &[u8] = include_bytes!("../../../fixtures/dump/full.dump");
/// Format 3, every revision (`svnadmin dump --deltas`).
const DELTAS: &[u8] = include_bytes!("../../../fixtures/dump/deltas.dump");
/// Format 2, revisions 0 to 3 (`svnadmin dump -r 0:3`).
const PART1: &[u8] = include_bytes!("../../../fixtures/dump/part1.dump");
/// Format 3, revisions 4 on, as changes (`svnadmin dump --deltas --incremental -r 4:13`).
const PART2: &[u8] = include_bytes!("../../../fixtures/dump/part2.dump");
/// `svnlook changed --copy-info` for every revision.
const CHANGED: &[u8] = include_bytes!("../../../fixtures/dump/changed.txt");
/// The SHA-256 of every file the script wrote.
const SUMS: &str = include_str!("../../../fixtures/dump/SHA256SUMS");

/// The last revision of `part1.dump`.
/// Format: `make-fixtures.sh` dumps `-r 0:3` into it.
const PART1_LAST: u64 = 3;

/// The vector named `name`, refused unless its SHA-256 is the one recorded when `svnadmin` wrote it.
fn vector(name: &str) -> &'static [u8] {
  let bytes = match name {
    "full.dump" => FULL,
    "deltas.dump" => DELTAS,
    "part1.dump" => PART1,
    "part2.dump" => PART2,
    "changed.txt" => CHANGED,
    other => panic!("no vector {other}"),
  };
  let recorded = SUMS
    .lines()
    .find_map(|line| {
      line
        .split_once("  ")
        .filter(|(_, file)| *file == name)
        .map(|(sum, _)| sum)
    })
    .unwrap_or_else(|| panic!("{name} is not in SHA256SUMS"));
  let sum = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
  let hex: String = sum
    .as_ref()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  assert_eq!(hex, recorded, "{name} is not the vector svnadmin wrote");
  bytes
}

/// A created repository with `dumps` imported one after the other, each whole.
fn imported(dumps: &[&str]) -> Repository {
  let mut repository = created();
  for name in dumps {
    import_whole(&mut repository, vector(name))
      .unwrap_or_else(|refusal| panic!("{name}: {refusal:?}"));
  }
  repository
}

/// `svnlook changed --copy-info`'s account of every revision of `repository` but 0, as the index records them: a
/// revision's paths in `svn_path_compare_paths` order, a directory's with a `/`, an addition `A` (`A +` and its source
/// for a copy), a deletion `D`, a replacement both, a modification `U` or `_` for its text and `U` for its properties.
fn changed_account(repository: &Repository) -> String {
  let mut account = String::new();
  for revision in 1..=repository.youngest() {
    writeln!(account, "r{revision}").expect("a line");
    let mut rows: Vec<&ChangedPath> = repository
      .index()
      .changes(revision)
      .expect("a revision")
      .iter()
      .collect();
    rows.sort_by(|first, second| {
      compare_paths(
        relative(&first.path).as_bytes(),
        relative(&second.path).as_bytes(),
      )
    });
    for row in rows {
      account_lines(&mut account, row);
    }
  }
  account
}

/// One changed path's lines in `svnlook changed`'s form.
fn account_lines(account: &mut String, row: &ChangedPath) {
  let shown = shown(&row.path, row.kind);
  match row.action {
    Action::Add => added_lines(account, row, &shown),
    Action::Delete => writeln!(account, "D   {shown}").expect("a line"),
    Action::Replace => {
      writeln!(account, "D   {shown}").expect("a line");
      added_lines(account, row, &shown);
    }
    Action::Modify => {
      let text = if row.text.is_some() { 'U' } else { '_' };
      let properties = if row.properties.is_some() { 'U' } else { ' ' };
      writeln!(account, "{text}{properties}  {shown}").expect("a line");
    }
  }
}

/// An addition's lines: `A`, or `A +` and the copy's source.
fn added_lines(account: &mut String, row: &ChangedPath, shown: &str) {
  match &row.copy_from {
    Some(source) => {
      writeln!(account, "A + {shown}").expect("a line");
      let from = self::shown(&source.path, row.kind);
      writeln!(account, "    (from {from}:r{})", source.revision).expect("a line");
    }
    None => writeln!(account, "A   {shown}").expect("a line"),
  }
}

/// An absolute path as `svnlook` shows it: relative, a directory's with a `/`, the root as `/`.
fn shown(path: &str, kind: NodeKind) -> String {
  let mut shown = relative(path).to_owned();
  if kind == NodeKind::Directory {
    shown.push('/');
  }
  shown
}

/// Holds `repository` to the history `full.dump` makes and to `full.dump`'s bytes once exported.
fn holds_the_full_history(name: &str, repository: &Repository, whole: &History) {
  assert!(history(repository) == *whole, "{name}: its history");
  assert!(
    export_all(repository).unwrap() == vector("full.dump"),
    "{name}: its export"
  );
}

/// A-628 (golden): do import every format-2 dump stock `svnadmin` wrote into a repository `CreateRepository` made, then
/// export the same range. Expect `svnadmin`'s bytes back byte for byte (`full.dump`: every revision; `part1.dump`:
/// revisions 0 to 3), the repository named by the dump's UUID.
#[test]
fn every_format_two_dump_svnadmin_wrote_is_imported_and_exported_again_byte_for_byte() {
  let full = imported(&["full.dump"]);
  assert_eq!(full.uuid().as_bytes(), VECTOR_UUID);
  assert!(
    export_all(&full).unwrap() == vector("full.dump"),
    "full.dump"
  );
  let part = imported(&["part1.dump"]);
  assert_eq!(part.youngest(), PART1_LAST);
  assert!(
    export_all(&part).unwrap() == vector("part1.dump"),
    "part1.dump"
  );
}

/// A-628 (golden): do import the history from format 3 (`deltas.dump`, every text a delta and every property section
/// a change), in two parts (`part1.dump` then `part2.dump`, the second format 3 and incremental), and as the one stream
/// the two parts make, each also a byte at a time and in pieces of 7 and 4,096 bytes. Expect every import to hold the
/// history `full.dump` makes, revision by revision, text by text, and to export as `full.dump` byte for byte.
#[test]
fn a_history_imported_from_deltas_in_parts_or_in_pieces_is_the_one_imported_whole() {
  let whole = history(&imported(&["full.dump"]));
  holds_the_full_history("deltas.dump", &imported(&["deltas.dump"]), &whole);
  holds_the_full_history(
    "part1.dump, part2.dump",
    &imported(&["part1.dump", "part2.dump"]),
    &whole,
  );
  let joined = [vector("part1.dump"), vector("part2.dump")].concat();
  for piece in [1, 7, 4096] {
    holds_the_full_history(
      "part1.dump and part2.dump as one stream",
      &imported_in_pieces(&joined, piece),
      &whole,
    );
    holds_the_full_history(
      "deltas.dump in pieces",
      &imported_in_pieces(vector("deltas.dump"), piece),
      &whole,
    );
  }
}

/// A created repository with `dump` imported, its bytes arriving `piece` at a time.
fn imported_in_pieces(dump: &[u8], piece: usize) -> Repository {
  let mut repository = created();
  import(&mut repository, dump, piece)
    .unwrap_or_else(|refusal| panic!("pieces of {piece}: {refusal:?}"));
  repository
}

/// A-628 (golden): do import `full.dump` and read every revision's changed paths. Expect the account `svnlook changed
/// --copy-info` gave of the repository `svnadmin` dumped, line for line: each path's action, its copy source, and a
/// modification's text and property flags as FSFS recorded them.
#[test]
fn every_revision_imported_records_the_changed_paths_svnlook_reported() {
  let account = changed_account(&imported(&["full.dump"]));
  assert_eq!(account, String::from_utf8_lossy(vector("changed.txt")));
}

/// A-628 (golden): do import `full.dump`, export it in two incremental parts (revisions 0 to 5, then 6 to 13), and
/// import the parts, one after the other and as one stream, into new repositories. Expect each to hold the history the
/// whole import holds, transaction names and contents included.
#[test]
fn a_history_exported_in_parts_imports_to_the_history_exported_whole() {
  let source = imported(&["full.dump"]);
  let first = export(&source, 0, 5).unwrap();
  let second = export(&source, 6, source.youngest()).unwrap();
  let mut parts = created();
  import_whole(&mut parts, &first).unwrap();
  import_whole(&mut parts, &second).unwrap();
  assert!(history(&parts) == history(&source));
  let mut joined = created();
  import_whole(&mut joined, &[first, second].concat()).unwrap();
  assert!(history(&joined) == history(&source));
}

/// A-652 (the export's units): do export the whole history stock `svnadmin` dumped, in each format, a call at a time.
/// Expect every revision written over at least its changed paths' count and two more calls (its revision record, then
/// the directories closed), revision 0 in one; no call appending more than one window of text and [`CALL_HEADROOM`],
/// though the history holds a 107 KB text; and the bytes `svnadmin dump` wrote (`full.dump`, `deltas.dump`). Before, a
/// call wrote a revision whole, then a changed path with its whole text: a revision with a large text was one unit of
/// the daemon's export (2.4 ms against a 14 µs slice, measured in debug, 2026-11-20).
#[test]
fn every_export_call_writes_one_changed_path_or_one_window_of_its_text() {
  use crate::migration::{Exporter, Exporting};
  use crate::svndiff::WINDOW_LIMIT;
  let source = imported(&["full.dump"]);
  let bound = usize::try_from(WINDOW_LIMIT).unwrap() + CALL_HEADROOM;
  for (format, expected) in [
    (Format::Two, vector("full.dump")),
    (Format::Three, vector("deltas.dump")),
  ] {
    let mut exporter = Exporter::new(&source, 0, source.youngest(), format).unwrap();
    let mut out = Vec::new();
    let mut calls = std::collections::BTreeMap::<u64, usize>::new();
    let mut largest = 0;
    loop {
      let before = out.len();
      let Exporting::Wrote(revision) = exporter.export(&source, &source, &mut out).unwrap() else {
        break;
      };
      largest = largest.max(out.len() - before);
      *calls.entry(revision).or_default() += 1;
    }
    for revision in 0..=source.youngest() {
      let least = match revision {
        0 => 1,
        _ => source.index().changes(revision).unwrap().len() + 2,
      };
      let made = calls.get(&revision).copied().unwrap_or(0);
      assert!(made >= least, "{format:?} r{revision}: {made} calls");
    }
    assert!(
      largest <= bound,
      "{format:?}: a call appended {largest} bytes"
    );
    assert!(out == expected, "{format:?}: the bytes svnadmin dump wrote");
  }
}

/// A-652 (golden): do import the history stock `svnadmin` dumped, from format 2, from format 3, and in two parts, and
/// export each repository in format 3, whole and from revision 4. Expect `svnadmin dump --deltas`'s bytes byte for byte
/// (`deltas.dump`), and `svnadmin dump --deltas --incremental -r 4:13`'s (`part2.dump`): every text the deltifier's
/// windows against its comparison, every property section of a change or a copy a change to its comparison's list.
#[test]
fn every_history_exports_in_format_three_as_svnadmin_dump_deltas_wrote_it() {
  let sources: [&[&str]; 3] = [
    &["full.dump"],
    &["deltas.dump"],
    &["part1.dump", "part2.dump"],
  ];
  for names in sources {
    let repository = imported(names);
    let whole = export_deltas(&repository).unwrap();
    assert!(whole == vector("deltas.dump"), "{names:?}: deltas.dump");
    let tail = export_in(
      &repository,
      PART1_LAST + 1,
      repository.youngest(),
      Format::Three,
    )
    .unwrap();
    assert!(tail == vector("part2.dump"), "{names:?}: part2.dump");
  }
}

/// A-652 (golden): do import `full.dump`, export it in format 3, and import that into a new repository. Expect the
/// history exported: import(export(R)) is R through deltas too.
#[test]
fn a_repository_imported_from_its_format_three_export_is_the_repository_exported() {
  let source = imported(&["full.dump"]);
  let mut again = created();
  import_whole(&mut again, &export_deltas(&source).unwrap()).unwrap();
  assert!(history(&again) == history(&source));
}

/// A-628 (golden): do import `full.dump` and `deltas.dump`, export each repository and import the export into a new
/// one. Expect the new repository's history to be the exported one's: import(export(R)) is R.
#[test]
fn a_repository_imported_from_its_own_export_is_the_repository_exported() {
  for name in ["full.dump", "deltas.dump"] {
    let source = imported(&[name]);
    let mut again = created();
    import_whole(&mut again, &export_all(&source).unwrap()).unwrap();
    assert!(history(&again) == history(&source), "{name}");
  }
}

/// What a record an import hands over names: the UUID, a revision's properties by its number, a revision by the number
/// it makes.
fn named(record: &Record) -> String {
  match record {
    Record::Created { .. } => "a creation".to_owned(),
    Record::Uuid(_) => "the UUID".to_owned(),
    Record::RevisionProperties { revision, .. } => format!("revision {revision}'s properties"),
    Record::Revision(revision) => format!("revision {}", revision.head + 1),
    Record::Locks(_) => "lock entries".to_owned(),
  }
}

/// Imports `dump` whole into a created repository, naming each record it hands over, each committed through the
/// records' own step but the revision numbered `withheld`, which the caller keeps back. The names, and how it ended.
fn handed_over(dump: &[u8], withheld: Option<u64>) -> (Vec<String>, Result<(), MigrationRefusal>) {
  let mut repository = created();
  let mut importer = Importer::new(ADMISSION);
  let mut rest = dump;
  let mut names = Vec::new();
  loop {
    let (consumed, record) = match importer.import(&mut repository, rest, true) {
      Ok(Importing::Commit { consumed, record }) => (consumed, *record),
      Ok(Importing::Done) => return (names, Ok(())),
      Ok(Importing::NeedMore { .. }) => return (names, Err(MigrationRefusal::Corrupt)),
      Err(refusal) => return (names, Err(refusal)),
    };
    rest = &rest[consumed..];
    names.push(named(&record));
    let kept_back =
      matches!(&record, Record::Revision(revision) if Some(revision.head + 1) == withheld);
    if !kept_back {
      repository.commit_record(record).unwrap();
    }
  }
}

/// A-652 part 3 (an import committed by its caller's step). Do: import `part1.dump` (revisions 0 to 3) whole, the caller
/// committing each record handed over; then again, the caller keeping revision 2 back. Expect: the records handed over in
/// the stream's order, the UUID first, then revision 0's properties, then revisions 1 to 3, each once the one before was
/// committed; and with revision 2 kept back, the import refusing revision 3 as out of its sequence (`E160006`), after
/// handing over four records.
#[test]
fn t_a652_an_import_hands_each_record_over_and_reads_on_once_it_is_committed() {
  let (names, ended) = handed_over(vector("part1.dump"), None);
  let wanted = [
    "the UUID",
    "revision 0's properties",
    "revision 1",
    "revision 2",
    "revision 3",
  ];
  assert_eq!((names, ended), (wanted.map(str::to_owned).to_vec(), Ok(())));
  let (names, ended) = handed_over(vector("part1.dump"), Some(2));
  let refused = MigrationRefusal::OutOfSequence {
    expected: 2,
    found: 3,
  };
  assert_eq!((names.len(), ended), (4, Err(refused)));
}

/// A source of texts held apart from the records, by identity.
struct HeldApart(std::collections::BTreeMap<crate::index::ContentIdentity, Vec<u8>>);

impl crate::migration::TextSource for HeldApart {
  fn text(&self, identity: &crate::index::ContentIdentity) -> Option<std::borrow::Cow<'_, [u8]>> {
    self
      .0
      .get(identity)
      .map(|text| std::borrow::Cow::Borrowed(text.as_slice()))
  }

  fn read_step(
    &self,
    identity: &crate::index::ContentIdentity,
    reading: &mut crate::migration::TextReading,
  ) -> Option<bool> {
    self.0.get(identity).map(|text| reading.copy_step(text))
  }
}

/// S4 (A-716's seam: a text held once, named by identity). Do: import `full.dump`; take a text a revision names; read
/// it through `WithTexts` over the records with a source holding it, from offset 0 and from offset 1, for the reads and
/// for the commit drive; then through a source holding nothing; and read a property list through both. Expect: the
/// text as the source holds it at each offset, by either trait; none where the source holds none, though the records
/// still hold its bytes; the property list the records' either way: a reader of the view takes texts from the lent
/// source alone.
#[test]
fn a_view_reads_texts_from_its_lent_source_alone() {
  use crate::commit::drive::Green;
  use crate::migration::WithTexts;
  use crate::reads::Repository as ReadSource;
  let repository = imported(&["full.dump"]);
  let (identity, list) = (1..=repository.youngest())
    .flat_map(|revision| repository.index().changes(revision).unwrap())
    .find_map(|change| change.text.map(|text| (text.identity, change.properties)))
    .expect("a revision names a text");
  let bytes = repository
    .text(&identity)
    .expect("the records hold it")
    .into_owned();
  let held = HeldApart(std::collections::BTreeMap::from([(
    identity,
    bytes.clone(),
  )]));
  let empty = HeldApart(std::collections::BTreeMap::new());
  let view = WithTexts {
    records: &repository,
    texts: &held,
  };
  let bare = WithTexts {
    records: &repository,
    texts: &empty,
  };
  let read =
    |source: &dyn ReadSource, offset| source.text(&identity, offset).map(|text| text.into_owned());
  assert_eq!(read(&view, 0), Some(bytes.clone()));
  assert_eq!(read(&view, 1), bytes.get(1..).map(<[u8]>::to_vec));
  assert_eq!(
    Green::text(&view, &identity).map(|text| text.into_owned()),
    Some(bytes)
  );
  assert_eq!(
    (read(&bare, 0), Green::text(&bare, &identity)),
    (None, None)
  );
  if let Some(crate::index::PropertyList::Held { identity: list, .. }) = list {
    assert_eq!(
      ReadSource::properties(&bare, &list),
      ReadSource::properties(&repository, &list)
    );
  }
}

/// Shape: the piece of a whole text the test's export writes a call, far below a window: what a slow digest's slice
/// would size it to.
const SMALL_PIECE: usize = 4096;

/// A-652 (format 2's piece sized by its caller: main's order of 2026-11-20, a call within the shard's slice). Do: export
/// the whole history in format 2 with each whole text written [`SMALL_PIECE`] bytes a call. Expect `svnadmin dump`'s
/// bytes (`full.dump`), and no call appending more than the piece and [`CALL_HEADROOM`]: the 107 KB text in pieces.
#[test]
fn a_whole_text_is_written_in_the_pieces_its_caller_sizes() {
  use crate::migration::{Exporter, Exporting};
  let source = imported(&["full.dump"]);
  let piece = std::num::NonZeroUsize::new(SMALL_PIECE).unwrap();
  let mut exporter = Exporter::new(&source, 0, source.youngest(), Format::Two)
    .unwrap()
    .with_text_piece(piece);
  let mut out = Vec::new();
  let mut largest = 0;
  loop {
    let before = out.len();
    if exporter.export(&source, &source, &mut out).unwrap() == Exporting::Done {
      break;
    }
    largest = largest.max(out.len() - before);
  }
  assert!(
    largest <= SMALL_PIECE + CALL_HEADROOM,
    "a call appended {largest} bytes"
  );
  assert!(out == vector("full.dump"), "the bytes svnadmin dump wrote");
}

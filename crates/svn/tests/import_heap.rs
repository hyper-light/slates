//! An import's texts kept off the system heap (goal 93, P7 finding (2); main 01a20182's order of 2026-11-22 05:28):
//! its own binary, so the slates binary's allocator ([`LargeMapped`]) is the global one and its figures read what the
//! import allocates on the system allocator's path, below the mapped floor, where freed blocks stay with the process.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout
)]

use std::borrow::Cow;
use std::cell::Cell;

use slates_mem::large::{LargeMapped, floor, heap};
use slates_svn::commit::drive::{StagedText, TextRefused, TextSink};
use slates_svn::dump::{Admission, Uuid};
use slates_svn::index::ContentIdentity;
use slates_svn::migration::{Importer, Importing, Repository, Room, Standalone};

#[global_allocator]
static ALLOCATOR: LargeMapped = LargeMapped;

/// Shape: the UUID the dump names.
const UUID: &str = "00000000-0000-4000-8000-000000000862";

/// Shape: a room past the dump's index and its property lists.
const ROOM: Room = Room {
  entries: 1 << 16,
  bytes: 1 << 26,
};

/// Shape: the revisions after revision 0, each the file's text whole and new.
const REVISIONS: usize = 8;

/// Shape: the dump's bytes each call of the import is handed, a daemon's copy slice: a text arrives in many pieces.
const PIECE: usize = 64 << 10;

/// A sink that holds nothing (the text's bytes counted and let go): what is left allocated is the import's own.
#[derive(Default)]
struct Passing {
  /// The next text's number.
  next: Cell<u64>,
  /// The bytes it was handed.
  bytes: Cell<u64>,
}

impl TextSink for Passing {
  fn stage(&self, _len: Option<u64>) -> Result<StagedText, TextRefused> {
    let number = self.next.get();
    self.next.set(number + 1);
    Ok(StagedText(number))
  }

  fn append(&self, _staged: StagedText, bytes: &[u8]) -> Result<(), TextRefused> {
    self.bytes.set(self.bytes.get() + bytes.len() as u64);
    Ok(())
  }

  fn seal(&self, _staged: StagedText, _identity: ContentIdentity) -> Result<(), TextRefused> {
    Ok(())
  }

  fn sealed(&self, _identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    None
  }

  fn sealed_piece(
    &self,
    _identity: &ContentIdentity,
    _offset: u64,
    _len: usize,
  ) -> Option<Vec<u8>> {
    None
  }
}

/// A format-2 dump of [`REVISIONS`] revisions after revision 0: revision 1 adds the file `f` of `size` bytes, and each
/// later one rewrites it whole, every byte its revision's.
fn dump(size: usize) -> Vec<u8> {
  let mut out = format!("SVN-fs-dump-format-version: 2\n\nUUID: {UUID}\n\n");
  let empty = "PROPS-END\n";
  out.push_str(&format!(
    "Revision-number: 0\nProp-content-length: {0}\nContent-length: {0}\n\n{empty}\n",
    empty.len()
  ));
  for number in 1..=REVISIONS {
    let date = format!("2026-11-22T00:00:00.{number:06}Z");
    let revision = format!(
      "K 10\nsvn:author\nV 5\nalice\nK 8\nsvn:date\nV {}\n{date}\nPROPS-END\n",
      date.len()
    );
    out.push_str(&format!(
      "Revision-number: {number}\nProp-content-length: {0}\nContent-length: {0}\n\n{revision}\n",
      revision.len()
    ));
    let line = format!("revision {number:08} ");
    let text: String = line.chars().cycle().take(size).collect();
    let (action, kind) = if number == 1 {
      ("add", "Node-kind: file\n")
    } else {
      ("change", "")
    };
    out.push_str(&format!(
      "Node-path: f\n{kind}Node-action: {action}\nText-content-length: {0}\nContent-length: {0}\n\n{text}\n\n",
      text.len()
    ));
  }
  out.into_bytes()
}

/// Goal 93, P7 finding (2) (main 01a20182, 05:28: the importer streams each text into its caller's store, no whole text
/// buffered by `node_record`). Do: import [`dump`]'s [`REVISIONS`] revisions of a file whose text is half the mapped
/// floor (larger than a content window, on the system allocator's path), handed to the import [`PIECE`] bytes at a time,
/// its texts put into a sink that holds none, reading the system path's figures before and after. Expect: every text's
/// bytes reached the sink, and the system path allocated less than one text over the whole import: no text held whole
/// on the system heap. Failing first: the importer read each text into a buffer of its header's length (at 2bb00e1cc,
/// measured over the same dump through `Standalone::import`, whose only text allocation was that buffer).
#[test]
fn an_import_holds_no_text_whole_on_the_system_heap() {
  let size = floor() / 2;
  let dump = dump(size);
  let uuid = Uuid::parse(UUID.as_bytes()).unwrap();
  let mut repository =
    Standalone::new(Repository::new(uuid, b"2026-11-22T00:00:00.000000Z", ROOM).unwrap());
  let mut importer = Importer::new(Admission {
    buffer_bytes: 1 << 20,
  });
  let sink = Passing::default();
  let before = heap();
  // What one reading allocates itself, taken off below.
  let reading = heap()
    .system_allocated
    .saturating_sub(before.system_allocated);
  let mut offset = 0;
  let mut wanted = PIECE;
  loop {
    let end = dump.len().min(offset + wanted);
    let (records, _) = repository.importing();
    match importer
      .import(records, &sink, &dump[offset..end], end == dump.len())
      .unwrap()
    {
      Importing::Done => break,
      Importing::NeedMore { consumed, needed } => {
        offset += consumed;
        wanted = PIECE.max(needed);
      }
      Importing::Commit { consumed, record } => {
        offset += consumed;
        wanted = PIECE;
        repository.commit_record(*record).unwrap();
      }
      Importing::Base { text, .. } => panic!("a whole-text dump asked for a base: {text:?}"),
    }
  }
  let after = heap();
  let churned = after
    .system_allocated
    .saturating_sub(before.system_allocated)
    .saturating_sub(2 * reading);
  let texts = (REVISIONS * size) as u64;
  println!(
    "an_import_holds_no_text_whole_on_the_system_heap: {REVISIONS} texts of {size} B, {texts} B in all; the system \
     path allocated {churned} B over the import; its live bytes {} B before, {} B after",
    before.system_live, after.system_live
  );
  assert_eq!(repository.youngest(), REVISIONS as u64);
  assert_eq!(
    sink.bytes.get(),
    texts,
    "every text's bytes reached the sink"
  );
  assert!(
    churned < size as u64,
    "the system path allocated {churned} B importing {REVISIONS} texts of {size} B"
  );
}

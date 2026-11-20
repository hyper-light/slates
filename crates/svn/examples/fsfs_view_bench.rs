//! The FSFS view's build, measured (A-645 P3 ruling 3's A/B): a repository whose one file of `SIZE` bytes changes a
//! line in each of `REVISIONS` revisions, imported, then its whole view laid out and every revision file read through
//! once, as a hook's `svnlook`/`svnadmin` would read it. Printed: the revisions, the view's bytes, the layout's and the
//! read's wall time. Peak memory is the process's, read by the caller (`/usr/bin/time -l`); a third argument
//! `baseline` stops after the import, so a run's peak less its baseline's is the view's own.
//!
//! Run (release): `cargo run --release -p slates-svn --example fsfs_view_bench -- REVISIONS SIZE`. The same source
//! builds against P1's view (format 6, every text whole) and P3's (format 8, every text its stored delta), which is the
//! A/B: run the two binaries interleaved.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout
)]

use std::time::Instant;

use slates_svn::dump::{Admission, Uuid};
use slates_svn::fsfs::{Advance, View};
use slates_svn::migration::{Importer, Importing, Repository, Room};

/// Shape: a room no bench history comes near.
const ROOMY: Room = Room {
  entries: 1 << 24,
  bytes: 1 << 34,
};

/// Shape: an admission past the bench's largest text.
const ADMISSION: Admission = Admission {
  buffer_bytes: 1 << 26,
};

/// Shape: the bytes a read asks for at once.
const READ_BYTES: usize = 1 << 20;

/// One property section holding `svn:log` = `message`.
fn props(message: &str) -> String {
  format!("K 7\nsvn:log\nV {}\n{message}\nPROPS-END\n", message.len())
}

/// The dump of `revisions` revisions of one file of about `size` bytes, a line changed in each.
fn dump(revisions: u64, size: usize) -> Vec<u8> {
  let line = 64usize;
  let lines = (size / line).max(1);
  let mut text: Vec<String> = (0..lines).map(|n| format!("{n:063}")).collect();
  let mut out =
    String::from("SVN-fs-dump-format-version: 2\n\nUUID: 00000000-0000-4000-8000-0000000064a5\n\n");
  let empty = "PROPS-END\n";
  out.push_str(&format!(
    "Revision-number: 0\nProp-content-length: {0}\nContent-length: {0}\n\n{empty}\n",
    empty.len()
  ));
  for revision in 1..=revisions {
    let at = usize::try_from(revision).unwrap() % lines;
    if let Some(line) = text.get_mut(at) {
      *line = format!("r{revision:062}");
    }
    let body: String = text.iter().map(|l| format!("{l}\n")).collect();
    let props = props(&format!("r{revision}"));
    out.push_str(&format!(
      "Revision-number: {revision}\nProp-content-length: {0}\nContent-length: {0}\n\n{props}\n",
      props.len()
    ));
    let action = if revision == 1 { "add" } else { "change" };
    out.push_str(&format!(
      "Node-path: file\nNode-kind: file\nNode-action: {action}\nText-content-length: {0}\nContent-length: {0}\n\n{body}\n\n",
      body.len()
    ));
  }
  out.into_bytes()
}

/// The repository `dump` makes.
fn imported(dump: &[u8]) -> Repository {
  let uuid = Uuid::parse(b"00000000-0000-4000-8000-0000000064a5").unwrap();
  let mut repository = Repository::new(uuid, b"2026-11-20T00:00:00.000000Z", ROOMY).unwrap();
  let mut importer = Importer::new(ADMISSION);
  let mut rest = dump;
  loop {
    match importer.import(&mut repository, rest, true).unwrap() {
      Importing::Done => return repository,
      Importing::NeedMore { .. } => panic!("the whole dump was given"),
      Importing::Commit { consumed, record } => {
        rest = rest.get(consumed..).unwrap_or_default();
        repository.commit_record(*record).unwrap();
      }
    }
  }
}

fn main() {
  let mut args = std::env::args().skip(1);
  let revisions: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(500);
  let size: usize = args
    .next()
    .and_then(|a| a.parse().ok())
    .unwrap_or(64 * 1024);
  let baseline = args.next().is_some_and(|a| a == "baseline");
  let repository = imported(&dump(revisions, size));
  if baseline {
    println!("revisions {revisions} size {size} baseline");
    return;
  }
  let laying = Instant::now();
  let mut view = View::new();
  while view
    .advance(repository.index(), &repository, usize::MAX)
    .unwrap()
    != Advance::Done
  {}
  let laid = laying.elapsed();
  let reading = Instant::now();
  let mut bytes = 0u64;
  let mut out = Vec::with_capacity(READ_BYTES);
  for revision in 0..=repository.youngest() {
    let file = view
      .revision_file(repository.index(), &repository, revision)
      .unwrap();
    let mut offset = 0u64;
    loop {
      out.clear();
      let read = file
        .read(&repository, offset, READ_BYTES, &mut out)
        .unwrap();
      if read == 0 {
        break;
      }
      offset += read as u64;
    }
    bytes += offset;
  }
  let read = reading.elapsed();
  println!(
    "revisions {revisions} size {size} view_bytes {bytes} layout_ms {:.1} read_ms {:.1}",
    laid.as_secs_f64() * 1e3,
    read.as_secs_f64() * 1e3
  );
}

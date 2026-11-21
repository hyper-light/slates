//! A PROPFIND's allocations against the properties it answers (MEMORY SIZING, Ada 2026-11-20: "If we don't need to
//! allocate or can architect things in such a way that we do not allocate, we do not allocate"; main's order: DAV
//! answers written into the buffer, no per-property allocations). The counting allocator counts the calling thread's
//! allocations alone, so other tests' threads never move the count.
#![allow(clippy::unwrap_used, clippy::panic, clippy::print_stdout)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_svn::dav::Bounds;
use slates_svn::dav::site::{Answer, Request, Served, answer};
use slates_svn::dump::{Admission, Uuid};
use slates_svn::lock::{AprTime, LockTable};
use slates_svn::migration::{Importer, Importing, Repository, Room};

struct Counting;
thread_local! {
  // Const initialization and no destructor: counting an allocation never allocates or reads a destroyed counter.
  static CALLS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every call is forwarded unchanged to the system allocator; the counter is a thread-local `Cell`.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    // SAFETY: forwarded unchanged.
    unsafe { System.dealloc(pointer, layout) }
  }
  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(pointer, layout, size) }
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Shape: the files the directory holds, enough that a per-property cost shows over the per-response one.
const FILES: usize = 32;

/// Shape: the UUID the dump names.
const UUID: &str = "00000000-0000-4000-8000-0000000072b2";

/// Shape: a room past the dump's index and bytes.
const ROOM: Room = Room {
  entries: 1 << 16,
  bytes: 1 << 26,
};

/// Shape: bounds past every request body here.
const BOUNDS: Bounds = Bounds {
  body_bytes: 1 << 16,
  depth: 32,
  name_bytes: 256,
  attributes: 32,
  elements: 1024,
};

/// Shape: the PROPFIND `svn info` sends: six properties of a node.
const SIX: &[u8] = b"<?xml version=\"1.0\" encoding=\"utf-8\"?><propfind xmlns=\"DAV:\"><prop>\
<resourcetype/><getcontentlength/><deadprop-count xmlns=\"http://subversion.tigris.org/xmlns/dav/\"/>\
<version-name/><creationdate/><creator-displayname/></prop></propfind>";

/// Shape: `allprop`, every live property and every one of the node's own.
const ALL: &[u8] =
  b"<?xml version=\"1.0\" encoding=\"utf-8\"?><propfind xmlns=\"DAV:\"><allprop/></propfind>";

/// A dump of revision 1 adding `/trunk` and `FILES` files beneath it, each with `svn:eol-style` and a line of text.
fn dump() -> Vec<u8> {
  let mut out = format!("SVN-fs-dump-format-version: 2\n\nUUID: {UUID}\n\n");
  let empty = "PROPS-END\n";
  out.push_str(&format!(
    "Revision-number: 0\nProp-content-length: {0}\nContent-length: {0}\n\n{empty}\n",
    empty.len()
  ));
  let revision =
    "K 10\nsvn:author\nV 5\nalice\nK 8\nsvn:date\nV 27\n2026-11-20T00:00:00.000000Z\nPROPS-END\n";
  out.push_str(&format!(
    "Revision-number: 1\nProp-content-length: {0}\nContent-length: {0}\n\n{revision}\n",
    revision.len()
  ));
  out.push_str(&format!(
    "Node-path: trunk\nNode-kind: dir\nNode-action: add\nProp-content-length: {0}\nContent-length: {0}\n\n{empty}\n\n",
    empty.len()
  ));
  let props = "K 13\nsvn:eol-style\nV 6\nnative\nPROPS-END\n";
  for file in 0..FILES {
    let text = format!("file {file}\n");
    out.push_str(&format!(
      "Node-path: trunk/f{file:02}\nNode-kind: file\nNode-action: add\nProp-content-length: {}\n\
       Text-content-length: {}\nContent-length: {}\n\n{props}{text}\n\n",
      props.len(),
      text.len(),
      props.len() + text.len()
    ));
  }
  out.into_bytes()
}

/// The repository `dump` makes.
fn imported() -> Repository {
  let uuid = Uuid::parse(UUID.as_bytes()).unwrap();
  let mut repository = Repository::new(uuid, b"2026-11-20T00:00:00.000000Z", ROOM).unwrap();
  let mut importer = Importer::new(Admission {
    buffer_bytes: 1 << 20,
  });
  let dump = dump();
  let mut rest = dump.as_slice();
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

/// The allocations this thread made answering a depth-1 PROPFIND of `/trunk` asking `body`, and the answer's bytes.
fn counted(repository: &Repository, locks: &LockTable, body: &[u8]) -> (usize, usize) {
  let served = Served {
    index: repository.index(),
    repository,
    root: "/repo",
    uuid: UUID,
    locks,
    rules: None,
    now: AprTime(1_800_000_000_000_000),
  };
  let request = Request {
    method: "PROPFIND",
    path: "/trunk",
    depth: Some("1"),
    body,
  };
  let before = CALLS.get();
  let answered = answer(&served, &request, &BOUNDS, 1 << 24);
  let calls = CALLS.get() - before;
  let Answer::Whole(whole) = answered else {
    panic!("a whole answer was due")
  };
  assert_eq!(whole.status, 207);
  (calls, whole.body.len())
}

/// MEMORY SIZING. Do: PROPFIND `/trunk` (32 files, each with a property) at depth 1, once asking `svn info`'s six
/// properties and once `allprop` (19 live properties and each file's own). Expect: `allprop`'s answer is several times
/// larger, yet it makes at most one allocation a response more than the six-property one: what a response costs does
/// not grow with the properties it answers. Before, every property's value and element was a string of its own (19
/// more a response for `allprop`).
#[test]
fn a_propfinds_allocations_do_not_grow_with_the_properties_it_answers() {
  let repository = imported();
  let locks = LockTable::new();
  // Warm once: the first answer's one-time costs (a thread-local's first use) are no one property's.
  let _ = counted(&repository, &locks, SIX);
  let (six, six_bytes) = counted(&repository, &locks, SIX);
  let (all, all_bytes) = counted(&repository, &locks, ALL);
  let responses = FILES + 1;
  println!("allocations over {responses} responses: six properties {six}, allprop {all}");
  assert!(
    all_bytes > 2 * six_bytes,
    "allprop {all_bytes} bytes, six {six_bytes}"
  );
  assert!(
    all <= six + responses,
    "allprop made {all} allocations, six properties {six}, over {responses} responses"
  );
}

//! A push's intake reads its pack's objects through buffers it reuses, never one made an object (main 01a20182's ruling
//! of 2026-11-28 01:23; GAPS' 772-787 MiB a landed git/git push kept: v2.0.0's 164,488 objects inflated into as many
//! buffers, about 2.3 GB made and freed in turn, which the system allocator answered by keeping memory). Its own binary,
//! so the slates binary's allocator ([`LargeMapped`]) is the global one and its figures read what the reader allocates.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::borrow::Cow;

use slates_git::object_name::{Kind, ObjectFormat, ObjectId};
use slates_git::unpack::{self, Contents, Keep, NoIndex, ObjectRef, Reader};
use slates_git::{Admission, Hold};
use slates_mem::large::{LargeMapped, heap};

#[global_allocator]
static ALLOCATOR: LargeMapped = LargeMapped;

/// Shape: the objects of the test's pack, each whole, as most of a history's are small (git/git v2.0.0: 158,351 of
/// 164,488 under 64 KiB).
const OBJECTS: usize = 4_000;

/// Shape: each object's length.
const OBJECT: usize = 4 << 10;

/// Shape: the bytes of the pack handed to the reader a call, as a client's arrive.
const PIECE: usize = 64 << 10;

/// A keeper that counts what it is handed and keeps nothing, giving each hold back: the reader's own allocations alone
/// are left to read.
#[derive(Default)]
struct Counted {
  objects: usize,
  bytes: usize,
}

impl Contents for Counted {
  fn content(&self, _at: usize, _id: &ObjectId) -> Option<Cow<'_, [u8]>> {
    None
  }
}

impl Keep for Counted {
  fn keep(
    &mut self,
    _at: usize,
    object: ObjectRef<'_>,
    hold: Hold,
    admission: &mut Admission,
  ) -> Result<(), unpack::Refusal> {
    self.objects += 1;
    self.bytes += object.content.len();
    admission.release(hold);
    Ok(())
  }

  fn release(&mut self, hold: Hold, admission: &mut Admission) {
    admission.release(hold);
  }
}

/// The push intake's buffers (main 01a20182's ruling of 2026-11-28 01:23). Do: make a pack of 4,000 whole objects of 4
/// KiB, each its own content, and read it a client's piece at a time into a keeper that keeps nothing, reading the
/// system allocator's figures before and after. Expect: every object handed over, their 16 MB of bytes; and the reader's
/// allocations over the read under a tenth of them: its buffers reused entry after entry, grown to the largest. Red
/// before: one buffer an object, at least the objects' bytes allocated.
#[test]
fn a_packs_objects_are_read_through_buffers_the_reader_reuses() {
  let contents: Vec<Vec<u8>> = (0..OBJECTS)
    .map(|n| {
      let mut bytes = format!("object {n}\n").into_bytes();
      bytes.resize(OBJECT, u8::try_from(n % 251).unwrap());
      bytes
    })
    .collect();
  let pack = slates_git::pack::of_whole(
    ObjectFormat::Sha1,
    contents.iter().map(|bytes| (Kind::Blob, bytes.as_slice())),
  )
  .unwrap();
  let mut reader = Reader::new(ObjectFormat::Sha1, &NoIndex);
  let mut admission = Admission::new(u64::MAX);
  let mut keeper = Counted::default();
  let before = heap();
  for piece in pack.chunks(PIECE) {
    let mut used = 0;
    while used < piece.len() {
      used += reader
        .read_into(&piece[used..], &mut admission, &mut keeper)
        .unwrap();
    }
  }
  let after = heap();
  assert!(reader.is_whole());
  assert_eq!((keeper.objects, keeper.bytes), (OBJECTS, OBJECTS * OBJECT));
  let allocated = after
    .system_allocated
    .saturating_sub(before.system_allocated);
  let handed = u64::try_from(OBJECTS * OBJECT).unwrap();
  eprintln!("the reader allocated {allocated} bytes reading {handed} bytes of objects");
  assert!(
    allocated < handed / 10,
    "the reader allocated {allocated} bytes reading {handed} bytes of objects"
  );
}

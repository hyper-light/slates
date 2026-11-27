//! A push's objects read lately kept off the system heap (main 01a20182's ruling of 2026-11-22 04:56; the 772 MiB a
//! landed git/git push kept, GAPS): their own binary, so the slates binary's allocator ([`LargeMapped`]) is the global
//! one and its live figures read what the push holds.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;

use slates_git::Admission;
use slates_git::object_name::{Kind, ObjectFormat};
use slates_git::store::{GitNaming, Incoming, Push, Widening};
use slates_mem::large::{LargeMapped, floor, heap};
use slates_vcs::ContentStore;

#[global_allocator]
static ALLOCATOR: LargeMapped = LargeMapped;

/// Shape: an object's length, small as most of a history's are (git/git v2.0.0: 158,351 of 164,488 under 64 KiB).
const OBJECT: usize = 4 << 10;

/// Shape: the reading budget the test's push is granted, about 8 MiB: what its chain asks
/// (`Recent::budget_keeping` of its depth and one times its longest object, [`OBJECT`]).
const BUDGET: usize = 8 << 20;

/// Goal 93, the push's temporaries (main 01a20182, 04:56). Do: make a push whose reads are granted about 8 MiB to keep
/// (main's order of 2026-11-26, C3 (b): the budget is what the push's admission grants for the chains it carries, never
/// its pack's length; [`granted_push`]), and keep 20,000 objects of 4 KiB as read lately, each handed over as the pack
/// reader hands it (its own buffer), three times, then one object of twice the mapped floor, reading the system
/// allocator's figures before, after the second round and at the end. Expect: what is kept held to the budget; the
/// system path's live bytes grown by less than a tenth of the budget over all of it (the index naming the objects, not
/// their bytes); and over the third round, the index at its working size by then, its churn past the test's own buffers
/// under a hundredth of the budget: what the push keeps lately costs the system heap nothing more for each object it
/// reads. Failing first: the objects kept as their own buffers, the live bytes grown by the whole budget, which, freed
/// oldest first as newer came, the system allocator kept (550-600 MB after a git/git push).
#[test]
fn a_push_keeps_its_objects_read_lately_off_the_system_heap() {
  const OBJECTS: usize = 20_000;
  let mut push = granted_push();
  let pack = push.reading_budget();
  let keep = |push: &mut Push, from: usize| {
    for n in from..from + OBJECTS {
      let bytes = vec![u8::try_from(n % 251).unwrap(); OBJECT];
      push.keep_read(u64::try_from(n * 16).unwrap(), Kind::Blob, bytes);
    }
  };
  let before = heap();
  keep(&mut push, 0);
  keep(&mut push, OBJECTS);
  let warm = heap();
  // What one reading allocates itself (its residency walk's scratch, after its counters are read), taken off below.
  let reading = heap()
    .system_allocated
    .saturating_sub(warm.system_allocated);
  keep(&mut push, 2 * OBJECTS);
  let large = vec![7u8; 2 * floor()];
  push.keep_read(u64::try_from(3 * OBJECTS * 16).unwrap(), Kind::Blob, large);
  let after = heap();
  let kept = push.kept_lately();
  assert!(kept <= pack, "kept {kept} past the budget {pack}");
  assert!(kept >= pack / 2, "kept {kept} of a budget of {pack}");
  let grown = after.system_live.saturating_sub(before.system_live);
  assert!(
    grown < u64::try_from(pack / 10).unwrap(),
    "the system path's live bytes grew by {grown} keeping {kept} bytes of objects"
  );
  let handed = u64::try_from(OBJECTS * OBJECT).unwrap();
  let churned = after
    .system_allocated
    .saturating_sub(warm.system_allocated)
    .saturating_sub(handed)
    .saturating_sub(2 * reading);
  assert!(
    churned < u64::try_from(pack / 100).unwrap(),
    "the system path allocated {churned} bytes past the objects handed over in the third round"
  );
}

/// A push granted its reading budget as a live one is (`Push::widen_reading`): its pack one object of [`OBJECT`] bytes,
/// admitted where it lies, so its longest object is [`OBJECT`]; a delta chain carried deep enough that one more times
/// that is [`BUDGET`]; then widened under an admission that grants all it asks.
fn granted_push() -> Push {
  let mut store = ContentStore::new(4 * OBJECT as u64, OBJECT).unwrap();
  let content = vec![3u8; OBJECT];
  let mut push = Push::new(ObjectFormat::Sha1);
  push.extend_pack(&mut store, &content).unwrap();
  let naming = GitNaming {
    format: ObjectFormat::Sha1,
  };
  let incoming = Incoming {
    kind: Kind::Blob,
    claimed: None,
    content: &content,
    depth: 0,
  };
  push
    .admit_placed::<_, Infallible>(&store, &naming, (incoming, 12))
    .unwrap();
  push.carried(u64::try_from(BUDGET / OBJECT - 1).unwrap());
  let widened = push.widen_reading(&mut Admission::new(u64::MAX));
  assert_eq!(widened, Widening::Grown, "granted all it asked");
  assert!(
    push.reading_budget() >= BUDGET,
    "granted {}",
    push.reading_budget()
  );
  push
}

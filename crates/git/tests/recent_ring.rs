//! A push's objects read lately kept off the system heap (main 01a20182's ruling of 2026-11-22 04:56; the 772 MiB a
//! landed git/git push kept, GAPS): their own binary, so the slates binary's allocator ([`LargeMapped`]) is the global
//! one and its live figures read what the push holds.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_git::object_name::{Kind, ObjectFormat};
use slates_git::store::Push;
use slates_mem::large::{LargeMapped, floor, heap};

#[global_allocator]
static ALLOCATOR: LargeMapped = LargeMapped;

/// Shape: an object's length, small as most of a history's are (git/git v2.0.0: 158,351 of 164,488 under 64 KiB).
const OBJECT: usize = 4 << 10;

/// Goal 93, the push's temporaries (main 01a20182, 04:56). Do: make a push whose pack is 8 MiB (its recent budget), and
/// keep 20,000 objects of 4 KiB as read lately, each handed over as the pack reader hands it (its own buffer), three
/// times, then one object of twice the mapped floor, reading the system allocator's figures before, after the second
/// round and at the end. Expect: what is kept held to the budget; the system path's live bytes grown by less than a tenth
/// of the budget over all of it (the index naming the objects, not their bytes); and over the third round, the index at
/// its working size by then, its churn past the test's own buffers under a hundredth of the budget: what the push keeps
/// lately costs the system heap nothing more for each object it reads. Failing first: the objects kept as their own
/// buffers, the live bytes grown by the whole budget, which, freed oldest first as newer came, the system allocator kept
/// (550-600 MB after a git/git push).
#[test]
fn a_push_keeps_its_objects_read_lately_off_the_system_heap() {
  const OBJECTS: usize = 20_000;
  let pack = 8usize << 20;
  let mut push = Push::new(ObjectFormat::Sha1);
  push.extend_pack(&vec![0u8; pack]);
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

//! A-727 slice a2's tests: the repository content store's windows kept as chunks of the shard's chunk store, lent for a
//! step: read back lent in place and freed with their last holder inside a lend; refused typed outside one; a lend inside a
//! lend taking nothing; and the shard's chunk store handed back holding the windows' chunks.

use std::borrow::Cow;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vcs::{ContentRefused, ContentStore};
use slates_vfs::content::ChunkStore;

use super::ChunkWindows;

/// Shape: the tests' page, and the arena's granule.
const PAGE: usize = 16 * 1024;
/// Shape: the chunk records the tests' chunk store may hold, far past what they keep.
const CHUNKS: usize = 1 << 12;
/// Shape: the content store's window, one page: an object of five pages is five windows.
const WINDOW: usize = PAGE;
/// Shape: a content store's budget, far past what the tests keep.
const BUDGET: u64 = 1 << 30;

/// A shard chunk store over one region of `pages` pages.
fn shard_chunks(pages: usize) -> ChunkStore {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * pages, PAGE, false).unwrap())
    .unwrap();
  ChunkStore::new(arena, PAGE, CHUNKS)
}

/// A content store over windows kept as chunks of a lent chunk store.
fn content() -> ContentStore {
  ContentStore::with_storage(BUDGET, WINDOW, Box::new(ChunkWindows::new(PAGE))).unwrap()
}

/// A-727 slice a2 (main's ruling (ii)). Do: lend a shard chunk store of 64 pages to a content store, put an object of
/// five windows and an empty one, read both back whole and window by window, hand the chunk store back, then lend it
/// again and release the long object. Expect: every window read lent in place from the chunks (`Cow::Borrowed`: a
/// chunk in the clear, nothing copied or allocated; Ada's ruling of 2026-11-20) and equal to
/// the bytes put; the chunk store handed back holding five chunks, and none once the long object is released in a
/// second lend; the empty object read back empty with no chunk throughout.
#[test]
fn windows_lent_the_shards_chunk_store_read_back_in_place_and_free_their_chunks() {
  let mut chunks = shard_chunks(64);
  let mut store = content();
  assert!(store.lend_in(&mut chunks));
  let long: Vec<u8> = (0..5 * WINDOW)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let root = store.put_object(&long).unwrap();
  let empty = store.put_object(&[]).unwrap();
  let len = u64::try_from(long.len()).unwrap();
  assert_eq!(store.object_bytes(&root, len), Some(long.clone()));
  assert!(
    store
      .windows(&root)
      .all(|window| matches!(window, Cow::Borrowed(_)))
  );
  assert!(store.hand_back(&mut chunks));
  assert_eq!(
    chunks.chunks(),
    5,
    "the shard's chunk store holds the windows' chunks"
  );
  assert_eq!(store.object_bytes(&empty, 0), Some(Vec::new()));
  assert!(store.lend_in(&mut chunks));
  store.release(&root).unwrap();
  assert!(store.hand_back(&mut chunks));
  assert_eq!(chunks.chunks(), 0, "the released windows' chunks are freed");
}

/// A-727 slice a2 (the lend's rules). Do: put and read on a content store no chunk store is lent to; lend one, then lend
/// a second inside the first; hand back twice. Expect: outside a lend a put refused `ContentRefused::Storage` and a
/// stored window read as none; inside, the window read; the second lend taking nothing and the first hand-back alone
/// giving the store back, so the second finds nothing to hand back.
#[test]
fn a_window_storage_refuses_typed_outside_a_lend_and_a_lend_inside_a_lend_takes_nothing() {
  let mut chunks = shard_chunks(8);
  let mut other = shard_chunks(8);
  let mut store = content();
  assert_eq!(store.put(b"too early"), Err(ContentRefused::Storage));
  assert!(store.lend_in(&mut chunks));
  let kept = store.put(b"kept").unwrap();
  assert!(
    !store.lend_in(&mut other),
    "a lend inside a lend takes nothing"
  );
  assert!(store.hand_back(&mut chunks));
  assert_eq!(store.bytes_of(&kept), None, "no read outside a lend");
  assert!(!store.hand_back(&mut chunks), "nothing more to hand back");
  assert!(store.lend_in(&mut chunks));
  assert_eq!(store.bytes_of(&kept).as_deref(), Some(&b"kept"[..]));
  assert!(store.hand_back(&mut chunks));
}

/// Ada's ruling of 2026-11-20 (a read lends a window, it allocates nothing; A-645 P3's referenced files read through
/// `ContentStore::run_at`). Do: lend a shard chunk store to a content store over these windows, put an object of five
/// windows and a one-window object, then read each from every offset a prime number of bytes apart through `run_at`,
/// and ask for the five-window object under a length of one window. Expect: every run lent in place
/// (`Cow::Borrowed`), the next bytes of the object from its offset to its window's end, so the runs make each object
/// whole; the end and a wrong length answered none.
#[test]
fn a_run_of_an_object_kept_as_chunks_is_lent_in_place_and_its_length_checked() {
  let mut chunks = shard_chunks(64);
  let mut store = content();
  assert!(store.lend_in(&mut chunks));
  let long: Vec<u8> = (0..5 * WINDOW)
    .map(|at| u8::try_from(at * 7 % 251).unwrap())
    .collect();
  let short: Vec<u8> = (0..WINDOW / 3)
    .map(|at| u8::try_from(at % 13).unwrap())
    .collect();
  for bytes in [&long, &short] {
    let identity = store.put_object(bytes).unwrap();
    let len = u64::try_from(bytes.len()).unwrap();
    let mut at = 0usize;
    while at < bytes.len() {
      let run = store
        .run_at(&identity, len, u64::try_from(at).unwrap())
        .unwrap();
      assert!(matches!(run, Cow::Borrowed(_)), "lent in place at {at}");
      assert_eq!(&run[..], &bytes[at..at + run.len()]);
      at += run.len().min(509);
    }
    assert_eq!(store.run_at(&identity, len, len), None);
  }
  let identity = store.object_id(&long);
  let one = u64::try_from(WINDOW).unwrap();
  assert_eq!(
    store.run_at(&identity, one, 0),
    None,
    "a wrong length is refused"
  );
  assert!(store.hand_back(&mut chunks));
}

/// Shape: arena-15's mean window (CI 45224166925: 131,425,429 bytes over 114,200 windows).
const MEAN_WINDOW: usize = 1_151;

/// The bytes of a window numbered `n` of `len` bytes, distinct for every `n`.
fn window_bytes(n: u64, len: usize) -> Vec<u8> {
  let mut bytes: Vec<u8> = (0..len)
    .map(|at| u8::try_from((at as u64 * 31 + n * 7) % 251).unwrap())
    .collect();
  let tag = n.to_le_bytes();
  let tagged = tag.len().min(len);
  bytes[..tagged].copy_from_slice(&tag[..tagged]);
  bytes
}

/// The arena's row of repository windows: its held and asked bytes.
fn windows_row(chunks: &ChunkStore) -> (u64, u64) {
  let owned = chunks.arena().owned(slates_mem::Owner::VcsWindows);
  (owned.held, owned.asked)
}

/// Goal 93 (main 01a20182's arena-15: 556,367,872 bytes of the arena held for 114,200 repository windows of 131,425,429
/// bytes, a page a window). Do: lend a shard chunk store to a content store and put 200 windows of arena-15's mean
/// length, 1,151 bytes, then read each back. Expect: every window read back lent in place and equal to its bytes; the
/// arena's repository windows row asked the windows' own bytes and holding at most a granule for each granule's worth of
/// whole windows (`PAGE / 1,151` a granule), not a granule a window. Failing first: 200 granules held.
#[test]
fn small_windows_share_slabs_and_hold_at_most_their_bytes_rounded_a_slab() {
  let mut chunks = shard_chunks(256);
  let mut store = content();
  assert!(store.lend_in(&mut chunks));
  let count = 200u64;
  let mut kept = Vec::new();
  for n in 0..count {
    let bytes = window_bytes(n, MEAN_WINDOW);
    kept.push((store.put(&bytes).unwrap(), bytes));
  }
  for (identity, bytes) in &kept {
    let read = store.bytes_of(identity).unwrap();
    assert!(matches!(read, Cow::Borrowed(_)), "lent in place");
    assert_eq!(&read[..], &bytes[..]);
  }
  assert!(store.hand_back(&mut chunks));
  let (held, asked) = windows_row(&chunks);
  let per_slab = u64::try_from(PAGE / MEAN_WINDOW).unwrap();
  let bound = count.div_ceil(per_slab) * u64::try_from(PAGE).unwrap();
  assert_eq!(
    asked,
    count * u64::try_from(MEAN_WINDOW).unwrap(),
    "asked the windows' bytes"
  );
  assert!(
    held <= bound,
    "{count} windows of {MEAN_WINDOW} bytes held {held} bytes, past {bound}"
  );
}

/// A seeded step source (xorshift), so a failure names its step.
struct Steps(u64);

impl Steps {
  fn next(&mut self) -> u64 {
    self.0 ^= self.0 << 13;
    self.0 ^= self.0 >> 7;
    self.0 ^= self.0 << 17;
    self.0
  }
}

/// Goal 93's slabs (main 01a20182's and w-shared's conditions). Do: on a content store lent a shard chunk store, take
/// 3,000 seeded steps: put a window of one byte to a whole window (most shorter than a granule), release one, let one
/// go (landed) and bring one back by its bytes, and every so often hand the chunk store back, release a window outside
/// the lend, purge the arena's freed bytes (A-105) and lend it again. After each step compare with a model of the
/// windows kept and let go. Expect: every window kept reads back byte for byte and every window let go reads nothing;
/// the arena's repository windows row asked exactly the kept windows' bytes and held at least that; slabs shared (fewer
/// chunks than windows, and windows let go from slabs still held); and, once every window is released, no chunk and
/// nothing held.
#[test]
fn slab_windows_read_back_byte_for_byte_through_let_go_bring_back_and_purge() {
  let mut chunks = shard_chunks(1024);
  let mut store = content();
  assert!(store.lend_in(&mut chunks));
  let mut steps = Steps(0x9e37_79b9_7f4a_7c15);
  let mut kept = KeptWindows::new();
  let (mut shared_seen, mut freed_kept_seen) = (false, false);
  for step in 0..3_000u64 {
    take_slab_step(step, &mut steps, (&mut store, &mut chunks), &mut kept);
    let asked = kept_windows_read_back(step, &store, &kept);
    let (shared, freed_kept) = windows_row_checked(step, (&mut store, &mut chunks), &kept, asked);
    shared_seen |= shared;
    freed_kept_seen |= freed_kept;
  }
  assert!(
    shared_seen && freed_kept_seen,
    "slabs shared ({shared_seen}) and kept freed slots ({freed_kept_seen})"
  );
  for (identity, (_, away)) in std::mem::take(&mut kept) {
    if !away {
      store.release(&identity).unwrap();
    }
  }
  assert!(store.hand_back(&mut chunks));
  assert_eq!(chunks.chunks(), 0, "every slab freed with its last window");
  assert_eq!(windows_row(&chunks), (0, 0), "nothing held");
}

/// The slab test's model: each window put, by its identity, with its bytes and whether it was let go.
type KeptWindows = std::collections::BTreeMap<slates_vcs::ContentId, (Vec<u8>, bool)>;

/// A window of `kept` drawn by `steps`, among those let go (`away`) or those held; none, and no draw, where there is
/// none.
fn pick_window(steps: &mut Steps, kept: &KeptWindows, away: bool) -> Option<slates_vcs::ContentId> {
  let named: Vec<slates_vcs::ContentId> = kept
    .iter()
    .filter(|(_, (_, is_away))| *is_away == away)
    .map(|(identity, _)| *identity)
    .collect();
  (!named.is_empty()).then(|| named[usize::try_from(steps.next()).unwrap() % named.len()])
}

/// One seeded step of the slab test on `store`, lent `chunks`: put a window, release one, let one go, bring one back, or
/// purge between lends; `kept` follows each.
fn take_slab_step(
  step: u64,
  steps: &mut Steps,
  (store, chunks): (&mut ContentStore, &mut ChunkStore),
  kept: &mut KeptWindows,
) {
  match steps.next() % 16 {
    0..=7 => put_window(step, steps, store, kept),
    8..=10 => release_window(steps, store, kept),
    11 | 12 => let_window_go(steps, store, kept),
    13 | 14 => bring_window_back(steps, store, kept),
    _ => purge_between_lends(steps, (store, chunks), kept),
  }
}

/// Puts window `step`, of one byte to a whole window and most shorter than a granule; `kept` holds it.
fn put_window(step: u64, steps: &mut Steps, store: &mut ContentStore, kept: &mut KeptWindows) {
  let len = if steps.next().is_multiple_of(8) {
    1 + usize::try_from(steps.next()).unwrap() % WINDOW
  } else {
    1 + usize::try_from(steps.next()).unwrap() % (2 * MEAN_WINDOW)
  };
  let bytes = window_bytes(step, len);
  let identity = store.put(&bytes).unwrap();
  kept.insert(identity, (bytes, false));
}

/// Releases a held window, where there is one; `kept` forgets it.
fn release_window(steps: &mut Steps, store: &mut ContentStore, kept: &mut KeptWindows) {
  if let Some(identity) = pick_window(steps, kept, false) {
    store.release(&identity).unwrap();
    kept.remove(&identity);
  }
}

/// Lets a held window go, landed, where there is one; `kept` marks it let go.
fn let_window_go(steps: &mut Steps, store: &mut ContentStore, kept: &mut KeptWindows) {
  if let Some(identity) = pick_window(steps, kept, false) {
    assert!(store.mark_landed(&identity));
    store.let_go(&identity).unwrap();
    kept.get_mut(&identity).unwrap().1 = true;
  }
}

/// Brings a window let go back by its bytes, where there is one; `kept` marks it held.
fn bring_window_back(steps: &mut Steps, store: &mut ContentStore, kept: &mut KeptWindows) {
  if let Some(identity) = pick_window(steps, kept, true) {
    let bytes = kept[&identity].0.clone();
    store.bring_back(&identity, &bytes).unwrap();
    kept.get_mut(&identity).unwrap().1 = false;
  }
}

/// Hands the chunk store back, releases a held window outside the lend, purges the arena's freed bytes (A-105) and
/// lends the chunk store again.
fn purge_between_lends(
  steps: &mut Steps,
  (store, chunks): (&mut ContentStore, &mut ChunkStore),
  kept: &mut KeptWindows,
) {
  assert!(store.hand_back(chunks));
  release_window(steps, store, kept);
  while chunks.arena_mut().purge(usize::MAX).more {}
  assert!(store.lend_in(chunks));
}

/// At `step`, every window `kept` holds reads back byte for byte and every window let go reads nothing: the bytes the
/// held windows ask.
fn kept_windows_read_back(step: u64, store: &ContentStore, kept: &KeptWindows) -> u64 {
  let mut asked = 0u64;
  for (identity, (bytes, away)) in kept {
    let read = store.bytes_of(identity);
    if *away {
      assert_eq!(read, None, "step {step}: a window let go reads nothing");
    } else {
      assert_eq!(
        read.as_deref(),
        Some(&bytes[..]),
        "step {step}: a window kept reads back"
      );
      asked += u64::try_from(bytes.len()).unwrap();
    }
  }
  asked
}

/// At `step`, with the chunk store handed back, the arena's repository windows row asked exactly `asked`, the kept
/// windows' bytes, and held at least that; lent again after. Whether slabs were shared (fewer slabs than windows held)
/// and whether a slab still held kept a slot let go.
fn windows_row_checked(
  step: u64,
  (store, chunks): (&mut ContentStore, &mut ChunkStore),
  kept: &KeptWindows,
  asked: u64,
) -> (bool, bool) {
  assert!(store.hand_back(chunks));
  let (held, row_asked) = windows_row(chunks);
  let census = super::chunk_windows_census(store);
  let windows = kept.values().filter(|(_, away)| !away).count();
  let seen = (
    census.slabs < u64::try_from(windows).unwrap(),
    census.freed_kept > 0,
  );
  assert_eq!(
    row_asked, asked,
    "step {step}: asked the kept windows' bytes"
  );
  assert!(
    held >= asked,
    "step {step}: held {held} under asked {asked}"
  );
  assert!(store.lend_in(chunks));
  seen
}

/// Goal 93's slabs across a restart (w-shared's condition (c); and main's Windows red of 2026-11-23, w-win2's run
/// 45269168513: a window in a chunk of its own whose block the seal trimmed was refused `Malformed` at every restart,
/// since its record names a head shorter than its bytes, so a Windows shard, whose granule is a 4 KiB page, lost every
/// window of a page or more). Do: lend a shard chunk store, put 40 windows of arena-15's mean length, one of a whole
/// window and one of [`TRIMMED_WINDOW`], take the shard image's window pieces, then restore them into a fresh arena
/// over a region of the same layout, copy the arena's bytes across as the anchor's content object carries them, and
/// read every window back; restore also a piece of the version before the slabs. Expect: each slab's block claimed
/// once (as many chunks as the first store held, fewer than the windows); the old piece refused typed (`OldVersion(1)`),
/// counted, the only piece refused; every window's piece encoded again from the restored windows as it was, the
/// trimmed one's naming a head shorter than its bytes; every window read back byte for byte in its slot (the trimmed
/// one's tail lies past the head the test can copy, so its piece holds it); and the restored row asking the windows'
/// bytes. Failing first: the trimmed window's piece refused too, two refused.
#[test]
fn a_restart_claims_each_slab_once_and_reads_its_windows_in_their_slots() {
  use slates_vcs::WindowStorage;
  let mut chunks = shard_chunks(64);
  let mut windows = ChunkWindows::new(PAGE);
  assert!(windows.lend_in(&mut chunks));
  let put = put_restart_windows(&mut windows);
  assert!(windows.hand_back(&mut chunks));
  let held_chunks = chunks.chunks();
  assert!(
    held_chunks < put.len(),
    "{held_chunks} chunks for {} windows",
    put.len()
  );
  let mut pieces = windows.pieces_in(0, u64::MAX);
  let mut old = pieces[0].1.clone();
  old[0] = 1;
  pieces.push((u64::MAX - 1, old.clone()));
  let mut successor = shard_chunks(64);
  let (mut restored, refused) = ChunkWindows::restore(PAGE, &pieces, &mut successor);
  assert_eq!(refused, 1, "the old piece alone refused");
  let trimmed = pieces
    .iter()
    .filter_map(|(_, bytes)| super::read_piece(bytes).ok())
    .filter_map(|(_, _, chunk)| chunk)
    .any(|chunk| u64::from(chunk.used) > chunk.block);
  assert!(trimmed, "a piece names a trimmed chunk's head");
  let original: Vec<(u64, Vec<u8>)> = pieces[..pieces.len() - 1].to_vec();
  assert_eq!(
    restored.pieces_in(0, u64::MAX),
    original,
    "every piece encoded again as it was"
  );
  assert_eq!(
    super::read_piece(&old),
    Err(super::PieceRefused::OldVersion(1))
  );
  assert_eq!(
    successor.chunks(),
    held_chunks,
    "each slab's block claimed once"
  );
  carry_slab_blocks(&put, (&windows, &chunks), (&restored, &mut successor));
  read_back_in_slots(&put, &mut restored, &mut successor);
  let asked: usize = put.iter().map(|(_, bytes)| bytes.len()).sum();
  assert_eq!(
    windows_row(&successor).1,
    u64::try_from(asked).unwrap(),
    "the restored row asks the windows' bytes"
  );
}

/// Shape: a window of two and a half pages: a chunk of its own whose block the seal trims (A-118), its record naming a
/// head shorter than its bytes.
const TRIMMED_WINDOW: usize = 2 * PAGE + PAGE / 2;

/// Puts 40 windows of arena-15's mean length, one of a whole window and one of [`TRIMMED_WINDOW`] into `windows`, lent
/// its chunk store, each named by an identity of its number: each window and its bytes.
fn put_restart_windows(windows: &mut ChunkWindows) -> Vec<(slates_vcs::StoredWindow, Vec<u8>)> {
  use slates_vcs::WindowStorage;
  let mut put = Vec::new();
  for n in 0..42u64 {
    let len = match n {
      40 => WINDOW,
      41 => TRIMMED_WINDOW,
      _ => MEAN_WINDOW,
    };
    let bytes = window_bytes(n, len);
    let window = windows.put(&bytes).unwrap();
    let identity = slates_vcs::ContentId([u8::try_from(n).unwrap(); 32]);
    windows.named(window, &identity);
    put.push((window, bytes));
  }
  put
}

/// Copies the block each of `put`'s windows lies in from the first arena into the restored one, as the anchor's content
/// object carries the arena's bytes across a restart.
fn carry_slab_blocks(
  put: &[(slates_vcs::StoredWindow, Vec<u8>)],
  (windows, chunks): (&ChunkWindows, &ChunkStore),
  (restored, successor): (&ChunkWindows, &mut ChunkStore),
) {
  for (window, _) in put {
    let block_of = |held: &ChunkWindows, store: &ChunkStore| {
      let (chunk, _) = held.held[window].placed.unwrap().slot().unwrap();
      store.chunk(chunk).unwrap().block
    };
    let from = chunks
      .arena()
      .bytes(block_of(windows, chunks))
      .unwrap()
      .to_vec();
    let into = block_of(restored, successor);
    successor
      .arena_mut()
      .bytes_mut(into)
      .unwrap()
      .copy_from_slice(&from);
  }
}

/// Each of `put`'s windows read back from `restored`, lent `successor`, byte for byte in its slot.
fn read_back_in_slots(
  put: &[(slates_vcs::StoredWindow, Vec<u8>)],
  restored: &mut ChunkWindows,
  successor: &mut ChunkStore,
) {
  use slates_vcs::WindowStorage;
  assert!(restored.lend_in(successor));
  // A trimmed window's tail lies past the head this test can copy across; its placement is held by its piece.
  for (window, bytes) in put.iter().filter(|(_, bytes)| bytes.len() <= WINDOW) {
    let read = restored.read(*window).unwrap();
    assert_eq!(
      &read[..],
      &bytes[..],
      "window {window:?} read back in its slot"
    );
  }
  assert!(restored.hand_back(successor));
}

//! A work's first edit of a large file costs what it wrote, never the file (§4.16, D-5: clones share what they did not
//! change). A one-byte edit of a base file holds the byte and the pieces that name the base's unchanged ranges; the
//! base's bytes stay the green's, pinned at the work's base version (`crate::work_files`). N works each making a
//! one-byte edit to one G-byte file add O(N x metadata) resident bytes, never N x G.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use std::time::{Duration, Instant};

use slates_client::{Client, Submitted};
use slates_ipc::protocol::{ReadAt, VolumeId};
use slates_machine::facts::Facts;
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::client::connect;

/// Shape: the one large file's bytes, large enough that one copy of it per work dwarfs a work's own metadata.
/// `SLATES_WORK_EDIT_COW_FILE_BYTES` overrides it for the measured sizes (BENCHMARKS).
const FILE_BYTES: usize = 64 * 1024 * 1024;
/// Shape: the works that each edit one byte of it (`SLATES_WORK_EDIT_COW_WORKS`).
const WORKS: usize = 8;
/// Bound: one work's own resident bytes after a one-byte edit: its catalog record, floor, journal entry, the byte and
/// the pieces naming the base's ranges, and the reply buffers its verbs touched. Generous; no term for the file.
const WORK_METADATA_BYTES: usize = 64 * 1024;
/// Bound: what the run may add whatever the works do (allocator page granularity, a map's growth): fixed.
const FIXED_BYTES: usize = 8 * 1024 * 1024;
/// Shape: where in the file each work writes: the middle, so the edit splits the base into two ranges.
const AT_DIVISOR: usize = 2;

fn daemon() -> (Daemon, String) {
  daemon_named("srv-work-edit-cow")
}

/// A daemon under `tag` and this process's id, so the file's tests run side by side.
fn daemon_named(tag: &str) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("{tag}-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(1)),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-{instance}"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn named(key: &str, default: usize) -> usize {
  std::env::var(key)
    .ok()
    .map_or(default, |value| value.parse().unwrap())
}

/// A green holding one file of `file_bytes`, landed by one seed work.
fn green_with_one_file(client: &mut Client, file_bytes: usize) -> VolumeId {
  let green = client.create_green("edit-cow-green", false).unwrap();
  let (seed, _) = client.create_work(green, "edit-cow-seed").unwrap();
  let bytes: Vec<u8> = (0..file_bytes)
    .map(|index| u8::try_from(index % 251).unwrap())
    .collect();
  client.edit(seed, "big", 0, 0, &bytes).unwrap();
  match client.submit(seed).unwrap() {
    Submitted::Accepted(_) => green,
    other => panic!("the seed's submit was not accepted: {other:?}"),
  }
}

/// One agent's first edit: request a work, overwrite one byte in the middle of the large file. The time it took.
fn first_edit(
  client: &mut Client,
  green: VolumeId,
  file_bytes: usize,
  index: usize,
) -> (VolumeId, Duration) {
  let started = Instant::now();
  let (work, _) = client
    .create_work(green, &format!("edit-cow-work-{index}"))
    .unwrap();
  let at = u64::try_from(file_bytes / AT_DIVISOR).unwrap();
  client.edit(work, "big", at, 1, b"w").unwrap();
  (work, started.elapsed())
}

fn resident() -> usize {
  usize::try_from(Facts::resident_now().expect("this OS reports resident memory")).unwrap()
}

/// The bytes live allocations hold now, as the allocator counts them (`slates_mem::heap`): unlike resident memory, a
/// freed block the allocator keeps for reuse is not counted, so a copy made into one shows.
fn live_heap() -> usize {
  usize::try_from(slates_mem::heap::stats().in_use).unwrap()
}

/// §4.16, D-5: N works over a green with one G-byte file, each overwriting one byte of it, add live heap bytes (and
/// resident bytes) bounded by N x a work's metadata plus a fixed term, below even one copy of the file; each work reads
/// its edit back where it made it, and the green's bytes around it.
#[test]
fn a_one_byte_edit_of_a_large_file_costs_the_byte_not_the_file() {
  let file_bytes = named("SLATES_WORK_EDIT_COW_FILE_BYTES", FILE_BYTES);
  let works = named("SLATES_WORK_EDIT_COW_WORKS", WORKS);
  let (_daemon, instance) = daemon();
  let mut client = connect(&instance);
  let green = green_with_one_file(&mut client, file_bytes);
  // One work first, so what a first work alone sets up is not charged to the N.
  let (_, warm) = first_edit(&mut client, green, file_bytes, works);
  let (before, heap_before) = (resident(), live_heap());
  let made: Vec<(VolumeId, Duration)> = (0..works)
    .map(|index| first_edit(&mut client, green, file_bytes, index))
    .collect();
  let added = resident().saturating_sub(before);
  let heap_added = live_heap().saturating_sub(heap_before);
  let bound = works * WORK_METADATA_BYTES + FIXED_BYTES;
  let times: Vec<Duration> = made.iter().map(|(_, took)| *took).collect();
  let total: Duration = times.iter().sum();
  println!(
    "work_edit_cow: one file of {file_bytes} B; {works} works, each request -> one-byte edit: live heap +{heap_added} B \
     ({} B/work), resident +{added} B, bound {bound} B; time warm {warm:?}, mean {:?}, slowest {:?}",
    heap_added / works,
    total / u32::try_from(works).unwrap(),
    times.iter().max().unwrap(),
  );
  let at = file_bytes / AT_DIVISOR;
  let (work, _) = made[0];
  let seen = client.read(work, "big", ReadAt::Head).unwrap();
  assert_eq!(seen.len(), file_bytes, "the edit kept the file's length");
  assert_eq!(seen[at], b'w', "the work reads its own byte");
  assert_eq!(
    seen[at - 1],
    u8::try_from((at - 1) % 251).unwrap(),
    "and the green's before it"
  );
  assert_eq!(
    seen[at + 1],
    u8::try_from((at + 1) % 251).unwrap(),
    "and after it"
  );
  drop(seen);
  assert!(
    bound < file_bytes,
    "the bound must rule out even one copy of the file"
  );
  assert!(
    heap_added <= bound,
    "{works} one-byte edits of a {file_bytes}-byte file added {heap_added} live heap bytes, past {bound}: a work copied \
     the file"
  );
  assert!(
    added <= bound,
    "{works} one-byte edits of a {file_bytes}-byte file added {added} resident bytes, past {bound}: a work copied the \
     file"
  );
}

/// Shape: the large file the work-log test rebases a small edit of: 1 MiB, so a whole copy dwarfs the edit.
const LOG_FILE_BYTES: usize = 1024 * 1024;
/// Shape: the bytes the work writes into it, a load slot's (`SLOT_BYTES` in `crates/client/examples/load_bench.rs`).
const LOG_EDIT_BYTES: usize = 64;
/// Bound: one work's log after its rebase, beside what it wrote: the restatement's framing, the file's path, the
/// pieces naming the base's ranges around the write (three at most) and the journal's operations. Fixed, no term for
/// the file.
const LOG_FRAMING_BYTES: u64 = 1024;

/// A-806 (goal 86b: at 100,000 clients over the Linux kernel one work's log reached 300,898,443 bytes and filled its
/// partition's merge budget; at 1,000 clients over `asic_reg`'s 1 MB headers the largest log was 35,249,811 bytes in 8
/// entries for 64-byte edits). Do: on a green holding a 1 MiB file and a small one, a work overwrites 64 bytes of the
/// large file; another work changes the small file and submits; the first work rebases. Expect: the work's log
/// is at most what the work wrote and a fixed framing, with no term for the file. Before, a rebase answered the work's changed file whole, and its restatement logged it whole.
#[test]
fn a_rebased_works_log_holds_what_it_wrote_not_the_file() {
  let (daemon, instance) = daemon_named("srv-work-log-bound");
  let mut client = connect(&instance);
  let green = client.create_green("log-bound-green", false).unwrap();
  let (seed, _) = client.create_work(green, "log-bound-seed").unwrap();
  let bytes: Vec<u8> = (0..LOG_FILE_BYTES)
    .map(|index| u8::try_from(index % 251).unwrap())
    .collect();
  client.edit(seed, "big", 0, 0, &bytes).unwrap();
  client.edit(seed, "small", 0, 0, b"small").unwrap();
  assert!(matches!(
    client.submit(seed).unwrap(),
    Submitted::Accepted(_)
  ));
  let (work, _) = client.create_work(green, "log-bound-work").unwrap();
  let at = u64::try_from(LOG_FILE_BYTES / AT_DIVISOR).unwrap();
  let written = vec![b'w'; LOG_EDIT_BYTES];
  let edit_len = u64::try_from(LOG_EDIT_BYTES).unwrap();
  client.edit(work, "big", at, edit_len, &written).unwrap();
  let (other, _) = client.create_work(green, "log-bound-other").unwrap();
  client.edit(other, "small", 0, 5, b"SMALL").unwrap();
  assert!(matches!(
    client.submit(other).unwrap(),
    Submitted::Accepted(_)
  ));
  assert!(matches!(
    client.rebase(work).unwrap(),
    slates_client::Rebased::Rebased(_)
  ));
  // The work's own log, read where it is held (the one shard of this daemon): the seed's log, which wrote the whole
  // file, is not this work's.
  let id = slates_db::catalog::VolumeId { bytes: work.bytes };
  let peak = daemon
    .observation(
      daemon.shards().first().copied(),
      slates_server::daemon::OBSERVE_BUDGET_NS,
      move |state: &mut slates_server::state::ShardState| {
        state
          .db
          .partition()
          .work_log(id)
          .iter()
          .map(Vec::len)
          .sum::<usize>()
      },
    )
    .unwrap()
    .wait()
    .unwrap();
  let peak = u64::try_from(peak).unwrap();
  let whole = client.read(work, "big", ReadAt::Head).unwrap();
  let start = usize::try_from(at).unwrap();
  let read = whole[start..start + LOG_EDIT_BYTES].to_vec();
  drop(client);
  daemon.stop();
  assert_eq!(read, written, "the rebased work still reads its write");
  assert!(
    peak <= edit_len + LOG_FRAMING_BYTES,
    "the rebased work's log held {peak} bytes for a {LOG_EDIT_BYTES}-byte write into a {LOG_FILE_BYTES}-byte file"
  );
}

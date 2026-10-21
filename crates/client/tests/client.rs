//! The client's tests (Phase 2 task 5; §4.4, §4.7, §4.9, AC-2.3's idempotency half): the
//! typed verbs over an in-process daemon, and a session that outlives a daemon restart: the
//! same client reconnects under its id and its retry meets the completion record; the
//! volume and its snapshot are recovered from anchor-owned RAM; a second client cannot take a
//! live session.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn
)]

use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use slates_anchor::AnchorSegment;
use slates_client::{
  Client, ClientError, CreateSpec, Deadlines, Intent, NamePolicy, SizeClass, Submitted,
};
use slates_ipc::protocol::{Refusal, RequestBody};
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the tests' machine profile (milliseconds). The profile is
/// measured once per test process ([`profile`]), so the budget is paid once.
const PROBE_MS: u64 = 5;
/// Shape: shards per test daemon: two, so a client's shard and the control shard differ.
const TEST_SHARDS: u16 = 2;
/// Shape: how long a client retries the rendezvous while a daemon starts.
const START_WAIT: Duration = Duration::from_secs(5);

/// The machine profile this binary's daemons derive from, measured once per process (§4.1), as
/// production measures once at the anchor's boot. Measured afresh per daemon, the wake probe ran beside
/// the other tests' probes and daemons and measured their load, or measured nothing and was refused
/// (`docs/bugs/2026-09-25-test-fixtures-measured-the-machine-beside-each-other.md`).
fn profile() -> MachineProfile {
  let measured = measured_profile();
  // This binary starts daemons of one and two shards, and `hyper_seal`'s key region is made **once per
  // process** by whichever starts first and never grows. A one-shard daemon winning that race locked 512
  // slots and left the two-shard sealing test refused the 1024 it needed — which is why that test failed
  // under a full `cargo test --workspace` and passed alone. Reserving for the largest daemon here, before
  // any of them starts, takes the order out of it for every test in the binary, not only the one that
  // asserts on sealing (`slates_server::seal_keys::reserve`).
  static RESERVED: OnceLock<bool> = OnceLock::new();
  let _ = RESERVED.get_or_init(|| {
    let largest = DaemonConfig::derive(&measured, "cl-reserve", Some(TEST_SHARDS));
    slates_server::seal_keys::reserve(largest.seal_key_slots)
  });
  measured
}

/// The measurement itself, once per process.
fn measured_profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures")
}

/// The product's own deadlines (`Deadlines::derive` over the anchor's liveness budget and the recovery
/// budget), as the CLI and `reap.rs` use them. A shorter hand-picked reply deadline (200 ms until
/// 2026-09-28) called a daemon stalled that its own contract still counted alive: on a loaded CI runner
/// the first `create` after a start had no reply within 200 ms
/// (`docs/bugs/2026-09-28-the-client-tests-judged-a-live-daemon-by-a-shorter-clock.md`).
fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::hint::spin_loop();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

fn scratch(name: &str) -> CreateSpec {
  CreateSpec {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

fn wait_until_listed(client: &mut Client, names: &[&str]) {
  let started = Instant::now();
  loop {
    let mut listed: Vec<String> = client
      .list()
      .unwrap()
      .iter()
      .map(|v| v.name.clone())
      .collect();
    listed.sort();
    if listed == names {
      return;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "listing settles to {names:?}: {listed:?}"
    );
  }
}

/// Create, the duplicate refused, snapshot, clone; the origin's id.
fn create_snapshot_clone(client: &mut Client) -> slates_client::VolumeId {
  let id = client.create(&scratch("one")).unwrap();
  assert_eq!(
    client.create(&scratch("one")),
    Err(ClientError::Refused(Refusal::AlreadyExists {
      existing: id
    }))
  );
  let snapshot = client.snapshot(id).unwrap();
  let clone = client.clone_snapshot(id, snapshot, "one-clone").unwrap();
  assert_ne!(clone, id);
  id
}

/// The register at f=0 (§4.8 task 7): the head is placed on the local append, the host epoch
/// is the first, no mirror exists; `await placed(region)` returns and the mirror is refused.
fn assert_register_at_f0(
  client: &mut Client,
  id: slates_client::VolumeId,
  report: &slates_client::StatusReport,
) {
  assert!(report.placed.region, "the head is placed at f=0");
  assert_eq!(report.placed.host_epoch, 1);
  assert_eq!(report.placed.mirror_age_ns, None);
  assert_eq!(
    client
      .await_placed(id, None, slates_client::Scope::Region)
      .unwrap(),
    (true, None),
    "await placed(region) is the local append"
  );
  assert_eq!(
    client.await_placed(id, None, slates_client::Scope::Mirror),
    Err(slates_client::ClientError::Refused(Refusal::Unsupported {
      feature: "mirror".to_owned()
    })),
    "no mirror on a laptop"
  );
}

/// Attach for writing takes the lease; status shows it; detach releases it.
fn attach_status_detach(client: &mut Client, id: slates_client::VolumeId) {
  let attached = client.attach(id, None, Intent::Write).unwrap();
  assert_eq!(attached.lease_epoch, Some(1));
  let report = client.status(id).unwrap();
  assert_eq!(report.name, "one");
  assert_eq!(report.attachments, 1);
  assert_eq!(report.snapshots, 1);
  assert_register_at_f0(client, id, &report);
  wait_until_listed(client, &["one", "one-clone"]);
  client.detach(attached.attachment).unwrap();
  assert_eq!(client.status(id).unwrap().lease_epoch, None);
}

/// AC-A-135 (§4.6 the same-kernel direct map): a `MapRead` returns the file's bytes whichever path it
/// takes. Do: a one-shard daemon (so the volume's owner shard is the client's home shard), write an
/// arena-backed file into a plain volume, then `map_read` it. Expect: the bytes equal what was written.
///
/// Which path it takes is reported, not asserted, because it depends on something outside this test:
/// the hand-off is refused unless the descriptor authorizes exactly the window, and no backing
/// satisfies that yet — a shard's content arena is one object holding every volume (the measured
/// escape of 2026-10-18, `docs/bugs/2026-10-18-a-read-handed-the-peer-the-whole-content-arena.md`).
/// The rule the descriptor must meet is held by
/// `a_zero_copy_reads_descriptor_authorizes_its_window_and_nothing_more`, which passes on either path;
/// asserting the path here instead would pin today's state in place.
#[cfg(unix)]
#[test]
fn a_same_kernel_client_maps_a_file_with_no_copy() {
  let profile = profile();
  let instance = format!("cl-dax-{}", std::process::id());
  // One shard, so owner == home: the cross-shard owner→home descriptor hand-off is a later step.
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-dax-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  let mut client = connect(&instance);

  let id = client.create(&scratch("dax")).unwrap();
  // Large enough to live in the content arena as a shared-object-backed block (not held inline), so a
  // same-kernel client can map it; one chunk, so one block.
  let bytes: Vec<u8> = (0..(20usize << 10))
    .map(|i| u8::try_from(i % 251).unwrap_or(0))
    .collect();
  let attached = client.attach(id, None, Intent::Write).unwrap();
  client
    .fs_write((id, attached.attachment), "/big.bin", &bytes, 0o644)
    .unwrap();
  client.detach(attached.attachment).unwrap();

  let mapped = client.map_read(id, "/big.bin").unwrap();
  // The hand-off is a Linux control-socket feature and is **owed on macOS** (the daemon answers
  // `Unsupported { feature: "same-kernel direct map (macOS control socket owed)" }`); on Linux it is
  // built but refused today for want of a confined backing. Either way the client gets its bytes, and
  // which path ran is recorded here rather than asserted.
  println!(
    "map_read path: {}",
    if mapped.is_zero_copy() {
      "zero-copy"
    } else {
      "copy"
    }
  );
  assert_eq!(
    mapped.bytes(),
    bytes.as_slice(),
    "the mapped bytes equal the written file, byte for byte"
  );

  client.destroy(id).unwrap();
}

/// Shape: the neighbour volume's marker — a byte run written into no other volume, so finding it
/// inside a descriptor proves that descriptor reaches content the peer never asked for.
#[cfg(target_os = "linux")]
const NEIGHBOUR_MARKER: &[u8; 16] = b"SL-NEIGHBOUR-SEC";

/// Shape: a file large enough that its content lives in the shared-object content arena rather than
/// inline in its inode, which is the only content a zero-copy read can hand over (§4.6, A-135).
#[cfg(target_os = "linux")]
const ARENA_BACKED_BYTES: usize = 20 << 10;

/// This process's descriptors that name a kernel memory object (`/memfd:…`) **of the daemon named by
/// `instance`**, by descriptor number. The daemon in these tests runs in this process, so its own
/// descriptors are in the set; the probe takes the set before a map and the set after, and what is new
/// is what the hand-off installed.
///
/// `instance` is not an optimisation, it is what makes the difference attributable. This test binary
/// runs its tests on parallel threads against one descriptor table, so an unfiltered scan reports every
/// other test's objects too, and `after - before` reports whatever a sibling happened to open while this
/// test ran. That is exactly how the ubuntu gates lane went red on 2026-10-20: thirteen descriptors
/// "arrived" on the copy path and every one of them was named `cl-resume-slow-1`, a different test's
/// daemon, with zero neighbour markers reachable through any of them. A descriptor this hand-off
/// installed necessarily names this daemon's own segment or content object, so filtering to this
/// instance keeps the whole property and drops only the noise. `crates/server/tests/recovery.rs`
/// (`occurrences_in_content`, `allocated_in_content`) already scoped its scans by object name; this was
/// the one that did not.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // reading this process's own descriptor table through /proc, read-only
fn memory_object_descriptors(instance: &str) -> std::collections::BTreeSet<i32> {
  let mut found = std::collections::BTreeSet::new();
  let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
    return found;
  };
  for entry in entries.flatten() {
    let target = std::fs::read_link(entry.path()).unwrap_or_default();
    let named = target.to_string_lossy();
    if !named.contains("memfd:") || !named.contains(instance) {
      continue;
    }
    if let Some(number) = entry
      .file_name()
      .to_string_lossy()
      .parse::<i32>()
      .ok()
      .filter(|number| *number >= 0)
    {
      found.insert(number);
    }
  }
  found
}

/// How many times `needle` occurs in the object `descriptor` names. Only the object's data ranges are
/// read (`SEEK_DATA`/`SEEK_HOLE`, which the memory filesystem answers), so an object with unwritten
/// pages costs nothing to scan and none of its holes are committed by the scan itself.
#[cfg(target_os = "linux")]
fn occurrences_through(descriptor: std::os::fd::BorrowedFd<'_>, needle: &[u8]) -> usize {
  let mut count = 0;
  let mut at: u64 = 0;
  while let Ok(data) = rustix::fs::seek(descriptor, rustix::fs::SeekFrom::Data(at)) {
    let end = rustix::fs::seek(descriptor, rustix::fs::SeekFrom::Hole(data)).unwrap_or(data);
    let span = usize::try_from(end.saturating_sub(data)).unwrap_or(0);
    let mut range = vec![0u8; span];
    let read = rustix::io::pread(descriptor, &mut range, data).unwrap_or(0);
    count += range
      .get(..read)
      .unwrap_or_default()
      .windows(needle.len())
      .filter(|window| *window == needle)
      .count();
    if end <= at {
      break;
    }
    at = end;
  }
  count
}

/// How many times `needle` occurs in any kernel memory object this process holds, the daemon's own
/// content object included (the daemon in these tests runs in this process). The non-vacuity side of
/// the probe: it says the marker is findable at all, so a clean scan of a handed descriptor is a fact
/// about that descriptor.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // reading this process's own descriptor table through /proc, read-only
fn occurrences_in_this_process(needle: &[u8], instance: &str) -> usize {
  use std::os::fd::AsFd;

  let mut total = 0;
  let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
    return 0;
  };
  for entry in entries.flatten() {
    let target = std::fs::read_link(entry.path()).unwrap_or_default();
    let named = target.to_string_lossy();
    // Scoped to this test's own daemon for the same reason `memory_object_descriptors` is: the claim
    // being made is about this daemon's content object, and a parallel sibling's multi-GiB objects are
    // neither part of it nor free to scan.
    if !named.contains("memfd:") || !named.contains(instance) {
      continue;
    }
    let Ok(file) = std::fs::File::open(entry.path()) else {
      continue;
    };
    total += occurrences_through(file.as_fd(), needle);
  }
  total
}

/// What an unexpected descriptor actually is, so a failure here can be acted on without a second run: the
/// object `/proc/self/fd/N` names, its size, and whether the neighbour's marker is reachable through it.
///
/// That last part is the one that matters. The probe above counts *every* memory object this process holds,
/// because the daemon runs in it, so a new one appearing between two samples is not by itself the arena
/// crossing into the peer — the pool growing for its own reasons would look the same. Reachability of the
/// marker is what separates the two, and a message that reports it turns one red CI run into the answer
/// rather than into a question. Added 2026-10-20, when this assertion fired on a GitHub runner and on no
/// machine here: the laptop's Docker Linux, the x86_64 box (32 cores, 124 GB) and the full test binary on
/// both passed it three times each.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // reading this process's own descriptor table through /proc, read-only
fn describe_descriptor(number: i32, needle: &[u8]) -> String {
  use std::os::fd::AsFd;

  let path = format!("/proc/self/fd/{number}");
  let target = std::fs::read_link(&path)
    .map(|link| link.to_string_lossy().into_owned())
    .unwrap_or_else(|_| "<unreadable link>".to_owned());
  let Ok(file) = std::fs::File::open(&path) else {
    return format!("fd {number} -> {target} (could not be reopened to scan)");
  };
  let size = file.metadata().map(|data| data.len()).unwrap_or(0);
  let markers = occurrences_through(file.as_fd(), needle);
  format!("fd {number} -> {target}, {size} bytes, {markers} neighbour marker(s) reachable")
}

/// What a hostile peer does with the descriptor a zero-copy read handed it, and what must come of it: the
/// neighbour's marker must not be reachable through it, and it must refuse a shared writable map. Split out of
/// the test so each does one thing (the cognitive-complexity bound) and so the hostile part reads as the attack
/// it is. `number` is the descriptor that arrived; `authorized` is what the read was for, reported for scale.
#[cfg(target_os = "linux")]
fn judge_the_handed_descriptor(number: i32, authorized: u64) {
  use std::os::fd::BorrowedFd;

  // SAFETY: `number` is a descriptor this process owns — installed in its table by the `recvmsg` inside
  // `map_read`, and held open by the caller's live `MappedRead` for the whole of this borrow.
  let handed = unsafe { BorrowedFd::borrow_raw(number) };
  let reachable = occurrences_through(handed, NEIGHBOUR_MARKER);
  let object_bytes = rustix::fs::fstat(handed).map(|s| s.st_size).unwrap_or(0);
  let span = usize::try_from(object_bytes).unwrap_or(1).max(1);

  // The hostile use of a read authorization: a shared writable map of the daemon's memory.
  // SAFETY: a fresh mapping at an address the kernel chooses, of `span` bytes of the handed descriptor, which
  // the caller keeps open; the result is only tested and unmapped below.
  let writable = unsafe {
    rustix::mm::mmap(
      std::ptr::null_mut(),
      span,
      rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
      rustix::mm::MapFlags::SHARED,
      handed,
      0,
    )
  };
  if let Ok(address) = writable {
    // SAFETY: `address` is this call's own mapping of `span` bytes; unmapped once and never used again.
    unsafe {
      let _ = rustix::mm::munmap(address, span);
    }
  }
  println!(
    "handed descriptor: object_bytes={object_bytes} authorized_bytes={authorized} \
     neighbour_marker_occurrences={reachable} writable_map={}",
    if writable.is_ok() {
      "granted"
    } else {
      "refused"
    }
  );

  assert_eq!(
    reachable, 0,
    "the neighbour's content is reachable through the descriptor handed for another volume's file \
     ({reachable} occurrences in {object_bytes} bytes, where {authorized} were authorized)"
  );
  assert!(
    writable.is_err(),
    "a descriptor handed to authorize a read mapped writable: the peer can write the daemon's memory"
  );
}

/// §4.6 A-135 with §4.13: the descriptor a zero-copy read hands a peer must authorize that read and
/// nothing more. Do: a one-shard daemon with two volumes of one account — a neighbour holding a marked
/// arena-backed file, and the peer's own — map the peer's file with no copy, then use the descriptor
/// that arrived the way a hostile peer would: scan it for the neighbour's marker and map it writable.
/// Expect: the neighbour's marker is not reachable through it, and it refuses a writable map. The same
/// uid is not the boundary here — a §4.13 consumer is an untrusted process of this very uid, holding
/// this very channel — so what the descriptor itself authorizes is the boundary.
///
/// Non-vacuity: the marker must be found in the daemon's own content object (the scan works, and the
/// neighbour's bytes really are in the arena), or a descriptor that reached everything would still
/// read as clean.
#[cfg(target_os = "linux")]
#[test]
fn a_zero_copy_reads_descriptor_authorizes_its_window_and_nothing_more() {
  let profile = profile();
  let instance = format!("cl-mapauth-{}", std::process::id());
  // One shard: the owner is the peer's home shard, which is where the hand-off happens, and both
  // volumes' content is then in one shard's arena — the reach being measured.
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-mapauth-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  let mut client = connect(&instance);

  // The neighbour: a volume this peer will not name, holding a file whose bytes are its marker.
  let neighbour = client.create(&scratch("neighbour")).unwrap();
  let secret: Vec<u8> = NEIGHBOUR_MARKER
    .iter()
    .copied()
    .cycle()
    .take(ARENA_BACKED_BYTES)
    .collect();
  let held = client.attach(neighbour, None, Intent::Write).unwrap();
  client
    .fs_write((neighbour, held.attachment), "/secret.bin", &secret, 0o600)
    .unwrap();
  client.detach(held.attachment).unwrap();

  // The peer's own volume and file.
  let mine = client.create(&scratch("mine")).unwrap();
  let bytes: Vec<u8> = (0..ARENA_BACKED_BYTES)
    .map(|i| u8::try_from(i % 251).unwrap_or(0))
    .collect();
  let held = client.attach(mine, None, Intent::Write).unwrap();
  client
    .fs_write((mine, held.attachment), "/mine.bin", &bytes, 0o644)
    .unwrap();
  client.detach(held.attachment).unwrap();

  // Non-vacuity, taken before the map: the neighbour's secret really is in this daemon's content
  // object, so "the handed descriptor does not show it" is a fact about the descriptor rather than
  // about the marker being absent everywhere.
  let in_the_arena = occurrences_in_this_process(NEIGHBOUR_MARKER, &instance);
  assert!(
    in_the_arena > 0,
    "the neighbour's content is in the daemon's content object, so the scan can find it"
  );

  let before = memory_object_descriptors(&instance);
  // Non-vacuity for the detector itself, not just for the marker scan. The copy branch below asserts that *no*
  // descriptor arrived, which a detector that can see none would satisfy however the daemon behaved — so prove
  // it sees memory objects at all. The daemon runs in this process, so its own are here.
  assert!(
    !before.is_empty(),
    "the descriptor probe can see this process's memory objects, so 'none arrived' means something"
  );
  let mapped = client.map_read(mine, "/mine.bin").unwrap();
  assert_eq!(mapped.bytes(), bytes.as_slice(), "the peer's own bytes");
  let after = memory_object_descriptors(&instance);
  let arrived: Vec<i32> = after.difference(&before).copied().collect();
  let authorized = u64::try_from(ARENA_BACKED_BYTES).unwrap_or(0);

  if !mapped.is_zero_copy() {
    // The hand-off was refused, so the bytes came by copy. Then nothing may have crossed: a refused
    // hand-off that still installed a descriptor in the peer would be the escape with the reply
    // changed.
    let described: Vec<String> = arrived
      .iter()
      .map(|number| describe_descriptor(*number, NEIGHBOUR_MARKER))
      .collect();
    assert!(
      arrived.is_empty(),
      "a copy-path read still installed {} memory-object descriptor(s) in the peer: {}",
      arrived.len(),
      described.join("; ")
    );
    println!(
      "map_read took the copy path; no descriptor crossed (arena holds {in_the_arena} markers)"
    );
    client.destroy(mine).unwrap();
    client.destroy(neighbour).unwrap();
    return;
  }

  assert_eq!(
    arrived.len(),
    1,
    "exactly one descriptor arrived with the mapping: {arrived:?}"
  );
  judge_the_handed_descriptor(arrived.first().copied().unwrap_or(-1), authorized);

  client.destroy(mine).unwrap();
  client.destroy(neighbour).unwrap();
}

/// AC-A-135 — a `MapRead` returns the **whole** file, whichever path it takes. Do: on a one-shard daemon, write
/// files from well under one content chunk to several chunks across, and `map_read` each. Expect: every one
/// returns every byte.
///
/// This is the test the A-135 suite did not have. Every other test here uses a 20 KiB file — "one chunk, so one
/// block", as the first of them says — and the zero-copy path maps the content block at offset 0 and answers as
/// though that block were the file. Measured on x86_64 Linux with the confinement rule lifted, a 256 KiB, a
/// 1 MiB and a 4 MiB file each returned exactly 65,536 bytes and `equal=false`, with nothing to tell the caller
/// (`docs/bugs/2026-10-18-a-zero-copy-read-of-a-multi-chunk-file-returned-one-chunk.md`). A silent short read is
/// worse than a refusal, so the server now falls back to the copy path for a file bigger than the block it
/// would map.
///
/// Sizes are chosen to straddle a chunk without depending on what the chunk size is: the smallest here is
/// certainly inside one and the largest certainly spans several.
#[cfg(unix)]
#[test]
fn a_map_read_returns_the_whole_file_at_every_size() {
  let profile = profile();
  let instance = format!("cl-whole-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-whole-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  let mut client = connect(&instance);

  // Zero is in the list because it was the corner none of these covered, and a 50,000-case sweep of the VFS
  // property above it found the hole: `dax_map_read` of a 0-byte file has no extent at offset 0 and refuses
  // `NotFound`, which the server turns into a refusal to the caller — so a zero-copy read of a file that
  // plainly exists reported that it did not, while the copy path returns its zero bytes without complaint.
  for kib in [0usize, 4, 64, 256, 1024] {
    let id = client
      .create(&CreateSpec {
        name: format!("whole-{kib}"),
        size: SizeClass::Bounded {
          // The volume still needs room for its own records when the file in it is empty.
          limit: u64::try_from(kib.max(4) << 10)
            .unwrap_or(1 << 20)
            .saturating_mul(8),
        },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .unwrap();
    let bytes: Vec<u8> = (0..(kib << 10))
      .map(|i| u8::try_from(i % 251).unwrap_or(0))
      .collect();
    let attached = client.attach(id, None, Intent::Write).unwrap();
    client
      .fs_write((id, attached.attachment), "/f.bin", &bytes, 0o644)
      .unwrap();
    client.detach(attached.attachment).unwrap();

    let mapped = client.map_read(id, "/f.bin").unwrap();
    assert_eq!(
      mapped.bytes().len(),
      bytes.len(),
      "{kib} KiB: a MapRead returned {} of {} bytes (path {})",
      mapped.bytes().len(),
      bytes.len(),
      if mapped.is_zero_copy() {
        "zero-copy"
      } else {
        "copy"
      }
    );
    assert_eq!(mapped.bytes(), bytes.as_slice(), "{kib} KiB: every byte");
    client.destroy(id).unwrap();
  }
}

/// AC-A-135 (cross-shard): on a multi-shard daemon a client maps files whose owner shard may not be its
/// home shard; it must always get the correct bytes — zero-copy where the volume is local, a copy where
/// it is owned by another shard (the descriptor cannot cross shards yet). Several volumes spread across
/// the two shards by id hash, so at least one is owned by the shard the client did not connect to, which
/// exercises the forwarded path (without the fix a `MapRead` there returns `NotFound` and the client errors).
#[cfg(unix)]
#[test]
fn a_client_maps_files_across_shards_and_always_gets_the_bytes() {
  let profile = profile();
  let instance = format!("cl-daxx-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-daxx-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  let mut client = connect(&instance);

  for n in 0..6u8 {
    let id = client.create(&scratch(&format!("daxx-{n}"))).unwrap();
    // 20 KiB — larger than one bulk chunk, so where map_read cannot map it the copy fallback must page
    // rather than return one oversized whole-file `ReadBytes` (the bug this test caught); it proves the
    // client always gets the bytes, zero-copy or paged-copy, without an over-capacity single reply.
    let bytes: Vec<u8> = (0..(20usize << 10))
      .map(|i| u8::try_from((i + usize::from(n)) % 251).unwrap_or(0))
      .collect();
    let attached = client.attach(id, None, Intent::Write).unwrap();
    client
      .fs_write((id, attached.attachment), "/f.bin", &bytes, 0o644)
      .unwrap();
    client.detach(attached.attachment).unwrap();
    let mapped = client.map_read(id, "/f.bin").unwrap();
    assert_eq!(
      mapped.bytes(),
      bytes.as_slice(),
      "volume {n}: the bytes are correct whether the map was zero-copy (local) or a copy (cross-shard)"
    );
    client.destroy(id).unwrap();
  }
}

/// The typed verbs over the rings: create, the duplicate refused, snapshot, clone, attach
/// with the lease, status, list, detach, resize, destroy, acknowledge.
#[test]
fn the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed() {
  let profile = profile();
  let instance = format!("cl-life-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-life-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let id = create_snapshot_clone(&mut client);
  attach_status_detach(&mut client, id);
  client
    .resize(id, SizeClass::Bounded { limit: 2 << 20 })
    .unwrap();
  client.destroy(id).unwrap();
  wait_until_listed(&mut client, &["one-clone"]);
  client.acknowledge_all().unwrap();
  let report = client.daemon_status().unwrap();
  assert_eq!(report.pid, std::process::id(), "the daemon is this process");
  assert_eq!(report.shards.len(), usize::from(TEST_SHARDS));
  assert_eq!(
    report.shards.iter().map(|s| s.clients).sum::<u32>(),
    1,
    "one live client"
  );
  assert!(
    report.shards.iter().map(|s| s.served).sum::<u64>() >= 10,
    "the calls were served"
  );
  assert!(
    report
      .shards
      .iter()
      .flat_map(|s| s.refusals.iter())
      .any(|r| r.kind == "already_exists" && r.count == 1),
    "the duplicate was counted: {:?}",
    report.shards
  );
  assert_eq!(client.reconnects(), 0, "nothing made the client reconnect");
  let (parks, replies) = client.park_ratio();
  let unanswered = client.unanswered_wakes();
  assert!(replies >= 10, "every call was a reply: {replies}");
  // Every park ends in a reply, in a wake that brought none (a spurious return from the OS wait, or a
  // previous reply's late wake landing on this park), or at the deadline (none here: nothing stalled).
  assert!(
    parks <= replies + unanswered,
    "every park is ended by a reply or counted as an unanswered wake: {parks} parks, {replies} replies, {unanswered} unanswered"
  );
  daemon.stop();
}

/// After the restart: the client's next call reconnects under its id and is served by the
/// restarted daemon, which rebuilt the volume and recovered its snapshot from anchor-owned RAM.
fn assert_served_after_restart(
  client: &mut Client,
  kept: slates_client::VolumeId,
  session: slates_client::Session,
  snapshot: slates_client::SnapshotId,
) {
  let report = client.status(kept).unwrap();
  assert_eq!(report.name, "kept");
  assert_eq!(client.reconnects(), 1, "one reconnect, under the old id");
  assert_eq!(client.client_id(), session.client_id);
  assert_eq!(
    report.snapshots, 1,
    "the snapshot's content was recovered from anchor-owned RAM (§4.8), not reconciled away"
  );
  assert_eq!(
    report.head.value, snapshot.value,
    "and the head still points at the recovered snapshot"
  );
}

/// Exactly-once across the restart: the retry answers from the replayed completion record.
fn assert_retry_meets_record(
  client: &mut Client,
  uncertain: slates_client::VolumeId,
  create_id: slates_wire::request::RequestId,
) {
  let retried = client
    .retry(
      create_id,
      &RequestBody::Create {
        name: "uncertain".to_owned(),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    )
    .unwrap();
  assert_eq!(
    retried,
    slates_ipc::protocol::ReplyBody::Created { id: uncertain },
    "the original reply, not a second create"
  );
  assert_eq!(
    client.reconnects(),
    1,
    "the retry reached the replacement daemon"
  );
  wait_until_listed(client, &["before-restart", "kept", "uncertain"]);
}

/// Lose the completed transport reply without acknowledging its durable completion. A subsequent
/// retry must read the replacement daemon's record, with no old ring reply left to satisfy it.
fn discard_reply(client: &mut Client, request: slates_wire::request::RequestId) {
  let waiting = Instant::now();
  while client.poll_reply(request).unwrap().is_none() {
    assert!(
      waiting.elapsed() < START_WAIT,
      "the committed create must reply"
    );
    std::thread::yield_now();
  }
}

/// A session outlives a daemon restart over the same segment (the test plays the anchor):
/// the client's next call finds the daemon gone, reconnects under its id and is served by
/// the restarted daemon, which recovered the volume and its snapshot from anchor-owned RAM; its
/// retry of a create whose reply was discarded without acknowledgment meets its completion (same id,
/// no second volume), while an acknowledged create stays refused; a second client cannot
/// take the live session. AC-2.3, §4.7: completions are retained until acknowledged.
#[test]
fn a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record() {
  let profile = profile();
  let instance = format!("cl-resume-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  // Shape: a small supported ring, so periodic acknowledgement runs during the scenario.
  config.region.slots = 4;
  // The test plays the anchor: it holds the segment and its content object across both daemons, so
  // anchor-owned volume storage survives the restart (§4.8). The content object is sized as the anchor sizes it
  // (`DaemonConfig::content_bytes`); it is lazily backed, so its unused tail costs no RAM.
  let content_bytes = config.content_bytes();
  let segment = AnchorSegment::create(
    &format!("slates-seg-cl-resume-{}", std::process::id()),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &format!("slates-con-cl-resume-{}", std::process::id()),
    content_bytes,
  )
  .unwrap();
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    let content = segment.content_handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content,
    }
  };
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let kept = client.create(&scratch("kept")).unwrap();
  let create_id = client.last_request();
  let snapshot = client.snapshot(kept).unwrap();
  assert_eq!(client.status(kept).unwrap().snapshots, 1);
  client.acknowledge_all().unwrap();
  let mut exited = connect(&instance);
  let before_restart = exited.create(&scratch("before-restart")).unwrap();
  let silent = connect(&instance);
  let highest_issued = silent.client_id();
  // Send without acknowledging its completion. A separate client observes the committed effect,
  // so the restart must replay the completion rather than merely execute an unsent request.
  let uncertain_request = client
    .begin(&RequestBody::Create {
      name: "uncertain".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  wait_until_listed(&mut exited, &["before-restart", "kept", "uncertain"]);
  let uncertain = exited
    .list()
    .unwrap()
    .into_iter()
    .find(|volume| volume.name == "uncertain")
    .unwrap()
    .id;
  // Discard the old transport reply without acknowledging it. Leaving it on the old ring would
  // let retry return that buffered reply and falsely certify recovery's completion record.
  discard_reply(&mut client, uncertain_request);
  let session = client.session();
  drop(exited);
  drop(silent);
  first.stop();
  let second = Daemon::start(&profile, config, source()).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut fresh = connect(&instance);
  assert_eq!(
    fresh.status(kept).unwrap().snapshots,
    1,
    "the new status must not replay an old client's create completion"
  );
  assert_eq!(fresh.status(before_restart).unwrap().snapshots, 0);
  assert!(
    fresh.client_id() > highest_issued,
    "even a client that ran no verb reserved its id before the restart"
  );
  assert_retry_meets_record(&mut client, uncertain, uncertain_request);
  assert_served_after_restart(&mut client, kept, session, snapshot);
  assert!(matches!(
    client.retry(
      create_id,
      &RequestBody::Create {
        name: "kept".to_owned(),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      }
    ),
    Err(ClientError::Refused(Refusal::DuplicateRequest))
  ));
  // New work continues under the session's sequence.
  let more = client.create(&scratch("more")).unwrap();
  assert_ne!(more, kept);
  // A second client cannot take a live session.
  assert!(matches!(
    Client::resume(&instance, session, deadlines()),
    Err(ClientError::SessionTaken { .. })
  ));
  second.stop();
  drop(segment);
}

/// AUD-29-21 (§4.9, RIFL): do: connect a client, restart its daemon over the same anchor-held segment, and
/// resume the client's session seeded at the last request sequence; create a volume under it, then ask for
/// more; expect the last create served, every later fresh request refused `SequencesExhausted` before it is
/// sent, the session carrying the exhausted marker, and a retry of the last id still answered from its
/// completion record — never a wrapped sequence met as already acknowledged (the old client wrapped to zero).
/// The restart is how a test frees the client id: the daemon keeps an id while its process lives.
#[test]
fn a_client_at_its_last_sequence_refuses_fresh_requests_and_keeps_its_retries() {
  let profile = profile();
  let instance = format!("cl-seq-end-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let content_bytes = config.content_bytes();
  let segment = AnchorSegment::create(
    &format!("slates-seg-cl-seq-end-{}", std::process::id()),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &format!("slates-con-cl-seq-end-{}", std::process::id()),
    content_bytes,
  )
  .unwrap();
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    let content = segment.content_handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content,
    }
  };
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let client_id = connect(&instance).client_id();
  first.stop();
  let second = Daemon::start(&profile, config, source()).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  // The resumed client's first call acknowledges what it holds (one sequence, the acknowledgement due at a
  // fresh watermark), so the create after it takes the last sequence.
  let seeded = slates_client::Session {
    client_id,
    next_sequence: slates_client::LAST_SEQUENCE - 1,
  };
  let mut client = Client::resume(&instance, seeded, deadlines()).unwrap();
  let last = client.create(&scratch("last")).unwrap();
  let last_id = client.last_request();
  assert_eq!(last_id.sequence, slates_client::LAST_SEQUENCE);
  assert!(matches!(
    client.create(&scratch("past")),
    Err(ClientError::SequencesExhausted { client }) if client == client_id
  ));
  assert_eq!(
    client.session().next_sequence,
    u32::MAX,
    "the exhausted marker"
  );
  let retried = client
    .retry(
      last_id,
      &RequestBody::Create {
        name: "last".to_owned(),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    )
    .unwrap();
  assert_eq!(
    retried,
    slates_ipc::protocol::ReplyBody::Created { id: last },
    "the retry meets the record"
  );
  second.stop();
  drop(segment);
}

/// AC-2.3 / AC-2.6: use the last fresh identity, refuse the next admission, restart, and
/// expect the existing session to keep working while fresh admission remains refused.
#[test]
fn exhausted_client_id_space_refuses_fresh_callers_but_preserves_a_resuming_session() {
  let profile = profile();
  let instance = format!("cl-id-end-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  // The anchor keeps both content publication slots as well as metadata across the restart.
  let content_bytes = config.content_bytes();
  let mut segment = AnchorSegment::create(
    &format!("slates-seg-cl-id-end-{}", std::process::id()),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &format!("slates-con-cl-id-end-{}", std::process::id()),
    content_bytes,
  )
  .unwrap();
  // Build the retained admission history at the format boundary without billions of connects.
  let (mut db, _) = slates_db::replay::recover(&mut segment, 0, config.caps, 0).unwrap();
  db.mutate(
    &mut segment,
    &slates_db::Op::ClientIdReserved {
      client: u32::MAX - 1,
    },
    0,
  )
  .unwrap();
  db.snapshot(&mut segment).unwrap();
  drop(db);
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content: segment.content_handoff().unwrap(),
    }
  };
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  assert_eq!(client.client_id(), u32::MAX);
  let kept = client.create(&scratch("last-identity")).unwrap();
  assert_fresh_identity_refused(&instance);
  assert_eq!(client.status(kept).unwrap().name, "last-identity");
  first.stop();
  let second = Daemon::start(&profile, config, source()).unwrap();
  second.bootstrap(true).unwrap();
  assert_eq!(client.status(kept).unwrap().name, "last-identity");
  assert_eq!(client.client_id(), u32::MAX);
  assert!(
    client.reconnects() > 0,
    "the existing session really resumed"
  );
  assert_fresh_identity_refused(&instance);
  assert_eq!(client.status(kept).unwrap().name, "last-identity");
  second.stop();
}

fn assert_fresh_identity_refused(instance: &str) {
  assert!(matches!(
    Client::connect(instance, deadlines()),
    Err(ClientError::Ipc(
      slates_ipc::IpcError::DaemonUnavailable { .. }
    ))
  ));
}

/// Commits two versions on a green: create `f`, then modify it, so the chain has versions 1 and 2.
fn seed_green_two_versions(client: &mut Client, green: slates_client::VolumeId) {
  let (w0, _) = client.create_work(green, "w0").unwrap();
  client.edit(w0, "f", 0, 0, b"hello").unwrap();
  assert!(matches!(client.submit(w0).unwrap(), Submitted::Accepted(1)));
  let (w1, _) = client.create_work(green, "w1").unwrap();
  client.edit(w1, "f", 0, 5, b"world").unwrap();
  assert!(matches!(client.submit(w1).unwrap(), Submitted::Accepted(2)));
  assert_eq!(client.versions(green).unwrap(), 2);
}

/// After the restart the green's chain recovered: the head is 2, the last-changed index still names
/// f after version 0, and a new increment continues from the recovered head (version 3, not 1).
fn assert_green_recovered(client: &mut Client, green: slates_client::VolumeId) {
  assert_eq!(
    client.versions(green).unwrap(),
    2,
    "the green's chain survived the restart"
  );
  assert_eq!(
    client.changed_since(green, 0).unwrap(),
    vec!["f".to_owned()]
  );
  assert!(client.changed_since(green, 2).unwrap().is_empty());
  let (w2, base) = client.create_work(green, "w2").unwrap();
  assert_eq!(base, 2, "a new work is based on the recovered head");
  client.edit(w2, "g", 0, 0, b"new").unwrap();
  match client.submit(w2).unwrap() {
    Submitted::Accepted(v) => assert_eq!(v, 3, "the recovered green advances to version 3"),
    other => panic!("expected accept, got {other:?}"),
  }
}

/// A green's version chain is durable (§4.16, §4.8): versions committed before a restart are
/// recovered from the partition log, so the head returns, the last-changed index is intact, and a
/// new increment continues the chain. Non-vacuous: without the persisted chain the recovered green
/// would be empty (head 0) and the next submit would be version 1, not 3.
#[test]
fn a_green_chain_survives_a_daemon_restart() {
  let profile = profile();
  let instance = format!("cl-green-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let content_bytes = config.content_bytes();
  let segment = AnchorSegment::create(
    &format!("slates-seg-cl-green-{}", std::process::id()),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &format!("slates-con-cl-green-{}", std::process::id()),
    content_bytes,
  )
  .unwrap();
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    let content = segment.content_handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content,
    }
  };
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("g", false).unwrap();
  seed_green_two_versions(&mut client, green);
  first.stop();

  let second = Daemon::start(&profile, config, source()).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  assert_green_recovered(&mut client, green);
  second.stop();
  drop(segment);
}

/// A host directory a test writes into, removed when dropped (a test's own disk, outside slates).
struct HostDir {
  path: String,
}

impl Drop for HostDir {
  fn drop(&mut self) {
    let _ = std::process::Command::new("rm")
      .args(["-rf", &self.path])
      .output();
  }
}

/// A fresh host directory in the build output named with the process id, holding `f.txt` = `disk bytes`.
fn host_dir_with_file() -> HostDir {
  // A-50: a test's base is a real host directory in the build output (`CARGO_TARGET_TMPDIR`), never `/tmp`.
  let path = format!(
    "{}/slates-origin-{}",
    env!("CARGO_TARGET_TMPDIR"),
    std::process::id()
  );
  let made = std::process::Command::new("mkdir")
    .args(["-p", &path])
    .output()
    .unwrap();
  assert!(made.status.success(), "mkdir -p {path}");
  let wrote = std::process::Command::new("sh")
    .arg("-c")
    .arg(format!("printf '%s' 'disk bytes' > '{path}/f.txt'"))
    .output()
    .unwrap();
  assert!(wrote.status.success(), "host write");
  HostDir { path }
}

/// A green over a complete immutable base: an overlay over the host directory, pinned whole and
/// snapshotted, then the green created over that snapshot and advanced once (`f.txt` prefixed).
fn seed_green_over_base(client: &mut Client, dir: &str) -> slates_client::VolumeId {
  let overlay = client
    .create(&CreateSpec {
      name: "over".to_owned(),
      size: slates_client::SizeClass::Dynamic { max: 1 << 24 },
      names: slates_client::NamePolicy::Exact,
      require_locked: false,
      base: Some(dir.to_owned()),
    })
    .unwrap();
  assert_eq!(
    client.pin(overlay, None).unwrap(),
    1,
    "the one base file pinned"
  );
  let snapshot = client.snapshot(overlay).unwrap();
  let green = client
    .create_green_over(
      "g-base",
      false,
      slates_client::GreenBase {
        volume: overlay,
        snapshot,
      },
    )
    .unwrap();
  let (work, base) = client.create_work(green, "w0").unwrap();
  assert_eq!(base, 0);
  client.edit(work, "f.txt", 0, 0, b"agent: ").unwrap();
  assert!(matches!(
    client.submit(work).unwrap(),
    Submitted::Accepted(1)
  ));
  green
}

/// After the restart the origin is version 0 again — re-seeded from its durable record before the
/// chain replays over it — so version 0 reads the base bytes, version 1 the increment over them, and a
/// new increment continues the chain at 2.
fn assert_origin_recovered(client: &mut Client, green: slates_client::VolumeId) {
  use slates_client::ReadAt;
  assert_eq!(client.versions(green).unwrap(), 1, "the chain survived");
  assert_eq!(
    client
      .read(green, "f.txt", ReadAt::Version { version: 0 })
      .unwrap(),
    b"disk bytes",
    "version 0 is the origin, re-seeded before the chain replayed"
  );
  assert_eq!(
    client
      .read(green, "f.txt", ReadAt::Version { version: 1 })
      .unwrap(),
    b"agent: disk bytes"
  );
  let (work, base) = client.create_work(green, "w1").unwrap();
  assert_eq!(base, 1);
  client.edit(work, "f.txt", 0, 0, b"again: ").unwrap();
  assert!(matches!(
    client.submit(work).unwrap(),
    Submitted::Accepted(2)
  ));
  assert_eq!(
    client.read(green, "f.txt", ReadAt::Head).unwrap(),
    b"again: agent: disk bytes"
  );
}

/// A base-seeded green's origin is durable (§4.16 "the origin version from a snapshot"; A-9: a chain
/// from "a complete immutable base"): the green is created over a pinned overlay snapshot and advanced
/// once; after a daemon restart over the same segment, version 0 reads the origin's bytes and the chain
/// replays over it. Non-vacuous: without the persisted origin the recovered green would replay its one
/// increment over an empty version 0 — `f.txt` at version 0 would be `NotFound`, not the base bytes.
#[test]
fn a_base_seeded_greens_origin_survives_a_daemon_restart() {
  let profile = profile();
  let instance = format!("cl-origin-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let content_bytes = config.content_bytes();
  let segment = AnchorSegment::create(
    &format!("slates-seg-cl-origin-{}", std::process::id()),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &format!("slates-con-cl-origin-{}", std::process::id()),
    content_bytes,
  )
  .unwrap();
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    let content = segment.content_handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content,
    }
  };
  let dir = host_dir_with_file();
  // Captured by the harness and shown only on failure: the CI macOS lane refused the base-seeded
  // overlay `NoSpace` on 2026-09-16 where this host admits it, and the daemon's derived budget is the
  // first thing that failure has to explain (§4.2 D-12 derives it from the runner's memory).
  eprintln!(
    "the fixture's daemon: reserve_per_shard {} bytes, content segment {content_bytes} bytes, {} shards; derivations: {:#?}",
    config.reserve_per_shard, TEST_SHARDS, config.derivations
  );
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = seed_green_over_base(&mut client, &dir.path);
  first.stop();

  let second = Daemon::start(&profile, config, source()).unwrap();
  second
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  assert_origin_recovered(&mut client, green);
  second.stop();
  drop(segment);
  drop(dir);
}

/// Shape: an insert several bulk chunks (4 KiB) long.
const LARGE_EDIT_BYTES: usize = 20 * 1024;

/// Drives the async edit `word` as an event loop does — `take_ready`, then the edit's poll — until it resolves,
/// within the reply deadline per step.
fn drive_edit(client: &mut Client, word: u64) -> Result<bool, slates_client::ClientError> {
  let started = Instant::now();
  loop {
    client.take_ready()?;
    if let Some(done) = client.edit_poll(word)? {
      return Ok(done);
    }
    assert!(
      started.elapsed() < START_WAIT,
      "the chained edit resolved in time"
    );
    std::hint::spin_loop();
  }
}

/// §4.12 (A-83, the async half): do begin an async edit inserting [`LARGE_EDIT_BYTES`] into a work and drive it as
/// an event loop would (one word, polled on readiness); expect it to resolve and the work's file to read back every
/// byte in order, the edit staged in pages and applied as one. Begin another and abandon it after its first step;
/// expect nothing left outstanding and the file unchanged.
#[test]
fn an_async_edit_larger_than_a_request_is_chained_and_resolves_as_one() {
  let profile = profile();
  let instance = format!("cl-chain-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-cl-chain-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("chain-g", false).unwrap();
  let (work, _) = client.create_work(green, "chain-w").unwrap();
  let text: Vec<u8> = (0..LARGE_EDIT_BYTES)
    .map(|at| b'a' + u8::try_from(at % 26).unwrap())
    .collect();
  let id = client.edit_begin(work, "big.txt", 0, 0, &text).unwrap();
  assert_eq!(drive_edit(&mut client, id.word()), Ok(true));
  assert!(
    client.outstanding().is_empty(),
    "nothing outstanding after it resolved"
  );
  let read = client
    .read(work, "big.txt", slates_client::ReadAt::Head)
    .unwrap();
  assert_eq!(read, text, "every byte, in order");
  let abandoned = client.edit_begin(work, "big.txt", 0, 0, &text).unwrap();
  client.take_ready().unwrap();
  let _ = client.edit_poll(abandoned.word());
  assert!(
    client.abandon(abandoned.word()),
    "the chain was outstanding"
  );
  assert!(
    client.outstanding().is_empty(),
    "abandoning a chain leaves no step behind"
  );
  let again = client
    .read(work, "big.txt", slates_client::ReadAt::Head)
    .unwrap();
  assert_eq!(again, text, "an abandoned chain changes nothing");
  drop(client);
  daemon.stop();
}

/// A daemon over `segment`'s handoff (its content object included), started on a fresh config of `instance`.
fn start_over(profile: &MachineProfile, config: &DaemonConfig, segment: &AnchorSegment) -> Daemon {
  let (handoff, len) = segment.handoff().unwrap();
  let content = segment.content_handoff().unwrap();
  Daemon::start(
    profile,
    config.clone(),
    SegmentSource::Handoff {
      handoff,
      len,
      content,
    },
  )
  .unwrap()
}

/// The anchor a test plays: a segment and its content object under `tag`.
fn played_anchor(profile: &MachineProfile, config: &DaemonConfig, tag: &str) -> AnchorSegment {
  AnchorSegment::create(
    &format!("slates-seg-{tag}-{}", std::process::id()),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &format!("slates-con-{tag}-{}", std::process::id()),
    config.content_bytes(),
  )
  .unwrap()
}

/// A-92 piece 2a: do start a daemon under an anchor and read its sealing root from its status, stop it and start a
/// second daemon under the same anchor, then start a third under a fresh anchor; expect the first to have minted a
/// root, the second to have adopted that very root (so content sealed under it stays openable across a restart), and
/// the third to have minted another (the root lives exactly as long as its anchor's RAM).
#[test]
fn a_restarted_daemon_adopts_its_anchors_sealing_root_and_a_fresh_anchor_mints_another() {
  let profile = profile();
  let instance = format!("cl-seal-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let anchor = played_anchor(&profile, &config, "cl-seal");
  let first = start_over(&profile, &config, &anchor);
  let minted = connect(&instance).daemon_status().unwrap().seal;
  first.stop();
  let second = start_over(&profile, &config, &anchor);
  let adopted = connect(&instance).daemon_status().unwrap().seal;
  second.stop();
  assert_eq!(
    minted.state, "minted",
    "the first daemon under an anchor makes the root: {minted:?}"
  );
  assert_eq!(
    minted.root_id.len(),
    16,
    "its id is reported, never its key: {minted:?}"
  );
  assert_eq!(adopted.state, "adopted", "a restart finds it: {adopted:?}");
  assert_eq!(adopted.root_id, minted.root_id, "the very same root");
  assert_eq!(
    minted.recipient_id.len(),
    16,
    "the node's ML-KEM recipient is made with the root: {minted:?}"
  );
  assert_eq!(
    adopted.recipient_id, minted.recipient_id,
    "and opened again from its sealed record after the restart (A-92 piece 4a)"
  );
  assert!(
    adopted.key_slots > 0 && adopted.keys_held > 0,
    "held in the locked region: {adopted:?}"
  );
  let fresh_anchor = played_anchor(&profile, &config, "cl-seal-fresh");
  let third = start_over(&profile, &config, &fresh_anchor);
  let other = connect(&instance).daemon_status().unwrap().seal;
  third.stop();
  assert_eq!(
    other.state, "minted",
    "a fresh anchor has no root: {other:?}"
  );
  assert_ne!(
    other.root_id, minted.root_id,
    "another anchor's life, another root"
  );
}

/// A slow CI lane (a loaded shared runner, or ThreadSanitizer's 10–20× slowdown) publishes a restarted daemon's
/// bootstrap stamp far later than this developer machine does. The restart here runs an *in-process* test daemon, so
/// its `stop` cannot fire the client's process-exit watch (the daemon is this very process); the client learns the
/// daemon is gone only once the successor republishes the stamp (`daemon_gone` = the exit watch OR a changed start
/// stamp). With a 1 ms reply deadline this raced the publish and the client reported `Stalled { after_ns: 1_000_000 }`
/// instead of awaiting the restart (CI macOS `Test` + TSan lanes, run 38537130473). `SLATES_TEST_RESTART_SLOWDOWN`
/// (an integer ≥ 1, default 1) scales every restart timing together so a slow lane gives the publish room; the
/// invariant the test rests on holds at any scale — publish ≪ reply ≪ recovery ≪ reconnect budget.
static RESTART_SLOWDOWN: LazyLock<u64> = LazyLock::new(|| {
  std::env::var("SLATES_TEST_RESTART_SLOWDOWN")
    .ok()
    .and_then(|value| value.parse::<u64>().ok())
    .filter(|&n| n >= 1)
    .unwrap_or(1)
});

/// The reply deadline in the slow-restart test: short enough that it fires well before the restart finishes (so the
/// test proves the client awaits the *restart*, not the reply deadline), yet — scaled for a slow lane — long enough
/// that the successor has republished its stamp before the deadline fires. Base 20 ms.
fn short_reply_ns() -> u64 {
  20_000_000u64.saturating_mul(*RESTART_SLOWDOWN)
}

/// How long the successor's held shard spends starting: far past the reply deadline (15×), well inside the reconnect
/// budget (a long recovery). Base 300 ms.
fn long_recovery_ns() -> u64 {
  300_000_000u64.saturating_mul(*RESTART_SLOWDOWN)
}

/// The client's reconnect budget in this test: well past the recovery time (10×) so a slow restart is awaited, not
/// judged stalled. Base 3 s. Scaled with the rest so a slow lane's longer recovery still fits inside it (the product
/// derives `reconnect_ns` from the recovery and liveness budgets, `Deadlines::derive`; this test fixes the ratio and
/// scales it, since its in-process daemon makes the wall-clock, not the formula, the thing under test here).
fn reconnect_budget_ns() -> u64 {
  3_000_000_000u64.saturating_mul(*RESTART_SLOWDOWN)
}

/// One crossing of a restart: a volume made, the daemon stopped, a successor whose shard `partition` takes
/// [`long_recovery_ns`] to start (as a long recovery does), and a status asked meanwhile by a client whose reply
/// deadline ([`short_reply_ns`]) is shorter; the status, the client's reconnect count, and how long the call took.
fn status_across_a_slow_restart(
  partition: u16,
) -> (
  Result<slates_ipc::protocol::StatusReport, ClientError>,
  u64,
  Duration,
) {
  let profile = profile();
  let instance = format!("cl-first-answer-{partition}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = played_anchor(&profile, &config, &format!("cl-first-answer-{partition}"));
  let first = start_over(&profile, &config, &segment);
  first
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  // The volume is made with the product's deadlines; the client under test, connected before the restart, makes
  // only the call that crosses it.
  let volume = connect(&instance).create(&scratch("first-answer")).unwrap();
  let short = Deadlines {
    reply_ns: short_reply_ns(),
    reconnect_ns: reconnect_budget_ns(),
  };
  let mut client = Client::connect(&instance, short).unwrap();
  first.stop();
  // The client asks from another thread while this one starts the successor (the anchor segment stays on this
  // thread, as the anchor process's does).
  let asking = std::thread::spawn(move || {
    let began = Instant::now();
    let status = client.status(volume);
    (status, client.reconnects(), began.elapsed())
  });
  let slow = config
    .clone()
    .with_boot_fault(slates_server::config::BootFault {
      partition,
      kind: slates_server::config::BootFaultKind::Busy,
      for_ns: long_recovery_ns(),
    });
  let second = start_over(&profile, &slow, &segment);
  let crossed = asking.join().unwrap();
  second.stop();
  crossed
}

/// §4.7 (`Deadlines::derive`: the reconnect budget is "the recovery budget the restart is bounded by, plus one reply
/// deadline for the restarted daemon's first answer"). Do: for each shard in turn, restart the daemon with that
/// shard's start held busy [`long_recovery_ns`] while a client whose reply deadline is shorter asks for a volume's
/// status. Expect: the status answers each time, and the call crossed the restart. The successor opens its rendezvous
/// while its shards still start (`Daemon::start` spawns their starts and opens the listener behind them), so this pins
/// that a client's reconnect is not complete until a slow shard has started: a recovering daemon is waited for under
/// the reconnect budget, never judged by one reply deadline. Written 2026-10-06 against CI's TSan lane
/// (`Stalled { after_ns: 1000000000 }` beside a shard that took 1.79 s to start, `crates/server/tests/recovery.rs`);
/// it passed on the code as it was, so that stall is not this path (recorded in GAPS).
#[test]
fn a_restarted_daemons_first_answer_is_awaited_for_the_reconnect_budget() {
  for partition in 0..TEST_SHARDS {
    let (status, reconnects, took) = status_across_a_slow_restart(partition);
    assert!(
      status.is_ok(),
      "shard {partition} held: the restarted daemon's first answer was awaited, not called stalled: {status:?}"
    );
    assert!(
      reconnects >= 1,
      "shard {partition} held: the call crossed the restart"
    );
    // Non-vacuity: the call waited out the held start, so the slow path is the one exercised.
    assert!(
      took >= Duration::from_nanos(long_recovery_ns()),
      "shard {partition} held: the call spanned the slow start ({took:?})"
    );
  }
}

/// Shape: a held shard start longer than the product's reply deadline (the anchor's liveness budget), as TSan's lane
/// measured (a 1.79 s start against a 1 s deadline); a start A-100 lets the supervisor sit out while the shard works.
const START_PAST_THE_REPLY_NS: u64 = 1_500_000_000;

/// A-100 against the client. Do: with the product's own deadlines, create a volume and keep the client; stop the daemon;
/// start its successor with one shard's start held busy past the reply deadline, and only then ask the same client for
/// the volume's status, for each shard in turn. Expect: the status answers: the successor's start returns only once its
/// shards have started, so a resumed session never meets a shard still starting. Written 2026-10-06 to test whether
/// that was the cause of CI's TSan-lane stall in `crates/server/tests/recovery.rs`; it passed on the code as it was, so
/// it was not.
#[test]
fn a_resumed_session_waits_out_a_shard_start_its_supervisor_allows() {
  for partition in 0..TEST_SHARDS {
    let profile = profile();
    let instance = format!("cl-resume-slow-{partition}-{}", std::process::id());
    let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
    let segment = played_anchor(&profile, &config, &format!("cl-resume-slow-{partition}"));
    let first = start_over(&profile, &config, &segment);
    first
      .bootstrap(true)
      .expect("the fixture explicitly creates its local consensus group");
    let mut client = connect(&instance);
    let volume = client.create(&scratch("resume-slow")).unwrap();
    first.stop();
    let slow = config
      .clone()
      .with_boot_fault(slates_server::config::BootFault {
        partition,
        kind: slates_server::config::BootFaultKind::Busy,
        for_ns: START_PAST_THE_REPLY_NS,
      });
    let second = start_over(&profile, &slow, &segment);
    let status = client.status(volume);
    let reconnects = client.reconnects();
    second.stop();
    assert!(
      status.is_ok(),
      "shard {partition} held {START_PAST_THE_REPLY_NS} ns: the resumed session's answer was waited for, not called stalled ({reconnects} reconnects): {status:?}"
    );
  }
}

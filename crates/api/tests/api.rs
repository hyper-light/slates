//! The library by use (§4.12 "Embedding", A-278): an embedding host's whole flow over a daemon in this process — attach,
//! the consensus group, volumes made, snapshotted, cloned and destroyed, an overlay written, its drift read, a landing
//! planned, granted under the issuer authority and landed exactly as approved, a Shards stream that serves the volume,
//! and the daemon observed — each refusal asserted as its typed variant. Every test drives the crate's own verbs and
//! judges only what a host could observe: the daemon's answers and the bytes on the disk.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::disallowed_methods
)]

use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_api::{
  Error, Filter, Instance, Intent, IssuerFault, PathFault, Planned, Refusal, SnapshotId, VolumeId,
};
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the tests' machine profile (milliseconds); measured once per process.
const PROBE_MS: u64 = 5;
/// Shape: one shard per test daemon: the library's verbs are the subject, not placement.
const TEST_SHARDS: u16 = 1;
/// Shape: how long a test waits for its daemon's rendezvous to answer after the start.
const START_WAIT: Duration = Duration::from_secs(20);
/// Shape: a scratch volume's quota and an overlay's growth bound: room for the few small files a test writes.
const ROOM: u64 = 4 << 20;
/// Shape: the permission bits of a file a test writes.
const MODE: u32 = 0o644;

/// The machine profile this binary's daemons derive from, measured once per process (§4.1), with the process's one
/// key region reserved for the largest daemon a test starts before any starts
/// (`docs/bugs/2026-09-25-test-fixtures-measured-the-machine-beside-each-other.md`).
fn profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  let profile = PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures");
  static RESERVED: OnceLock<bool> = OnceLock::new();
  let _ = RESERVED.get_or_init(|| {
    let largest = DaemonConfig::derive(&profile, "api-reserve", Some(TEST_SHARDS));
    slates_server::seal_keys::reserve(largest.seal_key_slots)
  });
  profile
}

/// A daemon of this process named after `tag` (its consensus group not made: [`Instance::ensure_group`] makes it), and
/// an instance attached to it, retried while the rendezvous comes up.
fn daemon(tag: &str) -> (Daemon, Instance) {
  static NEXT: AtomicU64 = AtomicU64::new(0);
  let profile = profile();
  let serial = NEXT.fetch_add(1, Ordering::Relaxed);
  let instance = format!("api-{tag}-{}-{serial}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-api-{tag}-{}-{serial}", std::process::id()),
    },
  )
  .unwrap();
  let started = Instant::now();
  let attached = loop {
    match Instance::attach(&instance) {
      Ok(attached) => break attached,
      Err(error) if error.is_unavailable() && started.elapsed() < START_WAIT => {
        std::thread::yield_now();
      }
      Err(error) => panic!("attaching {instance}: {error}"),
    }
  };
  (daemon, attached)
}

/// A daemon and an instance with its consensus group made, as a host makes it for an instance it runs alone.
fn grouped(tag: &str) -> (Daemon, Instance) {
  let (daemon, mut instance) = daemon(tag);
  instance.ensure_group().unwrap();
  (daemon, instance)
}

/// A real directory for an overlay's base and a landing's target (A-50: never `/tmp`, never a RAM directory): in the
/// build output, named with the process id and a counter, canonical, removed on drop.
struct Folder {
  path: String,
}

impl Folder {
  fn new() -> Folder {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let made = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
      "slates-api-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&made).unwrap();
    Folder {
      path: std::fs::canonicalize(&made)
        .unwrap()
        .to_string_lossy()
        .into_owned(),
    }
  }

  fn write(&self, name: &str, bytes: &[u8]) {
    let path = Path::new(&self.path).join(name);
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
  }

  fn read(&self, name: &str) -> Option<Vec<u8>> {
    std::fs::read(Path::new(&self.path).join(name)).ok()
  }
}

impl Drop for Folder {
  fn drop(&mut self) {
    let _removed = std::fs::remove_dir_all(&self.path);
  }
}

/// Writes `bytes` as the whole of `path` in `volume` through the typed file channel, under a write attachment held for
/// the one write.
fn write_file(instance: &mut Instance, volume: VolumeId, path: &str, bytes: &[u8]) {
  let attached = instance.attach_volume(volume, Intent::Write).unwrap();
  instance
    .client()
    .fs_write((volume, attached.attachment), path, bytes, MODE)
    .unwrap();
  instance.detach(attached.attachment).unwrap();
}

/// `path`'s bytes at `volume`'s head.
fn read_file(instance: &mut Instance, volume: VolumeId, path: &str) -> Vec<u8> {
  instance
    .client()
    .read(volume, path, slates_ipc::protocol::ReadAt::Head)
    .unwrap()
}

/// The landing of `volume`'s head into `folder`, `.git` left out as kiln's landings leave it.
fn landing_into(volume: VolumeId, folder: &Folder) -> slates_api::LandingRequest {
  slates_api::LandingRequest {
    volume,
    snapshot: None,
    target: folder.path.clone(),
    filter: Filter {
      include: Vec::new(),
      exclude: vec!["/.git".to_owned()],
    },
  }
}

/// The issuer authority of `daemon`'s instance: its anchor segment's handoff, as the anchor hands it to its children.
fn issuer_of(daemon: &Daemon) -> slates_api::Issuer {
  slates_api::Issuer::from_handoff(&daemon.segment().handoff_env().unwrap()).unwrap()
}

/// T-5.1's library form (A-278, §4.7): do attach to an instance no daemon serves; expect the typed `Unavailable`,
/// never a hang or a panic, naming the instance asked.
#[test]
fn attaching_where_no_daemon_serves_is_refused_unavailable() {
  let name = format!("api-nobody-{}", std::process::id());
  let refused = Instance::attach(&name).unwrap_err();
  assert!(refused.is_unavailable(), "{refused:?}");
  assert!(
    matches!(&refused, Error::Unavailable { instance, .. } if *instance == name),
    "{refused:?}"
  );
}

/// AC-5.12 / T-5.14 (A-278; §4.8 `bootstrap root`): do make an instance's group twice; expect both to succeed (a host's start path may run it
/// again), and a volume created only once the group exists, while before it the create is refused
/// `ConsensusNotInitialized` — the refusal a host meets if it skips the step.
#[test]
fn a_group_is_made_once_and_made_again_harmlessly_before_any_volume() {
  let (daemon, mut instance) = daemon("group");
  assert_eq!(
    instance.create(&slates_api::scratch("early", ROOM)),
    Err(Error::Refused(Refusal::ConsensusNotInitialized))
  );
  instance.ensure_group().unwrap();
  instance.ensure_group().unwrap();
  instance.create(&slates_api::scratch("late", ROOM)).unwrap();
  daemon.stop();
}

/// AC-5.12 / T-5.14 (A-278; §4.4, D-5): do create a scratch volume, write a file, snapshot, clone the snapshot, write the origin
/// again, then destroy the clone; expect the clone to read the snapshot's bytes while the origin reads its own, the
/// names to be listed and then the clone's gone, and a second create of a held name refused `AlreadyExists` naming
/// the holder.
#[test]
fn a_volume_is_created_snapshotted_cloned_and_destroyed() {
  let (daemon, mut instance) = grouped("lifecycle");
  let origin = instance
    .create(&slates_api::scratch("origin", ROOM))
    .unwrap();
  assert_eq!(
    instance.create(&slates_api::scratch("origin", ROOM)),
    Err(Error::Refused(Refusal::AlreadyExists { existing: origin }))
  );
  write_file(&mut instance, origin, "/f", b"at the snapshot");
  let snapshot: SnapshotId = instance.snapshot(origin).unwrap();
  let clone = instance.clone_snapshot(origin, snapshot, "clone").unwrap();
  write_file(&mut instance, origin, "/f", b"the origin's own");
  assert_eq!(read_file(&mut instance, clone, "/f"), b"at the snapshot");
  assert_eq!(read_file(&mut instance, origin, "/f"), b"the origin's own");
  let mut names: Vec<String> = instance
    .list()
    .unwrap()
    .into_iter()
    .map(|v| v.name)
    .collect();
  names.sort();
  assert_eq!(names, ["clone", "origin"]);
  instance.destroy(clone).unwrap();
  let started = Instant::now();
  loop {
    let names: Vec<String> = instance
      .list()
      .unwrap()
      .into_iter()
      .map(|v| v.name)
      .collect();
    if names == ["origin"] {
      break;
    }
    assert!(
      started.elapsed() < START_WAIT,
      "the clone leaves the list: {names:?}"
    );
    std::thread::yield_now();
  }
  daemon.stop();
}

/// AC-5.12 (A-278; §4.15, D-25): do make an overlay spec over a relative directory; expect the typed `NotAbsolute`, since
/// the daemon would resolve it against its own working directory.
#[test]
fn an_overlay_over_a_relative_directory_is_refused_before_the_daemon() {
  assert_eq!(
    slates_api::overlay("o", Path::new("relative/dir"), ROOM).unwrap_err(),
    Error::Path {
      path: "relative/dir".to_owned(),
      fault: PathFault::NotAbsolute
    }
  );
}

/// AC-5.10 (A-278; §4.13 "Grants"): do make an issuer from no handoff, and from a handoff with no or a malformed length; expect the
/// typed `NoHandoff` and `Malformed` — a process with no anchor in its environment holds no authority.
#[test]
fn an_issuer_needs_the_anchors_handoff() {
  assert_eq!(
    slates_api::Issuer::from_handoff(&[]).unwrap_err(),
    Error::Issuer(IssuerFault::NoHandoff)
  );
  assert!(matches!(
    slates_api::Issuer::from_handoff(&[("SLATES_ANCHOR".to_owned(), "x".to_owned())]),
    Err(Error::Issuer(IssuerFault::Malformed { .. }))
  ));
  assert!(matches!(
    slates_api::Issuer::from_handoff(&[
      ("SLATES_ANCHOR".to_owned(), "x".to_owned()),
      ("SLATES_ANCHOR_LEN".to_owned(), "many".to_owned())
    ]),
    Err(Error::Issuer(IssuerFault::Malformed { .. }))
  ));
}

/// AC-5.10 / T-5.12 and AC-5.12 / T-5.14 (A-278; §4.15, D-25, D-26) on the landing a host drives (Unix: the daemon lands on Unix). Do: lay an
/// overlay over a folder holding a file and a `.git`, change the file, add one and change `.git/HEAD` in the volume;
/// plan the landing with `/.git` left out; land it under a grant bound to the approved manifest. Expect: the folder
/// untouched until the landing; the plan presented with a manifest and no conflict; the landing `done` and the folder
/// then holding exactly the volume's two files, `.git/HEAD` as it was; a grant for a manifest the person never saw is
/// never asked for — the approval of a stale manifest writes nothing and returns the new plan.
#[cfg(unix)]
#[test]
fn an_overlay_lands_exactly_what_was_approved_and_nothing_else() {
  let (daemon, mut instance) = grouped("landing");
  let folder = Folder::new();
  folder.write("a.txt", b"a\n");
  folder.write(".git/HEAD", b"ref: refs/heads/main\n");
  let volume = instance
    .create(&slates_api::overlay("work", Path::new(&folder.path), ROOM).unwrap())
    .unwrap();
  assert_eq!(read_file(&mut instance, volume, "/a.txt"), b"a\n");
  write_file(&mut instance, volume, "/a.txt", b"a2\n");
  write_file(&mut instance, volume, "/n.txt", b"n\n");
  write_file(
    &mut instance,
    volume,
    "/.git/HEAD",
    b"ref: refs/heads/agent\n",
  );
  assert_eq!(
    folder.read("a.txt").unwrap(),
    b"a\n",
    "the folder is as it was"
  );
  assert_eq!(folder.read("n.txt"), None);

  let request = landing_into(volume, &folder);
  let Planned::Plan(plan) = instance.plan_landing(&request).unwrap() else {
    panic!("a landing with changes needs a grant");
  };
  assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
  assert!(plan.summary.filtered_out >= 1, "{:?}", plan.summary);
  assert_eq!(plan.manifest_text().len(), 64);

  let issuer = issuer_of(&daemon);
  let stale = [0u8; 32];
  let changed = instance
    .land_approved(
      &issuer,
      &request,
      &stale,
      (slates_api::GrantScope::Once, slates_api::GRANT_TERM_NS),
    )
    .unwrap();
  assert!(
    matches!(&changed, slates_api::Approved::Changed(again) if again.manifest == plan.manifest),
    "an approval of another manifest is no approval of this one: {changed:?}"
  );
  assert_eq!(folder.read("a.txt").unwrap(), b"a\n", "nothing was written");

  let landed = instance
    .land_approved(
      &issuer,
      &request,
      &plan.manifest,
      (slates_api::GrantScope::Once, slates_api::GRANT_TERM_NS),
    )
    .unwrap();
  let slates_api::Approved::Landed(outcome) = landed else {
    panic!("the approved manifest lands: {landed:?}");
  };
  assert_eq!(outcome.state, "done", "{outcome:?}");
  assert_eq!(folder.read("a.txt").unwrap(), b"a2\n");
  assert_eq!(folder.read("n.txt").unwrap(), b"n\n");
  assert_eq!(
    folder.read(".git/HEAD").unwrap(),
    b"ref: refs/heads/main\n",
    "git's own files were left out"
  );
  assert!(
    !instance.audit(0).unwrap().is_empty(),
    "the grant and the landing are audited"
  );
  daemon.stop();
}

/// AC-5.12 / T-5.14 (A-278; §4.15 "Drift"): do change a file in an overlay, then change the same file on the disk; expect the drift
/// list to name it and the landing's plan to carry it as a conflict; and a landing under a grant id nobody issued to
/// write nothing and present its plan again (an unknown grant is no grant).
#[cfg(unix)]
#[test]
fn a_file_changed_on_disk_after_the_volume_changed_it_is_drift_and_a_conflict() {
  let (daemon, mut instance) = grouped("drift");
  let folder = Folder::new();
  folder.write("d.txt", b"base\n");
  let volume = instance
    .create(&slates_api::overlay("drifting", Path::new(&folder.path), ROOM).unwrap())
    .unwrap();
  write_file(&mut instance, volume, "/d.txt", b"the volume's\n");
  assert!(instance.drift(volume).unwrap().is_empty());
  folder.write("d.txt", b"the person's\n");
  let drifted = instance.drift(volume).unwrap();
  assert!(
    drifted.iter().any(|entry| entry.starts_with("/d.txt")),
    "{drifted:?}"
  );
  let request = landing_into(volume, &folder);
  let Planned::Plan(plan) = instance.plan_landing(&request).unwrap() else {
    panic!("a drifted overlay still presents its plan");
  };
  assert!(
    plan.conflicts.iter().any(|c| c.contains("d.txt")),
    "{:?}",
    plan.conflicts
  );
  let unissued = instance.land(&request, u64::MAX).unwrap();
  assert!(
    matches!(&unissued, Planned::Plan(again) if again.manifest == plan.manifest),
    "a grant nobody issued lands nothing: {unissued:?}"
  );
  assert_eq!(folder.read("d.txt").unwrap(), b"the person's\n");
  daemon.stop();
}

/// AC-5.12 / T-5.14 (A-278; §4.14): do read the daemon's status and a shard's telemetry through the library; expect the daemon's shards reported
/// and the drain answered for the shard it has.
#[test]
fn the_daemon_is_observed_through_the_library() {
  let (daemon, mut instance) = grouped("observe");
  let report = instance.daemon_status().unwrap();
  assert_eq!(report.shards.len(), usize::from(TEST_SHARDS));
  instance.telemetry(0).unwrap();
  assert!(instance.grants().unwrap().is_empty());
  daemon.stop();
}

/// The Shards route by use (§4.6, A-161): the stream end the library hands a VMM serves the volume.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod shards {
  use std::io::{Read, Write};
  use std::os::unix::net::UnixStream;

  use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};

  use super::*;

  /// Format: the FUSE node id of the attachment's root.
  const ROOT: u64 = 1;
  /// Format: Linux `ENOENT`, the errno FUSE carries.
  const ENOENT: i32 = 2;
  /// Format: the FUSE protocol a guest kernel offers (7.31), and the largest read it asks.
  const MAJOR: u32 = 7;
  const MINOR: u32 = 31;
  const MAX_READAHEAD: u32 = 128 << 10;

  /// Shards' fs device as the end's holder speaks it: a little-endian length, then the FUSE request; one reply frame
  /// back.
  struct Device {
    stream: UnixStream,
    unique: u64,
  }

  impl Device {
    fn ask(&mut self, opcode: Opcode, nodeid: u64, body: &[u8]) -> Vec<u8> {
      self.unique += 1;
      let total = IN_HEADER_LEN + body.len();
      let mut request = vec![0u8; total];
      request[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
      request[4..8].copy_from_slice(&opcode.to_wire().to_le_bytes());
      request[8..16].copy_from_slice(&self.unique.to_le_bytes());
      request[16..24].copy_from_slice(&nodeid.to_le_bytes());
      request[IN_HEADER_LEN..].copy_from_slice(body);
      self
        .stream
        .write_all(&u32::try_from(total).unwrap().to_le_bytes())
        .unwrap();
      self.stream.write_all(&request).unwrap();
      let mut word = [0u8; 4];
      self.stream.read_exact(&mut word).unwrap();
      let mut reply = vec![0u8; usize::try_from(u32::from_le_bytes(word)).unwrap()];
      self.stream.read_exact(&mut reply).unwrap();
      reply
    }
  }

  fn errno(reply: &[u8]) -> i32 {
    -i32::from_le_bytes(reply[4..8].try_into().unwrap())
  }

  /// AC-5.12 / T-5.14 (A-278; §4.6, the Shards route, A-161): do serve a volume holding a file over the library's Shards stream, then
  /// speak FUSE through the end as Shards' device does; expect INIT answered and the file found by LOOKUP, a name the
  /// volume lacks answered `ENOENT` — the end serves that volume and no host mount is involved.
  #[test]
  fn a_shards_stream_end_serves_the_volume_it_was_made_for() {
    let (daemon, mut instance) = grouped("shards");
    let volume = instance
      .create(&slates_api::scratch("guest", ROOM))
      .unwrap();
    write_file(&mut instance, volume, "/present", b"through the stream\n");
    let served = instance.serve_shards(volume, Intent::Write, None).unwrap();
    let stream = UnixStream::from(served.stream);
    stream.set_read_timeout(Some(START_WAIT)).unwrap();
    let mut device = Device { stream, unique: 0 };
    let mut init = Vec::new();
    for word in [MAJOR, MINOR, MAX_READAHEAD, 0] {
      init.extend_from_slice(&word.to_le_bytes());
    }
    assert_eq!(errno(&device.ask(Opcode::Init, 0, &init)), 0, "INIT");
    let found = device.ask(Opcode::Lookup, ROOT, b"present\0");
    assert_eq!(errno(&found), 0, "LOOKUP of the volume's file");
    assert!(found.len() > OUT_HEADER_LEN);
    let missing = device.ask(Opcode::Lookup, ROOT, b"absent\0");
    assert_eq!(errno(&missing), ENOENT, "a name the volume lacks is ENOENT");
    drop(device);
    daemon.stop();
  }
}

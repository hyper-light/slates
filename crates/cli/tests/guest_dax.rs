//! A real VM over a slates mount, with DAX (`docs/wip/virtiofs.md` §8 item 7; §4.6 "Cache posture"; A-139, A-140,
//! A-146, A-248). A slates volume reaches a VM as a host mount the VMM shares in, with its own DAX window over the host's
//! cached copy of each file. Here crosvm's own virtio-fs device shares a `slates mount` folder into a guest whose
//! `/init` is `slates-guest-probe`. The test drives the guest over its serial console ([`slates_guest_probe::protocol`])
//! while it writes the same file through a second mount of the volume, the guest's other path.
//!
//! Gated. It needs Linux with KVM, and three files named in the environment; without them it skips loudly:
//! - `SLATES_GUEST_CROSVM`: a crosvm binary. Its device sandbox needs unprivileged user namespaces, which Linux
//!   distributions allow by default; with `--disable-sandbox` crosvm's virtio-fs device needs root.
//! - `SLATES_GUEST_KERNEL`: a guest kernel with virtio-fs and FUSE DAX built in (`CONFIG_VIRTIO_FS=y`,
//!   `CONFIG_FUSE_DAX=y`), such as many distributions' own.
//! - `SLATES_GUEST_PROBE`: the guest's `/init`, static, built by
//!   `RUSTFLAGS="-C target-feature=+crt-static" cargo build --release -p slates-guest-probe --target x86_64-unknown-linux-musl`
//!   (the workspace's own musl rustflags leave the C runtime dynamic, for its cdylibs).
//!
//! The guest's initramfs is written into a second slates volume through its mount, so the test writes nothing to disk
//! but its mount points (in `CARGO_TARGET_TMPDIR`, named with the process id, removed at the end).
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod vm;

use std::process::Command;

use slates_guest_probe::protocol::{Command as Ask, Reply};
use vm::{
  Anchor, DELIVERY_MILLIS, Guest, MountPoint, REPLY_WAIT, created_volume, initramfs,
  kill_the_daemon, run, start_anchor, write_at, write_whole,
};

/// Shape: the pages of each shared file; the guest's memory and processors.
const PAGES: u64 = 8;
/// Shape: the guest's memory, MiB.
const GUEST_MEMORY_MIB: u32 = 512;
/// Shape: the guest's processors.
const GUEST_CPUS: u32 = 2;
/// Shape: the generated histories' seeds, fixed so a failure is a sequence that replays, three so one lucky sequence
/// cannot pass alone; and each history's steps.
const SEEDS: [u64; 3] = [
  0x9e37_79b9_7f4a_7c15,
  0xd1b5_4a32_d192_ed03,
  0x2545_f491_4f6c_dd1d,
];
/// Shape: the steps of each generated history.
const STEPS: usize = 80;
/// Shape: the longest run of bytes one write or store covers: enough to cross a page now and then.
const LONGEST_RUN: u64 = 6000;
/// Format: the byte each shared file starts with.
const SEED_BYTE: u8 = b'.';
/// Format: the guest's stores take values from `[GUEST_FIRST, HOST_FIRST)` and the other path's writes from
/// `[HOST_FIRST, 256)`, each a fresh value per write, so every byte names its writer, and a wait for a write's value
/// can only end once that write has arrived.
const GUEST_FIRST: u8 = 1;
/// Format: see [`GUEST_FIRST`].
const HOST_FIRST: u8 = 128;
/// Format: the daemon's status counter of cache write-backs refused whole as conflicts (A-248).
const CONFLICTS: &str = "fuse.writeback_conflicts: ";

/// The paths the test needs from the environment, or `None` after saying loudly which are missing.
struct Gate {
  crosvm: String,
  kernel: String,
  probe: String,
}

impl Gate {
  fn from_env() -> Option<Gate> {
    let named = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    match (
      named("SLATES_GUEST_CROSVM"),
      named("SLATES_GUEST_KERNEL"),
      named("SLATES_GUEST_PROBE"),
    ) {
      (Some(crosvm), Some(kernel), Some(probe)) => Some(Gate {
        crosvm,
        kernel,
        probe,
      }),
      _ => {
        eprintln!(
          "SKIP: the guest-over-the-mount test needs SLATES_GUEST_CROSVM, SLATES_GUEST_KERNEL and SLATES_GUEST_PROBE \
           (tests/guest_dax.rs says how to build the probe)"
        );
        None
      }
    }
  }
}

/// Boots a guest under crosvm with `shared` (a slates mount) shared in as tag `slates` with DAX, and waits until it is
/// ready.
fn boot_crosvm(gate: &Gate, initrd: &str, shared: &str, most_bytes: usize) -> Guest {
  let shared_dir = format!(
    "{shared}:slates:type=fs:cache=always:timeout=3600:dax=true:writeback=false:posix_acl=false:security_ctx=false"
  );
  Guest::spawn(
    Command::new(&gate.crosvm).args([
      "run",
      "-m",
      &GUEST_MEMORY_MIB.to_string(),
      "--cpus",
      &GUEST_CPUS.to_string(),
      "--shared-dir",
      &shared_dir,
      "--initrd",
      initrd,
      "-p",
      "console=ttyS0 rdinit=/init quiet slates.dax=always",
      "--serial",
      "type=stdout,hardware=serial,num=1,console,stdin",
      &gate.kernel,
    ]),
    "crosvm: ",
    most_bytes,
  )
}

/// What each byte of a shared file may hold once the guest has synced, by the dirty-page write-back rule (A-248). The
/// latest write to it from either side, exactly, unless its page is at risk: the other path wrote into the page while
/// it held guest stores not yet synced, so the page's write-back, the guest's stores with the bytes it fetched before
/// the other write, may have been refused whole. Then the byte holds its settled value (the seed, the other path's
/// write, or what the last check saw) or one of the guest's stores since, never anything older, and never a stale byte
/// over the other path's write. The history makes the rest exact: after each of the other path's writes, the guest
/// waits until both ends of the write show in its map, so the guest's later stores land in pages already re-fetched
/// (the AC-3.3 residual, a store into a page whose change is still on its way, never arises).
struct Expected {
  /// The latest write to each byte from either side.
  bytes: Vec<u8>,
  /// Each byte's settled value and the guest's stores to it since, in order.
  history: Vec<(u8, Vec<u8>)>,
  /// The system's page size: the unit of a write-back.
  page: u64,
  /// Pages the guest stored into since its last sync.
  dirty: std::collections::BTreeSet<u64>,
  /// Pages the other path wrote into while they held unsynced guest stores, since the last check.
  at_risk: std::collections::BTreeSet<u64>,
}

impl Expected {
  fn new(len: u64) -> Expected {
    let bytes = vec![SEED_BYTE; usize::try_from(len).unwrap()];
    Expected {
      history: bytes.iter().map(|&byte| (byte, Vec::new())).collect(),
      bytes,
      page: u64::try_from(rustix::param::page_size()).unwrap(),
      dirty: std::collections::BTreeSet::new(),
      at_risk: std::collections::BTreeSet::new(),
    }
  }

  /// The byte range `[at, at + len)` and the pages it touches.
  fn span(&self, at: u64, len: u64) -> (std::ops::Range<usize>, std::ops::RangeInclusive<u64>) {
    let end = at.checked_add(len).unwrap();
    let bytes = usize::try_from(at).unwrap()..usize::try_from(end).unwrap();
    (bytes, at / self.page..=end.saturating_sub(1) / self.page)
  }

  /// The guest stored `value` over `[at, at + len)`, unsynced.
  fn guest_stored(&mut self, at: u64, len: u64, value: u8) {
    let (bytes, pages) = self.span(at, len);
    self.bytes[bytes.clone()].fill(value);
    for (_, stores) in &mut self.history[bytes] {
      stores.push(value);
    }
    self.dirty.extend(pages);
  }

  /// The other path wrote `value` over `[at, at + len)`: every page it touched that held unsynced guest stores is at
  /// risk.
  fn other_path_wrote(&mut self, at: u64, len: u64, value: u8) {
    let (bytes, pages) = self.span(at, len);
    self.bytes[bytes.clone()].fill(value);
    for byte in &mut self.history[bytes] {
      *byte = (value, Vec::new());
    }
    let risked: Vec<u64> = pages.filter(|page| self.dirty.contains(page)).collect();
    self.at_risk.extend(risked);
  }

  /// The guest synced: its stores so far were written back, or refused if their page is at risk.
  fn guest_synced(&mut self) {
    self.dirty.clear();
  }

  /// Whether byte `at` may hold `value` (see the type).
  fn allows(&self, at: usize, value: u8) -> bool {
    let page = u64::try_from(at).unwrap() / self.page;
    if !self.at_risk.contains(&page) {
      return self.bytes[at] == value;
    }
    let (settled, stores) = &self.history[at];
    *settled == value || stores.contains(&value)
  }

  /// Panics naming the first bytes of `actual` the rule does not allow, and which side wrote each.
  fn assert_held_by(&self, actual: &[u8], what: &str, guest: &Guest) {
    let allowed = actual.len() == self.bytes.len()
      && actual
        .iter()
        .enumerate()
        .all(|(at, &value)| self.allows(at, value));
    if allowed {
      return;
    }
    let writer = |value: u8| match value {
      SEED_BYTE => "the seed",
      value if value >= HOST_FIRST => "the other path",
      _ => "the guest",
    };
    let wrong: Vec<String> = actual
      .iter()
      .enumerate()
      .filter(|&(at, &value)| at >= self.bytes.len() || !self.allows(at, value))
      .take(8)
      .map(|(at, &held)| {
        let latest = self.bytes.get(at).copied().unwrap_or(0);
        format!(
          "byte {at}: holds {held} ({}), the latest write {latest} ({}), page at risk {}",
          writer(held),
          writer(latest),
          self
            .at_risk
            .contains(&(u64::try_from(at).unwrap() / self.page))
        )
      })
      .collect();
    panic!(
      "{what} holds a byte the write-back rule does not allow (lengths {} and {}):\n{}\n{}",
      actual.len(),
      self.bytes.len(),
      wrong.join("\n"),
      guest.transcript()
    );
  }

  /// A check saw `held`, which the rule allowed: it is every byte's settled value from here on. Answers how many bytes
  /// held something older than their latest write: guest stores lost with a refused write-back.
  fn settle(&mut self, held: &[u8]) -> usize {
    let lost = held
      .iter()
      .zip(&self.bytes)
      .filter(|(held, latest)| held != latest)
      .count();
    self.bytes = held.to_vec();
    self.history = held.iter().map(|&byte| (byte, Vec::new())).collect();
    self.dirty.clear();
    self.at_risk.clear();
    lost
  }
}

/// The next value of a fixed-seed xorshift64 generator (Marsaglia, "Xorshift RNGs", 2003), as the other chaos tests
/// use.
fn xorshift(state: &mut u64) -> u64 {
  let mut x = *state;
  x ^= x << 13;
  x ^= x >> 7;
  x ^= x << 17;
  *state = x;
  x
}

/// The next fresh value from `[first, end)`, cycling.
fn next_value(counter: &mut u8, first: u8, end: u16) -> u8 {
  let span = end.saturating_sub(u16::from(first));
  let value = u16::from(first).saturating_add(u16::from(*counter) % span);
  *counter = counter.wrapping_add(1);
  u8::try_from(value).unwrap()
}

/// A run within a file of `len` bytes, from `roll`'s bits: its start and length.
fn run_within(roll: u64, len: u64) -> (u64, u64) {
  let at = (roll >> 8) % len;
  let run = ((roll >> 32) % LONGEST_RUN).saturating_add(1).min(len - at);
  (at, run)
}

/// What one generated history did.
#[derive(Default)]
struct Tally {
  stores: u32,
  writes: u32,
  syncs: u32,
  /// Syncs the guest was told failed: a write-back of its stores was refused (A-248).
  refused_syncs: u32,
  checks: u32,
  /// Bytes a check saw hold something older than their latest write: the guest's stores lost with a refused
  /// write-back.
  lost_bytes: usize,
  slowest_arrival_micros: u64,
}

/// Waits until both ends of the other path's write of `value` over `[at, at + run)` show in the guest's map, memory
/// alone: the first and the last page of the write, since the kernel drops a written range's pages in order. The
/// slower of the two waits, in microseconds.
fn arrival_in_the_map((at, run): (u64, u64), value: u8, guest: &mut Guest) -> u64 {
  let mut slowest = 0;
  for end in [at, at.saturating_add(run).saturating_sub(1)] {
    match guest.ask(
      &Ask::Wait {
        at: end,
        byte: value,
        millis: DELIVERY_MILLIS,
      },
      REPLY_WAIT,
    ) {
      Reply::Waited { micros } => slowest = slowest.max(micros),
      other => panic!(
        "the other path's write of {value} at [{at}, {at} + {run}) did not reach the guest's map at {end}: \
         {other:?}\n{}",
        guest.transcript()
      ),
    }
  }
  slowest
}

/// One generated history over one shared file: the model of what every byte must hold, the generator, and the tally.
/// It can stop and go on around a daemon's death or a VM's.
struct History<'r> {
  rig: &'r Rig,
  file: String,
  guest_file: String,
  host_file: String,
  len: u64,
  expected: Expected,
  state: u64,
  guest_counter: u8,
  host_counter: u8,
  tally: Tally,
}

impl<'r> History<'r> {
  /// Writes `file` through the guest's mount, has `guest` map it with DAX, and seeds the generator.
  fn start(rig: &'r Rig, file: &str, seed: u64, guest: &mut Guest) -> History<'r> {
    let len = PAGES.saturating_mul(u64::try_from(rustix::param::page_size()).unwrap());
    let history = History {
      rig,
      file: file.to_owned(),
      guest_file: rig.guest_side.file(file),
      host_file: rig.host_side.file(file),
      len,
      expected: Expected::new(len),
      state: seed,
      guest_counter: 0,
      host_counter: 0,
      tally: Tally::default(),
    };
    write_whole(&history.guest_file, &history.expected.bytes);
    history.map(guest);
    history
  }

  /// Has `guest` map the file, which must be served through the DAX window.
  fn map(&self, guest: &mut Guest) {
    match guest.ask(
      &Ask::Map {
        path: self.file.clone(),
        pages: PAGES,
      },
      REPLY_WAIT,
    ) {
      Reply::Mapped { dax: true } => {}
      other => panic!(
        "map {} → {other:?}: DAX is what this test is for\n{}",
        self.file,
        guest.transcript()
      ),
    }
  }

  /// A span of the file for a step chosen outside [`History::steps`].
  fn span(&mut self) -> (u64, u64) {
    let roll = xorshift(&mut self.state);
    run_within(roll, self.len)
  }

  /// The guest stores a fresh value over `span`, unsynced.
  fn guest_stores(&mut self, (at, run): (u64, u64), guest: &mut Guest) {
    let value = next_value(&mut self.guest_counter, GUEST_FIRST, u16::from(HOST_FIRST));
    guest.done(&Ask::Store {
      at,
      len: run,
      byte: value,
    });
    self.expected.guest_stored(at, run, value);
    self.tally.stores = self.tally.stores.saturating_add(1);
  }

  /// The other path writes a fresh value over `span`. With a live guest, the guest waits until the write shows in its
  /// map ([`arrival_in_the_map`]).
  fn other_path_writes(&mut self, (at, run): (u64, u64), guest: Option<&mut Guest>) {
    let value = next_value(&mut self.host_counter, HOST_FIRST, 256);
    write_at(
      &self.host_file,
      at,
      &vec![value; usize::try_from(run).unwrap()],
    );
    if let Some(guest) = guest {
      let arrival = arrival_in_the_map((at, run), value, guest);
      self.tally.slowest_arrival_micros = self.tally.slowest_arrival_micros.max(arrival);
    }
    self.expected.other_path_wrote(at, run, value);
    self.tally.writes = self.tally.writes.saturating_add(1);
  }

  /// `count` generated steps: the guest's unsynced stores, its syncs, the other path's writes, and checks.
  fn steps(&mut self, count: usize, guest: &mut Guest) {
    for _ in 0..count {
      let roll = xorshift(&mut self.state);
      let span = run_within(roll, self.len);
      match roll % 10 {
        0..=3 => self.guest_stores(span, guest),
        4..=6 => self.other_path_writes(span, Some(guest)),
        7 | 8 => self.sync(guest),
        _ => self.check(guest),
      }
    }
  }

  /// The guest syncs its map. A sync may be refused, `EIO`, where a write-back of the guest's stores was (A-248): the
  /// guest is told, as a program is at its `msync`.
  fn sync(&mut self, guest: &mut Guest) {
    match guest.ask(&Ask::Sync, REPLY_WAIT) {
      Reply::Done => {}
      Reply::Refused(why) if why.starts_with("msync") => {
        self.tally.refused_syncs = self.tally.refused_syncs.saturating_add(1);
      }
      other => panic!("sync → {other:?}\n{}", guest.transcript()),
    }
    self.expected.guest_synced();
    self.tally.syncs = self.tally.syncs.saturating_add(1);
  }

  /// Syncs the guest, then holds the volume, read through slates itself past both mounts, to the write-back rule, and
  /// the guest's map to the volume, byte for byte; what they hold is settled from here on.
  fn check(&mut self, guest: &mut Guest) {
    self.sync(guest);
    let held = self.volume_holds_the_latest_writes(guest);
    match guest.ask(
      &Ask::Read {
        at: 0,
        len: self.len,
      },
      REPLY_WAIT,
    ) {
      Reply::Bytes(mapped) => assert!(
        mapped == held,
        "the guest's map of {} differs from the volume: {} byte(s)\n{}",
        self.file,
        mapped.iter().zip(&held).filter(|(a, b)| a != b).count(),
        guest.transcript()
      ),
      other => panic!("read → {other:?}\n{}", guest.transcript()),
    }
    let lost = self.expected.settle(&held);
    self.tally.lost_bytes = self.tally.lost_bytes.saturating_add(lost);
    self.tally.checks = self.tally.checks.saturating_add(1);
  }

  /// Holds the volume's file, read through slates itself past both mounts, to the write-back rule ([`Expected`]), and
  /// answers its bytes; `guest` only lends its console's transcript to a failure.
  fn volume_holds_the_latest_writes(&self, guest: &Guest) -> Vec<u8> {
    let (code, held, err) = run(
      &self.rig.instance,
      &["read", &self.rig.shared, &format!("/{}", self.file)],
    );
    assert_eq!(
      code, 0,
      "slates read {} /{}: {err}",
      self.rig.shared, self.file
    );
    self.expected.assert_held_by(
      &held,
      &format!("the volume's {}, read through slates", self.file),
      guest,
    );
    held
  }
}

/// The daemon's cache write-backs refused as conflicts so far, over every shard.
fn conflicts(instance: &str) -> u64 {
  let (code, out, err) = run(instance, &["status"]);
  assert_eq!(code, 0, "slates status: {err}");
  String::from_utf8_lossy(&out)
    .lines()
    .filter_map(|line| line.split_once(CONFLICTS))
    .filter_map(|(_, count)| count.trim().parse::<u64>().ok())
    .sum()
}

/// What every guest test stands on: an anchor; the shared volume mounted twice, once to be shared into the VM and once
/// as the host's other path; and a second volume holding the guest's initramfs. Fields drop in order, so the mounts go
/// before the anchor.
struct Rig {
  instance: String,
  shared: String,
  guest_side: MountPoint,
  host_side: MountPoint,
  _boot_side: MountPoint,
  initrd: String,
  most_bytes: usize,
  anchor: Anchor,
}

impl Rig {
  /// Starts an anchor for an instance named after `tag`, makes and mounts the volumes, and writes the initramfs.
  fn new(gate: &Gate, tag: &str) -> Rig {
    let instance = format!("{tag}-{}", std::process::id());
    let anchor = start_anchor(&instance);
    let shared = created_volume(&instance, "shared");
    let boot = created_volume(&instance, "boot");
    let guest_side = MountPoint::of(&instance, &shared);
    let host_side = MountPoint::of(&instance, &shared);
    let boot_side = MountPoint::of(&instance, &boot);
    #[allow(clippy::disallowed_methods)]
    // reads the built probe binary: a test input, never written
    let probe = std::fs::read(&gate.probe).unwrap_or_else(|e| panic!("read {}: {e}", gate.probe));
    let initrd = boot_side.file("initramfs.cpio");
    write_whole(&initrd, &initramfs(&probe, &[]));
    let most_bytes = usize::try_from(PAGES)
      .unwrap()
      .saturating_mul(rustix::param::page_size());
    Rig {
      instance,
      shared,
      guest_side,
      host_side,
      _boot_side: boot_side,
      initrd,
      most_bytes,
      anchor,
    }
  }

  /// Boots a guest with the guest side of the shared volume shared in.
  fn boot(&self, gate: &Gate) -> Guest {
    boot_crosvm(gate, &self.initrd, &self.guest_side.path, self.most_bytes)
  }
}

/// Shape: the guest's stores left unsynced across a daemon's or a VM's death.
const UNSYNCED_STORES: usize = 6;
/// Shape: the other path's writes after a VM's death, over spans its unsynced stores may share.
const WRITES_AFTER_THE_KILL: usize = 4;

/// Syncs `path` through its slates mount from the host, so what the host's page cache holds dirty is written back. A
/// refused write-back (A-248) fails the sync, which is one of the rule's outcomes: answered, not raised.
#[allow(clippy::disallowed_methods)] // the test's own sync through a slates mount: RAM, not disk
fn sync_through(path: &str) -> std::io::Result<()> {
  std::fs::File::open(path)?.sync_all()
}

/// §4.6 (the mount route, A-140/A-146/A-248) through a real guest with DAX. Do: share a slates mount into a crosvm
/// guest with DAX, and over each of three fixed-seed files run a generated history of the guest's unsynced stores into
/// its shared map, its syncs, and the other path's writes through a second mount of the volume, each waited for in the
/// guest's map as memory alone. At random points and at the end, sync the guest and read the file through slates
/// itself, past both mounts, and through the guest's map.
///
/// Expect: the guest's file is in DAX mode; every write of the other path reaches the guest's map as memory alone; and
/// at every check the guest's map equals the volume, and every byte holds what the write-back rule allows
/// ([`Expected`]): its latest write, unless the other path wrote into its page over guest stores not yet synced, whose
/// write-back is then refused whole; never a stale byte over the other path's write, which a dirty guest page written
/// back whole would put there (A-139). The conflict count must move, or no history put a guest store and a later write
/// on one page, and the test proved nothing.
#[test]
fn a_guest_maps_a_slates_mount_with_dax_and_holds_the_latest_write_to_every_byte() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let rig = Rig::new(&gate, "guest-dax");
  let mut guest = rig.boot(&gate);
  let mut tallies = Vec::new();
  for (index, seed) in SEEDS.into_iter().enumerate() {
    let mut history = History::start(&rig, &format!("history-{index}"), seed, &mut guest);
    history.steps(STEPS, &mut guest);
    history.check(&mut guest);
    let tally = std::mem::take(&mut history.tally);
    eprintln!(
      "history {index} (seed {seed:#x}): {} guest stores, {} other-path writes (slowest to reach the map: {} µs), \
       {} syncs ({} refused), {} checks, {} byte(s) of guest stores lost with refused write-backs",
      tally.stores,
      tally.writes,
      tally.slowest_arrival_micros,
      tally.syncs,
      tally.refused_syncs,
      tally.checks,
      tally.lost_bytes
    );
    tallies.push(tally);
  }
  let status = guest.quit();
  assert!(
    status.success(),
    "the VM powered off cleanly: {status}\n{}",
    guest.transcript()
  );
  let refused = conflicts(&rig.instance);
  eprintln!("the guest's write-backs refused as conflicts: {refused}");
  assert!(
    refused > 0,
    "non-vacuous: some history wrote through the other path over a page the guest had stored into unsynced"
  );
  assert!(
    tallies
      .iter()
      .all(|tally| tally.writes > 0 && tally.stores > 0 && tally.checks > 1),
    "every history stored, wrote through the other path and checked more than once"
  );
}

/// §4.6 with a takeover (A-61) under a real guest with DAX. Do: run half a generated history; leave several of the
/// guest's stores unsynced in its map; kill the daemon with `SIGKILL` while the guest holds the live map; wait for the
/// anchor's successor to answer; run the rest of the history on the same map.
///
/// Expect: the successor is a new process, and every check before and after holds the map equal to the volume and every
/// byte to the write-back rule ([`Expected`]), the stores left unsynced across the kill included. The restored mount judges the guest's
/// write-backs without the dead daemon's byte states, against every record since it started (GAPS), which this history
/// stays well inside.
#[test]
fn a_daemon_killed_under_a_guests_live_map_is_taken_over_and_the_guest_goes_on() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let rig = Rig::new(&gate, "guest-takeover");
  let mut guest = rig.boot(&gate);
  let mut history = History::start(&rig, "takeover", SEEDS[0], &mut guest);
  history.steps(STEPS / 2, &mut guest);
  history.check(&mut guest);
  for _ in 0..UNSYNCED_STORES {
    let span = history.span();
    history.guest_stores(span, &mut guest);
  }
  let (old, new) = kill_the_daemon(&rig.anchor, &rig.instance);
  assert_ne!(old, new, "the anchor's successor is a new daemon");
  history.steps(STEPS / 2, &mut guest);
  history.check(&mut guest);
  let tally = &history.tally;
  eprintln!(
    "daemon {old} killed, successor {new}: {} guest stores, {} other-path writes (slowest to reach the map: {} µs), \
     {} checks",
    tally.stores, tally.writes, tally.slowest_arrival_micros, tally.checks
  );
  assert!(
    tally.writes > 0 && tally.checks > 2,
    "non-vacuous: the history wrote through the other path and checked after the takeover"
  );
  let status = guest.quit();
  assert!(
    status.success(),
    "the VM powered off cleanly: {status}\n{}",
    guest.transcript()
  );
}

/// §4.6 through a real guest with DAX when the VM dies. Do: run half a generated history; have the guest store over
/// several spans without syncing; kill the VM and its device processes without warning; write over some spans through
/// the other path; then fsync the file through the guest's side of the mount from the host. Then boot a second guest
/// on the same mount and map the file.
///
/// Expect: the volume holds what the write-back rule allows ([`Expected`]): the dead guest's unsynced stores wherever no
/// later write touched their page, the other path's writes everywhere they wrote, and where they wrote into a page of
/// unsynced stores, those stores or what the page held before them; and the second guest's map equals the volume. The
/// host's page cache outlives the VM that dirtied it, so its stores are written back, judged by the rule like any other
/// (A-140, A-248); the host's sync fails where one of them was refused, which the test prints.
#[test]
fn a_guest_killed_with_unsynced_stores_leaves_them_to_land_and_a_new_guest_reads_them() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let rig = Rig::new(&gate, "guest-crash");
  let mut first = rig.boot(&gate);
  let mut history = History::start(&rig, "crash", SEEDS[1], &mut first);
  history.steps(STEPS / 2, &mut first);
  history.check(&mut first);
  for _ in 0..UNSYNCED_STORES {
    let span = history.span();
    history.guest_stores(span, &mut first);
  }
  first.kill();
  for _ in 0..WRITES_AFTER_THE_KILL {
    let span = history.span();
    history.other_path_writes(span, None);
  }
  history.expected.guest_synced();
  let synced = sync_through(&history.guest_file);
  assert!(
    synced
      .as_ref()
      .err()
      .is_none_or(|e| e.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error())),
    "the host's sync succeeds, or is refused EIO for a refused write-back: {synced:?}"
  );
  history.volume_holds_the_latest_writes(&first);
  let mut second = rig.boot(&gate);
  history.map(&mut second);
  history.check(&mut second);
  eprintln!(
    "the first VM killed with {UNSYNCED_STORES} unsynced stores, {WRITES_AFTER_THE_KILL} writes after it, the host's \
     sync {synced:?}: the volume and a second guest's map hold what the write-back rule allows, {} byte(s) lost",
    history.tally.lost_bytes
  );
  let status = second.quit();
  assert!(
    status.success(),
    "the second VM powered off cleanly: {status}\n{}",
    second.transcript()
  );
}

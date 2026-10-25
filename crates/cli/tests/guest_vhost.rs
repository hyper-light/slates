//! A real VM over slates' own virtio-fs device, with DAX (§4.6; A-168, A-169; goal G-vhost). `slates vhost-user` hands
//! crosvm the socket end of the daemon's device, which crosvm takes as `--vhost-user type=fs,socket={socket}`, and the
//! guest, whose `/init` is `slates-guest-probe`, mounts the tag with `dax=always` and maps a file through the device's
//! window. The window maps a copy of the file that slates keeps equal to the volume: every change by any path is written
//! into it before the change returns, and the guest's stores reach the volume when it syncs or closes the file (A-168).
//! The test drives the guest over its serial console ([`slates_guest_probe::protocol`]) while two other paths change
//! the file: a `slates mount` of the volume, and the SDK's whole-file write.
//!
//! The guest works in a session: a clone of a snapshot of an origin volume. Its writes land in the session itself, so
//! the session holds what the guest left with no merge between (the disk-image route, A-160, merges an image instead).
//!
//! What the guest must see and leave, each held byte for byte to a model of the design's rules ([`Model`]):
//! - another path's change in its map as memory alone the moment the writer is answered, a write that grows the file
//!   and a cut included;
//! - its own stores in the volume once it syncs, and once it closes the file without syncing;
//! - nothing it stores past the file's end in the volume, even once another path extends the file over those bytes: an
//!   extension zeroes the hole it opens, in the volume and in every map (POSIX `mmap`: the system "shall never write
//!   out any modified portions of the last page of an object which are beyond its end");
//! - the session's origin untouched by any of it, and the origin's own later write never in the guest's map (A-166).
//!
//! Gated like `tests/guest_dax.rs`: Linux with KVM and the same three files named in the environment, or it skips
//! loudly. The guest's initramfs is written into a slates volume through its mount, so the test writes nothing to disk
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

use std::time::Instant;

use slates_bridge_virtiofs::backed;
use slates_client::{Client, Intent, VolumeId};
use slates_guest_probe::protocol::{Command as Ask, Reply};
use vm::{
  Anchor, Guest, MountPoint, QUIT_WAIT, REPLY_WAIT, START_WAIT, created_volume, initramfs,
  kill_the_daemon, pause, run, start_anchor, write_at, write_whole,
};

/// Shape: the pages the guest maps; the file never grows past them, so every byte of it lies in the map.
const MAP_PAGES: u64 = 12;
/// Shape: the file's first size in pages, less [`SHORT_OF_A_PAGE`].
const FIRST_PAGES: u64 = 6;
/// Shape: how far the file's first size falls short of a whole page, so its last page holds bytes past its end.
const SHORT_OF_A_PAGE: u64 = 100;
/// Shape: the DAX window `slates vhost-user` asks for: four 2 MiB ranges, of which the one file uses one.
const WINDOW: &str = "8MiB";
/// Shape: the guest's memory, MiB.
const GUEST_MEMORY_MIB: u32 = 512;
/// Shape: the guest's processors.
const GUEST_CPUS: u32 = 2;
/// Shape: the generated histories' seeds, fixed so a failure is a sequence that replays, three so one lucky sequence
/// cannot pass alone.
const SEEDS: [u64; 3] = [
  0x9e37_79b9_7f4a_7c15,
  0xd1b5_4a32_d192_ed03,
  0x2545_f491_4f6c_dd1d,
];
/// Shape: the steps of each generated history.
const STEPS: usize = 80;
/// Shape: the longest run of bytes one write or store covers: enough to cross a page now and then.
const LONGEST_RUN: u64 = 6000;
/// Format: the file the tests map, at the root of the session.
const FILE: &str = "f";
/// Format: the file's mode.
const FILE_MODE: u32 = 0o644;
/// Format: the byte the file starts with. Every other byte but zero names its writer, a fresh value for each write:
/// the guest's stores take values from `[GUEST_FIRST, SDK_FIRST)`, the SDK's writes from `[SDK_FIRST, MOUNT_FIRST)`
/// and the mount's from `[MOUNT_FIRST, SEED_BYTE)`. Zero is what a cut or a hole reads.
const SEED_BYTE: u8 = 255;
/// Format: see [`SEED_BYTE`].
const GUEST_FIRST: u8 = 1;
/// Format: see [`SEED_BYTE`].
const SDK_FIRST: u8 = 64;
/// Format: see [`SEED_BYTE`].
const MOUNT_FIRST: u8 = 128;

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
          "SKIP: the guest-over-slates'-own-device test needs SLATES_GUEST_CROSVM, SLATES_GUEST_KERNEL and \
           SLATES_GUEST_PROBE (tests/guest_dax.rs says how to build the probe)"
        );
        None
      }
    }
  }
}

fn page() -> u64 {
  u64::try_from(rustix::param::page_size()).unwrap()
}

fn ix(at: u64) -> usize {
  usize::try_from(at).unwrap()
}

fn len_of(bytes: &[u8]) -> u64 {
  u64::try_from(bytes.len()).unwrap()
}

/// `at` rounded up to a whole page.
fn page_ceil(at: u64) -> u64 {
  at.div_ceil(page()).checked_mul(page()).unwrap()
}

/// What the session's file and the guest's map must hold, by the design's rules: A-163's write-through, A-167's range
/// past the end, A-168's window over the file's backing. The model states the rules, not the implementation:
/// - every change reaches the map before its writer is answered;
/// - a write past the end zeroes the hole it opens, and a size change zeroes what it cuts or opens, in the map too;
/// - a store of the guest's stays in its map until a change overwrites it;
/// - the guest's stores reach the file when it syncs or closes it, as far as the file's end and no further.
struct Model {
  /// The file, as the volume holds it.
  file: Vec<u8>,
  /// What the guest's map shows: the file's backing over the pages mapped.
  object: Vec<u8>,
  /// The size the guest learned at its last map, from the attributes its `statx` fetched: the device grants no
  /// attribute lifetime (a guest it cannot tell of changes revalidates on every use), so every `statx` asks.
  learned: u64,
  /// The least size the file has had since the guest's last map. Its kernel faults in only pages below its own idea of
  /// the size, refusing one at or past it with `SIGBUS` (`fs/dax.c`), which would kill the probe; and that idea is one
  /// of the sizes the file has had since the guest last asked. So the pages below this one are always safe to touch.
  floor: u64,
}

impl Model {
  fn new(seed: Vec<u8>, map_len: u64) -> Model {
    let mut object = seed.clone();
    object.resize(ix(map_len), 0);
    let size = len_of(&seed);
    Model {
      file: seed,
      object,
      learned: size,
      floor: size,
    }
  }

  fn size(&self) -> u64 {
    len_of(&self.file)
  }

  /// `at` within the map.
  fn clip(&self, at: u64) -> usize {
    ix(at).min(self.object.len())
  }

  /// The map's bytes `[from, to)` read zero, as far as the map reaches.
  fn zero(&mut self, from: u64, to: u64) {
    let (from, to) = (self.clip(from), self.clip(to));
    if from < to {
      self.object[from..to].fill(0);
    }
  }

  /// Another path writes `bytes` at `at`: a hole it opens past the end reads zero, and the bytes land in the file and
  /// in the map.
  fn write(&mut self, at: u64, bytes: &[u8]) {
    let size = self.size();
    if at > size {
      self.zero(size, at);
      self.file.resize(ix(at), 0);
    }
    let end = at.checked_add(len_of(bytes)).unwrap();
    if self.size() < end {
      self.file.resize(ix(end), 0);
    }
    self.file[ix(at)..ix(end)].copy_from_slice(bytes);
    for (offset, byte) in bytes.iter().enumerate() {
      if let Some(held) = self.object.get_mut(ix(at).checked_add(offset).unwrap()) {
        *held = *byte;
      }
    }
  }

  /// Another path sets the file's size to `len`: what the change cuts or opens reads zero.
  fn resize(&mut self, len: u64) {
    let size = self.size();
    self.zero(len.min(size), len.max(size));
    self.file.resize(ix(len), 0);
    self.floor = self.floor.min(len);
  }

  /// The SDK's whole-file write: the file cut to nothing, then written from the start, in one verb, so no reply can
  /// show the guest the empty file between.
  fn replace(&mut self, bytes: &[u8]) {
    let floor = self.floor;
    self.resize(0);
    self.floor = floor.min(len_of(bytes));
    self.write(0, bytes);
  }

  /// The guest stores `value` over `[at, at + len)` of its map.
  fn store(&mut self, at: u64, len: u64, value: u8) {
    self.object[ix(at)..ix(at.checked_add(len).unwrap())].fill(value);
  }

  /// The guest's stores reach the file: an ingest writes the map's bytes that differ from the file's, as far as the
  /// file's end and no further.
  fn sync(&mut self) {
    let end = self.file.len().min(self.object.len());
    self.file[..end].copy_from_slice(&self.object[..end]);
  }

  /// The guest maps the file again: it learns the size, then its earlier map's file closes, and the close ingests.
  fn remap(&mut self) {
    self.learned = self.size();
    self.floor = self.learned;
    self.sync();
  }

  /// The bytes of the map the guest may touch: the pages below [`Model::floor`].
  fn touchable(&self) -> u64 {
    page_ceil(self.floor).min(len_of(&self.object))
  }
}

/// Who wrote `value`, by [`SEED_BYTE`]'s ranges.
fn writer(value: u8) -> &'static str {
  match value {
    0 => "a cut or a hole",
    SEED_BYTE => "the seed",
    value if value >= MOUNT_FIRST => "the mount",
    value if value >= SDK_FIRST => "the SDK",
    _ => "the guest",
  }
}

/// Panics naming the first bytes of `held` that differ from `expected`, both starting at offset `at`, and who wrote
/// each.
fn assert_same(held: &[u8], expected: &[u8], at: u64, what: &str, guest: &Guest) {
  if held == expected {
    return;
  }
  let wrong: Vec<String> = expected
    .iter()
    .zip(held)
    .enumerate()
    .filter(|(_, (expected, held))| expected != held)
    .take(8)
    .map(|(offset, (expected, held))| {
      format!(
        "byte {}: holds {held} ({}), expected {expected} ({})",
        at.checked_add(u64::try_from(offset).unwrap()).unwrap(),
        writer(*held),
        writer(*expected)
      )
    })
    .collect();
  panic!(
    "{what} differs from the model (lengths {} and {}):\n{}\n{}",
    held.len(),
    expected.len(),
    wrong.join("\n"),
    guest.transcript()
  );
}

/// Holds the guest's map over `[at, at + len)`, read as memory alone, to the model's.
fn map_holds(guest: &mut Guest, model: &Model, (at, len): (u64, u64), what: &str) {
  if len == 0 {
    return;
  }
  match guest.ask(&Ask::Read { at, len }, REPLY_WAIT) {
    Reply::Bytes(held) => {
      let end = at.checked_add(len).unwrap();
      assert_same(&held, &model.object[ix(at)..ix(end)], at, what, guest);
    }
    other => panic!(
      "read [{at}, {at} + {len}) → {other:?}\n{}",
      guest.transcript()
    ),
  }
}

/// Has `guest` map the file, which must be served through the DAX window, and tells the model the guest learned the
/// size.
fn map(guest: &mut Guest, model: &mut Model) {
  match guest.ask(
    &Ask::Map {
      path: FILE.to_owned(),
      pages: MAP_PAGES,
    },
    REPLY_WAIT,
  ) {
    Reply::Mapped { dax: true } => model.remap(),
    other => panic!(
      "map {FILE} → {other:?}: the map is not DAX\n{}",
      guest.transcript()
    ),
  }
}

/// The daemon's status counter `name`, summed over its shards (`slates status` prints `name: count` per shard).
fn counter(instance: &str, name: &str) -> u64 {
  let (code, out, err) = run(instance, &["status"]);
  assert_eq!(code, 0, "slates status: {err}");
  let named = format!("{name}: ");
  String::from_utf8_lossy(&out)
    .lines()
    .filter_map(|line| line.split_once(named.as_str()))
    .filter_map(|(_, count)| count.trim().parse::<u64>().ok())
    .sum()
}

/// Prints every counter of slates' own device's backed DAX after `test`, from one `slates status`, for the run's record.
fn print_device_counters(instance: &str, test: &str) {
  let (code, out, err) = run(instance, &["status"]);
  assert_eq!(code, 0, "slates status: {err}");
  let status = String::from_utf8_lossy(&out);
  let counted: Vec<String> = backed::BackedCounts::default()
    .named()
    .iter()
    .map(|(name, _)| {
      let named = format!("{name}: ");
      let sum: u64 = status
        .lines()
        .filter_map(|line| line.split_once(named.as_str()))
        .filter_map(|(_, count)| count.trim().parse::<u64>().ok())
        .sum();
      format!("{}={sum}", name.trim_start_matches("virtiofs.dax_"))
    })
    .collect();
  eprintln!("{test}: the device's counters: {}", counted.join(" "));
}

/// Waits, polling, until `done` holds, for at most [`START_WAIT`].
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
  let started = Instant::now();
  while !done() {
    assert!(
      started.elapsed() < START_WAIT,
      "{what} within {START_WAIT:?}"
    );
    pause();
  }
}

/// The value after `key: ` in a verb's text output.
fn value_of(out: &[u8], key: &str) -> String {
  let keyed = format!("{key}: ");
  let text = String::from_utf8_lossy(out);
  text
    .lines()
    .find_map(|line| line.strip_prefix(keyed.as_str()))
    .map(str::to_owned)
    .unwrap_or_else(|| panic!("no {key} in {text}"))
}

fn volume_id(hex: &str) -> VolumeId {
  VolumeId {
    bytes: u128::from_str_radix(hex, 16).unwrap().to_be_bytes(),
  }
}

/// Truncates or extends `path` to `len` through its slates mount: the other path's size change.
#[allow(clippy::disallowed_methods)] // the test's own truncate through a slates mount: RAM, not disk
fn resize_through(path: &str, len: u64) {
  std::fs::OpenOptions::new()
    .write(true)
    .open(path)
    .and_then(|file| file.set_len(len))
    .unwrap_or_else(|e| panic!("truncate {path} to {len}: {e}"));
}

/// The next fresh value from `[first, end)`, cycling.
fn next_value(counter: &mut u8, first: u8, end: u8) -> u8 {
  let span = end.saturating_sub(first);
  let value = first.saturating_add(*counter % span);
  *counter = counter.wrapping_add(1);
  value
}

/// What every test stands on: an anchor; the origin volume holding the file; the session, a clone of a snapshot of the
/// origin, mounted as the guest's other path; an SDK client with a write attachment on each; and a volume holding the
/// guest's initramfs. Fields drop in order, so the mounts and the client go before the anchor.
struct Rig {
  instance: String,
  origin: String,
  session: String,
  session_side: MountPoint,
  _boot_side: MountPoint,
  initrd: String,
  map_len: u64,
  sdk: Client,
  origin_attachment: u64,
  session_attachment: u64,
  anchor: Anchor,
}

impl Rig {
  /// Starts an anchor for an instance named after `tag`, makes the origin with the file, snapshots and clones it into
  /// the session, mounts the session, and writes the initramfs: the rig and the file's first bytes.
  fn new(gate: &Gate, tag: &str) -> (Rig, Vec<u8>) {
    let instance = format!("{tag}-{}", std::process::id());
    let anchor = start_anchor(&instance);
    let first = FIRST_PAGES
      .checked_mul(page())
      .and_then(|bytes| bytes.checked_sub(SHORT_OF_A_PAGE))
      .unwrap();
    let seed = vec![SEED_BYTE; ix(first)];
    let origin = created_volume(&instance, "origin");
    let deadlines = slates_client::Deadlines::derive(
      slates_server::daemon::LIVENESS_BUDGET_NS,
      slates_db::replay::RECOVERY_BUDGET_NS,
    )
    .get();
    let mut sdk = Client::connect(&instance, deadlines).unwrap();
    let origin_attachment = sdk
      .attach(volume_id(&origin), None, Intent::Write)
      .unwrap()
      .attachment;
    sdk
      .fs_write(
        (volume_id(&origin), origin_attachment),
        FILE,
        &seed,
        FILE_MODE,
      )
      .unwrap();
    let (code, out, err) = run(&instance, &["volume", "snapshot", &origin]);
    assert_eq!(code, 0, "volume snapshot {origin}: {err}");
    let snapshot = value_of(&out, "snapshot");
    let (code, out, err) = run(
      &instance,
      &["volume", "clone", &origin, &snapshot, "session"],
    );
    assert_eq!(code, 0, "volume clone {origin} {snapshot}: {err}");
    let session = value_of(&out, "id");
    let session_side = MountPoint::of(&instance, &session);
    let session_attachment = sdk
      .attach(volume_id(&session), None, Intent::Write)
      .unwrap()
      .attachment;
    let boot = created_volume(&instance, "boot");
    let boot_side = MountPoint::of(&instance, &boot);
    #[allow(clippy::disallowed_methods)]
    // reads the built probe binary: a test input, never written
    let probe = std::fs::read(&gate.probe).unwrap_or_else(|e| panic!("read {}: {e}", gate.probe));
    let initrd = boot_side.file("initramfs.cpio");
    write_whole(&initrd, &initramfs(&probe, &[]));
    let rig = Rig {
      instance,
      origin,
      session,
      session_side,
      _boot_side: boot_side,
      initrd,
      map_len: MAP_PAGES.checked_mul(page()).unwrap(),
      sdk,
      origin_attachment,
      session_attachment,
      anchor,
    };
    (rig, seed)
  }

  /// Boots a guest whose virtio-fs device is slates' own, through `slates vhost-user` with a DAX window, and holds that
  /// it mounted the tag with `dax=always`, which a kernel refuses on a device with no window.
  fn boot(&self, gate: &Gate) -> Guest {
    let memory = GUEST_MEMORY_MIB.to_string();
    let cpus = GUEST_CPUS.to_string();
    let mut vmm = vm::slates();
    vmm.args([
      "--instance",
      &self.instance,
      "vhost-user",
      &self.session,
      "--dax-window",
      WINDOW,
      "--",
      &gate.crosvm,
      "run",
      "-m",
      &memory,
      "--cpus",
      &cpus,
      "--vhost-user",
      "type=fs,socket={socket}",
      "--initrd",
      &self.initrd,
      "-p",
      "console=ttyS0 rdinit=/init quiet slates.dax=always",
      "--serial",
      "type=stdout,hardware=serial,num=1,console,stdin",
      &gate.kernel,
    ]);
    let (guest, first) = Guest::start(&mut vmm, "crosvm: ", ix(self.map_len));
    assert_eq!(
      first,
      Reply::Ready,
      "the guest mounts the tag of slates' own device with dax=always, which a kernel refuses without the device's \
       DAX window\n{}",
      guest.transcript()
    );
    guest
  }

  /// The session's file, through its mount.
  fn mounted_file(&self) -> String {
    self.session_side.file(FILE)
  }

  /// Replaces the session's file through the SDK.
  fn replace_session(&mut self, bytes: &[u8]) {
    let session = volume_id(&self.session);
    self
      .sdk
      .fs_write((session, self.session_attachment), FILE, bytes, FILE_MODE)
      .unwrap();
  }

  /// Replaces the origin's file through the SDK.
  fn replace_origin(&mut self, bytes: &[u8]) {
    let origin = volume_id(&self.origin);
    self
      .sdk
      .fs_write((origin, self.origin_attachment), FILE, bytes, FILE_MODE)
      .unwrap();
  }

  /// Holds `volume`'s file, read through slates itself past every mount, to `expected`; `guest` only lends its
  /// console's transcript to a failure.
  fn volume_holds(&self, volume: &str, expected: &[u8], what: &str, guest: &Guest) {
    let (code, held, err) = run(&self.instance, &["read", volume, &format!("/{FILE}")]);
    assert_eq!(code, 0, "slates read {volume} /{FILE}: {err}");
    assert_same(&held, expected, 0, what, guest);
  }

  /// Waits until slates' own device has ended and let the file's backing go, after the VM is gone.
  fn await_device_end(&self) {
    wait_for(
      "the device ended with the VM and let the file's backing go",
      || counter(&self.instance, backed::RELEASED) >= 1,
    );
  }
}

/// The attachment `slates vhost-user` announced on the console as it booted `guest`: read before the console's
/// transcript can roll it out.
fn device_attachment(guest: &Guest) -> String {
  guest
    .transcript()
    .lines()
    .find_map(|line| line.strip_prefix("attachment: "))
    .map(str::to_owned)
    .unwrap_or_else(|| panic!("no attachment announced\n{}", guest.transcript()))
}

/// Waits for the VM to end by itself, for at most [`QUIT_WAIT`]: its exit status, its VMM's last words drained into the
/// transcript.
fn vm_end(guest: &mut Guest) -> std::process::ExitStatus {
  let started = Instant::now();
  let status = loop {
    if let Some(status) = guest.vm.try_wait().unwrap() {
      break status;
    }
    assert!(
      started.elapsed() < QUIT_WAIT,
      "the VM ended within {QUIT_WAIT:?} once its device was gone\n{}",
      guest.transcript()
    );
    pause();
  };
  guest.drain(QUIT_WAIT);
  status
}

/// §4.6 (A-168, A-169) through a real guest. Do: boot crosvm with slates' own device through `slates vhost-user` and
/// map the session's file; then, reading the guest's map as memory alone right after each writer is answered, write
/// through a `slates mount` of the session across a page; replace the file through the SDK; grow the file within its
/// last page; cut it two pages short; grow it past its first last page, leaving a hole, and map it again; and replace
/// the origin's file.
///
/// Expect: the map is DAX, and holds each change of the session the moment its writer is answered, with no request of
/// the guest's: the growth past the end the guest knows; zeros over the cut, where a guest of the mount route's window
/// hangs (GAPS, "a stated limit of DAX under KVM"); the grown file and its hole once mapped again; the origin's write
/// nowhere in it (A-166). The session and the origin each read through slates as written, and the device's counter
/// shows the map it sent crosvm.
#[test]
fn a_guest_maps_a_session_file_through_slates_own_device_with_dax_and_sees_each_change_as_its_writer_is_answered()
 {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let (mut rig, seed) = Rig::new(&gate, "vhost-see");
  let mut guest = rig.boot(&gate);
  let mut model = Model::new(seed.clone(), rig.map_len);
  let page = page();
  map(&mut guest, &mut model);
  let all = |model: &Model| (0, model.touchable());
  map_holds(&mut guest, &model, all(&model), "the guest's first map");

  let across = (page / 2, page);
  let bytes = vec![MOUNT_FIRST; ix(across.1)];
  write_at(&rig.mounted_file(), across.0, &bytes);
  model.write(across.0, &bytes);
  map_holds(
    &mut guest,
    &model,
    across,
    "the map as the mount's write is answered",
  );

  let mut replaced = model.file.clone();
  replaced[ix(3 * page)..ix(4 * page)].fill(SDK_FIRST);
  rig.replace_session(&replaced);
  model.replace(&replaced);
  map_holds(
    &mut guest,
    &model,
    all(&model),
    "the map as the SDK's write is answered",
  );

  let size = model.size();
  let tail = vec![MOUNT_FIRST + 1; ix(SHORT_OF_A_PAGE / 2)];
  write_at(&rig.mounted_file(), size, &tail);
  model.write(size, &tail);
  map_holds(
    &mut guest,
    &model,
    (size, len_of(&tail)),
    "the map past the end the guest knows, as the growing write is answered",
  );

  let cut = model.size() - 2 * page;
  resize_through(&rig.mounted_file(), cut);
  model.resize(cut);
  // The guest has made no request since its map, so its size is the one it learned there: it touches the cut pages.
  let known = page_ceil(model.learned);
  map_holds(
    &mut guest,
    &model,
    (cut, known - cut),
    "the map over the cut, as the cut is answered",
  );

  let past = page_ceil(len_of(&seed)) + page;
  let grown = vec![MOUNT_FIRST + 2; ix(page)];
  write_at(&rig.mounted_file(), past, &grown);
  model.write(past, &grown);
  map(&mut guest, &mut model);
  map_holds(
    &mut guest,
    &model,
    all(&model),
    "the map of the grown file, mapped again",
  );

  let origin_bytes = vec![SDK_FIRST + 1; seed.len()];
  rig.replace_origin(&origin_bytes);
  map_holds(
    &mut guest,
    &model,
    all(&model),
    "the session's map after the origin's own write (A-166)",
  );
  rig.volume_holds(
    &rig.session,
    &model.file,
    "the session, read through slates",
    &guest,
  );
  rig.volume_holds(
    &rig.origin,
    &origin_bytes,
    "the origin, read through slates",
    &guest,
  );
  assert!(
    counter(&rig.instance, backed::MAPS_SENT) >= 1,
    "non-vacuous: the device sent crosvm a map of the file's backing"
  );
  print_device_counters(&rig.instance, "sees each change");
  let status = guest.quit();
  assert!(
    status.success(),
    "the VM powered off cleanly: {status}\n{}",
    guest.transcript()
  );
}

/// §4.6 (A-168) through a real guest: its stores. Do: map the session's file and read it; store into it through the
/// map and `msync`; store past the file's end on its last page and `msync`; grow the file through the mount past
/// those bytes, within the page; then store again and quit without syncing, so the guest's close of the file writes
/// the store back.
///
/// Expect: the first store in the session by the sync's answer; the store past the end never in it, its size
/// unchanged; after the growth, zeros where the guest stored past the old end, in the session and in the guest's own
/// map; the last store in the session once the VM is gone; the origin as it was. The device's counters show the write
/// fault's upgrade answered with no message, the syncs held for their ingests, the runs ingested, and the file's
/// backing let go when the device ended.
#[test]
fn a_guests_stores_reach_the_session_when_it_syncs_or_closes_and_never_past_the_files_end() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let (rig, seed) = Rig::new(&gate, "vhost-store");
  let mut guest = rig.boot(&gate);
  let mut model = Model::new(seed.clone(), rig.map_len);
  let page = page();
  map(&mut guest, &mut model);
  // A read first, so the guest maps the range for reading and its first store upgrades it (`fuse_upgrade_dax_mapping`).
  map_holds(
    &mut guest,
    &model,
    (0, model.touchable()),
    "the guest's first map",
  );

  guest.done(&Ask::Store {
    at: page + 7,
    len: page,
    byte: GUEST_FIRST,
  });
  model.store(page + 7, page, GUEST_FIRST);
  guest.done(&Ask::Sync);
  model.sync();
  rig.volume_holds(
    &rig.session,
    &model.file,
    "the session by the sync's answer",
    &guest,
  );

  let size = model.size();
  let (beyond, beyond_len) = (size + 10, 40);
  guest.done(&Ask::Store {
    at: beyond,
    len: beyond_len,
    byte: GUEST_FIRST + 1,
  });
  model.store(beyond, beyond_len, GUEST_FIRST + 1);
  guest.done(&Ask::Sync);
  model.sync();
  rig.volume_holds(
    &rig.session,
    &model.file,
    "the session after a sync of a store past the end, which must not reach it",
    &guest,
  );

  let grown_at = size + 60;
  let grown = vec![MOUNT_FIRST; 10];
  write_at(&rig.mounted_file(), grown_at, &grown);
  model.write(grown_at, &grown);
  rig.volume_holds(
    &rig.session,
    &model.file,
    "the session after the growth over the guest's bytes past the old end",
    &guest,
  );
  map_holds(
    &mut guest,
    &model,
    (size, grown_at + len_of(&grown) - size),
    "the guest's map after the growth: the hole reads zero where it stored past the old end",
  );

  guest.done(&Ask::Store {
    at: 0,
    len: 100,
    byte: GUEST_FIRST + 2,
  });
  model.store(0, 100, GUEST_FIRST + 2);
  let status = guest.quit();
  assert!(
    status.success(),
    "the VM powered off cleanly: {status}\n{}",
    guest.transcript()
  );
  model.sync();
  rig.await_device_end();
  rig.volume_holds(
    &rig.session,
    &model.file,
    "the session once the VM is gone: its close wrote the last store back",
    &guest,
  );
  rig.volume_holds(&rig.origin, &seed, "the origin, untouched", &guest);
  for (name, least, what) in [
    (
      backed::MAPS_KEPT,
      1,
      "the write fault's upgrade answered with no message",
    ),
    (backed::INGESTS_HELD, 2, "each sync held for its ingest"),
    (backed::INGESTED, 2, "the stores written into the session"),
  ] {
    assert!(
      counter(&rig.instance, name) >= least,
      "non-vacuous: {what} ({name} at least {least})"
    );
  }
  print_device_counters(&rig.instance, "stores");
}

/// What one generated history did.
#[derive(Default)]
struct Tally {
  stores: u32,
  stores_past_the_end: u32,
  syncs: u32,
  mount_writes: u32,
  holes: u32,
  sdk_writes: u32,
  resizes: u32,
  maps: u32,
  checks: u32,
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

/// A run within `[0, len)` from `roll`'s bits: its start and length; `None` when `len` is zero.
fn run_within(roll: u64, len: u64) -> Option<(u64, u64)> {
  if len == 0 {
    return None;
  }
  let at = (roll >> 8) % len;
  let run = ((roll >> 32) % LONGEST_RUN).saturating_add(1).min(len - at);
  Some((at, run))
}

/// One generated history over the session's file, from `seed`: the model, the generator, the writers' next values and
/// the tally.
struct History<'r> {
  rig: &'r mut Rig,
  model: Model,
  state: u64,
  guest_counter: u8,
  sdk_counter: u8,
  mount_counter: u8,
  tally: Tally,
}

impl History<'_> {
  /// The guest stores a fresh value over a span it may touch, unsynced; past the file's end too, on its last page.
  fn guest_stores(&mut self, roll: u64, guest: &mut Guest) {
    let Some((at, len)) = run_within(roll, self.model.touchable()) else {
      return;
    };
    let value = next_value(&mut self.guest_counter, GUEST_FIRST, SDK_FIRST);
    guest.done(&Ask::Store {
      at,
      len,
      byte: value,
    });
    self.model.store(at, len, value);
    self.tally.stores += 1;
    if at + len > self.model.size() {
      self.tally.stores_past_the_end += 1;
    }
  }

  /// The mount writes a fresh value over a span of the map, past the end too, opening a hole; the guest's map holds the
  /// write where it may touch it, the moment the write is answered.
  fn mount_writes(&mut self, roll: u64, guest: &mut Guest) {
    let Some((at, len)) = run_within(roll, self.rig.map_len) else {
      return;
    };
    let value = next_value(&mut self.mount_counter, MOUNT_FIRST, SEED_BYTE);
    let bytes = vec![value; ix(len)];
    if at > self.model.size() {
      self.tally.holes += 1;
    }
    write_at(&self.rig.mounted_file(), at, &bytes);
    self.model.write(at, &bytes);
    self.tally.mount_writes += 1;
    let touchable = self.model.touchable();
    let shown = (at.min(touchable), (at + len).min(touchable));
    map_holds(
      guest,
      &self.model,
      (shown.0, shown.1 - shown.0),
      "the map as the mount's write is answered",
    );
  }

  /// The SDK replaces the file with what it holds, at a new size, with a run of a fresh value; the guest's map holds
  /// it where it may touch it, the moment the write is answered.
  fn sdk_writes(&mut self, roll: u64, guest: &mut Guest) {
    let len = (roll >> 16) % (self.rig.map_len + 1);
    let mut bytes = self.model.file.clone();
    bytes.resize(ix(len), 0);
    if let Some((at, run)) = run_within(roll >> 24, len) {
      let value = next_value(&mut self.sdk_counter, SDK_FIRST, MOUNT_FIRST);
      bytes[ix(at)..ix(at + run)].fill(value);
    }
    self.rig.replace_session(&bytes);
    self.model.replace(&bytes);
    self.tally.sdk_writes += 1;
    map_holds(
      guest,
      &self.model,
      (0, self.model.touchable()),
      "the map as the SDK's write is answered",
    );
  }

  /// The mount cuts the file or extends it; the guest's map holds what the change cut or opened where it may touch it.
  fn mount_resizes(&mut self, roll: u64, guest: &mut Guest) {
    let len = (roll >> 16) % (self.rig.map_len + 1);
    let old = self.model.size();
    resize_through(&self.rig.mounted_file(), len);
    self.model.resize(len);
    self.tally.resizes += 1;
    let touchable = self.model.touchable();
    let changed = (len.min(old).min(touchable), len.max(old).min(touchable));
    map_holds(
      guest,
      &self.model,
      (changed.0, changed.1 - changed.0),
      "the map over what the size change cut or opened",
    );
  }

  /// `count` generated steps of every act.
  fn steps(&mut self, count: usize, guest: &mut Guest) {
    for _ in 0..count {
      let roll = xorshift(&mut self.state);
      match roll % 20 {
        0..=5 => self.guest_stores(roll, guest),
        6 | 7 => {
          guest.done(&Ask::Sync);
          self.model.sync();
          self.tally.syncs += 1;
        }
        8..=11 => self.mount_writes(roll, guest),
        12 | 13 => self.sdk_writes(roll, guest),
        14 => self.mount_resizes(roll, guest),
        15 | 16 => {
          map(guest, &mut self.model);
          self.tally.maps += 1;
        }
        _ => self.check(guest),
      }
    }
  }

  /// Syncs the guest, then holds the session, read through slates past every mount, and the guest's map where it may
  /// touch it, to the model.
  fn check(&mut self, guest: &mut Guest) {
    guest.done(&Ask::Sync);
    self.model.sync();
    self.rig.volume_holds(
      &self.rig.session,
      &self.model.file,
      "the session, read through slates",
      guest,
    );
    map_holds(
      guest,
      &self.model,
      (0, self.model.touchable()),
      "the guest's map",
    );
    self.tally.checks += 1;
  }
}

/// §4.6 (A-163, A-167, A-168) through a real guest over generated histories. Do: map the session's file, then over each
/// of three fixed-seed histories mix the guest's unsynced stores (past the file's end too), its syncs and new maps, the
/// mount's writes (past the end too, opening holes) and size changes, and the SDK's whole-file writes at new sizes;
/// after each other path's change read the guest's map where it may touch it, and at random points sync the guest and
/// read the session through slates and the whole map. Then quit.
///
/// Expect: at every read the map holds the model's bytes and at every check the session holds the model's file, byte
/// for byte and in length: the latest write to each byte, zeros over every hole and cut, and none of the guest's bytes
/// past the end. The session holds the model's file once the VM is gone and the origin its own first bytes. Every kind
/// of act ran, and the device's counters show its map, its upgrade and its ingests.
#[test]
fn a_guest_holds_every_change_of_a_session_file_over_generated_histories_through_slates_own_device()
{
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let (mut rig, seed) = Rig::new(&gate, "vhost-history");
  let mut guest = rig.boot(&gate);
  let mut model = Model::new(seed.clone(), rig.map_len);
  map(&mut guest, &mut model);
  let mut tallies = Vec::new();
  for (index, state) in SEEDS.into_iter().enumerate() {
    let mut history = History {
      rig: &mut rig,
      model,
      state,
      guest_counter: 0,
      sdk_counter: 0,
      mount_counter: 0,
      tally: Tally::default(),
    };
    history.steps(STEPS, &mut guest);
    history.check(&mut guest);
    let tally = history.tally;
    model = history.model;
    eprintln!(
      "history {index} (seed {state:#x}): {} stores ({} past the end), {} syncs, {} mount writes ({} holes), {} SDK \
       writes, {} size changes, {} maps, {} checks",
      tally.stores,
      tally.stores_past_the_end,
      tally.syncs,
      tally.mount_writes,
      tally.holes,
      tally.sdk_writes,
      tally.resizes,
      tally.maps,
      tally.checks
    );
    tallies.push(tally);
  }
  let status = guest.quit();
  assert!(
    status.success(),
    "the VM powered off cleanly: {status}\n{}",
    guest.transcript()
  );
  model.sync();
  rig.await_device_end();
  rig.volume_holds(
    &rig.session,
    &model.file,
    "the session once the VM is gone",
    &guest,
  );
  rig.volume_holds(&rig.origin, &seed, "the origin, untouched", &guest);
  let sum = |field: fn(&Tally) -> u32| tallies.iter().map(field).sum::<u32>();
  for (kind, ran) in [
    ("a store", sum(|t| t.stores)),
    ("a store past the end", sum(|t| t.stores_past_the_end)),
    ("a sync", sum(|t| t.syncs)),
    ("a mount write", sum(|t| t.mount_writes)),
    ("a hole", sum(|t| t.holes)),
    ("an SDK write", sum(|t| t.sdk_writes)),
    ("a size change", sum(|t| t.resizes)),
    ("a new map", sum(|t| t.maps)),
    ("a check", sum(|t| t.checks)),
  ] {
    assert!(ran > 0, "non-vacuous: the histories ran {kind}");
  }
  for name in [backed::MAPS_SENT, backed::MAPS_KEPT, backed::INGESTED] {
    assert!(
      counter(&rig.instance, name) >= 1,
      "non-vacuous: the device's {name} moved"
    );
  }
  print_device_counters(&rig.instance, "histories");
}

/// The guest stores `value` over `[at, at + len)` of its map, unsynced, and the model with it.
fn store(guest: &mut Guest, model: &mut Model, (at, len): (u64, u64), value: u8) {
  guest.done(&Ask::Store {
    at,
    len,
    byte: value,
  });
  model.store(at, len, value);
}

/// §4.6 (A-168) when the VM dies. Do: map the session's file and read it; store into it through the map and `msync`;
/// store over three more spans without syncing, and over one past the file's end on its last page; kill crosvm and its
/// device processes without warning, and at once write through the session's mount over part of an unsynced span,
/// racing the device's end. Then boot a second guest on the session and map the file.
///
/// Expect: once the device has ended, the session holds each store the dead guest made within the file, which its
/// stream's end ingested, and the mount's write over its span, the later write; nothing of the store past the end; and
/// the second guest's map holds the same. The device counts runs ingested at its end.
#[test]
fn a_vm_killed_with_unsynced_stores_leaves_them_in_the_session_through_its_devices_end() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let (rig, seed) = Rig::new(&gate, "vhost-vmkill");
  let mut guest = rig.boot(&gate);
  let mut model = Model::new(seed, rig.map_len);
  let page = page();
  map(&mut guest, &mut model);
  map_holds(
    &mut guest,
    &model,
    (0, model.touchable()),
    "the first guest's map",
  );
  store(&mut guest, &mut model, (7, page), GUEST_FIRST);
  guest.done(&Ask::Sync);
  model.sync();
  let ingested_before = counter(&rig.instance, backed::INGESTED);
  let size = model.size();
  let racing = (3 * page + 5, 2 * page);
  for (span, value) in [
    ((page + 100, 3000), GUEST_FIRST + 1),
    (racing, GUEST_FIRST + 2),
    ((size - 50, 30), GUEST_FIRST + 3),
    ((size + 20, 40), GUEST_FIRST + 4),
  ] {
    store(&mut guest, &mut model, span, value);
  }
  guest.kill();
  let over = racing.0 + page;
  let bytes = vec![MOUNT_FIRST; 500];
  write_at(&rig.mounted_file(), over, &bytes);
  model.write(over, &bytes);
  // The end's ingest: the guest's stores within the file, never past its end.
  model.sync();
  rig.await_device_end();
  rig.volume_holds(
    &rig.session,
    &model.file,
    "the session once the killed VM's device has ended",
    &guest,
  );
  assert!(
    counter(&rig.instance, backed::INGESTED) > ingested_before,
    "non-vacuous: the device's end ingested the dead guest's stores"
  );
  let mut second = rig.boot(&gate);
  let mut fresh = Model::new(model.file.clone(), rig.map_len);
  map(&mut second, &mut fresh);
  map_holds(
    &mut second,
    &fresh,
    (0, fresh.touchable()),
    "the second guest's map of what the first left",
  );
  print_device_counters(&rig.instance, "a VM killed");
  let status = second.quit();
  assert!(
    status.success(),
    "the second VM powered off cleanly: {status}\n{}",
    second.transcript()
  );
}

/// §4.6 (A-161, A-168, A-169) with the daemon killed under a guest's live map. Do: map the session's file and read it;
/// store through the map and `msync`; write through the session's mount and through the SDK; store again without
/// syncing; kill the daemon with `SIGKILL` and wait for the anchor's successor; then write through the session's mount,
/// which the successor takes over (A-61), and boot a second guest on the session through the successor.
///
/// Expect: the VM ends by itself once its device's socket closes, as crosvm takes a vhost-user back end's hangup for a
/// crashed device (`VmEventType::DeviceCrashed`, `devices/src/virtio/vhost_user_frontend/mod.rs` at 1c8d17a85); the
/// session holds every byte the guest synced and every acknowledged write, and not the store the guest never synced,
/// which died with the daemon; recovery removed the device's record, so `detach` of its attachment is refused
/// `NotFound`; and the second guest's map holds the session, the write after the takeover included.
#[test]
fn a_daemon_killed_under_a_guests_live_map_ends_its_device_and_loses_nothing_the_guest_synced() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let (mut rig, seed) = Rig::new(&gate, "vhost-dkill");
  let mut guest = rig.boot(&gate);
  let attachment = device_attachment(&guest);
  let mut model = Model::new(seed, rig.map_len);
  let page = page();
  map(&mut guest, &mut model);
  map_holds(
    &mut guest,
    &model,
    (0, model.touchable()),
    "the first guest's map",
  );
  store(&mut guest, &mut model, (7, page), GUEST_FIRST);
  guest.done(&Ask::Sync);
  model.sync();
  let mounted = vec![MOUNT_FIRST; 900];
  write_at(&rig.mounted_file(), 2 * page - 300, &mounted);
  model.write(2 * page - 300, &mounted);
  let mut replaced = model.file.clone();
  replaced[ix(4 * page)..ix(4 * page + 1000)].fill(SDK_FIRST);
  rig.replace_session(&replaced);
  model.replace(&replaced);
  rig.volume_holds(
    &rig.session,
    &model.file,
    "the session before the kill",
    &guest,
  );
  let synced = model.file.clone();
  store(&mut guest, &mut model, (3 * page, 1000), GUEST_FIRST + 1);

  let (old, new) = kill_the_daemon(&rig.anchor, &rig.instance);
  assert_ne!(old, new, "the anchor's successor is a new daemon");
  let status = vm_end(&mut guest);
  let transcript = guest.transcript();
  let crosvm_said: Vec<&str> = transcript
    .lines()
    .filter(|line| line.starts_with("crosvm: "))
    .collect();
  eprintln!(
    "daemon {old} killed under the live map, successor {new}: the VM ended by itself, {status}; crosvm's last \
     words:\n{}",
    crosvm_said.join("\n")
  );
  rig.volume_holds(
    &rig.session,
    &synced,
    "the session after the takeover: every synced byte and acknowledged write, and not the unsynced store",
    &guest,
  );
  let (code, _, err) = run(&rig.instance, &["detach", &attachment]);
  assert!(
    code == 1 && err.contains("NotFound"),
    "recovery removed the device's record: detach {attachment} → exit {code}, {err}"
  );

  let mut after = Model::new(synced, rig.map_len);
  let later = vec![MOUNT_FIRST + 1; 700];
  write_at(&rig.mounted_file(), page / 3, &later);
  after.write(page / 3, &later);
  let mut second = rig.boot(&gate);
  map(&mut second, &mut after);
  map_holds(
    &mut second,
    &after,
    (0, after.touchable()),
    "the second guest's map, through the successor",
  );
  rig.volume_holds(
    &rig.session,
    &after.file,
    "the session after the takeover's write",
    &second,
  );
  let status = second.quit();
  assert!(
    status.success(),
    "the second VM powered off cleanly: {status}\n{}",
    second.transcript()
  );
}

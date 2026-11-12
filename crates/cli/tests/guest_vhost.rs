//! A real VM over slates' own virtio-fs device, with DAX (§4.6; A-168, A-169, A-189; goal G-vhost). `slates vhost-user`
//! hands the VMM the socket end of the daemon's device: crosvm takes it as `--vhost-user type=fs,socket={socket}`, and
//! QEMU 11.1 or later as `-chardev socket,id=slates,fd={fd}` under its generic vhost-user device, whose shared-memory BAR
//! is the window. The guest, whose `/init` is `slates-guest-probe`, mounts the tag with `dax=always` and maps a file through the device's
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
//! - the session's origin untouched by any of it, and the origin's own later write never in the guest's map (A-166);
//! - a setuid-root program planted on the volume by the guest's own root running with no privilege, since the guest
//!   mounts the tag `nosuid,nodev` as A-94 says every mount of a volume is (`docs/cli.md` documents that mount).
//!
//! Gated: Linux, with the guest's kernel and the probe named in the environment as for `tests/guest_dax.rs`, and crosvm
//! (`SLATES_GUEST_CROSVM`, with KVM) or QEMU (`SLATES_GUEST_QEMU`, with KVM where the host has it and its own emulation
//! where not), or it skips loudly. Every test runs once in each VMM named. The guest's initramfs is written into a slates volume through its mount, so the test writes nothing to disk
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

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use slates_bridge_virtiofs::backed;
use slates_bridge_virtiofs::serve::TOLD_ABANDONED;
use slates_client::{Client, Intent, VolumeId};
use slates_guest_probe::protocol::{Command as Ask, Reply};
use vm::{
  Anchor, Guest, MountPoint, QUIT_WAIT, REPLY_WAIT, START_WAIT, created_volume, daemon_of,
  initramfs, kill_daemon, kill_the_daemon, pause, run, start_anchor, start_anchor_with, write_at,
  write_whole,
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

/// Format: the guest's kernel arguments after its console: the probe is `/init`, and it mounts the tag with DAX.
const GUEST_ARGUMENTS: &str = "rdinit=/init quiet slates.dax=always";
/// Format: a file system's virtio device id (virtio 1.2 §5.11), which QEMU's generic vhost-user device is told.
const VIRTIO_ID_FS: u16 = 26;
/// Shape: the queues slates' own device serves: the high-priority queue and one request queue (`docs/wip/virtiofs.md`,
/// "request queues offered").
const FS_QUEUES: u32 = 2;
/// Format: the bytes of virtio-fs's configuration space, `struct virtio_fs_config`: the 36-byte tag and the request
/// queue count (virtio 1.2 §5.11.4). QEMU's generic device forwards the guest's reads of them to the back end.
const FS_CONFIG_BYTES: u32 = 40;

/// A VMM the tests boot their guests in. Every test runs once in each VMM the environment names.
enum Vmm {
  /// crosvm (`SLATES_GUEST_CROSVM`). It takes the device's end as an inherited socket's path
  /// (`--vhost-user type=fs,socket={socket}`), and ends the VM when the device hangs up, taking that for a crashed device
  /// (`VmEventType::DeviceCrashed`, `devices/src/virtio/vhost_user_frontend/mod.rs` at 1c8d17a85).
  Crosvm(String),
  /// QEMU (`SLATES_GUEST_QEMU`), 11.1 or later. Its generic vhost-user device asks the back end for its shared-memory
  /// regions (`VHOST_USER_GET_SHMEM_CONFIG`), makes them a PCI BAR, which is the guest's DAX window, and maps each
  /// `SHMEM_MAP` into it (`hw/virtio/vhost-user-test-device-pci.c` and `hw/virtio/vhost-user.c`, both from 11.1.0;
  /// 11.0's device has no BAR). It takes the device's end by its number (`-chardev socket,id=slates,fd={fd}`), and keeps
  /// the VM running when the device hangs up.
  Qemu(String),
}

impl Vmm {
  /// Its name, in the rig's instance name and the run's record.
  fn name(&self) -> &'static str {
    match self {
      Vmm::Crosvm(_) => "crosvm",
      Vmm::Qemu(_) => "qemu",
    }
  }

  /// The tag its own lines, its standard error, carry in a transcript.
  fn tag(&self) -> &'static str {
    match self {
      Vmm::Crosvm(_) => "crosvm: ",
      Vmm::Qemu(_) => "qemu: ",
    }
  }

  /// Whether it ends the VM by itself once the device hangs up.
  fn ends_the_vm_with_its_device(&self) -> bool {
    matches!(self, Vmm::Crosvm(_))
  }

  /// Its command line for a guest of `kernel` and `initrd` over slates' own device, the guest's kernel given
  /// `arguments` after its console, with the device's end where `{socket}` or `{fd}` stands. QEMU accelerates the guest
  /// with KVM where the host has it, and otherwise emulates it.
  fn command(&self, kernel: &str, initrd: &str, arguments: &str) -> Vec<String> {
    let memory = GUEST_MEMORY_MIB.to_string();
    let cpus = GUEST_CPUS.to_string();
    let words: Vec<String> = match self {
      Vmm::Crosvm(crosvm) => vec![
        crosvm.clone(),
        "run".into(),
        "-m".into(),
        memory,
        "--cpus".into(),
        cpus,
        "--vhost-user".into(),
        "type=fs,socket={socket}".into(),
        "--initrd".into(),
        initrd.into(),
        "-p".into(),
        format!("console=ttyS0 {arguments}"),
        "--serial".into(),
        "type=stdout,hardware=serial,num=1,console,stdin".into(),
        kernel.into(),
      ],
      Vmm::Qemu(qemu) => {
        let (machine, console) = if cfg!(target_arch = "aarch64") {
          (
            "virt,memory-backend=guest-memory,gic-version=max",
            "ttyAMA0",
          )
        } else {
          ("q35,memory-backend=guest-memory", "ttyS0")
        };
        vec![
          qemu.clone(),
          "-accel".into(),
          "kvm".into(),
          "-accel".into(),
          "tcg".into(),
          "-machine".into(),
          machine.into(),
          "-cpu".into(),
          "max".into(),
          "-m".into(),
          format!("{memory}M"),
          "-smp".into(),
          cpus,
          "-object".into(),
          format!("memory-backend-memfd,id=guest-memory,size={memory}M,share=on"),
          "-chardev".into(),
          "socket,id=slates,fd={fd}".into(),
          "-device".into(),
          format!(
            "vhost-user-test-device-pci,chardev=slates,virtio-id={VIRTIO_ID_FS},num_vqs={FS_QUEUES},\
             config_size={FS_CONFIG_BYTES}"
          ),
          "-kernel".into(),
          kernel.into(),
          "-initrd".into(),
          initrd.into(),
          "-append".into(),
          format!("console={console} {arguments}"),
          "-nodefaults".into(),
          "-display".into(),
          "none".into(),
          "-serial".into(),
          "stdio".into(),
          "-no-reboot".into(),
        ]
      }
    };
    words
  }

  /// The first line of its version, for the run's record (`crosvm version`, `qemu-system-* --version`).
  fn version(&self) -> String {
    let (program, ask) = match self {
      Vmm::Crosvm(path) => (path, "version"),
      Vmm::Qemu(path) => (path, "--version"),
    };
    std::process::Command::new(program)
      .arg(ask)
      .output()
      .map(|out| {
        String::from_utf8_lossy(&out.stdout)
          .lines()
          .next()
          .unwrap_or_default()
          .to_owned()
      })
      .unwrap_or_else(|e| format!("{program} {ask}: {e}"))
  }
}

/// The paths one run needs from the environment: its VMM, the guest's kernel and the probe.
struct Gate {
  vmm: Vmm,
  kernel: String,
  probe: String,
}

impl Gate {
  /// A gate for each VMM the environment names, or none after saying loudly what is missing.
  fn each_from_env() -> Vec<Gate> {
    let named = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let vmms: Vec<Vmm> = [
      named("SLATES_GUEST_CROSVM").map(Vmm::Crosvm),
      named("SLATES_GUEST_QEMU").map(Vmm::Qemu),
    ]
    .into_iter()
    .flatten()
    .collect();
    match (named("SLATES_GUEST_KERNEL"), named("SLATES_GUEST_PROBE")) {
      (Some(kernel), Some(probe)) if !vmms.is_empty() => vmms
        .into_iter()
        .map(|vmm| {
          eprintln!(
            "guest over slates' own device in {}: {}",
            vmm.name(),
            vmm.version()
          );
          Gate {
            vmm,
            kernel: kernel.clone(),
            probe: probe.clone(),
          }
        })
        .collect(),
      _ => {
        eprintln!(
          "SKIP: the guest-over-slates'-own-device test needs SLATES_GUEST_CROSVM or SLATES_GUEST_QEMU (11.1 or \
           later), SLATES_GUEST_KERNEL and SLATES_GUEST_PROBE (tests/guest_dax.rs says how to build the probe)"
        );
        Vec::new()
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
    let instance = format!("{tag}-{}-{}", gate.vmm.name(), std::process::id());
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

  /// Boots a guest whose virtio-fs device is slates' own, through `slates vhost-user` with a DAX window, in the gate's
  /// VMM, and holds that it mounted the tag with `dax=always`, which a kernel refuses on a device with no window.
  fn boot(&self, gate: &Gate) -> Guest {
    let mut vmm = vm::slates();
    vmm.args([
      "--instance",
      &self.instance,
      "vhost-user",
      &self.session,
      "--dax-window",
      WINDOW,
      "--",
    ]);
    vmm.args(
      gate
        .vmm
        .command(&gate.kernel, &self.initrd, GUEST_ARGUMENTS),
    );
    let (guest, first) = Guest::start(&mut vmm, gate.vmm.tag(), ix(self.map_len));
    assert_eq!(
      first,
      Reply::Ready,
      "the guest in {} mounts the tag of slates' own device with dax=always, which a kernel refuses without the \
       device's DAX window\n{}",
      gate.vmm.name(),
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

/// §4.6 (A-168, A-169) through a real guest. Do: boot a VM with slates' own device through `slates vhost-user` and
/// map the session's file; then, reading the guest's map as memory alone right after each writer is answered, write
/// through a `slates mount` of the session across a page; replace the file through the SDK; grow the file within its
/// last page; cut it two pages short; grow it past its first last page, leaving a hole, and map it again; and replace
/// the origin's file.
///
/// Expect: the map is DAX, and holds each change of the session the moment its writer is answered, with no request of
/// the guest's: the growth past the end the guest knows; zeros over the cut, where a guest of the mount route's window
/// hangs (GAPS, "a stated limit of DAX under KVM"); the grown file and its hole once mapped again; the origin's write
/// nowhere in it (A-166). The session and the origin each read through slates as written, and the device's counter
/// shows the map it sent the VMM.
#[test]
fn a_guest_maps_a_session_file_through_slates_own_device_with_dax_and_sees_each_change_as_its_writer_is_answered()
 {
  for gate in Gate::each_from_env() {
    sees_each_change(&gate);
  }
}

/// [`a_guest_maps_a_session_file_through_slates_own_device_with_dax_and_sees_each_change_as_its_writer_is_answered`] in the VMM `gate` names.
fn sees_each_change(gate: &Gate) {
  let (mut rig, seed) = Rig::new(gate, "vhost-see");
  let mut guest = rig.boot(gate);
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
    "non-vacuous: the device sent the VMM a map of the file's backing"
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
  for gate in Gate::each_from_env() {
    stores_reach_the_session(&gate);
  }
}

/// [`a_guests_stores_reach_the_session_when_it_syncs_or_closes_and_never_past_the_files_end`] in the VMM `gate` names.
fn stores_reach_the_session(gate: &Gate) {
  let (rig, seed) = Rig::new(gate, "vhost-store");
  let mut guest = rig.boot(gate);
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
  for gate in Gate::each_from_env() {
    holds_every_change_over_histories(&gate);
  }
}

/// [`a_guest_holds_every_change_of_a_session_file_over_generated_histories_through_slates_own_device`] in the VMM `gate` names.
fn holds_every_change_over_histories(gate: &Gate) {
  let (mut rig, seed) = Rig::new(gate, "vhost-history");
  let mut guest = rig.boot(gate);
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
/// store over three more spans without syncing, and over one past the file's end on its last page; kill the VMM and its
/// device processes without warning, and at once write through the session's mount over part of an unsynced span,
/// racing the device's end. Then boot a second guest on the session and map the file.
///
/// Expect: once the device has ended, the session holds each store the dead guest made within the file, which its
/// stream's end ingested, and the mount's write over its span, the later write; nothing of the store past the end; and
/// the second guest's map holds the same. The device counts runs ingested at its end.
#[test]
fn a_vm_killed_with_unsynced_stores_leaves_them_in_the_session_through_its_devices_end() {
  for gate in Gate::each_from_env() {
    a_vm_killed(&gate);
  }
}

/// [`a_vm_killed_with_unsynced_stores_leaves_them_in_the_session_through_its_devices_end`] in the VMM `gate` names.
fn a_vm_killed(gate: &Gate) {
  let (rig, seed) = Rig::new(gate, "vhost-vmkill");
  let mut guest = rig.boot(gate);
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
  let mut second = rig.boot(gate);
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
/// Expect: under crosvm the VM ends by itself once its device's socket closes, as crosvm takes a vhost-user back end's
/// hangup for a crashed device (`VmEventType::DeviceCrashed`, `devices/src/virtio/vhost_user_frontend/mod.rs` at
/// 1c8d17a85); QEMU keeps the VM, and the test kills it. In either, the session holds every byte the guest synced and every acknowledged write, and not the store the guest never synced,
/// which died with the daemon; recovery removed the device's record, so `detach` of its attachment is refused
/// `NotFound`; and the second guest's map holds the session, the write after the takeover included.
#[test]
fn a_daemon_killed_under_a_guests_live_map_ends_its_device_and_loses_nothing_the_guest_synced() {
  for gate in Gate::each_from_env() {
    a_daemon_killed(&gate);
  }
}

/// [`a_daemon_killed_under_a_guests_live_map_ends_its_device_and_loses_nothing_the_guest_synced`] in the VMM `gate` names.
fn a_daemon_killed(gate: &Gate) {
  let (mut rig, seed) = Rig::new(gate, "vhost-dkill");
  let mut guest = rig.boot(gate);
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
  let ending = if gate.vmm.ends_the_vm_with_its_device() {
    format!("the VM ended by itself, {}", vm_end(&mut guest))
  } else {
    // QEMU keeps a VM whose vhost-user device hung up, so the test ends it; what the guest synced is all it asks.
    guest.kill();
    "the VMM keeps a VM whose device hung up, so the test killed it".to_owned()
  };
  let transcript = guest.transcript();
  let vmm_said: Vec<&str> = transcript
    .lines()
    .filter(|line| line.starts_with(gate.vmm.tag()))
    .collect();
  eprintln!(
    "daemon {old} killed under the live map, successor {new}: {ending}; {}'s last words:\n{}",
    gate.vmm.name(),
    vmm_said.join("\n")
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
  let mut second = rig.boot(gate);
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

// AC-3.3 for a guest writer (§4.6 "Cache posture"; A-191, A-196): a write through slates' own device is answered once
// every FUSE mount of its volume has been told of it, as an SDK or NFS write is, and the device serves on meanwhile.

/// Shape: the anchors' shard counts the told test runs on. On one shard the device, the volume and its mount share one
/// thread, so a reply sent unheld leaves before the mount's round has run at all; on two the device's verbs and the
/// mount's may arrive at a shard other than the volume's.
const TOLD_SHARDS: [u16; 2] = [1, 2];
/// Shape: the guest's writes in each told run, as many as A-191's SDK test makes (`crates/server/tests/fuse_told.rs`),
/// where without the wait about one first read in ten was stale: a pass by chance is about 0.9^200.
const TOLD_ROUNDS: u32 = 200;
/// Shape: the guests the guest-kill test boots and kills. Each is killed a step later after its write is sent, the
/// steps sweeping one write's round trip as the same guest just took it, so some kills land while the device holds the
/// reply and others before or after it.
const GUEST_KILLS: u32 = 12;
/// Shape: the daemons the daemon-kill test kills under a guest's write, each a step later in the same sweep: fewer
/// than the guests, since each one's takeover takes seconds.
const DAEMON_KILLS: u32 = 6;
/// Format: the guest's kernel arguments for the told tests: the probe is `/init`, and it mounts the tag with `dax=never`,
/// so each write it makes is a FUSE WRITE the device answers, with no window between.
const TOLD_GUEST_ARGUMENTS: &str = "rdinit=/init quiet slates.dax=never";
/// Format: the file the told tests' guest writes and the host maps, one page, at the root of the volume.
const TOLD_FILE: &str = "told.bin";
/// Format: the byte the told file starts with; [`told_fill`] never returns it.
const TOLD_SEED: u8 = b'A';
/// Format: the daemon's counter of writers whose reply waited for their volume's FUSE mounts (AC-3.3).
const TOLD_WAITS: &str = "fuse.told_waits";
/// Format: the daemon's counter of waits that reached their bound with a mount untold.
const TOLD_OVERDUE: &str = "fuse.told_overdue";

/// The fill of told write `round`: a byte no two consecutive writes share, never [`TOLD_SEED`].
fn told_fill(round: u32) -> u8 {
  b'B' + u8::try_from(round % 24).unwrap()
}

/// The guest's command writing `fill` over the told file's page.
fn patch_page(fill: u8) -> Ask {
  Ask::Patch {
    path: TOLD_FILE.into(),
    at: 0,
    bytes: vec![fill; ix(page())],
  }
}

/// Spins until `delay` has passed: a sleep's timer slack would blur the sweep's steps, which are microseconds apart.
fn spin_for(delay: Duration) {
  let started = Instant::now();
  while started.elapsed() < delay {
    std::hint::spin_loop();
  }
}

/// Step `step` of `steps` across `span`: `span × step / steps`.
fn sweep_step(span: Duration, step: u32, steps: u32) -> Duration {
  span
    .checked_mul(step)
    .and_then(|scaled| scaled.checked_div(steps))
    .unwrap()
}

/// A file mapped read-only and shared through a `slates mount` on the host, as a program that only reads a shared file
/// holds it: AC-3.3's reader.
struct HostMap {
  map: *mut u8,
  length: usize,
  _file: std::fs::File,
}

impl HostMap {
  /// Maps `length` bytes of `path` and reads them once, so the mount's kernel holds the pages.
  #[allow(clippy::disallowed_methods)] // the test's own open through a slates mount: RAM, not disk
  fn of(path: &str, length: usize) -> HostMap {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    // SAFETY: a fresh shared, read-only map of `length` bytes of a file this value holds open, at an address the kernel
    // chooses; nothing else maps it, and `Drop` unmaps it before the file closes.
    let map = unsafe {
      rustix::mm::mmap(
        std::ptr::null_mut(),
        length,
        rustix::mm::ProtFlags::READ,
        rustix::mm::MapFlags::SHARED,
        file.as_fd(),
        0,
      )
    }
    .unwrap()
    .cast::<u8>();
    let mapped = HostMap {
      map,
      length,
      _file: file,
    };
    let _ = mapped.seen();
    mapped
  }

  /// The map's bytes as they read now, a byte at a time with volatile loads: the kernel replaces the map's pages when
  /// the file changes, and a slice held over memory that changes under it is not sound.
  fn seen(&self) -> Vec<u8> {
    (0..self.length)
      .map(|offset| {
        // SAFETY: `map` is `length` readable bytes, mapped in `of` and unmapped only in `Drop`, and `offset` is below
        // `length`; a volatile load of one byte holds no reference across the kernel's page replacement.
        unsafe { std::ptr::read_volatile(self.map.add(offset)) }
      })
      .collect()
  }

  /// Whether every byte reads `fill` now.
  fn shows(&self, fill: u8) -> bool {
    self.seen().iter().all(|byte| *byte == fill)
  }
}

impl Drop for HostMap {
  fn drop(&mut self) {
    // SAFETY: unmaps exactly the map made in `of`, which nothing reads after this.
    let _ = unsafe { rustix::mm::munmap(self.map.cast(), self.length) };
  }
}

/// What the told tests stand on: an anchor of a chosen shard count; a volume holding [`TOLD_FILE`], one page of
/// [`TOLD_SEED`], written through the SDK so no host page of it is dirty; a `slates mount` of the volume on the host,
/// through which the test maps the file; and a volume holding the guest's initramfs. Fields drop in order: the map
/// before its mount, the mounts and the client before the anchor.
struct ToldRig {
  instance: String,
  volume: String,
  map: HostMap,
  _host_side: MountPoint,
  _boot_side: MountPoint,
  initrd: String,
  _sdk: Client,
  anchor: Anchor,
}

impl ToldRig {
  /// Starts an anchor of `shards` shards for an instance named after `tag`, makes the volume with the file, mounts it
  /// and maps the file on the host, and writes the initramfs.
  fn new(gate: &Gate, tag: &str, shards: u16) -> ToldRig {
    let instance = format!("{tag}{shards}-{}-{}", gate.vmm.name(), std::process::id());
    let anchor = start_anchor_with(&instance, shards);
    let volume = created_volume(&instance, "told");
    let deadlines = slates_client::Deadlines::derive(
      slates_server::daemon::LIVENESS_BUDGET_NS,
      slates_db::replay::RECOVERY_BUDGET_NS,
    )
    .get();
    let mut sdk = Client::connect(&instance, deadlines).unwrap();
    let writer = sdk
      .attach(volume_id(&volume), None, Intent::Write)
      .unwrap()
      .attachment;
    sdk
      .fs_write(
        (volume_id(&volume), writer),
        TOLD_FILE,
        &vec![TOLD_SEED; ix(page())],
        FILE_MODE,
      )
      .unwrap();
    let host_side = MountPoint::of(&instance, &volume);
    let map = HostMap::of(&host_side.file(TOLD_FILE), ix(page()));
    let boot = created_volume(&instance, "boot");
    let boot_side = MountPoint::of(&instance, &boot);
    #[allow(clippy::disallowed_methods)]
    // reads the built probe binary: a test input, never written
    let probe = std::fs::read(&gate.probe).unwrap_or_else(|e| panic!("read {}: {e}", gate.probe));
    let initrd = boot_side.file("initramfs.cpio");
    write_whole(&initrd, &initramfs(&probe, &[]));
    ToldRig {
      instance,
      volume,
      map,
      _host_side: host_side,
      _boot_side: boot_side,
      initrd,
      _sdk: sdk,
      anchor,
    }
  }

  /// Boots a guest over slates' own device through `slates vhost-user`, in the gate's VMM, mounting the tag with
  /// `dax=never` ([`TOLD_GUEST_ARGUMENTS`]).
  fn boot(&self, gate: &Gate) -> Guest {
    let mut vmm = vm::slates();
    vmm.args([
      "--instance",
      &self.instance,
      "vhost-user",
      &self.volume,
      "--dax-window",
      WINDOW,
      "--",
    ]);
    vmm.args(
      gate
        .vmm
        .command(&gate.kernel, &self.initrd, TOLD_GUEST_ARGUMENTS),
    );
    let (guest, first) = Guest::start(&mut vmm, gate.vmm.tag(), ix(page()));
    assert_eq!(
      first,
      Reply::Ready,
      "the guest in {} mounts the tag of slates' own device without DAX\n{}",
      gate.vmm.name(),
      guest.transcript()
    );
    guest
  }

  /// The guest writes `fill` over the file's page, and the test reads the host's map as memory alone the moment the
  /// guest's write returns: whether that first read was stale, and how long the write took as the test saw it.
  fn told_round(&self, guest: &mut Guest, fill: u8) -> (bool, Duration) {
    let started = Instant::now();
    guest.done(&patch_page(fill));
    let took = started.elapsed();
    (!self.map.shows(fill), took)
  }

  /// The file as the volume holds it, read through slates itself past every mount.
  fn volume_file(&self) -> Vec<u8> {
    let (code, held, err) = run(
      &self.instance,
      &["read", &self.volume, &format!("/{TOLD_FILE}")],
    );
    assert_eq!(code, 0, "slates read {} /{TOLD_FILE}: {err}", self.volume);
    held
  }

  /// The volume's attachments, as `slates status` counts them.
  fn attachments(&self) -> String {
    let (code, out, err) = run(&self.instance, &["status", &self.volume]);
    assert_eq!(code, 0, "slates status {}: {err}", self.volume);
    value_of(&out, "attachments")
  }

  /// Holds that the volume's page is whole after a write was cut off: all of `earlier`, the write before it, or all of
  /// `fill`, the write itself, and never a mix. Whether the write was applied.
  fn whole_after_cut(&self, earlier: u8, fill: u8, what: &str, guest: &Guest) -> bool {
    let held = self.volume_file();
    let page = ix(page());
    let applied = held == vec![fill; page];
    assert!(
      applied || held == vec![earlier; page],
      "{what}: the page holds all of the earlier write ({}) or all of the cut one ({}), never a mix: {}\n{}",
      writer_byte(earlier),
      writer_byte(fill),
      runs_of_page(&held),
      guest.transcript()
    );
    applied
  }

  /// Waits until the host's map shows what the volume holds, for at most [`START_WAIT`]: the mount was told of whatever
  /// the volume took. How long the map took to follow, from the moment the volume was read.
  fn map_follows_the_volume(&self, what: &str) -> Duration {
    let held = self.volume_file();
    let started = Instant::now();
    wait_for(what, || self.map.seen() == held);
    started.elapsed()
  }
}

/// A fill's name in a failure: the byte as a character.
fn writer_byte(fill: u8) -> String {
  format!("{:?}", char::from(fill))
}

/// A page's bytes as runs of one value, for a failure's report.
fn runs_of_page(bytes: &[u8]) -> String {
  let mut runs: Vec<(u8, usize)> = Vec::new();
  for &byte in bytes {
    match runs.last_mut() {
      Some((value, count)) if *value == byte => *count += 1,
      _ => runs.push((byte, 1)),
    }
  }
  runs
    .iter()
    .map(|(value, count)| format!("{}x{count}", writer_byte(*value)))
    .collect::<Vec<_>>()
    .join(",")
}

/// AC-3.3 for a guest writer (§4.6 "Cache posture"; A-191, A-196) through a real guest. Do: on an anchor of one shard
/// and then of two ([`TOLD_SHARDS`]), map [`TOLD_FILE`] read-only through a `slates mount` on the host, boot a guest over
/// slates' own device that mounts the tag without DAX, and have it write the file's page [`TOLD_ROUNDS`] times, reading
/// the host's map as memory alone the moment each write returns. Expect: every first read shows the write, since the
/// guest's reply waited until the mount was told; the daemon counts a wait for every write and none overdue.
#[test]
fn a_guests_write_through_slates_own_device_is_answered_once_every_fuse_mount_of_its_volume_was_told()
 {
  for gate in Gate::each_from_env() {
    for shards in TOLD_SHARDS {
      guest_writes_are_told(&gate, shards);
    }
  }
}

/// [`a_guests_write_through_slates_own_device_is_answered_once_every_fuse_mount_of_its_volume_was_told`] in the VMM
/// `gate` names, on an anchor of `shards` shards.
fn guest_writes_are_told(gate: &Gate, shards: u16) {
  let rig = ToldRig::new(gate, "vhost-told", shards);
  let mut guest = rig.boot(gate);
  let (waits_before, overdue_before) = (
    counter(&rig.instance, TOLD_WAITS),
    counter(&rig.instance, TOLD_OVERDUE),
  );
  let mut stale = Vec::new();
  let mut took = Vec::new();
  for round in 0..TOLD_ROUNDS {
    let (was_stale, round_took) = rig.told_round(&mut guest, told_fill(round));
    if was_stale {
      stale.push(round);
    }
    took.push(round_took);
  }
  let waited = counter(&rig.instance, TOLD_WAITS).saturating_sub(waits_before);
  let overdue = counter(&rig.instance, TOLD_OVERDUE).saturating_sub(overdue_before);
  took.sort_unstable();
  let at_share = |share: usize| took[(took.len() - 1) * share / 100];
  eprintln!(
    "AC-3.3 guest in {} ({shards} shards): {} of {TOLD_ROUNDS} first reads stale; {waited} writes waited, {overdue} \
     overdue; a write took {:?} at the median, {:?} at p99 and {:?} at most, as the test saw it (the console's carriage \
     included)",
    gate.vmm.name(),
    stale.len(),
    at_share(50),
    at_share(99),
    at_share(100)
  );
  assert!(
    stale.is_empty(),
    "the host's map read the old bytes right after the guest's write returned ({shards} shards), rounds {stale:?}\n{}",
    guest.transcript()
  );
  assert!(
    waited >= u64::from(TOLD_ROUNDS),
    "non-vacuous: each write's reply waited for the mount ({waited} waits)"
  );
  assert_eq!(overdue, 0, "no wait reached its bound");
  let status = guest.quit();
  assert!(
    status.success(),
    "the VM powered off cleanly: {status}\n{}",
    guest.transcript()
  );
}

/// AC-3.3, chaos (A-196): a guest killed while its write's reply waits. Do: [`GUEST_KILLS`] times, boot a guest, have it
/// write the page once (a told round, timed), then send another write and kill the VM a step later each time, the steps
/// sweeping the timed round. Expect: the device ends and lets its attachment go; the volume's page holds all of one
/// write or all of the other, never a mix; the host's map comes to show what the volume holds; and a reply the daemon
/// counts abandoned with its device ([`TOLD_ABANDONED`]) belongs to a write the volume holds.
#[test]
fn a_guest_killed_while_its_writes_reply_waits_leaves_the_write_whole_and_its_device_ended() {
  for gate in Gate::each_from_env() {
    guests_killed_mid_write(&gate);
  }
}

/// [`a_guest_killed_while_its_writes_reply_waits_leaves_the_write_whole_and_its_device_ended`] in the VMM `gate` names.
fn guests_killed_mid_write(gate: &Gate) {
  let rig = ToldRig::new(gate, "vhost-told-gkill", vm::SHARDS);
  let attachments = rig.attachments();
  let mut abandoned_in_all = 0u64;
  let mut applied_in_all = 0u32;
  for kill in 0..GUEST_KILLS {
    let mut guest = rig.boot(gate);
    let earlier = told_fill(kill.saturating_mul(2));
    let (was_stale, took) = rig.told_round(&mut guest, earlier);
    assert!(
      !was_stale,
      "the host's map showed the guest's write once it returned\n{}",
      guest.transcript()
    );
    let fill = told_fill(kill.saturating_mul(2).saturating_add(1));
    let delay = sweep_step(took, kill, GUEST_KILLS);
    let abandoned_before = counter(&rig.instance, TOLD_ABANDONED);
    guest.send(&patch_page(fill));
    spin_for(delay);
    guest.kill();
    wait_for(
      "the killed guest's device ended and let its attachment go",
      || rig.attachments() == attachments,
    );
    let applied = rig.whole_after_cut(earlier, fill, "the volume after the guest's kill", &guest);
    let told_after = rig.map_follows_the_volume("the host's map shows what the volume holds");
    let abandoned = counter(&rig.instance, TOLD_ABANDONED).saturating_sub(abandoned_before);
    eprintln!(
      "guest kill {kill} in {}, {delay:?} after its write was sent (a round took {took:?}): applied {applied}, \
       {abandoned} reply abandoned, the map followed in {told_after:?}",
      gate.vmm.name()
    );
    assert!(
      abandoned == 0 || applied,
      "a reply abandoned with its device belongs to a write the volume holds"
    );
    abandoned_in_all = abandoned_in_all.saturating_add(abandoned);
    applied_in_all = applied_in_all.saturating_add(u32::from(applied));
  }
  eprintln!(
    "guest kills in {}: {applied_in_all} of {GUEST_KILLS} cut writes applied; {abandoned_in_all} kills landed while the \
     write's reply waited for the mount",
    gate.vmm.name()
  );
  let mut last = rig.boot(gate);
  let (was_stale, _) = rig.told_round(&mut last, TOLD_SEED.saturating_sub(1));
  assert!(
    !was_stale,
    "a guest after the kills is still answered once the mount was told\n{}",
    last.transcript()
  );
  let status = last.quit();
  assert!(
    status.success(),
    "the last VM powered off cleanly: {status}\n{}",
    last.transcript()
  );
}

/// AC-3.3, chaos (A-196): the daemon killed under a guest's write and taken over. Do: [`DAEMON_KILLS`] times, boot a
/// guest, have it write the page once (a told round, timed), then send another write and kill the daemon a step later
/// each time, the steps sweeping the timed round; the anchor restarts it, and the host's mount is taken over. Expect:
/// the successor's page holds all of one write or all of the other, never a mix; the host's map, through the mount the
/// successor took over, comes to show what the successor holds; and the next guest's write, through the successor, is
/// told before it returns.
#[test]
fn a_daemon_killed_under_a_guests_write_is_taken_over_with_the_write_whole_and_told() {
  for gate in Gate::each_from_env() {
    daemons_killed_mid_write(&gate);
  }
}

/// [`a_daemon_killed_under_a_guests_write_is_taken_over_with_the_write_whole_and_told`] in the VMM `gate` names.
fn daemons_killed_mid_write(gate: &Gate) {
  let rig = ToldRig::new(gate, "vhost-told-dkill", vm::SHARDS);
  let mut applied_in_all = 0u32;
  for kill in 0..DAEMON_KILLS {
    let mut guest = rig.boot(gate);
    let earlier = told_fill(kill.saturating_mul(2));
    let (was_stale, took) = rig.told_round(&mut guest, earlier);
    assert!(
      !was_stale,
      "the host's map showed the guest's write once it returned, through the daemon {kill} kills in\n{}",
      guest.transcript()
    );
    let fill = told_fill(kill.saturating_mul(2).saturating_add(1));
    let delay = sweep_step(took, kill, DAEMON_KILLS);
    let old = daemon_of(&rig.anchor).expect("the anchor runs a daemon");
    guest.send(&patch_page(fill));
    spin_for(delay);
    let new = kill_daemon(&rig.anchor, &rig.instance, old);
    assert_ne!(old, new, "the anchor's successor is a new daemon");
    if gate.vmm.ends_the_vm_with_its_device() {
      let _ = vm_end(&mut guest);
    } else {
      // QEMU keeps a VM whose vhost-user device hung up, so the test ends it.
      guest.kill();
    }
    let applied = rig.whole_after_cut(earlier, fill, "the successor's volume", &guest);
    let told_after = rig.map_follows_the_volume(
      "the host's map, through the mount the successor took over, shows what the successor holds",
    );
    eprintln!(
      "daemon {old} killed in {}, {delay:?} after the guest's write was sent (a round took {took:?}), successor \
       {new}: applied {applied}, the map followed in {told_after:?}",
      gate.vmm.name()
    );
    applied_in_all = applied_in_all.saturating_add(u32::from(applied));
  }
  eprintln!(
    "daemon kills in {}: {applied_in_all} of {DAEMON_KILLS} cut writes applied",
    gate.vmm.name()
  );
  let mut last = rig.boot(gate);
  let (was_stale, _) = rig.told_round(&mut last, TOLD_SEED.saturating_sub(1));
  assert!(
    !was_stale,
    "a guest through the last successor is answered once the mount it took over was told\n{}",
    last.transcript()
  );
  let status = last.quit();
  assert!(
    status.success(),
    "the last VM powered off cleanly: {status}\n{}",
    last.transcript()
  );
}

/// Format: the uid the guest runs a planted program as (the probe's `nobody`, 65534).
const NOBODY: u32 = 65_534;

/// A-94 through a real guest. Do: boot a VM with slates' own device; have the guest's root copy the probe onto the
/// session as a setuid-root program (mode 04755, owned by uid 0, as any root writer of a volume can make one: a guest's
/// root, or an NFS client asserting uid 0, since A-94 squashes no root) and run it as `nobody`.
///
/// Expect: it runs as 65534, `nobody`, since the guest mounts the tag `nosuid,nodev` as `docs/cli.md` documents. Under
/// the mount documented before (`-o dax=always` alone) it ran as uid 0: a setuid bit on a shared volume was root for any
/// user of the guest.
#[test]
fn a_setuid_program_planted_on_the_volume_runs_without_privilege_in_a_guest() {
  for gate in Gate::each_from_env() {
    let (rig, _) = Rig::new(&gate, "nosuid");
    let mut guest = rig.boot(&gate);
    let reply = guest.ask(
      &Ask::Escalate {
        path: "suid-probe".to_owned(),
      },
      REPLY_WAIT,
    );
    assert_eq!(
      reply,
      Reply::Ran { euid: NOBODY },
      "a planted setuid-root program runs as nobody in {}\n{}",
      gate.vmm.name(),
      guest.transcript()
    );
    let status = guest.quit();
    assert!(
      status.success(),
      "the VM powered off cleanly: {status}\n{}",
      guest.transcript()
    );
  }
}

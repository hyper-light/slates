//! A real Firecracker microVM over a slates volume's disk image (§4.6, A-160). Firecracker shares no folder with its
//! guest, only block devices backed by files, so a volume reaches its guest as an image: `slates image build` renders
//! a snapshot as an ext2 file in a new volume, and Firecracker attaches that file, through a slates mount, as the
//! guest's virtio-blk disk. The guest's `/init` (`slates-guest-probe`) mounts the disk, lists every node with the
//! digest of its bytes, makes the changes the test asks for, flushes and ends the VM. `slates image merge` then makes
//! a new clone of the snapshot hold what the guest left, and the clone is read through its own mount.
//!
//! **The pmem form (A-165).** Firecracker can attach the same image file as a virtio-pmem device instead (since 1.14,
//! its `docs/pmem.md`): it maps the file shared, through the slates mount, as a range of the guest's physical memory,
//! and the guest mounts it as ext4 with DAX. A file's reads, writes and maps then reach the image file's pages with no
//! page cache of the guest's own between, so a store through the guest's map is in the host's file before the guest
//! syncs, a write through the slates mount shows in the guest's map, and a guest's flush is Firecracker's `msync` of
//! the file, which writes its pages back into the volume.
//!
//! **QEMU (A-189).** The same tests run in QEMU when the environment names it: QEMU maps the image file shared as a
//! virtio-pmem device's memory (`-object memory-backend-file,share=on` with `-device virtio-pmem-pci`), or attaches it
//! as a virtio-blk drive, and with `-no-reboot` takes the guest's restart as its clean exit. QEMU's own virtio-fs device
//! has no window and virtiofsd has no DAX, so this is QEMU's mount route, as it is Firecracker's.
//! QEMU builds virtio-pmem for x86 machines only, so on arm64 its pmem tests skip loudly and its drive test runs.
//! **Cloud Hypervisor (A-189)** likewise: its virtio-pmem device maps the image file shared (`--pmem
//! file=…,discard_writes=off`), and its guest powers off at the end, since it reboots a guest that restarts. Its own
//! virtio-fs device has no DAX ("it is not available in Cloud Hypervisor", its `docs/fs.md`).
//!
//! Gated. It needs Linux and three files named in the environment for each VMM it runs; without them it skips loudly:
//! - `SLATES_GUEST_FIRECRACKER`: a Firecracker binary, v1.17.0 or later (the release binary, its checksum verified
//!   against the release's `SHA256SUMS`). It runs with `--no-api`, so it opens no socket, and with `--enable-pci`, so
//!   the guest finds its disk on PCI.
//! - `SLATES_GUEST_VMLINUX`: an uncompressed guest kernel (ELF, as Firecracker boots on x86_64; the `Image`, as it boots
//!   on aarch64) with virtio-pci built in, and virtio-blk and ext2 (or `CONFIG_EXT4_USE_FOR_EXT2`) built in or carried
//!   as modules (below). A distribution's kernel serves: unpacked from its bzImage by the kernel tree's
//!   `scripts/extract-vmlinux` on x86_64, and Debian's arm64 `vmlinuz` is the `Image` already.
//! - `SLATES_GUEST_QEMU` and `SLATES_GUEST_KERNEL`, in place of or beside the two above: a QEMU binary and the kernel
//!   its `-kernel` boots (a distribution's `vmlinuz` serves). It runs with KVM where the host has it and with its own
//!   emulation where not.
//! - `SLATES_GUEST_CLOUD_HYPERVISOR`, with `SLATES_GUEST_VMLINUX`: a Cloud Hypervisor binary (the release's static one,
//!   its digest checked), booting the kernel by its PVH entry.
//! - `SLATES_GUEST_PROBE`: the guest's `/init`, static (`tests/guest_dax.rs` says how to build it).
//!
//! The pmem test also needs `SLATES_GUEST_MODULES`, the guest kernel's module directory (`/lib/modules/<release>` of the
//! same build as the kernel), read only. A distribution's kernel builds virtio-pmem and the nvdimm drivers behind it as
//! modules, and some build the drive and the file system as modules too (Debian's arm64 kernel builds `virtio_blk` and
//! `ext4` so). The test carries in the initramfs every module a form needs that `modules.builtin` does not list, with
//! the ones `modules.dep` says those need in turn, and the guest loads them. Without the variable the pmem test skips
//! loudly, and the drive's test carries no module, so its kernel must build them in.
//!
//! The initramfs and Firecracker's configuration are written into a volume of their own through its mount, so the
//! test writes nothing to disk but its mount points.
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod vm;

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use slates_guest_probe::protocol::{Command as Ask, EntryKind, Reply, digest};
use vm::{
  Anchor, DELIVERY_MILLIS, Guest, Module, MountPoint, QUIT_WAIT, REPLY_WAIT, created_volume,
  initramfs, kill_the_daemon, run, start_anchor, write_at, write_whole,
};

/// Shape: the image's capacity: the source's files and the guest's room to write.
const CAPACITY: &str = "32MiB";
/// Format: Firecracker's exit code for a VM stopped by an error (`FcExitCode::GenericError`, as its log names it:
/// "Firecracker exiting with error. exit_code=1").
const FIRECRACKER_ERROR_EXIT: i32 = 1;
/// Format: the guest kernel's line as it restarts the machine (`kernel/reboot.c`).
const GUEST_RESTARTS: &str = "reboot: Restarting system";
/// Format: Firecracker's line for a vCPU's KVM shutdown exit, which it leaves unhandled (v1.17.0,
/// `EmulationError::UnhandledKvmExit("Shutdown")`).
const VCPU_SHUT_DOWN: &str = "Unexpected exit reason on vcpu run: Shutdown";
/// Shape: the guest's memory, MiB, and processors.
const GUEST_MEMORY_MIB: u32 = 256;
/// See [`GUEST_MEMORY_MIB`].
const GUEST_CPUS: u32 = 1;
/// Format: a mebibyte, in bytes.
const MIB: u64 = 1 << 20;
/// Shape: the granule QEMU's memory-device room above the guest's RAM (`maxmem`) is rounded up to, MiB: a gibibyte,
/// so the room for the pmem device's memory is whole on every machine type.
const MEMORY_DEVICE_ROOM_MIB: u64 = 1 << 10;
/// Shape: the most bytes a reply's run may carry; the listing's replies carry none.
const MOST_BYTES: usize = 1 << 20;
/// Shape: the bytes the large file runs past its third chunk, so its last chunk is partial.
const LARGE_TAIL: usize = 1_234;
/// Shape: where in its second chunk the guest patches the large file, and how many bytes.
const PATCH: (usize, usize) = (100, 2_000);
/// Format: the byte the guest patches with.
const PATCH_BYTE: u8 = b'#';
/// Format: the bytes the guest writes as its new file.
const NEW_FILE: &[u8] = b"written by the guest kernel";
/// Format: the modules a pmem guest needs, beside what they need in turn: `virtio_pmem` drives the device and
/// registers its memory as an nvdimm region; `nd_pmem` makes the region's block device, with DAX, `/dev/pmem0`; `ext4`
/// mounts it. Each is carried only where `modules.builtin` does not list it.
const PMEM_MODULES: [&str; 3] = ["virtio_pmem", "nd_pmem", "ext4"];
/// Format: the modules a drive's guest needs, beside what they need in turn: `virtio_blk` drives the disk, `/dev/vda`;
/// `ext4` mounts its ext2 (`CONFIG_EXT4_USE_FOR_EXT2`, as Debian and CachyOS build it). Each is carried only where
/// `modules.builtin` does not list it.
const DRIVE_MODULES: [&str; 2] = ["virtio_blk", "ext4"];
/// Shape: how long one e2fsck run may take before the test gives up on it, as `crates/image/tests/e2fsck.rs` bounds
/// it: a check of this image takes well under a second, and e2fsck 1.47.4 can deadlock on an image it would fix.
const FSCK_WAIT_SECONDS: &str = "120";
/// Shape: how long after [`FSCK_WAIT_SECONDS`]'s `SIGTERM` e2fsck is killed outright (a deadlocked e2fsck never
/// reaches its cancel; `crates/image/tests/e2fsck.rs`).
const FSCK_KILL_AFTER_SECONDS: &str = "10";
/// Format: e2fsck's exit codes for a file system left clean: nothing wrong, or errors corrected (`e2fsck(8)`).
const FSCK_CLEAN: [i32; 2] = [0, 1];

/// A VMM the tests boot their guests in. Every test runs once in each VMM the environment names.
enum Vmm {
  /// Firecracker (`SLATES_GUEST_FIRECRACKER`, booting the uncompressed kernel `SLATES_GUEST_VMLINUX`): it attaches the
  /// image as its configuration file says, and ends the VM at the guest's restart with an error code (`reboot=k`).
  Firecracker { binary: String, vmlinux: String },
  /// QEMU (`SLATES_GUEST_QEMU`, booting `SLATES_GUEST_KERNEL`): it attaches the image as its command line says, and
  /// with `-no-reboot` ends the VM cleanly at the guest's restart.
  Qemu { binary: String, kernel: String },
  /// Cloud Hypervisor (`SLATES_GUEST_CLOUD_HYPERVISOR`, booting the uncompressed kernel `SLATES_GUEST_VMLINUX` by its
  /// PVH entry): it attaches the image as its command line says, a virtio-pmem device "bypassing the guest page cache"
  /// (its `docs/device_model.md`) or a virtio-blk disk. It reboots a guest that restarts, so its guest powers off.
  CloudHypervisor { binary: String, vmlinux: String },
}

impl Vmm {
  /// Its name, in the rig's instance name and the run's record.
  fn name(&self) -> &'static str {
    match self {
      Vmm::Firecracker { .. } => "firecracker",
      Vmm::Qemu { .. } => "qemu",
      Vmm::CloudHypervisor { .. } => "cloud-hypervisor",
    }
  }

  /// The tag its own lines, its standard error, carry in a transcript.
  fn tag(&self) -> &'static str {
    match self {
      Vmm::Firecracker { .. } => "firecracker: ",
      Vmm::Qemu { .. } => "qemu: ",
      Vmm::CloudHypervisor { .. } => "cloud-hypervisor: ",
    }
  }
}

/// The paths one run needs from the environment: its VMM, the probe, and the guest kernel's module directory.
struct Gate {
  vmm: Vmm,
  probe: String,
  /// The guest kernel's module directory, for the pmem test.
  modules: Option<String>,
}

impl Gate {
  /// A gate for each VMM the environment names, or none after saying loudly what is missing.
  fn each_from_env() -> Vec<Gate> {
    let named = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let vmms: Vec<Vmm> = [
      named("SLATES_GUEST_FIRECRACKER")
        .zip(named("SLATES_GUEST_VMLINUX"))
        .map(|(binary, vmlinux)| Vmm::Firecracker { binary, vmlinux }),
      named("SLATES_GUEST_QEMU")
        .zip(named("SLATES_GUEST_KERNEL"))
        .map(|(binary, kernel)| Vmm::Qemu { binary, kernel }),
      named("SLATES_GUEST_CLOUD_HYPERVISOR")
        .zip(named("SLATES_GUEST_VMLINUX"))
        .map(|(binary, vmlinux)| Vmm::CloudHypervisor { binary, vmlinux }),
    ]
    .into_iter()
    .flatten()
    .collect();
    match named("SLATES_GUEST_PROBE") {
      Some(probe) if !vmms.is_empty() => vmms
        .into_iter()
        .map(|vmm| Gate {
          vmm,
          probe: probe.clone(),
          modules: named("SLATES_GUEST_MODULES"),
        })
        .collect(),
      _ => {
        eprintln!(
          "SKIP: the image test needs SLATES_GUEST_FIRECRACKER and SLATES_GUEST_VMLINUX, SLATES_GUEST_QEMU and \
           SLATES_GUEST_KERNEL, or SLATES_GUEST_CLOUD_HYPERVISOR and SLATES_GUEST_VMLINUX, and SLATES_GUEST_PROBE \
           (tests/guest_image.rs says what each is)"
        );
        Vec::new()
      }
    }
  }
}

/// The source volume's tree: a small file, a pattern three chunks and more long, another small file and a hard link
/// to it, a file the guest removes, and a symbolic link.
struct Source {
  chunk: usize,
  /// The unit a merge compares and writes in: the daemon's content granule.
  page: usize,
  files: Vec<(&'static str, Vec<u8>)>,
}

impl Source {
  fn new() -> Source {
    let chunk = slates_vfs::content::chunk_bytes(rustix::param::page_size()).get();
    let large: Vec<u8> = (0..3 * chunk + LARGE_TAIL)
      .map(|at| u8::try_from(at % 251).unwrap())
      .collect();
    Source {
      chunk,
      page: slates_server::config::content_granule(rustix::param::page_size()),
      files: vec![
        ("docs/a.txt", b"the first file".to_vec()),
        ("docs/sub/b.bin", large),
        ("c.txt", b"see".to_vec()),
        ("gone.txt", b"removed by the guest".to_vec()),
      ],
    }
  }

  /// Writes the tree into the volume mounted at `mount`.
  #[allow(clippy::disallowed_methods)] // the test's own calls through the slates mount: RAM, not disk
  fn write_into(&self, mount: &MountPoint) {
    for directory in ["docs", "docs/sub"] {
      std::fs::create_dir(mount.file(directory)).unwrap();
    }
    for (path, bytes) in &self.files {
      write_whole(&mount.file(path), bytes);
    }
    std::fs::hard_link(mount.file("c.txt"), mount.file("docs/link.txt")).unwrap();
    std::os::unix::fs::symlink("docs/a.txt", mount.file("s")).unwrap();
  }

  /// The listing a guest must read of the tree, in path order.
  fn listing(&self) -> Vec<Reply> {
    let mut entries = vec![
      directory("docs"),
      directory("docs/sub"),
      link("s", b"docs/a.txt"),
    ];
    for (path, bytes) in &self.files {
      entries.push(file(path, bytes));
    }
    entries.push(file("docs/link.txt", b"see"));
    sorted(entries)
  }

  /// The large file as the guest leaves it: patched in its second chunk.
  fn patched(&self) -> Vec<u8> {
    let mut bytes = self.files[1].1.clone();
    let (at, len) = PATCH;
    bytes[self.chunk + at..self.chunk + at + len].fill(PATCH_BYTE);
    bytes
  }

  /// The listing after the patch alone.
  fn patched_listing(&self) -> Vec<Reply> {
    self
      .listing()
      .into_iter()
      .map(|entry| match &entry {
        Reply::Entry { path, .. } if path == "docs/sub/b.bin" => file(path, &self.patched()),
        _ => entry,
      })
      .collect()
  }

  /// The listing after the guest's patch and its other changes ([`the_other_changes`]).
  fn changed_listing(&self) -> Vec<Reply> {
    sorted(vec![
      directory("docs"),
      directory("docs/sub"),
      directory("made"),
      link("s", b"docs/a.txt"),
      file("docs/sub/renamed.txt", b"the first file"),
      file("docs/sub/b.bin", &self.patched()),
      file("c.txt", b"see"),
      file("docs/link.txt", b"see"),
      file("new.txt", NEW_FILE),
    ])
  }
}

/// A file's entry.
fn file(path: &str, bytes: &[u8]) -> Reply {
  Reply::Entry {
    kind: EntryKind::File,
    size: u64::try_from(bytes.len()).unwrap(),
    digest: digest(bytes),
    path: path.to_owned(),
  }
}

/// A directory's entry.
fn directory(path: &str) -> Reply {
  Reply::Entry {
    kind: EntryKind::Dir,
    size: 0,
    digest: 0,
    path: path.to_owned(),
  }
}

/// A symbolic link's entry.
fn link(path: &str, target: &[u8]) -> Reply {
  Reply::Entry {
    kind: EntryKind::Link,
    size: u64::try_from(target.len()).unwrap(),
    digest: digest(target),
    path: path.to_owned(),
  }
}

/// `entries` in path order, as a guest lists them.
fn sorted(mut entries: Vec<Reply>) -> Vec<Reply> {
  entries.sort_by_key(|entry| match entry {
    Reply::Entry { path, .. } => path.clone(),
    _ => String::new(),
  });
  entries
}

/// The guest's listing: every [`Reply::Entry`] until its [`Reply::Done`].
fn listed(guest: &mut Guest) -> Vec<Reply> {
  let mut entries = Vec::new();
  let mut reply = guest.ask(&Ask::List, REPLY_WAIT);
  loop {
    match reply {
      Reply::Done => return entries,
      entry @ Reply::Entry { .. } => entries.push(entry),
      other => panic!("list → {other:?}\n{}", guest.transcript()),
    }
    reply = guest.next_reply(REPLY_WAIT);
  }
}

/// The guest's patch of the large file inside its second chunk, written with `pwrite`.
fn guest_patches(guest: &mut Guest, source: &Source) {
  let (at, len) = PATCH;
  guest.done(&Ask::Patch {
    path: "docs/sub/b.bin".to_owned(),
    at: u64::try_from(source.chunk + at).unwrap(),
    bytes: vec![PATCH_BYTE; len],
  });
}

/// The guest's changes past its patch: a rename into another directory, a removal, a new file and a new directory, then
/// a flush. The removal comes before the new file, so the kernel may give the new file the freed inode number: under a
/// kernel it carries a generation of its own, and the merge must still take it for new.
fn the_other_changes(guest: &mut Guest) {
  guest.done(&Ask::Rename {
    from: "docs/a.txt".to_owned(),
    to: "docs/sub/renamed.txt".to_owned(),
  });
  guest.done(&Ask::Remove {
    path: "gone.txt".to_owned(),
  });
  guest.done(&Ask::Write {
    path: "new.txt".to_owned(),
    bytes: NEW_FILE.to_vec(),
  });
  guest.done(&Ask::Mkdir {
    path: "made".to_owned(),
  });
  guest.done(&Ask::Flush);
}

/// How the VMM presents the image to its guest.
enum Disk {
  /// A writable virtio-blk drive, its flushes written through to the file (`cache_type: Writeback`, so a guest's sync
  /// reaches the volume), mounted as ext2 (A-160) once the guest has loaded `modules`, in their order.
  Drive { modules: Vec<Module> },
  /// A writable virtio-pmem device, which Firecracker maps shared (its flushes an `msync`), mounted as ext4 with DAX
  /// once the guest has loaded `modules`, in their order (A-165).
  Pmem { modules: Vec<Module> },
}

impl Disk {
  /// The devices' sections of Firecracker's configuration, over `image`. The file must hold `drives` even when it names
  /// none (v1.17.0 refuses one without: "missing field `drives`").
  fn device(&self, image: &str) -> String {
    match self {
      Disk::Drive { .. } => format!(
        r#""drives": [
    {{
      "drive_id": "image",
      "path_on_host": "{image}",
      "is_root_device": false,
      "is_read_only": false,
      "cache_type": "Writeback"
    }}
  ]"#
      ),
      Disk::Pmem { .. } => format!(
        r#""drives": [],
  "pmem": [
    {{
      "id": "image",
      "path_on_host": "{image}",
      "root_device": false,
      "read_only": false
    }}
  ]"#
      ),
    }
  }

  /// QEMU's arguments for the image at `image`, `len` bytes: a drive QEMU writes through its own cache, its flushes
  /// reaching the file; or the file mapped shared as a virtio-pmem device's memory, its flushes an `msync`.
  fn qemu_device(&self, image: &str, len: u64) -> Vec<String> {
    match self {
      Disk::Drive { .. } => vec![
        "-drive".to_owned(),
        format!("file={image},if=none,id=image,format=raw,cache=writeback"),
        "-device".to_owned(),
        "virtio-blk-pci,drive=image".to_owned(),
      ],
      Disk::Pmem { .. } => vec![
        "-object".to_owned(),
        format!("memory-backend-file,id=image,mem-path={image},size={len},share=on"),
        "-device".to_owned(),
        "virtio-pmem-pci,memdev=image,id=pmem".to_owned(),
      ],
    }
  }

  /// Cloud Hypervisor's arguments for the image at `image`: a disk it writes through its own cache, its flushes reaching
  /// the file; or the file mapped shared as a virtio-pmem device's memory, every write kept (`discard_writes=off`).
  fn cloud_hypervisor_device(&self, image: &str) -> Vec<String> {
    match self {
      Disk::Drive { .. } => vec!["--disk".to_owned(), format!("path={image},image_type=raw")],
      Disk::Pmem { .. } => vec![
        "--pmem".to_owned(),
        format!("file={image},discard_writes=off"),
      ],
    }
  }

  /// What the guest's command line tells its probe to load and mount.
  fn guest_options(&self) -> String {
    match self {
      Disk::Drive { modules } if modules.is_empty() => "slates.block=/dev/vda".to_owned(),
      Disk::Drive { modules } => format!("slates.block=/dev/vda slates.modules={}", names(modules)),
      Disk::Pmem { modules } => format!(
        "slates.block=/dev/pmem0 slates.fs=ext4 slates.dax=always slates.modules={}",
        names(modules)
      ),
    }
  }

  /// The modules the guest's initramfs carries.
  fn modules(&self) -> &[Module] {
    match self {
      Disk::Drive { modules } | Disk::Pmem { modules } => modules,
    }
  }
}

/// The probe's list of `modules`, by file name, in their order.
fn names(modules: &[Module]) -> String {
  let names: Vec<&str> = modules.iter().map(|module| module.name.as_str()).collect();
  names.join(",")
}

/// Firecracker's configuration: the kernel, the initramfs, the image as `disk` presents it, and a small machine. The
/// guest ends the VM by rebooting, which Firecracker takes as its exit (`reboot=k`).
fn firecracker_config(vmlinux: &str, initrd: &str, image: &str, disk: &Disk) -> String {
  format!(
    r#"{{
  "boot-source": {{
    "kernel_image_path": "{kernel}",
    "initrd_path": "{initrd}",
    "boot_args": "console=ttyS0 reboot=k panic=1 rdinit=/init quiet {options} slates.end=reboot"
  }},
  {device},
  "machine-config": {{
    "vcpu_count": {GUEST_CPUS},
    "mem_size_mib": {GUEST_MEMORY_MIB}
  }}
}}
"#,
    kernel = vmlinux,
    options = disk.guest_options(),
    device = disk.device(image),
  )
}

/// QEMU's command line: the kernel, the initramfs, the image as `disk` presents it (`len` bytes), and a small machine
/// with room above its RAM for the pmem device's memory. The guest's restart ends QEMU cleanly (`-no-reboot`).
fn qemu_arguments(
  kernel: &str,
  initrd: &str,
  (image, len): (&str, u64),
  disk: &Disk,
) -> Vec<String> {
  let (machine, console) = if cfg!(target_arch = "aarch64") {
    ("virt,gic-version=max", "ttyAMA0")
  } else {
    ("q35", "ttyS0")
  };
  let room = u64::from(GUEST_MEMORY_MIB)
    .saturating_add(len.div_ceil(MIB))
    .next_multiple_of(MEMORY_DEVICE_ROOM_MIB);
  let mut arguments = vec![
    "-accel".to_owned(),
    "kvm".to_owned(),
    "-accel".to_owned(),
    "tcg".to_owned(),
    "-machine".to_owned(),
    machine.to_owned(),
    "-cpu".to_owned(),
    "max".to_owned(),
    "-m".to_owned(),
    format!("{GUEST_MEMORY_MIB}M,slots=1,maxmem={room}M"),
    "-smp".to_owned(),
    GUEST_CPUS.to_string(),
    "-kernel".to_owned(),
    kernel.to_owned(),
    "-initrd".to_owned(),
    initrd.to_owned(),
    "-append".to_owned(),
    format!(
      "console={console} reboot=k panic=1 rdinit=/init quiet {} slates.end=reboot",
      disk.guest_options()
    ),
    "-nodefaults".to_owned(),
    "-display".to_owned(),
    "none".to_owned(),
    "-serial".to_owned(),
    "stdio".to_owned(),
    "-no-reboot".to_owned(),
  ];
  arguments.extend(disk.qemu_device(image, len));
  arguments
}

/// Cloud Hypervisor's command line: the kernel by its PVH entry, the initramfs, the image as `disk` presents it, a small
/// machine, and the serial console on its standard streams. Its guest ends the VM by powering off, since Cloud
/// Hypervisor reboots a guest that restarts.
fn cloud_hypervisor_arguments(
  vmlinux: &str,
  initrd: &str,
  image: &str,
  disk: &Disk,
) -> Vec<String> {
  let console = if cfg!(target_arch = "aarch64") {
    "ttyAMA0"
  } else {
    "ttyS0"
  };
  let mut arguments = vec![
    "--kernel".to_owned(),
    vmlinux.to_owned(),
    "--initramfs".to_owned(),
    initrd.to_owned(),
    "--cmdline".to_owned(),
    format!(
      "console={console} panic=1 rdinit=/init quiet {}",
      disk.guest_options()
    ),
    "--cpus".to_owned(),
    format!("boot={GUEST_CPUS}"),
    "--memory".to_owned(),
    format!("size={GUEST_MEMORY_MIB}M"),
    "--serial".to_owned(),
    "tty".to_owned(),
    "--console".to_owned(),
    "off".to_owned(),
  ];
  arguments.extend(disk.cloud_hypervisor_device(image));
  arguments
}

/// A module's name from its file's path in a module directory: the file name up to its first dot, dashes read as
/// underscores, as the kernel names modules (`encrypted-keys.ko.zst` is `encrypted_keys`).
fn module_name(path: &str) -> String {
  let file = path.rsplit('/').next().unwrap_or(path);
  file.split('.').next().unwrap_or(file).replace('-', "_")
}

/// What a module needs before it loads, by path: `hard`, the modules its `modules.dep` line names, and `soft`, the
/// modules its `modules.softdep` line names `pre:` (resolved through `modules.alias`), which `modprobe` loads first and
/// the guest, having no `modprobe`, must be given (Debian's ext4 and jbd2 load `crypto-crc32c` so, without which ext4
/// refuses to mount: "Cannot load crc32c driver", measured on arm64, 2026-10-27).
struct Needs {
  hard: BTreeMap<String, Vec<String>>,
  soft: BTreeMap<String, Vec<String>>,
}

/// Places the module at `path` after every module it needs, soft ones first, depth first. `entered` catches a cycle
/// through hard needs, which depmod never writes; a soft need already entered is left where it is.
fn place(path: &str, needs: &Needs, entered: &mut BTreeSet<String>, placed: &mut Vec<String>) {
  if placed.iter().any(|done| done == path) {
    return;
  }
  assert!(
    entered.insert(path.to_owned()),
    "modules.dep has a cycle through {path}"
  );
  for soft in needs.soft.get(path).into_iter().flatten() {
    if !entered.contains(soft) {
      place(soft, needs, entered, placed);
    }
  }
  for needed in needs
    .hard
    .get(path)
    .unwrap_or_else(|| panic!("modules.dep has no line for {path}"))
  {
    place(needed, needs, entered, placed);
  }
  placed.push(path.to_owned());
}

/// The modules `wanted` needs from the guest kernel's module directory `directory`, each after every module it needs, as
/// the directory's `modules.dep` names them. A module its `modules.builtin` lists is in the kernel already and is not
/// carried. Read only.
#[allow(clippy::disallowed_methods)] // reads the module directory the environment names
fn modules_needed(directory: &str, wanted: &[&str]) -> Vec<Module> {
  let read = |name: &str| {
    std::fs::read_to_string(format!("{directory}/{name}"))
      .unwrap_or_else(|e| panic!("read {directory}/{name}: {e}"))
  };
  let builtin: BTreeSet<String> = read("modules.builtin").lines().map(module_name).collect();
  let mut hard = BTreeMap::new();
  let mut path_of = BTreeMap::new();
  for line in read("modules.dep").lines() {
    let Some((path, needed)) = line.split_once(':') else {
      continue;
    };
    path_of.insert(module_name(path), path.to_owned());
    hard.insert(
      path.to_owned(),
      needed
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>(),
    );
  }
  // `alias NAME MODULE`: the first module an alias names, which is the one `modprobe` tries first.
  let mut module_of_alias = BTreeMap::new();
  for line in read("modules.alias").lines() {
    if let Some(["alias", alias, module]) = line.split_whitespace().collect::<Vec<_>>().get(..3) {
      module_of_alias
        .entry((*alias).to_owned())
        .or_insert_with(|| (*module).replace('-', "_"));
    }
  }
  // `softdep MODULE pre: A B post: C`: A and B, each a module name or an alias, as a carried module's path; a name
  // the kernel builds in, or that names nothing here, needs nothing carried.
  let mut soft = BTreeMap::new();
  for line in read("modules.softdep").lines() {
    let mut words = line.split_whitespace();
    let (Some("softdep"), Some(module)) = (words.next(), words.next()) else {
      continue;
    };
    let Some(path) = path_of.get(&module.replace('-', "_")) else {
      continue;
    };
    let pre: Vec<String> = words
      .skip_while(|word| *word != "pre:")
      .skip(1)
      .take_while(|word| !word.ends_with(':'))
      .filter_map(|name| {
        // A module's name reads dashes as underscores; an alias is matched as written (`crypto-crc32c`).
        let as_module = name.replace('-', "_");
        let module = if path_of.contains_key(&as_module) {
          Some(as_module)
        } else {
          module_of_alias.get(name).cloned()
        }?;
        (!builtin.contains(&module)).then(|| path_of.get(&module).cloned())?
      })
      .collect();
    soft.insert(path.clone(), pre);
  }
  let needs = Needs { hard, soft };
  let mut entered = BTreeSet::new();
  let mut placed = Vec::new();
  for name in wanted {
    if builtin.contains(*name) {
      continue;
    }
    let path = path_of.get(*name).unwrap_or_else(|| {
      panic!("{name} is in neither modules.dep nor modules.builtin of {directory}")
    });
    place(path, &needs, &mut entered, &mut placed);
  }
  let modules: Vec<Module> = placed
    .iter()
    .map(|path| Module {
      name: path.rsplit('/').next().unwrap_or(path).to_owned(),
      bytes: std::fs::read(format!("{directory}/{path}"))
        .unwrap_or_else(|e| panic!("read {directory}/{path}: {e}")),
    })
    .collect();
  let names: BTreeSet<&str> = modules.iter().map(|module| module.name.as_str()).collect();
  assert_eq!(
    names.len(),
    modules.len(),
    "two modules share a file name: {placed:?}"
  );
  modules
}

/// Quits the guest and holds the VM's end to the guest's own restart. The guest ends the VM by restarting
/// (`reboot=k`): the kernel pulses the keyboard controller's reset, which Firecracker takes as the VM's end, ten times
/// over about a millisecond, then falls through to the BIOS restart and a triple fault (arch/x86/kernel/reboot.c,
/// `native_machine_emergency_restart`). Firecracker stops the vCPU from its main thread; when that thread is later than
/// the millisecond, the vCPU meets the fallback first, a KVM shutdown exit Firecracker leaves unhandled and exits 1 for.
/// Measured on the box at load 46-53 (2026-10-24): 1 run of 18. Either way the guest ended the VM by its own restart,
/// after its flush; the merge that follows holds the bytes to it. QEMU (`-no-reboot`) exits 0 at the restart, and a
/// Cloud Hypervisor guest powers off instead, which ends it with 0.
fn the_guest_ends_the_vm_by_its_restart(guest: &mut Guest) {
  let status = guest.quit();
  if !status.success() {
    guest.drain(QUIT_WAIT);
  }
  let restarted = status.success()
    || (status.code() == Some(FIRECRACKER_ERROR_EXIT)
      && guest.said(GUEST_RESTARTS)
      && guest.said(VCPU_SHUT_DOWN));
  assert!(
    restarted,
    "the VMM exited {status}, not by the guest's restart\n{}",
    guest.transcript()
  );
  eprintln!("the VMM ended by the guest's restart: {status}");
}

/// Every node of the volume mounted at `mount`, as a guest would list it. A directory the test's user may not read is
/// listed but not walked: e2fsck makes `lost+found` root's, mode 0700, and the merge's counts say what it holds.
#[allow(clippy::disallowed_methods)] // reads through the slates mount: RAM, not disk
fn volume_listing(mount: &MountPoint) -> Vec<Reply> {
  let mut entries = Vec::new();
  let mut pending = vec![String::new()];
  while let Some(parent) = pending.pop() {
    for entry in std::fs::read_dir(mount.file(&parent)).unwrap() {
      let entry = entry.unwrap();
      let name = entry.file_name().to_string_lossy().into_owned();
      let path = if parent.is_empty() {
        name
      } else {
        format!("{parent}/{name}")
      };
      let kind = std::fs::symlink_metadata(entry.path()).unwrap().file_type();
      if kind.is_dir() {
        match std::fs::read_dir(entry.path()) {
          Ok(_) => pending.push(path.clone()),
          Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
          Err(e) => panic!("list {path}: {e}"),
        }
        entries.push(directory(&path));
      } else if kind.is_symlink() {
        let target = std::fs::read_link(entry.path()).unwrap();
        entries.push(link(&path, target.as_os_str().as_encoded_bytes()));
      } else {
        entries.push(file(&path, &std::fs::read(entry.path()).unwrap()));
      }
    }
  }
  sorted(entries)
}

/// A count `slates image merge` printed.
fn count(out: &str, key: &str) -> u64 {
  out
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no {key} in {out}"))
    .parse()
    .unwrap()
}

/// A line `slates` printed: the value after `key: `.
fn printed(out: &[u8], key: &str) -> String {
  String::from_utf8_lossy(out)
    .lines()
    .find_map(|line| line.strip_prefix(&format!("{key}: ")))
    .unwrap_or_else(|| panic!("no {key} in {}", String::from_utf8_lossy(out)))
    .to_owned()
}

/// Where the first run of at least `len` bytes of [`PATCH_BYTE`] in `bytes` begins.
fn run_of(bytes: &[u8], len: usize) -> Option<usize> {
  let mut run = 0usize;
  for (offset, byte) in bytes.iter().enumerate() {
    run = if *byte == PATCH_BYTE { run + 1 } else { 0 };
    if run == len {
      return Some(offset + 1 - len);
    }
  }
  None
}

/// The longest run of [`PATCH_BYTE`] in `bytes`: where it begins and its length, for a failure's report.
fn longest_run(bytes: &[u8]) -> (usize, usize) {
  let (mut best, mut run) = ((0usize, 0usize), 0usize);
  for (offset, byte) in bytes.iter().enumerate() {
    run = if *byte == PATCH_BYTE { run + 1 } else { 0 };
    if run > best.1 {
      best = (offset + 1 - run, run);
    }
  }
  best
}

/// The daemon's `fuse.*` counters that are not zero, one per line, as `slates status` prints them: the write-backs it
/// kept back or refused, the takeover rounds it sent.
fn fuse_counters(rig: &Rig) -> String {
  let (code, out, err) = run(&rig.instance, &["status"]);
  assert_eq!(code, 0, "slates status: {err}");
  String::from_utf8_lossy(&out)
    .lines()
    .filter(|line| line.contains("fuse.") && !line.trim_end().ends_with(": 0"))
    .collect::<Vec<_>>()
    .join("\n")
}

/// Whether `tool` is on the path.
fn present(tool: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {tool}")])
    .output()
    .is_ok_and(|out| out.status.success())
}

/// `e2fsck -fy` over the image file through its slates mount, bounded by coreutils' `timeout`: it checks the whole file
/// system and repairs what it finds, which marks one no guest unmounted clean again. It must leave the file system
/// clean. The repairs it made: each line of its report that answers a question yes.
fn repaired(rig: &Rig) -> Vec<String> {
  let image = rig.image_file();
  let out = Command::new("timeout")
    .args([
      "--kill-after",
      FSCK_KILL_AFTER_SECONDS,
      FSCK_WAIT_SECONDS,
      "e2fsck",
      "-fy",
      &image,
    ])
    .output()
    .unwrap();
  assert!(
    out
      .status
      .code()
      .is_some_and(|code| FSCK_CLEAN.contains(&code)),
    "e2fsck -fy {image} exit {:?}\n{}{}",
    out.status.code(),
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  let report = String::from_utf8_lossy(&out.stdout).into_owned();
  eprintln!("e2fsck -fy, exit {:?}:\n{report}", out.status.code());
  report
    .lines()
    .filter(|line| line.trim_end().ends_with("? yes"))
    .map(str::to_owned)
    .collect()
}

/// The daemon, the source volume and the image built from its snapshot, and a volume for the VM's boot files, each
/// mounted until the test ends. Fields drop in order, so the mounts go before the daemon.
struct Rig {
  instance: String,
  source: Source,
  image: String,
  image_mount: MountPoint,
  boot_mount: MountPoint,
  _source_mount: MountPoint,
  anchor: Anchor,
}

impl Rig {
  /// Starts a daemon for `instance`, writes the source tree into a volume, snapshots it and builds the image.
  fn new(instance: &str) -> Rig {
    let anchor = start_anchor(instance);
    let source = Source::new();
    let volume = created_volume(instance, "source");
    let source_mount = MountPoint::of(instance, &volume);
    source.write_into(&source_mount);
    let (code, out, err) = run(instance, &["volume", "snapshot", &volume]);
    assert_eq!(code, 0, "volume snapshot: {err}");
    let snapshot = printed(&out, "snapshot");
    let (code, out, err) = run(
      instance,
      &[
        "image",
        "build",
        &volume,
        &snapshot,
        "image",
        "--capacity",
        CAPACITY,
      ],
    );
    assert_eq!(code, 0, "image build: {err}");
    let image = printed(&out, "id");
    let image_mount = MountPoint::of(instance, &image);
    let boot = created_volume(instance, "boot");
    let boot_mount = MountPoint::of(instance, &boot);
    Rig {
      instance: instance.to_owned(),
      source,
      image,
      image_mount,
      boot_mount,
      _source_mount: source_mount,
      anchor,
    }
  }

  /// The image file, through its mount.
  fn image_file(&self) -> String {
    self.image_mount.file("image.ext2")
  }

  /// The image file's bytes, read through its mount.
  #[allow(clippy::disallowed_methods)] // reads through the slates mount: RAM, not disk
  fn image_bytes(&self) -> Vec<u8> {
    std::fs::read(self.image_file()).unwrap()
  }

  /// The image file's bytes as slates itself holds them, read past the kernel and its cache (`slates read`).
  fn held_by_slates(&self) -> Vec<u8> {
    let (code, held, err) = run(&self.instance, &["read", &self.image, "/image.ext2"]);
    assert_eq!(code, 0, "slates read {} /image.ext2: {err}", self.image);
    held
  }

  /// Boots the gate's VMM from an initramfs held in the boot volume (Firecracker from a configuration held there too),
  /// the image presented as `disk` says, and waits until the guest is ready.
  fn boot(&self, gate: &Gate, disk: &Disk) -> Guest {
    #[allow(clippy::disallowed_methods)] // reads the probe binary the environment names
    let probe = std::fs::read(&gate.probe).unwrap();
    let initrd = self.boot_mount.file("initramfs.cpio");
    write_whole(&initrd, &initramfs(&probe, disk.modules()));
    let image = self.image_file();
    let booting = std::time::Instant::now();
    let guest = match &gate.vmm {
      Vmm::Firecracker { binary, vmlinux } => {
        let config = self.boot_mount.file("vm.json");
        write_whole(
          &config,
          firecracker_config(vmlinux, &initrd, &image, disk).as_bytes(),
        );
        Guest::spawn(
          Command::new(binary).args(["--no-api", "--enable-pci", "--config-file", &config]),
          gate.vmm.tag(),
          MOST_BYTES,
        )
      }
      Vmm::Qemu { binary, kernel } => {
        #[allow(clippy::disallowed_methods)]
        // the image file's length, through the slates mount: RAM, not disk
        let len = std::fs::metadata(&image).unwrap().len();
        Guest::spawn(
          Command::new(binary).args(qemu_arguments(kernel, &initrd, (&image, len), disk)),
          gate.vmm.tag(),
          MOST_BYTES,
        )
      }
      Vmm::CloudHypervisor { binary, vmlinux } => Guest::spawn_on_terminal(
        Command::new(binary).args(cloud_hypervisor_arguments(vmlinux, &initrd, &image, disk)),
        gate.vmm.tag(),
        MOST_BYTES,
      ),
    };
    eprintln!(
      "{} booted the guest to its probe's ready in {} ms",
      gate.vmm.name(),
      booting.elapsed().as_millis()
    );
    guest
  }

  /// `slates image merge`s the image the guest left into a clone named `name`, and holds the merge to `counts` (the
  /// nodes it made, moved and removed) and `bytes_written`, and the clone to `changed`.
  fn merged_holds(
    &self,
    name: &str,
    changed: &[Reply],
    counts: (u64, u64, u64),
    bytes_written: usize,
  ) {
    let (code, out, err) = run(&self.instance, &["image", "merge", &self.image, name]);
    assert_eq!(code, 0, "image merge: {err}");
    let text = String::from_utf8_lossy(&out).into_owned();
    eprintln!("slates image merge:\n{text}");
    assert_eq!(
      (
        count(&text, "created"),
        count(&text, "moved"),
        count(&text, "removed")
      ),
      counts,
      "{text}"
    );
    assert_eq!(
      count(&text, "bytes_written"),
      u64::try_from(bytes_written).unwrap(),
      "{text}"
    );
    let merged_mount = MountPoint::of(&self.instance, &printed(&out, "id"));
    assert_eq!(
      volume_listing(&merged_mount),
      changed,
      "the clone holds what the guest left"
    );
  }

  /// The merge's expectation for the guest's whole set of changes: two nodes made, one moved and one removed, and the
  /// new file's bytes and the one page the patch lies in written.
  fn merged_holds_every_change(&self) {
    self.merged_holds(
      "merged",
      &self.source.changed_listing(),
      (2, 1, 1),
      self.source.page + NEW_FILE.len(),
    );
  }

  /// `slates image merge` into a clone named `name` is refused: the image's file system was not cleanly unmounted.
  fn merge_is_refused_unclean(&self, name: &str, when: &str) {
    let (code, out, err) = run(&self.instance, &["image", "merge", &self.image, name]);
    assert!(
      code != 0 && err.contains("NotClean"),
      "{when}, the merge refuses the image NotClean: exit {code}\n{}{err}",
      String::from_utf8_lossy(&out)
    );
  }
}

/// A-160 by use: a real VM (Firecracker v1.17.0, QEMU) reads and writes a slates volume through its disk image. Do: write
/// a tree into a volume, snapshot it and `slates image build` it; boot the VM from an initramfs held in a slates
/// volume, its disk the image file read through a slates mount; have the guest list every node, patch a file inside its
/// second chunk, rename one into another directory, remove one, write a new file and make a directory, list again, flush
/// and end the VM; `slates image merge` the image and read the clone through its mount. Expect the guest to read every
/// node as the volume holds it (kind, size and digest), and its changes in its second listing; the VM to end
/// cleanly; the merge to make two nodes, move one and remove one, and to write the new file's bytes and the one page
/// the patch lies in; and the clone to hold exactly what the guest left.
#[test]
fn a_guest_reads_and_writes_a_volume_through_its_image() {
  for gate in Gate::each_from_env() {
    reads_and_writes_through_the_image(&gate);
  }
}

/// [`a_guest_reads_and_writes_a_volume_through_its_image`] in the VMM `gate` names.
fn reads_and_writes_through_the_image(gate: &Gate) {
  let rig = Rig::new(&format!(
    "guest-image-{}-{}",
    gate.vmm.name(),
    std::process::id()
  ));
  let mut guest = rig.boot(gate, &drive_disk(gate));
  assert_eq!(
    listed(&mut guest),
    rig.source.listing(),
    "the guest reads every node as the volume holds it\n{}",
    guest.transcript()
  );
  guest_patches(&mut guest, &rig.source);
  the_other_changes(&mut guest);
  let changed = rig.source.changed_listing();
  assert_eq!(
    listed(&mut guest),
    changed,
    "the guest reads its own changes\n{}",
    guest.transcript()
  );
  the_guest_ends_the_vm_by_its_restart(&mut guest);
  rig.merged_holds_every_change();
}

/// The drive, with the modules its guest needs from the module directory the environment names, or none when it names
/// none (its kernel builds them in).
fn drive_disk(gate: &Gate) -> Disk {
  Disk::Drive {
    modules: gate
      .modules
      .as_deref()
      .map(|directory| modules_needed(directory, &DRIVE_MODULES))
      .unwrap_or_default(),
  }
}

/// The pmem disk, with the modules its guest needs from the module directory the environment names; `None`, after
/// saying so loudly, when it names none.
fn pmem_disk(gate: &Gate) -> Option<Disk> {
  if matches!(gate.vmm, Vmm::Qemu { .. }) && cfg!(target_arch = "aarch64") {
    eprintln!(
      "SKIP: QEMU builds virtio-pmem for x86 machines only (11.1: hw/i386/Kconfig selects VIRTIO_PMEM_SUPPORTED, \
       hw/arm/Kconfig does not), so an arm64 QEMU guest has no pmem device; measured in diagnostics run 40883925064"
    );
    return None;
  }
  let Some(directory) = gate.modules.as_deref() else {
    eprintln!(
      "SKIP: the pmem tests need SLATES_GUEST_MODULES, the guest kernel's module directory \
       (tests/guest_image.rs says why)"
    );
    return None;
  };
  Some(Disk::Pmem {
    modules: modules_needed(directory, &PMEM_MODULES),
  })
}

/// The guest lists every node, which must be as the volume holds it, and maps the large file through the patch, which
/// must be DAX: the patch's offset in the file.
fn lists_and_maps(rig: &Rig, guest: &mut Guest) -> usize {
  assert_eq!(
    listed(guest),
    rig.source.listing(),
    "the guest reads every node as the volume holds it\n{}",
    guest.transcript()
  );
  let (at, len) = PATCH;
  let patch_at = rig.source.chunk + at;
  let pages = (patch_at + len).div_ceil(rustix::param::page_size());
  match guest.ask(
    &Ask::Map {
      path: "docs/sub/b.bin".to_owned(),
      pages: u64::try_from(pages).unwrap(),
    },
    REPLY_WAIT,
  ) {
    Reply::Mapped { dax: true } => {}
    other => panic!(
      "map docs/sub/b.bin → {other:?}: DAX is what this test is for\n{}",
      guest.transcript()
    ),
  }
  patch_at
}

/// The guest stores `len` bytes of [`PATCH_BYTE`] at `at` through its map, unsynced.
fn guest_stores(guest: &mut Guest, at: usize, len: usize) {
  guest.done(&Ask::Store {
    at: u64::try_from(at).unwrap(),
    len: u64::try_from(len).unwrap(),
    byte: PATCH_BYTE,
  });
}

/// Where the image file holds the first `len` bytes of the patch: the first run of them, which must lie at the patch's
/// offset within its block (ext4 maps with DAX only when its block is the page).
fn patch_in(image: &[u8], len: usize, patch_at: usize, what: &str) -> usize {
  let found = run_of(image, len).unwrap_or_else(|| {
    let (start, longest) = longest_run(image);
    panic!(
      "{what}: the guest's store is not there (the longest run of the patch's byte is {longest} bytes at {start}; \
       the patch's offset in its block is {})",
      patch_at % rustix::param::page_size()
    )
  });
  let page = rustix::param::page_size();
  assert_eq!(
    found % page,
    patch_at % page,
    "{what}: the store lies at the patch's offset within its block"
  );
  found
}

/// The guest stores the patch's first `len` bytes through its DAX map of the large file, unsynced, and the host finds
/// them in the image file, read through the slates mount, at the patch's offset within its block: where the image file
/// holds them. Nothing in the image was that run before. Without DAX the store would sit in the guest's own page cache
/// until the guest wrote it back.
fn guests_store_is_in_the_host_file(rig: &Rig, guest: &mut Guest, len: usize) -> usize {
  let patch_at = rig.source.chunk + PATCH.0;
  assert_eq!(
    run_of(&rig.image_bytes(), len),
    None,
    "the image holds no run of the patch's byte before the guest stores one"
  );
  guest_stores(guest, patch_at, len);
  patch_in(
    &rig.image_bytes(),
    len,
    patch_at,
    "the host's image file, read through the slates mount before the guest syncs (its map is not the file's pages)",
  )
}

/// The host writes `len` bytes of [`PATCH_BYTE`] at `offset` of the image file through the slates mount, and the guest
/// waits, touching memory alone, until the last of them shows at `map_at + len - 1` in its map.
fn hosts_write_shows_in_the_map(
  rig: &Rig,
  guest: &mut Guest,
  offset: usize,
  len: usize,
  map_at: usize,
) {
  write_at(
    &rig.image_file(),
    u64::try_from(offset).unwrap(),
    &vec![PATCH_BYTE; len],
  );
  match guest.ask(
    &Ask::Wait {
      at: u64::try_from(map_at + len - 1).unwrap(),
      byte: PATCH_BYTE,
      millis: DELIVERY_MILLIS,
    },
    REPLY_WAIT,
  ) {
    Reply::Waited { micros } => {
      eprintln!("the host's write showed in the guest's map in {micros} µs")
    }
    other => panic!(
      "the host's write through the slates mount did not show in the guest's map: {other:?}\n{}",
      guest.transcript()
    ),
  }
}

/// The guest stores the patch's last `len` bytes at `map_at` through its map, then syncs. Nothing has opened the image
/// file since, so only the guest's flush, which the VMM carries out on the file (Firecracker by an `msync`, QEMU 11.1 by
/// an `fsync`, `hw/virtio/virtio-pmem.c`), writes them into the volume. slates itself, read past the kernel, must then
/// hold the whole patch at `found`.
fn guests_sync_reaches_the_volume(
  rig: &Rig,
  guest: &mut Guest,
  map_at: usize,
  len: usize,
  found: usize,
) {
  guest_stores(guest, map_at, len);
  guest.done(&Ask::Sync);
  assert!(
    rig
      .held_by_slates()
      .get(found..found + PATCH.1)
      .is_some_and(|run| run.iter().all(|byte| *byte == PATCH_BYTE)),
    "after the guest's sync, slates itself holds the whole patch, the guest's synced store with the rest"
  );
}

/// A-165 by use: a real VM (Firecracker v1.17.0, QEMU) maps a slates volume's image with DAX through a virtio-pmem device.
/// Do: build the image as the drive test does; boot the VM with the image file, through a slates mount, as its pmem
/// device, the guest loading the nvdimm modules its kernel needs and mounting the device as ext4 with DAX; have the
/// guest list every node and map the large file. Then write the patch in three parts: the guest stores its first
/// quarter through the map, and the host finds it in the image file read through the slates mount; the host writes the
/// second quarter into the image file through the slates mount, and the guest waits for it in its map; the guest stores
/// the rest through the map and syncs, and slates itself is read. Make the drive test's other changes, list again, end
/// the VM and merge. Expect the guest to read every node as the volume holds it; its map to be DAX; its unsynced store
/// to be in the host's file, at the patch's offset within its block, and nowhere before it; the host's write to show in
/// the guest's map, the guest touching memory alone; after the guest's sync, slates to hold the whole patch; and the
/// merge and the clone to hold exactly the guest's tree, the drive test's to the byte.
#[test]
fn a_guest_maps_a_volume_image_with_dax_through_virtio_pmem() {
  for gate in Gate::each_from_env() {
    maps_the_image_with_dax(&gate);
  }
}

/// [`a_guest_maps_a_volume_image_with_dax_through_virtio_pmem`] in the VMM `gate` names.
fn maps_the_image_with_dax(gate: &Gate) {
  let Some(disk) = pmem_disk(gate) else {
    return;
  };
  let rig = Rig::new(&format!(
    "guest-pmem-{}-{}",
    gate.vmm.name(),
    std::process::id()
  ));
  let mut guest = rig.boot(gate, &disk);
  let patch_at = lists_and_maps(&rig, &mut guest);
  let len = PATCH.1;
  let quarter = len / 4;
  let found = guests_store_is_in_the_host_file(&rig, &mut guest, quarter);
  hosts_write_shows_in_the_map(
    &rig,
    &mut guest,
    found + quarter,
    quarter,
    patch_at + quarter,
  );
  guests_sync_reaches_the_volume(
    &rig,
    &mut guest,
    patch_at + 2 * quarter,
    len - 2 * quarter,
    found,
  );
  the_other_changes(&mut guest);
  let changed = rig.source.changed_listing();
  assert_eq!(
    listed(&mut guest),
    changed,
    "the guest reads its own changes\n{}",
    guest.transcript()
  );
  the_guest_ends_the_vm_by_its_restart(&mut guest);
  rig.merged_holds_every_change();
}

/// Where the host's image file holds the guest's first `len` stores after a takeover, the unsynced one included. When
/// it does not, the failure reports what each party holds: the daemon's counters, a second read through the mount,
/// slates itself, and the guest's map.
fn stores_after_the_takeover(rig: &Rig, guest: &mut Guest, patch_at: usize, len: usize) -> usize {
  let image = rig.image_bytes();
  if run_of(&image, len).is_some() {
    return patch_in(
      &image,
      len,
      patch_at,
      "the host's image file after the takeover",
    );
  }
  let first = longest_run(&image);
  let counters = fuse_counters(rig);
  let again = longest_run(&rig.image_bytes());
  let held = longest_run(&rig.held_by_slates());
  let map = guest.ask(
    &Ask::Read {
      at: u64::try_from(patch_at).unwrap(),
      len: u64::try_from(len).unwrap(),
    },
    REPLY_WAIT,
  );
  let mapped = match map {
    Reply::Bytes(bytes) => format!("{:?}", longest_run(&bytes)),
    other => format!("{other:?}"),
  };
  panic!(
    "after the takeover the host's image file lacks the guest's {len} stored bytes: the longest run of the patch's \
     byte (start, length) read through the mount was {first:?}, then {again:?}; slates holds {held:?}; the guest's \
     map holds {mapped}; the daemon's counters:\n{counters}"
  );
}

/// A-165 with a takeover (A-61, A-158) under a pmem guest's live DAX map. Do: boot a pmem guest, list, map the large
/// file; store the patch's first quarter through the map and sync; store its second quarter, unsynced; kill the daemon
/// with `SIGKILL` while the VMM maps the image through the slates mount; wait for the anchor's successor; find the
/// guest's stores in the image file through the mount; have the host write the third quarter through the mount and the
/// guest wait for it in its map; store the rest and sync; make the other changes, list, end the VM and merge. Expect the
/// successor to be a new daemon; the guest to go on; both its stores, the unsynced one included, in the image after the
/// takeover; the host's write to reach the guest's map; slates to hold the whole patch after the last sync; and the
/// merge and the clone to hold exactly the guest's tree.
#[test]
fn a_daemon_killed_under_a_pmem_guests_live_map_is_taken_over_and_the_guest_goes_on() {
  for gate in Gate::each_from_env() {
    a_daemon_killed_under_the_map(&gate);
  }
}

/// [`a_daemon_killed_under_a_pmem_guests_live_map_is_taken_over_and_the_guest_goes_on`] in the VMM `gate` names.
fn a_daemon_killed_under_the_map(gate: &Gate) {
  let Some(disk) = pmem_disk(gate) else {
    return;
  };
  let rig = Rig::new(&format!(
    "guest-pmem-takeover-{}-{}",
    gate.vmm.name(),
    std::process::id()
  ));
  let mut guest = rig.boot(gate, &disk);
  let patch_at = lists_and_maps(&rig, &mut guest);
  let quarter = PATCH.1 / 4;
  guest_stores(&mut guest, patch_at, quarter);
  guest.done(&Ask::Sync);
  guest_stores(&mut guest, patch_at + quarter, quarter);
  let (old, new) = kill_the_daemon(&rig.anchor, &rig.instance);
  assert_ne!(old, new, "the anchor's successor is a new daemon");
  eprintln!("daemon {old} killed under the guest's live map; successor {new}");
  let found = stores_after_the_takeover(&rig, &mut guest, patch_at, 2 * quarter);
  hosts_write_shows_in_the_map(
    &rig,
    &mut guest,
    found + 2 * quarter,
    quarter,
    patch_at + 2 * quarter,
  );
  guests_sync_reaches_the_volume(
    &rig,
    &mut guest,
    patch_at + 3 * quarter,
    PATCH.1 - 3 * quarter,
    found,
  );
  the_other_changes(&mut guest);
  assert_eq!(
    listed(&mut guest),
    rig.source.changed_listing(),
    "the guest reads its own changes after the takeover\n{}",
    guest.transcript()
  );
  the_guest_ends_the_vm_by_its_restart(&mut guest);
  rig.merged_holds_every_change();
}

/// A-165 when the VM dies. A pmem guest's DAX stores are the image file's own pages, so a store the guest never synced
/// is in the image when the VMM is killed. The guest never unmounted the file system, so the merge refuses the image
/// as possibly half-written until e2fsck has checked it. Do: boot a pmem guest, list, map the large file and store the
/// whole patch through the map, unsynced; kill the VMM without warning; read the image through slates; merge it;
/// boot a second pmem guest on the image, list, end it; merge again; `e2fsck -fy` the image through the slates mount;
/// merge. Expect slates to hold the whole patch after the kill; both merges before the check to be refused `NotClean`
/// (ext4 leaves a file system it mounted unchecked unchecked at its unmount); the second guest to read the patched file;
/// e2fsck to leave the file system clean, its one repair the `lost+found` it makes (slates' images carry none, A-160),
/// so the crash left nothing else to mend; and the last merge to take that directory as new, move and remove nothing,
/// and write the one page the patch lies in, its clone holding the patched tree and `lost+found`.
#[test]
fn a_pmem_guest_killed_with_unsynced_stores_leaves_them_in_the_image_for_a_checked_merge() {
  for gate in Gate::each_from_env() {
    a_guest_killed_with_unsynced_stores(&gate);
  }
}

/// [`a_pmem_guest_killed_with_unsynced_stores_leaves_them_in_the_image_for_a_checked_merge`] in the VMM `gate` names.
fn a_guest_killed_with_unsynced_stores(gate: &Gate) {
  let Some(disk) = pmem_disk(gate) else {
    return;
  };
  if !present("e2fsck") {
    eprintln!(
      "SKIP: the pmem crash test needs e2fsck (e2fsprogs) to check the image a killed guest left"
    );
    return;
  }
  let rig = Rig::new(&format!(
    "guest-pmem-crash-{}-{}",
    gate.vmm.name(),
    std::process::id()
  ));
  let mut first = rig.boot(gate, &disk);
  let patch_at = lists_and_maps(&rig, &mut first);
  guest_stores(&mut first, patch_at, PATCH.1);
  first.kill();
  patch_in(
    &rig.held_by_slates(),
    PATCH.1,
    patch_at,
    "slates itself, after the VM's death",
  );
  rig.merge_is_refused_unclean("refused-after-the-kill", "after the guest's death");
  let mut second = rig.boot(gate, &disk);
  let patched = rig.source.patched_listing();
  assert_eq!(
    listed(&mut second),
    patched,
    "a second guest reads the dead guest's store\n{}",
    second.transcript()
  );
  the_guest_ends_the_vm_by_its_restart(&mut second);
  rig.merge_is_refused_unclean(
    "refused-after-the-second-guest",
    "after a second guest's clean unmount of the unchecked file system",
  );
  let repairs = repaired(&rig);
  assert!(
    matches!(repairs.as_slice(), [only] if only.contains("lost+found")),
    "e2fsck's one repair is the lost+found it makes: {repairs:?}"
  );
  let mut checked = patched;
  checked.push(directory("lost+found"));
  rig.merged_holds("merged", &sorted(checked), (1, 0, 0), rig.source.page);
}

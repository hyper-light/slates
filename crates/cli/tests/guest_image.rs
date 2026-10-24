//! A real Firecracker microVM over a slates volume's disk image (§4.6, A-160). Firecracker shares no folder with its
//! guest, only block devices backed by files, so a volume reaches its guest as an image: `slates image build` renders
//! a snapshot as an ext2 file in a new volume, and Firecracker attaches that file, through a slates mount, as the
//! guest's virtio-blk disk. The guest's `/init` (`slates-guest-probe`) mounts the disk, lists every node with the
//! digest of its bytes, makes the changes the test asks for, flushes and ends the VM. `slates image merge` then makes
//! a new clone of the snapshot hold what the guest left, and the clone is read through its own mount.
//!
//! Gated. It needs Linux with KVM and three files named in the environment; without them it skips loudly:
//! - `SLATES_GUEST_FIRECRACKER`: a Firecracker binary, v1.17.0 or later (the release binary, its checksum verified
//!   against the release's `SHA256SUMS`). It runs with `--no-api`, so it opens no socket, and with `--enable-pci`, so
//!   the guest finds its disk on PCI.
//! - `SLATES_GUEST_VMLINUX`: an uncompressed guest kernel (ELF, as Firecracker boots on x86_64) with virtio-blk and
//!   virtio-pci built in, and ext2 (or `CONFIG_EXT4_USE_FOR_EXT2`). A distribution's kernel serves, unpacked from its
//!   bzImage by the kernel tree's `scripts/extract-vmlinux`.
//! - `SLATES_GUEST_PROBE`: the guest's `/init`, static (`tests/guest_dax.rs` says how to build it).
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

use std::process::Command;

use slates_guest_probe::protocol::{Command as Ask, EntryKind, Reply, digest};
use vm::{
  Guest, MountPoint, REPLY_WAIT, created_volume, initramfs, run, start_anchor, write_whole,
};

/// Shape: the image's capacity: the source's files and the guest's room to write.
const CAPACITY: &str = "32MiB";
/// Shape: the guest's memory, MiB, and processors.
const GUEST_MEMORY_MIB: u32 = 256;
/// See [`GUEST_MEMORY_MIB`].
const GUEST_CPUS: u32 = 1;
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

/// The paths the test needs from the environment, or `None` after saying loudly which are missing.
struct Gate {
  firecracker: String,
  vmlinux: String,
  probe: String,
}

impl Gate {
  fn from_env() -> Option<Gate> {
    let named = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    match (
      named("SLATES_GUEST_FIRECRACKER"),
      named("SLATES_GUEST_VMLINUX"),
      named("SLATES_GUEST_PROBE"),
    ) {
      (Some(firecracker), Some(vmlinux), Some(probe)) => Some(Gate {
        firecracker,
        vmlinux,
        probe,
      }),
      _ => {
        eprintln!(
          "SKIP: the Firecracker image test needs SLATES_GUEST_FIRECRACKER, SLATES_GUEST_VMLINUX and \
           SLATES_GUEST_PROBE (tests/guest_image.rs says what each is)"
        );
        None
      }
    }
  }
}

/// The source volume's tree: a small file, a pattern three chunks and more long, another small file and a hard link
/// to it, a file the guest removes, and a symbolic link.
struct Source {
  chunk: usize,
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

  /// The listing after the guest's changes ([`guest_changes`]).
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

/// The guest's changes: the large file patched in its second chunk, a rename into another directory, a removal, a new
/// file and a new directory, then a flush. The removal comes before the new file, so the kernel may give the new file
/// the freed inode number: under a kernel it carries a generation of its own, and the merge must still take it for new.
fn guest_changes(guest: &mut Guest, source: &Source) {
  let (at, len) = PATCH;
  guest.done(&Ask::Patch {
    path: "docs/sub/b.bin".to_owned(),
    at: u64::try_from(source.chunk + at).unwrap(),
    bytes: vec![PATCH_BYTE; len],
  });
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

/// Firecracker's configuration: the kernel, the initramfs, the image as the guest's writable virtio-blk disk (its
/// flushes written through to the file, so a guest's sync reaches the volume), and a small machine. The guest mounts
/// the disk as ext2 and ends the VM by rebooting, which Firecracker takes as its exit (`reboot=k`).
fn firecracker_config(gate: &Gate, initrd: &str, image: &str) -> String {
  format!(
    r#"{{
  "boot-source": {{
    "kernel_image_path": "{kernel}",
    "initrd_path": "{initrd}",
    "boot_args": "console=ttyS0 reboot=k panic=1 rdinit=/init quiet slates.block=/dev/vda slates.end=reboot"
  }},
  "drives": [
    {{
      "drive_id": "image",
      "path_on_host": "{image}",
      "is_root_device": false,
      "is_read_only": false,
      "cache_type": "Writeback"
    }}
  ],
  "machine-config": {{
    "vcpu_count": {GUEST_CPUS},
    "mem_size_mib": {GUEST_MEMORY_MIB}
  }}
}}
"#,
    kernel = gate.vmlinux
  )
}

/// Every node of the volume mounted at `mount`, as a guest would list it.
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
        pending.push(path.clone());
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

/// A-160 by use: a real Firecracker v1.17.0 microVM reads and writes a slates volume through its disk image. Do: write
/// a tree into a volume, snapshot it and `slates image build` it; boot Firecracker from an initramfs held in a slates
/// volume, its disk the image file read through a slates mount; have the guest list every node, patch a file inside its
/// second chunk, rename one into another directory, remove one, write a new file and make a directory, list again, flush
/// and end the VM; `slates image merge` the image and read the clone through its mount. Expect the guest to read every
/// node as the volume holds it (kind, size and digest), and its changes in its second listing; Firecracker to exit
/// cleanly; the merge to make two nodes, move one and remove one, and to write the new file's bytes and the one chunk
/// the patch lies in; and the clone to hold exactly what the guest left.
#[test]
fn a_firecracker_guest_reads_and_writes_a_volume_through_its_image() {
  let Some(gate) = Gate::from_env() else {
    return;
  };
  let instance = format!("guest-image-{}", std::process::id());
  let _anchor = start_anchor(&instance);
  let source = Source::new();
  let volume = created_volume(&instance, "source");
  let source_mount = MountPoint::of(&instance, &volume);
  source.write_into(&source_mount);
  let (code, out, err) = run(&instance, &["volume", "snapshot", &volume]);
  assert_eq!(code, 0, "volume snapshot: {err}");
  let out = String::from_utf8_lossy(&out).into_owned();
  let snapshot = out
    .lines()
    .find_map(|line| line.strip_prefix("snapshot: "))
    .unwrap()
    .to_owned();
  let (code, out, err) = run(
    &instance,
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
  let out = String::from_utf8_lossy(&out).into_owned();
  let image = out
    .lines()
    .find_map(|line| line.strip_prefix("id: "))
    .unwrap()
    .to_owned();
  let image_mount = MountPoint::of(&instance, &image);

  let boot = created_volume(&instance, "boot");
  let boot_mount = MountPoint::of(&instance, &boot);
  #[allow(clippy::disallowed_methods)] // reads the probe binary the environment names
  let probe = std::fs::read(&gate.probe).unwrap();
  let initrd = boot_mount.file("initramfs.cpio");
  write_whole(&initrd, &initramfs(&probe));
  let config = boot_mount.file("vm.json");
  write_whole(
    &config,
    firecracker_config(&gate, &initrd, &image_mount.file("image.ext2")).as_bytes(),
  );

  let booting = std::time::Instant::now();
  let mut guest = Guest::spawn(
    Command::new(&gate.firecracker).args(["--no-api", "--enable-pci", "--config-file", &config]),
    "firecracker: ",
    MOST_BYTES,
  );
  eprintln!(
    "Firecracker booted the guest to its probe's ready in {} ms",
    booting.elapsed().as_millis()
  );
  assert_eq!(
    listed(&mut guest),
    source.listing(),
    "the guest reads every node as the volume holds it\n{}",
    guest.transcript()
  );
  guest_changes(&mut guest, &source);
  let changed = source.changed_listing();
  assert_eq!(
    listed(&mut guest),
    changed,
    "the guest reads its own changes\n{}",
    guest.transcript()
  );
  let status = guest.quit();
  assert!(
    status.success(),
    "Firecracker exited {status}\n{}",
    guest.transcript()
  );

  let (code, out, err) = run(&instance, &["image", "merge", &image, "merged"]);
  assert_eq!(code, 0, "image merge: {err}");
  let out = String::from_utf8_lossy(&out).into_owned();
  eprintln!("slates image merge:\n{out}");
  assert_eq!(
    (
      count(&out, "created"),
      count(&out, "moved"),
      count(&out, "removed")
    ),
    (2, 1, 1),
    "{out}"
  );
  assert_eq!(
    count(&out, "bytes_written"),
    u64::try_from(source.chunk + NEW_FILE.len()).unwrap(),
    "{out}"
  );
  let merged = out
    .lines()
    .find_map(|line| line.strip_prefix("id: "))
    .unwrap()
    .to_owned();
  let merged_mount = MountPoint::of(&instance, &merged);
  assert_eq!(
    volume_listing(&merged_mount),
    changed,
    "the clone holds what the guest left"
  );
}

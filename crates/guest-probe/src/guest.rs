//! The guest's `/init` (Linux; `docs/wip/virtiofs.md` §8 item 7). It mounts what a kernel needs and takes the console
//! as its standard streams in raw mode, so a command arrives as the host sent it and is never echoed into the replies.
//! It mounts the virtio-fs tag the VMM shares in, with the DAX option the kernel command line asks for. Then it answers
//! the host's commands ([`slates_guest_probe::protocol`]) one line at a time until told to quit, and powers the VM off.
//!
//! It never returns: as the first process, a return would panic the guest's kernel. A command it cannot carry out is
//! answered [`Reply::Refused`]; when nothing can go on, it says [`Reply::Fatal`] and powers off.
//!
//! The map's bytes are read and written as volatile memory accesses, so each happens where the command says. A wait
//! polls the map's memory alone (the vDSO's clock is no system call): seeing another path's write that way is what DAX
//! through a mount has to provide.
//!
//! **The image guest (A-160).** With `slates.block=DEVICE` on its command line, the guest mounts that block device as
//! ext2 instead of a virtio-fs tag: a Firecracker guest, which reaches a slates volume only as an image file the VMM
//! presents as a virtio-blk disk. It then lists every node with the digest of its bytes, so the host can check the
//! guest read every file as the volume holds it, and makes the changes the host asks for: writes, patches, renames,
//! removals, directories, a flush. With `slates.end=reboot` it ends the VM by rebooting, which is how a Firecracker
//! guest stops it (its command line's `reboot=k` resets the keyboard controller, and Firecracker exits); otherwise by
//! powering off, as under crosvm.
//!
//! **The pmem guest (A-165).** Firecracker can present the same image file as a virtio-pmem device instead: the file's
//! pages, mapped shared by Firecracker through the slates mount, become a range of the guest's physical memory. With
//! `slates.fs=ext4` and `slates.dax=always` the guest mounts it with DAX, so a file's reads, writes and maps reach
//! those pages with no page cache of the guest's own between (ext2's own driver no longer offers DAX; ext4's mounts an
//! ext2 image). A distribution's kernel builds virtio-pmem and the nvdimm drivers behind it as modules, so with
//! `slates.modules=` the guest first loads the modules its initramfs carries under `/modules`, in the order named, and
//! waits for the device's node, which the drivers create after their loads return.

use std::ffi::{CStr, CString, c_int};
use std::io::Write;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;

use rustix::fs::inotify::{CreateFlags, Reader, WatchFlags};
use rustix::fs::{AtFlags, Mode, OFlags, StatxAttributes, StatxFlags};
use rustix::mm::{MapFlags, MsyncFlags, ProtFlags};
use rustix::mount::{MountFlags, UnmountFlags};
use rustix::system::RebootCommand;
use rustix::termios::OptionalActions;
use rustix::time::ClockId;
use slates_guest_probe::protocol::{Command, EntryKind, LONGEST_COMMAND, Reply, digest};

/// Format: where the guest mounts the shared tag.
const MOUNT_POINT: &str = "/mnt";
/// Format: the console the kernel gives the first process.
const CONSOLE: &str = "/dev/console";
/// Format: the kernel command line's key naming the virtio-fs tag to mount.
const TAG_KEY: &str = "slates.tag=";
/// Format: the kernel command line's key naming the `dax=` mount option (`always`, `never`, `inode`).
const DAX_KEY: &str = "slates.dax=";
/// Format: the tag mounted when the command line names none.
const DEFAULT_TAG: &str = "slates";
/// Format: the kernel command line's key naming a block device to mount as ext2 in place of a virtio-fs tag.
const BLOCK_KEY: &str = "slates.block=";
/// Format: the kernel command line's key naming how the VM ends: `reboot`, or a power-off otherwise.
const END_KEY: &str = "slates.end=";
/// Format: the file system a block device holds: the image a slates volume is presented as (A-160). A kernel without
/// the ext2 driver mounts it through ext4's (`CONFIG_EXT4_USE_FOR_EXT2`).
const BLOCK_FILESYSTEM: &str = "ext2";
/// Format: the kernel command line's key naming the file system to mount a block device as, in place of
/// [`BLOCK_FILESYSTEM`]: `ext4` for a DAX mount of the image (A-165).
const FILESYSTEM_KEY: &str = "slates.fs=";
/// Format: the kernel command line's key naming the kernel modules to load before the mount: files of
/// [`MODULE_DIRECTORY`], comma-separated, in load order.
const MODULES_KEY: &str = "slates.modules=";
/// Format: where the initramfs carries the modules [`MODULES_KEY`] names.
const MODULE_DIRECTORY: &str = "/modules";
/// Format: `finit_module`'s flag for a compressed module file, which the kernel decompresses itself
/// (`MODULE_INIT_COMPRESSED_FILE`, `include/uapi/linux/module.h`, since Linux 5.17).
const MODULE_INIT_COMPRESSED_FILE: c_int = 4;
/// Format: the suffixes of a compressed module file, one per compression a kernel can be built to load
/// (`CONFIG_MODULE_COMPRESS_ZSTD`, `_XZ`, `_GZIP`).
const COMPRESSED_MODULE_SUFFIXES: [&str; 3] = [".zst", ".xz", ".gz"];
/// Format: the fixed part of an inotify event (`struct inotify_event`: `wd`, `mask`, `cookie`, `len`, four bytes each).
const INOTIFY_EVENT_HEADER: usize = 16;
/// Format: the longest name an inotify event carries (`NAME_MAX`).
const NAME_MAX: usize = 255;
/// Derived: the smallest buffer that reads at least one inotify event, as `inotify(7)` gives it:
/// `sizeof(struct inotify_event) + NAME_MAX + 1`.
const INOTIFY_BUFFER: usize = INOTIFY_EVENT_HEADER + NAME_MAX + 1;
/// Shape: the most bytes one `write` or `patch` may carry, expanded from its runs: a few chunks of a test file.
const LONGEST_WRITE: usize = 16 << 20;
/// Shape: the most of the kernel command line read: x86_64's own limit is 2,048 bytes.
const LONGEST_CMDLINE: usize = 4096;
/// Shape: the bytes the console is read in at a time.
const CONSOLE_CHUNK: usize = 256;
/// Format: microseconds in a millisecond.
const MICROS_PER_MILLI: u64 = 1_000;
/// Format: microseconds in a second.
const MICROS_PER_SECOND: u64 = 1_000_000;
/// Format: nanoseconds in a microsecond.
const NANOS_PER_MICRO: u64 = 1_000;

/// How the VM ends.
#[derive(Clone, Copy)]
enum End {
  /// A power-off (crosvm).
  PowerOff,
  /// A reboot, which Firecracker takes as the VM's end.
  Reboot,
}

/// Runs the guest; never returns.
pub(crate) fn run() -> ! {
  let mut end = End::PowerOff;
  if let Err(fatal) = serve(&mut end) {
    // The end below is the report if this line cannot be written either.
    if say(&Reply::Fatal(fatal)).is_err() {
      eprintln!("slates-guest-probe: the console is gone");
    }
  }
  power_off(end)
}

/// Takes the console, mounts the tag or the block device, and answers commands until told to quit.
fn serve(end: &mut End) -> Result<(), String> {
  attach_console()?;
  let cmdline = kernel_command_line()?;
  if option(&cmdline, END_KEY) == Some("reboot") {
    *end = End::Reboot;
  }
  load_modules(&cmdline)?;
  match option(&cmdline, BLOCK_KEY) {
    Some(device) => {
      await_node(device)?;
      mount_block(&cmdline, device)?;
    }
    None => mount_tag(&cmdline)?,
  }
  say(&Reply::Ready)?;
  let mut map: Option<SharedMap> = None;
  let mut console = LineReader::default();
  loop {
    let line = console.next_line()?;
    let reply = match Command::parse_within(&line, LONGEST_WRITE) {
      Ok(Command::Quit) => break,
      Ok(Command::List) => {
        list()?;
        Reply::Done
      }
      Ok(command) => execute(command, &mut map),
      Err(refusal) => Reply::Refused(refusal.to_string()),
    };
    say(&reply)?;
  }
  drop(map);
  rustix::fs::sync();
  rustix::mount::unmount(MOUNT_POINT, UnmountFlags::empty())
    .map_err(|e| format!("unmount {MOUNT_POINT}: {e}"))?;
  say(&Reply::Done)
}

/// One command, carried out against the map or the mounted file system.
fn execute(command: Command, map: &mut Option<SharedMap>) -> Reply {
  match command {
    Command::Map { path, pages } => match SharedMap::open(&path, pages) {
      Ok((made, dax)) => {
        *map = Some(made);
        Reply::Mapped { dax }
      }
      Err(refusal) => Reply::Refused(refusal),
    },
    Command::Write { path, bytes } => done_or_refused(write_file(&path, &bytes)),
    Command::Patch { path, at, bytes } => done_or_refused(patch_file(&path, at, &bytes)),
    Command::Rename { from, to } => done_or_refused(rename(&from, &to)),
    Command::Remove { path } => done_or_refused(remove(&path)),
    Command::Mkdir { path } => done_or_refused(make_directory(&path)),
    Command::Flush => {
      rustix::fs::sync();
      Reply::Done
    }
    other => match map.as_ref() {
      Some(mapped) => mapped.answer(other),
      None => Reply::Refused("no map has been made".to_owned()),
    },
  }
}

/// [`Reply::Done`], or the refusal.
fn done_or_refused(result: Result<(), String>) -> Reply {
  result.map_or_else(Reply::Refused, |()| Reply::Done)
}

/// `path` under the mount, refused when it climbs out of it.
fn under_mount(path: &str) -> Result<std::path::PathBuf, String> {
  let relative = std::path::Path::new(path.trim_start_matches('/'));
  if relative
    .components()
    .any(|part| !matches!(part, std::path::Component::Normal(_)))
  {
    return Err(format!("{path} is not a plain path under the mount"));
  }
  Ok(std::path::Path::new(MOUNT_POINT).join(relative))
}

/// Makes `path` hold exactly `bytes`.
#[allow(clippy::disallowed_methods)] // the guest's own file system, in a VM: this binary never runs on a host
fn write_file(path: &str, bytes: &[u8]) -> Result<(), String> {
  // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
  std::fs::write(under_mount(path)?, bytes).map_err(|e| format!("write {path}: {e}"))
}

/// Writes `bytes` into `path` at `at`.
fn patch_file(path: &str, at: u64, bytes: &[u8]) -> Result<(), String> {
  use std::os::unix::fs::FileExt;
  // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
  std::fs::OpenOptions::new()
    .write(true)
    .open(under_mount(path)?)
    .and_then(|file| file.write_all_at(bytes, at))
    .map_err(|e| format!("patch {path} at {at}: {e}"))
}

/// Renames `from` to `to`.
#[allow(clippy::disallowed_methods)] // the guest's own file system, in a VM: this binary never runs on a host
fn rename(from: &str, to: &str) -> Result<(), String> {
  // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
  std::fs::rename(under_mount(from)?, under_mount(to)?)
    .map_err(|e| format!("rename {from} {to}: {e}"))
}

/// Removes `path`: an empty directory, or a name of anything else.
#[allow(clippy::disallowed_methods)] // the guest's own file system, in a VM: this binary never runs on a host
fn remove(path: &str) -> Result<(), String> {
  let full = under_mount(path)?;
  // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
  let directory = std::fs::symlink_metadata(&full)
    .map_err(|e| format!("stat {path}: {e}"))?
    .is_dir();
  if directory {
    // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
    std::fs::remove_dir(&full)
  } else {
    // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
    std::fs::remove_file(&full)
  }
  .map_err(|e| format!("remove {path}: {e}"))
}

/// Makes the directory `path`.
#[allow(clippy::disallowed_methods)] // the guest's own file system, in a VM: this binary never runs on a host
fn make_directory(path: &str) -> Result<(), String> {
  // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
  std::fs::create_dir(under_mount(path)?).map_err(|e| format!("mkdir {path}: {e}"))
}

/// Says every node under the mount, one [`Reply::Entry`] each, in path order. A directory the walk cannot read ends
/// the guest, since its listing would be a lie by omission.
fn list() -> Result<(), String> {
  let mut entries = Vec::new();
  let mut pending = vec![String::new()];
  while let Some(directory) = pending.pop() {
    let full = format!("{MOUNT_POINT}/{directory}");
    // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
    let read = std::fs::read_dir(&full).map_err(|e| format!("list {full}: {e}"))?;
    for entry in read {
      let entry = entry.map_err(|e| format!("list {full}: {e}"))?;
      let name = entry.file_name().to_string_lossy().into_owned();
      let path = if directory.is_empty() {
        name
      } else {
        format!("{directory}/{name}")
      };
      let (kind, size, sum) = described(&entry.path(), &path)?;
      if kind == EntryKind::Dir {
        pending.push(path.clone());
      }
      entries.push(Reply::Entry {
        kind,
        size,
        digest: sum,
        path,
      });
    }
  }
  entries.sort_by(|left, right| match (left, right) {
    (Reply::Entry { path: left, .. }, Reply::Entry { path: right, .. }) => left.cmp(right),
    _ => std::cmp::Ordering::Equal,
  });
  for entry in &entries {
    say(entry)?;
  }
  Ok(())
}

/// A node's kind, size and digest: a file's length and the digest of its bytes, a link's target's, zero otherwise.
fn described(full: &std::path::Path, path: &str) -> Result<(EntryKind, u64, u64), String> {
  use std::os::unix::fs::FileTypeExt;
  // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
  let metadata = std::fs::symlink_metadata(full).map_err(|e| format!("stat {path}: {e}"))?;
  let kind = metadata.file_type();
  Ok(if kind.is_file() {
    // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
    let bytes = std::fs::read(full).map_err(|e| format!("read {path}: {e}"))?;
    (EntryKind::File, metadata.len(), digest(&bytes))
  } else if kind.is_symlink() {
    // structural: allow — the guest's own file system, in a VM: this binary never runs on a host.
    let target = std::fs::read_link(full).map_err(|e| format!("readlink {path}: {e}"))?;
    let target = target.as_os_str().as_encoded_bytes().to_vec();
    (
      EntryKind::Link,
      u64::try_from(target.len()).unwrap_or(u64::MAX),
      digest(&target),
    )
  } else if kind.is_dir() {
    (EntryKind::Dir, 0, 0)
  } else if kind.is_fifo() {
    (EntryKind::Fifo, 0, 0)
  } else if kind.is_socket() {
    (EntryKind::Socket, 0, 0)
  } else {
    (EntryKind::Other, 0, 0)
  })
}

/// Loads the kernel modules the command line names ([`MODULES_KEY`]), in its order, each from [`MODULE_DIRECTORY`]; a
/// compressed one the kernel decompresses. A module that does not load ends the guest, since the device it drives
/// would never come.
fn load_modules(cmdline: &str) -> Result<(), String> {
  let Some(names) = option(cmdline, MODULES_KEY) else {
    return Ok(());
  };
  for name in names.split(',').filter(|name| !name.is_empty()) {
    let path = format!("{MODULE_DIRECTORY}/{name}");
    let file = rustix::fs::open(
      path.as_str(),
      OFlags::RDONLY | OFlags::CLOEXEC,
      Mode::empty(),
    )
    .map_err(|e| format!("open {path}: {e}"))?;
    let flags = if COMPRESSED_MODULE_SUFFIXES
      .iter()
      .any(|suffix| name.ends_with(suffix))
    {
      MODULE_INIT_COMPRESSED_FILE
    } else {
      0
    };
    rustix::system::finit_module(&file, c"", flags)
      .map_err(|e| format!("load module {name}: {e}"))?;
  }
  Ok(())
}

/// Waits until the device node `device` exists. A module's driver binds its device, and the kernel names it in `/dev`,
/// after the load returns (libnvdimm registers its namespaces asynchronously), so the wait is on an inotify watch of the
/// node's directory, set before the first look so that no creation falls between the two. The host's boot wait bounds
/// it: a node that never comes is a guest that never says it is ready.
fn await_node(device: &str) -> Result<(), String> {
  let directory = std::path::Path::new(device)
    .parent()
    .ok_or_else(|| format!("{device} names no directory"))?;
  let inotify =
    rustix::fs::inotify::init(CreateFlags::CLOEXEC).map_err(|e| format!("inotify: {e}"))?;
  rustix::fs::inotify::add_watch(&inotify, directory, WatchFlags::CREATE)
    .map_err(|e| format!("watch {}: {e}", directory.display()))?;
  let mut buffer = [MaybeUninit::<u8>::uninit(); INOTIFY_BUFFER];
  let mut events = Reader::new(&inotify, &mut buffer);
  loop {
    match rustix::fs::stat(device) {
      Ok(_) => return Ok(()),
      Err(rustix::io::Errno::NOENT) => {
        events.next().map_err(|e| format!("await {device}: {e}"))?;
      }
      Err(e) => return Err(format!("stat {device}: {e}")),
    }
  }
}

/// Mounts block device `device` at [`MOUNT_POINT`] as the file system the command line names ([`FILESYSTEM_KEY`], or
/// [`BLOCK_FILESYSTEM`]), with the `dax=` option it names.
fn mount_block(cmdline: &str, device: &str) -> Result<(), String> {
  let filesystem = option(cmdline, FILESYSTEM_KEY).unwrap_or(BLOCK_FILESYSTEM);
  let data = dax_option(cmdline)?;
  rustix::mount::mount(
    device,
    MOUNT_POINT,
    filesystem,
    MountFlags::empty(),
    data.as_deref(),
  )
  .map_err(|e| format!("mount {device} as {filesystem} at {MOUNT_POINT}: {e}"))
}

/// The `dax=` mount option the command line names ([`DAX_KEY`]), if it names one.
fn dax_option(cmdline: &str) -> Result<Option<CString>, String> {
  option(cmdline, DAX_KEY)
    .map(|mode| CString::new(format!("dax={mode}")))
    .transpose()
    .map_err(|e| format!("the dax option: {e}"))
}

/// Mounts a pseudo-filesystem the guest needs.
fn mount_pseudo(source: &str, target: &str, kind: &str) -> Result<(), String> {
  rustix::mount::mount(source, target, kind, MountFlags::empty(), None::<&CStr>)
    .map_err(|e| format!("mount {kind} at {target}: {e}"))
}

/// Mounts `/dev`, makes the console the standard streams in raw mode, and mounts `/proc` and `/sys`.
fn attach_console() -> Result<(), String> {
  mount_pseudo("dev", "/dev", "devtmpfs")?;
  // structural: allow — the guest's console device, opened for its two directions; this runs only in a VM guest.
  let console = rustix::fs::open(CONSOLE, OFlags::RDWR | OFlags::NOCTTY, Mode::empty())
    .map_err(|e| format!("open {CONSOLE}: {e}"))?;
  rustix::stdio::dup2_stdin(&console).map_err(|e| format!("console as stdin: {e}"))?;
  rustix::stdio::dup2_stdout(&console).map_err(|e| format!("console as stdout: {e}"))?;
  rustix::stdio::dup2_stderr(&console).map_err(|e| format!("console as stderr: {e}"))?;
  let mut terminal =
    rustix::termios::tcgetattr(&console).map_err(|e| format!("console attributes: {e}"))?;
  terminal.make_raw();
  rustix::termios::tcsetattr(&console, OptionalActions::Now, &terminal)
    .map_err(|e| format!("raw console: {e}"))?;
  mount_pseudo("proc", "/proc", "proc")?;
  mount_pseudo("sys", "/sys", "sysfs")
}

/// The kernel command line, up to [`LONGEST_CMDLINE`] bytes.
fn kernel_command_line() -> Result<String, String> {
  let file = rustix::fs::open("/proc/cmdline", OFlags::RDONLY, Mode::empty())
    .map_err(|e| format!("open /proc/cmdline: {e}"))?;
  let mut text = [0u8; LONGEST_CMDLINE];
  let read = rustix::io::read(&file, &mut text).map_err(|e| format!("read /proc/cmdline: {e}"))?;
  Ok(String::from_utf8_lossy(text.get(..read).unwrap_or_default()).into_owned())
}

/// The value of `key` on the kernel command line, if it is there.
fn option<'c>(cmdline: &'c str, key: &str) -> Option<&'c str> {
  cmdline
    .split_whitespace()
    .find_map(|word| word.strip_prefix(key))
}

/// Mounts the virtio-fs tag at [`MOUNT_POINT`], with the DAX option the command line names.
fn mount_tag(cmdline: &str) -> Result<(), String> {
  let tag = option(cmdline, TAG_KEY).unwrap_or(DEFAULT_TAG);
  let data = dax_option(cmdline)?;
  rustix::mount::mount(
    tag,
    MOUNT_POINT,
    "virtiofs",
    MountFlags::empty(),
    data.as_deref(),
  )
  .map_err(|e| format!("mount virtiofs tag {tag} at {MOUNT_POINT}: {e}"))
}

/// Writes one reply line to the console.
fn say(reply: &Reply) -> Result<(), String> {
  let mut out = std::io::stdout().lock();
  writeln!(out, "{}", reply.encode())
    .and_then(|()| out.flush())
    .map_err(|e| format!("the console: {e}"))
}

/// The monotonic clock in microseconds (through the vDSO, so a wait's poll makes no system call).
fn now_micros() -> u64 {
  let now = rustix::time::clock_gettime(ClockId::Monotonic);
  let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
  let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
  seconds
    .saturating_mul(MICROS_PER_SECOND)
    .saturating_add(nanos / NANOS_PER_MICRO)
}

/// Syncs and ends the VM as `end` says. If the kernel refuses, the guest parks for good rather than return.
fn power_off(end: End) -> ! {
  // Every byte said reaches the host before the VM ends. A write returns once the tty holds the bytes, not once the
  // serial port has sent them, and the VM's end drops what the port still holds: a fatal reply arrived cut short
  // ("mount virtiofs tag slates at /mn", 2026-10-25).
  if let Err(undrained) = rustix::termios::tcdrain(std::io::stdout()) {
    eprintln!("slates-guest-probe: the console did not drain: {undrained}");
  }
  rustix::fs::sync();
  let command = match end {
    End::PowerOff => RebootCommand::PowerOff,
    End::Reboot => RebootCommand::Restart,
  };
  if let Err(refused) = rustix::system::reboot(command) {
    eprintln!("slates-guest-probe: the VM's end refused: {refused}");
  }
  loop {
    std::thread::park();
  }
}

/// The console's lines, read in chunks and split at either line ending.
#[derive(Default)]
struct LineReader {
  pending: Vec<u8>,
}

impl LineReader {
  /// The next non-empty line, without its line ending. A line longer than [`LONGEST_COMMAND`] is refused, as is a
  /// console that closes.
  fn next_line(&mut self) -> Result<String, String> {
    loop {
      if let Some(end) = self
        .pending
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')
      {
        let line: Vec<u8> = self.pending.drain(..=end).collect();
        let text = String::from_utf8_lossy(line.get(..end).unwrap_or_default()).into_owned();
        if !text.trim().is_empty() {
          return Ok(text);
        }
        continue;
      }
      if self.pending.len() > LONGEST_COMMAND {
        return Err(format!(
          "a command line longer than {LONGEST_COMMAND} bytes"
        ));
      }
      let mut chunk = [0u8; CONSOLE_CHUNK];
      let read = rustix::io::read(rustix::stdio::stdin(), &mut chunk)
        .map_err(|e| format!("the console: {e}"))?;
      if read == 0 {
        return Err("the console closed".to_owned());
      }
      self
        .pending
        .extend_from_slice(chunk.get(..read).unwrap_or_default());
    }
  }
}

/// A file of the shared tag, mapped shared for reading and writing; unmapped on drop, before the file closes.
struct SharedMap {
  base: *mut u8,
  /// The bytes mapped: a whole number of pages.
  len: usize,
  /// Held so the map's file stays open; it closes after `Drop` unmaps.
  _file: OwnedFd,
}

impl SharedMap {
  /// Opens `path` under the mount and maps its first `pages` pages shared. Says whether the kernel serves the file's
  /// inode in DAX mode.
  fn open(path: &str, pages: u64) -> Result<(SharedMap, bool), String> {
    let full = format!("{MOUNT_POINT}/{}", path.trim_start_matches('/'));
    // structural: allow — a file of the slates volume a VMM shares into this guest; this binary never runs on a host.
    let file = rustix::fs::open(full.as_str(), OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
      .map_err(|e| format!("open {full}: {e}"))?;
    let dax = rustix::fs::statx(&file, c"", AtFlags::EMPTY_PATH, StatxFlags::BASIC_STATS)
      .map(|stat| stat.stx_attributes.contains(StatxAttributes::DAX))
      .map_err(|e| format!("statx {full}: {e}"))?;
    let len = usize::try_from(pages)
      .ok()
      .and_then(|pages| pages.checked_mul(rustix::param::page_size()))
      .filter(|len| *len > 0)
      .ok_or_else(|| format!("{pages} pages is no map this guest can make"))?;
    // SAFETY: a fresh shared map of `len` bytes of the open `file`, at an address the kernel chooses; nothing else maps
    // it here, and `Drop` unmaps it before `file` closes.
    let base = unsafe {
      rustix::mm::mmap(
        std::ptr::null_mut(),
        len,
        ProtFlags::READ | ProtFlags::WRITE,
        MapFlags::SHARED,
        &file,
        0,
      )
    }
    .map_err(|e| format!("mmap {full}: {e}"))?;
    Ok((
      SharedMap {
        base: base.cast::<u8>(),
        len,
        _file: file,
      },
      dax,
    ))
  }

  /// The map's offsets `[at, at + len)`, refused unless they lie within it.
  fn span(&self, at: u64, len: u64) -> Result<std::ops::Range<usize>, String> {
    let start = usize::try_from(at).map_err(|_| format!("offset {at} is out of range"))?;
    let len = usize::try_from(len).map_err(|_| format!("length {len} is out of range"))?;
    let end = start
      .checked_add(len)
      .filter(|end| *end <= self.len)
      .ok_or_else(|| {
        format!(
          "[{at}, {at} + {len}) is outside the {} mapped bytes",
          self.len
        )
      })?;
    Ok(start..end)
  }

  /// The byte at `offset`, which [`SharedMap::span`] has bounded.
  fn byte_at(&self, offset: usize) -> u8 {
    // SAFETY: `offset` is below `self.len` (bounded by `span`), so the byte lies inside the live map made in `open`; a
    // volatile read, so it reads the page the kernel maps now.
    unsafe { self.base.add(offset).read_volatile() }
  }

  /// A store, a sync, a read or a wait against the map.
  fn answer(&self, command: Command) -> Reply {
    let answered = match command {
      Command::Store { at, len, byte } => self.store(at, len, byte),
      Command::Sync => self.sync(),
      Command::Read { at, len } => self
        .span(at, len)
        .map(|offsets| Reply::Bytes(offsets.map(|offset| self.byte_at(offset)).collect())),
      Command::Wait { at, byte, millis } => self
        .span(at, 1)
        .map(|offsets| self.wait(offsets.start, byte, millis)),
      Command::Map { .. }
      | Command::Quit
      | Command::List
      | Command::Write { .. }
      | Command::Patch { .. }
      | Command::Rename { .. }
      | Command::Remove { .. }
      | Command::Mkdir { .. }
      | Command::Flush => Err("not a command a map answers".to_owned()),
    };
    answered.unwrap_or_else(Reply::Refused)
  }

  /// Stores `byte` into `[at, at + len)`, leaving the pages dirty.
  fn store(&self, at: u64, len: u64, byte: u8) -> Result<Reply, String> {
    for offset in self.span(at, len)? {
      // SAFETY: `offset` is within `span`'s bounds, inside the live, writable map made in `open`; a volatile store, so
      // it is made here, into the page the kernel maps.
      unsafe { self.base.add(offset).write_volatile(byte) };
    }
    Ok(Reply::Done)
  }

  /// `msync`s the whole map: its dirty bytes are written back before this returns, or the refusal says why not.
  fn sync(&self) -> Result<Reply, String> {
    // SAFETY: syncs exactly the live map made in `open`.
    unsafe { rustix::mm::msync(self.base.cast(), self.len, MsyncFlags::SYNC) }
      .map(|()| Reply::Done)
      .map_err(|e| format!("msync: {e}"))
  }

  /// Polls the byte at `offset`, memory alone, until it holds `byte` or `millis` pass.
  fn wait(&self, offset: usize, byte: u8, millis: u64) -> Reply {
    let started = now_micros();
    let limit = millis.saturating_mul(MICROS_PER_MILLI);
    loop {
      let seen = self.byte_at(offset);
      let elapsed = now_micros().saturating_sub(started);
      if seen == byte {
        return Reply::Waited { micros: elapsed };
      }
      if elapsed >= limit {
        return Reply::TimedOut { seen };
      }
    }
  }
}

impl Drop for SharedMap {
  fn drop(&mut self) {
    // SAFETY: unmaps exactly the map made in `open`, once; nothing reads it afterwards, and the file closes after.
    if let Err(e) = unsafe { rustix::mm::munmap(self.base.cast(), self.len) } {
      eprintln!("slates-guest-probe: munmap: {e}");
    }
  }
}

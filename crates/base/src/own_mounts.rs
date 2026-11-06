//! The daemon's own mounts, and the check that refuses a path on one before any open (§4.6, the rule of 2026-11-06:
//! no daemon thread opens a path on a volume the daemon serves; A-227).
//!
//! A request a daemon sends to its own volume is answered only by that daemon. On one shard that is a deadlock on every
//! OS, and on Windows a daemon killed with one outstanding cannot exit (A-190's amendment of 2026-11-06). So every place
//! the daemon opens a host path it was given (a volume's base directory and every lookup below it, a landing's target,
//! a FUSE mount point) asks this module first, and is refused typed when the path lies on a mount this daemon serves.
//!
//! **What is this daemon's.** A process-wide set of the mounts it made or adopted, recorded as each is made: mount ids on
//! Linux. It is lock-free and bounded: atomic slots, a capacity from the most attachments the daemon's shards can hold
//! ([`install`]), and a typed refusal at it ([`Full`]). Between a mount and its record a lookup may already cross it, so
//! a mount the set does not hold is looked up once in the kernel's table: a source naming this daemon's tag
//! ([`fsname`]), a random value made at each start for this alone, is its own. The table is world-readable, so the tag
//! names nothing else (never an attachment, never the instance). The kernel reuses mount ids, so the set is asked first:
//! a stale entry can only refuse, never admit.
//!
//! **How a path's mount is found (Linux).** Each component is opened `O_PATH | O_NOFOLLOW`, which calls no filesystem
//! code, and its mount id read from `/proc/self/fdinfo` (procfs, never the filesystem). Only a component whose mount
//! differs from its parent's is checked against the set. A symbolic link along a root path is read on the filesystem
//! that holds it (never ours: its own mount was checked) and followed by hand, within the kernel's own bound.
//!
//! **macOS.** The NFS mount's source (`slates:/<volume>`) names no daemon, so a mount is recorded when the CLI binds it
//! (`BindMount`) and when a successor adopts its record: the kernel's table (`getfsstat(MNT_NOWAIT)`, no RPC) names the
//! mount at the bound point, and its filesystem id is recorded. A crossing is then the kernel's path of the parent
//! directory (`F_GETPATH`, the name cache, on a filesystem that is not ours) with the entry's name, compared exactly
//! with the mount point the table names for each recorded id, read once a thread and again only when the set changes.
//!
//! **Windows.** Each WinFsp volume's kernel name is recorded by its id ([`volume_id`]); the base plane names a path's
//! drive by `QueryDosDeviceW` and compares (`crate::windows`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use slates_vfs::host::HostError;

/// The set is full: this daemon has as many mounts as it can record, so it makes no more (A-227).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full {
  /// The most mounts the set records.
  pub capacity: usize,
}

/// The process's own mounts: their ids in atomic slots, zero for a free one, scanned up to the highest slot ever used.
struct Set {
  slots: Box<[AtomicU64]>,
  high: AtomicUsize,
  /// Moves on at every record and forget, so a thread's cache of the set's mount points knows to refresh (macOS).
  generation: AtomicU64,
}

static SET: OnceLock<Set> = OnceLock::new();

/// This daemon's tag, made once at its start ([`install`]).
static TAG: OnceLock<u64> = OnceLock::new();

/// Format: the prefix of a slates FUSE mount's source in the kernel's table.
pub const SOURCE_PREFIX: &str = "slates:";

/// Makes the process's set of its own mounts, room for `capacity` (the most attachments its shards can hold: each is
/// at most one mount), and its tag. Once per process; a second call changes nothing and reports `false`.
pub fn install(capacity: usize) -> Result<bool, HostError> {
  let tag = fresh_tag()?;
  let made = SET
    .set(Set {
      slots: (0..capacity.max(1)).map(|_| AtomicU64::new(0)).collect(),
      high: AtomicUsize::new(0),
      generation: AtomicU64::new(0),
    })
    .is_ok();
  let _ = TAG.set(tag);
  Ok(made)
}

/// The source a FUSE mount this daemon makes names (`slates:<tag>`), `None` before [`install`].
pub fn fsname() -> Option<String> {
  TAG.get().map(|tag| format!("{SOURCE_PREFIX}{tag:016x}"))
}

/// Records `id` as one of this daemon's own mounts; refused [`Full`] at the bound, and nothing recorded. Each record
/// takes a slot of its own and each [`forget`] frees one, so every holder of a record (a mount learned at a restart,
/// then kept by its mount) owns its own and none forgets another's. Before [`install`] nothing is recorded (a process
/// that serves no mounts).
pub fn record(id: u64) -> Result<(), Full> {
  SET.get().map_or(Ok(()), |set| record_into(set, id))
}

fn record_into(set: &Set, id: u64) -> Result<(), Full> {
  if id == 0 {
    return Ok(());
  }
  for (at, slot) in set.slots.iter().enumerate() {
    if slot
      .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      set.high.fetch_max(at.saturating_add(1), Ordering::AcqRel);
      set.generation.fetch_add(1, Ordering::AcqRel);
      return Ok(());
    }
  }
  Err(Full {
    capacity: set.slots.len(),
  })
}

/// Why a mount was not recorded as this daemon's own.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unrecorded {
  /// The kernel's table names no such mount at the point.
  NotMounted,
  /// The set holds as many mounts as it can.
  Full(Full),
}

#[cfg(unix)]
impl std::fmt::Display for Unrecorded {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::NotMounted => write!(f, "is not in the kernel's mount table"),
      Self::Full(full) => write!(
        f,
        "is past the {} mounts this daemon records as its own",
        full.capacity
      ),
    }
  }
}

/// Records the mount at exactly `point` in the kernel's table whose source `wanted` takes ([`mount_id_at`], nothing
/// opened), returning its id for the caller's [`forget`].
#[cfg(unix)]
pub fn record_at(point: &str, wanted: impl Fn(&str) -> bool) -> Result<u64, Unrecorded> {
  let id = mount_id_at(point, wanted).ok_or(Unrecorded::NotMounted)?;
  record(id).map_err(Unrecorded::Full)?;
  Ok(id)
}

/// Forgets `id`, a mount of this daemon's that has ended.
pub fn forget(id: u64) {
  if let Some(set) = SET.get() {
    forget_in(set, id);
  }
}

fn forget_in(set: &Set, id: u64) {
  for slot in set.slots.iter().take(set.high.load(Ordering::Acquire)) {
    if slot
      .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      set.generation.fetch_add(1, Ordering::AcqRel);
      return;
    }
  }
}

/// Whether `id` is one of this daemon's own mounts, as recorded (a FUSE mount's table id, A-227).
pub fn is_recorded(id: u64) -> bool {
  SET.get().is_some_and(|set| recorded(set, id))
}

fn recorded(set: &Set, id: u64) -> bool {
  set
    .slots
    .iter()
    .take(set.high.load(Ordering::Acquire))
    .any(|slot| slot.load(Ordering::Acquire) == id)
}

/// The shards of this process still learning the mounts they keep across a restart (A-227), and the budget a check
/// waits for them. A successor learns its predecessor's surviving mounts from each shard's catalog as the shard
/// starts, and one shard's base may lie under another's mount, so no crossing is judged until every shard has learned.
#[cfg(unix)]
static LEARNING: AtomicUsize = AtomicUsize::new(0);

#[cfg(unix)]
static LEARNING_BUDGET_NS: AtomicU64 = AtomicU64::new(0);

/// One shard's learning of the mounts it keeps: taken before the shard starts, let go once its kept mounts are
/// recorded (or its start fails), so a crossing checked meanwhile anywhere in the process waits for it.
#[cfg(unix)]
#[derive(Debug)]
pub struct Learning(());

#[cfg(unix)]
impl Drop for Learning {
  fn drop(&mut self) {
    LEARNING.fetch_sub(1, Ordering::AcqRel);
  }
}

/// A shard's [`Learning`], taken before it starts; a check waits at most `budget_ns` for every shard's (the daemon's
/// liveness budget: a start that takes longer is a daemon the anchor replaces).
#[cfg(unix)]
pub fn learning(budget_ns: u64) -> Learning {
  LEARNING_BUDGET_NS.fetch_max(budget_ns, Ordering::AcqRel);
  LEARNING.fetch_add(1, Ordering::AcqRel);
  Learning(())
}

/// Waits until no shard is learning its kept mounts, so a crossing is judged against every mount this daemon serves;
/// refused `EAGAIN` past the budget, never judged against a partial record.
#[cfg(unix)]
fn await_learned() -> Result<(), HostError> {
  if LEARNING.load(Ordering::Acquire) == 0 {
    return Ok(());
  }
  let budget = std::time::Duration::from_nanos(LEARNING_BUDGET_NS.load(Ordering::Acquire));
  let started = std::time::Instant::now();
  while LEARNING.load(Ordering::Acquire) > 0 {
    if started.elapsed() >= budget {
      return Err(HostError::Unavailable(
        rustix::io::Errno::AGAIN.raw_os_error(),
      ));
    }
    std::thread::yield_now();
  }
  Ok(())
}

/// The refusal of a path on this daemon's own mount, with the errno the platform names it by: a deadlock avoided.
pub fn refusal() -> HostError {
  HostError::OwnMount(deadlock_errno())
}

#[cfg(unix)]
fn deadlock_errno() -> i32 {
  rustix::io::Errno::DEADLK.raw_os_error()
}

/// Format: Windows' `ERROR_POSSIBLE_DEADLOCK` (winerror.h), its name for the same refusal.
#[cfg(not(unix))]
const ERROR_POSSIBLE_DEADLOCK: i32 = 1131;

#[cfg(not(unix))]
fn deadlock_errno() -> i32 {
  ERROR_POSSIBLE_DEADLOCK
}

/// Format: the bytes of a tag, drawn from the system's random source.
#[cfg(target_os = "linux")]
const TAG_BYTES: usize = size_of::<u64>();

#[cfg(target_os = "linux")]
fn fresh_tag() -> Result<u64, HostError> {
  let source = rustix::fs::open(
    "/dev/urandom",
    rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
    rustix::fs::Mode::empty(),
  )
  .map_err(|e| HostError::Unavailable(e.raw_os_error()))?;
  let mut bytes = [0u8; TAG_BYTES];
  let read =
    rustix::io::read(&source, &mut bytes).map_err(|e| HostError::Unavailable(e.raw_os_error()))?;
  if read != TAG_BYTES {
    return Err(HostError::Unavailable(rustix::io::Errno::IO.raw_os_error()));
  }
  Ok(u64::from_le_bytes(bytes))
}

/// Elsewhere no fsname carries a tag (A-227: macOS names the export's host and port, Windows its volume's device).
#[cfg(not(target_os = "linux"))]
fn fresh_tag() -> Result<u64, HostError> {
  Ok(0)
}

/// Format: FNV-1a's 64-bit offset basis and prime (Fowler, Noll, Vo; RFC draft-eastlake-fnv), over a volume name's
/// UTF-16 units.
#[cfg(windows)]
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// Format: see [`FNV_OFFSET`].
#[cfg(windows)]
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The id a WinFsp volume's kernel name (`\Device\Volume{…}`, up to its first NUL) is recorded under in the set
/// (A-227): FNV-1a of its units, never zero. Two names that hash alike can only refuse a path wrongly, never admit
/// one on this daemon's own volume.
#[cfg(windows)]
pub fn volume_id(name: &[u16]) -> u64 {
  let hash = name
    .iter()
    .take_while(|unit| **unit != 0)
    .fold(FNV_OFFSET, |hash, unit| {
      (hash ^ u64::from(*unit)).wrapping_mul(FNV_PRIME)
    });
  hash.max(1)
}

#[cfg(target_os = "linux")]
pub use linux::{check_child, check_child_in, mount_id_at, mount_id_of, open_checked_root};

#[cfg(target_os = "linux")]
mod linux {
  use super::{SET, await_learned, fsname, recorded, refusal};
  use rustix::fs::{AtFlags, FileType, Mode, OFlags};
  use slates_vfs::host::HostError;
  use std::cell::RefCell;
  use std::ffi::{OsStr, OsString};
  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
  use std::os::unix::ffi::OsStrExt;
  use std::path::Path;

  /// Shape: the foreign mounts one thread remembers having looked up, so a listing of a directory with many mounts
  /// under it reads the kernel's table once a mount, not once an entry.
  const FOREIGN_REMEMBERED: usize = 64;

  /// Derived: the most symbolic links a root path's walk follows, the kernel's own bound (`MAXSYMLINKS`, 40,
  /// `include/linux/namei.h`): a path the kernel would refuse `ELOOP` is refused the same.
  const MAX_SYMLINKS: usize = 40;

  /// Format: enough of an fdinfo file to hold its `mnt_id:` line, which follows `pos:` and `flags:` (fs/proc/fd.c).
  const FDINFO_BYTES: usize = 256;

  /// Format: the base of the mount table's escapes.
  const OCTAL: u32 = 8;

  /// Format: the mount point is the fifth field of a mountinfo line, four past the mount id (proc(5): parent id,
  /// device, root, mount point).
  const POINT_AFTER_ID: usize = 3;

  /// Format: an escape's backslash and its three octal digits.
  const ESCAPE_BYTES: usize = 4;

  thread_local! {
    /// Mount ids this thread found in the kernel's table and not this daemon's, newest last.
    static FOREIGN: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
  }

  fn unavailable(e: rustix::io::Errno) -> HostError {
    match e {
      rustix::io::Errno::NOENT => HostError::NotFound,
      rustix::io::Errno::NOTDIR => HostError::NotDirectory,
      other => HostError::Unavailable(other.raw_os_error()),
    }
  }

  /// The mount id of the open `fd`, from `/proc/self/fdinfo/<fd>` (procfs: no filesystem code runs).
  pub fn mount_id_of(fd: BorrowedFd<'_>) -> Result<u64, HostError> {
    use std::os::fd::AsRawFd;
    let path = format!("/proc/self/fdinfo/{}", fd.as_raw_fd());
    let info = rustix::fs::open(
      path.as_str(),
      OFlags::RDONLY | OFlags::CLOEXEC,
      Mode::empty(),
    )
    .map_err(unavailable)?;
    let mut text = [0u8; FDINFO_BYTES];
    let read = rustix::io::read(&info, &mut text).map_err(unavailable)?;
    let text = std::str::from_utf8(text.get(..read).unwrap_or_default()).unwrap_or("");
    text
      .lines()
      .find_map(|line| line.strip_prefix("mnt_id:"))
      .and_then(|id| id.trim().parse::<u64>().ok())
      .ok_or(HostError::Unavailable(
        rustix::io::Errno::NOSYS.raw_os_error(),
      ))
  }

  /// Whether mount `id` is this daemon's: recorded, or (not yet recorded) named by its tag in the kernel's table.
  fn is_own(id: u64) -> Result<bool, HostError> {
    await_learned()?;
    if SET.get().is_some_and(|set| recorded(set, id)) {
      return Ok(true);
    }
    if FOREIGN.with_borrow(|foreign| foreign.contains(&id)) {
      return Ok(false);
    }
    let Some(own) = fsname() else {
      return Ok(false);
    };
    let tagged = table_entry(|entry| entry.id == id).is_some_and(|entry| entry.source == own);
    if !tagged {
      FOREIGN.with_borrow_mut(|foreign| {
        if foreign.len() >= FOREIGN_REMEMBERED {
          foreign.remove(0);
        }
        foreign.push(id);
      });
    }
    Ok(tagged)
  }

  /// One line of `/proc/self/mountinfo` (proc(5)): the mount's id, where it is mounted, and its source.
  struct Entry {
    id: u64,
    point: String,
    source: String,
  }

  /// The last entry of the kernel's mount table that `wanted` takes (the newest mount over a point is listed last).
  fn table_entry(wanted: impl Fn(&Entry) -> bool) -> Option<Entry> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    text
      .lines()
      .filter_map(parse)
      .filter(|entry| wanted(entry))
      .last()
  }

  fn parse(line: &str) -> Option<Entry> {
    let mut fields = line.split(' ');
    let id = fields.next()?.parse::<u64>().ok()?;
    let point = unescape(fields.nth(POINT_AFTER_ID)?);
    // The optional fields end at a lone `-`; then the type and the source.
    let mut rest = fields.skip_while(|field| *field != "-");
    rest.next()?;
    rest.next()?;
    let source = unescape(rest.next()?);
    Some(Entry { id, point, source })
  }

  /// A mount table field with its octal escapes (`\040` a space) undone.
  fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0usize;
    while let Some(&byte) = bytes.get(at) {
      let octal = bytes
        .get(at.saturating_add(1)..at.saturating_add(ESCAPE_BYTES))
        .and_then(|digits| std::str::from_utf8(digits).ok())
        .and_then(|digits| u8::from_str_radix(digits, OCTAL).ok());
      match (byte, octal) {
        (b'\\', Some(value)) => {
          out.push(value);
          at = at.saturating_add(ESCAPE_BYTES);
        }
        _ => {
          out.push(byte);
          at = at.saturating_add(1);
        }
      }
    }
    String::from_utf8_lossy(&out).into_owned()
  }

  /// The id of the newest mount at `point` whose source `wanted` takes: how a mount just made (its source this daemon's
  /// [`fsname`]) or adopted (a predecessor's tag) is named for [`super::record`].
  pub fn mount_id_at(point: &str, wanted: impl Fn(&str) -> bool) -> Option<u64> {
    table_entry(|entry| entry.point == point && wanted(&entry.source)).map(|entry| entry.id)
  }

  /// Refuses [`refusal`] when the entry `name` in `dir` lies on one of this daemon's own mounts, before anything opens
  /// it: the entry is opened `O_PATH | O_NOFOLLOW` (no filesystem code runs) and its mount compared with `dir`'s. An entry
  /// that is not there is no crossing; the caller's own open reports it.
  pub fn check_child(dir: BorrowedFd<'_>, name: &OsStr) -> Result<(), HostError> {
    check_child_in(dir, mount_id_of(dir)?, name)
  }

  /// [`check_child`] for a directory whose mount id the caller already holds (read once when it opened `dir`), so a
  /// lookup costs one `O_PATH` open and one fdinfo read.
  pub fn check_child_in(
    dir: BorrowedFd<'_>,
    dir_mount: u64,
    name: &OsStr,
  ) -> Result<(), HostError> {
    let Some(child) = open_path(dir, name)? else {
      return Ok(());
    };
    let below = mount_id_of(child.as_fd())?;
    if below != dir_mount && is_own(below)? {
      return Err(refusal());
    }
    Ok(())
  }

  /// The entry opened `O_PATH | O_NOFOLLOW`, or `None` when it is not there.
  fn open_path(dir: BorrowedFd<'_>, name: &OsStr) -> Result<Option<OwnedFd>, HostError> {
    match rustix::fs::openat(
      dir,
      name,
      OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
      Mode::empty(),
    ) {
      Ok(fd) => Ok(Some(fd)),
      Err(rustix::io::Errno::NOENT) => Ok(None),
      Err(e) => Err(unavailable(e)),
    }
  }

  /// Refuses when `child` is on a mount other than `parent`'s that is this daemon's own.
  fn crossing(parent: BorrowedFd<'_>, child: BorrowedFd<'_>) -> Result<(), HostError> {
    let below = mount_id_of(child)?;
    if below != mount_id_of(parent)? && is_own(below)? {
      return Err(refusal());
    }
    Ok(())
  }

  /// Opens the directory at `path` with `flags`, walking every component first with the crossing check, so no
  /// component on one of this daemon's own mounts is ever entered. Symbolic links along the way are followed by hand
  /// within the kernel's bound; the last component is opened with `flags` itself (`O_NOFOLLOW` refuses a final link).
  pub fn open_checked_root(path: &Path, flags: OFlags) -> Result<OwnedFd, HostError> {
    let (parent, last) = walk(path)?;
    match last {
      Some(last) => {
        check_child(parent.as_fd(), &last)?;
        rustix::fs::openat(&parent, last.as_os_str(), flags, Mode::empty()).map_err(unavailable)
      }
      None => rustix::fs::openat(&parent, c".", flags, Mode::empty()).map_err(unavailable),
    }
  }

  /// Resolves `path` component by component up to its last: the directory holding the last component (opened
  /// `O_PATH`) and that component's name (`None` for a path that ends at a directory itself, `/` or `.`).
  fn walk(path: &Path) -> Result<(OwnedFd, Option<OsString>), HostError> {
    let mut at = open_start(path.is_absolute())?;
    let mut pending = components(path);
    pending.reverse();
    let mut followed = 0usize;
    while let Some(name) = pending.pop() {
      if pending.is_empty() {
        return Ok((at, Some(name)));
      }
      let Some(child) = open_path(at.as_fd(), &name)? else {
        return Err(HostError::NotFound);
      };
      crossing(at.as_fd(), child.as_fd())?;
      if is_symlink(child.as_fd())? {
        followed = followed.saturating_add(1);
        at = follow(at, &child, &mut pending, followed)?;
        continue;
      }
      at = child;
    }
    Ok((at, None))
  }

  fn open_start(absolute: bool) -> Result<OwnedFd, HostError> {
    let start = if absolute { c"/" } else { c"." };
    rustix::fs::open(
      start,
      OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
      Mode::empty(),
    )
    .map_err(unavailable)
  }

  /// The components of `path` that name an entry (`.` dropped; `..` kept, which the kernel resolves).
  fn components(path: &Path) -> Vec<OsString> {
    path
      .components()
      .filter_map(|component| match component {
        std::path::Component::Normal(name) => Some(name.to_os_string()),
        std::path::Component::ParentDir => Some(OsString::from("..")),
        _ => None,
      })
      .collect()
  }

  fn is_symlink(fd: BorrowedFd<'_>) -> Result<bool, HostError> {
    let stat = rustix::fs::statat(fd, c"", AtFlags::EMPTY_PATH | AtFlags::SYMLINK_NOFOLLOW)
      .map_err(unavailable)?;
    Ok(FileType::from_raw_mode(stat.st_mode) == FileType::Symlink)
  }

  /// Puts the link `link`'s target in front of the components still to walk: from `/` for an absolute target, from the
  /// link's own directory `at` otherwise. Refused `ELOOP` past [`MAX_SYMLINKS`].
  fn follow(
    at: OwnedFd,
    link: &OwnedFd,
    pending: &mut Vec<OsString>,
    followed: usize,
  ) -> Result<OwnedFd, HostError> {
    if followed > MAX_SYMLINKS {
      return Err(HostError::Unavailable(
        rustix::io::Errno::LOOP.raw_os_error(),
      ));
    }
    let target = rustix::fs::readlinkat(link, c"", Vec::new()).map_err(unavailable)?;
    let target = Path::new(OsStr::from_bytes(target.as_bytes()));
    let mut parts = components(target);
    parts.reverse();
    pending.extend(parts);
    if target.is_absolute() {
      return open_start(true);
    }
    Ok(at)
  }

  #[cfg(test)]
  mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A-227. Do: parse a mount table line with optional fields and an escaped mount point. Expect: its id, its point
    /// with the space restored, and its source.
    #[test]
    fn a_mount_table_line_names_its_id_point_and_source() {
      let line = "712 30 0:114 / /home/u/my\\040mount rw,nosuid shared:5 - fuse.slates slates:00ab rw,user_id=1000";
      let entry = parse(line).unwrap();
      assert_eq!(
        (entry.id, entry.point.as_str(), entry.source.as_str()),
        (712, "/home/u/my mount", "slates:00ab")
      );
    }

    /// A-227. Do: read this process's root directory's mount id, check an entry below it, and open a directory through
    /// the checked walk. Expect: the ids read from procfs, and no path refused where this process has no mount.
    #[test]
    fn a_path_on_no_mount_of_this_daemons_is_admitted() {
      let root = open_start(true).unwrap();
      assert!(mount_id_of(root.as_fd()).unwrap() > 0);
      assert!(check_child(root.as_fd(), OsStr::new("tmp")).is_ok());
      assert!(open_checked_root(Path::new("/tmp"), OFlags::RDONLY | OFlags::DIRECTORY).is_ok());
    }
  }
}

#[cfg(target_os = "macos")]
pub use macos::{check_child, mount_id_at, open_checked_root};

#[cfg(target_os = "macos")]
mod macos {
  use super::{SET, await_learned, recorded, refusal};
  use rustix::fs::{AtFlags, FileType, Mode, OFlags};
  use slates_vfs::host::HostError;
  use std::cell::RefCell;
  use std::ffi::{OsStr, OsString};
  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
  use std::os::unix::ffi::OsStrExt;
  use std::path::Path;
  use std::sync::atomic::Ordering;

  /// Format: the filesystem type the macOS NFS client records for a mount (`statfs.f_fstypename`).
  const NFS_FSTYPE: &str = "nfs";

  /// Derived: the most symbolic links a root path's walk follows, the kernel's own bound (`MAXSYMLINKS`, 32,
  /// `sys/param.h`): a path the kernel would refuse `ELOOP` is refused the same.
  const MAX_SYMLINKS: usize = 32;

  thread_local! {
    /// The mount points of this daemon's own mounts as the kernel's table names them, with the set's generation they
    /// were read at.
    static POINTS: RefCell<(u64, Vec<String>)> = const { RefCell::new((u64::MAX, Vec::new())) };
  }

  fn unavailable(e: rustix::io::Errno) -> HostError {
    match e {
      rustix::io::Errno::NOENT => HostError::NotFound,
      rustix::io::Errno::NOTDIR => HostError::NotDirectory,
      other => HostError::Unavailable(other.raw_os_error()),
    }
  }

  /// The id a mount's table entry is recorded under: its filesystem id's two words.
  fn id_of(entry: &slates_bridge_oci::mount_table::MountEntry) -> u64 {
    (entry.identity.device << u32::BITS) | (entry.identity.mount & u64::from(u32::MAX))
  }

  /// The id of the NFS mount at exactly `point` (the kernel's own path) whose source `wanted` takes, from the kernel's
  /// table without contacting any filesystem: how a mount the CLI bound, or a successor adopts, is named for
  /// [`super::record`].
  pub fn mount_id_at(point: &str, wanted: impl Fn(&str) -> bool) -> Option<u64> {
    let table = slates_bridge_oci::mount_table::mount_table().ok()?;
    table
      .iter()
      .rev()
      .find(|entry| {
        entry.mount_point == point && entry.fstype == NFS_FSTYPE && wanted(&entry.source)
      })
      .map(id_of)
  }

  /// Lends the mount points of this daemon's own mounts to `judge`, read from the kernel's table again only when the set
  /// has changed since this thread last read them.
  fn with_own_points<R>(judge: impl FnOnce(&[String]) -> R) -> R {
    let Some(set) = SET.get() else {
      return judge(&[]);
    };
    let generation = set.generation.load(Ordering::Acquire);
    POINTS.with_borrow_mut(|(seen, points)| {
      if *seen != generation {
        *points = slates_bridge_oci::mount_table::mount_table()
          .map(|table| {
            table
              .iter()
              .filter(|entry| recorded(set, id_of(entry)))
              .map(|entry| entry.mount_point.clone())
              .collect()
          })
          .unwrap_or_default();
        *seen = generation;
      }
      judge(points)
    })
  }

  /// `name` in the directory the kernel names `parent`.
  fn joined(parent: &str, name: &OsStr) -> String {
    let separator = if parent.ends_with('/') { "" } else { "/" };
    format!("{parent}{separator}{}", name.to_string_lossy())
  }

  /// Refuses [`refusal`] when the entry `name` in `dir` is one of this daemon's own mount points, before anything opens
  /// it: the kernel's path of `dir` (on a filesystem that is not ours, since every crossing to it was checked) with
  /// `name`, compared exactly with the kernel's mount point of each own mount.
  pub fn check_child(dir: BorrowedFd<'_>, name: &OsStr) -> Result<(), HostError> {
    await_learned()?;
    with_own_points(|points| {
      if points.is_empty() {
        return Ok(());
      }
      let parent = rustix::fs::getpath(dir).map_err(unavailable)?;
      let candidate = joined(&parent.to_string_lossy(), name);
      if points.contains(&candidate) {
        return Err(refusal());
      }
      Ok(())
    })
  }

  /// Opens the directory at `path` with `flags`, every component checked first, so no component that is one of this
  /// daemon's own mount points is ever entered; symbolic links along the way are followed by hand within the kernel's
  /// bound, and the last component is opened with `flags` itself.
  pub fn open_checked_root(path: &Path, flags: OFlags) -> Result<OwnedFd, HostError> {
    let mut at = open_start(path.is_absolute())?;
    let mut pending = components(path);
    pending.reverse();
    let mut followed = 0usize;
    while let Some(name) = pending.pop() {
      check_child(at.as_fd(), &name)?;
      if pending.is_empty() {
        return rustix::fs::openat(&at, name.as_os_str(), flags, Mode::empty())
          .map_err(unavailable);
      }
      let stat = rustix::fs::statat(&at, name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)
        .map_err(unavailable)?;
      if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
        followed = followed.saturating_add(1);
        if followed > MAX_SYMLINKS {
          return Err(HostError::Unavailable(
            rustix::io::Errno::LOOP.raw_os_error(),
          ));
        }
        let target =
          rustix::fs::readlinkat(&at, name.as_os_str(), Vec::new()).map_err(unavailable)?;
        let target = Path::new(OsStr::from_bytes(target.as_bytes()));
        let mut parts = components(target);
        parts.reverse();
        pending.extend(parts);
        if target.is_absolute() {
          at = open_start(true)?;
        }
        continue;
      }
      at = rustix::fs::openat(
        &at,
        name.as_os_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
      )
      .map_err(unavailable)?;
    }
    rustix::fs::openat(&at, c".", flags, Mode::empty()).map_err(unavailable)
  }

  fn open_start(absolute: bool) -> Result<OwnedFd, HostError> {
    let start = if absolute { c"/" } else { c"." };
    rustix::fs::open(
      start,
      OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
      Mode::empty(),
    )
    .map_err(unavailable)
  }

  /// The components of `path` that name an entry (`.` dropped; `..` kept, which the kernel resolves).
  fn components(path: &Path) -> Vec<OsString> {
    path
      .components()
      .filter_map(|component| match component {
        std::path::Component::Normal(name) => Some(name.to_os_string()),
        std::path::Component::ParentDir => Some(OsString::from("..")),
        _ => None,
      })
      .collect()
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]
  use super::*;

  /// A-227. Do: record two mounts in a set of two (one of them twice), a third, then forget one and record the third
  /// again. Expect: the repeat recorded once, the third refused `Full` until a slot is freed, then taken.
  #[test]
  fn the_set_records_its_own_mounts_up_to_its_bound() {
    let set = Set {
      slots: (0..2).map(|_| AtomicU64::new(0)).collect(),
      high: AtomicUsize::new(0),
      generation: AtomicU64::new(0),
    };
    record_into(&set, 7).unwrap();
    record_into(&set, 9).unwrap();
    record_into(&set, 9).unwrap();
    assert_eq!(record_into(&set, 11), Err(Full { capacity: 2 }));
    assert!(recorded(&set, 7) && recorded(&set, 9) && !recorded(&set, 11));
    forget_in(&set, 7);
    assert!(!recorded(&set, 7));
    record_into(&set, 11).unwrap();
    assert!(recorded(&set, 11));
  }
}

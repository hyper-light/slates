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
//! Elsewhere the set holds nothing yet and no path is refused: macOS and Windows identify their mounts their own way
//! (A-227), each in its own change.

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
    })
    .is_ok();
  let _ = TAG.set(tag);
  Ok(made)
}

/// The source a FUSE mount this daemon makes names (`slates:<tag>`), `None` before [`install`].
pub fn fsname() -> Option<String> {
  TAG.get().map(|tag| format!("{SOURCE_PREFIX}{tag:016x}"))
}

/// Records `id` as one of this daemon's own mounts; refused [`Full`] at the bound, and nothing recorded. An id already
/// recorded is recorded once. Before [`install`] nothing is recorded (a process that serves no mounts).
pub fn record(id: u64) -> Result<(), Full> {
  SET.get().map_or(Ok(()), |set| record_into(set, id))
}

fn record_into(set: &Set, id: u64) -> Result<(), Full> {
  if id == 0 || recorded(set, id) {
    return Ok(());
  }
  for (at, slot) in set.slots.iter().enumerate() {
    if slot
      .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      set.high.fetch_max(at.saturating_add(1), Ordering::AcqRel);
      return Ok(());
    }
  }
  Err(Full {
    capacity: set.slots.len(),
  })
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

#[cfg(target_os = "linux")]
pub use linux::{check_child, mount_id_at, mount_id_of, open_checked_root};

#[cfg(target_os = "linux")]
mod linux {
  use super::{SET, fsname, recorded, refusal};
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
  fn is_own(id: u64) -> bool {
    if SET.get().is_some_and(|set| recorded(set, id)) {
      return true;
    }
    if FOREIGN.with_borrow(|foreign| foreign.contains(&id)) {
      return false;
    }
    let Some(own) = fsname() else {
      return false;
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
    tagged
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
    let point = unescape(fields.nth(3)?);
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
    let Some(child) = open_path(dir, name)? else {
      return Ok(());
    };
    crossing(dir, child.as_fd())
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
    if below != mount_id_of(parent)? && is_own(below) {
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

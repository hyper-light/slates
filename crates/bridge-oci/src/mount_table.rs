//! The kernel's mount table, read as a query that never touches a mount (§4.6 A-9; the crate doc
//! states why a `statfs` of the path would deadlock a single-shard daemon). One seam,
//! [`mount_table`], with a paired implementation per platform: macOS asks `getfsstat(MNT_NOWAIT)`
//! for the kernel's cached table; Linux reads `/proc/self/mountinfo`, the kernel's own table, through
//! a read-only descriptor; every other platform is refused typed. The parser of the Linux text is pure
//! and tested on every host.
//!
//! The table's changes come through a second seam, [`UnmountWatch`] (A-511): a descriptor that reads ready once the
//! kernel has posted that a filesystem left the table, so a daemon learns of an unmount by the kernel's own event and
//! reads the table once for it, never polling it and never trusting a protocol's hint (an NFS `UMNT` is one any local
//! process can send, and the kernel sends none at a forced unmount).

use std::fmt;

/// The kernel's identity of one mount instance (AUD-29-66): on Linux the mount's id and its device numbers
/// (`mountinfo`'s first and third fields), on macOS the filesystem id `statfs` reports. A mount replaced at
/// the same path — unmounted and mounted again, or another filesystem mounted over it — has another identity,
/// so a harness that checks it just before its runtime binds the path does not bind a replacement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MountIdentity {
  /// The mount's id (Linux) or the filesystem id's second word (macOS).
  pub mount: u64,
  /// The mount's device: `major << 32 | minor` (Linux) or the filesystem id's first word (macOS).
  pub device: u64,
}

/// One mounted filesystem as the kernel lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
  /// The kernel's identity of this mount instance.
  pub identity: MountIdentity,
  /// Where it is mounted (the real path the kernel resolved at mount time).
  pub mount_point: String,
  /// Its filesystem type (`nfs`, `apfs`, `fuse.slates`, `ext4`, ...).
  pub fstype: String,
  /// Its source (`slates:/<name>`, `/dev/disk3s1`, `slates`, ...).
  pub source: String,
}

/// Why the table could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountTableError {
  /// No table query is built for this platform.
  Unsupported {
    /// The platform.
    platform: &'static str,
  },
  /// The kernel refused the query.
  Query {
    /// The errno.
    errno: i32,
  },
  /// The kernel's text did not have the documented shape at this line (1-based).
  Malformed {
    /// The line.
    line: usize,
  },
  /// The table exceeded its derived bound.
  TooLarge {
    /// The bound in bytes.
    cap: usize,
  },
  /// The kernel does not report a mount id it never reuses, so no identity here can tell one mount instance
  /// from the next at the same path (Linux before 6.8, which added `STATX_MNT_ID_UNIQUE`).
  NoUniqueMountId,
}

impl fmt::Display for MountTableError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Unsupported { platform } => write!(f, "no mount-table query on {platform}"),
      Self::Query { errno } => write!(f, "the kernel refused the mount-table query: errno {errno}"),
      Self::Malformed { line } => write!(f, "the mount table is malformed at line {line}"),
      Self::TooLarge { cap } => write!(f, "the mount table exceeds its bound of {cap} bytes"),
      Self::NoUniqueMountId => write!(
        f,
        "this kernel does not report a mount id it never reuses, so a replaced mount cannot be told apart"
      ),
    }
  }
}

impl std::error::Error for MountTableError {}

impl MountTableError {
  /// The errno behind the error, 0 when there is none.
  pub const fn errno(&self) -> i32 {
    match self {
      Self::Query { errno } => *errno,
      Self::Unsupported { .. }
      | Self::Malformed { .. }
      | Self::TooLarge { .. }
      | Self::NoUniqueMountId => 0,
    }
  }
}

/// Every mounted filesystem, in mount order (a later mount at the same path shadows an earlier one).
#[cfg(target_os = "macos")]
pub fn mount_table() -> Result<Vec<MountEntry>, MountTableError> {
  macos::query()
}

/// Every mounted filesystem, in mount order (a later mount at the same path shadows an earlier one).
#[cfg(target_os = "linux")]
pub fn mount_table() -> Result<Vec<MountEntry>, MountTableError> {
  parse_mountinfo(&linux::read_mountinfo()?)
}

/// Every mounted filesystem: no query is built here.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn mount_table() -> Result<Vec<MountEntry>, MountTableError> {
  Err(MountTableError::Unsupported {
    platform: std::env::consts::OS,
  })
}

/// The kernel's identity of the mount at `path`, read now (AUD-29-66) — the one a harness compares with the
/// identity its attach recorded, so it never binds a replacement.
///
/// This is a seam of its own rather than a field of [`mount_table`] because the identity that survives the
/// comparison is not in the table. `mountinfo`'s mount id is reused: the kernel allocates it from an IDA that
/// hands back the lowest free number, so unmounting a mount and mounting another at the same path reproduces it
/// — measured 927 for both a victim and an attacker mount on kernel 7.2.9-2-cachyos (2026-10-17), first try,
/// with the FUSE superblock's anonymous device (0:114) reused with it. Linux 6.8 added `STATX_MNT_ID_UNIQUE` for
/// exactly this reason and it told the same two mounts apart (2156321564 against 2156321579), so that is what is
/// asked for here. Reading one path costs one `statx` against the table's whole-file read, and it is asked only
/// of the path being verified, never of every mount on the host.
#[cfg(target_os = "linux")]
pub fn mount_identity_at(path: &str) -> Result<MountIdentity, MountTableError> {
  linux::identity_at(path)
}

/// The kernel's identity of the mount at `path`, read now: on macOS the `statfs` filesystem id the table
/// already carries, found by mount point. Whether `f_fsid` is reused across an unmount and a remount is not
/// established here, so this is the table's own answer and claims nothing more.
#[cfg(target_os = "macos")]
pub fn mount_identity_at(path: &str) -> Result<MountIdentity, MountTableError> {
  let wanted = path.trim_end_matches('/');
  let wanted = if wanted.is_empty() { "/" } else { wanted };
  mount_table()?
    .iter()
    .rev()
    .find(|entry| {
      let point = entry.mount_point.trim_end_matches('/');
      (if point.is_empty() { "/" } else { point }) == wanted
    })
    .map(|entry| entry.identity)
    .ok_or(MountTableError::Query { errno: 0 })
}

/// The kernel's identity of the mount at `path`: no query is built here.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn mount_identity_at(_path: &str) -> Result<MountIdentity, MountTableError> {
  Err(MountTableError::Unsupported {
    platform: std::env::consts::OS,
  })
}

/// A watch of the kernel's unmounts (A-511, §4.6): a descriptor that reads ready once the kernel has posted that a
/// filesystem left its mount table. A shard's readiness wait registers [`UnmountWatch::descriptor`]
/// (`slates_rt::readiness::readable`, A-149), and [`UnmountWatch::take`] empties what was posted, so the descriptor
/// reads ready again only at the next change. Which filesystem left is not said: the caller reads the table once
/// ([`mount_table`]) and judges which of its mounts are gone. One seam, paired per platform:
///
/// - **macOS.** A kqueue holding one `EVFILT_FS` filter (`sys/event.h`) on `VQ_UNMOUNT`, "filesystem has left"
///   (`sys/mount.h`). Apple's kqueue(2) does not document the filter (it lists `EVFILT_READ` through `EVFILT_TIMER`),
///   so its behaviour is read from XNU's source (`bsd/vfs/vfs_subr.c`, read 2026-11-18): the filter watches the
///   whole system, not a descriptor (`filt_fsattach` hangs it on the one list `vfs_event_signal` posts to); it sees
///   only events after it is attached and clears itself once read (`EV_CLEAR`, set by the attach); its flag word
///   picks the events it fires on (`filt_fsevent`). XNU's `dounmount` (`bsd/vfs/vfs_syscalls.c`) takes the mount off
///   the list `getfsstat` reads (`mount_list_remove`) before it posts `VQ_UNMOUNT`, so a table read after the event
///   never still names the mount that left. A kqueue's own descriptor reads ready while the queue holds an event
///   (`bsd/kern/kern_event.c`, `filt_kqueue`), and that is the edge a shard's own kqueue waits for.
/// - **Linux.** An epoll instance holding `/proc/self/mountinfo` (proc_pid_mountinfo(5)) for `EPOLLPRI`. The file
///   is pollable as `/proc/pid/mounts` is: "a change in this file (i.e., a filesystem mount or unmount) causes …
///   poll(2) and epoll_wait(2) [to] mark the file as having a priority event (POLLPRI)" (proc_pid_mounts(5)); the
///   two files share one poll function (`fs/proc_namespace.c`, `mounts_poll`, read 2026-11-18). That function reports
///   the file readable always, so a readable wait on the file itself would fire at once, every time; it adds
///   `EPOLLPRI | EPOLLERR` once per change of the mount namespace since the open file was last polled, and the poll
///   itself consumes the change, not a read. The instance's own descriptor reads ready while the file has such a
///   change (epoll(7): an epoll descriptor is itself pollable), so the shard's epoll waits on the instance, never on
///   the file. Every change posts, a mount as well as an unmount; the caller's judgment finds nothing gone then.
/// - **Elsewhere** the watch is refused typed, as the table is.
#[derive(Debug)]
pub struct UnmountWatch {
  queue: watch::Queue,
}

impl UnmountWatch {
  /// Opens the watch: the kernel's queue and its one filter, registered now. Only changes after this call are posted,
  /// so a caller reads the table once after opening for anything that left before.
  pub fn open() -> Result<UnmountWatch, MountTableError> {
    watch::open().map(|queue| UnmountWatch { queue })
  }

  /// The descriptor that reads ready once a change was posted: the queue's own, never the table's.
  pub fn descriptor(&self) -> i32 {
    watch::descriptor(&self.queue)
  }

  /// Takes what was posted, without waiting, so the descriptor reads ready again only at the next change.
  pub fn take(&mut self) -> Result<(), MountTableError> {
    watch::take(&mut self.queue)
  }
}

#[cfg(target_os = "macos")]
use self::macos as watch;

#[cfg(target_os = "linux")]
use self::linux as watch;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use self::other as watch;

/// The unmount watch where no queue is built: [`UnmountWatch::open`] is refused, so no watch is ever made.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod other {
  use super::MountTableError;

  /// No queue exists here.
  #[derive(Debug)]
  pub(super) enum Queue {}

  pub(super) fn open() -> Result<Queue, MountTableError> {
    Err(MountTableError::Unsupported {
      platform: std::env::consts::OS,
    })
  }

  pub(super) fn descriptor(queue: &Queue) -> i32 {
    match *queue {}
  }

  pub(super) fn take(queue: &mut Queue) -> Result<(), MountTableError> {
    match *queue {}
  }
}

#[cfg(target_os = "macos")]
mod macos {
  use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

  use super::{MountEntry, MountTableError};

  /// Format: `VQ_UNMOUNT`, the vfsquery flag "filesystem has left" (`sys/mount.h`): the one event the unmount watch's
  /// filter fires on (XNU's `filt_fsevent` sets a flag only where the filter's own flag word has it).
  const VQ_UNMOUNT: u32 = 0x0010;

  /// Shape: the events one take reads. The queue holds one filter, and a cleared-on-read filter is one event however
  /// many unmounts it gathered (`filt_fsevent` ORs each into the event's flags).
  const POSTED_PER_TAKE: usize = 1;

  /// The unmount watch's queue: a kqueue holding the one `EVFILT_FS` filter ([`super::UnmountWatch`]).
  #[derive(Debug)]
  pub(super) struct Queue {
    queue: OwnedFd,
  }

  /// The filter's record with `flags`: `EVFILT_FS` on `VQ_UNMOUNT`. Its identifier is ignored (the filter watches the
  /// whole system, XNU `filt_fsattach`).
  fn unmount_filter(flags: u16) -> libc::kevent {
    libc::kevent {
      ident: 0,
      filter: libc::EVFILT_FS,
      flags,
      fflags: VQ_UNMOUNT,
      data: 0,
      udata: std::ptr::null_mut(),
    }
  }

  /// One `kevent(2)` on `queue` that waits for nothing: `changes` applied, then at most `posted.len()` posted events
  /// read into `posted`. How many were read, or the errno: with no room for an error record, a change the kernel
  /// refuses fails the whole call (kevent(2)).
  fn kevent_now(
    queue: BorrowedFd<'_>,
    changes: &[libc::kevent],
    posted: &mut [libc::kevent],
  ) -> Result<usize, MountTableError> {
    let too_many = || MountTableError::TooLarge {
      cap: usize::try_from(libc::c_int::MAX).unwrap_or(usize::MAX),
    };
    let changes_len = libc::c_int::try_from(changes.len()).map_err(|_| too_many())?;
    let posted_len = libc::c_int::try_from(posted.len()).map_err(|_| too_many())?;
    let now = libc::timespec {
      tv_sec: 0,
      tv_nsec: 0,
    };
    // SAFETY: `queue` is a live kqueue descriptor the watch owns for the call; `changes` holds `changes_len`
    // initialized records and `posted` has room for `posted_len`, both borrowed for the call, which reads the first
    // and writes at most `posted_len` whole records into the second (kevent(2)); `now` is a zero timeout, so the call
    // returns at once. rustix's `kevent` cannot name `EVFILT_FS` (its `EventFilter` has no such variant), so this is
    // the raw call.
    let read = unsafe {
      libc::kevent(
        queue.as_raw_fd(),
        changes.as_ptr(),
        changes_len,
        posted.as_mut_ptr(),
        posted_len,
        &now,
      )
    };
    usize::try_from(read).map_err(|_| MountTableError::Query {
      errno: last_errno(),
    })
  }

  /// A kqueue with the unmount filter registered on it.
  pub(super) fn open() -> Result<Queue, MountTableError> {
    let queue = rustix::event::kqueue::kqueue().map_err(|e| MountTableError::Query {
      errno: e.raw_os_error(),
    })?;
    kevent_now(queue.as_fd(), &[unmount_filter(libc::EV_ADD)], &mut [])?;
    Ok(Queue { queue })
  }

  /// The queue's own descriptor.
  pub(super) fn descriptor(queue: &Queue) -> i32 {
    queue.queue.as_raw_fd()
  }

  /// Reads the filter's posted event, if any, which clears it.
  pub(super) fn take(queue: &mut Queue) -> Result<(), MountTableError> {
    let mut posted = [unmount_filter(0); POSTED_PER_TAKE];
    kevent_now(queue.queue.as_fd(), &[], &mut posted).map(|_| ())
  }

  /// A NUL-terminated C string field of a `statfs` record as text, stopping at the first NUL. Each
  /// `c_char` is one byte, reinterpreted bit for bit (a path byte above 127 is a UTF-8 continuation
  /// byte, not a negative number).
  fn field(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars
      .iter()
      .map(|c| u8::from_ne_bytes(c.to_ne_bytes()))
      .take_while(|b| *b != 0)
      .collect();
    String::from_utf8_lossy(&bytes).into_owned()
  }

  fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
  }

  /// The table through `getfsstat(MNT_NOWAIT)`: the count first, then the records into a buffer
  /// sized for exactly that count.
  pub(super) fn query() -> Result<Vec<MountEntry>, MountTableError> {
    let record_len = size_of::<libc::statfs>();
    // SAFETY: `getfsstat(NULL, 0, MNT_NOWAIT)` writes nothing and returns the number of mounted
    // filesystems (getfsstat(2)); the second call is given a buffer this function owns with capacity
    // for `count` records and a byte size that is exactly `count * sizeof(statfs)`, so the kernel
    // writes at most that many records into memory that is ours; `set_len` is then called with the
    // number of records the kernel reports it wrote, never more than the capacity, and every record
    // is a plain C struct the kernel filled in full. `MNT_NOWAIT` asks for the cached table without
    // contacting any filesystem, so no mount served by this process is touched. Each record's `f_fsid` is
    // read as its two 32-bit words with `transmute_copy`: `fsid_t` is `#[repr(C)] { val: [i32; 2] }` (its
    // field private in `libc`), exactly the size and layout of `[i32; 2]`, and fully initialized by the kernel.
    let (records, fsids): (Vec<libc::statfs>, Vec<[i32; 2]>) = unsafe {
      let count = libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT);
      let count = usize::try_from(count).map_err(|_| MountTableError::Query {
        errno: last_errno(),
      })?;
      let bytes = count
        .checked_mul(record_len)
        .and_then(|b| libc::c_int::try_from(b).ok())
        .ok_or(MountTableError::TooLarge {
          cap: usize::try_from(libc::c_int::MAX).unwrap_or(usize::MAX),
        })?;
      let mut records: Vec<libc::statfs> = Vec::with_capacity(count);
      let written = libc::getfsstat(records.as_mut_ptr(), bytes, libc::MNT_NOWAIT);
      let written = usize::try_from(written).map_err(|_| MountTableError::Query {
        errno: last_errno(),
      })?;
      records.set_len(written.min(count));
      let fsids = records
        .iter()
        .map(|record| std::mem::transmute_copy::<libc::fsid_t, [i32; 2]>(&record.f_fsid))
        .collect();
      (records, fsids)
    };
    let word = |w: i32| u64::from(u32::from_ne_bytes(w.to_ne_bytes()));
    Ok(
      records
        .iter()
        .zip(fsids)
        .map(|(record, [first, second])| MountEntry {
          identity: super::MountIdentity {
            mount: word(second),
            device: word(first),
          },
          mount_point: field(&record.f_mntonname),
          fstype: field(&record.f_fstypename),
          source: field(&record.f_mntfromname),
        })
        .collect(),
    )
  }
}

#[cfg(target_os = "linux")]
mod linux {
  use std::os::fd::{AsRawFd, OwnedFd};

  use rustix::event::epoll;
  use rustix::fs::{Mode, OFlags};

  use super::MountTableError;

  /// Shape: the events one take reads: the instance holds one file.
  const POSTED_PER_TAKE: usize = 1;

  /// The unmount watch's queue ([`super::UnmountWatch`]): an epoll instance holding `/proc/self/mountinfo` for
  /// `EPOLLPRI`, and the open file it holds.
  #[derive(Debug)]
  pub(super) struct Queue {
    events: OwnedFd,
    /// Kept open for the registration: closing the file takes it out of the instance's interest list (epoll(7)).
    _table: OwnedFd,
  }

  /// An epoll instance with `/proc/self/mountinfo` registered for its priority event, level-triggered: the file's poll
  /// reports a change once and consumes it (`fs/proc_namespace.c`, `mounts_poll`), so the registration need not be
  /// edge-triggered to fire once per change.
  pub(super) fn open() -> Result<Queue, MountTableError> {
    let table = rustix::fs::open(MOUNTINFO, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
      .map_err(errno_of)?;
    let events = epoll::create(epoll::CreateFlags::CLOEXEC).map_err(errno_of)?;
    epoll::add(
      &events,
      &table,
      epoll::EventData::new_u64(0),
      epoll::EventFlags::PRI,
    )
    .map_err(errno_of)?;
    Ok(Queue {
      events,
      _table: table,
    })
  }

  /// The instance's own descriptor.
  pub(super) fn descriptor(queue: &Queue) -> i32 {
    queue.events.as_raw_fd()
  }

  /// Reads the file's posted change, if the instance still holds one, without waiting. A wait the kernel interrupted
  /// took nothing, and the next readiness wait finds the change still there.
  pub(super) fn take(queue: &mut Queue) -> Result<(), MountTableError> {
    let mut posted: Vec<epoll::Event> = Vec::with_capacity(POSTED_PER_TAKE);
    let now = rustix::event::Timespec {
      tv_sec: 0,
      tv_nsec: 0,
    };
    match epoll::wait(
      &queue.events,
      rustix::buffer::spare_capacity(&mut posted),
      Some(&now),
    ) {
      Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
      Err(e) => Err(errno_of(e)),
    }
  }

  /// Format: the kernel's table of this process's mount namespace (proc(5)).
  const MOUNTINFO: &str = "/proc/self/mountinfo";
  /// Format: the kernel's bound on mounts per namespace (`fs.mount-max`, proc(5); Linux 4.9+, under
  /// the 5.10 floor).
  const MOUNT_MAX: &str = "/proc/sys/fs/mount-max";
  /// Format: `PATH_MAX` on Linux (limits.h): the bound on each of a line's three path fields.
  const PATH_MAX: usize = 4096;
  /// Derived: a `mountinfo` line holds three paths (the root, the mount point, the source) and the
  /// fixed-width ids, options and type fields; four `PATH_MAX` bounds all of them.
  const LINE_CAP: usize = 4 * PATH_MAX;

  /// Format: `STATX_MNT_ID_UNIQUE` (Linux 6.8, `include/uapi/linux/stat.h`) — the mask bit that asks for the
  /// mount id the kernel never reuses. rustix 1.1.5 names `STATX_MNT_ID` (5.8) but not this one.
  const STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;

  fn errno_of(e: rustix::io::Errno) -> MountTableError {
    MountTableError::Query {
      errno: e.raw_os_error(),
    }
  }

  /// One `statx` for the unique mount id of `path`. `AT_STATX_DONT_SYNC` asks the kernel not to go to the
  /// filesystem for freshness, so a mount whose attributes are cached answers without a round trip to its
  /// daemon. A kernel that does not set the mask bit is refused rather than answered from the reusable id.
  pub(super) fn identity_at(path: &str) -> Result<super::MountIdentity, MountTableError> {
    let answer = rustix::fs::statx(
      rustix::fs::CWD,
      path,
      rustix::fs::AtFlags::STATX_DONT_SYNC,
      rustix::fs::StatxFlags::from_bits_retain(STATX_MNT_ID_UNIQUE),
    )
    .map_err(errno_of)?;
    if answer.stx_mask & STATX_MNT_ID_UNIQUE == 0 {
      return Err(MountTableError::NoUniqueMountId);
    }
    Ok(super::MountIdentity {
      mount: answer.stx_mnt_id,
      device: (u64::from(answer.stx_dev_major) << super::MAJOR_SHIFT)
        | u64::from(answer.stx_dev_minor),
    })
  }

  /// Reads a pseudo-file whole through a read-only descriptor, refusing past `cap` bytes.
  fn read_pseudo_file(path: &str, cap: usize) -> Result<String, MountTableError> {
    let fd =
      rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).map_err(errno_of)?;
    let mut page = vec![0u8; rustix::param::page_size()];
    let mut text: Vec<u8> = Vec::new();
    loop {
      let read = rustix::io::read(&fd, &mut page).map_err(errno_of)?;
      // `read` never exceeds the buffer it was given; a count past it would be the kernel's bug, refused
      // as a malformed read rather than trusted.
      let got = page
        .get(..read)
        .ok_or(MountTableError::Malformed { line: 0 })?;
      if got.is_empty() {
        break;
      }
      text.extend_from_slice(got);
      if text.len() > cap {
        return Err(MountTableError::TooLarge { cap });
      }
    }
    String::from_utf8(text).map_err(|_| MountTableError::Malformed { line: 0 })
  }

  /// The kernel's mount bound, as the sysctl states it.
  fn mount_max() -> Result<usize, MountTableError> {
    read_pseudo_file(MOUNT_MAX, LINE_CAP)?
      .trim()
      .parse()
      .map_err(|_| MountTableError::Malformed { line: 0 })
  }

  /// The text of `/proc/self/mountinfo`, bounded by the kernel's mount bound times the line bound.
  pub(super) fn read_mountinfo() -> Result<String, MountTableError> {
    let cap = mount_max()?.saturating_mul(LINE_CAP);
    read_pseudo_file(MOUNTINFO, cap)
  }
}

/// Format: the field separator `mountinfo` puts between the optional fields and the filesystem type
/// (proc(5)).
const MOUNTINFO_SEPARATOR: &str = "-";
/// Format: the zero-based index of the mount point in a `mountinfo` line (proc(5): mount ID, parent
/// ID, `major:minor`, root, mount point, ...).
const MOUNT_POINT_FIELD: usize = 4;
/// Format: the zero-based index of the mount ID and of `major:minor` in a `mountinfo` line (proc(5)).
const MOUNT_ID_FIELD: usize = 0;
/// Format: see [`MOUNT_ID_FIELD`].
const DEVICE_FIELD: usize = 2;
/// Format: the shift that packs a device's major number above its minor in [`MountIdentity::device`].
const MAJOR_SHIFT: u32 = 32;

/// The identity a `mountinfo` line names: its mount ID and its `major:minor`; `None` when either is not a
/// number.
fn identity_of(fields: &[&str]) -> Option<MountIdentity> {
  let mount = fields.get(MOUNT_ID_FIELD)?.parse::<u64>().ok()?;
  let (major, minor) = fields.get(DEVICE_FIELD)?.split_once(':')?;
  let major = major.parse::<u64>().ok()?;
  let minor = minor.parse::<u64>().ok()?;
  Some(MountIdentity {
    mount,
    device: (major << MAJOR_SHIFT) | minor,
  })
}

/// Format: a `mountinfo` escape is a backslash followed by exactly three octal digits (proc(5):
/// `\040` space, `\011` tab, `\012` newline, `\134` backslash).
const ESCAPE_DIGITS: usize = 3;
/// Format: the escape's radix, octal.
const ESCAPE_RADIX: u32 = 8;

/// Undoes `mountinfo`'s octal escapes; a backslash not followed by three octal digits is kept as is.
fn unescape(field: &str) -> String {
  let mut out = String::with_capacity(field.len());
  let mut chars = field.chars().peekable();
  while let Some(c) = chars.next() {
    if c != '\\' {
      out.push(c);
      continue;
    }
    let digits: String = chars.clone().take(ESCAPE_DIGITS).collect();
    match u8::from_str_radix(&digits, ESCAPE_RADIX) {
      Ok(byte) if digits.len() == ESCAPE_DIGITS => {
        out.push(char::from(byte));
        for _ in 0..ESCAPE_DIGITS {
          chars.next();
        }
      }
      _ => out.push(c),
    }
  }
  out
}

/// Parses the text of `/proc/self/mountinfo` (proc(5)): per line, the mount point is the fifth field,
/// and the filesystem type and source follow the `-` separator. Pure, so it is tested on every host.
pub fn parse_mountinfo(text: &str) -> Result<Vec<MountEntry>, MountTableError> {
  let mut entries = Vec::new();
  for (index, line) in text.lines().enumerate() {
    if line.trim().is_empty() {
      continue;
    }
    let fields: Vec<&str> = line.split(' ').collect();
    let malformed = MountTableError::Malformed {
      line: index.saturating_add(1),
    };
    let mount_point = fields.get(MOUNT_POINT_FIELD).ok_or(malformed.clone())?;
    let after_mount_point = MOUNT_POINT_FIELD.saturating_add(1);
    let separator = fields
      .iter()
      .skip(after_mount_point)
      .position(|f| *f == MOUNTINFO_SEPARATOR)
      .map(|p| p.saturating_add(after_mount_point))
      .ok_or(malformed.clone())?;
    let fstype = fields
      .get(separator.saturating_add(1))
      .ok_or(malformed.clone())?;
    let source = fields
      .get(separator.saturating_add(2))
      .ok_or(malformed.clone())?;
    let identity = identity_of(&fields).ok_or(malformed)?;
    entries.push(MountEntry {
      identity,
      mount_point: unescape(mount_point),
      fstype: unescape(fstype),
      source: unescape(source),
    });
  }
  Ok(entries)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
  use super::UnmountWatch;

  /// A-511 (§4.6). Do: open the unmount watch on this host's kernel, take what it holds twice, and poll its descriptor
  /// without waiting, as a shard's readiness wait registers it. Expect the kernel to take the filter (`EVFILT_FS` on
  /// `VQ_UNMOUNT` on macOS, `/proc/self/mountinfo` for `EPOLLPRI` on Linux), each take to answer, and poll(2) to answer
  /// for the descriptor. Whether anything was posted is not asserted: any process on the host may unmount at any
  /// moment. A real unmount's event ending its attachment is the CLI's live mount suite's to prove.
  #[test]
  fn the_unmount_watch_opens_on_the_kernels_own_event_and_its_descriptor_is_pollable() {
    let mut watch = UnmountWatch::open().expect("the kernel took the unmount filter");
    watch.take().expect("a take answers");
    watch.take().expect("a second take answers");
    // SAFETY: the descriptor is the watch's own queue, open for as long as `watch` lives, which outlives the borrow.
    let queue = unsafe { std::os::fd::BorrowedFd::borrow_raw(watch.descriptor()) };
    let mut polled = [rustix::event::PollFd::from_borrowed_fd(
      queue,
      rustix::event::PollFlags::IN,
    )];
    let now = rustix::event::Timespec {
      tv_sec: 0,
      tv_nsec: 0,
    };
    let ready =
      rustix::event::poll(&mut polled, Some(&now)).expect("poll(2) answers for the queue");
    println!("the unmount watch's queue: {ready} descriptor(s) ready at once");
  }
}

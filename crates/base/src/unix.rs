//! The Unix host: descriptors from rustix, bulk listings from `getdents` with one `statat` per
//! entry, and hints from inotify (Linux) or `EVFILT_VNODE` on a kqueue (macOS, BSD).

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat};
use slates_vfs::host::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, HostKind, WatchState,
};
use slates_vfs::inode::Fingerprint;

use crate::{FsKind, granularity_for};

/// Format: nanoseconds per second.
const NS_PER_S: i64 = 1_000_000_000;

/// The Unix host.
pub struct OsHost {
  dirs: BTreeMap<u64, OwnedFd>,
  files: BTreeMap<u64, OwnedFd>,
  next: u64,
  watcher: watch::Watcher,
}

impl std::fmt::Debug for OsHost {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OsHost")
      .field("dirs", &self.dirs.len())
      .field("files", &self.files.len())
      .finish()
  }
}

fn refusal(e: rustix::io::Errno) -> HostError {
  match e {
    rustix::io::Errno::NOENT => HostError::NotFound,
    rustix::io::Errno::NOTDIR => HostError::NotDirectory,
    rustix::io::Errno::LOOP | rustix::io::Errno::ISDIR => HostError::NotFile,
    other => HostError::Unavailable(other.raw_os_error()),
  }
}

/// The fingerprint of a `stat` result: device, inode, size, both timestamps in nanoseconds,
/// and the mode.
fn fingerprint(st: &Stat) -> Fingerprint {
  Fingerprint {
    dev: u64::try_from(i128::from(st.st_dev)).unwrap_or(0),
    ino: u64::try_from(i128::from(st.st_ino)).unwrap_or(0),
    size: u64::try_from(st.st_size).unwrap_or(0),
    mtime_ns: stamp_ns(widen(st.st_mtime), widen(st.st_mtime_nsec)),
    ctime_ns: stamp_ns(widen(st.st_ctime), widen(st.st_ctime_nsec)),
    mode: u32::from(st.st_mode),
  }
}

/// A `stat` time field as `i64`, whatever width the platform gives it (`c_long` is 32 bits on
/// i686).
fn widen<T: Into<i64>>(x: T) -> i64 {
  x.into()
}

/// Seconds and nanoseconds as one nanosecond count, saturating at the type's edges (year 2262).
fn stamp_ns(seconds: i64, nanos: i64) -> i64 {
  seconds.saturating_mul(NS_PER_S).saturating_add(nanos)
}

fn kind_of(file_type: FileType) -> HostKind {
  match file_type {
    FileType::RegularFile => HostKind::File,
    FileType::Directory => HostKind::Dir,
    FileType::Symlink => HostKind::Symlink,
    _ => HostKind::Other,
  }
}

impl OsHost {
  /// Opens a base directory (`O_DIRECTORY|O_NOFOLLOW`): the one path this host ever resolves.
  pub fn open_root(path: &Path) -> Result<(Self, HostDir), HostError> {
    let fd = rustix::fs::open(
      path,
      OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
      Mode::empty(),
    )
    .map_err(refusal)?;
    let mut host = Self {
      dirs: BTreeMap::new(),
      files: BTreeMap::new(),
      next: 1,
      watcher: watch::Watcher::new(),
    };
    let root = host.keep_dir(fd);
    Ok((host, root))
  }

  fn keep_dir(&mut self, fd: OwnedFd) -> HostDir {
    let h = self.next;
    self.next += 1;
    self.dirs.insert(h, fd);
    HostDir(h)
  }

  fn dir(&self, dir: HostDir) -> Result<&OwnedFd, HostError> {
    self.dirs.get(&dir.0).ok_or(HostError::StaleHandle)
  }

  fn file(&self, file: HostFile) -> Result<&OwnedFd, HostError> {
    self.files.get(&file.0).ok_or(HostError::StaleHandle)
  }

  /// Open handles, for leak checks.
  pub fn open_handles(&self) -> usize {
    self.dirs.len() + self.files.len()
  }
}

impl HostFs for OsHost {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    let fd = self.dir(dir)?;
    let fs = rustix::fs::fstatfs(fd).map_err(refusal)?;
    let resolution = rustix::time::clock_getres(rustix::time::ClockId::Realtime);
    let clock_ns = u64::try_from(resolution.tv_nsec).unwrap_or(1).max(1);
    Ok(HostFacts {
      timestamp_granularity_ns: granularity_for(fs_kind(&fs)).max(clock_ns),
    })
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    let fd = self.dir(dir)?;
    rustix::fs::fstat(fd)
      .map(|st| fingerprint(&st))
      .map_err(refusal)
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    let fd = self.dir(dir)?;
    let reader = Dir::read_from(fd).map_err(refusal)?;
    let mut out = Vec::new();
    for entry in reader {
      let entry = entry.map_err(refusal)?;
      let raw = entry.file_name();
      if raw.to_bytes() == b"." || raw.to_bytes() == b".." {
        continue;
      }
      let Ok(name) = raw.to_str() else {
        // Not UTF-8: listed as something the volume never serves.
        out.push(BaseEntry {
          name: raw.to_string_lossy().into_owned().into(),
          kind: HostKind::Other,
          fingerprint: Fingerprint::default(),
        });
        continue;
      };
      let st = match rustix::fs::statat(fd, raw, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => st,
        // Gone between the listing and the stat: the disk is the truth, it is not there.
        Err(rustix::io::Errno::NOENT) => continue,
        Err(e) => return Err(refusal(e)),
      };
      out.push(BaseEntry {
        name: name.into(),
        kind: kind_of(entry.file_type()),
        fingerprint: fingerprint(&st),
      });
    }
    Ok(out)
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    let fd = rustix::fs::openat(
      self.dir(parent)?,
      name,
      OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
      Mode::empty(),
    )
    .map_err(|e| match e {
      rustix::io::Errno::LOOP => HostError::NotDirectory,
      other => refusal(other),
    })?;
    Ok(self.keep_dir(fd))
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let fd = rustix::fs::openat(
      self.dir(dir)?,
      name,
      OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
      Mode::empty(),
    )
    .map_err(refusal)?;
    let st = rustix::fs::fstat(&fd).map_err(refusal)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
      return Err(HostError::NotFile);
    }
    let h = self.next;
    self.next += 1;
    self.files.insert(h, fd);
    Ok(HostFile(h))
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    let fd = self.file(file)?;
    rustix::fs::fstat(fd)
      .map(|st| fingerprint(&st))
      .map_err(refusal)
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    let fd = self.file(file)?;
    rustix::io::pread(fd, buf, off).map_err(refusal)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    let fd = self.dir(dir)?;
    let target = rustix::fs::readlinkat(fd, name, Vec::new()).map_err(refusal)?;
    target
      .to_str()
      .map(|s| s.into())
      .map_err(|_| HostError::Unavailable(rustix::io::Errno::ILSEQ.raw_os_error()))
  }

  fn close_file(&mut self, file: HostFile) {
    self.files.remove(&file.0);
  }

  fn close_dir(&mut self, dir: HostDir) {
    if let Some(fd) = self.dirs.remove(&dir.0) {
      self.watcher.forget(dir, &fd);
    }
  }

  fn watch(&mut self, dir: HostDir) -> WatchState {
    match self.dirs.get(&dir.0) {
      Some(fd) => self.watcher.add(dir, fd),
      None => WatchState::Unavailable,
    }
  }

  fn hints(&mut self) -> Vec<Hint> {
    self.watcher.drain()
  }
}

/// The filesystem class beneath a descriptor, from `fstatfs`.
#[cfg(target_os = "linux")]
fn fs_kind(fs: &rustix::fs::StatFs) -> FsKind {
  /// Format: `statfs(2)`'s `f_type` magic for ext2/3/4.
  const EXT4: i64 = 0xEF53;
  /// Format: the magic for XFS.
  const XFS: i64 = 0x5846_5342;
  /// Format: the magic for Btrfs.
  const BTRFS: i64 = 0x9123_683E;
  /// Format: the magic for tmpfs.
  const TMPFS: i64 = 0x0102_1994;
  /// Format: the magic for FAT (msdos/vfat).
  const MSDOS: i64 = 0x4d44;
  /// Format: the magic for exFAT.
  const EXFAT: i64 = 0x2011_BAB0;
  /// Format: the magic for NTFS.
  const NTFS: i64 = 0x5346_544e;
  /// Format: the magic for HFS+.
  const HFSPLUS: i64 = 0x482b;
  match i64::from(fs.f_type) {
    EXT4 | XFS | BTRFS | TMPFS => FsKind::Nanosecond,
    NTFS => FsKind::HundredNanoseconds,
    HFSPLUS => FsKind::Second,
    MSDOS | EXFAT => FsKind::TwoSeconds,
    _ => FsKind::Unknown,
  }
}

/// The filesystem class beneath a descriptor, from `fstatfs`'s type name.
#[cfg(not(target_os = "linux"))]
fn fs_kind(fs: &rustix::fs::StatFs) -> FsKind {
  let name: Vec<u8> = fs
    .f_fstypename
    .iter()
    .take_while(|c| **c != 0)
    .map(|c| u8::try_from(i32::from(*c)).unwrap_or(b'?'))
    .collect();
  match name.as_slice() {
    b"apfs" => FsKind::Nanosecond,
    b"hfs" => FsKind::Second,
    b"msdos" | b"exfat" => FsKind::TwoSeconds,
    b"ntfs" => FsKind::HundredNanoseconds,
    _ => FsKind::Unknown,
  }
}

#[cfg(target_os = "linux")]
mod watch {
  //! inotify: one instance, one watch per directory through `/proc/self/fd`, drained without
  //! blocking; `IN_Q_OVERFLOW` is the overflow hint.

  use std::collections::BTreeMap;
  use std::mem::MaybeUninit;
  use std::os::fd::{AsRawFd, OwnedFd};

  use rustix::fs::inotify::{CreateFlags, ReadFlags, Reader, WatchFlags};
  use slates_vfs::host::{Hint, HostDir, WatchState};

  /// Derived: the read buffer holds one page of events, the smallest buffer `read(2)` on an
  /// inotify descriptor accepts for a full event with the longest name (`sizeof(inotify_event)
  /// + NAME_MAX + 1` = 272 bytes) fifteen times over.
  const BUFFER_BYTES: usize = 4096;

  pub(super) struct Watcher {
    inotify: Option<OwnedFd>,
    by_wd: BTreeMap<i32, HostDir>,
    wd_of: BTreeMap<HostDir, i32>,
  }

  impl Watcher {
    pub(super) fn new() -> Self {
      Self {
        inotify: rustix::fs::inotify::init(CreateFlags::NONBLOCK | CreateFlags::CLOEXEC).ok(),
        by_wd: BTreeMap::new(),
        wd_of: BTreeMap::new(),
      }
    }

    pub(super) fn add(&mut self, dir: HostDir, fd: &OwnedFd) -> WatchState {
      let Some(inotify) = &self.inotify else {
        return WatchState::Unavailable;
      };
      let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
      let flags = WatchFlags::CREATE
        | WatchFlags::DELETE
        | WatchFlags::MODIFY
        | WatchFlags::MOVED_FROM
        | WatchFlags::MOVED_TO
        | WatchFlags::ATTRIB
        | WatchFlags::CLOSE_WRITE
        | WatchFlags::DELETE_SELF
        | WatchFlags::MOVE_SELF;
      match rustix::fs::inotify::add_watch(inotify, path, flags) {
        Ok(wd) => {
          self.by_wd.insert(wd, dir);
          self.wd_of.insert(dir, wd);
          WatchState::Live
        }
        Err(_) => WatchState::Unavailable,
      }
    }

    pub(super) fn forget(&mut self, dir: HostDir, _fd: &OwnedFd) {
      if let (Some(inotify), Some(wd)) = (&self.inotify, self.wd_of.remove(&dir)) {
        let _ = rustix::fs::inotify::remove_watch(inotify, wd);
        self.by_wd.remove(&wd);
      }
    }

    pub(super) fn drain(&mut self) -> Vec<Hint> {
      let Some(inotify) = &self.inotify else {
        return Vec::new();
      };
      let mut buf = [MaybeUninit::<u8>::uninit(); BUFFER_BYTES];
      let mut reader = Reader::new(inotify, &mut buf);
      let mut out = Vec::new();
      loop {
        match reader.next() {
          Ok(event) => {
            if event.events().contains(ReadFlags::QUEUE_OVERFLOW) {
              out.push(Hint::Overflow);
            } else if let Some(dir) = self.by_wd.get(&event.wd()) {
              out.push(Hint::Changed(*dir));
            }
          }
          Err(_) => break,
        }
      }
      out.dedup();
      out
    }
  }
}

#[cfg(not(target_os = "linux"))]
mod watch {
  //! kqueue: one queue, one `EVFILT_VNODE` registration per directory descriptor (the
  //! descriptor stays open for the listing anyway), drained with a zero timeout. kqueue does
  //! not report lost events; a registration failure is `Unavailable`.

  use std::collections::BTreeMap;
  use std::os::fd::{AsFd, AsRawFd, OwnedFd};

  use rustix::event::kqueue::{Event, EventFilter, EventFlags, VnodeEvents, kevent, kqueue};
  use slates_vfs::host::{Hint, HostDir, WatchState};

  /// Shape: events drained per call; the queue is drained again on the next call.
  const DRAIN: usize = 64;

  pub(super) struct Watcher {
    queue: Option<OwnedFd>,
    by_fd: BTreeMap<i32, HostDir>,
  }

  impl Watcher {
    pub(super) fn new() -> Self {
      Self {
        queue: kqueue().ok(),
        by_fd: BTreeMap::new(),
      }
    }

    pub(super) fn add(&mut self, dir: HostDir, fd: &OwnedFd) -> WatchState {
      let Some(queue) = &self.queue else {
        return WatchState::Unavailable;
      };
      let flags = VnodeEvents::WRITE
        | VnodeEvents::DELETE
        | VnodeEvents::RENAME
        | VnodeEvents::ATTRIBUTES
        | VnodeEvents::EXTEND
        | VnodeEvents::LINK;
      let change = Event::new(
        EventFilter::Vnode {
          vnode: fd.as_fd().as_raw_fd(),
          flags,
        },
        EventFlags::ADD | EventFlags::CLEAR,
        std::ptr::null_mut(),
      );
      let mut none: Vec<Event> = Vec::new();
      // SAFETY: the change list is one event on a descriptor this host owns; the output list is
      // empty and stays empty (its capacity is zero), so the kernel writes nothing.
      let registered = unsafe { kevent(queue, &[change], &mut none, None) };
      match registered {
        Ok(_) => {
          self.by_fd.insert(fd.as_raw_fd(), dir);
          WatchState::Live
        }
        Err(_) => WatchState::Unavailable,
      }
    }

    pub(super) fn forget(&mut self, _dir: HostDir, fd: &OwnedFd) {
      // Closing the descriptor removes its registration.
      self.by_fd.remove(&fd.as_raw_fd());
    }

    pub(super) fn drain(&mut self) -> Vec<Hint> {
      let Some(queue) = &self.queue else {
        return Vec::new();
      };
      let mut events: Vec<Event> = Vec::with_capacity(DRAIN);
      // SAFETY: no changes are submitted; the output vector has the capacity the kernel may
      // fill, and rustix sets its length from the count the kernel returns; a zero timeout
      // never blocks.
      let drained = unsafe { kevent(queue, &[], &mut events, Some(std::time::Duration::ZERO)) };
      if drained.is_err() {
        return Vec::new();
      }
      let mut out: Vec<Hint> = events
        .iter()
        .filter_map(|e| match e.filter() {
          EventFilter::Vnode { vnode, .. } => self.by_fd.get(&vnode).map(|d| Hint::Changed(*d)),
          _ => None,
        })
        .collect();
      out.dedup();
      out
    }
  }
}

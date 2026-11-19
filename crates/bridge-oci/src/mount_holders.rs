//! The processes holding a mount, read from the kernel's own tables on a busy unmount's cold path, never touching the
//! mount (§4.6; A-537, the item A-507 left owed). One seam, [`holders`], with a paired implementation per platform, as
//! [`crate::mount_table`] has.
//!
//! **Linux.** Every process `/proc` lists, and of each its descriptors (`/proc/<pid>/fd`, "a subdirectory containing one
//! entry for each file which the process has open, named by its file descriptor, and which is a symbolic link to the
//! actual file", proc_pid_fd(5)) and its working and root directories (`/proc/<pid>/cwd`, `/proc/<pid>/root`), each
//! `statx`ed through its link with `AT_STATX_DONT_SYNC` ("don't synchronize anything, but rather just take whatever the
//! system has cached if possible", statx(2)): the file's device (`stx_dev_major`/`stx_dev_minor`, "the device on which
//! this file (inode) resides") is compared with the mount's, and no request reaches the FUSE daemon serving it. A
//! process whose links the caller may not follow (proc_pid_fd(5): "governed by a ptrace access mode
//! `PTRACE_MODE_READ_FSCREDS` check", so another user's) is counted unreadable, since it may hold the mount too; one
//! that exits meanwhile is passed by. The read is bounded by what the kernel states it lists: the processes by
//! `/proc/sys/kernel/pid_max` ("the value at which PIDs wrap around", proc_sys_kernel(5)) and each process's
//! descriptors by `/proc/sys/fs/nr_open` ("a ceiling on the value to which the `RLIMIT_NOFILE` resource limit can be
//! raised", proc_sys_fs(5)); a listing past either is refused [`HoldersError::TooLarge`]. A device is a superblock's, so
//! a file held through another bind mount of the same filesystem is named too; slates makes no bind mount of its own.
//!
//! **macOS.** Not read. Every libproc answer that reports a file is filled by xnu's `fill_vnodeinfo`
//! (`bsd/kern/proc_info.c`), which runs `vn_stat`, a `VNOP_GETATTR` of the vnode, before it reads the vnode's filesystem
//! id; and `proc_listpidspath` (`libsyscall/wrappers/libproc/proc_listpidspath.c`) begins with a `stat(2)` of the path.
//! The slates mount on macOS is a `hard` loopback NFS mount, on which either waits without bound once the daemon serving
//! it has gone, the very case a busy unmount meets (A-507: "the only way off it for a holder whose daemon has gone"). So
//! the macOS answer is [`HoldersError::Unsupported`], naming why.

use std::fmt;

/// What the kernel's tables showed holding one mount.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Holders {
  /// The processes holding a descriptor, a working directory or a root on the mount, ascending, each once.
  pub pids: Vec<u32>,
  /// The processes listed whose links this caller may not follow (another user's): any of them may hold the mount too.
  pub unreadable: u32,
}

/// Why no holder could be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldersError {
  /// This platform has no reader that leaves the mount alone.
  Unsupported {
    /// The platform.
    platform: &'static str,
    /// Why its readers would touch the mount, or that none is built.
    reason: &'static str,
  },
  /// The kernel refused its process list or one of its bounds.
  Query {
    /// The errno.
    errno: i32,
  },
  /// A listing went past the bound the kernel states for it (`pid_max` processes, `nr_open` descriptors a process).
  TooLarge {
    /// The bound.
    cap: usize,
  },
  /// A bound's sysctl did not hold one decimal number.
  Malformed {
    /// The sysctl.
    path: &'static str,
  },
}

impl fmt::Display for HoldersError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Unsupported { platform, reason } => {
        write!(f, "no holder of a mount is read on {platform}: {reason}")
      }
      Self::Query { errno } => write!(f, "the kernel refused the process tables: errno {errno}"),
      Self::TooLarge { cap } => write!(f, "a process listing passed its kernel's bound of {cap}"),
      Self::Malformed { path } => write!(f, "{path} did not hold one decimal number"),
    }
  }
}

impl std::error::Error for HoldersError {}

/// What one listed process's tables showed of the mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
  /// A descriptor, its working directory or its root is on the mount.
  Holds,
  /// None of them is.
  Clear,
  /// Its links may not be followed by this caller.
  Unreadable,
  /// It exited while it was read.
  Gone,
}

/// The holders gathered from the processes the kernel listed, one [`Seen`] each, and the bound on their number: a
/// listing past `cap` processes is refused rather than read on.
#[derive(Debug)]
pub struct Tally {
  holders: Holders,
  listed: usize,
  cap: usize,
}

impl Tally {
  /// A tally of at most `cap` listed processes.
  pub const fn within(cap: usize) -> Self {
    Self {
      holders: Holders {
        pids: Vec::new(),
        unreadable: 0,
      },
      listed: 0,
      cap,
    }
  }

  /// Records what process `pid`'s tables showed; refused once the listing passes the bound.
  pub fn record(&mut self, pid: u32, seen: Seen) -> Result<(), HoldersError> {
    self.listed = self.listed.saturating_add(1);
    if self.listed > self.cap {
      return Err(HoldersError::TooLarge { cap: self.cap });
    }
    match seen {
      Seen::Holds => self.holders.pids.push(pid),
      Seen::Unreadable => self.holders.unreadable = self.holders.unreadable.saturating_add(1),
      Seen::Clear | Seen::Gone => {}
    }
    Ok(())
  }

  /// The holders, ascending and each once.
  pub fn holders(mut self) -> Holders {
    self.holders.pids.sort_unstable();
    self.holders.pids.dedup();
    self.holders
  }
}

/// The process id a `/proc` entry names, or `None` for an entry that is not a process (`self`, `sys`, ...): its name is
/// all decimal digits.
pub fn pid_of(name: &[u8]) -> Option<u32> {
  if name.is_empty() || !name.iter().all(u8::is_ascii_digit) {
    return None;
  }
  std::str::from_utf8(name).ok()?.parse().ok()
}

/// The processes holding the mount whose device is `device` (`major << 32 | minor`, as
/// [`crate::mount_table::MountIdentity::device`] packs it), read from the kernel's own tables without touching the mount.
#[cfg(target_os = "linux")]
pub fn holders(device: u64) -> Result<Holders, HoldersError> {
  linux::holders(device)
}

/// See the Linux seam. macOS reads none: its readers ask the mount (the module doc).
#[cfg(target_os = "macos")]
pub fn holders(_device: u64) -> Result<Holders, HoldersError> {
  Err(HoldersError::Unsupported {
    platform: "macos",
    reason: "libproc stats every file it reports (xnu fill_vnodeinfo runs vn_stat), which a hard NFS mount whose \
             daemon has gone waits on without bound",
  })
}

/// See the Linux seam. No reader is built for this platform.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn holders(_device: u64) -> Result<Holders, HoldersError> {
  Err(HoldersError::Unsupported {
    platform: std::env::consts::OS,
    reason: "no reader is built",
  })
}

#[cfg(target_os = "linux")]
mod linux {
  use rustix::fs::{AtFlags, Dir, Mode, OFlags, StatxFlags};
  use rustix::io::Errno;

  use super::{Holders, HoldersError, Seen, Tally, pid_of};

  /// Format: the kernel's table of processes (proc(5)).
  const PROC: &str = "/proc";
  /// Format: the value PIDs wrap around at, one past the largest (proc_sys_kernel(5)).
  const PID_MAX: &str = "/proc/sys/kernel/pid_max";
  /// Format: the ceiling on `RLIMIT_NOFILE`, so on any process's open descriptors (proc_sys_fs(5)).
  const NR_OPEN: &str = "/proc/sys/fs/nr_open";
  /// Derived: a sysctl's whole answer read here: `u64::MAX`'s twenty decimal digits and the newline after them.
  const SYSCTL_BYTES: usize = 21;

  fn errno_of(e: Errno) -> HoldersError {
    HoldersError::Query {
      errno: e.raw_os_error(),
    }
  }

  /// The number a sysctl holds.
  fn sysctl(path: &'static str) -> Result<usize, HoldersError> {
    let fd =
      rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).map_err(errno_of)?;
    let mut text = [0u8; SYSCTL_BYTES];
    let read = rustix::io::read(&fd, &mut text).map_err(errno_of)?;
    text
      .get(..read)
      .and_then(|number| std::str::from_utf8(number).ok())
      .and_then(|number| number.trim().parse().ok())
      .ok_or(HoldersError::Malformed { path })
  }

  /// Whether `link` (a `/proc` link) leads to a file on `device`, asked of the kernel's cache alone.
  fn on_device(link: &str, device: u64) -> Result<bool, Errno> {
    let answer = rustix::fs::statx(
      rustix::fs::CWD,
      link,
      AtFlags::STATX_DONT_SYNC,
      StatxFlags::TYPE,
    )?;
    let at = (u64::from(answer.stx_dev_major) << crate::mount_table::MAJOR_SHIFT)
      | u64::from(answer.stx_dev_minor);
    Ok(at == device)
  }

  /// What one link's answer says of its process: a hold, nothing yet, the process gone (`ENOENT`, `ESRCH`), or a link
  /// the caller may not follow or the kernel would not resolve, which may be a hold.
  fn judged(answer: Result<bool, Errno>) -> Option<Seen> {
    match answer {
      Ok(true) => Some(Seen::Holds),
      Ok(false) => None,
      Err(Errno::NOENT | Errno::SRCH) => Some(Seen::Gone),
      Err(_) => Some(Seen::Unreadable),
    }
  }

  /// What process `pid`'s tables show of `device`: its working and root directories, then each descriptor the kernel
  /// lists for it, at most `descriptors` of them. A descriptor closed since it was listed is passed by; one the kernel
  /// would not resolve leaves the process unreadable unless another shows a hold.
  fn seen(pid: u32, device: u64, descriptors: usize) -> Result<Seen, HoldersError> {
    for link in ["cwd", "root"] {
      if let Some(seen) = judged(on_device(&format!("{PROC}/{pid}/{link}"), device)) {
        return Ok(seen);
      }
    }
    let table = format!("{PROC}/{pid}/fd");
    let opened = rustix::fs::open(
      table.as_str(),
      OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
      Mode::empty(),
    );
    let listing = match opened.and_then(|fd| Dir::read_from(&fd)) {
      Ok(listing) => listing,
      Err(e) => return Ok(judged(Err(e)).unwrap_or(Seen::Gone)),
    };
    let mut listed = 0usize;
    let mut unresolved = false;
    for entry in listing {
      let entry = match entry {
        Ok(entry) => entry,
        Err(e) => return Ok(judged(Err(e)).unwrap_or(Seen::Gone)),
      };
      let Some(number) = pid_of(entry.file_name().to_bytes()) else {
        continue;
      };
      listed = listed.saturating_add(1);
      if listed > descriptors {
        return Err(HoldersError::TooLarge { cap: descriptors });
      }
      match judged(on_device(&format!("{table}/{number}"), device)) {
        Some(Seen::Holds) => return Ok(Seen::Holds),
        Some(Seen::Unreadable) => unresolved = true,
        Some(Seen::Clear | Seen::Gone) | None => {}
      }
    }
    Ok(if unresolved {
      Seen::Unreadable
    } else {
      Seen::Clear
    })
  }

  pub(super) fn holders(device: u64) -> Result<Holders, HoldersError> {
    let mut tally = Tally::within(sysctl(PID_MAX)?);
    let descriptors = sysctl(NR_OPEN)?;
    let fd = rustix::fs::open(
      PROC,
      OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
      Mode::empty(),
    )
    .map_err(errno_of)?;
    for entry in Dir::read_from(&fd).map_err(errno_of)? {
      let entry = entry.map_err(errno_of)?;
      let Some(pid) = pid_of(entry.file_name().to_bytes()) else {
        continue;
      };
      tally.record(pid, seen(pid, device, descriptors)?)?;
    }
    Ok(tally.holders())
  }
}

#[cfg(test)]
mod tests {
  use super::{Holders, HoldersError, Seen, Tally, pid_of};

  /// A-537, the pure tally every host runs: each listed process counted once by what its tables showed, the holders
  /// ascending and each once, the unreadable counted, the exited and the clear passed by. Do: record holders out of
  /// order and twice, a clear, an unreadable and a gone process. Expect the holders `[3, 7]` and one unreadable.
  #[test]
  fn a_tally_names_each_holder_once_in_order_and_counts_the_unreadable() {
    let mut tally = Tally::within(8);
    for (pid, seen) in [
      (7, Seen::Holds),
      (2, Seen::Clear),
      (3, Seen::Holds),
      (9, Seen::Unreadable),
      (4, Seen::Gone),
      (7, Seen::Holds),
    ] {
      tally.record(pid, seen).unwrap();
    }
    assert_eq!(
      tally.holders(),
      Holders {
        pids: vec![3, 7],
        unreadable: 1,
      }
    );
  }

  /// A-537, the bound: a listing is read only as far as the kernel's stated bound and refused typed past it. Do: tally
  /// three processes within a bound of two. Expect the first two recorded and the third refused `TooLarge { cap: 2 }`.
  #[test]
  fn a_listing_past_its_kernels_bound_is_refused_typed() {
    let mut tally = Tally::within(2);
    assert!(tally.record(1, Seen::Holds).is_ok());
    assert!(tally.record(2, Seen::Clear).is_ok());
    assert_eq!(
      tally.record(3, Seen::Holds),
      Err(HoldersError::TooLarge { cap: 2 })
    );
  }

  /// A-537: only an all-digit `/proc` entry is a process. Do: read `1234`, `self`, `sys`, an empty name, `12a` and a
  /// number past `u32`. Expect only `1234` read.
  #[test]
  fn only_an_all_digit_proc_entry_is_a_process() {
    assert_eq!(pid_of(b"1234"), Some(1234));
    for name in [&b"self"[..], b"sys", b"", b"12a", b"99999999999"] {
      assert_eq!(pid_of(name), None, "{name:?}");
    }
  }

  /// A-537 on Linux, by use: the caller's own process holds the root mount (its root directory is there), so a read of
  /// the root's device names it. Do: `statx` `/` for its device and read that device's holders. Expect this process
  /// among them, so the read reached the kernel's tables and compared by device.
  #[cfg(target_os = "linux")]
  #[test]
  fn the_caller_is_named_holding_the_mount_its_root_is_on() {
    let root = rustix::fs::statx(
      rustix::fs::CWD,
      "/",
      rustix::fs::AtFlags::STATX_DONT_SYNC,
      rustix::fs::StatxFlags::TYPE,
    )
    .unwrap();
    let device = (u64::from(root.stx_dev_major) << crate::mount_table::MAJOR_SHIFT)
      | u64::from(root.stx_dev_minor);
    let holders = super::holders(device).unwrap();
    assert!(
      holders.pids.contains(&std::process::id()),
      "this process ({}) holds the root mount: {holders:?}",
      std::process::id()
    );
  }

  /// A-537 on macOS: no holder is read, and the refusal says why. Do: ask for any device's holders. Expect `Unsupported`
  /// naming macOS.
  #[cfg(target_os = "macos")]
  #[test]
  fn macos_reads_no_holder_and_says_why() {
    assert!(matches!(
      super::holders(0),
      Err(HoldersError::Unsupported {
        platform: "macos",
        ..
      })
    ));
  }
}

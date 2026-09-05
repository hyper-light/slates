//! The write seam over the operating system (§4.15 step 6, "Exchange fallback"; D-26): the
//! only code in the workspace that creates, writes, links, exchanges, renames or removes a host
//! path. It runs inside a granted landing only; the engine calls nothing here before the grant
//! and the lease.
//!
//! Shape: every verb is relative to a descriptor the base host holds (`OsHost`, which resolves
//! the one root path with `O_NOFOLLOW` and hands out handles); no verb takes a path string.
//! Linux: `O_TMPFILE` temporaries linked by `linkat` through `/proc/self/fd`, `fdatasync`,
//! `renameat2(RENAME_EXCHANGE)`, `fsync` on directories. macOS: hidden-name temporaries
//! (`O_CREAT|O_EXCL`), `fcntl(F_BARRIERFSYNC)` as the data barrier, `renameatx_np(RENAME_SWAP)`,
//! `fcntl(F_FULLFSYNC)` on the target as the media barrier when the grant asked for it. Both:
//! `fchmod`, `futimens`, `unlinkat`, `mkdirat`, `symlinkat`, `renameat`. The three `unsafe`
//! sites are the libc calls rustix does not wrap (`F_BARRIERFSYNC`, `F_FULLFSYNC`,
//! `renameatx_np`), each on a descriptor this host owns.
//!
//! Containment (§4.15 step 4, §4.13): [`OsLand::open_target`] opens the target path one
//! component at a time with `O_DIRECTORY|O_NOFOLLOW`, so a symlink anywhere in it is `ELOOP`
//! and refused as `EscapesTarget`; `..` is refused; the target must be owned by the effective
//! user (`TargetNotOwned`). `TargetIsVolume` waits on the mount table of Phase 3 (GAPS §8c).

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Component, Path};

use rustix::fs::{AtFlags, Mode, OFlags};
use slates_base::OsHost;
use slates_vfs::host::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, LandCapabilities, LandFs,
  WatchState,
};
use slates_vfs::inode::Fingerprint;

use crate::engine::LandingTarget;

/// Format: the mode a temporary is created with before `set_mode` applies the overlay's: owner
/// read and write only, so a half-written file is never readable by others.
const TEMP_MODE: u32 = 0o600;
/// Format: nanoseconds per second, for `futimens`.
const NS_PER_S: i64 = 1_000_000_000;
/// Format: the permission and setuid/setgid/sticky bits of a POSIX mode; the type bits above
/// them never reach `fchmod` or `mkdirat`.
const PERMISSION_MASK: u32 = 0o7777;

/// A mode's permission bits in the platform's `mode_t` width (16 bits on macOS).
fn mode_bits(mode: u32) -> Mode {
  Mode::from_bits_truncate(rustix::fs::RawMode::try_from(mode & PERMISSION_MASK).unwrap_or(0))
}

/// Why a target path was refused before the lease (§4.15 step 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetRefusal {
  /// The path is not absolute.
  NotAbsolute,
  /// A component is a symlink or `..`: the path would escape the directory the human named.
  EscapesTarget,
  /// The target is owned by another user.
  TargetNotOwned,
  /// The host refused.
  Unavailable(HostError),
}

/// A temporary's bookkeeping: where it was created and under which name, if any.
#[derive(Clone, Debug)]
struct Temp {
  dir: HostDir,
  created_as: Option<Box<str>>,
}

/// The operating-system writer.
pub struct OsLand {
  host: OsHost,
  temps: BTreeMap<u64, Temp>,
}

impl std::fmt::Debug for OsLand {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OsLand")
      .field("host", &self.host)
      .field("temps", &self.temps.len())
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

/// The directory flags every containment step uses.
fn dir_flags() -> OFlags {
  OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

impl OsLand {
  /// Opens an absolute target path with containment and the ownership check, and returns the
  /// writer with the target (its parent opened too, for stage-and-exchange).
  pub fn open_target(path: &Path) -> Result<(Self, LandingTarget), TargetRefusal> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
      return Err(TargetRefusal::NotAbsolute);
    }
    let mut current = rustix::fs::open("/", dir_flags(), Mode::empty())
      .map_err(|e| TargetRefusal::Unavailable(refusal(e)))?;
    let mut parent: Option<(OwnedFd, Box<str>)> = None;
    let mut key = String::new();
    for component in components {
      let name = match component {
        Component::Normal(n) => n.to_str().ok_or(TargetRefusal::EscapesTarget)?,
        Component::CurDir => continue,
        _ => return Err(TargetRefusal::EscapesTarget),
      };
      let next =
        rustix::fs::openat(&current, name, dir_flags(), Mode::empty()).map_err(|e| match e {
          rustix::io::Errno::LOOP => TargetRefusal::EscapesTarget,
          other => TargetRefusal::Unavailable(refusal(other)),
        })?;
      key.push('/');
      key.push_str(name);
      parent = Some((current, name.into()));
      current = next;
    }
    let st = rustix::fs::fstat(&current).map_err(|e| TargetRefusal::Unavailable(refusal(e)))?;
    if st.st_uid != rustix::process::geteuid().as_raw() {
      return Err(TargetRefusal::TargetNotOwned);
    }
    let (mut host, _) = OsHost::open_root(Path::new("/")).map_err(TargetRefusal::Unavailable)?;
    let dir = host.adopt_dir(current);
    let parent = parent.map(|(fd, name)| (host.adopt_dir(fd), name));
    let target = LandingTarget {
      dir,
      key: if key.is_empty() {
        "/".into()
      } else {
        key.into()
      },
      parent,
    };
    Ok((
      Self {
        host,
        temps: BTreeMap::new(),
      },
      target,
    ))
  }

  /// The base host beneath, for the volume's read verbs.
  pub fn host(&mut self) -> &mut OsHost {
    &mut self.host
  }

  fn dir(&self, dir: HostDir) -> Result<BorrowedFd<'_>, HostError> {
    self.host.dir_fd(dir)
  }

  fn file(&self, file: HostFile) -> Result<BorrowedFd<'_>, HostError> {
    self.host.file_fd(file)
  }

  /// A named temporary at `name` in `dir` (`O_CREAT|O_EXCL`).
  fn create_named(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let fd = rustix::fs::openat(
      self.dir(dir)?,
      name,
      OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
      mode_bits(TEMP_MODE),
    )
    .map_err(refusal)?;
    let file = self.host.adopt_file(fd);
    self.temps.insert(
      file.0,
      Temp {
        dir,
        created_as: Some(name.into()),
      },
    );
    Ok(file)
  }

  /// An unnamed temporary (`O_TMPFILE`), where the filesystem offers it.
  #[cfg(target_os = "linux")]
  fn create_unnamed(&mut self, dir: HostDir) -> Result<Option<HostFile>, HostError> {
    let opened = rustix::fs::openat(
      self.dir(dir)?,
      ".",
      OFlags::WRONLY | OFlags::TMPFILE | OFlags::CLOEXEC,
      mode_bits(TEMP_MODE),
    );
    match opened {
      Ok(fd) => {
        let file = self.host.adopt_file(fd);
        self.temps.insert(
          file.0,
          Temp {
            dir,
            created_as: None,
          },
        );
        Ok(Some(file))
      }
      Err(rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::ISDIR | rustix::io::Errno::INVAL) => {
        Ok(None)
      }
      Err(e) => Err(refusal(e)),
    }
  }

  #[cfg(not(target_os = "linux"))]
  fn create_unnamed(&mut self, _dir: HostDir) -> Result<Option<HostFile>, HostError> {
    Ok(None)
  }

  /// Links an unnamed temporary at `name`.
  #[cfg(target_os = "linux")]
  fn link_unnamed(&self, file: HostFile, dir: HostDir, name: &str) -> Result<(), HostError> {
    let proc_path = format!("/proc/self/fd/{}", self.file(file)?.as_raw_fd());
    rustix::fs::linkat(
      rustix::fs::CWD,
      proc_path.as_str(),
      self.dir(dir)?,
      name,
      AtFlags::SYMLINK_FOLLOW,
    )
    .map_err(refusal)
  }

  #[cfg(not(target_os = "linux"))]
  fn link_unnamed(&self, _file: HostFile, _dir: HostDir, _name: &str) -> Result<(), HostError> {
    Err(HostError::Unavailable(
      rustix::io::Errno::OPNOTSUPP.raw_os_error(),
    ))
  }
}

/// The exchange: Linux `renameat2(RENAME_EXCHANGE)`.
#[cfg(target_os = "linux")]
fn exchange_names(dir: BorrowedFd<'_>, a: &str, b: &str) -> Result<(), HostError> {
  rustix::fs::renameat_with(dir, a, dir, b, rustix::fs::RenameFlags::EXCHANGE).map_err(refusal)
}

/// The exchange: macOS `renameatx_np(RENAME_SWAP)`.
#[cfg(target_os = "macos")]
fn exchange_names(dir: BorrowedFd<'_>, a: &str, b: &str) -> Result<(), HostError> {
  let a = std::ffi::CString::new(a)
    .map_err(|_| HostError::Unavailable(rustix::io::Errno::INVAL.raw_os_error()))?;
  let b = std::ffi::CString::new(b)
    .map_err(|_| HostError::Unavailable(rustix::io::Errno::INVAL.raw_os_error()))?;
  // SAFETY: `dir` is a directory descriptor this host owns for the call's duration; `a` and
  // `b` are NUL-terminated C strings that outlive the call; `renameatx_np` reads them and
  // writes nothing to memory the caller holds.
  let rc = unsafe {
    libc::renameatx_np(
      dir.as_raw_fd(),
      a.as_ptr(),
      dir.as_raw_fd(),
      b.as_ptr(),
      libc::RENAME_SWAP,
    )
  };
  if rc == 0 {
    Ok(())
  } else {
    Err(refusal(rustix::io::Errno::from_raw_os_error(
      std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    )))
  }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exchange_names(_dir: BorrowedFd<'_>, _a: &str, _b: &str) -> Result<(), HostError> {
  Err(HostError::Unavailable(
    rustix::io::Errno::OPNOTSUPP.raw_os_error(),
  ))
}

/// The data barrier: Linux `fdatasync`.
#[cfg(not(target_os = "macos"))]
fn data_barrier(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  rustix::fs::fdatasync(fd).map_err(refusal)
}

/// The data barrier: macOS `fcntl(F_BARRIERFSYNC)` (ordered against later writes, not a
/// media flush; the media barrier is `sync_media`).
#[cfg(target_os = "macos")]
fn data_barrier(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  // SAFETY: `fcntl` with `F_BARRIERFSYNC` takes no pointer argument; `fd` is a descriptor
  // this host owns for the call's duration.
  let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_BARRIERFSYNC) };
  if rc == 0 {
    Ok(())
  } else {
    Err(refusal(rustix::io::Errno::from_raw_os_error(
      std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    )))
  }
}

/// The media barrier: Linux `fsync` reaches the media already.
#[cfg(not(target_os = "macos"))]
fn media_barrier(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  rustix::fs::fsync(fd).map_err(refusal)
}

/// The media barrier: macOS `fcntl(F_FULLFSYNC)`.
#[cfg(target_os = "macos")]
fn media_barrier(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  // SAFETY: `fcntl` with `F_FULLFSYNC` takes no pointer argument; `fd` is a descriptor this
  // host owns for the call's duration.
  let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_FULLFSYNC) };
  if rc == 0 {
    Ok(())
  } else {
    Err(refusal(rustix::io::Errno::from_raw_os_error(
      std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    )))
  }
}

impl HostFs for OsLand {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    self.host.facts(dir)
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    self.host.fingerprint_dir(dir)
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    self.host.list(dir)
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    self.host.open_dir(parent, name)
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    self.host.open_file(dir, name)
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    self.host.fstat(file)
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    self.host.read_at(file, off, buf)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    self.host.read_link(dir, name)
  }

  fn close_file(&mut self, file: HostFile) {
    self.temps.remove(&file.0);
    self.host.close_file(file);
  }

  fn close_dir(&mut self, dir: HostDir) {
    self.host.close_dir(dir);
  }

  fn watch(&mut self, dir: HostDir) -> WatchState {
    self.host.watch(dir)
  }

  fn hints(&mut self) -> Vec<Hint> {
    self.host.hints()
  }
}

impl LandFs for OsLand {
  fn capabilities(&mut self, dir: HostDir) -> Result<LandCapabilities, HostError> {
    self.dir(dir)?;
    Ok(LandCapabilities {
      exchange: cfg!(any(target_os = "linux", target_os = "macos")),
      reflink: false,
      unnamed_temporaries: cfg!(target_os = "linux"),
    })
  }

  fn create_temp(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    if let Some(file) = self.create_unnamed(dir)? {
      return Ok(file);
    }
    self.create_named(dir, name)
  }

  fn write_at(&mut self, file: HostFile, off: u64, bytes: &[u8]) -> Result<(), HostError> {
    let fd = self.file(file)?;
    let mut written = 0usize;
    while written < bytes.len() {
      let at = off.saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
      let n = rustix::io::pwrite(fd, &bytes[written..], at).map_err(refusal)?;
      if n == 0 {
        return Err(HostError::Unavailable(rustix::io::Errno::IO.raw_os_error()));
      }
      written += n;
    }
    Ok(())
  }

  fn sync_file(&mut self, file: HostFile) -> Result<(), HostError> {
    data_barrier(self.file(file)?)
  }

  fn set_mode(&mut self, file: HostFile, mode: u32) -> Result<(), HostError> {
    rustix::fs::fchmod(self.file(file)?, mode_bits(mode)).map_err(refusal)
  }

  fn set_mtime(&mut self, file: HostFile, mtime_ns: i64) -> Result<(), HostError> {
    let stamp = rustix::fs::Timespec {
      tv_sec: mtime_ns.div_euclid(NS_PER_S),
      tv_nsec: mtime_ns.rem_euclid(NS_PER_S),
    };
    let omit = rustix::fs::Timespec {
      tv_sec: 0,
      tv_nsec: rustix::fs::UTIME_OMIT,
    };
    rustix::fs::futimens(
      self.file(file)?,
      &rustix::fs::Timestamps {
        last_access: omit,
        last_modification: stamp,
      },
    )
    .map_err(refusal)
  }

  fn place(&mut self, file: HostFile, dir: HostDir, name: &str) -> Result<(), HostError> {
    let temp = self
      .temps
      .get(&file.0)
      .cloned()
      .ok_or(HostError::StaleHandle)?;
    match temp.created_as {
      None => self.link_unnamed(file, dir, name),
      Some(created) if temp.dir == dir && created.as_ref() == name => Ok(()),
      Some(created) => rustix::fs::linkat(
        self.dir(temp.dir)?,
        created.as_ref(),
        self.dir(dir)?,
        name,
        AtFlags::empty(),
      )
      .map_err(refusal),
    }
  }

  fn exchange(&mut self, dir: HostDir, a: &str, b: &str) -> Result<(), HostError> {
    exchange_names(self.dir(dir)?, a, b)
  }

  fn rename(
    &mut self,
    dir: HostDir,
    from: &str,
    to_dir: HostDir,
    to: &str,
  ) -> Result<(), HostError> {
    rustix::fs::renameat(self.dir(dir)?, from, self.dir(to_dir)?, to).map_err(refusal)
  }

  fn unlink(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    rustix::fs::unlinkat(self.dir(dir)?, name, AtFlags::empty()).map_err(refusal)
  }

  fn mkdir(&mut self, dir: HostDir, name: &str, mode: u32) -> Result<(), HostError> {
    rustix::fs::mkdirat(self.dir(dir)?, name, mode_bits(mode)).map_err(refusal)
  }

  fn rmdir(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    rustix::fs::unlinkat(self.dir(dir)?, name, AtFlags::REMOVEDIR).map_err(refusal)
  }

  fn symlink(&mut self, dir: HostDir, name: &str, target: &str) -> Result<(), HostError> {
    rustix::fs::symlinkat(target, self.dir(dir)?, name).map_err(refusal)
  }

  fn sync_dir(&mut self, dir: HostDir) -> Result<(), HostError> {
    rustix::fs::fsync(self.dir(dir)?).map_err(refusal)
  }

  fn sync_media(&mut self, dir: HostDir) -> Result<(), HostError> {
    media_barrier(self.dir(dir)?)
  }
}

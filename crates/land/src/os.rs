//! The write seam over the operating system (§4.15 step 6, "Exchange fallback"; D-26): the
//! only code in the workspace that creates, writes, links, exchanges, renames or removes a host
//! path. It runs inside a granted landing only; the engine calls nothing here before the grant
//! and the lease.
//!
//! Shape: every verb is relative to a descriptor the base host holds (`OsHost`, which resolves
//! the one root path with `O_NOFOLLOW` and hands out handles); no verb takes a path string.
//! Linux: `O_TMPFILE` temporaries linked by `linkat` through `/proc/self/fd`, a batch's barrier as
//! its files' `fdatasync`s all in flight at once (A-130: journal group commit merges them into few
//! commits and flushes), `renameat2(RENAME_EXCHANGE)`, `fsync` on directories. macOS: hidden-name
//! temporaries (`O_CREAT|O_EXCL`), `fsync` as each file's write-out and one `fcntl(F_BARRIERFSYNC)` per
//! device as a batch's barrier, `renameatx_np(RENAME_SWAP)`, `fcntl(F_FULLFSYNC)` on the target as the
//! media barrier when the grant asked for it. Other unixes: `fdatasync` as each file's write-out, the
//! barrier adding nothing. Linux's one call that covers a batch, `syncfs`, flushes every other writer's
//! dirty data too: beside one busy writer a barrier took up to 3.7 s and a 16-file batch cost 4.0–7.8 ms
//! a file against `fdatasync`'s 0.65–0.79 ms (2026-10-10, `BENCHMARKS.md`), so it is not used. Both:
//! `fchmod` (on files and on each made directory, so the umask never edits a recorded mode),
//! `futimens`, `unlinkat`, `mkdirat`, `symlinkat`, `renameat`. The three `unsafe`
//! sites are the libc calls rustix does not wrap (`F_BARRIERFSYNC`, `F_FULLFSYNC`,
//! `renameatx_np`), each on a descriptor this host owns.
//!
//! Containment (§4.15 step 4, §4.13): [`OsLand::open_target`] opens the target path one
//! component at a time with `O_DIRECTORY|O_NOFOLLOW`; an unfollowed symlink is classified
//! as `EscapesTarget` even when the OS reports `ENOTDIR`. `..` is refused; the target must be owned by the effective
//! user (`TargetNotOwned`). `TargetIsVolume` waits on the mount table of Phase 3 (GAPS §8c).

use std::collections::BTreeMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
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

/// A writer's own state while it has lent its host away ([`OsLand::lend`]).
#[derive(Debug)]
pub struct Away {
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

/// Linux may report ENOTDIR for O_DIRECTORY|O_NOFOLLOW on a symlink. Inspect only the
/// refused component, without following it; every successful open still uses NOFOLLOW.
fn target_component_refusal(
  dir: BorrowedFd<'_>,
  name: &str,
  error: rustix::io::Errno,
) -> TargetRefusal {
  if error == rustix::io::Errno::LOOP {
    return TargetRefusal::EscapesTarget;
  }
  if error == rustix::io::Errno::NOTDIR {
    let stat = match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
      Ok(stat) => stat,
      Err(error) => return TargetRefusal::Unavailable(refusal(error)),
    };
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::Symlink {
      return TargetRefusal::EscapesTarget;
    }
  }
  TargetRefusal::Unavailable(refusal(error))
}

/// Walks an absolute target path from `/` one component at a time (no `..`, no symlink: containment), checks
/// the target is owned by this process's user, and returns its descriptor and the key it lands under.
fn walk_target(path: &Path) -> Result<(OwnedFd, String), TargetRefusal> {
  let mut components = path.components();
  if components.next() != Some(Component::RootDir) {
    return Err(TargetRefusal::NotAbsolute);
  }
  let mut current = rustix::fs::open("/", dir_flags(), Mode::empty())
    .map_err(|e| TargetRefusal::Unavailable(refusal(e)))?;
  let mut key = String::new();
  for component in components {
    let name = match component {
      Component::Normal(n) => n.to_str().ok_or(TargetRefusal::EscapesTarget)?,
      Component::CurDir => continue,
      _ => return Err(TargetRefusal::EscapesTarget),
    };
    let next = rustix::fs::openat(&current, name, dir_flags(), Mode::empty())
      .map_err(|error| target_component_refusal(current.as_fd(), name, error))?;
    key.push('/');
    key.push_str(name);
    current = next;
  }
  let st = rustix::fs::fstat(&current).map_err(|e| TargetRefusal::Unavailable(refusal(e)))?;
  if st.st_uid != rustix::process::geteuid().as_raw() {
    return Err(TargetRefusal::TargetNotOwned);
  }
  Ok((current, key))
}

impl OsLand {
  /// Opens an absolute target path with containment and the ownership check, and returns the
  /// writer with the target. Ancestor descriptors used to resolve it are closed on the way.
  pub fn open_target(path: &Path) -> Result<(Self, LandingTarget), TargetRefusal> {
    let (current, key) = walk_target(path)?;
    let (host, _) = OsHost::open_root(Path::new("/")).map_err(TargetRefusal::Unavailable)?;
    Self::adopt_target(host, current, key).map_err(|(_, refusal)| refusal)
  }

  /// [`OsLand::open_target`] inside an existing host — the base host an overlay volume's slot holds — so the
  /// target and every handle the volume's base names live in one host (a handle is an index into its own
  /// host's table; another host's same number names another directory). A refusal hands the host back, so
  /// the volume keeps it.
  pub fn open_target_in(
    host: OsHost,
    path: &Path,
  ) -> Result<(Self, LandingTarget), (OsHost, TargetRefusal)> {
    match walk_target(path) {
      Ok((current, key)) => Self::adopt_target(host, current, key),
      Err(refusal) => Err((host, refusal)),
    }
  }

  /// The writer over `host`, with the walked target `current` adopted into it under `key`.
  fn adopt_target(
    mut host: OsHost,
    current: OwnedFd,
    key: String,
  ) -> Result<(Self, LandingTarget), (OsHost, TargetRefusal)> {
    let dir = match host.adopt_dir(current) {
      Ok(dir) => dir,
      Err(error) => return Err((host, TargetRefusal::Unavailable(error))),
    };
    let target = LandingTarget {
      dir,
      key: if key.is_empty() {
        "/".into()
      } else {
        key.into()
      },
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

  /// The base host beneath, kept after the landing: a scratch volume the landing made an overlay over its
  /// target (`Overlay::land_advance`) names the target by this host's handles, so the volume must be served
  /// through this host and no other. The writer's temporaries are names only; any a crash left are its
  /// hidden siblings, which the next landing's sweep removes.
  pub fn into_host(self) -> OsHost {
    self.host
  }

  /// Lends the host back between a sliced landing's slices (AUD-29-25): an overlay volume serves its base
  /// through it while the landing waits for its next slice. The writer's own state — its temporaries, none
  /// open between units — is kept in the returned [`Away`], which [`OsLand::resume`] takes back with the host.
  pub fn lend(self) -> (OsHost, Away) {
    (self.host, Away { temps: self.temps })
  }

  /// The writer again, over the host it lent and its own state.
  pub fn resume(host: OsHost, away: Away) -> Self {
    Self {
      host,
      temps: away.temps,
    }
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
    let file = self.host.adopt_file(fd)?;
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
        let file = self.host.adopt_file(fd)?;
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
  #[allow(clippy::disallowed_methods)] // the write seam's own placement (R1, D-26; AUD-29-31).
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

  /// The batch barrier on macOS: one `F_BARRIERFSYNC` per device among `files`, on that device's last file. The
  /// barrier applies to everything `fsync`ed on its device before it, so it orders every file written out there; files
  /// on another device (a mount inside the target) need that device's own.
  #[cfg(target_os = "macos")]
  fn order_written_out(&self, files: &[HostFile]) -> Result<(), HostError> {
    let mut last_on_device = BTreeMap::new();
    for file in files {
      let device = rustix::fs::fstat(self.file(*file)?)
        .map_err(refusal)?
        .st_dev;
      last_on_device.insert(device, *file);
    }
    for file in last_on_device.into_values() {
      data_barrier(self.file(file)?)?;
    }
    Ok(())
  }

  /// The batch barrier on Linux: every file's `fdatasync` in flight at once, from one scoped thread a file but the
  /// caller's, so the journal's group commit (ext4's jbd2 joins every sync waiting on a commit to it; XFS forces its
  /// log once for all) merges them into few commits and device flushes instead of one each. Each file is durable when
  /// its own call returns, exactly as alone; the batch is refused when any file's is, and every thread is joined first.
  /// A thread the system refuses to start leaves its file to the caller, in turn. Measured (2026-10-10,
  /// `BENCHMARKS.md`): 16 at once against one at a time, 446–1,122 µs a file against 2,670–4,142 beside a busy writer.
  #[cfg(target_os = "linux")]
  fn order_written_out(&self, files: &[HostFile]) -> Result<(), HostError> {
    let fds = files
      .iter()
      .map(|file| self.file(*file))
      .collect::<Result<Vec<_>, _>>()?;
    let Some((first, rest)) = fds.split_first() else {
      return Ok(());
    };
    std::thread::scope(|scope| {
      let mut started = Vec::with_capacity(rest.len());
      let mut refused = Vec::new();
      for fd in rest.iter().copied() {
        match std::thread::Builder::new().spawn_scoped(scope, move || rustix::fs::fdatasync(fd)) {
          Ok(thread) => started.push(thread),
          Err(_) => refused.push(fd),
        }
      }
      let mut synced = rustix::fs::fdatasync(*first).map_err(refusal);
      for fd in refused {
        synced = synced.and(rustix::fs::fdatasync(fd).map_err(refusal));
      }
      for thread in started {
        let joined = thread.join().map_or(
          Err(HostError::Unavailable(rustix::io::Errno::IO.raw_os_error())),
          |own| own.map_err(refusal),
        );
        synced = synced.and(joined);
      }
      synced
    })
  }

  /// The batch barrier on the other unixes: each file's write-out (`fdatasync`) already made it durable, so nothing
  /// remains but to refuse a handle this writer does not hold.
  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  fn order_written_out(&self, files: &[HostFile]) -> Result<(), HostError> {
    for file in files {
      self.file(*file)?;
    }
    Ok(())
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
  renameatx(dir, a, dir, b, libc::RENAME_SWAP)
}

/// A rename that never replaces (AUD-29-04): Linux `renameat2(RENAME_NOREPLACE)`, refused `EEXIST` when
/// `to` exists.
#[cfg(target_os = "linux")]
fn rename_without_replacing(
  dir: BorrowedFd<'_>,
  from: &str,
  to_dir: BorrowedFd<'_>,
  to: &str,
) -> Result<(), HostError> {
  rustix::fs::renameat_with(dir, from, to_dir, to, rustix::fs::RenameFlags::NOREPLACE)
    .map_err(refusal)
}

/// A rename that never replaces: macOS `renameatx_np(RENAME_EXCL)`, refused `EEXIST` when `to` exists.
#[cfg(target_os = "macos")]
fn rename_without_replacing(
  dir: BorrowedFd<'_>,
  from: &str,
  to_dir: BorrowedFd<'_>,
  to: &str,
) -> Result<(), HostError> {
  renameatx(dir, from, to_dir, to, libc::RENAME_EXCL)
}

/// A named temporary moved from `from` to `to`, refused `EEXIST` when `to` exists, holding the one name after: the
/// rename that never replaces, one metadata operation where a link and an unlink were two. APFS measured 46–57 µs a
/// file against 198–259 µs for the pair (2026-10-10, `BENCHMARKS.md`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn move_without_replacing(
  dir: BorrowedFd<'_>,
  from: &str,
  to_dir: BorrowedFd<'_>,
  to: &str,
) -> Result<(), HostError> {
  rename_without_replacing(dir, from, to_dir, to)
}

/// A named temporary moved where no rename refuses an existing name: a link (which refuses one) and the old name's
/// removal, the temporary at both names between the two; the sweep removes a hidden one a crash leaves.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[allow(clippy::disallowed_methods)] // the write seam's own placement (R1, D-26; AUD-29-31).
fn move_without_replacing(
  dir: BorrowedFd<'_>,
  from: &str,
  to_dir: BorrowedFd<'_>,
  to: &str,
) -> Result<(), HostError> {
  rustix::fs::linkat(dir, from, to_dir, to, AtFlags::empty()).map_err(refusal)?;
  rustix::fs::unlinkat(dir, from, AtFlags::empty()).map_err(refusal)
}

/// Where neither exists, the rename that never replaces is unavailable: a removal refuses rather than race.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_without_replacing(
  _dir: BorrowedFd<'_>,
  _from: &str,
  _to_dir: BorrowedFd<'_>,
  _to: &str,
) -> Result<(), HostError> {
  Err(HostError::Unavailable(
    rustix::io::Errno::OPNOTSUPP.raw_os_error(),
  ))
}

/// macOS `renameatx_np` from `a` under `dir` to `b` under `to_dir`, with `flag` (`RENAME_SWAP` or
/// `RENAME_EXCL`).
#[cfg(target_os = "macos")]
fn renameatx(
  dir: BorrowedFd<'_>,
  a: &str,
  to_dir: BorrowedFd<'_>,
  b: &str,
  flag: libc::c_uint,
) -> Result<(), HostError> {
  let a = std::ffi::CString::new(a)
    .map_err(|_| HostError::Unavailable(rustix::io::Errno::INVAL.raw_os_error()))?;
  let b = std::ffi::CString::new(b)
    .map_err(|_| HostError::Unavailable(rustix::io::Errno::INVAL.raw_os_error()))?;
  // SAFETY: `dir` and `to_dir` are directory descriptors this host owns for the call's duration; `a` and
  // `b` are NUL-terminated C strings that outlive the call; `renameatx_np` reads them and
  // writes nothing to memory the caller holds.
  let rc = unsafe {
    libc::renameatx_np(
      dir.as_raw_fd(),
      a.as_ptr(),
      to_dir.as_raw_fd(),
      b.as_ptr(),
      flag,
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

/// A file's write-out on Linux: nothing yet — the batch's barrier syncs every file of the batch at once.
#[cfg(target_os = "linux")]
fn write_out_file(_fd: BorrowedFd<'_>) -> Result<(), HostError> {
  Ok(())
}

/// A file's write-out on the other unixes: `fdatasync`, which makes the file durable alone; their batch barrier then
/// adds nothing.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn write_out_file(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  rustix::fs::fdatasync(fd).map_err(refusal)
}

/// A file's write-out: macOS `fsync`, which hands the file's data and attributes to the device (not a media flush);
/// the batch's barrier after it orders them ahead of every later write.
#[cfg(target_os = "macos")]
#[allow(clippy::disallowed_methods)] // the write seam's own write-out (R1, D-26; AUD-29-31).
fn write_out_file(fd: BorrowedFd<'_>) -> Result<(), HostError> {
  rustix::fs::fsync(fd).map_err(refusal)
}

/// The data barrier: macOS `fcntl(F_BARRIERFSYNC)`: `fsync` of the file, then a barrier that "applies to I/O that have
/// been flushed with fsync(2) on the same device before" — ordered against later writes, not a media flush (the media
/// barrier is `sync_media`). fcntl(2) names the batch's use: "execute operations of phase one, then fsync(2) each FD and
/// issue a single barrier. Finally execute operations of phase two" (implemented on HFS and APFS) [B: macOS fcntl(2)].
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
#[allow(clippy::disallowed_methods)] // the write seam's own barrier (R1, D-26; AUD-29-31).
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

  fn now_ns(&mut self) -> i64 {
    self.host.now_ns()
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

// The write seam (R1, D-26): the one place in slates that writes host paths, under a granted landing. The
// resolved-path lint that refuses these calls everywhere else (`clippy.toml`, AUD-29-31) is allowed here only.
#[allow(clippy::disallowed_methods)]
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
      let n =
        rustix::io::pwrite(fd, bytes.get(written..).unwrap_or_default(), at).map_err(refusal)?;
      if n == 0 {
        return Err(HostError::Unavailable(rustix::io::Errno::IO.raw_os_error()));
      }
      written = written.saturating_add(n);
    }
    Ok(())
  }

  fn write_out(&mut self, file: HostFile) -> Result<(), HostError> {
    write_out_file(self.file(file)?)
  }

  fn barrier(&mut self, files: &[HostFile]) -> Result<(), HostError> {
    self.order_written_out(files)
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
      Some(created) => {
        move_without_replacing(self.dir(temp.dir)?, created.as_ref(), self.dir(dir)?, name)?;
        self.temps.insert(
          file.0,
          Temp {
            dir,
            created_as: Some(name.into()),
          },
        );
        Ok(())
      }
    }
  }

  fn exchange(&mut self, dir: HostDir, a: &str, b: &str) -> Result<(), HostError> {
    exchange_names(self.dir(dir)?, a, b)
  }

  fn rename_noreplace(
    &mut self,
    dir: HostDir,
    from: &str,
    to_dir: HostDir,
    to: &str,
  ) -> Result<(), HostError> {
    rename_without_replacing(self.dir(dir)?, from, self.dir(to_dir)?, to)
  }

  fn entry_fingerprint(&mut self, dir: HostDir, name: &str) -> Result<Fingerprint, HostError> {
    self.host.entry_fingerprint(dir, name)
  }

  fn unlink(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    rustix::fs::unlinkat(self.dir(dir)?, name, AtFlags::empty()).map_err(refusal)
  }

  /// `mkdirat` applies the process umask, so the directory is then opened (`O_DIRECTORY|O_NOFOLLOW`,
  /// never a symlink swapped in under the name) and given the recorded mode exactly with `fchmod`,
  /// as a file's temporary is by `set_mode`. Before 2026-10-01 the umask cleared bits the volume
  /// held (0775 landed 0755; docs/bugs/2026-10-01-landed-directory-mode-umask.md).
  fn mkdir(&mut self, dir: HostDir, name: &str, mode: u32) -> Result<(), HostError> {
    let parent = self.dir(dir)?;
    rustix::fs::mkdirat(parent, name, mode_bits(mode)).map_err(refusal)?;
    let made = rustix::fs::openat(parent, name, dir_flags(), Mode::empty()).map_err(refusal)?;
    rustix::fs::fchmod(&made, mode_bits(mode)).map_err(refusal)
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
